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

use crate::handler::{self, SessionContext};
use crate::quota::QuotaCache;
use crate::ratelimit::TenantLimiter;

/// Best-effort cancel of all builds tracked in `active_build_ids`.
///
/// Called from three session-exit paths, all converging here so
/// `r[gw.conn.cancel-on-disconnect]` holds regardless of which wins the
/// race on TCP RST:
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
///    cleanup runs, build leaks until `r[sched.backstop.timeout]`.
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
    let build_ids: Vec<String> = ctx.active_build_ids.keys().cloned().collect();
    for build_id in build_ids {
        debug!(build_id = %build_id, reason = %reason, "cancelling build on disconnect");
        let req = types::CancelBuildRequest {
            build_id: build_id.clone(),
            reason: reason.to_string(),
        };
        // Best-effort cancel: wrap in timeout so an unreachable
        // scheduler doesn't block the disconnect cleanup loop.
        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            ctx.scheduler_client.cancel_build(req),
        )
        .await
        {
            Ok(Ok(_)) => {
                debug!(build_id = %build_id, "cancelled build on disconnect");
            }
            Ok(Err(e)) => {
                warn!(build_id = %build_id, error = %e, "failed to cancel build on disconnect");
            }
            Err(_) => {
                warn!(build_id = %build_id, "cancel_build timed out on disconnect");
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

// r[impl gw.conn.per-channel-state]
// r[impl gw.conn.sequential]
// r[impl gw.conn.lifecycle]
// r[impl gw.handshake.phases]
// r[impl gw.compat.version-range]
/// Runs the Nix worker protocol on separate read/write streams,
/// delegating store operations to `StoreServiceClient` and build
/// operations to `SchedulerServiceClient`.
// 8 args is one over clippy's default of 7. The alternatives
// (grouping into a struct, or building SessionContext at the call
// site) both add more noise than the extra arg costs. The session
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
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    tenant_name: Option<NormalizedName>,
    jwt_token: Option<String>,
    // Per-tenant rate limiter, shared across all sessions via
    // `Arc`-inside-`TenantLimiter`. Checked in the build handlers
    // before `SubmitBuild`. Disabled limiter (default) is a no-op.
    limiter: TenantLimiter,
    // Per-tenant store-quota cache (30s TTL). Checked alongside
    // `limiter` before `SubmitBuild`. Shared via inner `Arc`.
    quota_cache: QuotaCache,
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
        scheduler_client.clone(),
        tenant_name,
        jwt_token,
        limiter,
        quota_cache,
    );

    let version_string = format!("rio-gateway {}", env!("CARGO_PKG_VERSION"));
    match handshake::server_handshake_split(reader, writer, &version_string).await {
        Ok(result) => {
            let (major, minor) = handshake::decode_version(result.negotiated_version());
            metrics::counter!("rio_gateway_handshakes_total", "result" => "success").increment(1);
            info!(
                client_version_major = major,
                client_version_minor = minor,
                "handshake complete"
            );
        }
        // r[impl gw.handshake.version-negotiation]
        // handshake.rs returns VersionTooOld for client < 1.37; the session
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
            stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!("rio-gateway requires Nix protocol version 1.37+, client sent {client_major}.{client_minor}"),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            metrics::counter!("rio_gateway_handshakes_total", "result" => "failure").increment(1);
            warn!(error = %e, "handshake failed");
            return Err(anyhow::anyhow!("handshake failed: {e}"));
        }
    }

    loop {
        // r[impl gw.conn.cancel-on-disconnect]
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
                cancel_active_builds(&mut ctx, "channel_close").await;
                return Ok(());
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
                return Ok(());
            }
            Ok(Err(wire::WireError::Io(e))) if e.kind() == std::io::ErrorKind::UnexpectedEof => {
                // Between-opcode disconnect: client sent EOF while we were
                // waiting for the next opcode. In practice active_build_ids
                // is usually empty here (handlers remove on completion) —
                // this catches abnormal handler exits that left an entry.
                debug!("client disconnected (EOF)");
                cancel_active_builds(&mut ctx, "client_disconnect").await;
                return Ok(());
            }
            Ok(Err(e)) => {
                metrics::counter!("rio_gateway_errors_total", "type" => "wire_read").increment(1);
                error!(error = %e, "error reading opcode");
                return Err(e.into());
            }
        };

        debug!(opcode = opcode, "received opcode");

        if let Err(e) = handler::handle_opcode(opcode, reader, writer, &mut ctx).await {
            // Mid-opcode disconnect: handler error is typically BrokenPipe
            // on a response write (client dropped during wopBuildDerivation
            // or wopBuildPathsWithResults). The build handler leaves the
            // build_id in active_build_ids on StreamProcessError::Wire
            // (see handler/build.rs) specifically so we can cancel here.
            //
            // Without this, the ? propagated straight out and the EOF arm
            // above was never reached — it only catches opcode-READ errors,
            // not handler-execution errors. P0331.
            cancel_active_builds(&mut ctx, "client_disconnect").await;
            return Err(e);
        }

        writer.flush().await?;
    }
}
