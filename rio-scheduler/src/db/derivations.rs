//! Per-derivation state + poison tracking — `derivations` table.

use sqlx::PgConnection;

use super::{
    AssignmentStatus, FencedWrite, PoisonedDerivationRow, SchedulerDb, terminal_status_sql,
};
use crate::state::{DerivationStatus, DrvHash, ExecutorId};

/// Map a terminal `DerivationStatus` to the `assignments.status` value
/// the active row should transition to. `None` for non-terminal
/// statuses (the assignment stays `pending` until re-dispatch
/// overwrites it via `insert_assignment`'s ON CONFLICT, or a later
/// terminal write closes it). The recovery join's
/// `assigned_builder_id IS NOT NULL` guard exists because of this
/// leaked-`pending` window — see `load_nonterminal_derivations`.
///
/// I-209/I-210: every terminal-status persist now closes the active
/// assignment row in the same call, so a new terminal-transition
/// callsite can't forget it. Before this, only
/// `handle_success_completion` did the assignment write — every
/// other path (poison, cancel, cache-hit-at-merge, orphan recovery,
/// FOD-from-store) left the row at `'pending'`, the pruner's
/// `NOT EXISTS assignments` never matched, and `derivations` leaked
/// (12,609 stuck rows on terminal derivations observed in production).
pub(super) fn terminal_assignment_status(drv_status: DerivationStatus) -> Option<AssignmentStatus> {
    use DerivationStatus::*;
    // Exhaustive — no `_` arm: a new variant is a compile error here,
    // so the I-209 leak this function guards against can't be silently
    // re-introduced. Belt-and-suspenders runtime check is in
    // `tests::transactions::test_terminal_statuses_match_is_terminal`.
    match drv_status {
        Completed => Some(AssignmentStatus::Completed),
        Poisoned | DependencyFailed => Some(AssignmentStatus::Failed),
        Cancelled | Skipped => Some(AssignmentStatus::Cancelled),
        Created | Queued | Ready | Assigned | Running | Substituting | Failed => None,
    }
}

impl SchedulerDb {
    /// Transaction-joining body of [`Self::update_derivation_status`]:
    /// the status UPDATE plus, for terminal statuses, the
    /// active-assignment close — on the caller's connection, so an
    /// appending site can put the status persist in the same
    /// transaction as its `drv_attempts` append (the 1a write
    /// discipline). Same `db/batch.rs` parameter shape
    /// (`&mut PgConnection`).
    // r[impl sched.db.assignment-terminal-on-status+2]
    pub(crate) async fn update_derivation_status_in_tx(
        tx: &mut PgConnection,
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
        .execute(&mut *tx)
        .await?;

        if let Some(assign_status) = terminal_assignment_status(status) {
            sqlx::query!(
                "UPDATE assignments
                 SET status = $2, completed_at = now()
                 WHERE derivation_id = (SELECT derivation_id FROM derivations WHERE drv_hash = $1)
                   AND status IN ('pending', 'acknowledged')",
                drv_hash.as_str(),
                assign_status.as_str(),
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(())
    }

    /// Update a derivation's status. If the new status is terminal,
    /// also closes the active `assignments` row (pending/acknowledged
    /// → mapped terminal status, `completed_at = now()`) **in the
    /// same transaction** so a crash between can't leave a permanent
    /// un-GC-able row (terminal derivation + pending assignment).
    ///
    /// Claims-floor fenced (`sched.evidence.durability`): a
    /// `serving_generation` below the durable floor rolls back having
    /// written nothing and returns [`FencedWrite::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `update_derivation_status_in_tx` instead (their fence lives at
    /// the transaction owner).
    // r[impl sched.evidence.durability]
    pub async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_executor: Option<&ExecutorId>,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        Self::update_derivation_status_in_tx(&mut tx, drv_hash, status, assigned_executor).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(0))
    }

