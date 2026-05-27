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
//!
//! # Reconnection (mountd restart resilience)
//!
//! The daemon is a per-node DaemonSet; a restart (upgrade, crash,
//! force-delete) kills every UDS connection while the builds it served
//! keep running. A client built by [`MountdClient::connect`] therefore
//! re-establishes its session when a `Promote`/`PromoteChunks` fails
//! because the connection is gone: re-dial the socket, re-issue
//! `Mount{build_id}` (the daemon requires it as the first request on
//! every connection, and a restarted daemon's per-connection state died
//! with the old process — its EEXIST-tolerant staging setup re-adopts
//! the surviving dir), and retry the failed request. Attempts are
//! bounded and backoff-jittered ([`MOUNTD_RECONNECT_ATTEMPTS`],
//! [`MOUNTD_RECONNECT_BACKOFF`], re-Mount capped at
//! [`MOUNTD_RECONNECT_MOUNT_TIMEOUT`]); after an exhausted cycle a
//! cooldown ([`MOUNTD_RECONNECT_COOLDOWN`]) makes further attempts fail
//! fast so a long outage degrades exactly like the pre-reconnect
//! behavior instead of stalling every FUSE callback. A re-dial that
//! reaches the daemon but has its re-`Mount` REJECTED build-fatally
//! (e.g. `Unauthorized` after the mountd key rotated mid-build) aborts
//! the cycle immediately and surfaces that rejection — more re-dials
//! cannot fix an explicit refusal, and the crisp error beats reporting
//! the stale connection loss.
//!
//! `BackingOpen`/`BackingClose` deliberately do NOT enter the cycle
//! ([`OnConnLoss::FailFast`]): their callers already have a cheap
//! per-open degradation (keep-cache reads; a deferred close), and
//! `open()` issues BackingOpen while holding the per-build
//! backing-table lock — paying the backoff schedule there would stall
//! every concurrent open of the build for seconds when the
//! pre-reconnect behavior (instant keep-cache fallback) is perfectly
//! serviceable. A successful Promote-driven reconnect swaps the shared
//! connection, so later opens regain passthrough automatically. Test
//! clients built by [`MountdClient::from_fd`] (socketpairs) have
//! nothing to re-dial and keep the old fail-fast behavior for every
//! request.
// r[impl builder.fs.mountd-reconnect]

use std::collections::HashMap;
use std::os::fd::{AsRawFd, BorrowedFd, OwnedFd, RawFd};
use std::path::{Path, PathBuf};
use std::sync::atomic::{AtomicU32, Ordering};
use std::sync::mpsc::{Receiver, SyncSender};
use std::sync::{Arc, Mutex};
use std::time::{Duration, Instant};

use nix::sys::socket::{AddressFamily, SockFlag, SockType, UnixAddr, connect, socket};

use super::mountd_proto::{self as proto, ErrKind, Reply, Req, Request, Resp};
use crate::IgnorePoison;

/// Re-dial attempts per RPC after a connection-loss failure. Each
/// attempt is one backoff sleep plus one `connect(2)` (fails in
/// microseconds while the daemon is away) plus, on success, one
/// re-`Mount` round-trip capped at [`MOUNTD_RECONNECT_MOUNT_TIMEOUT`].
/// Worst case — a daemon that accepts connections but never replies —
/// is therefore 4 × (≤2.5 s jittered backoff + 5 s re-Mount) ≈ 30 s,
/// about one `mountd_request_timeout`, before the RPC's original error
/// surfaces; the typical restart outage (connection refused) costs only
/// the ≈5.5 s of backoff sleeps.
const MOUNTD_RECONNECT_ATTEMPTS: u32 = 4;

/// Backoff slept before each re-dial attempt: 500 ms → 1 s → 2 s → 2 s
/// ceilings with ±25% jitter (≈5.5 s expected total) — sized to cover a
/// systemd/DaemonSet restart of rio-mountd with margin while staying
/// well inside the 30 s per-request budget the caller already holds.
const MOUNTD_RECONNECT_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(500),
    mult: 2.0,
    cap: Duration::from_secs(2),
    jitter: rio_common::backoff::Jitter::Proportional(0.25),
};

/// Budget for the re-`Mount` issued inside one reconnect attempt. The
/// daemon answers `Mount` inline (mkdir + chown + quota ioctls — no
/// copy/hash), so a healthy daemon replies in milliseconds; capping the
/// wait well below the configured `mountd_request_timeout` keeps the
/// whole bounded cycle (see [`MOUNTD_RECONNECT_ATTEMPTS`]) inside about
/// one request budget even against a daemon that accepts connections
/// but never replies, and bounds how long `reconnect()` can hold the
/// connection slot lock.
const MOUNTD_RECONNECT_MOUNT_TIMEOUT: Duration = Duration::from_secs(5);

/// After a reconnect cycle exhausts every attempt without reaching the
/// daemon, further attempts are skipped (the RPC fails fast with its
/// original error, exactly like the pre-reconnect behavior) until this
/// much time has passed — a long mountd outage must not turn every
/// promote into a multi-second stall when the degraded staged serve
/// handles it just fine. The next eligible RPC after the cooldown
/// re-probes.
const MOUNTD_RECONNECT_COOLDOWN: Duration = Duration::from_secs(15);

/// What [`MountdClient::call`] does when the connection turns out to be
/// gone: re-establish the session, or surface the error immediately.
#[derive(Clone, Copy, PartialEq, Eq)]
enum OnConnLoss {
    /// Re-dial + re-`Mount` + retry, bounded — for `Promote`/
    /// `PromoteChunks`, where giving up costs a re-fetch (and, before
    /// the degrade path, used to cost the build).
    Reconnect,
    /// Surface the error immediately — for `BackingOpen`/`BackingClose`,
    /// whose callers already degrade cheaply (keep-cache reads; a
    /// deferred close) and may hold per-build locks across the call.
    FailFast,
}

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

