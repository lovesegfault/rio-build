//! `build_wanted_outputs` (migration 078) integration tests: the
//! durable per-(build, derivation) wanted relation — record/replace
//! isolation, the live-only saturating union, the claims-floor fence,
//! and build purge.

use crate::db::ServingGeneration;
use rio_test_support::TestDb;
use uuid::Uuid;

use super::insert_test_derivation;
use crate::db::wanted::WantedRow;
use crate::db::{FencedOutcome, SchedulerDb};
use crate::state::BuildState;

/// Fresh ephemeral PG + a SchedulerDb handle.
async fn setup() -> (TestDb, SchedulerDb) {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());
    (test_db, db)
}

/// Insert a build (status 'pending') and return its id.
async fn insert_test_build(db: &SchedulerDb) -> anyhow::Result<Uuid> {
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
    Ok(build_id)
}

/// Read the raw wanted rows for a derivation, keyed by build.
async fn raw_rows(
    pool: &sqlx::PgPool,
    derivation_id: Uuid,
) -> anyhow::Result<Vec<(Uuid, Vec<String>)>> {
    Ok(sqlx::query_as(
        "SELECT build_id, wanted_output_names FROM build_wanted_outputs \
         WHERE derivation_id = $1",
    )
    .bind(derivation_id)
    .fetch_all(pool)
    .await?)
}

/// (a) `record_wanted_fenced` writes one row per (build, derivation)
/// pair; re-recording the same build UNIONS into its row (saturating,
/// sorted-distinct — the multi-root accumulation semantics shared with
/// `union_wanted_saturating`, merged_bug_176) and never touches
/// another build's row (PK isolation, design §6/PP-5).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn wanted_rows_recorded_and_isolated_per_build() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d1 = insert_test_derivation(&db, "wanted-iso-d1").await?;
    let d2 = insert_test_derivation(&db, "wanted-iso-d2").await?;
    let b1 = insert_test_build(&db).await?;
    let b2 = insert_test_build(&db).await?;

    // b1 contributes for both derivations; b2 for d1 only.
    let applied = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[
                WantedRow {
                    build_id: b1,
                    derivation_id: d1,
                    wanted_output_names: &["out".to_string()],
                },
                WantedRow {
                    build_id: b1,
                    derivation_id: d2,
                    wanted_output_names: &["lib".to_string()],
                },
            ],
        )
        .await?;
    assert_eq!(applied, FencedOutcome::Applied(2), "two rows recorded");
    let applied = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b2,
                derivation_id: d1,
                wanted_output_names: &["dev".to_string()],
            }],
        )
        .await?;
    assert_eq!(applied, FencedOutcome::Applied(1));

    // Re-record b1's d1 contribution: replaces b1's row only.
    let fenced_outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b1,
                derivation_id: d1,
                wanted_output_names: &["out".to_string(), "doc".to_string()],
            }],
        )
        .await?;
    assert!(fenced_outcome.settled());

    let d1_rows = raw_rows(&test_db.pool, d1).await?;
    assert_eq!(d1_rows.len(), 2, "both builds' d1 rows coexist");
    let names_of = |b: Uuid| {
        d1_rows
            .iter()
            .find(|(rb, _)| *rb == b)
            .map(|(_, n)| n.clone())
            .expect("row present")
    };
    assert_eq!(
        names_of(b1),
        vec!["doc".to_string(), "out".to_string()],
        "b1's re-record unioned into b1's row (sorted, distinct)"
    );
    assert_eq!(
        names_of(b2),
        vec!["dev".to_string()],
        "b2's row untouched by b1's re-record (PK isolation)"
    );

    // b1's d2 contribution untouched by the d1 re-record.
    let d2_rows = raw_rows(&test_db.pool, d2).await?;
    assert_eq!(d2_rows.len(), 1);
    assert_eq!(d2_rows[0].1, vec!["lib".to_string()]);
    Ok(())
}

