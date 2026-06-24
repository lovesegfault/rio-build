//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel, delegating operations
//! to gRPC store and scheduler services.

mod connection;
mod keys;
pub(crate) mod session_jwt;

pub use connection::ConnectionHandler;
pub use keys::{
    AUTHORIZED_KEYS_POLL_INTERVAL, AuthorizedKeys, load_authorized_keys, load_or_generate_host_key,
    spawn_authorized_keys_watcher,
};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::pin::Pin;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
use std::task::{Context as TaskContext, Poll};
use std::time::Duration;

use anyhow::Context;
use arc_swap::ArcSwap;
use ed25519_dalek::SigningKey;
use rio_common::config::JwtConfig;
use rio_common::signal::Token as CancellationToken;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use russh::keys::{PrivateKey, PublicKey};
use russh::server::{Server as _, run_stream};
use russh::{Disconnect, MethodKind, MethodSet, Preferred, compression};
use tokio::io::{AsyncRead, AsyncWrite, ReadBuf};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;

/// Default global connection cap (`r[gw.conn.cap]`). At this many
/// concurrent SSH connections, new accepts are rejected immediately.
/// Bounds per-connection russh state and file descriptors; the memory
/// consumed by protocol sessions is bounded separately by
/// [`DEFAULT_MAX_SESSIONS`]. Configurable via `gateway.toml
/// max_connections`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1000;

/// Default global active-session cap (`r[gw.conn.session-cap+2]`). Each
/// exec'd protocol session allocates 2×256 KiB duplex buffers + a
/// 64-slot mpsc + 3 task stacks (~550 KiB worst case), so 4096 sessions
/// ≈ 2.2 GiB of the pod's 4 GiB limit. Sized to ~2× the largest
/// plausible legitimate burst (a few CI machines × 64–128
/// nix-fast-build workers each) so it only fires under genuine
/// overload, where a clean per-exec rejection beats an OOMKill of every
/// other session.
///
/// The cap is the pod's memory backstop only because per-session egress
/// is separately flow-controlled: the response pump hands russh nothing
/// the client has not granted SSH channel window for (the window-aware
/// write half blocks otherwise, bounded by `HANDLE_SEND_TIMEOUT`), so
/// russh-side buffering per session stays around one client-advertised
/// window plus the bounded handle queue instead of growing with the
/// response stream. The one large per-session allocation outside both
/// bounds is the transient NAR buffer a `wopNarFromPath` holds while in
/// flight (≤ `MAX_NAR_SIZE`; the protocol cannot signal an error once
/// raw NAR bytes start, so the fetch must complete before streaming).
///
/// Checked in `exec_request`, NOT `channel_open_session`: an exec-time
/// `channel_failure` is a clean `ssh` exit for a ControlMaster mux
/// client, while a channel-open refusal makes OpenSSH silently fall
/// back to a direct connection that nix's `LocalCommand` corrupts.
/// Configurable via `gateway.toml max_sessions`.
///
/// Horizontal scaling note: this is a per-pod cap. Adding replicas adds
/// aggregate capacity for additional client *connections*, but a
/// ControlMaster pins all of its channels to one pod's TCP connection —
/// no amount of scale-out redistributes an already-multiplexed client.
pub const DEFAULT_MAX_SESSIONS: usize = 4096;

/// Default per-connection SSH channel bound
/// (`r[gw.conn.channel-limit+4]`). An absurdity detector, not a
/// resource bound: an attacker distributes sessions across the
/// [`DEFAULT_MAX_CONNECTIONS`] allowed connections, so only the global
/// [`DEFAULT_MAX_SESSIONS`] semaphore bounds pod memory. 512 covers a
/// 128-core CI machine running nix-fast-build behind one ControlMaster
/// with 4× headroom; its real job is stopping a burst of CHANNEL_OPENs
/// with no exec (each allocates a russh channel-table entry but no
/// ChannelSession) from growing without bound — a connection that
/// tries to exceed it is terminated. Configurable via
/// `gateway.toml max_channels_per_connection`.
pub const DEFAULT_MAX_CHANNELS_PER_CONNECTION: usize = 512;

/// Grace period a connection may have zero active protocol sessions —
/// measured from authentication (established, nothing exec'd yet) or
/// from the last session ending — before the gateway disconnects it
/// (`r[gw.conn.exit-status+3]`). Keyed on exec'd sessions, not open SSH
/// channels: a channel that is opened but never exec'd has no protocol
/// task and no deadline of its own, so it must not count as activity.
/// The same duration also bounds the pre-auth phase: a connection that
/// has not authenticated within it is disconnected by the accept-site
/// deadline in [`GatewayServer::run_on_listener`] (russh has no
/// login-grace of its own, and an answering-but-idle client never trips
/// `inactivity_timeout`/`keepalive_max`), with [`FORCE_CLOSE_SLACK`]
/// bounding how long that disconnect may remain undeliverable before the
/// transport is closed outright.
///
/// A ControlMaster mux whose in-flight session count transits through
/// zero between builds must NOT lose its transport — the master would
/// exit and every remaining nix process in the batch would fall back to
/// a corrupted direct connection. 60 s comfortably covers inter-build
/// gaps, the auth-to-first-exec delay of any real nix client
/// (milliseconds), and any legitimate auth handshake (OpenSSH's own
/// LoginGraceTime default is 120 s); a genuinely abandoned or never-used
/// connection is still reaped 60× sooner than `inactivity_timeout`,
/// which an idle-but-keepalive-answering client never trips.
/// Overridable for tests via `with_empty_connection_grace`.
pub const EMPTY_CONNECTION_GRACE: std::time::Duration = std::time::Duration::from_secs(60);

/// Extra time the gateway gives a peer to act on a polite
/// `SSH_MSG_DISCONNECT` before it force-closes the transport
/// (`r[gw.conn.exit-status+3]`).
///
/// A queued disconnect is best-effort twice over: russh only drains handle
/// messages between key exchanges — a peer that keeps a key exchange
/// perpetually in flight (banner, then trickled `SSH_MSG_IGNORE`s and never
/// a KEX reply) defers delivery indefinitely while also resetting the
/// keepalive/inactivity timers with every packet it sends — and a peer that
/// does receive it can simply never close its end, parking russh's
/// post-disconnect drain-read loop (which has no timeout and arms no
/// keepalives) forever. So every site that decides a connection must go
/// away arms the transport-level `ConnDeadline` this far in the future:
/// the pre-auth deadline arms it at accept (covering the
/// never-authenticated population), the auth-timeout and
/// empty-connection-grace disconnects arm it the moment they are queued
/// (the former matters because an authentication that completes with that
/// disconnect still queued takes the connection out of the pre-auth
/// deadline's reach), and a session whose sends to the client (channel
/// data through the window-aware writer, or the close-out on the handle
/// queue) have stalled past `HANDLE_SEND_TIMEOUT` (connection.rs) arms it
/// with no disconnect at all — the peer is either not draining the queue a
/// disconnect would ride or not acting on what it is sent. Once the
/// deadline passes, the transport read or write fails,
/// russh's session loop (or its drain loop) returns, and the handler +
/// stream drop — releasing the permit, fd, and gauges exactly once through
/// the normal drop path. A few seconds is plenty for any compliant peer to
/// have closed; only one gaming the key exchange or squatting on the
/// socket ever reaches the hard close.
pub const FORCE_CLOSE_SLACK: std::time::Duration = std::time::Duration::from_secs(5);

/// Kernel-level bound on how long transmitted-but-unacknowledged data (or
/// a persistent zero receive window) may pend on an accepted connection
/// before the kernel errors the socket out (`TCP_USER_TIMEOUT`, applied at
/// accept by [`set_tcp_user_timeout`]).
///
/// It exists because every in-process bound assumes the session loop keeps
/// getting polled: a peer that stops reading at the TCP level parks russh
/// inline in a bulk channel-data write, and from that moment neither the
/// protocol keepalive nor any timer this process arms afterwards can run —
/// the kernel is the only clock left holding the connection's fate. Set
/// equal to the SSH keepalive bound (30 s interval × (9+1) missed replies
/// ≈ 300 s, see [`build_ssh_config`]), the documented tolerance for an
/// unresponsive peer the gateway has NOT decided to disconnect, so an
/// honest-but-flaky client gains no new failure window from this option —
/// the connection dies at the same point keepalive would have killed it
/// anyway.
const TCP_USER_TIMEOUT: Duration = Duration::from_secs(300);

