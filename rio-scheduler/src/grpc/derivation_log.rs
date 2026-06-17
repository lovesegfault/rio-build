//! `SchedulerService.GetDerivationLog` — tenant-facing stored-log reads.
//!
//! Serves a derivation execution's log for a build the caller owns:
//! `rio log` asks for any derivation it built, and `rio build`'s failure
//! replay asks for the poisoned culprit named by `BuildFailed`. The
//! actual byte-serving (ring buffer → S3 fallback, chunking) is the same
//! body `AdminService.GetDerivationLogs` uses
//! ([`crate::admin::logs::get_derivation_logs`]); this module owns what
//! the admin surface doesn't need — tenant scoping and the server-side
//! tail cursor.
//!
//! Tenancy (r\[sched.log.tenant-scoped\]): log CONTENT is only ever served
//! for an execution attributable to one of the caller's builds via
//! `build_derivations.exec_id`. A pinned execution that belongs to
//! another tenant (the typical cross-tenant culprit case) yields an
//! empty stream — the failure replay then falls back to the persisted
//! reason text. A derivation with no execution under the caller's builds
//! is NOT_FOUND, the same answer whether or not other tenants ever built
//! it.

use tokio_stream::wrappers::ReceiverStream;
use tonic::Status;
use tracing::warn;
use uuid::Uuid;

use rio_common::grpc::StatusExt;
use rio_proto::types::DerivationLogChunk;

use crate::logs::LogBuffers;

use super::SchedulerGrpc;

/// Handler body for `GetDerivationLog` (the trait method in
/// `scheduler_service.rs` is a thin wrapper around this).
// r[impl sched.log.tenant-scoped]
pub(super) async fn serve(
    grpc: &SchedulerGrpc,
    caller_tenant: Option<Uuid>,
    req: rio_proto::scheduler::GetDerivationLogRequest,
) -> Result<ReceiverStream<Result<DerivationLogChunk, Status>>, Status> {
    if req.derivation_path.is_empty() {
        return Err(Status::invalid_argument("derivation_path is required"));
    }
    let db = grpc.db.as_ref().ok_or_else(|| {
        Status::failed_precondition("GetDerivationLog requires a database connection")
    })?;
    let pinned_exec: Option<Uuid> = if req.exec_id.is_empty() {
        None
    } else {
        Some(
            req.exec_id
                .parse()
                .map_err(|e| Status::invalid_argument(format!("invalid exec_id UUID: {e}")))?,
        )
    };

    // Ownership anchor: a pinned build must exist, belong to the caller,
    // and contain the derivation. Empty build_id = no pin; resolution
    // below searches the caller's builds only.
    let mut build_recorded_exec: Option<Uuid> = None;
    let mut drv_in_callers_build = false;
    if !req.build_id.is_empty() {
        let build_id = SchedulerGrpc::parse_build_id(&req.build_id)?;
        let build_tenant = db
            .build_tenant(build_id)
            .await
            .status_internal("build lookup failed")?
            .ok_or_else(|| Status::not_found(format!("unknown build {build_id}")))?;
        if caller_tenant.is_some() && build_tenant != caller_tenant {
            return Err(Status::permission_denied(
                "build belongs to a different tenant",
            ));
        }
        match db
            .build_drv_exec(build_id, &req.derivation_path)
            .await
            .status_internal("build_derivations lookup failed")?
        {
            Some(exec) => {
                drv_in_callers_build = true;
                build_recorded_exec = exec;
            }
            None => {
                return Err(Status::not_found(format!(
                    "derivation {} is not part of build {build_id}",
                    req.derivation_path
                )));
            }
        }
    }

    // Resolve which execution may be served. Log content only ever comes
    // from an execution attributable to the caller's builds — this is
    // checked on the EXECUTION that would be served, not merely on the
    // requested build, so a culprit that last ran for another tenant
    // never leaks its log content.
    let exec = match pinned_exec {
        Some(exec) => db
            .exec_attributable(exec, caller_tenant)
            .await
            .status_internal("execution attribution lookup failed")?
            .then_some(exec),
        None => match build_recorded_exec {
            Some(exec) => Some(exec),
            None => db
                .latest_exec_for_drv(&req.derivation_path, caller_tenant)
                .await
                .status_internal("execution lookup failed")?,
        },
    };

    let Some(exec) = exec else {
        if drv_in_callers_build || pinned_exec.is_some() {
            // The derivation is legitimately the caller's to ask about,
            // but no caller-attributable execution exists (never executed
            // for the caller, or the pinned execution ran for another
            // tenant): close the stream with no content. Failure-replay
            // callers fall back to the persisted reason text.
            return Ok(empty_stream());
        }
        // Nothing about this derivation is visible to the caller — same
        // NOT_FOUND whether or not other tenants ever built it.
        return Err(Status::not_found(format!(
            "no execution of {} found among your builds",
            req.derivation_path
        )));
    };

    // Server-side tail: rebase the cursor so only the last `tail_lines`
    // lines are fetched/decoded — the client never downloads the whole
    // blob just to show a tail.
    let since = tail_since(
        db.pool(),
        &grpc.log_buffers,
        &req.derivation_path,
        exec,
        req.tail_lines,
        req.since_line,
    )
    .await;

    let inner = rio_proto::types::GetDerivationLogsRequest {
        derivation_path: req.derivation_path,
        exec_id: exec.to_string(),
        since_line: since,
    };
    Ok(
        crate::admin::logs::get_derivation_logs(&grpc.log_buffers, &grpc.s3, db.pool(), inner)
            .await,
    )
}

/// A stream that closes immediately with no chunks: the
/// "nothing to serve, and that is not an error" answer.
fn empty_stream() -> ReceiverStream<Result<DerivationLogChunk, Status>> {
    let (_tx, rx) = tokio::sync::mpsc::channel(1);
    ReceiverStream::new(rx)
}

/// Compute the effective `since_line` for a tail request: the cursor
/// that leaves exactly the last `tail_lines` lines of the execution's
/// log (or the caller's own `since_line`, whichever is further along).
/// `tail_lines == 0` = full log.
///
/// The stored `drv_logs` row is consulted first (final or periodic
/// snapshot); the live ring-buffer span covers deployments without S3
/// where the execution's lines only exist in memory. Lookup failures
/// degrade to the caller's cursor — a full log is a worse answer than a
/// tail, but better than an error.
async fn tail_since(
    pool: &sqlx::PgPool,
    log_buffers: &LogBuffers,
    drv_path: &str,
    exec: Uuid,
    tail_lines: u32,
    since_line: u64,
) -> u64 {
    if tail_lines == 0 {
        return since_line;
    }
    let tail = u64::from(tail_lines);
    let stored: Option<(i64, i64)> =
        match sqlx::query_as("SELECT first_line, line_count FROM drv_logs WHERE exec_id = $1")
            .bind(exec)
            .fetch_optional(pool)
            .await
        {
            Ok(row) => row,
            Err(e) => {
                warn!(exec_id = %exec, error = %e,
                  "drv_logs span lookup failed; serving from the caller's cursor");
                None
            }
        };
    if let Some((first_line, line_count)) = stored {
        let end = first_line.max(0) as u64 + line_count.max(0) as u64;
        return since_line.max(end.saturating_sub(tail));
    }
    if let Some((_first, last, count)) = log_buffers.span(drv_path, exec)
        && count > 0
    {
        return since_line.max((last + 1).saturating_sub(tail));
    }
    since_line
}
