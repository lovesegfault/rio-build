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
//!     [`SchedulerDb::claims_floor`] /
//!     [`SchedulerDb::at_or_above_floor`] helpers, and calls the
//!     in-tx core.
// r[impl sched.materialize.job]

use std::collections::{HashMap, HashSet};

use sqlx::PgConnection;
use uuid::Uuid;

use super::{FencedWrite, SchedulerDb};
use crate::state::{JobOrigin, JobState};

/// One materialization-job row, as the store poll and the consumption
/// transaction read it.
#[derive(Debug, Clone)]
pub(crate) struct MaterializationJobRow {
    pub job_id: Uuid,
    pub derivation_id: Uuid,
    pub drv_hash: String,
    pub tenant_id: Option<Uuid>,
    pub origin: JobOrigin,
    pub state: JobState,
    /// Epoch seconds; `None` = not parked.
    pub park_until_epoch: Option<f64>,
    pub created_generation: i64,
    pub resolution_exec_id: Option<Uuid>,
}

/// Raw FromRow shape for the claimable-list query: TEXT enums come back
/// as `String` and are parsed into the typed vocabulary in
/// [`MaterializationJobRow::try_from`] — the CHECK constraints make a
/// parse failure a schema/code drift bug, surfaced as a decode error
/// rather than silently skipped (the `RawAttemptRow` pattern).
#[derive(Debug, sqlx::FromRow)]
struct RawJobRow {
    job_id: Uuid,
    derivation_id: Uuid,
    drv_hash: String,
    tenant_id: Option<Uuid>,
    origin: String,
    state: String,
    park_until_epoch: Option<f64>,
    created_generation: i64,
    resolution_exec_id: Option<Uuid>,
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
        Ok(Self {
            job_id: raw.job_id,
            derivation_id: raw.derivation_id,
            tenant_id: raw.tenant_id,
            origin: raw
                .origin
                .parse()
                .map_err(|_| parse_err("origin", &raw.origin))?,
            state: raw
                .state
                .parse()
                .map_err(|_| parse_err("state", &raw.state))?,
            drv_hash: raw.drv_hash,
            park_until_epoch: raw.park_until_epoch,
            created_generation: raw.created_generation,
            resolution_exec_id: raw.resolution_exec_id,
        })
    }
}

/// One job to create (the in-tx batch input).
pub(crate) struct NewJobRow<'a> {
    pub derivation_id: Uuid,
    pub drv_hash: &'a str,
    pub tenant_id: Option<Uuid>,
    pub origin: JobOrigin,
}

/// Outcome of the standalone fenced job creation: the unresolved job
/// for the derivation (created or found by the dedup), or the fence
/// refused the write.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum FencedJobCreate {
    /// The write applied. `created` is false when an unresolved job
    /// already existed (the partial-unique-index dedup found it).
    Applied { job_id: Uuid, created: bool },
    /// The serving generation is below the claims floor: the
    /// transaction rolled back having written nothing.
    Fenced,
}

impl SchedulerDb {
    /// THE creation core (adjudication PDQ-9 / design §2.1 rows 1–2,
    /// A13/B6): create (or find existing) unresolved jobs for a batch
    /// of derivations, inside the caller's transaction. The caller's
    /// tx carries the merge fence; this helper adds no second floor
    /// read. Batch UNNEST INSERT + ON CONFLICT dedup (the partial-
    /// unique index); returns `(job_id, created)` per input row in
    /// input order.
    pub(crate) async fn create_materialization_jobs_in_tx(
        tx: &mut PgConnection,
        rows: &[NewJobRow<'_>],
        created_generation: i64,
    ) -> Result<Vec<(Uuid, bool)>, sqlx::Error> {
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

        rows.iter()
            .map(|r| {
                let job_id = by_drv.get(&r.derivation_id).copied().ok_or_else(|| {
                    sqlx::Error::Protocol(format!(
                        "materialization job for derivation {} missing inside the creating tx",
                        r.derivation_id
                    ))
                })?;
                Ok((job_id, inserted.contains(&job_id)))
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
        serving_generation: i64,
    ) -> Result<FencedJobCreate, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedJobCreate::Fenced);
        }
        let created = Self::create_materialization_jobs_in_tx(
            &mut tx,
            &[NewJobRow {
                derivation_id,
                drv_hash,
                tenant_id,
                origin,
            }],
            serving_generation,
        )
        .await?;
        tx.commit().await?;
        let &(job_id, created) = created.first().ok_or_else(|| {
            sqlx::Error::Protocol(
                "create_materialization_jobs_in_tx returned no row for a one-row input".into(),
            )
        })?;
        Ok(FencedJobCreate::Applied { job_id, created })
    }

