//! Per-connection SSH state machine.
//!
//! [`ConnectionHandler`] is the `russh::server::Handler` impl — one per
//! accepted TCP stream, constructed by `GatewayServer::new_client` in
//! `mod.rs`. [`ChannelSession`] tracks each open SSH channel's protocol
//! task. Split out of `server/mod.rs` so the server-wide accept loop and
//! the per-connection state machine live in separate files.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};

use ed25519_dalek::SigningKey;
use rio_auth::jwt;
use rio_common::config::JwtConfig;
use rio_common::signal::Token as CancellationToken;
use rio_common::tenant::{NameError, NormalizedName};
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use russh::keys::PublicKey;
use russh::server::{Auth, Handler, Msg, Session};
use russh::{ChannelId, ChannelWriteHalf, Disconnect};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::transport::Channel;
use tracing::{Instrument, debug, error, info, trace, warn};

use super::AuthorizedKeys;
use super::session_jwt::{mint_session_jwt, refresh_session_jwt};
use crate::handler::SessionJwt;
use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;
use crate::session::run_protocol;

/// How far an SSH connection got before it ended. Stored as an
/// `AtomicU8` shared between the [`ConnectionHandler`] (advances it)
/// and the spawned `ssh-session` task in `mod.rs` (reads it for
/// `log_session_end`). A `Keepalive timeout` at `tcp-accepted` means
/// the client opened TCP but never sent SSH bytes (e.g., wedged on a
/// hung ssh-agent before the version exchange) — versus the same error
/// at `channel-open` meaning a real session went silent mid-build.
#[derive(Clone, Copy)]
#[repr(u8)]
pub(super) enum ConnStage {
    /// TCP accepted; no SSH protocol bytes yet (NLB probe or wedged client).
    TcpAccepted = 0,
    /// First `auth_*` callback fired (real SSH client, not a TCP probe).
    AuthAttempted = 1,
    /// `auth_publickey` returned `Accept`.
    Authenticated = 2,
    /// At least one `channel_open_session` accepted.
    ChannelOpen = 3,
}

impl ConnStage {
    pub(super) fn name(v: u8) -> &'static str {
        match v {
            0 => "tcp-accepted",
            1 => "auth-attempted",
            2 => "authenticated",
            3 => "channel-open",
            _ => "?",
        }
    }
}

/// State for an active protocol session on one SSH channel.
pub(super) struct ChannelSession {
    /// Send client data to the protocol handler.
    client_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Protocol handler task. NOT aborted in Drop — dropping a
    /// `JoinHandle` detaches the task, it keeps running. `shutdown`
    /// below is the graceful stop signal. Held (not immediately
    /// detached at spawn) so the detach happens at ChannelSession
    /// lifetime end, preserving the option to `await` it later.
    /// Underscore-prefixed: never read, intentionally so.
    _proto_task: tokio::task::JoinHandle<()>,
    /// Response pump task. Owns the [`SessionGuard`] (session permit,
    /// gauge, live-session count) — see the guard's doc for why the
    /// resources live with the task rather than this map entry.
    response_task: tokio::task::JoinHandle<()>,
    /// Fired in Drop to let `proto_task` run its cancel-on-disconnect
    /// loop before exiting. Replaces the hard `abort()` that raced the
    /// EOF-detection path: `channel_close → Drop → abort()` could fire
    /// before `session.rs` saw `UnexpectedEof` from the dropped mpsc
    /// sender. Aborted futures get no cleanup — `CancelBuild` never
    /// sent, the build leaked until `r[sched.backstop.orphan-watcher]`.
    shutdown: CancellationToken,
}

impl Drop for ChannelSession {
    fn drop(&mut self) {
        // Signal graceful shutdown. proto_task's select picks this up
        // and runs the same CancelBuild loop as the EOF arm, THEN
        // returns naturally. The JoinHandle is dropped here too, but
        // dropping a JoinHandle detaches the task — it does NOT abort
        // it. The task finishes its cancel loop (bounded by
        // DEFAULT_GRPC_TIMEOUT × active_build_ids.len()) and exits.
        //
        // Subtle: the select only guards the opcode-READ, not the
        // handler body. If Drop fires mid-handle_opcode (e.g., deep in
        // a wopBuildDerivation stream loop), the token is already
        // cancelled but nobody's polling it yet. That's fine —
        // response_task.abort() below breaks the outbound pipe, the
        // handler's next stderr write gets BrokenPipe, handle_opcode
        // returns Err, and the mid-opcode cancel path (session.rs
        // handler-Err arm) runs. Same destination, different entrance.
        self.shutdown.cancel();
        // response_task is a dumb pump — no state to clean up. Abort
        // is still correct, and it's load-bearing for the mid-opcode
        // case above (breaks the outbound pipe). Aborting also drops the
        // task's SessionGuard (permit, gauge, live-session count) if the
        // session was still live — the abnormal-path release.
        self.response_task.abort();
    }
}

/// Per-connection bookkeeping of LIVE protocol sessions and the
/// empty-connection grace timer (`r[gw.conn.exit-status+3]`).
///
/// Shared (`Arc`) between the [`ConnectionHandler`] (auth-time arming,
/// exec-time disarm, connection drop) and every session's
/// [`SessionGuard`] (released when the protocol session ACTUALLY ends).
/// The session count and the timer live under one mutex so
/// "last live session ended → arm" and "exec admitted → disarm" are
/// atomic transitions — without that, a guard dropping concurrently with
/// an exec admission could arm a timer while a live session exists.
///
/// Why not key emptiness on the `sessions` map: the map entry is only
/// removed by CLIENT action (channel close, data on a dead channel) or
/// connection end. A session that ends SERVER-side (handshake/idle
/// timeout, protocol error) with a client that ignores the server's
/// channel close has no further handler callback to run the bookkeeping
/// in — the release has to ride on the session's own task ending.
pub(super) struct EmptyConnectionTimer {
    inner: std::sync::Mutex<EmptyConnectionTimerInner>,
    /// Transport force-close deadline shared with the accept-site
    /// [`super::ConnDeadline`] wrapper. Armed by the timer task at the
    /// moment it queues its `Disconnect::ByApplication`, so a peer that
    /// keeps that disconnect undeliverable (parked key exchange) or
    /// ignores it (never closes its socket) is force-closed
    /// [`super::FORCE_CLOSE_SLACK`] later anyway.
    force_close: Arc<super::ForceClose>,
}

struct EmptyConnectionTimerInner {
    /// Number of protocol sessions currently live on this connection:
    /// incremented when an exec is admitted, decremented when the
    /// session's [`SessionGuard`] drops.
    live_sessions: usize,
    /// The armed idle-disconnect task, if the connection currently has
    /// zero live sessions. Aborted on exec admission and connection drop.
    pending_disconnect: Option<tokio::task::JoinHandle<()>>,
    /// Set when the `ConnectionHandler` drops: the connection is gone,
    /// so guards that drop afterwards must not arm fresh timers against
    /// a dead `Handle` (they'd just leave a useless sleeper behind).
    connection_closed: bool,
}

impl EmptyConnectionTimer {
    pub(super) fn new(force_close: Arc<super::ForceClose>) -> Self {
        Self {
            inner: std::sync::Mutex::new(EmptyConnectionTimerInner {
                live_sessions: 0,
                pending_disconnect: None,
                connection_closed: false,
            }),
            force_close,
        }
    }

    fn lock(&self) -> std::sync::MutexGuard<'_, EmptyConnectionTimerInner> {
        self.inner
            .lock()
            .expect("empty-connection timer mutex poisoned")
    }

    // r[impl gw.conn.exit-status+3]
    /// Arm the empty-connection grace timer: after `grace` with zero live
    /// protocol sessions, disconnect the connection. No-op if a timer is
    /// already running (the grace measures how long the connection has
    /// *continuously* had zero live sessions, so re-arming must not reset
    /// the clock), if a session is currently live, or if the connection
    /// has already ended.
    ///
    /// Race at the boundary: an exec admitted in the same instant the
    /// timer fires may lose (the abort lands after the disconnect was
    /// already queued). The timer re-checks the live count when it wakes
    /// to shrink that window, but it cannot be eliminated — the inherent
    /// boundary condition of any idle timeout; the client retries on a
    /// fresh connection.
    fn arm_if_idle(
        self: Arc<Self>,
        handle: russh::server::Handle,
        peer: Option<SocketAddr>,
        grace: std::time::Duration,
    ) {
        let mut inner = self.lock();
        if inner.pending_disconnect.is_some() || inner.live_sessions > 0 || inner.connection_closed
        {
            return;
        }
        let timer = Arc::clone(&self);
        inner.pending_disconnect = Some(rio_common::task::spawn_monitored(
            "idle-disconnect",
            async move {
                tokio::time::sleep(grace).await;
                {
                    let mut inner = timer.lock();
                    if inner.live_sessions > 0 || inner.connection_closed {
                        // An exec was admitted (or the connection already
                        // ended) while we slept and the abort lost the
                        // race. Clear the slot so a later last-session-end
                        // can arm a fresh timer, and stand down.
                        inner.pending_disconnect = None;
                        return;
                    }
                }
                debug!(
                    peer = ?peer,
                    grace_secs = grace.as_secs(),
                    "no active sessions for the grace period; disconnecting"
                );
                // Decide-then-enforce: arm the transport force-close BEFORE
                // queueing the polite disconnect. The disconnect rides the
                // russh handle queue, which is only drained between key
                // exchanges and whose delivery the peer can defeat (parked
                // rekey) or ignore (never closing its socket — russh's
                // post-disconnect drain-read loop has no timeout of its
                // own). Once armed, the accept-site `ConnDeadline` fails
                // the transport read `FORCE_CLOSE_SLACK` from now, ending
                // russh's session loop (or its drain loop) and releasing
                // the permit, fd, and gauges through the normal drop paths
                // with zero further cooperation from the peer. Arming
                // before the send also keeps this task bounded if the
                // handle queue itself is parked full.
                // r[impl gw.conn.force-close]
                timer.force_close.arm_within(super::FORCE_CLOSE_SLACK);
                // Err = the connection already ended for another
                // reason — nothing left to disconnect.
                let _ = handle
                    .disconnect(
                        Disconnect::ByApplication,
                        "no active sessions".to_owned(),
                        String::new(),
                    )
                    .await;
            },
        ));
    }

    /// An exec was admitted: the connection is no longer empty. Bumps the
    /// live count and disarms any pending idle-disconnect.
    fn session_admitted(&self) {
        let mut inner = self.lock();
        inner.live_sessions += 1;
        if let Some(timer) = inner.pending_disconnect.take() {
            timer.abort();
        }
    }

    /// A protocol session ended (its [`SessionGuard`] dropped). Decrements
    /// the live count and, if that was the last live session, starts the
    /// empty-connection grace clock.
    fn session_ended(
        self: Arc<Self>,
        handle: russh::server::Handle,
        peer: Option<SocketAddr>,
        grace: std::time::Duration,
    ) {
        {
            let mut inner = self.lock();
            inner.live_sessions = inner.live_sessions.saturating_sub(1);
        }
        // Re-checks live count / closed flag under its own lock; the gap
        // between the two locks only matters if an exec lands exactly in
        // between, in which case arming correctly does nothing.
        self.arm_if_idle(handle, peer, grace);
    }

    /// The connection ended: abort any pending timer (no reason to keep a
    /// 60 s sleeper alive per churned connection) and stop future arming.
    fn connection_dropped(&self) {
        let mut inner = self.lock();
        inner.connection_closed = true;
        if let Some(timer) = inner.pending_disconnect.take() {
            timer.abort();
        }
    }
}

// r[impl gw.conn.session-cap+2]
/// Owns one protocol session's capacity accounting: the global
/// session-cap permit, the `rio_gateway_channels_active` increment, and
/// the connection's live-session count. Held by the session's response
/// task and dropped when the session ACTUALLY ends — the protocol task
/// has returned (client EOF, handshake/idle timeout, protocol error, or
/// graceful shutdown) or its output can no longer be delivered (a send
/// stalled past [`HANDLE_SEND_TIMEOUT`], so the transport is treated as
/// wedged), BEFORE the server-side `exit-status`/`eof`/`close` are
/// attempted — or when the `ChannelSession` is dropped early (client
/// channel close, connection end) and the response task is aborted with
/// it.
///
/// This is what makes a SERVER-side session ending release capacity: a
/// client that ignores the server's channel close and keeps answering
/// keepalives never triggers another handler callback, so the release
/// cannot live on the `sessions` map entry (which only client action
/// removes). Tying it to the task means every ending — client- or
/// server-initiated — releases the permit, the gauge, and (via
/// [`EmptyConnectionTimer::session_ended`]) starts the empty-connection
/// grace when the last live session is gone. The drop deliberately
/// precedes the close-out sends (see [`finish_channel_session`]): those
/// ride the per-connection russh handle queue, which a hostile peer can
/// park, and capacity release must never depend on the peer's transport
/// cooperation.
struct SessionGuard {
    /// Global session-cap permit (`r[gw.conn.session-cap+2]`), acquired in
    /// `exec_request` before the duplex buffers are allocated.
    /// Underscore-prefixed: never read, only dropped.
    _permit: OwnedSemaphorePermit,
    /// Shared live-session/grace-timer bookkeeping for the owning
    /// connection.
    timer: Arc<EmptyConnectionTimer>,
    /// Handle to the SSH connection, for arming the grace timer when the
    /// last live session ends.
    handle: russh::server::Handle,
    peer: Option<SocketAddr>,
    grace: std::time::Duration,
}