    /// Transaction-joining body of
    /// [`Self::update_derivation_status_batch`] — same split as
    /// `update_derivation_status_in_tx`. Returns the number of
    /// derivation rows updated.
    // r[impl sched.db.assignment-terminal-on-status+2]
    pub(crate) async fn update_derivation_status_batch_in_tx(
        tx: &mut PgConnection,
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
        .execute(&mut *tx)
        .await?;

        if let Some(assign_status) = terminal_assignment_status(status) {
            sqlx::query!(
                "UPDATE assignments
                 SET status = $2, completed_at = now()
                 WHERE derivation_id IN
                       (SELECT derivation_id FROM derivations WHERE drv_hash = ANY($1::text[]))
                   AND status IN ('pending', 'acknowledged')",
                drv_hashes as &[&str],
                assign_status.as_str(),
            )
            .execute(&mut *tx)
            .await?;
        }
        Ok(result.rows_affected())
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
    /// Claims-floor fenced (`sched.evidence.durability`): a
    /// `serving_generation` below the durable floor rolls back having
    /// written nothing and returns [`FencedWrite::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `update_derivation_status_batch_in_tx` instead (their fence
    /// lives at the transaction owner).
    ///
    /// [`update_derivation_status`]: Self::update_derivation_status
    // r[impl sched.evidence.durability]
    pub async fn update_derivation_status_batch(
        &self,
        drv_hashes: &[&str],
        status: DerivationStatus,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(FencedWrite::Applied(0));
        }
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let updated =
            Self::update_derivation_status_batch_in_tx(&mut tx, drv_hashes, status).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(updated))
    }

    // r[impl sched.sla.reactive-floor+3]
    /// Persist a derivation's reactive `resource_floor` (D4, `M_044`).
    ///
    /// Called from `bump_floor_or_count` right after the in-mem
    /// doubling so a scheduler failover between OOM and retry doesn't
    /// reset the floor to zero → re-dispatch at probe defaults → OOM
    /// again. Write-at-mutation (NOT in `batch_upsert_derivations` —
    /// merge-time floor is always zero and `ON CONFLICT DO UPDATE`
    /// there would clobber a promoted floor on re-merge).
    pub async fn update_resource_floor(
        &self,
        drv_hash: &DrvHash,
        floor: &crate::state::ResourceFloor,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations SET \
               floor_mem_bytes = $2, \
               floor_disk_bytes = $3, \
               floor_deadline_secs = $4, \
               updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
            floor.mem_bytes as i64,
            floor.disk_bytes as i64,
            i64::from(floor.deadline_secs),
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    // r[impl sched.poison.ttl-persist]
    /// Transaction-joining body of [`Self::persist_poisoned`]: the
    /// atomic `status='poisoned'` + `poisoned_at=now()` write plus the
    /// active-assignment close, on the caller's connection — so a 1a
    /// appending site can carry the poison persist in the same
    /// transaction as its `drv_attempts` append.
    // r[impl sched.db.assignment-terminal-on-status+2]
    pub(crate) async fn persist_poisoned_in_tx(
        tx: &mut PgConnection,
        drv_hash: &DrvHash,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations \
             SET status = 'poisoned', poisoned_at = now(), \
                 assigned_builder_id = NULL, updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        sqlx::query!(
            "UPDATE assignments
             SET status = 'failed', completed_at = now()
             WHERE derivation_id = (SELECT derivation_id FROM derivations WHERE drv_hash = $1)
               AND status IN ('pending', 'acknowledged')",
            drv_hash.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

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
    ///
    /// Claims-floor fenced (`sched.evidence.durability`): a
    /// `serving_generation` below the durable floor rolls back having
    /// written nothing and returns [`FencedWrite::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `persist_poisoned_in_tx` instead (their fence lives at the
    /// transaction owner).
    // r[impl sched.evidence.durability]
    pub async fn persist_poisoned(
        &self,
        drv_hash: &DrvHash,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        Self::persist_poisoned_in_tx(&mut tx, drv_hash).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(0))
    }

    // r[impl sched.db.assignment-stale-sweep]
    /// Recovery backstop: close any `pending`/`acknowledged`
    /// `assignments` row whose derivation is already terminal.
    /// Mirrors [`Self::sweep_stale_live_pins`].
    ///
    /// The transactional chokepoint above (`update_derivation_status`,
    /// `update_derivation_status_batch`, `persist_poisoned`) makes the
    /// torn state structurally impossible going forward, but rows leaked
    /// by older binaries (pre-tx-wrap) are still permanently un-GC-able:
    /// `load_nonterminal_derivations` filters them out so
    /// `collect_orphaned_assignments` never sees them, and
    /// `gc_orphan_terminal_derivations`' `NOT EXISTS … pending|
    /// acknowledged` is forever false. This sweeps them on every
    /// recovery; it's also defense-in-depth if a future caller forgets
    /// the transaction discipline.
    pub async fn sweep_stale_assignments(&self) -> Result<u64, sqlx::Error> {
        // Compile-time splice of the terminal-status tuple — see
        // terminal_status_sql! for why it isn't a bind param.
        let result = sqlx::query(terminal_status_sql!(
            "UPDATE assignments \
             SET status = 'failed', completed_at = now() \
             WHERE status IN ('pending', 'acknowledged') \
               AND derivation_id IN \
                 (SELECT derivation_id FROM derivations \
                  WHERE status IN ",
            ")"
        ))
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    /// Transaction-joining body of [`Self::clear_poison`] — so a reset
    /// site can carry the poison clear in the same transaction as its
    /// `drv_attempts` reset row (the 1a write discipline).
    pub(crate) async fn clear_poison_in_tx(
        tx: &mut PgConnection,
        drv_hash: &DrvHash,
    ) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, status = 'created', updated_at = now()
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&mut *tx)
        .await
        .map(|_| ())
    }

    /// Clear poison state: NULL `poisoned_at`, status='created'. Used
    /// by ClearPoison admin RPC + TTL expiry in `handle_tick`. The
    /// retry/poison counters themselves are not derivations columns any
    /// more (migration 075 dropped the frozen mirror columns) — the
    /// budget reset is carried by the `poison_cleared` / `resubmit_reset`
    /// ledger row appended in the same transaction, which starts a fresh
    /// fold suffix.
    ///
    /// Claims-floor fenced (`sched.evidence.durability`): a
    /// `serving_generation` below the durable floor rolls back having
    /// written nothing and returns [`FencedWrite::Fenced`].
    ///
    /// Owns its connection; reset sites that already hold a transaction
    /// use `clear_poison_in_tx` instead (their fence lives at the
    /// transaction owner).
    // r[impl sched.evidence.durability]
    pub async fn clear_poison(
        &self,
        drv_hash: &DrvHash,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        Self::clear_poison_in_tx(&mut tx, drv_hash).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(0))
    }

    // r[impl sched.db.clear-poison-batch+3]
    /// Batch poison clear for the resubmit-reset path: one round-trip
    /// for N hashes via `WHERE drv_hash = ANY($1)`. Clears the
    /// poison-lifecycle state (`poisoned_at`, status) — the new cycle
    /// index that keeps the `r[sched.merge.poisoned-resubmit-bounded]`
    /// bound alive across a leader failover is carried by the
    /// `resubmit_reset` ledger row appended in the same transaction (the
    /// in-mem carry at `dag::merge` is the dispatch-time seed it
    /// stamps).
    ///
    /// I-169: `merge.rs`' resubmit-reset path called `clear_poison`
    /// per-hash inside the single-threaded actor — a 500-node resubmit
    /// blocked heartbeat/dispatch for 500 sequential PG round-trips.
    /// Same shape as [`update_derivation_status_batch`].
    ///
    /// Same column set as [`clear_poison`] since migration 075 dropped
    /// the frozen mirror columns; the two stay separate functions
    /// because the batch form is the hot-path vehicle and the scalar
    /// form owns its own connection.
    ///
    /// Claims-floor fenced (`sched.evidence.durability`): a
    /// `serving_generation` below the durable floor rolls back having
    /// written nothing and returns [`FencedWrite::Fenced`].
    ///
    /// [`clear_poison`]: Self::clear_poison
    /// [`update_derivation_status_batch`]: Self::update_derivation_status_batch
    // r[impl sched.evidence.durability]
    pub async fn clear_poison_batch(
        &self,
        drv_hashes: &[DrvHash],
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(FencedWrite::Applied(0));
        }
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let cleared = Self::clear_poison_batch_in_tx(&mut tx, drv_hashes).await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(cleared))
    }

    /// Transaction-joining body of [`Self::clear_poison_batch`] — the
    /// resubmit-reset site carries this clear in the same transaction
    /// as its `drv_attempts` reset rows.
    pub(crate) async fn clear_poison_batch_in_tx(
        tx: &mut PgConnection,
        drv_hashes: &[DrvHash],
    ) -> Result<u64, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(0);
        }
        let hashes: Vec<&str> = drv_hashes.iter().map(DrvHash::as_str).collect();
        let result = sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, status = 'created', updated_at = now()
             WHERE drv_hash = ANY($1::text[])",
            &hashes as &[&str],
        )
        .execute(&mut *tx)
        .await?;
        Ok(result.rows_affected())
    }

    // r[impl sched.db.derivations-gc+3]
    /// Delete up to `limit` orphan-terminal `derivations` rows: status
    /// is terminal AND no `build_derivations` link AND no `assignments`
    /// row. Returns rows deleted.
    ///
    /// I-169.2: 1.16M `dependency_failed` rows accumulated. Terminal
    /// rows are never re-read (recovery filters via
    /// `TERMINAL_STATUS_SQL`); once the owning build is deleted
    /// (008's `ON DELETE CASCADE` drops the `build_derivations` link)
    /// nothing references them. One caveat to "never re-read": the
    /// recovery-time `topdown_pruned` gate joins terminal CHILD rows
    /// through `derivation_edges` as produced-closure evidence — but a
    /// parent whose un-produced child this GC deletes carries the
    /// persisted `closure_hole` breadcrumb (`migrations/064`), which
    /// vetoes that gate, so the deletion can no longer launder a clear
    /// out of the surviving produced siblings. Subselect-LIMIT — PG has
    /// no `DELETE ... LIMIT` — so a 1M-row backlog drains over many
    /// ticks instead of one long table lock.
    ///
    /// `NOT EXISTS … pending|acknowledged`: an ACTIVE assignment row
    /// means the derivation may still be dispatched (assignment is the
    /// recovery source-of-truth). Terminal assignment rows
    /// (completed/failed/cancelled — closed by I-209's
    /// `terminal_assignment_status` fold) don't block: 034 changed the
    /// FK to ON DELETE CASCADE, so the DELETE removes them too. The
    /// pre-I-209 unconditional `NOT EXISTS assignments` blocked on ANY
    /// row, and only `handle_success_completion` ever closed one — so
    /// every poisoned/cancelled/cache-hit derivation leaked forever.
    ///
    /// `derivation_edges` rows referencing the deleted ids are removed
    /// in the same statement via the `del_edges` CTE. Migration 028
    /// dropped both FKs (no cascade), and re-submitted drv_hashes get
    /// fresh UUIDs after GC, so orphan edges are otherwise permanent.
    /// `load_edges_for_derivations` filters by `ANY(nonterminal_ids)`
    /// on both endpoints (orphans never loaded — correctness OK), but
    /// the table still grows unbounded at avg-fanout× the I-169.2
    /// churn rate (1.16M derivations) without the edge delete.
    pub async fn gc_orphan_terminal_derivations(&self, limit: i64) -> Result<u64, sqlx::Error> {
        // Compile-time splice of the terminal-status tuple — see
        // terminal_status_sql! for why it isn't a bind param.
        let result = sqlx::query(terminal_status_sql!(
            "WITH victims AS (
                 SELECT d.derivation_id FROM derivations d
                 WHERE d.status IN ",
            "
                   AND NOT EXISTS (SELECT 1 FROM build_derivations bd
                                   WHERE bd.derivation_id = d.derivation_id)
                   AND NOT EXISTS (SELECT 1 FROM assignments a
                                   WHERE a.derivation_id = d.derivation_id
                                     AND a.status IN ('pending', 'acknowledged'))
                 LIMIT $1
             ),
             del_edges AS (
                 DELETE FROM derivation_edges e
                 WHERE e.parent_id IN (SELECT derivation_id FROM victims)
                    OR e.child_id  IN (SELECT derivation_id FROM victims)
                 RETURNING 1
             )
             DELETE FROM derivations d USING victims v
             WHERE d.derivation_id = v.derivation_id"
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
    /// TTL-tracked + resubmit-bound checked (the exclusion set and the
    /// resubmit bound are rebuilt from the attempt-ledger fold after the
    /// suffix load). `elapsed_secs` is `now() - poisoned_at` computed
    /// PG-side so the caller can convert
    /// `Instant::now() - Duration::from_secs(elapsed)`.
    pub(crate) async fn load_poisoned_derivations(
        &self,
    ) -> Result<Vec<PoisonedDerivationRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT derivation_id, drv_hash, drv_path, pname, system,
                   is_fixed_output,
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

    /// Operator-facing poisoned listing for `AdminService.ListPoisoned`:
    /// `(drv_path, failed_executors, poisoned_secs_ago)` per poisoned
    /// derivation, with `failed_executors` aggregated from the attempt
    /// ledger (the legacy `failed_builders` column union retired with
    /// migration 075 — the ledger is the only failure-history record).
    /// Only the ledger classes whose fold arm charges the exclusion set
    /// (`transient`, `permanent`, `backstop`, `executor_crash`) are
    /// aggregated, so the displayed set keeps the as-built meaning of
    /// "executors whose failure charged this derivation" rather than
    /// every executor that ever touched it.
    pub(crate) async fn load_poisoned_display(
        &self,
    ) -> Result<Vec<(String, Vec<String>, f64)>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT d.drv_path,
                   COALESCE(
                       (SELECT array_agg(DISTINCT a.executor_id ORDER BY a.executor_id)
                        FROM drv_attempts a
                        WHERE a.derivation_id = d.derivation_id
                          AND a.executor_id IS NOT NULL
                          AND a.event_kind = 'attempt'
                          AND a.outcome_class IN
                              ('transient', 'permanent', 'backstop', 'executor_crash')),
                       '{}'::text[]
                   ) AS failed_executors,
                   COALESCE(
                       EXTRACT(EPOCH FROM (now() - d.poisoned_at))::float8,
                       0.0
                   ) AS poisoned_secs_ago
            FROM derivations d
            WHERE d.status = 'poisoned'
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }
}
