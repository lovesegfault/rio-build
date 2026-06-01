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

// ── Establishment + cancellation (T-3.6) ───────────────────────────────

// r[verify sched.materialize.routing]
/// A dead store replica's open materialization attempt is established
/// as materialization_infra — never executor_crash, never adopted —
/// and the job returns to pending (claimable again). BC-2/BC-3: no
/// adopt arm for the materialization kind (the outputs ARE present in
/// the store here — the adopt-arm bait — and the establishment still
/// charges instead of completing); the charge is invisible to build
/// budgets by the kernel kind partition.
#[tokio::test]
async fn establishment_writes_materialization_infra_never_adopts() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // Job via the merge new_sub lane + a store-replica claim.
    let out = test_store_path("maton-est-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-est");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-est".into(),
            auth_intent: Some("maton-est".into()),
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

    // The adopt-arm bait: the wanted output IS present in the store.
    store
        .state
        .paths
        .write()
        .unwrap()
        .insert(out.clone(), Default::default());

    // Age the attempt past any deadline + slack, then sweep.
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(&db.pool)
    .await?;
    tick(&handle).await?;
    barrier(&handle).await;

    // Exactly one charge row: materialization_infra (never
    // executor_crash — BC-2) with the materialization kind. The adopt
    // arm would have written NO row at all (and completed the node);
    // the as-built C2 arm would have written executor_crash.
    let rows: Vec<(String, String)> = sqlx::query_as(
        "SELECT t.outcome_class, COALESCE(e.attempt_kind, 'build') \
           FROM drv_attempts t \
           LEFT JOIN drv_executions e ON e.exec_id = t.exec_id",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(rows.len(), 1, "exactly one establishment row, got {rows:?}");
    assert_eq!(
        rows[0].0, "materialization_infra",
        "the charge class is materialization_infra, never executor_crash (BC-2)"
    );
    assert_eq!(rows[0].1, "materialization");

    // The job returned to pending (claimable again): never resolved by
    // the establishment, never adopted-as-success.
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "pending", "the job stays pending (claimable)");
    Ok(())
}

// r[verify sched.materialize.job]
/// Cancellation: when the last live interested build goes terminal, the
/// housekeeping backstop (flag-gated) cancels the job and closes any
/// open materialization attempt CHARGE-FREE — no charge row of any
/// class is appended (BC-2's no-controller closer).
#[tokio::test]
async fn cancellation_closes_open_attempt_charge_free() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    let out = test_store_path("maton-cancel-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-cancel");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-cancel".into(),
            auth_intent: Some("maton-cancel".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0".into()),
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "claim delivered, got {outcome:?}"
    );

    // The last (only) live interested build goes terminal.
    cancel_build(&handle, build_id).await?;
    barrier(&handle).await;
    // The flag-gated housekeeping backstop runs the closer.
    tick(&handle).await?;
    barrier(&handle).await;

    // Charge-free: no charge row of ANY class for the closed attempt
    // (cancellation is not a failure; the budget is untouched).
    let charges: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drv_attempts \
          WHERE outcome_class IN ('materialization_infra', 'materialization_unobtainable', \
                                  'executor_crash', 'infra', 'transient')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(charges, 0, "the cancellation close is charge-free (BC-2)");

    // The job is cancelled (terminal, never claimable again).
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "cancelled");

    // The assignment row is closed (no open attempt remains).
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(open, 0, "the open attempt was closed");
    Ok(())
}

// ── T-6.2: the Phase A flag-on smoke battery (the dormancy proof's
//    "dormant ≠ vestigial" half) ─────────────────────────────────────────

/// A materialization Success outcome covering `ingested` + `verified`.
fn mat_success_outcome(
    ingested: Vec<String>,
    verified: Vec<String>,
) -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
            rio_proto::types::materialization_outcome::Success {
                ingested_paths: ingested,
                verified_paths: verified,
            },
        )),
    }
}

/// A materialization Unobtainable outcome.
fn mat_unobtainable_outcome(
    missing: Vec<String>,
    verified: Vec<String>,
    cause: &str,
) -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(
            rio_proto::types::materialization_outcome::Outcome::Unobtainable(
                rio_proto::types::materialization_outcome::Unobtainable {
                    missing_paths: missing,
                    verified_paths: verified,
                    cause: cause.into(),
                },
            ),
        ),
    }
}

/// A materialization InfraFailure outcome.
fn mat_infra_outcome(detail: &str) -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(
            rio_proto::types::materialization_outcome::Outcome::InfraFailure(
                rio_proto::types::materialization_outcome::InfraFailure {
                    detail: detail.into(),
                },
            ),
        ),
    }
}

/// Claim the materialization job for `drv` as store replica `instance`
/// (kind=Materialization; the BC-1 composite identity `{drv}@{instance}`).
async fn claim_materialization(
    handle: &ActorHandle,
    drv: &str,
    instance: &str,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: drv.into(),
            auth_intent: Some(drv.into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some(instance.into()),
            reply,
        })
        .await
        .expect("actor alive")
}

/// Report a materialization outcome against an open attempt's exec_id
/// through the production report intake.
async fn report_materialization_outcome(
    handle: &ActorHandle,
    exec_id: Uuid,
    intent: &str,
    outcome: rio_proto::types::MaterializationOutcome,
) -> Result<(), PullRejection> {
    let mut payload = pull_payload(rio_proto::types::BuildResult::default());
    payload.materialization_outcome = Some(outcome);
    handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some(intent.into()),
            payload,
            reply,
        })
        .await
        .expect("actor alive")
}