impl Drop for SessionGuard {
    fn drop(&mut self) {
        // Mirrors the increment in `exec_request` — exactly one guard is
        // created per increment, so the gauge tracks live protocol
        // sessions on every exit path (normal end, abort via
        // ChannelSession::Drop, connection teardown).
        metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
        Arc::clone(&self.timer).session_ended(self.handle.clone(), self.peer, self.grace);
    }
}

// r[impl gw.conn.send-deadline]
/// How long the gateway waits for the SSH client to take a single send —
/// a response-data chunk in [`pump_session_responses`] (sent through the
/// channel's window-aware write half), or the end-of-session close-out
/// batch in [`finish_channel_session`] (sent on the russh handle queue) —
/// before concluding the transport is wedged.
///
/// A send can legitimately wait on two things, both peer-paced. The
/// per-connection handle queue is shared by every session and only drained
/// between key exchanges, so a wait there is normally fair-share queueing
/// behind other sessions' chunks: even a fully loaded multiplexed
/// connection (`max_channels_per_connection` = 512 sessions × 32 KiB
/// chunks) on a slow client link clears a full round of sends in well
/// under a minute at ~1 MB/s. A response-data send additionally waits for
/// the client-granted SSH channel window, which the client tops up as it
/// reads; completing one send needs at most one 32 KiB chunk's worth of
/// window across the whole bound, so any client that is actually draining
/// — however slowly — clears it with orders of magnitude to spare. A send
/// still pending after this long is therefore not slow — the peer has
/// stopped taking output entirely (a key exchange held open forever, a
/// reader that stopped with the kernel buffer full, or a client that
/// withholds CHANNEL_WINDOW_ADJUST while keeping TCP and keepalives
/// healthy). 300 s matches the gateway's existing tolerance for an
/// unresponsive peer, the SSH keepalive policy in
/// [`build_ssh_config`](super::build_ssh_config) (30 s interval × (9 + 1)
/// misses), while turning "forever" into "minutes" for a genuinely wedged
/// peer: the transport is detected and force-closed instead of pinning
/// sessions, tasks, and buffers until the pod restarts.
pub(super) const HANDLE_SEND_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(300);

/// End-of-session bookkeeping run by the response task once the protocol
/// handler's output stream is exhausted: release the session's capacity
/// FIRST (drop the [`SessionGuard`]), then make a bounded best-effort
/// attempt to deliver the RFC 4254 close-out (`exit-status 0`, `eof`,
/// `close`), then reap the client pump.
///
/// Both the ordering and the bound are load-bearing:
///
/// - The close-out rides this connection's russh handle queue (10 slots),
///   which russh only drains between key exchanges. A peer that parks a
///   key exchange while that queue is full would wedge these sends
///   forever; if the guard were still held, the dead session's permit,
///   gauge slot, and live-session count would stay pinned with it — and
///   so would every later ended session's, since their response tasks
///   block on the same queue. Capacity release must not depend on the
///   peer's transport cooperation, so the guard drops before the first
///   send. The empty-connection grace the drop may arm measures live
///   sessions, not whether these close-out bytes were flushed.
/// - The sends are bounded by `close_out_timeout`, but generously
///   ([`HANDLE_SEND_TIMEOUT`] in production): a session ending is NORMAL
///   on a healthy multiplexed connection — one build of many finishing
///   while its siblings keep streaming — and on a congested client link
///   the close-out can legitimately queue behind those siblings for a
///   while. Abandoning it early would strand that client's foreground
///   `ssh` (and the `nix` invocation blocked on it), exactly the hang the
///   exit-status rule exists to prevent, so the budget must be one that
///   congestion alone cannot exhaust. The bound exists so a queue that
///   has genuinely stopped draining cannot park this task forever; when
///   it expires the transport is treated as wedged — the close-out is
///   abandoned and the connection's force-close is armed so the
///   accept-site deadline ([`super::ConnDeadline`]) ends the connection
///   without further cooperation from the peer.
async fn finish_channel_session(
    handle: russh::server::Handle,
    channel_id: ChannelId,
    session_guard: SessionGuard,
    client_pump: tokio::task::JoinHandle<()>,
    close_out_timeout: std::time::Duration,
    force_close: Arc<super::ForceClose>,
) {
    drop(session_guard);
    // r[impl gw.conn.exit-status+3]
    // RFC 4254 §6.10: send `exit-status` BEFORE eof/close. Without it,
    // openssh's foreground client process (under ControlMaster) never
    // returns to nix → nix blocks in pipe-read → `nom build` hangs until
    // ControlPersist expires. Unconditionally 0: the wire-level
    // `BuildResult` already conveys per-build success/failure to the nix
    // client; the ssh exit code only signals "the daemon session ended",
    // which it did.
    let close_out = async {
        if handle.exit_status_request(channel_id, 0).await.is_err() {
            warn!(channel = ?channel_id, "failed to send exit-status to SSH client");
        }
        if let Err(e) = handle.eof(channel_id).await {
            warn!(channel = ?channel_id, error = ?e, "failed to send EOF to SSH client");
        }
        if let Err(e) = handle.close(channel_id).await {
            warn!(channel = ?channel_id, error = ?e, "failed to close SSH channel");
        }
    };
    // r[impl gw.conn.send-deadline]
    if tokio::time::timeout(close_out_timeout, close_out)
        .await
        .is_err()
    {
        warn!(
            channel = ?channel_id,
            timeout_secs = close_out_timeout.as_secs(),
            "timed out delivering exit-status/eof/close (russh handle queue not draining); \
             abandoning the channel close-out and force-closing the connection"
        );
        // A queue that would not accept the close-out within the generous
        // budget is wedged, not congested (see the doc above). Treat the
        // transport as dead: arming the force-close lets the accept-site
        // deadline end the connection without any further cooperation
        // from the peer, instead of leaving a transport that can no
        // longer deliver anything to keep wedging future sessions.
        // r[impl gw.conn.force-close]
        force_close.arm_within(super::FORCE_CLOSE_SLACK);
    }
    // Reap the pump rather than wait for it: the session is over (on the
    // EOF exit the protocol reader is already gone; on the send-failure
    // and stalled-send exits the transport it would answer through is
    // dead or wedged), so forwarding further client data serves nothing.
    // On its own the pump only exits when the CLIENT acts (channel close,
    // EOF, more data on a dead channel) or the connection ends; waiting
    // on the peer would retain the pump, this task, and their buffers for
    // as long as the peer pleases — uncounted by the session cap, whose
    // permit was already released above. Aborting is safe: the pump is a
    // dumb copy loop with nothing to clean up beyond its pipe halves, and
    // a proto task that is still alive just sees inbound EOF and runs its
    // normal wind-down.
    client_pump.abort();
    // A JoinError from the abort above is expected cancellation, not a
    // failure; only surface real panics.
    if let Err(e) = client_pump.await
        && e.is_panic()
    {
        error!(channel = ?channel_id, "client pump task panicked: {e}");
    }
}

/// How the response pump delivers one chunk of protocol output to the SSH
/// client. Module-private seam: production sends through the channel's
/// window-aware write half (below); the unit tests in this file substitute
/// controllable sinks (the russh `Handle` path, a send that never
/// resolves) because `ChannelWriteHalf` has no public constructor and a
/// stock russh client cannot be made to park a handle queue or withhold
/// window on demand. Nothing outside this module can name or implement it.
trait SessionSink {
    /// Send one chunk toward the client. `Err` means the channel or
    /// session is gone (russh torn down, channel closed) — the pump treats
    /// it as the end of the session. Completion is peer-paced and possibly
    /// unbounded; the pump wraps every call in its send timeout.
    async fn send(&self, data: Vec<u8>) -> Result<(), ()>;
}

// r[impl gw.conn.session-cap+2]
/// Production sink: the window-aware write half of the session's SSH
/// channel, retained at `channel_open_session` and handed over at exec.
///
/// `data_bytes` chunks by min(max packet size, remaining window) and waits
/// while the client-granted window is exhausted, so russh is never handed
/// more channel data than the client has agreed to receive — the
/// per-channel `pending_data` buffer russh keeps for data awaiting window
/// (unbounded, drained only by client CHANNEL_WINDOW_ADJUST) stays at
/// roughly one advertised window. `Handle::data` has no such accounting:
/// it queues unconditionally, so a client that withholds window adjusts
/// while keeping TCP and keepalives healthy would let the entire response
/// stream (up to a `MAX_NAR_SIZE` NAR) accumulate in russh memory with no
/// existing bound firing. Through the write half that client instead gets
/// a send that never completes, which the pump's send timeout converts
/// into the wedged-transport response.
///
/// Caveat that keeps the timeout load-bearing: a send parked on the window
/// is only woken by a WINDOW_ADJUST — not by channel close or connection
/// teardown — so it can outlive the session it belongs to; the send bound
/// here plus the response task's abortability (`ChannelSession::Drop`) are
/// what reclaim it. A send into an already torn-down session fails fast.
impl SessionSink for ChannelWriteHalf<Msg> {
    async fn send(&self, data: Vec<u8>) -> Result<(), ()> {
        self.data_bytes(data).await.map_err(|_| ())
    }
}

/// Body of the per-session response task: forward protocol output to the
/// SSH client one bounded send at a time, then run the end-of-session
/// bookkeeping ([`finish_channel_session`]).
///
/// Every send is bounded by `send_timeout` ([`HANDLE_SEND_TIMEOUT`] in
/// production). A send completes only once the data is on the
/// per-connection russh handle queue AND covered by SSH channel window the
/// client has granted (the production sink is the channel's window-aware
/// write half — see [`SessionSink`]). The peer controls both: it can park
/// the handle queue (a key exchange held open forever, a reader that
/// simply stops draining the socket) or withhold CHANNEL_WINDOW_ADJUST
/// while answering keepalives. Without the bound this task would wait
/// inside the loop indefinitely while still owning the [`SessionGuard`],
/// pinning a global session permit, the channels-active gauge, and the
/// connection's live-session count. Nothing else can step in: the wedged
/// guards keep the live-session count positive, so the empty-connection
/// grace never fires, no disconnect is queued, and the transport
/// force-close is never armed — a handful of such connections could pin
/// the pod's entire session capacity until restart.
///
/// A send that exhausts the bound therefore means the peer has stopped
/// taking output, not that it is slow (see [`HANDLE_SEND_TIMEOUT`] for the
/// margin), and the response is connection-level: arm the transport
/// force-close — the accept-site deadline then tears the connection down
/// without the peer's cooperation — and end this session so its capacity
/// is released. That stays the right scope even when the proximate stall
/// is one channel's window: a peer that starves a channel it asked us to
/// stream into for the entire bound is not multiplexing in good faith, and
/// keeping the rest of its transport alive would keep the wedge it
/// created.
// 8 args is one over clippy's default of 7. Same call as run_protocol
// (session.rs): each arg is a distinct moving part of the session
// (output source, send target, close-out handle, capacity guard, …) and
// grouping them into a struct would just rename the args at one call
// site plus the unit tests.
#[allow(clippy::too_many_arguments)]
async fn pump_session_responses(
    mut outbound_reader: tokio::io::DuplexStream,
    sink: impl SessionSink,
    handle: russh::server::Handle,
    channel_id: ChannelId,
    session_guard: SessionGuard,
    client_pump: tokio::task::JoinHandle<()>,
    force_close: Arc<super::ForceClose>,
    send_timeout: std::time::Duration,
) {
    let mut buf = vec![0u8; 32 * 1024];
    loop {
        match outbound_reader.read(&mut buf).await {
            Ok(0) => break,
            Ok(n) => {
                metrics::counter!("rio_gateway_bytes_total", "direction" => "tx")
                    .increment(n as u64);
                // r[impl gw.conn.send-deadline]
                match tokio::time::timeout(send_timeout, sink.send(buf[..n].to_vec())).await {
                    Ok(Ok(())) => {}
                    Ok(Err(())) => {
                        warn!(channel = ?channel_id, "response pump: SSH send failed");
                        metrics::counter!("rio_gateway_errors_total", "type" => "ssh_send")
                            .increment(1);
                        break;
                    }
                    Err(_) => {
                        // Not slow — wedged: the peer took nothing for the
                        // entire generous bound (see the fn doc) — it
                        // either let the handle queue sit undrained or
                        // granted no channel window. Arm the transport
                        // force-close for this connection-level fault, then
                        // fall through to finish_channel_session so this
                        // session stops counting against the global cap.
                        warn!(
                            channel = ?channel_id,
                            timeout_secs = send_timeout.as_secs(),
                            "response pump: SSH send stalled (handle queue not draining or no \
                             channel window granted); treating the transport as wedged and \
                             force-closing the connection"
                        );
                        metrics::counter!("rio_gateway_errors_total", "type" => "ssh_send_stall")
                            .increment(1);
                        // r[impl gw.conn.force-close]
                        force_close.arm_within(super::FORCE_CLOSE_SLACK);
                        break;
                    }
                }
            }
            Err(e) => {
                error!(error = %e, "error reading protocol response");
                break;
            }
        }
    }
    // The protocol session is over: release the permit, the gauge,
    // and the live-session count, then deliver the close-out to
    // the client on a best-effort, bounded basis. Waiting for the
    // client to acknowledge (or for the close-out sends to make it
    // through a handle queue the peer may have parked) would let a
    // client that ignores the close pin a global session slot
    // indefinitely — the dead session must stop counting the
    // moment the server is done with it.
    finish_channel_session(
        handle,
        channel_id,
        session_guard,
        client_pump,
        send_timeout,
        force_close,
    )
    .await;
}

