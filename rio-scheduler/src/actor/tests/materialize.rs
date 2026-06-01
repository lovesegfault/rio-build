//! Materialization-job actor batteries (substitution-replacement
//! Phase A): the dormancy pin for job creation (flag-off, the deployed
//! state, creates nothing and the as-built walk runs unchanged) and the
//! flag-on creation paths (dispatch-probe partition, merge new_sub
//! lane in-tx, pruned origin in-tx, dedup).

use super::*;
use crate::state::{DerivationStatus, JobOrigin};

/// Open a `SchedulerDb` over the test pool (the same pub(crate) db
/// surface production uses; the actor's own handle is private).
fn sdb(pool: &sqlx::PgPool) -> crate::db::SchedulerDb {
    crate::db::SchedulerDb::new(pool.clone())
}

// r[verify sched.materialize.job]
/// FLAG OFF (default): a merge + dispatch cycle for a substitutable-
/// upstream node creates ZERO job rows, ZERO wanted-relation rows, and
/// spawns the as-built substitution walk exactly as baseline (the
/// walk's SubstituteComplete lands, the node completes via the
/// as-built path). The dormancy pin for job creation.
///
/// NOTE: this is a PIN, not a red-first test — it passes before the
/// creation paths exist (nothing can create rows) and must keep
/// passing after they land (they are flag-gated off).
#[tokio::test]
async fn flag_off_merge_dispatch_creates_no_materialization_state() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("matoff-out");
    let mut n = make_node("matoff");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Seed substitutable AFTER merge so the dispatch-time probe (not
    // the merge classification) is the deciding site — the same shape
    // as the as-built dispatch_time_substitutable_completes battery.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    settle_substituting(&handle, &["matoff"]).await;
    tick(&handle).await?;

    // As-built completion happened (node Completed via the walk).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "flag-off must complete via the as-built substitution walk"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            qpi.contains(&out),
            "flag-off the detached walk runs exactly as baseline; qpi_calls={qpi:?}"
        );
    }

    // THE dormancy pin: zero materialization rows after the full cycle.
    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(
        (jobs, wanted),
        (0, 0),
        "flag-off must create no materialization rows"
    );
    Ok(())
}

// r[verify sched.materialize.job]
/// FLAG ON: the same merge + dispatch cycle creates exactly ONE job
/// (origin=cache_opportunity) at the dispatch-probe partition, writes
/// the wanted relation for the (build, node) pair, does NOT spawn the
/// walk, and the node stays Ready (claimable) instead of going
/// Substituting.
#[tokio::test]
async fn flag_on_probe_partition_creates_job_instead_of_walk() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    let out = test_store_path("maton-probe-out");
    let mut n = make_node("maton-probe");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Not substitutable at merge time (nothing seeded) → the merge
    // classifies nothing; the node seeds Ready. Seed + tick → the
    // dispatch-probe partition routes it to a job instead of the walk.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;

    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "exactly one job, got {jobs:?}");
    assert_eq!(
        jobs[0].origin,
        JobOrigin::CacheOpportunity,
        "the dispatch-probe site creates cache_opportunity jobs"
    );
    assert_eq!(jobs[0].drv_hash, "maton-probe");

    // The node stays Ready (claimable by a store replica) — never
    // Substituting; the walk was not spawned.
    let drv = expect_drv(&handle, "maton-probe").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Ready,
        "flag-on the job row is the in-flight marker; the node stays Ready"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out),
            "flag-on the detached walk must NOT spawn; qpi_calls={qpi:?}"
        );
    }

    // The wanted relation was recorded for the creating build (the
    // merge writes it for every (build, node) pair flag-on).
    let (_, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(wanted, 1, "one (build, node) wanted-relation row");
    Ok(())
}

