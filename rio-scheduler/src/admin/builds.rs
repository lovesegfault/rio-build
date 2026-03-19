//! `AdminService.ListBuilds` implementation.

use rio_proto::types::{BuildInfo, BuildState, ListBuildsResponse};
use tonic::Status;
use uuid::Uuid;

use crate::db::{BuildListRow, SchedulerDb};

/// Query PG for builds with optional filters + offset pagination.
///
/// `cached_derivations` heuristic: "completed with no assignment row" —
/// a cache-hit derivation transitions directly to Completed at merge
/// time without ever being dispatched. See `actor/merge.rs` for the
/// in-memory `cached_count` tracking; this is the DB-side reconstruction
/// for historical queries.
///
/// NOTE: `phase4a.md:40` specifies cursor-by-`submitted_at` pagination,
/// but proto `ListBuildsRequest.offset` (field 3) already exists. Using
/// offset here to avoid a proto change in 4a; offset instability under
/// concurrent inserts (newly submitted builds shift pages) is acceptable
/// for an admin dashboard where operators usually filter by status first.
/// TODO(P0271): add `optional string cursor` to proto + keyset query
/// here if offset instability becomes operationally relevant.
// r[impl sched.admin.list-builds]
pub(super) async fn list_builds(
    db: &SchedulerDb,
    status_filter: &str,
    tenant_filter: Option<Uuid>,
    limit: u32,
    offset: u32,
) -> Result<ListBuildsResponse, Status> {
    let status_opt = (!status_filter.is_empty()).then_some(status_filter);
    let limit = limit.clamp(1, 1000) as i64;
    let offset = offset as i64;

    let (total, rows) = db
        .list_builds(status_opt, tenant_filter, limit, offset)
        .await
        .map_err(|e| Status::internal(format!("list_builds: {e}")))?;

    Ok(ListBuildsResponse {
        builds: rows.into_iter().map(row_to_proto).collect(),
        total_count: total as u32,
    })
}

fn row_to_proto(r: BuildListRow) -> BuildInfo {
    let state = r
        .status
        .parse::<crate::state::BuildState>()
        .map(BuildState::from)
        .unwrap_or_else(|_| {
            tracing::warn!(status = %r.status, build_id = %r.build_id,
                "unknown build status from PG — rendering as Pending");
            BuildState::Pending
        }) as i32;
    BuildInfo {
        build_id: r.build_id,
        tenant_id: r.tenant_id.unwrap_or_default(),
        priority_class: r.priority_class,
        state,
        total_derivations: r.total_derivations as u32,
        completed_derivations: r.completed_derivations as u32,
        cached_derivations: r.cached_derivations as u32,
        error_summary: r.error_summary.unwrap_or_default(),
        // Timestamps not populated in 4a — PG ::text format isn't RFC3339
        // and we don't have chrono. Same deferral as TenantInfo.created_at.
        // Dashboard can query PG directly if needed; 4b adds sqlx chrono
        // feature or extract(epoch) cast.
        submitted_at: None,
        started_at: None,
        finished_at: None,
    }
}