/// The SSH server that accepts connections and spawns protocol sessions.
pub struct GatewayServer {
    store_client: StoreServiceClient<Channel>,
    /// `LogService` client for the build-log live tail. Same upstream
    /// (the store's port 9002) as `store_client`; threaded separately
    /// because tonic's generated clients don't expose their inner
    /// channel for re-wrapping.
    log_client: rio_proto::LogServiceClient<Channel>,
    scheduler_client: SchedulerServiceClient<Channel>,
    /// Hot-swappable key set. [`spawn_authorized_keys_watcher`] holds
    /// another `Arc` to the same `ArcSwap` and `.store()`s a fresh
    /// `Vec` when the backing file changes; every `ConnectionHandler`
    /// `.load()`s the current set per auth attempt (I-109).
    authorized_keys: AuthorizedKeys,
    /// ed25519 JWT signing key. `None` → JWT issuance disabled (the
    /// `x-rio-tenant-token` header is never set; downstream services
    /// fall back to `SubmitBuildRequest.tenant_name` per
    /// `r[gw.jwt.dual-mode]`). `Some` → every accepted SSH connection
    /// attempts a ResolveTenant round-trip + mint.
    jwt_signing_key: Option<Arc<SigningKey>>,
    /// JWT policy — `required` controls whether mint failure is fatal
    /// (reject SSH auth) or degradable (fall back to tenant_name).
    /// Cloned into every ConnectionHandler.
    jwt_config: JwtConfig,
    /// ResolveTenant RPC timeout — gateway-only knob, lives here rather
    /// than on `JwtConfig` (scheduler/store never read it).
    resolve_timeout: std::time::Duration,
    /// Service-identity HMAC signer (`RIO_SERVICE_HMAC_KEY_PATH`).
    /// Cloned into every `SessionContext` so write opcodes can attach
    /// `x-rio-service-token` on store `PutPath`. `None` = disabled.
    service_signer: Option<Arc<rio_auth::hmac::HmacSigner>>,
    /// Per-tenant build-submit rate limiter keyed on `tenant_name`
    /// (authorized_keys comment). Disabled by default. Clones share
    /// state (inner `Arc`), so the tenant's bucket is counted across
    /// all their concurrent SSH connections, not per-connection.
    /// See `r[gw.rate.per-tenant]`.
    limiter: TenantLimiter,
    /// Per-tenant store-quota cache (30s TTL). Clones share state
    /// — a quota reading fetched by one connection is warm for all.
    /// Always enabled: single-tenant mode (empty `tenant_name`)
    /// skips the check inside the cache, so there's no disabled
    /// variant. See `r[store.gc.tenant-quota-enforce]`.
    quota_cache: QuotaCache,
    // r[impl gw.conn.cap]
    /// Global connection cap. `try_acquire_owned()` in `new_client`;
    /// the permit is moved into the `ConnectionHandler` and dropped
    /// on disconnect. At cap: `new_client` returns a handler with
    /// `conn_permit: None`, and `auth_none` (the first callback a
    /// real SSH client fires) rejects with a clear error before any
    /// further work. `russh::Server::new_client` has no "reject at
    /// accept" hook — this is the earliest gate.
    ///
    /// Default [`DEFAULT_MAX_CONNECTIONS`] = 1000; override via
    /// `with_max_connections()`.
    conn_sem: Arc<Semaphore>,
    // r[impl gw.conn.session-cap+2]
    /// Global active-session semaphore. One permit per spawned protocol
    /// session across all connections; acquired in `exec_request` and
    /// owned by the session's `SessionGuard` (held by its response
    /// task), so it is released when the protocol session actually ends
    /// — server- or client-side — or when the session is torn down early
    /// and the aborted task drops the guard. Default
    /// [`DEFAULT_MAX_SESSIONS`]; override via `with_max_sessions()`.
    session_sem: Arc<Semaphore>,
    /// Per-connection SSH channel absurdity bound. Default
    /// [`DEFAULT_MAX_CHANNELS_PER_CONNECTION`]; override via
    /// `with_max_channels_per_connection()`.
    max_channels_per_connection: usize,
    /// See [`EMPTY_CONNECTION_GRACE`]. Overridable for tests via
    /// `with_empty_connection_grace()`.
    empty_connection_grace: std::time::Duration,
    /// Max wait for `WORKER_MAGIC_1` on an exec'd channel before the
    /// protocol session ends server-side. Default
    /// [`crate::session::HANDSHAKE_TIMEOUT`]; overridable for tests via
    /// `with_handshake_timeout()` so the exec'd-but-silent scenario
    /// resolves in milliseconds instead of 30 s.
    handshake_timeout: std::time::Duration,
    /// Count of REAL (post-auth-handshake) connections currently open.
    /// Same lifecycle as the `rio_gateway_connections_active` gauge —
    /// incremented in [`ConnectionHandler::mark_real_connection`],
    /// decremented in `Drop`. Exposed via [`Self::active_conns_handle`]
    /// so `main.rs` can poll for session-drain after the accept loop
    /// stops (I-064: previously, dropping `run()` disconnected all
    /// sessions; now main awaits this → 0 OR a timeout before exit).
    /// Separate from `conn_sem`: the semaphore counts permits including
    /// briefly-held ones for TCP probes; this counts only sessions that
    /// reached an `auth_*` callback.
    active_conns: Arc<AtomicUsize>,
    /// Parent of every per-channel `ChannelSession::shutdown` token.
    /// Cancelling this cascades to all proto_tasks, each of which runs
    /// `cancel_active_builds` (session.rs:221) so the scheduler hears
    /// `CancelBuild` for every in-flight build before process exit.
    /// I-081: previously each channel created an isolated root token —
    /// the drain timeout in main.rs just exited, leaking builds Active.
    sessions_shutdown: CancellationToken,
}

impl GatewayServer {
    pub fn new(
        store_client: StoreServiceClient<Channel>,
        log_client: rio_proto::LogServiceClient<Channel>,
        scheduler_client: SchedulerServiceClient<Channel>,
        authorized_keys: Vec<PublicKey>,
    ) -> Self {
        if authorized_keys.is_empty() {
            warn!("no authorized keys configured; all SSH connections will be rejected");
        }
        GatewayServer {
            store_client,
            log_client,
            scheduler_client,
            authorized_keys: Arc::new(ArcSwap::from_pointee(authorized_keys)),
            jwt_signing_key: None,
            jwt_config: JwtConfig::default(),
            resolve_timeout: std::time::Duration::from_millis(500),
            service_signer: None,
            limiter: TenantLimiter::disabled(),
            quota_cache: QuotaCache::new(),
            conn_sem: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
            session_sem: Arc::new(Semaphore::new(DEFAULT_MAX_SESSIONS)),
            max_channels_per_connection: DEFAULT_MAX_CHANNELS_PER_CONNECTION,
            empty_connection_grace: EMPTY_CONNECTION_GRACE,
            handshake_timeout: crate::session::HANDSHAKE_TIMEOUT,
            active_conns: Arc::new(AtomicUsize::new(0)),
            sessions_shutdown: CancellationToken::new(),
        }
    }

    /// Clone of the live-connection counter. Call BEFORE [`Self::run`]
    /// (which consumes `self`) so the caller can poll for session
    /// drain after the accept loop returns. Returns `Arc` not `usize`
    /// so the caller observes drops that happen post-`run`.
    pub fn active_conns_handle(&self) -> Arc<AtomicUsize> {
        Arc::clone(&self.active_conns)
    }

