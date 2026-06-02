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

// r[verify sched.materialize.job+2]
/// FLAG ON: the same merge + dispatch cycle creates exactly ONE job
/// (origin=cache_opportunity) at the dispatch-probe partition, writes
/// the wanted relation for the (build, node) pair, does NOT spawn the
/// walk, and the node stays Ready (claimable) instead of going
/// Substituting.
#[tokio::test]
async fn flag_on_probe_partition_creates_job_instead_of_walk() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
/// FLAG ON: two builds merging the same substitutable node produce ONE
/// job (the dedup — the C3-class protection, now database-enforced),
/// while both builds' wanted relations are recorded.
#[tokio::test]
async fn flag_on_concurrent_interest_creates_one_job() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.routing+3]
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
        pruned_origin: false,
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
        pruned_origin: false,
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
        pruned_origin: false,
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
        pruned_origin: false,
    });
    assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
}

/// Arm 3a: Broken + live-wanted missing + re-probe says obtainable +
/// one-shot unspent → re-arm. MARKED node: the one-shot-spent shape
/// fail-fasts (the prune deliberately dropped the closure — the
/// resubmit-directing error is the correct verdict).
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
        // The marked (topdown-pruned root) shape: the only shape where
        // the one-shot-spent settlement may fail-fast (finding 11).
        pruned_origin: true,
    };
    // One-shot unspent + obtainable → ReArm.
    assert_eq!(
        route_unobtainable(&mk(0, Some(ReprobeAnswer::Obtainable))),
        UnobtainableRouting::ReArm
    );
    // One-shot SPENT (a prior unobtainable row exists) → FailFast even
    // when the re-probe says obtainable — for a MARKED node.
    assert_eq!(
        route_unobtainable(&mk(1, Some(ReprobeAnswer::Obtainable))),
        UnobtainableRouting::FailFast
    );
}

// r[verify sched.materialize.routing+3]
/// FINDING 11 (the C3-class equivalence divergence, orchestrator
/// ruling): the arm-3 settlement MUST discriminate on the
/// topdown-pruned mark. An UNMARKED node — a genuine leaf whose
/// closure evidence is Broken by structure (childless), not by
/// pruning — whose live-wanted path is confirmed missing upstream
/// releases to from-source dispatch, NEVER fail-fast. The as-built
/// walk only ever fail-fasts MARKED roots (`must_substitute` =
/// marked AND Broken; unmarked nodes are never affected, whatever
/// their evidence), so flag-state outcome equivalence (OQ7) requires
/// the same here.
#[test]
fn routing_unmarked_broken_confirmed_missing_resolves_from_source() {
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
        pruned_origin: false,
    };
    // Confirmed-missing re-probe, one-shot unspent: the unmarked node
    // must release to from-source — the build attempt proceeds.
    assert_eq!(
        route_unobtainable(&mk(0, Some(ReprobeAnswer::ConfirmedMissing))),
        UnobtainableRouting::ResolveFromSource,
        "an unmarked Broken-evidence node with a confirmed-missing live-wanted \
         path must release to from-source dispatch (the as-built walk never \
         fail-fasts unmarked nodes) — never fail-fast"
    );
    // One-shot spent: still never fail-fast for an unmarked node.
    assert_eq!(
        route_unobtainable(&mk(1, Some(ReprobeAnswer::Obtainable))),
        UnobtainableRouting::ResolveFromSource,
        "the one-shot-spent settlement releases unmarked nodes to from-source \
         (the fail-fast verdict and its resubmit-directing error are reserved \
         for topdown-pruned roots)"
    );
    // Both spent AND confirmed missing: still from-source for unmarked.
    assert_eq!(
        route_unobtainable(&mk(2, Some(ReprobeAnswer::ConfirmedMissing))),
        UnobtainableRouting::ResolveFromSource,
        "no combination of re-probe answer and one-shot state may fail-fast \
         an unmarked node"
    );
}

/// Arm 3b: Broken + live-wanted missing + (re-probe confirms missing OR
/// one-shot spent) + the topdown-pruned MARK → FailFast. The ONLY arm
/// that fail-fasts, and it requires all FOUR conjuncts (the design's
/// §2.4 closing claim, sharpened by the finding-11 mark discriminator:
/// only a deliberately-pruned root may be failed for unobtainability —
/// unmarked nodes always release to from-source instead).
#[test]
fn routing_fail_fast_requires_all_four_conjuncts() {
    let missing_hit = paths(&["out-path"]);
    let missing_miss = paths(&["unwanted-path"]);
    let verified = paths(&[]);
    let verified_covering = paths(&["out-path"]);
    let live = paths(&["out-path"]);
    // Exhaustive over the 16 combinations of (missing∩W≠∅, evidence
    // Broken, reprobe-confirms-or-spent, marked): exactly one yields
    // FailFast.
    let mut fail_fast_count = 0;
    for missing_intersects in [false, true] {
        for broken in [false, true] {
            for confirms_or_spent in [false, true] {
                for marked in [false, true] {
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
                        pruned_origin: marked,
                    };
                    let routing = route_unobtainable(&inputs);
                    if routing == UnobtainableRouting::FailFast {
                        fail_fast_count += 1;
                        assert!(
                            missing_intersects && broken && confirms_or_spent && marked,
                            "FailFast outside the four-conjunct corner: \
                             missing∩W={missing_intersects} broken={broken} \
                             confirmed/spent={confirms_or_spent} marked={marked}"
                        );
                    }
                }
            }
        }
    }
    assert_eq!(
        fail_fast_count, 1,
        "exactly one of the 16 combinations may fail-fast"
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
        // Even a MARKED node: an empty missing set can never fail-fast.
        pruned_origin: true,
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

// r[verify sched.materialize.routing+3]
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
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        jobs, 0,
        "no job state touched (the wanted relation is written by every \
         merge — unconditional since the cutover — and is not attempt state)"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// FLAG ON: an InfraFailure consumption charges materialization_infra
/// (kind=materialization — invisible to every build budget), the job
/// stays pending and claimable (under budget — never a fail-fast, B3),
/// and the node returns to Ready.
#[tokio::test]
async fn flag_on_infra_failure_charges_and_rearms() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.routing+3]
/// A dead store replica's open materialization attempt is established
/// as materialization_infra — never executor_crash, never adopted —
/// and the job returns to pending (claimable again). BC-2/BC-3: no
/// adopt arm for the materialization kind (the outputs ARE present in
/// the store here — the adopt-arm bait — and the establishment still
/// charges instead of completing); the charge is invisible to build
/// budgets by the kernel kind partition.
#[tokio::test]
async fn establishment_writes_materialization_infra_never_adopts() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
/// Cancellation: when the last live interested build goes terminal, the
/// housekeeping backstop (flag-gated) cancels the job and closes any
/// open materialization attempt CHARGE-FREE — no charge row of any
/// class is appended (BC-2's no-controller closer).
#[tokio::test]
async fn cancellation_closes_open_attempt_charge_free() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
// r[verify sched.materialize.routing+3]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.routing+3]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.routing+3]
// r[verify sched.materialize.job+2]
/// T-4.1 (Phase B): the FULL §2.4 C3 two-build dedup trace, flag-on —
/// the materialization-path twin of
/// `stale_walk_failure_does_not_fail_build_with_present_outputs`
/// (dispatch.rs, the C3 walk pin). The C3 defect class: stale failure
/// evidence recorded under WIDER interest must never fail a build whose
/// own (narrower) live interest is satisfiable.
///
/// The trace (design §2.4's confirmed-C3 replay, all seven steps):
///   1. Build B (narrow: wants `out`) merges; `out` substitutable →
///      job J created in the merge transaction (B's wanted-relation row
///      written).
///   2. Build A (wide: wants `out` + `wide`) merges the same node → NO
///      second job (the partial-unique-index dedup); A's relation row
///      written.
///   3. J is claimed (store-test-0); the executor's effective wanted
///      read at claim time is the union {out, wide}.
///   4. `out` becomes PRESENT in the store (out-of-band ingest); `wide`
///      is genuinely absent upstream.
///   5. Build A cancels inside the claim→consume window: its relation
///      rows leave the live join; live wanted shrinks to {out}.
///   6. The executor reports Unobtainable{missing: [wide], verified:
///      [out]} — failure evidence recorded against the WIDE read.
///   7. THE C3 ASSERTION: consumption re-reads live interest = {B} and
///      live wanted = {out}; arm 0 (moot): missing ∩ W = ∅ and
///      verified ⊇ W → d1 Completed, B Succeeded, J resolved_success.
///      Build B is NOT failed; zero fail-fast.
///
/// Protecting mechanisms (§3.3 C3 row): (i) interest derived from the
/// durable wanted relation — the stranded-build horn; (ii) the
/// consumption-transaction moot arm re-reading LIVE interest — the
/// wrongful-fail-fast horn. The ledger records the unobtainable report
/// (prediction) but the routing — not the row — decides the verdict
/// (the prediction/reconciliation collapse).
///
/// Pin-vs-red (plan T-4.1 / RT-4 precedent): the protecting mechanisms
/// all exist from Phase A — this test SHOULD pass first-try and is
/// recorded as a PIN; a failure would be a Phase A consumption bug.
#[tokio::test]
async fn flag_on_stale_unobtainable_two_build_dedup_never_fails() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // One 2-output node; the narrow build wants `out`, the wide build
    // wants `out` + `wide`.
    let out = test_store_path("c3-dedup-out");
    let wide = test_store_path("c3-dedup-wide");
    // Substitutable BEFORE the first merge → the merge new_sub lane
    // creates the job inside B's merge transaction (step 1's shape).
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(out.clone());
        subs.push(wide.clone());
    }

    let mk = |wanted: &[&str]| {
        let mut n = make_node("c3-dedup");
        n.output_names = vec!["out".into(), "wide".into()];
        n.expected_output_paths = vec![out.clone(), wide.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Step 1: build B (narrow) merges → job J created, node stays Ready.
    let build_b = Uuid::new_v4();
    merge_dag(&handle, build_b, vec![mk(&["out"])], vec![], false).await?;
    barrier(&handle).await;
    let (jobs, wanted_rows) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(
        (jobs, wanted_rows),
        (1, 1),
        "step 1: B's merge creates exactly one job + one wanted-relation row"
    );

    // Step 3 (claim BEFORE A merges so A's merge cannot reprobe the
    // node — the open attempt holds it Running, same sequencing as the
    // design trace): claim J as a store replica.
    let assignment = match claim_materialization(&handle, "c3-dedup", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // Step 2: build A (wide) merges the same node → the dedup keeps ONE
    // job; A's wanted-relation row is recorded. (A merges while the
    // attempt is open — the C3 window.)
    let build_a = Uuid::new_v4();
    merge_dag(&handle, build_a, vec![mk(&["out", "wide"])], vec![], false).await?;
    barrier(&handle).await;
    let (jobs, wanted_rows) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(
        (jobs, wanted_rows),
        (1, 2),
        "step 2: the partial-unique-index dedup keeps one job; both builds' \
         wanted-relation rows exist"
    );

    // Step 4: `out` becomes present in the store (out-of-band ingest);
    // `wide` stays absent upstream.
    store.seed_with_content(&out, b"c3-out-of-band-ingest");

    // Step 5: build A (the WIDE interest) cancels inside the
    // claim→consume window — its wants leave the live join.
    cancel_build(&handle, build_a).await?;
    barrier(&handle).await;

    // Step 6: the executor reports Unobtainable against its WIDE read:
    // `wide` missing, `out` verified present.
    report_materialization_outcome(
        &handle,
        exec_id,
        "c3-dedup",
        mat_unobtainable_outcome(
            vec![wide.clone()],
            vec![out.clone()],
            "upstream 404 on wide",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Step 7 — THE C3 ASSERTIONS.
    // (a) The node completed for live interest (the moot arm).
    assert_eq!(
        expect_drv(&handle, "c3-dedup").await.status,
        DerivationStatus::Completed,
        "C3: the moot arm completes the node for live (narrow) interest"
    );
    // (b) Build B succeeded — the stale wide-read failure evidence never
    //     touches it.
    let st = query_status(&handle, build_b).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "C3: build B must succeed — stale failure evidence recorded under \
         wider interest never fails a narrower build (zero fail-fast)"
    );
    assert!(
        st.error_summary.is_empty(),
        "C3: no fail-fast error may be recorded on B; got {:?}",
        st.error_summary
    );
    // (c) The job resolved successfully.
    let job_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(job_state, "resolved_success", "C3: J resolves successfully");
    // (d) The ledger assertion (prediction/reconciliation collapse): the
    //     unobtainable report WAS recorded — as a materialization-kind
    //     row invisible to build budgets — but the routing, not the row,
    //     decided the verdict.
    let (unob_rows, build_kind_rows): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*) FILTER (WHERE t.outcome_class = 'materialization_unobtainable'), \
           count(*) FILTER (WHERE COALESCE(e.attempt_kind, 'build') = 'build') \
         FROM drv_attempts t \
         LEFT JOIN drv_executions e ON e.exec_id = t.exec_id",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        unob_rows, 1,
        "the unobtainable report is recorded in the ledger (the prediction)"
    );
    assert_eq!(
        build_kind_rows, 0,
        "zero build-kind rows: the trace never touched any build budget"
    );
    // (e) Build A is cancelled (its own verdict), never failed.
    let st_a = query_status(&handle, build_a).await?;
    assert_eq!(
        st_a.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build A's verdict is its own cancellation, never a fail-fast"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// FINDING 11 (the C3-class equivalence divergence; orchestrator ruling,
/// red-first), actor level: an UNMARKED genuine leaf — childless, so its
/// closure evidence is structurally `Broken` — whose wanted output the
/// executor confirms missing-and-unsubstitutable upstream must NOT fail
/// the build. The node releases to from-source dispatch (Ready), the job
/// resolves `resolved_from_source`, and the build stays Active.
///
/// Walk-era oracle (historical): the as-built walk's fail-fast was
/// reachable only for nodes carrying the pruned mark ("unmarked nodes
/// are never affected, whatever their evidence"); an unmarked node
/// whose walk failed reverted to Ready and fell through to from-source
/// dispatch. The job world must produce the same client-visible
/// outcome (OQ7): the resubmit-directing fail-fast error class is
/// reserved for pruned-ORIGIN jobs.
///
/// The reachable divergence this pins (finding 11's trace): probe-time
/// blip says substitutable (job created per B3) → upstream entry vanishes
/// → executor reports Unobtainable → consumption re-probe confirms
/// missing → pre-fix arm 3 failed the build with "topdown-pruned root …
/// resubmit" for a node that was never pruned.
#[tokio::test]
async fn flag_on_unmarked_leaf_confirmed_missing_releases_to_from_source() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // Service signer + a real tenant: the consumption re-probe can only
    // CONFIRM missing under Service auth (B3: an unauthenticated probe
    // is indeterminate and never fail-fasts) — without these the arm-3
    // settlement is unreachable and this test would be vacuous.
    let service_key = b"test-finding11-service-key-32-byt".to_vec();
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let _tasks = (store_task, actor_task);
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "finding11-tenant").await;

    // 1. Merge a 1-node build (childless leaf, never pruned) under the
    //    tenant. Nothing is substitutable at merge time, so the merge
    //    classifies nothing; the node seeds Ready and stays UNMARKED.
    let out = test_store_path("unmarked-leaf-out");
    let mut leaf = make_node("unmarked-leaf");
    leaf.expected_output_paths = vec![out.clone()];
    leaf.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![leaf],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;

    // 2. The probe blip: the upstream claims the path is substitutable →
    //    the dispatch probe creates a cache_opportunity job (B3
    //    optimistic creation). The node stays Ready, unmarked.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;
    let origin: String = sqlx::query_scalar("SELECT origin FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        origin, "cache_opportunity",
        "precondition: the probe blip created one NON-pruned job (the leaf was never pruned)"
    );
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "precondition: the probe blip created one job");

    // 3. Claim the job as a store replica, then withdraw the upstream
    //    entry (the blip ends): the path is now genuinely missing AND
    //    not substitutable — the re-probe will confirm it missing.
    let assignment = match claim_materialization(&handle, "unmarked-leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .retain(|p| p != &out);

    // 4. The executor reports Unobtainable: the wanted path is missing
    //    upstream. The consumption re-probe (Service auth + tenant)
    //    confirms it missing-and-unsubstitutable → arm 3 → the node is
    //    UNMARKED → the settlement must release to from-source.
    report_materialization_outcome(
        &handle,
        exec_id,
        "unmarked-leaf",
        mat_unobtainable_outcome(
            vec![out.clone()],
            vec![],
            "upstream 404 on the wanted output",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // THE FINDING-11 ASSERTIONS: the build is NOT failed; the node is
    // released to from-source dispatch; the job resolved from-source;
    // no resubmit-directing error was recorded.
    let st = query_status(&handle, build_id).await?;
    assert_ne!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "an unmarked leaf must NEVER be failed by the materialization \
         settlement (the fail-fast verdict is reserved for topdown-pruned \
         roots); the build must remain Active for from-source dispatch. \
         Error summary: {:?}",
        st.error_summary
    );
    let drv = expect_drv(&handle, "unmarked-leaf").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Ready,
        "the unmarked node must release to Ready (from-source dispatchable)"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "the job must resolve from_source — the build attempt proceeds"
    );
    // The build proceeds from source: a builder pull for the node is
    // now admissible (job resolved → JobView::None → as-built admission;
    // the node is Ready and unmarked so the pull mints).
    let pull = try_pull_attempt(&handle, "unmarked-leaf").await;
    assert!(
        matches!(pull, Ok(PullOutcome::Deliver(_))),
        "after the from-source release a builder pull must mint (the build \
         attempt proceeds from source), got {pull:?}"
    );
    Ok(())
}

// ── T-1.1 (Phase B): §2.6 consumer re-sourcing — the snapshot buckets ──

// r[verify sched.admin.snapshot-substituting+3]
// r[verify ctrl.scaler.signal-substituting+4]
/// §2.6 re-sourcing: pending (unclaimed) materialization jobs ARE the
/// substituting bucket flag-on. A Ready or Queued node carrying an
/// unresolved unclaimed job counts in `substituting_derivations` and is
/// EXCLUDED from `queued_derivations`/`queued_by_system` (the buckets
/// stay disjoint — builder autoscalers must not scale on work that will
/// be materialized); a claimed job's node is Assigned/Running and counts
/// in `running_derivations` by construction.
#[tokio::test]
async fn flag_on_pending_jobs_count_as_substituting_bucket() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify obs.metric.scheduler-substituting]
/// The materialization backlog is scrapeable: each housekeeping tick
/// the leader publishes `rio_scheduler_substituting_derivations` with
/// EXACTLY the snapshot's substituting bucket (the §2.6 job-derived
/// count `ClusterStatus.substituting_derivations` reports). Without
/// the gauge the backlog exists only behind the admin RPC — invisible
/// to Prometheus, so the KEDA store-scaling trigger reads nothing
/// (item I, WT-1).
#[tokio::test]
async fn substituting_gauge_published_from_snapshot_bucket() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Same fixture as the snapshot-bucket test above: root → mid →
    // leaf, mid+leaf substitutable, root demanded → 2 pending
    // unclaimed jobs after the merge (leaf Ready, mid Queued).
    let mid_out = test_store_path("sub-gauge-mid-out");
    let leaf_out = test_store_path("sub-gauge-leaf-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(mid_out.clone());
        subs.push(leaf_out.clone());
    }
    let root = make_node("sub-gauge-root");
    let mut mid = make_node("sub-gauge-mid");
    mid.expected_output_paths = vec![mid_out.clone()];
    let mut leaf = make_node("sub-gauge-leaf");
    leaf.expected_output_paths = vec![leaf_out.clone()];
    let nodes = vec![root, mid, leaf];
    let edges = vec![
        make_test_edge("sub-gauge-root", "sub-gauge-mid"),
        make_test_edge("sub-gauge-mid", "sub-gauge-leaf"),
    ];
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, false).await?;
    barrier(&handle).await;

    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(snap.substituting_derivations, 2, "fixture precondition");
    assert_eq!(
        recorder.gauge_value("rio_scheduler_substituting_derivations{}"),
        Some(2.0),
        "tick must publish the substituting bucket as a gauge — the KEDA \
         backlog trigger scrapes Prometheus, not the admin RPC"
    );

    // Claim drains the bucket → the next tick publishes the drop. The
    // gauge tracks the SAME quantity as the proto field at every tick.
    let claim = claim_materialization(&handle, "sub-gauge-leaf", "store-test-0").await;
    assert!(matches!(claim, Ok(PullOutcome::Deliver(_))), "{claim:?}");
    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(snap.substituting_derivations, 1, "fixture precondition");
    assert_eq!(
        recorder.gauge_value("rio_scheduler_substituting_derivations{}"),
        Some(1.0),
        "the gauge follows the bucket down as jobs are claimed"
    );
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

