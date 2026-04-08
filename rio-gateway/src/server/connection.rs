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
use std::sync::atomic::{AtomicUsize, Ordering};

use ed25519_dalek::SigningKey;
use rio_common::config::JwtConfig;
use rio_common::jwt;
use rio_common::signal::Token as CancellationToken;
use rio_common::tenant::{NameError, NormalizedName};
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use russh::ChannelId;
use russh::keys::PublicKey;
use russh::server::{Auth, Handler, Msg, Session};
use tokio::io::{AsyncReadExt, AsyncWriteExt};
use tokio::sync::OwnedSemaphorePermit;
use tonic::transport::Channel;
use tracing::{Instrument, debug, error, info, trace, warn};

use super::AuthorizedKeys;
use super::session_jwt::{mint_session_jwt, refresh_session_jwt};
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
    pub(super) peer_addr: Option<SocketAddr>,
    pub(super) store_client: StoreServiceClient<Channel>,
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
    /// [`ensure_fresh_jwt`](Self::ensure_fresh_jwt) can read
    /// `sub`/`exp` to re-mint without re-parsing the token or
    /// re-resolving the tenant. `None` → header injection skipped →
    /// dual-mode fallback.
    pub(super) jwt_token: Option<(String, jwt::TenantClaims)>,
    /// Set on the first `auth_*` callback. Distinguishes real SSH
    /// clients from TCP probes (NLB/kubelet health checks) — probes
    /// close before any SSH bytes, so no auth callback ever fires.
    pub(super) auth_attempted: bool,
    /// Global connection-cap permit (`r[gw.conn.cap]`). Acquired in
    /// `GatewayServer::new_client`; dropped here in `Drop` so every
    /// disconnect path (EOF, error, abort) releases the slot. `None`
    /// means `new_client` hit the cap — `auth_none` checks this and
    /// returns `Err` to tear down the connection before any channel
    /// work. Underscore-prefixed: never read directly, only dropped.
    /// The option-ness IS read (`ensure_permit`).
    pub(super) conn_permit: Option<OwnedSemaphorePermit>,
    /// Shared with [`GatewayServer::active_conns`]. Bumped in
    /// [`Self::mark_real_connection`], decremented in `Drop` — same
    /// gate as the `connections_active` gauge so TCP probes don't
    /// count toward session-drain.
    pub(super) active_conns: Arc<AtomicUsize>,
    /// Clone of [`GatewayServer::sessions_shutdown`]. Each channel's
    /// `ChannelSession::shutdown` is `child_token()` of this, so
    /// cancelling the server-wide parent reaches every proto_task
    /// regardless of which connection/channel owns it.
    pub(super) sessions_shutdown: CancellationToken,
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

#[cfg(test)]
mod tests {
    use super::*;

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
}