    /// Clone of the hot-swappable authorized-key set. Hand this to
    /// [`spawn_authorized_keys_watcher`] (or, in tests, `.store()` on
    /// it directly) so file changes propagate to running auth checks
    /// without a restart. Call BEFORE [`Self::run`] (consumes `self`).
    pub fn authorized_keys_handle(&self) -> AuthorizedKeys {
        Arc::clone(&self.authorized_keys)
    }

    /// Clone of the server-wide session shutdown token. Cancelling it
    /// cascades to every open channel's proto_task, which runs
    /// `cancel_active_builds` before returning. main.rs fires this
    /// when `session_drain_secs` expires with sessions still open
    /// (I-081), so the scheduler gets `CancelBuild` instead of the
    /// builds being leaked Active until 24h TTL.
    pub fn sessions_shutdown_handle(&self) -> CancellationToken {
        self.sessions_shutdown.clone()
    }

    /// Enable per-tenant rate limiting. Until called, `TenantLimiter`
    /// is the disabled variant (every `check()` passes). Builder-style
    /// so main.rs composes alongside `with_jwt_signing_key`.
    pub fn with_rate_limiter(mut self, limiter: TenantLimiter) -> Self {
        self.limiter = limiter;
        self
    }

    /// Override the global connection cap. Default
    /// [`DEFAULT_MAX_CONNECTIONS`]. Must be called before `run()` —
    /// replaces the semaphore, losing any already-acquired permits
    /// (there are none before `run()`).
    pub fn with_max_connections(mut self, max: usize) -> Self {
        self.conn_sem = Arc::new(Semaphore::new(max));
        self
    }

    /// Override the global active-session cap. Default
    /// [`DEFAULT_MAX_SESSIONS`]. Must be called before `run()` —
    /// replaces the semaphore, losing any already-acquired permits
    /// (there are none before `run()`).
    pub fn with_max_sessions(mut self, max: usize) -> Self {
        self.session_sem = Arc::new(Semaphore::new(max));
        self
    }

    /// Override the per-connection SSH channel absurdity bound. Default
    /// [`DEFAULT_MAX_CHANNELS_PER_CONNECTION`].
    pub fn with_max_channels_per_connection(mut self, max: usize) -> Self {
        self.max_channels_per_connection = max;
        self
    }

    /// Override the idle-connection grace period. Default
    /// [`EMPTY_CONNECTION_GRACE`]. Exposed so tests can use a short
    /// grace without waiting 60 s for the disconnect to fire.
    pub fn with_empty_connection_grace(mut self, grace: std::time::Duration) -> Self {
        self.empty_connection_grace = grace;
        self
    }

