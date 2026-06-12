//! Per-SSH-channel protocol session state machine.
//!
//! Each SSH channel runs an independent protocol session with its own
//! handshake, option negotiation, derivation cache, and build tracking.

use rio_common::signal::Token as CancellationToken;
use rio_common::tenant::NormalizedName;
use rio_nix::protocol::handshake;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite, AsyncWriteExt};
use tonic::transport::Channel;
use tracing::{debug, error, info, warn};

use crate::handler::{self, SessionContext, with_jwt};
use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;

/// Best-effort cancel of all builds tracked in `active_build_ids`.
///
/// Called from EXACTLY ONE place — the typed-exit chokepoint in
/// [`run_protocol_loop`] (bug_319) — so
/// `r[gw.conn.cancel-on-disconnect]` holds on EVERY path out of the
/// protocol loop by construction, not by per-arm enumeration. The
/// historical exit shapes (each now a [`SessionExit`] variant; the
/// chokepoint also covers idle-timeout, the post-opcode flush
/// failure that falsified the old 'ALL six exits' enumeration, and
/// the handshake-phase ends):
///
/// 1. **Between-opcode EOF** — client sent clean EOF while we waited for
///    the next opcode; `wire::read_u64` returns `UnexpectedEof`.
/// 2. **Mid-opcode handler error** — typically `BrokenPipe` on a stderr
///    write (client dropped during a build). The build handler leaves
///    the build_id in the map on `StreamProcessError::Wire` (see
///    `handler/build.rs`) specifically so this loop has something to
///    cancel.
/// 3. **Graceful-shutdown signal** — `ChannelSession::Drop` (server.rs)
///    fires a `CancellationToken` instead of hard-aborting `proto_task`.
///    The opcode-read `select!` picks it up and routes here. Without the
///    token, `channel_close → Drop → abort()` could fire before path 1
///    or 2 reached the cancel loop — the abort drops the future, no
///    cleanup runs, build leaks until `r[sched.backstop.orphan-watcher]`.
/// 4. **Mid-opcode shutdown** — same token as (3), but firing while
///    `handle_opcode` is awaiting (e.g. inside `process_build_events`
///    on the scheduler stream). The opcode-read `select!` is past at
///    that point. I-157: `wait_for_session_drain` (main.rs) fires
///    `sessions_shutdown` after `session_drain_secs` with NO pipe
///    break — the handler's next write never fails because it's
///    blocked on the inbound stream. The handle_opcode-wrapping
///    `select!` below catches this; dropping the handler future is
///    safe (build_id is in `active_build_ids` since SubmitBuild
///    returned, gRPC stream reset is fine — we send `CancelBuild`
///    explicitly here, and the SSH wire is dying within main.rs's
///    `CANCEL_GRACE` regardless).
///
/// The `reason` string propagates to `CancelBuildRequest.reason`:
/// `"client_disconnect"` for paths 1+2 (the client went away),
/// `"channel_close"` for path 3 (russh told us the channel is gone).
/// Distinguishes "did the client send SSH-EOF cleanly or did the TCP
/// connection die?" in scheduler-side debugging.
async fn cancel_active_builds(ctx: &mut SessionContext, reason: &str) {
    // Collect build_ids first to avoid holding an immutable borrow on
    // active_build_ids across the await points (scheduler_client.cancel_build
    // needs &mut ctx.scheduler_client — split field borrows don't survive
    // across .await in all cases).
    let build_ids: Vec<String> = ctx.active_build_ids.iter().cloned().collect();
    let jwt_token = ctx.jwt.token_owned();
    let jwt_token = jwt_token.as_deref();
    for build_id in build_ids {
        debug!(build_id = %build_id, reason = %reason, "cancelling build on disconnect");
        // r[impl gw.jwt.propagate]
        // with_jwt on a JWT can't actually fail (handler/mod.rs:47-51) but
        // this loop is best-effort either way: log+skip rather than `?`.
        let req = match with_jwt(
            types::CancelBuildRequest {
                build_id: build_id.clone(),
                reason: reason.to_string(),
            },
            jwt_token,
        ) {
            Ok(r) => r,
            Err(e) => {
                warn!(build_id = %build_id, error = %e, "with_jwt failed; skipping cancel");
                continue;
            }
        };
        // Best-effort cancel: wrap in timeout so an unreachable
        // scheduler doesn't block the disconnect cleanup loop.
        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            // no-jwt: best-effort cleanup on a closing session;
            // CancelBuild keys on build_id only (no tenant scope) and
            // attaching trace context here would orphan-root anyway —
            // the session span is being torn down.
            ctx.scheduler_client.cancel_build(req),
        )
        .await
        {
            Ok(Ok(_)) => {
                debug!(build_id = %build_id, "cancelled build on disconnect");
            }
            Ok(Err(e)) => {
                warn!(build_id = %build_id, error = %e, "failed to cancel build on disconnect");
                // sh-009: the cancel did not reach the scheduler. The
                // build is now orphaned until
                // `r[sched.backstop.orphan-watcher]` reaps it; this
                // counter is the gateway-side observable for that gap.
                metrics::counter!(
                    "rio_gateway_builds_leaked_on_disconnect_total",
                    "reason" => "rpc_error"
                )
                .increment(1);
            }
            Err(_) => {
                warn!(build_id = %build_id, "cancel_build timed out on disconnect");
                metrics::counter!(
                    "rio_gateway_builds_leaked_on_disconnect_total",
                    "reason" => "rpc_timeout"
                )
                .increment(1);
            }
        }
    }
}

