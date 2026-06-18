//! In-process client for the rio-mountd UDS protocol.
//!
//! **Synchronous by design.** The callers are FUSE callbacks on
//! fuser's thread pool — not a tokio context — so every method blocks
//! the calling thread. Replies arrive out of order (`Promote` runs on
//! the daemon's `spawn_blocking` pool while `BackingOpen` is answered
//! inline), so a dedicated reader thread owns the receive side of the
//! socket and dispatches each reply to the waiter registered for its
//! `seq`. Senders share the socket directly: `SOCK_SEQPACKET` makes
//! each `sendmsg` an atomic datagram, so concurrent sends cannot
//! interleave frame bytes.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{self, RecvTimeoutError, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};

use super::mountd_proto::{self as proto, ErrKind, Reply, Req, Request, Resp};
use crate::IgnorePoison;

/// In-flight requests: `seq` → the blocked caller's reply channel.
type Pending = HashMap<u32, SyncSender<(Resp, Vec<OwnedFd>)>>;

/// Errors from the mountd client. Every variant means the mountd
/// session for this build is dead — the caller fails the build as
/// `InfrastructureFailure` (re-queue), except where
/// [`ErrKind::is_build_fatal`] says otherwise for `Rejected`.
#[derive(Debug, thiserror::Error)]
pub enum MountdError {
    /// `connect(2)` to the mountd UDS failed.
    #[error("connect {path}: {source}")]
    Connect {
        /// The mountd socket path that was dialled.
        path: String,
        /// The underlying `connect(2)` error.
        source: std::io::Error,
    },
    /// A frame failed to encode/decode or send/recv on the socket.
    #[error("mountd request: {0}")]
    Frame(#[from] proto::FrameError),
    /// The daemon closed the connection or the reader thread died.
    /// Every outstanding and future request fails with this — the
    /// build's mount is gone and the build must fail as infra.
    #[error("mountd connection closed")]
    Closed,
    /// `recv_timeout` elapsed waiting for the matching `seq` reply.
    #[error("mountd did not reply within {0:?}")]
    Timeout(Duration),
    /// A typed protocol rejection. `ErrKind::is_build_fatal` decides
    /// whether the caller surfaces a build failure or an infra retry.
    #[error("mountd rejected request: {0}")]
    Rejected(ErrKind),
    /// The reply's [`Resp`] variant does not match the request kind
    /// (e.g. `BackingId` for a `Promote`). Daemon bug or wire
    /// corruption — fail closed.
    #[error("mountd reply has unexpected variant for this request")]
    UnexpectedReply,
}

struct Inner {
    sock: OwnedFd,
    next_seq: AtomicU32,
    /// `seq` → the waiter's channel. The reader thread takes the entry
    /// when the matching reply arrives; `call()` removes its own entry
    /// on timeout so an abandoned waiter doesn't leak.
    pending: Mutex<Pending>,
    /// Set once by the reader thread when the socket dies. Checked
    /// before every send so post-mortem calls fail immediately instead
    /// of writing into a dead socket and waiting out the full timeout.
    closed: std::sync::atomic::AtomicBool,
}

/// Drops `shutdown(2)` the socket when the last [`MountdClient`] clone
/// goes away. The reader thread keeps a *strong* `Arc<Inner>` while
/// blocked in `recvmsg`, so the fd cannot be closed out from under it
/// (a close would let an unrelated thread reuse the fd number mid-
/// recvmsg). Shutdown instead wakes the reader with EOF; the reader
/// exits, drops its `Arc`, and *that* closes the fd — which is the
/// protocol's teardown signal (mountd detaches the FUSE mount and
/// reaps the staging dir).
struct ShutdownOnDrop(Arc<Inner>);

impl Drop for ShutdownOnDrop {
    fn drop(&mut self) {
        let _ =
            nix::sys::socket::shutdown(self.0.sock.as_raw_fd(), nix::sys::socket::Shutdown::Both);
    }
}

/// Handle to one mountd connection (== one build). Cloneable; all
/// clones share the socket and the reader thread.
#[derive(Clone)]
pub struct MountdClient {
    inner: Arc<Inner>,
    _shutdown: Arc<ShutdownOnDrop>,
}

impl MountdClient {
    /// Connect to the daemon's `SOCK_SEQPACKET` socket and start the
    /// reply-dispatch thread. Does not send anything — the caller's
    /// first request must be `Mount{}` per the protocol.
    pub fn connect(path: &Path) -> Result<Self, MountdError> {
        let conn_err = |source: nix::Error| MountdError::Connect {
            path: path.display().to_string(),
            source: source.into(),
        };
        let sock = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .map_err(conn_err)?;
        let addr = UnixAddr::new(path).map_err(conn_err)?;
        connect(sock.as_raw_fd(), &addr).map_err(conn_err)?;

        let inner = Arc::new(Inner {
            sock,
            next_seq: AtomicU32::new(1),
            pending: Mutex::new(HashMap::new()),
            closed: std::sync::atomic::AtomicBool::new(false),
        });

        // Reader thread: blocking recvmsg loop for the connection's
        // lifetime. Exits on EOF — either mountd closed its end or the
        // last client clone's ShutdownOnDrop fired. A spawn failure
        // (thread limit, OOM) is an infra error like any other connect
        // failure, not a panic.
        let reader = Arc::clone(&inner);
        let spawned = std::thread::Builder::new()
            .name("mountd-client-rx".into())
            .spawn(move || {
                loop {
                    match proto::recv_frame(reader.sock.as_raw_fd()) {
                        Ok(frame) => match proto::decode::<Reply>(&frame.bytes) {
                            Ok(reply) => {
                                let tx = reader.pending.lock().ignore_poison().remove(&reply.seq);
                                match tx {
                                    // The waiter may have timed out and removed
                                    // itself; a send error here is the same race.
                                    Some(tx) => {
                                        let _ = tx.send((reply.resp, frame.fds));
                                    }
                                    None => tracing::warn!(
                                        seq = reply.seq,
                                        "mountd reply for unknown seq (waiter timed out?)"
                                    ),
                                }
                            }
                            Err(e) => {
                                tracing::error!(error = %e, "undecodable mountd reply; closing");
                                reader.fail_all();
                                return;
                            }
                        },
                        Err(e) => {
                            if !matches!(e, proto::FrameError::Eof) {
                                tracing::warn!(error = %e, "mountd socket error");
                            }
                            reader.fail_all();
                            return;
                        }
                    }
                }
            });
        if let Err(source) = spawned {
            return Err(MountdError::Connect {
                path: path.display().to_string(),
                source,
            });
        }

        Ok(Self {
            _shutdown: Arc::new(ShutdownOnDrop(Arc::clone(&inner))),
            inner,
        })
    }

