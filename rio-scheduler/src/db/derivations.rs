//! Per-derivation state + poison tracking — `derivations` table.

use super::{PoisonedDerivationRow, SchedulerDb, TERMINAL_STATUS_SQL};
use crate::state::{DerivationStatus, DrvHash, ExecutorId};

impl SchedulerDb {
    /// Update a derivation's status.
    pub async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_executor: Option<&ExecutorId>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE derivations
            SET status = $2, assigned_builder_id = $3, updated_at = now()
            WHERE drv_hash = $1
            "#,
            drv_hash.as_str(),
            status.as_str(),
            assigned_executor.map(ExecutorId::as_str),
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Batch variant of [`update_derivation_status`]: set the same
    /// status on many derivations in one round-trip.
    ///
    /// Used by `cancel_build_derivations` (N derivations → Cancelled)
    /// where the per-item variant caused N sequential PG round-trips
    /// inside the single-threaded actor — a 500-derivation cancel
    /// blocked heartbeats/dispatch for ~1000 RTTs. `ANY($1::text[])`
    /// collapses that to one round-trip.
    ///
    /// `assigned_builder_id` is NULLed: all current batch callers are
    /// terminal transitions (Cancelled) where the assignment is over.
    /// If a future caller needs per-row worker IDs, add a UNNEST
    /// variant — don't make this one variadic.
    ///
    /// [`update_derivation_status`]: Self::update_derivation_status
    pub async fn update_derivation_status_batch(
        &self,
        drv_hashes: &[&str],
        status: DerivationStatus,
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let result = sqlx::query!(
            r#"
            UPDATE derivations
            SET status = $2, assigned_builder_id = NULL, updated_at = now()
            WHERE drv_hash = ANY($1::text[])
            "#,
            drv_hashes as &[&str],
            status.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Increment the retry count for a derivation.
    pub async fn increment_retry_count(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations SET retry_count = retry_count + 1, updated_at = now() WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Append a worker ID to a derivation's `failed_builders` array.
    ///
    /// Called from `handle_transient_failure` and `reassign_derivations`
    /// (worker disconnect mid-build) so recovery can rebuild the
    /// HashSet that feeds best_executor exclusion + poison detection.
    ///
    /// The `WHERE NOT ($2 = ANY(failed_builders))` guard makes this a
    /// no-op when the worker is already recorded. PG arrays are NOT
    /// sets — without the guard, a flapping worker (disconnect →
    /// reconnect → disconnect) would append duplicates unboundedly.
    /// Recovery builds a HashSet from this array so dupes would
    /// collapse in-mem, but the PG row itself grows forever. The
    /// guard keeps the array bounded to distinct-workers-ever-failed.
    pub async fn append_failed_worker(
        &self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations \
             SET failed_builders = array_append(failed_builders, $2), updated_at = now() \
             WHERE drv_hash = $1 AND NOT ($2 = ANY(failed_builders))",
            drv_hash.as_str(),
            executor_id.as_str(),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // r[impl sched.fod.size-class-reactive]
    /// Persist a FOD's reactive `size_class_floor` (I-170, P0556).
    ///
    /// Called from `record_failure_and_check_poison` right after the
    /// in-mem promotion so a scheduler failover between OOM and retry
    /// doesn't reset the floor to None → re-dispatch to tiny → OOM
    /// again. Same write-at-mutation pattern as `append_failed_worker`
    /// above (NOT in `batch_upsert_derivations` — merge-time floor is
    /// always None and `ON CONFLICT DO UPDATE` there would clobber a
    /// promoted floor on re-merge).
    pub async fn update_size_class_floor(
        &self,
        drv_hash: &DrvHash,
        floor: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations SET size_class_floor = $2, updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
            floor,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // r[impl sched.poison.ttl-persist]
    /// Atomically set `status='poisoned'` AND `poisoned_at=now()`.
    ///
    /// Replaces the previous two-call sequence (`update_derivation_status`
    /// then `set_poisoned_at`) which had a crash window: status='poisoned'
    /// but poisoned_at=NULL. Rows in that state were invisible to
    /// `load_poisoned_derivations` (filtered by `poisoned_at IS NOT NULL`)
    /// — poison TTL tracking silently broken for those rows.
    ///
    /// `assigned_builder_id` is NULLed: a poisoned derivation has no
    /// assignment. Matches the in-mem semantics the caller should enforce.
    pub async fn persist_poisoned(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations \
             SET status = 'poisoned', poisoned_at = now(), \
                 assigned_builder_id = NULL, updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Clear poison state: NULL `poisoned_at`, empty `failed_builders`,
    /// zero `retry_count`, status='created'. Used by ClearPoison admin
    /// RPC + TTL expiry in `handle_tick`.
    pub async fn clear_poison(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, failed_builders = '{}', retry_count = 0,
                 status = 'created', updated_at = now()
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    // r[impl sched.db.clear-poison-batch]
    /// Batch variant of [`clear_poison`]: one round-trip for N hashes
    /// via `WHERE drv_hash = ANY($1)`. Same column set as the scalar.
    ///
    /// I-169: `merge.rs`' resubmit-reset path called `clear_poison`
    /// per-hash inside the single-threaded actor — a 500-node resubmit
    /// blocked heartbeat/dispatch for 500 sequential PG round-trips.
    /// Same shape as [`update_derivation_status_batch`].
    ///
    /// [`clear_poison`]: Self::clear_poison
    /// [`update_derivation_status_batch`]: Self::update_derivation_status_batch
    pub async fn clear_poison_batch(&self, drv_hashes: &[DrvHash]) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let hashes: Vec<&str> = drv_hashes.iter().map(DrvHash::as_str).collect();
        let result = sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, failed_builders = '{}', retry_count = 0,
                 status = 'created', updated_at = now()
             WHERE drv_hash = ANY($1::text[])",
            &hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // r[impl sched.db.derivations-gc]
    /// Delete up to `limit` orphan-terminal `derivations` rows: status
    /// is terminal AND no `build_derivations` link AND no `assignments`
    /// row. Returns rows deleted.
    ///
    /// I-169.2: 1.16M `dependency_failed` rows accumulated. Terminal
    /// rows are never re-read (recovery filters via
    /// `TERMINAL_STATUS_SQL`); once the owning build is deleted
    /// (008's `ON DELETE CASCADE` drops the `build_derivations` link)
    /// nothing references them. Subselect-LIMIT — PG has no
    /// `DELETE ... LIMIT` — so a 1M-row backlog drains over many
    /// ticks instead of one long table lock.
    ///
    /// `NOT EXISTS assignments`: the FK on `assignments.derivation_id`
    /// is still RESTRICT (028 dropped only the edges/build_derivations
    /// FKs). A `completed` row that ran keeps its assignment until that
    /// table's own retention sweeps it. `dependency_failed` rows (the
    /// 1.16M case) never dispatched → no assignment → eligible
    /// immediately.
    ///
    /// `derivation_edges` rows referencing deleted ids are left in
    /// place (FK dropped in 028). They're harmless:
    /// `load_edges_for_derivations` filters by `ANY(nonterminal_ids)`
    /// on both endpoints, so orphans are never loaded.
    pub async fn gc_orphan_terminal_derivations(&self, limit: i64) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(&format!(
            "DELETE FROM derivations WHERE derivation_id IN (
                 SELECT d.derivation_id FROM derivations d
                 WHERE d.status IN {TERMINAL_STATUS_SQL}
                   AND NOT EXISTS (SELECT 1 FROM build_derivations bd
                                   WHERE bd.derivation_id = d.derivation_id)
                   AND NOT EXISTS (SELECT 1 FROM assignments a
                                   WHERE a.derivation_id = d.derivation_id)
                 LIMIT $1
             )"
        ))
        .bind(limit)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Load poisoned derivations with their `poisoned_at` timestamps
    /// for recovery. Separate from `load_nonterminal_derivations`
    /// because `TERMINAL_STATUSES` includes `"poisoned"`.
    ///
    /// Rows with `poisoned_at IS NULL` are crash-window artifacts from
    /// the old two-call persist sequence (status set, timestamp not yet).
    /// `COALESCE(..., 0.0)` treats them as freshly poisoned (elapsed=0)
    /// — conservative: a slight TTL over-extension is harmless; omitting
    /// the row entirely (the old `IS NOT NULL` filter) caused spurious
    /// Succeeded on recovery. After `persist_poisoned` landed, new rows
    /// can never be in this state.
    ///
    /// Returns minimal fields — poisoned rows aren't dispatched, just
    /// TTL-tracked. The `failed_builders` count matters for display;
    /// `elapsed_secs` is `now() - poisoned_at` computed PG-side so
    /// the caller can convert `Instant::now() - Duration::from_secs(elapsed)`.
    pub async fn load_poisoned_derivations(
        &self,
    ) -> Result<Vec<PoisonedDerivationRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT derivation_id, drv_hash, drv_path, pname, system,
                   failed_builders, is_fixed_output,
                   COALESCE(
                       EXTRACT(EPOCH FROM (now() - poisoned_at))::float8,
                       0.0
                   ) AS elapsed_secs
            FROM derivations
            WHERE status = 'poisoned'
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }
}