// r[impl gw.conn.exit-status+3]
/// Reject an exec request and tear the channel down so the client `ssh`
/// process exits. `channel_failure` alone leaves the channel open;
/// openssh under ControlMaster waits for `exit-status` before its
/// foreground process returns. Send failure (so the client knows exec
/// was refused), then `exit-status 1` + `eof` + `close` so `ssh gw foo`
/// exits 1 instead of hanging until ControlPersist.
fn reject_exec(
    session: &mut Session,
    channel: ChannelId,
) -> Result<(), <ConnectionHandler as Handler>::Error> {
    session.channel_failure(channel)?;
    session.exit_status_request(channel, 1)?;
    session.eof(channel)?;
    session.close(channel)?;
    Ok(())
}

/// Per-connection handler that manages SSH channels.
pub struct ConnectionHandler {
    pub(super) peer_addr: Option<SocketAddr>,
    pub(super) store_client: StoreServiceClient<Channel>,
    pub(super) log_client: rio_proto::LogServiceClient<Channel>,
    /// DrvBlobService client on the store channel (ADR-024 drv-digest
    /// population). `None` = legacy submissions only.
    pub(super) drv_blob_client: Option<rio_proto::DrvBlobServiceClient<Channel>>,
    pub(super) scheduler_client: SchedulerServiceClient<Channel>,
    /// Shared with `GatewayServer` + the watcher task. `.load()` per
    /// auth attempt — NOT snapshotted at connection-accept, so a key
    /// rotated mid-handshake (between TCP accept and `auth_publickey`)
    /// is judged against the current set.
    pub(super) authorized_keys: AuthorizedKeys,
    /// Active protocol sessions, indexed by channel ID.
    pub(super) sessions: HashMap<ChannelId, ChannelSession>,
    /// JWT signing key, cloned from `GatewayServer`. `None` → mint
    /// skipped in `auth_publickey`. Arc because `SigningKey` isn't
    /// `Clone` (zeroize-on-drop semantics) but we need one per
    /// connection handler.
    pub(super) jwt_signing_key: Option<Arc<SigningKey>>,
    /// JWT policy. `required` → whether mint failure rejects auth.
    pub(super) jwt_config: JwtConfig,
    /// ResolveTenant RPC timeout — gateway-only knob, lives here rather
    /// than on `JwtConfig` (scheduler/store never read it).
    pub(super) resolve_timeout: std::time::Duration,
    /// Service-identity HMAC signer (`RIO_SERVICE_HMAC_KEY_PATH`).
    /// Cloned into every `SessionContext` so write opcodes can attach
    /// `x-rio-service-token` on store `PutPath`. `None` = disabled.
    pub(super) service_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// Per-tenant rate limiter, cloned from `GatewayServer`. Passed
    /// through to every spawned protocol session. Clones share the
    /// underlying `dashmap` — the bucket for `tenant_name` "foo" is
    /// the same `dashmap` entry regardless of which SSH connection
    /// submits.
    pub(super) limiter: TenantLimiter,
    /// Per-tenant quota cache, cloned from `GatewayServer`. Shared
    /// state — a quota fetched by one channel is warm for all.
    pub(super) quota_cache: QuotaCache,
    /// Tenant name from the matched `authorized_keys` entry's comment
    /// field. Set in `auth_publickey` when a key matches. Passed to
    /// the scheduler as `SubmitBuildRequest.tenant_name` which resolves
    /// it to a UUID via the `tenants` table. `None` = single-tenant
    /// mode (empty comment) OR malformed comment (interior whitespace
    /// — logged at warn in `auth_publickey`). The [`NormalizedName`]
    /// type guarantees the `Some` case is trimmed and whitespace-free
    /// — no downstream `.trim()` needed anywhere in the request chain.
    pub(super) tenant_name: Option<NormalizedName>,
    /// Minted JWT + its claims, set in `auth_publickey` IFF
    /// `jwt_signing_key` is `Some` and minting succeeds. The token
    /// string is cloned into every `SessionContext` spawned from this
    /// connection (multiple SSH channels share one token — they're the
    /// same authenticated session). The claims are kept so
    /// [`session_jwt`](Self::session_jwt) / [`SessionJwt::token`] can
    /// read `sub`/`exp` to re-mint without re-parsing the token or
    /// re-resolving the tenant. `None` → header injection skipped →
    /// dual-mode fallback.
    pub(super) jwt_token: Option<(String, jwt::TenantClaims)>,
    /// Set on the first `auth_*` callback. Distinguishes real SSH
    /// clients from TCP probes (NLB/kubelet health checks) — probes
    /// close before any SSH bytes, so no auth callback ever fires.
    pub(super) auth_attempted: bool,
    /// Highest [`ConnStage`] reached. Shared with the `ssh-session`
    /// spawn site so `log_session_end` can report how far the
    /// connection got — see the `ConnStage` doc.
    pub(super) stage: Arc<AtomicU8>,
    /// Global connection-cap permit (`r[gw.conn.cap]`). Acquired in
    /// `GatewayServer::new_client`; dropped here in `Drop` so every
    /// disconnect path (EOF, error, abort) releases the slot. `None`
    /// means `new_client` hit the cap — `auth_none` checks this and
    /// returns `Err` to tear down the connection before any channel
    /// work. Underscore-prefixed: never read directly, only dropped.
    /// The option-ness IS read (`ensure_permit`).
    pub(super) conn_permit: Option<OwnedSemaphorePermit>,
    /// Shared with `GatewayServer::active_conns`. Bumped in
    /// [`Self::mark_real_connection`], decremented in `Drop` — same
    /// gate as the `connections_active` gauge so TCP probes don't
    /// count toward session-drain.
    pub(super) active_conns: Arc<AtomicUsize>,
    /// Clone of `GatewayServer::sessions_shutdown`. Each channel's
    /// `ChannelSession::shutdown` is `child_token()` of this, so
    /// cancelling the server-wide parent reaches every proto_task
    /// regardless of which connection/channel owns it.
    pub(super) sessions_shutdown: CancellationToken,
    /// Global active-session semaphore (`r[gw.conn.session-cap+2]`),
    /// shared with `GatewayServer` and every other connection.
    /// `exec_request` does `try_acquire_owned()`; the permit is owned by
    /// the session's [`SessionGuard`] (held by its response task), so it
    /// is released when the protocol session actually ends rather than
    /// when the client deigns to close the channel. This — not the
    /// per-connection channel bound — is the limit on per-pod session
    /// count and its steady-state buffers; what each session may buffer
    /// inside russh on top of that is bounded by the client-granted SSH
    /// window, because the response pump only sends through the
    /// channel's window-aware write half ([`SessionSink`]).
    pub(super) session_sem: Arc<Semaphore>,
    /// Per-connection SSH channel absurdity bound
    /// (`r[gw.conn.channel-limit+4]`). See
    /// [`super::DEFAULT_MAX_CHANNELS_PER_CONNECTION`].
    pub(super) max_channels_per_connection: usize,
    /// SSH-level open channel count: incremented when
    /// `channel_open_session` accepts, decremented in `channel_close`
    /// for channels in [`Self::accepted_channels`]. NOT `sessions.len()`
    /// — a channel that has been opened but not yet exec'd is counted
    /// here and invisible there, so a burst of opens with no execs is
    /// still bounded.
    pub(super) open_channels: usize,
    /// Channel ids this connection actually ACCEPTED in
    /// `channel_open_session`. russh invokes the per-channel handler
    /// callbacks for any well-formed channel id a peer puts on the wire
    /// — including ids that were never opened or are already closed — so
    /// the close-side bookkeeping for `r[gw.conn.channel-limit+4]` must
    /// be keyed on our own record, not on the peer's claim. Bounded by
    /// `max_channels_per_connection` (only accepted opens insert; closes
    /// remove).
    pub(super) accepted_channels: std::collections::HashSet<ChannelId>,
    /// Window-aware write halves of accepted channels, split off in
    /// `channel_open_session` and held until the channel either execs
    /// (`exec_request` moves the half into the session's response pump —
    /// its [`SessionSink`]) or closes un-exec'd (`channel_close` discards
    /// it). One entry per accepted, not-yet-exec'd channel, so it shares
    /// the `accepted_channels` bound; connection teardown drops the map
    /// with the handler. A channel whose half has already been consumed
    /// cannot exec again — see `exec_request`.
    pub(super) channel_writers: HashMap<ChannelId, ChannelWriteHalf<Msg>>,
    /// Grace period a connection may sit with zero active protocol
    /// sessions — from authentication (nothing exec'd yet) or from the
    /// last session ending — before it is disconnected. See
    /// [`super::EMPTY_CONNECTION_GRACE`].
    pub(super) empty_connection_grace: std::time::Duration,
    /// Max wait for `WORKER_MAGIC_1` on an exec'd channel, passed to
    /// [`run_protocol`]. Default [`crate::session::HANDSHAKE_TIMEOUT`];
    /// shrunk by tests via `GatewayServer::with_handshake_timeout`.
    pub(super) handshake_timeout: std::time::Duration,
    /// Live-session count + empty-connection grace timer, shared with
    /// every session's [`SessionGuard`] so the grace can be armed when
    /// the last LIVE session ends — even when that ending is server-side
    /// and the client never sends another SSH message. See
    /// [`EmptyConnectionTimer`].
    pub(super) idle: Arc<EmptyConnectionTimer>,
    /// Transport force-close deadline for this connection, created in
    /// `new_client` and shared with the accept-site
    /// [`super::ConnDeadline`] stream wrapper (which enforces it) and
    /// with [`Self::idle`] (whose grace timer arms it when it queues a
    /// disconnect). Held here so the accept site can reach it after
    /// `new_client` returns; the handler itself never arms it.
    pub(super) force_close: Arc<super::ForceClose>,
}

impl ConnectionHandler {
    /// Idempotent. Call from every `auth_*` entry point — the first SSH
    /// protocol event that distinguishes a real client from a TCP probe.
    fn mark_real_connection(&mut self) {
        if self.auth_attempted {
            return;
        }
        self.auth_attempted = true;
        self.stage
            .store(ConnStage::AuthAttempted as u8, Ordering::Relaxed);
        self.active_conns.fetch_add(1, Ordering::Relaxed);
        metrics::counter!("rio_gateway_connections_total", "result" => "new").increment(1);
        metrics::gauge!("rio_gateway_connections_active").increment(1.0);
        info!(peer = ?self.peer_addr, "new SSH connection");
    }

