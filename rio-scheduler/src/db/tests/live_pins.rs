//! `scheduler_live_pins` pin-kind discrimination (migration 078,
//! design §5.2/§5.3): the build-input release paths never touch
//! materialization pins; materialization pins are released only by
//! the all-interest-terminal rule (PP-2).

use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::SchedulerDb;
use crate::state::DrvHash;

/// Fresh ephemeral PG + a SchedulerDb handle.
async fn setup() -> (TestDb, SchedulerDb) {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    (test_db, db)
}

/// Count pins for a drv by kind.
async fn pin_count(pool: &sqlx::PgPool, drv: &str, kind: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = $1 AND pin_kind = $2",
    )
    .bind(drv)
    .bind(kind)
    .fetch_one(pool)
    .await?)
}

/// SHA-256 of a store path (the `store_path_hash` keying convention).
fn path_hash(path: &str) -> Vec<u8> {
    use sha2::Digest;
    sha2::Sha256::digest(path.as_bytes()).to_vec()
}

// r[verify sched.materialize.pinning]
/// PP-2: `sweep_stale_live_pins` NEVER deletes materialization pins
/// (its terminal-status premise is false for them: the pin outlives the
/// creating build), and `unpin_live_inputs` / `unpin_live_inputs_batch`
/// never touch them either. Each of the three build-input release
/// paths is exercised against a drv that IS terminal — exactly the
/// state where the as-built paths would delete everything.
#[tokio::test]
async fn build_pin_release_paths_exclude_materialization_pins() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    // A derivation that is TERMINAL (the build-input release premise).
    insert_test_derivation(&db, "pin-kind-drv").await?;
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'pin-kind-drv'")
        .execute(&test_db.pool)
        .await?;

    // One build-input pin + one materialization pin for the same drv.
    let build_pin = path_hash("/nix/store/aaa-build-input");
    let mat_pin = path_hash("/nix/store/bbb-materialized");
    let seed_build_pin = || async {
        sqlx::query(
            "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) \
             VALUES ($1, 'pin-kind-drv') ON CONFLICT DO NOTHING",
        )
        .bind(&build_pin)
        .execute(&test_db.pool)
        .await
    };
    seed_build_pin().await?;
    sqlx::query(
        "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash, pin_kind, job_id) \
         VALUES ($1, 'pin-kind-drv', 'materialization', $2)",
    )
    .bind(&mat_pin)
    .bind(Uuid::now_v7())
    .execute(&test_db.pool)
    .await?;

    // Release path 1: the per-drv terminal-status unpin.
    db.unpin_live_inputs(&DrvHash::from("pin-kind-drv")).await?;
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "build_input").await?,
        0,
        "unpin_live_inputs releases the build-input pin"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "materialization").await?,
        1,
        "PP-2: unpin_live_inputs must NOT release materialization pins \
         (their release rule is all-interest-terminal, not build-terminal)"
    );

    // Release path 2: the batch unpin (cancel-build path).
    seed_build_pin().await?;
    db.unpin_live_inputs_batch(&["pin-kind-drv"]).await?;
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "build_input").await?,
        0,
        "unpin_live_inputs_batch releases the build-input pin"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "materialization").await?,
        1,
        "PP-2: unpin_live_inputs_batch must NOT release materialization pins"
    );

    // Release path 3: the stale-pin recovery sweep (the drv is terminal,
    // so the as-built sweep deletes every pin it finds for it).
    seed_build_pin().await?;
    let swept = db.sweep_stale_live_pins().await?;
    assert!(swept >= 1, "the sweep releases the stale build-input pin");
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "build_input").await?,
        0,
        "sweep_stale_live_pins releases the build-input pin"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-kind-drv", "materialization").await?,
        1,
        "PP-2: sweep_stale_live_pins must NOT release materialization pins"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// §5.3 release rule: `release_materialization_pins_for_resolved_jobs`
