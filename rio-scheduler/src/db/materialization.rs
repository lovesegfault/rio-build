//! Materialization-job persistence (design §6 / §2.4, migration 078
//! `materialization_jobs`). Every standalone write is claims-floor
//! fenced; resolution is exec_id-keyed and at-most-once.
//!
//! Two creation layers (the db/wanted.rs pattern, adjudication PDQ-9):
//!   - The core, [`SchedulerDb::create_materialization_jobs_in_tx`],
//!     runs inside the caller's transaction (the merge tx), which
//!     already carries the claims-floor fence — no second floor read
//!     (one fence per transaction, the merge-tx discipline from
//!     Phase-1 Wave 3). A rolled-back caller transaction creates no
//!     rows (B6/A13).
//!   - The standalone wrapper,
//!     [`SchedulerDb::create_materialization_job_fenced`], serves
//!     non-merge callers (the dispatch-probe partition, tests): it
//!     opens its own tx, performs the floor check via the canonical
//!     `SchedulerDb::claims_floor` /
//!     `SchedulerDb::at_or_above_floor` helpers, and calls the
//!     in-tx core.
// r[impl sched.materialize.job+2]

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;
use uuid::Uuid;

use super::{FencedBegin, FencedOutcome, SchedulerDb, ServingGeneration};
use crate::db::attempts::AttemptRow;
use crate::state::{JobOrigin, JobState};

/// One materialization-job row, as the store poll and the consumption
/// transaction read it. Carries exactly the fields the descriptor
/// consumer reads (merged_bug_284 dead-code sweep trimmed the five
/// load-only columns); the raw row still SELECTs and PARSES the full
/// vocabulary — `try_from` is the CHECK-drift tripwire and validates
/// `state` even though no production reader keeps it.
#[derive(Debug, Clone)]
pub(crate) struct MaterializationJobRow {
    pub job_id: Uuid,
    pub drv_hash: String,
    pub tenant_id: Option<Uuid>,
    pub origin: JobOrigin,
}

/// Raw FromRow shape for the claimable-list query: TEXT enums come back
/// as `String` and are parsed into the typed vocabulary in
/// [`MaterializationJobRow::try_from`] — the CHECK constraints make a
/// parse failure a schema/code drift bug, surfaced as a decode error
/// rather than silently skipped (the `RawAttemptRow` pattern).
#[derive(Debug, sqlx::FromRow)]
struct RawJobRow {
    job_id: Uuid,
    drv_hash: String,
    tenant_id: Option<Uuid>,
    origin: String,
    state: String,
}

impl TryFrom<RawJobRow> for MaterializationJobRow {
    type Error = sqlx::Error;

    fn try_from(raw: RawJobRow) -> Result<Self, sqlx::Error> {
        let parse_err = |col: &str, val: &str| {
            sqlx::Error::Decode(
                format!("materialization_jobs.{col}: value {val:?} not in the rust-side alphabet")
                    .into(),
            )
        };
        // Vocabulary validation for the dropped-by-the-consumer column:
        // a `state` outside the rust-side alphabet is schema/code drift
        // and must surface as a decode error, not load silently.
        let _: JobState = raw
            .state
            .parse()
            .map_err(|_| parse_err("state", &raw.state))?;
        Ok(Self {
            job_id: raw.job_id,
            tenant_id: raw.tenant_id,
            origin: raw
                .origin
                .parse()
                .map_err(|_| parse_err("origin", &raw.origin))?,
            drv_hash: raw.drv_hash,
        })
    }
}

/// One job to create (the in-tx batch input).
pub(crate) struct NewJobRow<'a> {
    pub derivation_id: Uuid,
    pub drv_hash: &'a str,
    pub tenant_id: Option<Uuid>,
    pub origin: JobOrigin,
    /// Realized-path carrier (migration 082) — the floating-CA paths
    /// the stale-Completed reset destroyed in memory. `Some` ONLY for
    /// the `stale_reset` origin (a creation-time snapshot of immutable
    /// content-addressed paths; the wanted NAME set stays live).
    /// Set-if-null on the dedup arm: an existing pending row gains the
    /// carrier (it executes post-reset and hits the same empty-wanted
    /// hole), but a present carrier is never overwritten.
    pub carried_realized_paths: Option<&'a [String]>,
}

