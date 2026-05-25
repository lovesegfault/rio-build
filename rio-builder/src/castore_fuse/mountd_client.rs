//! In-process client for the `rio-mountd` UDS protocol.
//!
//! The builder side of [`super::mountd_proto`]: connects to the
//! daemon's `SOCK_SEQPACKET` socket, sends seq-tagged requests, and
//! correlates out-of-order replies. Replaces `bin/spike_mountd_client.rs`
//! as the protocol's reference consumer (the spike binary stays until
//! P0560 deletes it with the rest of the old stack).
//!
//! **std::sync ONLY.** The primary callers are FUSE callbacks
//! (`open()` → `BackingOpen`, `release()` → `BackingClose`, the JIT
//! fill → `Promote`) running on fuser's thread pool, not in a tokio
//! context. A dedicated reader thread demuxes replies to per-call
//! rendezvous channels keyed by `seq`, so a multi-second `Promote`
//! (spawn_blocking on the daemon side) never blocks a concurrent
//! sub-millisecond `BackingOpen`.
//!
//! Every call takes an explicit timeout (`mountd_request_timeout` from
//! config). A timed-out call deregisters its `seq` so a late reply is
//! dropped instead of leaking a channel.

use std::collections::HashMap;
use std::os::fd::{AsRawFd, OwnedFd, RawFd};
use std::path::Path;
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::Duration;

use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};

use super::mountd_proto::{self as proto, ErrKind, Reply, Req, Request, Resp};
use crate::IgnorePoison;

/// A mountd request failure, classified for the build-vs-infra
/// decision.
#[derive(Debug, thiserror::Error)]
pub enum MountdError {
    /// The daemon replied `Err(kind)`. [`ErrKind::is_build_fatal`]
    /// decides whether retrying can ever succeed.
    #[error("rio-mountd rejected the request: {0}")]
    Rejected(ErrKind),
    /// The daemon replied with a `Resp` variant the request cannot
    /// produce (protocol bug on one side or the other).
    #[error("rio-mountd sent an unexpected reply: {0:?}")]
    UnexpectedReply(Resp),
    /// No reply within the caller's timeout. The request may still be
    /// in flight daemon-side; the late reply is discarded.
    #[error("rio-mountd did not reply within {0:?}")]
    Timeout(Duration),
    /// The connection is gone (daemon restarted, socket error). All
    /// in-flight and future calls fail immediately.
    #[error("rio-mountd connection lost: {0}")]
    Disconnected(String),
    /// Local frame encode/send failure.
    #[error("rio-mountd send: {0}")]
    Send(#[from] proto::FrameError),
}

impl MountdError {
    /// `true` for errors that re-fetching/re-staging the same bytes
    /// would reproduce — the build must fail rather than retry-loop.
    /// Connection loss and timeouts are infrastructure failures.
    pub fn is_build_fatal(&self) -> bool {
        match self {
            MountdError::Rejected(kind) => kind.is_build_fatal(),
            MountdError::UnexpectedReply(_) => true,
            MountdError::Timeout(_) | MountdError::Disconnected(_) | MountdError::Send(_) => false,
        }
    }
}

/// One reply as delivered to a waiting caller: the decoded `Resp` plus
/// any fds that arrived in the datagram's `SCM_RIGHTS` cmsg (only
/// `Mounted` carries one).
type Delivery = Result<(Resp, Vec<OwnedFd>), MountdError>;

struct Inner {
    sock: OwnedFd,
    /// Serializes `sendmsg` calls. `SOCK_SEQPACKET` datagrams are
    /// atomic so interleaved sends would not corrupt frames, but the
    /// lock keeps the (alloc seq → register pending → send) sequence
    /// atomic with respect to a concurrent disconnect-drain.
    send: Mutex<()>,
    /// In-flight requests awaiting their reply, keyed by `seq`.
    /// `None` value = the connection is dead; new calls fail fast.
    pending: Mutex<Option<HashMap<u32, SyncSender<Delivery>>>>,
    next_seq: AtomicU32,
}

/// Handle to one mountd connection. Cheap to clone; the underlying
/// socket and reader thread are shared.
///
/// Dropping the last handle shuts the socket down, which (a) wakes the
/// reader thread so it exits and (b) signals teardown to the daemon
/// (mountd unmounts the build's castore mount and reaps its staging
/// dir on connection close).
#[derive(Clone)]
pub struct MountdClient {
    inner: Arc<Inner>,
}

impl MountdClient {
    /// Connect to the daemon's socket and start the reply-demux reader
    /// thread.
    pub fn connect(socket_path: &Path) -> std::io::Result<Self> {
        let sock = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let addr = UnixAddr::new(socket_path)?;
        connect(sock.as_raw_fd(), &addr)?;
        Ok(Self::from_fd(sock))
    }

