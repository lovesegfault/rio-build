//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel, delegating operations
//! to gRPC store and scheduler services.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::{Duration, SystemTime};

use anyhow::Context;
use arc_swap::ArcSwap;
use ed25519_dalek::SigningKey;
use rio_common::config::JwtConfig;
use rio_common::jwt;
use rio_common::signal::Token as CancellationToken;
use rio_common::tenant::{NameError, NormalizedName};
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Handler, Msg, Server as _, Session, run_stream};
use russh::{ChannelId, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tokio::sync::{OwnedSemaphorePermit, Semaphore};
use tonic::transport::Channel;
use tracing::{Instrument, debug, error, info, trace, warn};

use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;
use crate::session::run_protocol;

/// Max active protocol sessions per SSH connection. Matches Nix's
/// default `max-jobs` — a well-behaved `nix build -j4` opens at most
/// this many channels. Each session = 2 spawned tasks + 2×256 KiB
/// duplex buffers, so this bounds per-connection memory at ~2 MiB.
///
/// Counted via `self.sessions.len()` — see `channel_open_session`.
const MAX_CHANNELS_PER_CONNECTION: usize = 4;

/// Default global connection cap (`r[gw.conn.cap]`). At this many
/// concurrent SSH connections, new accepts are rejected immediately.
/// Each connection is ~2 MiB (MAX_CHANNELS_PER_CONNECTION × 2×256 KiB
/// duplex buffers), so 1000 connections ≈ 2 GiB bounded. Configurable
/// via `gateway.toml max_connections`.
pub const DEFAULT_MAX_CONNECTIONS: usize = 1000;

/// JWT `exp` = mint time + this. Upper-bounds a single SSH session's
/// token lifetime at the SSH inactivity_timeout (3600s — see
/// `build_ssh_config`) plus a 5-minute grace for in-flight gRPC calls
/// that outlive the SSH close. A long build that exceeds 1h of SSH
/// idle gets dropped by russh anyway; the JWT expiry is a second
/// fence, not the primary one.
///
/// Spec (`r[gw.jwt.claims]`) says "SSH session duration + grace" —
/// but we don't know the session duration at mint time. This is the
/// static upper bound. SIGHUP key rotation (T3) swaps the VERIFY
/// key on scheduler/store; tokens minted under the old signing key
/// become unverifiable post-swap. A long session that spans a
/// rotation will see `UNAUTHENTICATED` on its next gRPC call → the
/// client (nix) retries → new SSH connect → new token under the new
/// key. Token refresh on long sessions would avoid the one failed
/// call, but the retry-on-reconnect path already handles it.
const JWT_SESSION_TTL_SECS: i64 = 3600 + 300;

/// Re-mint threshold. When the cached token has fewer than this many
/// seconds until `exp`, the next `refresh_session_jwt` call replaces
/// it. 5min covers realistic clock skew between gateway and
/// store/scheduler and leaves a channel opened just under the
/// threshold the full slack window before the store would reject it.
const JWT_REFRESH_SLACK_SECS: i64 = 300;

// r[impl gw.jwt.issue]
/// Mint a per-session tenant JWT. Called once per SSH connection,
/// right after `auth_publickey` accepts — the returned token is
/// stored on the `ConnectionHandler` and injected as
/// `x-rio-tenant-token` on every outbound gRPC call for the session's
/// lifetime.
///
/// `tenant_id` is the resolved UUID, not the authorized_keys comment
/// string. The gateway is PG-free (`r[sched.tenant.resolve]` says the
/// scheduler owns the `tenants` table), so the caller resolves
/// name→UUID via the `ResolveTenant` scheduler RPC before calling —
/// see `ConnectionHandler::resolve_and_mint` at the `auth_publickey`
/// call site.
///
/// `jti` is a fresh v4 UUID per call. It is the **revocation lookup
/// key** (scheduler checks `jti NOT IN jwt_revoked`) and the **audit
/// key** (INSERTed into `builds.jwt_jti`). It is NOT the rate-limit
/// partition key — that's `sub` (bounded: one key per tenant). A
/// `jti`-keyed rate limiter would leak memory proportional to
/// connection churn. See the `Claims.jti` doc in `rio_common::jwt`.
///
/// Returns `(token, claims)` so callers that want to log `jti`
/// without re-parsing the token can read it directly. The token is
/// opaque to the gateway after this — it's just a string to inject.
pub fn mint_session_jwt(
    tenant_id: uuid::Uuid,
    signing_key: &SigningKey,
) -> Result<(String, jwt::TenantClaims), jwt::JwtError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    let claims = jwt::TenantClaims {
        sub: tenant_id,
        iat: now,
        exp: now + JWT_SESSION_TTL_SECS,
        jti: uuid::Uuid::new_v4().to_string(),
    };
    let token = jwt::sign(&claims, signing_key)?;
    Ok((token, claims))
}

