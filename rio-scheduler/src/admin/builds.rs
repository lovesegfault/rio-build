//! `AdminService.ListBuilds` implementation.

use rio_proto::types::{BuildInfo, BuildState, ListBuildsResponse};
use sqlx::PgPool;
use tonic::Status;
use uuid::Uuid;

/// Query PG for builds with optional filters + offset pagination.
///
/// `cached_derivations` heuristic: "completed with no assignment row" —
/// a cache-hit derivation transitions directly to Completed at merge
/// time without ever being dispatched. See `actor/merge.rs:144,156,277`
/// for the in-memory `cached_count` tracking; this is the DB-side
/// reconstruction for historical queries.
///
/// NOTE: `phase4a.md:40` specifies cursor-by-`submitted_at` pagination,
/// but proto `ListBuildsRequest.offset` (field 3) already exists. Using
/// offset here to avoid a proto change in 4a; offset instability under
/// concurrent inserts (newly submitted builds shift pages) is acceptable
/// for an admin dashboard where operators usually filter by status first.
/// TODO(phase4c): add `optional string cursor` to proto + keyset query
/// here if offset instability becomes operationally relevant.
// r[impl sched.admin.list-builds]
pub(super) async fn list_builds(
    pool: &PgPool,
    status_filter: &str,
    tenant_filter: Option<Uuid>,
    limit: u32,
    offset: u32,
) -> Result<ListBuildsResponse, Status> {
    let status_opt = (!status_filter.is_empty()).then_some(status_filter);
    let limit = limit.clamp(1, 1000) as i64;
    let offset = offset as i64;

    // Total count (for pagination UI).
    let total: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM builds b
         WHERE ($1::text IS NULL OR b.status = $1)
           AND ($2::uuid IS NULL OR b.tenant_id = $2)",
    )
    .bind(status_opt)
    .bind(tenant_filter)
    .fetch_one(pool)
    .await
    .map_err(|e| Status::internal(format!("list_builds count: {e}")))?;

    // Page of builds with derivation counts via LEFT JOINs.
    // Timestamps ::text for consistency with the rest of the codebase
    // (no chrono dep). BuildInfo.submitted_at etc. will be None in
    // proto — TODO(phase4b) same as TenantInfo.created_at.
    let rows: Vec<BuildListRow> = sqlx::query_as(
        r#"
        SELECT
            b.build_id::text,
            b.tenant_id::text,
            b.priority_class,
            b.status,
            b.error_summary,
            COALESCE(COUNT(bd.derivation_id), 0)::bigint AS total_derivations,
            COALESCE(COUNT(*) FILTER (WHERE d.status = 'completed'), 0)::bigint
                AS completed_derivations,
            COALESCE(COUNT(*) FILTER (
                WHERE d.status = 'completed'
                  AND NOT EXISTS (SELECT 1 FROM assignments a
                                  WHERE a.derivation_id = d.derivation_id)
            ), 0)::bigint AS cached_derivations
        FROM builds b
        LEFT JOIN build_derivations bd USING (build_id)
        LEFT JOIN derivations d ON bd.derivation_id = d.derivation_id
        WHERE ($1::text IS NULL OR b.status = $1)
          AND ($2::uuid IS NULL OR b.tenant_id = $2)
        GROUP BY b.build_id
        ORDER BY b.submitted_at DESC, b.build_id DESC
        LIMIT $3 OFFSET $4
        "#,
    )
    .bind(status_opt)
    .bind(tenant_filter)
    .bind(limit)
    .bind(offset)
    .fetch_all(pool)
    .await
    .map_err(|e| Status::internal(format!("list_builds: {e}")))?;

    Ok(ListBuildsResponse {
        builds: rows.into_iter().map(row_to_proto).collect(),
        total_count: total as u32,
    })
}

#[derive(Debug, sqlx::FromRow)]
struct BuildListRow {
    build_id: String,
    tenant_id: Option<String>,
    priority_class: String,
    status: String,
    error_summary: Option<String>,
    total_derivations: i64,
    completed_derivations: i64,
    cached_derivations: i64,
}

fn row_to_proto(r: BuildListRow) -> BuildInfo {
    BuildInfo {
        build_id: r.build_id,
        tenant_id: r.tenant_id.unwrap_or_default(),
        priority_class: r.priority_class,
        state: build_state_from_str(&r.status) as i32,
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

fn build_state_from_str(s: &str) -> BuildState {
    match s {
        "pending" => BuildState::Pending,
        "active" => BuildState::Active,
        "succeeded" => BuildState::Succeeded,
        "failed" => BuildState::Failed,
        "cancelled" => BuildState::Cancelled,
        _ => BuildState::Pending,
    }
}