    /// ResolveTenant round-trip + JWT mint. Called from
    /// `auth_publickey` when `jwt_signing_key` is `Some` and
    /// `tenant_name` is `Some` — the caller pattern-matches and
    /// passes the [`NormalizedName`] directly, so this function
    /// never sees single-tenant mode.
    ///
    /// Returns `(token, claims)` on success — the caller stores both
    /// so [`refresh_session_jwt`] can re-mint locally. Error covers: RPC timeout,
    /// scheduler unavailable, unknown tenant (InvalidArgument), UUID
    /// parse failure, mint failure (corrupt key). Caller decides
    /// reject-vs-degrade based on `jwt_config.required`.
    ///
    /// The RPC is bounded by `resolve_timeout_ms`. A slow/stuck
    /// scheduler makes SSH auth slow by AT MOST that much — the
    /// round-trip is once per connect, so a 500ms penalty is
    /// acceptable (and invisible when warm: PG index lookup + RPC
    /// overhead is ~1-2ms). The timeout wraps the WHOLE RPC future,
    /// not just the connect — a scheduler that accepts the RPC but
    /// then blocks on PG is also covered.
    ///
    /// NOT cached across connections: each SSH connect gets a fresh
    /// resolve. The tenants table is tiny and the lookup is indexed;
    /// a per-gateway cache would need TTL/invalidation when a tenant
    /// is added/renamed, which is complexity for no measurable win at
    /// typical connect rates.
    async fn resolve_and_mint(
        &mut self,
        signing_key: &SigningKey,
        tenant_name: &NormalizedName,
    ) -> anyhow::Result<(String, jwt::TenantClaims)> {
        use rio_proto::scheduler::ResolveTenantRequest;

        let timeout = self.resolve_timeout;
        let req = tonic::Request::new(ResolveTenantRequest {
            tenant_name: tenant_name.to_string(),
        });

        // `scheduler_client` is `SchedulerServiceClient<Channel>`.
        // The tonic-generated `resolve_tenant` method takes `&mut self`
        // — clone here so we don't hold a &mut borrow across the
        // await (auth_publickey is `&mut self` already, and the
        // compiler doesn't like stacked &muts through field paths).
        // Channel is Arc-backed; the clone is a pointer copy.
        let mut client = self.scheduler_client.clone();

        let resp = tokio::time::timeout(timeout, client.resolve_tenant(req))
            .await
            .map_err(|_| {
                anyhow::anyhow!(
                    "ResolveTenant timed out after {}ms (scheduler slow or unreachable)",
                    timeout.as_millis()
                )
            })?
            .map_err(|status| {
                // The scheduler's InvalidArgument includes the tenant
                // name in the message (resolve_tenant_name's format
                // string). Pass it through — "unknown tenant: foo" is
                // more actionable than "RPC failed".
                anyhow::anyhow!(
                    "ResolveTenant RPC: {} ({})",
                    status.message(),
                    status.code()
                )
            })?;

        let tenant_id: uuid::Uuid = resp.into_inner().tenant_id.parse().map_err(|e| {
            // Should be unreachable — the scheduler's handler does
            // `Uuid::to_string()` on a UUID it just read from PG. If
            // this fires, the scheduler is serving garbage.
            anyhow::anyhow!("scheduler returned unparseable tenant_id UUID: {e}")
        })?;

        let (token, claims) = mint_session_jwt(tenant_id, signing_key)?;
        Ok((token, claims))
    }

    /// Construct a [`SessionJwt`] for a freshly-spawned protocol task.
    /// Refreshes the connection-level cached token first (so all
    /// channels on a ControlMaster mux see a fresh token at open —
    /// I-129) then hands a clone of `(cached, signing_key)` to the
    /// task. The task's [`SessionJwt::token`] re-mints lazily on every
    /// access, so a single channel that outlives `JWT_SESSION_TTL_SECS`
    /// (long build) never sends a stale token.
    fn session_jwt(&mut self) -> SessionJwt {
        refresh_session_jwt(&mut self.jwt_token, self.jwt_signing_key.as_deref());
        SessionJwt::new(self.jwt_token.clone(), self.jwt_signing_key.clone())
    }

    /// Enforce `r[gw.conn.cap]`: if `new_client` hit the cap
    /// (`conn_permit: None`), return `Err` so russh tears down the
    /// connection. Called from every `auth_*` entry point — the
    /// earliest we can surface a visible SSH-level disconnect
    /// reason. The error propagates to `log_session_end` (with
    /// `stage=auth-attempted`).
    fn ensure_permit(&self) -> Result<(), anyhow::Error> {
        if self.conn_permit.is_none() {
            // The cap value lives on GatewayServer (semaphore), not here.
            // Client sees an SSH disconnect; server logs the `conn_cap`
            // error counter. Operator checks gateway.toml max_connections.
            return Err(anyhow::anyhow!("connection cap reached"));
        }
        Ok(())
    }

    // r[impl gw.conn.exit-status+3]
    /// Arm the empty-connection grace timer (delegates to
    /// [`EmptyConnectionTimer::arm_if_idle`]). Called from
    /// `auth_succeeded` — the connection is established with nothing
    /// exec'd yet. The other entry into the zero-live-sessions state is a
    /// session ending, which arms via [`SessionGuard`]'s drop (the
    /// session's own task), because a server-side ending may come with no
    /// further SSH callback to do it in. `exec_request` disarms it when a
    /// session is admitted; `Drop` aborts it.
    fn arm_empty_connection_timer(&mut self, session: &Session) {
        Arc::clone(&self.idle).arm_if_idle(
            session.handle(),
            self.peer_addr,
            self.empty_connection_grace,
        );
    }

    // r[impl gw.conn.channel-limit+4]
    /// Record an accepted `channel_open_session` toward the
    /// per-connection channel bound. The set membership is what later
    /// authorizes the matching close (and exec) to be acted on.
    fn note_channel_accepted(&mut self, channel: ChannelId) {
        if self.accepted_channels.insert(channel) {
            self.open_channels += 1;
        }
    }

    // r[impl gw.conn.channel-limit+4]
    /// Record a `CHANNEL_CLOSE` for `channel`. Returns `true` if the
    /// channel was one this connection actually accepted (now untracked
    /// and decremented); `false` for ids that were never accepted or are
    /// already closed — the caller must treat those as no-ops. russh
    /// dispatches the close callback for ANY well-formed id the peer
    /// puts on the wire, so without this gate an authenticated client
    /// could interleave real opens with forged or duplicate closes to
    /// hold `open_channels` near zero while russh's channel table keeps
    /// growing, defeating the absurdity bound.
    fn note_channel_closed(&mut self, channel: ChannelId) -> bool {
        if !self.accepted_channels.remove(&channel) {
            debug!(
                channel = ?channel,
                "ignoring CHANNEL_CLOSE for a channel this connection never accepted \
                 (forged or duplicate)"
            );
            return false;
        }
        self.open_channels = self.open_channels.saturating_sub(1);
        true
    }

    // r[impl gw.conn.channel-types]
    /// Build the error that ends the connection in response to a
    /// non-`session` channel-open request — the shared body of the
    /// `channel_open_direct_tcpip` / `channel_open_x11` /
    /// `channel_open_forwarded_tcpip` / `channel_open_direct_streamlocal`
    /// overrides. The gateway exists solely to carry `nix-daemon
    /// --stdio` over session channels, so none of these are ever part
    /// of the build-submission protocol; they come from a stray
    /// `LocalForward`/`DynamicForward`/ProxyJump in a client config or
    /// from a hostile peer. Refusing per-open (`Ok(false)`, russh's own
    /// default for these callbacks) is not an option for the same
    /// reason as the channel bound in `channel_open_session`: russh
    /// registers the channel's state in its per-connection map for any
    /// non-error result and never removes it for a refused open — and
    /// these opens never count toward `open_channels`, so they cannot
    /// even trip that bound. Erroring keeps russh-side state bounded; a
    /// terminated connection is an acceptable outcome for a
    /// forwarding-configured client, whose forward was never going to
    /// be honored anyway.
    fn reject_unsupported_channel_open(&self, channel_type: &'static str) -> anyhow::Error {
        warn!(
            peer = ?self.peer_addr,
            channel_type,
            "closing SSH connection: gateway does not support TCP/X11/socket forwarding \
             channels, only sessions"
        );
        metrics::counter!("rio_gateway_errors_total", "type" => "unsupported_channel_type")
            .increment(1);
        anyhow::anyhow!("unsupported SSH channel-open type: {channel_type}")
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        // The idle-disconnect timer holds a russh `Handle` to this
        // connection; if the connection is already gone the disconnect
        // would be a harmless no-op, but there's no reason to keep a
        // 60s sleeper alive per churned connection. Marking the
        // connection closed also stops SessionGuards that drop after us
        // (the map below clears, aborting their response tasks) from
        // arming fresh timers against a dead connection.
        //
        // sh-009: dropping `self.sessions` below runs N×
        // `ChannelSession::Drop`, each firing its per-channel
        // `shutdown.cancel()`; each detached `_proto_task` reaches the
        // unconditional `cancel_active_builds` chokepoint with THAT
        // channel's set — the union over all channels IS every build on
        // this connection (tripwire:
        // `multi_channel_disconnect_cancels_all_builds`). A residual
        // leak is therefore either a best-effort `CancelBuild` that
        // never reached the scheduler
        // (`rio_gateway_builds_leaked_on_disconnect_total`) or a
        // build_id never inserted into the per-channel set; both fall
        // through to `r[sched.backstop.orphan-watcher]`.
        self.idle.connection_dropped();
        if self.auth_attempted {
            self.active_conns.fetch_sub(1, Ordering::Relaxed);
            metrics::gauge!("rio_gateway_connections_active").decrement(1.0);
            // Channel gauge decrement is handled by ChannelSession::Drop
            // when the sessions HashMap is cleared.
            debug!(
                peer = ?self.peer_addr,
                remaining_channels = self.sessions.len(),
                "SSH connection handler dropped"
            );
        } else {
            trace!(peer = ?self.peer_addr, "TCP probe dropped (no SSH handshake)");
        }
    }
}

/// Normalize an `authorized_keys` comment into a tenant name.
///
/// Three outcomes per `NormalizedName::new`:
///
/// - `Ok(name)` → multi-tenant mode with a valid tenant identifier.
/// - `Err(Empty)` → single-tenant mode. Intentional — the operator
///   left the comment blank. `None`, no noise.
/// - `Err(InteriorWhitespace)` → MISCONFIGURED. The operator typo'd
///   `team a` instead of `team-a` in `authorized_keys`. Degrade to
///   single-tenant (the comment isn't a usable identifier — same
///   outcome as Empty) but SURFACE the misconfig: `warn!` makes it
///   visible in logs, `rio_gateway_auth_degraded_total{reason=
///   interior_whitespace}` makes it alertable. Without this, builds
///   succeed in single-tenant mode and the operator never learns
///   their tenant isolation is silently off.
///
/// Extracted as a free function so tests can assert the counter fires
/// without constructing a full `ConnectionHandler` (which needs live
/// gRPC clients). Takes `key_fingerprint` as `impl Display` — the call
/// site passes `matched.fingerprint(Default::default())`; tests pass
/// a string literal.
// r[impl gw.auth.tenant-from-key-comment]
fn normalize_key_comment(
    comment: &[u8],
    key_fingerprint: &dyn std::fmt::Display,
) -> Option<NormalizedName> {
    // ssh-key 0.7 (via russh 0.61) models comments as raw bytes — RFC
    // 4251 strings need not be UTF-8. A tenant name must be a string:
    // treat invalid UTF-8 exactly like interior whitespace below — a
    // MISCONFIGURED authorized_keys entry → degrade to single-tenant,
    // but WARN + bump the counter so the operator notices. Strict
    // `from_utf8` (not `Comment::as_str_lossy`): lossy truncates to the
    // longest valid prefix, which could silently turn a corrupted
    // `team-alpha\xFF…` comment into the REAL tenant `team-alpha`.
    let comment = match std::str::from_utf8(comment) {
        Ok(comment) => comment,
        Err(_) => {
            warn!(
                key_fingerprint = %key_fingerprint,
                "authorized_keys comment is not valid UTF-8 — \
                 degrading to single-tenant mode; re-write the comment \
                 as plain UTF-8 (e.g. `team-a`)"
            );
            metrics::counter!(
                "rio_gateway_auth_degraded_total",
                "reason" => "invalid_utf8"
            )
            .increment(1);
            return None;
        }
    };
    match NormalizedName::new(comment) {
        Ok(name) => Some(name),
        // Intentional single-tenant: empty comment. No noise.
        Err(NameError::Empty) => None,
        // Misconfigured: interior whitespace. Degrade + warn.
        Err(NameError::InteriorWhitespace(raw)) => {
            warn!(
                comment = %raw,
                key_fingerprint = %key_fingerprint,
                "authorized_keys comment has interior whitespace — \
                 degrading to single-tenant mode; fix the comment \
                 (e.g. `team a` → `team-a`)"
            );
            metrics::counter!(
                "rio_gateway_auth_degraded_total",
                "reason" => "interior_whitespace"
            )
            .increment(1);
            None
        }
    }
}