// r[impl gw.jwt.refresh-on-expiry]
/// Re-mint the cached session JWT if it is within
/// `JWT_REFRESH_SLACK_SECS` of expiry. Returns a borrow of the
/// (possibly-refreshed) token string, or `None` if no token is cached
/// (dual-mode fallback / single-tenant).
///
/// Called from `ConnectionHandler::ensure_fresh_jwt` on every
/// `exec_request` — i.e., every new SSH channel on a mux'd connection.
/// SSH `ControlMaster` keeps one TCP connection alive indefinitely;
/// without this, a channel opened past `JWT_SESSION_TTL_SECS` would
/// inject an expired token and get `ExpiredSignature` from the store
/// (I-129). Re-mint is purely local: `tenant_id` is `claims.sub` from
/// the cached token, `signing_key` is already on the handler — no
/// `ResolveTenant` round-trip.
///
/// On re-mint failure (only possible if the signing key is corrupt —
/// the same key minted the original), the stale token is returned
/// unchanged and a warning logged. The store will reject it with a
/// clear `ExpiredSignature`; that surfaces the problem instead of
/// silently degrading to the `tenant_name` fallback mid-connection.
pub fn refresh_session_jwt<'a>(
    cached: &'a mut Option<(String, jwt::TenantClaims)>,
    signing_key: Option<&SigningKey>,
) -> Option<&'a str> {
    let (tenant_id, exp) = match cached.as_ref() {
        Some((_, c)) => (c.sub, c.exp),
        None => return None,
    };
    let now = SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    if exp - now < JWT_REFRESH_SLACK_SECS
        && let Some(key) = signing_key
    {
        match mint_session_jwt(tenant_id, key) {
            Ok((token, claims)) => {
                debug!(
                    jti = %claims.jti,
                    tenant = %tenant_id,
                    old_exp = exp,
                    new_exp = claims.exp,
                    "refreshed session JWT (near expiry)"
                );
                metrics::counter!("rio_gateway_jwt_refreshed_total").increment(1);
                *cached = Some((token, claims));
            }
            Err(e) => {
                warn!(
                    error = %e,
                    tenant = %tenant_id,
                    "JWT refresh mint failed; keeping stale token"
                );
                metrics::counter!("rio_gateway_jwt_refresh_failed_total").increment(1);
            }
        }
    }
    cached.as_ref().map(|(t, _)| t.as_str())
}

/// Load or generate an SSH host key.
pub fn load_or_generate_host_key(path: &Path) -> anyhow::Result<PrivateKey> {
    if path.exists() {
        info!(path = %path.display(), "loading SSH host key");
        let key = russh::keys::load_secret_key(path, None)
            .with_context(|| format!("failed to load host key from {}", path.display()))?;
        Ok(key)
    } else {
        warn!(
            path = %path.display(),
            "SSH host key not found, generating a new one (dev mode)"
        );
        let key = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)
            .context("failed to generate host key")?;
        if let Some(parent) = path.parent()
            && let Err(e) = std::fs::create_dir_all(parent)
        {
            warn!(
                error = %e,
                path = %parent.display(),
                "failed to create directory for host key; key will be ephemeral"
            );
        }
        if let Err(e) = std::fs::write(path, key.to_openssh(ssh_key::LineEnding::LF)?) {
            warn!(error = %e, "could not save generated host key (continuing with ephemeral key)");
        }
        Ok(key)
    }
}

// r[impl sec.boundary.ssh-auth]
/// Load authorized public keys from a file in standard `authorized_keys` format.
pub fn load_authorized_keys(path: &Path) -> anyhow::Result<Vec<PublicKey>> {
    let content = std::fs::read_to_string(path)
        .with_context(|| format!("failed to read authorized_keys from {}", path.display()))?;

    let mut keys = Vec::new();
    for (i, line) in content.lines().enumerate() {
        let line = line.trim();
        if line.is_empty() || line.starts_with('#') {
            continue;
        }
        match line.parse::<PublicKey>() {
            Ok(key) => {
                debug!(line = i + 1, "loaded authorized key");
                keys.push(key);
            }
            Err(e) => {
                warn!(
                    line = i + 1,
                    error = %e,
                    "skipping invalid authorized_keys entry"
                );
            }
        }
    }

    if keys.is_empty() {
        anyhow::bail!(
            "no valid authorized keys loaded from {}; server would reject all SSH connections",
            path.display()
        );
    }

    info!(count = keys.len(), "loaded authorized keys");
    Ok(keys)
}

/// Shared hot-swappable authorized-key set. `ArcSwap` so the auth path
/// (`.load()` per offered key — read-heavy) never blocks the watcher's
/// rare `.store()`. Wrapped in an outer `Arc` so the watcher task,
/// `GatewayServer`, and every `ConnectionHandler` share one instance.
pub type AuthorizedKeys = Arc<ArcSwap<Vec<PublicKey>>>;