/// Per-input outcome of the in-tx batch creation.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) struct JobCreateResult {
    pub job_id: Uuid,
    /// False when an unresolved job already existed (the
    /// partial-unique-index dedup found it).
    pub created: bool,
    /// The dedup found an existing NON-pruned pending row and this
    /// batch's pruned input upgraded its origin to `'pruned'`
    /// (pruned-wins, PD-D1). Always false when `created` is true.
    pub upgraded: bool,
}

/// Outcome of the standalone fenced job creation: the unresolved job
/// for the derivation (created or found by the dedup), or the fence
/// refused the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FencedJobCreate {
    /// The write applied. `created` is false when an unresolved job
    /// already existed (the partial-unique-index dedup found it);
    /// `upgraded` reports the PD-D1 pruned-wins origin upgrade.
    Applied {
        job_id: Uuid,
        created: bool,
        upgraded: bool,
    },
    /// The serving generation is below the claims floor: the
    /// transaction rolled back having written nothing.
    Fenced,
}

/// Outcome of the batched zero-interest cancel sweep
/// ([`SchedulerDb::cancel_jobs_and_close_attempts_fenced`]): the
/// whole sweep is ONE fenced transaction, so the fence verdict is
/// sweep-level while application is reported per job.
#[derive(Debug, PartialEq, Eq)]
pub(crate) enum FencedCancelSweep {
    /// Committed. `cancelled` holds exactly the job_ids whose row
    /// flipped pending → cancelled; a member absent from the set was
    /// already terminal (idempotent re-entry — its attempt close
    /// still ran). The caller folds this into per-job
    /// [`rio_evidence_kernel::settle::WriteDisposition`]s for the
    /// settled-gated view removal.
    Applied { cancelled: HashSet<Uuid> },
    /// The serving generation is below the claims floor: the
    /// transaction rolled back having written nothing — for ANY
    /// member of the sweep.
    Fenced,
}

impl SchedulerDb {
    /// THE creation core (adjudication PDQ-9 / design §2.1 rows 1–2,
    /// A13/B6): create (or find existing) unresolved jobs for a batch
    /// of derivations, inside the caller's transaction. The caller's
    /// tx carries the merge fence; this helper adds no second floor
    /// read. Batch UNNEST INSERT + ON CONFLICT dedup (the partial-
    /// unique index); returns one [`JobCreateResult`] per input row in
    /// input order.
    ///
    /// **Pruned-wins origin upgrade (PD-D1, T-D2.1):** a pruned input
    /// whose creation dedups onto an existing non-pruned pending row
    /// upgrades that row's origin to `'pruned'` in a follow-up UPDATE
    /// inside the same transaction (the durable mark must not be lost
    /// to the dedup order — design §4/A2/A13). The upgrade is monotone
    /// (`origin <> 'pruned'` guard; never downgraded) and reserved to
    /// the pruned origin. The follow-up-UPDATE form (rather than
    /// `ON CONFLICT … DO UPDATE`) keeps the INSERT's RETURNING an
    /// inserts-only set, so `created` discrimination is untouched
    /// (an upgrade is NOT a creation).
    pub(crate) async fn create_materialization_jobs_in_tx(
        tx: &mut PgConnection,
        rows: &[NewJobRow<'_>],
        created_generation: i64,
    ) -> Result<Vec<JobCreateResult>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(Vec::new());
        }
        // Mint job ids in Rust (UUIDv7 — time-ordered, like exec ids).
        let minted: Vec<Uuid> = rows.iter().map(|_| Uuid::now_v7()).collect();
        let drv_ids: Vec<Uuid> = rows.iter().map(|r| r.derivation_id).collect();
        let drv_hashes: Vec<String> = rows.iter().map(|r| r.drv_hash.to_string()).collect();
        let tenant_ids: Vec<Option<Uuid>> = rows.iter().map(|r| r.tenant_id).collect();
        let origins: Vec<String> = rows.iter().map(|r| r.origin.as_str().to_string()).collect();

