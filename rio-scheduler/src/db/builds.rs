//! Build CRUD + status transitions — `builds` table.
//!
//! Also hosts the `list_builds` / `list_builds_keyset` admin-facing read
//! queries (single-table since I-103). The shared SELECT clause lives in
//! [`super::LIST_BUILDS_SELECT`].

use uuid::Uuid;

use super::{BuildListRow, LIST_BUILDS_SELECT, SchedulerDb};
use crate::state::{BuildState, BuildStateExt};

impl SchedulerDb {
    /// List builds with optional status/tenant filters + offset pagination
    /// (for AdminService.ListBuilds). Returns `(total_count, page_rows)`.
    ///
    /// `status_opt`: `None` = no filter, `Some(s)` = `b.status = s`.
    /// `limit` is taken as-is — caller clamps.
    ///
    /// Offset pagination is unstable under concurrent inserts (newly
    /// submitted builds shift later pages). Kept for dashboard backward
    /// compat; new callers should prefer
    /// [`list_builds_keyset`](Self::list_builds_keyset).
    pub async fn list_builds(
        &self,
        status_opt: Option<&str>,
        tenant_filter: Option<Uuid>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<BuildListRow>), sqlx::Error> {
        let total = self.count_builds(status_opt, tenant_filter).await?;
        let rows: Vec<BuildListRow> = sqlx::query_as(&format!(
            "{LIST_BUILDS_SELECT}
            WHERE ($1::text IS NULL OR b.status = $1)
              AND ($2::uuid IS NULL OR b.tenant_id = $2)
            ORDER BY b.submitted_at DESC, b.build_id DESC
            LIMIT $3 OFFSET $4"
        ))
        .bind(status_opt)
        .bind(tenant_filter)
        .bind(limit)
        .bind(offset)
        .fetch_all(&self.pool)
        .await?;

        Ok((total, rows))
    }

    /// Keyset-paginated variant of [`list_builds`](Self::list_builds).
    /// Stable under
    /// concurrent inserts: the cursor is `(submitted_at_micros, build_id)`
    /// — a compound key that's monotone-decreasing through pages. A row
    /// inserted between page N and N+1 never shifts already-seen rows (it
    /// sorts before the cursor, so it's simply not visible to this walk).
    ///
    /// `cursor_micros`/`cursor_id`: strictly-less-than bound. Pass
    /// `(i64::MAX, Uuid::max())` for the first page (unbounded).
    ///
    /// Row-value comparison `(a, b) < (x, y)` is SQL-standard
    /// lexicographic: `a < x OR (a = x AND b < y)`. Uses
    /// `builds_keyset_idx` (migration 022) — composite DESC-DESC matches
    /// this query's ORDER BY, so the planner does an index-scan + no Sort.
    ///
    /// Cursor-timestamp reconstruction: `to_timestamp($3/1e6)` alone would
    /// pass through `to_timestamp(double precision)`, coercing a ~16-digit
    /// value (~1.74×10⁹ seconds with 6-decimal microsecond fraction) to
    /// float8 — right at the IEEE754 limit, so a page-boundary row could
    /// lose a microsecond and be skipped or duplicated. Instead, split
    /// into integer seconds (bigint÷1000000, exact in float8 — seconds
    /// are ~10⁹, way under 2⁵³) plus an integer-microsecond interval
    /// (`modulo × interval '1 microsecond'`). Both halves are exact; the
    /// sum is a TIMESTAMPTZ with the same microsecond as the source.
    ///
    /// Returns `Vec<BuildListRow>` WITHOUT a total count. `count_builds`
    /// is an O(n) seq-scan; calling it per page defeats the O(limit)-per-
    /// page guarantee. The first page comes through
    /// [`list_builds`](Self::list_builds) (offset mode), which does compute
    /// total; subsequent pages carry it client-side. If a caller needs a
    /// total on a cursor-only walk, they can call `list_builds(limit=0)`
    /// once or use `count_builds` directly.
    pub async fn list_builds_keyset(
        &self,
        status_opt: Option<&str>,
        tenant_filter: Option<Uuid>,
        limit: i64,
        cursor_micros: i64,
        cursor_id: Uuid,
    ) -> Result<Vec<BuildListRow>, sqlx::Error> {
        sqlx::query_as(&format!(
            "{LIST_BUILDS_SELECT}
            WHERE ($1::text IS NULL OR b.status = $1)
              AND ($2::uuid IS NULL OR b.tenant_id = $2)
              AND (b.submitted_at, b.build_id)
                  < ( to_timestamp($3::bigint / 1000000)
                      + ($3::bigint % 1000000) * interval '1 microsecond',
                      $4::uuid )
            ORDER BY b.submitted_at DESC, b.build_id DESC
            LIMIT $5"
        ))
        .bind(status_opt)
        .bind(tenant_filter)
        .bind(cursor_micros)
        .bind(cursor_id)
        .bind(limit)
        .fetch_all(&self.pool)
        .await
    }

