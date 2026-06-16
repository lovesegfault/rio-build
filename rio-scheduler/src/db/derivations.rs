//! Per-derivation state + poison tracking — `derivations` table.

use std::sync::LazyLock;

use sqlx::PgConnection;

use super::{
    AssignmentCloseStatus, FencedBegin, FencedOutcome, PoisonedDerivationRow, SchedulerDb,
    ServingGeneration, terminal_status_sql,
};
use crate::state::{DerivationStatus, DrvHash, ExecutorId};

/// PG-domain latch age (merged_bug_017): the precedence anchor of a
/// status-outbox replay, constructible ONLY from the batch's MONOTONIC
/// enqueue instant at the open replay transaction (merged_bug_004 —
/// see [`LatchAge::at_replay_boundary`]). There is deliberately NO
/// constructor from `SystemTime`/epoch floats — the replay's
/// precedence comparison must live entirely in the PG clock domain
/// (`status_changed_at <= now() - make_interval(secs => age)`, both
/// comparands PG-stamped), so binding ANY absolute timestamp (app
/// epoch OR a pg-read instant) against the PG-stamped comparand no
/// longer typechecks. This is
/// the repo's BACKSTOP_DUE_SQL discipline (rio-store/src/gc/state.rs:
/// durations cross the clock boundary; instants never do) made a
/// type. Residual error: ppm monotonic frequency drift over the
/// latch→flush window plus microseconds of latch-to-enqueue skew —
/// vs. the seconds-to-minutes of NTP epoch disagreement the old
/// `to_timestamp(pod_epoch)` conjunct silently ate (PG-ahead: a fresh
/// terminal latch zero-rowed and was popped as "flushed"; pod-ahead:
/// the stale-overwrite window on the DAG-absent terminal-KEEP arm
/// re-opened).
pub(crate) struct LatchAge(f64);

impl LatchAge {
    /// Boundary-witnessed constructor (merged_bug_004 hole 3): the
    /// latch's age, measured monotonically from the batch's enqueue
    /// instant AT THE REPLAY TRANSACTION BOUNDARY — demanding the
    /// open [`super::FencedTx`] makes pre-BEGIN sampling
    /// unrepresentable (the pre-fix age was sampled at argument
    /// construction, before `begin_fenced`'s pool-acquire/BEGIN, so
    /// the realized cut landed at enqueue+delta and admitted stale
    /// overwrites of rows advanced within delta).
    ///
    /// Envelope law (typed direction): PG `now()` froze at BEGIN ≤
    /// this sample instant, so the realized cut = enqueue − (sample −
    /// BEGIN) ≤ the ENQUEUE instant — the residual error is bounded
    /// by the awaits between BEGIN and this call and points in the
    /// refuse-never-overwrite direction. Recomputed from the
    /// immutable enqueue instant per flush attempt, so a re-pushed
    /// batch keeps its latch-pinned cut.
    fn at_replay_boundary(enqueued_at: std::time::Instant, _tx: &super::FencedTx) -> Self {
        Self(enqueued_at.elapsed().as_secs_f64())
    }

    /// The age in seconds — the `make_interval(secs => $n)` bind.
    fn secs(&self) -> f64 {
        self.0
    }
}

/// Outcome of one guarded status-batch replay (merged_bug_017): the
/// rows the precedence conjunct allowed are a NAMED set, so a
/// zero-row (or partial) guarded replay is a loud, attributable
/// outcome the flusher warns/counts on — never an indistinguishable
/// `Ok` count it does not consult. `FencedOutcome` stays untouched
/// for every other fenced writer.
#[derive(Debug)]
pub(crate) enum StatusReplay {
    /// Below the claims floor: nothing written; the caller re-pushes
    /// (a deposed replica writes nothing).
    Fenced,
    /// The replay transaction committed. `replayed` names the drvs
    /// whose rows the PG-domain precedence conjunct allowed;
    /// `residual` classifies every kept-but-not-replayed drv at the
    /// durability point (merged_bug_108) — the flusher consumes the
    /// alphabet exhaustively and pops the batch in every lane (the
    /// pop-as-final law is attribution-independent).
    Applied {
        replayed: Vec<String>,
        residual: Vec<(String, ReplayResidual)>,
    },
}