/// (b) `effective_wanted_union` unions over LIVE builds' rows only;
/// '{}' saturates to "all declared"; terminal builds' rows drop out
/// (C5); zero live rows → None (never a vacuous empty set — B4).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn effective_wanted_union_is_live_only_and_saturating() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d = insert_test_derivation(&db, "wanted-union-d").await?;
    let b1 = insert_test_build(&db).await?;
    let b2 = insert_test_build(&db).await?;
    let b3 = insert_test_build(&db).await?;

    // b1/b2 live (pending counts as live), b3 terminal.
    db.update_build_status(b3, BuildState::Succeeded, None)
        .await?;
    for b in [b1, b2, b3] {
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(d)
            .execute(&test_db.pool)
            .await?;
    }

    for (b, names) in [
        (b1, vec!["out".to_string()]),
        (b2, vec!["dev".to_string()]),
        (b3, vec!["doc".to_string()]),
    ] {
        let fenced_outcome = db
            .record_wanted_fenced(
                ServingGeneration::stamp_from_claim(1),
                &[WantedRow {
                    build_id: b,
                    derivation_id: d,
                    wanted_output_names: &names,
                }],
            )
            .await?;
        assert!(fenced_outcome.settled());
    }

    // Union over live builds only: b3's 'doc' is excluded.
    let union = db.effective_wanted_union(d).await?;
    let mut got = union.expect("live contributions exist");
    got.sort();
    assert_eq!(
        got,
        vec!["dev".to_string(), "out".to_string()],
        "union covers live builds only (terminal b3's contribution dropped)"
    );

    // An empty contribution saturates the union to "all declared".
    let fenced_outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b2,
                derivation_id: d,
                wanted_output_names: &[],
            }],
        )
        .await?;
    assert!(fenced_outcome.settled());
    assert_eq!(
        db.effective_wanted_union(d).await?,
        Some(Vec::new()),
        "any live '{{}}' contribution saturates to all-declared (empty vec)"
    );

    // b2 terminal: only b1's contribution remains.
    db.update_build_status(b2, BuildState::Succeeded, None)
        .await?;
    assert_eq!(
        db.effective_wanted_union(d).await?,
        Some(vec!["out".to_string()]),
        "terminal builds' rows drop out of the union (C5)"
    );

    // No live contributions at all: None, never a vacuous empty set.
    db.update_build_status(b1, BuildState::Succeeded, None)
        .await?;
    assert_eq!(
        db.effective_wanted_union(d).await?,
        None,
        "zero live rows must be None (B4: never a vacuous verdict)"
    );
    Ok(())
}

/// (c) The fence: `record_wanted_fenced` with a serving generation
/// below the durable claims floor → `FencedOutcome::Fenced`, zero rows
/// written (the A17/A18 extension the Phase A exit gate names).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn wanted_write_below_floor_is_fenced() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d = insert_test_derivation(&db, "wanted-fence-d").await?;
    let b = insert_test_build(&db).await?;

    // The successor tenure claimed generation 2: that is the floor.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES ($1, 'tenure-current')",
    )
    .bind(2i64)
    .execute(&test_db.pool)
    .await?;

    // The deposed tenure's late write (serving generation 1) must be
    // fenced: rolled back, nothing written.
    let outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b,
                derivation_id: d,
                wanted_output_names: &["out".to_string()],
            }],
        )
        .await?;
    assert_eq!(
        outcome,
        FencedOutcome::Fenced,
        "below-floor wanted write must be fenced"
    );
    assert!(
        raw_rows(&test_db.pool, d).await?.is_empty(),
        "a fenced write must leave zero rows"
    );

    // Positive control: the current tenure (at the floor) applies.
    let outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(2),
            &[WantedRow {
                build_id: b,
                derivation_id: d,
                wanted_output_names: &["out".to_string()],
            }],
        )
        .await?;
    assert_eq!(
        outcome,
        FencedOutcome::Applied(1),
        "an at-the-floor wanted write must apply"
    );
    assert_eq!(raw_rows(&test_db.pool, d).await?.len(), 1);
    Ok(())
}