    /// Persist denormalized derivation counts to `builds` (I-103).
    /// Best-effort write — caller logs and continues on error.
    pub async fn persist_build_counts(
        &self,
        build_id: Uuid,
        total: u32,
        completed: u32,
        cached: u32,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE builds SET total_drvs = $2, completed_drvs = $3, cached_drvs = $4
             WHERE build_id = $1",
        )
        .bind(build_id)
        .bind(total as i32)
        .bind(completed as i32)
        .bind(cached as i32)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    async fn count_builds(
        &self,
        status_opt: Option<&str>,
        tenant_filter: Option<Uuid>,
    ) -> Result<i64, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT COUNT(*) FROM builds b
             WHERE ($1::text IS NULL OR b.status = $1)
               AND ($2::uuid IS NULL OR b.tenant_id = $2)",
        )
        .bind(status_opt)
        .bind(tenant_filter)
        .fetch_one(&self.pool)
        .await
    }

    /// Insert a new build record.
    ///
    /// `keep_going` + `options` are for Phase 3b state recovery —
    /// `recover_from_pg()` reads them back to rebuild BuildInfo.
    /// `options` is serialized to JSONB (`sqlx::types::Json`
    /// wrapper handles the serde round-trip).
    pub async fn insert_build(
        &self,
        build_id: Uuid,
        tenant_id: Option<Uuid>,
        priority_class: crate::state::PriorityClass,
        keep_going: bool,
        options: &crate::state::BuildOptions,
        // r[impl gw.jwt.issue]
        // JWT ID for audit trail — migration 016 added builds.jwt_jti
        // but nothing wrote to it until this param (T77). NULL when
        // Claims absent (dual-mode fallback — gateway may run jwt-off).
        jti: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO builds
                (build_id, tenant_id, requestor, status, priority_class,
                 keep_going, options_json, jwt_jti)
            VALUES ($1, $2, '', 'pending', $3, $4, $5, $6)
            "#,
        )
        .bind(build_id)
        .bind(tenant_id)
        .bind(priority_class.as_str())
        .bind(keep_going)
        // Json<&T>: sqlx serializes via serde_json and binds as
        // JSONB. BuildOptions derives Serialize (add if missing).
        .bind(sqlx::types::Json(options))
        .bind(jti)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete a build row (best-effort cleanup after a failed merge).
    /// Cascade-deletes build_derivations links (migration 008 added
    /// ON DELETE CASCADE). Used by handle_merge_dag rollback to clean up
    /// the orphan build row if DB persistence fails after insert_build
    /// succeeded.
    ///
    /// In practice, cleanup_failed_merge calls this BEFORE
    /// persist_merge_to_db has run, so there are typically no
    /// build_derivations rows to cascade. The CASCADE is defense-in-depth
    /// for the persist-failed path (where rows exist but the tx rolled
    /// back, so they don't) and for manual admin cleanup.
    pub async fn delete_build(&self, build_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query!("DELETE FROM builds WHERE build_id = $1", build_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update a build's status.
    pub async fn update_build_status(
        &self,
        build_id: Uuid,
        status: BuildState,
        error_summary: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now_col = match status {
            BuildState::Active => "started_at",
            BuildState::Succeeded | BuildState::Failed | BuildState::Cancelled => "finished_at",
            BuildState::Pending | BuildState::Unspecified => "",
        };

        if now_col.is_empty() {
            sqlx::query("UPDATE builds SET status = $2 WHERE build_id = $1")
                .bind(build_id)
                .bind(status.as_str())
                .execute(&self.pool)
                .await?;
        } else if now_col == "started_at" {
            sqlx::query("UPDATE builds SET status = $2, started_at = now() WHERE build_id = $1")
                .bind(build_id)
                .bind(status.as_str())
                .execute(&self.pool)
                .await?;
        } else {
            sqlx::query(
                "UPDATE builds SET status = $2, finished_at = now(), error_summary = $3 WHERE build_id = $1",
            )
            .bind(build_id)
            .bind(status.as_str())
            .bind(error_summary)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }
}
