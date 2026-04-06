//! Per-derivation state + poison tracking — `derivations` table.

use super::{PoisonedDerivationRow, SchedulerDb};
use crate::state::{DerivationStatus, DrvHash, WorkerId};

impl SchedulerDb {
    /// Update a derivation's status.
    pub async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_worker: Option<&WorkerId>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            r#"
            UPDATE derivations
            SET status = $2, assigned_worker_id = $3, updated_at = now()
            WHERE drv_hash = $1
            "#,
            drv_hash.as_str(),
            status.as_str(),
            assigned_worker.map(WorkerId::as_str),
        )
        .execute(&self.pool)
        .await?;

        Ok(())
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

    /// Append a worker ID to a derivation's `failed_workers` array.
    ///
    /// Called from `handle_transient_failure` and `reassign_derivations`
    /// (worker disconnect mid-build) so recovery can rebuild the
    /// HashSet that feeds best_worker exclusion + poison detection.
    ///
    /// The `WHERE NOT ($2 = ANY(failed_workers))` guard makes this a
    /// no-op when the worker is already recorded. PG arrays are NOT
    /// sets — without the guard, a flapping worker (disconnect →
    /// reconnect → disconnect) would append duplicates unboundedly.
    /// Recovery builds a HashSet from this array so dupes would
    /// collapse in-mem, but the PG row itself grows forever. The
    /// guard keeps the array bounded to distinct-workers-ever-failed.
    pub async fn append_failed_worker(
        &self,
        drv_hash: &DrvHash,
        worker_id: &WorkerId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations \
             SET failed_workers = array_append(failed_workers, $2), updated_at = now() \
             WHERE drv_hash = $1 AND NOT ($2 = ANY(failed_workers))",
            drv_hash.as_str(),
            worker_id.as_str(),
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
    /// `assigned_worker_id` is NULLed: a poisoned derivation has no
    /// assignment. Matches the in-mem semantics the caller should enforce.
    pub async fn persist_poisoned(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations \
             SET status = 'poisoned', poisoned_at = now(), \
                 assigned_worker_id = NULL, updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Clear poison state: NULL `poisoned_at`, empty `failed_workers`,
    /// zero `retry_count`, status='created'. Used by ClearPoison admin
    /// RPC + TTL expiry in `handle_tick`.
    pub async fn clear_poison(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, failed_workers = '{}', retry_count = 0,
                 status = 'created', updated_at = now()
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
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
    /// TTL-tracked. The `failed_workers` count matters for display;
    /// `elapsed_secs` is `now() - poisoned_at` computed PG-side so
    /// the caller can convert `Instant::now() - Duration::from_secs(elapsed)`.
    pub async fn load_poisoned_derivations(
        &self,
    ) -> Result<Vec<PoisonedDerivationRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT derivation_id, drv_hash, drv_path, pname, system,
                   failed_workers,
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