    /// Shut the connection down NOW, regardless of how many clones are
    /// still alive (the Opener inside a running FUSE session holds
    /// some). The per-build teardown path (`CastoreSession::drop`, see
    /// its doc for the full ordering rationale) needs this. `shutdown(2)`,
    /// not `close(2)`, for the same fd-reuse-under-recvmsg reason as the
    /// last-clone `ShutdownOnDrop` path.
    pub fn shutdown(&self) {
        self.inner.closed.store(true, Ordering::Release);
        let _ = nix::sys::socket::shutdown(
            self.inner.sock.as_raw_fd(),
            nix::sys::socket::Shutdown::Both,
        );
    }

    /// Send `req` (with `fds` in the frame's `SCM_RIGHTS` cmsg) and
    /// block until its reply arrives or `timeout` elapses. A timeout
    /// abandons the reply — if it arrives later the reader logs and
    /// drops it — but does not poison the connection.
    pub fn call(
        &self,
        req: Req,
        fds: &[RawFd],
        timeout: Duration,
    ) -> Result<(Resp, Vec<OwnedFd>), MountdError> {
        if self.inner.closed.load(Ordering::Acquire) {
            return Err(MountdError::Closed);
        }
        let seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        // Rendezvous channel: the reader's send blocks only if the
        // waiter is between registering and recv'ing, which is a few
        // instructions. Capacity 1 keeps it nonblocking in practice.
        let (tx, rx) = mpsc::sync_channel(1);
        self.inner.pending.lock().ignore_poison().insert(seq, tx);

        let bytes = proto::encode(&Request { seq, req })?;
        if let Err(e) = proto::send_frame(self.inner.sock.as_raw_fd(), &bytes, fds) {
            self.inner.pending.lock().ignore_poison().remove(&seq);
            return Err(e.into());
        }

        match rx.recv_timeout(timeout) {
            Ok(reply) => Ok(reply),
            Err(RecvTimeoutError::Disconnected) => Err(MountdError::Closed),
            Err(RecvTimeoutError::Timeout) => {
                self.inner.pending.lock().ignore_poison().remove(&seq);
                Err(MountdError::Timeout(timeout))
            }
        }
    }

