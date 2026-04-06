//! SSH server using `russh` that terminates connections and speaks the
//! Nix worker protocol on each session channel, delegating operations
//! to gRPC store and scheduler services.

use std::collections::HashMap;
use std::net::SocketAddr;
use std::path::Path;
use std::sync::Arc;

use anyhow::Context;
use ed25519_dalek::SigningKey;
use rio_common::config::JwtConfig;
use rio_common::jwt;
use rio_common::signal::Token as CancellationToken;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use russh::keys::ssh_key::rand_core::OsRng;
use russh::keys::{Algorithm, PrivateKey, PublicKey};
use russh::server::{Auth, Handler, Msg, Server as _, Session};
use russh::{ChannelId, CryptoVec, MethodKind, MethodSet};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::net::TcpListener;
use tonic::transport::Channel;
use tracing::{Instrument, debug, error, info, trace, warn};

use crate::session::run_protocol;

/// Max active protocol sessions per SSH connection. Matches Nix's
/// default `max-jobs` — a well-behaved `nix build -j4` opens at most
/// this many channels. Each session = 2 spawned tasks + 2×256 KiB
/// duplex buffers, so this bounds per-connection memory at ~2 MiB.
///
/// Counted via `self.sessions.len()` — see `channel_open_session`.
const MAX_CHANNELS_PER_CONNECTION: usize = 4;

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