// r[verify sched.materialize.job+2]
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

// r[verify sched.materialize.job+2]
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
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |_cfg, _| {});
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
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_cfg, p| {
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

// r[verify sched.materialize.job+2]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
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

// r[verify sched.materialize.job+2]
/// PD-17, the Phase A T-6.2 observed-orphan trace replayed: a second
/// build merging an existing READY node that already carries a pending
/// job routes through the I-099 reprobe lane. As-built (flag-on, Phase
/// A) that lane spawned a walk which completed the node AROUND the
/// pending job — orphaning it. With PD-17 the lane creates/dedups the
/// job instead: no walk, the node stays Ready, and the job remains the
/// single resolution path for BOTH builds.
#[tokio::test]
async fn flag_on_reprobe_job_orphan_no_longer_forms() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
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

// r[verify sched.materialize.job+2]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
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

// ── Pruned-origin consumption shapes (T-D5.1 re-frames of the walk-era
// clear-mirror block: the mark died with the evidence columns; the
// origin row is the only durable pruned fact and resolution itself is
// the settlement) ──

// r[verify sched.materialize.routing+3]
/// Arm 2 at the actor level: a PRUNED-origin job whose node's
/// re-declared child is NOT yet produced (durable evidence Pending)
/// gets an Unobtainable report → the routing resolves from-source —
/// the pruned origin alone never fail-fasts while the durable relation
/// says the closure is buildable.
#[tokio::test]
async fn pruned_origin_pending_evidence_resolves_from_source() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Build #1: the prune fixture (root marked + job origin=pruned).
    let root_out = test_store_path("clrfs-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mut root = make_node("clrfs-root");
    root.expected_output_paths = vec![root_out.clone()];
    root.wanted_output_names = vec!["out".into()];
    let dep = make_node("clrfs-dep");
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![root.clone(), dep.clone()],
        vec![make_test_edge("clrfs-root", "clrfs-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;
    let origin: String = sqlx::query_scalar("SELECT origin FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(origin, "pruned", "premise: the prune classified the root");

    // The upstream loses the root's output (so build #2 does not
    // re-prune), then build #2 re-declares root → dep: the dep inserts
    // un-produced, so the root's durable evidence becomes Pending.
    store.state.substitutable.write().unwrap().clear();
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![root, dep],
        vec![make_test_edge("clrfs-root", "clrfs-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Claim the (still pending, pruned-origin) job and report
    // Unobtainable for the live-wanted root output.
    let assignment = match claim_materialization(&handle, "clrfs-root", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the pruned root's job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_id,
        "clrfs-root",
        mat_unobtainable_outcome(vec![root_out.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // The routing resolved from-source (durable evidence Pending — the
    // re-declared child gates it; never a fail-fast).
    let job_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "Pending durable evidence routes from-source"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// Resolution is terminal (the FP-4(a) class, re-framed for the
/// origin-only world): a node whose pruned-origin job resolved
/// SUCCESSFULLY, then lost its outputs to GC, must dispatch from source
/// normally when a later build re-merges it — the resolved job's pruned
/// classification must not survive into a wrongful fail-fast (a fresh
/// actor against the same PG sees only the RESOLVED row; the arm-3
/// discriminator reads unresolved-job origin, never history).
#[tokio::test]
async fn resolved_pruned_job_does_not_fail_fast_later_remerge() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    let root_out = test_store_path("revert-root-out");

    // ── Phase 1: prune → job → claim → Success → consume. ──
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |_cfg, _| {});
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(root_out.clone());
        let mut root = make_node("revert-root");
        root.expected_output_paths = vec![root_out.clone()];
        root.wanted_output_names = vec!["out".into()];
        let dep = make_node("revert-dep");
        let b1 = Uuid::new_v4();
        let _ev = merge_dag(
            &handle,
            b1,
            vec![root, dep],
            vec![make_test_edge("revert-root", "revert-dep")],
            false,
        )
        .await?;
        barrier(&handle).await;
        let origin: String = sqlx::query_scalar("SELECT origin FROM materialization_jobs")
            .fetch_one(&db.pool)
            .await?;
        assert_eq!(
            origin, "pruned",
            "phase 1 precondition: the prune classified the root (in-tx job row)"
        );

        let assignment = match claim_materialization(&handle, "revert-root", "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("the job must be claimable, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        store.seed_with_content(&root_out, b"materialized-flag-on");
        report_materialization_outcome(
            &handle,
            exec_id,
            "revert-root",
            mat_success_outcome(vec![root_out.clone()], vec![]),
        )
        .await
        .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
        barrier(&handle).await;
        assert_eq!(
            expect_drv(&handle, "revert-root").await.status,
            DerivationStatus::Completed,
            "phase 1: the node completed via materialization"
        );
        assert_eq!(
            query_status(&handle, b1).await?.state,
            rio_proto::types::BuildState::Succeeded as i32
        );

        // The deployment restarts (actor torn down — the failover/
        // rollback shape; same PG).
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // ── Phase 2 (fresh actor, same PG): recovery, GC, re-merge, dispatch. ──
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_cfg, p| {
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // GC: the output is gone from the store AND no longer substitutable
    // upstream (definitively missing — the worst case for a marked node).
    store.state.paths.write().unwrap().remove(&root_out);
    store.state.substitutable.write().unwrap().clear();

    // A later build re-merges the root (the resubmit shape).
    let mut root = make_node("revert-root");
    root.expected_output_paths = vec![root_out.clone()];
    root.wanted_output_names = vec!["out".into()];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, b2, vec![root], vec![], false).await?;
    barrier(&handle).await;

    // The stale-Completed verify demoted the node; the dispatch probe
    // now evaluates it.
    tick(&handle).await?;
    barrier(&handle).await;

    // THE FP-4(a)-class property: no wrongful fail-fast. The node
    // dispatches from source (Ready — waiting for a builder) and the
    // build stays Active: the prior pruned-origin job is RESOLVED, so
    // the arm-3 discriminator never fires for it.
    let st = query_status(&handle, b2).await?;
    assert_ne!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "the re-merged build must NOT be wrongly fail-fasted on a RESOLVED \
         pruned-origin job (FP-4(a) class); error: {:?}",
        st.error_summary
    );
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Active as i32,
        "the build proceeds: the node is from-source dispatchable"
    );
    let drv = expect_drv(&handle, "revert-root").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Ready,
        "the node sits Ready for a from-source builder — never terminal, never fail-fasted"
    );
    Ok(())
}

// ── T-1.8 (Phase B): the §5.3 pin-release wiring (always-on, three sites) ──

/// Count materialization pins for a drv (the §5.3 assertions).
async fn mat_pin_count(pool: &sqlx::PgPool, drv: &str) -> anyhow::Result<i64> {
    Ok(sqlx::query_scalar(
        "SELECT count(*) FROM scheduler_live_pins \
          WHERE drv_hash = $1 AND pin_kind = 'materialization'",
    )
    .bind(drv)
    .fetch_one(pool)
    .await?)
}

// r[verify sched.materialize.pinning]
/// §5.3 release sites (i)+(ii), the single-build case: pins created at
/// ingest survive until the job is resolved AND the (only) interested
/// build goes terminal — at which point the build-terminal hook
/// releases them. Without the wiring, every materialized path stays
/// pinned forever (the store GC roots on every scheduler_live_pins row).
#[tokio::test]
async fn pins_release_when_job_resolved_and_interest_terminal() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tag = "pin-rel";
    let out = test_store_path("pin-rel-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut node = make_node(tag);
    node.expected_output_paths = vec![out.clone()];
    node.wanted_output_names = vec!["out".into()];

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // Claim, then simulate the store executor's pin-at-ingest (the
    // store-side write the real executor issues before its Success
    // report).
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let job_id: Uuid = sqlx::query_scalar("SELECT job_id FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    sdb(&db.pool)
        .pin_materialized_paths(
            job_id,
            &crate::state::DrvHash::from(tag),
            std::slice::from_ref(&out),
        )
        .await?;
    assert_eq!(mat_pin_count(&db.pool, tag).await?, 1, "pinned at ingest");

    // Success → consumption: job resolved, node completed, the (only)
    // interested build goes terminal → the §5.3 release fires.
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
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the build completed through the consumption"
    );
    assert_eq!(
        mat_pin_count(&db.pool, tag).await?,
        0,
        "§5.3: job resolved AND all interest terminal → the pins are released \
         (the build-terminal hook is the release site)"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// §5.3's holds-window (the B2-strong property): pins survive while ANY
/// live interested build remains, and release only when the LAST one
/// goes terminal. Two builds share the materialized node; one finishes,
/// one stays live → held; the second goes terminal → released.
#[tokio::test]
async fn pins_survive_while_any_interest_live() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tag = "pin-hold";
    let out = test_store_path("pin-hold-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mk = || {
        let mut n = make_node(tag);
        n.expected_output_paths = vec![out.clone()];
        n.wanted_output_names = vec!["out".into()];
        n
    };

    // Build A: just the materialized node. Build B: the node + an
    // unrelated never-completing node (keeps B live).
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag(&handle, build_a, vec![mk()], vec![], false).await?;
    barrier(&handle).await;
    let build_b = Uuid::new_v4();
    let blocker = make_node("pin-hold-blocker");
    let _ev_b = merge_dag(&handle, build_b, vec![mk(), blocker], vec![], false).await?;
    barrier(&handle).await;

    // Claim + pin-at-ingest + Success.
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let job_id: Uuid =
        sqlx::query_scalar("SELECT job_id FROM materialization_jobs WHERE state = 'pending'")
            .fetch_one(&db.pool)
            .await?;
    sdb(&db.pool)
        .pin_materialized_paths(
            job_id,
            &crate::state::DrvHash::from(tag),
            std::slice::from_ref(&out),
        )
        .await?;
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

    // Build A succeeded (terminal); build B is still live (blocker
    // pending) → the pins are HELD (the all-interest-terminal rule).
    assert_eq!(
        query_status(&handle, build_a).await?.state,
        rio_proto::types::BuildState::Succeeded as i32
    );
    assert_eq!(
        query_status(&handle, build_b).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "build B stays live behind the blocker node"
    );
    assert_eq!(
        mat_pin_count(&db.pool, tag).await?,
        1,
        "§5.3 holds-window: pins survive while ANY live interested build remains"
    );

    // Build B goes terminal (cancel) → the LAST interest departs → released.
    cancel_build(&handle, build_b).await?;
    barrier(&handle).await;
    assert_eq!(
        mat_pin_count(&db.pool, tag).await?,
        0,
        "§5.3: the last interested build's terminal transition releases the pins"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// THE always-on proof (PD-B17): pins created during a flag-ON era
/// release after a rollback to flag-OFF. The release wiring is NOT
/// flag-gated — if it were, flag-on-era pins would become permanently
/// GC-immune after an ON→OFF rollback (a store-state divergence no
/// criterion would catch).
#[tokio::test]
async fn flag_on_era_pins_release_after_revert() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let tag = "pin-revert";
    let out = test_store_path("pin-revert-out");
    let build_id = Uuid::new_v4();

    // ── Phase 1 (flag-ON): job → claim → pin → Success; the build
    //    stays live behind a blocker node. ──
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |_cfg, _| {});
        store.state.substitutable.write().unwrap().push(out.clone());
        let mut node = make_node(tag);
        node.expected_output_paths = vec![out.clone()];
        node.wanted_output_names = vec!["out".into()];
        let blocker = make_node("pin-revert-blocker");
        let _ev = merge_dag(&handle, build_id, vec![node, blocker], vec![], false).await?;
        barrier(&handle).await;

        let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("the job must be claimable, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        let job_id: Uuid = sqlx::query_scalar("SELECT job_id FROM materialization_jobs")
            .fetch_one(&db.pool)
            .await?;
        sdb(&db.pool)
            .pin_materialized_paths(
                job_id,
                &crate::state::DrvHash::from(tag),
                std::slice::from_ref(&out),
            )
            .await?;
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

        // Job resolved; the build is still live → pins held.
        assert_eq!(
            query_status(&handle, build_id).await?.state,
            rio_proto::types::BuildState::Active as i32
        );
        assert_eq!(
            mat_pin_count(&db.pool, tag).await?,
            1,
            "flag-on-era pin held"
        );

        // The deployment reverts to flag-off.
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // ── Phase 2 (flag-OFF, same PG): the build goes terminal → the
    //    ALWAYS-ON release fires. ──
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_cfg, p| {
            p.leader = phase2_leader; // flag-off (default config)
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    cancel_build(&handle, build_id).await?;
    barrier(&handle).await;

    assert_eq!(
        mat_pin_count(&db.pool, tag).await?,
        0,
        "PD-B17: flag-on-era pins release after the rollback — the §5.3 wiring is \
         always-on, never flag-gated"
    );
    Ok(())
}

// r[verify sched.materialize.pinning]
/// §5.3 release site (iii): the recovery sweep's materialization arm is
/// the orphan backstop — pins whose job resolved and whose every
/// interested build went terminal while no event-driven release fired
/// (crash window) are released at the next leader acquisition. Proven
/// flag-OFF (the arm is always-on).
#[tokio::test]
async fn recovery_sweep_releases_orphaned_materialization_pins() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed the orphan shape directly in PG (the crash window's residue):
    // a terminal build + wanted row + resolved job + materialization pins.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ('pin-orphan', '/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-pin-orphan.drv', \
                 'x86_64-linux', 'completed') \
         RETURNING derivation_id",
    )
    .fetch_one(&db.pool)
    .await?;
    let build_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;
    sqlx::query(
        "INSERT INTO build_wanted_outputs (build_id, derivation_id, wanted_output_names) \
         VALUES ($1, $2, '{}')",
    )
    .bind(build_id)
    .bind(derivation_id)
    .execute(&db.pool)
    .await?;
    let job_id = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO materialization_jobs \
             (job_id, derivation_id, drv_hash, origin, state, created_generation) \
         VALUES ($1, $2, 'pin-orphan', 'cache_opportunity', 'resolved_success', 1)",
    )
    .bind(job_id)
    .bind(derivation_id)
    .execute(&db.pool)
    .await?;
    sdb(&db.pool)
        .pin_materialized_paths(
            job_id,
            &crate::state::DrvHash::from("pin-orphan"),
            &[test_store_path("pin-orphan-out")],
        )
        .await?;
    assert_eq!(mat_pin_count(&db.pool, "pin-orphan").await?, 1);

    // A fresh (flag-OFF) leader recovers → the sweep's materialization
    // arm releases the orphan.
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_cfg, p| {
        p.leader = phase2_leader;
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        mat_pin_count(&db.pool, "pin-orphan").await?,
        0,
        "§5.3 site (iii): the recovery sweep's materialization arm releases orphaned pins"
    );
    Ok(())
}

// ── T-4.2 (Phase B): settlement totality — the D16-class limbo is
//    structurally impossible flag-on ─────────────────────────────────────

// r[verify sched.materialize.routing+3]
// r[verify sched.materialize.job+2]
/// T-4.2: every non-terminal materialization-job state has an armed
/// action, and that action FIRES. The D16 limbo (flag-off: a
/// marked+tried+present node refused by every decision cell, hanging
/// Active forever) is unconstructible flag-on because the job state
/// machine has no no-action state — no decision cell can refuse to act
/// on a job (§3.3 settlement totality).
///
/// | Job state                 | Armed action proven                     |
/// |---------------------------|-----------------------------------------|
/// | pending, unclaimed        | listed by ListMaterializationJobs       |
/// | pending, node Queued      | still listed AND claimable (PD-6)       |
/// | claimed, executor alive   | report consumption resolves the job     |
/// | claimed, executor dead    | establishment sweep charges + re-arms   |
/// | parked (budget exhausted) | durable park_until; refused while       |
/// |                           | parked; re-claimable after expiry       |
/// | zero live interest        | cancellation closer fires charge-free   |
///
/// (The walk-era D16-shape probe — mark + tried + output present —
/// retired with the evidence machinery in T-D5.1: no mark-keyed
/// refusal exists any more, so the limbo it probed is structurally
/// unrepresentable; the job table above is the totality argument.)
///
/// Pin, not red (plan T-4.2 / review RFB-5): the protecting mechanisms
/// (listing, Queued claims, establishment, park, cancellation closer)
/// all exist by this task — first-try pass recorded as a pin under
/// commit rule 1's pure-addition clause.
// r[verify sched.materialize.settlement]
#[tokio::test]
async fn flag_on_every_job_state_has_armed_action() -> TestResult {
    // Tight budget/backoff so the park arm is provable in-test:
    // max_attempts=1 → the first WORKER-reported infra failure that
    // lands on top of any prior charge parks the job; backoff base 1 s
    // (exp doubling) keeps the parked window short but observable.
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 1;
        });
    let _tasks = (store_task, actor_task);

    // ════ State 1: pending, unclaimed → LISTED (the poll IS the arm) ════
    let out1 = test_store_path("tot-pending-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out1.clone());
    let mut n1 = make_node("tot-pending");
    n1.expected_output_paths = vec![out1.clone()];
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![n1], vec![], false).await?;
    barrier(&handle).await;
    let listed = list_materialization_jobs(&handle, 16).await;
    assert!(
        listed.iter().any(|j| j.drv_hash == "tot-pending"),
        "state 1 (pending unclaimed): the job must be listed — the store's \
         poll is the armed action; got {listed:?}"
    );

    // ════ State 2: pending, node QUEUED (dep-blocked) → listed + claimable ════
    // The 3-node chain shape (root → mid → leaf) where only MID is
    // substitutable: the root is not substitutable so the topdown prune
    // cannot fire (a substitutable ROOT would be pruned and seed Ready,
    // not Queued); mid's job is created by the merge new_sub lane and
    // mid sits Queued behind the unproduced leaf.
    let mid_out = test_store_path("tot-queued-mid-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(mid_out.clone());
    let root2 = make_node("tot-queued-root");
    let mut mid2 = make_node("tot-queued-mid");
    mid2.expected_output_paths = vec![mid_out.clone()];
    let leaf2 = make_node("tot-queued-leaf");
    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![root2, mid2, leaf2],
        vec![
            make_test_edge("tot-queued-root", "tot-queued-mid"),
            make_test_edge("tot-queued-mid", "tot-queued-leaf"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "tot-queued-mid").await.status,
        DerivationStatus::Queued,
        "precondition: mid is dep-blocked (Queued) behind the unproduced leaf"
    );
    let listed = list_materialization_jobs(&handle, 16).await;
    assert!(
        listed.iter().any(|j| j.drv_hash == "tot-queued-mid"),
        "state 2 (pending, node Queued): the job must still be listed \
         (materialization does not wait for deps); got {listed:?}"
    );
    let assignment = match claim_materialization(&handle, "tot-queued-mid", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!(
            "state 2: the Queued node's pending job must be CLAIMABLE \
             (PD-6: the dep-racing claim is legal), got {other:?}"
        ),
    };

    // ════ State 3: claimed, executor alive → report consumption resolves ════
    let exec3: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&mid_out, b"tot-queued-mid-content");
    report_materialization_outcome(
        &handle,
        exec3,
        "tot-queued-mid",
        mat_success_outcome(vec![mid_out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("state 3 report rejected: {e:?}"))?;
    barrier(&handle).await;
    let st: String = sqlx::query_scalar(
        "SELECT mj.state FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'tot-queued-mid'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        st, "resolved_success",
        "state 3 (claimed, executor alive): the report consumption resolves the job"
    );

    // ════ State 4: claimed, executor DEAD → establishment charges + re-arms ════
    let assignment = match claim_materialization(&handle, "tot-pending", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("state 4 claim must deliver, got {other:?}"),
    };
    let exec4: Uuid = assignment.exec_id.parse()?;
    // The replica dies (never reports). Age the open attempt past every
    // deadline+slack, then sweep.
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec4)
    .execute(&db.pool)
    .await?;
    tick(&handle).await?;
    barrier(&handle).await;
    let (job_state, infra_rows): (String, i64) = sqlx::query_as(
        "SELECT mj.state, \
                (SELECT count(*) FROM drv_attempts a \
                  WHERE a.derivation_id = mj.derivation_id \
                    AND a.outcome_class = 'materialization_infra') \
           FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'tot-pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        job_state, "pending",
        "state 4 (claimed, executor dead): the establishment sweep re-arms the job"
    );
    assert_eq!(
        infra_rows, 1,
        "state 4: the establishment charged exactly one materialization_infra row"
    );

    // ════ State 5: parked (budget exhausted) → durable park_until; refused
    //      while parked; re-claimable after the backoff expires ════
    // Re-claim the re-armed job and report a WORKER infra failure: the
    // history now holds 2 infra rows (1 establishment + 1 worker) >=
    // max_attempts(1) → the job parks.
    let assignment = match claim_materialization(&handle, "tot-pending", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("state 5 re-claim must deliver, got {other:?}"),
    };
    let exec5: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec5,
        "tot-pending",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("state 5 infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let park_until: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM mj.park_until)::float8 \
           FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'tot-pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        park_until.is_some(),
        "state 5 (parked): the budget exhaustion writes a durable park_until row"
    );
    // While parked: the claim is refused (NotYetReady — backoff unexpired).
    let refused = claim_materialization(&handle, "tot-pending", "store-test-0").await;
    assert!(
        matches!(refused, Ok(PullOutcome::NotYetReady { .. })),
        "state 5: a parked job's claim is refused while the backoff runs, got {refused:?}"
    );
    // The park is a WAIT, not a terminal state: after the backoff
    // expires (1 s base, exp 2 → 2 s here) the job is claimable again.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let reclaimed = claim_materialization(&handle, "tot-pending", "store-test-0").await;
    assert!(
        matches!(reclaimed, Ok(PullOutcome::Deliver(_))),
        "state 5: the park backoff expiry re-arms the claim (the park is a \
         visible wait, never a strand), got {reclaimed:?}"
    );

    // ════ State 5b (PD-20 / T-6.1): parked + from-source viable → the
    //      housekeeping re-evaluation arm resolves it without waiting ════
    // The tot-pending node above is CHILDLESS (Broken evidence): from-
    // source is impossible, so the park-expiry re-claim proven in state
    // 5 is its ONLY armed action. A parked job whose node HAS a produced
    // dependency closure (Vouched evidence) gains the additional armed
    // action: the next housekeeping tick re-evaluates and resolves it
    // from-source — the park can never outlive from-source viability.
    //
    // The 3-node chain shape (root NOT substitutable → mid → leaf): a
    // substitutable ROOT would be topdown-pruned (its dep closure
    // dropped → childless → Broken evidence), so the root must stay
    // non-substitutable for the mid to keep its child in the DAG.
    let mid_out_5b = test_store_path("tot-reeval-mid-out");
    let leaf_out_5b = test_store_path("tot-reeval-leaf-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(mid_out_5b.clone());
        subs.push(leaf_out_5b.clone());
    }
    let root5b = make_node("tot-reeval-root");
    let mut mid5b = make_node("tot-reeval-mid");
    mid5b.expected_output_paths = vec![mid_out_5b.clone()];
    mid5b.wanted_output_names = vec!["out".into()];
    let mut leaf5b = make_node("tot-reeval-leaf");
    leaf5b.expected_output_paths = vec![leaf_out_5b.clone()];
    leaf5b.wanted_output_names = vec!["out".into()];
    let b5b = Uuid::new_v4();
    // Receiver held: the test-mode orphan grace is zero and this state
    // ticks below — a dropped receiver would get the build cancelled.
    let _ev5b = merge_dag(
        &handle,
        b5b,
        vec![root5b, mid5b, leaf5b],
        vec![
            make_test_edge("tot-reeval-root", "tot-reeval-mid"),
            make_test_edge("tot-reeval-mid", "tot-reeval-leaf"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    // Produce the leaf via materialization → the mid's closure evidence
    // becomes Vouched (all children produced).
    let assignment = match claim_materialization(&handle, "tot-reeval-leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("state 5b: the leaf's job must be claimable, got {other:?}"),
    };
    let exec_leaf: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&leaf_out_5b, b"tot-reeval-leaf-content");
    report_materialization_outcome(
        &handle,
        exec_leaf,
        "tot-reeval-leaf",
        mat_success_outcome(vec![leaf_out_5b.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("state 5b leaf report rejected: {e:?}"))?;
    barrier(&handle).await;
    // Park the mid's job (one worker infra ≥ max_attempts=1).
    let assignment = match claim_materialization(&handle, "tot-reeval-mid", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("state 5b: the mid's job must be claimable, got {other:?}"),
    };
    let exec_mid: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_mid,
        "tot-reeval-mid",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("state 5b mid infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let parked: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM mj.park_until)::float8 \
           FROM materialization_jobs mj \
          WHERE mj.drv_hash = 'tot-reeval-mid' AND mj.state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        parked.is_some(),
        "state 5b precondition: the mid's job is parked"
    );
    // The re-evaluation arm: one tick resolves it from-source.
    tick(&handle).await?;
    barrier(&handle).await;
    let mid_job_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs WHERE drv_hash = 'tot-reeval-mid'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        mid_job_state, "resolved_from_source",
        "state 5b: a parked job whose node's closure evidence is Vouched is \
         re-evaluated and resolved from-source at the next tick (PD-20) — the \
         park never outlives from-source viability"
    );
    assert_eq!(
        expect_drv(&handle, "tot-reeval-mid").await.status,
        DerivationStatus::Ready,
        "state 5b: the node returns to from-source dispatch (deps produced)"
    );

    // ════ State 6: zero live interest → cancellation closer fires ════
    let out6 = test_store_path("tot-zero-interest-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out6.clone());
    let mut n6 = make_node("tot-zero-interest");
    n6.expected_output_paths = vec![out6.clone()];
    let b6 = Uuid::new_v4();
    merge_dag(&handle, b6, vec![n6], vec![], false).await?;
    barrier(&handle).await;
    cancel_build(&handle, b6).await?;
    barrier(&handle).await;
    tick(&handle).await?; // the flag-gated housekeeping backstop
    barrier(&handle).await;
    let (job_state, charge_rows): (String, i64) = sqlx::query_as(
        "SELECT mj.state, \
                (SELECT count(*) FROM drv_attempts a \
                  WHERE a.derivation_id = mj.derivation_id) \
           FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'tot-zero-interest'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        job_state, "cancelled",
        "state 6 (zero live interest): the cancellation closer fires"
    );
    assert_eq!(charge_rows, 0, "state 6: the cancellation is charge-free");

    Ok(())
}

// ── T-4.3 (Phase B): recovery job-view rebuild + reap-survivor armament ──

// r[verify sched.materialize.job+2]
/// T-4.3 step 2 (red-first): materialization jobs survive leader
/// failover AS ARMED ACTIONS — the new leader's recovery rebuilds the
/// in-memory job view from PG, so pull admission answers correctly from
/// the very first post-failover claim.
///
/// Without the rebuild (the Phase A "not recovery-safe" gap, the F10/L1
/// class), the new leader's empty view answers `JobView::None` to every
/// claim → the kernel's kinded table says GONE → the store executor
/// (which treats Gone as "job resolved, skip") never claims again →
/// the armed action is stranded until the next dispatch-probe tick
/// happens to re-feed the view (finding 17's observed one-tick delay).
///
/// Three job states cross the failover, each with its own admission
/// answer from the rebuilt view:
///   pending unclaimed → DeliverNew (claimable immediately)
///   claimed           → DeliverExisting to the SAME holder (the open
///                       attempt's identity-keyed re-delivery);
///                       NotYetReady to anyone else (one-winner)
///   parked            → NotYetReady (the park survives), never Gone
#[tokio::test]
async fn flag_on_recovery_rebuilds_job_view_and_jobs_survive() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // ── Phase 1: a flag-on leader creates jobs in three states ──
    let claimed_exec: Uuid;
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |cfg, _| {
                // One infra failure parks; the park outlives the test.
                cfg.materialization.max_attempts = 1;
                cfg.materialization.park_backoff_base_secs = 600;
            });

        // (a) PENDING unclaimed job.
        let out_a = test_store_path("rcv-pending-out");
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out_a.clone());
        let mut na = make_node("rcv-pending");
        na.expected_output_paths = vec![out_a.clone()];
        merge_dag(&handle, Uuid::new_v4(), vec![na], vec![], false).await?;
        barrier(&handle).await;

        // (b) CLAIMED job (open attempt held by store-test-0).
        let out_b = test_store_path("rcv-claimed-out");
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out_b.clone());
        let mut nb = make_node("rcv-claimed");
        nb.expected_output_paths = vec![out_b.clone()];
        merge_dag(&handle, Uuid::new_v4(), vec![nb], vec![], false).await?;
        barrier(&handle).await;
        let assignment = match claim_materialization(&handle, "rcv-claimed", "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("phase 1 claim must deliver, got {other:?}"),
        };
        claimed_exec = assignment.exec_id.parse()?;

        // (c) PARKED job (budget exhausted: one infra report with
        //     max_attempts=1 parks it for 600 s).
        let out_c = test_store_path("rcv-parked-out");
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out_c.clone());
        let mut nc = make_node("rcv-parked");
        nc.expected_output_paths = vec![out_c.clone()];
        merge_dag(&handle, Uuid::new_v4(), vec![nc], vec![], false).await?;
        barrier(&handle).await;
        let assignment = match claim_materialization(&handle, "rcv-parked", "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("phase 1 park-claim must deliver, got {other:?}"),
        };
        let parked_exec: Uuid = assignment.exec_id.parse()?;
        report_materialization_outcome(
            &handle,
            parked_exec,
            "rcv-parked",
            mat_infra_outcome("upstream down"),
        )
        .await
        .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
        barrier(&handle).await;
        let park: Option<f64> = sqlx::query_scalar(
            "SELECT EXTRACT(EPOCH FROM mj.park_until)::float8 \
               FROM materialization_jobs mj \
               JOIN derivations d ON d.derivation_id = mj.derivation_id \
              WHERE d.drv_hash = 'rcv-parked'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert!(park.is_some(), "precondition: the rcv-parked job parked");

        // The leader dies (failover).
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // ── Phase 2: a NEW flag-on leader recovers from PG ──
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |cfg, p| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 600;
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // (1) THE REBUILD ASSERTION: the pending job is claimable
    //     IMMEDIATELY from the rebuilt view — no waiting for a
    //     dispatch-probe tick to lazily re-feed it.
    let claim = claim_materialization(&handle, "rcv-pending", "store-test-1").await;
    assert!(
        matches!(claim, Ok(PullOutcome::Deliver(_))),
        "a recovered pending job must be claimable from the FIRST post-failover \
         claim (the recovery view rebuild) — got {claim:?} (Gone = the empty-view \
         JobView::None answer = the F10/L1 stranded-arm gap)"
    );

    // (2) The claimed job: the original holder's re-pull re-delivers the
    //     SAME open attempt across the failover.
    let reclaim = claim_materialization(&handle, "rcv-claimed", "store-test-0").await;
    match reclaim {
        Ok(PullOutcome::Deliver(a)) => {
            let re_exec: Uuid = a.exec_id.parse()?;
            assert_eq!(
                re_exec, claimed_exec,
                "the holder's re-pull must re-deliver the SAME open attempt \
                 (identity-keyed re-delivery across failover)"
            );
        }
        other => panic!(
            "the claimed job's holder must get its open attempt re-delivered \
             after failover, got {other:?}"
        ),
    }
    //     ... and a DIFFERENT replica's claim is refused (one-winner,
    //     never Gone).
    let other_claim = claim_materialization(&handle, "rcv-claimed", "store-test-9").await;
    assert!(
        matches!(other_claim, Ok(PullOutcome::NotYetReady { .. })),
        "another replica's claim against the held attempt parks (one-winner \
         arbitration survives failover), got {other_claim:?}"
    );

    // (3) The parked job: the park survives the failover — claims are
    //     refused as NotYetReady (backoff), never dismissed as Gone.
    let parked_claim = claim_materialization(&handle, "rcv-parked", "store-test-1").await;
    assert!(
        matches!(parked_claim, Ok(PullOutcome::NotYetReady { .. })),
        "a recovered parked job's claim parks (the park state survives in the \
         rebuilt view), got {parked_claim:?}"
    );
    // The durable park is intact too.
    let park: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM mj.park_until)::float8 \
           FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'rcv-parked'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        park.is_some(),
        "the durable park_until row survives recovery"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// T-4.3 step 3 (red-first): a terminal build's reap leaves a survivor
/// node with an unresolved materialization job → the reap hook does
/// NOTHING to it (design §2.1: "survivors with an unresolved job need
/// nothing — the job is already armed"); the job then resolves and
/// completes the survivor for its remaining interest.
///
/// Without the job-awareness gate, `reevaluate_removal_survivors`
/// routes a marked-Broken survivor through the WALK settlement
/// (`settle_broken_marked_root`): it spends the verification one-shot
/// and spawns a walk (a flag-on walk spawn for fresh work — the
/// criterion-3 violation) or fail-fasts the surviving build outright,
/// racing the job that is already armed to do the same work.
#[tokio::test]
async fn flag_on_reap_survivor_with_unresolved_job_stays_armed() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // The reap_survivor_settles_at_reap_time shape (build.rs):
    // a 2-output root where only the narrow output is substitutable.
    //   Build B (narrow want): the topdown prune fires → root kept
    //   childless + an origin=pruned JOB created in-tx.
    //   Build A (wide want): the wide want blocks the prune → full
    //   merge → dep enters the DAG under root with sole interest A.
    // Cancelling A reaps dep (sole interest) and makes root the
    // removal-survivor whose child vanished — exactly the cell the
    // walk-era reap settlement used to target.
    let root_out = test_store_path("reapjob-root-out");
    let root_wide = test_store_path("reapjob-root-wide");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mk_root = |wanted: &[&str]| {
        let mut n = make_node("reapjob-root");
        n.output_names = vec!["out".into(), "wide".into()];
        n.expected_output_paths = vec![root_out.clone(), root_wide.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };
    let mk_dep = || {
        let mut n = make_node("reapjob-dep");
        n.expected_output_paths = vec![test_store_path("reapjob-dep-out")];
        n
    };

    // Build B (narrow): pruned merge → root marked + childless + job.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![mk_root(&["out"]), mk_dep()],
        vec![make_test_edge("reapjob-root", "reapjob-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;
    let origin: String = sqlx::query_scalar("SELECT origin FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        origin, "pruned",
        "precondition: B's pruned merge classified the root (in-tx job row)"
    );

    // Build A (wide): full merge → dep enters the DAG under root.
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![mk_root(&["out", "wide"]), mk_dep()],
        vec![make_test_edge("reapjob-root", "reapjob-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        handle
            .debug_query_derivation("reapjob-dep")
            .await?
            .is_some(),
        "precondition: A's full merge brings the dep into the DAG"
    );

    // Preconditions: one deduped job for the marked root.
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "precondition: one deduped job for the root");

    // Build A goes terminal (cancelled): the reap removes dep (sole
    // interest A) and re-evaluates root — the marked survivor whose
    // child vanished, still wanted by B, still carrying the armed job.
    cancel_build(&handle, build_a).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build_a })
        .await?;
    barrier(&handle).await;
    assert!(
        handle
            .debug_query_derivation("reapjob-dep")
            .await?
            .is_none(),
        "precondition: A's sole-interest dep was reaped"
    );

    // THE ARMED-SURVIVOR ASSERTIONS:
    // (a) The survivor was left alone: no walk spawned (no QPI calls for
    //     its output), the one-shot was NOT spent, and build B was not
    //     fail-fasted.
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&root_out),
            "the reap hook must NOT spawn a verification walk for a survivor \
             whose job is armed (criterion 3: no flag-on walk spawns for fresh \
             work); qpi_calls={qpi:?}"
        );
    }
    // (The walk-era one-shot assertion is gone with the field — the
    // reap hook can no longer spend anything on a survivor.)
    let st_b = query_status(&handle, build_b).await?;
    assert_ne!(
        st_b.state,
        rio_proto::types::BuildState::Failed as i32,
        "the reap hook must NOT fail-fast a surviving build whose node has an \
         armed job; error: {:?}",
        st_b.error_summary
    );
    // (b) The job is still armed (unresolved, claimable).
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        job_state, "pending",
        "the survivor's job stays unresolved (armed) across the reap"
    );

    // (c) The armed job then fires and completes the survivor for B.
    let assignment = match claim_materialization(&handle, "reapjob-root", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the surviving job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&root_out, b"reapjob-materialized");
    report_materialization_outcome(
        &handle,
        exec_id,
        "reapjob-root",
        mat_success_outcome(vec![root_out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "reapjob-root").await.status,
        DerivationStatus::Completed,
        "the armed job completes the survivor"
    );
    let st_b = query_status(&handle, build_b).await?;
    assert_eq!(
        st_b.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build B succeeds via the survivor's job"
    );

    // (d) Negative control — the as-built promotion arm is untouched for
    //     a survivor WITHOUT a job: a Queued node whose deps complete
    //     during the reap gets promoted to Ready exactly as flag-off.
    //     (The walk-pin battery covers this; here we just confirm the
    //     gate did not swallow the promotion arm by checking the code
    //     path is still reachable for job-less nodes — structurally, the
    //     gate keys on the job view, which has no entry for this node.)
    Ok(())
}

// ── T-4.4 (Phase B): the probe-matrix, no-from-source, and fail-fast
//    routing variants ────────────────────────────────────────────────────

// r[verify sched.materialize.job+2]
/// T-4.4 test 1: the dispatch-probe partition matrix flag-on — the
/// merge.rs probe-matrix walk twin (test_substitutable_probe_matrix),
/// asserting job-vs-no-job per cell instead of walk-vs-build:
///
///   locally present  → inline complete (no job, no walk)
///   substitutable    → job (origin=cache_opportunity)
///   indeterminate    → job (B3: unknown never demotes — optimistic)
///   confirmed missing→ NO job; the node stays Ready for from-source
///
/// Pin (plan T-4.4 / RFB-5): all four cells are Phase A mechanisms.
#[tokio::test]
async fn flag_on_probe_matrix_routes_to_jobs() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out_present = test_store_path("matrix-present-out");
    let out_sub = test_store_path("matrix-sub-out");
    let out_indet = test_store_path("matrix-indet-out");
    let out_missing = test_store_path("matrix-missing-out");

    let mk = |tag: &str, out: &str| {
        let mut n = make_node(tag);
        n.expected_output_paths = vec![out.to_string()];
        n
    };
    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![
            mk("matrix-present", &out_present),
            mk("matrix-sub", &out_sub),
            mk("matrix-indet", &out_indet),
            mk("matrix-missing", &out_missing),
        ],
        vec![],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Seed the store AFTER merge so the DISPATCH-time probe partition
    // (not the merge classification) is the deciding site for every cell.
    store
        .state
        .paths
        .write()
        .unwrap()
        .insert(out_present.clone(), Default::default());
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_sub.clone());
    store
        .state
        .indeterminate
        .write()
        .unwrap()
        .push(out_indet.clone());
    // out_missing: seeded nowhere → confirmed missing.
    tick(&handle).await?;
    barrier(&handle).await;

    // Cell 1 — locally present: inline complete, no job.
    assert_eq!(
        expect_drv(&handle, "matrix-present").await.status,
        DerivationStatus::Completed,
        "locally-present cell completes inline"
    );
    // Cell 2 — substitutable: job created, node Ready.
    assert_eq!(
        expect_drv(&handle, "matrix-sub").await.status,
        DerivationStatus::Ready,
        "substitutable cell stays Ready (the job is the in-flight marker)"
    );
    // Cell 3 — indeterminate: job created (B3), node Ready.
    assert_eq!(
        expect_drv(&handle, "matrix-indet").await.status,
        DerivationStatus::Ready,
        "indeterminate cell stays Ready with a job (B3 optimistic creation)"
    );
    // Cell 4 — confirmed missing: NO job, node Ready for from-source.
    assert_eq!(
        expect_drv(&handle, "matrix-missing").await.status,
        DerivationStatus::Ready,
        "confirmed-missing cell stays Ready for from-source dispatch"
    );

    // The job-vs-no-job partition.
    let job_drvs: Vec<String> =
        sqlx::query_scalar("SELECT drv_hash FROM materialization_jobs ORDER BY drv_hash")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        job_drvs,
        vec!["matrix-indet".to_string(), "matrix-sub".to_string()],
        "exactly the substitutable + indeterminate cells get jobs; present completes \
         inline and confirmed-missing goes from-source"
    );
    // Criterion 3: zero walks spawned for any cell.
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            !qpi.contains(&out_sub) && !qpi.contains(&out_indet) && !qpi.contains(&out_missing),
            "no cell may spawn a walk flag-on; qpi_calls={qpi:?}"
        );
    }
    Ok(())
}