// r[verify sched.materialize.job]
/// FLAG ON: two builds merging the same substitutable node produce ONE
/// job (the dedup — the C3-class protection, now database-enforced),
/// while both builds' wanted relations are recorded.
#[tokio::test]
async fn flag_on_concurrent_interest_creates_one_job() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    let out = test_store_path("maton-dedup-out");
    // Substitutable BEFORE the first merge: build A's merge classifies
    // the node pending_substitute → the new_sub-lane in-tx creation.
    store.state.substitutable.write().unwrap().push(out.clone());

    let mk = || {
        let mut n = make_node("maton-dedup");
        n.expected_output_paths = vec![out.clone()];
        n
    };

    let build_a = Uuid::new_v4();
    merge_dag(&handle, build_a, vec![mk()], vec![], false).await?;
    barrier(&handle).await;

    // The dispatch probe (inside the merge command and on tick) also
    // sees the Ready substitutable node — the dedup must keep it ONE job.
    tick(&handle).await?;
    barrier(&handle).await;

    // A second build merges the same node: its wanted relation is
    // recorded; the unresolved-job dedup still holds.
    let build_b = Uuid::new_v4();
    merge_dag(&handle, build_b, vec![mk()], vec![], false).await?;
    barrier(&handle).await;

    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(
        jobs, 1,
        "the partial-unique-index dedup: at most one unresolved job per derivation"
    );
    assert_eq!(wanted, 2, "both builds' wanted-relation rows recorded");
    Ok(())
}

// r[verify sched.materialize.job]
/// FLAG ON: the prune origin — a topdown-pruned kept root creates a job
/// with origin=pruned IN the merge transaction (adjudication PDQ-9 /
/// design A13/B6).
///
/// The in-tx property's actor-level discrimination: the dispatch-probe
/// site (which runs AFTER persist, at the merge command's trailing
/// sweep and on every tick) creates cache_opportunity jobs; the merge
/// transaction creates the pruned-origin job FIRST. The unresolved-job
/// dedup keeps the first writer's row, so origin == pruned proves the
/// in-tx site exists and ran before every post-commit site. The
/// created_generation == the merge's serving generation pins the
/// fence stamp.
#[tokio::test]
async fn flag_on_pruned_root_creates_job_at_merge() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // The canonical real-prune setup (test_topdown_root_substitutable_
    // prunes_deps): root output substitutable BEFORE merge, root → dep
    // edges, prune fires and keeps the root only.
    let root_out = test_store_path("maton-prune-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());

    let mut root = make_node("maton-prune-root");
    root.expected_output_paths = vec![root_out.clone()];
    let mut dep = make_node("maton-prune-dep");
    dep.expected_output_paths = vec![test_store_path("maton-prune-dep-out")];
    let nodes = vec![root, dep];
    let edges = vec![make_test_edge("maton-prune-root", "maton-prune-dep")];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "exactly one job, got {jobs:?}");
    assert_eq!(
        jobs[0].origin,
        JobOrigin::Pruned,
        "the kept root's job carries the pruned origin (the in-tx merge site won, \
         not the post-commit probe site)"
    );
    assert_eq!(jobs[0].drv_hash, "maton-prune-root");
    assert_eq!(
        jobs[0].created_generation, 1,
        "created with the merge transaction's serving generation (always-leader = 1)"
    );

    // The kept root stays Ready (claimable); the walk never spawned.
    let drv = expect_drv(&handle, "maton-prune-root").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Ready,
        "flag-on the pruned root stays Ready instead of going Substituting"
    );
    assert!(
        drv.topdown_pruned,
        "the topdown_pruned stamp still applies (the job complements it in Phase A)"
    );
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        !qpi.contains(&root_out),
        "flag-on no walk spawns for the pruned root; qpi_calls={qpi:?}"
    );
    Ok(())
}

// ── The four-arm Unobtainable routing core (pure — no PG) ──────────────

use crate::actor::materialize::{
    DurableEvidence, ReprobeAnswer, RoutingInputs, UnobtainableRouting, route_unobtainable,
    success_covers_live_wanted,
};

fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