    /// Override the protocol handshake timeout (max wait for
    /// `WORKER_MAGIC_1` on an exec'd channel). Default
    /// [`crate::session::HANDSHAKE_TIMEOUT`]. Exposed so tests can make
    /// an exec'd-but-never-speaking channel end its protocol session
    /// server-side in milliseconds instead of 30 s.
    pub fn with_handshake_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.handshake_timeout = timeout;
        self
    }

    /// Set the ResolveTenant RPC timeout. Bounds the auth-time latency
    /// penalty when the scheduler is slow or unreachable — the RPC sits
    /// in the SSH auth hot path (every connect, once). Default 500ms.
    /// Builder-style so main.rs composes alongside `with_jwt_signing_key`.
    pub fn with_resolve_timeout(mut self, timeout: std::time::Duration) -> Self {
        self.resolve_timeout = timeout;
        self
    }

    /// Enable JWT issuance. Until called, `auth_publickey` accepts
    /// without minting (dual-mode fallback path). After: every
    /// accepted connection attempts a ResolveTenant round-trip +
    /// `session_jwt::mint_session_jwt`. Whether mint FAILURE is fatal depends
    /// on `config.required`.
    ///
    /// Builder-style (`self` → `Self`) so main.rs composes it:
    /// `GatewayServer::new(...).with_jwt_signing_key(k, cfg)`. Keeps
    /// `new()` stable for existing call sites (tests, VM fixtures)
    /// that don't care about JWT.
    pub fn with_jwt_signing_key(mut self, key: SigningKey, config: JwtConfig) -> Self {
        self.jwt_signing_key = Some(Arc::new(key));
        self.jwt_config = config;
        self
    }

    /// Enable `x-rio-service-token` minting on store `PutPath`. Until
    /// called, write opcodes attach no `x-rio-service-token` (store
    /// rejects unless its verifier is also unconfigured). Builder-style.
    pub fn with_service_hmac_signer(mut self, signer: rio_auth::hmac::HmacSigner) -> Self {
        self.service_signer = Some(Arc::new(signer));
        self
    }

    /// Start the SSH accept loop on the given address. Returns when
    /// `serve_shutdown` fires; spawned per-connection tasks CONTINUE
    /// running detached after return — they hold `active_conns` and
    /// release on disconnect, so the caller polls [`Self::
    /// active_conns_handle`] → 0 to know when all sessions have ended.
    ///
    /// # Why not `russh::server::Server::run_on_socket`
    ///
    /// I-064: `run_on_socket` couples accept-stop to session-disconnect
    /// via a single broadcast channel — dropping its future (or
    /// `RunningServerHandle::shutdown`) drops `shutdown_tx`, every
    /// spawned session's `select!` arm fires `handle.disconnect()`.
    /// A gateway rollout (k8s `kubectl rollout restart`) thus killed
    /// every in-flight `nix build --store ssh-ng://` client with `Nix
    /// daemon disconnected unexpectedly`. The cluster-side build
    /// survives (gateway → scheduler `WatchBuild` reconnects), but the
    /// client doesn't.
    ///
    /// This loop decouples: `serve_shutdown` cancellation breaks the
    /// accept `select!`, but spawned [`run_stream`] tasks have no
    /// shutdown subscription — they run to natural completion (client
    /// EOF, error, or process exit at `terminationGracePeriodSeconds`).
    ///
    /// Transient `accept()` errors (ECONNABORTED, EMFILE, …) are
    /// logged-and-continued; the loop only returns on `serve_shutdown`
    /// (or a `bind()` failure before the loop starts). A `?` here would
    /// reproduce the I-064 outcome via process-exit: main.rs `?`s past
    /// `wait_for_session_drain` and every detached session aborts.
    pub async fn run(
        self,
        host_key: PrivateKey,
        addr: SocketAddr,
        serve_shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        info!(addr = %addr, "starting SSH server");

        let socket = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind SSH server to {addr}"))?;

        self.run_on_listener(host_key, socket, serve_shutdown).await
    }

    /// The accept loop behind [`Self::run`], on an already-bound
    /// listener. Split out so integration tests can exercise the REAL
    /// production accept path (including the pre-auth deadline below)
    /// against an ephemeral `127.0.0.1:0` listener whose port they know.
    /// See [`Self::run`] for the lifecycle/decoupling contract.
    // r[impl gw.conn.session-drain]
    pub async fn run_on_listener(
        mut self,
        host_key: PrivateKey,
        socket: TcpListener,
        serve_shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let config = Arc::new(build_ssh_config(host_key));

        loop {
            let (stream, peer) = tokio::select! {
                // biased: check shutdown first so a pending accept() never
                // sneaks one more connection through after cancellation.
                biased;
                () = serve_shutdown.cancelled() => {
                    info!("SSH accept loop: serve_shutdown received, stopping accept");
                    return Ok(());
                }
                r = socket.accept() => match r {
                    Ok(pair) => pair,
                    // r[impl gw.conn.accept-resilience]
                    Err(e) => {
                        warn!(error = %e, "SSH accept failed; retrying");
                        metrics::counter!("rio_gateway_errors_total", "type" => "accept")
                            .increment(1);
                        if let AcceptErrAction::RetryAfter(d) = classify_accept_error(&e) {
                            tokio::time::sleep(d).await;
                        }
                        continue;
                    }
                },
            };
            let handler = self.new_client(Some(peer));
            let stage = Arc::clone(&handler.stage);
            let force_close = Arc::clone(&handler.force_close);
            let config = Arc::clone(&config);
            let auth_grace = self.empty_connection_grace;
            // Detached: NOT coupled to accept-loop lifetime. The handler
            // holds `active_conns` (via `mark_real_connection`/`Drop`),
            // so main.rs's drain poll observes natural session end.
            // `handle_session_error` is on the `Server` trait and only
            // reachable inside `run_on_socket`'s error channel — instead,
            // log here with the same benign-disconnect downgrade.
            rio_common::task::spawn_monitored("ssh-session", async move {
                if config.nodelay
                    && let Err(e) = stream.set_nodelay(true)
                {
                    warn!(%peer, error = %e, "set_nodelay failed");
                }
                // Kernel-level transport bound: undeliverable bytes (or a
                // peer that holds its receive window at zero) error the
                // socket out after [`TCP_USER_TIMEOUT`] even when russh is
                // parked inline in a write and no in-process timer can run.
                // See the constant for why it equals the keepalive bound.
                if let Err(e) = set_tcp_user_timeout(&stream, TCP_USER_TIMEOUT) {
                    warn!(%peer, error = %e, "set TCP_USER_TIMEOUT failed");
                }
                // One pre-auth deadline for the whole connection,
                // measured from accept: a connection that has not
                // completed authentication by this instant is dropped or
                // disconnected below. It has to live at the accept site
                // because nothing post-accept bounds it: the auth
                // callbacks have no `&mut Session` to arm the
                // empty-connection timer with, russh 0.61 has no
                // login-grace config (and never enforces
                // `max_auth_attempts`), and a client that answers
                // keepalives resets both `alive_timeouts` and
                // `inactivity_timeout` on every reply — so a rejected
                // key or a wedged ssh-agent would otherwise hold its
                // `conn_permit`, fd, and `connections_active` slot
                // forever.
                let auth_deadline = tokio::time::Instant::now() + auth_grace;
                // r[impl gw.conn.exit-status+3]
                // Hard backstop a fixed slack later, enforced on the
                // transport itself: the polite disconnect below cannot
                // reach a peer that keeps the initial key exchange in
                // flight (russh only drains handle messages between key
                // exchanges), so after the slack the wrapper fails the
                // read, russh's session loop returns the error, and the
                // handler/stream drop releases the permit and fd. The same
                // wrapper also enforces the force-close armed by any
                // "this connection must go away" decision (the auth-timeout
                // disconnect below, the empty-connection grace timer). See
                // [`ConnDeadline`].
                let stream = ConnDeadline::new(
                    stream,
                    Arc::clone(&stage),
                    Arc::clone(&force_close),
                    auth_deadline + FORCE_CLOSE_SLACK,
                );
                // r[impl gw.conn.exit-status+3]
                // Phase 1 — version exchange / transport setup. A client
                // that never sends its SSH identification string parks
                // `run_stream` inside `read_ssh_id`, which russh bounds
                // only by the 1 h `inactivity_timeout`, so the deadline
                // must cover `run_stream` itself. Losing the race drops
                // the future, which still owns the handler and the TCP
                // stream (russh only spawns the session loop immediately
                // before returning, with no await point after it): the
                // conn permit and fd release through the handler's
                // `Drop`, exactly once, and the `connections_active`
                // gauge stays untouched because no auth callback can
                // have fired yet (it is only incremented in
                // `mark_real_connection`).
                let mut session = tokio::select! {
                    r = run_stream(config, stream, handler) => match r {
                        Ok(s) => s,
                        Err(e) => {
                            log_session_end(peer, &stage, &e);
                            return;
                        }
                    },
                    () = tokio::time::sleep_until(auth_deadline) => {
                        debug!(
                            %peer,
                            deadline_secs = auth_grace.as_secs(),
                            "no SSH handshake within the deadline; dropping connection"
                        );
                        return;
                    }
                };
                // r[impl gw.conn.exit-status+3]
                // Phase 2 — same deadline, transport established. The
                // `stage` check immediately before acting keeps a
                // connection that authenticated at the last moment
                // alive; an auth that lands in the instant between that
                // check and the disconnect being processed is still torn
                // down — the same inherent boundary race as any idle
                // timeout, and the client just reconnects. Delivery
                // caveat: russh only drains server-queued messages
                // between key exchanges (`!kex.active()`), so a peer
                // that keeps a key exchange perpetually in flight never
                // receives this polite disconnect; the transport-level
                // deadline is what actually bounds it. That deadline must
                // be the shared force-close, armed HERE, at the decision
                // point (decide-then-enforce, same as the empty-connection
                // grace timer): the wrapper's pre-auth deadline stops
                // applying the instant the connection authenticates, so an
                // auth that completes while this disconnect is still
                // queued — entirely possible, the key check plus the
                // ResolveTenant round-trip can straddle the deadline —
                // followed by a peer that ignores the disconnect and holds
                // its socket would otherwise be bounded by nothing but
                // russh's leftover keepalive/inactivity timers. For a
                // connection that never authenticates the arm changes
                // nothing: the pre-auth deadline expires at this same
                // instant. The polite path stays because for every
                // deliverable case (rejected key, wedged ssh-agent,
                // stalled-but-not-rekeying client) it ends the connection
                // cleanly and promptly.
                tokio::select! {
                    r = &mut session => {
                        if let Err(e) = r {
                            log_session_end(peer, &stage, &e);
                        }
                        return;
                    }
                    () = tokio::time::sleep_until(auth_deadline) => {
                        if stage.load(Ordering::Relaxed)
                            < connection::ConnStage::Authenticated as u8
                        {
                            debug!(
                                %peer,
                                deadline_secs = auth_grace.as_secs(),
                                "connection did not authenticate within the deadline; \
                                 disconnecting"
                            );
                            // r[impl gw.conn.force-close]
                            force_close.arm_within(FORCE_CLOSE_SLACK);
                            let _ = session
                                .handle()
                                .disconnect(
                                    Disconnect::ByApplication,
                                    "authentication timeout".to_owned(),
                                    String::new(),
                                )
                                .await;
                        }
                    }
                }
                if let Err(e) = session.await {
                    log_session_end(peer, &stage, &e);
                }
            });
        }
    }
}

// r[impl gw.conn.force-close]
/// Transport force-close deadline for one connection, shared between the
/// [`ConnDeadline`] stream wrapper (which enforces it) and every site that
/// decides the connection must go away (which arms it). Created in
/// [`GatewayServer::new_client`], threaded to the wrapper at the accept
/// site and to the empty-connection grace timer via the
/// `ConnectionHandler`.
///
/// The contract: arm it AT THE MOMENT a `Disconnect::ByApplication` is
/// queued for the connection — the empty-connection grace timer in
/// `connection.rs` and the auth-timeout disconnect in
/// [`GatewayServer::run_on_listener`] both do. The latter matters even
/// though the pre-auth deadline expires at the same instant: that deadline
/// stops applying the moment the connection authenticates, and an auth
/// that completes with the disconnect still queued (the peer then ignoring
/// it) must stay bounded by the decision that was already made. Arming
/// before the handle send matters: the polite disconnect rides the russh
/// handle queue, which a hostile peer can park (key exchange held open) —
/// the decision to disconnect must be bounded even if the polite send
/// itself never completes. A third site arms it with no disconnect at
/// all: a session whose sends to the client (channel data through the
/// window-aware writer, or the close-out on the handle queue) have
/// stalled past `HANDLE_SEND_TIMEOUT` (connection.rs) — the peer is
/// either not draining the queue a polite disconnect would ride or not
/// acting on anything it is sent, so the armed deadline is the entire
/// response.
///
/// Lock-free: a single `AtomicU64` of nanoseconds relative to `origin`
/// (`NOT_ARMED` = nothing pending), so the wrapper's hot path is one
/// relaxed load. Arming keeps the earliest deadline (`fetch_min`) — repeat
/// decisions can only tighten the bound — and there is deliberately no
/// disarm: once the gateway has told a connection to go away, nothing
/// un-decides it.
struct ForceClose {
    /// Reference instant for the nanosecond encoding (handler creation,
    /// i.e. accept time).
    origin: tokio::time::Instant,
    /// Nanoseconds after `origin` at which transport reads must start
    /// failing. [`Self::NOT_ARMED`] = no force-close pending.
    kill_at_nanos: std::sync::atomic::AtomicU64,
}