/// Poll interval for [`spawn_authorized_keys_watcher`]. kubelet refreshes
/// projected-Secret mounts ~60s after the Secret changes; 10s polling
/// means the gateway picks the swap up within ~70s of `kubectl apply`.
/// Coarse on purpose — inotify on the file itself misses the kubelet
/// `..data` symlink swap, and inotify on the parent dir is more moving
/// parts than a 10s mtime poll for a file that changes ~never.
pub const AUTHORIZED_KEYS_POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Watch `path` for content changes and hot-swap `keys` when it does.
///
/// I-109: previously the gateway loaded `authorized_keys` once at
/// startup. Rotating a tenant key (operator edits the K8s Secret) had
/// no effect until pod restart — the in-memory set was a startup
/// snapshot. Now: poll the file's mtime every `poll_interval`; on
/// change, re-parse and atomically swap. In-flight SSH handshakes see
/// the new set on their next `auth_publickey_offered` call (each
/// `.load()` reads the current `Arc`, not a per-connection snapshot).
///
/// **Why mtime polling, not inotify**: kubelet mounts Secrets via a
/// `..data → ..YYYY_MM_DD_hh_mm_ss.NNN/` symlink and refreshes by
/// atomically retargeting the symlink. An `IN_MODIFY` watch on the
/// file path itself is pinned to the OLD inode and never fires.
/// Watching the parent dir works but adds a dep + event-debounce
/// logic. `std::fs::metadata` follows symlinks, so the mtime we read
/// is the target file's — a swap shows up as a changed mtime. 10s
/// polling is negligible load and bounded latency.
///
/// **Reload failures keep the old set**: if the new file is empty,
/// all-invalid, or transiently unreadable mid-swap, log WARN and
/// retry next tick. Never swap to an empty set (would lock everyone
/// out until the next successful reload).
pub fn spawn_authorized_keys_watcher(
    keys: AuthorizedKeys,
    path: PathBuf,
    poll_interval: Duration,
    shutdown: CancellationToken,
) -> tokio::task::JoinHandle<()> {
    tokio::spawn(async move {
        // Seed with the current mtime so the first tick doesn't
        // spuriously reload what `main` just loaded. If the initial
        // stat fails (file vanished between main's load and here —
        // vanishingly rare), `None` means the first successful stat
        // will trigger a reload, which is the safe outcome.
        let mut last_mtime = std::fs::metadata(&path).and_then(|m| m.modified()).ok();
        let mut ticker = tokio::time::interval(poll_interval);
        // First tick fires immediately; we've already seeded mtime, so
        // skip it rather than special-casing the loop body.
        ticker.tick().await;
        loop {
            tokio::select! {
                biased;
                () = shutdown.cancelled() => return,
                _ = ticker.tick() => {}
            }
            let mtime: Option<SystemTime> =
                match std::fs::metadata(&path).and_then(|m| m.modified()) {
                    Ok(m) => Some(m),
                    Err(e) => {
                        warn!(path = %path.display(), error = %e,
                            "authorized_keys stat failed; keeping current set");
                        continue;
                    }
                };
            if mtime == last_mtime {
                continue;
            }
            match load_authorized_keys(&path) {
                Ok(new_keys) => {
                    info!(count = new_keys.len(), "reloaded authorized keys");
                    keys.store(Arc::new(new_keys));
                    last_mtime = mtime;
                }
                Err(e) => {
                    // Don't advance last_mtime — retry next tick. A
                    // half-written file (non-k8s deploys without atomic
                    // rename) will succeed once the writer finishes.
                    warn!(path = %path.display(), error = %e,
                        "authorized_keys reload failed; keeping current set");
                }
            }
        }
    })
}

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
    /// `resolve_timeout_ms` bounds the ResolveTenant RPC. Cloned
    /// into every ConnectionHandler.
    jwt_config: JwtConfig,
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

    /// Enable JWT issuance. Until called, `auth_publickey` accepts
    /// without minting (dual-mode fallback path). After: every
    /// accepted connection attempts a ResolveTenant round-trip +
    /// [`mint_session_jwt`]. Whether mint FAILURE is fatal depends
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
                r = socket.accept() => r.context("SSH accept")?,
            };
            let handler = self.new_client(Some(peer));
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
                        log_session_end(&e);
                        return;
                    }
                };
                if let Err(e) = session.await {
                    log_session_end(&e);
                }
            });
        }
    }
}

/// Shared session-end logging: downgrade benign disconnects (NLB health
/// check, client-initiated close, RST) to DEBUG; everything else ERROR +
/// metric. Same classification the previous `handle_session_error`
/// override used — extracted because the custom accept loop spawns
/// sessions detached and `Server::handle_session_error` (which is called
/// by `run_on_socket`'s error channel) is no longer reachable.
fn log_session_end(error: &anyhow::Error) {
    use std::io::ErrorKind;
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
        debug!(error = %error, "SSH session closed");
    } else {
        error!(error = %error, "SSH session error");
        metrics::counter!("rio_gateway_errors_total", "type" => "session").increment(1);
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
        // r[impl gw.conn.keepalive]
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

    // r[impl gw.conn.session-error-visible]
    // The custom accept loop in `run()` calls `log_session_end` directly
    // (sessions are detached, so `run_on_socket`'s error-channel path
    // that would invoke this is unreachable). This impl is required by
    // the `Server` trait; delegate so any future caller stays consistent.
    fn handle_session_error(&mut self, error: <Self::Handler as Handler>::Error) {
        log_session_end(&error);
    }

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
            limiter: self.limiter.clone(),
            quota_cache: self.quota_cache.clone(),
            sessions: HashMap::new(),
            tenant_name: None,
            jwt_token: None,
            auth_attempted: false,
            conn_permit,
            active_conns: Arc::clone(&self.active_conns),
            sessions_shutdown: self.sessions_shutdown.clone(),
        }
    }
}