impl Handler for ConnectionHandler {
    type Error = anyhow::Error;

    // r[impl gw.conn.real-connection-marker]
    /// OpenSSH clients send `none` first (RFC 4252 §5.2 probe). This is
    /// the FIRST auth callback for a well-behaved client — the earliest
    /// point we can distinguish "real SSH client" from "TCP probe."
    /// Without this override, `mark_real_connection` only fires on
    /// `auth_password`/`auth_publickey`, missing clients that probe and
    /// disconnect (or probe, see `publickey` in the method list, and
    /// then fail key offering below before ever reaching
    /// `auth_publickey`).
    async fn auth_none(&mut self, _user: &str) -> Result<Auth, Self::Error> {
        self.mark_real_connection();
        self.ensure_permit()?;
        Ok(Auth::reject())
    }

    /// russh default accepts every offered key, forcing the client to
    /// compute a signature we'll then reject in `auth_publickey`. Check
    /// `authorized_keys` here instead — unknown key → reject before
    /// signature, saving the client a round-trip per ssh-agent key.
    ///
    /// DO NOT set `self.tenant_name` here. The client hasn't proven
    /// ownership yet (no signature). `auth_publickey` does the final
    /// match-and-set after russh verifies the signature.
    // r[impl gw.conn.real-connection-marker]
    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
        // Receiving an offered key means KEX completed and a
        // SSH_MSG_USERAUTH_REQUEST arrived — provably a real SSH
        // client. RFC 4252 §5.2 makes the `none` probe optional, so a
        // client that skips it and offers only unauthorized keys would
        // otherwise leave `auth_attempted=false` and be logged as a TCP
        // probe (invisible at INFO, no metrics, no `r[gw.conn.cap]`
        // enforcement). Idempotent — the OpenSSH `auth_none` →
        // `auth_publickey_offered` → `auth_publickey` path is unaffected.
        self.mark_real_connection();
        self.ensure_permit()?;
        let known = self
            .authorized_keys
            .load()
            .iter()
            .any(|authorized| authorized.key_data() == key.key_data());
        if known {
            Ok(Auth::Accept)
        } else {
            debug!(peer = ?self.peer_addr, "offered key not in authorized_keys");
            Ok(Auth::reject())
        }
    }

    async fn auth_password(&mut self, _user: &str, _password: &str) -> Result<Auth, Self::Error> {
        self.mark_real_connection();
        self.ensure_permit()?;
        warn!(peer = ?self.peer_addr, "rejecting password authentication");
        Ok(Auth::reject())
    }

    // r[impl gw.auth.tenant-from-key-comment]
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.mark_real_connection();
        self.ensure_permit()?;
        // The comment lives in the SERVER-SIDE authorized_keys entry, not
        // the client's key (SSH key auth sends raw key data only). We
        // match the client's key against our loaded entries, then read
        // .comment() from the MATCHED entry.
        let keys = self.authorized_keys.load();
        let matched = keys
            .iter()
            .find(|authorized| authorized.key_data() == key.key_data());

        if let Some(matched) = matched {
            // Normalize via the shared newtype so every tenant-name
            // consumer (scheduler, store, quota cache) sees the exact
            // same bytes. The `Option<NormalizedName>` type IS the
            // mode flag, threaded all the way through `run_protocol` /
            // `SessionContext` / `translate::build_submit_request`.
            // No downstream `.trim()` or `.is_empty()` checks needed
            // — the type guarantees the `Some` case is trimmed,
            // non-empty, and whitespace-free.
            //
            // Interior whitespace (`"team a"`) is a MISCONFIGURED
            // authorized_keys entry — degrade to single-tenant (same
            // as Empty; the comment isn't a usable identifier) but
            // WARN + bump `rio_gateway_auth_degraded_total` so the
            // operator notices their tenant isolation is off. The
            // helper is extracted for direct unit-testability (no
            // full `ConnectionHandler` needed to assert the counter
            // fires).
            self.tenant_name = normalize_key_comment(
                matched.comment().as_bytes(),
                &matched.fingerprint(Default::default()),
            );

            // r[impl gw.jwt.dual-mode+2]
            //
            // Dual-mode PERMANENT. Two branches maintained forever:
            //
            //   signing_key = None  → JWT disabled. Fall through to
            //     Auth::Accept; tenant identity flows via
            //     SubmitBuildRequest.tenant_name. This is the
            //     r[gw.auth.tenant-from-key-comment] path, unbumped.
            //
            //   signing_key = Some  → attempt mint. ResolveTenant
            //     round-trip to scheduler (gateway is PG-free per
            //     r[sched.tenant.resolve+2]). On success: mint + store
            //     in self.jwt_token → SessionContext → handler/build.rs
            //     injects as x-rio-tenant-token. On FAILURE
            //     (timeout, unknown tenant, mint error):
            //       required=true  → reject SSH auth
            //       required=false → degrade (jwt_token stays None,
            //                        fallback path same as key=None)
            //
            // The round-trip is once-per-SSH-connect, not per-request
            // (jwt_token is on ConnectionHandler, shared across all
            // channels). Bounded by resolve_timeout_ms (default 500).
            //
            // Empty tenant_name (single-tenant mode) skips the RPC
            // entirely — no JWT for single-tenant, same as key=None.
            // The scheduler's ResolveTenant rejects empty-name
            // (caller-error contract); gating here avoids the
            // pointless call.
            // Arc::clone out of the Option before calling the &mut
            // helper — `&self.jwt_signing_key` would hold an immutable
            // borrow of self across the &mut self.resolve_and_mint
            // call (E0502). The Arc clone is a pointer copy; the
            // SigningKey itself isn't cloned (zeroize-on-drop still
            // fires exactly once, on the original Arc's last drop).
            if let Some(signing_key) = self.jwt_signing_key.clone()
                && let Some(tenant_name) = self.tenant_name.clone()
            {
                match self.resolve_and_mint(&signing_key, &tenant_name).await {
                    Ok((token, claims)) => {
                        debug!(jti = %claims.jti, tenant = %tenant_name, "minted session JWT");
                        self.jwt_token = Some((token, claims));
                    }
                    Err(e) if self.jwt_config.required => {
                        // required=true: mint failure is an AUTH
                        // failure. Return reject (NOT an Err —
                        // russh::Error would close the whole TCP
                        // connection; reject lets the client know
                        // auth failed and disconnect cleanly).
                        warn!(
                            error = %e,
                            tenant = %tenant_name,
                            peer = ?self.peer_addr,
                            "JWT mint failed and jwt.required=true; rejecting SSH auth"
                        );
                        metrics::counter!(
                            "rio_gateway_connections_total",
                            "result" => "rejected_jwt"
                        )
                        .increment(1);
                        return Ok(Auth::reject());
                    }
                    Err(e) => {
                        // required=false: degrade. jwt_token stays
                        // None → handler/build.rs skips header inject
                        // → scheduler reads tenant_name from proto.
                        // Same behavior as key=None / pre-JWT.
                        warn!(
                            error = %e,
                            tenant = %tenant_name,
                            "JWT mint failed; degrading to tenant_name fallback"
                        );
                        metrics::counter!("rio_gateway_jwt_mint_degraded_total").increment(1);
                    }
                }
            }

            metrics::counter!("rio_gateway_connections_total", "result" => "accepted").increment(1);
            self.stage
                .store(ConnStage::Authenticated as u8, Ordering::Relaxed);
            info!(
                user = user,
                peer = ?self.peer_addr,
                tenant = self.tenant_name.as_deref().unwrap_or("-"),
                "SSH public key authentication accepted"
            );
            Ok(Auth::Accept)
        } else {
            metrics::counter!("rio_gateway_connections_total", "result" => "rejected").increment(1);
            warn!(
                user = user,
                peer = ?self.peer_addr,
                "SSH public key authentication rejected"
            );
            Ok(Auth::reject())
        }
    }

    // r[impl gw.conn.exit-status+3]
    /// The connection is established (counted against `r[gw.conn.cap]`,
    /// `connections_active` already incremented by the auth callbacks)
    /// but has zero active protocol sessions — start the
    /// empty-connection grace clock now. Without this, a client that
    /// authenticates and never execs (`ssh -N`, a ControlMaster held
    /// open with no commands, a client wedged before exec) is held
    /// forever: it answers the 30s keepalives (any received data resets
    /// russh's `alive_timeouts`), each reply also resets
    /// `inactivity_timeout`, and the close-time arm in `channel_close`
    /// never runs because no session ever existed — one
    /// `max_connections` slot, one fd, and a permanently elevated
    /// `connections_active` gauge. `exec_request` disarms the timer
    /// when a session is admitted, exactly as it does for the
    /// arm-on-last-session-close path.
    ///
    /// A connection that never even reaches this point (auth attempted
    /// but never succeeded) is bounded by the pre-auth deadline at the
    /// accept site (`GatewayServer::run_on_listener`) — the auth
    /// callbacks have no `&mut Session` to arm this timer with.
    async fn auth_succeeded(&mut self, session: &mut Session) -> Result<(), Self::Error> {
        self.arm_empty_connection_timer(session);
        Ok(())
    }

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        self.stage
            .store(ConnStage::ChannelOpen as u8, Ordering::Relaxed);
        let channel_id = channel.id();
        // r[impl gw.conn.channel-limit+4]
        // Absurdity bound on SSH-level open channels, NOT a resource
        // bound — that is the global session semaphore in
        // `exec_request`. Gate on `open_channels` (counted at
        // open/close), not `sessions.len()` (counted at exec/close): a
        // burst of N opens with no execs is invisible to the session
        // map but still allocates a russh channel-table entry each.
        //
        // Crossing the bound ends the CONNECTION (handler error ends
        // russh's session loop), not just the offending open: russh
        // allocates the channel's state (an eager mpsc plus window ref)
        // before consulting this handler, inserts it into its
        // per-connection channel map for any non-error result, and
        // never removes that entry for a refused open — the client
        // sends no CHANNEL_CLOSE for an open that failed, and russh's
        // open-failure removal only covers server-initiated opens — so
        // a per-open `Ok(false)` refusal lets an over-bound client keep
        // looping CHANNEL_OPENs and grow per-connection memory without
        // bound, defeating the bound itself. Erroring out is the only
        // response that keeps russh-side state bounded: nothing is
        // inserted for this open, and the connection unwinds through
        // the normal drop paths (conn permit/fd/gauges, per-channel
        // tasks, session permits), surfacing via the accept-site
        // `log_session_end`. A connection at this bound is already
        // leaking channels or hostile — legitimate ControlMaster
        // fan-out sits far below it (a 128-core CI box running
        // nix-fast-build behind one mux is ~128 channels, ~4×
        // headroom), so the corrupted-fallback concern that argues for
        // polite per-open handling of stock nix clients does not apply
        // here.
        if self.open_channels >= self.max_channels_per_connection {
            warn!(
                peer = ?self.peer_addr,
                open = self.open_channels,
                limit = self.max_channels_per_connection,
                "closing SSH connection: per-connection channel bound exceeded"
            );
            metrics::counter!("rio_gateway_errors_total", "type" => "channel_limit").increment(1);
            anyhow::bail!(
                "per-connection channel bound exceeded ({} open, limit {})",
                self.open_channels,
                self.max_channels_per_connection
            );
        }
        self.note_channel_accepted(channel_id);
        // Keep the channel's window-aware write half: it is what the
        // session's response pump will send protocol output through, so
        // the client's granted SSH window — not russh's unbounded
        // pending-data buffer — is what paces (and bounds) per-session
        // egress. The read half is dropped on purpose: inbound channel
        // data already reaches the session via the `Handler::data`
        // callback → `client_tx` → client pump, and russh delivers to a
        // retained read half with a blocking send, so holding one we
        // never drain would park the whole connection. russh's attempts
        // to deliver to the dropped half fail fast and are ignored.
        let (read_half, write_half) = channel.split();
        drop(read_half);
        self.channel_writers.insert(channel_id, write_half);
        // Deliberately NOT disarming the empty-connection grace timer
        // here: a bare channel open is not activity. An open-but-never-
        // exec'd channel has no ChannelSession, no session permit, and
        // no protocol task (the handshake/idle timeouts only start at
        // exec), so disarming on open would let a client hold the
        // connection forever by opening a channel it never uses. The
        // timer is disarmed in `exec_request` when a protocol session
        // is admitted — for a real nix client open→exec is
        // milliseconds, far inside the grace.
        info!(
            channel = ?channel_id,
            open = self.open_channels,
            "SSH session channel opened"
        );
        Ok(true)
    }

    // r[impl gw.conn.channel-types]
    // Non-session channel opens (this and the three overrides below)
    // end the connection; see `reject_unsupported_channel_open` for the
    // shared rationale.
    async fn channel_open_direct_tcpip(
        &mut self,
        _channel: russh::Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Err(self.reject_unsupported_channel_open("direct-tcpip"))
    }

    async fn channel_open_x11(
        &mut self,
        _channel: russh::Channel<Msg>,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Err(self.reject_unsupported_channel_open("x11"))
    }

    async fn channel_open_forwarded_tcpip(
        &mut self,
        _channel: russh::Channel<Msg>,
        _host_to_connect: &str,
        _port_to_connect: u32,
        _originator_address: &str,
        _originator_port: u32,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Err(self.reject_unsupported_channel_open("forwarded-tcpip"))
    }

    async fn channel_open_direct_streamlocal(
        &mut self,
        _channel: russh::Channel<Msg>,
        _socket_path: &str,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        Err(self.reject_unsupported_channel_open("direct-streamlocal"))
    }

    // r[impl gw.conn.exec-request]
    async fn exec_request(
        &mut self,
        channel_id: ChannelId,
        data: &[u8],
        session: &mut Session,
    ) -> Result<(), Self::Error> {
        let Ok(command) = String::from_utf8(data.to_vec()) else {
            warn!(channel = ?channel_id, "rejecting exec request: command is not valid UTF-8");
            return reject_exec(session, channel_id);
        };
        info!(channel = ?channel_id, command = %command, "exec request");

        let args: Vec<&str> = command.split_whitespace().collect();
        let is_nix_daemon = args.len() >= 2
            && args[args.len() - 2].ends_with("nix-daemon")
            && args[args.len() - 1] == "--stdio";
        if !is_nix_daemon {
            warn!(command = %command, "rejecting non-nix-daemon exec request");
            return reject_exec(session, channel_id);
        }

        // r[impl gw.conn.channel-limit+4]
        // Same unvalidated-id exposure as channel_close: russh dispatches
        // exec_request for any well-formed channel id, including ones
        // that were never opened at all. Don't spawn a protocol session
        // (and consume a global session permit) for a channel this
        // connection never accepted.
        if !self.accepted_channels.contains(&channel_id) {
            warn!(
                channel = ?channel_id,
                "rejecting exec request on a channel this connection never accepted"
            );
            return reject_exec(session, channel_id);
        }

        // r[impl gw.conn.session-cap+2]
        // Global admission control: one permit per spawned protocol
        // session, across ALL connections on this pod. Acquired BEFORE
        // `channel_success` and before the ~550 KiB of duplex buffers
        // below are allocated — a rejected exec must cost nothing.
        //
        // This is the safe place to shed load. By the time the exec
        // reply arrives, an OpenSSH mux master has already told its
        // client `MUX_S_SESSION_OPENED`, so a `channel_failure` here
        // produces a clean `ssh` exit — no fallback to a direct
        // connection, no LocalCommand corruption (the fallback a refused
        // channel OPEN would trigger; see gw.conn.session-cap+2).
        // At the default cap (4096 ≈ 2.2 GiB of steady-state buffers;
        // russh-side buffering on top of that is paced per session by
        // the client-granted window via the write half taken below)
        // this only fires when the pod is genuinely at its memory
        // ceiling, where a visible rejection beats an OOMKill of every
        // other session.
        let Ok(session_permit) = Arc::clone(&self.session_sem).try_acquire_owned() else {
            warn!(
                peer = ?self.peer_addr,
                "rejecting exec request: global session cap reached"
            );
            metrics::counter!("rio_gateway_errors_total", "type" => "session_cap").increment(1);
            return reject_exec(session, channel_id);
        };

        // The channel's write half is what the response pump sends through
        // (see [`SessionSink`]); each accepted channel has exactly one,
        // and the first admitted exec consumes it. No half left means this
        // channel already exec'd — RFC 4254 sessions run one command, and
        // OpenSSH never re-execs a channel — so refuse rather than spawn a
        // second protocol session whose output could not be delivered.
        // The just-acquired permit is released by the early return.
        let Some(write_half) = self.channel_writers.remove(&channel_id) else {
            warn!(
                channel = ?channel_id,
                "rejecting exec request: channel already ran an exec"
            );
            return reject_exec(session, channel_id);
        };

        // r[impl gw.conn.exit-status+3]
        // An admitted exec creates a protocol session — the connection
        // is no longer empty, so bump the live-session count and disarm
        // any pending idle-disconnect (armed at auth time or when the
        // previous last session ended).
        self.idle.session_admitted();

        session.channel_success(channel_id)?;
        metrics::gauge!("rio_gateway_channels_active").increment(1.0);
        // Owns the permit, the gauge decrement, and the live-session
        // count for this session; moved into the response task below and
        // dropped when the session actually ends. See [`SessionGuard`].
        let session_guard = SessionGuard {
            _permit: session_permit,
            timer: Arc::clone(&self.idle),
            handle: session.handle(),
            peer: self.peer_addr,
            grace: self.empty_connection_grace,
        };

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let (inbound_reader, mut inbound_writer) = tokio::io::duplex(256 * 1024);
        let (outbound_reader, outbound_writer) = tokio::io::duplex(256 * 1024);

        // Task: forward SSH client data -> inbound pipe
        let client_pump = rio_common::task::spawn_monitored("client-pump", async move {
            while let Some(data) = client_rx.recv().await {
                if let Err(e) = inbound_writer.write_all(&data).await {
                    debug!(error = %e, "client pump: inbound write failed");
                    break;
                }
            }
            drop(inbound_writer);
        });

        // Task: run the protocol handler with gRPC clients
        let mut store_client = self.store_client.clone();
        let mut log_client = self.log_client.clone();
        let drv_blob_client = self.drv_blob_client.clone();
        let mut scheduler_client = self.scheduler_client.clone();
        let tenant_name = self.tenant_name.clone();
        // One token per SSH connection, shared across all channels.
        // The spawned task gets a `SessionJwt` (cached + signing_key)
        // and refreshes lazily on every `.token()` access — covers
        // BOTH I-129 (ControlMaster mux opens a new channel past
        // `JWT_SESSION_TTL_SECS`) AND a single channel outliving the
        // TTL (keepalive resets `inactivity_timeout`, so a >65min
        // build never trips it; the post-build `wopQueryPathInfo`
        // would otherwise send an expired token).
        let jwt = self.session_jwt();
        // Shared-state clone: all channels on all connections drain
        // the same per-tenant bucket.
        let service_signer = self.service_signer.clone();
        let limiter = self.limiter.clone();
        let quota_cache = self.quota_cache.clone();
        // Graceful-shutdown link: Drop fires this, run_protocol selects
        // on it. One token per channel — each channel's cancel loop is
        // independent. Child of the server-wide `sessions_shutdown`
        // (I-081) so the drain-timeout path can broadcast cancel to
        // every open channel; ChannelSession::Drop cancelling the child
        // affects only that channel (children don't cascade upward).
        let shutdown = self.sessions_shutdown.child_token();
        let shutdown_child = shutdown.child_token();
        let handshake_timeout = self.handshake_timeout;
        let proto_task = rio_common::task::spawn_monitored(
            "proto-task",
            async move {
                let mut reader = inbound_reader;
                let mut writer = outbound_writer;
                if let Err(e) = run_protocol(
                    &mut reader,
                    &mut writer,
                    &mut store_client,
                    &mut log_client,
                    drv_blob_client,
                    &mut scheduler_client,
                    tenant_name,
                    jwt,
                    service_signer,
                    limiter,
                    quota_cache,
                    handshake_timeout,
                    shutdown_child,
                )
                .await
                {
                    error!(error = %e, "protocol session error");
                }
                debug!("protocol handler finished");
            }
            .instrument(tracing::info_span!("channel", channel = ?channel_id)),
        );

        // Task: pump protocol responses -> SSH client, through the
        // channel's window-aware write half taken above. The body is a
        // free fn so the stalled-send behavior can be unit-tested; it owns
        // the SessionGuard and runs `finish_channel_session` when the
        // protocol output ends or a send stalls. The handle is only for
        // that close-out (exit-status/eof/close are small control
        // messages); response data goes through the write half.
        let handle = session.handle();
        // A stalled send (or close-out) is a connection-level fault: the
        // pump arms this connection's transport force-close, shared with
        // the accept-site `ConnDeadline` wrapper that enforces it.
        let force_close = Arc::clone(&self.force_close);
        let response_task = rio_common::task::spawn_monitored("response-task", async move {
            pump_session_responses(
                outbound_reader,
                write_half,
                handle,
                channel_id,
                session_guard,
                client_pump,
                force_close,
                HANDLE_SEND_TIMEOUT,
            )
            .await;
        });

        self.sessions.insert(
            channel_id,
            ChannelSession {
                client_tx: Some(client_tx),
                _proto_task: proto_task,
                response_task,
                shutdown,
            },
        );

        Ok(())
    }

    async fn data(
        &mut self,
        channel: ChannelId,
        data: &[u8],
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        metrics::counter!("rio_gateway_bytes_total", "direction" => "rx")
            .increment(data.len() as u64);
        if let Some(chan_session) = self.sessions.get(&channel) {
            if let Some(tx) = &chan_session.client_tx {
                debug!(channel = ?channel, len = data.len(), "forwarding client data to protocol");
                if tx.send(data.to_vec()).await.is_err() {
                    warn!(channel = ?channel, "protocol session dead, closing channel");
                    // Bookkeeping cleanup only: the dead session's permit,
                    // gauge slot, and live-session count were already
                    // released when its tasks ended (SessionGuard), and the
                    // empty-connection grace clock started then if this was
                    // the last live session — client data on the dead
                    // channel must not extend anything.
                    self.sessions.remove(&channel);
                    return Ok(());
                }
            }
        } else {
            debug!(channel = ?channel, len = data.len(), "data for channel with no session");
        }
        Ok(())
    }

    async fn channel_eof(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        debug!(channel = ?channel, "SSH channel EOF");
        if let Some(session) = self.sessions.get_mut(&channel) {
            session.client_tx.take();
        }
        Ok(())
    }

    async fn channel_close(
        &mut self,
        channel: ChannelId,
        _session: &mut Session,
    ) -> Result<(), Self::Error> {
        // r[impl gw.conn.channel-limit+4]
        // Only act on closes for channels this connection actually
        // accepted (see note_channel_closed): russh hands us a close for
        // any id the peer claims, and trusting it would let forged or
        // duplicate closes drain `open_channels` while russh's own
        // channel table keeps growing.
        if !self.note_channel_closed(channel) {
            return Ok(());
        }
        debug!(channel = ?channel, "SSH channel closed");
        // A channel that closes without ever exec'ing still has its write
        // half parked here; discard it. Exec'd channels' halves were moved
        // into their response pump at exec time, so this is a no-op for
        // them.
        self.channel_writers.remove(&channel);
        // Removing the entry aborts the response task (ChannelSession::
        // Drop), whose SessionGuard then releases the permit/gauge/live
        // count if the session was still live. r[gw.conn.exit-status+3]:
        // if that was the connection's last LIVE session, the guard's
        // drop arms the empty-connection grace — NOT an immediate
        // disconnect, because a ControlMaster's in-flight session count
        // legitimately transits through zero between builds, and killing
        // the transport there poisons the rest of the batch (the master
        // exits, OpenSSH unlinks the "stale" control socket, every
        // remaining nix process falls back to a corrupted direct
        // connection). A session that already ended server-side released
        // its slot back then; this close is pure bookkeeping.
        self.sessions.remove(&channel);
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify gw.conn.real-connection-marker]
    /// `auth_publickey_offered` MUST set `auth_attempted` even on the
    /// reject branch. RFC 4252 §5.2 makes the `none` probe optional, so
    /// a non-OpenSSH client that skips it and offers only unauthorized
    /// keys would otherwise leave `auth_attempted=false` → Drop logs it
    /// as a TCP probe, no metrics, no `r[gw.conn.cap]` enforcement.
    ///
    /// Regression: at b62291b8 this assertion fails (auth_attempted
    /// stays false — the only `auth_*` override that skipped
    /// `mark_real_connection`).
    #[tokio::test]
    async fn auth_publickey_offered_marks_real_on_reject() -> anyhow::Result<()> {
        use rio_test_support::grpc::{spawn_mock_scheduler, spawn_mock_store};
        use russh::keys::{Algorithm, PrivateKey};
        use russh::server::Server as _;

        let (_s, store_addr, _sh) = spawn_mock_store().await?;
        let (_d, sched_addr, _dh) = spawn_mock_scheduler().await?;
        let store = rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let logs: rio_proto::LogServiceClient<_> =
            rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let sched = rio_proto::client::connect_single(&sched_addr.to_string()).await?;

        // Server with ZERO authorized keys — every offer is rejected.
        let mut server = super::super::GatewayServer::new(store, logs, sched, vec![]);
        let mut handler = server.new_client(None);

        assert!(!handler.auth_attempted, "precondition: fresh handler");

        let unknown = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let res = handler
            .auth_publickey_offered("nix", unknown.public_key())
            .await?;
        assert!(
            matches!(res, Auth::Reject { .. }),
            "unknown key must be rejected at offer"
        );
        assert!(
            handler.auth_attempted,
            "auth_publickey_offered is an auth_* entry point — must mark_real_connection"
        );
        assert_eq!(
            handler.stage.load(Ordering::Relaxed),
            ConnStage::AuthAttempted as u8,
            "stage must advance to auth-attempted"
        );
        Ok(())
    }

    /// Fabricate a russh `ChannelId` for driving the per-channel
    /// bookkeeping helpers directly. russh keeps the constructor private
    /// but implements `ssh_encoding::Decode` (the wire form is a plain
    /// u32), which is all a test needs.
    fn channel_id(n: u32) -> ChannelId {
        use russh::keys::ssh_encoding::Decode;
        ChannelId::decode(&mut &n.to_be_bytes()[..]).expect("4 BE bytes decode as a ChannelId")
    }

    // r[verify gw.conn.channel-limit+4]
    /// bug_005: russh invokes `channel_close` for ANY channel id a peer
    /// puts in a `CHANNEL_CLOSE` — never-opened or already-closed ids
    /// included — so the `open_channels` bookkeeping must only count
    /// closes for channels this connection actually accepted.
    /// Otherwise an authenticated client interleaving real opens with
    /// forged closes holds `open_channels` near zero while russh
    /// channel-table entries grow without bound (defeating the
    /// per-connection absurdity bound), and duplicate closes skew the
    /// counter on legitimate connections.
    ///
    /// Tested at the bookkeeping-helper level: a `russh::server::Session`
    /// cannot be constructed outside russh, and the russh CLIENT refuses
    /// to emit forged or duplicate closes (`Encrypted::close`/`byte`
    /// no-op once the channel is gone from its table), so neither the
    /// callback level nor an end-to-end client can stage the hostile
    /// frames honestly. The helpers carry the entire counting logic; the
    /// callbacks are thin wrappers around them, and the legitimate
    /// open→close→slot-freed path stays covered end-to-end by
    /// `test_channel_open_slot_reuse_under_bound` in `ssh_hardening.rs`.
    #[tokio::test]
    async fn forged_or_duplicate_channel_close_does_not_skew_open_channels() -> anyhow::Result<()> {
        use rio_test_support::grpc::{spawn_mock_scheduler, spawn_mock_store};
        use russh::server::Server as _;

        let (_s, store_addr, _sh) = spawn_mock_store().await?;
        let (_d, sched_addr, _dh) = spawn_mock_scheduler().await?;
        let store = rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let log = rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let sched = rio_proto::client::connect_single(&sched_addr.to_string()).await?;
        let mut server = super::super::GatewayServer::new(store, log, sched, vec![]);
        let mut handler = server.new_client(None);

        let real = channel_id(1);
        let forged = channel_id(2);

        handler.note_channel_accepted(real);
        assert_eq!(handler.open_channels, 1, "accepted open must be counted");

        // A close for a channel that was never accepted must be ignored.
        assert!(
            !handler.note_channel_closed(forged),
            "a CHANNEL_CLOSE for a never-accepted channel must be ignored"
        );
        assert_eq!(
            handler.open_channels, 1,
            "a forged close must not decrement open_channels"
        );

        // The real close is counted exactly once and stops being tracked.
        assert!(
            handler.note_channel_closed(real),
            "the close of an accepted channel must be counted"
        );
        assert_eq!(handler.open_channels, 0);
        assert!(
            handler.accepted_channels.is_empty(),
            "closing must remove the tracking entry"
        );

        // A duplicate close of the already-closed channel must be ignored.
        assert!(
            !handler.note_channel_closed(real),
            "a duplicate CHANNEL_CLOSE must be ignored"
        );
        assert_eq!(
            handler.open_channels, 0,
            "a duplicate close must not skew the counter"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // finish_channel_session — the response task's end-of-session tail.
    // Driven against a REAL russh session handle whose receiver never
    // drains: russh only drains handle messages between key exchanges, and
    // these sessions never complete the initial exchange (the "client" is a
    // raw duplex half that sends its banner and nothing else), which is the
    // same parked-queue condition a hostile peer creates by stalling a
    // rekey. Staging an authenticated client that parks a mid-session
    // rekey end-to-end is not possible with the available plumbing (the
    // russh client exposes no "start a rekey and stall" control, and a raw
    // TCP client would need a full SSH implementation to authenticate), so
    // the guarantee is pinned here at the unit level instead.
    // -----------------------------------------------------------------------

    /// Spawn a real russh server session over an in-memory duplex whose
    /// handle queue is permanently parked (initial key exchange never
    /// completes), and fill that queue to capacity. Returns the handle, the
    /// number of messages it took to fill the queue, and the client half of
    /// the duplex — KEEP IT ALIVE: dropping it EOFs the transport, the
    /// session loop exits, the receiver drops, and every parked send fails
    /// instead of blocking (which would let a wrong implementation pass).
    async fn parked_handle_with_full_queue()
    -> anyhow::Result<(russh::server::Handle, usize, tokio::io::DuplexStream)> {
        use rio_test_support::grpc::{spawn_mock_scheduler, spawn_mock_store};
        use russh::keys::{Algorithm, PrivateKey};
        use russh::server::Server as _;

        let (_s, store_addr, _sh) = spawn_mock_store().await?;
        let (_d, sched_addr, _dh) = spawn_mock_scheduler().await?;
        let store = rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let log = rio_proto::client::connect_single(&store_addr.to_string()).await?;
        let sched = rio_proto::client::connect_single(&sched_addr.to_string()).await?;
        let mut server = super::super::GatewayServer::new(store, log, sched, vec![]);
        let handler = server.new_client(None);

        let host_key = PrivateKey::random(&mut rand::rng(), Algorithm::Ed25519)?;
        let mut config = super::super::build_ssh_config(host_key);
        // The production keepalive policy would end this completely silent
        // session on its own once enough (virtual) time passes, and then
        // the parked sends would FAIL fast instead of blocking. The peer
        // these tests model defeats exactly that backstop: every packet it
        // trickles resets the liveness clocks while the queue still never
        // drains. Disable the liveness timers so the parked state holds
        // across paused-clock advances, as it does against that peer.
        config.keepalive_interval = None;
        config.inactivity_timeout = None;
        let config = Arc::new(config);

        let (mut client_io, server_io) = tokio::io::duplex(64 * 1024);
        // run_stream reads the client identification string before
        // spawning the session loop; provide one, then go silent so the
        // server-initiated initial key exchange stays active forever.
        client_io.write_all(b"SSH-2.0-rio-test-parked\r\n").await?;
        let running = russh::server::run_stream(config, server_io, handler).await?;
        let handle = running.handle();

        // Fill the handle queue until a send no longer completes. With the
        // exchange active the receiver arm of russh's session loop is
        // disabled, so nothing ever drains what we queue here.
        let filler = channel_id(99);
        let mut filled = 0usize;
        loop {
            match tokio::time::timeout(
                std::time::Duration::from_millis(50),
                handle.exit_status_request(filler, 0),
            )
            .await
            {
                Ok(Ok(())) => filled += 1,
                Ok(Err(())) => anyhow::bail!("russh receiver dropped while filling the queue"),
                Err(_) => break,
            }
            anyhow::ensure!(
                filled < 1024,
                "handle queue never filled; russh appears to drain it during a key exchange now"
            );
        }
        Ok((handle, filled, client_io))
    }

    /// Build a [`SessionGuard`] holding the only permit of `sem`.
    fn guard_holding_only_permit(
        sem: &Arc<Semaphore>,
        handle: russh::server::Handle,
    ) -> SessionGuard {
        let force_close = Arc::new(super::super::ForceClose::new());
        SessionGuard {
            _permit: Arc::clone(sem)
                .try_acquire_owned()
                .expect("fresh semaphore must have its permit available"),
            timer: Arc::new(EmptyConnectionTimer::new(force_close)),
            handle,
            peer: None,
            grace: std::time::Duration::from_secs(60),
        }
    }

    // r[verify gw.conn.session-cap+2]
    /// The session permit (and gauge/live-count, all owned by the
    /// [`SessionGuard`]) MUST be released before — and independently of —
    /// the exit-status/eof/close sends: those ride the per-connection
    /// handle queue, which here is full and never drained, exactly like a
    /// peer that parks a rekey. The deliberately huge close-out timeout
    /// means a wrong ordering (guard dropped after the sends) can only
    /// release the permit after that timeout, which the test window never
    /// reaches — so a pass here proves the release does not depend on the
    /// handle queue at all.
    #[tokio::test]
    async fn ended_session_releases_permit_with_parked_handle_queue() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());
        assert_eq!(sem.available_permits(), 0, "guard must hold the permit");

        let finish = tokio::spawn(finish_channel_session(
            handle,
            channel_id(1),
            guard,
            tokio::spawn(async {}),
            std::time::Duration::from_secs(600),
            Arc::new(super::super::ForceClose::new()),
        ));

        // The permit must come back promptly even though the close-out can
        // never complete (600 s timeout, queue never drained).
        let mut released = false;
        for _ in 0..200 {
            if sem.available_permits() == 1 {
                released = true;
                break;
            }
            tokio::time::sleep(std::time::Duration::from_millis(25)).await;
        }
        assert!(
            released,
            "the ended session's permit must be released even though the handle \
             queue is parked full and the close-out sends cannot complete"
        );
        finish.abort();
        Ok(())
    }

    // r[verify gw.conn.exit-status+3]
    // r[verify gw.conn.send-deadline]
    /// The close-out sends themselves MUST be bounded: with the handle
    /// queue parked full, `finish_channel_session` gives up after its
    /// close-out timeout instead of parking the response task forever
    /// (which would pin the task — and under the old ordering the permit —
    /// until the connection dies).
    #[tokio::test]
    async fn parked_close_out_is_abandoned_after_timeout() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());

        let finish = finish_channel_session(
            handle,
            channel_id(1),
            guard,
            tokio::spawn(async {}),
            std::time::Duration::from_millis(200),
            Arc::new(super::super::ForceClose::new()),
        );
        // Generous outer bound: the helper must return on its own once the
        // 200 ms close-out budget expires; without the bound it would park
        // here forever.
        tokio::time::timeout(std::time::Duration::from_secs(10), finish)
            .await
            .expect("finish_channel_session must give up on a parked handle queue");
        assert_eq!(
            sem.available_permits(),
            1,
            "the permit must have been released along the way"
        );
        Ok(())
    }

    /// Test sink that sends through `Handle::data` — the queue-only path
    /// with no window accounting. Lets the parked-handle-queue test below
    /// keep pinning exactly what it always pinned: a send that the
    /// per-connection handle queue never accepts must trip the send bound,
    /// independently of any window bookkeeping.
    struct HandleQueueSink {
        handle: russh::server::Handle,
        channel: ChannelId,
    }

    impl SessionSink for HandleQueueSink {
        async fn send(&self, data: Vec<u8>) -> Result<(), ()> {
            self.handle.data(self.channel, data).await.map_err(|_| ())
        }
    }

    // r[verify gw.conn.session-cap+2]
    // r[verify gw.conn.send-deadline]
    // r[verify gw.conn.force-close]
    /// A peer that parks the handle queue must not be able to pin session
    /// capacity through the response pump: each send is bounded by
    /// [`HANDLE_SEND_TIMEOUT`], and when that bound expires the
    /// pump must end the session (releasing the [`SessionGuard`]'s permit)
    /// and treat the transport as wedged (force-close armed). Without
    /// both, every wedged connection pins its sessions' permits until the
    /// pod restarts, and the empty-connection grace can never step in
    /// because the wedged guards themselves keep the live-session count
    /// positive.
    ///
    /// Real-time setup (mock gRPC, russh session, queue fill), then the
    /// clock is paused so the test can cross the production bound
    /// instantly. Uses the production constant on the real production
    /// task body ([`pump_session_responses`]).
    #[tokio::test]
    async fn stalled_response_send_releases_permit_and_arms_force_close() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());
        assert_eq!(sem.available_permits(), 0, "guard must hold the permit");
        let force_close = Arc::new(super::super::ForceClose::new());

        // Protocol output waiting to be forwarded. The writer half stays
        // alive so the pump can only leave its loop via the send bound —
        // never via EOF.
        let (outbound_reader, mut proto_writer) = tokio::io::duplex(64 * 1024);
        proto_writer.write_all(b"protocol response bytes").await?;

        tokio::time::pause();
        let pump = tokio::spawn(pump_session_responses(
            outbound_reader,
            HandleQueueSink {
                handle: handle.clone(),
                channel: channel_id(1),
            },
            handle,
            channel_id(1),
            guard,
            tokio::spawn(async {}),
            Arc::clone(&force_close),
            HANDLE_SEND_TIMEOUT,
        ));
        // Let the pump reach the parked send and register its timeout.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // Well inside the bound: a fair-share wait behind a slow but still
        // draining link must not be treated as a stall.
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            0,
            "a send still inside HANDLE_SEND_TIMEOUT must not end the session"
        );
        assert!(
            force_close.armed_deadline().is_none(),
            "a send still inside HANDLE_SEND_TIMEOUT must not arm the force-close"
        );

        // Past the bound: the stalled send must end the session (the
        // permit comes back via finish_channel_session) and arm the
        // connection's transport force-close.
        tokio::time::advance(HANDLE_SEND_TIMEOUT).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            1,
            "a send stalled past HANDLE_SEND_TIMEOUT must release the session permit"
        );
        assert!(
            force_close.armed_deadline().is_some(),
            "a send stalled past HANDLE_SEND_TIMEOUT must arm the transport force-close"
        );
        pump.abort();
        Ok(())
    }

    /// A sink whose send never resolves — the shape of a client that
    /// withholds every CHANNEL_WINDOW_ADJUST: the production write half
    /// parks waiting for window that never comes while TCP and keepalives
    /// stay healthy. Only a controllable fake can stage this — the write
    /// half has no public constructor, and a stock russh client grants
    /// window automatically as it reads.
    struct WindowStarvedSink;

    impl SessionSink for WindowStarvedSink {
        async fn send(&self, _data: Vec<u8>) -> Result<(), ()> {
            std::future::pending().await
        }
    }

    // r[verify gw.conn.session-cap+2]
    // r[verify gw.conn.send-deadline]
    // r[verify gw.conn.force-close]
    /// A client that keeps the transport healthy but never grants channel
    /// window must not be able to pin session capacity through the
    /// response pump: the data send parks on the never-granted window, and
    /// once it has been parked for [`HANDLE_SEND_TIMEOUT`] the pump must
    /// end the session (releasing the [`SessionGuard`]'s permit) and arm
    /// the transport force-close — the same wedge response as a parked
    /// handle queue. Driven through the pump's send seam with a sink whose
    /// send never resolves, which is how the production write half behaves
    /// while the client-granted window stays exhausted.
    #[tokio::test]
    async fn window_starved_send_releases_permit_and_arms_force_close() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());
        assert_eq!(sem.available_permits(), 0, "guard must hold the permit");
        let force_close = Arc::new(super::super::ForceClose::new());

        // Protocol output waiting to be forwarded. The writer half stays
        // alive so the pump can only leave its loop via the send bound —
        // never via EOF.
        let (outbound_reader, mut proto_writer) = tokio::io::duplex(64 * 1024);
        proto_writer.write_all(b"protocol response bytes").await?;

        tokio::time::pause();
        let pump = tokio::spawn(pump_session_responses(
            outbound_reader,
            WindowStarvedSink,
            handle,
            channel_id(1),
            guard,
            tokio::spawn(async {}),
            Arc::clone(&force_close),
            HANDLE_SEND_TIMEOUT,
        ));
        // Let the pump reach the parked send and register its timeout.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // Well inside the bound: a client that is merely slow to grant
        // window must not be treated as wedged.
        tokio::time::advance(std::time::Duration::from_secs(60)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            0,
            "a send still inside HANDLE_SEND_TIMEOUT must not end the session"
        );
        assert!(
            force_close.armed_deadline().is_none(),
            "a send still inside HANDLE_SEND_TIMEOUT must not arm the force-close"
        );

        // Past the bound: the window-starved send must end the session
        // (the permit comes back via finish_channel_session) and arm the
        // connection's transport force-close.
        tokio::time::advance(HANDLE_SEND_TIMEOUT).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert_eq!(
            sem.available_permits(),
            1,
            "a send starved of window past HANDLE_SEND_TIMEOUT must release the session permit"
        );
        assert!(
            force_close.armed_deadline().is_some(),
            "a send starved of window past HANDLE_SEND_TIMEOUT must arm the transport force-close"
        );
        pump.abort();
        Ok(())
    }

    // r[verify gw.conn.exit-status+3]
    // r[verify gw.conn.send-deadline]
    // r[verify gw.conn.force-close]
    /// The close-out budget must tolerate congestion and only condemn the
    /// transport once it is exhausted. A session ending is normal on a
    /// healthy multiplexed connection (one build of many finishing while
    /// its siblings keep streaming), so with the production budget
    /// ([`HANDLE_SEND_TIMEOUT`]) the close-out must still be pending a few
    /// seconds in — abandoning it that early on a merely congested link
    /// strands the client's foreground `ssh` and the `nix` invocation
    /// blocked on it. Once the budget expires against a genuinely parked
    /// queue, the helper must abandon the close-out, arm the force-close,
    /// and return.
    #[tokio::test]
    async fn parked_close_out_outlasts_congestion_then_arms_force_close() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());
        let force_close = Arc::new(super::super::ForceClose::new());

        tokio::time::pause();
        let finish = tokio::spawn(finish_channel_session(
            handle,
            channel_id(1),
            guard,
            tokio::spawn(async {}),
            HANDLE_SEND_TIMEOUT,
            Arc::clone(&force_close),
        ));
        // Let the helper park on the first close-out send.
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }

        // A few seconds in — the kind of delay ordinary congestion
        // produces — the close-out must still be pending and the
        // transport must not yet be condemned.
        tokio::time::advance(std::time::Duration::from_secs(6)).await;
        for _ in 0..16 {
            tokio::task::yield_now().await;
        }
        assert!(
            !finish.is_finished(),
            "the close-out must not be abandoned after a few seconds of queueing"
        );
        assert!(
            force_close.armed_deadline().is_none(),
            "the force-close must not be armed before the close-out budget expires"
        );

        // Past the full budget the queue is provably wedged: abandon the
        // close-out, arm the force-close, return.
        tokio::time::advance(HANDLE_SEND_TIMEOUT).await;
        tokio::time::timeout(std::time::Duration::from_secs(60), finish)
            .await
            .expect("finish_channel_session must return once the close-out budget expires")?;
        assert!(
            force_close.armed_deadline().is_some(),
            "an exhausted close-out budget must arm the transport force-close"
        );
        assert_eq!(
            sem.available_permits(),
            1,
            "the permit must have been released before the close-out attempt"
        );
        Ok(())
    }

    /// The client pump only exits on its own when the CLIENT acts (channel
    /// close / EOF / connection end). Once the protocol session is over the
    /// reader side of its pipe is gone, so `finish_channel_session` MUST
    /// reap the pump rather than wait for it: waiting would retain the
    /// pump, the response task, and their buffers for as long as the peer
    /// pleases — uncounted by the session cap, whose permit was already
    /// released. The pump here waits on a channel whose sender the test
    /// keeps open (the shape of a peer that never closes its channel), so
    /// only the reap can let `finish_channel_session` return.
    #[tokio::test]
    async fn finish_reaps_client_pump_that_never_exits_on_its_own() -> anyhow::Result<()> {
        let (handle, _filled, _client_io) = parked_handle_with_full_queue().await?;
        let sem = Arc::new(Semaphore::new(1));
        let guard = guard_holding_only_permit(&sem, handle.clone());

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(1);
        let client_pump = tokio::spawn(async move { while client_rx.recv().await.is_some() {} });

        let finish = finish_channel_session(
            handle,
            channel_id(1),
            guard,
            client_pump,
            std::time::Duration::from_millis(200),
            Arc::new(super::super::ForceClose::new()),
        );
        // Bounded from the outside: without the reap, the helper parks on
        // the pump join until the peer closes the channel — i.e. forever
        // here — and this timeout (not the harness) reports it.
        tokio::time::timeout(std::time::Duration::from_secs(10), finish)
            .await
            .expect(
                "finish_channel_session must reap the client pump instead of waiting \
                 for the peer to close the channel",
            );
        // Keep the sender alive to the very end so the pump can never have
        // exited on its own — only the reap can have ended it.
        drop(client_tx);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // normalize_key_comment — the extracted tenant-name normalization
    // helper. Tests all three NameError branches + the counter emit.
    // -----------------------------------------------------------------------

    // r[verify gw.auth.tenant-from-key-comment]
    /// T4 regression for P0367-T1: interior-whitespace comment (e.g.,
    /// `team a` typo'd from `team-a`) degrades to single-tenant (None)
    /// but BUMPS `rio_gateway_auth_degraded_total{reason=
    /// interior_whitespace}`. Before the fix, `from_maybe_empty`
    /// silently returned None — the operator never learned their
    /// tenant isolation was off.
    ///
    /// Mutation-checked: replacing the `InteriorWhitespace` arm with
    /// a bare `=> None` (no warn, no counter) fails the counter
    /// assertion below.
    #[test]
    fn interior_whitespace_comment_warns_and_degrades() {
        use rio_test_support::metrics::CountingRecorder;

        let recorder = CountingRecorder::default();
        let result = metrics::with_local_recorder(&recorder, || {
            normalize_key_comment(b"team a", &"SHA256:test-fingerprint")
        });

        // Degrades to single-tenant:
        assert_eq!(result, None, "interior-ws must degrade to single-tenant");
        // But counter bumped — the misconfig is alertable:
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            1,
            "interior-ws must bump auth_degraded counter; saw keys: {:?}",
            recorder.all_keys()
        );
    }

    /// Positive control for the above: a valid comment produces
    /// `Some(name)` and does NOT bump the counter. Without this, the
    /// interior-whitespace test above could pass while the helper
    /// unconditionally returns None (e.g., if the match was written
    /// with the Ok arm unreachable).
    #[test]
    fn valid_comment_returns_some_no_counter() {
        use rio_test_support::metrics::CountingRecorder;

        let recorder = CountingRecorder::default();
        let result =
            metrics::with_local_recorder(&recorder, || normalize_key_comment(b"  team-a  ", &"fp"));

        assert_eq!(
            result.as_deref(),
            Some("team-a"),
            "valid comment should be trimmed+Some"
        );
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            0,
            "valid comment must NOT bump the degrade counter"
        );
    }

    /// Empty comment → None, no counter. Intentional single-tenant
    /// mode — the operator left the comment blank on purpose. Distinct
    /// from interior-whitespace (misconfig): empty is quiet, interior-
    /// ws is loud. Proves the two Err variants are branched separately.
    #[test]
    fn empty_comment_returns_none_no_counter() {
        use rio_test_support::metrics::CountingRecorder;

        let recorder = CountingRecorder::default();
        let result = metrics::with_local_recorder(&recorder, || normalize_key_comment(b"", &"fp"));

        assert_eq!(result, None, "empty comment → single-tenant (None)");
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            0,
            "empty comment is INTENTIONAL single-tenant — no counter"
        );
        // Also whitespace-only (trims to empty → Empty variant):
        let ws_result =
            metrics::with_local_recorder(&recorder, || normalize_key_comment(b"   ", &"fp"));
        assert_eq!(ws_result, None);
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            0,
            "whitespace-only → Empty (not InteriorWhitespace) → no counter"
        );
    }

    /// Non-UTF-8 comment → None + counter. ssh-key 0.7 comments are raw
    /// bytes (RFC 4251); a binary/corrupted comment is a misconfigured
    /// entry, not a tenant identity. Strict rejection (vs `as_str_lossy`'s
    /// longest-valid-prefix) so `team-alpha\xFF…` can't silently become
    /// tenant `team-alpha`.
    #[test]
    fn invalid_utf8_comment_warns_and_degrades() {
        use rio_test_support::metrics::CountingRecorder;

        let recorder = CountingRecorder::default();
        let result = metrics::with_local_recorder(&recorder, || {
            normalize_key_comment(b"team-alpha\xff\xfe", &"fp")
        });

        assert_eq!(result, None, "invalid UTF-8 must degrade to single-tenant");
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=invalid_utf8}"),
            1,
            "invalid UTF-8 must bump auth_degraded counter; saw keys: {:?}",
            recorder.all_keys()
        );
    }
}