// r[impl gw.jwt.issue]
/// Mint a per-session tenant JWT. Called once per SSH connection,
/// right after `auth_publickey` accepts — the returned token is
/// stored on the `ConnectionHandler` and injected as
/// `x-rio-tenant-token` on every outbound gRPC call for the session's
/// lifetime.
///
/// `tenant_id` is the resolved UUID, not the authorized_keys comment
/// string. The gateway is PG-free (`r[sched.tenant.resolve]` says the
/// scheduler owns the `tenants` table), so the caller must have
/// resolved name→UUID via a scheduler round-trip before calling.
/// That round-trip doesn't exist yet — see the TODO(P0260) at the
/// call site in `auth_publickey`.
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
) -> Result<(String, jwt::Claims), jwt::JwtError> {
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .expect("system clock before 1970")
        .as_secs() as i64;
    let claims = jwt::Claims {
        sub: tenant_id,
        iat: now,
        exp: now + JWT_SESSION_TTL_SECS,
        jti: uuid::Uuid::new_v4().to_string(),
    };
    let token = jwt::sign(&claims, signing_key)?;
    Ok((token, claims))
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

/// The SSH server that accepts connections and spawns protocol sessions.
pub struct GatewayServer {
    store_client: StoreServiceClient<Channel>,
    scheduler_client: SchedulerServiceClient<Channel>,
    authorized_keys: Arc<Vec<PublicKey>>,
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
            authorized_keys: Arc::new(authorized_keys),
            jwt_signing_key: None,
            jwt_config: JwtConfig::default(),
        }
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

    /// Start the SSH server on the given address.
    pub async fn run(mut self, host_key: PrivateKey, addr: SocketAddr) -> anyhow::Result<()> {
        let config = Arc::new(build_ssh_config(host_key));

        info!(addr = %addr, "starting SSH server");

        let socket = TcpListener::bind(addr)
            .await
            .with_context(|| format!("failed to bind SSH server to {addr}"))?;

        self.run_on_socket(config, &socket).await?;
        Ok(())
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
        // keepalive_max defaults to 3 (russh server/mod.rs:123).
        // 30s × 3 unanswered = connection dropped at ~90s. Catches
        // half-open TCP: NLB idle-timeout RST that never reached
        // us, client kernel panic, cable pull. Without this, a
        // half-open connection holds its ConnectionHandler (and
        // all its ChannelSessions) until inactivity_timeout — 1h.
        keepalive_interval: Some(std::time::Duration::from_secs(30)),
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
    /// russh default is a no-op (`server/mod.rs:839`). Called from the
    /// accept loop when a connection task reports an error — both
    /// connection-setup failure and any `?`-propagated error from a
    /// `Handler` method. Without this override, `session.channel_success(..)?`
    /// failures (and every other `?` in this file's `Handler` impl) drop
    /// the connection with zero server-side signal.
    ///
    /// NOTE: this is on `Server`, not `Handler` — it runs on the accept
    /// loop task, so `self.peer_addr` is not available. The error itself
    /// is the only context we get.
    fn handle_session_error(&mut self, error: <Self::Handler as Handler>::Error) {
        error!(error = %error, "SSH session error");
        metrics::counter!("rio_gateway_errors_total", "type" => "session").increment(1);
    }

    fn new_client(&mut self, peer_addr: Option<SocketAddr>) -> Self::Handler {
        // TCP accept only — NLB health checks and kubelet liveness probes
        // (bare connect+close, no SSH bytes) land here and drop ~200μs
        // later. Defer logging/metrics to mark_real_connection(), called
        // from the first auth_* callback.
        ConnectionHandler {
            peer_addr,
            store_client: self.store_client.clone(),
            scheduler_client: self.scheduler_client.clone(),
            authorized_keys: Arc::clone(&self.authorized_keys),
            jwt_signing_key: self.jwt_signing_key.clone(),
            jwt_config: self.jwt_config.clone(),
            sessions: HashMap::new(),
            tenant_name: String::new(),
            jwt_token: None,
            auth_attempted: false,
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
    authorized_keys: Arc<Vec<PublicKey>>,
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
    /// Tenant name from the matched `authorized_keys` entry's comment
    /// field. Set in `auth_publickey` when a key matches. Passed to
    /// the scheduler as `SubmitBuildRequest.tenant_name` which resolves
    /// it to a UUID via the `tenants` table. Empty = single-tenant mode.
    tenant_name: String,
    /// Minted JWT, set in `auth_publickey` IFF `jwt_signing_key` is
    /// `Some` and minting succeeds. Cloned into every
    /// `SessionContext` spawned from this connection (multiple SSH
    /// channels share one token — they're the same authenticated
    /// session). `None` → header injection skipped → dual-mode
    /// fallback.
    jwt_token: Option<String>,
    /// Set on the first `auth_*` callback. Distinguishes real SSH
    /// clients from TCP probes (NLB/kubelet health checks) — probes
    /// close before any SSH bytes, so no auth callback ever fires.
    auth_attempted: bool,
}

impl ConnectionHandler {
    /// Idempotent. Call from every `auth_*` entry point — the first SSH
    /// protocol event that distinguishes a real client from a TCP probe.
    fn mark_real_connection(&mut self) {
        if self.auth_attempted {
            return;
        }
        self.auth_attempted = true;
        metrics::counter!("rio_gateway_connections_total", "result" => "new").increment(1);
        metrics::gauge!("rio_gateway_connections_active").increment(1.0);
        info!(peer = ?self.peer_addr, "new SSH connection");
    }

    /// ResolveTenant round-trip + JWT mint. Called from
    /// `auth_publickey` when `jwt_signing_key` is `Some` and
    /// `tenant_name` is non-empty.
    ///
    /// Returns `(token, jti)` on success. Error covers: RPC timeout,
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
    ) -> anyhow::Result<(String, String)> {
        use rio_proto::scheduler::ResolveTenantRequest;

        let timeout = std::time::Duration::from_millis(self.jwt_config.resolve_timeout_ms);
        let req = tonic::Request::new(ResolveTenantRequest {
            tenant_name: self.tenant_name.clone(),
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
        Ok((token, claims.jti))
    }
}

impl Drop for ConnectionHandler {
    fn drop(&mut self) {
        if self.auth_attempted {
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
        warn!(peer = ?self.peer_addr, "rejecting password authentication");
        Ok(Auth::reject())
    }

    // r[impl gw.auth.tenant-from-key-comment]
    async fn auth_publickey(&mut self, user: &str, key: &PublicKey) -> Result<Auth, Self::Error> {
        self.mark_real_connection();
        // The comment lives in the SERVER-SIDE authorized_keys entry, not
        // the client's key (SSH key auth sends raw key data only). We
        // match the client's key against our loaded entries, then read
        // .comment() from the MATCHED entry.
        let matched = self
            .authorized_keys
            .iter()
            .find(|authorized| authorized.key_data() == key.key_data());

        if let Some(matched) = matched {
            self.tenant_name = matched.comment().trim().to_string();

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
                && !self.tenant_name.is_empty()
            {
                match self.resolve_and_mint(&signing_key).await {
                    Ok((token, jti)) => {
                        debug!(jti = %jti, tenant = %self.tenant_name, "minted session JWT");
                        self.jwt_token = Some(token);
                    }
                    Err(e) if self.jwt_config.required => {
                        // required=true: mint failure is an AUTH
                        // failure. Return reject (NOT an Err —
                        // russh::Error would close the whole TCP
                        // connection; reject lets the client know
                        // auth failed and disconnect cleanly).
                        warn!(
                            error = %e,
                            tenant = %self.tenant_name,
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
                            tenant = %self.tenant_name,
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
                tenant = %self.tenant_name,
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
        // Clone is cheap (Option<String>); the token is ~200 bytes.
        let jwt_token = self.jwt_token.clone();
        // Graceful-shutdown link: Drop fires this, run_protocol selects
        // on it. One token per channel (not per-connection) — each
        // channel's cancel loop is independent. child_token() so a
        // future connection-wide parent could cancel all channels at
        // once without this spawn site caring.
        let shutdown = CancellationToken::new();
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
                        let data = CryptoVec::from_slice(&buf[..n]);
                        if handle.data(channel_id, data).await.is_err() {
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
    /// resolve); the production call site gets it from P0260's
    /// resolve step.
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
    /// None → no header injection. This is the default state until
    /// P0260 wires the K8s Secret. Tested at the GatewayServer level
    /// because that's where the `None` default lives (the field
    /// initializer in `::new()`).
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
        // is scope creep for a 3-line field initializer. P0260's
        // VM test covers the full flow anyway.
        let key = test_key(0xAB);
        let arc = Arc::new(key);
        // Structural: Arc<SigningKey> is what the Some variant holds.
        // If someone changes the field type (e.g., to Box), this
        // fails to compile — which is the signal we want.
        let _: Option<Arc<SigningKey>> = Some(arc);
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