// r[verify sched.materialize.routing]
/// Arm 0 (moot-failure / the C3 arm): missing ∩ live-wanted = ∅ and
/// verified ⊇ live-wanted → CompleteForLiveInterest. The design's
/// §2.4 confirmed-C3-trace replay, steps 4–5: b2 cancelled in the
/// report→consume window; b1's narrower wants are covered.
#[test]
fn routing_moot_failure_completes_for_live_interest() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out2-path"]),
        verified_paths: &paths(&["out1-path"]),
        live_wanted_paths: &paths(&["out1-path"]), // b2 gone; b1 wants out1 only
        durable_evidence: DurableEvidence::Broken, // irrelevant for arm 0
        prior_unobtainable_count: 0,
        reprobe: None,
    });
    assert_eq!(routing, UnobtainableRouting::CompleteForLiveInterest);
}

/// Arm 0 residual: moot but not covered → re-arm (never fail-fast).
#[test]
fn routing_moot_but_uncovered_rearms() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out2-path"]),
        verified_paths: &paths(&[]),
        live_wanted_paths: &paths(&["out1-path"]),
        durable_evidence: DurableEvidence::Broken,
        prior_unobtainable_count: 0,
        reprobe: None,
    });
    assert_eq!(routing, UnobtainableRouting::ReArm);
}

/// Arm 1: durable Vouched → ResolveFromSource.
#[test]
fn routing_durable_vouched_resolves_from_source() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out-path"]),
        verified_paths: &paths(&[]),
        live_wanted_paths: &paths(&["out-path"]),
        durable_evidence: DurableEvidence::Vouched,
        prior_unobtainable_count: 0,
        reprobe: None,
    });
    assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
}

/// Arm 2: durable Pending → ResolveFromSource (normal dep gating).
#[test]
fn routing_durable_pending_resolves_from_source() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out-path"]),
        verified_paths: &paths(&[]),
        live_wanted_paths: &paths(&["out-path"]),
        durable_evidence: DurableEvidence::Pending,
        prior_unobtainable_count: 0,
        reprobe: None,
    });
    assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
}

/// Arm 3a: Broken + live-wanted missing + re-probe says obtainable +
/// one-shot unspent → re-arm.
#[test]
fn routing_broken_with_obtainable_reprobe_rearms_once() {
    // hold the path vecs alive for the borrow
    let missing = paths(&["out-path"]);
    let verified = paths(&[]);
    let live = paths(&["out-path"]);
    let mk = |prior: u32, reprobe| RoutingInputs {
        missing_paths: &missing,
        verified_paths: &verified,
        live_wanted_paths: &live,
        durable_evidence: DurableEvidence::Broken,
        prior_unobtainable_count: prior,
        reprobe,
    };
    // One-shot unspent + obtainable → ReArm.
    assert_eq!(
        route_unobtainable(&mk(0, Some(ReprobeAnswer::Obtainable))),
        UnobtainableRouting::ReArm
    );
    // One-shot SPENT (a prior unobtainable row exists) → FailFast even
    // when the re-probe says obtainable.
    assert_eq!(
        route_unobtainable(&mk(1, Some(ReprobeAnswer::Obtainable))),
        UnobtainableRouting::FailFast
    );
}