impl ForceClose {
    const NOT_ARMED: u64 = u64::MAX;

    fn new() -> Self {
        Self {
            origin: tokio::time::Instant::now(),
            kill_at_nanos: std::sync::atomic::AtomicU64::new(Self::NOT_ARMED),
        }
    }

    /// Arm the force-close `slack` from now (keeping an earlier already
    /// armed deadline). Relaxed ordering is sufficient: the value itself is
    /// the entire message, no other memory is published with it.
    fn arm_within(&self, slack: Duration) {
        let nanos = u64::try_from(
            (tokio::time::Instant::now() + slack)
                .saturating_duration_since(self.origin)
                .as_nanos(),
        )
        .unwrap_or(Self::NOT_ARMED - 1)
        .min(Self::NOT_ARMED - 1);
        self.kill_at_nanos.fetch_min(nanos, Ordering::Relaxed);
    }

    /// The armed force-close instant, if any. One relaxed atomic load.
    fn armed_deadline(&self) -> Option<tokio::time::Instant> {
        match self.kill_at_nanos.load(Ordering::Relaxed) {
            Self::NOT_ARMED => None,
            nanos => Some(self.origin + Duration::from_nanos(nanos)),
        }
    }
}

// r[impl gw.conn.exit-status+3]
// r[impl gw.conn.force-close]
/// Transport wrapper that enforces the gateway's deadlines at the stream
/// level, on both the read and the write path — the only lever that works
/// without the peer's cooperation. russh 0.61 only drains handle messages
/// between key exchanges (`!kex.active()` gates the `receiver.recv()` arm
/// of its session loop), offers no way to abort the session loop it
/// spawned (`RunningSession` wraps a result oneshot, not an abortable
/// `JoinHandle`), awaits its writes inline (a peer that stops reading
/// parks the whole loop in a bulk channel-data write), and once a queued
/// disconnect IS processed it parks in a post-disconnect drain-read loop
/// that has no timeout and arms no keepalives, waiting for the peer to
/// close. Failing the read or write ends every one of those states: both
/// the main loop and the drain loop surface the error and return, the
/// spawned session task drops the handler and stream, and the connection
/// permit, fd, and gauges release exactly once through the existing drop
/// paths.
///
/// Two deadlines, one mechanism:
///
/// - **Pre-auth:** reads/writes fail once `pre_auth_deadline` (accept +
///   grace + [`FORCE_CLOSE_SLACK`]) passes with the connection still not
///   authenticated. Ignored permanently once the connection reaches
///   `ConnStage::Authenticated`; an authenticated session can idle or
///   re-key for as long as the post-auth limits allow.
/// - **Force-close:** reads/writes fail once the shared [`ForceClose`]
///   deadline passes, regardless of auth state. It is armed the moment the
///   gateway decides the connection must go — when a
///   `Disconnect::ByApplication` is queued, or when a session's sends to
///   the client stall past `HANDLE_SEND_TIMEOUT` (a peer in that state is
///   not taking what it is sent, a polite disconnect included) — so a peer
///   that keeps a disconnect undeliverable (parked key exchange) or
///   ignores it (never closes its socket) is closed within the slack
///   anyway.
///
/// Cost on the hot path (authenticated, nothing armed): one relaxed atomic
/// load per read/write poll — the pre-auth stage check latches off after
/// the first authenticated poll, and the internal sleep is only
/// armed/polled while a deadline is actually pending. Polling the sleep is
/// what wakes a read or write that is parked with nothing else to wake it
/// (the KEX-parked or socket-holding peer's read, the zero-window peer's
/// write) when its deadline arrives; a kill armed only after the poll
/// already parked is picked up on the next inbound packet or keepalive
/// tick for a parked read, and by the kernel-level [`TCP_USER_TIMEOUT`]
/// for a parked write (nothing in-process re-polls a parked write).
struct ConnDeadline<S> {
    inner: S,
    /// Highest [`connection::ConnStage`] reached, shared with the
    /// `ConnectionHandler`. At `Authenticated` and above the pre-auth
    /// deadline no longer applies.
    stage: Arc<AtomicU8>,
    /// Latched copy of "stage reached `Authenticated`" so the hot path
    /// does not re-load `stage` on every poll (auth is monotonic).
    authenticated: bool,
    /// Shared force-close deadline, armed by whichever site queues a
    /// `Disconnect::ByApplication` for this connection.
    force_close: Arc<ForceClose>,
    /// The pre-auth bound: accept + grace + [`FORCE_CLOSE_SLACK`].
    pre_auth_deadline: tokio::time::Instant,
    /// Sleep used to wake a parked read when the applicable deadline
    /// fires. Re-armed whenever that deadline changes (pre-auth → armed
    /// force-close). Boxed so the wrapper stays `Unpin` (russh requires
    /// it of the stream).
    deadline: Pin<Box<tokio::time::Sleep>>,
    /// Which instant `deadline` is currently set to, so redundant resets
    /// are skipped.
    sleep_target: Option<tokio::time::Instant>,
    /// Latched once a deadline has fired so subsequent reads keep failing
    /// without polling a completed timer again.
    expired: Option<&'static str>,
}

impl<S> ConnDeadline<S> {
    fn new(
        inner: S,
        stage: Arc<AtomicU8>,
        force_close: Arc<ForceClose>,
        pre_auth_deadline: tokio::time::Instant,
    ) -> Self {
        Self {
            inner,
            stage,
            authenticated: false,
            force_close,
            pre_auth_deadline,
            deadline: Box::pin(tokio::time::sleep_until(pre_auth_deadline)),
            sleep_target: Some(pre_auth_deadline),
            expired: None,
        }
    }

    /// Whether a deadline applies and has passed. While one is pending,
    /// polling the internal sleep registers this task's waker so a read
    /// that is parked with no inbound bytes is still woken when the
    /// deadline arrives (a KEX-parked peer's read would otherwise only be
    /// re-polled when the peer sends more bytes; a post-disconnect socket
    /// holder's never).
    fn check_expired(&mut self, cx: &mut TaskContext<'_>) -> Option<&'static str> {
        if self.expired.is_some() {
            return self.expired;
        }
        // Hot path: a single relaxed load. `None` (nothing armed) on an
        // authenticated connection falls straight through to the inner
        // read.
        let force_close_at = self.force_close.armed_deadline();
        let (target, reason) = match force_close_at {
            Some(at) => (
                at,
                "connection was told to disconnect and did not close within the slack; \
                 force-closing transport",
            ),
            None => {
                if self.authenticated {
                    return None;
                }
                if self.stage.load(Ordering::Relaxed) >= connection::ConnStage::Authenticated as u8
                {
                    // Monotonic: the pre-auth deadline can never apply
                    // again, so skip the stage load on future polls.
                    self.authenticated = true;
                    return None;
                }
                (
                    self.pre_auth_deadline,
                    "pre-auth deadline exceeded with authentication incomplete; closing transport",
                )
            }
        };
        if self.sleep_target != Some(target) {
            self.deadline.as_mut().reset(target);
            self.sleep_target = Some(target);
        }
        if self.deadline.as_mut().poll(cx).is_ready() {
            self.expired = Some(reason);
        }
        self.expired
    }
}

impl<S: AsyncRead + Unpin> AsyncRead for ConnDeadline<S> {
    fn poll_read(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &mut ReadBuf<'_>,
    ) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(reason) = this.check_expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                reason,
            )));
        }
        Pin::new(&mut this.inner).poll_read(cx, buf)
    }
}