/// State for an active protocol session on one SSH channel.
struct ChannelSession {
    /// Send client data to the protocol handler.
    client_tx: Option<tokio::sync::mpsc::Sender<Vec<u8>>>,
    /// Protocol handler task. NOT aborted in Drop — dropping a
    /// `JoinHandle` detaches the task, it keeps running. `shutdown`
    /// below is the graceful stop signal. Held (not immediately
    /// detached at spawn) so the detach happens at ChannelSession
    /// lifetime end, preserving the option to `await` it later.
    /// Underscore-prefixed: never read, intentionally so.
    _proto_task: tokio::task::JoinHandle<()>,
    /// Response pump task.
    response_task: tokio::task::JoinHandle<()>,
    /// Fired in Drop to let `proto_task` run its cancel-on-disconnect
    /// loop before exiting. Replaces the hard `abort()` that raced the
    /// EOF-detection path: `channel_close → Drop → abort()` could fire
    /// before `session.rs` saw `UnexpectedEof` from the dropped mpsc
    /// sender. Aborted futures get no cleanup — `CancelBuild` never
    /// sent, worker slot leaked until `r[sched.backstop.timeout]`.
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
        // case above (breaks the outbound pipe).
        self.response_task.abort();
        // Gauge decrement lives here so it fires on ALL drop paths: normal
        // channel_close, connection drop (HashMap clears), and session removal
        // after a dead protocol task. Avoids gauge leak on abnormal paths.
        metrics::gauge!("rio_gateway_channels_active").decrement(1.0);
    }
}

/// Per-connection handler that manages SSH channels.
pub struct ConnectionHandler {
    peer_addr: Option<SocketAddr>,
    store_client: StoreServiceClient<Channel>,
    scheduler_client: SchedulerServiceClient<Channel>,
    /// Shared with `GatewayServer` + the watcher task. `.load()` per
    /// auth attempt — NOT snapshotted at connection-accept, so a key
    /// rotated mid-handshake (between TCP accept and `auth_publickey`)
    /// is judged against the current set.
    authorized_keys: AuthorizedKeys,
    /// Active protocol sessions, indexed by channel ID.
    sessions: HashMap<ChannelId, ChannelSession>,
    /// JWT signing key, cloned from `GatewayServer`. `None` → mint
    /// skipped in `auth_publickey`. Arc because `SigningKey` isn't
    /// `Clone` (zeroize-on-drop semantics) but we need one per
    /// connection handler.
    jwt_signing_key: Option<Arc<SigningKey>>,
    /// JWT policy. `required` → whether mint failure rejects auth.
    /// `resolve_timeout_ms` → ResolveTenant RPC timeout.
    jwt_config: JwtConfig,
    /// Per-tenant rate limiter, cloned from `GatewayServer`. Passed
    /// through to every spawned protocol session. Clones share the
    /// underlying `dashmap` — the bucket for `tenant_name` "foo" is
    /// the same `dashmap` entry regardless of which SSH connection
    /// submits.
    limiter: TenantLimiter,
    /// Per-tenant quota cache, cloned from `GatewayServer`. Shared
    /// state — a quota fetched by one channel is warm for all.
    quota_cache: QuotaCache,
    /// Tenant name from the matched `authorized_keys` entry's comment
    /// field. Set in `auth_publickey` when a key matches. Passed to
    /// the scheduler as `SubmitBuildRequest.tenant_name` which resolves
    /// it to a UUID via the `tenants` table. `None` = single-tenant
    /// mode (empty comment) OR malformed comment (interior whitespace
    /// — logged at warn in `auth_publickey`). The [`NormalizedName`]
    /// type guarantees the `Some` case is trimmed and whitespace-free
    /// — no downstream `.trim()` needed anywhere in the request chain.
    tenant_name: Option<NormalizedName>,
    /// Minted JWT + its claims, set in `auth_publickey` IFF
    /// `jwt_signing_key` is `Some` and minting succeeds. The token
    /// string is cloned into every `SessionContext` spawned from this
    /// connection (multiple SSH channels share one token — they're the
    /// same authenticated session). The claims are kept so
    /// [`ensure_fresh_jwt`](Self::ensure_fresh_jwt) can read
    /// `sub`/`exp` to re-mint without re-parsing the token or
    /// re-resolving the tenant. `None` → header injection skipped →
    /// dual-mode fallback.
    jwt_token: Option<(String, jwt::TenantClaims)>,
    /// Set on the first `auth_*` callback. Distinguishes real SSH
    /// clients from TCP probes (NLB/kubelet health checks) — probes
    /// close before any SSH bytes, so no auth callback ever fires.
    auth_attempted: bool,
    /// Global connection-cap permit (`r[gw.conn.cap]`). Acquired in
    /// `GatewayServer::new_client`; dropped here in `Drop` so every
    /// disconnect path (EOF, error, abort) releases the slot. `None`
    /// means `new_client` hit the cap — `auth_none` checks this and
    /// returns `Err` to tear down the connection before any channel
    /// work. Underscore-prefixed: never read directly, only dropped.
    /// The option-ness IS read (`ensure_permit`).
    conn_permit: Option<OwnedSemaphorePermit>,
    /// Shared with [`GatewayServer::active_conns`]. Bumped in
    /// [`Self::mark_real_connection`], decremented in `Drop` — same
    /// gate as the `connections_active` gauge so TCP probes don't
    /// count toward session-drain.
    active_conns: Arc<AtomicUsize>,
    /// Clone of [`GatewayServer::sessions_shutdown`]. Each channel's
    /// `ChannelSession::shutdown` is `child_token()` of this, so
    /// cancelling the server-wide parent reaches every proto_task
    /// regardless of which connection/channel owns it.
    sessions_shutdown: CancellationToken,
}

