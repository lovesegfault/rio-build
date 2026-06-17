//! `materialization_jobs` (migration 078) integration tests: fenced
//! creation with database-enforced dedup, the in-tx core's atomicity
//! with the caller's transaction (B6), the claimable-list anti-join,
//! exec_id-keyed at-most-once resolution, parking, and cancellation.

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::materialization::{FencedJobCreate, NewJobRow};
use crate::db::{FencedOutcome, SchedulerDb};
use crate::state::{JobOrigin, JobState};

/// Fresh ephemeral PG + a SchedulerDb handle + one derivation.
async fn setup(hash: &str) -> anyhow::Result<(TestDb, SchedulerDb, Uuid)> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let derivation_id = insert_test_derivation(&db, hash).await?;
    Ok((test_db, db, derivation_id))
}

/// Count job rows for a derivation.
async fn job_count(pool: &sqlx::PgPool, derivation_id: Uuid) -> anyhow::Result<i64> {
    Ok(
        sqlx::query_scalar("SELECT COUNT(*) FROM materialization_jobs WHERE derivation_id = $1")
            .bind(derivation_id)
            .fetch_one(pool)
            .await?,
    )
}

/// (a) `create_materialization_job_fenced` creates a pending job; a
/// second create for the same derivation is idempotent — it returns
/// the existing job_id with `created: false` and writes no second row
/// (the database-enforced C3-class dedup).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn job_creation_is_dedup_idempotent() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-dedup-hash").await?;

    let first = db
        .create_materialization_job_fenced(
            drv,
            "job-dedup-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: first_id,
        created: true,
        ..
    } = first
    else {
        anyhow::bail!("first create must apply with created=true, got {first:?}");
    };

    // Second create for the same derivation: finds the existing
    // unresolved job, writes nothing.
    let second = db
        .create_materialization_job_fenced(
            drv,
            "job-dedup-hash",
            None,
            JobOrigin::CacheOpportunity,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: second_id,
        created: false,
        ..
    } = second
    else {
        anyhow::bail!("second create must be the dedup no-op (created=false), got {second:?}");
    };
    assert_eq!(
        first_id, second_id,
        "the dedup must return the existing unresolved job's id"
    );
    assert_eq!(
        job_count(&test_db.pool, drv).await?,
        1,
        "exactly one job row exists after the duplicate create"
    );

    // Resolving the job frees the partial index: a new create for the
    // same derivation now creates a NEW job.
    let resolved = db
        .resolve_materialization_job_fenced(
            first_id,
            Some(Uuid::now_v7()),
            JobState::ResolvedSuccess,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(resolved, FencedOutcome::Applied(1));
    let third = db
        .create_materialization_job_fenced(
            drv,
            "job-dedup-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: third_id,
        created: true,
        ..
    } = third
    else {
        anyhow::bail!("create after resolution must create a fresh job, got {third:?}");
    };
    assert_ne!(third_id, first_id, "a fresh job gets a fresh id");
    assert_eq!(job_count(&test_db.pool, drv).await?, 2);
    Ok(())
}

/// (b) Creation below the durable claims floor → `Fenced`, no row
/// written (the A17/A18 fence extension to the job table).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn job_creation_below_floor_is_fenced() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-fence-hash").await?;

    // The successor tenure claimed generation 2: that is the floor.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES ($1, 'tenure-current')",
    )
    .bind(2i64)
    .execute(&test_db.pool)
    .await?;

    // The deposed tenure's late create (serving generation 1).
    let outcome = db
        .create_materialization_job_fenced(
            drv,
            "job-fence-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(
        outcome,
        FencedJobCreate::Fenced,
        "below-floor job creation must be fenced"
    );
    assert_eq!(
        job_count(&test_db.pool, drv).await?,
        0,
        "a fenced create must leave zero rows"
    );

    // Positive control: the current tenure (at the floor) creates.
    let outcome = db
        .create_materialization_job_fenced(
            drv,
            "job-fence-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(2),
        )
        .await?;
    assert!(
        matches!(outcome, FencedJobCreate::Applied { created: true, .. }),
        "an at-the-floor create must apply, got {outcome:?}"
    );
    assert_eq!(job_count(&test_db.pool, drv).await?, 1);
    Ok(())
}

/// (c) `list_claimable_materialization_jobs`: pending jobs with NO
/// active assignment row for their derivation (the anti-join), not
/// parked, capped by limit, oldest first.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn list_claimable_excludes_claimed_and_parked() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Three derivations with one pending job each, created oldest-first
    // (explicit created_at so the ordering assertion is deterministic).
    let mut drvs = Vec::new();
    let mut jobs = Vec::new();
    for (i, hash) in ["claim-a", "claim-b", "claim-c"].iter().enumerate() {
        let drv = insert_test_derivation(&db, hash).await?;
        let created = db
            .create_materialization_job_fenced(
                drv,
                hash,
                None,
                JobOrigin::CacheOpportunity,
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await?;
        let FencedJobCreate::Applied { job_id, .. } = created else {
            anyhow::bail!("create must apply");
        };
        sqlx::query(
            "UPDATE materialization_jobs \
             SET created_at = now() - make_interval(secs => $2) WHERE job_id = $1",
        )
        .bind(job_id)
        .bind(100.0 - (i as f64) * 10.0)
        .execute(&test_db.pool)
        .await?;
        drvs.push(drv);
        jobs.push(job_id);
    }

    // All three are claimable; oldest (claim-a) first.
    let claimable = db.list_claimable_materialization_jobs(10).await?;
    assert_eq!(claimable.len(), 3, "all three pending jobs are claimable");
    assert_eq!(
        claimable.iter().map(|j| j.job_id).collect::<Vec<_>>(),
        jobs,
        "claimable list is oldest-first"
    );
    assert_eq!(claimable[0].origin, JobOrigin::CacheOpportunity);
    // state is WHERE-clause-pinned ('pending') and vocabulary-validated
    // by the loader's try_from; the descriptor row no longer carries it
    // (merged_bug_284 trim) — assert the durable value directly.
    let state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE job_id = $1")
            .bind(claimable[0].job_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(state, "pending");

    // The limit caps the list (oldest win).
    let capped = db.list_claimable_materialization_jobs(2).await?;
    assert_eq!(capped.len(), 2);
    assert_eq!(
        capped.iter().map(|j| j.job_id).collect::<Vec<_>>(),
        jobs[..2],
        "the cap keeps the oldest jobs"
    );

    // An ACTIVE assignment row for claim-b's derivation hides its job
    // (the anti-join: an open attempt exists).
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status) \
         VALUES ($1, 'store-replica-0', 1, 'pending')",
    )
    .bind(drvs[1])
    .execute(&test_db.pool)
    .await?;
    let claimable = db.list_claimable_materialization_jobs(10).await?;
    assert_eq!(
        claimable.iter().map(|j| j.job_id).collect::<Vec<_>>(),
        vec![jobs[0], jobs[2]],
        "a derivation with an active assignment is excluded from the claimable list"
    );

    // A COMPLETED assignment does not hide the job (only open attempts
    // count).
    sqlx::query("UPDATE assignments SET status = 'completed' WHERE derivation_id = $1")
        .bind(drvs[1])
        .execute(&test_db.pool)
        .await?;
    let claimable = db.list_claimable_materialization_jobs(10).await?;
    assert_eq!(
        claimable.len(),
        3,
        "a terminal assignment row does not block the claim"
    );
    Ok(())
}