/// Max time to wait for the NEXT opcode after the previous one completes.
/// Catches alive-but-idle clients (handshake, then silence). Does NOT
/// bound per-opcode duration — `handle_opcode` runs outside this timer,
/// so a 3-hour `wopBuildDerivation` is unaffected. Real Nix clients'
/// inter-opcode gap is milliseconds.
const OPCODE_IDLE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(600);

// r[impl gw.handshake.timeout]
/// Max time to wait for the FIRST byte (`WORKER_MAGIC_1`) through the
/// handshake completing. Guards the gap before `OPCODE_IDLE_TIMEOUT`
/// applies: an authenticated client that opens a channel + sends the
/// exec_request then goes silent would otherwise park on
/// `read_u64(WORKER_MAGIC_1)` forever (russh keepalives keep transport
/// alive). Real Nix clients complete the handshake in <100ms; 30s is
/// generous but far below the 600s opcode-idle bound — no legitimate
/// client idles before handshake.
///
/// This is the default for `SessionContext::handshake_timeout`; the SSH
/// layer can override it per server via
/// `GatewayServer::with_handshake_timeout` (tests shrink it so the
/// exec'd-but-never-speaks scenario resolves in milliseconds).
pub const HANDSHAKE_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);

// r[impl gw.conn.per-channel-state]
// r[impl gw.conn.sequential]
// r[impl gw.conn.lifecycle]
// r[impl gw.handshake.phases]
// r[impl gw.compat.version-range+2]
/// Runs the Nix worker protocol on separate read/write streams,
/// delegating store operations to `StoreServiceClient` and build
/// operations to `SchedulerServiceClient`.
// 9 args is over clippy's default of 7. The alternatives
// (grouping into a struct, or building SessionContext at the call
// site) both add more noise than the extra args cost. The session
// entry point is the natural narrowing-point: everything before is
// SSH plumbing, everything after is protocol handling.
#[allow(clippy::too_many_arguments)]
#[tracing::instrument(
    name = "session",
    skip_all,
    // `NormalizedName` derefs to str; `-` for single-tenant mode
    // (None) so the span field is never blank in log grep.
    fields(tenant = tenant_name.as_deref().unwrap_or("-"))
)]
pub async fn run_protocol<R, W>(
    reader: &mut R,
    writer: &mut W,
    store_client: &mut StoreServiceClient<Channel>,
    log_client: &mut rio_proto::LogServiceClient<Channel>,
    // DrvBlobService client on the store channel. `None` -> the build
    // handlers skip ADR-024 drv-digest population (legacy submission).
    drv_blob_client: Option<rio_proto::DrvBlobServiceClient<Channel>>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    tenant_name: Option<NormalizedName>,
    jwt: crate::handler::SessionJwt,
    service_signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    // Per-tenant rate limiter, shared across all sessions via
    // `Arc`-inside-`TenantLimiter`. Checked in the build handlers
    // before `SubmitBuild`. Disabled limiter (default) is a no-op.
    limiter: TenantLimiter,
    // Per-tenant store-quota cache (30s TTL). Checked alongside
    // `limiter` before `SubmitBuild`. Shared via inner `Arc`.
    quota_cache: QuotaCache,
    // Max wait for `WORKER_MAGIC_1` through handshake completion.
    // Production passes `GatewayServer`'s setting (default
    // [`HANDSHAKE_TIMEOUT`]); tests shrink it.
    handshake_timeout: std::time::Duration,
    // Fired by `ChannelSession::Drop` (server.rs) when russh signals
    // `channel_close`. The opcode-read select picks this up and runs
    // the cancel loop — same outcome as the UnexpectedEof arm, but
    // reachable even when `channel_close → Drop` races ahead of the
    // mpsc-sender drop that would otherwise surface EOF here. See
    // `cancel_active_builds` docstring for the full race shape.
    shutdown: CancellationToken,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let mut ctx = SessionContext::new(
        store_client.clone(),
        log_client.clone(),
        scheduler_client.clone(),
        tenant_name,
        jwt,
        service_signer,
        limiter,
        quota_cache,
    );
    ctx.drv_blob_client = drv_blob_client;
    ctx.handshake_timeout = handshake_timeout;
    run_protocol_loop(reader, writer, &mut ctx, shutdown).await
}