/// `true` when the error means the connection itself is unusable (the
/// daemon went away or the socket died) — the class of failure a
/// re-dial can fix. Daemon-side rejections, unexpected replies, local
/// encode errors, and timeouts (the request may still be executing
/// daemon-side; re-sending could double-execute it) are NOT in it; nor
/// are send-side errors other than the peer-gone trio (EPIPE,
/// ECONNRESET, ENOTCONN) — an ENOBUFS/EMSGSIZE against a healthy daemon
/// must not trigger a pointless re-dial cycle that ends in the
/// duplicate-uid rejection.
fn is_connection_loss(err: &MountdError) -> bool {
    match err {
        MountdError::Disconnected(_) => true,
        MountdError::Send(proto::FrameError::Io(io)) => matches!(
            io.kind(),
            std::io::ErrorKind::BrokenPipe
                | std::io::ErrorKind::ConnectionReset
                | std::io::ErrorKind::NotConnected
        ),
        _ => false,
    }
}

/// One reply as delivered to a waiting caller: the decoded `Resp` plus
/// any fds that arrived in the datagram's `SCM_RIGHTS` cmsg (no current
/// reply carries one — fds only flow builder → daemon — but the demux
/// keeps them owned so a misbehaving daemon cannot leak fds into us).
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

/// One established connection: the socket, its reply-demux reader
/// thread, and the in-flight call registry. Connections are immutable
/// once made — a reconnect builds a fresh `Conn` and swaps it into the
/// client's shared slot rather than re-dialing in place.
///
/// The reader thread holds an `Arc<Inner>` directly (NOT an
/// `Arc<Conn>`). The two refcounts are deliberately separate: the
/// reader must keep the socket and the pending map alive while it
/// drains, but its own liveness must not keep the *connection* alive —
/// otherwise dropping every client handle would leave the reader
/// parked in `recvmsg` holding the last strong reference forever, the
/// socket would never shut down, and the daemon would never see the
/// EOF that triggers its conn-drop teardown (reap the staging dir,
/// release the build_id/uid claims).
struct Conn {
    shared: Arc<Inner>,
    /// Joined on drop, after the shutdown that guarantees it exits.
    /// `Option` so `Drop` can `take()` it.
    reader: Option<std::thread::JoinHandle<()>>,
}

impl Drop for Conn {
    /// Runs when the last reference drops (the last client handle, or a
    /// reconnect swapping in a replacement). Shuts the socket down —
    /// the daemon's `recvmsg` returns EOF (its cue to tear the build
    /// down) and the reader thread's blocked `recvmsg` returns EOF
    /// (closing the fd alone does NOT reliably interrupt a thread
    /// already parked in `recvmsg(2)`; `shutdown(2)` does) — then joins
    /// the reader so no thread outlives the connection. The join is
    /// bounded: post-shutdown the reader's next `recvmsg` returns
    /// immediately and its drain path only sends on never-blocking
    /// capacity-1 channels.
    fn drop(&mut self) {
        let _ = nix::sys::socket::shutdown(
            self.shared.sock.as_raw_fd(),
            nix::sys::socket::Shutdown::Both,
        );
        if let Some(reader) = self.reader.take()
            && let Err(e) = reader.join()
        {
            tracing::error!(?e, "mountd-reader thread panicked");
        }
    }
}

impl Conn {
    /// Wrap an already-connected `SOCK_SEQPACKET` fd and start its
    /// reply-demux reader thread.
    fn new(sock: OwnedFd) -> Self {
        let shared = Arc::new(Inner {
            sock,
            send: Mutex::new(()),
            pending: Mutex::new(Some(HashMap::new())),
            next_seq: AtomicU32::new(1),
        });
        let reader_shared = Arc::clone(&shared);
        // The reader holds its own strong Arc<Inner> (NOT an Arc<Conn>)
        // so the socket and pending map outlive its drain path without
        // its liveness keeping the connection open. It exits when
        // recv_frame returns Eof/Err — on daemon-side close, or on the
        // shutdown(2) issued by Conn::drop when the connection is
        // released.
        let reader = std::thread::Builder::new()
            .name("mountd-reader".into())
            .spawn(move || reader_loop(&reader_shared))
            .expect("spawn mountd-reader thread");
        Self {
            shared,
            reader: Some(reader),
        }
    }

    /// Allocate a seq, register the reply slot, and send one frame.
    ///
    /// TODO: `send_frame` runs on the blocking socket with no deadline —
    /// it is the one mountd interaction not bounded by
    /// `mountd_request_timeout` (the caller's `recv_timeout` only starts
    /// after the send returns). A wedged daemon that stops draining the
    /// socket buffer can therefore park a FUSE thread in `sendmsg`
    /// indefinitely; bound it with `SO_SNDTIMEO` (or poll-with-deadline)
    /// if that ever shows up in practice.
    fn send_request(
        &self,
        req: &Req,
        fds: &[RawFd],
    ) -> Result<(u32, Receiver<Delivery>), MountdError> {
        let seq = self.shared.next_seq.fetch_add(1, Ordering::Relaxed);
        let frame = proto::encode(&Request {
            seq,
            req: req.clone(),
        })?;
        // Rendezvous capacity 1: the reader's send never blocks even if
        // the caller has already timed out and gone away (the buffered
        // slot absorbs the late reply, then the whole channel drops).
        let (tx, rx) = std::sync::mpsc::sync_channel::<Delivery>(1);
        {
            let mut pending = self.shared.pending.lock().ignore_poison();
            let Some(map) = pending.as_mut() else {
                return Err(MountdError::Disconnected(
                    "connection already closed".into(),
                ));
            };
            map.insert(seq, tx);
        }
        let _send_guard = self.shared.send.lock().ignore_poison();
        if let Err(e) = proto::send_frame(self.shared.sock.as_raw_fd(), &frame, fds) {
            // Send failed — deregister so the slot doesn't leak.
            if let Some(map) = self.shared.pending.lock().ignore_poison().as_mut() {
                map.remove(&seq);
            }
            return Err(e.into());
        }
        Ok((seq, rx))
    }