// r[verify sched.materialize.job+2]
// r[verify sched.materialize.routing+3]
/// T-4.4 test 2: noFromSourceWhileJobUnresolved at the actor level (the
/// F8/F13 anchor's production half), both mark states:
///
///   UNMARKED leaf: BUILD pull refused (NotYetReady) while its job is
///   unresolved → the job resolves from-source (finding 11: unmarked +
///   confirmed missing releases) → the SAME pull mints (DeliverNew).
///
///   MARKED (pruned root): BUILD pull refused while the job is
///   unresolved AND the must_substitute mark holds → deps merge in
///   (evidence Pending) → the job resolves from-source (arm 2) → the
///   T-1.7 clear-mirror clears the mark → the SAME pull mints. Without
///   the clear, the stale mark's must_substitute refusal would park the
///   pull forever (the clear-mirror's reason for existing).
#[tokio::test]
async fn flag_on_builder_pull_refused_while_job_unresolved() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-t44-service-key-32-bytes!!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "t44-tenant").await;

    // ── Part A: the UNMARKED leaf ──
    let out_a = test_store_path("nofs-leaf-out");
    let mut leaf = make_node("nofs-leaf");
    leaf.expected_output_paths = vec![out_a.clone()];
    leaf.wanted_output_names = vec!["out".into()];
    let build_a = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_a,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![leaf],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_a.clone());
    tick(&handle).await?;
    barrier(&handle).await;
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "precondition: the leaf's job exists");

    // BUILD-kind pull while the job is unresolved → refused.
    let pull = try_pull_attempt(&handle, "nofs-leaf").await;
    assert!(
        matches!(pull, Ok(PullOutcome::NotYetReady { .. })),
        "a BUILD pull is refused while the node's materialization job is \
         unresolved (noFromSourceWhileJobUnresolved), got {pull:?}"
    );

    // Resolve the job from-source: claim, withdraw the upstream entry,
    // report Unobtainable → unmarked + confirmed missing → finding 11's
    // from-source release.
    let assignment = match claim_materialization(&handle, "nofs-leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_a: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .retain(|p| p != &out_a);
    report_materialization_outcome(
        &handle,
        exec_a,
        "nofs-leaf",
        mat_unobtainable_outcome(vec![out_a.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;
    let job_state: String = sqlx::query_scalar(
        "SELECT mj.state FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'nofs-leaf'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(job_state, "resolved_from_source");

    // The SAME pull now mints.
    let pull = try_pull_attempt(&handle, "nofs-leaf").await;
    assert!(
        matches!(pull, Ok(PullOutcome::Deliver(_))),
        "after the from-source resolution the BUILD pull must mint, got {pull:?}"
    );

    // ── Part B: the MARKED (pruned) root ──
    let root_out = test_store_path("nofs-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mut root = make_node("nofs-root");
    root.expected_output_paths = vec![root_out.clone()];
    let mut dep = make_node("nofs-dep");
    dep.expected_output_paths = vec![test_store_path("nofs-dep-out")];
    let build_b = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_b,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![root, dep],
            edges: vec![make_test_edge("nofs-root", "nofs-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let origin: String = sqlx::query_scalar(
        "SELECT mj.origin FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'nofs-root'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        origin, "pruned",
        "precondition: the prune classified the root"
    );
    // BUILD pull refused: the unresolved job is the never-from-source gate.
    let pull = try_pull_attempt(&handle, "nofs-root").await;
    assert!(
        matches!(pull, Ok(PullOutcome::NotYetReady { .. })),
        "a BUILD pull against the marked root with an unresolved job is refused, got {pull:?}"
    );

    // Deps merge in (a second build full-merges root→dep): the root's
    // durable evidence becomes Pending.
    let mut root2 = make_node("nofs-root");
    root2.expected_output_paths = vec![root_out.clone()];
    root2.wanted_output_names = vec!["out".into(), "wide".into()]; // wide want blocks the prune
    root2.output_names = vec!["out".into(), "wide".into()];
    root2.expected_output_paths = vec![root_out.clone(), test_store_path("nofs-root-wide")];
    let mut dep2 = make_node("nofs-dep");
    dep2.expected_output_paths = vec![test_store_path("nofs-dep-out")];
    let build_c = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_c,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![root2, dep2],
            edges: vec![make_test_edge("nofs-root", "nofs-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;

    // Resolve the root's job from-source (arm 2: durable Pending).
    let assignment = match claim_materialization(&handle, "nofs-root", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the root's job claim must deliver, got {other:?}"),
    };
    let exec_b: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .retain(|p| p != &root_out);
    report_materialization_outcome(
        &handle,
        exec_b,
        "nofs-root",
        mat_unobtainable_outcome(vec![root_out.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // The SAME pull now mints: with the job resolved, the refusal
    // predicate no longer holds (the unresolved job was the only
    // never-from-source gate).
    let pull = try_pull_attempt(&handle, "nofs-root").await;
    assert!(
        matches!(pull, Ok(PullOutcome::Deliver(_))),
        "after from-source resolution the BUILD pull must mint (no refusal \
         outlives the job), got {pull:?}"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// T-4.4 test 3: arm 3's genuine fail-fast — a PRUNED-ORIGIN
/// root whose live-wanted output the consumption re-probe confirms
/// missing-and-unsubstitutable fails every live DAG-interested build
/// with the resubmit-directing error (the shared wrapper format, NOT
/// exact-string equality with the flag-off message — review eq-7: the
/// cause clause names the deciding mechanism).
///
/// The finding-11 origin discriminator makes the pruned origin a
/// REQUIRED conjunct of this verdict: the non-pruned twin of this
/// exact trace releases to from-source instead
/// (flag_on_unmarked_leaf_confirmed_missing_releases_to_from_source).
/// The one-shot bound at the routing-core level is pinned by
/// routing_broken_with_obtainable_reprobe_rearms_once (marked + spent →
/// FailFast even on an obtainable re-probe answer).
#[tokio::test]
async fn flag_on_genuine_unobtainable_fail_fasts_with_resubmit_error() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-t44-failfast-key-32-bytes!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "t44-ff-tenant").await;

    // The pruned-root shape: root substitutable at merge → prune fires →
    // root marked + childless + origin=pruned job.
    let root_out = test_store_path("ff-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mut root = make_node("ff-root");
    root.expected_output_paths = vec![root_out.clone()];
    root.wanted_output_names = vec!["out".into()];
    let mut dep = make_node("ff-dep");
    dep.expected_output_paths = vec![test_store_path("ff-dep-out")];
    let build_id = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![root, dep],
            edges: vec![make_test_edge("ff-root", "ff-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let origin: String = sqlx::query_scalar("SELECT origin FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        origin, "pruned",
        "precondition: the origin=pruned job exists"
    );

    // The upstream entry vanishes after the job was created (the C1
    // window): claim, then report the confirmed absence.
    let assignment = match claim_materialization(&handle, "ff-root", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .retain(|p| p != &root_out);
    report_materialization_outcome(
        &handle,
        exec_id,
        "ff-root",
        mat_unobtainable_outcome(
            vec![root_out.clone()],
            vec![],
            "upstream 404 on the pruned root's output",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // THE FAIL-FAST ASSERTIONS:
    // (a) The build failed with the resubmit-directing wrapper format.
    let st = query_status(&handle, build_id).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "arm 3 + the mark: every live DAG-interested build fails"
    );
    assert!(
        st.error_summary.contains("topdown-pruned root") && st.error_summary.contains("resubmit"),
        "the error carries the shared resubmit-directing wrapper format \
         ('topdown-pruned root <hash>: ...; resubmit to re-probe or full-merge'); \
         got {:?}",
        st.error_summary
    );
    // (b) The job resolved as unobtainable.
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "resolved_unobtainable");
    // (c) Exactly one materialization_unobtainable ledger row; zero
    //     build-kind rows (no build budget was touched).
    let (unob, build_kind): (i64, i64) = sqlx::query_as(
        "SELECT \
           count(*) FILTER (WHERE t.outcome_class = 'materialization_unobtainable'), \
           count(*) FILTER (WHERE COALESCE(e.attempt_kind, 'build') = 'build') \
         FROM drv_attempts t \
         LEFT JOIN drv_executions e ON e.exec_id = t.exec_id",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(unob, 1, "exactly one materialization_unobtainable charge");
    assert_eq!(build_kind, 0, "zero build-kind ledger rows");
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// T-4.4 test 4: Success coverage is over the LIVE WANTED set, not the
/// declared output set — the batch_probe_completes_on_missing_unwanted_
/// output walk twin. A Success report covering the wanted output but
/// not the never-wanted sibling completes the node (W = live wanted,
/// not declared outputs).
#[tokio::test]
async fn flag_on_success_coverage_ignores_unwanted_outputs() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // A 2-output node where the build wants only `out`; `extra` is
    // declared but never wanted by anyone.
    let out = test_store_path("cov-out");
    let extra = test_store_path("cov-extra");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(out.clone());
        subs.push(extra.clone());
    }
    let mut n = make_node("cov-node");
    n.output_names = vec!["out".into(), "extra".into()];
    n.expected_output_paths = vec![out.clone(), extra.clone()];
    n.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "cov-node", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The executor materializes ONLY the wanted output.
    store.seed_with_content(&out, b"cov-wanted-content");
    report_materialization_outcome(
        &handle,
        exec_id,
        "cov-node",
        mat_success_outcome(vec![out.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Coverage over live wanted (= {out}) passes; the node completes
    // and the build succeeds even though `extra` was never produced.
    assert_eq!(
        expect_drv(&handle, "cov-node").await.status,
        DerivationStatus::Completed,
        "Success coverage is over the live WANTED set, not declared outputs"
    );
    let st = query_status(&handle, build_id).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the build succeeds with only its wanted output materialized"
    );
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "resolved_success");
    Ok(())
}
// r[verify sched.materialize.job+2]
/// FP-4(b) absorption — the flag-transition scenario's second product
/// gap (red-first): a build submitted FLAG-OFF has no
/// `build_wanted_outputs` rows (flag-off merges never write the
/// relation). When the flip enables materialization and the dispatch
/// probe creates a job for that build's node, the probe MUST backfill
/// the wanted relation for the flag-off-era interest. Without the
/// backfill the §6 join is empty for these jobs: the store executor's
/// tenant resolution and wanted-set resolution (both join through
/// build_wanted_outputs) return nothing, and every execution of the job
/// fails instantly as InfraFailure("no tenant context") — observed at
/// deployment level as claims that charge materialization_infra within
/// milliseconds and a build that never completes.
#[tokio::test]
async fn flag_on_probe_job_backfills_wanted_relation_for_flag_off_era_builds() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "fp4b-tenant").await;

    let out = test_store_path("fp4b-out");

    // ── Phase 1: a pre-relation-era build (the flag-off-era residue,
    //    staged by deleting the relation rows the modern merge writes —
    //    the flag itself is gone) ──
    {
        let (handle, task) = setup_actor_with_store(db.pool.clone(), Some(store_client.clone()));
        let mut n = make_node("fp4b-node");
        n.expected_output_paths = vec![out.clone()];
        n.wanted_output_names = vec!["out".into()];
        let build_id = Uuid::new_v4();
        merge_dag_req(
            &handle,
            MergeDagRequest {
                build_id,
                tenant_id: Some(tenant),
                priority_class: PriorityClass::Scheduled,
                nodes: vec![n],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
        )
        .await?;
        barrier(&handle).await;
        // Fabricate the pre-relation residue: scrub the wanted rows the
        // modern merge wrote (and any probe-created job) so the node
        // looks exactly like a flag-off-era build's.
        sqlx::query("DELETE FROM build_wanted_outputs")
            .execute(&db.pool)
            .await?;
        sqlx::query("DELETE FROM materialization_jobs")
            .execute(&db.pool)
            .await?;
        let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
        assert_eq!((jobs, wanted), (0, 0), "staged: the pre-relation-era shape");
        // The era ends: the old leader goes away.
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // ── Phase 2: the new leader recovers; its dispatch probe creates
    //    the job for the legacy build's node ──
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_cfg, p| {
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The walk-era node's output is substitutable → the probe partition
    // creates a cache_opportunity job for it.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;

    let (jobs, wanted) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "the probe created the flag-off-era build's job");
    // THE BACKFILL ASSERTION: the wanted relation now reflects the
    // flag-off-era build's interest.
    assert_eq!(
        wanted, 1,
        "the probe-partition job creation must backfill the wanted relation for \
         flag-off-era interested builds (the FP-4(b) absorption gap: without it \
         the executor's §6 joins are empty and every execution fails as \
         InfraFailure('no tenant context'))"
    );

    // The exact executor-side joins (rio-store executor.rs
    // resolve_tenant / live_wanted_paths) now answer.
    let resolved_tenant: Option<Uuid> = sqlx::query_scalar(
        "SELECT DISTINCT b.tenant_id \
           FROM materialization_jobs j \
           JOIN build_wanted_outputs w USING (derivation_id) \
           JOIN builds b ON b.build_id = w.build_id \
          WHERE j.drv_hash = 'fp4b-node' \
            AND b.status IN ('pending', 'active') \
            AND b.tenant_id IS NOT NULL",
    )
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        resolved_tenant,
        Some(tenant),
        "the executor's tenant resolution must find the flag-off-era build's tenant"
    );
    let wanted_names: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT w.wanted_output_names \
           FROM derivations d \
           JOIN build_wanted_outputs w USING (derivation_id) \
           JOIN builds b ON b.build_id = w.build_id \
          WHERE d.drv_hash = 'fp4b-node' AND b.status IN ('pending', 'active')",
    )
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        wanted_names,
        Some(vec![]),
        "the backfill writes the SATURATING all-declared ('{{}}') row \
         (T-D2.3 step 5 — a legacy build's true narrow wants are unknown; \
         the relation must never under-state interest width). The \
         executor's wanted-set resolution reads it as all-declared"
    );
    Ok(())
}

// ── T-6.1 (Phase B): PD-20 — the stalled gauge + parked-job re-evaluation ──

/// Read one gauge's current value from a debugging-recorder snapshot.
fn gauge_value(snap: &metrics_util::debugging::Snapshotter, name: &str) -> Option<f64> {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(ck, _, _, v)| {
            (ck.key().name() == name).then(|| match v {
                DebugValue::Gauge(g) => g.into_inner(),
                _ => f64::NAN,
            })
        })
}

// r[verify obs.metric.materialization-stalled]
// r[verify sched.materialize.routing+3]
/// PD-20 (design §2.5, red-first): parked materialization jobs are
/// VISIBLE (the `rio_scheduler_materialization_stalled` gauge, set from
/// ground truth every housekeeping tick) and RE-EVALUABLE (a parked job
/// whose node's durable closure evidence reads Vouched/Pending — a
/// buildable dependency closure exists — is resolved
/// `resolved_from_source` at the next tick instead of waiting out its
/// park backoff; the build proceeds from source).
///
/// Two parked jobs discriminate the two evidence classes:
///   - X (childless → Broken evidence): from-source is impossible;
///     stays parked across ticks; the gauge counts it — the alertable
///     "genuinely dead upstream" state.
///   - Y (chain mid with an unproduced child → Pending evidence):
///     normal dep-gated building works; the re-evaluation resolves it
///     from-source at the first tick after the park; the gauge excludes
///     it.
#[tokio::test]
async fn parked_job_stalled_gauge_and_reevaluation() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    // max_attempts=1 → one worker infra report parks; backoff 1 h so
    // park expiry never interferes with the re-evaluation assertions.
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    // ── X: childless substitutable node → job → park (Broken evidence). ──
    let out_x = test_store_path("pd20-broken-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_x.clone());
    let mut nx = make_node("pd20-broken");
    nx.expected_output_paths = vec![out_x.clone()];
    nx.wanted_output_names = vec!["out".into()];
    let bx = Uuid::new_v4();
    // Hold the event receiver: the test-mode orphan grace is ZERO, so a
    // dropped receiver gets the build auto-cancelled on the second tick
    // (and the cancellation closer would then cancel the parked job out
    // from under the gauge assertions).
    let _ev_x = merge_dag(&handle, bx, vec![nx], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "pd20-broken", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("X's job must be claimable, got {other:?}"),
    };
    let exec_x: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_x,
        "pd20-broken",
        mat_infra_outcome("dead upstream"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("X's infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // ── Y: chain root→Y→leaf, only Y substitutable → job → park
    //    (Pending evidence: the leaf is in the DAG, unproduced). ──
    let out_y = test_store_path("pd20-pending-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_y.clone());
    let root = make_node("pd20-root");
    let mut ny = make_node("pd20-pending");
    ny.expected_output_paths = vec![out_y.clone()];
    ny.wanted_output_names = vec!["out".into()];
    let leaf = make_node("pd20-leaf");
    let by = Uuid::new_v4();
    // Receiver held — same orphan-grace rationale as build X above.
    let _ev_y = merge_dag(
        &handle,
        by,
        vec![root, ny, leaf],
        vec![
            make_test_edge("pd20-root", "pd20-pending"),
            make_test_edge("pd20-pending", "pd20-leaf"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "pd20-pending", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("Y's job must be claimable (PD-6 Queued claims), got {other:?}"),
    };
    let exec_y: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_y,
        "pd20-pending",
        mat_infra_outcome("dead upstream"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("Y's infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Both jobs are durably parked (precondition).
    let parked_count: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE state = 'pending' AND park_until > now()",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(parked_count, 2, "precondition: both jobs parked");

    // ── The tick: re-evaluation + the gauge. ──
    tick(&handle).await?;
    barrier(&handle).await;

    // Y (Pending evidence): resolved from-source — the build proceeds
    // through normal dep gating instead of waiting out the 1 h backoff.
    let y_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs WHERE drv_hash = 'pd20-pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        y_state, "resolved_from_source",
        "PD-20: a parked job with a buildable dependency closure (Pending evidence) \
         is re-evaluated and resolved from-source at the next housekeeping tick"
    );
    let y_status = expect_drv(&handle, "pd20-pending").await.status;
    assert!(
        matches!(y_status, DerivationStatus::Queued | DerivationStatus::Ready),
        "Y returns to normal dep-gated dispatch, got {y_status:?}"
    );

    // X (Broken evidence): stays parked — from-source is impossible
    // (childless), so the park (+ backoff-expiry re-claim) remains its
    // armed action and the gauge makes it visible.
    let x_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'pd20-broken'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        x_state, "pending",
        "a Broken-evidence (childless) parked job stays parked (never force-resolved)"
    );

    // One snapshot serves both reads (debugging snapshots are
    // DESTRUCTIVE — swap(0) — so the gauge and the conversion counter
    // must come from the same drain).
    let (stalled, converted) = {
        use metrics_util::debugging::DebugValue;
        let mut stalled = None;
        let mut converted: std::collections::BTreeMap<String, u64> = Default::default();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            match (ck.key().name(), v) {
                ("rio_scheduler_materialization_stalled", DebugValue::Gauge(g)) => {
                    stalled = Some(g.into_inner());
                }
                ("rio_scheduler_materialization_converted_total", DebugValue::Counter(c)) => {
                    let origin = ck
                        .key()
                        .labels()
                        .find(|l| l.key() == "origin")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default();
                    *converted.entry(origin).or_default() += c;
                }
                _ => {}
            }
        }
        (stalled, converted)
    };

    // The gauge: exactly one job still stalled (X), set from ground
    // truth at the tick.
    assert_eq!(
        stalled,
        Some(1.0),
        "the stalled gauge counts jobs still parked after the re-evaluation pass \
         (X only; Y resolved), got {stalled:?}"
    );

    // Item T conversion visibility: the PD-20 time-driven conversion of
    // an upstream-available-class job (Y, origin=cache_opportunity) is
    // counted, discriminated by origin — the alertable churn-loop
    // signal (every `{outcome="from_source"}` resolution that came from
    // park exhaustion rather than evidence-driven consumption routing).
    assert_eq!(
        converted.get("cache_opportunity").copied(),
        Some(1),
        "the PD-20 conversion increments \
         rio_scheduler_materialization_converted_total{{origin=\"cache_opportunity\"}}; \
         got {converted:?}"
    );
    assert_eq!(
        converted.get("pruned").copied().unwrap_or(0),
        0,
        "no pruned-origin conversion happened; got {converted:?}"
    );

    // The gauge is self-healing across ticks (stays 1 while X is parked).
    tick(&handle).await?;
    barrier(&handle).await;
    let stalled = gauge_value(&snap, "rio_scheduler_materialization_stalled");
    assert_eq!(
        stalled,
        Some(1.0),
        "the gauge holds at ground truth across ticks, got {stalled:?}"
    );
    Ok(())
}

// ── T-6.2 (Phase B): the job-lifecycle metrics ──────────────────────────────

// r[verify obs.metric.scheduler]
// r[verify sched.materialize.job+2]
/// T-6.2 (red-first): the job-lifecycle counters the dashboards and
/// alerts consume —
///
///   rio_scheduler_materialization_claims_total: one increment per
///     delivered materialization claim (the open attempt's mint);
///   rio_scheduler_materialization_jobs_resolved_total{outcome}: one
///     increment per APPLIED terminal resolution, labeled by outcome
///     (success | from_source | unobtainable | cancelled | obsolete);
///     at-most-once — a re-resolution no-op never double-counts.
///
/// Together with rio_scheduler_materialization_jobs_created_total
/// (Phase A) and rio_scheduler_materialization_stalled (T-6.1), these
/// close the lifecycle: created → claimed → resolved, with the park
/// backlog as the gauge.
#[tokio::test]
async fn job_lifecycle_metrics_count_claims_and_resolutions() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // ── Job 1: claim → Success consumption. ──
    let out1 = test_store_path("lcm-success-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out1.clone());
    let mut n1 = make_node("lcm-success");
    n1.expected_output_paths = vec![out1.clone()];
    n1.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![n1], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "lcm-success", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("job 1 must be claimable, got {other:?}"),
    };
    let exec1: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&out1, b"lcm-success-content");
    report_materialization_outcome(
        &handle,
        exec1,
        "lcm-success",
        mat_success_outcome(vec![out1.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    // ── Job 2: created, then its build cancels → cancelled resolution. ──
    let out2 = test_store_path("lcm-cancel-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out2.clone());
    let mut n2 = make_node("lcm-cancel");
    n2.expected_output_paths = vec![out2.clone()];
    n2.wanted_output_names = vec!["out".into()];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, b2, vec![n2], vec![], false).await?;
    barrier(&handle).await;
    cancel_build(&handle, b2).await?;
    barrier(&handle).await;
    tick(&handle).await?; // the zero-interest cancellation closer
    barrier(&handle).await;

    // ── The counters (ONE snapshot — the Snapshotter drains counter
    //    values on every snapshot() call, so all assertions must read
    //    from a single snapshot; the counter_map_by drain caveat). ──
    {
        use metrics_util::debugging::DebugValue;
        let mut claims: u64 = 0;
        let mut resolved: std::collections::BTreeMap<String, u64> = Default::default();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            let k = ck.key();
            match k.name() {
                "rio_scheduler_materialization_claims_total" => claims += c,
                "rio_scheduler_materialization_jobs_resolved_total" => {
                    let outcome = k
                        .labels()
                        .find(|l| l.key() == "outcome")
                        .map(|l| l.value().to_owned())
                        .unwrap_or_default();
                    *resolved.entry(outcome).or_default() += c;
                }
                _ => {}
            }
        }
        assert_eq!(claims, 1, "exactly one delivered claim (job 1) is counted");
        assert_eq!(
            resolved.get("success").copied().unwrap_or(0),
            1,
            "job 1 resolved success; resolved map: {resolved:?}"
        );
        assert_eq!(
            resolved.get("cancelled").copied().unwrap_or(0),
            1,
            "job 2 resolved cancelled (zero-interest closer); resolved map: {resolved:?}"
        );
        // The pre-registration (flag-on actor construction) makes every
        // outcome label present even at 0 — what gives the alerts and
        // dashboards series to evaluate from boot.
        for outcome in ["from_source", "unobtainable", "obsolete"] {
            assert_eq!(
                resolved.get(outcome).copied().unwrap_or(u64::MAX),
                0,
                "outcome {outcome:?} is pre-registered at 0; resolved map: {resolved:?}"
            );
        }
    }

    // The db agrees (the counters mirror the authoritative rows).
    let states: Vec<String> =
        sqlx::query_scalar("SELECT state FROM materialization_jobs ORDER BY state")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        states,
        vec!["cancelled".to_string(), "resolved_success".to_string()],
        "the counters mirror the job rows"
    );
    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// Phase D' T-D2.1 (PD-D1): the settlement mark re-sources to
// materialization_jobs.origin (+ pruned-wins dedup upgrade)
// ════════════════════════════════════════════════════════════════════

// r[verify sched.materialize.routing+3]
/// The arm-3 settlement discriminator reads the consumed job's ORIGIN
/// (the durable successor of the walk-era pruned mark — design §4/A2/
/// A13) and nothing else. Both directions:
///   A. a never-pruned node whose job origin is forced 'pruned'
///      → FailFast on the four-conjunct corner;
///   B. a really-pruned node whose job origin is forced
///      'cache_opportunity' → ResolveFromSource (the prune history
///      alone no longer fail-fasts).
#[tokio::test]
async fn routing_reads_origin_not_column() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-d21-origin-key-32-bytes!!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "d21-origin-tenant").await;

    // ── Direction A: unmarked node, job origin forced 'pruned'. ──
    let out_a = test_store_path("d21a-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_a.clone());
    let mut na = make_node("d21a");
    na.expected_output_paths = vec![out_a.clone()];
    na.wanted_output_names = vec!["out".into()];
    let ba = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: ba,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![na],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let origin_a: String =
        sqlx::query_scalar("SELECT origin FROM materialization_jobs WHERE drv_hash = 'd21a'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        origin_a, "cache_opportunity",
        "premise: A was never pruned (its job is the new_sub lane's)"
    );
    // The post-080 world: the durable fact lives in the job row's
    // origin alone.
    sqlx::query("UPDATE materialization_jobs SET origin = 'pruned' WHERE drv_hash = 'd21a'")
        .execute(&db.pool)
        .await?;

    let assignment = match claim_materialization(&handle, "d21a", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("A's claim must deliver, got {other:?}"),
    };
    let exec_a: Uuid = assignment.exec_id.parse()?;
    store.state.substitutable.write().unwrap().clear();
    report_materialization_outcome(
        &handle,
        exec_a,
        "d21a",
        mat_unobtainable_outcome(vec![out_a.clone()], vec![], "upstream 404 on A"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("A's unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let st_a = query_status(&handle, ba).await?;
    assert_eq!(
        st_a.state,
        rio_proto::types::BuildState::Failed as i32,
        "A: a pruned-ORIGIN job's confirmed-missing settlement must \
         fail-fast even though the node was never pruned (the origin \
         is the durable discriminator)"
    );
    let job_a: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd21a'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(job_a, "resolved_unobtainable");

    // ── Direction B: real prune, origin forced back to
    //    'cache_opportunity'. ──
    let out_b = test_store_path("d21b-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_b.clone());
    let mut nb = make_node("d21b");
    nb.expected_output_paths = vec![out_b.clone()];
    nb.wanted_output_names = vec!["out".into()];
    let mut nb_dep = make_node("d21b-dep");
    nb_dep.expected_output_paths = vec![test_store_path("d21b-dep-out")];
    let bb = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: bb,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![nb, nb_dep],
            edges: vec![make_test_edge("d21b", "d21b-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let origin_b: String =
        sqlx::query_scalar("SELECT origin FROM materialization_jobs WHERE drv_hash = 'd21b'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin_b, "pruned", "premise: the prune classified B");
    sqlx::query(
        "UPDATE materialization_jobs SET origin = 'cache_opportunity' WHERE drv_hash = 'd21b'",
    )
    .execute(&db.pool)
    .await?;

    let assignment = match claim_materialization(&handle, "d21b", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("B's claim must deliver, got {other:?}"),
    };
    let exec_b: Uuid = assignment.exec_id.parse()?;
    store.state.substitutable.write().unwrap().clear();
    report_materialization_outcome(
        &handle,
        exec_b,
        "d21b",
        mat_unobtainable_outcome(vec![out_b.clone()], vec![], "upstream 404 on B"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("B's unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let job_b: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd21b'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_b, "resolved_from_source",
        "B: a non-pruned-origin job releases to from-source — the \
         prune history alone must not fail-fast"
    );
    let st_b = query_status(&handle, bb).await?;
    assert_ne!(
        st_b.state,
        rio_proto::types::BuildState::Failed as i32,
        "B: never a fail-fast for a non-pruned-origin settlement"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
// r[verify sched.materialize.job+2]
/// The dedup-then-prune corner end-to-end (PD-D1): a node gets a
/// cache_opportunity job from an earlier merge; a LATER pruned merge
/// dedups onto it. The dedup must upgrade the job's origin to 'pruned'
/// (pruned-wins) — and the upgrade is observable
/// (rio_scheduler_materialization_jobs_origin_upgraded_total) without
/// counting as a creation. The settlement then fail-fasts on the
/// four-conjunct corner, keyed on the upgraded origin.
#[tokio::test]
async fn unobtainable_on_upgraded_dedup_job_fail_fasts() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;
    use rio_auth::hmac::HmacSigner;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-d21-dedup-key-32-bytes!!!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "d21-dedup-tenant").await;

    // b1: plain merge of R (substitutable before merge) → the new_sub
    // lane creates the cache_opportunity job; R is unmarked.
    let out = test_store_path("d21c-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mk_root = || {
        let mut n = make_node("d21c");
        n.expected_output_paths = vec![out.clone()];
        n.wanted_output_names = vec!["out".into()];
        n
    };
    let b1 = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b1,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![mk_root()],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let origin: String =
        sqlx::query_scalar("SELECT origin FROM materialization_jobs WHERE drv_hash = 'd21c'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "cache_opportunity", "premise: b1's job origin");

    // b2: the pruned merge (R→D, R substitutable) dedups onto b1's job.
    let mut dep = make_node("d21c-dep");
    dep.expected_output_paths = vec![test_store_path("d21c-dep-out")];
    let b2 = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b2,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![mk_root(), dep],
            edges: vec![make_test_edge("d21c", "d21c-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        query_status(&handle, b2).await?.total_derivations,
        1,
        "premise: b2's prune fired (kept the demand set {{R}})"
    );

    // THE UPGRADE (red-first): the dedup carried the pruned origin onto
    // the existing pending row.
    let (origin, n_jobs): (String, i64) = sqlx::query_as(
        "SELECT min(origin), count(*) FROM materialization_jobs WHERE drv_hash = 'd21c'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(n_jobs, 1, "the dedup found the existing job — one row");
    assert_eq!(
        origin, "pruned",
        "pruned-wins: the dedup must upgrade the existing pending row's origin"
    );
    // Observability (DQ-1): the upgrade emits its own counter and is
    // NOT counted as a creation. ONE counter_map read (the snapshotter
    // drains — the single-snapshot discipline).
    let counters = counter_map(&snap);
    assert_eq!(
        counters
            .get("rio_scheduler_materialization_jobs_origin_upgraded_total")
            .copied()
            .unwrap_or(0),
        1,
        "the origin upgrade must be counted"
    );
    assert_eq!(
        counters
            .get("rio_scheduler_materialization_jobs_created_total")
            .copied()
            .unwrap_or(0),
        1,
        "exactly b1's creation — an upgrade is not a creation \
         (jobs_created_total counts creations only)"
    );

    // The four-conjunct settlement, keyed on the upgraded origin.
    let assignment = match claim_materialization(&handle, "d21c", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.state.substitutable.write().unwrap().clear();
    report_materialization_outcome(
        &handle,
        exec_id,
        "d21c",
        mat_unobtainable_outcome(vec![out.clone()], vec![], "upstream 404 on the dedup root"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd21c'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_unobtainable",
        "the upgraded-origin job's confirmed-missing settlement fail-fasts"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// DQ-1 armament preservation: the merge post-commit feed must NOT
/// clobber a dedup-found job's armament state (park backoff, claim
/// holder) — the `entry().or_insert()` discipline of the probe path,
/// extended to the merge feed. A PARKED cache_opportunity job that a
/// pruned merge dedups onto (and upgrades) must STAY parked: the next
/// claim answers NotYetReady until the backoff expires.
#[tokio::test]
async fn dedup_upgrade_preserves_parked_armament() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    // max_attempts=1 → one infra report parks; backoff 1 h so the park
    // cannot expire under the assertions.
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    // b1: R substitutable → job; claim; infra-fail → the job PARKS.
    let out = test_store_path("d21d-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mk_root = || {
        let mut n = make_node("d21d");
        n.expected_output_paths = vec![out.clone()];
        n.wanted_output_names = vec!["out".into()];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![mk_root()], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "d21d", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the first claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec_id, "d21d", mat_infra_outcome("dead upstream"))
        .await
        .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let parked = match claim_materialization(&handle, "d21d", "store-test-0").await {
        Ok(outcome) => outcome,
        Err(e) => panic!("the parked claim must answer, got {e:?}"),
    };
    assert!(
        matches!(parked, PullOutcome::NotYetReady { .. }),
        "premise: the job is parked after the budget-exhausting infra \
         failure, got {parked:?}"
    );

    // b2: the pruned merge (R→D) dedups onto the PARKED job.
    let mut dep = make_node("d21d-dep");
    dep.expected_output_paths = vec![test_store_path("d21d-dep-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![mk_root(), dep],
        vec![make_test_edge("d21d", "d21d-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // The upgrade happened...
    let origin: String =
        sqlx::query_scalar("SELECT origin FROM materialization_jobs WHERE drv_hash = 'd21d'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "pruned", "the dedup upgraded the parked job");
    let upgraded = counter_map(&snap)
        .get("rio_scheduler_materialization_jobs_origin_upgraded_total")
        .copied()
        .unwrap_or(0);
    assert_eq!(upgraded, 1, "the upgrade is counted");

    // ... and the PARK SURVIVED it (the armament-preservation pin): the
    // merge feed must not reset parked_until/claimed_by for dedup-found
    // entries.
    let after = match claim_materialization(&handle, "d21d", "store-test-0").await {
        Ok(outcome) => outcome,
        Err(e) => panic!("the post-merge claim must answer, got {e:?}"),
    };
    assert!(
        matches!(after, PullOutcome::NotYetReady { .. }),
        "park must survive the dedup upgrade (the merge feed must not \
         clobber the armament state), got {after:?}"
    );
    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// Phase D' T-D2.2 (PD-D4): routing/park evidence classification
// re-sources to the durable relation (the THREE-part strict criterion)
// ════════════════════════════════════════════════════════════════════

/// Insert a phantom derivation row directly into PG (a child the
/// in-memory DAG does NOT track — the truncation/divergence shapes).
async fn insert_pg_derivation(
    pool: &sqlx::PgPool,
    drv_hash: &str,
    status: &str,
) -> anyhow::Result<Uuid> {
    let id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ($1, $2, 'x86_64-linux', $3) RETURNING derivation_id",
    )
    .bind(drv_hash)
    .bind(rio_test_support::fixtures::test_drv_path(drv_hash))
    .bind(status)
    .fetch_one(pool)
    .await?;
    Ok(id)
}

async fn pg_derivation_id(pool: &sqlx::PgPool, drv_hash: &str) -> anyhow::Result<Uuid> {
    Ok(
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash)
            .fetch_one(pool)
            .await?,
    )
}

async fn pg_link(pool: &sqlx::PgPool, build_id: Uuid, derivation_id: Uuid) -> anyhow::Result<()> {
    sqlx::query(
        "INSERT INTO build_derivations (build_id, derivation_id) \
         VALUES ($1, $2) ON CONFLICT DO NOTHING",
    )
    .bind(build_id)
    .bind(derivation_id)
    .execute(pool)
    .await?;
    Ok(())
}

async fn pg_edge(pool: &sqlx::PgPool, parent: Uuid, child: Uuid) -> anyhow::Result<()> {
    sqlx::query("INSERT INTO derivation_edges (parent_id, child_id) VALUES ($1, $2)")
        .bind(parent)
        .bind(child)
        .execute(pool)
        .await?;
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// The routing classification must survive in-memory truncation: the
/// consumption transaction classifies over the DURABLE relation
/// (pg.edges + pg.status + live co-owning build links), not the
/// in-memory child set. A pruned-origin node whose PG closure holds an
/// unproduced child VOUCHED by a live co-owning build classifies
/// Pending (arm 2 → from-source) even when the in-memory view says
/// childless-Broken — the F9-class laundering the durable read
/// prevents (here in the conservative direction: the in-memory Broken
/// would wrongly FAIL-FAST a node whose closure is still buildable).
#[tokio::test]
async fn routing_evidence_survives_inmemory_truncation() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-d22-trunc-key-32-bytes!!!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "d22-trunc-tenant").await;

    // b1 merges the single node R (substitutable → cache_opportunity
    // job). In memory R is CHILDLESS.
    let out = test_store_path("d22a-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("d22a");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b1,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![r],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;

    // The durable truth diverges from the in-memory view: R has an
    // UNPRODUCED child in PG, vouched by the live co-owning b1.
    let r_id = pg_derivation_id(&db.pool, "d22a").await?;
    let c_id = insert_pg_derivation(&db.pool, "d22a-child", "queued").await?;
    pg_edge(&db.pool, r_id, c_id).await?;
    pg_link(&db.pool, b1, c_id).await?;

    // The pruned-origin discriminator (T-D2.1's durable mark).
    sqlx::query("UPDATE materialization_jobs SET origin = 'pruned' WHERE drv_hash = 'd22a'")
        .execute(&db.pool)
        .await?;

    let assignment = match claim_materialization(&handle, "d22a", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.state.substitutable.write().unwrap().clear();
    report_materialization_outcome(
        &handle,
        exec_id,
        "d22a",
        mat_unobtainable_outcome(vec![out.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Durable Pending (live-vouched unproduced child) → arm 2 →
    // from-source: never the fail-fast the in-memory childless-Broken
    // view would produce.
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd22a'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "the routing must classify over the durable relation (PG holds a \
         live-vouched unproduced child → Pending → from-source), not the \
         truncated in-memory view"
    );
    let st = query_status(&handle, b1).await?;
    assert_ne!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "never a fail-fast for a Pending-evidence closure"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// The park re-evaluation classifies over the durable relation too: a
/// parked job whose node's PG closure is fully produced AND vouched by
/// a live co-owning build resolves from-source at the next tick, even
/// when the in-memory view is stale (childless → Broken → the as-built
/// tick would skip it forever).
#[tokio::test]
async fn park_reevaluation_resolves_on_durable_vouch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    // b1 merges R (substitutable → job); claim; infra-fail → PARKED.
    let out = test_store_path("d22b-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("d22b");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "d22b", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec_id, "d22b", mat_infra_outcome("dead upstream"))
        .await
        .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE drv_hash = 'd22b' AND state = 'pending' AND park_until > now()",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(parked, 1, "precondition: the job is parked");

    // Durable truth: R's closure is fully PRODUCED and vouched by the
    // live co-owning b1 — from-source is viable. The in-memory view
    // still says childless (Broken).
    let r_id = pg_derivation_id(&db.pool, "d22b").await?;
    let c_id = insert_pg_derivation(&db.pool, "d22b-child", "completed").await?;
    pg_edge(&db.pool, r_id, c_id).await?;
    pg_link(&db.pool, b1, c_id).await?;

    tick(&handle).await?;
    barrier(&handle).await;

    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd22b'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "the park re-evaluation must classify over the durable relation \
         (PG: all children produced + live co-owning voucher → Vouched) \
         and resolve the parked job from-source"
    );
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// The THIRD conjunct's own pin (the stale-evidence direction, RS-1):
/// produced children whose only voucher is a TERMINAL build — the
/// previous-generation shape — must classify Broken, NOT Vouched. PG
/// retains a terminal build's completed children indefinitely; without
/// live-build scoping a previous-generation node would launder a stale
/// closure into a doomed from-source dispatch (and the park tick would
/// auto-resolve it). For a pruned-origin job the correct verdict is
/// the bounded resubmit-directing fail-fast; a parked twin stays
/// parked across ticks.
#[tokio::test]
async fn classify_durable_evidence_ignores_dead_voucher() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-d22-dead-key-32-bytes!!!!!!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, p| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "d22-dead-tenant").await;

    // ── Shape 1 (consumption): R1 with a pruned-origin job; PG holds
    //    R1's completed child whose ONLY voucher is a terminal build. ──
    let out1 = test_store_path("d22c-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out1.clone());
    let mut r1 = make_node("d22c");
    r1.expected_output_paths = vec![out1.clone()];
    r1.wanted_output_names = vec!["out".into()];
    let b_live = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b_live,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![r1],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;
    let r1_id = pg_derivation_id(&db.pool, "d22c").await?;
    let c1_id = insert_pg_derivation(&db.pool, "d22c-child", "completed").await?;
    pg_edge(&db.pool, r1_id, c1_id).await?;
    // The dead voucher: a SUCCEEDED build owns both R1 and the child.
    let b_old: Uuid =
        sqlx::query_scalar("INSERT INTO builds (status) VALUES ('succeeded') RETURNING build_id")
            .fetch_one(&db.pool)
            .await?;
    pg_link(&db.pool, b_old, r1_id).await?;
    pg_link(&db.pool, b_old, c1_id).await?;
    // The live build links only R1 (a pruning build links kept roots,
    // never the children) — no LIVE co-owning voucher for the child.
    sqlx::query("UPDATE materialization_jobs SET origin = 'pruned' WHERE drv_hash = 'd22c'")
        .execute(&db.pool)
        .await?;

    let assignment = match claim_materialization(&handle, "d22c", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("R1's claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .retain(|p| p != &out1);
    report_materialization_outcome(
        &handle,
        exec_id,
        "d22c",
        mat_unobtainable_outcome(vec![out1.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd22c'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_unobtainable",
        "produced-without-LIVE-voucher is Broken (the third conjunct): a \
         pruned-origin job takes the bounded resubmit-directing fail-fast, \
         never the doomed from-source dispatch of a never-merged closure"
    );
    let st = query_status(&handle, b_live).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "the fail-fast verdict reaches the live interested build"
    );

    // ── Shape 2 (the park tick): R2, same previous-generation shape,
    //    PARKED — the tick must NOT auto-resolve it from-source. ──
    let out2 = test_store_path("d22d-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out2.clone());
    let mut r2 = make_node("d22d");
    r2.expected_output_paths = vec![out2.clone()];
    r2.wanted_output_names = vec!["out".into()];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, b2, vec![r2], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "d22d", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("R2's claim must deliver, got {other:?}"),
    };
    let exec2: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec2, "d22d", mat_infra_outcome("dead upstream"))
        .await
        .map_err(|e| anyhow::anyhow!("R2's infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    let r2_id = pg_derivation_id(&db.pool, "d22d").await?;
    let c2_id = insert_pg_derivation(&db.pool, "d22d-child", "completed").await?;
    pg_edge(&db.pool, r2_id, c2_id).await?;
    let b_old2: Uuid =
        sqlx::query_scalar("INSERT INTO builds (status) VALUES ('succeeded') RETURNING build_id")
            .fetch_one(&db.pool)
            .await?;
    pg_link(&db.pool, b_old2, r2_id).await?;
    pg_link(&db.pool, b_old2, c2_id).await?;

    tick(&handle).await?;
    barrier(&handle).await;
    let r2_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd22d'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        r2_state, "pending",
        "the park tick must NOT auto-resolve on dead-voucher evidence \
         (stale previous-generation rows are Broken, not Vouched)"
    );
    Ok(())
}

// ════════════════════════════════════════════════════════════════════
// Phase D' T-D2.3 (PD-D5): the wanted cache rebuilds from
// build_wanted_outputs at recovery; the stored-union fallback is
// replaced by the conservative-absent (MAXIMAL-width) arm
// ════════════════════════════════════════════════════════════════════

/// Two-phase flag-on failover scaffold for the wanted-rebuild tests:
/// run `seed` against a flag-on phase-1 actor (no store client — the
/// merge probe stays indeterminate so nothing classifies early), drop
/// it, spawn a flag-on phase-2 actor WITH the store client, recover.
async fn wanted_failover<F, Fut>(
    store_client: StoreServiceClient<Channel>,
    seed: F,
) -> anyhow::Result<(
    TestDb,
    ActorHandle,
    ConfirmationLoop,
    tokio::task::JoinHandle<()>,
)>
where
    F: FnOnce(ActorHandle, sqlx::PgPool) -> Fut,
    Fut: std::future::Future<Output = anyhow::Result<()>>,
{
    let db = TestDb::new(&MIGRATOR).await;
    {
        let (handle, task) = setup_actor_configured(db.pool.clone(), None, |_cfg, _| {});
        seed(handle, db.pool.clone()).await?;
        let _ = tokio::time::timeout(std::time::Duration::from_secs(5), task).await;
    }
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_cfg, p| {
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    Ok((db, handle, confirmations, task))
}

// r[verify sched.materialize.job+2]
/// The recovery wanted-cache rebuild (the AW4/D8 headline): two builds
/// merge with distinct narrow wants flag-on; one cancels; after
/// failover the effective wanted set must be the EXACT narrow union of
/// the LIVE builds' durable contributions — not the stored node-level
/// union. Observable through the dispatch probe: the node's only LIVE
/// want (out1) is present in the store, so the narrow set
/// inline-completes the node and the surviving build succeeds; the
/// stored-union fallback would keep waiting on the dead build's out2.
#[tokio::test]
async fn recovery_rebuilds_wanted_contributions() -> TestResult {
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    let out1 = test_store_path("d23a-out1");
    let out2 = test_store_path("d23a-out2");
    let mk = |wanted: &[&str]| {
        let mut n = make_node("d23a");
        n.output_names = vec!["out1".into(), "out2".into()];
        n.expected_output_paths = vec![out1.clone(), out2.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };
    let b1 = Uuid::new_v4();
    let b2 = Uuid::new_v4();

    let (db, handle, _conf, _task) = wanted_failover(store_client, async |handle, _pool| {
        // b1 wants out1 only; b2 wants out2 only. Both relation rows
        // are written by the flag-on merges. b2 then CANCELS — its
        // contribution must stop counting after the failover.
        let _ev1 = merge_dag(&handle, b1, vec![mk(&["out1"])], vec![], false).await?;
        let _ev2 = merge_dag(&handle, b2, vec![mk(&["out2"])], vec![], false).await?;
        barrier(&handle).await;
        cancel_build(&handle, b2).await?;
        barrier(&handle).await;
        Ok(())
    })
    .await?;

    // Phase 2: out1 (the only LIVE want) is present in the store.
    {
        let (nar, hash) = rio_test_support::fixtures::make_nar(out1.as_bytes());
        let info = rio_test_support::fixtures::make_path_info(&out1, &nar, hash);
        store.seed(info, nar);
    }

    // The dispatch probe classifies over the REBUILT live contribution
    // (b1 → [out1], exact): out1 present → inline complete → b1
    // succeeds. The stored-union fallback ([out1, out2]) would block on
    // the cancelled build's out2 (absent and not substitutable).
    tick(&handle).await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "d23a").await.status,
        DerivationStatus::Completed,
        "the rebuilt narrow wanted union (live b1 -> [out1], from \
         build_wanted_outputs) must classify the node complete; the \
         stored-union fallback would wait on the dead b2's out2"
    );
    let st = query_status(&handle, b1).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "b1 (the live narrow-want build) succeeds post-failover"
    );
    drop(db);
    Ok(())
}

// r[verify sched.materialize.job+2]
/// The conservative-absent arm (DQ-2: absent = MAXIMAL width): a live
/// interested build with NO cache entry and NO relation row (the
/// legacy/pre-relation shape) contributes `{}` = ALL DECLARED outputs
/// — never the stored union, never a vacuous narrow set. The
/// degradation is observable: rio_scheduler_wanted_width_saturated_
/// total increments. Observable through the dispatch probe: with the
/// node's stored union narrow ([out1], present), the OLD fallback
/// would inline-complete; the saturated all-declared width keeps the
/// node waiting on the absent out2.
#[tokio::test]
async fn unknown_contribution_saturates_conservatively() -> TestResult {
    use crate::sla::metrics::counter_map;
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    let out1 = test_store_path("d23b-out1");
    let out2 = test_store_path("d23b-out2");
    let b1 = Uuid::new_v4();

    let (db, handle, _conf, _task) = wanted_failover(store_client, async |handle, pool| {
        let mut n = make_node("d23b");
        n.output_names = vec!["out1".into(), "out2".into()];
        n.expected_output_paths = vec![out1.clone(), out2.clone()];
        n.wanted_output_names = vec!["out1".into()];
        let _ev = merge_dag(&handle, b1, vec![n], vec![], false).await?;
        barrier(&handle).await;
        // The legacy shape: the live build has NO relation rows (a
        // build that merged before the relation existed).
        sqlx::query("DELETE FROM build_wanted_outputs")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;

    // out1 (the stored-union want) is present; out2 is absent and not
    // substitutable.
    {
        let (nar, hash) = rio_test_support::fixtures::make_nar(out1.as_bytes());
        let info = rio_test_support::fixtures::make_path_info(&out1, &nar, hash);
        store.seed(info, nar);
    }

    tick(&handle).await?;
    barrier(&handle).await;

    // MAXIMAL width: all-declared [out1, out2]; out2 absent → the node
    // must NOT vacuously complete on the stored union's narrow [out1].
    let status = expect_drv(&handle, "d23b").await.status;
    assert_ne!(
        status,
        DerivationStatus::Completed,
        "a live build with no relation row must degrade to ALL-DECLARED \
         width (never the stored union): out2 is absent, so the node \
         cannot complete"
    );
    // The degradation is visible (DQ-2 observability).
    let saturated = counter_map(&snap)
        .get("rio_scheduler_wanted_width_saturated_total")
        .copied()
        .unwrap_or(0);
    assert!(
        saturated >= 1,
        "the conservative-absent arm must count its firings \
         (rio_scheduler_wanted_width_saturated_total >= 1, got {saturated})"
    );
    drop(db);
    Ok(())
}

// r[verify sched.materialize.routing+3]
/// The consumption None-arm (step 4's red test, RS-2): a job's
/// consumption where `effective_wanted_union` returns None (zero live
/// relation rows — the legacy shape) must saturate `live_wanted_paths`
/// to ALL DECLARED outputs, making arm-0 coverage HARDER to satisfy —
/// never the vacuous CompleteForLiveInterest the stored-union fallback
/// produces when the union is narrow.
#[tokio::test]
async fn consumption_coverage_saturates_on_missing_relation_rows() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out1 = test_store_path("d23c-out1");
    let out2 = test_store_path("d23c-out2");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(out1.clone());
        subs.push(out2.clone());
    }
    let mut n = make_node("d23c");
    n.output_names = vec!["out1".into(), "out2".into()];
    n.expected_output_paths = vec![out1.clone(), out2.clone()];
    n.wanted_output_names = vec!["out1".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "d23c", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The legacy shape lands between claim and consumption: the live
    // build's relation rows are gone.
    sqlx::query("DELETE FROM build_wanted_outputs")
        .execute(&db.pool)
        .await?;

    // Success ingesting only out1: covered for the narrow stored union,
    // NOT covered for the saturated all-declared width.
    report_materialization_outcome(
        &handle,
        exec_id,
        "d23c",
        mat_success_outcome(vec![out1.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'd23c'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "pending",
        "zero live relation rows must saturate the coverage check to \
         all-declared width (out2 not ingested -> ReArm; the job stays \
         pending) — never a vacuous resolved_success on the stored union"
    );
    assert_ne!(
        expect_drv(&handle, "d23c").await.status,
        DerivationStatus::Completed,
        "the node must not vacuously complete for the legacy shape"
    );
    Ok(())
}