/// (d) Purge: `delete_wanted_for_build` removes that build's wanted
/// rows (and only that build's), and the `materialization_interest`
/// view loses them.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn wanted_rows_purged_with_build() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d = insert_test_derivation(&db, "wanted-purge-d").await?;
    let b1 = insert_test_build(&db).await?;
    let b2 = insert_test_build(&db).await?;

    let fenced_outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[
                WantedRow {
                    build_id: b1,
                    derivation_id: d,
                    wanted_output_names: &["out".to_string()],
                },
                WantedRow {
                    build_id: b2,
                    derivation_id: d,
                    wanted_output_names: &["out".to_string()],
                },
            ],
        )
        .await?;
    assert!(fenced_outcome.settled());

    // A pending job for the derivation so the interest view has a
    // job to derive interest for.
    let job = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO materialization_jobs \
         (job_id, derivation_id, drv_hash, origin, created_generation) \
         VALUES ($1, $2, 'wanted-purge-d', 'cache_opportunity', 1)",
    )
    .bind(job)
    .bind(d)
    .execute(&test_db.pool)
    .await?;

    let interested = |pool: sqlx::PgPool| async move {
        let rows: Vec<(Uuid,)> =
            sqlx::query_as("SELECT build_id FROM materialization_interest WHERE job_id = $1")
                .bind(job)
                .fetch_all(&pool)
                .await?;
        anyhow::Ok(rows.into_iter().map(|(b,)| b).collect::<Vec<_>>())
    };

    for b in [b1, b2] {
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(d)
            .execute(&test_db.pool)
            .await?;
    }
    let before = interested(test_db.pool.clone()).await?;
    assert!(
        before.contains(&b1) && before.contains(&b2),
        "both live builds appear in the interest view before the purge"
    );

    // Purge b1's contributions.
    let deleted = match db
        .delete_build(b1, ServingGeneration::stamp_from_claim(1))
        .await?
    {
        crate::db::FencedOutcome::Applied(n) => n,
        other => panic!("expected Applied, got {other:?}"),
    };
    assert_eq!(deleted, 1, "exactly b1's one row deleted");

    let d_rows = raw_rows(&test_db.pool, d).await?;
    assert_eq!(d_rows.len(), 1, "b2's row survives the purge");
    assert_eq!(d_rows[0].0, b2);

    let after = interested(test_db.pool.clone()).await?;
    assert!(
        !after.contains(&b1),
        "the purged build leaves the interest view"
    );
    assert!(
        after.contains(&b2),
        "the surviving build's interest is untouched"
    );
    Ok(())
}

/// D1/A6 (merged_bug_163): the wanted-outputs retention sweep deletes
/// rows for long-terminal builds and orphan rows whose build is gone;
/// live builds' rows and freshly-finished builds' rows survive.
// r[verify sched.db.table-retention+1]
#[tokio::test]
async fn gc_dead_wanted_sweeps_terminal_and_orphans() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d = insert_test_derivation(&db, "gc-wanted-d").await?;
    let b_old = insert_test_build(&db).await?;
    let b_live = insert_test_build(&db).await?;
    let b_fresh = insert_test_build(&db).await?;
    let b_orphan = Uuid::new_v4(); // never inserted into builds

    for b in [b_old, b_live, b_fresh, b_orphan] {
        sqlx::query(
            "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
             VALUES ($1, $2, '{}')",
        )
        .bind(b)
        .bind(d)
        .execute(&test_db.pool)
        .await?;
    }
    sqlx::query(
        "UPDATE builds SET status = 'succeeded', finished_at = now() - interval '3 days' \
         WHERE build_id = $1",
    )
    .bind(b_old)
    .execute(&test_db.pool)
    .await?;
    sqlx::query("UPDATE builds SET status = 'succeeded', finished_at = now() WHERE build_id = $1")
        .bind(b_fresh)
        .execute(&test_db.pool)
        .await?;

    let deleted = db
        .gc_dead_build_wanted_outputs(86_400.0, 1000, ServingGeneration::stamp_from_claim(1))
        .await?;
    assert_eq!(deleted, 2, "terminal-old + orphan");
    let remaining: Vec<Uuid> =
        sqlx::query_scalar("SELECT build_id FROM build_wanted_outputs ORDER BY build_id")
            .fetch_all(&test_db.pool)
            .await?;
    let mut expect = vec![b_live, b_fresh];
    expect.sort_unstable();
    assert_eq!(remaining, expect, "live + fresh survive");
    Ok(())
}