    /// `Mount{build_id}` → the staging quota and the handed-off
    /// `/dev/fuse` fd. Must be the first request on the connection.
    pub fn mount(&self, build_id: &str, timeout: Duration) -> Result<(u64, OwnedFd), MountdError> {
        let (resp, mut fds) = self.call(
            Req::Mount {
                build_id: build_id.to_owned(),
            },
            &[],
            timeout,
        )?;
        match resp {
            Resp::Mounted {
                staging_quota_bytes,
            } => match fds.pop() {
                Some(fd) if fds.is_empty() => Ok((staging_quota_bytes, fd)),
                _ => Err(MountdError::UnexpectedReply),
            },
            Resp::Err(e) => Err(MountdError::Rejected(e)),
            _ => Err(MountdError::UnexpectedReply),
        }
    }

    /// Register `fd` as a FUSE passthrough backing file via the
    /// daemon's privileged `BACKING_OPEN` ioctl. The returned id is
    /// scoped to this connection's FUSE mount.
    pub fn backing_open(&self, fd: RawFd, timeout: Duration) -> Result<u32, MountdError> {
        match self.call(Req::BackingOpen, &[fd], timeout)?.0 {
            Resp::BackingId(id) => Ok(id),
            Resp::Err(e) => Err(MountdError::Rejected(e)),
            _ => Err(MountdError::UnexpectedReply),
        }
    }

    /// Release a `backing_id`. Best-effort: the kernel holds its own
    /// reference for any still-open file, and the whole connection's
    /// ids are reaped at unmount, so a failure here only delays IDR
    /// slot reuse.
    pub fn backing_close(&self, backing_id: u32, timeout: Duration) -> Result<(), MountdError> {
        unit_reply(self.call(Req::BackingClose { backing_id }, &[], timeout)?.0)
    }

    /// Verify-copy `staging/{build_id}/{hex(digest)}` into the shared
    /// node cache. Blocks for the daemon's copy+hash (seconds for a
    /// multi-GiB file) — size the timeout accordingly.
    pub fn promote(&self, digest: [u8; 32], timeout: Duration) -> Result<(), MountdError> {
        unit_reply(self.call(Req::Promote { digest }, &[], timeout)?.0)
    }