/// List claimable jobs through the production actor command (the path
/// `ExecutorService.ListMaterializationJobs` drives).
async fn list_materialization_jobs(
    handle: &ActorHandle,
    limit: u32,
) -> Vec<crate::actor::materialize::JobDescriptor> {
    handle
        .query_unchecked(|reply| ActorCommand::ListMaterializationJobs { limit, reply })
        .await
        .expect("actor alive")
}

// r[verify sched.materialize.job]
// r[verify sched.materialize.routing]
// r[verify sched.materialize.pinning]
/// THE Phase A keystone: one materialization job, end-to-end, flag-on,
/// exercising every dormant mechanism this campaign added in one
/// composed pass:
///
///   merge (interest recorded, wanted relation written)
///   → probe partition (job created instead of walk; node stays Ready)
///   → ListMaterializationJobs (the job is listed via the actor command)
///   → claim kind=MATERIALIZATION as "store-test-0" (fenced mint;
///     attempt_kind=materialization; node Running; one-winner: a second
///     claim from "store-test-1" gets NotYetReady)
///   → report InfraFailure (the materialization budget charge — the job
///     re-arms under budget; the charge row is the kind-partition
///     witness for the budget assertion below)
///   → re-claim → report Success covering the wanted set
///   → consumption (coverage check passes; node Completed; job
///     resolved_success; build Succeeded)
///
/// Plus the two partition pins:
///   - budget partition (`materializationInvisibleToBuildBudgets`,
///     production half): the node's ledger suffix — one
///     materialization_infra row — folds to the SAME build verdict as
///     an empty history.
///   - pin partition (review finding RB-5): a materialization mint
///     writes ZERO scheduler_live_pins rows for the drv (pin-at-ingest
///     is store-side).
///
/// NOTE (RT-4): this is an integration PIN, not a red-first test — the
/// individual mechanisms were red-first tested in Waves 3/4; this test
/// proves they compose.
#[tokio::test]
async fn flag_on_materialization_job_end_to_end() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // 1. Merge a 1-node build; the wanted output becomes substitutable
    //    only AFTER merge so the dispatch-probe partition (not the
    //    merge new_sub lane) is the creating site.
    let out = test_store_path("mat-e2e-out");
    let mut d1 = make_node("mat-e2e");
    d1.expected_output_paths = vec![out.clone()];
    d1.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![d1], vec![], false).await?;
    barrier(&handle).await;

    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?; // probe partition runs → job created

    // 2. The job exists and is listed through the production actor
    //    command; the node stays Ready (claimable), no walk spawned.
    let jobs = list_materialization_jobs(&handle, 16).await;
    assert_eq!(jobs.len(), 1, "exactly one job listed, got {jobs:?}");
    assert_eq!(jobs[0].drv_hash, "mat-e2e");
    assert_eq!(jobs[0].origin, JobOrigin::CacheOpportunity);
    assert_eq!(
        expect_drv(&handle, "mat-e2e").await.status,
        DerivationStatus::Ready,
        "the job row is the in-flight marker; the node stays Ready"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(!qpi.contains(&out), "no walk spawned; qpi_calls={qpi:?}");
    }

    // 3. Claim it as store replica 0; verify the one-winner arbiter.
    let assignment = match claim_materialization(&handle, "mat-e2e", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the first claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let second = claim_materialization(&handle, "mat-e2e", "store-test-1").await;
    assert!(
        matches!(second, Ok(PullOutcome::NotYetReady { .. })),
        "one-winner arbitration: a second replica's claim parks, got {second:?}"
    );
    let kind: String =
        sqlx::query_scalar("SELECT attempt_kind FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(kind, "materialization", "the mint persists the work class");
    assert_eq!(
        expect_drv(&handle, "mat-e2e").await.status,
        DerivationStatus::Running,
        "the open attempt holds the node Running"
    );

    // 4. The winner reports an infrastructure failure → exactly one
    //    materialization_infra charge; the job re-arms (1 < max_attempts).
    report_materialization_outcome(
        &handle,
        exec_id,
        "mat-e2e",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // 5. Re-claim (the re-armed job) and report Success covering the
    //    wanted set.
    let assignment = match claim_materialization(&handle, "mat-e2e", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the re-claim after re-arm must deliver, got {other:?}"),
    };
    let exec_id2: Uuid = assignment.exec_id.parse()?;
    assert_ne!(exec_id, exec_id2, "the re-claim opens a fresh attempt");
    store.seed_with_content(&out, b"materialized");
    report_materialization_outcome(
        &handle,
        exec_id2,
        "mat-e2e",
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    // 6. Consumption: node Completed, job resolved_success, build
    //    Succeeded, no unresolved job remains.
    assert_eq!(
        expect_drv(&handle, "mat-e2e").await.status,
        DerivationStatus::Completed,
        "the Success consumption completes the node"
    );
    let st = query_status(&handle, build_id).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the creating build succeeds through the consumption"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "resolved_success");
    let unresolved: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE state = 'pending'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(unresolved, 0, "no unresolved job remains");

    // 7. The budget partition (`materializationInvisibleToBuildBudgets`,
    //    production half): the suffix carries exactly the stage-4 infra
    //    charge — kind=materialization — and decide() folds it to the
    //    SAME verdict as an empty history for build budgets.
    let derivation_id: Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind("mat-e2e")
            .fetch_one(&db.pool)
            .await?;
    let suffix = sdb(&db.pool).load_attempt_suffix(&[derivation_id]).await?;
    let rows = suffix.get(&derivation_id).cloned().unwrap_or_default();
    assert_eq!(
        rows.len(),
        1,
        "exactly the stage-4 infra charge row, got {rows:?}"
    );
    assert_eq!(
        rows[0].attempt_kind,
        crate::state::AttemptKind::Materialization
    );
    assert_eq!(
        rows[0].outcome_class,
        crate::state::OutcomeClass::MaterializationInfra
    );
    let records: Vec<crate::state::AttemptRecord> = rows.iter().map(|r| r.to_record()).collect();
    let now = crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
    let budget = crate::retry_policy::Budget::default();
    assert_eq!(
        crate::retry_policy::decide(&records, &budget, now),
        crate::retry_policy::decide(&[], &budget, now),
        "the materialization charge must be invisible to every build budget"
    );

    // 8. The pin partition (RB-5): zero scheduler_live_pins rows for the
    //    drv — both mints skipped pin_live_inputs.
    let pins: i64 =
        sqlx::query_scalar("SELECT count(*) FROM scheduler_live_pins WHERE drv_hash = $1")
            .bind("mat-e2e")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        pins, 0,
        "materialization mints must not write build_input pins"
    );
    Ok(())
}

// r[verify sched.materialize.routing]
/// The Unobtainable moot arm (the C3 trace), flag-on, end-to-end through
/// the actor: report Unobtainable for a path no LIVE build wants → the
/// node completes for live interest, NEVER fail-fasts. The design §2.4
/// confirmed-C3-trace replay as a production test (AS-2/PP-1): b2 (the
/// build wanting the missing output) joins and then cancels INSIDE the
/// claim→consume window; b1's narrower wants are fully covered by the
/// verified set.
///
/// Sequencing note (the OQ7/PD-17 partial-coexistence boundary): b2
/// merges while the materialization attempt is OPEN (node Running). A
/// merge against a Ready node would route it through the I-099
/// existing-node reprobe lane, which keeps spawning as-built walks
/// flag-on (PD-17 — its job creation is Phase B work); the open
/// attempt is what keeps the C3 window walk-free, exactly as the
/// design's trace has it.
#[tokio::test]
async fn flag_on_moot_unobtainable_never_fail_fasts() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // One 2-output node; two builds with different wants:
    //   b1 wants out1 only; b2 wants out1 + out2.
    let out1 = test_store_path("mat-moot-out1");
    let out2 = test_store_path("mat-moot-out2");
    // Substitutable BEFORE merge → the merge new_sub lane creates the job.
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(out1.clone());
        subs.push(out2.clone());
    }

    let mk = |wanted: &[&str]| {
        let mut n = make_node("mat-moot");
        n.output_names = vec!["out1".into(), "out2".into()];
        n.expected_output_paths = vec![out1.clone(), out2.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // b1 merges → the new_sub lane creates the job in-tx; the node
    // stays Ready (no walk).
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![mk(&["out1"])], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "mat-moot").await.status,
        DerivationStatus::Ready,
        "flag-on the merge creates the job and the node stays Ready"
    );

    // Claim it as a store replica → the attempt opens, node Running.
    let assignment = match claim_materialization(&handle, "mat-moot", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // b2 joins while the attempt is open (the node is Running, so b2's
    // merge does NOT reprobe it — no walk spawns), widening live
    // interest to out1+out2.
    let b2 = Uuid::new_v4();
    merge_dag(&handle, b2, vec![mk(&["out1", "out2"])], vec![], false).await?;
    barrier(&handle).await;

    // One job (the dedup), both builds' wanted relations recorded.
    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(
        (jobs, wanted),
        (1, 2),
        "one deduped job, two wanted-relation rows"
    );

    // b2 cancels in the claim→consume window: its wants leave the live
    // join; only b1's narrower wants (out1) remain live.
    cancel_build(&handle, b2).await?;
    barrier(&handle).await;

    // Report Unobtainable{missing=[out2], verified=[out1]}: out2 is
    // confirmed absent upstream but NO live build wants it (the moot
    // conjunct), and everything live interest wants (out1) was verified
    // present.
    report_materialization_outcome(
        &handle,
        exec_id,
        "mat-moot",
        mat_unobtainable_outcome(
            vec![out2.clone()],
            vec![out1.clone()],
            "upstream 404 on out2",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // The moot arm (arm 0): node Completed for live interest, b1
    // Succeeded, job resolved_success — NEVER a fail-fast.
    assert_eq!(
        expect_drv(&handle, "mat-moot").await.status,
        DerivationStatus::Completed,
        "the C3/moot arm completes the node for live interest"
    );
    let st = query_status(&handle, b1).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "b1 (the surviving build) succeeds — never a fail-fast"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "resolved_success");

    // The charge row exists (Unobtainable IS a fold event for the
    // materialization budget) but carries the materialization kind —
    // invisible to build budgets — and the node was never poisoned.
    let (class, joined_kind): (String, String) = sqlx::query_as(
        "SELECT t.outcome_class, COALESCE(e.attempt_kind, 'build') \
           FROM drv_attempts t \
           LEFT JOIN drv_executions e ON e.exec_id = t.exec_id",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(class, "materialization_unobtainable");
    assert_eq!(joined_kind, "materialization");
    Ok(())
}

// ── T-1.1 (Phase B): §2.6 consumer re-sourcing — the snapshot buckets ──

// r[verify sched.admin.snapshot-substituting+2]
// r[verify ctrl.scaler.signal-substituting+2]
/// §2.6 re-sourcing: pending (unclaimed) materialization jobs ARE the
/// substituting bucket flag-on. A Ready or Queued node carrying an
/// unresolved unclaimed job counts in `substituting_derivations` and is
/// EXCLUDED from `queued_derivations`/`queued_by_system` (the buckets
/// stay disjoint — builder autoscalers must not scale on work that will
/// be materialized); a claimed job's node is Assigned/Running and counts
/// in `running_derivations` by construction.
#[tokio::test]
async fn flag_on_pending_jobs_count_as_substituting_bucket() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // 3-node chain root → mid → leaf where mid and leaf are
    // substitutable but the demanded root is NOT (so the topdown prune
    // does not fire). The merge new_sub lane creates jobs for mid +
    // leaf in-tx; seed_initial_states leaves leaf Ready, mid Queued
    // (behind the unproduced leaf), root Queued.
    let mid_out = test_store_path("sub-bucket-mid-out");
    let leaf_out = test_store_path("sub-bucket-leaf-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(mid_out.clone());
        subs.push(leaf_out.clone());
    }

    let root = make_node("sub-bucket-root");
    let mut mid = make_node("sub-bucket-mid");
    mid.expected_output_paths = vec![mid_out.clone()];
    let mut leaf = make_node("sub-bucket-leaf");
    leaf.expected_output_paths = vec![leaf_out.clone()];
    let nodes = vec![root, mid, leaf];
    let edges = vec![
        make_test_edge("sub-bucket-root", "sub-bucket-mid"),
        make_test_edge("sub-bucket-mid", "sub-bucket-leaf"),
    ];
    let build_id = Uuid::new_v4();
    // Hold the event receiver: cfg(test) ORPHAN_BUILD_GRACE is ZERO, so a
    // dropped receiver + two ticks auto-cancels the build mid-test.
    let _ev = merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // Both jobs exist; leaf is Ready, mid is Queued.
    assert_eq!(
        expect_drv(&handle, "sub-bucket-leaf").await.status,
        DerivationStatus::Ready
    );
    assert_eq!(
        expect_drv(&handle, "sub-bucket-mid").await.status,
        DerivationStatus::Queued
    );

    // Tick refreshes the cached snapshot.
    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations, 2,
        "one Ready+job and one Queued+job node both count as substitution backlog \
         (the §2.6 re-sourcing); got {snap:?}"
    );
    assert_eq!(
        snap.queued_derivations, 0,
        "pending-job nodes are excluded from queued_derivations (bucket disjointness)"
    );
    assert_eq!(
        snap.queued_by_system.values().sum::<u32>(),
        0,
        "pending-job nodes are excluded from queued_by_system"
    );
    assert_eq!(snap.running_derivations, 0);

    // Claim the leaf's job → the node goes Assigned/Running (claimed
    // jobs leave the substituting bucket and surface as running).
    let claim = claim_materialization(&handle, "sub-bucket-leaf", "store-test-0").await;
    assert!(
        matches!(claim, Ok(PullOutcome::Deliver(_))),
        "the leaf's job must be claimable, got {claim:?}"
    );
    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations, 1,
        "the claimed job's node leaves the substituting bucket (mid's pending job remains)"
    );
    assert_eq!(
        snap.running_derivations, 1,
        "the claimed job's node counts as running (Assigned/Running by the mint)"
    );
    assert_eq!(snap.queued_derivations, 0);
    Ok(())
}