/// Arm 3b: Broken + live-wanted missing + (re-probe confirms missing OR
/// one-shot spent) → FailFast. The ONLY arm that fail-fasts, and it
/// requires all three conjuncts (the design's §2.4 closing claim).
#[test]
fn routing_fail_fast_requires_all_three_conjuncts() {
    let missing_hit = paths(&["out-path"]);
    let missing_miss = paths(&["unwanted-path"]);
    let verified = paths(&[]);
    let verified_covering = paths(&["out-path"]);
    let live = paths(&["out-path"]);
    // Exhaustive over the 8 combinations of (missing∩W≠∅, evidence
    // Broken, reprobe-confirms-or-spent): exactly one yields FailFast.
    let mut fail_fast_count = 0;
    for missing_intersects in [false, true] {
        for broken in [false, true] {
            for confirms_or_spent in [false, true] {
                let inputs = RoutingInputs {
                    missing_paths: if missing_intersects {
                        &missing_hit
                    } else {
                        &missing_miss
                    },
                    // When the missing set does not intersect, make the
                    // live set covered so arm 0 takes its Complete arm
                    // (the uncovered residual is ReArm — also not
                    // FailFast, so either choice serves the conjunct
                    // counting).
                    verified_paths: if missing_intersects {
                        &verified
                    } else {
                        &verified_covering
                    },
                    live_wanted_paths: &live,
                    durable_evidence: if broken {
                        DurableEvidence::Broken
                    } else {
                        DurableEvidence::Vouched
                    },
                    prior_unobtainable_count: u32::from(confirms_or_spent),
                    reprobe: Some(if confirms_or_spent {
                        ReprobeAnswer::ConfirmedMissing
                    } else {
                        ReprobeAnswer::Obtainable
                    }),
                };
                let routing = route_unobtainable(&inputs);
                if routing == UnobtainableRouting::FailFast {
                    fail_fast_count += 1;
                    assert!(
                        missing_intersects && broken && confirms_or_spent,
                        "FailFast outside the three-conjunct corner: \
                         missing∩W={missing_intersects} broken={broken} \
                         confirmed/spent={confirms_or_spent}"
                    );
                }
            }
        }
    }
    assert_eq!(
        fail_fast_count, 1,
        "exactly one of the 8 combinations may fail-fast"
    );
}

/// Success coverage: reported ⊇ live-wanted → covered (Complete); else
/// not covered (ReArm — the CE-17 class: interest grew between
/// execution and consumption).
#[test]
fn success_consumption_coverage_check() {
    // Covered: ingested + verified together cover the live wanted set.
    assert!(success_covers_live_wanted(
        &paths(&["out1"]),
        &paths(&["out2"]),
        &paths(&["out1", "out2"]),
    ));
    // Not covered: a live-wanted path is in neither set.
    assert!(!success_covers_live_wanted(
        &paths(&["out1"]),
        &paths(&[]),
        &paths(&["out1", "out2"]),
    ));
    // Empty live-wanted set is vacuously covered.
    assert!(success_covers_live_wanted(
        &paths(&[]),
        &paths(&[]),
        &paths(&[])
    ));
}

/// InfraFailure routing is decided by the budget (the kernel's
/// materialization_decide), never by route_unobtainable: the routing
/// core has no infra arm at all — there is no input that produces a
/// FailFast or ResolveFromSource from an infra failure (B3). This pin
/// asserts the core's domain is the Unobtainable report only, by
/// checking that a moot infra-shaped input (no missing paths at all)
/// routes to the non-failing arms.
#[test]
fn infra_failure_never_failfasts_never_routes_from_source() {
    // An infra failure carries NO missing/verified paths (nothing was
    // confirmed). Mapped onto the core's vocabulary that is the
    // empty-missing input — which can only ever produce arm 0
    // (Complete when covered / ReArm when not), never FailFast.
    let live = paths(&["out-path"]);
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&[]),
        verified_paths: &paths(&[]),
        live_wanted_paths: &live,
        durable_evidence: DurableEvidence::Broken,
        prior_unobtainable_count: 99,
        reprobe: Some(ReprobeAnswer::ConfirmedMissing),
    });
    assert!(
        matches!(
            routing,
            UnobtainableRouting::ReArm | UnobtainableRouting::CompleteForLiveInterest
        ),
        "an empty missing set (the infra shape) can never fail-fast or route from source, \
         got {routing:?}"
    );
}

// ── The consumption transaction (handler level, PG-backed) ─────────────