    /// Send `req` (with optional `SCM_RIGHTS` fds) and block for its
    /// reply, at most `timeout`.
    fn call(
        &self,
        req: &Req,
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
                if let Some(map) = self.shared.pending.lock().ignore_poison().as_mut() {
                    map.remove(&seq);
                }
                Err(MountdError::Timeout(timeout))
            }
        }
    }

    /// `Mount{build_id, token}` as the first request on this
    /// connection, handing the daemon a dup of `fuse_fd` via
    /// `SCM_RIGHTS`. `token` is the scheduler-minted Mount-admission
    /// credential (`WorkAssignment.mountd_token`); `None` when the
    /// deployment mints none (gid-admitted standalone path).
    fn mount(
        &self,
        build_id: &str,
        token: Option<&str>,
        fuse_fd: RawFd,
        timeout: Duration,
    ) -> Result<u64, MountdError> {
        let (resp, _) = self.call(
            &Req::Mount {
                build_id: build_id.to_owned(),
                token: token.map(str::to_owned),
            },
            &[fuse_fd],
            timeout,
        )?;
        match resp {
            Resp::Mounted {
                staging_quota_bytes,
            } => Ok(staging_quota_bytes),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }
}

/// What a fresh connection needs to become usable again after a daemon
/// restart: the protocol requires `Mount{build_id}` (carrying this
/// build's `/dev/fuse` fd) as the first request on every connection.
/// Captured by the first successful [`MountdClient::mount`].
struct MountSession {
    build_id: String,
    /// The Mount-admission token the original `Mount` presented
    /// (ADR-022 §P0559), retained so the re-`Mount` on a restarted
    /// daemon presents the same credential — its TTL covers the whole
    /// build window precisely so this mid-build re-issue stays valid.
    /// `None` when the deployment mints none.
    token: Option<String>,
    /// A dup of the build's own `/dev/fuse` fd, held so a re-`Mount`
    /// can hand the restarted daemon a fresh `SCM_RIGHTS` copy (its
    /// previous dup died with the old process). Closing this dup at
    /// client drop is inert — the fuse connection's lifetime is owned
    /// by the fuser session and the mount, not by this fd.
    fuse_fd: OwnedFd,
    /// The per-request budget the original `Mount` used; the re-`Mount`
    /// reuses it.
    mount_timeout: Duration,
}

/// State shared by every clone of one [`MountdClient`].
struct ClientShared {
    /// The live connection. Swapped wholesale by a successful
    /// reconnect; callers clone the `Arc` out and run their RPC on that
    /// snapshot, so an in-flight call on the old connection and the
    /// swap never race on the socket itself.
    conn: Mutex<Arc<Conn>>,
    /// Where [`MountdClient::connect`] dialed, for re-dialing. `None`
    /// for [`MountdClient::from_fd`] clients (tests, socketpairs) —
    /// they cannot reconnect.
    socket_path: Option<PathBuf>,
    /// Mount parameters for re-establishing the session on a fresh
    /// connection. `None` until the first successful `mount()` (and
    /// forever for clients that never mount — nothing to re-establish).
    session: Mutex<Option<MountSession>>,
    /// Set when a reconnect cycle exhausted every attempt; until it
    /// expires, RPCs skip reconnection and fail fast with their
    /// original error.
    reconnect_cooldown_until: Mutex<Option<Instant>>,
}

/// Handle to one mountd connection. Cheap to clone; the underlying
/// socket and reader thread are shared.
///
/// Dropping the last handle disconnects: the socket is shut down, the
/// reader thread is joined, and the daemon observes EOF — which is its
/// signal to reap the build's staging dir and release its build_id/uid
/// claims. There is deliberately no force-close that yanks the socket out
/// from under live clones: the FUSE callbacks hold a clone for
/// `BackingOpen`/`BackingClose`/`Promote`, and the connection must
/// outlive the last of them.
#[derive(Clone)]
pub struct MountdClient {
    inner: Arc<ClientShared>,
}

impl MountdClient {
    /// Connect to the daemon's socket and start the reply-demux reader
    /// thread. The socket path is remembered so a later daemon restart
    /// can be survived by re-dialing it (see the module docs).
    pub fn connect(socket_path: &Path) -> std::io::Result<Self> {
        let conn = Self::dial(socket_path)?;
        Ok(Self {
            inner: Arc::new(ClientShared {
                conn: Mutex::new(Arc::new(conn)),
                socket_path: Some(socket_path.to_path_buf()),
                session: Mutex::new(None),
                reconnect_cooldown_until: Mutex::new(None),
            }),
        })
    }

    /// Wrap an already-connected `SOCK_SEQPACKET` fd (tests use a
    /// `socketpair` half). Such a client has no socket path to re-dial,
    /// so a lost connection stays lost (the pre-reconnect fail-fast
    /// behavior).
    pub fn from_fd(sock: OwnedFd) -> Self {
        Self {
            inner: Arc::new(ClientShared {
                conn: Mutex::new(Arc::new(Conn::new(sock))),
                socket_path: None,
                session: Mutex::new(None),
                reconnect_cooldown_until: Mutex::new(None),
            }),
        }
    }