// r[verify sched.admin.snapshot-substituting+2]
/// Equivalence criterion 2 / stop condition 8: flag-OFF the snapshot
/// buckets are byte-identical to the as-built status-only semantics —
/// Substituting counts in substituting_derivations, Ready in
/// queued_derivations — EVEN IF the in-memory job view somehow carries
/// entries (defense in depth: the bucket re-sourcing is gated on the
/// flag itself, not just on the view being empty). This test PINS
/// criterion 2 and must never change.
#[tokio::test]
async fn flag_off_snapshot_buckets_match_baseline() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone()); // flag-off default

    // 1 Substituting, 1 Ready, 1 Running — the as-built disjoint counts.
    actor.test_inject_ready("off-sub", None, "x86_64-linux", false);
    actor
        .dag
        .node_mut("off-sub")
        .unwrap()
        .set_status_for_test(DerivationStatus::Substituting);
    actor.test_inject_ready("off-ready", None, "x86_64-linux", false);
    actor.test_inject_ready("off-run", None, "x86_64-linux", false);
    actor
        .dag
        .node_mut("off-run")
        .unwrap()
        .set_status_for_test(DerivationStatus::Running);

    // Defense-in-depth: even with a (production-unreachable) job-view
    // entry for the Ready node, flag-off buckets stay status-only.
    actor.materialization_jobs.insert(
        DrvHash::from("off-ready"),
        crate::actor::materialize::JobViewEntry {
            job_id: Uuid::new_v4(),
            parked_until: None,
            claimed_by: None,
        },
    );

    let snap = actor.compute_cluster_snapshot();
    assert_eq!(
        snap.substituting_derivations, 1,
        "flag-off: substituting counts ONLY Substituting-status nodes"
    );
    assert_eq!(
        snap.queued_derivations, 1,
        "flag-off: a Ready node counts in queued even when a job-view entry exists"
    );
    assert_eq!(snap.running_derivations, 1);
    assert_eq!(snap.queued_by_system.values().sum::<u32>(), 1);
    Ok(())
}