        // Batch UNNEST INSERT; the materialization_jobs_unresolved
        // partial-unique index dedups (at most one pending job per
        // derivation). RETURNING reports which rows actually inserted.
        let inserted: Vec<(Uuid,)> = sqlx::query_as(
            "INSERT INTO materialization_jobs \
                 (job_id, derivation_id, drv_hash, tenant_id, origin, created_generation) \
             SELECT j, d, h, t, o, $6 \
               FROM UNNEST($1::uuid[], $2::uuid[], $3::text[], $4::uuid[], $5::text[]) \
                    AS u(j, d, h, t, o) \
             ON CONFLICT (derivation_id) WHERE state = 'pending' DO NOTHING \
             RETURNING job_id",
        )
        .bind(&minted)
        .bind(&drv_ids)
        .bind(&drv_hashes)
        .bind(&tenant_ids)
        .bind(&origins)
        .bind(created_generation)
        .fetch_all(&mut *tx)
        .await?;
        let inserted: HashSet<Uuid> = inserted.into_iter().map(|(id,)| id).collect();

        // The authoritative pending job per derivation — covers both the
        // rows just inserted and the pre-existing ones the dedup found.
        let pending: Vec<(Uuid, Uuid)> = sqlx::query_as(
            "SELECT derivation_id, job_id FROM materialization_jobs \
              WHERE derivation_id = ANY($1::uuid[]) AND state = 'pending'",
        )
        .bind(&drv_ids)
        .fetch_all(&mut *tx)
        .await?;
        let by_drv: HashMap<Uuid, Uuid> = pending.into_iter().collect();

        // Pruned-wins upgrade (PD-D1): pruned inputs whose row was
        // dedup-found (not inserted) upgrade the existing pending row.
        // The `origin <> 'pruned'` guard makes the statement idempotent
        // and monotone; RETURNING reports the rows actually upgraded.
        let pruned_dedup_losers: Vec<Uuid> = rows
            .iter()
            .filter(|r| matches!(r.origin, JobOrigin::Pruned))
            .filter_map(|r| by_drv.get(&r.derivation_id).copied())
            .filter(|job_id| !inserted.contains(job_id))
            .collect();
        let upgraded: HashSet<Uuid> = if pruned_dedup_losers.is_empty() {
            HashSet::new()
        } else {
            let rows: Vec<(Uuid,)> = sqlx::query_as(
                "UPDATE materialization_jobs SET origin = 'pruned' \
                  WHERE job_id = ANY($1::uuid[]) \
                    AND state = 'pending' \
                    AND origin <> 'pruned' \
                  RETURNING job_id",
            )
            .bind(&pruned_dedup_losers)
            .fetch_all(&mut *tx)
            .await?;
            rows.into_iter().map(|(id,)| id).collect()
        };

        // Realized-path carrier (migration 082, the floating-CA
        // stale-reset lane): set-if-null so the dedup arm gains the
        // carrier when the found pending row has none, while a present
        // carrier is never overwritten (the snapshot is immutable
        // content-addressed data; first writer wins).
        for r in rows {
            let Some(carried) = r.carried_realized_paths else {
                continue;
            };
            if carried.is_empty() {
                continue;
            }
            let Some(job_id) = by_drv.get(&r.derivation_id) else {
                continue;
            };
            sqlx::query(
                "UPDATE materialization_jobs \
                    SET carried_realized_paths = $2 \
                  WHERE job_id = $1 AND state = 'pending' \
                    AND carried_realized_paths IS NULL",
            )
            .bind(job_id)
            .bind(carried)
            .execute(&mut *tx)
            .await?;
        }