    /// One `connect(2)` to the daemon's `SOCK_SEQPACKET` socket.
    fn dial(socket_path: &Path) -> std::io::Result<Conn> {
        let sock = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )?;
        let addr = UnixAddr::new(socket_path)?;
        connect(sock.as_raw_fd(), &addr)?;
        Ok(Conn::new(sock))
    }

    /// Snapshot the current connection.
    fn current_conn(&self) -> Arc<Conn> {
        Arc::clone(&self.inner.conn.lock().ignore_poison())
    }

    /// Whether a reconnect may be attempted right now: the client knows
    /// where to re-dial, has a completed `Mount` to re-establish, and is
    /// not inside the post-exhaustion cooldown.
    fn reconnect_possible(&self) -> bool {
        if self.inner.socket_path.is_none() {
            return false;
        }
        if self.inner.session.lock().ignore_poison().is_none() {
            return false;
        }
        let cooldown = self.inner.reconnect_cooldown_until.lock().ignore_poison();
        match *cooldown {
            Some(until) => Instant::now() >= until,
            None => true,
        }
    }

    /// Mark a fully-exhausted reconnect cycle so subsequent RPCs fail
    /// fast (instead of each re-paying the backoff schedule) until the
    /// cooldown expires.
    fn begin_reconnect_cooldown(&self) {
        *self.inner.reconnect_cooldown_until.lock().ignore_poison() =
            Some(Instant::now() + MOUNTD_RECONNECT_COOLDOWN);
    }

    /// A reconnect succeeded — clear any cooldown so later failures get
    /// a full retry budget again.
    fn clear_reconnect_cooldown(&self) {
        *self.inner.reconnect_cooldown_until.lock().ignore_poison() = None;
    }

    /// Re-establish the session after `failed` died: dial the socket,
    /// issue `Mount{build_id}` (with the kept `/dev/fuse` dup) as the
    /// new connection's first request, and swap it in. If another
    /// thread already swapped a fresh connection in, adopt that one
    /// without dialing.
    ///
    /// The restarted daemon accepts the re-`Mount` for the surviving
    /// staging dir: its staging setup tolerates the existing directory
    /// (EEXIST), re-chowns it to the same peer uid, and re-applies the
    /// staging quota to the project id the dir already carries, while
    /// the builder's flock'd `.rio-live` sentinel keeps the startup
    /// orphan scan and the disk-pressure sweep away from the dir in the
    /// meantime.
    ///
    /// The sentinel itself is NOT re-planted here. The recreated-without-
    /// a-sentinel case only arises when THIS daemon (no restart) tore
    /// the old connection down on a socket-level error and deleted the
    /// dir — at which point the staged content is already gone and the
    /// only cost of the missing sentinel is that a *later* restart's
    /// scan may reap the recreated dir, sending the affected fill down
    /// the input-EIO → infra-retry path (the pre-sentinel status quo,
    /// not data loss). Re-planting would require this client to know
    /// the staging path, which it deliberately does not.
    fn reconnect(&self, failed: &Arc<Conn>) -> Result<Arc<Conn>, MountdError> {
        let Some(socket_path) = self.inner.socket_path.as_deref() else {
            return Err(MountdError::Disconnected(
                "client has no socket path to re-dial".into(),
            ));
        };
        let session = self.inner.session.lock().ignore_poison();
        let Some(session) = session.as_ref() else {
            return Err(MountdError::Disconnected(
                "no completed Mount to re-establish".into(),
            ));
        };
        let mut current = self.inner.conn.lock().ignore_poison();
        if !Arc::ptr_eq(&current, failed) {
            // Someone else already reconnected; use their connection.
            return Ok(Arc::clone(&current));
        }
        let outcome = (|| {
            let conn = Self::dial(socket_path).map_err(|e| {
                MountdError::Disconnected(format!("re-dial {} failed: {e}", socket_path.display()))
            })?;
            let conn = Arc::new(conn);
            // The capped timeout (not the original Mount's): a healthy
            // daemon answers Mount in milliseconds, and this round-trip
            // both holds the connection-slot lock and multiplies into
            // the worst-case cycle bound. The retained token rides
            // along — a token-admitted build must re-authenticate its
            // re-Mount the same way it authenticated the first one.
            conn.mount(
                &session.build_id,
                session.token.as_deref(),
                session.fuse_fd.as_raw_fd(),
                session.mount_timeout.min(MOUNTD_RECONNECT_MOUNT_TIMEOUT),
            )?;
            Ok(conn)
        })();
        match outcome {
            Ok(conn) => {
                metrics::counter!(
                    "rio_builder_castore_fuse_mountd_reconnect_total",
                    "outcome" => "ok"
                )
                .increment(1);
                tracing::info!(
                    build_id = %session.build_id,
                    socket = %socket_path.display(),
                    "re-established the rio-mountd session after a connection loss"
                );
                // The previous (dead) connection is released here: its
                // Drop shuts the socket down so a still-living daemon
                // observes EOF for it promptly.
                *current = Arc::clone(&conn);
                Ok(conn)
            }
            Err(e) => {
                metrics::counter!(
                    "rio_builder_castore_fuse_mountd_reconnect_total",
                    "outcome" => "error"
                )
                .increment(1);
                Err(e)
            }
        }
    }

    /// Send `req` and block for its reply. When the connection turns
    /// out to be gone, either re-dial + re-Mount + retry (bounded,
    /// backoff-jittered — `OnConnLoss::Reconnect`) or surface the error
    /// immediately (`OnConnLoss::FailFast`). Non-connection failures
    /// (rejections, timeouts) surface unchanged either way.
    fn call(
        &self,
        req: &Req,
        fds: &[RawFd],
        timeout: Duration,
        on_loss: OnConnLoss,
    ) -> Result<(Resp, Vec<OwnedFd>), MountdError> {
        let mut conn = self.current_conn();
        let mut redials = 0u32;
        loop {
            let err = match conn.call(req, fds, timeout) {
                Ok(ok) => return Ok(ok),
                Err(e) => e,
            };
            if !is_connection_loss(&err) || on_loss == OnConnLoss::FailFast {
                return Err(err);
            }
            if redials >= MOUNTD_RECONNECT_ATTEMPTS {
                // A whole cycle of re-dials could not reach the daemon:
                // remember that so concurrent/subsequent RPCs fail fast
                // instead of each re-paying the backoff schedule.
                self.begin_reconnect_cooldown();
                return Err(err);
            }
            if !self.reconnect_possible() {
                return Err(err);
            }
            std::thread::sleep(MOUNTD_RECONNECT_BACKOFF.duration(redials));
            redials += 1;
            match self.reconnect(&conn) {
                Ok(fresh) => {
                    self.clear_reconnect_cooldown();
                    conn = fresh;
                }
                // The daemon was reachable but REJECTED the re-Mount
                // build-fatally (Unauthorized after the mountd key
                // rotated mid-build, BadBuildId, ...): more re-dials
                // cannot fix an explicit refusal, so abort the cycle
                // and surface the rejection instead of burning the rest
                // of the budget and reporting a stale connection loss.
                // Deliberately NO cooldown — the daemon is up, and
                // later RPCs should keep surfacing the same crisp
                // rejection (each pays one short re-dial, never the
                // exhausted-cycle schedule).
                Err(re_err) if matches!(&re_err, MountdError::Rejected(k) if k.is_build_fatal()) => {
                    return Err(re_err);
                }
                Err(re_err) => {
                    tracing::warn!(
                        error = %re_err,
                        redials,
                        max = MOUNTD_RECONNECT_ATTEMPTS,
                        "rio-mountd re-dial failed; retrying within the bounded budget"
                    );
                    // Keep the dead connection: the next loop iteration
                    // fails fast on it and lands back here until the
                    // attempts run out.
                }
            }
        }
    }

    /// `Mount{build_id, token}`: claim the build id, hand the daemon a
    /// dup of `fuse_fd` (this build's own `/dev/fuse`, which the caller
    /// mounts itself — see [`super::mount::mount_castore_background`])
    /// so it can broker `BackingOpen`/`BackingClose` against that
    /// connection, and have the daemon set up this build's staging dir
    /// and quota. `token` is the scheduler-minted Mount-admission
    /// credential from `WorkAssignment.mountd_token` (`None` = none
    /// minted; the daemon then admits by peer gid). Must be the first
    /// request on a connection; exactly one per connection lifetime.
    /// Returns `staging_quota_bytes`.
    ///
    /// On success the client keeps `build_id`, the token, and a dup of
    /// `fuse_fd` so a daemon restart can be survived by re-issuing the
    /// same `Mount` on a fresh connection (see the module docs).
    pub fn mount(
        &self,
        build_id: &str,
        token: Option<&str>,
        fuse_fd: RawFd,
        timeout: Duration,
    ) -> Result<u64, MountdError> {
        let conn = self.current_conn();
        let quota = conn.mount(build_id, token, fuse_fd, timeout)?;
        // Keep what a re-Mount needs. Best-effort: if the dup fails the
        // client simply cannot reconnect (the pre-reconnect behavior).
        // SAFETY: `fuse_fd` is a live fd owned by the caller for the
        // duration of this call; `try_clone_to_owned` dups it into an
        // independently-owned descriptor before we return.
        match unsafe { BorrowedFd::borrow_raw(fuse_fd) }.try_clone_to_owned() {
            Ok(dup) => {
                *self.inner.session.lock().ignore_poison() = Some(MountSession {
                    build_id: build_id.to_owned(),
                    token: token.map(str::to_owned),
                    fuse_fd: dup,
                    mount_timeout: timeout,
                });
            }
            Err(e) => {
                tracing::warn!(
                    build_id,
                    error = %e,
                    "could not dup the /dev/fuse fd; this build's mountd session will not \
                     survive a daemon restart"
                );
            }
        }
        Ok(quota)
    }

    /// `BackingOpen`: register `fd` as a FUSE passthrough backing file
    /// via the daemon's privileged `FUSE_DEV_IOC_BACKING_OPEN` ioctl.
    /// Returns the connection-scoped `backing_id`.
    ///
    /// Fails fast on connection loss (no reconnect cycle): the caller
    /// degrades the open to keep-cache reads on the spot, and it may be
    /// holding the per-build backing-table lock across this call.
    pub fn backing_open(&self, fd: RawFd, timeout: Duration) -> Result<u32, MountdError> {
        let (resp, _) = self.call(&Req::BackingOpen, &[fd], timeout, OnConnLoss::FailFast)?;
        match resp {
            Resp::BackingId(id) => Ok(id),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `BackingClose{id}`: release a backing id from a prior
    /// [`Self::backing_open`]. Fails fast on connection loss — the
    /// close is best-effort (the kernel-side registration dies with the
    /// connection anyway).
    pub fn backing_close(&self, backing_id: u32, timeout: Duration) -> Result<(), MountdError> {
        let (resp, _) = self.call(
            &Req::BackingClose { backing_id },
            &[],
            timeout,
            OnConnLoss::FailFast,
        )?;
        match resp {
            Resp::Ok => Ok(()),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `Promote{digest}`: ask the daemon to verify-copy
    /// `staging/{build_id}/{hex(digest)}` into the shared backing cache.
    /// Survives a daemon restart via the bounded reconnect cycle (see
    /// the module docs).
    pub fn promote(&self, digest: [u8; 32], timeout: Duration) -> Result<(), MountdError> {
        let (resp, _) = self.call(
            &Req::Promote { digest },
            &[],
            timeout,
            OnConnLoss::Reconnect,
        )?;
        match resp {
            Resp::Ok => Ok(()),
            Resp::Err(kind) => Err(MountdError::Rejected(kind)),
            other => Err(MountdError::UnexpectedReply(other)),
        }
    }

    /// `PromoteChunks{digests}`: ask the daemon to verify-copy each
    /// `staging/{build_id}/chunks/{hex}` into the shared chunk cache.
    /// Used by the P0575 streaming fill so other builds on this node
    /// can source those chunks locally; the caller's own assembly never
    /// depends on the outcome. Survives a daemon restart via the
    /// bounded reconnect cycle (it runs on the fill thread, holds no
    /// per-build locks, and a successful reconnect heals the session
    /// for the fill's eventual whole-file `Promote`).
    pub fn promote_chunks(
        &self,
        chunk_digests: Vec<[u8; 32]>,
        timeout: Duration,
    ) -> Result<(), MountdError> {
        let (resp, _) = self.call(
            &Req::PromoteChunks { chunk_digests },
            &[],
            timeout,
            OnConnLoss::Reconnect,
        )?;
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

// No `Drop for Inner`: by the time the last `Arc<Inner>` drops, the
// socket has already been shut down (by `Conn::drop`) or closed by the
// daemon, and the reader has exited. `OwnedFd`'s own drop closes the fd.

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

    /// Dropping the last client handle disconnects: the daemon observes
    /// EOF (its cue to reap the build's staging dir and release its
    /// claims) and the reader thread exits and releases its
    /// resources. A surviving clone keeps the connection open — only
    /// the LAST drop disconnects. This is the contract P0560's whole
    /// teardown path hangs off; a reader thread holding a strong
    /// reference to the connection would break it (the socket would
    /// never shut down and every build would leak its mount until the
    /// builder process exited).
    #[test]
    fn dropping_the_last_handle_disconnects_the_daemon() {
        let (client, daemon) = pair();
        // Observes the reader thread's exit: the reader holds the only
        // other strong `Arc<Inner>`, so a strong count of zero proves
        // it returned (and that the socket fd was released).
        let inner = Arc::downgrade(&client.current_conn().shared);

        // A live clone keeps the connection open after the original
        // handle drops.
        let survivor = client.clone();
        drop(client);
        let c = survivor.clone();
        let call = std::thread::spawn(move || c.backing_close(1, T));
        let req = recv_request(&daemon);
        send_reply(
            &daemon,
            &Reply {
                seq: req.seq,
                resp: Resp::Ok,
            },
        );
        call.join()
            .unwrap()
            .expect("connection still live while any handle survives");

        // Dropping the last handle shuts the socket down and joins the
        // reader before returning, so both effects are observable
        // immediately — no polling, no sleeps.
        drop(survivor);
        assert_eq!(
            inner.strong_count(),
            0,
            "the reader thread must exit and release the connection state"
        );
        assert!(
            matches!(
                proto::recv_frame(daemon.as_raw_fd()),
                Err(proto::FrameError::Eof)
            ),
            "the daemon must observe EOF when the last client handle drops"
        );
    }

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
    /// `from_fd` clients have no socket path, so the reconnect machinery
    /// never engages for them — this is the pre-reconnect contract,
    /// unchanged.
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

    // ─── Reconnect-after-restart coverage ──────────────────────────────
    //
    // These tests run a minimal scripted daemon on a real UDS path (the
    // socketpair fakes above cannot be re-dialed), kill it, and assert
    // the client's bounded re-dial + re-Mount + retry behavior.

    /// Everything one scripted daemon incarnation observed.
    #[derive(Default)]
    struct DaemonLog {
        /// Request names in arrival order ("mount:<build_id>",
        /// "promote", ...).
        requests: Mutex<Vec<String>>,
        /// Whether each Mount frame carried at least one SCM_RIGHTS fd.
        mount_had_fd: Mutex<Vec<bool>>,
        /// The token each Mount frame carried (None = token-less).
        mount_tokens: Mutex<Vec<Option<String>>>,
        /// When set, every `Mount` is answered with this rejection
        /// instead of `Mounted{0}` — the "daemon up but refusing"
        /// incarnation (key rotated, admission revoked).
        mount_reject: Mutex<Option<ErrKind>>,
        /// The most recently accepted connection, so the test can kill
        /// this incarnation (shutdown → the client sees EOF, the serve
        /// thread exits) the way a real daemon restart does.
        conn: Mutex<Option<Arc<OwnedFd>>>,
    }

    impl DaemonLog {
        /// Kill this daemon incarnation: the client observes EOF on its
        /// connection and the serve thread exits.
        fn kill(&self) {
            if let Some(conn) = self.conn.lock().unwrap().as_ref() {
                let _ =
                    nix::sys::socket::shutdown(conn.as_raw_fd(), nix::sys::socket::Shutdown::Both);
            }
        }
    }

    /// Bind a `SOCK_SEQPACKET` listener at `path`.
    fn bind_listener(path: &Path) -> OwnedFd {
        use nix::sys::socket::{Backlog, bind, listen};
        let _ = std::fs::remove_file(path);
        let fd = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::SOCK_CLOEXEC,
            None,
        )
        .expect("listener socket");
        bind(fd.as_raw_fd(), &UnixAddr::new(path).expect("addr")).expect("bind");
        listen(&fd, Backlog::new(8).expect("backlog")).expect("listen");
        fd
    }

    /// Accept ONE connection on `listener` and serve it until EOF:
    /// `Mount` → `Mounted{0}` (or `log.mount_reject`),
    /// `Promote`/`BackingClose` → `Ok`, `BackingOpen` → `BackingId(1)`.
    /// Every request is recorded in `log`.
    fn serve_one_connection(listener: OwnedFd, log: Arc<DaemonLog>) -> std::thread::JoinHandle<()> {
        serve_connections(listener, log, 1)
    }

    /// [`serve_one_connection`] generalized to `conns` sequential
    /// connections — for tests where the client legitimately re-dials
    /// the same incarnation more than once (e.g. each RPC's reconnect
    /// gets its re-Mount rejected and drops the fresh connection).
    fn serve_connections(
        listener: OwnedFd,
        log: Arc<DaemonLog>,
        conns: usize,
    ) -> std::thread::JoinHandle<()> {
        use nix::sys::socket::accept4;
        std::thread::spawn(move || {
            for _ in 0..conns {
                let conn = match accept4(listener.as_raw_fd(), SockFlag::SOCK_CLOEXEC) {
                    // SAFETY: accept4 just returned a fresh fd owned by
                    // nobody else.
                    Ok(raw) => {
                        Arc::new(unsafe { <OwnedFd as std::os::fd::FromRawFd>::from_raw_fd(raw) })
                    }
                    Err(_) => return,
                };
                *log.conn.lock().unwrap() = Some(Arc::clone(&conn));
                loop {
                    let frame = match proto::recv_frame(conn.as_raw_fd()) {
                        Ok(f) => f,
                        Err(_) => break,
                    };
                    let req: Request = match proto::decode(&frame.bytes) {
                        Ok(r) => r,
                        Err(_) => break,
                    };
                    let resp = match &req.req {
                        Req::Mount { build_id, token } => {
                            log.requests
                                .lock()
                                .unwrap()
                                .push(format!("mount:{build_id}"));
                            log.mount_had_fd.lock().unwrap().push(!frame.fds.is_empty());
                            log.mount_tokens.lock().unwrap().push(token.clone());
                            match log.mount_reject.lock().unwrap().clone() {
                                Some(kind) => Resp::Err(kind),
                                None => Resp::Mounted {
                                    staging_quota_bytes: 0,
                                },
                            }
                        }
                        Req::Promote { .. } => {
                            log.requests.lock().unwrap().push("promote".into());
                            Resp::Ok
                        }
                        Req::BackingOpen => {
                            log.requests.lock().unwrap().push("backing_open".into());
                            Resp::BackingId(1)
                        }
                        Req::BackingClose { .. } => {
                            log.requests.lock().unwrap().push("backing_close".into());
                            Resp::Ok
                        }
                        Req::PromoteChunks { .. } => {
                            log.requests.lock().unwrap().push("promote_chunks".into());
                            Resp::Ok
                        }
                    };
                    let bytes = proto::encode(&Reply { seq: req.seq, resp }).expect("encode");
                    if proto::send_frame(conn.as_raw_fd(), &bytes, &[]).is_err() {
                        break;
                    }
                }
            }
        })
    }

    /// The headline reconnect contract: an RPC that fails because the
    /// daemon went away re-dials the socket, re-issues `Mount{build_id}`
    /// (with the kept /dev/fuse dup AND the kept admission token) as
    /// the new connection's first request, and then retries the
    /// original RPC — so a Promote issued across a daemon restart
    /// succeeds instead of surfacing EIO.
    // r[verify builder.fs.mountd-reconnect]
    // r[verify builder.mountd.token-admission]
    #[test]
    fn promote_across_a_daemon_restart_redials_and_remounts() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");

        // First incarnation: serve the initial Mount, then die.
        let log1 = Arc::new(DaemonLog::default());
        let daemon1 = serve_one_connection(bind_listener(&sock), Arc::clone(&log1));

        let client = MountdClient::connect(&sock).expect("connect to incarnation 1");
        let fuse_stub = tempfile::tempfile().unwrap();
        client
            .mount(
                "b-restart",
                Some("mountd-token-claims.sig"),
                fuse_stub.as_raw_fd(),
                T,
            )
            .expect("initial mount");
        assert_eq!(
            log1.requests.lock().unwrap().as_slice(),
            ["mount:b-restart"]
        );
        assert_eq!(
            log1.mount_tokens.lock().unwrap().as_slice(),
            [Some("mountd-token-claims.sig".to_string())],
            "the initial Mount must carry the admission token"
        );

        // Kill incarnation 1: unlink the socket so dials fail exactly
        // like they do while the real daemon is restarting, and shut its
        // accepted connection down so the client observes EOF and the
        // serve thread exits.
        std::fs::remove_file(&sock).unwrap();
        log1.kill();
        daemon1.join().unwrap();
        // The second incarnation binds the same path immediately — the
        // client's backoff (≥375 ms before the first re-dial) means it
        // is listening well before the first reconnect attempt.
        let log2 = Arc::new(DaemonLog::default());
        let listener2 = bind_listener(&sock);
        let daemon2 = serve_one_connection(listener2, Arc::clone(&log2));

        let started = std::time::Instant::now();
        client
            .promote([0xCD; 32], T)
            .expect("promote must survive the daemon restart via reconnect");
        assert!(
            started.elapsed() < Duration::from_secs(30),
            "the reconnect cycle is bounded"
        );

        let seen = log2.requests.lock().unwrap().clone();
        assert_eq!(
            seen,
            ["mount:b-restart", "promote"],
            "the new connection must re-Mount the same build_id before retrying the RPC"
        );
        assert!(
            log2.mount_had_fd.lock().unwrap().iter().all(|had| *had),
            "the re-Mount must carry the kept /dev/fuse dup in SCM_RIGHTS"
        );
        assert_eq!(
            log2.mount_tokens.lock().unwrap().as_slice(),
            [Some("mountd-token-claims.sig".to_string())],
            "the re-Mount must present the same admission token as the original Mount"
        );
        drop(client);
        daemon2.join().unwrap();
    }

    /// When the daemon never comes back the re-dial attempts are
    /// bounded (the call returns an error rather than hanging), and a
    /// fully-exhausted cycle starts the cooldown so the NEXT call fails
    /// fast instead of re-paying the whole backoff schedule.
    // r[verify builder.fs.mountd-reconnect]
    #[test]
    fn reconnect_attempts_are_bounded_and_cooldown_fails_fast() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");
        let log = Arc::new(DaemonLog::default());
        let daemon = serve_one_connection(bind_listener(&sock), Arc::clone(&log));

        let client = MountdClient::connect(&sock).expect("connect");
        let fuse_stub = tempfile::tempfile().unwrap();
        client
            .mount("b-gone", None, fuse_stub.as_raw_fd(), T)
            .expect("initial mount");

        // The daemon dies and never returns.
        std::fs::remove_file(&sock).unwrap();
        log.kill();
        daemon.join().unwrap();

        let first_started = std::time::Instant::now();
        let err = client
            .promote([0x11; 32], T)
            .expect_err("no daemon → the bounded cycle must give up");
        let first_elapsed = first_started.elapsed();
        assert!(matches!(err, MountdError::Disconnected(_)), "got {err:?}");
        assert!(
            !err.is_build_fatal(),
            "connection loss stays an infra failure"
        );
        assert!(
            first_elapsed < Duration::from_secs(30),
            "the re-dial cycle must stay bounded (took {first_elapsed:?})"
        );

        // Second call inside the cooldown: no backoff schedule, fails
        // fast. The full cycle sleeps ≥4 s even at the jitter floor, so
        // a 2 s ceiling cleanly separates "skipped the schedule" from
        // "paid it again" while leaving slack for a loaded builder.
        let second_started = std::time::Instant::now();
        let err = client.promote([0x22; 32], T).expect_err("still no daemon");
        assert!(matches!(
            err,
            MountdError::Disconnected(_) | MountdError::Send(_)
        ));
        assert!(
            second_started.elapsed() < Duration::from_secs(2),
            "inside the cooldown the call must fail fast, not re-pay the backoff \
             (took {:?})",
            second_started.elapsed()
        );
    }

    /// A reconnect that reaches the daemon but has its re-Mount
    /// REJECTED build-fatally (the admission credential no longer
    /// verifies — e.g. the mountd key rotated mid-build) must abort the
    /// cycle at that first rejection and surface it: more re-dials
    /// cannot fix an explicit refusal, and the crisp `Unauthorized`
    /// beats reporting a stale connection loss after the full budget.
    /// No cooldown is entered — a later RPC re-probes and surfaces the
    /// same rejection again instead of a masked fail-fast error.
    // r[verify builder.fs.mountd-reconnect]
    // r[verify builder.mountd.token-admission]
    #[test]
    fn reconnect_aborts_on_build_fatal_mount_rejection() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");

        // Incarnation 1: healthy — serves the initial Mount, then dies.
        let log1 = Arc::new(DaemonLog::default());
        let daemon1 = serve_one_connection(bind_listener(&sock), Arc::clone(&log1));
        let client = MountdClient::connect(&sock).expect("connect");
        let fuse_stub = tempfile::tempfile().unwrap();
        client
            .mount("b-revoked", Some("stale-token"), fuse_stub.as_raw_fd(), T)
            .expect("initial mount");
        std::fs::remove_file(&sock).unwrap();
        log1.kill();
        daemon1.join().unwrap();

        // Incarnation 2: up, but rejects every Mount as Unauthorized.
        // Two connections — one per promote attempt below.
        let log2 = Arc::new(DaemonLog::default());
        *log2.mount_reject.lock().unwrap() = Some(ErrKind::Unauthorized);
        let daemon2 = serve_connections(bind_listener(&sock), Arc::clone(&log2), 2);

        // First RPC: the cycle aborts at the FIRST rejected re-Mount —
        // exactly one Mount reaches the daemon (no multi-attempt burn)
        // and the caller sees the rejection, not a connection-loss
        // error.
        let err = client
            .promote([0xAA; 32], T)
            .expect_err("rejected re-Mount must fail the RPC");
        assert!(
            matches!(&err, MountdError::Rejected(ErrKind::Unauthorized)),
            "got {err:?}"
        );
        assert!(err.is_build_fatal());
        assert_eq!(
            log2.requests.lock().unwrap().as_slice(),
            ["mount:b-revoked"],
            "the cycle must stop at the first rejected re-Mount"
        );

        // Second RPC: no cooldown was entered, so it re-probes the
        // daemon and surfaces the same rejection (a cooldown would fail
        // fast with the stale connection-loss error instead).
        let err = client.promote([0xBB; 32], T).expect_err("still rejected");
        assert!(
            matches!(&err, MountdError::Rejected(ErrKind::Unauthorized)),
            "got {err:?}"
        );
        assert_eq!(
            log2.requests.lock().unwrap().as_slice(),
            ["mount:b-revoked", "mount:b-revoked"],
            "each later RPC re-probes once and surfaces the rejection"
        );
        drop(client);
        daemon2.join().unwrap();
    }

    /// During an outage, `BackingOpen` must NOT pay the reconnect
    /// cycle: its caller (a cache-hit `open()` holding the per-build
    /// backing-table lock) degrades to keep-cache reads on the spot, so
    /// the call has to surface the connection loss promptly and without
    /// touching the daemon. A `Promote` afterwards DOES reconnect, and
    /// once it has healed the session a later `BackingOpen` succeeds on
    /// the new connection — passthrough comes back without the open
    /// path ever stalling.
    // r[verify builder.fs.mountd-reconnect]
    #[test]
    fn backing_open_fails_fast_during_an_outage_and_heals_via_promote() {
        let tmp = tempfile::tempdir().unwrap();
        let sock = tmp.path().join("mountd.sock");

        let log1 = Arc::new(DaemonLog::default());
        let daemon1 = serve_one_connection(bind_listener(&sock), Arc::clone(&log1));
        let client = MountdClient::connect(&sock).expect("connect");
        let fuse_stub = tempfile::tempfile().unwrap();
        client
            .mount("b-fastfail", None, fuse_stub.as_raw_fd(), T)
            .expect("initial mount");

        // The daemon dies; its replacement is ALREADY listening, so a
        // BackingOpen that (wrongly) entered the reconnect cycle would
        // succeed against it instead of failing.
        log1.kill();
        daemon1.join().unwrap();
        let log2 = Arc::new(DaemonLog::default());
        let daemon2 = serve_one_connection(bind_listener(&sock), Arc::clone(&log2));

        let backing_file = tempfile::tempfile().unwrap();
        let started = std::time::Instant::now();
        let err = client
            .backing_open(backing_file.as_raw_fd(), T)
            .expect_err("BackingOpen on a dead connection must fail, not reconnect");
        assert!(matches!(
            err,
            MountdError::Disconnected(_) | MountdError::Send(_)
        ));
        assert!(
            started.elapsed() < Duration::from_secs(2),
            "BackingOpen must fail promptly (no backoff schedule), took {:?}",
            started.elapsed()
        );
        assert!(
            log2.requests.lock().unwrap().is_empty(),
            "BackingOpen must not have re-dialed the daemon: {:?}",
            log2.requests.lock().unwrap()
        );

        // A Promote takes the reconnect path, heals the session…
        client
            .promote([0x33; 32], T)
            .expect("promote reconnects to the new daemon");
        // …and BackingOpen works again on the healed connection.
        let id = client
            .backing_open(backing_file.as_raw_fd(), T)
            .expect("BackingOpen succeeds once the session is healed");
        assert_eq!(id, 1);
        assert_eq!(
            log2.requests.lock().unwrap().as_slice(),
            ["mount:b-fastfail", "promote", "backing_open"],
            "the heal happens via the Promote-driven re-Mount, never via BackingOpen"
        );
        drop(client);
        daemon2.join().unwrap();
    }
}
