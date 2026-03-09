//! PostgreSQL persistence for scheduler state.
//!
//! Synchronous writes: state transitions, assignment changes, build terminal status.
//! Async/batched: build_history EMA updates.
//!
//! UUIDs are bound natively via the sqlx `uuid` feature — no `::uuid` casts or
//! `.to_string()` conversions needed.

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool, QueryBuilder};
use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus, DrvHash, WorkerId};

/// Terminal derivation statuses (wire strings). Single source of
/// truth for SQL filters in both `sweep_stale_live_pins` (delete
/// pins whose drv is terminal) and `load_nonterminal_derivations`
/// (exclude terminal drvs from recovery). A new terminal status
/// added to [`DerivationStatus::is_terminal`] must also be added
/// here or recovery/GC will diverge.
///
/// Bound as `status <> ALL($1::text[])` — cleaner than a hardcoded
/// `NOT IN (...)` literal and gets the planner the same predicate.
const TERMINAL_STATUSES: &[&str] = &["completed", "poisoned", "dependency_failed", "cancelled"];

/// One row from `build_history`. Fields in SELECT order:
/// `(pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores)`.
///
/// Type alias not struct: `sqlx::query_as` maps to tuples by ordinal
/// position, and a struct would need `#[derive(FromRow)]` + named
/// columns — more ceremony than a tuple for what's just a pipe from
/// the DB read into `Estimator::refresh()`. The alias exists because
/// 5-element tuples trip clippy's type-complexity lint, and naming
/// it documents the field order where it matters (3 float-ish fields
/// are easy to mix up).
pub type BuildHistoryRow = (String, String, f64, Option<f64>, Option<f64>);

/// Assignment lifecycle status (assignments table).
///
/// Only `Pending` and `Completed` are currently set from Rust. The schema
/// also supports `acknowledged`/`failed`/`cancelled` — reserved for phase2c
/// worker-ack and distinct failure reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentStatus {
    Pending,
    Acknowledged,
    Completed,
    Failed,
    Cancelled,
}

impl AssignmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged => "acknowledged",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
/// Database operations for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerDb {
    pool: PgPool,
}

/// EMA alpha for duration estimation updates.
const EMA_ALPHA: f64 = 0.3;

/// Row from `load_nonterminal_builds`. FromRow for named-column
/// mapping (tuples at this arity are error-prone).
///
/// `options_json`: `Option<Json<BuildOptions>>` because the column
/// is NULLable (rows from before migration 004). Caller unwraps
/// with `.map(|j| j.0).unwrap_or_default()`.
#[derive(Debug, sqlx::FromRow)]
pub struct RecoveryBuildRow {
    pub build_id: Uuid,
    pub tenant_id: Option<String>,
    pub status: String,
    pub priority_class: String,
    pub keep_going: bool,
    pub options_json: Option<sqlx::types::Json<crate::state::BuildOptions>>,
}

/// Row from `load_nonterminal_derivations`. Mirrors the INSERT
/// columns from `batch_upsert_derivations` plus live-state fields
/// (retry_count, assigned_worker_id, failed_workers).
#[derive(Debug, sqlx::FromRow)]
pub struct RecoveryDerivationRow {
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: String,
    pub required_features: Vec<String>,
    pub assigned_worker_id: Option<String>,
    pub retry_count: i32,
    pub expected_output_paths: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
    pub failed_workers: Vec<String>,
}

/// Row for [`SchedulerDb::batch_upsert_derivations`].
#[derive(Debug)]
pub struct DerivationRow {
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: DerivationStatus,
    pub required_features: Vec<String>,
    // Phase 3b recovery columns. These are written at merge time
    // so recover_from_pg() can fully reconstruct DerivationState.
    pub expected_output_paths: Vec<String>,
    pub output_names: Vec<String>,
    pub is_fixed_output: bool,
}

impl SchedulerDb {
    /// Create a new database handle from a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get a reference to the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -----------------------------------------------------------------------
    // Build operations
    // -----------------------------------------------------------------------

    /// Insert a new build record.
    ///
    /// `keep_going` + `options` are for Phase 3b state recovery —
    /// `recover_from_pg()` reads them back to rebuild BuildInfo.
    /// `options` is serialized to JSONB (`sqlx::types::Json`
    /// wrapper handles the serde round-trip).
    pub async fn insert_build(
        &self,
        build_id: Uuid,
        tenant_id: Option<&str>,
        priority_class: crate::state::PriorityClass,
        keep_going: bool,
        options: &crate::state::BuildOptions,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO builds
                (build_id, tenant_id, requestor, status, priority_class,
                 keep_going, options_json)
            VALUES ($1, $2::uuid, '', 'pending', $3, $4, $5)
            "#,
        )
        .bind(build_id)
        .bind(tenant_id)
        .bind(priority_class.as_str())
        .bind(keep_going)
        // Json<&T>: sqlx serializes via serde_json and binds as
        // JSONB. BuildOptions derives Serialize (add if missing).
        .bind(sqlx::types::Json(options))
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
        sqlx::query("DELETE FROM builds WHERE build_id = $1")
            .bind(build_id)
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
            BuildState::Pending => "",
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