/// D1/A6 (merged_bug_163) composed with A1: delete_build removes the
/// build row AND its wanted rows in ONE FENCED transaction — the
/// failed-merge rollback can no longer leak orphans, and a deposed
/// replica's late rollback is fenced to a no-op.
// r[verify sched.db.table-retention+1]
#[tokio::test]
async fn delete_build_purges_wanted_rows_same_fenced_tx() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d = insert_test_derivation(&db, "gc-deltx-d").await?;
    let b = insert_test_build(&db).await?;
    sqlx::query(
        "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
         VALUES ($1, $2, '{}')",
    )
    .bind(b)
    .bind(d)
    .execute(&test_db.pool)
    .await?;

    assert_eq!(
        db.delete_build(b, ServingGeneration::stamp_from_claim(1))
            .await?,
        FencedOutcome::Applied(1),
        "one wanted row purged with the build"
    );

    let builds: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM builds WHERE build_id = $1")
        .bind(b)
        .fetch_one(&test_db.pool)
        .await?;
    let wanted: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM build_wanted_outputs WHERE build_id = $1")
            .bind(b)
            .fetch_one(&test_db.pool)
            .await?;
    assert_eq!((builds, wanted), (0, 0), "both gone atomically");
    Ok(())
}

/// merged_bug_176 (bughunt wave): a LIVE build that is a member of the
/// derivation (`build_derivations`) but has NO `build_wanted_outputs`
/// row contributes the saturating "all declared outputs" default — the
/// effective union must saturate to `Some(vec![])`, not let another
/// build's narrow row win. Pre-fix: the union read joined
/// `build_wanted_outputs` directly, so the row-less build was
/// invisible and the narrow row decided the width.
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn effective_wanted_union_saturates_for_rowless_live_build() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d1 = insert_test_derivation(&db, "wanted-rowless-d1").await?;
    let b1 = insert_test_build(&db).await?;
    let b2 = insert_test_build(&db).await?;

    // b1 contributes a narrow row; b2 is a member with NO wanted row.
    let fenced_outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b1,
                derivation_id: d1,
                wanted_output_names: &["out".to_string()],
            }],
        )
        .await?;
    assert!(fenced_outcome.settled());
    for b in [b1, b2] {
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(d1)
            .execute(&test_db.pool)
            .await?;
    }

    assert_eq!(
        db.effective_wanted_union(d1).await?,
        Some(Vec::new()),
        "a row-less live member saturates the union to all-declared"
    );

    // The row-less build going terminal un-saturates: b1's narrow row wins again.
    sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
        .bind(b2)
        .execute(&test_db.pool)
        .await?;
    assert_eq!(
        db.effective_wanted_union(d1).await?,
        Some(vec!["out".to_string()]),
        "saturation is live-scoped"
    );
    Ok(())
}

/// merged_bug_176 (bughunt wave): the per-(build, derivation) upsert is
/// a SATURATING UNION on conflict, matching
/// `rio_common::wanted_outputs::union_wanted_saturating` — a multi-root
/// submission recording the same derivation twice within one build must
/// accumulate demand, never replace it (the in-memory DAG fold and the
/// gateway dedup already union; the SQL row was the one LWW outlier).
// r[verify sched.materialize.job+2]
#[tokio::test]
async fn record_wanted_upsert_unions_not_replaces() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let d1 = insert_test_derivation(&db, "wanted-union-d1").await?;
    let b1 = insert_test_build(&db).await?;

    for names in [&["out".to_string()][..], &["doc".to_string()][..]] {
        let fenced_outcome = db
            .record_wanted_fenced(
                ServingGeneration::stamp_from_claim(1),
                &[WantedRow {
                    build_id: b1,
                    derivation_id: d1,
                    wanted_output_names: names,
                }],
            )
            .await?;
        assert!(fenced_outcome.settled());
    }
    let rows = raw_rows(&test_db.pool, d1).await?;
    assert_eq!(rows.len(), 1);
    assert_eq!(
        rows[0].1,
        vec!["doc".to_string(), "out".to_string()],
        "conflict unions (sorted, distinct) instead of replacing"
    );

    // Either side '{}' saturates the stored row to '{}' (= all).
    let fenced_outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b1,
                derivation_id: d1,
                wanted_output_names: &[],
            }],
        )
        .await?;
    assert!(fenced_outcome.settled());
    let rows = raw_rows(&test_db.pool, d1).await?;
    assert_eq!(rows[0].1, Vec::<String>::new(), "empty saturates the union");
    Ok(())
}