/// Every way out of the protocol loop, typed (bug_319).
///
/// The inner loop returns THIS — not `anyhow::Result` — so a bare `?`
/// does not typecheck inside it: a NEW exit path must mint (or reuse)
/// a variant, and the single chokepoint in [`run_protocol_loop`] maps
/// every variant to its cancel-on-disconnect obligation through an
/// exhaustive match. The pre-fix shape enumerated cancel calls
/// per-arm and documented "ALL six exits" — while the post-opcode
/// `writer.flush().await?` was a SEVENTH, leaking active builds on a
/// response-flush failure. The 'all exits cancel' sentence is now
/// carried by the compiler, not by a comment.
enum SessionExit {
    /// Clean pre-build end: pre-handshake shutdown, handshake
    /// timeout, or a delivered version rejection. The build map is
    /// empty by construction on these paths; the chokepoint sweep
    /// still runs (defense-in-depth, no-op on an empty map).
    Clean,
    /// The handshake phase ended in an error the caller must see
    /// (protocol garbage, or the version-rejection write itself
    /// failed).
    HandshakeFailed(anyhow::Error),
    /// russh channel-close / graceful-shutdown token observed.
    ChannelClose,
    /// No opcode arrived inside the idle bound.
    IdleTimeout,
    /// Clean client EOF between opcodes.
    ClientEof,
    /// Opcode-read wire error (ConnectionReset, malformed wire, ...).
    ReadError(anyhow::Error),
    /// `handle_opcode` returned `Err` (typically `BrokenPipe` on a
    /// response write mid-build).
    HandlerError(anyhow::Error),
    /// The post-opcode response flush failed — bug_319's seventh
    /// exit, previously a bare `?` with no cancel.
    FlushError(std::io::Error),
}

/// Handshake + opcode loop. [`run_protocol`] constructs a fresh
/// [`SessionContext`] and delegates here; tests that need a pre-seeded
/// context (e.g. non-empty `active_build_ids` to exercise a cancel path)
/// build the context themselves and call this directly. Both entry points
/// run IDENTICAL code from this line on — the split exists purely so the
/// exit-path cancel contracts can be regression-tested without
/// having to drive a full build opcode into the exact state that leaves
/// a build_id in the map between opcodes.
///
/// THE cancel chokepoint (bug_319): the typed inner loop cannot
/// return without choosing a `SessionExit` variant (private enum —
/// see its definition above), and this function's exhaustive match
/// runs `cancel_active_builds` on every one of them — an exit path
/// that skips the cancel contract does not typecheck.
// r[impl gw.conn.cancel-on-disconnect+3]
pub async fn run_protocol_loop<R, W>(
    reader: &mut R,
    writer: &mut W,
    ctx: &mut SessionContext,
    shutdown: CancellationToken,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let exit = protocol_loop_inner(reader, writer, ctx, &shutdown).await;
    // merged_bug_117: attribution that depends on which select arm won
    // computes from the AUTHORITATIVE SIGNAL — the token state — ABOVE
    // the arm dispatch. `biased` selects prefer the token arm when
    // both are Ready on the same poll, but ChannelSession::Drop fires
    // cancel() then breaks the pipe — if those land between the
    // token-poll and a synchronous I/O error within ONE poll cycle,
    // the error exit carries a stale attribution. The retired per-arm
    // re-check guarded only HandlerError, and the commit that added it
    // minted the unguarded FlushError sibling in the same diff —
    // per-arm guards do not survive their own evolution. Hoisted, the
    // race window is one cell for every variant, present and future:
    // a new exit arm inherits the guard by construction.
    // r[impl gw.session.exit-attribution]
    let reason = if shutdown.is_cancelled() {
        "channel_close"
    } else {
        match &exit {
            SessionExit::ChannelClose => "channel_close",
            SessionExit::IdleTimeout => "idle_timeout",
            SessionExit::Clean
            | SessionExit::HandshakeFailed(_)
            | SessionExit::ClientEof
            | SessionExit::ReadError(_)
            | SessionExit::HandlerError(_)
            | SessionExit::FlushError(_) => "client_disconnect",
        }
    };
    // Unconditional: cheap on an empty map, and the Clean/handshake
    // variants are exactly the defense-in-depth sweeps the old
    // per-arm shape could not express.
    cancel_active_builds(ctx, reason).await;
    match exit {
        SessionExit::Clean
        | SessionExit::ChannelClose
        | SessionExit::IdleTimeout
        | SessionExit::ClientEof => Ok(()),
        SessionExit::HandshakeFailed(e)
        | SessionExit::ReadError(e)
        | SessionExit::HandlerError(e) => Err(e),
        SessionExit::FlushError(e) => Err(e.into()),
    }
}