/// Write-path enforcement of the same deadlines as the read path. Server
/// writes are NOT all tiny: once a session is exec'd the gateway streams
/// bulk CHANNEL_DATA (build logs, NAR bytes) to the client through this
/// stream, and russh's session loop awaits its write/flush inline rather
/// than as a select arm — a peer that stops reading at the TCP level (zero
/// receive window: suspended laptop, wedged client, or deliberate) parks
/// the loop in the write, after which the read arm, keepalive arm, and
/// handle queue are never polled again. Checking the deadline here means a
/// deadline that was already pending when the write parked still fires:
/// `check_expired` keeps the polled sleep registered with this task's
/// waker, so the parked write is woken at the deadline and fails with
/// `TimedOut`, ending russh's session loop exactly like a failed read.
///
/// This deliberately does NOT cover a deadline armed only AFTER the write
/// parked — nothing re-polls the task then, so no in-process timer can run
/// at all; the kernel-level `TCP_USER_TIMEOUT` set at accept
/// ([`set_tcp_user_timeout`]) bounds that case. `poll_shutdown` stays
/// untouched so tearing the connection down is never itself blocked by a
/// deadline.
impl<S: AsyncWrite + Unpin> AsyncWrite for ConnDeadline<S> {
    fn poll_write(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        buf: &[u8],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Some(reason) = this.check_expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                reason,
            )));
        }
        Pin::new(&mut this.inner).poll_write(cx, buf)
    }

    fn poll_flush(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        let this = self.get_mut();
        if let Some(reason) = this.check_expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                reason,
            )));
        }
        Pin::new(&mut this.inner).poll_flush(cx)
    }

    fn poll_shutdown(self: Pin<&mut Self>, cx: &mut TaskContext<'_>) -> Poll<std::io::Result<()>> {
        Pin::new(&mut self.get_mut().inner).poll_shutdown(cx)
    }

    fn poll_write_vectored(
        self: Pin<&mut Self>,
        cx: &mut TaskContext<'_>,
        bufs: &[std::io::IoSlice<'_>],
    ) -> Poll<std::io::Result<usize>> {
        let this = self.get_mut();
        if let Some(reason) = this.check_expired(cx) {
            return Poll::Ready(Err(std::io::Error::new(
                std::io::ErrorKind::TimedOut,
                reason,
            )));
        }
        Pin::new(&mut this.inner).poll_write_vectored(cx, bufs)
    }

    fn is_write_vectored(&self) -> bool {
        self.inner.is_write_vectored()
    }
}

/// What the accept loop should do with a `TcpListener::accept()` error.
#[derive(Debug, PartialEq, Eq)]
enum AcceptErrAction {
    /// Log + metric + `continue` immediately.
    Retry,
    /// Log + metric + sleep + `continue`. For fd exhaustion: lets
    /// in-flight sessions close and free descriptors instead of
    /// hot-spinning a core.
    RetryAfter(Duration),
}

// r[impl gw.conn.accept-resilience]
/// Classify an `accept()` error. Separate fn for unit testability —
/// can't inject ECONNABORTED into a real `TcpListener`.
///
/// tokio's `TcpListener::accept()` only retries `WouldBlock` internally;
/// `ECONNABORTED` (client RST between SYN-ACK and userspace accept),
/// `EMFILE`/`ENFILE` (fd exhaustion — happens precisely when many
/// sessions are live), `ENOMEM`/`ENOBUFS` all surface. Hyper/tonic
/// precedent: ALL accept errors are transient — the listener fd is
/// owned and never closed, so `EBADF` is unreachable.
fn classify_accept_error(e: &std::io::Error) -> AcceptErrAction {
    match e.raw_os_error() {
        Some(libc::EMFILE | libc::ENFILE) => {
            AcceptErrAction::RetryAfter(Duration::from_millis(100))
        }
        _ => AcceptErrAction::Retry,
    }
}

/// Apply [`TCP_USER_TIMEOUT`] to an accepted connection: tell the kernel to
/// error the socket out once transmitted data has gone unacknowledged (or a
/// zero receive window has persisted) for `timeout`. tokio's `TcpStream`
/// exposes no setter for this option, so it is set through `libc` on the
/// raw fd; the unsafety is contained here behind a safe signature. Failure
/// is returned for the caller to warn-and-continue (same handling as a
/// `set_nodelay` failure): a connection without the option is merely
/// missing one backstop, not unusable.
#[cfg(target_os = "linux")]
fn set_tcp_user_timeout(stream: &tokio::net::TcpStream, timeout: Duration) -> std::io::Result<()> {
    use std::os::fd::AsRawFd;

    let millis: libc::c_uint = timeout.as_millis().try_into().unwrap_or(libc::c_uint::MAX);
    // SAFETY: the fd is owned by `stream`, which outlives this call; the
    // option value is a properly-sized, properly-aligned c_uint on the
    // stack; setsockopt copies it and does not retain the pointer.
    let rc = unsafe {
        libc::setsockopt(
            stream.as_raw_fd(),
            libc::IPPROTO_TCP,
            libc::TCP_USER_TIMEOUT,
            (&raw const millis).cast(),
            std::mem::size_of::<libc::c_uint>() as libc::socklen_t,
        )
    };
    if rc == 0 {
        Ok(())
    } else {
        Err(std::io::Error::last_os_error())
    }
}

/// `TCP_USER_TIMEOUT` is Linux-specific and the gateway only ships on
/// Linux; on other targets (local development builds) this is a documented
/// no-op — the in-process deadlines and the SSH keepalive still apply.
#[cfg(not(target_os = "linux"))]
fn set_tcp_user_timeout(
    _stream: &tokio::net::TcpStream,
    _timeout: Duration,
) -> std::io::Result<()> {
    Ok(())
}

// r[impl gw.conn.session-error-visible]
/// Shared session-end logging: downgrade benign disconnects (NLB health
/// check, client-initiated close, RST) to DEBUG; everything else ERROR +
/// metric. `stage` reports the highest `ConnStage` reached
/// — `Keepalive timeout` at `tcp-accepted` means the client opened TCP
/// but never sent the SSH version string (e.g., wedged on a hung
/// ssh-agent before the protocol exchange).
pub fn log_session_end(peer: SocketAddr, stage: &Arc<AtomicU8>, error: &anyhow::Error) {
    use std::io::ErrorKind;
    let stage = connection::ConnStage::name(stage.load(Ordering::Relaxed));
    let benign = error
        .downcast_ref::<russh::Error>()
        .is_some_and(|e| match e {
            russh::Error::Disconnect | russh::Error::HUP => true,
            russh::Error::IO(io) => matches!(
                io.kind(),
                ErrorKind::ConnectionReset
                    | ErrorKind::BrokenPipe
                    | ErrorKind::ConnectionAborted
                    | ErrorKind::UnexpectedEof
            ),
            _ => false,
        });
    if benign {
        debug!(%peer, stage, error = %error, "SSH session closed");
    } else {
        error!(%peer, stage, error = %error, "SSH session error");
        metrics::counter!(
            "rio_gateway_errors_total",
            "type" => "session",
            "stage" => stage,
        )
        .increment(1);
    }
}