        // Migration 085: ONE materialization-lane reset row per genuinely
        // created job, in the SAME transaction — the per-job budget
        // window. The dedup arm writes none (a found pending job keeps
        // its window); the row's class is data, the kernel cut is
        // `(attempt_kind, event_kind)`.
        let reset_rows: Vec<AttemptRow> = rows
            .iter()
            .filter(|r| {
                by_drv
                    .get(&r.derivation_id)
                    .is_some_and(|id| inserted.contains(id))
            })
            .map(|r| {
                AttemptRow::new_reset(
                    r.derivation_id,
                    crate::state::OutcomeClass::MaterializationReset,
                    crate::state::ReportingParty::Scheduler,
                    0,
                    crate::state::AttemptKind::Materialization,
                )
            })
            .collect();
        if !reset_rows.is_empty() {
            Self::append_attempts_batch(&mut *tx, &reset_rows).await?;
        }

        rows.iter()
            .map(|r| {
                let job_id = by_drv.get(&r.derivation_id).copied().ok_or_else(|| {
                    sqlx::Error::Protocol(format!(
                        "materialization job for derivation {} missing inside the creating tx",
                        r.derivation_id
                    ))
                })?;
                Ok(JobCreateResult {
                    job_id,
                    created: inserted.contains(&job_id),
                    upgraded: matches!(r.origin, JobOrigin::Pruned) && upgraded.contains(&job_id),
                })
            })
            .collect()
    }

    /// Standalone fenced wrapper (for callers with NO enclosing
    /// transaction — the dispatch-probe partition and tests). Opens a
    /// tx, performs the claims-floor check, delegates to the in-tx
    /// core, commits. Returns [`FencedJobCreate::Fenced`] (nothing
    /// written) below the floor.
    pub(crate) async fn create_materialization_job_fenced(
        &self,
        derivation_id: Uuid,
        drv_hash: &str,
        tenant_id: Option<Uuid>,
        origin: JobOrigin,
        carried_realized_paths: Option<&[String]>,
        serving_generation: ServingGeneration,
    ) -> Result<FencedJobCreate, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedJobCreate::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let created = Self::create_materialization_jobs_in_tx(
            tx.conn(),
            &[NewJobRow {
                derivation_id,
                drv_hash,
                tenant_id,
                origin,
                carried_realized_paths,
            }],
            serving_generation.as_i64(),
        )
        .await?;
        tx.commit().await?;
        let &JobCreateResult {
            job_id,
            created,
            upgraded,
        } = created.first().ok_or_else(|| {
            sqlx::Error::Protocol(
                "create_materialization_jobs_in_tx returned no row for a one-row input".into(),
            )
        })?;
        Ok(FencedJobCreate::Applied {
            job_id,
            created,
            upgraded,
        })
    }

    /// The store-poll query: pending, not parked, no active assignment
    /// (the anti-join), oldest first, capped by `limit`.
    ///
    /// bug_099 — query-construction law: **no LIMIT without a total
    /// order.** Batch-minted jobs tie on `created_at` (DEFAULT `now()`
    /// is transaction-stable; the merge mints whole batches in one
    /// UNNEST INSERT) and the consumer makes the returned order
    /// load-bearing (512-window partition coverage + within-slice
    /// fairness) — an unspecified tie order is the SQL twin of the
    /// repo's HashMap-iteration-order rule. `job_id` is `Uuid::now_v7`
    /// (time-ordered), so `(created_at, job_id)` is the total unique
    /// key; PG satisfies it via incremental sort above the existing
    /// `(created_at) WHERE state = 'pending'` partial index — no DDL.
    pub(crate) async fn list_claimable_materialization_jobs(
        &self,
        limit: i64,
    ) -> Result<Vec<MaterializationJobRow>, sqlx::Error> {
        let raw: Vec<RawJobRow> = sqlx::query_as(
            "SELECT j.job_id, j.drv_hash, j.tenant_id, j.origin, j.state \
               FROM materialization_jobs j \
              WHERE j.state = 'pending' \
                AND (j.park_until IS NULL OR j.park_until <= now()) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM assignments a \
                     WHERE a.derivation_id = j.derivation_id \
                       AND a.status IN ('pending', 'acknowledged')) \
              ORDER BY j.created_at, j.job_id \
              LIMIT $1",
        )
        .bind(limit)
        .fetch_all(&self.pool)
        .await?;
        raw.into_iter()
            .map(MaterializationJobRow::try_from)
            .collect()
    }

    /// Terminal resolution, exec_id-keyed, fenced, at-most-once: only a
    /// `pending` job resolves (`Applied(1)`); an already-resolved job is
    /// a no-op (`Applied(0)` — terminal-row-wins, the D7 identity rule).
    pub(crate) async fn resolve_materialization_job_fenced(
        &self,
        job_id: Uuid,
        resolution_exec_id: Option<Uuid>,
        to_state: JobState,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        debug_assert!(
            to_state != JobState::Pending,
            "resolution target must be a terminal job state"
        );
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let result = sqlx::query(
            "UPDATE materialization_jobs \
                SET state = $2, resolution_exec_id = $3, resolved_at = now() \
              WHERE job_id = $1 AND state = 'pending'",
        )
        .bind(job_id)
        .bind(to_state.as_str())
        .bind(resolution_exec_id)
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(result.rows_affected()))
    }

    /// Park (infra-budget exhaustion, design §2.5) — the job stays
    /// `pending`, `park_until` excludes it from the claimable list
    /// until the backoff expires.
    pub(crate) async fn park_materialization_job_fenced(
        &self,
        job_id: Uuid,
        park_until_epoch: f64,
        serving_generation: ServingGeneration,
    ) -> Result<FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedOutcome::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let result = sqlx::query(
            "UPDATE materialization_jobs \
                SET park_until = to_timestamp($2), park_began_at = now() \
              WHERE job_id = $1 AND state = 'pending'",
        )
        .bind(job_id)
        .bind(park_until_epoch)
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        Ok(FencedOutcome::Applied(result.rows_affected()))
    }

    // r[impl sched.materialize.view-settlement]
    /// The zero-live-interest closer (BC-2), job_id-keyed, BATCHED,
    /// and TOTAL over the DAG-absent arm: ONE fenced transaction
    /// (1) cancels every pending job row in the sweep and (2) closes
    /// their open materialization-kind assignments — found through
    /// the durable `drv_executions.attempt_kind` join, never an
    /// in-memory exec_id (nodes may be reaped; the post-failover view
    /// rebuild presents exactly that shape). Charge-free by
    /// construction: no `drv_attempts` row is written, and a closed
    /// assignment is invisible to the establishment sweep — the
    /// leaked-attempt → `materialization_infra` conversion is
    /// unreachable for cancelled jobs.
    ///
    /// Batched for the actor's sake (the `build.rs`
    /// persist_status_batch precedent): the per-job predecessor cost
    /// ~5 PG round-trips per job inside the single-threaded actor —
    /// live_053's 134.65s Tick spent 16.6s cancelling 5,258 jobs
    /// sequentially at 3.16ms each while every queued RPC waited
    /// head-of-line. A sweep is now a constant number of round-trips
    /// regardless of N.
    pub(crate) async fn cancel_jobs_and_close_attempts_fenced(
        &self,
        job_ids: &[Uuid],
        serving_generation: ServingGeneration,
    ) -> Result<FencedCancelSweep, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            FencedBegin::Fenced { .. } => return Ok(FencedCancelSweep::Fenced),
            FencedBegin::Open(ftx) => ftx,
        };
        let cancelled: Vec<Uuid> = sqlx::query_scalar(
            "UPDATE materialization_jobs \
                SET state = 'cancelled', resolved_at = now() \
              WHERE job_id = ANY($1) AND state = 'pending' \
              RETURNING job_id",
        )
        .bind(job_ids)
        .fetch_all(tx.conn())
        .await?;
        // The attempt close runs over the WHOLE sweep, not just the
        // freshly-flipped rows — re-entry on an already-terminal job
        // still closes a straggler open attempt (the same idempotent
        // semantics the per-job closer had).
        static SQL: std::sync::LazyLock<String> = std::sync::LazyLock::new(|| {
            super::close_assignments_sql(
                "derivation_id IN \
                     (SELECT derivation_id FROM materialization_jobs WHERE job_id = ANY($1)) \
                 AND exec_id IN (SELECT e.exec_id FROM drv_executions e \
                                 WHERE e.attempt_kind = 'materialization')",
                2,
            )
        });
        sqlx::query_scalar::<_, i64>(SQL.as_str())
            .bind(job_ids)
            .bind(super::AssignmentCloseStatus::Cancelled.as_str())
            .bind(super::AssignmentCloseStatus::Cancelled.exec_status())
            .fetch_one(tx.conn())
            .await?;
        tx.commit().await?;
        Ok(FencedCancelSweep::Applied {
            cancelled: cancelled.into_iter().collect(),
        })
    }

    /// The unresolved (pending) job for one derivation, if any — the
    /// consumption transaction's job lookup (PG is the authority; the
    /// actor's in-memory view is a cache). Carries the row's ORIGIN:
    /// `origin = 'pruned'` is the durable settlement discriminator
    /// (the walk-era pruned mark's successor — design §4/A2/A13,
    /// T-D2.1).
    pub(crate) async fn unresolved_job_for_derivation(
        &self,
        derivation_id: Uuid,
    ) -> Result<Option<(Uuid, JobOrigin, Option<Vec<String>>)>, sqlx::Error> {
        let row: Option<(Uuid, String, Option<Vec<String>>)> = sqlx::query_as(
            "SELECT job_id, origin, carried_realized_paths FROM materialization_jobs \
              WHERE derivation_id = $1 AND state = 'pending' \
              LIMIT 1",
        )
        .bind(derivation_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|(job_id, origin, carried)| {
            let origin = origin.parse().map_err(|_| {
                sqlx::Error::Decode(
                    format!(
                        "materialization_jobs.origin: value {origin:?} not in the rust-side \
                         alphabet"
                    )
                    .into(),
                )
            })?;
            Ok((job_id, origin, carried))
        })
        .transpose()
    }

    /// Dormancy probe (Wave 6 / VM subtest support): row counts of
    /// `(materialization_jobs, build_wanted_outputs)`.
    /// Test diagnostic (merged_bug_284 sweep): row-count assertions
    /// for the db/tests + actor/tests materialization batteries; no
    /// production reader.
    #[cfg(test)]
    pub(crate) async fn count_materialization_rows(&self) -> Result<(i64, i64), sqlx::Error> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM materialization_jobs), \
                    (SELECT COUNT(*) FROM build_wanted_outputs)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }

    /// D1/A6 (merged_bug_163): delete RESOLVED materialization jobs past
    /// the forensic horizon once nothing references them — no live pin
    /// row (the 093 kind key: pins are released by the resolve path) and
    /// no remaining interest (the 078 derived view: interest ends when
    /// every interested build is terminal). `pending` jobs are NEVER
    /// deleted: claimable is the armed action. The NOT-EXISTS-pins
    /// conjunct is the ordering guard the schema deliberately has no FK
    /// for (093 commentary).
    // r[impl sched.db.table-retention+1]
    pub(crate) async fn gc_resolved_materialization_jobs(
        &self,
        horizon_secs: f64,
        limit: i64,
        serving_generation: ServingGeneration,
    ) -> Result<u64, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            crate::db::FencedBegin::Fenced { .. } => return Ok(0),
            crate::db::FencedBegin::Open(ftx) => ftx,
        };
        let result = sqlx::query(
            "DELETE FROM materialization_jobs WHERE job_id IN (
                 SELECT j.job_id FROM materialization_jobs j
                 WHERE j.state <> 'pending'
                   AND COALESCE(j.resolved_at, j.created_at)
                       < now() - make_interval(secs => $1)
                   AND NOT EXISTS (SELECT 1 FROM scheduler_live_pins p
                                   WHERE p.job_id = j.job_id)
                   AND NOT EXISTS (SELECT 1 FROM materialization_interest i
                                   WHERE i.job_id = j.job_id)
                 ORDER BY COALESCE(j.resolved_at, j.created_at)
                 LIMIT $2)",
        )
        .bind(horizon_secs)
        .bind(limit)
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        Ok(result.rows_affected())
    }
}
