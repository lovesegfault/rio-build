//! Per-derivation state + poison tracking — `derivations` table.

use super::{
    AssignmentStatus, PoisonedDerivationRow, SchedulerDb, SettledIdentityRow, terminal_status_sql,
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
    /// Update a derivation's status. If the new status is terminal,
    /// also closes the active `assignments` row (pending/acknowledged
    /// → mapped terminal status, `completed_at = now()`) **in the
    /// same transaction** so a crash between can't leave a permanent
    /// un-GC-able row (terminal derivation + pending assignment).
    // r[impl sched.db.assignment-terminal-on-status+2]
    pub async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_executor: Option<&ExecutorId>,
    ) -> Result<(), sqlx::Error> {
        let mut tx = self.pool.begin().await?;
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

        tx.commit().await
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
        let mut tx = self.pool.begin().await?;
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

        // r[impl sched.db.assignment-terminal-on-status+2]
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

        tx.commit().await?;
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

    // r[impl sched.sla.reactive-floor+2]
    /// Persist a derivation's reactive `resource_floor` (D4, `M_044`).
    ///
    /// Called from `bump_floor_or_count` right after the in-mem
    /// doubling so a scheduler failover between OOM and retry doesn't
    /// reset the floor to zero → re-dispatch at probe defaults → OOM
    /// again. Same write-at-mutation pattern as `append_failed_worker`
    /// above (NOT in `batch_upsert_derivations` — merge-time floor is
    /// always zero and `ON CONFLICT DO UPDATE` there would clobber a
    /// promoted floor on re-merge).
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
        let mut tx = self.pool.begin().await?;
        sqlx::query!(
            "UPDATE derivations \
             SET status = 'poisoned', poisoned_at = now(), \
                 assigned_builder_id = NULL, updated_at = now() \
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        // r[impl sched.db.assignment-terminal-on-status+2]
        sqlx::query!(
            "UPDATE assignments
             SET status = 'failed', completed_at = now()
             WHERE derivation_id = (SELECT derivation_id FROM derivations WHERE drv_hash = $1)
               AND status IN ('pending', 'acknowledged')",
            drv_hash.as_str(),
        )
        .execute(&mut *tx)
        .await?;
        tx.commit().await
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

    /// Clear poison state: NULL `poisoned_at`, empty `failed_builders`,
    /// zero `retry_count`, zero `resubmit_cycles`, status='created'. Used
    /// by ClearPoison admin RPC + TTL expiry in `handle_tick` — admin
    /// override and TTL expiry are full resets (unlike
    /// [`Self::clear_poison_batch`], which preserves+increments
    /// `resubmit_cycles` for the resubmit-bound).
    ///
    /// Deliberately FLOOR-PRESERVING: the reactive resource floors
    /// (`floor_mem_bytes`/`floor_disk_bytes`/`floor_deadline_secs`) are
    /// same-definition sizing memory (M_044) and survive an admin/TTL
    /// poison clear. A *definition change* (displacement, or a
    /// store-backed re-creation taking over an authoritative claim) is
    /// handled by the recreate-refresh upsert's in-tx definition-change
    /// reset (plus [`Self::reset_displaced_derivation`] as
    /// belt-and-suspenders), which also zeroes the floors.
    pub async fn clear_poison(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, failed_builders = '{}', retry_count = 0,
                 resubmit_cycles = 0, status = 'created', updated_at = now()
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    // r[impl sched.db.clear-poison-batch]
    /// Batch poison clear for the resubmit-reset path: one round-trip
    /// for N hashes via `WHERE drv_hash = ANY($1)`. Clears per-cycle
    /// state (`poisoned_at`, `failed_builders`, `retry_count`) AND
    /// increments `resubmit_cycles` so the
    /// `r[sched.merge.poisoned-resubmit-bounded]` bound survives leader
    /// failover. The in-mem increment at `dag::merge` is the
    /// dispatch-time source of truth; PG mirrors it here.
    ///
    /// I-169: `merge.rs`' resubmit-reset path called `clear_poison`
    /// per-hash inside the single-threaded actor — a 500-node resubmit
    /// blocked heartbeat/dispatch for 500 sequential PG round-trips.
    /// Same shape as [`update_derivation_status_batch`].
    ///
    /// NOT the same column set as [`clear_poison`]: that one zeroes
    /// `resubmit_cycles` (admin/TTL → full reset); this one increments
    /// it (resubmit → bound accumulates).
    ///
    /// Scope: SAME-definition resubmit-resets only. Authority takeovers
    /// (`MergeResult::authority_takeovers`) are excluded by the caller —
    /// their rows already got the full definition-change reset inside the
    /// recreate-refresh upsert, and incrementing `resubmit_cycles` here
    /// would diverge from the fresh in-memory budget.
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
                 resubmit_cycles = resubmit_cycles + 1,
                 status = 'created', updated_at = now()
             WHERE drv_hash = ANY($1::text[])",
            &hashes as &[&str],
        )
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }

    // r[impl sched.merge.displaced-failure-reset+2]
    /// Full failure-history reset for a DISPLACED derivation row
    /// (`sched.merge.authoritative-conflict`): the displacing submission
    /// is a different derivation definition, so nothing learned from the
    /// displaced definition's failures may carry over. One statement so
    /// there is no partial state where poison is cleared but the floors
    /// survive: NULL `poisoned_at`, empty `failed_builders`, zero
    /// `retry_count`/`resubmit_cycles`, status='created', AND zero the
    /// reactive resource floors (`floor_*`) that
    /// [`Self::clear_poison`]/[`Self::clear_poison_batch`] deliberately
    /// preserve for same-definition resets.
    ///
    /// Since the definition-change reset moved into
    /// `batch_upsert_derivations`' ON CONFLICT (riding the merge
    /// transaction), this post-commit per-hash call is redundant
    /// belt-and-suspenders for displaced rows whose persisted content had
    /// drifted from the displaced in-memory definition; the in-tx reset
    /// is the primary enforcement.
    pub async fn reset_displaced_derivation(&self, drv_hash: &DrvHash) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "UPDATE derivations
             SET poisoned_at = NULL, failed_builders = '{}', retry_count = 0,
                 resubmit_cycles = 0, status = 'created',
                 floor_mem_bytes = 0, floor_disk_bytes = 0,
                 floor_deadline_secs = 0, updated_at = now()
             WHERE drv_hash = $1",
            drv_hash.as_str(),
        )
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    // r[impl sched.db.derivations-gc+2]
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
                 SELECT d.derivation_id, d.drv_hash FROM derivations d
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
             ),
             del_closure_missing AS (
                 DELETE FROM derivation_closure_missing m
                 WHERE m.drv_hash IN (SELECT drv_hash FROM victims)
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
    /// Returns the full creation-time snapshot (the same column set as
    /// `load_nonterminal_derivations`): the merge gate
    /// (`sched.merge.authoritative-conflict`) keys on the recovered
    /// node's authoritative content and verifiable identity, so a
    /// poisoned row recovered as an empty stub would silently disable
    /// the gate after failover. Poisoned executions are already
    /// finalized by `persist_poisoned`, so `exec_id` /
    /// `assigned_builder_id` are selected as NULL. `elapsed_secs` is
    /// `now() - poisoned_at` computed PG-side so the caller can convert
    /// `Instant::now() - Duration::from_secs(elapsed)`.
    pub(crate) async fn load_poisoned_derivations(
        &self,
    ) -> Result<Vec<PoisonedDerivationRow>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT derivation_id, drv_hash, drv_path, pname, system, status,
                   required_features,
                   NULL::text AS assigned_builder_id,
                   retry_count, resubmit_cycles,
                   expected_output_paths, output_names, wanted_output_names,
                   is_fixed_output,
                   is_ca, topdown_pruned, closure_hole,
                   failed_builders,
                   floor_mem_bytes, floor_disk_bytes, floor_deadline_secs,
                   drv_content, ca_modular_hash, evidence_rank,
                   NULL::uuid AS exec_id,
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

    /// Load the identity columns of SETTLED (`completed`/`skipped`)
    /// derivation rows for the given hashes.
    ///
    /// r[impl sched.persist.settled-identity-freeze+1]
    /// A settled row is the durable record of a successful build whose
    /// DAG node may already have been reaped (terminal cleanup); the
    /// merge gate cannot protect it (no resident node), so the
    /// pre-merge settled-identity check loads these rows and rejects
    /// conflicting re-creations before any DAG or DB mutation happens.
    /// Indexed by the `derivations.drv_hash` unique constraint — one
    /// batched lookup per merge that contains non-resident hashes.
    pub(crate) async fn load_settled_identity_rows(
        &self,
        hashes: &[String],
    ) -> Result<Vec<SettledIdentityRow>, sqlx::Error> {
        if hashes.is_empty() {
            return Ok(Vec::new());
        }
        sqlx::query_as(
            r#"
            SELECT drv_hash, drv_path, system, output_names,
                   expected_output_paths, is_fixed_output, is_ca,
                   ca_modular_hash, evidence_rank
            FROM derivations
            WHERE drv_hash = ANY($1) AND status IN ('completed', 'skipped')
            "#,
        )
        .bind(hashes)
        .fetch_all(&self.pool)
        .await
    }

    // r[impl sched.derivation.evidence-rank]
    /// Persist a definition-evidence rank UPGRADE outside the
    /// creation-scoped upsert: the settle chokepoint
    /// (`PathBoundBytes` → `VerifiedBuilt` in
    /// `DerivationState::transition`) and the dispatch-time claims
    /// derivation (`UnverifiedClaim` → `PathBoundBytes`). Runtime
    /// query (NOT `sqlx::query!`): keeps `M_067` out of the offline
    /// `.sqlx` cache so the migration and its writers land in one
    /// commit. Best-effort at every call site — a lost write degrades
    /// to the persisted ingress rank at recovery, which never weakens
    /// a victim (no displacer outranks `path_bound_bytes`).
    pub(crate) async fn persist_evidence_rank(
        &self,
        drv_hash: &str,
        rank: crate::state::DefinitionEvidence,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations SET evidence_rank = $2, updated_at = now() WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .bind(rank.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Persist a stripped-claim verification: the rank rises on the
    /// verified bytes AND the unverifiable declared modular hash is
    /// cleared in the same statement (`sched.dispatch.claims-derived+2`
    /// — an unverifiable claim is NO claim, never persisted; exact
    /// ingress-strip parity, `ingress-inline-drv-binding+1`). One
    /// statement so a failover between the two writes cannot leave the
    /// raised rank with the unverified hash still attached.
    pub(crate) async fn persist_evidence_rank_and_clear_modular_hash(
        &self,
        drv_hash: &str,
        rank: crate::state::DefinitionEvidence,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations SET evidence_rank = $2, ca_modular_hash = NULL, \
             updated_at = now() WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .bind(rank.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }
}