// r[verify sched.materialize.job+2]
/// merged_bug_059 class pin: the stored row equals the kernel fold
/// (`rio_common::wanted_outputs::union_wanted_saturating`) over
/// ARBITRARY contribution sequences — the falsify-twin of any
/// "last-write-wins" reading of the upsert (the header that claimed
/// exactly that is synced in this commit). 64 deterministic LCG-seeded
/// sequences over distinct-name subsets of {a, b, c} plus the '{}'
/// (= all declared) sentinel; the SQL row is checked against the
/// kernel fold after EVERY step, so a divergence names its full
/// sequence.
#[tokio::test]
async fn record_wanted_sql_equals_kernel_union_fold() -> anyhow::Result<()> {
    let (test_db, db) = setup().await;
    let mut state: u64 = 0x0059_1176;
    let mut next = move |bound: usize| -> usize {
        state = state
            .wrapping_mul(6364136223846793005)
            .wrapping_add(1442695040888963407);
        ((state >> 33) as usize) % bound
    };
    const ALPHABET: [&str; 3] = ["a", "b", "c"];
    for case in 0..64 {
        let d = insert_test_derivation(&db, &format!("wanted-prop-{case}")).await?;
        let b = insert_test_build(&db).await?;
        let len = 1 + next(5);
        let mut fold: Option<Vec<String>> = None;
        let mut seq: Vec<Vec<String>> = Vec::new();
        for _ in 0..len {
            // A contribution is either the '{}' sentinel or a
            // DISTINCT-name subset of the alphabet in mask order
            // (production contributions are distinct by construction).
            let contribution: Vec<String> = match next(5) {
                0 => Vec::new(),
                _ => {
                    let mask = 1 + next(7);
                    ALPHABET
                        .iter()
                        .enumerate()
                        .filter(|(i, _)| mask & (1 << i) != 0)
                        .map(|(_, n)| n.to_string())
                        .collect()
                }
            };
            seq.push(contribution.clone());
            let outcome = db
                .record_wanted_fenced(
                    ServingGeneration::stamp_from_claim(1),
                    &[WantedRow {
                        build_id: b,
                        derivation_id: d,
                        wanted_output_names: &contribution,
                    }],
                )
                .await?;
            assert!(outcome.settled());
            match &mut fold {
                None => fold = Some(contribution.clone()),
                Some(acc) => {
                    rio_common::wanted_outputs::union_wanted_saturating(acc, &contribution);
                }
            }
            let rows = raw_rows(&test_db.pool, d).await?;
            assert_eq!(
                rows.len(),
                1,
                "case {case}: exactly one row per (build, drv)"
            );
            assert_eq!(
                &rows[0].1,
                fold.as_ref().expect("fold seeded"),
                "case {case}: stored row diverged from the kernel fold after {seq:?}"
            );
        }
    }
    Ok(())
}

// r[verify sched.materialize.job+2]
/// merged_bug_059 (3): the DQ-2 saturation note is a function of the
/// row SET, not of PG heap order. An explicit '{}' row (b1, inserted
/// FIRST so the old early-return hit it first) coexists with a legacy
/// defaulted row (b2, build_derivations member with no wanted row):
/// the union saturates either way, and the saturation note MUST fire
/// for the defaulted row regardless of which row the scan sees first.
/// Pre-fix RED (strawman: return-at-first-empty restored): the
/// counter stayed at zero — `left: 0 / right: 1`.
#[tokio::test]
async fn saturation_note_is_order_independent() -> anyhow::Result<()> {
    use metrics_util::debugging::DebuggingRecorder;

    let (test_db, db) = setup().await;
    let d1 = insert_test_derivation(&db, "wanted-order-d1").await?;
    let b1 = insert_test_build(&db).await?;
    let b2 = insert_test_build(&db).await?;

    // b1: EXPLICIT '{}' (all-declared) contribution — inserted first.
    let outcome = db
        .record_wanted_fenced(
            ServingGeneration::stamp_from_claim(1),
            &[WantedRow {
                build_id: b1,
                derivation_id: d1,
                wanted_output_names: &[],
            }],
        )
        .await?;
    assert!(outcome.settled());
    // Both builds are live members; b2 has NO wanted row (the legacy
    // defaulted shape — `saturated_default` in the 086 view).
    for b in [b1, b2] {
        sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
            .bind(b)
            .bind(d1)
            .execute(&test_db.pool)
            .await?;
    }

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let union = {
        let _g = metrics::set_default_local_recorder(&rec);
        db.effective_wanted_union(d1).await?
    };
    assert_eq!(union, Some(Vec::new()), "the union saturates");
    let counters = crate::sla::metrics::counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_wanted_width_saturated_total")
            .copied()
            .unwrap_or(0),
        1,
        "the defaulted row's saturation is noted regardless of row order"
    );
    Ok(())
}