    // -----------------------------------------------------------------------
    // Derivation operations
    // -----------------------------------------------------------------------

    /// Update a derivation's status.
    pub async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_worker: Option<&WorkerId>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE derivations
            SET status = $2, assigned_worker_id = $3, updated_at = now()
            WHERE drv_hash = $1
            "#,
        )
        .bind(drv_hash.as_str())
        .bind(status.as_str())
        .bind(assigned_worker.map(WorkerId::as_str))
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Increment the retry count for a derivation.
    pub async fn increment_retry_count(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations SET retry_count = retry_count + 1, updated_at = now() WHERE drv_hash = $1",
        )
        .bind(drv_hash.as_str())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Append a worker ID to a derivation's `failed_workers` array.
    ///
    /// Called from `handle_transient_failure` and `reassign_derivations`
    /// (worker disconnect mid-build) so recovery can rebuild the
    /// HashSet that feeds best_worker exclusion + poison detection.
    ///
    /// `array_append` is idempotent for our purposes: PG arrays CAN
    /// have duplicates (not a set), but recovery builds a HashSet
    /// from the array so dupes collapse. We COULD de-dup in SQL with
    /// `WHERE NOT ($2 = ANY(failed_workers))` — not worth the extra
    /// clause for a rare path (transient failures).
    pub async fn append_failed_worker(
        &self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations \
             SET failed_workers = array_append(failed_workers, $2), updated_at = now() \
             WHERE drv_hash = $1",
        )
        .bind(drv_hash.as_str())
        .bind(worker_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Clear `failed_workers` + `retry_count` for a derivation.
    /// Called on poison-TTL expiry (reset_from_poison) so PG
    /// matches in-mem. Without this, crash after poison-reset →
    /// recovery loads stale failed_workers → immediately excluded
    /// from dispatch (best_worker skips them).
    pub async fn clear_failed_workers_and_retry(
        &self,
        drv_hash: &DrvHash,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations \
             SET failed_workers = '{}', retry_count = 0, updated_at = now() \
             WHERE drv_hash = $1",
        )
        .bind(drv_hash.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // scheduler_live_pins — auto-pin live-build input closure
    //
    // Scheduler+store share PG (same migrations/ dir). Scheduler writes
    // directly to scheduler_live_pins; store's gc/mark.rs seeds from it.
    // Best-effort: PG failure during pin/unpin logs + continues (24h grace
    // period is the fallback safety net).
    // -----------------------------------------------------------------------

    /// Pin a batch of store paths as live-build inputs for a drv.
    /// SHA-256 each path for store_path_hash (matches narinfo keying).
    /// ON CONFLICT DO NOTHING: re-pin is idempotent.
    pub async fn pin_live_inputs(
        &self,
        drv_hash: &DrvHash,
        store_paths: &[String],
    ) -> Result<(), sqlx::Error> {
        if store_paths.is_empty() {
            return Ok(());
        }
        use sha2::Digest;
        let hashes: Vec<Vec<u8>> = store_paths
            .iter()
            .map(|p| sha2::Sha256::digest(p.as_bytes()).to_vec())
            .collect();

        // Batch INSERT via UNNEST. Arrays are parallel (same length
        // by construction: same source vec). ON CONFLICT DO NOTHING
        // for idempotence — re-dispatching a drv (after reassign)
        // shouldn't error.
        sqlx::query(
            r#"
            INSERT INTO scheduler_live_pins (store_path_hash, drv_hash)
            SELECT * FROM UNNEST($1::bytea[], $2::text[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&hashes)
        .bind(vec![drv_hash.as_str(); hashes.len()])
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Unpin all live inputs for a drv. Called on terminal status.
    /// Idempotent: unpinning a never-pinned drv = 0 rows deleted.
    pub async fn unpin_live_inputs(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM scheduler_live_pins WHERE drv_hash = $1")
            .bind(drv_hash.as_str())
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Sweep stale pins: delete rows for derivations that are no
    /// longer in non-terminal state. Called after recovery (handles
    /// crash-between-pin-and-unpin — scheduler crashed after pin at
    /// dispatch but before unpin at completion).
    ///
    /// The subquery matches load_nonterminal_derivations' filter
    /// (both use `TERMINAL_STATUSES`): a drv NOT in that set is
    /// terminal (or deleted entirely).
    pub async fn sweep_stale_live_pins(&self) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            r#"
            DELETE FROM scheduler_live_pins
             WHERE drv_hash NOT IN (
               SELECT drv_hash FROM derivations
                WHERE status <> ALL($1::text[])
             )
            "#,
        )
        .bind(TERMINAL_STATUSES)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // -----------------------------------------------------------------------
    // Build-derivation mapping
    // -----------------------------------------------------------------------

    /// Link a build to a derivation.
    pub async fn insert_build_derivation(
        &self,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Batch operations (for persist_merge_to_db)
    // -----------------------------------------------------------------------

    /// Batch-upsert derivations. Returns a map `drv_hash -> derivation_id`.
    ///
    /// Uses QueryBuilder::push_values for multi-row INSERT. RETURNING includes
    /// drv_hash because PG doesn't guarantee RETURNING order matches input.
    pub async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
    ) -> Result<HashMap<String, Uuid>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        let mut qb = QueryBuilder::new(
            "INSERT INTO derivations \
             (drv_hash, drv_path, pname, system, status, required_features, \
              expected_output_paths, output_names, is_fixed_output) ",
        );
        qb.push_values(rows, |mut b, row| {
            b.push_bind(&row.drv_hash)
                .push_bind(&row.drv_path)
                .push_bind(&row.pname)
                .push_bind(&row.system)
                .push_bind(row.status.as_str())
                .push_bind(&row.required_features)
                .push_bind(&row.expected_output_paths)
                .push_bind(&row.output_names)
                .push_bind(row.is_fixed_output);
        });
        // ON CONFLICT: update the recovery columns too. A second
        // build requesting the same derivation may have fresher
        // expected_output_paths (same drv_hash → same outputs, so
        // this is idempotent in practice, but keeps the row in sync
        // with in-mem). status/retry etc stay as-is — those reflect
        // LIVE state, not merge-time snapshot.
        qb.push(
            " ON CONFLICT (drv_hash) DO UPDATE SET \
                updated_at = now(), \
                expected_output_paths = EXCLUDED.expected_output_paths, \
                output_names = EXCLUDED.output_names, \
                is_fixed_output = EXCLUDED.is_fixed_output \
             RETURNING drv_hash, derivation_id",
        );

        let result: Vec<(String, Uuid)> = qb.build_query_as().fetch_all(&mut *tx).await?;
        Ok(result.into_iter().collect())
    }

    /// Batch-insert build_derivations links.
    pub async fn batch_insert_build_derivations(
        tx: &mut PgConnection,
        build_id: Uuid,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(());
        }

        let mut qb = QueryBuilder::new("INSERT INTO build_derivations (build_id, derivation_id) ");
        qb.push_values(derivation_ids, |mut b, did| {
            b.push_bind(build_id).push_bind(did);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
        Ok(())
    }

    /// Batch-insert edges.
    pub async fn batch_insert_edges(
        tx: &mut PgConnection,
        edges: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if edges.is_empty() {
            return Ok(());
        }

        let mut qb = QueryBuilder::new("INSERT INTO derivation_edges (parent_id, child_id) ");
        qb.push_values(edges, |mut b, (p, c)| {
            b.push_bind(p).push_bind(c);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Assignment operations
    // -----------------------------------------------------------------------

    /// Create a new assignment record. Returns the assignment_id.
    pub async fn insert_assignment(
        &self,
        derivation_id: Uuid,
        worker_id: &WorkerId,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let row: (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO assignments (derivation_id, worker_id, generation, status)
            VALUES ($1, $2, $3, 'pending')
            RETURNING assignment_id
            "#,
        )
        .bind(derivation_id)
        .bind(worker_id.as_str())
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Delete the in-progress assignment for a derivation.
    /// Called from dispatch.rs on try_send failure to clean up the
    /// PG row that insert_assignment wrote (no worker ever got the
    /// assignment, so the row is misleading on recovery).
    ///
    /// Only deletes `pending`/`acknowledged` rows — terminal rows
    /// are audit-valuable even for stale derivations.
    pub async fn delete_latest_assignment(&self, derivation_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            "DELETE FROM assignments \
             WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')",
        )
        .bind(derivation_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update an assignment status.
    pub async fn update_assignment_status(
        &self,
        derivation_id: Uuid,
        status: AssignmentStatus,
    ) -> Result<(), sqlx::Error> {
        if status.is_terminal() {
            sqlx::query(
                r#"
                UPDATE assignments
                SET status = $2, completed_at = now()
                WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')
                "#,
            )
            .bind(derivation_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE assignments
                SET status = $2
                WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')
                "#,
            )
            .bind(derivation_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Build history (async/batched)
    // -----------------------------------------------------------------------

    /// Read the full build_history table for estimator refresh.
    ///
    /// Return tuple: `(pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores)`.
    /// Aliased as [`BuildHistoryRow`] — 5-element tuples trip clippy's
    /// type-complexity lint, and naming it documents the field order
    /// at the one place where it's easy to mix up (3 f64-ish fields).
    ///
    /// 5-tuple (cpu included). Estimator::refresh signature matches.
    /// The estimator doesn't USE cpu_cores yet (size-class routing
    /// bumps on memory only, not cpu) but reading it now means future
    /// cpu-bump logic is a pure-estimator change, no DB roundtrip.
    ///
    /// The estimator loads this into a HashMap at startup and refreshes
    /// on Tick (every 60s). A full table scan is fine: even 10k distinct
    /// (pname, system) pairs is ~1 MB and <10ms. The alternative —
    /// loading on-demand per estimate — would be an async PG roundtrip
    /// inside the single-threaded actor loop on every dispatch decision.
    /// Batch-load once, query in-memory.
    pub async fn read_build_history(&self) -> Result<Vec<BuildHistoryRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT pname, system, ema_duration_secs, ema_peak_memory_bytes, ema_peak_cpu_cores
            FROM build_history
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    // -----------------------------------------------------------------------
    // Phase 3b: state recovery read queries
    //
    // Called by recover_from_pg() on LeaderAcquired transition. Loads
    // all non-terminal builds + derivations + edges + build_derivations +
    // assignments, from which the actor rebuilds its in-mem DAG.
    //
    // FromRow structs (not tuples): recovery needs ~10 fields per
    // derivation, tuples at that arity are error-prone (wrong-field
    // assignment). #[derive(FromRow)] + named columns is safer.
    // -----------------------------------------------------------------------

    /// Load all non-terminal builds. Terminal builds (succeeded/
    /// failed/cancelled) don't need recovery — they're done, any
    /// WatchBuild subscriber has already received the terminal event
    /// (or will time out waiting, which is the same as "scheduler
    /// restarted and forgot").
    pub async fn load_nonterminal_builds(&self) -> Result<Vec<RecoveryBuildRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT build_id, tenant_id::text, status, priority_class,
                   keep_going, options_json
            FROM builds
            WHERE status IN ('pending', 'active')
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Load all non-terminal derivations. Same terminal-exclusion
    /// filter (`TERMINAL_STATUSES`) as sweep_stale_live_pins; the
    /// partial index (status_idx) makes this efficient for large
    /// tables.
    pub async fn load_nonterminal_derivations(
        &self,
    ) -> Result<Vec<RecoveryDerivationRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT derivation_id, drv_hash, drv_path, pname, system, status,
                   required_features, assigned_worker_id, retry_count,
                   expected_output_paths, output_names, is_fixed_output,
                   failed_workers
            FROM derivations
            WHERE status <> ALL($1::text[])
            "#,
        )
        .bind(TERMINAL_STATUSES)
        .fetch_all(&self.pool)
        .await
    }

    /// Load edges for a set of derivation IDs. Only loads edges
    /// where BOTH endpoints are in the set — an edge to a completed
    /// derivation (not in `derivation_ids`) is dropped; the in-mem
    /// DAG treats "no edge" = "no incomplete dependency" = "ready"
    /// (compute_initial_states). This is correct: a completed
    /// dependency IS satisfied.
    ///
    /// ANY($1): PG unnest-style array comparison. Scales to ~100k
    /// IDs before the planner starts preferring a temp table; recovery
    /// DAGs are typically <10k nodes.
    pub async fn load_edges_for_derivations(
        &self,
        derivation_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT parent_id, child_id FROM derivation_edges
            WHERE parent_id = ANY($1) AND child_id = ANY($1)
            "#,
        )
        .bind(derivation_ids)
        .fetch_all(&self.pool)
        .await
    }

    /// Load (build_id, derivation_id) links for a set of builds.
    /// `recover_from_pg` uses this to rebuild `interested_builds`
    /// on each DerivationState and `derivation_hashes` on BuildInfo.
    pub async fn load_build_derivations(
        &self,
        build_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, Uuid)>, sqlx::Error> {
        if build_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT build_id, derivation_id FROM build_derivations
            WHERE build_id = ANY($1)
            "#,
        )
        .bind(build_ids)
        .fetch_all(&self.pool)
        .await
    }

    /// Max assignment generation ever written. recover_from_pg()
    /// seeds its generation counter from this + 1 — defensive
    /// monotonicity guard in case the Lease's generation (in its
    /// annotation) was lost/reset (e.g., someone `kubectl delete
    /// lease`). Workers with a stale generation reject assignments;
    /// if we accidentally reused a gen, workers that received old
    /// assignments would ALSO accept new ones from that gen —
    /// dual-processing. Seeding from PG's high-water mark prevents
    /// that regardless of Lease state.
    ///
    /// BIGINT → i64 → u64 cast at the caller. `None` = no
    /// assignments ever (fresh cluster).
    pub async fn max_assignment_generation(&self) -> Result<Option<i64>, sqlx::Error> {
        let row: (Option<i64>,) = sqlx::query_as("SELECT MAX(generation) FROM assignments")
            .fetch_one(&self.pool)
            .await?;
        Ok(row.0)
    }

    /// Max sequence number per build_id from build_event_log.
    /// recover_from_pg() seeds `build_sequences` from this so new
    /// events continue from where the old leader left off — a
    /// reconnecting WatchBuild client with `since_sequence=N` would
    /// miss events if we reset to 0 and emitted new events with
    /// seq=1 (<N → filtered by the client).
    ///
    /// Only for builds that are still active (caller filters by
    /// build_ids from load_nonterminal_builds).
    pub async fn max_sequence_per_build(
        &self,
        build_ids: &[Uuid],
    ) -> Result<Vec<(Uuid, i64)>, sqlx::Error> {
        if build_ids.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT build_id, MAX(sequence) FROM build_event_log
            WHERE build_id = ANY($1) GROUP BY build_id
            "#,
        )
        .bind(build_ids)
        .fetch_all(&self.pool)
        .await
    }

    /// Update the build history EMA for a (pname, system) pair.
    ///
    /// `peak_memory_bytes`/`output_size_bytes`/`peak_cpu_cores`:
    /// None means the worker had no signal (build failed before cgroup
    /// populated, or exited in <1s before CPU poll sampled). Must NOT
    /// drag the EMA toward zero — None → column unchanged.
    ///
    /// The COALESCE(blend, new, old) pattern handles all four
    /// old×new nullability combinations:
    ///   old=Some, new=Some → blend (normal EMA)
    ///   old=None, new=Some → blend is NULL (NULL*x), falls to new (first sample)
    ///   old=Some, new=None → blend is NULL (x+NULL), falls to new=NULL, falls to old (keep)
    ///   old=None, new=None → all NULL (still no signal)
    ///
    /// PG: any arithmetic with NULL yields NULL; COALESCE picks first non-NULL.
    ///
    /// # Historical memory data
    ///
    /// Older worker versions sent VmHWM of nix-daemon (~10MB
    /// regardless of builder memory). cgroup memory.peak fixes that.
    /// If existing build_history rows have wrong memory values, EMA
    /// alpha=0.3 means ~10 completions per (pname,system) washes the
    /// bad data out (0.7^10 ≈ 2.8% of the old value remains). No
    /// migration needed — time heals it.
    pub async fn update_build_history(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
        peak_memory_bytes: Option<u64>,
        output_size_bytes: Option<u64>,
        peak_cpu_cores: Option<f64>,
    ) -> Result<(), sqlx::Error> {
        // u64 → f64 for DOUBLE PRECISION binding. Precision loss at
        // ~2^53 bytes (~9 PB) is not a concern. cpu is already f64.
        let peak_mem = peak_memory_bytes.map(|b| b as f64);
        let out_size = output_size_bytes.map(|b| b as f64);

        sqlx::query(
            r#"
            INSERT INTO build_history
              (pname, system, ema_duration_secs,
               ema_peak_memory_bytes, ema_output_size_bytes,
               ema_peak_cpu_cores,
               sample_count, last_updated)
            VALUES ($1, $2, $3, $5, $6, $7, 1, now())
            ON CONFLICT (pname, system) DO UPDATE SET
                ema_duration_secs = build_history.ema_duration_secs * (1.0 - $4) + $3 * $4,
                ema_peak_memory_bytes = COALESCE(
                    build_history.ema_peak_memory_bytes * (1.0 - $4) + $5 * $4,
                    $5,
                    build_history.ema_peak_memory_bytes),
                ema_output_size_bytes = COALESCE(
                    build_history.ema_output_size_bytes * (1.0 - $4) + $6 * $4,
                    $6,
                    build_history.ema_output_size_bytes),
                ema_peak_cpu_cores = COALESCE(
                    build_history.ema_peak_cpu_cores * (1.0 - $4) + $7 * $4,
                    $7,
                    build_history.ema_peak_cpu_cores),
                sample_count = build_history.sample_count + 1,
                last_updated = now()
            "#,
        )
        .bind(pname)
        .bind(system)
        .bind(actual_duration_secs)
        .bind(EMA_ALPHA)
        .bind(peak_mem)
        .bind(out_size)
        .bind(peak_cpu_cores)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Penalty-write for misclassified builds: a build that took >2×
    /// its class cutoff was routed wrong. Overwrite the EMA with the
    /// actual duration (NOT blend — a blend would take multiple
    /// overruns to correct) so the NEXT classify() picks a larger
    /// class. Also bump the counter for dashboard/alerting.
    ///
    /// This is intentionally harsh: one bad classification is enough
    /// to fix the estimate. If the build was a fluke (transient slow
    /// disk), the next normal completion blends it back down. Better
    /// to over-correct once than to keep OOMing small workers.
    pub async fn update_build_history_misclassified(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE build_history
            SET ema_duration_secs = $3,
                misclassification_count = misclassification_count + 1,
                last_updated = now()
            WHERE pname = $1 AND system = $2
            "#,
        )
        .bind(pname)
        .bind(system)
        .bind(actual_duration_secs)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

/// Read persisted build events for since_sequence replay.
///
/// Returns events in the half-open range `(since, until]` — strictly
/// after `since` (the gateway's last-seen seq), at most `until`
/// (the actor's last-emitted seq at subscribe time). `until` bounds
/// the replay so we don't duplicate what the broadcast will carry:
/// everything with seq > until was emitted AFTER subscribe and is
/// guaranteed to be on the broadcast channel.
///
/// Free fn (not `SchedulerDb` method): `bridge_build_events` is a
/// free fn in grpc/mod.rs that only has a `PgPool`, not a
/// `SchedulerDb`. Adding `SchedulerDb` to `SchedulerGrpc` would
/// drag the whole db module into grpc; a bare pool is cheaper.
///
/// u64 → i64 cast: PG BIGINT is signed. See event_log.rs for the
/// same rationale (2^63 events per build is not a real concern).
pub async fn read_event_log(
    pool: &PgPool,
    build_id: Uuid,
    since: u64,
    until: u64,
) -> Result<Vec<(u64, Vec<u8>)>, sqlx::Error> {
    let rows: Vec<(i64, Vec<u8>)> = sqlx::query_as(
        "SELECT sequence, event_bytes FROM build_event_log \
         WHERE build_id = $1 AND sequence > $2 AND sequence <= $3 \
         ORDER BY sequence",
    )
    .bind(build_id)
    .bind(since as i64)
    .bind(until as i64)
    .fetch_all(pool)
    .await?;
    // i64 → u64: rows were written with `seq as i64` from a u64, so
    // they round-trip exactly. No values < 0 exist (seq starts at 1).
    Ok(rows.into_iter().map(|(s, b)| (s as u64, b)).collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_alpha_range() {
        const { assert!(EMA_ALPHA > 0.0 && EMA_ALPHA < 1.0) };
    }

    #[test]
    fn test_derivation_status_roundtrip() -> anyhow::Result<()> {
        for status in [
            DerivationStatus::Created,
            DerivationStatus::Queued,
            DerivationStatus::Ready,
            DerivationStatus::Assigned,
            DerivationStatus::Running,
            DerivationStatus::Completed,
            DerivationStatus::Failed,
            DerivationStatus::Poisoned,
            DerivationStatus::DependencyFailed,
        ] {
            let s = status.as_str();
            let parsed: DerivationStatus = s.parse()?;
            assert_eq!(parsed, status);
        }
        Ok(())
    }

    #[test]
    fn test_build_state_roundtrip() -> anyhow::Result<()> {
        for state in [
            BuildState::Pending,
            BuildState::Active,
            BuildState::Succeeded,
            BuildState::Failed,
            BuildState::Cancelled,
        ] {
            let s = state.as_str();
            let parsed: BuildState = s.parse()?;
            assert_eq!(parsed, state);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Direct DB integration tests via TestDb
    // -----------------------------------------------------------------------

    use rio_test_support::TestDb;

    #[test]
    fn test_assignment_status_as_str_exhaustive() {
        assert_eq!(AssignmentStatus::Pending.as_str(), "pending");
        assert_eq!(AssignmentStatus::Acknowledged.as_str(), "acknowledged");
        assert_eq!(AssignmentStatus::Completed.as_str(), "completed");
        assert_eq!(AssignmentStatus::Failed.as_str(), "failed");
        assert_eq!(AssignmentStatus::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn test_assignment_status_is_terminal() {
        assert!(!AssignmentStatus::Pending.is_terminal());
        assert!(!AssignmentStatus::Acknowledged.is_terminal());
        assert!(AssignmentStatus::Completed.is_terminal());
        assert!(AssignmentStatus::Failed.is_terminal());
        assert!(AssignmentStatus::Cancelled.is_terminal());
    }

    /// Upsert a single derivation and return its db_id. Helper for tests
    /// that need a valid derivation_id foreign key.
    async fn insert_test_derivation(db: &SchedulerDb, drv_hash: &str) -> anyhow::Result<Uuid> {
        let mut tx = db.pool.begin().await?;
        let row = DerivationRow {
            drv_hash: drv_hash.into(),
            drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
        };
        let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
        tx.commit().await?;
        Ok(*ids.get(drv_hash).expect("just inserted"))
    }

    #[tokio::test]
    async fn test_insert_build_derivation_idempotent() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(
            build_id,
            None,
            crate::state::PriorityClass::Scheduled,
            true,
            &Default::default(),
        )
        .await?;
        let drv_id = insert_test_derivation(&db, "aaa").await?;

        // Call twice — ON CONFLICT DO NOTHING should make the second call a no-op.
        db.insert_build_derivation(build_id, drv_id).await?;
        db.insert_build_derivation(build_id, drv_id).await?;

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(count.0, 1, "ON CONFLICT should prevent duplicate");
        Ok(())
    }

    /// BuildState::Pending has now_col="" → no timestamp column touched.
    #[tokio::test]
    async fn test_update_build_status_pending_no_timestamps() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(
            build_id,
            None,
            crate::state::PriorityClass::Scheduled,
            true,
            &Default::default(),
        )
        .await?;

        // Insert starts at 'pending'. Transition to Active then back to Pending
        // (unusual but valid at the DB layer — the state machine rejects it,
        // but this is testing the raw SQL branch).
        db.update_build_status(build_id, BuildState::Active, None)
            .await?;
        db.update_build_status(build_id, BuildState::Pending, None)
            .await?;

        // Query timestamp as Option<String> via text cast to avoid adding a
        // chrono dep just for test assertions.
        let (status, finished_at): (String, Option<String>) =
            sqlx::query_as("SELECT status, finished_at::text FROM builds WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(status, "pending");
        // Pending transition does NOT set finished_at (the now_col="" branch).
        assert!(finished_at.is_none());
        Ok(())
    }

    /// Non-terminal status (Acknowledged) → completed_at stays NULL.
    #[tokio::test]
    async fn test_update_assignment_status_acknowledged_no_completed_at() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "bbb").await?;
        db.insert_assignment(drv_id, &WorkerId::from("worker-1"), 1)
            .await?;

        db.update_assignment_status(drv_id, AssignmentStatus::Acknowledged)
            .await?;

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await?;
        assert_eq!(status, "acknowledged");
        assert!(completed_at.is_none(), "non-terminal → completed_at NULL");
        Ok(())
    }

    /// Terminal status (Completed) → completed_at = now().
    #[tokio::test]
    async fn test_update_assignment_status_completed_sets_completed_at() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "ccc").await?;
        db.insert_assignment(drv_id, &WorkerId::from("worker-1"), 1)
            .await?;

        db.update_assignment_status(drv_id, AssignmentStatus::Completed)
            .await?;

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await?;
        assert_eq!(status, "completed");
        assert!(completed_at.is_some(), "terminal → completed_at set");
        Ok(())
    }

    // r[verify sched.estimate.ema-alpha]
    /// EMA: first insert uses the duration directly; second update blends.
    /// ema = old * (1-ALPHA) + new * ALPHA = 10 * 0.7 + 20 * 0.3 = 13.
    #[tokio::test]
    async fn test_update_build_history_ema_accumulates() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        db.update_build_history("hello", "x86_64-linux", 10.0, None, None, None)
            .await?;
        let (ema1, count1): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        assert!((ema1 - 10.0).abs() < 0.001, "first insert: ema={ema1}");
        assert_eq!(count1, 1);

        db.update_build_history("hello", "x86_64-linux", 20.0, None, None, None)
            .await?;
        let (ema2, count2): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        let expected = 10.0 * (1.0 - EMA_ALPHA) + 20.0 * EMA_ALPHA;
        assert!(
            (ema2 - expected).abs() < 0.001,
            "second update: expected {expected}, got {ema2}"
        );
        assert_eq!(count2, 2);
        Ok(())
    }

    /// The COALESCE(blend, new, old) pattern for the nullable resource
    /// EMAs. All four old×new combinations in one test: the hard part
    /// is the `None` cases, where a naïve `old*0.7 + 0*0.3` would drag
    /// a real EMA toward zero on every build with a failed proc read.
    #[tokio::test]
    async fn test_update_build_history_memory_ema_none_is_no_signal() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let fetch = || async {
            sqlx::query_as::<_, (Option<f64>, Option<f64>)>(
                "SELECT ema_peak_memory_bytes, ema_output_size_bytes \
                 FROM build_history WHERE pname = 'mem' AND system = 'x86_64-linux'",
            )
            .fetch_one(&test_db.pool)
            .await
        };

        // 1. old=None, new=None → stays None.
        db.update_build_history("mem", "x86_64-linux", 1.0, None, None, None)
            .await?;
        let (mem, out) = fetch().await?;
        assert_eq!(mem, None, "None×None: mem should stay NULL");
        assert_eq!(out, None, "None×None: out should stay NULL");

        // 2. old=None, new=Some → initializes (blend is NULL, falls
        //    to $new).
        db.update_build_history(
            "mem",
            "x86_64-linux",
            1.0,
            Some(100_000_000),
            Some(5000),
            None,
        )
        .await?;
        let (mem, out) = fetch().await?;
        assert!(
            mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
            "None×Some: mem should initialize to 100M, got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - 5000.0).abs() < 0.001),
            "None×Some: out should initialize to 5000, got {out:?}"
        );

        // 3. old=Some, new=None → KEEPS old (the important case — no
        //    signal doesn't drag). blend is NULL (x+NULL), falls to
        //    $new=NULL, falls to old.
        db.update_build_history("mem", "x86_64-linux", 1.0, None, None, None)
            .await?;
        let (mem, out) = fetch().await?;
        assert!(
            mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
            "Some×None: mem should be UNCHANGED at 100M (no drag), got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - 5000.0).abs() < 0.001),
            "Some×None: out should be UNCHANGED at 5000 (no drag), got {out:?}"
        );

        // 4. old=Some, new=Some → normal EMA blend.
        db.update_build_history(
            "mem",
            "x86_64-linux",
            1.0,
            Some(200_000_000),
            Some(10_000),
            None,
        )
        .await?;
        let (mem, out) = fetch().await?;
        let expect_mem = 100_000_000.0 * (1.0 - EMA_ALPHA) + 200_000_000.0 * EMA_ALPHA;
        let expect_out = 5000.0 * (1.0 - EMA_ALPHA) + 10_000.0 * EMA_ALPHA;
        assert!(
            mem.is_some_and(|m| (m - expect_mem).abs() < 0.001),
            "Some×Some: mem should blend to {expect_mem}, got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - expect_out).abs() < 0.001),
            "Some×Some: out should blend to {expect_out}, got {out:?}"
        );

        Ok(())
    }

    /// Misclassification penalty: overwrites EMA (not blends), bumps count.
    /// Harsh correction so the next classify() picks the right class after
    /// ONE bad route, not several.
    #[tokio::test]
    async fn test_update_build_history_misclassified_overwrites() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        // Seed: EMA at 10s (would classify as "small").
        db.update_build_history("slowpoke", "x86_64-linux", 10.0, None, None, None)
            .await?;

        // Actual took 200s. Penalty write.
        db.update_build_history_misclassified("slowpoke", "x86_64-linux", 200.0)
            .await?;

        let (ema, miscount): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, misclassification_count FROM build_history \
             WHERE pname = 'slowpoke' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await?;

        // EMA is OVERWRITTEN (not 10*0.7+200*0.3=67). The next
        // classify() sees 200s, routes to large. That's the point.
        assert!(
            (ema - 200.0).abs() < 0.001,
            "penalty overwrites (not blends): expected 200, got {ema}"
        );
        assert_eq!(miscount, 1, "misclassification counter bumped");

        // Second penalty: counter increments, EMA overwritten again.
        db.update_build_history_misclassified("slowpoke", "x86_64-linux", 180.0)
            .await?;
        let (ema2, miscount2): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, misclassification_count FROM build_history \
             WHERE pname = 'slowpoke'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        assert!((ema2 - 180.0).abs() < 0.001);
        assert_eq!(miscount2, 2);

        Ok(())
    }

    /// The estimator reads `ema_peak_memory_bytes` for memory-bump
    /// classify(). Verify the write→read roundtrip works end to end.
    #[tokio::test]
    async fn test_build_history_memory_roundtrip_read() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        db.update_build_history(
            "roundtrip",
            "aarch64-linux",
            42.0,
            Some(1_073_741_824),
            None,
            Some(4.5), // peak_cpu_cores
        )
        .await?;

        let rows = db.read_build_history().await?;
        let row = rows
            .iter()
            .find(|(p, s, _, _, _)| p == "roundtrip" && s == "aarch64-linux")
            .expect("written row should be readable");

        assert!((row.2 - 42.0).abs() < 0.001, "duration: {}", row.2);
        assert!(
            row.3.is_some_and(|m| (m - 1_073_741_824.0).abs() < 0.001),
            "peak mem: {:?}",
            row.3
        );
        // cpu_cores round-trips too. Verifies the column is in both
        // the INSERT and the SELECT.
        assert!(
            row.4.is_some_and(|c| (c - 4.5).abs() < 0.001),
            "peak cpu cores: {:?}",
            row.4
        );
        Ok(())
    }

    /// cpu_cores uses the same COALESCE(blend, new, old) pattern as
    /// memory. A build that exits in <1s (before the 1Hz poller
    /// samples) reports 0.0 → None → column unchanged. The next real
    /// build blends properly. Same "no signal doesn't drag" semantics.
    ///
    /// Separate test from the memory one: checks the NEW column's
    /// COALESCE specifically (easy to forget to add it to the UPSERT).
    #[tokio::test]
    async fn test_update_build_history_cpu_cores_coalesce() -> anyhow::Result<()> {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let fetch_cpu = || async {
            sqlx::query_scalar::<_, Option<f64>>(
                "SELECT ema_peak_cpu_cores FROM build_history \
                 WHERE pname = 'cpu' AND system = 'x86_64-linux'",
            )
            .fetch_one(&test_db.pool)
            .await
        };

        // First: None (short build, no samples) → column stays NULL.
        db.update_build_history("cpu", "x86_64-linux", 0.5, None, None, None)
            .await?;
        assert_eq!(fetch_cpu().await?, None, "None initializes to NULL");

        // Second: Some(4.0) → initializes.
        db.update_build_history("cpu", "x86_64-linux", 120.0, None, None, Some(4.0))
            .await?;
        let cpu = fetch_cpu().await?;
        assert!(
            cpu.is_some_and(|c| (c - 4.0).abs() < 0.001),
            "Some initializes: expected 4.0, got {cpu:?}"
        );

        // Third: None again (another short build) → UNCHANGED at 4.0.
        // This is the bug-catcher: if COALESCE is missing from the
        // UPSERT for this column, NULL*0.7 + NULL*0.3 = NULL would
        // clobber the good data.
        db.update_build_history("cpu", "x86_64-linux", 0.5, None, None, None)
            .await?;
        let cpu = fetch_cpu().await?;
        assert!(
            cpu.is_some_and(|c| (c - 4.0).abs() < 0.001),
            "Some×None: UNCHANGED (no drag). If this fails, \
             COALESCE for ema_peak_cpu_cores is missing: {cpu:?}"
        );

        // Fourth: Some(8.0) → blends: 4.0*0.7 + 8.0*0.3 = 5.2.
        db.update_build_history("cpu", "x86_64-linux", 300.0, None, None, Some(8.0))
            .await?;
        let cpu = fetch_cpu().await?;
        let expected = 4.0 * (1.0 - EMA_ALPHA) + 8.0 * EMA_ALPHA;
        assert!(
            cpu.is_some_and(|c| (c - expected).abs() < 0.001),
            "Some×Some blends: expected {expected}, got {cpu:?}"
        );

        Ok(())
    }
}