/// Why a kept drv was NOT replayed — the closed three-valued residual
/// alphabet (merged_bug_108): a two-valued applied/refused partition
/// forced three causally distinct zero-row outcomes (foreign
/// precedence, our own lost commit, a GC-vanished row) into one lying
/// refusal warn+counter, exactly during the PG brownouts the outbox
/// exists for. Classified row-evidence-pure in the SAME transaction
/// as the guarded UPDATE: absent row → `Vanished`; row at the latched
/// target → `AlreadyApplied` (the comparand's `status IS DISTINCT
/// FROM` guard left it un-stamped, merged_bug_004 — our own lost-ack
/// commit, or anyone having landed the same truth: either way not a
/// refusal); else → `RefusedNewer` (present with a DIFFERENT status —
/// evidenced foreign precedence, the only lane that may warn/count).
///
/// READ COMMITTED tolerance (derivation): the diagnostic SELECT is a
/// separate statement, so a cross-statement movement can land between
/// the UPDATE's snapshot and the SELECT's — every such movement still
/// lands in a TRUTHFUL lane (a row deleted after the UPDATE reads
/// Vanished, a row advanced to the target reads AlreadyApplied, any
/// other advance reads RefusedNewer): the classification describes
/// the durable world AT the durability point, never a stale shadow.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum ReplayResidual {
    /// The durable status already equals the latched truth — a
    /// lost-ack retry of our own landed commit, or an equivalent
    /// foreign write. Reconciled, not refused.
    AlreadyApplied,
    /// The row stands with a DIFFERENT status: evidenced foreign
    /// precedence. `stamp_newer_than_cut` is a consistency field —
    /// under the precedence conjunct it is true modulo PG clock
    /// steps; a false value is logged as an anomaly, never minted
    /// into a fourth lane.
    RefusedNewer { stamp_newer_than_cut: bool },
    /// No row: GC collected it before the replay (the orphan-GC tick
    /// runs ahead of the flush in the same housekeeping pass).
    /// Nothing stands; nothing to refuse.
    Vanished,
}

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
pub(super) fn terminal_assignment_status(
    drv_status: DerivationStatus,
) -> Option<AssignmentCloseStatus> {
    use DerivationStatus::*;
    // Exhaustive — no `_` arm: a new variant is a compile error here,
    // so the I-209 leak this function guards against can't be silently
    // re-introduced. Belt-and-suspenders runtime check is in
    // `tests::transactions::test_terminal_statuses_match_is_terminal`.
    match drv_status {
        Completed => Some(AssignmentCloseStatus::Completed),
        Poisoned | DependencyFailed => Some(AssignmentCloseStatus::Failed),
        Cancelled | Skipped => Some(AssignmentCloseStatus::Cancelled),
        Created | Queued | Ready | Assigned | Running | Failed => None,
    }
}