/// deletes a materialization pin only when its job is RESOLVED (state
/// != 'pending') AND no live interest remains (no live build carries a
/// wanted-relation row for the job's derivation) — the
/// all-interest-terminal rule. Either condition alone keeps the pin.
#[tokio::test]
async fn materialization_pins_released_only_after_all_interest_terminal() -> anyhow::Result<()> {
    use crate::db::materialization::FencedJobCreate;
    use crate::db::wanted::WantedRow;
    use crate::state::{JobOrigin, JobState};

    let (test_db, db) = setup().await;
    let drv_id = insert_test_derivation(&db, "pin-release-drv").await?;

    // A live build interested in the derivation (wanted-relation row +
    // build status 'pending' — the materialization_interest view).
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
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(drv_id)
        .execute(&test_db.pool)
        .await?;
    let fenced_outcome = db
        .record_wanted_fenced(
            1,
            &[WantedRow {
                build_id,
                derivation_id: drv_id,
                wanted_output_names: &[],
            }],
        )
        .await?;
    assert!(fenced_outcome.settled());

    // The materialization job + the pins its execution ingested.
    let FencedJobCreate::Applied { job_id, .. } = db
        .create_materialization_job_fenced(
            drv_id,
            "pin-release-drv",
            None,
            JobOrigin::Pruned,
            None,
            1,
        )
        .await?
    else {
        anyhow::bail!("job create must apply");
    };
    let paths = vec![
        "/nix/store/ccc-materialized-root".to_string(),
        "/nix/store/ddd-materialized-dep".to_string(),
    ];
    db.pin_materialized_paths(job_id, &DrvHash::from("pin-release-drv"), &paths)
        .await?;
    assert_eq!(
        pin_count(&test_db.pool, "pin-release-drv", "materialization").await?,
        2,
        "pin-at-ingest writes one materialization pin per path"
    );

    // (1) Job unresolved + live interest → nothing released.
    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(
        released, 0,
        "an unresolved job's pins are never released (regardless of interest)"
    );

    // (2) Job resolved, but the interested build is still live → kept.
    let fenced_outcome = db
        .resolve_materialization_job_fenced(
            job_id,
            Some(Uuid::now_v7()),
            JobState::ResolvedSuccess,
            1,
        )
        .await?;
    assert!(fenced_outcome.settled());
    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(
        released, 0,
        "a resolved job's pins survive while a live build is still interested \
         (the all-interest-terminal rule, §5.3)"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-release-drv", "materialization").await?,
        2
    );

    // (3) The interested build goes terminal → all interest gone → released.
    sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
        .bind(build_id)
        .execute(&test_db.pool)
        .await?;
    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(
        released, 2,
        "job resolved AND all interest terminal → the pins are released"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-release-drv", "materialization").await?,
        0
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// bug_253 (bughunt wave, migration 093): pin kinds are DISJOINT ROW
/// SETS with independent lifecycles — the PK is
/// (store_path_hash, drv_hash, pin_kind). A materialization pin for
/// the same (path, drv) coexists with the build_input pin instead of
/// re-kinding it; the from_source sequence (build pin → mat pin →
/// resolve+release) leaves the build_input row protecting the path
/// for the still-live build.
#[tokio::test]
async fn pin_kinds_are_disjoint_rows() -> anyhow::Result<()> {
    use crate::db::materialization::FencedJobCreate;
    use crate::state::{JobOrigin, JobState};

    let (test_db, db) = setup().await;
    let drv_id = insert_test_derivation(&db, "fs-seq-drv").await?;

    // The from_source build pinned its input path.
    let shared = "/nix/store/fff-from-source-input".to_string();
    db.pin_live_inputs(&DrvHash::from("fs-seq-drv"), std::slice::from_ref(&shared))
        .await?;

    // The drv is re-minted as a materialization job; its execution
    // ingests/verifies the same path and pins at ingest.
    let FencedJobCreate::Applied { job_id, .. } = db
        .create_materialization_job_fenced(drv_id, "fs-seq-drv", None, JobOrigin::Reprobe, None, 1)
        .await?
    else {
        anyhow::bail!("job create must apply");
    };
    db.pin_materialized_paths(job_id, &DrvHash::from("fs-seq-drv"), &[shared])
        .await?;

    // Disjoint rows: BOTH kinds present (the pre-093 DO-UPDATE re-kind
    // collapsed them to one materialization row).
    assert_eq!(
        pin_count(&test_db.pool, "fs-seq-drv", "build_input").await?,
        1,
        "the build_input pin must coexist, not be re-kinded away"
    );
    assert_eq!(
        pin_count(&test_db.pool, "fs-seq-drv", "materialization").await?,
        1,
        "pin-at-ingest writes its own materialization row"
    );

    // Independent release: the job resolves with no live interest, the
    // §5.3 release deletes ONLY the materialization row.
    let resolved = db
        .resolve_materialization_job_fenced(
            job_id,
            Some(Uuid::now_v7()),
            JobState::ResolvedSuccess,
            1,
        )
        .await?;
    assert_eq!(
        resolved,
        crate::db::FencedOutcome::Applied(1),
        "the live leader's resolve applies (must_use discipline)"
    );
    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(released, 1, "only the materialization pin is released");
    assert_eq!(
        pin_count(&test_db.pool, "fs-seq-drv", "build_input").await?,
        1,
        "the still-live build's input stays protected by its own row \
         (pre-093 RED: the single re-kinded row was deleted mid-build)"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// bug_233 (bughunt wave, migration 093): the CHECK constraint makes
/// the unreleasable NULL-job materialization pin UNREPRESENTABLE — a
/// direct INSERT violating it is rejected by PG.
#[tokio::test]
async fn null_job_materialization_pin_is_unrepresentable() -> anyhow::Result<()> {
    let (test_db, _db) = setup().await;
    let res = sqlx::query(
        "INSERT INTO scheduler_live_pins (store_path_hash, drv_hash, pin_kind, job_id) \
         VALUES ($1, 'null-job-drv', 'materialization', NULL)",
    )
    .bind(path_hash("/nix/store/ggg-null-job"))
    .execute(&test_db.pool)
    .await;
    assert!(
        res.is_err(),
        "a NULL-job materialization pin must violate \
         scheduler_live_pins_materialization_job (pre-093 RED: insert succeeded)"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// PD-10 (superseded by migration 093 / bug_253): pin-at-ingest writes
/// the materialization pin as its OWN row under the
/// (store_path_hash, drv_hash, pin_kind) key — an existing build_input
/// pin for the same (path, drv) is untouched, and re-pinning the same
/// materialization path is an idempotent job_id refresh. The
/// materialization row then survives the original build's terminal
/// release paths (the §5.3/B2-strong GC window stays closed).
#[tokio::test]
async fn pin_at_ingest_writes_disjoint_materialization_row() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    insert_test_derivation(&db, "pin-rekind-drv").await?;

    // The as-built build-input pin (a build attempt pinned this path).
    let shared_path = "/nix/store/eee-shared-input".to_string();
    db.pin_live_inputs(
        &DrvHash::from("pin-rekind-drv"),
        std::slice::from_ref(&shared_path),
    )
    .await?;
    assert_eq!(
        pin_count(&test_db.pool, "pin-rekind-drv", "build_input").await?,
        1
    );

    // A materialization execution ingests/verifies the same path —
    // twice (the second pin is the idempotent job_id refresh).
    let job_id = Uuid::now_v7();
    db.pin_materialized_paths(
        job_id,
        &DrvHash::from("pin-rekind-drv"),
        std::slice::from_ref(&shared_path),
    )
    .await?;
    db.pin_materialized_paths(job_id, &DrvHash::from("pin-rekind-drv"), &[shared_path])
        .await?;

    // Disjoint rows: the build_input pin is untouched; the
    // materialization row carries the job attribution.
    assert_eq!(
        pin_count(&test_db.pool, "pin-rekind-drv", "build_input").await?,
        1,
        "the build_input row is untouched by pin-at-ingest (093 key)"
    );
    let (kind, pinned_job): (String, Option<Uuid>) = sqlx::query_as(
        "SELECT pin_kind, job_id FROM scheduler_live_pins \
          WHERE drv_hash = 'pin-rekind-drv' AND pin_kind = 'materialization'",
    )
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(kind, "materialization");
    assert_eq!(pinned_job, Some(job_id));

    // The original build's drv goes terminal and its release paths run:
    // the materialization row SURVIVES them (the §5.3/B2-strong GC
    // window stays closed); only the build_input row is released.
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'pin-rekind-drv'")
        .execute(&test_db.pool)
        .await?;
    db.unpin_live_inputs(&DrvHash::from("pin-rekind-drv"))
        .await?;
    db.sweep_stale_live_pins().await?;
    assert_eq!(
        pin_count(&test_db.pool, "pin-rekind-drv", "build_input").await?,
        0,
        "the build_input row is released by the build-terminal paths"
    );
    assert_eq!(
        pin_count(&test_db.pool, "pin-rekind-drv", "materialization").await?,
        1,
        "the materialization pin survives the original build's terminal release paths"
    );
    Ok(())
}

/// merged_bug_176 (bughunt wave): a resolved job's pins survive while a
/// ROW-LESS live build is interested — interest derives from
/// `build_derivations` membership (which every build has), not from
/// the optional `build_wanted_outputs` row. Pre-fix the
/// `materialization_interest` view joined the wanted relation, so a
/// live build without a row was invisible and the §5.3 release
/// predicate dropped pins a live build still needed.
// r[verify sched.materialize.pinning]
#[tokio::test]
async fn pins_survive_rowless_live_interest() -> anyhow::Result<()> {
    use crate::db::materialization::FencedJobCreate;
    use crate::state::{JobOrigin, JobState};

    let (test_db, db) = setup().await;
    let drv_id = insert_test_derivation(&db, "pin-rowless-drv").await?;

    // A live build that is a MEMBER (build_derivations) but recorded
    // no wanted row (legacy gap / row-less submission shape).
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
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(drv_id)
        .execute(&test_db.pool)
        .await?;

    let FencedJobCreate::Applied { job_id, .. } = db
        .create_materialization_job_fenced(
            drv_id,
            "pin-rowless-drv",
            None,
            JobOrigin::Pruned,
            None,
            1,
        )
        .await?
    else {
        anyhow::bail!("job create must apply");
    };
    db.pin_materialized_paths(
        job_id,
        &DrvHash::from("pin-rowless-drv"),
        &["/nix/store/eee-rowless-root".to_string()],
    )
    .await?;

    let fenced_outcome = db
        .resolve_materialization_job_fenced(
            job_id,
            Some(Uuid::now_v7()),
            JobState::ResolvedSuccess,
            1,
        )
        .await?;
    assert!(fenced_outcome.settled());

    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(
        released, 0,
        "a row-less live member is interest — its pins must survive the sweep"
    );

    sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
        .bind(build_id)
        .execute(&test_db.pool)
        .await?;
    let released = db.release_materialization_pins_for_resolved_jobs().await?;
    assert_eq!(released, 1, "interest gone -> released");
    Ok(())
}