// ── T-1.2 (Phase B): BC-4 — SUBSTITUTING at claim, stop at consumption ──

/// Drain every Derivation event currently in the ring, returning the
/// kinds seen (in order).
fn drain_derivation_kinds(
    rx: &mut broadcast::Receiver<rio_proto::types::BuildEvent>,
) -> Vec<rio_proto::types::DerivationEventKind> {
    let mut kinds = Vec::new();
    while let Ok(event) = rx.try_recv() {
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = event.event {
            kinds.push(d.kind());
        }
    }
    kinds
}

// r[verify sched.materialize.job]
/// BC-4 (design §2.4 "Progress and gateway events"): flag-on, the
/// SUBSTITUTING DerivationEvent — the wire-retained kind the gateway's
/// actSubstitute/actCopyPath pair creation keys on — is emitted at
/// materialization-CLAIM intake (the walk-spawn site never runs for
/// fresh flag-on work), and the consumption Success path emits the
/// terminal CACHED event (the pair's stop trigger) through the
/// completion chokepoint. The claim must NOT emit STARTED (a
/// materialization claim is substitution work, not a builder
/// dispatch — STARTED is one of the gateway's pair-STOP triggers and
/// would close the pair the same instant it opened).
#[tokio::test]
async fn flag_on_claim_emits_substituting_event_and_consumption_stops_it() -> TestResult {
    use rio_proto::types::DerivationEventKind as K;
    let (_db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // Substitutable BEFORE merge → the merge new_sub lane creates the job.
    let out = test_store_path("bc4-claim-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("bc4-claim");
    n.expected_output_paths = vec![out.clone()];
    n.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    let mut ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // No SUBSTITUTING event before the claim: flag-on, job creation is
    // silent (the in-flight marker is the job row, not a status/event).
    let pre_claim = drain_derivation_kinds(&mut ev);
    assert!(
        !pre_claim.contains(&K::Substituting),
        "no SUBSTITUTING event may fire before the claim (got {pre_claim:?})"
    );

    // Claim → the SUBSTITUTING event fires at claim intake (BC-4).
    let assignment = match claim_materialization(&handle, "bc4-claim", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    barrier(&handle).await;

    let at_claim = drain_derivation_kinds(&mut ev);
    assert!(
        at_claim.contains(&K::Substituting),
        "the materialization claim emits the SUBSTITUTING event (BC-4 re-siting); got {at_claim:?}"
    );
    assert!(
        !at_claim.contains(&K::Started),
        "a materialization claim must NOT emit STARTED (it would stop the gateway pair \
         the moment it opened); got {at_claim:?}"
    );

    // Report Success → consumption completes the node → the terminal
    // CACHED event (the gateway pair's stop trigger) arrives through
    // the completion chokepoint.
    store.seed_with_content(&out, b"materialized");
    report_materialization_outcome(
        &handle,
        exec_id,
        "bc4-claim",
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    let at_consumption = drain_derivation_kinds(&mut ev);
    assert!(
        at_consumption.contains(&K::Cached),
        "the Success consumption emits the terminal CACHED event (the pair's stop); \
         got {at_consumption:?}"
    );
    Ok(())
}

// r[verify sched.materialize.job]
/// BC-4 flag-off invariance pin: the as-built walk's SUBSTITUTING
/// emission at spawn time is byte-identical — flag-off, the event still
/// fires when the walk spawns (NOT at any claim; no claims exist), and
/// the walk completion emits the terminal stop. This pins criterion 2
/// for the event surface and must never change.
#[tokio::test]
async fn flag_off_walk_substituting_events_unchanged() -> TestResult {
    use rio_proto::types::DerivationEventKind as K;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?; // flag-off

    let out = test_store_path("bc4-off-out");
    // Substitutable BEFORE merge → the as-built merge classification
    // spawns the walk (Substituting status + the SUBSTITUTING event).
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("bc4-off");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let mut ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    settle_substituting(&handle, &["bc4-off"]).await;
    barrier(&handle).await;

    let kinds = drain_derivation_kinds(&mut ev);
    assert!(
        kinds.contains(&K::Substituting),
        "flag-off the walk-spawn SUBSTITUTING emission is unchanged; got {kinds:?}"
    );
    // The walk completed the node → the terminal event closed the pair.
    assert!(
        kinds.contains(&K::Cached) || kinds.contains(&K::Completed),
        "flag-off the walk completion emits the terminal event; got {kinds:?}"
    );
    Ok(())
}

// r[verify sched.materialize.job]
// r[verify sched.state.machine+2]
/// PD-6 (Phase B, the PDQ-6 amendment's prescribed flip): a Queued node
/// (a parent behind an unproduced dep) with a pending job ACCEPTS a
/// flag-on materialization claim — DeliverNew, one drv_executions row
/// with attempt_kind='materialization', node transitions Queued →
/// Assigned → Running through the kinded mint edge. Materialization
/// does not wait for deps: the store fetches from upstream, so dep
/// state is irrelevant to the claim.
///
/// (Phase A pinned the opposite — Ready-only claims — as
/// `flag_on_queued_node_refuses_materialization_claim`; this is that
/// pin flipped red-first per the PDQ-6 amendment.)
#[tokio::test]
async fn flag_on_queued_node_accepts_materialization_claim() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;

    // 3-node chain root → mid → leaf where only MID's output is
    // substitutable:
    //   - the topdown prune does NOT fire (the demanded root is not
    //     substitutable),
    //   - the merge new_sub lane creates mid's job in-tx,
    //   - seed_initial_states leaves mid Queued (leaf is unproduced).
    let mid_out = test_store_path("mat-queued-mid-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(mid_out.clone());

    let root = make_node("mat-queued-root");
    let mut mid = make_node("mat-queued-mid");
    mid.expected_output_paths = vec![mid_out.clone()];
    let leaf = make_node("mat-queued-leaf");
    let nodes = vec![root, mid, leaf];
    let edges = vec![
        make_test_edge("mat-queued-root", "mat-queued-mid"),
        make_test_edge("mat-queued-mid", "mat-queued-leaf"),
    ];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // The job exists for mid (created by the merge new_sub lane) and
    // mid is Queued behind the unproduced leaf.
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "the merge new_sub lane created mid's job");
    assert_eq!(
        expect_drv(&handle, "mat-queued-mid").await.status,
        DerivationStatus::Queued,
        "mid is Queued behind the unproduced leaf"
    );

    // PD-6: the flag-on materialization claim against the Queued node
    // DELIVERS (the dep race is the point — materialization does not
    // wait for the leaf to build).
    let claim = claim_materialization(&handle, "mat-queued-mid", "store-test-0").await;
    let assignment = match claim {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("PD-6: a Queued node's materialization claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The fenced mint persisted exactly one materialization-kind
    // execution row.
    let kind: String =
        sqlx::query_scalar("SELECT attempt_kind FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(kind, "materialization", "the mint persists the work class");
    let execs: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_executions")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(execs, 1, "exactly one execution row minted");

    // The kinded mint edge: the node transitioned Queued → Assigned →
    // Running (never a stranded mint — the in-memory transition accepts
    // what the kernel admitted).
    assert_eq!(
        expect_drv(&handle, "mat-queued-mid").await.status,
        DerivationStatus::Running,
        "the Queued claim's mint transitions the node through the kinded edge"
    );
    Ok(())
}

// r[verify sched.materialize.job]
// r[verify sched.state.machine+2]
/// PD-6 mint-ordering / no-strand proof: the durable mint commits
/// BEFORE the in-memory transition (the as-built ordering, unchanged).
/// If the actor crashes in that window for a QUEUED claim, the
/// crashed mint's durable rows are absorbed — recovery loads the node,
/// the establishment sweep closes the orphaned open attempt
/// (materialization_infra, job re-armed), and a fresh claim DELIVERS
/// from Queued (the kinded edge). Nothing strands.
#[tokio::test]
async fn flag_on_queued_mint_crash_between_commit_and_transition_recovers() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // ── Phase 1: flag-on actor; merge the chain; simulate the crashed
    //    mint window with direct SQL (the exact rows
    //    mint_pull_attempt_fenced commits, with derivations.status left
    //    at 'queued' — the in-memory transition never ran). ──
    let mid_out = test_store_path("mat-crash-mid-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(mid_out.clone());
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |cfg, _| {
                cfg.materialization.enabled = true;
            });
        let root = make_node("mat-crash-root");
        let mut mid = make_node("mat-crash-mid");
        mid.expected_output_paths = vec![mid_out.clone()];
        let leaf = make_node("mat-crash-leaf");
        let nodes = vec![root, mid, leaf];
        let edges = vec![
            make_test_edge("mat-crash-root", "mat-crash-mid"),
            make_test_edge("mat-crash-mid", "mat-crash-leaf"),
        ];
        let build_id = Uuid::new_v4();
        let _ev = merge_dag(&handle, build_id, nodes, edges, false).await?;
        barrier(&handle).await;
        assert_eq!(
            expect_drv(&handle, "mat-crash-mid").await.status,
            DerivationStatus::Queued
        );
        // The crashed-mint window, reproduced durably.
        let derivation_id: Uuid =
            sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                .bind("mat-crash-mid")
                .fetch_one(&db.pool)
                .await?;
        let exec_id = Uuid::now_v7();
        sqlx::query(
            "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
             VALUES ($1, $2, 1, 'pending', $3)",
        )
        .bind(derivation_id)
        .bind("mat-crash-mid@store-test-0")
        .bind(exec_id)
        .execute(&db.pool)
        .await?;
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at, \
                                         attempt_kind) \
             VALUES ($1, $2, $3, now() - interval '1 hour', 'materialization')",
        )
        .bind(exec_id)
        .bind("mat-crash-mid")
        .bind("mat-crash-mid@store-test-0")
        .execute(&db.pool)
        .await?;
        // Backdate the assignment so the establishment window has
        // expired by the time phase 2 sweeps.
        sqlx::query("UPDATE assignments SET assigned_at = now() - interval '100 days'")
            .execute(&db.pool)
            .await?;
        // Crash: drop the actor.
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // ── Phase 2: fresh flag-on actor on the same PG; recovery; the
    //    establishment sweep absorbs the orphan; a fresh claim delivers
    //    from Queued. ──
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |cfg, p| {
            cfg.materialization.enabled = true;
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The node recovered (Queued — its dep is still unproduced).
    assert_eq!(
        expect_drv(&handle, "mat-crash-mid").await.status,
        DerivationStatus::Queued,
        "the crashed-mint node recovers at its durable status"
    );

    // The establishment sweep (PG-driven: kind read from the durable
    // drv_executions row, never from the in-memory view) closes the
    // orphaned open attempt: assignment row completed, charge class
    // materialization_infra (the kind partition — never executor_crash),
    // job stays pending.
    tick(&handle).await?;
    barrier(&handle).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        open, 0,
        "the establishment sweep closed the orphaned attempt"
    );
    let charges: Vec<String> = sqlx::query_scalar("SELECT outcome_class FROM drv_attempts")
        .fetch_all(&db.pool)
        .await?;
    assert_eq!(
        charges,
        vec!["materialization_infra".to_string()],
        "the orphaned materialization attempt is established as materialization_infra \
         (never executor_crash — the kind partition holds across the crash)"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        job_state, "pending",
        "the job survived the crash, still claimable"
    );

    // The job is listed claimable to any store replica (the durable
    // listing is PG-driven). The end-to-end re-claim from a RECOVERED
    // Queued node additionally needs the in-memory job view rebuilt at
    // recovery — that is T-4.3's obligation (Wave 4); the non-crash
    // Queued claim path is proven by
    // flag_on_queued_node_accepts_materialization_claim above.
    let listed = list_materialization_jobs(&handle, 16).await;
    assert_eq!(
        listed.len(),
        1,
        "the surviving job is listed claimable after recovery, got {listed:?}"
    );
    assert_eq!(listed[0].drv_hash, "mat-crash-mid");
    Ok(())
}

// ── T-1.5 (Phase B): PD-17 — reprobe-lane job creation + the AS-5 reset ──

// r[verify sched.materialize.job]
/// PD-17 + AS-5 (design §2.1 reprobe row): flag-on, a previously-failed
/// (Poisoned) pre-existing node whose output is upstream-substitutable
/// again gets, at the re-merging build's 6d slot:
///   - the AS-5 status reset to its dep-derived non-failed status
///     (Queued/Ready) — never Substituting (no walk spawns; this is the
///     first of the two merge-lane spawn sites criterion 3 closes),
///   - a durable poison_cleared budget-reset ledger row riding the
///     merge transaction,
///   - a materialization job row with origin='reprobe' riding the same
///     transaction,
/// and a dependent inserted in the SAME merge seeds against the
/// CORRECTED status (the bug_089/bug_132 phase-ordering invariant: the
/// 6d correction runs before 6e's seeding) — never DependencyFailed.
#[tokio::test]
async fn flag_on_reprobe_poisoned_node_creates_job_with_reset() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;
    let tag = "pd17-poisoned";
    let out = test_store_path("pd17-poisoned-out");
    let mut node = make_node(tag);
    node.expected_output_paths = vec![out.clone()];

    // Build #1: merge + force-poison at the resubmit limit so build #2's
    // merge does NOT reset it (it stays Poisoned and pre-existing → the
    // existing_reprobe lane).
    let _ev1 = merge_dag(&handle, Uuid::new_v4(), vec![node.clone()], vec![], false).await?;
    assert!(
        handle
            .debug_force_poisoned(tag, crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Poisoned,
        "precondition"
    );

    // The output becomes upstream-substitutable (NOT locally present).
    store.state.substitutable.write().unwrap().push(out.clone());

    // Build #2: resubmit the poisoned node together with a NEW dependent
    // in the same submission — the bug_132 assertion target.
    let parent = make_node("pd17-parent");
    let build2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        build2,
        vec![node, parent],
        vec![make_test_edge("pd17-parent", tag)],
        false,
    )
    .await?;
    barrier(&handle).await;

    // AS-5: the node sits at its dep-derived non-failed status — never
    // Poisoned (the reset happened), never Substituting (no walk).
    let info = expect_drv(&handle, tag).await;
    assert!(
        matches!(
            info.status,
            DerivationStatus::Queued | DerivationStatus::Ready
        ),
        "AS-5: the reprobe reset puts the node at its dep-derived status, got {:?}",
        info.status
    );

    // The poison_cleared budget-reset row is durable (rode the merge tx).
    let classes: Vec<String> = sqlx::query_scalar(
        "SELECT outcome_class FROM drv_attempts t \
           JOIN derivations d USING (derivation_id) \
          WHERE d.drv_hash = $1",
    )
    .bind(tag)
    .fetch_all(&db.pool)
    .await?;
    assert!(
        classes.contains(&"poison_cleared".to_string()),
        "the poison_cleared reset row rides the merge transaction, got {classes:?}"
    );

    // The job row exists with origin='reprobe', pending (claimable).
    let (origin, job_state): (String, String) =
        sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "reprobe", "the reprobe lane's job origin");
    assert_eq!(job_state, "pending");

    // No walk spawned (criterion 3: the merge-lane spawn site is closed).
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out),
            "flag-on the reprobe lane must not spawn walks; qpi_calls={qpi:?}"
        );
    }

    // bug_132: the dependent merged in the same submission seeds against
    // the CORRECTED status — Queued (gated on the reprobe node), never
    // DependencyFailed.
    assert_eq!(
        expect_drv(&handle, "pd17-parent").await.status,
        DerivationStatus::Queued,
        "the dependent seeds against the corrected (non-failed) status — bug_132 ordering"
    );

    // The build is Active (never fail-fasted against the stale poison).
    let st = query_status(&handle, build2).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Active as i32,
        "build #2 proceeds (the prior failure is moot)"
    );
    Ok(())
}