/// The protocol loop body. Returns [`SessionExit`] — see its doc for
/// why a bare `?` out of here is unrepresentable.
async fn protocol_loop_inner<R, W>(
    reader: &mut R,
    writer: &mut W,
    ctx: &mut SessionContext,
    shutdown: &CancellationToken,
) -> SessionExit
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let version_string = format!("rio-gateway {}", env!("CARGO_PKG_VERSION"));
    // Same select-on-shutdown shape as the opcode-read below: a
    // pre-handshake stall must not outlive the channel. No
    // cancel_active_builds here — the map is empty by construction
    // until the first build opcode runs.
    let handshake_timeout = ctx.handshake_timeout;
    let handshake_result = tokio::select! {
        biased;
        () = shutdown.cancelled() => {
            debug!("channel closed before handshake (graceful shutdown signal)");
            return SessionExit::Clean;
        }
        r = tokio::time::timeout(
            handshake_timeout,
            handshake::server_handshake_split(reader, writer, &version_string),
        ) => r,
    };
    let handshake_result = match handshake_result {
        Ok(r) => r,
        Err(_elapsed) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "timeout").increment(1);
            warn!(timeout = ?handshake_timeout, "handshake timeout: no WORKER_MAGIC_1 received");
            return SessionExit::Clean;
        }
    };
    match handshake_result {
        Ok(result) => {
            ctx.negotiated_version = result.negotiated_version();
            let (major, minor) = handshake::decode_version(result.negotiated_version());
            metrics::counter!("rio_gateway_handshakes_total", "result" => "success").increment(1);
            info!(
                client_version_major = major,
                client_version_minor = minor,
                "handshake complete"
            );
        }
        // r[impl gw.handshake.version-negotiation+2]
        // handshake.rs returns VersionTooOld for client < 1.35; the session
        // layer is responsible for converting that into STDERR_ERROR on the
        // wire before closing. The annotation in handshake.rs covers the
        // version check; this one covers the STDERR_ERROR send.
        Err(handshake::HandshakeError::VersionTooOld {
            client_major,
            client_minor,
        }) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "rejected").increment(1);
            warn!(
                client_version_major = client_major,
                client_version_minor = client_minor,
                "rejecting client: protocol version too old"
            );
            let mut stderr = StderrWriter::new(&mut *writer);
            if let Err(e) = stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!("rio-gateway requires Nix protocol version 1.35+, client sent {client_major}.{client_minor}"),
                ))
                .await
            {
                // The rejection could not be delivered — the caller
                // sees the error; the build map is empty either way.
                return SessionExit::HandshakeFailed(e.into());
            }
            return SessionExit::Clean;
        }
        Err(e) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "failure").increment(1);
            warn!(error = %e, "handshake failed");
            return SessionExit::HandshakeFailed(anyhow::anyhow!("handshake failed: {e}"));
        }
    }

    loop {
        // r[impl gw.conn.cancel-on-disconnect+3]
        // Select over shutdown-signal + opcode-read. `biased` polls
        // shutdown first: when BOTH are ready on the same poll (TCP RST
        // → russh fires channel_eof AND channel_close near-simultaneously
        // → mpsc-EOF AND token-cancel both land), we prefer the
        // channel_close attribution. Correctness is identical either
        // way — both branches converge on cancel_active_builds — but the
        // reason string is more accurate: the russh-level signal is what
        // actually triggered the Drop.
        //
        // Without `biased;` tokio randomizes branch order (fairness for
        // the starvation case). We don't want fairness here; we want the
        // deterministic attribution.
        let read_result = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                debug!("channel closed (graceful shutdown signal)");
                return SessionExit::ChannelClose;
            }
            r = tokio::time::timeout(OPCODE_IDLE_TIMEOUT, wire::read_u64(reader)) => r,
        };
        let opcode = match read_result {
            Ok(Ok(op)) => op,
            Err(_elapsed) => {
                warn!(timeout = ?OPCODE_IDLE_TIMEOUT, "idle timeout waiting for next opcode");
                metrics::counter!("rio_gateway_errors_total", "type" => "idle_timeout")
                    .increment(1);
                // Best-effort: try to tell the client why. If the
                // write fails, the connection is already dead.
                let mut stderr = StderrWriter::new(&mut *writer);
                let _ = stderr
                    .error(&StderrError::simple(
                        "rio-gateway",
                        format!("idle timeout: no opcode received in {OPCODE_IDLE_TIMEOUT:?}"),
                    ))
                    .await;
                // P0444: fourth exit path — same cancel contract as the
                // other three. Today active_build_ids is empty here in
                // practice (handlers run outside the idle timer and
                // remove on completion), but this is the defense-in-depth
                // for future handler changes that leave entries between
                // opcodes. Cheap when empty (one .keys() on empty map).
                return SessionExit::IdleTimeout;
            }
            Ok(Err(wire::WireError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Between-opcode disconnect: client sent EOF while we were
                // waiting for the next opcode. In practice active_build_ids
                // is usually empty here (handlers remove on completion) —
                // this catches abnormal handler exits that left an entry.
                debug!("client disconnected (EOF)");
                return SessionExit::ClientEof;
            }
            Ok(Err(e)) => {
                // Non-EOF read error (ConnectionReset, malformed wire,
                // etc). The client is gone or hostile either way; same
                // cancel contract as the EOF arm above so
                // cancel-on-disconnect holds for ALL six session-exit
                // paths, not five.
                metrics::counter!("rio_gateway_errors_total", "type" => "wire_read").increment(1);
                error!(error = %e, "error reading opcode");
                return SessionExit::ReadError(e.into());
            }
        };

        debug!(opcode = opcode, "received opcode");

        // I-157: select on shutdown DURING handle_opcode, not just at
        // opcode-read above. The build handlers block on
        // `event_stream.message()` for the duration of a build; if the
        // token fires WITHOUT a pipe break (wait_for_session_drain
        // timeout — there is no `ChannelSession::Drop` for a half-open
        // TCP that russh hasn't noticed), nothing wakes the handler.
        // Dropping the future on token fire is safe: the only state we
        // need is `active_build_ids`, populated before the long await.
        let opcode_result = tokio::select! {
            biased;
            () = shutdown.cancelled() => {
                debug!("shutdown signal during handle_opcode");
                return SessionExit::ChannelClose;
            }
            r = handler::handle_opcode(opcode, reader, writer, ctx) => r,
        };
        if let Err(e) = opcode_result {
            // Mid-opcode disconnect: handler error is typically BrokenPipe
            // on a response write (client dropped during wopBuildDerivation
            // or wopBuildPathsWithResults). The build handler leaves the
            // build_id in active_build_ids on StreamProcessError::Wire
            // (see handler/build.rs) specifically so the chokepoint can
            // cancel. The channel_close-vs-client_disconnect attribution
            // re-check lives at the chokepoint (P0331 lineage).
            return SessionExit::HandlerError(e);
        }

        // bug_319: this flush was the SEVENTH Err-propagating exit —
        // a bare `?` past a totality comment that said "ALL six". The
        // typed exit routes it through the same chokepoint as every
        // other path; a response-flush failure now cancels the
        // client's active builds like any other disconnect.
        if let Err(e) = writer.flush().await {
            return SessionExit::FlushError(e);
        }
    }
}