    /// Verify-copy a batch of `staging/{build_id}/chunks/{hex}` entries
    /// into the shared node chunk cache. Best-effort from the caller's
    /// point of view: the streaming fill assembles from its own staging
    /// copy and never depends on the promoted entries.
    pub fn promote_chunks(
        &self,
        chunk_digests: Vec<[u8; 32]>,
        timeout: Duration,
    ) -> Result<(), MountdError> {
        unit_reply(
            self.call(Req::PromoteChunks { chunk_digests }, &[], timeout)?
                .0,
        )
    }
}

/// Map a reply that carries no data: `Ok` is success, a typed
/// rejection becomes [`MountdError::Rejected`], anything else is a
/// protocol violation.
fn unit_reply(resp: Resp) -> Result<(), MountdError> {
    match resp {
        Resp::Ok => Ok(()),
        Resp::Err(e) => Err(MountdError::Rejected(e)),
        _ => Err(MountdError::UnexpectedReply),
    }
}

impl Inner {
    /// Mark the connection dead and wake every waiter with
    /// [`MountdError::Closed`] (by dropping their senders).
    fn fail_all(&self) {
        self.closed.store(true, Ordering::Release);
        self.pending.lock().ignore_poison().clear();
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::{Backlog, bind, listen};
    use std::os::fd::FromRawFd;

    /// A real `SOCK_SEQPACKET` listener speaking the real wire codec —
    /// the daemon's transport without its privileged guts. The test
    /// controls reply order explicitly because that is the property
    /// under test: a daemon answering `Promote` from `spawn_blocking`
    /// replies out of request order.
    struct FakeDaemon {
        conn: OwnedFd,
    }

    impl FakeDaemon {
        fn listen(path: &Path) -> OwnedFd {
            let sock = socket(
                AddressFamily::Unix,
                SockType::SeqPacket,
                SockFlag::empty(),
                None,
            )
            .unwrap();
            bind(sock.as_raw_fd(), &UnixAddr::new(path).unwrap()).unwrap();
            listen(&sock, Backlog::new(1).unwrap()).unwrap();
            sock
        }

        fn accept(listener: &OwnedFd) -> Self {
            let conn = nix::sys::socket::accept(listener.as_raw_fd()).unwrap();
            // SAFETY: accept(2) just returned a fresh fd we own.
            let conn = unsafe { OwnedFd::from_raw_fd(conn) };
            Self { conn }
        }

        fn recv(&self) -> (Request, Vec<OwnedFd>) {
            let frame = proto::recv_frame(self.conn.as_raw_fd()).unwrap();
            (proto::decode(&frame.bytes).unwrap(), frame.fds)
        }

        fn reply(&self, seq: u32, resp: Resp) {
            let bytes = proto::encode(&Reply { seq, resp }).unwrap();
            proto::send_frame(self.conn.as_raw_fd(), &bytes, &[]).unwrap();
        }
    }

    const T: Duration = Duration::from_secs(5);

    /// Replies delivered out of request order must reach the waiter
    /// whose `seq` they echo — a correlation bug here hands one file's
    /// `BackingId` to another file's `open()`, which the kernel then
    /// happily serves as that file's content.
    #[test]
    fn out_of_order_replies_reach_the_right_waiter() {
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("mountd.sock");
        let listener = FakeDaemon::listen(&sock_path);

        let client = MountdClient::connect(&sock_path).unwrap();
        let daemon = FakeDaemon::accept(&listener);

        // Two requests in flight from two threads; the daemon answers
        // the second one first.
        let c1 = client.clone();
        let t1 = std::thread::spawn(move || c1.promote([0xAA; 32], T));
        let (req1, _) = daemon.recv();
        let c2 = client.clone();
        let t2 = std::thread::spawn(move || c2.backing_close(7, T));
        let (req2, _) = daemon.recv();

        assert!(matches!(req1.req, Req::Promote { digest } if digest == [0xAA; 32]));
        assert!(matches!(req2.req, Req::BackingClose { backing_id: 7 }));

        // Answer in reverse order; give the slow Promote an error so
        // the two replies are distinguishable by more than arrival
        // order.
        daemon.reply(req2.seq, Resp::Ok);
        daemon.reply(req1.seq, Resp::Err(ErrKind::DigestMismatch));

        assert!(t2.join().unwrap().is_ok(), "BackingClose got the Ok");
        let promote = t1.join().unwrap();
        assert!(
            matches!(promote, Err(MountdError::Rejected(ErrKind::DigestMismatch))),
            "Promote got the DigestMismatch, not the other request's reply: {promote:?}"
        );
    }

    /// The daemon closing the socket must wake every blocked caller
    /// with `Closed` — not leave FUSE threads parked until their full
    /// request timeout — and fail subsequent calls immediately.
    #[test]
    fn daemon_close_fails_blocked_and_future_calls() {
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("mountd.sock");
        let listener = FakeDaemon::listen(&sock_path);

        let client = MountdClient::connect(&sock_path).unwrap();
        let daemon = FakeDaemon::accept(&listener);

        let c1 = client.clone();
        let blocked = std::thread::spawn(move || c1.promote([1; 32], Duration::from_secs(30)));
        // Make sure the request is in flight before killing the daemon.
        let _ = daemon.recv();
        drop(daemon);

        let started = std::time::Instant::now();
        let r = blocked.join().unwrap();
        assert!(
            matches!(r, Err(MountdError::Closed)),
            "blocked caller must see Closed, got {r:?}"
        );
        assert!(
            started.elapsed() < Duration::from_secs(10),
            "must not wait out the 30s request timeout"
        );

        let r = client.promote([2; 32], T);
        assert!(
            matches!(r, Err(MountdError::Closed)),
            "post-close calls must fail fast, got {r:?}"
        );
    }

    /// A reply that arrives after its waiter timed out must be dropped
    /// without disturbing other in-flight requests.
    #[test]
    fn late_reply_after_timeout_is_dropped() {
        let tmp = tempfile::tempdir().unwrap();
        let sock_path = tmp.path().join("mountd.sock");
        let listener = FakeDaemon::listen(&sock_path);

        let client = MountdClient::connect(&sock_path).unwrap();
        let daemon = FakeDaemon::accept(&listener);

        let r = client.backing_open(daemon.conn.as_raw_fd(), Duration::from_millis(50));
        assert!(matches!(r, Err(MountdError::Timeout(_))), "got {r:?}");
        let (req, fds) = daemon.recv();
        assert_eq!(fds.len(), 1, "BackingOpen carries its fd via SCM_RIGHTS");

        // The late reply lands after the waiter gave up; the next
        // request on the same connection must still work.
        daemon.reply(req.seq, Resp::BackingId(3));
        let c = client.clone();
        let t = std::thread::spawn(move || c.backing_close(3, T));
        let (req2, _) = daemon.recv();
        daemon.reply(req2.seq, Resp::Ok);
        assert!(t.join().unwrap().is_ok());
    }
}