// r[verify sched.materialize.routing]
/// A BUILD attempt receiving a payload with materialization_outcome set
/// is acknowledged-and-ignored: no ledger row appended, no status
/// change, no job state touched. Reachable FLAG-OFF (any builder could
/// send this) — the warn+ack arm is a dormancy guarantee, not just a
/// flag-on routing rule (review finding RB-5).
#[tokio::test]
async fn build_attempt_with_materialization_payload_acked_and_ignored() -> TestResult {
    let (db, handle, _task) = setup().await; // flag-off
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "rb5-pin", PriorityClass::Scheduled).await?;

    // Mint a BUILD attempt via the as-built pull path.
    let assignment = pull_attempt(&handle, "rb5-pin").await;
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // Report a MATERIALIZATION payload against the build attempt.
    let result = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("rb5-pin".into()),
            payload: crate::actor::pull::PullReportPayload {
                result: rio_proto::types::BuildResult::default(),
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: 0,
                materialization_outcome: Some(rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::InfraFailure(
                            rio_proto::types::materialization_outcome::InfraFailure {
                                detail: "hostile payload".into(),
                            },
                        ),
                    ),
                }),
            },
            reply,
        })
        .await
        .expect("actor alive");
    assert!(result.is_ok(), "acknowledged, never an error: {result:?}");
    barrier(&handle).await;

    // Nothing changed: no ledger row, the node is still Running on its
    // open attempt, and no materialization rows exist.
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_attempts")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(attempts, 0, "no ledger row appended");
    let drv = expect_drv(&handle, "rb5-pin").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Running,
        "the open build attempt is untouched"
    );
    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!((jobs, wanted), (0, 0), "no materialization state touched");
    Ok(())
}

// r[verify sched.materialize.routing]
/// FLAG ON: an InfraFailure consumption charges materialization_infra
/// (kind=materialization — invisible to every build budget), the job
/// stays pending and claimable (under budget — never a fail-fast, B3),
/// and the node returns to Ready.
#[tokio::test]
async fn flag_on_infra_failure_charges_and_rearms() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // Job via the merge new_sub lane (substitutable before merge).
    let out = test_store_path("maton-infra-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-infra");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Claim it as a store replica (kind=Materialization, BC-1 identity).
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-infra".into(),
            auth_intent: Some("maton-infra".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0".into()),
            reply,
        })
        .await
        .expect("actor alive");
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("flag-on materialization claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The execution row carries the materialization kind.
    let kind: String =
        sqlx::query_scalar("SELECT attempt_kind FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(kind, "materialization", "the mint persists the work class");

    // Report an infrastructure failure.
    let result = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("maton-infra".into()),
            payload: crate::actor::pull::PullReportPayload {
                result: rio_proto::types::BuildResult::default(),
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: 0,
                materialization_outcome: Some(rio_proto::types::MaterializationOutcome {
                    outcome: Some(
                        rio_proto::types::materialization_outcome::Outcome::InfraFailure(
                            rio_proto::types::materialization_outcome::InfraFailure {
                                detail: "upstream 503".into(),
                            },
                        ),
                    ),
                }),
            },
            reply,
        })
        .await
        .expect("actor alive");
    assert!(result.is_ok(), "consumption must succeed: {result:?}");
    barrier(&handle).await;

    // The charge: exactly one ledger row, class=materialization_infra,
    // joined kind=materialization (invisible to build budgets by the
    // kernel's kind partition).
    let (class, joined_kind): (String, String) = sqlx::query_as(
        "SELECT t.outcome_class, COALESCE(e.attempt_kind, 'build') \
           FROM drv_attempts t \
           LEFT JOIN drv_executions e ON e.exec_id = t.exec_id",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(class, "materialization_infra");
    assert_eq!(joined_kind, "materialization");

    // The job is still pending and claimable again (under budget:
    // 1 infra row < max_attempts=3; the closed assignment no longer
    // blocks the anti-join).
    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(
        jobs.len(),
        1,
        "the job re-armed (pending, unclaimed, unparked), got {jobs:?}"
    );

    // The node returned to Ready (re-claimable / dispatchable).
    let drv = expect_drv(&handle, "maton-infra").await;
    assert_eq!(drv.status, DerivationStatus::Ready, "the node requeued");
    Ok(())
}
