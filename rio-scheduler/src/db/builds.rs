//! Build CRUD + status transitions — `builds` table.
//!
//! Also hosts the `list_builds` / `list_builds_keyset` admin-facing read
//! queries (single-table since I-103). The shared SELECT clause lives in
//! [`super::list_builds_select!`].

use sqlx::PgConnection;
use uuid::Uuid;

use super::{
    BuildListRow, FencedBegin, FencedOutcome, SchedulerDb, ServingGeneration, list_builds_select,
};
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
    pub(crate) async fn list_builds(
        &self,
        status_opt: Option<&str>,
        tenant_filter: Option<Uuid>,
        limit: i64,
        offset: i64,
    ) -> Result<(i64, Vec<BuildListRow>), sqlx::Error> {
        let total = self.count_builds(status_opt, tenant_filter).await?;
        let rows: Vec<BuildListRow> = sqlx::query_as(list_builds_select!(
            "WHERE ($1::text IS NULL OR b.status = $1)
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
    pub(crate) async fn list_builds_keyset(
        &self,
        status_opt: Option<&str>,
        tenant_filter: Option<Uuid>,
        limit: i64,
        cursor_micros: i64,
        cursor_id: Uuid,
    ) -> Result<Vec<BuildListRow>, sqlx::Error> {
        sqlx::query_as(list_builds_select!(
            "WHERE ($1::text IS NULL OR b.status = $1)
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
    ///
    /// One UNNEST UPDATE for N builds (sh-007c S5 — replaces the
    /// per-build singular). Same best-effort, unfenced posture as the
    /// retired singular — these are display-only denormalized counts,
    /// recovery
    /// re-runs the in-mem accounting for active builds, so a missed
    /// write self-heals on failover. The actor's per-build-tail loops
    /// (`complete_ready_from_store_batch` / `release_downstream`)
    /// collect `(build_id, total, completed, cached)` tuples and issue
    /// ONE call instead of N serial RTTs.
    pub(crate) async fn persist_build_counts_batch(
        &self,
        rows: &[(Uuid, u32, u32, u32, u32)],
    ) -> Result<(), sqlx::Error> {
        if rows.is_empty() {
            return Ok(());
        }
        let ids: Vec<Uuid> = rows.iter().map(|(id, ..)| *id).collect();
        // u32→i32: column is INTEGER (migration 030); same explicit
        // wrap-clamp as the singular.
        let totals: Vec<i32> = rows
            .iter()
            .map(|(_, t, ..)| i32::try_from(*t).unwrap_or(i32::MAX))
            .collect();
        let completeds: Vec<i32> = rows
            .iter()
            .map(|(_, _, c, ..)| i32::try_from(*c).unwrap_or(i32::MAX))
            .collect();
        let cacheds: Vec<i32> = rows
            .iter()
            .map(|(_, _, _, c, _)| i32::try_from(*c).unwrap_or(i32::MAX))
            .collect();
        let builts: Vec<i32> = rows
            .iter()
            .map(|(.., b)| i32::try_from(*b).unwrap_or(i32::MAX))
            .collect();
        sqlx::query(
            "UPDATE builds SET total_drvs = u.t, completed_drvs = u.c, \
                               cached_drvs = u.h, built_drvs = u.b \
               FROM UNNEST($1::uuid[], $2::int[], $3::int[], $4::int[], $5::int[]) \
                 AS u(id, t, c, h, b) \
              WHERE builds.build_id = u.id",
        )
        .bind(&ids)
        .bind(&totals)
        .bind(&completeds)
        .bind(&cacheds)
        .bind(&builts)
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
    pub(crate) async fn insert_build(
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
                (build_id, tenant_id, status, priority_class,
                 keep_going, options_json, jwt_jti)
            VALUES ($1, $2, 'pending', $3, $4, $5, $6)
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
    /// One FENCED transaction: the build row AND its wanted rows
    /// (D1/A6 merged_bug_163 composed with A1 fenced-write discipline)
    /// — the failed-merge rollback can no longer leave
    /// build_wanted_outputs orphans behind, and a deposed replica's
    /// late rollback cannot destroy a successor's rows.
    // r[impl sched.db.table-retention+1]
    pub(crate) async fn delete_build(
        &self,
        build_id: Uuid,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let n = sqlx::query("DELETE FROM build_wanted_outputs WHERE build_id = $1")
            .bind(build_id)
            .execute(tx.conn())
            .await?
            .rows_affected();
        sqlx::query!("DELETE FROM builds WHERE build_id = $1", build_id)
            .execute(tx.conn())
            .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(n))
    }

    // r[impl sched.evidence.durability+4]
    /// Flip a build to Active inside an existing transaction. The merge
    /// path runs this as the LAST statement of `persist_merge_to_db`'s
    /// transaction so a committed merge implies an Active build: an
    /// activation failure aborts the whole merge — including the
    /// pruned-origin job rows and the build_derivations links — instead
    /// of leaving committed side effects behind for a build the caller
    /// is about to reject and roll back in memory. Mirrors the
    /// `BuildState::Active` arm of [`Self::update_build_status`]; all
    /// other (non-merge) status transitions keep going through that
    /// pool-level method via `transition_build`.
    ///
    /// Errors with [`sqlx::Error::RowNotFound`] if the UPDATE touches
    /// anything other than exactly one row (i.e. the build row is
    /// missing), so the caller's transaction aborts instead of
    /// committing a merge whose build was never activated. Unreachable
    /// through the single-threaded actor path today — the same command
    /// inserted the row — but kept as a cheap guard against a silent
    /// half-done commit.
    pub(crate) async fn activate_build_tx(
        tx: &mut PgConnection,
        build_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        let result = sqlx::query(
            "UPDATE builds SET status = 'active', started_at = now() WHERE build_id = $1",
        )
        .bind(build_id)
        .execute(&mut *tx)
        .await?;
        // != 1 rather than == 0: >1 is impossible (build_id is the PK),
        // but anything other than exactly one activated row means the
        // merge must not commit.
        if result.rows_affected() != 1 {
            return Err(sqlx::Error::RowNotFound);
        }
        Ok(())
    }

    /// Update a build's status. Terminal statuses persist the settled
    /// payload's accounting in the same UPDATE: the counts are FINAL at
    /// the terminal transition (`update_build_counts_with` no-ops on
    /// settled builds, so nothing can shrink them afterwards —
    /// merged_bug_097's PG leg).
    pub(crate) async fn update_build_status(
        &self,
        build_id: Uuid,
        status: BuildState,
        settled: Option<&crate::state::SettledBuild>,
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
            // Terminal: persist the WHOLE settled payload in ONE
            // UPDATE with the status flip — counts, outcome arm, and
            // finished_at land atomically, so the row a post-cleanup /
            // post-failover WatchBuild reads (migration 087,
            // sched.watch.terminal-from-durable-row) is never a
            // half-written verdict.
            use crate::state::TerminalOutcome;
            let (error_summary, failed_derivation, failure_status, cancel_reason, output_paths) =
                match settled.map(|s| &s.outcome) {
                    Some(TerminalOutcome::Failed(ff)) => (
                        Some(ff.summary.as_str()),
                        ff.failed_drv.as_deref(),
                        ff.status.map(|st| st.as_str_name()),
                        None,
                        None,
                    ),
                    Some(TerminalOutcome::Cancelled { reason }) => {
                        (None, None, None, Some(reason.as_str()), None)
                    }
                    Some(TerminalOutcome::Succeeded { output_paths }) => {
                        (None, None, None, None, Some(output_paths.as_slice()))
                    }
                    None => (None, None, None, None, None),
                };
            let completed = settled.map(|s| s.counts.completed as i32);
            let cached = settled.map(|s| s.counts.cached as i32);
            let built = settled.map(|s| s.counts.built as i32);
            let failed = settled.map(|s| s.counts.failed as i32);
            sqlx::query(
                "UPDATE builds SET status = $2, finished_at = now(), error_summary = $3, \
                 completed_drvs = COALESCE($4, completed_drvs), \
                 cached_drvs = COALESCE($5, cached_drvs), \
                 built_drvs = COALESCE($6, built_drvs), \
                 failed_drvs = $7, \
                 failed_derivation = $8, failure_status = $9, \
                 cancel_reason = $10, output_paths = $11 \
                 WHERE build_id = $1",
            )
            .bind(build_id)
            .bind(status.as_str())
            .bind(error_summary)
            .bind(completed)
            .bind(cached)
            .bind(built)
            .bind(failed)
            .bind(failed_derivation)
            .bind(failure_status)
            .bind(cancel_reason)
            .bind(output_paths)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    /// Fetch the terminal row of a build the actor no longer holds
    /// (post-cleanup or post-failover). `None` when the row is missing
    /// OR not terminal — callers fall back to `NotFound` then.
    ///
    /// Tenant-bound (bug_213): the caller's [`CallerTenant`](crate::grpc::CallerTenant) witness is
    /// part of the query — a foreign tenant's row is ABSENT, so the
    /// caller takes the same `NotFound` as for a build that never
    /// existed (the resident-phase arm keeps `PermissionDenied`: the
    /// spec-pinned status asymmetry). Dev mode (`tenant() == None`)
    /// binds NULL and matches every row.
    // r[impl sched.watch.terminal-from-durable-row+2]
    // r[impl sched.tenant.authz+3]
    pub(crate) async fn get_build_terminal_row(
        &self,
        build_id: Uuid,
        caller: &crate::grpc::CallerTenant,
    ) -> Result<Option<BuildTerminalRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT status, error_summary, failed_derivation, failure_status, \
                    cancel_reason, output_paths, \
                    total_drvs, completed_drvs, cached_drvs, built_drvs, failed_drvs \
             FROM builds \
             WHERE build_id = $1 AND status IN ('succeeded','failed','cancelled') \
               AND ($2::uuid IS NULL OR tenant_id = $2)",
        )
        .bind(build_id)
        .bind(caller.tenant())
        .fetch_optional(&self.pool)
        .await
    }
}

/// One terminal `builds` row — the durable settled payload
/// (migration 087) a late `WatchBuild` synthesizes its snapshot from.
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct BuildTerminalRow {
    pub status: String,
    pub error_summary: Option<String>,
    pub failed_derivation: Option<String>,
    pub failure_status: Option<String>,
    pub cancel_reason: Option<String>,
    pub output_paths: Option<Vec<String>>,
    pub total_drvs: Option<i32>,
    pub completed_drvs: Option<i32>,
    pub cached_drvs: Option<i32>,
    pub built_drvs: Option<i32>,
    pub failed_drvs: Option<i32>,
}

impl SchedulerDb {
    // ── GetDerivationLog resolution queries ─────────────────────────────
    //
    // Tenant-facing log access (r[sched.log.tenant-scoped]): every query
    // below either anchors on a build the caller owns or filters by the
    // caller's tenant, so log content can never be resolved through
    // another tenant's builds. Runtime-checked queries (read path, same
    // style as `list_builds`).

    /// `builds.tenant_id` for `build_id`. Outer `None` = no such build;
    /// inner `None` = single-tenant/dev build with no tenant recorded.
    pub(crate) async fn build_tenant(
        &self,
        build_id: Uuid,
    ) -> Result<Option<Option<Uuid>>, sqlx::Error> {
        sqlx::query_scalar("SELECT tenant_id FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_optional(&self.pool)
            .await
    }

    /// The execution recorded for (`build_id`, `drv_path`) on
    /// `build_derivations`. Outer `None` = the derivation is not part of
    /// that build; inner `None` = part of the build but no execution was
    /// observed for it (cache hit, never dispatched, fail-fast).
    pub(crate) async fn build_drv_exec(
        &self,
        build_id: Uuid,
        drv_path: &str,
    ) -> Result<Option<Option<Uuid>>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT bd.exec_id FROM build_derivations bd \
             JOIN derivations d ON d.derivation_id = bd.derivation_id \
             WHERE bd.build_id = $1 AND d.drv_path = $2",
        )
        .bind(build_id)
        .bind(drv_path)
        .fetch_optional(&self.pool)
        .await
    }

    /// Latest execution of `drv_path` among builds owned by `tenant`
    /// (`None` tenant = single-tenant mode, no filter). UUIDv7 ordering
    /// = newest dispatch. Uses `derivations_drv_path_idx` (M_115).
    pub(crate) async fn latest_exec_for_drv(
        &self,
        drv_path: &str,
        tenant: Option<Uuid>,
    ) -> Result<Option<Uuid>, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT bd.exec_id FROM build_derivations bd \
             JOIN builds b ON b.build_id = bd.build_id \
             JOIN derivations d ON d.derivation_id = bd.derivation_id \
             WHERE d.drv_path = $1 AND bd.exec_id IS NOT NULL \
               AND ($2::uuid IS NULL OR b.tenant_id = $2) \
             ORDER BY bd.exec_id DESC LIMIT 1",
        )
        .bind(drv_path)
        .bind(tenant)
        .fetch_optional(&self.pool)
        .await
    }

    /// Whether `exec_id` is attributable to a build owned by `tenant`
    /// (`None` tenant = single-tenant mode, always attributable if the
    /// execution is recorded at all). Uses `build_derivations_exec_idx`
    /// (M_118).
    pub(crate) async fn exec_attributable(
        &self,
        exec_id: Uuid,
        tenant: Option<Uuid>,
    ) -> Result<bool, sqlx::Error> {
        sqlx::query_scalar(
            "SELECT EXISTS( \
                 SELECT 1 FROM build_derivations bd \
                 JOIN builds b ON b.build_id = bd.build_id \
                 WHERE bd.exec_id = $1 \
                   AND ($2::uuid IS NULL OR b.tenant_id = $2))",
        )
        .bind(exec_id)
        .bind(tenant)
        .fetch_one(&self.pool)
        .await
    }
}
