//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel, delegating operations
//! to gRPC store and scheduler services.

mod connection;
mod keys;
mod session_jwt;

pub use connection::ConnectionHandler;
pub use keys::{
    AUTHORIZED_KEYS_POLL_INTERVAL, AuthorizedKeys, load_authorized_keys, load_or_generate_host_key,
    spawn_authorized_keys_watcher,
};

use std::collections::HashMap;
use std::net::SocketAddr;
use std::sync::Arc;
use std::sync::atomic::{AtomicU8, AtomicUsize, Ordering};
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
use russh::{MethodKind, MethodSet};
use tokio::net::TcpListener;
use tokio::sync::Semaphore;
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;

/// Default global connection cap (`r[gw.conn.cap]`). At this many
/// concurrent SSH connections, new accepts are rejected immediately.
/// Each connection is ~2 MiB (MAX_CHANNELS_PER_CONNECTION × 2×256 KiB
/// duplex buffers), so 1000 connections ≈ 2 GiB bounded. Configurable
/// via `gateway.toml max_connections`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1000;

/// The SSH server that accepts connections and spawns protocol sessions.
pub struct GatewayServer {
    store_client: StoreServiceClient<Channel>,
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
        scheduler_client: SchedulerServiceClient<Channel>,
        authorized_keys: Vec<PublicKey>,
    ) -> Self {
        if authorized_keys.is_empty() {
            warn!("no authorized keys configured; all SSH connections will be rejected");
        }
        GatewayServer {
            store_client,
            scheduler_client,
            authorized_keys: Arc::new(ArcSwap::from_pointee(authorized_keys)),
            jwt_signing_key: None,
            jwt_config: JwtConfig::default(),
            resolve_timeout: std::time::Duration::from_millis(500),
            service_signer: None,
            limiter: TenantLimiter::disabled(),
            quota_cache: QuotaCache::new(),
            conn_sem: Arc::new(Semaphore::new(DEFAULT_MAX_CONNECTIONS)),
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
    /// called, write opcodes attach no service token (store falls back
    /// to mTLS CN-allowlist or rejects). Builder-style.
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
    // r[impl gw.conn.session-drain]
    pub async fn run(
        mut self,
        host_key: PrivateKey,
        addr: SocketAddr,
        serve_shutdown: CancellationToken,
    ) -> anyhow::Result<()> {
        let config = Arc::new(build_ssh_config(host_key));

        info!(addr = %addr, "starting SSH server");

        let socket = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind SSH server to {addr}"))?;

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
            let config = Arc::clone(&config);
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
                let session = match run_stream(config, stream, handler).await {
                    Ok(s) => s,
                    Err(e) => {
                        log_session_end(peer, &stage, &e);
                        return;
                    }
                };
                if let Err(e) = session.await {
                    log_session_end(peer, &stage, &e);
                }
            });
        }
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
        ConnectionHandler {
            peer_addr,
            store_client: self.store_client.clone(),
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
            sessions: HashMap::new(),
            tenant_name: None,
            jwt_token: None,
            auth_attempted: false,
            stage: Arc::new(AtomicU8::new(connection::ConnStage::TcpAccepted as u8)),
            conn_permit,
            active_conns: Arc::clone(&self.active_conns),
            sessions_shutdown: self.sessions_shutdown.clone(),
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
        let permits: Vec<_> = (0..DEFAULT_MAX_CONNECTIONS)
            .map(|_| Arc::clone(&sem).try_acquire_owned().expect("under cap"))
            .collect();
        // At cap → Err.
        assert!(
            Arc::clone(&sem).try_acquire_owned().is_err(),
            "N+1th acquire on Semaphore::new(N) must fail"
        );
        // Drop one → slot freed.
        drop(permits.into_iter().next());
        assert!(
            Arc::clone(&sem).try_acquire_owned().is_ok(),
            "dropping a permit must free a slot"
        );
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

// r[verify gw.conn.session-drain]
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