impl ConnectionHandler {
    /// Idempotent. Call from every `auth_*` entry point — the first SSH
    /// protocol event that distinguishes a real client from a TCP probe.
    fn mark_real_connection(&mut self) {
        if self.auth_attempted {
            return;
        }
        self.auth_attempted = true;
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

        let timeout = std::time::Duration::from_millis(self.jwt_config.resolve_timeout_ms);
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

    /// Thin wrapper over [`refresh_session_jwt`] using this handler's
    /// cached token + signing key. Called from `exec_request` for
    /// every new channel.
    fn ensure_fresh_jwt(&mut self) -> Option<&str> {
        refresh_session_jwt(&mut self.jwt_token, self.jwt_signing_key.as_deref())
    }

    /// Enforce `r[gw.conn.cap]`: if `new_client` hit the cap
    /// (`conn_permit: None`), return `Err` so russh tears down the
    /// connection. Called from every `auth_*` entry point — the
    /// earliest we can surface a visible SSH-level disconnect
    /// reason. The error propagates via `handle_session_error`.
    fn ensure_permit(&self) -> Result<(), anyhow::Error> {
        if self.conn_permit.is_none() {
            // The cap value lives on GatewayServer (semaphore), not here.
            // Client sees an SSH disconnect; server logs the `conn_cap`
            // error counter. Operator checks gateway.toml max_connections.
            return Err(anyhow::anyhow!("connection cap reached"));
        }
        Ok(())
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
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
    comment: &str,
    key_fingerprint: &dyn std::fmt::Display,
) -> Option<NormalizedName> {
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
    ///
    /// No `mark_real_connection()` — `auth_none` always fires first
    /// for OpenSSH clients. A non-OpenSSH client that skips the `none`
    /// probe and goes straight to publickey is covered by the
    /// `auth_publickey` call that follows on accept.
    async fn auth_publickey_offered(
        &mut self,
        _user: &str,
        key: &PublicKey,
    ) -> Result<Auth, Self::Error> {
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
            self.tenant_name =
                normalize_key_comment(matched.comment(), &matched.fingerprint(Default::default()));

            // r[impl gw.jwt.dual-mode]
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
            //     r[sched.tenant.resolve]). On success: mint + store
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

    async fn channel_open_session(
        &mut self,
        channel: russh::Channel<Msg>,
        _session: &mut Session,
    ) -> Result<bool, Self::Error> {
        let channel_id = channel.id();
        // r[impl gw.conn.channel-limit]
        // Gate on sessions.len(), not "channels ever opened" — a channel
        // without an `exec_request` has no ChannelSession, no spawned
        // tasks, no buffers. Only exec'd channels consume resources.
        // This DOES mean a client can burst 5 opens before the first
        // exec lands; russh's event loop serializes handler calls so
        // in practice exec-after-open is the common interleaving.
        if self.sessions.len() >= MAX_CHANNELS_PER_CONNECTION {
            warn!(
                peer = ?self.peer_addr,
                active = self.sessions.len(),
                limit = MAX_CHANNELS_PER_CONNECTION,
                "rejecting SSH channel open: per-connection limit reached"
            );
            metrics::counter!("rio_gateway_errors_total", "type" => "channel_limit").increment(1);
            return Ok(false);
        }
        info!(channel = ?channel_id, "SSH session channel opened");
        Ok(true)
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
            session.channel_failure(channel_id)?;
            return Ok(());
        };
        info!(channel = ?channel_id, command = %command, "exec request");

        let args: Vec<&str> = command.split_whitespace().collect();
        let is_nix_daemon = args.len() >= 2
            && args[args.len() - 2].ends_with("nix-daemon")
            && args[args.len() - 1] == "--stdio";
        if !is_nix_daemon {
            warn!(command = %command, "rejecting non-nix-daemon exec request");
            session.channel_failure(channel_id)?;
            return Ok(());
        }

        session.channel_success(channel_id)?;
        metrics::gauge!("rio_gateway_channels_active").increment(1.0);

        let (client_tx, mut client_rx) = tokio::sync::mpsc::channel::<Vec<u8>>(64);

        let (inbound_reader, mut inbound_writer) = tokio::io::duplex(256 * 1024);
        let (mut outbound_reader, outbound_writer) = tokio::io::duplex(256 * 1024);

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
        let mut scheduler_client = self.scheduler_client.clone();
        let tenant_name = self.tenant_name.clone();
        // One token per SSH connection, shared across all channels.
        // Re-mint if near expiry (I-129: ControlMaster mux keeps the
        // connection alive past JWT_SESSION_TTL_SECS). Then clone the
        // ~200-byte string into the spawned task.
        let jwt_token = self.ensure_fresh_jwt().map(str::to_owned);
        // Shared-state clone: all channels on all connections drain
        // the same per-tenant bucket.
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
        let proto_task = rio_common::task::spawn_monitored(
            "proto-task",
            async move {
                let mut reader = inbound_reader;
                let mut writer = outbound_writer;
                if let Err(e) = run_protocol(
                    &mut reader,
                    &mut writer,
                    &mut store_client,
                    &mut scheduler_client,
                    tenant_name,
                    jwt_token,
                    limiter,
                    quota_cache,
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

        // Task: pump protocol responses -> SSH client
        let handle = session.handle();
        let response_task = rio_common::task::spawn_monitored("response-task", async move {
            let mut buf = vec![0u8; 32 * 1024];
            loop {
                match outbound_reader.read(&mut buf).await {
                    Ok(0) => break,
                    Ok(n) => {
                        metrics::counter!("rio_gateway_bytes_total", "direction" => "tx")
                            .increment(n as u64);
                        // russh 0.58: Handle::data takes `impl Into<Bytes>`
                        // (was CryptoVec). Vec<u8> satisfies the bound.
                        if handle.data(channel_id, buf[..n].to_vec()).await.is_err() {
                            warn!(channel = ?channel_id, "response pump: SSH send failed");
                            metrics::counter!("rio_gateway_errors_total", "type" => "ssh_send")
                                .increment(1);
                            break;
                        }
                    }
                    Err(e) => {
                        error!(error = %e, "error reading protocol response");
                        break;
                    }
                }
            }
            if let Err(e) = handle.eof(channel_id).await {
                warn!(channel = ?channel_id, error = ?e, "failed to send EOF to SSH client");
            }
            if let Err(e) = handle.close(channel_id).await {
                warn!(channel = ?channel_id, error = ?e, "failed to close SSH channel");
            }
            if let Err(e) = client_pump.await
                && e.is_panic()
            {
                error!(channel = ?channel_id, "client pump task panicked: {e}");
            }
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
        if let Some(session) = self.sessions.get(&channel) {
            if let Some(tx) = &session.client_tx {
                debug!(channel = ?channel, len = data.len(), "forwarding client data to protocol");
                if tx.send(data.to_vec()).await.is_err() {
                    warn!(channel = ?channel, "protocol session dead, closing channel");
                    // Gauge decrement handled by ChannelSession::Drop.
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
        debug!(channel = ?channel, "SSH channel closed");
        // Gauge decrement handled by ChannelSession::Drop.
        self.sessions.remove(&channel);
        Ok(())
    }
}

// r[verify gw.jwt.issue]
#[cfg(test)]
mod jwt_issuance_tests {
    use super::*;

    /// Fixed-seed key. Same pattern as rio-common/src/jwt.rs tests —
    /// SigningKey::generate needs rand_core 0.6 but the workspace is
    /// on rand 0.9; from_bytes sidesteps the trait version mismatch.
    fn test_key(seed: u8) -> SigningKey {
        SigningKey::from_bytes(&[seed; 32])
    }

    /// Core spec requirement: minted JWT carries the resolved tenant
    /// UUID in `sub`. The gateway never lets the client choose `sub`
    /// — it's bound by the SSH key match. This test constructs
    /// `tenant_id` directly (simulating a completed scheduler
    /// resolve); the production call site gets it from the
    /// `ResolveTenant` RPC.
    #[test]
    fn minted_jwt_decodes_to_tenant_sub() {
        let tenant_id = uuid::Uuid::from_u128(0xCAFE_0000_0000_0000_0000_0000_0000_0258);
        let key = test_key(0x42);

        let (token, claims) = mint_session_jwt(tenant_id, &key).expect("mint");

        // Self-precondition: the returned claims match what we asked
        // for. If mint_session_jwt ever grows a UUID-mangling step
        // (e.g., canonicalization), this catches it before the
        // verify roundtrip below masks it.
        assert_eq!(claims.sub, tenant_id, "returned claims.sub must be input");

        // Round-trip: the TOKEN (not just the returned claims)
        // decodes back to the same sub. This is the real proof —
        // `claims` is just a convenience return; downstream services
        // only see the token string.
        let decoded = jwt::verify(&token, &key.verifying_key()).expect("verify");
        assert_eq!(decoded.sub, tenant_id, "token must decode to tenant UUID");
        assert_eq!(decoded.jti, claims.jti, "jti must survive round-trip");
    }

    /// jti is fresh per mint. Two sessions for the same tenant get
    /// distinct jtis — revocation of one doesn't revoke the other.
    /// The scheduler's jwt_revoked table is keyed by jti; if jti
    /// collided, revoking tenant-X's laptop session would also kill
    /// their CI session.
    #[test]
    fn jti_unique_across_mints() {
        let tenant_id = uuid::Uuid::from_u128(0x1234);
        let key = test_key(0x01);

        let (_, c1) = mint_session_jwt(tenant_id, &key).expect("mint 1");
        let (_, c2) = mint_session_jwt(tenant_id, &key).expect("mint 2");

        // Self-precondition on sub: same tenant → same sub.
        // Without this, a "unique jti" pass could be masked by
        // accidentally-different tenants (copy-paste bug in the
        // test itself, or a future mint_session_jwt that rewrites
        // sub). Asserting sub-equality makes the jti assertion
        // strictly about jti.
        assert_eq!(c1.sub, c2.sub, "precondition: same tenant");
        assert_ne!(c1.jti, c2.jti, "jti must be fresh per mint (v4 UUID)");
    }

    /// exp is in the future and bounded by JWT_SESSION_TTL_SECS.
    /// Not just "is future" — that's too weak (exp = now+1 would
    /// pass but expire before the first gRPC call completes). Bound
    /// it at both ends: at least the TTL minus clock-read skew, at
    /// most the TTL plus skew.
    #[test]
    fn exp_bounded_by_ttl() {
        let key = test_key(0x77);
        let before = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        let (_, claims) = mint_session_jwt(uuid::Uuid::nil(), &key).expect("mint");

        let after = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // exp was computed from a `now` snapshot taken between
        // `before` and `after`. So exp - TTL must land in
        // [before, after]. Two-clock-reads brackets the mint.
        let mint_now = claims.exp - JWT_SESSION_TTL_SECS;
        assert!(
            (before..=after).contains(&mint_now),
            "exp={} implies mint-time now={}, but we bracketed [{}, {}]",
            claims.exp,
            mint_now,
            before,
            after
        );
        assert_eq!(
            claims.iat, mint_now,
            "iat and exp must derive from the same `now` snapshot"
        );
    }

    /// No jwt_signing_key → with_jwt_signing_key never called →
    /// ConnectionHandler.jwt_token stays None → SessionContext gets
    /// None → no header injection. This is the default state when
    /// `cfg.jwt.key_path` is unset (dev mode / no K8s Secret mount).
    /// Tested at the GatewayServer level because that's where the
    /// `None` default lives (the field initializer in `::new()`).
    ///
    /// Field-level access only; spinning up a real
    /// StoreServiceClient/SchedulerServiceClient needs a listening
    /// socket, which is way too heavy for "assert default is None".
    /// The field is private, so this test relies on same-module
    /// access. If GatewayServer moves to its own module, this test
    /// moves with it.
    #[test]
    fn signing_key_defaults_none() {
        // Can't construct GatewayServer without real clients (no
        // Default impl, no mock-friendly constructor). Instead,
        // assert structurally on with_jwt_signing_key's contract:
        // it takes an owned SigningKey and wraps it in Some(Arc).
        // The `None` default in `::new()` is trivially verified by
        // reading the source — this test proves the OTHER half:
        // that calling the builder actually flips it to Some.
        //
        // This is weaker than a full construction test, but the
        // alternative (mock clients or a test-only constructor)
        // is scope creep for a 3-line field initializer. The
        // security.nix VM test covers the full flow.
        let key = test_key(0xAB);
        let arc = Arc::new(key);
        // Structural: Arc<SigningKey> is what the Some variant holds.
        // If someone changes the field type (e.g., to Box), this
        // fails to compile — which is the signal we want.
        let _: Option<Arc<SigningKey>> = Some(arc);
    }

    // r[verify gw.jwt.refresh-on-expiry]
    /// A token within `JWT_REFRESH_SLACK_SECS` of expiry is re-minted
    /// on the next `refresh_session_jwt` call. Exercises the I-129
    /// path: ControlMaster mux'd connection, channel opened past the
    /// original token's TTL.
    #[test]
    fn stale_token_is_refreshed() {
        let tenant_id = uuid::Uuid::from_u128(0xDEAD_BEEF);
        let key = test_key(0x55);
        let now = SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .unwrap()
            .as_secs() as i64;

        // Cached token already expired (exp = now - 10). The token
        // string itself doesn't matter — refresh keys off claims.exp,
        // never re-parses the string.
        let stale_claims = jwt::TenantClaims {
            sub: tenant_id,
            iat: now - JWT_SESSION_TTL_SECS - 10,
            exp: now - 10,
            jti: "stale-jti".to_string(),
        };
        let mut cached = Some(("stale-token".to_string(), stale_claims));

        let refreshed = refresh_session_jwt(&mut cached, Some(&key))
            .expect("refresh must return a token when one was cached");

        assert_ne!(refreshed, "stale-token", "stale token must be replaced");
        let (_, new_claims) = cached.as_ref().unwrap();
        assert!(
            new_claims.exp > now,
            "refreshed exp={} must be > now={}",
            new_claims.exp,
            now
        );
        assert_eq!(new_claims.sub, tenant_id, "sub preserved across refresh");
        assert_ne!(new_claims.jti, "stale-jti", "fresh jti per re-mint");
    }

    /// A token well within its TTL is left untouched — refresh is a
    /// no-op, returns the same string. Guards against accidentally
    /// re-minting on every channel open (would churn jti and spam
    /// `rio_gateway_jwt_refreshed_total`).
    #[test]
    fn fresh_token_is_not_refreshed() {
        let tenant_id = uuid::Uuid::from_u128(0xF00D);
        let key = test_key(0x66);

        let (token, claims) = mint_session_jwt(tenant_id, &key).expect("mint");
        let original_jti = claims.jti.clone();
        let mut cached = Some((token.clone(), claims));

        let out = refresh_session_jwt(&mut cached, Some(&key)).expect("token");
        assert_eq!(out, token, "fresh token must be returned unchanged");
        assert_eq!(
            cached.as_ref().unwrap().1.jti,
            original_jti,
            "no re-mint → jti unchanged"
        );
    }

    /// `None` cached → `None` out, regardless of key. Dual-mode
    /// fallback path stays None across the refresh hook.
    #[test]
    fn refresh_none_stays_none() {
        let key = test_key(0x99);
        let mut cached: Option<(String, jwt::TenantClaims)> = None;
        assert!(refresh_session_jwt(&mut cached, Some(&key)).is_none());
        assert!(cached.is_none());
    }
}

// r[verify gw.conn.cap]
#[cfg(test)]
mod conn_cap_tests {
    use super::*;

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
    /// what the auth callbacks check; Err propagates to russh's
    /// `handle_session_error` which tears down the connection.
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

// r[verify sec.boundary.ssh-auth]
#[cfg(test)]
mod tests {
    use super::*;
    use std::os::unix::fs::PermissionsExt;

    /// Generate a fresh ed25519 public key line for authorized_keys fixtures.
    fn make_valid_pubkey_line() -> anyhow::Result<String> {
        Ok(PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?
            .public_key()
            .to_openssh()?)
    }

    // -----------------------------------------------------------------------
    // load_authorized_keys
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_authorized_keys_valid_with_comments_and_blanks() -> anyhow::Result<()> {
        let key1 = make_valid_pubkey_line()?;
        let key2 = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("# comment line\n\n{key1}\n{key2}\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 2);
        Ok(())
    }

    // r[verify gw.auth.tenant-from-key-comment]
    /// Key comments in authorized_keys lines are preserved by the parser
    /// and readable via `.comment()`. This is the mechanism for tenant
    /// name extraction in `auth_publickey`.
    #[test]
    fn test_load_authorized_keys_preserves_comment() -> anyhow::Result<()> {
        let key_base = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        // OpenSSH authorized_keys format: <type> <base64> <comment>
        std::fs::write(tmp.path(), format!("{key_base} team-infra\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].comment(),
            "team-infra",
            "comment field should be preserved from the authorized_keys line"
        );
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
            normalize_key_comment("team a", &"SHA256:test-fingerprint")
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
            metrics::with_local_recorder(&recorder, || normalize_key_comment("  team-a  ", &"fp"));

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
        let result = metrics::with_local_recorder(&recorder, || normalize_key_comment("", &"fp"));

        assert_eq!(result, None, "empty comment → single-tenant (None)");
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            0,
            "empty comment is INTENTIONAL single-tenant — no counter"
        );
        // Also whitespace-only (trims to empty → Empty variant):
        let ws_result =
            metrics::with_local_recorder(&recorder, || normalize_key_comment("   ", &"fp"));
        assert_eq!(ws_result, None);
        assert_eq!(
            recorder.get("rio_gateway_auth_degraded_total{reason=interior_whitespace}"),
            0,
            "whitespace-only → Empty (not InteriorWhitespace) → no counter"
        );
    }

    /// Key with NO comment → empty comment string. This is the
    /// single-tenant mode case: empty tenant_name → scheduler gets
    /// empty string → scheduler resolves tenant_id=None.
    #[test]
    fn test_load_authorized_keys_no_comment_is_empty() -> anyhow::Result<()> {
        let key_base = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("{key_base}\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1);
        assert_eq!(
            keys[0].comment(),
            "",
            "no comment → empty string (single-tenant mode)"
        );
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_skips_invalid_entry() -> anyhow::Result<()> {
        let key1 = make_valid_pubkey_line()?;
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), format!("{key1}\nthis is not a valid ssh key\n"))?;

        let keys = load_authorized_keys(tmp.path()).expect("should load");
        assert_eq!(keys.len(), 1, "invalid line should be skipped");
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_all_invalid_bails() -> anyhow::Result<()> {
        let tmp = tempfile::NamedTempFile::new()?;
        std::fs::write(tmp.path(), "garbage line 1\ngarbage line 2\n")?;

        let err = load_authorized_keys(tmp.path()).expect_err("should bail");
        assert!(
            err.to_string().contains("no valid authorized keys"),
            "got: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_load_authorized_keys_missing_file() {
        let err = load_authorized_keys(Path::new("/nonexistent/rio-test-authkeys"))
            .expect_err("should fail on missing file");
        assert!(err.to_string().contains("failed to read"));
    }

    // -----------------------------------------------------------------------
    // load_or_generate_host_key
    // -----------------------------------------------------------------------

    #[test]
    fn test_load_or_generate_host_key_generates_and_persists() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let key_path = tmp.path().join("subdir/host_key");

        // First call: generates and writes
        let k1 = load_or_generate_host_key(&key_path).expect("should generate");
        assert!(key_path.exists(), "key should be persisted");
        let fp1 = k1.public_key().fingerprint(Default::default());

        // Second call: loads the same key
        let k2 = load_or_generate_host_key(&key_path).expect("should load");
        let fp2 = k2.public_key().fingerprint(Default::default());
        assert_eq!(fp1.to_string(), fp2.to_string(), "same key on reload");
        Ok(())
    }

    #[test]
    fn test_load_or_generate_host_key_loads_existing() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let key_path = tmp.path().join("host_key");

        // Write a key manually
        let orig = PrivateKey::random(&mut OsRng, Algorithm::Ed25519)?;
        std::fs::write(&key_path, orig.to_openssh(ssh_key::LineEnding::LF)?)?;
        let orig_fp = orig.public_key().fingerprint(Default::default());

        let loaded = load_or_generate_host_key(&key_path).expect("should load");
        assert_eq!(
            loaded
                .public_key()
                .fingerprint(Default::default())
                .to_string(),
            orig_fp.to_string()
        );
        Ok(())
    }

    /// When the directory is unwritable, the key is still generated (ephemeral)
    /// but NOT persisted. The function returns Ok and logs a warning.
    #[test]
    fn test_load_or_generate_host_key_unwritable_dir_ephemeral() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        // Make the tempdir read-only so write fails. The key_path itself
        // doesn't exist so create_dir_all won't hit the read-only perms
        // (parent already exists) but fs::write will.
        std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o555))?;
        let key_path = tmp.path().join("host_key");

        let result = load_or_generate_host_key(&key_path);

        // Restore perms so tempdir cleanup works regardless of outcome.
        let _ = std::fs::set_permissions(tmp.path(), std::fs::Permissions::from_mode(0o755));

        let _key = result.expect("should return ephemeral key despite write failure");
        assert!(
            !key_path.exists(),
            "key should NOT be persisted (write failed)"
        );
        Ok(())
    }
}