// r[verify sched.materialize.job]
/// PD-17, the Phase A T-6.2 observed-orphan trace replayed: a second
/// build merging an existing READY node that already carries a pending
/// job routes through the I-099 reprobe lane. As-built (flag-on, Phase
/// A) that lane spawned a walk which completed the node AROUND the
/// pending job — orphaning it. With PD-17 the lane creates/dedups the
/// job instead: no walk, the node stays Ready, and the job remains the
/// single resolution path for BOTH builds.
#[tokio::test]
async fn flag_on_reprobe_job_orphan_no_longer_forms() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;
    let tag = "pd17-orphan";
    let out = test_store_path("pd17-orphan-out");
    // Substitutable BEFORE merge → build #1's merge creates the job
    // (the new_sub lane, in-tx).
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut node = make_node(tag);
    node.expected_output_paths = vec![out.clone()];

    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![node.clone()], vec![], false).await?;
    barrier(&handle).await;
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "build #1's merge created the job");
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Ready
    );

    // Build #2 merges the SAME node (pre-existing Ready, job pending) —
    // the I-099 reprobe lane.
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, b2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // The orphan-former is gone: no walk transition (the node would be
    // Substituting as-built — the walk's 6d transition is synchronous
    // inside the merge turn), no QueryPathInfo walk traffic.
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Ready,
        "the node stays Ready: the job remains the in-flight marker; \
         nothing completes it around the job"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out),
            "the reprobe lane must not spawn walks flag-on; qpi_calls={qpi:?}"
        );
    }

    // One job (the dedup found build #1's), both builds' wanted rows.
    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "the reprobe lane dedups onto the existing job");
    assert_eq!(wanted, 2, "both builds' wanted-relation rows recorded");

    // The job resolves BOTH builds: claim → Success → consumption.
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the surviving job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&out, b"materialized");
    report_materialization_outcome(
        &handle,
        exec_id,
        tag,
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Completed,
        "the job's consumption completes the node"
    );
    for b in [b1, b2] {
        assert_eq!(
            query_status(&handle, b).await?.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "both interested builds succeed through the single job"
        );
    }
    Ok(())
}

