//! `materialization_jobs` (migration 078) integration tests: fenced
//! creation with database-enforced dedup, the in-tx core's atomicity
//! with the caller's transaction (B6), the claimable-list anti-join,
//! exec_id-keyed at-most-once resolution, parking, and cancellation.

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::materialization::{FencedJobCreate, NewJobRow};
use crate::db::{FencedWrite, SchedulerDb};
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
// r[verify sched.materialize.job]
#[tokio::test]
async fn job_creation_is_dedup_idempotent() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-dedup-hash").await?;

    let first = db
        .create_materialization_job_fenced(drv, "job-dedup-hash", None, JobOrigin::Pruned, 1)
        .await?;
    let FencedJobCreate::Applied {
        job_id: first_id,
        created: true,
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
            1,
        )
        .await?;
    let FencedJobCreate::Applied {
        job_id: second_id,
        created: false,
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
            1,
        )
        .await?;
    assert_eq!(resolved, FencedWrite::Applied(1));
    let third = db
        .create_materialization_job_fenced(drv, "job-dedup-hash", None, JobOrigin::Pruned, 1)
        .await?;
    let FencedJobCreate::Applied {
        job_id: third_id,
        created: true,
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
// r[verify sched.materialize.job]
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
        .create_materialization_job_fenced(drv, "job-fence-hash", None, JobOrigin::Pruned, 1)
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
        .create_materialization_job_fenced(drv, "job-fence-hash", None, JobOrigin::Pruned, 2)
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
// r[verify sched.materialize.job]
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
            .create_materialization_job_fenced(drv, hash, None, JobOrigin::CacheOpportunity, 1)
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
    assert_eq!(claimable[0].state, JobState::Pending);

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

/// (d) Resolution is exec_id-keyed, fenced, and at-most-once: the
/// first resolve stamps `resolution_exec_id`/`resolved_at`; resolving
/// an already-resolved job is a no-op (`Applied(0)` — terminal-row-
/// wins); resolving below the floor is `Fenced` and changes nothing.
// r[verify sched.materialize.job]
#[tokio::test]
async fn job_resolution_is_fenced_and_at_most_once() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-resolve-hash").await?;

    let created = db
        .create_materialization_job_fenced(drv, "job-resolve-hash", None, JobOrigin::Pruned, 1)
        .await?;
    let FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("create must apply");
    };

    let exec_id = Uuid::now_v7();
    let resolved = db
        .resolve_materialization_job_fenced(job_id, Some(exec_id), JobState::ResolvedSuccess, 1)
        .await?;
    assert_eq!(resolved, FencedWrite::Applied(1), "first resolve applies");

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
            1,
        )
        .await?;
    assert_eq!(
        second,
        FencedWrite::Applied(0),
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
        .create_materialization_job_fenced(drv2, "job-resolve-hash-2", None, JobOrigin::Pruned, 1)
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
        .resolve_materialization_job_fenced(job2, None, JobState::Cancelled, 1)
        .await?;
    assert_eq!(
        fenced,
        FencedWrite::Fenced,
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
// r[verify sched.materialize.job]
#[tokio::test]
async fn parked_job_excluded_until_backoff_expires() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-park-hash").await?;

    let created = db
        .create_materialization_job_fenced(drv, "job-park-hash", None, JobOrigin::Pruned, 1)
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
        .park_materialization_job_fenced(job_id, now_epoch + 3600.0, 1)
        .await?;
    assert_eq!(parked, FencedWrite::Applied(1));
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
        .park_materialization_job_fenced(job_id, now_epoch - 1.0, 1)
        .await?;
    assert_eq!(parked, FencedWrite::Applied(1));
    assert_eq!(
        db.list_claimable_materialization_jobs(10).await?.len(),
        1,
        "an expired park no longer excludes the job"
    );
    Ok(())
}

/// (f) `cancel_materialization_jobs_for_derivation_fenced`: pending →
/// cancelled (the zero-live-interest closer; Phase A: tests only).
// r[verify sched.materialize.job]
#[tokio::test]
async fn job_cancellation_marks_cancelled() -> anyhow::Result<()> {
    let (test_db, db, drv) = setup("job-cancel-hash").await?;

    let created = db
        .create_materialization_job_fenced(drv, "job-cancel-hash", None, JobOrigin::Pruned, 1)
        .await?;
    let FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("create must apply");
    };

    let cancelled = db
        .cancel_materialization_jobs_for_derivation_fenced(drv, 1)
        .await?;
    assert_eq!(
        cancelled,
        FencedWrite::Applied(1),
        "one pending job cancelled"
    );

    let (state, resolved_at): (String, Option<String>) = sqlx::query_as(
        "SELECT state, resolved_at::text FROM materialization_jobs WHERE job_id = $1",
    )
    .bind(job_id)
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(state, "cancelled");
    assert!(resolved_at.is_some(), "cancellation stamps resolved_at");

    // Cancelling again (nothing pending): no-op.
    let again = db
        .cancel_materialization_jobs_for_derivation_fenced(drv, 1)
        .await?;
    assert_eq!(again, FencedWrite::Applied(0));
    Ok(())
}

/// (g) THE B6 assertion (adjudication PDQ-9):
/// `create_materialization_jobs_in_tx` called inside a transaction
/// that is then ROLLED BACK leaves zero job rows — a rolled-back merge
/// creates no jobs (design B6: rollbackRestoresWantedAndEvidence;
/// A13: stampAtomicWithActivation).
// r[verify sched.materialize.job]
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
        }],
        1,
    )
    .await?;
    assert_eq!(created.len(), 1, "the in-tx core returns one row per input");
    assert!(created[0].1, "the job reports created inside the tx");

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
        }],
        1,
    )
    .await?;
    tx.commit().await?;
    assert_eq!(created.len(), 1);
    assert_eq!(job_count(&test_db.pool, drv).await?, 1);
    Ok(())
}

/// The Rust-side `JobOrigin`/`JobState` alphabets and the migration-078
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