/// **W12-S9A (live061-R2)** — *a pending job whose DERIVATION is
/// terminal is not claimable: the durable listing carries the
/// node-state predicate*. The live_061 zombie shape: a node completes
/// by other means (store probe, sibling production) while its
/// materialization job is still `pending` — every claim against the
/// terminal node answers `Gone` (the kernel base table), the store's
/// ledger entry resolves, and the job re-enters the next listing beat
/// to burn another mint: 10,876 Gone-answers in one 78s live window,
/// the fleet pinned at ~0.5% conversion. Listed ⇒ admittable
/// (`sched.materialize.claimability-projection`) must hold on the
/// node axis too. Every terminal status is exercised via the same
/// derived set production splices (`terminal_status_sql!` ⇔
/// `TERMINAL_STATUSES`, drift-pinned); non-terminal statuses are the
/// stay-listed control.
// r[verify sched.materialize.claimability-projection+1]
#[tokio::test]
async fn list_claimable_excludes_terminal_node_jobs() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // One pending job per derivation status in the FULL alphabet —
    // the claimable verdict is then asserted per `is_terminal()`
    // (the one terminal-set producer), never a hand list.
    let mut by_status: Vec<(crate::state::DerivationStatus, Uuid)> = Vec::new();
    for status in crate::state::DerivationStatus::ALL {
        let hash = format!("term-list-{status}");
        let drv = insert_test_derivation(&db, &hash).await?;
        let created = db
            .create_materialization_job_fenced(
                drv,
                &hash,
                None,
                JobOrigin::CacheOpportunity,
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await?;
        let FencedJobCreate::Applied { job_id, .. } = created else {
            anyhow::bail!("create must apply for {status}");
        };
        sqlx::query("UPDATE derivations SET status = $2 WHERE derivation_id = $1")
            .bind(drv)
            .bind(status.as_str())
            .execute(&test_db.pool)
            .await?;
        by_status.push((*status, job_id));
    }

    let listed: std::collections::HashSet<Uuid> = db
        .list_claimable_materialization_jobs(64)
        .await?
        .into_iter()
        .map(|j| j.job_id)
        .collect();
    for (status, job_id) in &by_status {
        if status.is_terminal() {
            assert!(
                !listed.contains(job_id),
                "a pending job on a {status} (terminal) derivation must NOT be \
                 claimable — its claim can only answer Gone (the live_061 \
                 zombie head: the job pins the oldest-first window forever)"
            );
        } else {
            assert!(
                listed.contains(job_id),
                "a pending job on a {status} (non-terminal) derivation must \
                 stay claimable"
            );
        }
    }
    Ok(())
}