/// Build the russh server `Config` with hardened defaults.
///
/// Extracted from `GatewayServer::run` so tests can assert individual
/// field values (keepalive, nodelay, methods) without spinning up a
/// real SSH server.
pub fn build_ssh_config(host_key: PrivateKey) -> russh::server::Config {
    russh::server::Config {
        keys: vec![host_key],
        // r[impl gw.conn.keepalive+2]
        // russh increments `alive_timeouts` THEN compares with `>`
        // (server/session.rs:553-554), so the drop happens at
        // `interval × (max+1)`. 30s × (9+1) = 300s. I-161: max was 3
        // (=120s) which fired during a client's cold-eval idle window
        // over the SSM-tunnel path — server-originated keepalives don't
        // reliably round-trip the SSM websocket layer when there's
        // zero client→server data, so a client without
        // `ServerAliveInterval` looked dead at exactly 120s. xtask now
        // sets ServerAliveInterval (`shared::NIX_SSHOPTS_BASE`); this
        // 5-minute budget is the gateway-side defense for direct
        // `nix --store ssh-ng://…` clients we don't control.
        //
        // Still catches half-open TCP (NLB idle-timeout RST that never
        // reached us, client kernel panic, cable pull) — without this,
        // a half-open connection holds its ConnectionHandler and all
        // its ChannelSessions until inactivity_timeout (1h).
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
        keepalive_max: 9,
        // Keep inactivity_timeout as a backstop; keepalive is primary.
        inactivity_timeout: Some(std::time::Duration::from_secs(3600)),
        // r[impl gw.conn.nodelay]
        // Worker protocol is small-request/small-response ping-pong
        // (opcode u64 + a few strings, then STDERR_LAST + result).
        // Nagle buffers the response waiting for more bytes that
        // won't come until the client sends the NEXT opcode —
        // which it won't until it sees this response. ~40ms/RTT.
        nodelay: true,
        // Only advertise publickey. Default MethodSet::all()
        // includes none/password/hostbased/keyboard-interactive
        // which we reject anyway — advertising them wastes a
        // client round-trip per rejected method.
        //
        // &[..] coercion: russh's `From` impl is for `&[MethodKind]`
        // (slice), not `[MethodKind; N]` (array) — auth.rs:83.
        methods: MethodSet::from(&[MethodKind::PublicKey][..]),
        auth_rejection_time: std::time::Duration::from_secs(1),
        // OpenSSH sends `none` first to probe available methods
        // (RFC 4252 §5.2). That probe is not an attack; skip the
        // constant-time delay for it. Subsequent real rejections
        // (unknown pubkey) still get the full 1s.
        auth_rejection_time_initial: Some(std::time::Duration::from_millis(10)),
        // Advertise `none` only. NAR bodies are already compressed,
        // so zlib is net expansion + CPU; and russh's compress loop
        // can ship a truncated packet on incompressible input (#89).
        preferred: Preferred {
            compression: std::borrow::Cow::Borrowed(&[compression::NONE]),
            ..Preferred::DEFAULT
        },
        ..Default::default()
    }
}

impl russh::server::Server for GatewayServer {
    type Handler = ConnectionHandler;

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        // TCP accept only — NLB health checks and kubelet liveness probes
        // (bare connect+close, no SSH bytes) land here and drop ~200μs
        // later. Defer logging/metrics to mark_real_connection(), called
        // from the first auth_* callback.
        //
        // Connection cap (r[gw.conn.cap]): acquire a permit NOW, at
        // accept time. `try_acquire_owned` — never block the accept
        // loop. On Err (at cap): the handler is returned with
        // `conn_permit: None`; `auth_none` (first real-client callback,
        // see `mark_real_connection`) checks this and rejects with a
        // visible disconnect reason. The permit consumed here is held
        // for the `ConnectionHandler`'s lifetime and released in its
        // `Drop` — every disconnect path (EOF, error, abort) frees
        // the slot.
        //
        // TCP probes (NLB health checks, connect+close, no SSH) DO
        // briefly consume a permit. They drop ~200μs later, so this
        // is negligible unless the probe rate approaches
        // 1000/200μs ≈ 5M/s. If that becomes a problem: defer the
        // acquire to `mark_real_connection()` instead (trades
        // earliest-possible-reject for probe-transparency).
        let conn_permit = Arc::clone(&self.conn_sem).try_acquire_owned().ok();
        if conn_permit.is_none() {
            warn!(peer = ?peer_addr, "connection cap reached; rejecting at auth");
            metrics::counter!("rio_gateway_errors_total", "type" => "conn_cap").increment(1);
        }
        // One per connection, shared between the accept-site ConnDeadline
        // wrapper (enforces it) and the empty-connection grace timer
        // (arms it when it queues a disconnect).
        let force_close = Arc::new(ForceClose::new());
        ConnectionHandler {
            peer_addr,
            store_client: self.store_client.clone(),
            log_client: self.log_client.clone(),
            scheduler_client: self.scheduler_client.clone(),
            authorized_keys: Arc::clone(&self.authorized_keys),
            jwt_signing_key: self.jwt_signing_key.clone(),
            jwt_config: self.jwt_config.clone(),
            resolve_timeout: self.resolve_timeout,
            // ^ threaded separately from jwt_config since JwtConfig is shared
            // with scheduler/store which never need it.
            service_signer: self.service_signer.clone(),
            limiter: self.limiter.clone(),
            quota_cache: self.quota_cache.clone(),
            channels: HashMap::new(),
            tenant_name: None,
            jwt_token: None,
            auth_attempted: false,
            stage: Arc::new(AtomicU8::new(connection::ConnStage::TcpAccepted as u8)),
            conn_permit,
            active_conns: Arc::clone(&self.active_conns),
            sessions_shutdown: self.sessions_shutdown.clone(),
            session_sem: Arc::clone(&self.session_sem),
            max_channels_per_connection: self.max_channels_per_connection,
            open_channels: 0,
            empty_connection_grace: self.empty_connection_grace,
            handshake_timeout: self.handshake_timeout,
            idle: Arc::new(connection::EmptyConnectionTimer::new(Arc::clone(
                &force_close,
            ))),
            force_close,
        }
    }
}

// r[verify gw.conn.cap]
#[cfg(test)]
mod conn_cap_tests {
    use super::*;
    use tokio::sync::OwnedSemaphorePermit;

    /// Connection cap: `try_acquire_owned` at the limit returns Err.
    /// This is the primitive `new_client` relies on — if tokio's
    /// semantics change (say, a future version blocks on
    /// `try_acquire_owned`), this test catches it before
    /// `new_client` starts blocking the accept loop.
    #[test]
    fn semaphore_at_cap_rejects() {
        let sem = Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS));
        // Drain.
        let mut permits: Vec<_> = (0..DEFAULT_MAX_CONNECTIONS)
            .map(|_| Arc::clone(&sem).try_acquire_owned().expect("under cap"))
            .collect();
        // At cap → Err.
        assert!(
            Arc::clone(&sem).try_acquire_owned().is_err(),
            "N+1th acquire on Semaphore::new(N) must fail"
        );
        // Drop one → slot freed. NOT `into_iter().next()` — that moves
        // the Vec into a temporary IntoIter whose end-of-statement drop
        // releases ALL N permits, making the assertion below vacuous.
        drop(permits.pop());
        assert_eq!(sem.available_permits(), 1, "exactly one slot freed");
        assert!(
            Arc::clone(&sem).try_acquire_owned().is_ok(),
            "dropping a permit must free a slot"
        );
        drop(permits);
    }

    /// `ensure_permit` with `conn_permit: None` returns Err. This is
    /// what the auth callbacks check; Err propagates to the spawned
    /// session task → `log_session_end` (with `stage=auth-attempted`).
    ///
    /// Structural — constructing a real `ConnectionHandler` needs
    /// live gRPC clients. We test the invariant that matters: the
    /// `Option<OwnedSemaphorePermit>` wrapping survives Drop
    /// semantics (dropping a `None` permit doesn't panic, doesn't
    /// leak, doesn't release a phantom slot).
    #[test]
    fn none_permit_drop_is_noop() {
        let sem = Arc::new(Semaphore::new(1));
        let before = sem.available_permits();
        {
            let _none: Option<OwnedSemaphorePermit> = None;
        } // Drop of None — nothing released.
        assert_eq!(
            sem.available_permits(),
            before,
            "dropping None must not release a permit"
        );
    }
}

// r[verify gw.conn.exit-status+3]
// r[verify gw.conn.force-close]
#[cfg(test)]
mod conn_deadline_tests {
    use super::*;
    use tokio::io::AsyncReadExt;
    use tokio::io::AsyncWriteExt;

    fn stage(stage: connection::ConnStage) -> Arc<AtomicU8> {
        Arc::new(AtomicU8::new(stage as u8))
    }