/// The close+stamp statement for the single-derivation closers (the
/// terminal-status persist at `update_derivation_status_in_tx` and the
/// poison close at `persist_poisoned_in_tx` share the selector; binds:
/// $1 drv_hash, $2 assignment status, $3 execution status).
static CLOSE_BY_DRV_HASH_SQL: LazyLock<String> = LazyLock::new(|| {
    super::close_assignments_sql(
        "derivation_id = (SELECT derivation_id FROM derivations WHERE drv_hash = $1)",
        2,
    )
});

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

        if let Some(close) = terminal_assignment_status(status) {
            sqlx::query_scalar::<_, i64>(CLOSE_BY_DRV_HASH_SQL.as_str())
                .bind(drv_hash.as_str())
                .bind(close.as_str())
                .bind(close.exec_status())
                .fetch_one(&mut *tx)
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
    /// written nothing and returns [`FencedOutcome::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `update_derivation_status_in_tx` instead (their fence lives at
    /// the transaction owner).
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn update_derivation_status(
        &self,
        drv_hash: &DrvHash,
        status: DerivationStatus,
        assigned_executor: Option<&ExecutorId>,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        Self::update_derivation_status_in_tx(tx.conn(), drv_hash, status, assigned_executor)
            .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(0))
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

        if let Some(close) = terminal_assignment_status(status) {
            static SQL: LazyLock<String> = LazyLock::new(|| {
                super::close_assignments_sql(
                    "derivation_id IN \
                     (SELECT derivation_id FROM derivations WHERE drv_hash = ANY($1::text[]))",
                    2,
                )
            });
            sqlx::query_scalar::<_, i64>(SQL.as_str())
                .bind(drv_hashes)
                .bind(close.as_str())
                .bind(close.exec_status())
                .fetch_one(&mut *tx)
                .await?;
        }
        Ok(result.rows_affected())
    }

    /// Outbox replay (merged_bug_011): re-drive a latched status batch
    /// with the assignment close scoped to the LATCHED exec_ids — the
    /// in-memory active attempts at persist-failure time — never the
    /// derivation. A successor attempt minted between latch and flush
    /// carries a different exec_id, so the close cannot match it by
    /// construction (the absolute writer's derivation-scoped close
    /// cancelled a resubmitted build's fresh `pending` row). The
    /// caller (the outbox flusher) re-derives the kept derivation set
    /// against the authoritative in-memory DAG first — this writer
    /// trusts that set for the derivation UPDATE and trusts ONLY the
    /// latched exec_ids for the close.
    ///
    /// The close is UNCONDITIONAL on the latched exec_ids (bug_158):
    /// even when re-derivation dropped EVERY drv (the kept set is
    /// empty because the world advanced past the latch), the latched
    /// attempts still ended at latch time and their rows must close —
    /// only the derivation-status UPDATE is kept-set-scoped. Pre-fix,
    /// the empty-drv early return discarded the close and the rows
    /// stayed open until they aged into ChargeExecutorCrash against a
    /// healthily-rebuilding derivation.
    ///
    /// Returns the [`StatusReplay`] named set (merged_bug_017) with
    /// every kept-but-not-replayed drv CLASSIFIED at the durability
    /// point (merged_bug_108, [`ReplayResidual`]): the caller surfaces
    /// each residual through its own lane — a zero-row replay is a
    /// loud, truthfully-attributed outcome, never an unconsulted
    /// count and never a blanket refusal.
    ///
    /// Runtime-bound (not `query!`): `cargo xtask regen sqlx`
    /// self-builds this crate (the same posture as the other fenced
    /// writers added since).
    ///
    /// Claims-floor fenced like every evidence writer.
    // r[impl sched.attempt.cancel-close-driven+3]
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn replay_status_batch_guarded(
        &self,
        drv_hashes: &[&str],
        status: DerivationStatus,
        latched_exec_ids: &[uuid::Uuid],
        enqueued_at: std::time::Instant,
        serving_generation: ServingGeneration,
    ) -> Result<StatusReplay, sqlx::Error> {
        if drv_hashes.is_empty() && latched_exec_ids.is_empty() {
            return Ok(StatusReplay::Applied {
                replayed: vec![],
                residual: vec![],
            });
        }
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(StatusReplay::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        // merged_bug_004 hole 3: the age is minted AFTER `begin_fenced`
        // returned the open transaction — PG `now()` froze at BEGIN ≤
        // the sample instant, so the realized cut can only TRAIL the
        // enqueue instant (refuse-never-overwrite; the constructor doc
        // carries the envelope law).
        let latch_age = LatchAge::at_replay_boundary(enqueued_at, &tx);
        let replayed: Vec<String> = if drv_hashes.is_empty() {
            vec![]
        } else {
            // merged_bug_025: the precedence conjunct — a row the
            // world advanced AFTER the latch (resubmitted drv:
            // Running with a newer status stamp) refuses the replay
            // row-locally, even when the in-memory re-derivation
            // could not see it. The timestamp form is PINNED over the
            // status-set form: the two diverge exactly on a newer
            // NON-terminal durable row, which the status-set guard
            // would have overwritten.
            //
            // merged_bug_004/merged_bug_006: the comparand is
            // `status_changed_at` — stamped exclusively by the
            // migration 102 BEFORE UPDATE trigger, WHEN the status
            // VALUE changes (no Rust SET list names it; the total-ban
            // census in db/tests/fence_coverage.rs) — so neither a
            // non-status write (the resource-floor ratchet, the
            // merge-parity upsert) nor a value-preserving status
            // write (same-status re-assignment, duplicate cancel,
            // clear_poison on an already-'created' row) can refuse a
            // latched terminal persist. The `status IS DISTINCT FROM
            // $2` conjunct below is row FILTERING (precedence law —
            // an already-at-target row is left to the residual lane),
            // not stamping; the stamp's meaning ("the instant the
            // status VALUE last changed") is exact by schema.
            //
            // merged_bug_017: BOTH comparands are PG-domain — the
            // latch crosses the clock boundary as a monotonic AGE
            // (`make_interval`), never as an absolute pod timestamp
            // (`to_timestamp(epoch_now())` silently ate NTP skew:
            // PG-ahead dropped fresh terminal latches as zero-row
            // "flushed" pops; pod-ahead re-opened the stale-overwrite
            // window this conjunct is the only defense against). The
            // BACKSTOP_DUE_SQL discipline. RETURNING names the
            // allowed rows so the caller can surface the refused set.
            sqlx::query_scalar::<_, String>(
                "UPDATE derivations \
                 SET status = $2, assigned_builder_id = NULL, updated_at = now() \
                 WHERE drv_hash = ANY($1::text[]) \
                   AND status_changed_at <= now() - make_interval(secs => $3) \
                   AND status IS DISTINCT FROM $2 \
                 RETURNING drv_hash",
            )
            .bind(drv_hashes)
            .bind(status.as_str())
            .bind(latch_age.secs())
            .fetch_all(tx.conn())
            .await?
        };
        // merged_bug_108: classify the residual (kept − replayed) at
        // the durability point, in the SAME transaction as the UPDATE.
        // The SELECT is structurally guarded on a non-empty residual —
        // the healthy path pays ZERO extra round trips (the guard is
        // the cost envelope's testable form).
        let residual_keys: Vec<&str> = drv_hashes
            .iter()
            .copied()
            .filter(|h| !replayed.iter().any(|r| r == h))
            .collect();
        let residual: Vec<(String, ReplayResidual)> = if residual_keys.is_empty() {
            vec![]
        } else {
            let rows: Vec<(String, String, bool)> = sqlx::query_as(
                "SELECT drv_hash, status, \
                        status_changed_at > (now() - make_interval(secs => $2)) AS newer \
                 FROM derivations WHERE drv_hash = ANY($1::text[])",
            )
            .bind(&residual_keys)
            .bind(latch_age.secs())
            .fetch_all(tx.conn())
            .await?;
            residual_keys
                .iter()
                .map(|h| {
                    let kind = match rows.iter().find(|(rh, _, _)| rh == h) {
                        None => ReplayResidual::Vanished,
                        Some((_, durable, _)) if durable == status.as_str() => {
                            ReplayResidual::AlreadyApplied
                        }
                        Some((_, _, newer)) => ReplayResidual::RefusedNewer {
                            stamp_newer_than_cut: *newer,
                        },
                    };
                    (h.to_string(), kind)
                })
                .collect()
        };
        if let Some(close) = terminal_assignment_status(status)
            && !latched_exec_ids.is_empty()
        {
            static SQL: LazyLock<String> =
                LazyLock::new(|| super::close_assignments_sql("exec_id = ANY($1::uuid[])", 2));
            sqlx::query_scalar::<_, i64>(SQL.as_str())
                .bind(latched_exec_ids)
                .bind(close.as_str())
                .bind(close.exec_status())
                .fetch_one(tx.conn())
                .await?;
        }
        tx.commit().await?;
        Ok(StatusReplay::Applied { replayed, residual })
    }

    /// Batch variant of [`update_derivation_status`]: set the same
    /// status on many derivations in one round-trip.
    ///
    /// FRESH-WRITE ONLY (merged_bug_011): the absolute UPDATE and the
    /// derivation-scoped assignment close are sound exactly when the
    /// write happens AT the in-memory transition (the caller just
    /// made this status the node's truth). Delayed re-drives go
    /// through [`Self::replay_status_batch_guarded`] — replaying an
    /// absolute batch after the world moved regresses newer rows and
    /// closes successor attempts. The policy census in
    /// `db/tests/fence_coverage.rs` pins the production caller set to
    /// `persist_status_batch` alone.
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
    /// written nothing and returns [`FencedOutcome::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `update_derivation_status_batch_in_tx` instead (their fence
    /// lives at the transaction owner).
    ///
    /// [`update_derivation_status`]: Self::update_derivation_status
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn update_derivation_status_batch(
        &self,
        drv_hashes: &[&str],
        status: DerivationStatus,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(FencedOutcome::Applied(0));
        }
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let updated =
            Self::update_derivation_status_batch_in_tx(tx.conn(), drv_hashes, status).await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(updated))
    }

    // r[impl sched.sla.reactive-floor+5]
    // r[impl sched.evidence.durability+4]
    /// Persist a derivation's reactive `resource_floor` (D4, `M_044`),
    /// fenced and server-side monotone.
    ///
    /// Called from `bump_floor_or_count` right after the in-mem
    /// doubling so a scheduler failover between OOM and retry doesn't
    /// reset the floor to zero → re-dispatch at probe defaults → OOM
    /// again. Write-at-mutation (NOT in `batch_upsert_derivations` —
    /// merge-time floor is always zero and `ON CONFLICT DO UPDATE`
    /// there would clobber a promoted floor on re-merge).
    ///
    /// Two independent guards:
    /// - the claims-floor fence (this was the ONE unfenced
    ///   derivations-table decision writer: a deposed replica's late
    ///   OOM report could regress a successor's promoted floor);
    /// - per-dimension `GREATEST()` ratchets — production floors are
    ///   ratchet-up-only (the doubling ladder), so a same-tenure stale
    ///   in-memory base can never lower a dimension the fence cannot
    ///   see (the debug force-floor path is in-memory only and never
    ///   reaches this writer).
    pub(crate) async fn update_resource_floor(
        &self,
        drv_hash: &DrvHash,
        floor: &crate::state::ResourceFloor,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        // Runtime-bound (not `query!`): the GREATEST ratchet form is
        // new and `cargo xtask regen sqlx` itself builds this crate —
        // the same runtime form every fenced writer in db/mod.rs and
        // db/open_attempts.rs uses.
        let updated = sqlx::query(
            "UPDATE derivations SET \
               floor_mem_bytes = GREATEST(floor_mem_bytes, $2), \
               floor_disk_bytes = GREATEST(floor_disk_bytes, $3), \
               floor_deadline_secs = GREATEST(floor_deadline_secs, $4), \
               floor_cores = GREATEST(floor_cores, $5), \
               updated_at = now() \
             WHERE drv_hash = $1",
        )
        .bind(drv_hash.as_str())
        .bind(floor.mem_bytes as i64)
        .bind(floor.disk_bytes as i64)
        .bind(i64::from(floor.deadline_secs))
        .bind(i64::from(floor.cores))
        .execute(&mut *tx.conn())
        .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(updated.rows_affected()))
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
        sqlx::query_scalar::<_, i64>(CLOSE_BY_DRV_HASH_SQL.as_str())
            .bind(drv_hash.as_str())
            .bind(AssignmentCloseStatus::Failed.as_str())
            .bind(AssignmentCloseStatus::Failed.exec_status())
            .fetch_one(&mut *tx)
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
    /// written nothing and returns [`FencedOutcome::Fenced`].
    ///
    /// Owns its transaction; appending sites that already hold one use
    /// `persist_poisoned_in_tx` instead (their fence lives at the
    /// transaction owner).
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn persist_poisoned(
        &self,
        drv_hash: &DrvHash,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        Self::persist_poisoned_in_tx(tx.conn(), drv_hash).await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(0))
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
    /// Claims-floor fenced: the repair sweep runs at recovery with the
    /// new tenure's generation — a deposed replica's concurrent sweep
    /// writes nothing.
    pub(crate) async fn sweep_stale_assignments(
        &self,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        // Compile-time splice of the terminal-status tuple — see
        // terminal_status_sql! for why it isn't a bind param.
        static SQL: LazyLock<String> = LazyLock::new(|| {
            super::close_assignments_sql(
                terminal_status_sql!(
                    "derivation_id IN \
                     (SELECT derivation_id FROM derivations \
                      WHERE status IN ",
                    ")"
                ),
                1,
            )
        });
        let closed: i64 = sqlx::query_scalar(SQL.as_str())
            .bind(AssignmentCloseStatus::Failed.as_str())
            .bind(AssignmentCloseStatus::Failed.exec_status())
            .fetch_one(tx.conn())
            .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(
            u64::try_from(closed).expect("count(*) is non-negative"),
        ))
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
    /// written nothing and returns [`FencedOutcome::Fenced`].
    ///
    /// Owns its connection; reset sites that already hold a transaction
    /// use `clear_poison_in_tx` instead (their fence lives at the
    /// transaction owner).
    // r[impl sched.evidence.durability+4]
    pub(crate) async fn clear_poison(
        &self,
        drv_hash: &DrvHash,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        Self::clear_poison_in_tx(tx.conn(), drv_hash).await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(0))
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
    /// written nothing and returns [`FencedOutcome::Fenced`].
    ///
    /// [`clear_poison`]: Self::clear_poison
    /// [`update_derivation_status_batch`]: Self::update_derivation_status_batch
    // r[impl sched.evidence.durability+4]
    /// Test-battery twin (merged_bug_284 sweep): the production batch
    /// clear is the in-tx form (`clear_poison_batch_in_tx`,
    /// completion.rs resubmit path); this pool-direct twin seeds and
    /// pins the db/tests/derivations.rs battery.
    #[cfg(test)]
    pub(crate) async fn clear_poison_batch(
        &self,
        drv_hashes: &[DrvHash],
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        if drv_hashes.is_empty() {
            return Ok(FencedOutcome::Applied(0));
        }
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let cleared = Self::clear_poison_batch_in_tx(tx.conn(), drv_hashes).await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(cleared))
    }

    /// Transaction-joining body of `Self::clear_poison_batch` — the
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

    // r[impl sched.db.derivations-gc+4]
    /// Delete up to `limit` orphan-terminal `derivations` rows: status
    /// is terminal AND no `build_derivations` link AND no `assignments`
    /// row. Returns rows deleted.
    ///
    /// I-169.2: 1.16M `dependency_failed` rows accumulated. Terminal
    /// rows are never re-read (recovery filters via
    /// `TERMINAL_STATUS_SQL`); once the owning build is deleted
    /// (008's `ON DELETE CASCADE` drops the `build_derivations` link)
    /// nothing references them. One caveat to "never re-read": the
    /// durable closure-evidence classifier
    /// (`classify_durable_evidence`) joins terminal CHILD rows through
    /// `derivation_edges` as produced-closure evidence — its strict
    /// criterion requires a LIVE co-owning build voucher per child, so
    /// rows this GC deletes (orphans by definition: no
    /// build_derivations link at all) were never voucher-bearing
    /// evidence; truncation here only moves a node toward the
    /// conservative Broken/childless cells. Subselect-LIMIT — PG has
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
    pub(crate) async fn gc_orphan_terminal_derivations(
        &self,
        limit: i64,
    ) -> Result<u64, sqlx::Error> {
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
                              ('transient', 'permanent', 'executor_variant',
                               'backstop', 'executor_crash')),
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