    /// Wrap an already-connected `SOCK_SEQPACKET` fd (tests use a
    /// `socketpair` half).
    pub fn from_fd(sock: OwnedFd) -> Self {
        let inner = Arc::new(Inner {
            sock,
            send: Mutex::new(()),
            pending: Mutex::new(Some(HashMap::new())),
            next_seq: AtomicU32::new(1),
        });
        let reader = Arc::clone(&inner);
        // The reader thread holds an Arc, so the socket outlives it.
        // It exits when recv_frame returns Eof/Err — which happens when
        // the daemon closes the connection or when the last client
        // handle drops and `Drop` calls shutdown(2).
        std::thread::Builder::new()
            .name("mountd-reader".into())
            .spawn(move || reader_loop(&reader))
            .expect("spawn mountd-reader thread");
        Self { inner }
    }

    /// Allocate a seq, register the reply slot, and send one frame.
    fn send_request(
        &self,
        req: Req,
        fds: &[RawFd],
    ) -> Result<(u32, Receiver<Delivery>), MountdError> {
        let bytes_seq = self.inner.next_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&Request {
            seq: bytes_seq,
            req,
        })?;
        // Rendezvous capacity 1: the reader's send never blocks even if
        // the caller has already timed out and gone away (the buffered
        // slot absorbs the late reply, then the whole channel drops).
        let (tx, rx) = std::sync::mpsc::sync_channel::<Delivery>(1);
        {
            let mut pending = self.inner.pending.lock().ignore_poison();
            let Some(map) = pending.as_mut() else {
                return Err(MountdError::Disconnected(
                    "connection already closed".into(),
                ));
            };
            map.insert(bytes_seq, tx);
        }
        let _send_guard = self.inner.send.lock().ignore_poison();
        if let Err(e) = proto::send_frame(self.inner.sock.as_raw_fd(), &frame, fds) {
            // Send failed — deregister so the slot doesn't leak.
            if let Some(map) = self.inner.pending.lock().ignore_poison().as_mut() {
                map.remove(&bytes_seq);
            }
            return Err(e.into());
        }
        Ok((bytes_seq, rx))
    }

    /// Send `req` (with optional `SCM_RIGHTS` fds) and block for its
    /// reply, at most `timeout`.
    fn call(
        &self,
        req: Req,
        fds: &[RawFd],
        timeout: Duration,
    ) -> Result<(Resp, Vec<OwnedFd>), MountdError> {
        let (seq, rx) = self.send_request(req, fds)?;
        match rx.recv_timeout(timeout) {
            Ok(delivery) => delivery,
            Err(_) => {
                // Timed out (or the reader dropped the sender without a
                // drain message, which cannot happen — the drain always
                // sends). Deregister so a late reply is dropped.
                if let Some(map) = self.inner.pending.lock().ignore_poison().as_mut() {
                    map.remove(&seq);
                }
                Err(MountdError::Timeout(timeout))
            }
        }
    }