/// (d) Resolution is exec_id-keyed, fenced, and at-most-once: the
/// first resolve stamps `resolution_exec_id`/`resolved_at`; resolving
/// an already-resolved job is a no-op (`Applied(0)` — terminal-row-
/// wins); resolving below the floor is `Fenced` and changes nothing.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn job_resolution_is_fenced_and_at_most_once() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-resolve-hash").await?;

    let created = db
        .create_materialization_job_fenced(
            drv,
            "job-resolve-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("create must apply");
    };

    let exec_id = Uuid::now_v7();
    let resolved = db
        .resolve_materialization_job_fenced(
            job_id,
            Some(exec_id),
            JobState::ResolvedSuccess,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(resolved, FencedOutcome::Applied(1), "first resolve applies");

    let (state, resolution_exec, resolved_at): (String, Option<Uuid>, Option<String>) =
        sqlx::query_as(
            "SELECT state, resolution_exec_id, resolved_at::text \
             FROM materialization_jobs WHERE job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&test_db.pool)
        .await?;
    assert_eq!(state, "resolved_success");
    assert_eq!(
        resolution_exec,
        Some(exec_id),
        "the resolving execution's identity is stamped (D7)"
    );
    assert!(resolved_at.is_some(), "resolved_at is stamped");

    // Second resolve (different exec, different terminal state): no-op.
    let second = db
        .resolve_materialization_job_fenced(
            job_id,
            Some(Uuid::now_v7()),
            JobState::ResolvedUnobtainable,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(
        second,
        FencedOutcome::Applied(0),
        "resolving an already-resolved job must be a no-op (at-most-once)"
    );
    let (state, resolution_exec): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT state, resolution_exec_id FROM materialization_jobs WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(state, "resolved_success", "the first resolution wins");
    assert_eq!(resolution_exec, Some(exec_id));

    // Below-floor resolution of a fresh pending job: fenced, unchanged.
    let drv2 = insert_test_derivation(&db, "job-resolve-hash-2").await?;
    let created = db
        .create_materialization_job_fenced(
            drv2,
            "job-resolve-hash-2",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied { job_id: job2, .. } = created else {
        anyhow::bail!("create must apply");
    };
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (5, 'tenure-successor')",
    )
    .execute(&test_db.pool)
    .await?;
    let fenced = db
        .resolve_materialization_job_fenced(
            job2,
            None,
            JobState::Cancelled,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(
        fenced,
        FencedOutcome::Fenced,
        "below-floor resolution must be fenced"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE job_id = $1")
            .bind(job2)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(state, "pending", "a fenced resolution changes nothing");
    Ok(())
}

/// (e) Parking: `park_materialization_job_fenced` sets `park_until`
/// (the job stays `pending`); the claimable list excludes it until the
/// backoff expires.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn parked_job_excluded_until_backoff_expires() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-park-hash").await?;

    let created = db
        .create_materialization_job_fenced(
            drv,
            "job-park-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("create must apply");
    };
    assert_eq!(
        db.list_claimable_materialization_jobs(10).await?.len(),
        1,
        "the fresh job is claimable"
    );

    // Park for an hour: excluded, but still pending.
    let now_epoch: f64 = sqlx::query_scalar("SELECT EXTRACT(EPOCH FROM now())::float8")
        .fetch_one(&test_db.pool)
        .await?;
    let parked = db
        .park_materialization_job_fenced(
            job_id,
            now_epoch + 3600.0,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(parked, FencedOutcome::Applied(1));
    assert!(
        db.list_claimable_materialization_jobs(10).await?.is_empty(),
        "a parked job is not claimable before its backoff expires"
    );
    let state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(state, "pending", "parking never resolves the job");

    // Re-park with an already-expired backoff: claimable again.
    let parked = db
        .park_materialization_job_fenced(
            job_id,
            now_epoch - 1.0,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(parked, FencedOutcome::Applied(1));
    assert_eq!(
        db.list_claimable_materialization_jobs(10).await?.len(),
        1,
        "an expired park no longer excludes the job"
    );
    Ok(())
}

/// (f) `resolve_moot_jobs_and_close_attempts_fenced` (Cancelled
/// letter): ONE fenced sweep
/// cancels every pending job in the set (pending → cancelled) and
/// closes their open materialization-kind assignments in the SAME
/// transaction (the zero-live-interest closer, total over the
/// DAG-absent arm), charge-free. Application is per-job — the
/// returned `cancelled` set carries exactly the rows that flipped, an
/// already-terminal member is simply absent (idempotent re-entry,
/// attempt close still runs) — while the fence verdict is sweep-level.
/// The batch IS the contract (live_053: 5,258 sequential per-job
/// fenced cancels = 16.6s inside one Tick).
// r[verify sched.materialize.job+2]
// r[verify sched.materialize.view-settlement]
#[tokio::test]
async fn job_cancellation_sweep_marks_cancelled_and_closes_attempts() -> anyhow::Result<()> {
    use crate::db::materialization::FencedCancelSweep;
    let (test_db, db, drv_a) = setup("job-cancel-hash-a").await?;
    let drv_b = insert_test_derivation(&db, "job-cancel-hash-b").await?;

    let mut job_ids = Vec::new();
    for (drv, hash) in [(drv_a, "job-cancel-hash-a"), (drv_b, "job-cancel-hash-b")] {
        let created = db
            .create_materialization_job_fenced(
                drv,
                hash,
                None,
                JobOrigin::Pruned,
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await?;
        let FencedJobCreate::Applied { job_id, .. } = created else {
            anyhow::bail!("create must apply");
        };
        job_ids.push(job_id);

        // Seed an open materialization-kind attempt for the derivation
        // (assignment + drv_executions row carrying the kind).
        let exec_id = uuid::Uuid::new_v4();
        db.insert_assignment(drv, &crate::state::ExecutorId::from("store-0"), 1, exec_id)
            .await?;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, attempt_kind) \
             VALUES ($1, $2, 'store-0', now(), 'materialization')",
        )
        .bind(exec_id)
        .bind(hash)
        .execute(&test_db.pool)
        .await?;
    }

    let swept = db
        .resolve_moot_jobs_and_close_attempts_fenced(
            &job_ids,
            JobState::Cancelled,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedCancelSweep::Applied {
        resolved: cancelled,
    } = swept
    else {
        anyhow::bail!("sweep must apply, got {swept:?}");
    };
    assert_eq!(
        cancelled,
        job_ids.iter().copied().collect(),
        "both pending jobs flipped in one sweep"
    );

    for job_id in &job_ids {
        let (state, resolved_at): (String, Option<String>) = sqlx::query_as(
            "SELECT state, resolved_at::text FROM materialization_jobs WHERE job_id = $1",
        )
        .bind(job_id)
        .fetch_one(&test_db.pool)
        .await?;
        assert_eq!(state, "cancelled");
        assert!(resolved_at.is_some(), "cancellation stamps resolved_at");
    }
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(
        open, 0,
        "every open attempt closes in the same fenced sweep"
    );
    let closed: i64 =
        sqlx::query_scalar("SELECT count(*) FROM assignments WHERE status = 'cancelled'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(closed, 2, "closed BY the cancel, one per job");
    let (charges,): (i64,) =
        sqlx::query_as("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(charges, 0, "charge-free (BC-2)");

    // Sweeping again (nothing pending): idempotent re-entry — the
    // sweep applies with an EMPTY cancelled set.
    let again = db
        .resolve_moot_jobs_and_close_attempts_fenced(
            &job_ids,
            JobState::Cancelled,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(
        again,
        FencedCancelSweep::Applied {
            resolved: std::collections::HashSet::new()
        },
        "already-terminal members are absent from the cancelled set"
    );

    // Mixed sweep: a fresh pending job alongside the two terminal
    // ones — exactly the fresh one flips.
    let drv_c = insert_test_derivation(&db, "job-cancel-hash-c").await?;
    let created = db
        .create_materialization_job_fenced(
            drv_c,
            "job-cancel-hash-c",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied { job_id: job_c, .. } = created else {
        anyhow::bail!("create must apply");
    };
    let mixed_ids: Vec<uuid::Uuid> = job_ids.iter().copied().chain([job_c]).collect();
    let mixed = db
        .resolve_moot_jobs_and_close_attempts_fenced(
            &mixed_ids,
            JobState::Cancelled,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(
        mixed,
        FencedCancelSweep::Applied {
            resolved: [job_c].into_iter().collect()
        },
        "a mixed sweep reports exactly the rows that flipped"
    );

    // Below the floor: the WHOLE sweep is fenced, nothing written.
    sqlx::query("INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'succ')")
        .execute(&test_db.pool)
        .await?;
    let fenced = db
        .resolve_moot_jobs_and_close_attempts_fenced(
            &job_ids,
            JobState::Cancelled,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert_eq!(fenced, FencedCancelSweep::Fenced);
    Ok(())
}

/// (g) THE B6 assertion (adjudication PDQ-9):
/// `create_materialization_jobs_in_tx` called inside a transaction
/// that is then ROLLED BACK leaves zero job rows — a rolled-back merge
/// creates no jobs (design B6: rollbackRestoresWantedAndEvidence;
/// A13: stampAtomicWithActivation).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn job_create_in_rolled_back_tx_leaves_no_row() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-rollback-hash").await?;

    let mut tx = db.pool().begin().await?;
    let created = SchedulerDb::create_materialization_jobs_in_tx(
        &mut tx,
        &[NewJobRow {
            derivation_id: drv,
            drv_hash: "job-rollback-hash",
            tenant_id: None,
            origin: JobOrigin::CacheOpportunity,
            carried_realized_paths: None,
        }],
        1,
    )
    .await?;
    assert_eq!(created.len(), 1, "the in-tx core returns one row per input");
    assert!(created[0].created, "the job reports created inside the tx");

    // The merge fails: the whole transaction rolls back.
    tx.rollback().await?;

    let (jobs, wanted) = db.count_materialization_rows().await?;
    assert_eq!(
        (jobs, wanted),
        (0, 0),
        "a rolled-back creating transaction must leave zero materialization rows (B6)"
    );
    assert_eq!(job_count(&test_db.pool, drv).await?, 0);

    // The same creation in a COMMITTED tx persists (positive control).
    let mut tx = db.pool().begin().await?;
    let created = SchedulerDb::create_materialization_jobs_in_tx(
        &mut tx,
        &[NewJobRow {
            derivation_id: drv,
            drv_hash: "job-rollback-hash",
            tenant_id: None,
            origin: JobOrigin::CacheOpportunity,
            carried_realized_paths: None,
        }],
        1,
    )
    .await?;
    tx.commit().await?;
    assert_eq!(created.len(), 1);
    assert_eq!(job_count(&test_db.pool, drv).await?, 1);
    Ok(())
}

/// (h) T-5.2 (PDQ-9 disposition, Phase B obligation 2): the CROSS-SITE
/// dedup — the dispatch-probe site (the standalone fenced helper, §2.1
/// row 3) and a concurrent merge (the in-tx core riding an open merge
/// transaction, §2.1 rows 1/2/4 + reprobe) race to create a job for the
/// same derivation. The `materialization_jobs_unresolved` partial-unique
/// index arbitrates ACROSS the two creation layers, not just within
/// one:
///
///   - the loser's INSERT blocks behind the winner's uncommitted row
///     (PG speculative-insertion xact wait), then takes the dedup arm;
///   - exactly one pending row survives, both sites converge on its
///     job_id;
///   - the property holds in both orders (merge-first and probe-first).
///
/// This is the one structural property the per-§2.1-row transaction-
/// posture split (in-tx for merge origins, standalone fenced for the
/// probe origin — the kept PDQ-9 verdict, PD-B9) could break; pinning it
/// is what makes keeping the split safe.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn flag_on_concurrent_probe_and_merge_create_one_job() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-cross-site-hash").await?;

    // ── Order 1: the merge transaction opens and creates its job in-tx
    //    but has NOT committed yet (a mid-flight merge). ──
    let mut merge_tx = db.pool().begin().await?;
    let merge_created = SchedulerDb::create_materialization_jobs_in_tx(
        &mut merge_tx,
        &[NewJobRow {
            derivation_id: drv,
            drv_hash: "job-cross-site-hash",
            tenant_id: None,
            origin: JobOrigin::CacheOpportunity,
            carried_realized_paths: None,
        }],
        1,
    )
    .await?;
    assert!(merge_created[0].created, "the merge's in-tx create inserts");
    let merge_job_id = merge_created[0].job_id;

    // The dispatch-probe site fires CONCURRENTLY on a separate pool
    // connection: its INSERT ... ON CONFLICT DO NOTHING must block on
    // the uncommitted conflicting row until the merge tx resolves
    // (cross-site arbitration is the database's, not the actor's).
    let probe_db = SchedulerDb::new(test_db.pool.clone());
    let probe_task = tokio::spawn(async move {
        probe_db
            .create_materialization_job_fenced(
                drv,
                "job-cross-site-hash",
                None,
                JobOrigin::CacheOpportunity,
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await
    });

    // The probe-site insert must be BLOCKED behind the open merge tx —
    // if it completed while the merge is uncommitted, the index did not
    // arbitrate across sites.
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    assert!(
        !probe_task.is_finished(),
        "the probe-site create must block behind the open merge transaction \
         (the partial-unique index is the cross-site arbiter)"
    );

    // The merge commits → the probe unblocks, hits the conflict, and
    // takes the dedup arm.
    merge_tx.commit().await?;
    let probe_outcome = probe_task.await??;
    let FencedJobCreate::Applied {
        job_id: probe_job,
        created,
        ..
    } = probe_outcome
    else {
        anyhow::bail!("the probe-site create must apply (dedup), got {probe_outcome:?}");
    };
    assert!(
        !created,
        "the probe site must find the merge's job (created=false), never insert a second"
    );
    assert_eq!(
        probe_job, merge_job_id,
        "both creation sites converge on the same job row"
    );
    assert_eq!(
        job_count(&test_db.pool, drv).await?,
        1,
        "exactly one job row after the cross-site race"
    );

    // ── Order 2 (the reverse): the probe site committed first; a merge
    //    transaction's in-tx core then encounters the existing row. ──
    let drv2 = insert_test_derivation(&db, "job-cross-site-hash-2").await?;
    let probe_first = db
        .create_materialization_job_fenced(
            drv2,
            "job-cross-site-hash-2",
            None,
            JobOrigin::CacheOpportunity,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: probe_first_id,
        created: true,
        ..
    } = probe_first
    else {
        anyhow::bail!("the probe-first create must insert, got {probe_first:?}");
    };
    let mut merge_tx = db.pool().begin().await?;
    let merge_second = SchedulerDb::create_materialization_jobs_in_tx(
        &mut merge_tx,
        &[NewJobRow {
            derivation_id: drv2,
            drv_hash: "job-cross-site-hash-2",
            tenant_id: None,
            origin: JobOrigin::Pruned,
            carried_realized_paths: None,
        }],
        1,
    )
    .await?;
    merge_tx.commit().await?;
    assert!(
        !merge_second[0].created,
        "the merge's in-tx core must take the dedup arm against the probe's row"
    );
    assert!(
        merge_second[0].upgraded,
        "the merge's PRUNED dedup against the probe's cache_opportunity row \
         is the PD-D1 upgrade"
    );
    assert_eq!(
        merge_second[0].job_id, probe_first_id,
        "the merge converges on the probe site's job"
    );
    assert_eq!(job_count(&test_db.pool, drv2).await?, 1);

    // Origins after both orders (the pruned-wins extension of this
    // pin, T-D2.1): order 1 is cache_opportunity-vs-cache_opportunity —
    // no upgrade, the winner's origin survives; order 2's winner was
    // the probe's cache_opportunity row, which the merge's PRUNED
    // dedup UPGRADES (the durable mark must not be lost to the dedup
    // order).
    let origins: Vec<(String, String)> =
        sqlx::query_as("SELECT drv_hash, origin FROM materialization_jobs ORDER BY drv_hash")
            .fetch_all(&test_db.pool)
            .await?;
    assert_eq!(
        origins,
        vec![
            (
                "job-cross-site-hash".to_string(),
                "cache_opportunity".to_string()
            ),
            ("job-cross-site-hash-2".to_string(), "pruned".to_string()),
        ],
        "non-pruned dedups never rewrite; pruned dedups upgrade"
    );
    Ok(())
}

/// The Rust-side `JobOrigin`/`JobState` alphabets and the 078_materialization_jobs
/// CHECK constraints stay in lockstep: every enum variant is accepted
/// by PG, and every literal PG accepts has an enum variant (the
/// `OutcomeClass` lockstep pattern).
#[tokio::test]
async fn materialization_job_alphabets_match_check_constraints() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;

    let defs: Vec<String> = sqlx::query_scalar(
        "SELECT pg_get_constraintdef(c.oid) \
         FROM pg_constraint c JOIN pg_class t ON c.conrelid = t.oid \
         WHERE t.relname = 'materialization_jobs' AND c.contype = 'c'",
    )
    .fetch_all(&test_db.pool)
    .await?;

    let literals = |needle: &str| -> std::collections::BTreeSet<String> {
        let def = defs
            .iter()
            .find(|d| d.contains(needle))
            .unwrap_or_else(|| panic!("no CHECK constraint mentioning {needle}"));
        def.split('\'')
            .skip(1)
            .step_by(2)
            .map(str::to_string)
            .collect()
    };

    let check_states = literals("state");
    let rust_states: std::collections::BTreeSet<String> = JobState::ALL
        .iter()
        .map(|s| s.as_str().to_string())
        .collect();
    assert_eq!(
        rust_states, check_states,
        "JobState and the 078 CHECK constraint must carry the same alphabet \
         (extending it is a new migration plus a variant)"
    );

    let check_origins = literals("origin");
    let rust_origins: std::collections::BTreeSet<String> = JobOrigin::ALL
        .iter()
        .map(|o| o.as_str().to_string())
        .collect();
    assert_eq!(
        rust_origins, check_origins,
        "JobOrigin and the 078 CHECK constraint must carry the same alphabet"
    );
    Ok(())
}

// ── Phase D' T-D2.1 (PD-D1): pruned-wins origin upgrade on the dedup arm ──

/// The dedup-then-prune corner (PD-D1): a prune landing on a node with
/// an existing unresolved job must not lose its mark in the origin
/// world. `create_materialization_jobs_in_tx` upgrades the existing
/// pending row's origin to 'pruned' (pruned-wins); the upgrade is
/// monotone (never downgraded by a later non-pruned creation) and
/// reserved to the pruned origin (reprobe/stale_reset never upgrade).
/// `created` stays false for the dedup arm — an upgrade is NOT a
/// creation (jobs_created_total counts creations only).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn dedup_upgrade_is_pruned_wins_and_monotone() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-upgrade-hash").await?;
    let origin_of = |pool: sqlx::PgPool, id: Uuid| async move {
        let o: String =
            sqlx::query_scalar("SELECT origin FROM materialization_jobs WHERE derivation_id = $1")
                .bind(id)
                .fetch_one(&pool)
                .await?;
        anyhow::Ok(o)
    };

    // cache_opportunity first (the probe's creation)...
    let first = db
        .create_materialization_job_fenced(
            drv,
            "job-upgrade-hash",
            None,
            JobOrigin::CacheOpportunity,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: first_id,
        created: true,
        ..
    } = first
    else {
        anyhow::bail!("first create must apply with created=true, got {first:?}");
    };

    // ... then a pruned merge dedups onto it: created=false AND the
    // existing row's origin upgrades to 'pruned' (the durable mark).
    let second = db
        .create_materialization_job_fenced(
            drv,
            "job-upgrade-hash",
            None,
            JobOrigin::Pruned,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: second_id,
        created: false,
        ..
    } = second
    else {
        anyhow::bail!("the pruned dedup must not be a creation, got {second:?}");
    };
    assert_eq!(first_id, second_id, "the dedup found the existing job");
    assert_eq!(
        origin_of(test_db.pool.clone(), drv).await?,
        "pruned",
        "pruned-wins: the dedup must upgrade the existing pending row's origin"
    );
    assert_eq!(job_count(&test_db.pool, drv).await?, 1, "still one row");

    // Monotone: a later cache_opportunity dedup never downgrades.
    let third = db
        .create_materialization_job_fenced(
            drv,
            "job-upgrade-hash",
            None,
            JobOrigin::CacheOpportunity,
            None,
            ServingGeneration::stamp_from_claim(1),
        )
        .await?;
    assert!(
        matches!(third, FencedJobCreate::Applied { created: false, .. }),
        "the post-upgrade dedup is still a no-op creation, got {third:?}"
    );
    assert_eq!(
        origin_of(test_db.pool.clone(), drv).await?,
        "pruned",
        "monotone: pruned is never downgraded"
    );

    // Reserved to pruned: reprobe/stale_reset dedups never upgrade.
    let drv2 = insert_test_derivation(&db, "job-upgrade-hash-2").await?;
    db.create_materialization_job_fenced(
        drv2,
        "job-upgrade-hash-2",
        None,
        JobOrigin::CacheOpportunity,
        None,
        ServingGeneration::stamp_from_claim(1),
    )
    .await?;
    for origin in [JobOrigin::Reprobe, JobOrigin::StaleReset] {
        let r = db
            .create_materialization_job_fenced(
                drv2,
                "job-upgrade-hash-2",
                None,
                origin,
                None,
                ServingGeneration::stamp_from_claim(1),
            )
            .await?;
        assert!(
            matches!(r, FencedJobCreate::Applied { created: false, .. }),
            "non-pruned dedup is a no-op, got {r:?}"
        );
        assert_eq!(
            origin_of(test_db.pool.clone(), drv2).await?,
            "cache_opportunity",
            "only the pruned origin upgrades (got an upgrade from {origin:?})"
        );
    }
    Ok(())
}

/// D1/A6 (merged_bug_163): the resolved-jobs retention sweep deletes
/// ONLY resolved+old+unpinned+interest-free rows — pending jobs, fresh
/// jobs, pinned jobs, and jobs with live interest all survive.
// r[verify sched.db.table-retention+1]
#[tokio::test]
async fn gc_resolved_jobs_sweeps_only_unreferenced_resolved() -> anyhow::Result<()> {
    let (test_db, db, drv_swept) = setup("gc-jobs-swept").await?;
    let drv_pending = insert_test_derivation(&db, "gc-jobs-pending").await?;
    let drv_pinned = insert_test_derivation(&db, "gc-jobs-pinned").await?;
    let drv_interest = insert_test_derivation(&db, "gc-jobs-interest").await?;
    let drv_fresh = insert_test_derivation(&db, "gc-jobs-fresh").await?;

    let mk = |drv: Uuid, hash: &'static str| {
        let db = db.clone();
        async move {
            let FencedJobCreate::Applied { job_id, .. } = db
                .create_materialization_job_fenced(
                    drv,
                    hash,
                    None,
                    JobOrigin::Pruned,
                    None,
                    ServingGeneration::stamp_from_claim(1),
                )
                .await?
            else {
                anyhow::bail!("create must apply");
            };
            anyhow::Ok(job_id)
        }
    };
    let j_swept = mk(drv_swept, "gc-jobs-swept").await?;
    let _j_pending = mk(drv_pending, "gc-jobs-pending").await?;
    let j_pinned = mk(drv_pinned, "gc-jobs-pinned").await?;
    let j_interest = mk(drv_interest, "gc-jobs-interest").await?;
    let j_fresh = mk(drv_fresh, "gc-jobs-fresh").await?;

    // Resolve everything except the pending one; backdate all but fresh.
    for (job, old) in [
        (j_swept, true),
        (j_pinned, true),
        (j_interest, true),
        (j_fresh, false),
    ] {
        sqlx::query(
            "UPDATE materialization_jobs SET state = 'resolved_success', \
             resolved_at = CASE WHEN $2 THEN now() - interval '3 days' ELSE now() END \
             WHERE job_id = $1",
        )
        .bind(job)
        .bind(old)
        .execute(&test_db.pool)
        .await?;
    }
    // Live pin referencing j_pinned (the 093 materialization kind key).
    sqlx::query(
        "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash, pin_kind, job_id) \
         VALUES ('gcpinhash', 'gc-jobs-pinned', 'materialization', $1)",
    )
    .bind(j_pinned)
    .execute(&test_db.pool)
    .await?;
    // Live interest for j_interest: an active build wanting the drv.
    let build = {
        let build_id = Uuid::new_v4();
        db.insert_build(
            build_id,
            None,
            crate::state::PriorityClass::Scheduled,
            true,
            &Default::default(),
            None,
        )
        .await?;
        build_id
    };
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build)
        .bind(drv_interest)
        .execute(&test_db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
         VALUES ($1, $2, '{}')",
    )
    .bind(build)
    .bind(drv_interest)
    .execute(&test_db.pool)
    .await?;

    let deleted = db
        .gc_resolved_materialization_jobs(86_400.0, 1000, ServingGeneration::stamp_from_claim(1))
        .await?;
    assert_eq!(deleted, 1, "exactly the unreferenced resolved-old job");
    assert_eq!(job_count(&test_db.pool, drv_swept).await?, 0, "swept");
    assert_eq!(
        job_count(&test_db.pool, drv_pending).await?,
        1,
        "pending kept"
    );
    assert_eq!(
        job_count(&test_db.pool, drv_pinned).await?,
        1,
        "pinned kept"
    );
    assert_eq!(
        job_count(&test_db.pool, drv_interest).await?,
        1,
        "interest kept"
    );
    assert_eq!(job_count(&test_db.pool, drv_fresh).await?, 1, "fresh kept");
    Ok(())
}

/// bug_266 (kind partition, A2): the recovery view's `claimed_by`
/// holder join resolves ONLY through a materialization-kind execution.
/// A live BUILD-kind attempt on the same derivation (the stale-reset
/// lane: the job is created while a build attempt is still open) must
/// not be reported as the job's holder — pre-fix the kind-blind
/// `LEFT JOIN assignments` stamped the builder identity onto the job
/// and recovery rebuilt a Claimed view nobody held (re-claims answered
/// `NotYetReady` against a phantom holder).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn unresolved_job_view_ignores_build_kind_assignments() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-kindblind-hash").await?;

    db.create_materialization_job_fenced(
        drv,
        "job-kindblind-hash",
        None,
        JobOrigin::Pruned,
        None,
        ServingGeneration::stamp_from_claim(1),
    )
    .await?;
    // An ACTIVE build-kind attempt on the same derivation — the
    // production mint shape (assignments + drv_executions rows in one
    // fenced transaction, attempt_kind='build').
    let minted = db
        .mint_pull_attempt_fenced(
            drv,
            &crate::state::ExecutorId::from("job-kindblind-hash"),
            ServingGeneration::stamp_from_claim(1),
            Uuid::now_v7(),
            "kindblindloghash",
            None,
            None,
            crate::state::AttemptKind::Build,
            None,
        )
        .await?;
    assert!(
        matches!(minted, FencedOutcome::Applied(_)),
        "build mint must apply, got {minted:?}"
    );

    let rows = db.load_unresolved_materialization_jobs().await?;
    assert_eq!(rows.len(), 1, "exactly one unresolved job");
    assert_eq!(
        rows[0].claimed_by, None,
        "a build-kind assignment must never resolve as a materialization job's holder"
    );

    // Green companion: a materialization-kind claim IS the holder.
    let drv2 = insert_test_derivation(&db, "job-kindheld-hash").await?;
    db.create_materialization_job_fenced(
        drv2,
        "job-kindheld-hash",
        None,
        JobOrigin::Pruned,
        None,
        ServingGeneration::stamp_from_claim(1),
    )
    .await?;
    let holder = crate::state::ExecutorId::from("job-kindheld-hash@store-0");
    let minted = db
        .mint_pull_attempt_fenced(
            drv2,
            &holder,
            ServingGeneration::stamp_from_claim(1),
            Uuid::now_v7(),
            "kindheldloghash",
            None,
            None,
            crate::state::AttemptKind::Materialization,
            None,
        )
        .await?;
    assert!(matches!(minted, FencedOutcome::Applied(_)));
    let mut rows = db.load_unresolved_materialization_jobs().await?;
    rows.sort_by(|a, b| a.drv_hash.cmp(&b.drv_hash));
    assert_eq!(rows.len(), 2);
    assert_eq!(
        rows.iter()
            .find(|r| r.drv_hash == "job-kindheld-hash")
            .map(|r| r.claimed_by.as_deref()),
        Some(Some("job-kindheld-hash@store-0")),
        "a materialization-kind claim resolves as the holder"
    );

    drop(test_db);
    Ok(())
}

/// (k) bug_099 — the claimable listing's ORDER BY must be a TOTAL
/// unique key. Batch-minted jobs tie on `created_at` (DEFAULT `now()`
/// is transaction-stable; the merge mints whole batches in one UNNEST
/// INSERT), and the wave-5 consumer made the returned order
/// load-bearing (512-window partition coverage + within-slice
/// fairness, actor/materialize.rs) — an unspecified tie order makes
/// consecutive head-window snapshots disagree on window membership
/// and within-slice order.
///
/// Witness strength (R16): this red certifies the ORDER itself — the
/// full returned sequence equals the strict `(created_at, job_id)`
/// order — not row membership.
///
/// Fixture provenance (R13): rows are minted through the production
/// batch creator (`create_materialization_jobs_in_tx`) only;
/// `created_at` is never hand-stamped. The tie-displacement is itself
/// a production flow: a second merge batch re-presenting the first
/// eight derivations with the `pruned` origin dedups onto the
/// existing pending rows and the PD-D1 pruned-wins UPDATE rewrites
/// exactly those tuples — PostgreSQL places the new tuple versions
/// physically AFTER the untouched rows, so the heap order no longer
/// matches the `job_id` order while every `created_at` still ties.
///
/// Disclosed plan-dependence: the red is reliable on the seeded shape
/// (fresh table; all-equal sort keys preserve heap order; the
/// displaced tuples sit at the heap tail) but PG does not contract
/// the pre-fix order. The post-fix assertion is plan-independent.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn listing_order_is_total_under_batch_minted_ties() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // 96 derivations → one 96-job UNNEST batch in ONE transaction:
    // every `created_at` ties (transaction-stable now()); job ids are
    // minted `Uuid::now_v7()` in input order, so the strict
    // `(created_at, job_id)` order equals the mint order.
    let mut drvs = Vec::new();
    let mut hashes = Vec::new();
    for i in 0..96 {
        let hash = format!("order-tie-{i:03}");
        let drv = insert_test_derivation(&db, &hash).await?;
        drvs.push(drv);
        hashes.push(hash);
    }
    let rows: Vec<NewJobRow<'_>> = drvs
        .iter()
        .zip(hashes.iter())
        .map(|(drv, hash)| NewJobRow {
            derivation_id: *drv,
            drv_hash: hash,
            tenant_id: None,
            origin: JobOrigin::CacheOpportunity,
            carried_realized_paths: None,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let created = SchedulerDb::create_materialization_jobs_in_tx(&mut tx, &rows, 1).await?;
    tx.commit().await?;
    let minted: Vec<Uuid> = created.iter().map(|r| r.job_id).collect();
    assert!(created.iter().all(|r| r.created), "all 96 insert fresh");
    {
        let mut sorted = minted.clone();
        sorted.sort();
        assert_eq!(
            minted, sorted,
            "precondition: now_v7 mint order is the job_id order"
        );
    }
    let distinct_created_at: i64 =
        sqlx::query_scalar("SELECT COUNT(DISTINCT created_at) FROM materialization_jobs")
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!(
        distinct_created_at, 1,
        "precondition: the whole batch ties on created_at"
    );

    // Production tie-displacement: a second merge batch re-presents
    // the first eight derivations as `pruned` — the dedup finds the
    // existing pending rows and the PD-D1 upgrade UPDATE rewrites
    // them, moving their tuple versions to the heap tail.
    let upgrade_rows: Vec<NewJobRow<'_>> = drvs[..8]
        .iter()
        .zip(hashes[..8].iter())
        .map(|(drv, hash)| NewJobRow {
            derivation_id: *drv,
            drv_hash: hash,
            tenant_id: None,
            origin: JobOrigin::Pruned,
            carried_realized_paths: None,
        })
        .collect();
    let mut tx = db.pool().begin().await?;
    let upgraded =
        SchedulerDb::create_materialization_jobs_in_tx(&mut tx, &upgrade_rows, 1).await?;
    tx.commit().await?;
    assert!(
        upgraded.iter().all(|r| !r.created && r.upgraded),
        "the second batch dedups and upgrades (PD-D1) — no new rows"
    );

    // The listing must return the strict (created_at, job_id) order.
    let listed = db.list_claimable_materialization_jobs(96).await?;
    let returned: Vec<Uuid> = listed.iter().map(|j| j.job_id).collect();
    assert_eq!(
        returned, minted,
        "left: returned order follows the shuffled heap/insert order within \
         the created_at tie / right: job_id-ordered within the tie"
    );
    Ok(())
}

/// sh-007c S6 row-set parity: `unresolved_jobs_for_derivations` over a
/// set returns the same `(job_id, origin, carried)` per key as the
/// singleton; absent keys (no pending row) are not in the map.
#[tokio::test]
async fn unresolved_jobs_for_derivations_parity_with_singleton() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    let drv_a = insert_test_derivation(&db, "batch-unres-a").await?;
    let drv_b = insert_test_derivation(&db, "batch-unres-b").await?;
    let drv_absent = insert_test_derivation(&db, "batch-unres-absent").await?;
    let g = ServingGeneration::stamp_from_claim(1);
    db.create_materialization_job_fenced(drv_a, "batch-unres-a", None, JobOrigin::Pruned, None, g)
        .await?;
    let carried = vec!["/nix/store/carried".to_string()];
    db.create_materialization_job_fenced(
        drv_b,
        "batch-unres-b",
        None,
        JobOrigin::CacheOpportunity,
        Some(&carried),
        g,
    )
    .await?;

    let batch = db
        .unresolved_jobs_for_derivations(&[drv_a, drv_b, drv_absent])
        .await?;
    assert_eq!(batch.len(), 2, "absent keys are not in the map");

    let single_a = db.unresolved_job_for_derivation(drv_a).await?.unwrap();
    let single_b = db.unresolved_job_for_derivation(drv_b).await?.unwrap();
    assert_eq!(batch[&drv_a], single_a);
    assert_eq!(batch[&drv_b], single_b);
    assert_eq!(batch[&drv_a].1, JobOrigin::Pruned);
    assert_eq!(
        batch[&drv_b].2,
        Some(vec!["/nix/store/carried".to_string()])
    );
    drop(test_db);
    Ok(())
}

/// sh-007c S6: `close_and_resolve_materialization_batch_fenced` —
/// closed_set / inserted_set / resolved_set parity with the per-item
/// composition; idempotent re-entry yields empty sets; Fenced is
/// batch-wide.
#[tokio::test]
async fn close_and_resolve_materialization_batch_idempotent() -> anyhow::Result<()> {
    use crate::db::attempts::AttemptRow;
    use crate::state::{AttemptKind, ExecutorId, OutcomeClass, ReportingParty};

    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    let g = ServingGeneration::stamp_from_claim(1);

    // Two derivations; each with an open assignment, an execution row,
    // and a pending job.
    let mut exec_ids = Vec::new();
    let mut job_ids = Vec::new();
    let mut charges = Vec::new();
    for tag in ["bcr-a", "bcr-b"] {
        let drv = insert_test_derivation(&db, tag).await?;
        let exec = Uuid::now_v7();
        db.insert_assignment(drv, &ExecutorId::from(tag), 1, exec)
            .await?;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, \
                                         attempt_kind) \
             VALUES ($1, $2, 'store-0', now(), 'materialization')",
        )
        .bind(exec)
        .bind(format!("{tag:0>32}"))
        .execute(&test_db.pool)
        .await?;
        let FencedJobCreate::Applied { job_id, .. } = db
            .create_materialization_job_fenced(drv, tag, None, JobOrigin::Pruned, None, g)
            .await?
        else {
            anyhow::bail!("job create must apply");
        };
        let mut row = AttemptRow::new(
            drv,
            OutcomeClass::MaterializationUnobtainable,
            ReportingParty::Worker,
            AttemptKind::Materialization,
        );
        row.exec_id = Some(exec);
        exec_ids.push(exec);
        job_ids.push(job_id);
        charges.push(row);
    }
    let resolves: Vec<_> = job_ids
        .iter()
        .zip(exec_ids.iter())
        .map(|(j, e)| (*j, JobState::ResolvedSuccess, Some(*e)))
        .collect();

    // First pass: every set is full.
    let r1 = db
        .close_and_resolve_materialization_batch_fenced(g, &exec_ids, &charges, &resolves)
        .await?;
    assert!(!r1.fenced);
    assert_eq!(
        r1.closed_set,
        exec_ids.iter().copied().collect(),
        "every active assignment closed"
    );
    assert_eq!(
        r1.inserted_set,
        exec_ids.iter().copied().collect(),
        "every charge row inserted (RETURNING exec_id)"
    );
    assert_eq!(
        r1.resolved_set,
        job_ids.iter().copied().collect(),
        "every pending job resolved"
    );

    // Second pass (re-delivery): every set is empty — idempotent.
    let r2 = db
        .close_and_resolve_materialization_batch_fenced(g, &exec_ids, &charges, &resolves)
        .await?;
    assert!(!r2.fenced);
    assert!(r2.closed_set.is_empty(), "already-closed → AlreadyResolved");
    assert!(r2.inserted_set.is_empty(), "ON CONFLICT DO NOTHING");
    assert!(r2.resolved_set.is_empty(), "state='pending' guard");

    // Fenced: the floor is per-tx — a deposed generation rolls back
    // having written nothing for any member.
    let r3 = db
        .close_and_resolve_materialization_batch_fenced(
            ServingGeneration::stamp_from_claim(0),
            &exec_ids,
            &charges,
            &resolves,
        )
        .await?;
    assert!(r3.fenced);
    drop(test_db);
    Ok(())
}