// ── T-1.6 (Phase B): PD-18 — stale-Completed-verify job creation ──

// r[verify sched.materialize.job]
/// PD-18 (design §2.1 row 4): flag-on, the stale-Completed verify (6c)
/// routes the substitutable subset of demoted nodes to materialization
/// jobs (origin='stale_reset') instead of spawning its OWN walks (the
/// to_spawn lane — the SECOND of the two ungated merge-lane spawn
/// sites; criterion 3's walk-unreachability for fresh flag-on work is
/// provable from this change on, since this site is reachable for a
/// node that completed VIA materialization and then lost its outputs
/// to GC).
///
/// The trace (the design §2.4 C3-trace's stale-Completed origin):
///   build #1 completes a node via materialization (job → claim →
///   Success → consumption) → the outputs vanish from the store (GC) →
///   build #2 merges the same node → the verify demotes it (Completed →
///   Ready), creates the stale_reset job, spawns NO walk → the new job
///   resolves build #2.
#[tokio::test]
async fn flag_on_stale_completed_demote_creates_stale_reset_job() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store_materialization_enabled().await?;
    let tag = "pd18-stale";
    let out = test_store_path("pd18-stale-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut node = make_node(tag);
    node.expected_output_paths = vec![out.clone()];
    node.wanted_output_names = vec!["out".into()];

    // ── Build #1: complete the node via materialization. ──
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![node.clone()], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("build #1's job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&out, b"materialized-v1");
    report_materialization_outcome(
        &handle,
        exec_id,
        tag,
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Completed,
        "build #1 completed the node via materialization"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Succeeded as i32
    );

    // ── GC: the output vanishes from the store (still substitutable
    //    upstream — the missing-but-substitutable shape). ──
    store.state.paths.write().unwrap().remove(&out);

    // ── Build #2: merges the same (now stale-Completed) node. ──
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, b2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // The verify demoted the node and created the stale_reset job — it
    // did NOT spawn a walk (the node would be Substituting; the walk's
    // transition is synchronous inside the merge turn).
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Ready,
        "the stale-Completed verify demotes to Ready; the job (not a walk) owns re-materialization"
    );
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out),
            "the stale-verify lane must not spawn walks flag-on; qpi_calls={qpi:?}"
        );
    }

    // The new pending job carries origin='stale_reset' (build #1's
    // resolved job is a separate, terminal row).
    let (origin, job_state): (String, String) = sqlx::query_as(
        "SELECT origin, state FROM materialization_jobs \
          WHERE drv_hash = $1 AND state = 'pending'",
    )
    .bind(tag)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(origin, "stale_reset", "the stale-verify lane's job origin");
    assert_eq!(job_state, "pending");
    let resolved: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE drv_hash = $1 AND state = 'resolved_success'",
    )
    .bind(tag)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(resolved, 1, "build #1's resolved job row is untouched");

    // ── The new job resolves build #2. ──
    let assignment = match claim_materialization(&handle, tag, "store-test-1").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the stale_reset job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&out, b"materialized-v2");
    report_materialization_outcome(
        &handle,
        exec_id,
        tag,
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Completed,
        "the stale_reset job's consumption re-completes the node"
    );
    assert_eq!(
        query_status(&handle, b2).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build #2 succeeds through the stale_reset job"
    );
    Ok(())
}