    /// The store-poll query: pending, not parked, no active assignment
    /// (the anti-join), oldest first, capped by `limit`.
    pub(crate) async fn list_claimable_materialization_jobs(
        &self,
        limit: i64,
    ) -> Result<Vec<MaterializationJobRow>, sqlx::Error> {
        let raw: Vec<RawJobRow> = sqlx::query_as(
            "SELECT j.job_id, j.derivation_id, j.drv_hash, j.tenant_id, j.origin, j.state, \
                    EXTRACT(EPOCH FROM j.park_until)::float8 AS park_until_epoch, \
                    j.created_generation, j.resolution_exec_id \
               FROM materialization_jobs j \
              WHERE j.state = 'pending' \
                AND (j.park_until IS NULL OR j.park_until <= now()) \
                AND NOT EXISTS ( \
                    SELECT 1 FROM assignments a \
                     WHERE a.derivation_id = j.derivation_id \
                       AND a.status IN ('pending', 'acknowledged')) \
              ORDER BY j.created_at \
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
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        debug_assert!(
            to_state != JobState::Pending,
            "resolution target must be a terminal job state"
        );
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let result = sqlx::query(
            "UPDATE materialization_jobs \
                SET state = $2, resolution_exec_id = $3, resolved_at = now() \
              WHERE job_id = $1 AND state = 'pending'",
        )
        .bind(job_id)
        .bind(to_state.as_str())
        .bind(resolution_exec_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(result.rows_affected()))
    }

    /// Park (infra-budget exhaustion, design §2.5) — the job stays
    /// `pending`, `park_until` excludes it from the claimable list
    /// until the backoff expires.
    pub(crate) async fn park_materialization_job_fenced(
        &self,
        job_id: Uuid,
        park_until_epoch: f64,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let result = sqlx::query(
            "UPDATE materialization_jobs \
                SET park_until = to_timestamp($2) \
              WHERE job_id = $1 AND state = 'pending'",
        )
        .bind(job_id)
        .bind(park_until_epoch)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(result.rows_affected()))
    }

    /// Cancel every unresolved job for a derivation (the zero-live-
    /// interest closer).
    pub(crate) async fn cancel_materialization_jobs_for_derivation_fenced(
        &self,
        derivation_id: Uuid,
        serving_generation: i64,
    ) -> Result<FencedWrite, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        let floor = Self::claims_floor(&mut tx).await?;
        if !Self::at_or_above_floor(floor, serving_generation) {
            tx.rollback().await?;
            return Ok(FencedWrite::Fenced);
        }
        let result = sqlx::query(
            "UPDATE materialization_jobs \
                SET state = 'cancelled', resolved_at = now() \
              WHERE derivation_id = $1 AND state = 'pending'",
        )
        .bind(derivation_id)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(FencedWrite::Applied(result.rows_affected()))
    }

    /// Dormancy probe (Wave 6 / VM subtest support): row counts of
    /// `(materialization_jobs, build_wanted_outputs)`.
    pub(crate) async fn count_materialization_rows(&self) -> Result<(i64, i64), sqlx::Error> {
        let row: (i64, i64) = sqlx::query_as(
            "SELECT (SELECT COUNT(*) FROM materialization_jobs), \
                    (SELECT COUNT(*) FROM build_wanted_outputs)",
        )
        .fetch_one(&self.pool)
        .await?;
        Ok(row)
    }
}