    /// Pre-auth deadline in the past + connection not authenticated →
    /// reads fail with `TimedOut` even though bytes are available, which
    /// is what ends russh's session loop for a never-authenticating peer.
    #[tokio::test]
    async fn unauthenticated_read_fails_after_pre_auth_deadline() {
        let (mut far, near) = tokio::io::duplex(1024);
        far.write_all(b"client bytes").await.unwrap();
        let mut wrapped = ConnDeadline::new(
            near,
            stage(connection::ConnStage::AuthAttempted),
            Arc::new(ForceClose::new()),
            tokio::time::Instant::now() - Duration::from_millis(1),
        );
        let mut buf = [0u8; 16];
        let err = wrapped
            .read(&mut buf)
            .await
            .expect_err("read must fail once the pre-auth deadline passed unauthenticated");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    /// Once the connection is authenticated the pre-auth deadline never
    /// applies again: with nothing armed, the wrapper is transparent — a
    /// read past the (long-expired) pre-auth instant still returns data.
    #[tokio::test]
    async fn authenticated_read_ignores_pre_auth_deadline() {
        let (mut far, near) = tokio::io::duplex(1024);
        far.write_all(b"post-auth data").await.unwrap();
        let mut wrapped = ConnDeadline::new(
            near,
            stage(connection::ConnStage::Authenticated),
            Arc::new(ForceClose::new()),
            tokio::time::Instant::now() - Duration::from_secs(60),
        );
        let mut buf = [0u8; 32];
        let n = wrapped
            .read(&mut buf)
            .await
            .expect("authenticated connection with nothing armed must read through");
        assert_eq!(&buf[..n], b"post-auth data");
    }

    /// The force-close deadline applies to the WRITE path too, and the
    /// wrapper's own timer wakes a write that is parked because the peer
    /// stopped reading (zero receive window). Once a session is exec'd the
    /// gateway streams bulk channel data through this stream and russh
    /// awaits the write inline — a parked write means the read arm,
    /// keepalive arm, and handle queue are never polled again, so a
    /// deadline that was already pending when the write parked must fire
    /// from the write poll itself or it never fires at all.
    #[tokio::test(start_paused = true)]
    async fn force_close_fails_a_parked_authenticated_write() {
        const SLACK: Duration = Duration::from_millis(150);
        // Tiny duplex buffer that the first write fills; the far end never
        // reads, so the second write parks exactly like a transport whose
        // peer advertises a zero receive window.
        let (_far, near) = tokio::io::duplex(8);
        let force_close = Arc::new(ForceClose::new());
        let mut wrapped = ConnDeadline::new(
            near,
            stage(connection::ConnStage::Authenticated),
            Arc::clone(&force_close),
            tokio::time::Instant::now() + Duration::from_secs(600),
        );
        wrapped
            .write_all(&[0u8; 8])
            .await
            .expect("filling the duplex buffer must succeed");
        // Armed BEFORE the write parks — the production shape the
        // in-process check covers: a disconnect or stalled-send decision
        // has already armed the force-close, and a later write (the polite
        // disconnect, a close-out, the next bulk data chunk) parks against
        // a peer that stopped reading. A force-close armed only AFTER the
        // write parked is TCP_USER_TIMEOUT's case, not this one.
        force_close.arm_within(SLACK);
        // Bounded from the outside so a regression (write parked forever,
        // deadline never checked on the write path) fails the test instead
        // of hanging it.
        let err = tokio::time::timeout(Duration::from_secs(5), wrapped.write(b"y"))
            .await
            .expect("force-close must wake the parked write at the deadline")
            .expect_err("write must fail once the force-close deadline passes");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
    }

    /// The force-close deadline applies REGARDLESS of auth state, and the
    /// wrapper's own timer wakes a read that is parked with no inbound
    /// bytes — the post-disconnect "holds the socket" peer parks russh in
    /// its drain-read loop exactly like this, with nothing else left to
    /// wake it.
    #[tokio::test]
    async fn force_close_fails_a_parked_authenticated_read() {
        const SLACK: Duration = Duration::from_millis(150);
        let (_far, near) = tokio::io::duplex(1024);
        let force_close = Arc::new(ForceClose::new());
        let mut wrapped = ConnDeadline::new(
            near,
            stage(connection::ConnStage::Authenticated),
            Arc::clone(&force_close),
            tokio::time::Instant::now() + Duration::from_secs(600),
        );
        // Armed BEFORE the read is polled — the production ordering: the
        // grace timer arms the force-close before queueing the disconnect,
        // and russh only reaches its drain-read loop after processing that
        // disconnect.
        force_close.arm_within(SLACK);
        let started = tokio::time::Instant::now();
        let mut buf = [0u8; 16];
        // Bounded from the outside so a regression (parked forever) fails
        // the test instead of hanging it.
        let err = tokio::time::timeout(Duration::from_secs(5), wrapped.read(&mut buf))
            .await
            .expect("force-close must wake the parked read at the deadline")
            .expect_err("read must fail once the force-close deadline passes");
        assert_eq!(err.kind(), std::io::ErrorKind::TimedOut);
        assert!(
            started.elapsed() >= SLACK,
            "force-close must not fire before its slack elapses"
        );
    }
}

/// Linux-only: the option must actually land on the socket, read back via
/// `getsockopt`, so a refactor that silently stops applying it (or applies
/// it to the wrong fd/level) is caught here rather than by a peer that
/// stops reading in production.
#[cfg(all(test, target_os = "linux"))]
mod tcp_user_timeout_tests {
    use super::*;

    #[tokio::test]
    async fn helper_sets_tcp_user_timeout_on_accepted_socket() -> anyhow::Result<()> {
        use std::os::fd::AsRawFd;

        let listener = TcpListener::bind(("127.0.0.1", 0)).await?;
        let addr = listener.local_addr()?;
        let _client = tokio::net::TcpStream::connect(addr).await?;
        let (accepted, _) = listener.accept().await?;

        set_tcp_user_timeout(&accepted, TCP_USER_TIMEOUT)?;

        let mut value: libc::c_uint = 0;
        let mut len = std::mem::size_of::<libc::c_uint>() as libc::socklen_t;
        // SAFETY: `value` and `len` are valid, properly aligned stack
        // locations for the duration of the call, and the fd is owned by
        // `accepted`, which outlives it.
        let rc = unsafe {
            libc::getsockopt(
                accepted.as_raw_fd(),
                libc::IPPROTO_TCP,
                libc::TCP_USER_TIMEOUT,
                (&raw mut value).cast(),
                &raw mut len,
            )
        };
        anyhow::ensure!(
            rc == 0,
            "getsockopt(TCP_USER_TIMEOUT) failed: {}",
            std::io::Error::last_os_error()
        );
        assert_eq!(
            u128::from(value),
            TCP_USER_TIMEOUT.as_millis(),
            "the kernel must report the configured TCP_USER_TIMEOUT (in milliseconds)"
        );
        Ok(())
    }
}

// r[verify gw.conn.session-drain]
// r[verify gw.conn.accept-resilience]
#[cfg(test)]
mod accept_err_tests {
    use super::*;

    /// `classify_accept_error`: ECONNABORTED and arbitrary errors →
    /// immediate retry; EMFILE/ENFILE → 100ms backoff. Structural test
    /// — the accept loop's `?` removal means the only return path is
    /// `serve_shutdown`; this proves the classifier the loop depends on.
    #[test]
    fn classify_accept_error_transient() {
        use std::io;
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::ECONNABORTED)),
            AcceptErrAction::Retry,
            "ECONNABORTED (client RST mid-handshake) → immediate retry"
        );
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::EMFILE)),
            AcceptErrAction::RetryAfter(Duration::from_millis(100)),
            "EMFILE → backoff so in-flight sessions can free fds"
        );
        assert_eq!(
            classify_accept_error(&io::Error::from_raw_os_error(libc::ENFILE)),
            AcceptErrAction::RetryAfter(Duration::from_millis(100)),
            "ENFILE → backoff"
        );
        assert_eq!(
            classify_accept_error(&io::Error::other("arbitrary")),
            AcceptErrAction::Retry,
            "non-OS / unknown → immediate retry (hyper/tonic precedent)"
        );
    }
}