    /// `Mount{build_id}`: claim the build id, fuse-mount
    /// `/var/rio/castore/{build_id}`, and receive the `/dev/fuse` fd.
    /// Must be the first request on a connection; exactly one per
    /// connection lifetime. Returns `(staging_quota_bytes, fuse_fd)`.
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
            } => {
                let fd = fds.pop().ok_or_else(|| {
                    MountdError::Disconnected("Mounted reply carried no fuse fd".into())
                })?;
                Ok((staging_quota_bytes, fd))
            }
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `BackingOpen`: register `fd` as a FUSE passthrough backing file
    /// via the daemon's privileged `FUSE_DEV_IOC_BACKING_OPEN` ioctl.
    /// Returns the connection-scoped `backing_id`.
    pub fn backing_open(&self, fd: RawFd, timeout: Duration) -> Result<u32, MountdError> {
        let (resp, _) = self.call(Req::BackingOpen, &[fd], timeout)?;
        match resp {
            Resp::BackingId(id) => Ok(id),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `BackingClose{id}`: release a backing id from a prior
    /// [`Self::backing_open`].
    pub fn backing_close(&self, backing_id: u32, timeout: Duration) -> Result<(), MountdError> {
        let (resp, _) = self.call(Req::BackingClose { backing_id }, &[], timeout)?;
        match resp {
            Resp::Ok => Ok(()),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `Promote{digest}`: ask the daemon to verify-copy
    /// `staging/{build_id}/{hex(digest)}` into the shared backing cache.
    pub fn promote(&self, digest: [u8; 32], timeout: Duration) -> Result<(), MountdError> {
        let (resp, _) = self.call(Req::Promote { digest }, &[], timeout)?;
        match resp {
            Resp::Ok => Ok(()),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }
}

/// Receive frames until the socket dies, routing each reply to its
/// waiting caller by `seq`. On exit, fail every still-pending call so
/// no caller blocks for its full timeout on a connection that is
/// already known dead.
fn reader_loop(inner: &Inner) {
    let reason = loop {
        match proto::recv_frame(inner.sock.as_raw_fd()) {
            Ok(frame) => {
                let reply: Reply = match proto::decode(&frame.bytes) {
                    Ok(r) => r,
                    Err(e) => {
                        // An undecodable frame means the two sides
                        // disagree about the protocol — nothing after
                        // this can be trusted to correlate correctly.
                        break format!("undecodable reply frame: {e}");
                    }
                };
                let waiter = inner
                    .pending
                    .lock()
                    .ignore_poison()
                    .as_mut()
                    .and_then(|map| map.remove(&reply.seq));
                match waiter {
                    // Capacity-1 channel: this send never blocks. If the
                    // caller already timed out and dropped the receiver,
                    // the send fails — fine, the reply is discarded.
                    Some(tx) => {
                        let _ = tx.send(Ok((reply.resp, frame.fds)));
                    }
                    None => {
                        tracing::warn!(
                            seq = reply.seq,
                            "rio-mountd reply for unknown seq (caller timed out?)"
                        );
                    }
                }
            }
            Err(proto::FrameError::Eof) => break "connection closed by rio-mountd".to_string(),
            Err(e) => break format!("recv: {e}"),
        }
    };

    // Mark the connection dead and drain every waiter. Taking the map
    // (Option → None) makes subsequent send_request calls fail fast
    // instead of registering a slot no reader will ever fill.
    let drained = inner.pending.lock().ignore_poison().take();
    if let Some(map) = drained {
        for (_, tx) in map {
            let _ = tx.send(Err(MountdError::Disconnected(reason.clone())));
        }
    }
    tracing::info!(reason, "rio-mountd connection reader exiting");
}

impl Drop for Inner {
    fn drop(&mut self) {
        // Wake the reader thread out of its blocking recvmsg. Closing
        // the fd alone does NOT reliably interrupt a thread already
        // parked in recvmsg(2); shutdown(2) does (the recv returns 0 →
        // Eof). Best-effort: if the socket is already dead this is
        // ENOTCONN, which is fine.
        let _ = nix::sys::socket::shutdown(self.sock.as_raw_fd(), nix::sys::socket::Shutdown::Both);
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use nix::sys::socket::socketpair;

    /// A fake daemon end of the socketpair: receives Requests, replies
    /// with whatever the test scripted for that request's variant.
    fn pair() -> (MountdClient, OwnedFd) {
        let (a, b) = socketpair(
            AddressFamily::Unix,
            SockType::SeqPacket,
            None,
            SockFlag::empty(),
        )
        .expect("socketpair");
        (MountdClient::from_fd(a), b)
    }

    fn recv_request(daemon: &OwnedFd) -> Request {
        let frame = proto::recv_frame(daemon.as_raw_fd()).expect("daemon recv");
        proto::decode(&frame.bytes).expect("daemon decode")
    }

    fn send_reply(daemon: &OwnedFd, reply: &Reply) {
        let bytes = proto::encode(reply).expect("daemon encode");
        proto::send_frame(daemon.as_raw_fd(), &bytes, &[]).expect("daemon send");
    }

    const T: Duration = Duration::from_secs(5);

    #[test]
    fn backing_open_roundtrip() {
        let (client, daemon) = pair();
        let f = tempfile::tempfile().unwrap();
        let raw = f.as_raw_fd();
        let handle = std::thread::spawn(move || client.backing_open(raw, T));
        let req = recv_request(&daemon);
        assert!(matches!(req.req, Req::BackingOpen));
        send_reply(
            &daemon,
            &Reply {
                seq: req.seq,
                resp: Resp::BackingId(42),
            },
        );
        assert_eq!(handle.join().unwrap().unwrap(), 42);
    }

    /// Out-of-order replies route to the right callers: a slow Promote
    /// must not block a concurrent BackingClose, and each reply lands
    /// at the caller whose seq it echoes.
    #[test]
    fn out_of_order_replies_correlate_by_seq() {
        let (client, daemon) = pair();
        let c1 = client.clone();
        let promote = std::thread::spawn(move || c1.promote([0xAB; 32], T));
        let promote_req = recv_request(&daemon);
        assert!(matches!(promote_req.req, Req::Promote { .. }));

        let c2 = client.clone();
        let close = std::thread::spawn(move || c2.backing_close(7, T));
        let close_req = recv_request(&daemon);
        assert!(matches!(close_req.req, Req::BackingClose { backing_id: 7 }));

        // Reply to the SECOND request first.
        send_reply(
            &daemon,
            &Reply {
                seq: close_req.seq,
                resp: Resp::Ok,
            },
        );
        close.join().unwrap().expect("backing_close");
        // The promote is still pending; now complete it with an error.
        send_reply(
            &daemon,
            &Reply {
                seq: promote_req.seq,
                resp: Resp::Err(ErrKind::DigestMismatch),
            },
        );
        let err = promote.join().unwrap().expect_err("promote must fail");
        assert!(matches!(
            err,
            MountdError::Rejected(ErrKind::DigestMismatch)
        ));
        assert!(err.is_build_fatal(), "DigestMismatch is build-fatal");
    }

    /// A call with no reply times out, and the late reply is dropped
    /// without disturbing later calls.
    #[test]
    fn timeout_deregisters_the_pending_slot() {
        let (client, daemon) = pair();
        let err = client
            .backing_close(1, Duration::from_millis(50))
            .expect_err("no reply → timeout");
        assert!(matches!(err, MountdError::Timeout(_)));
        assert!(!err.is_build_fatal(), "timeout is an infra failure");

        // The daemon replies late; the reader drops it (unknown seq).
        let req = recv_request(&daemon);
        send_reply(
            &daemon,
            &Reply {
                seq: req.seq,
                resp: Resp::Ok,
            },
        );

        // A subsequent call still works.
        let c = client.clone();
        let h = std::thread::spawn(move || c.backing_close(2, T));
        let req2 = recv_request(&daemon);
        send_reply(
            &daemon,
            &Reply {
                seq: req2.seq,
                resp: Resp::Ok,
            },
        );
        h.join().unwrap().expect("second call succeeds");
    }

    /// Daemon disconnect fails the in-flight call immediately (not
    /// after the full timeout) and every subsequent call fails fast.
    #[test]
    fn disconnect_drains_pending_and_fails_fast() {
        let (client, daemon) = pair();
        let c = client.clone();
        let inflight = std::thread::spawn(move || c.promote([1; 32], Duration::from_secs(60)));
        let _ = recv_request(&daemon);
        let started = std::time::Instant::now();
        drop(daemon);
        let err = inflight.join().unwrap().expect_err("disconnect");
        assert!(matches!(err, MountdError::Disconnected(_)));
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "in-flight call must fail on disconnect, not wait out its timeout"
        );
        let err = client
            .backing_close(1, T)
            .expect_err("post-disconnect call");
        assert!(matches!(
            err,
            MountdError::Disconnected(_) | MountdError::Send(_)
        ));
    }
}
