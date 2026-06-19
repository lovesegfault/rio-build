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
    // (the descriptor row no longer carries created_generation — the
    // merged_bug_284 sweep trimmed load-only columns; pin via SQL)
    let created_gen: i64 =
        sqlx::query_scalar("SELECT created_generation FROM materialization_jobs WHERE job_id = $1")
            .bind(jobs[0].job_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        created_gen, 1,
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
    LiveWanted, Refusal, ReprobeAnswer, RoutingInputs, UnobtainableRouting, refusal_from_wire,
    route_unobtainable, success_covers_live_wanted,
};
use rio_evidence_kernel::ClosureEvidence as DurableEvidence;

fn paths(v: &[&str]) -> Vec<String> {
    v.iter().map(|s| (*s).to_string()).collect()
}

fn live_wanted(v: &[&str]) -> LiveWanted {
    LiveWanted::new(paths(v)).expect("test live-wanted sets are non-empty")
}

// r[verify sched.materialize.routing+7]
/// Arm 0 (moot-failure / the C3 arm): missing ∩ live-wanted = ∅ and
/// verified ⊇ live-wanted → CompleteForLiveInterest. The design's
/// §2.4 confirmed-C3-trace replay, steps 4–5: b2 cancelled in the
/// report→consume window; b1's narrower wants are covered.
#[test]
fn routing_moot_failure_completes_for_live_interest() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out2-path"]),
        missing_references: &[],
        verified_paths: &paths(&["out1-path"]),
        live_wanted_paths: &live_wanted(&["out1-path"]), // b2 gone; b1 wants out1 only
        durable_evidence: DurableEvidence::Holed,        // irrelevant for arm 0
        prior_unobtainable_count: 0,
        reprobe: None,
        pruned_origin: false,
        refusal: Refusal::None,
    });
    assert_eq!(routing, UnobtainableRouting::CompleteForLiveInterest);
}

/// Arm 0 residual: moot but not covered → re-arm (never fail-fast).
#[test]
fn routing_moot_but_uncovered_rearms() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out2-path"]),
        missing_references: &[],
        verified_paths: &paths(&[]),
        live_wanted_paths: &live_wanted(&["out1-path"]),
        durable_evidence: DurableEvidence::Holed,
        prior_unobtainable_count: 0,
        reprobe: None,
        pruned_origin: false,
        refusal: Refusal::None,
    });
    assert_eq!(routing, UnobtainableRouting::ReArm);
}

/// Arm 1: durable Vouched → ResolveFromSource.
#[test]
fn routing_durable_vouched_resolves_from_source() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out-path"]),
        missing_references: &[],
        verified_paths: &paths(&[]),
        live_wanted_paths: &live_wanted(&["out-path"]),
        durable_evidence: DurableEvidence::Vouched,
        prior_unobtainable_count: 0,
        reprobe: None,
        pruned_origin: false,
        refusal: Refusal::None,
    });
    assert_eq!(routing, UnobtainableRouting::ResolveFromSource);
}

/// Arm 2: durable Pending → ResolveFromSource (normal dep gating).
#[test]
fn routing_durable_pending_resolves_from_source() {
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&["out-path"]),
        missing_references: &[],
        verified_paths: &paths(&[]),
        live_wanted_paths: &live_wanted(&["out-path"]),
        durable_evidence: DurableEvidence::Pending,
        prior_unobtainable_count: 0,
        reprobe: None,
        pruned_origin: false,
        refusal: Refusal::None,
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
    let live = live_wanted(&["out-path"]);
    let mk = |prior: u32, reprobe| RoutingInputs {
        missing_paths: &missing,
        missing_references: &[],
        verified_paths: &verified,
        live_wanted_paths: &live,
        durable_evidence: DurableEvidence::Holed,
        prior_unobtainable_count: prior,
        reprobe,
        // The marked (topdown-pruned root) shape: the only shape where
        // the one-shot-spent settlement may fail-fast (finding 11).
        pruned_origin: true,
        refusal: Refusal::None,
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

// r[verify sched.materialize.routing+7]
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
    let live = live_wanted(&["out-path"]);
    let mk = |prior: u32, reprobe| RoutingInputs {
        missing_paths: &missing,
        missing_references: &[],
        verified_paths: &verified,
        live_wanted_paths: &live,
        durable_evidence: DurableEvidence::Holed,
        prior_unobtainable_count: prior,
        reprobe,
        pruned_origin: false,
        refusal: Refusal::None,
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
    let live = live_wanted(&["out-path"]);
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
                        missing_references: &[],
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
                            DurableEvidence::Holed
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
                        refusal: Refusal::None,
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
        &live_wanted(&["out1", "out2"]),
    ));
    // Not covered: a live-wanted path is in neither set.
    assert!(!success_covers_live_wanted(
        &paths(&["out1"]),
        &paths(&[]),
        &live_wanted(&["out1", "out2"]),
    ));
    // The empty live-wanted set is UNREPRESENTABLE (merged_bug_194):
    // the witness constructor rejects it, so the vacuously-covered
    // cell cannot reach the coverage check at all.
    assert!(LiveWanted::new(vec![]).is_none());
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
    let live = live_wanted(&["out-path"]);
    let routing = route_unobtainable(&RoutingInputs {
        missing_paths: &paths(&[]),
        missing_references: &[],
        verified_paths: &paths(&[]),
        live_wanted_paths: &live,
        durable_evidence: DurableEvidence::Holed,
        prior_unobtainable_count: 99,
        reprobe: Some(ReprobeAnswer::ConfirmedMissing),
        // Even a MARKED node: an empty missing set can never fail-fast.
        pruned_origin: true,
        refusal: Refusal::None,
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

// ── The wire-refusal decode chokepoint (bug_084) ────────────────────────
//
// Decode-law pins for `refusal_from_wire`. SIGNED Q6 (bughunt-5 §5-S
// 2026-06-09, --wipe rollout): the old-store fallback lane is RETIRED —
// field 5 (`trust_refused`) is never consulted, so the former
// `refusal_decode_falls_back_to_trust_bool` pin and the tag-6-less
// "routes exactly as pre-field" back-compat pin do not exist. The
// `Unrecognized` lane below is future-evolution robustness (an UNKNOWN
// nonzero value from a NEWER store), not a rollout hedge.

/// bug_084: an unknown wire value (a FUTURE refusal axis) decodes
/// `Unrecognized` and settles from-source — never laundered into the
/// clean lane (which is exactly what the prost accessor's
/// default-to-UNSPECIFIED would have done; the chokepoint consumes the
/// raw value via `try_from` for this reason).
#[test]
fn refusal_decode_unknown_value_settles_conservative() {
    let refusal = refusal_from_wire(99);
    assert_eq!(
        refusal,
        Refusal::Unrecognized,
        "left: {refusal:?} / right: Unrecognized (an unknown nonzero wire \
         value is a future refusal axis, not the clean lane)"
    );
    // Threaded through the routing core: the future axis settles
    // from-source over the doomed-ReArm and resubmit-loop shapes.
    let live = live_wanted(&["out-path"]);
    let missing = paths(&["out-path"]);
    for (reprobe, pruned) in [
        (Some(ReprobeAnswer::Obtainable), false),
        (Some(ReprobeAnswer::ConfirmedMissing), true),
        (None, true),
    ] {
        let routing = route_unobtainable(&RoutingInputs {
            missing_paths: &missing,
            missing_references: &[],
            verified_paths: &paths(&[]),
            live_wanted_paths: &live,
            durable_evidence: DurableEvidence::ChildlessLeaf,
            prior_unobtainable_count: 1,
            reprobe,
            pruned_origin: pruned,
            refusal,
        });
        assert_eq!(
            routing,
            UnobtainableRouting::ResolveFromSource,
            "reprobe={reprobe:?} pruned={pruned}"
        );
    }
}

/// bug_084 + SIGNED Q6: wire 0 (and absent, which shares the proto3
/// decode path) is `Refusal::None` — and field 5 is IGNORED: even the
/// incoherent shape the store mint cannot emit (trust_refused=true
/// with refusal=0) decodes None, because the echo field is dead to
/// decoders (no --wipe-retired skew lane consults it).
#[test]
fn refusal_decode_zero_is_none() {
    assert_eq!(refusal_from_wire(0), Refusal::None);
    // The known nonzero values decode their named variants.
    use rio_proto::types::UnobtainableRefusal as Wire;
    assert_eq!(refusal_from_wire(Wire::Trust.into()), Refusal::Trust);
    assert_eq!(refusal_from_wire(Wire::Content.into()), Refusal::Content);
    assert_eq!(
        refusal_from_wire(Wire::TrustAndContent.into()),
        Refusal::TrustAndContent
    );
    // Field-5 independence is structural: refusal_from_wire does not
    // even take the echo — binding it is unwritable, not reviewed.
    // (The store-side mint coherence is pinned in rio-store's
    // refusal_wire tests; this module's fixture helper derives the
    // echo the same way.)
}

// ── The consumption transaction (handler level, PG-backed) ─────────────

// r[verify sched.materialize.routing+7]
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
    let attempts: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
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

// r[verify sched.materialize.routing+7]
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
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
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
        "SELECT outcome_class, attempt_kind FROM drv_attempts \
          WHERE event_kind = 'attempt'",
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

// r[verify sched.materialize.routing+7]
/// merged_bug_189 / owner Q3 (charge-free Aborted): a worker-aborted
/// walk (SIGTERM during a store rollout) closes the attempt with ZERO
/// ledger rows of any class, the job returns to pending claimable, and
/// the node returns to Ready — routine rollouts never burn the park
/// budget. (Pre-fix RED: the proto variant did not exist; the
/// exhaustive consumption match is the compile-level red.)
#[tokio::test]
async fn aborted_outcome_closes_attempt_uncharged() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-abort-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-abort");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-abort".into(),
            auth_intent: Some("maton-abort".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0-w0".into()),
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive");
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("flag-on materialization claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The worker reports Aborted (SIGTERM mid-walk).
    let result = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("maton-abort".into()),
            payload: crate::actor::pull::PullReportPayload {
                result: rio_proto::types::BuildResult::default(),
                peak_memory_bytes: 0,
                peak_cpu_cores: 0.0,
                node_name: None,
                hw_class: None,
                final_resources: None,
                final_line_count: 0,
                materialization_outcome: Some(rio_proto::types::MaterializationOutcome {
                    outcome: Some(rio_proto::types::materialization_outcome::Outcome::Aborted(
                        rio_proto::types::materialization_outcome::Aborted {
                            detail: "walk aborted by SIGTERM (store shutdown/rollout)".into(),
                        },
                    )),
                }),
            },
            reply,
        })
        .await
        .expect("actor alive");
    assert!(result.is_ok(), "consumption must succeed: {result:?}");
    barrier(&handle).await;

    // CHARGE-FREE: zero ledger rows of any class.
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "an aborted walk charges nothing (owner Q3)");

    // The job is pending and claimable again.
    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "the job re-armed, got {jobs:?}");

    // The node returned to Ready.
    let drv = expect_drv(&handle, "maton-abort").await;
    assert_eq!(drv.status, DerivationStatus::Ready, "the node requeued");

    // And a fresh claim delivers again (no wedge: rearm + reassign).
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-abort".into(),
            auth_intent: Some("maton-abort".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-1-w0".into()),
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "the next claim must deliver after an aborted close, got {outcome:?}"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_178 (178b): a RetryLater outcome (raced placeholder /
/// upstream 429) closes the attempt with ZERO ledger rows, sets the
/// VIEW-ONLY deferral, re-arms the node — and admission refuses the
/// claim until the deferral lapses while the parked population stays
/// untouched (a 429 wave never walks a job toward PD-20). Pre-fix RED:
/// the proto variant did not exist; the consumption arm is the
/// compile-level red, and the strawman swap (RetryLater handled as
/// ack-and-ignore, the pre-fix decode of an unknown oneof) leaves the
/// attempt OPEN and the job unclaimable — captured in the commit body.
#[tokio::test]
async fn retry_later_consumption_closes_uncharged_and_defers() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-retry-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-retry");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-retry".into(),
            auth_intent: Some("maton-retry".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("flag-on materialization claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The worker reports RetryLater (upstream 429, Retry-After 60s).
    let result = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("maton-retry".into()),
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
                        rio_proto::types::materialization_outcome::Outcome::RetryLater(
                            rio_proto::types::materialization_outcome::RetryLater {
                                detail: "upstream rate-limited".into(),
                                retry_after_secs: 60,
                                class: "rate_limited".into(),
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

    // CHARGE-FREE: zero ledger rows of any class.
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "a transient retry charges nothing");

    // Durably the job is pending-unclaimed (claimable on the DB axis —
    // the deferral is deliberately VIEW-ONLY).
    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "the job re-armed durably, got {jobs:?}");

    // The node returned to Ready (rearm + reassign — no wedge).
    let drv = expect_drv(&handle, "maton-retry").await;
    assert_eq!(drv.status, DerivationStatus::Ready, "the node requeued");

    // ADMISSION refuses while the deferral is active: NotYetReady, not
    // Deliver — and NOT parked (the parked population is untouched).
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-retry".into(),
            auth_intent: Some("maton-retry".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-1-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::NotYetReady { .. })),
        "an actively-deferred job must answer NotYetReady, got {outcome:?}"
    );
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs WHERE park_until IS NOT NULL",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        parked, 0,
        "deferral is not park — the parked population is untouched"
    );
    Ok(())
}

// r[verify sched.materialize.claimability-projection+1]
/// bug_170 — listed ⊆ admittable, end-to-end: the leader listing reads
/// THE SAME [`Claimability`] law admission reads, so it can never
/// advertise work every claim of which admission refuses (the
/// store-replica NotYetReady busy-loop). The deferral axis is the
/// distinguishing case: it is VIEW-ONLY (no durable column — the 178
/// contract), so the durable listing query alone cannot hide it.
///
/// Two jobs: A driven into an active transient deferral (RetryLater
/// consumption), B fresh. The listing answers exactly [B]; B's claim
/// delivers; A's claim refuses NotYetReady — listed and admittable
/// coincide on both rows. Pre-fix RED (view filter strawman'd to
/// pass-through): the listing answered BOTH jobs —
/// `left: 2 / right: 1` — while A's every claim was refused.
#[tokio::test]
async fn deferred_job_hidden_from_listing_until_admittable() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out_a = test_store_path("matlist-defer-out");
    let out_b = test_store_path("matlist-fresh-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(out_a.clone());
        subs.push(out_b.clone());
    }
    let mut a = make_node("matlist-defer");
    a.expected_output_paths = vec![out_a];
    let mut b = make_node("matlist-fresh");
    b.expected_output_paths = vec![out_b];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![a, b], vec![], false).await?;
    barrier(&handle).await;

    // Claim A and report RetryLater → uncharged close + view-only
    // deferral (the production writer of the defer axis).
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "matlist-defer".into(),
            auth_intent: Some("matlist-defer".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the fresh job's claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let result = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("matlist-defer".into()),
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
                        rio_proto::types::materialization_outcome::Outcome::RetryLater(
                            rio_proto::types::materialization_outcome::RetryLater {
                                detail: "upstream rate-limited".into(),
                                retry_after_secs: 60,
                                class: "rate_limited".into(),
                            },
                        ),
                    ),
                }),
            },
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        result.is_ok(),
        "RetryLater consumption must ack: {result:?}"
    );
    barrier(&handle).await;

    // The listing reads the law: exactly the admittable job survives.
    let listed = list_materialization_jobs(&handle, 16).await;
    assert_eq!(
        listed.len(),
        1,
        "exactly the admittable job is listed, got {listed:?}"
    );
    assert_eq!(listed[0].drv_hash, "matlist-fresh");

    // The property's two rows: the listed job's claim DELIVERS …
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "matlist-fresh".into(),
            auth_intent: Some("matlist-fresh".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-1-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "the listed job must be admittable, got {outcome:?}"
    );
    // … and the unlisted job's claim REFUSES (same law, same answer).
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "matlist-defer".into(),
            auth_intent: Some("matlist-defer".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-1-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::NotYetReady { .. })),
        "the unlisted job must be refused, got {outcome:?}"
    );
    Ok(())
}

// r[verify sched.materialize.ack-law]
/// bug_182 (the NACK law): a consumption close that cannot become
/// durable must NACK retryably. Pre-fix, every arm acked
/// unconditionally — an ack over a lost close kills the store's 600 s
/// report redelivery, so the attempt settled ~an hour later through
/// the CHARGED 'unreported' establishment sweep. PG trigger injection
/// (drop-in-cleanup asserted below) makes the close UPDATE fail: the
/// intake must answer `Err(ConsumptionNotDurable)` with the attempt
/// still OPEN and nothing charged; dropping the trigger and
/// re-delivering the SAME outcome then consumes uncharged (the free
/// retry the law buys).
#[tokio::test]
async fn failed_close_nacks_retryably_then_redelivery_consumes() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-nack-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-nack");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "maton-nack", "store-replica-0-w0").await
    {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // Trigger injection: every UPDATE on assignments RAISEs — the
    // consumption close's write fails while everything else (the
    // already-committed mint) stands.
    sqlx::query(
        "CREATE FUNCTION rio_test_fail_assignment_update() RETURNS trigger
         LANGUAGE plpgsql AS $$ BEGIN RAISE EXCEPTION 'injected close failure (bug_182)'; END $$",
    )
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "CREATE TRIGGER rio_test_fail_close BEFORE UPDATE ON assignments
         FOR EACH ROW EXECUTE FUNCTION rio_test_fail_assignment_update()",
    )
    .execute(&db.pool)
    .await?;

    let retry_later = rio_proto::types::MaterializationOutcome {
        outcome: Some(
            rio_proto::types::materialization_outcome::Outcome::RetryLater(
                rio_proto::types::materialization_outcome::RetryLater {
                    detail: "upstream rate-limited".into(),
                    retry_after_secs: 60,
                    class: "rate_limited".into(),
                },
            ),
        ),
    };
    let result =
        report_materialization_outcome(&handle, exec_id, "maton-nack", retry_later.clone()).await;
    assert_eq!(
        result,
        Err(PullRejection::ConsumptionNotDurable),
        "a non-durable close must NACK retryably, never ack"
    );
    barrier(&handle).await;

    // NOT consumed: the assignment is still open and nothing charged —
    // the durable state is exactly as before the report.
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(open, 1, "the NACKed report must leave the attempt open");
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "a NACKed consumption charges nothing");

    // Drop-in-cleanup, asserted: the injection is scoped to exactly
    // the NACK leg of this test.
    sqlx::query("DROP TRIGGER rio_test_fail_close ON assignments")
        .execute(&db.pool)
        .await?;
    sqlx::query("DROP FUNCTION rio_test_fail_assignment_update()")
        .execute(&db.pool)
        .await?;

    // The store's redelivery (the same outcome, the same exec) now
    // consumes: closed, still uncharged, job re-armed claimable with
    // the RetryLater deferral.
    let result = report_materialization_outcome(&handle, exec_id, "maton-nack", retry_later).await;
    assert!(result.is_ok(), "redelivery must consume: {result:?}");
    barrier(&handle).await;
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(open, 0, "the redelivered report closes the attempt");
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "the transient retry stays uncharged end-to-end");
    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "the job re-armed durably, got {jobs:?}");
    Ok(())
}

// r[verify sched.materialize.ack-law]
/// merged_bug_055 C (the inverse tripwire): a view entry still CLAIMED
/// while its open assignment is gone — seeded here by closing the
/// assignment out-of-band, the crash-window / pipeline-bypass shape —
/// is the claimed-no-attempt ghost: the listing's claimed-filter hides
/// the job from every replica and, with the attempt closed, even the
/// establishment sweep (which lists OPEN attempts) never touches it.
/// Pre-fix the sweep's `claimed_by.is_some() ⇒ continue` blinder
/// skipped it forever (the red: a fresh claim still NotYetReady after
/// two sweeps). The two-strike repair releases the claim uncharged on
/// the second consecutive unbacked observation: strike on tick 1,
/// repair on tick 2, re-claimable immediately after.
// r[verify sched.materialize.claim-coherence]
#[tokio::test]
async fn claimed_no_attempt_ghost_repaired_after_two_sweeps() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-ghost-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-ghost");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "maton-ghost", "store-replica-0-w0").await
    {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let _exec_id: Uuid = assignment.exec_id.parse()?;

    // Seed the ghost: close the assignment OUT-OF-BAND (no companion
    // ran, no view update — the crash window between close-commit and
    // view mutation, or any future bypass of the witness pipeline).
    let closed = sqlx::query(
        "UPDATE assignments SET status = 'completed', completed_at = now()
         WHERE status IN ('pending', 'acknowledged')",
    )
    .execute(&db.pool)
    .await?
    .rows_affected();
    assert_eq!(closed, 1, "the seed closes exactly the open assignment");

    // Sweep 1: first unbacked observation — strike recorded, claim
    // still held (the one-sweep insurance against the snapshot race).
    tick(&handle).await?;
    let refused = claim_materialization(&handle, "maton-ghost", "store-replica-1-w0").await;
    assert!(
        matches!(refused, Ok(PullOutcome::NotYetReady { .. })),
        "one unbacked sweep must NOT yet repair (snapshot-race insurance), got {refused:?}"
    );

    // Sweep 2: second consecutive unbacked observation — the ghost is
    // repaired (claim released uncharged, job re-listable).
    tick(&handle).await?;
    let jobs = list_materialization_jobs(&handle, 16).await;
    assert_eq!(
        jobs.len(),
        1,
        "the repaired job is listed again, got {jobs:?}"
    );
    let redelivered = claim_materialization(&handle, "maton-ghost", "store-replica-1-w0").await;
    assert!(
        matches!(redelivered, Ok(PullOutcome::Deliver(_))),
        "after the repair a fresh identity must claim (pre-fix red: NotYetReady \
         forever — the blinder skipped claimed entries and establishment only \
         lists OPEN attempts), got {redelivered:?}"
    );

    // The repair is UNCHARGED: zero ledger rows of any class.
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "the ghost repair charges nothing");
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_028 (028b, settlement leg / owner Q2): the arm-3
/// re-probe asks EVERY live tenant and `ConfirmedMissing` is the
/// all-tenant conjunction — one tenant confirming missing while
/// another still sees the path substitutable keeps the job armed
/// (ReArm), never from-source/fail-fast. RED (pre-fix): one
/// find_map-picked tenant answered alone; structurally, only ONE
/// probe left the scheduler.
#[tokio::test]
async fn reprobe_confirmed_missing_requires_all_tenants() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-028b-settle-service-key-32-b".to_vec();
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let _tasks = (store_task, actor_task);
    let tenant_a = rio_store::test_helpers::seed_tenant(&db.pool, "028b-tenant-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "028b-tenant-b").await;

    // A childless leaf wanted by two builds under different tenants.
    let out = test_store_path("028b-leaf-out");
    let mut leaf = make_node("028b-leaf");
    leaf.expected_output_paths = vec![out.clone()];
    leaf.wanted_output_names = vec!["out".into()];
    for build_tenant in [tenant_a, tenant_b] {
        merge_dag_req(
            &handle,
            MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: Some(build_tenant),
                nodes: vec![leaf.clone()],
                edges: vec![],
                ..Default::default()
            },
        )
        .await?;
    }
    barrier(&handle).await;

    // The probe blip creates a cache_opportunity job.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "precondition: one job");

    // Claim, then split the tenant views: tenant A now confirms the
    // path missing-and-unsubstitutable; tenant B still sees it
    // substitutable (the global seed stands).
    let assignment = match claim_materialization(&handle, "028b-leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store
        .state
        .per_tenant_unobtainable
        .write()
        .unwrap()
        .insert(tenant_a.to_string(), vec![out.clone()]);
    let probes_before = store.calls.find_missing_tenants.read().unwrap().len();

    // The executor reports the wanted path Unobtainable.
    report_materialization_outcome(
        &handle,
        exec_id,
        "028b-leaf",
        mat_unobtainable_outcome(
            vec![out.clone()],
            vec![],
            "confirmed absent under the executing tenant",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Structural pin: the settlement re-probe asked BOTH tenants.
    {
        let probed = store.calls.find_missing_tenants.read().unwrap();
        let mut seen: Vec<&str> = probed[probes_before..]
            .iter()
            .flatten()
            .map(|s| s.as_str())
            .collect();
        seen.sort_unstable();
        seen.dedup();
        let ta = tenant_a.to_string();
        let tb = tenant_b.to_string();
        assert!(
            seen.contains(&ta.as_str()) && seen.contains(&tb.as_str()),
            "the arm-3 re-probe must ask EVERY live tenant, asked={seen:?}"
        );
    }

    // The fold is the all-tenant conjunction: tenant B's obtainable
    // view keeps the job armed — pending again, never resolved.
    let (state,): (String,) =
        sqlx::query_as("SELECT state FROM materialization_jobs WHERE drv_hash = '028b-leaf'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        state, "pending",
        "one obtainable tenant view must re-arm, not settle from-source/fail-fast"
    );
    let drv = expect_drv(&handle, "028b-leaf").await;
    assert_eq!(drv.status, DerivationStatus::Ready, "the node re-armed");
    Ok(())
}

// ── Establishment + cancellation (T-3.6) ───────────────────────────────

// r[verify sched.materialize.routing+7]
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
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
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
        "SELECT outcome_class, attempt_kind FROM drv_attempts \
          WHERE event_kind = 'attempt'",
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
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
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

// r[verify sched.gc.path-tenants-upsert]
/// Signed Q2 (bug_139): walk-Success ownership stamps INTERSECT the
/// wire-carried per-path verified-tenant sets — a Success whose walk
/// verified a path under tenant A only must NOT stamp tenant B's
/// ownership, even though B is attributed (interested). B's interest
/// stays open; a later walk that verifies under B stamps lawfully.
///
/// Strawman red (intersection reversed to the attributed cartesian —
/// the pre-Q2 shape): `rows == [A, B]` where the law demands `[A]`.
#[tokio::test]
async fn walk_success_stamps_only_wire_verified_tenants() -> TestResult {
    use rio_auth::hmac::HmacSigner;
    use sha2::Digest;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
        });
    let tenant_a = rio_store::test_helpers::seed_tenant(&db.pool, "q2-stamp-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "q2-stamp-b").await;

    let out = test_store_path("q2-stamp-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut node = make_node("q2stamp");
    node.expected_output_paths = vec![out.clone()];
    node.wanted_output_names = vec!["out".into()];

    // Two builds, two tenants, one node: both attributed.
    for tenant in [tenant_a, tenant_b] {
        let mut n = make_node("q2stamp");
        n.expected_output_paths = vec![out.clone()];
        n.wanted_output_names = vec!["out".into()];
        merge_dag_req(
            &handle,
            MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: Some(tenant),
                nodes: vec![n],
                edges: vec![],
                jwt_token: Some("harness-tenant-jwt".into()),
                ..Default::default()
            },
        )
        .await?;
    }
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "q2stamp", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The walk verified the path under tenant A ONLY.
    report_materialization_outcome(
        &handle,
        exec_id,
        "q2stamp",
        mat_success_outcome_verified(
            vec![out.clone()],
            vec![],
            vec![(out.clone(), vec![tenant_a])],
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;

    let out_hash = sha2::Sha256::digest(out.as_bytes()).to_vec();
    let mut rows: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    rows.sort();
    assert_eq!(
        rows,
        vec![tenant_a],
        "ownership stamps must INTERSECT the wire-verified sets: tenant B \
         is attributed but its view never validated the path"
    );
    Ok(())
}

// ── T-6.2: the Phase A flag-on smoke battery (the dormancy proof's
//    "dormant ≠ vestigial" half) ─────────────────────────────────────────

/// A materialization Success outcome covering `ingested` + `verified`.
///
/// Signed Q2: carries NO per-path verified-tenant sets — consumption
/// stamps nothing into `path_tenants` (the conservative wire shape).
/// Tests pinning the stamping law use
/// [`mat_success_outcome_verified`].
fn mat_success_outcome(
    ingested: Vec<String>,
    verified: Vec<String>,
) -> rio_proto::types::MaterializationOutcome {
    mat_success_outcome_verified(ingested, verified, vec![])
}

/// [`mat_success_outcome`] with explicit per-path verified-tenant
/// sets (the signed-Q2 wire: `(store_path, verified tenants)`).
fn mat_success_outcome_verified(
    ingested: Vec<String>,
    verified: Vec<String>,
    verified_tenants: Vec<(String, Vec<Uuid>)>,
) -> rio_proto::types::MaterializationOutcome {
    rio_proto::types::MaterializationOutcome {
        outcome: Some(rio_proto::types::materialization_outcome::Outcome::Success(
            rio_proto::types::materialization_outcome::Success {
                ingested_paths: ingested,
                verified_paths: verified,
                verified_tenants: verified_tenants
                    .into_iter()
                    .map(|(p, ts)| {
                        rio_proto::types::materialization_outcome::success::PathTenants {
                            store_path: p,
                            verified_tenant_ids: ts.iter().map(|t| t.to_string()).collect(),
                        }
                    })
                    .collect(),
            },
        )),
    }
}

/// A materialization Unobtainable outcome (no refusal axis).
fn mat_unobtainable_outcome(
    missing: Vec<String>,
    verified: Vec<String>,
    cause: &str,
) -> rio_proto::types::MaterializationOutcome {
    mat_unobtainable_refused(
        missing,
        verified,
        cause,
        rio_proto::types::UnobtainableRefusal::Unspecified,
    )
}

/// A materialization Unobtainable outcome carrying a typed refusal —
/// the tests' SOLE writer of the wire refusal pair (R13 witness
/// provenance): field 5 (`trust_refused`) is DERIVED from the enum
/// exactly as the store's `refusal_wire` mint derives it, so fixtures
/// cannot express wire shapes the store cannot emit (e.g. a content
/// refusal with the trust echo set).
fn mat_unobtainable_refused(
    missing: Vec<String>,
    verified: Vec<String>,
    cause: &str,
    refusal: rio_proto::types::UnobtainableRefusal,
) -> rio_proto::types::MaterializationOutcome {
    use rio_proto::types::UnobtainableRefusal as R;
    rio_proto::types::MaterializationOutcome {
        outcome: Some(
            rio_proto::types::materialization_outcome::Outcome::Unobtainable(
                rio_proto::types::materialization_outcome::Unobtainable {
                    missing_paths: missing,
                    verified_paths: verified,
                    cause: cause.into(),
                    missing_reference_paths: vec![],
                    trust_refused: matches!(refusal, R::Trust | R::TrustAndContent),
                    refusal: refusal.into(),
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
            resume_exec_id: None,
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive")
}

/// [`claim_materialization`] presenting the merged_bug_158 resume token
/// — the re-pull of a replica that already holds the open attempt and
/// proves it with the original delivery's exec id.
async fn resume_materialization(
    handle: &ActorHandle,
    drv: &str,
    instance: &str,
    exec_id: Uuid,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: drv.into(),
            auth_intent: Some(drv.into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some(instance.into()),
            resume_exec_id: Some(exec_id),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive")
}

/// [`claim_materialization`] carrying the bug_251 claim nonce: the
/// fresh-claim shape whose mint PERSISTS the nonce (the lost-response
/// credential, `assignments.claim_nonce`), and — presented tokenless —
/// the credential-only re-pull shape a worker uses when the original
/// response never arrived.
async fn claim_materialization_with_nonce(
    handle: &ActorHandle,
    drv: &str,
    instance: &str,
    nonce: Uuid,
) -> Result<PullOutcome, PullRejection> {
    handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: drv.into(),
            auth_intent: Some(drv.into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some(instance.into()),
            resume_exec_id: None,
            claim_nonce: Some(nonce),
            confirm_only: false,
            executor_token_sha256: None,
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
/// `ExecutorService.ListMaterializationJobs` drives). Instance-less —
/// the dev-mode lane: served the unpartitioned listing (live_041).
async fn list_materialization_jobs(
    handle: &ActorHandle,
    limit: u32,
) -> Vec<crate::actor::materialize::JobDescriptor> {
    handle
        .query_unchecked(|reply| ActorCommand::ListMaterializationJobs {
            limit,
            instance: None,
            reply,
        })
        .await
        .expect("actor alive")
}

/// List as an identity-bearing store worker (live_041): the verified
/// `{pod}-w{n}` member identity the gRPC chokepoint threads from the
/// signed instance claim.
async fn list_materialization_jobs_as(
    handle: &ActorHandle,
    limit: u32,
    member: &str,
) -> Vec<crate::actor::materialize::JobDescriptor> {
    handle
        .query_unchecked(|reply| ActorCommand::ListMaterializationJobs {
            limit,
            instance: Some(member.to_owned()),
            reply,
        })
        .await
        .expect("actor alive")
}

// r[verify sched.materialize.job+2]
// r[verify sched.materialize.routing+7]
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
    let loaded = suffix.get(&derivation_id).cloned().unwrap_or_default();
    // The loaded suffix includes its 085 creation-reset anchor (the
    // window cut); exactly one CHARGE row rides above it.
    let rows: Vec<_> = loaded
        .iter()
        .filter(|r| r.event_kind == crate::state::AttemptEventKind::Attempt)
        .cloned()
        .collect();
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

// r[verify sched.materialize.routing+7]
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
        "SELECT outcome_class, attempt_kind FROM drv_attempts \
          WHERE event_kind = 'attempt'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(class, "materialization_unobtainable");
    assert_eq!(joined_kind, "materialization");
    Ok(())
}

// r[verify sched.materialize.routing+7]
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
           count(*) FILTER (WHERE t.attempt_kind = 'build') \
         FROM drv_attempts t",
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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![leaf],
            edges: vec![],
            ..Default::default()
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

// r[verify store.substitute.unverifiable-token-rejects]
/// merged_bug_003 (Q3): `can_confirm` derives from the store's
/// `probe_ran_tenant_scoped` ECHO, never from the scheduler having
/// attached a probe header. The mock simulates the pre-Q3 silent
/// downgrade (header ignored, no upstream probe, empty
/// substitutable/indeterminate — wire-identical to confirmed 404s —
/// echo false): the settlement re-probe must treat every missing path
/// as NON-confirmable and re-arm, never resolve from-source off a
/// tenant-blind answer. Pre-fix `can_confirm = !probe.is_empty()`
/// (sender intent) confirmed the miss and resolved the job
/// from_source.
#[tokio::test]
async fn reprobe_scope_dropped_echo_never_confirms_missing() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-m003-echo-service-key-32-byt".to_vec();
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let _tasks = (store_task, actor_task);
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "m003-tenant").await;

    // A childless leaf; the probe blip creates the job (same shape as
    // the unmarked-leaf settlement test above).
    let out = test_store_path("m003-leaf-out");
    let mut leaf = make_node("m003-leaf");
    leaf.expected_output_paths = vec![out.clone()];
    leaf.wanted_output_names = vec!["out".into()];
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            tenant_id: Some(tenant),
            nodes: vec![leaf],
            edges: vec![],
            ..Default::default()
        },
    )
    .await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    barrier(&handle).await;
    let (jobs, _) = sdb(&db.pool).count_materialization_rows().await?;
    assert_eq!(jobs, 1, "precondition: the probe blip created one job");

    let assignment = match claim_materialization(&handle, "m003-leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The store loses its ability to honor tenant scope (HMAC
    // rotation skew at the store boundary): every subsequent
    // FindMissingPaths ignores the header, runs no upstream probe,
    // and echoes probe_ran_tenant_scoped=false.
    store
        .faults
        .drop_tenant_scope
        .store(true, std::sync::atomic::Ordering::SeqCst);

    report_materialization_outcome(
        &handle,
        exec_id,
        "m003-leaf",
        mat_unobtainable_outcome(
            vec![out.clone()],
            vec![],
            "upstream 404 on the wanted output",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // THE Q3 ASSERTION: a scope-dropped probe answer must never
    // anchor a from-source resolution (B3: identity honored, not
    // identity attached). The claim releases and the job re-arms for
    // a replica whose probe CAN be honored.
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_ne!(
        job_state, "resolved_from_source",
        "a tenant-blind (echo=false) probe answer must not confirm missing — \
         the job must re-arm, not resolve from-source"
    );
    Ok(())
}

// ── T-1.1 (Phase B): §2.6 consumer re-sourcing — the snapshot buckets ──

// r[verify sched.admin.snapshot-substituting+4]
// r[verify ctrl.scaler.signal-substituting+5]
/// §2.6 re-sourcing: CLAIMABLE materialization jobs (unclaimed, not
/// parked, not deferred) ARE the substituting bucket flag-on. A Ready
/// or Queued node carrying a claimable job counts in
/// `substituting_derivations` and is EXCLUDED from
/// `queued_derivations`/`queued_by_system` (the buckets stay disjoint
/// — builder autoscalers must not scale on work that will be
/// materialized); parked/deferred jobs leave the gauge (pacing, not
/// demand); a claimed job's node is Assigned/Running and counts in
/// `running_derivations` by construction.
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

// r[verify obs.metric.scheduler-substituting+2]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    let charges: Vec<String> =
        sqlx::query_scalar("SELECT outcome_class FROM drv_attempts WHERE event_kind = 'attempt'")
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

// r[verify sched.merge.stale-substitutable+3]
/// Floating-CA stale-reset carrier (follow-up ledger row 1): the
/// stale-Completed verify destroys the realized output path in memory
/// (`state.output_paths.clear()`), and pre-fix the `stale_reset` job
/// carried nothing — the executor's wanted set resolved through
/// `expected_output_paths == [""]` to `[]`, a vacuous
/// `Success{[],[]}` covered the empty wanted set, and the node
/// "re-completed" with `[""]` (GC retention dropped; clients got
/// `""`). The carrier (migration 082's
/// `materialization_jobs.carried_realized_paths`, written only by the
/// stale_reset origin) makes the coverage check non-vacuous and
/// restores the realized path on re-completion.
///
/// Red pre-fix: (a) the vacuous outcome RE-COMPLETED the node, and
/// (b) the honest re-fetch left `output_paths == [""]`.
#[tokio::test]
async fn flag_on_stale_reset_floating_ca_carries_realized_path() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tag = "fca-stale";
    let real = test_store_path("fca-stale-realized");

    // Floating-CA shape: path unknown until built.
    let mut node = make_node(tag);
    node.is_content_addressed = true;
    node.ca_modular_hash = [0x66u8; 32].to_vec();
    node.expected_output_paths = vec![String::new()];
    node.wanted_output_names = vec!["out".into()];

    // Build #1 holds the node in DAG; complete it with realized path R
    // (the retired walk-era staging: debug-force + debug-set).
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![node.clone()], vec![], false).await?;
    handle
        .debug_force_status(tag, DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths(tag, vec![real.clone()])
        .await?;
    barrier(&handle).await;

    // R is gone from the store but substitutable upstream.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(real.clone());

    // Build #2 re-merges the node: the verify resets it and creates the
    // stale_reset job (the IA twin pins this half).
    let b2 = Uuid::new_v4();
    let mut ev2 = merge_dag(&handle, b2, vec![node], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Ready,
        "stale verify demotes the floating-CA node"
    );
    let (origin, job_state): (String, String) = sqlx::query_as(
        "SELECT origin, state FROM materialization_jobs \
          WHERE drv_hash = $1 AND state = 'pending'",
    )
    .bind(tag)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(origin, "stale_reset");
    assert_eq!(job_state, "pending");

    // Claim #1. The SUBSTITUTING claim-intake event must carry the
    // CARRIED realized path, not the [""] placeholder (display half of
    // the same carrier; the walk-era emission carried the real fetch
    // targets).
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the stale_reset job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let substituting_paths = loop {
        let e = ev2.recv().await?;
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = e.event
            && d.kind() == rio_proto::types::DerivationEventKind::Substituting
        {
            break d.output_paths;
        }
    };
    assert_eq!(
        substituting_paths,
        vec![real.clone()],
        "the claim-intake SUBSTITUTING event must carry the carried \
         realized path, not the [\"\"] placeholder"
    );

    // (a) RED: the vacuous outcome a pre-fix executor produces (empty
    // wanted set → Success{[],[]}) must NOT re-complete the node — the
    // carried path makes coverage non-vacuous, so the job re-arms.
    report_materialization_outcome(&handle, exec_id, tag, mat_success_outcome(vec![], vec![]))
        .await
        .map_err(|e| anyhow::anyhow!("vacuous report rejected: {e:?}"))?;
    barrier(&handle).await;
    assert_ne!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Completed,
        "a vacuous Success must not complete a floating-CA stale-reset \
         node (the carried realized path is uncovered)"
    );
    let pending: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE drv_hash = $1 AND state = 'pending'",
    )
    .bind(tag)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(pending, 1, "the uncovered job re-arms (stays pending)");

    // (b) Honest re-fetch: the executor reports R covered; the node
    // re-completes WITH the realized path (not [""]).
    let assignment = match claim_materialization(&handle, tag, "store-test-1").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the re-armed job must be claimable, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    store.seed_with_content(&real, b"fca-materialized");
    report_materialization_outcome(
        &handle,
        exec_id,
        tag,
        mat_success_outcome(vec![real.clone()], vec![]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("honest report rejected: {e:?}"))?;
    barrier(&handle).await;
    let post = expect_drv(&handle, tag).await;
    assert_eq!(post.status, DerivationStatus::Completed);
    assert_eq!(
        post.output_paths,
        vec![real],
        "re-completion must stamp the carried realized path, not \
         expected_output_paths == [\"\"]"
    );
    assert_eq!(
        query_status(&handle, b2).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build #2 succeeds through the carried stale_reset job"
    );
    Ok(())
}

// ── Pruned-origin consumption shapes (T-D5.1 re-frames of the walk-era
// clear-mirror block: the mark died with the evidence columns; the
// origin row is the only durable pruned fact and resolution itself is
// the settlement) ──

// r[verify sched.materialize.routing+7]
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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;

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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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

    // ════ State 4: claimed, executor DEAD → establishment charges + PARKS ════
    // The Q5-signed residual-(a) reversal (2026-06-03): the
    // establishment channel runs the SAME park decision as a worker
    // charge. With max_attempts=1 the single establishment charge
    // exhausts the budget — the job parks (durable park_until) instead
    // of re-arming into an invisible crash-loop.
    let assignment = match claim_materialization(&handle, "tot-pending", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("state 4 claim must deliver, got {other:?}"),
    };
    let exec4: Uuid = assignment.exec_id.parse()?;
    // The replica dies (never reports). Age the open attempt past every
    // deadline+slack, then sweep. merged_bug_301: pin the pruned origin
    // so the parked state survives the same tick's re-evaluation (a
    // NON-pruned childless leaf converts now); this state's subject is
    // the establishment sweep + park, not the conversion.
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec4)
    .execute(&db.pool)
    .await?;
    set_origin_pruned(&handle, &db.pool, "tot-pending").await?;
    tick(&handle).await?;
    barrier(&handle).await;
    let (job_state, infra_rows, park4): (String, i64, Option<f64>) = sqlx::query_as(
        "SELECT mj.state, \
                (SELECT count(*) FROM drv_attempts a \
                  WHERE a.derivation_id = mj.derivation_id \
                    AND a.outcome_class = 'materialization_infra'), \
                EXTRACT(EPOCH FROM mj.park_until)::float8 \
           FROM materialization_jobs mj \
           JOIN derivations d ON d.derivation_id = mj.derivation_id \
          WHERE d.drv_hash = 'tot-pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        job_state, "pending",
        "state 4 (claimed, executor dead): the job stays pending (parked is a \
         wait, not a resolution)"
    );
    assert_eq!(
        infra_rows, 1,
        "state 4: the establishment charged exactly one materialization_infra row"
    );
    assert!(
        park4.is_some(),
        "state 4: the establishment charge runs the park decision — at \
         max_attempts=1 the job parks (the residual-(a) reversal)"
    );

    // ════ State 5: parked → refused while parked; re-claimable after the
    //      backoff expires; the WORKER channel parks identically ════
    // While parked: the claim is refused (NotYetReady — backoff unexpired).
    let refused = claim_materialization(&handle, "tot-pending", "store-test-0").await;
    assert!(
        matches!(refused, Ok(PullOutcome::NotYetReady { .. })),
        "state 5: a parked job's claim is refused while the backoff runs, got {refused:?}"
    );
    // The park is a WAIT, not a terminal state: after the backoff
    // expires (1 s base, exp 1-1=0 → 1 s here) the job is claimable again.
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let assignment = match claim_materialization(&handle, "tot-pending", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!(
            "state 5: the park backoff expiry re-arms the claim (the park is a \
             visible wait, never a strand), got {other:?}"
        ),
    };
    // The WORKER channel parks through the same chokepoint: a reported
    // infra failure (2 infra rows >= max_attempts=1) re-parks the job.
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
        "state 5 (parked): the worker-channel budget exhaustion writes a \
         durable park_until row through the same chokepoint"
    );
    // Re-claimable again after the longer backoff (exp 2-1=1 → 2 s).
    tokio::time::sleep(std::time::Duration::from_secs(3)).await;
    let reclaimed = claim_materialization(&handle, "tot-pending", "store-test-0").await;
    assert!(
        matches!(reclaimed, Ok(PullOutcome::Deliver(_))),
        "state 5: the park backoff expiry re-arms the claim after the worker \
         park too, got {reclaimed:?}"
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
                  WHERE a.derivation_id = mj.derivation_id \
                    AND a.event_kind = 'attempt') \
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
// r[verify sched.materialize.claim-resume]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // ── Phase 1: a flag-on leader creates jobs in three states ──
    let claimed_exec: Uuid;
    // bug_251: the nonce minted by the "store" for the rcv-claimed
    // claim — survives the failover client-side (the worker process
    // outlives the scheduler) and resumes the attempt tokenlessly.
    let claimed_nonce = Uuid::new_v4();
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
        // bug_251 (rule-4b): the claim carries a client-chosen nonce —
        // the mint persists it (`assignments.claim_nonce`, 096) and
        // the post-failover assertions below prove the nonce leg of
        // the credential disjunction survives recovery.
        let assignment = match claim_materialization_with_nonce(
            &handle,
            "rcv-claimed",
            "store-test-0",
            claimed_nonce,
        )
        .await
        {
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
    //     SAME open attempt across the failover — presenting the
    //     merged_bug_158 resume token (the exec id its original
    //     delivery carried; the replica survived the SCHEDULER
    //     failover, so its in-process claim state still has it). A
    //     tokenless re-pull — even from the holder — parks and settles
    //     via the establishment window (the rule-4 amendment).
    let tokenless = claim_materialization(&handle, "rcv-claimed", "store-test-0").await;
    assert!(
        matches!(tokenless, Ok(PullOutcome::NotYetReady { .. })),
        "a CREDENTIAL-LESS same-identity re-pull must NOT re-deliver \
         (158 + rule-4b: identity agreement alone is forgeable), got {tokenless:?}"
    );
    // rule-4b credential rows (bug_251):
    //   wrong nonce  → NotYetReady (a guessed/stale nonce is no credential);
    //   right nonce  → DeliverExisting, SAME exec — the nonce was
    //                  persisted by the OLD leader's mint and rehydrated
    //                  by the NEW leader's recovery (the end-to-end
    //                  persistence pin for migration 096).
    let wrong_nonce =
        claim_materialization_with_nonce(&handle, "rcv-claimed", "store-test-0", Uuid::new_v4())
            .await;
    assert!(
        matches!(wrong_nonce, Ok(PullOutcome::NotYetReady { .. })),
        "a mismatched nonce must NOT re-deliver, got {wrong_nonce:?}"
    );
    match claim_materialization_with_nonce(&handle, "rcv-claimed", "store-test-0", claimed_nonce)
        .await
    {
        Ok(PullOutcome::Deliver(a)) => {
            let re_exec: Uuid = a.exec_id.parse()?;
            assert_eq!(
                re_exec, claimed_exec,
                "the nonce-presenting tokenless re-pull must re-deliver the SAME \
                 open attempt across failover (mint persisted + recovery rehydrated)"
            );
        }
        other => panic!(
            "the holder's nonce re-pull must re-deliver across failover \
             (the lost-response credential), got {other:?}"
        ),
    }
    let reclaim =
        resume_materialization(&handle, "rcv-claimed", "store-test-0", claimed_exec).await;
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
// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![leaf],
            edges: vec![],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
            nodes: vec![root, dep],
            edges: vec![make_test_edge("nofs-root", "nofs-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
            nodes: vec![root2, dep2],
            edges: vec![make_test_edge("nofs-root", "nofs-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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

    // The job refusal no longer holds — but the root is now genuinely
    // DEP-BLOCKED (build_c full-merged root→dep and nofs-dep is
    // unbuilt), so the kinded release returned it to Queued (A2.5,
    // merged_bug_318) and the pull answers NotYetReady on STATUS, not
    // on the job. Pre-A2.5 the requeue forced Ready and this pull
    // minted a from-source build against an input that was never
    // produced.
    let pull = try_pull_attempt(&handle, "nofs-root").await;
    assert!(
        matches!(pull, Ok(PullOutcome::NotYetReady { .. })),
        "the resolved-but-dep-blocked root waits on its dep (status gate, \
         not the job refusal), got {pull:?}"
    );
    assert_eq!(
        expect_drv(&handle, "nofs-root").await.status,
        DerivationStatus::Queued,
        "dep-derived release status (deps unbuilt)"
    );

    // Build the dep; the root promotes and the pull mints — proving
    // the refusal did not outlive the job (the original property).
    let dep_assignment = pull_attempt(&handle, "nofs-dep").await;
    let dep_exec: Uuid = dep_assignment.exec_id.parse()?;
    let dep_out = test_store_path("nofs-dep-out");
    handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id: dep_exec,
            auth_intent: Some("nofs-dep".into()),
            payload: pull_payload(rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: dep_out,
                    output_hash: vec![0u8; 32],
                }],
                ..Default::default()
            }),
            reply,
        })
        .await
        .expect("actor alive")
        .map_err(|e| anyhow::anyhow!("dep success report rejected: {e:?}"))?;
    barrier(&handle).await;
    let pull = try_pull_attempt(&handle, "nofs-root").await;
    assert!(
        matches!(pull, Ok(PullOutcome::Deliver(_))),
        "with the dep built, the BUILD pull mints — no refusal outlives the \
         resolved job, got {pull:?}"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![root, dep],
            edges: vec![make_test_edge("ff-root", "ff-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
           count(*) FILTER (WHERE t.attempt_kind = 'build') \
         FROM drv_attempts t",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(unob, 1, "exactly one materialization_unobtainable charge");
    assert_eq!(build_kind, 0, "zero build-kind ledger rows");
    Ok(())
}

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
                nodes: vec![n],
                edges: vec![],
                jwt_token: Some("harness-tenant-jwt".into()),
                ..Default::default()
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
            p.service_signer = Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
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

/// Labeled-counter reader (destructive snapshot — call once per
/// assertion batch or share a drain like the PD-20 test does).
fn counter_value(
    snap: &metrics_util::debugging::Snapshotter,
    name: &str,
    labels: &[(&str, &str)],
) -> Option<u64> {
    use metrics_util::debugging::DebugValue;
    snap.snapshot()
        .into_vec()
        .into_iter()
        .find_map(|(ck, _, _, v)| {
            let key = ck.key();
            let label_match = labels
                .iter()
                .all(|(lk, lv)| key.labels().any(|l| l.key() == *lk && l.value() == *lv));
            (key.name() == name && label_match).then_some(match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
        })
}

// r[verify obs.metric.materialization-stalled+2]
// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(rio_auth::hmac::HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
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
    // merged_bug_301: a NON-pruned childless leaf now CONVERTS at the
    // re-evaluation; the stays-parked arm of this test is the
    // pruned-origin shape (closure deliberately dropped).
    set_origin_pruned(&handle, &db.pool, "pd20-broken").await?;

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

    // X (pruned-origin, childless): stays parked — the prune dropped
    // its closure on purpose, so the park (+ backoff-expiry re-claim)
    // remains its armed action and the gauge makes it visible.
    let x_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'pd20-broken'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        x_state, "pending",
        "a pruned-origin childless parked job stays parked (never force-resolved; \
         a NON-pruned childless leaf converts — merged_bug_301)"
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

// ── Item T conversion strictness (follow-up ledger row 7, second half;
//    knob default-off — the F6 closure) ──────────────────────────────────────

/// Shared staging for the strictness tests: the Y-chain (root→Y→leaf,
/// only Y substitutable, leaf unproduced ⇒ Pending evidence — the
/// from-source-viable shape PD-20 would convert) merged under `tag`,
/// Y's job claimed and parked per the caller's charge recipe.
async fn merge_pending_evidence_chain(
    handle: &ActorHandle,
    store: &rio_test_support::grpc::MockStore,
    tag: &str,
) -> anyhow::Result<(
    Uuid,
    tokio::sync::broadcast::Receiver<rio_proto::types::BuildEvent>,
)> {
    let out_y = test_store_path(&format!("{tag}-out"));
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out_y.clone());
    let root = make_node(&format!("{tag}-root"));
    let mut ny = make_node(tag);
    ny.expected_output_paths = vec![out_y];
    ny.wanted_output_names = vec!["out".into()];
    let leaf = make_node(&format!("{tag}-leaf"));
    let by = Uuid::new_v4();
    let ev = merge_dag(
        handle,
        by,
        vec![root, ny, leaf],
        vec![
            make_test_edge(&format!("{tag}-root"), tag),
            make_test_edge(tag, &format!("{tag}-leaf")),
        ],
        false,
    )
    .await?;
    barrier(handle).await;
    Ok((by, ev))
}

/// Drive ONE Scheduler-party establishment charge against `tag`'s job:
/// claim, age the open attempt past every deadline+slack, tick (the
/// establishment sweep closes it with a `materialization_infra`
/// "unreported" charge and re-arms the job claimable).
async fn drive_establishment_charge(
    handle: &ActorHandle,
    pool: &sqlx::PgPool,
    tag: &str,
    instance: &str,
) -> anyhow::Result<()> {
    let assignment = match claim_materialization(handle, tag, instance).await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => anyhow::bail!("claim for establishment must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(pool)
    .await?;
    tick(handle).await?;
    barrier(handle).await;
    Ok(())
}

// r[verify sched.materialize.conversion-strictness]
/// Knob ON, worker-charge half (the §6 block B interpretation (i) in
/// its REACHABLE form): a job parked party-blind by 2 establishment
/// charges + 1 worker charge (3 ≥ max_attempts=3) must NOT convert at
/// PD-20 — the worker-only recount (1 < 3) refuses; the job stays
/// parked, the stalled gauge counts it, the conversion counter stays
/// silent. Red pre-gate: the conversion happened anyway.
#[tokio::test]
async fn conversion_strictness_requires_worker_charges_to_exhaust() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 3;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
            cfg.materialization.conversion_requires_worker_charge = true;
        });
    let _tasks = (store_task, actor_task);

    let tag = "strict-worker";
    let (_by, _ev) = merge_pending_evidence_chain(&handle, &store, tag).await?;

    // Two Scheduler-party establishment charges...
    drive_establishment_charge(&handle, &db.pool, tag, "store-test-0").await?;
    drive_establishment_charge(&handle, &db.pool, tag, "store-test-0").await?;
    // ...plus one worker-reported InfraFailure: parks party-blind
    // (count 3 ≥ 3; OQ1 amendment 1 — parking is unchanged by the knob).
    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the worker claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec_id, tag, mat_infra_outcome("upstream 503"))
        .await
        .map_err(|e| anyhow::anyhow!("worker infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE drv_hash = $1 AND state = 'pending' AND park_until > now()",
    )
    .bind(tag)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(parked, 1, "precondition: the job parked party-blind");

    // The PD-20 tick: the strictness gate must refuse the conversion.
    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "pending",
        "knob ON: a budget exhausted by establishment charges (worker-only \
         recount 1 < 3) must NOT authorize conversion — the job stays parked"
    );
    let stalled = gauge_value(&snap, "rio_scheduler_materialization_stalled");
    assert_eq!(
        stalled,
        Some(1.0),
        "the deferred-conversion job is counted by the stalled gauge, got {stalled:?}"
    );
    let converted = counter_value(
        &snap,
        "rio_scheduler_materialization_converted_total",
        &[("origin", "cache_opportunity")],
    );
    assert!(
        matches!(converted, None | Some(0)),
        "no conversion was counted (the applied-only edge never fired; the \
         per-origin counter is pre-registered at 0), got {converted:?}"
    );
    Ok(())
}

// r[verify sched.materialize.conversion-strictness]
/// Knob ON, the condition-clears half: when worker-reported charges
/// ALONE exhaust the budget (2 of 2 — no establishment rows at all),
/// the strictness gate is satisfied and PD-20 converts exactly once
/// (the applied-only at-most-once edge).
#[tokio::test]
async fn conversion_strictness_converts_when_worker_charges_alone_exhaust() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _guard = metrics::set_default_local_recorder(&rec);

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 2;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
            cfg.materialization.conversion_requires_worker_charge = true;
        });
    let _tasks = (store_task, actor_task);

    let tag = "strict-clears";
    let (_by, _ev) = merge_pending_evidence_chain(&handle, &store, tag).await?;

    for _ in 0..2 {
        let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("worker claim must deliver, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        report_materialization_outcome(&handle, exec_id, tag, mat_infra_outcome("upstream 503"))
            .await
            .map_err(|e| anyhow::anyhow!("worker infra report rejected: {e:?}"))?;
        barrier(&handle).await;
    }

    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "worker charges alone exhausted the budget — the strict gate clears \
         and PD-20 converts"
    );
    let converted = counter_value(
        &snap,
        "rio_scheduler_materialization_converted_total",
        &[("origin", "cache_opportunity")],
    );
    assert_eq!(
        converted,
        Some(1),
        "exactly one conversion counted, got {converted:?}"
    );
    Ok(())
}

// r[verify sched.materialize.conversion-strictness]
/// Knob ON, dwell half: a freshly parked job must NOT convert before
/// `conversion_min_park_dwell_secs` has elapsed since the park began;
/// it converts at the first tick after the dwell. Red pre-gate: the
/// first tick converted immediately.
#[tokio::test]
async fn conversion_strictness_dwell_defers_then_converts() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
            cfg.materialization.conversion_min_park_dwell_secs = 3;
        });
    let _tasks = (store_task, actor_task);

    let tag = "strict-dwell";
    let (_by, _ev) = merge_pending_evidence_chain(&handle, &store, tag).await?;

    let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("worker claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec_id, tag, mat_infra_outcome("upstream 503"))
        .await
        .map_err(|e| anyhow::anyhow!("worker infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Tick well inside the dwell: must stay parked.
    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "pending",
        "the dwell gate defers conversion inside the dwell window"
    );

    // After the dwell elapses the next tick converts.
    tokio::time::sleep(std::time::Duration::from_millis(3400)).await;
    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = $1")
            .bind(tag)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "the first tick after the dwell converts"
    );
    Ok(())
}

// r[verify sched.materialize.conversion-strictness]
/// Dwell carrier recovery posture (failover-EXACT, the owner's durable
///-column choice): the dwell clock anchors to the DURABLE
/// `park_began_at`, so a failover does NOT restart it. Phase 1 parks
/// the job and lets the dwell elapse in real time; phase 2 (a fresh
/// actor recovering from PG) must convert at its first tick — a
/// rebuild-time clock would have restarted the dwell and refused.
#[tokio::test]
async fn conversion_strictness_dwell_survives_failover() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let configure = |cfg: &mut crate::actor::config::DagActorConfig| {
        cfg.materialization.max_attempts = 1;
        cfg.materialization.park_backoff_base_secs = 3600;
        cfg.materialization.park_backoff_cap_secs = 3600;
        cfg.materialization.conversion_min_park_dwell_secs = 3;
    };

    // Phase 1: park the job, let the dwell elapse.
    {
        let (store, store_client, store_task) =
            rio_test_support::grpc::spawn_mock_store_with_client().await?;
        let (handle, actor_task) =
            setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| configure(cfg));
        let tag = "strict-failover";
        let (_by, _ev) = merge_pending_evidence_chain(&handle, &store, tag).await?;
        let assignment = match claim_materialization(&handle, tag, "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("worker claim must deliver, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        report_materialization_outcome(&handle, exec_id, tag, mat_infra_outcome("upstream 503"))
            .await
            .map_err(|e| anyhow::anyhow!("worker infra report rejected: {e:?}"))?;
        barrier(&handle).await;
        let began: Option<bool> = sqlx::query_scalar(
            "SELECT park_began_at IS NOT NULL FROM materialization_jobs WHERE drv_hash = $1",
        )
        .bind(tag)
        .fetch_optional(&db.pool)
        .await?;
        assert_eq!(began, Some(true), "the park persisted park_began_at");
        tokio::time::sleep(std::time::Duration::from_millis(3400)).await;
        drop(handle);
        drop(store);
        let _ = tokio::time::timeout(Duration::from_secs(5), actor_task).await;
        store_task.abort();
    }

    // Phase 2: fresh actor recovers; its first tick must convert.
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |cfg, p| {
        configure(cfg);
        p.leader = phase2_leader;
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = $1")
            .bind("strict-failover")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "resolved_from_source",
        "the recovered leader's dwell clock anchors to the durable park_began_at \
         (elapsed pre-failover) — its first tick converts; a rebuild-time clock \
         would have restarted the dwell and refused"
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
        // The pre-registration moved from actor construction to the
        // boot path (describe_metrics → ALERT_SEEDED_COUNTERS, C3
        // metric-ownership): every outcome label is born at 0 on the
        // process scrape surface, not per-actor. Run the boot seed
        // under this recorder and assert the full outcome product is
        // present at its seeded floor (the two production increments
        // above ride on top).
        crate::describe_metrics();
        let mut seeded: std::collections::BTreeMap<String, u64> = Default::default();
        for (ck, _, _, v) in snap.snapshot().into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            if ck.key().name() == "rio_scheduler_materialization_jobs_resolved_total" {
                let outcome = ck
                    .key()
                    .labels()
                    .find(|l| l.key() == "outcome")
                    .map(|l| l.value().to_owned())
                    .unwrap_or_default();
                seeded.insert(outcome, c);
            }
        }
        for outcome in ["from_source", "unobtainable", "obsolete"] {
            assert_eq!(
                seeded.get(outcome).copied().unwrap_or(u64::MAX),
                0,
                "outcome {outcome:?} is boot-seeded at 0; seeded map: {seeded:?}"
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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![na],
            edges: vec![],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
            nodes: vec![nb, nb_dep],
            edges: vec![make_test_edge("d21b", "d21b-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![mk_root()],
            edges: vec![],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
            nodes: vec![mk_root(), dep],
            edges: vec![make_test_edge("d21c", "d21c-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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

/// Set a job's origin to `Pruned` in BOTH PG and the in-memory mirror
/// (sh-044 r2): both phase-15 arms now read `entry.origin` (the
/// PG-authoritative mirror) instead of the per-entry PG round-trip,
/// so a test that SQL-UPDATEs PG alone leaves the mirror at
/// `CacheOpportunity` and `from_source_viable(ChildlessLeaf,
/// CacheOpportunity)=true` — the parked entry resolves instead of
/// staying parked.
async fn set_origin_pruned(
    handle: &ActorHandle,
    pool: &sqlx::PgPool,
    drv_hash: &str,
) -> anyhow::Result<()> {
    sqlx::query("UPDATE materialization_jobs SET origin = 'pruned' WHERE drv_hash = $1")
        .bind(drv_hash)
        .execute(pool)
        .await?;
    let ok = handle
        .debug_set_job_origin(drv_hash, JobOrigin::Pruned)
        .await?;
    anyhow::ensure!(ok, "no view entry for {drv_hash} to set origin on");
    Ok(())
}

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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![r],
            edges: vec![],
            ..Default::default()
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

// r[verify sched.materialize.routing+7]
/// The park re-evaluation classifies over the durable relation too: a
/// parked job whose node's PG closure is fully produced AND vouched by
/// a live co-owning build resolves from-source at the next tick, even
/// when the in-memory view is stale (childless → Broken → the as-built
/// tick would skip it forever).
#[tokio::test]
async fn park_reevaluation_resolves_on_durable_vouch() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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

// r[verify sched.materialize.routing+7]
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
            nodes: vec![r1],
            edges: vec![],
            ..Default::default()
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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

// r[verify sched.materialize.routing+7]
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

// ── A1 Layer 2: outcome-derived view settlement (133/276) ──────────────────
//
// The job-view is a droppable cache of the durable job table; a view
// REMOVAL is only sound when the durable resolution SETTLED
// (Applied/AlreadyResolved). A Fenced or Failed durable write keeping
// the view entry is what makes the armed action level-triggered
// instead of stranding the pending row invisibly.

// r[verify obs.metric.scheduler]
/// bug_086: the zero-width event (NoVerifiableSet) is a CONSUMPTION
/// event — pre-fix the note + warn fired BEFORE close_for_consumption,
/// so a deposed believer's Deferred close counted the event the
/// successor counts again, and a NotDurable NACK (store-redelivered
/// same outcome, up to 600s) counted once per delivery attempt —
/// contradicting the HELP ("closed uncharged and deferred").
///
/// The non-settled legs are now UNREPRESENTABLE rather than tested:
/// constructing `WidthEvent::NoVerifiableSet` demands the
/// `&SettledClose` witness, which only `close_for_consumption`'s
/// settled arm produces (compile-level red — the pre-fix call site,
/// note-before-close, does not typecheck; driving the Deferred leg
/// end-to-end is blocked by upstream report guards, verified
/// empirically). This test pins the settled path end-to-end: the arm
/// fires through the production handler and counts exactly once.
#[tokio::test]
async fn zero_width_event_counts_only_on_settled_close() -> TestResult {
    let rec = rio_test_support::metrics::CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&rec);

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let counter = || rec.get("rio_scheduler_materialization_no_verifiable_wanted_total{}");

    let out1 = test_store_path("zw-settled-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(out1.clone());
    let mut n1 = make_node("zw-settled");
    n1.expected_output_paths = vec![out1.clone()];
    // The zero-width shape: live interest WANTS an output name the
    // node never declares — the wanted union resolves to no
    // verifiable path (LiveWanted::new -> None), with the build alive
    // and the leader serving, so the close SETTLES.
    n1.wanted_output_names = vec!["name-the-node-never-declares".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![n1], vec![], false).await?;
    barrier(&handle).await;
    let a1 = match claim_materialization(&handle, "zw-settled", "store-replica-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    report_materialization_outcome(
        &handle,
        a1.exec_id.parse()?,
        "zw-settled",
        mat_success_outcome(vec![], vec![]),
    )
    .await
    .expect("settled zero-width close acks");
    barrier(&handle).await;
    assert_eq!(counter(), 1, "the settled zero-width close counts ONCE");
    Ok(())
}

// r[verify sched.materialize.view-settlement]
/// A deposed believer's zero-interest cancel is refused by the fence —
/// and the view entry SURVIVES the refusal, so the cancel re-attempts
/// every tick (level-triggered) instead of stranding the durable
/// pending row behind an empty view (the 133 class).
#[tokio::test]
async fn fenced_resolve_keeps_view_entry_and_gates() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out = test_store_path("matview-fence-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("matview-fence");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    let jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE state = 'pending'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(jobs, 1, "precondition: the probe partition created the job");

    // The only interested build goes terminal (zero live interest)…
    cancel_build(&handle, build_id).await?;
    barrier(&handle).await;
    // …then a successor claims generation 2: this actor is now a
    // deposed believer (it never observes a lease transition).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&db.pool)
    .await?;

    let before = handle.debug_counters().await?.evidence_writes_fenced;
    tick(&handle).await?;
    barrier(&handle).await;
    let after_first = handle.debug_counters().await?.evidence_writes_fenced;
    assert!(
        after_first > before,
        "the deposed cancel must be refused by the fence (counter {before} -> {after_first})"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        state, "pending",
        "the fenced cancel must leave the durable row pending"
    );

    // THE 133 OBSERVABLE: a second tick must RE-ATTEMPT the cancel
    // (the view entry survived the Fenced disposition). Pre-fix the
    // entry was removed unconditionally and the second tick finds
    // nothing — the fenced counter freezes while the durable row
    // stays pending forever (until recovery).
    tick(&handle).await?;
    barrier(&handle).await;
    let after_second = handle.debug_counters().await?.evidence_writes_fenced;
    assert!(
        after_second > after_first,
        "the view entry must survive a Fenced resolution: tick 2 re-attempts the \
         cancel ({after_first} -> {after_second}); a removed entry strands the \
         durable pending row behind an empty view"
    );
    let state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        state, "pending",
        "still pending — only a live leader settles it"
    );
    Ok(())
}

// r[verify sched.materialize.view-settlement]
/// The zero-interest cancel is TOTAL over the DAG-absent arm (276):
/// after the sole-interest build's cleanup reaps the node, the cancel
/// must still close the open materialization attempt — in the same
/// fenced transaction as the job resolution, charge-free, with no
/// in-memory exec_id read. Pre-fix the exec_id came from the (gone)
/// DAG node, the attempt stayed open, and the establishment sweep
/// later converted the leak into a materialization_infra charge.
#[tokio::test]
async fn zero_interest_cancel_closes_attempt_without_dag_node() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out = test_store_path("matview-nonode-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("matview-nonode");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    let outcome = claim_materialization(&handle, "matview-nonode", "store-replica-0").await;
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "claim delivered, got {outcome:?}"
    );

    // Build terminal + cleanup: the sole-interest node is reaped from
    // the DAG while the materialization attempt is still open.
    cancel_build(&handle, build_id).await?;
    barrier(&handle).await;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await?;
    barrier(&handle).await;
    assert!(
        handle
            .debug_query_derivation("matview-nonode")
            .await?
            .is_none(),
        "precondition: the cancelled sole-interest node was reaped"
    );
    // Re-open the attempt row: the cancel transition's terminal
    // persist fold (I-209) closed it alongside the node, but the
    // node-absent + open-attempt state is exactly what a post-failover
    // view rebuild presents (the job and its open attempt are durable;
    // the reaped node is not). The zero-interest arm must be TOTAL
    // over that state — the close may never key on the in-memory node
    // its own trigger arm (`None => true`) guarantees absent.
    let exec_id: Uuid = sqlx::query_scalar("SELECT exec_id FROM assignments LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    sqlx::query(
        "UPDATE assignments SET status = 'pending', completed_at = NULL WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(&db.pool)
    .await?;

    // The zero-interest tick: cancel the job AND close the attempt,
    // node-absent, in one fenced transaction.
    tick(&handle).await?;
    barrier(&handle).await;
    let job_state: String = sqlx::query_scalar("SELECT state FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(job_state, "cancelled");
    let open: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE status IN ('pending', 'acknowledged')",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        open, 0,
        "the open materialization attempt must be closed by the node-absent cancel"
    );
    let closed_status: String =
        sqlx::query_scalar("SELECT status FROM assignments ORDER BY assigned_at DESC LIMIT 1")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        closed_status, "cancelled",
        "closed BY the cancel (charge-free), not failed by a later sweep"
    );

    // No charge ever lands: zero rows now, and the establishment sweep
    // has nothing to establish (the leaked-attempt path is unreachable
    // for cancelled jobs).
    let exec_id: Uuid = sqlx::query_scalar("SELECT exec_id FROM assignments LIMIT 1")
        .fetch_one(&db.pool)
        .await?;
    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(&db.pool)
    .await?;
    tick(&handle).await?;
    barrier(&handle).await;
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        charges, 0,
        "a cancelled job's closed attempt must never be establishment-charged"
    );
    Ok(())
}

// r[verify sched.materialize.view-settlement]
/// A deposed believer's establishment sweep: the close is fenced (no
/// charge row) AND the companions gate on the close disposition — no
/// rearm, no requeue. Pre-fix both ran unconditionally: a deposed
/// actor rearmed the view and requeued the node it no longer owns.
#[tokio::test]
async fn deposed_establishment_performs_no_rearm_or_requeue() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out = test_store_path("est-mat-fence-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("est-mat-fence");
    n.expected_output_paths = vec![out];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;
    let outcome = claim_materialization(&handle, "est-mat-fence", "store-replica-0").await;
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("expected Deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let status_claimed = expect_drv(&handle, "est-mat-fence").await.status;

    sqlx::query(
        "UPDATE assignments SET assigned_at = now() - interval '100 days' WHERE exec_id = $1",
    )
    .bind(exec_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&db.pool)
    .await?;

    tick(&handle).await?;
    barrier(&handle).await;

    let rows: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(rows, 0, "the deposed establishment's charge must be fenced");
    assert_eq!(
        expect_drv(&handle, "est-mat-fence").await.status,
        status_claimed,
        "a deposed establishment must not requeue the node (the companion \
         actions gate on the close disposition)"
    );
    Ok(())
}

// r[verify sched.attempt.synthesized-verdict+4]
/// merged_bug_146 (A2): controller-facing attempt surfaces are
/// kind-typed. A controller-synthesized verdict (`ReportAttemptOutcome`
/// with reason Reaped/Cancelled/Preempted) names a BUILD-lifecycle
/// event — the controller deleting a builder Job — and MUST NOT consume
/// a store replica's open MATERIALIZATION attempt: the report is
/// acknowledged charge-free, no drv_attempts row is written, the
/// assignment stays active, the in-memory job view stays Claimed, and
/// the store's own later outcome report consumes the attempt normally.
/// Pre-fix the kind-blind synthesized arm closed the attempt and
/// requeued the node out from under the still-running store fetch.
#[tokio::test]
async fn controller_verdict_never_consumes_materialization_attempt() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("mat-ctrlverdict-out");
    let mut d1 = make_node("mat-ctrlverdict");
    d1.expected_output_paths = vec![out.clone()];
    d1.wanted_output_names = vec!["out".into()];
    merge_dag(&handle, Uuid::new_v4(), vec![d1], vec![], false).await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;

    let assignment = match claim_materialization(&handle, "mat-ctrlverdict", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // The controller's synthesized verdict for the (builder-shaped)
    // attempt identity arrives — e.g. a reap pass that resolved the
    // intent to this open attempt.
    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ReportAttemptOutcome {
            identity: crate::actor::pull::AttemptIdentity {
                intent_id: Some("mat-ctrlverdict".into()),
                job_name: None,
                exec_id: Some(exec_id),
            },
            reason: rio_proto::types::AttemptTerminalReason::Reaped,
            node_name: None,
            resubmit_cycle: 0,
            reply: reply_tx,
        })
        .await?;
    // Row F3 of the merged_bug_080 attempt_resolved per-arm census
    // (the table's home is pull.rs::report_ack_attempt_resolved_per_arm_census;
    // this arm needs the store-claim harness): the materialization-kind
    // refusal is charge-free — Unresolved on the wire bit.
    assert_eq!(
        reply_rx.await?.expect("verdict acked"),
        crate::actor::pull::AttemptResolution::Unresolved,
        "F3: a controller verdict on a materialization attempt acks Unresolved"
    );

    let rows: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE exec_id = $1")
        .bind(exec_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        rows, 0,
        "a controller verdict must write no row for a materialization attempt"
    );
    let active: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM assignments WHERE exec_id = $1 \
         AND status IN ('pending', 'acknowledged')",
    )
    .bind(exec_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(active, 1, "the materialization attempt stays open");
    assert_eq!(
        expect_drv(&handle, "mat-ctrlverdict").await.status,
        DerivationStatus::Running,
        "the node stays Running under the open store claim"
    );

    // The store's own success report still consumes normally.
    report_materialization_outcome(
        &handle,
        exec_id,
        "mat-ctrlverdict",
        mat_success_outcome(vec![out.clone()], vec![out.clone()]),
    )
    .await
    .map_err(|e| anyhow::anyhow!("success report rejected: {e:?}"))?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "mat-ctrlverdict").await.status,
        DerivationStatus::Completed,
        "the store's own report consumes the attempt normally after the ignored verdict"
    );
    Ok(())
}

// r[verify sched.materialize.job+2]
/// bug_075 (A2.3 MintProfile): a materialization mint must NOT inherit
/// the builder pod's controller-authoritative binding. The dep-racing
/// shape — a builder pod bound for the same derivation while the
/// materialization job is claimed — pre-fix stamped the builder's
/// `source_node` onto the STORE execution row (feeding wrong exclusion
/// keys) and, with migration 084's build-only CHECK, turned every such
/// claim into a mint error. The mat profile pins `source_node = None`
/// and anchors the deadline to `materialization.attempt_deadline_secs`,
/// never the build solve.
#[tokio::test]
async fn materialization_mint_carries_no_builder_binding() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("mat075-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("mat075");
    n.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    // The dep-racing ingredient: a builder pod binding for the SAME
    // derivation (controller pod informer → authoritative_binding).
    bind_intent_node(&handle, "mat075", "builder-node-a").await?;

    let outcome = claim_materialization(&handle, "mat075", "store-replica-0").await;
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!(
            "materialization claim must mint cleanly with a builder binding present \
             (pre-fix: the binding leaked into the mint and the 084 CHECK rejected \
             the row), got {other:?}"
        ),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let (source_node, deadline): (Option<String>, Option<f64>) =
        sqlx::query_as("SELECT source_node, deadline_secs FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        source_node, None,
        "node attribution is a build-lane concept; the mat profile pins None"
    );
    assert_eq!(
        deadline,
        Some(3600.0),
        "the mat deadline anchors to materialization.attempt_deadline_secs \
         (default 3600), never the build solve"
    );
    Ok(())
}

// r[verify sched.sla.hw-class.ice-mask]
/// bug_091 (A2.3 MintProfile): a materialization claim is NOT the
/// build-spawn success edge — it must neither clear an ICE mask nor
/// consume the `dispatched_cells` arming for its derivation. Pre-fix
/// the un-gated clear at the mint fired for both work classes, so a
/// store replica claiming a job silently un-ICEd a cell whose builder
/// pod had never scheduled.
#[tokio::test]
async fn materialization_mint_leaves_ice_mask_untouched() -> TestResult {
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};
    // bug_119: the membership gate admits only configured classes —
    // the test's h-mat cell needs a config home (same mock-store
    // wiring as setup_with_mock_store, with the class added).
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.sla
                .hw_classes
                .insert("h-mat".into(), minimal_hw_class("h-mat"));
        });
    let (_db, _tasks) = (db, (actor_task, store_task));

    let out = test_store_path("mat091-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("mat091");
    n.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Arm a SINGLE cell for the derivation and mark it ICE — the
    // |A'| == 1 shape the build-pull clear targets.
    handle
        .send_unchecked(ActorCommand::AckSpawnedIntents {
            rejected: vec![],
            reply: tokio::sync::oneshot::channel().0,
            spawned: vec![SpawnIntent {
                intent_id: "mat091".into(),
                hw_class_names: vec!["h-mat".into()],
                node_affinity: vec![NodeSelectorTerm {
                    match_expressions: vec![
                        NodeSelectorRequirement {
                            key: "rio.build/hw-class".into(),
                            operator: "In".into(),
                            values: vec!["h-mat".into()],
                        },
                        NodeSelectorRequirement {
                            key: "karpenter.sh/capacity-type".into(),
                            operator: "In".into(),
                            values: vec!["spot".into()],
                        },
                    ],
                }],
                ..Default::default()
            }],
            unfulfillable_cells: vec!["h-mat:spot".into()],
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![],
            binding_snapshot: None,
        })
        .await?;
    barrier(&handle).await;
    let masked = |snap: &crate::actor::SpawnIntentsSnapshot| {
        snap.ice_masked_cells.iter().any(|c| c == "h-mat:spot")
    };
    let snap = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                req: crate::actor::SpawnIntentsRequest::default(),
                reply,
            })
        })
        .await
        .expect("actor alive");
    assert!(
        masked(&snap),
        "precondition: h-mat:spot is ICE before the claim"
    );

    let outcome = claim_materialization(&handle, "mat091", "store-replica-0").await;
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "claim must deliver, got {outcome:?}"
    );
    let snap = handle
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                req: crate::actor::SpawnIntentsRequest::default(),
                reply,
            })
        })
        .await
        .expect("actor alive");
    assert!(
        masked(&snap),
        "a materialization claim is not the build-spawn success edge: \
         the ICE mask must survive the mint"
    );
    Ok(())
}

// r[verify sched.admin.snapshot-substituting+4]
/// bug_217 (A2.4 typed split): the snapshot's EXECUTOR view counts
/// builder pods only — a materialization-claimed node is store-side
/// work holding no builder slot. Pre-fix `total/active_executors`
/// counted M+N (every Assigned|Running node), so substitution waves
/// inflated the busy-fleet view (and every consumer sized off it) by
/// the store-claim count. `running_derivations` deliberately keeps
/// counting both work classes (the derivation IS running; documented
/// at the claim-intake).
#[tokio::test]
async fn cluster_snapshot_executors_exclude_materialization_claims() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // One materialization claim (store-side).
    let out = test_store_path("mat217-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut m = make_node("mat217");
    m.expected_output_paths = vec![out.clone()];
    let _ev1 = merge_dag(&handle, Uuid::new_v4(), vec![m], vec![], false).await?;
    barrier(&handle).await;
    let claim = claim_materialization(&handle, "mat217", "store-test-0").await;
    assert!(
        matches!(claim, Ok(PullOutcome::Deliver(_))),
        "claim: {claim:?}"
    );

    // One build attempt (builder-side).
    let _ev2 = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "build217",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = pull_attempt(&handle, "build217").await;

    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.running_derivations, 2,
        "both work classes run (the running bucket is kind-blind by design)"
    );
    assert_eq!(
        (snap.total_executors, snap.active_executors),
        (1, 1),
        "the executor view counts the builder pod only — a store claim \
         holds no builder slot (pre-fix: M+N)"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_318 (A2.5 kinded release edge): requeueing a DEP-RACING
/// (Queued-origin) materialization claim must return the node to its
/// DEP-DERIVED status — Queued while its deps are unbuilt — never
/// force Ready. Pre-fix every requeue path went through the build
/// reset (`reset_to_ready`), so an infra-failed store claim on a
/// dep-blocked node surfaced as Ready: from-source dispatchable
/// against inputs that do not exist (InfrastructureFailure → wasted
/// retries → wrong-reason Poisoned, the same chain the recovery
/// cascade gate closes).
#[tokio::test]
async fn requeued_dep_racing_claim_returns_to_queued() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // root → mid → leaf; only MID substitutable (the dep-racing shape:
    // mid sits Queued behind the unproduced leaf, its job claimable).
    let mid_out = test_store_path("rq318-mid-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(mid_out.clone());
    let root = make_node("rq318-root");
    let mut mid = make_node("rq318-mid");
    mid.expected_output_paths = vec![mid_out.clone()];
    let leaf = make_node("rq318-leaf");
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![root, mid, leaf],
        vec![
            make_test_edge("rq318-root", "rq318-mid"),
            make_test_edge("rq318-mid", "rq318-leaf"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "rq318-mid").await.status,
        DerivationStatus::Queued,
        "precondition: mid is dep-blocked"
    );

    let assignment = match claim_materialization(&handle, "rq318-mid", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("dep-racing claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // Infra failure under budget → consumption re-arms the job and
    // requeues the node.
    report_materialization_outcome(
        &handle,
        exec_id,
        "rq318-mid",
        mat_infra_outcome("upstream wedged"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "rq318-mid").await.status,
        DerivationStatus::Queued,
        "the requeued dep-racing claim returns to its dep-derived status \
         (deps unbuilt => Queued; pre-fix: forced Ready, from-source \
         dispatchable against missing inputs)"
    );
    Ok(())
}

// ── A3 [A]: charge→verdict fusion (bug_067 + merged_bug_020) ───────────

/// bug_067 (the Q5-signed reversal of counter-signed residual (a)):
/// establishment-written charges run the SAME park decision as worker
/// charges — party-blind parking via the kernel verdict. A job whose
/// claiming replica dies before reporting, `max_attempts` times in a
/// row, PARKS (durable `park_until`; excluded from the listing; claim
/// refused) instead of re-listing forever as an armed-cycle crash-loop
/// invisible to the MD-D1 stalled population.
// r[verify sched.materialize.routing+7]
#[tokio::test]
async fn establishment_only_charges_park_at_max_attempts() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 2;
            // Long base so the park cannot expire mid-assertion.
            cfg.materialization.park_backoff_base_secs = 600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("est-park-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("est-park");
    n.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;
    // merged_bug_301: pin the pruned origin so the eventual park
    // survives the same ticks' re-evaluation (a NON-pruned childless
    // leaf converts now); this test's subject is the party-blind
    // establishment budget, not the conversion.
    set_origin_pruned(&handle, &db.pool, "est-park").await?;

    // max_attempts establishment cycles: claim → the replica dies
    // unreported (assignment aged out) → the sweep establishes the
    // scheduler-party charge.
    for cycle in 0..2 {
        let claimed = claim_materialization(&handle, "est-park", "store-replica-0").await;
        let assignment = match claimed {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("cycle {cycle}: claim must deliver, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        sqlx::query(
            "UPDATE assignments SET assigned_at = now() - interval '100 days' \
              WHERE exec_id = $1",
        )
        .bind(exec_id)
        .execute(&db.pool)
        .await?;
        tick(&handle).await?;
        barrier(&handle).await;
    }

    let (charges, park_until): (i64, Option<f64>) = sqlx::query_as(
        "SELECT (SELECT count(*) FROM drv_attempts a \
                  WHERE a.derivation_id = mj.derivation_id \
                    AND a.outcome_class = 'materialization_infra'), \
                EXTRACT(EPOCH FROM mj.park_until)::float8 \
           FROM materialization_jobs mj WHERE mj.drv_hash = 'est-park'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(charges, 2, "two establishment charges accrued");
    assert!(
        park_until.is_some(),
        "establishment charges park at max_attempts (party-blind budget, \
         residual-(a) reversal): an establishment-only crash-loop must not \
         re-list forever"
    );
    let listed = list_materialization_jobs(&handle, 16).await;
    assert!(
        !listed.iter().any(|j| j.drv_hash == "est-park"),
        "a parked job is excluded from the claimable listing, got {listed:?}"
    );
    let refused = claim_materialization(&handle, "est-park", "store-replica-1").await;
    assert!(
        matches!(refused, Ok(PullOutcome::NotYetReady { .. })),
        "a parked job's claim is refused while the backoff runs, got {refused:?}"
    );
    Ok(())
}

/// merged_bug_020: the budget window is PER JOB. Migration 085 writes a
/// materialization-lane reset row at job creation, so a successor job's
/// budget starts fresh instead of inheriting the resolved predecessor's
/// charges through the flat drv-level history count.
#[tokio::test]
async fn second_job_budget_window_starts_fresh() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 2;
            cfg.materialization.park_backoff_base_secs = 600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("fresh-window-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("fresh-window");
    n.expected_output_paths = vec![out.clone()];
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Job 1: one worker infra charge (max-1), then resolution via the
    // zero-interest cancellation closer.
    let assignment = match claim_materialization(&handle, "fresh-window", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("job-1 claim must deliver, got {other:?}"),
    };
    let exec1: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec1,
        "fresh-window",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("job-1 infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    cancel_build(&handle, b1).await?;
    barrier(&handle).await;
    tick(&handle).await?; // the cancellation-closer backstop
    barrier(&handle).await;
    let job1_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs WHERE drv_hash = 'fresh-window' \
          ORDER BY job_id LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(job1_state, "cancelled", "job 1 resolved by the closer");

    // Job 2: a new build re-merges the node; the probe partition
    // creates a fresh job (job 1 is terminal → the dedup finds nothing).
    let b2 = Uuid::new_v4();
    let mut n2 = make_node("fresh-window");
    n2.expected_output_paths = vec![out.clone()];
    let _ev2 = merge_dag(&handle, b2, vec![n2], vec![], false).await?;
    barrier(&handle).await;
    tick(&handle).await?;
    barrier(&handle).await;

    // Every genuinely created job writes ONE mat-lane reset row (the
    // 085 window): job 1's creation + job 2's creation = two. Job 2's
    // is the LAST ledger event, so its window holds zero charges.
    let resets: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drv_attempts WHERE outcome_class = 'materialization_reset'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        resets, 2,
        "one creation reset per created job (job 1 + job 2)"
    );

    // Job 2's first infra charge: ONE row in the fresh window (< max 2)
    // — the job re-arms claimable. Pre-085 the flat count spans job 1's
    // charge (2 >= 2) and wrongly parks.
    let assignment = match claim_materialization(&handle, "fresh-window", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("job-2 claim must deliver, got {other:?}"),
    };
    let exec2: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec2,
        "fresh-window",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("job-2 infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let park2: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM park_until)::float8 FROM materialization_jobs \
          WHERE drv_hash = 'fresh-window' AND state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        park2.is_none(),
        "job 2's first charge stays under ITS OWN budget window (one row \
         since the 085 creation reset) — the predecessor's charges must \
         not park the successor"
    );
    let reclaimed = claim_materialization(&handle, "fresh-window", "store-test-1").await;
    assert!(
        matches!(reclaimed, Ok(PullOutcome::Deliver(_))),
        "job 2 re-arms claimable after an under-budget charge, got {reclaimed:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// A3 commit 3 [B,C]: atomic release_claim + the carrier chokepoints
// (merged_bug_015 / merged_bug_307 a+b / merged_bug_055 / merged_bug_257).
// ---------------------------------------------------------------------------

// r[verify sched.materialize.routing+7]
/// merged_bug_015 / merged_bug_307(a)(b): an uncovered arm-0 ReArm
/// consumption must RELEASE the claim — view re-armed AND the node
/// requeued off the mint's Assigned/Running bookkeeping — so a SECOND
/// replica's claim is deliverable. Pre-fix the ReArm arm only cleared
/// `claimed_by`: the node stayed Running under closed-attempt
/// bookkeeping, the admission table answered NotYetReady to EVERY
/// identity, and the pending-unclaimed job wedged with no armed action.
#[tokio::test]
async fn unobtainable_uncovered_rearm_releases_claim_for_second_replica() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out1 = test_store_path("rearm-out1");
    {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out1.clone());
    }
    let mut n = make_node("mat-rearm");
    n.expected_output_paths = vec![out1.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    let assignment = match claim_materialization(&handle, "mat-rearm", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("first claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    // Arm 0, uncovered: nothing missing-and-live-wanted, but the live
    // wanted set (out1) is NOT covered by the verified set → ReArm.
    report_materialization_outcome(
        &handle,
        exec_id,
        "mat-rearm",
        mat_unobtainable_outcome(vec![], vec![], "transient upstream noise"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // The re-armed job must be deliverable to ANOTHER replica: a claim
    // drop without the requeue is the documented wedge (NotYetReady
    // forever — pending-unclaimed job, node held Running).
    match claim_materialization(&handle, "mat-rearm", "store-test-1").await {
        Ok(PullOutcome::Deliver(_)) => {}
        other => panic!(
            "the re-armed job must be deliverable to a second replica \
             (release_claim = re-arm + requeue in ONE step); got {other:?}"
        ),
    }
    Ok(())
}

// r[verify sched.materialize.routing+7]
// r[verify sched.merge.stale-substitutable+3]
/// merged_bug_055: the CompleteForLiveInterest arm must stamp the
/// carried realized paths through the SAME completion chokepoint the
/// Success arm uses. Floating-CA stale-reset shape: the node's
/// expected paths are placeholder-empty, so the live wanted set is
/// the carried realized path (unioned into the wanted set, the 194
/// closure); an Unobtainable report whose missing paths are all moot
/// and whose verified set covers the carrier
/// completes-for-live-interest. Pre-fix that arm skipped the carrier
/// stamp: the node re-completed with `[""]` (GC retention dropped and
/// the placeholder emitted to clients).
#[tokio::test]
async fn complete_for_live_interest_stamps_carried_paths() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let realized = test_store_path("cfli-realized");
    {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(realized.clone());
    }
    // Floating-CA: the WANTED output's expected path is placeholder-
    // empty. A second, UNWANTED declared output ("doc") carries a real
    // path — the moot-failure shape after the A4 partition: a missing
    // path must be ATTRIBUTABLE (expected ∪ carried ∪ live) to count
    // as a wanted-class miss; an unattributable one is treated as a
    // closure-reference miss and never completes (merged_bug_193).
    let moot = test_store_path("cfli-moot-doc");
    let mut n = make_node("cfli-a");
    n.output_names = vec!["out".into(), "doc".into()];
    n.expected_output_paths = vec![String::new(), moot.clone()];
    n.wanted_output_names = vec!["out".into()];
    merge_dag(&handle, Uuid::new_v4(), vec![n.clone()], vec![], false).await?;
    handle
        .debug_force_status("cfli-a", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("cfli-a", vec![realized.clone()])
        .await?;
    barrier(&handle).await;

    // Re-merge: the realized output is gone from the store and
    // substitutable → stale-completed reset + a carried stale_reset job.
    merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;
    let carried: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT carried_realized_paths FROM materialization_jobs WHERE state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        carried.as_deref(),
        Some(std::slice::from_ref(&realized)),
        "the stale_reset job carries the realized path (migration 082)"
    );

    let assignment = match claim_materialization(&handle, "cfli-a", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    // Missing paths are all moot (nothing live-wanted: the floating-CA
    // wanted set resolves empty) → arm 0 covered → CompleteForLiveInterest.
    report_materialization_outcome(
        &handle,
        exec_id,
        "cfli-a",
        mat_unobtainable_outcome(
            vec![moot.clone()],
            vec![realized.clone()],
            "upstream 404 on an unwanted declared output; the carried path verified present",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let info = expect_drv(&handle, "cfli-a").await;
    assert_eq!(info.status, DerivationStatus::Completed);
    assert_eq!(
        info.output_paths,
        vec![realized],
        "the completion chokepoint stamps the carried realized path — \
         never the [\"\"] placeholder"
    );
    Ok(())
}

// r[verify sched.merge.stale-substitutable+3]
/// merged_bug_257: a floating-CA chain reset (A depends on B, both
/// stale-reset in one pass) must give the !deps_ok PARENT a carried
/// stale_reset job too — the carrier is captured at the moment the
/// reset destroys the realized paths, and the job is claimable while
/// deps settle (PD-6: Queued claims are legal). Pre-fix the !deps_ok
/// arm dropped the carrier on the floor: A lost its realized paths
/// and later re-dispatched from source.
#[tokio::test]
async fn chain_reset_parent_gets_carrier_job() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let ra = test_store_path("chain-a-realized");
    let rb = test_store_path("chain-b-realized");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(ra.clone());
        subs.push(rb.clone());
    }
    // Floating-CA chain: A depends on B; both expected placeholder-empty.
    let mk = |tag: &str| {
        let mut n = make_node(tag);
        n.expected_output_paths = vec![String::new()];
        n
    };
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![mk("chain-a"), mk("chain-b")],
        vec![make_test_edge("chain-a", "chain-b")],
        false,
    )
    .await?;
    for (tag, path) in [("chain-a", &ra), ("chain-b", &rb)] {
        handle
            .debug_force_status(tag, DerivationStatus::Completed)
            .await?;
        handle
            .debug_set_output_paths(tag, vec![path.clone()])
            .await?;
    }
    barrier(&handle).await;

    // Re-merge: both outputs gone-and-substitutable → both reset. B
    // (leaf) goes Ready; A sees its child in the reset set → Queued
    // (!deps_ok).
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![mk("chain-a"), mk("chain-b")],
        vec![make_test_edge("chain-a", "chain-b")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "chain-a").await.status,
        DerivationStatus::Queued,
        "A's reset lands Queued (its dep B reset in the same pass)"
    );

    let a_carried: Option<Option<Vec<String>>> = sqlx::query_scalar(
        "SELECT carried_realized_paths FROM materialization_jobs \
          WHERE drv_hash = 'chain-a' AND state = 'pending'",
    )
    .fetch_optional(&db.pool)
    .await?;
    let a_carried = a_carried.unwrap_or_else(|| {
        panic!(
            "the !deps_ok parent must get a stale_reset job too — the \
             carrier is captured at destruction, not dropped with the \
             Queued continue"
        )
    });
    assert_eq!(
        a_carried.as_deref(),
        Some(std::slice::from_ref(&ra)),
        "A's job carries A's realized path"
    );

    // PD-6: the Queued-origin job is claimable while deps settle.
    match claim_materialization(&handle, "chain-a", "store-test-0").await {
        Ok(PullOutcome::Deliver(_)) => {}
        other => panic!("the Queued carrier job must be claimable (PD-6), got {other:?}"),
    }
    Ok(())
}

// r[verify sched.merge.stale-substitutable+3]
/// merged_bug_257(b): a fenced/failed stale-reset job creation must
/// not drop the carrier — it survives in the leader-scoped stash and
/// the housekeeping tick retries until the row applies. Driven by an
/// external PG fault (the table hidden for one merge — the post-tx
/// standalone create errors), then healed and ticked.
#[tokio::test]
async fn failed_fenced_job_create_retries_via_stash() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    let realized = test_store_path("stash-realized");
    {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(realized.clone());
    }
    let mut n = make_node("stash-a");
    n.expected_output_paths = vec![String::new()];
    merge_dag(&handle, Uuid::new_v4(), vec![n.clone()], vec![], false).await?;
    handle
        .debug_force_status("stash-a", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("stash-a", vec![realized.clone()])
        .await?;
    barrier(&handle).await;

    // External fault injection: hide the table so the standalone
    // fenced create ERRORS (the merge transaction itself committed
    // long before this post-tx site runs — design §2.1 row 4's
    // standalone-helper posture is exactly what makes the fault
    // injectable here).
    sqlx::query("ALTER TABLE materialization_jobs RENAME TO materialization_jobs_hidden")
        .execute(&db.pool)
        .await?;

    // The re-merge resets the node and tries to create the carried
    // job — the create FAILS; the carrier must land in the stash,
    // not on the floor.
    merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    // The table heals and the housekeeping tick retries the stash.
    sqlx::query("ALTER TABLE materialization_jobs_hidden RENAME TO materialization_jobs")
        .execute(&db.pool)
        .await?;
    let jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE drv_hash = 'stash-a'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(jobs, 0, "the failed create wrote no job row");
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    let carried: Option<Vec<String>> = sqlx::query_scalar(
        "SELECT carried_realized_paths FROM materialization_jobs \
          WHERE drv_hash = 'stash-a' AND state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        carried.as_deref(),
        Some(std::slice::from_ref(&realized)),
        "the stash retry created the job WITH the carrier intact"
    );
    Ok(())
}

/// merged_bug_246 (required load): a term whose job-view load fails
/// must FAIL recovery and serve fail-closed — a materialization claim
/// answers NotYetReady (the Unavailable projection), never `Gone`.
///
/// RED (pre-fix): the load failure was warn-and-continue — recovery
/// returned Ok with an EMPTY-but-trusted view, the claim projected
/// `JobView::None`, and the kernel answered `Gone`: the store treats
/// Gone as "resolved, skip" and never claims again (the stranded
/// armed action). The next LeaderAcquired heals (level-triggered).
#[tokio::test]
#[tracing_test::traced_test]
async fn recovery_fails_when_job_view_load_fails() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // Phase 1: a healthy leader creates one PENDING job, then dies.
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |_, _| {});
        let out = test_store_path("jvl-fail-out");
        store.state.substitutable.write().unwrap().push(out.clone());
        let mut n = make_node("jvl-fail");
        n.expected_output_paths = vec![out];
        let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
        barrier(&handle).await;
        let jobs: i64 = sqlx::query_scalar(
            "SELECT count(*) FROM materialization_jobs WHERE drv_hash = 'jvl-fail' \
              AND state = 'pending'",
        )
        .fetch_one(&db.pool)
        .await?;
        assert_eq!(jobs, 1, "precondition: the pending job exists durably");
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Phase 2: the successor's job-view load fails → degraded term.
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_, p| {
            p.leader = phase2_leader;
            p.fail_next_job_view_load = true;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    let degraded_claim = claim_materialization(&handle, "jvl-fail", "store-test-0").await;
    assert!(
        matches!(degraded_claim, Ok(PullOutcome::NotYetReady { .. })),
        "a degraded term answers NotYetReady (fail-closed Unavailable projection), \
         never Gone (the stranded-skip) and never Deliver (a job we cannot see); \
         got {degraded_claim:?}"
    );

    // The next acquisition heals: recovery succeeds, the view hydrates,
    // the pending job delivers.
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    let healed_claim = claim_materialization(&handle, "jvl-fail", "store-test-0").await;
    assert!(
        matches!(healed_claim, Ok(PullOutcome::Deliver(_))),
        "the next successful recovery hydrates the view and the job delivers; \
         got {healed_claim:?}"
    );
    Ok(())
}

/// merged_bug_246 (re-feed) + bug_385 (scheduler leg): the PG-backed
/// backstop sweep re-feeds pending rows the view does not track —
/// rehydrating their TRUE state (a parked row claims NotYetReady, never
/// the fabricated-unparked Deliver) — and moot rows (no live
/// derivation) converge to cancelled instead of producing refusals
/// forever.
///
/// RED (pre-fix): no sweep existed — an untracked row answered `Gone`
/// to every claim (view-absence projected `JobView::None`), and the
/// dedup re-feed inserted a DEFAULT unparked/unclaimed entry,
/// delivering parked jobs early.
#[tokio::test]
#[tracing_test::traced_test]
async fn backstop_refeeds_untracked_rows_and_cancels_moot() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // A live node, merged WITHOUT a job (not substitutable at merge).
    let out = test_store_path("bk-live-out");
    let mut live = make_node("bk-live");
    live.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![live], vec![], false).await?;
    barrier(&handle).await;
    // Make the path substitutable so a re-fed claim can deliver later
    // if the park were (wrongly) ignored.
    store.state.substitutable.write().unwrap().push(out);

    // Simulate a deposed leader's late commit: a PARKED pending job for
    // the live node, written directly to PG — the view never saw it.
    let live_id: Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = 'bk-live'")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO materialization_jobs \
             (job_id, derivation_id, drv_hash, origin, state, created_generation, \
              park_until, park_began_at) \
         VALUES ($1, $2, 'bk-live', 'cache_opportunity', 'pending', 1, \
                 now() + interval '600 seconds', now())",
    )
    .bind(Uuid::now_v7())
    .bind(live_id)
    .execute(&db.pool)
    .await?;

    // And a moot row: a derivation no live build references.
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ('bk-moot', $1, 'x86_64-linux', 'queued')",
    )
    .bind(rio_test_support::fixtures::test_drv_path("bk-moot"))
    .execute(&db.pool)
    .await?;
    let moot_id: Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = 'bk-moot'")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO materialization_jobs \
             (job_id, derivation_id, drv_hash, origin, state, created_generation) \
         VALUES ($1, $2, 'bk-moot', 'cache_opportunity', 'pending', 1)",
    )
    .bind(Uuid::now_v7())
    .bind(moot_id)
    .execute(&db.pool)
    .await?;

    // Pre-sweep: the untracked parked row answers Gone (the hole this
    // sweep exists to close — asserted here as the documented
    // pre-refeed shape, NOT the desired end state).
    let pre = claim_materialization(&handle, "bk-live", "store-test-0").await;
    assert!(
        matches!(pre, Ok(PullOutcome::Gone(_))),
        "precondition (the documented hole): an untracked row projects None → Gone; \
         got {pre:?}"
    );

    // merged_bug_301: pin the pruned origin so the park survives 30
    // re-evaluation ticks (a NON-pruned childless leaf converts now);
    // the subject here is the backstop re-feed of an untracked row.
    sqlx::query("UPDATE materialization_jobs SET origin = 'pruned' WHERE drv_hash = 'bk-live'")
        .execute(&db.pool)
        .await?;
    // Drive past the backstop cadence (every 30th tick).
    for _ in 0..30 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }
    barrier(&handle).await;

    // The parked row was re-fed with its TRUE state: NotYetReady (the
    // park honored), not Deliver (fabricated-unparked), not Gone
    // (untracked).
    let refed = claim_materialization(&handle, "bk-live", "store-test-0").await;
    assert!(
        matches!(refed, Ok(PullOutcome::NotYetReady { .. })),
        "the re-fed parked job answers NotYetReady (park rehydrated from PG, \
         never fabricated unparked); got {refed:?}"
    );

    // The moot row converged: cancelled charge-free by the
    // zero-interest pass over the re-fed entry.
    let moot_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'bk-moot'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        moot_state, "cancelled",
        "a pending job with no live derivation converges to cancelled (385's \
         refusal-producing rows converge)"
    );
    let moot_attempts: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM drv_attempts WHERE derivation_id = $1 \
          AND event_kind = 'attempt'",
    )
    .bind(moot_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(moot_attempts, 0, "the moot cancel is charge-free");
    Ok(())
}

/// bug_252 (the predicate split, extending
/// `flag_on_pending_jobs_count_as_substituting_bucket` with the
/// parked-excluded leg): a PARKED job's node counts in NEITHER the
/// substituting bucket (KEDA must drain — the store cannot progress a
/// parked job) NOR the queued/builder buckets (the node will be
/// materialized, not built; spawn exclusion still holds).
///
/// RED (pre-fix): the parked node stayed in substituting_derivations —
/// KEDA held store replicas up against a backlog no replica could
/// claim.
#[tokio::test]
#[tracing_test::traced_test]
async fn flag_on_parked_jobs_leave_substituting_bucket() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
        // One infra failure parks (long backoff — outlives the test).
        cfg.materialization.max_attempts = 1;
        cfg.materialization.park_backoff_base_secs = 600;
    });

    let out = test_store_path("parked-bucket-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("parked-bucket");
    n.expected_output_paths = vec![out];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations, 1,
        "precondition: the claimable pending job counts as backlog"
    );

    // Park it: claim + infra report at max_attempts=1.
    let assignment = match claim_materialization(&handle, "parked-bucket", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the pending job must deliver, got {other:?}"),
    };
    let exec: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec,
        "parked-bucket",
        mat_infra_outcome("upstream down"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let park: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM (park_until - now()))::float8 \
           FROM materialization_jobs WHERE drv_hash = 'parked-bucket'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        park.is_some_and(|s| s > 0.0),
        "precondition: the job parked"
    );
    // merged_bug_301: keep the job in the PARKED population across the
    // tick (a non-pruned childless leaf would convert at the
    // re-evaluation now) — this test pins the gauge semantics of a
    // parked job, not the conversion.
    set_origin_pruned(&handle, &db.pool, "parked-bucket").await?;

    tick(&handle).await?;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations, 0,
        "a parked job leaves the substituting bucket (KEDA drains; bug_252)"
    );
    assert_eq!(
        snap.queued_derivations, 0,
        "the parked node does NOT fall into the builder bucket (it will be \
         materialized once the backoff lapses — spawn exclusion holds)"
    );
    assert_eq!(
        snap.queued_by_system.values().sum::<u32>(),
        0,
        "queued_by_system mirrors the queued bucket"
    );
    Ok(())
}

/// merged_bug_020 (020b, the one-shot half): the Unobtainable re-probe
/// one-shot is PER JOB — a successor job's first Unobtainable re-arms
/// (the one-shot is fresh inside its 085 window) instead of resolving
/// from source through the predecessor's consumed one-shot.
///
/// RED (pre-085): the flat drv-level history counted job 1's
/// Unobtainable, so job 2's FIRST Unobtainable read as "re-probe
/// already spent" and went straight to from-source.
#[tokio::test]
async fn one_shot_fresh_for_second_job() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 3;
            cfg.materialization.park_backoff_base_secs = 600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("oneshot-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("oneshot");
    n.expected_output_paths = vec![out.clone()];
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(&handle, b1, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Job 1: one Unobtainable (spends ITS one-shot → ReArm), then
    // resolution via build-cancel + the zero-interest closer.
    let a1 = match claim_materialization(&handle, "oneshot", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("job-1 claim must deliver, got {other:?}"),
    };
    let exec1: Uuid = a1.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec1,
        "oneshot",
        mat_unobtainable_outcome(vec![out.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("job-1 unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;
    cancel_build(&handle, b1).await?;
    barrier(&handle).await;
    tick(&handle).await?;
    barrier(&handle).await;

    // Job 2: a fresh build re-creates the job.
    let b2 = Uuid::new_v4();
    let mut n2 = make_node("oneshot");
    n2.expected_output_paths = vec![out.clone()];
    let _ev2 = merge_dag(&handle, b2, vec![n2], vec![], false).await?;
    barrier(&handle).await;
    tick(&handle).await?;
    barrier(&handle).await;

    // Job 2's FIRST Unobtainable: the one-shot is fresh → ReArm (the
    // job stays pending, claimable again), NOT resolved_from_source.
    let a2 = match claim_materialization(&handle, "oneshot", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("job-2 claim must deliver, got {other:?}"),
    };
    let exec2: Uuid = a2.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec2,
        "oneshot",
        mat_unobtainable_outcome(vec![out.clone()], vec![], "upstream 404"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("job-2 unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let job2_state: String = sqlx::query_scalar(
        "SELECT state FROM materialization_jobs WHERE drv_hash = 'oneshot' \
          AND state != 'cancelled' ORDER BY job_id DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        job2_state, "pending",
        "job 2's first Unobtainable re-arms (its own one-shot, fresh in its \
         085 window) — RED pre-085: resolved_from_source through job 1's \
         consumed one-shot"
    );
    let reclaim = claim_materialization(&handle, "oneshot", "store-test-1").await;
    assert!(
        matches!(reclaim, Ok(PullOutcome::Deliver(_))),
        "the re-armed job is claimable again, got {reclaim:?}"
    );
    Ok(())
}

/// merged_bug_020 (020c, the failover twin): the per-job window
/// computed from the LOADED suffix (post-failover) yields the same
/// verdict as the in-memory fold — one pre-failover charge still
/// counts (claim delivers at 1 < max 2; the next charge parks at 2).
#[tokio::test]
async fn failover_preserves_job_budget_window() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // Phase 1: one charge against max_attempts=2, then failover.
    {
        let (handle, task) =
            setup_actor_configured(db.pool.clone(), Some(store_client.clone()), |cfg, _| {
                cfg.materialization.max_attempts = 2;
                cfg.materialization.park_backoff_base_secs = 600;
            });
        let out = test_store_path("fo-window-out");
        store.state.substitutable.write().unwrap().push(out.clone());
        let mut n = make_node("fo-window");
        n.expected_output_paths = vec![out];
        let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
        barrier(&handle).await;
        let a = match claim_materialization(&handle, "fo-window", "store-test-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("phase-1 claim must deliver, got {other:?}"),
        };
        let exec: Uuid = a.exec_id.parse()?;
        report_materialization_outcome(
            &handle,
            exec,
            "fo-window",
            mat_infra_outcome("upstream 503"),
        )
        .await
        .map_err(|e| anyhow::anyhow!("phase-1 infra report rejected: {e:?}"))?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Phase 2: the successor recovers; the loaded suffix carries the
    // one charge.
    let leader = crate::lease::LeaderState::always_leader(std::sync::Arc::new(
        std::sync::atomic::AtomicU64::new(1),
    ));
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let phase2_leader = leader.clone();
    let (handle, _task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |cfg, p| {
            cfg.materialization.max_attempts = 2;
            cfg.materialization.park_backoff_base_secs = 600;
            p.leader = phase2_leader;
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // 1 < 2: still claimable on the recovered leader (the loaded
    // window matches the in-memory fold's verdict).
    let a = match claim_materialization(&handle, "fo-window", "store-test-1").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("post-failover claim must deliver at 1 < max 2, got {other:?}"),
    };
    let exec: Uuid = a.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec,
        "fo-window",
        mat_infra_outcome("upstream 503"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("phase-2 infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // 2 >= 2: the second charge parks — the identical verdict the
    // no-failover history produces.
    let park: Option<f64> = sqlx::query_scalar(
        "SELECT EXTRACT(EPOCH FROM park_until)::float8 FROM materialization_jobs \
          WHERE drv_hash = 'fo-window' AND state = 'pending'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        park.is_some_and(|s| s > 0.0),
        "the post-failover second charge parks at the budget — the loaded \
         suffix and the live fold agree (020c)"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_301 (bughunt wave, A4): a parked CHILDLESS LEAF with a
/// non-pruned origin must CONVERT at the park re-evaluation — a
/// structural leaf has no closure to be missing, so from-source is
/// viable; only the pruned origin (the prune deliberately dropped a
/// closure) and the holed cell (stale produced evidence) keep a job
/// parked. Pre-fix the classifier conflated leaf and hole into one
/// `Broken` cell and the gate skipped both.
#[tokio::test]
async fn parked_childless_leaf_non_pruned_converts() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("a4leaf-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("a4leaf");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "a4leaf", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_id,
        "a4leaf",
        mat_infra_outcome("dead upstream"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    let parked: i64 = sqlx::query_scalar(
        "SELECT count(*) FROM materialization_jobs \
          WHERE drv_hash = 'a4leaf' AND state = 'pending' AND park_until > now()",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(parked, 1, "precondition: the job is parked");

    // No PG children at all: a structural leaf. Tick the park
    // re-evaluation — the leaf must convert to from-source.
    tick(&handle).await?;
    barrier(&handle).await;
    let (job_state, origin): (String, String) =
        sqlx::query_as("SELECT state, origin FROM materialization_jobs WHERE drv_hash = 'a4leaf'")
            .fetch_one(&db.pool)
            .await?;
    assert_ne!(
        origin, "pruned",
        "fixture premise: non-pruned origin (got {origin})"
    );
    assert_eq!(
        job_state, "resolved_from_source",
        "a parked childless leaf with a non-pruned origin converts \
         (ChildlessLeaf is from-source-viable; only Pruned/Holed stay parked)"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_301 green guard: a parked childless job whose origin IS
/// `pruned` stays parked — the prune dropped its closure on purpose;
/// converting it would dispatch the doomed from-source build the
/// classification exists to prevent.
#[tokio::test]
async fn parked_pruned_childless_stays_parked() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("a4prn-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("a4prn");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "a4prn", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_id,
        "a4prn",
        mat_infra_outcome("dead upstream"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;
    set_origin_pruned(&handle, &db.pool, "a4prn").await?;

    tick(&handle).await?;
    barrier(&handle).await;
    let (job_state, parked): (String, bool) = sqlx::query_as(
        "SELECT state, park_until > now() FROM materialization_jobs WHERE drv_hash = 'a4prn'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        (job_state.as_str(), parked),
        ("pending", true),
        "a parked PRUNED childless job stays parked (closure deliberately dropped)"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_301 green guard: the HOLED cell (produced children whose
/// only voucher is a terminal build — the stale-evidence shape) stays
/// parked at the re-evaluation, exactly as the dead-voucher
/// classification demands.
#[tokio::test]
async fn parked_holed_stays_parked() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("a4hole-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("a4hole");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "a4hole", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(
        &handle,
        exec_id,
        "a4hole",
        mat_infra_outcome("dead upstream"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Durable: a produced child whose ONLY voucher is a terminal
    // build (the previous-generation shape) — the holed cell.
    let r_id = pg_derivation_id(&db.pool, "a4hole").await?;
    let c_id = insert_pg_derivation(&db.pool, "a4hole-child", "completed").await?;
    pg_edge(&db.pool, r_id, c_id).await?;
    let dead_build = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded')")
        .bind(dead_build)
        .execute(&db.pool)
        .await?;
    pg_link(&db.pool, dead_build, c_id).await?;

    tick(&handle).await?;
    barrier(&handle).await;
    let (job_state, parked): (String, bool) = sqlx::query_as(
        "SELECT state, park_until > now() FROM materialization_jobs WHERE drv_hash = 'a4hole'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        (job_state.as_str(), parked),
        ("pending", true),
        "the holed cell (stale produced evidence) stays parked"
    );
    Ok(())
}

// r[verify sched.materialize.routing+7]
/// merged_bug_194 (bughunt wave, A4): wanted names that match NO
/// declared output resolve to an EMPTY live-wanted path set — coverage
/// over the empty set is vacuously true, and pre-fix the consumption
/// completed the node for live interest having verified NOTHING. The
/// witness type makes the vacuous cell unrepresentable: an empty
/// verifiable set re-arms instead.
#[tokio::test]
async fn bogus_wanted_names_never_complete_vacuously() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // merged_bug_003 (Q3): the dispatch probe that creates this test's
    // cache_opportunity job only runs tenant-scoped under service
    // auth; an anonymous probe gets no substitutable answer (the
    // pre-fix mock's anonymous fiction is gone). Sign like a
    // configured deployment — the vacuity law under test is
    // orthogonal to probe auth.
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(std::sync::Arc::new(HmacSigner::from_key(
                b"mock-store-harness-service-key32".to_vec(),
            )));
        });
    let _tasks = (store_task, actor_task);

    let out = test_store_path("a4bogus-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("a4bogus");
    r.expected_output_paths = vec![out.clone()];
    // The wanted name matches no declared output: the verifiable
    // wanted set is empty.
    r.wanted_output_names = vec!["bogus".into()];
    let b1 = Uuid::new_v4();
    let _ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "a4bogus", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    // The executor confirms the (unwanted) declared output missing and
    // verified nothing: with an empty verifiable wanted set, coverage
    // is vacuous.
    report_materialization_outcome(
        &handle,
        exec_id,
        "a4bogus",
        mat_unobtainable_outcome(vec![out.clone()], vec![], "404 everywhere"),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'a4bogus'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        job_state, "pending",
        "an empty verifiable wanted set must RE-ARM, never complete \
         vacuously (nothing was verified for the live interest)"
    );
    let status = expect_drv(&handle, "a4bogus").await.status;
    assert_ne!(
        status,
        DerivationStatus::Completed,
        "the node must not complete on vacuous coverage"
    );
    Ok(())
}

/// merged_bug_026 (producer half): every materialization delivery —
/// the fresh mint AND the held-attempt re-delivery — echoes the job it
/// was minted under (`WorkAssignment.job_id`, the producer-asserted
/// binding the client keys identity by). The kernel's Pending arm can
/// answer a stale nonce-presenting pull with the SUCCESSOR job's
/// delivery, so the binding must ride the wire, not be reconstructed
/// from the puller's ledger.
#[tokio::test]
async fn delivery_echoes_minted_job_binding() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("mat-jobbind-out");
    let mut n = make_node("mat-jobbind");
    n.expected_output_paths = vec![out.clone()];
    n.wanted_output_names = vec!["out".into()];
    merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;

    let jobs = list_materialization_jobs(&handle, 16).await;
    assert_eq!(jobs.len(), 1, "exactly one job listed, got {jobs:?}");
    let minted_job = jobs[0].job_id;

    // Fresh mint (the mint_and_deliver path).
    let assignment = match claim_materialization(&handle, "mat-jobbind", "store-bind-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the first claim must deliver, got {other:?}"),
    };
    assert_eq!(
        assignment.job_id,
        minted_job.to_string(),
        "the fresh mint echoes the job it was minted under"
    );

    // Held-attempt re-delivery (the DeliverExisting path, resume token).
    let exec_id: Uuid = assignment.exec_id.parse()?;
    let redelivered =
        match resume_materialization(&handle, "mat-jobbind", "store-bind-0", exec_id).await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("the resume re-pull must re-deliver, got {other:?}"),
        };
    assert_eq!(
        redelivered.job_id,
        minted_job.to_string(),
        "the re-delivery echoes the same job binding"
    );
    Ok(())
}

/// bug_184: a FOREIGN executor's open materialization attempt must
/// not mask another executor's claimed-no-attempt ghost. The backed
/// check is pair-keyed (drv, holder) — pre-fix it keyed on drv_hash
/// alone, so executor A's open attempt (the fresh-INSERT-below-floor
/// fence residual shape) both masked executor B's ghost AND cleared
/// its armed strike every sweep, deferring the self-heal a full
/// establishment window plus two sweeps.
// r[verify sched.materialize.claim-coherence]
#[tokio::test]
async fn foreign_open_attempt_does_not_mask_anothers_ghost() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-foreign-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-foreign");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;

    // Executor B claims; its assignment dies out-of-band (the ghost
    // seed, same crash-window shape as the sibling test above).
    let assignment =
        match claim_materialization(&handle, "maton-foreign", "store-replica-b-w0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => panic!("claim must deliver, got {other:?}"),
        };
    let _exec_id: Uuid = assignment.exec_id.parse()?;
    let closed = sqlx::query(
        "UPDATE assignments SET status = 'completed', completed_at = now()
         WHERE status IN ('pending', 'acknowledged')",
    )
    .execute(&db.pool)
    .await?
    .rows_affected();
    assert_eq!(closed, 1, "the seed closes exactly B's open assignment");

    // FOREIGN open materialization attempt by executor A on the SAME
    // drv (raw seed — the fence-residual shape: durable rows the
    // in-memory view never minted).
    let foreign_exec = Uuid::now_v7();
    sqlx::query(
        "INSERT INTO drv_executions \
             (exec_id, drv_hash, executor_id, started_at, deadline_secs, attempt_kind) \
         VALUES ($1, $2, 'store-replica-a-w0', now(), 600, 'materialization')",
    )
    .bind(foreign_exec)
    .bind(format!("{:0>32}", "matonforeign"))
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         SELECT derivation_id, 'store-replica-a-w0', 1, 'pending', $1 \
         FROM derivations WHERE drv_hash = 'maton-foreign'",
    )
    .bind(foreign_exec)
    .execute(&db.pool)
    .await?;

    // Two sweeps: B's claim is unbacked BY B both times (A's foreign
    // attempt must not count), so the strike arms then the repair
    // releases B's claim.
    tick(&handle).await?;
    tick(&handle).await?;
    let redelivered = claim_materialization(&handle, "maton-foreign", "store-replica-c-w0").await;
    assert!(
        matches!(redelivered, Ok(PullOutcome::Deliver(_))),
        "after two unbacked-by-holder sweeps the ghost must be repaired and \
         re-claimable (pre-fix red: the foreign drv-keyed match masked the \
         ghost forever), got {redelivered:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Defer rides the release disposition (bug_220)
// ---------------------------------------------------------------------------

/// bug_220 red: a stale executor's redelivered RetryLater settles
/// through `companion_release` while a DIFFERENT executor holds a
/// fresh claim. Pre-fix the companion stamped `defer_until`
/// unconditionally BEFORE the holder-gated release, so the stale
/// report defaced the fresh holder's job: when the fresh holder later
/// released through a defer-less companion, the job sat invisibly
/// Deferred (refused NotYetReady at admission, filtered from the
/// listing, counted in neither gauge) for up to the clamped window.
#[tokio::test]
async fn stale_retrylater_redelivery_cannot_defer_a_fresh_holders_job() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor(db.pool.clone());
    let drv = DrvHash::from("defer-stale-drv");
    let mut entry = crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4());
    entry.mint_claim(crate::state::ExecutorId::from("fresh-holder"));
    actor.materialization_jobs.insert(drv.clone(), entry);

    // Stale redelivery: settled close + 300s defer from the FORMER
    // holder. The release is a compare-and-clear miss (StaleHolder).
    let _ack = actor
        .companion_release(
            &drv,
            &crate::state::ExecutorId::from("stale-holder"),
            Some(std::time::Duration::from_secs(300)),
            crate::actor::materialize::SettledClose::test_witness(),
        )
        .await;

    // The fresh holder's own clean release (no defer).
    let _ack = actor
        .companion_release(
            &drv,
            &crate::state::ExecutorId::from("fresh-holder"),
            None,
            crate::actor::materialize::SettledClose::test_witness(),
        )
        .await;

    let claimability = actor
        .materialization_jobs
        .get(&drv)
        .expect("entry survives both releases")
        .claimability(std::time::Instant::now());
    assert!(
        matches!(
            claimability,
            crate::actor::materialize::Claimability::ClaimableNow
        ),
        "left: {claimability:?} / right: ClaimableNow (a stale holder's \
         redelivery must not deface the fresh claim's deferral axis)"
    );
    Ok(())
}

/// merged_bug_262: `entry_from_recovered_row`'s parked_until lane piped
/// PG-derived `park_remaining_secs` (EXTRACT(EPOCH ...)::float8 -- and
/// 'infinity'::timestamptz is valid SQL) unclamped into
/// `Duration::from_secs_f64`, whose contract PANICS on +inf; the
/// `> 0.0` filter passes +inf straight through. One such row panicked
/// the recovery job-view rebuild on EVERY leader candidate: a
/// fleet-wide crash loop. The sibling parked_at lane in the same
/// constructor was already clamped (RecoveredInstant::from_age_secs)
/// for exactly this case.
#[test]
fn recovered_row_with_infinite_park_does_not_panic() {
    let row = crate::db::open_attempts::RecoveredJobRow {
        job_id: Uuid::new_v4(),
        drv_hash: "inf-park".into(),
        carried_realized_paths: None,
        park_remaining_secs: Some(f64::INFINITY),
        park_began_secs_ago: Some(f64::INFINITY),
        // sh-044: extends the merged_bug_262 clamp pin to the new
        // created_at lane (RecoveredInstant::from_age_secs is total).
        age_secs: f64::INFINITY,
        origin: "cache_opportunity".into(),
        claimed_by: None,
    };
    // Pre-fix: panics inside Duration::from_secs_f64 (+inf).
    let entry = crate::actor::materialize::JobViewEntry::from_recovered_row_for_test(row);
    // The clamp lands at the one-year ceiling: still parked, never a
    // panic, and a bounded wait rather than an unreachable instant.
    assert!(matches!(
        entry.claimability(std::time::Instant::now()),
        crate::actor::materialize::Claimability::Parked
    ));
}

/// Shared body of the sh-044 unclaimed-age-out grid. Seeds one entry
/// under a 3×60s threshold, varies the three predicate conjuncts
/// (holder, parked_until, created_at backdate) AND the two
/// `from_source_viable` gate inputs (origin, evidence — via
/// `seed_holed_child`), runs phase-15, and returns whether the entry
/// SURVIVED (still in the view + durable row still pending). With
/// `origin=CacheOpportunity` and `seed_holed_child=false` the gate
/// sees `ChildlessLeaf × CacheOpportunity` → viable, so the predicate
/// alone is the variable under test; the gate-refusal cells set one
/// of the two.
async fn run_age_out_predicate(
    hash: &str,
    backdate_secs: u64,
    parked_until: Option<std::time::Instant>,
    holder: Option<crate::state::ExecutorId>,
    origin: JobOrigin,
    seed_holed_child: bool,
) -> anyhow::Result<bool> {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            materialization: crate::config::MaterializationConfig {
                max_attempts: 3,
                // 60s slack convention (recovered_instant.rs:101): the
                // predicate's std::Instant comparison is un-mockable —
                // 1s margins are the ci-failure-patterns.md
                // 'Wall-clock gate under load' class.
                attempt_deadline_secs: 60,
                ..Default::default()
            },
            ..Default::default()
        },
    );
    let drv = insert_test_derivation_local(&db.pool, hash).await?;
    // The age-out arm's `from_source_viable` gate reads
    // `dag.node(..).db_id` → `classify_durable_evidence`. Zero
    // children → `ChildlessLeaf`. `seed_holed_child` adds one
    // 'completed' child WITH NO live co-owning build voucher
    // (`CHILD_PRODUCED_SQL=true ∧ CHILD_LIVE_VOUCHER_SQL=false`) →
    // `Holed`.
    if seed_holed_child {
        let child = insert_pg_derivation(&db.pool, &format!("{hash}-child"), "completed").await?;
        pg_edge(&db.pool, drv, child).await?;
    }
    actor.test_inject_ready(hash, None, "x86_64-linux", false);
    actor.dag.node_mut(hash).expect("just injected").db_id = Some(drv);
    let created = sdb(&db.pool)
        .create_materialization_job_fenced(
            drv,
            hash,
            None,
            origin,
            None,
            0.0,
            actor.serving_generation(),
        )
        .await?;
    let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("job create must apply");
    };
    let mut entry = crate::actor::materialize::JobViewEntry::new_unclaimed(job_id, None, origin);
    // RecoveredInstant::backdated (the DebugBackdate* mechanism) —
    // NOT tokio::time::pause()/advance(): RecoveredInstant.elapsed()
    // reads std::time::Instant which tokio's paused clock cannot
    // advance, AND the harness's real sqlx pool PoolTimedOut's under
    // start_paused.
    entry.test_set_created_at(crate::state::RecoveredInstant::backdated(backdate_secs));
    entry.test_set_parked_until(parked_until);
    if let Some(ex) = holder {
        entry.mint_claim(ex);
    }
    let h = crate::state::DrvHash::from(hash);
    actor.materialization_jobs.insert(h.clone(), entry);

    let authority = actor.dag_authority().expect("always-leader test actor");
    actor.tick_reevaluate_materialization_jobs(&authority).await;

    let in_view = actor.materialization_jobs.get(&h).is_some();
    let in_pg = sdb(&db.pool)
        .unresolved_job_for_derivation(drv)
        .await?
        .is_some();
    assert_eq!(
        in_view, in_pg,
        "view eviction and durable resolve are atomic per remove_settled"
    );
    Ok(in_view)
}

/// Positive-path wrapper: the two `is_none_or(|u| u <= now)` arms
/// (never-parked and park-expired) at 240s > 3×60s threshold, no
/// holder, viable gate inputs.
async fn assert_unclaimed_ages_out(
    hash: &str,
    parked_until: Option<std::time::Instant>,
) -> TestResult {
    let survived = run_age_out_predicate(
        hash,
        240,
        parked_until,
        None,
        JobOrigin::CacheOpportunity,
        false,
    )
    .await?;
    assert!(
        !survived,
        "the age-out arm collects holder()=None && parked_until.\
         is_none_or(|u| u <= now) && created_at.elapsed() > 3s → \
         resolve_materialization_job(ResolvedFromSource) → \
         remove_settled evicts the entry (240s > 3×60s threshold); \
         pre-fix the phase-15 filter was parked_until.is_some_and(|u| \
         u > now) only — never-parked AND park-expired entries excluded"
    );
    Ok(())
}

/// sh-044 (never-parked case): a Pending-unclaimed job with
/// `parked_until=None` and NO open attempt is reached by NEITHER
/// phase-12 (draws from open-attempt rows only) NOR the
/// parked-conversion arm (filters on `parked_until.is_some_and(..)`)
/// at 368e279cf. With the phase-17 candidate filter skipping nodes
/// that carry an unresolved job, this is the residual that strands a
/// Ready node forever — c4's age-out arm closes it.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn pending_unclaimed_job_ages_out_to_from_source() -> TestResult {
    assert_unclaimed_ages_out("ageout-never-parked", None).await
}

/// sh-044 (park-expired case): `parked_until=Some(past)` — the
/// once-parked-then-abandoned state (establishment-only crash-loop →
/// park → executor gone). `parked_until` is written only at
/// `{new_unclaimed, park, entry_from_recovered_row}` and no
/// live-process path resets `Some→None` (the four claim-lifecycle
/// mutators write `episode`/`defer_until` only), so the entry sits at
/// `Some(past)` indefinitely; an `is_none()` age-out predicate would
/// miss it. The `is_none_or(|u| u <= now)` conjunct is the EXACT
/// complement of the parked-conversion arm's `is_some_and(|u| u >
/// now)` — the two arms partition `holder()=None`.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn park_expired_unclaimed_job_ages_out_to_from_source() -> TestResult {
    assert_unclaimed_ages_out(
        "ageout-park-expired",
        Some(std::time::Instant::now() - std::time::Duration::from_millis(1)),
    )
    .await
}

/// sh-044 negative conjunct (a): `created_at.elapsed() <= threshold` ⇒
/// the entry survives. A refactor that off-by-ones the `>` comparison
/// (or drops it) would mass-resolve every fresh unclaimed job
/// `ResolvedFromSource` on the next tick — this is the predicate's
/// only age conjunct, and the positive tests above only ever backdate
/// PAST the threshold.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn aged_out_survives_under_threshold() -> TestResult {
    let survived = run_age_out_predicate(
        "ageout-fresh",
        0,
        None,
        None,
        JobOrigin::CacheOpportunity,
        false,
    )
    .await?;
    assert!(
        survived,
        "0s ≤ 3×60s threshold: the age-out arm's `created_at.elapsed() \
         > age_out_after` conjunct must NOT match a fresh entry"
    );
    Ok(())
}

/// sh-044 negative conjunct (b): `holder()=Some` past the threshold ⇒
/// the entry survives. A refactor that drops the `if e.holder()
/// .is_some() { continue }` guard at the top of the partition would
/// resolve actively-claimed jobs `ResolvedFromSource` MID-PULL — every
/// other `JobViewEntry` in the suite is `fresh_now()`-stamped and
/// never reaches the age-out arm, so without this pin the guard
/// deletion passes the entire test set.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn aged_out_survives_when_holder_some() -> TestResult {
    let survived = run_age_out_predicate(
        "ageout-held",
        240,
        None,
        Some(crate::state::ExecutorId::from("store-0-w0")),
        JobOrigin::CacheOpportunity,
        false,
    )
    .await?;
    assert!(
        survived,
        "holder()=Some past the threshold: the partition's \
         `e.holder().is_some() → continue` guard must keep the \
         actively-claimed entry out of BOTH arms"
    );
    Ok(())
}

/// sh-044 r2 gate-refusal pin (origin axis): an aged-out
/// `ChildlessLeaf` entry with `origin=Pruned` MUST stay in the view
/// — `from_source_viable(ChildlessLeaf, Some(Pruned))=false`. A
/// refactor that drops the `from_source_viable` call from the
/// aged-out arm (reverting to r1's unconditional resolve) passes
/// every other grid cell: this and the `Holed` cell below are the
/// only pins that vary the gate inputs themselves.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn aged_out_survives_when_origin_pruned() -> TestResult {
    let survived =
        run_age_out_predicate("ageout-pruned", 240, None, None, JobOrigin::Pruned, false).await?;
    assert!(
        survived,
        "ChildlessLeaf+Pruned past the threshold: the age-out arm's \
         `from_source_viable` gate must refuse (the bc84397f9 hazard \
         — a pruned root's closure was deliberately dropped and must \
         NOT evict-and-requeue)"
    );
    Ok(())
}

/// sh-044 r2 gate-refusal pin (evidence axis): an aged-out entry
/// whose durable evidence is `Holed` MUST stay in the view —
/// `from_source_viable(Holed, _)=false` regardless of origin. The
/// 1-child seed (completed, no live co-owning build voucher) is the
/// `CHILD_PRODUCED_SQL ∧ ¬CHILD_LIVE_VOUCHER_SQL` previous-generation
/// shape.
// r[verify sched.materialize.unclaimed-age-out]
#[tokio::test]
async fn aged_out_survives_when_evidence_holed() -> TestResult {
    let survived = run_age_out_predicate(
        "ageout-holed",
        240,
        None,
        None,
        JobOrigin::CacheOpportunity,
        true,
    )
    .await?;
    assert!(
        survived,
        "Holed evidence past the threshold: the age-out arm's \
         `from_source_viable` gate must refuse (a previous-generation \
         child without a live voucher means from-source is NOT viable)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// live_041 — listing distribution (rendezvous partition + steal horizon)
// ---------------------------------------------------------------------------

/// Seed `n` claimable materialization jobs through the production
/// creation path: one merged build of `n` independent nodes, outputs
/// marked substitutable AFTER the merge, dispatch ticks until the
/// probe partition has created every job (bounded loop — one tick per
/// dispatch wave, panics rather than spinning forever).
async fn seed_claimable_jobs(
    handle: &ActorHandle,
    store: &rio_test_support::grpc::MockStore,
    prefix: &str,
    n: usize,
) -> TestResult {
    let mut nodes = Vec::with_capacity(n);
    let mut outs = Vec::with_capacity(n);
    for i in 0..n {
        let out = test_store_path(&format!("{prefix}-{i}-out"));
        let mut node = make_node(&format!("{prefix}-{i}"));
        node.expected_output_paths = vec![out.clone()];
        nodes.push(node);
        outs.push(out);
    }
    let build_id = Uuid::new_v4();
    merge_dag(handle, build_id, nodes, vec![], false).await?;
    barrier(handle).await;
    store.state.substitutable.write().unwrap().extend(outs);
    for _ in 0..(n * 2).max(8) {
        tick(handle).await?;
        barrier(handle).await;
        let listed = list_materialization_jobs(handle, (n as u32) * 2).await;
        if listed.len() >= n {
            return Ok(());
        }
    }
    panic!("seed_claimable_jobs: probe partition never created all {n} jobs");
}

/// The per-worker member identity exactly as the store's claim path
/// mints it (mod.rs: `executor_instance().with_worker(w)` — sanitize
/// + the one composer; R13 witness provenance).
fn worker_member(pod: &str, w: usize) -> String {
    rio_common::dns::Dns1123Label::sanitize(
        pod,
        rio_common::dns::WORKER_SUFFIX_RESERVED,
        "rio-store-dev",
    )
    .with_worker(w)
    .as_str()
    .to_owned()
}

// r[verify sched.materialize.listing-distribution]
/// live_041 RED (1): the leader listing must PARTITION the claimable
/// head across the live store-worker membership — three workers'
/// listings pairwise disjoint and jointly covering the claimable set.
/// Pre-fix there is no identity axis at all: every caller is served
/// the same deterministic `ORDER BY created_at` head, so N replicas
/// race the head job, one wins, N-1 burn their pass (live exhibit:
/// 95.6% of claim attempts wasted; 29.1s fleet-wide zero-throughput
/// stall; KEDA scale-out during a stall ADDS RACERS).
#[tokio::test]
async fn rendezvous_listings_are_disjoint_and_cover() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "rdv-part", 12).await?;

    let members: Vec<String> = (0..3).map(|w| worker_member("store-rdv", w)).collect();
    // Beat 0: every member announces itself (its first listing call
    // IS its membership registration — sparse-membership listings
    // during this warm-up are the designed bootstrap and not asserted
    // on).
    for m in &members {
        let _ = list_materialization_jobs_as(&handle, 64, m).await;
    }
    // Beat 1: full membership — the partition law applies.
    let mut listings = Vec::new();
    for m in &members {
        listings.push(list_materialization_jobs_as(&handle, 64, m).await);
    }
    let sets: Vec<std::collections::HashSet<Uuid>> = listings
        .iter()
        .map(|l| l.iter().map(|j| j.job_id).collect())
        .collect();
    // Pairwise disjoint.
    for a in 0..sets.len() {
        for b in (a + 1)..sets.len() {
            assert!(
                sets[a].is_disjoint(&sets[b]),
                "left: 3 identical heads (every member served the same \
                 deterministic listing) / right: partition — workers {a} and \
                 {b} overlap on {:?}",
                sets[a].intersection(&sets[b]).collect::<Vec<_>>()
            );
        }
    }
    // Jointly cover the claimable set.
    let union: std::collections::HashSet<Uuid> = sets.iter().flatten().copied().collect();
    assert_eq!(
        union.len(),
        12,
        "the three slices must jointly cover the claimable head-window"
    );

    // Parity through the ONE owner fn (Q1: a second partition
    // computation is the banned mirrored-literal shape): each served
    // slice must equal the rendezvous_owner-derived slice exactly.
    let member_refs: Vec<&str> = members.iter().map(|s| s.as_str()).collect();
    for (i, m) in members.iter().enumerate() {
        let expected: std::collections::HashSet<Uuid> = union
            .iter()
            .copied()
            .filter(|j| {
                crate::actor::materialize::rendezvous_owner(*j, member_refs.iter().copied())
                    == Some(m.as_str())
            })
            .collect();
        assert_eq!(
            sets[i], expected,
            "worker {i}'s served slice must be derived from the single \
             rendezvous_owner source"
        );
    }
    Ok(())
}

// r[verify sched.materialize.listing-distribution]
/// live_041 green pin (CF-1 fallback law): instance-less callers
/// (full dev mode — no member, no slice) are served the UNPARTITIONED
/// listing, byte-identical to the pre-partition deterministic head —
/// even while identity-bearing members exist (unreachable mix in
/// production, pinned total here).
#[tokio::test]
async fn instance_less_listing_stays_unpartitioned() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "rdv-dev", 12).await?;

    let head_a: Vec<Uuid> = list_materialization_jobs(&handle, 8)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    let head_b: Vec<Uuid> = list_materialization_jobs(&handle, 8)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(
        head_a, head_b,
        "dev mode: the deterministic head, unchanged"
    );
    assert_eq!(head_a.len(), 8);

    // Identity-bearing members join; the instance-less caller still
    // sees the full head (it cannot own a slice).
    for w in 0..2 {
        let _ = list_materialization_jobs_as(&handle, 8, &worker_member("store-dev", w)).await;
    }
    let head_c: Vec<Uuid> = list_materialization_jobs(&handle, 8)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    assert_eq!(
        head_a, head_c,
        "an instance-less caller is served the unpartitioned head even \
         alongside live members"
    );
    Ok(())
}

// r[verify sched.materialize.listing-distribution]
/// live_041 RED (3), the KEDA shape (structural count, not
/// wall-clock): per-beat DISTINCT listed jobs must strictly grow when
/// a member joins. Pre-fix the fleet advances at most one listing
/// window per round regardless of replica count — a hard
/// ~window-size/round ceiling that inverts the KEDA feedback
/// (scale-out adds racers, not throughput).
#[tokio::test]
async fn member_join_grows_distinct_listed_jobs() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "rdv-grow", 40).await?;

    // One member, window 8: at most 8 distinct jobs per beat (solo
    // membership serves unpartitioned — the worker owns everything).
    let m0 = worker_member("store-grow", 0);
    let solo = list_materialization_jobs_as(&handle, 8, &m0).await;
    let solo_distinct: std::collections::HashSet<Uuid> = solo.iter().map(|j| j.job_id).collect();
    assert_eq!(
        solo_distinct.len(),
        8,
        "precondition: window-bound solo beat"
    );

    // A second member joins: the two members' post-join beats draw
    // from DISJOINT slices of the 40-job head, so their union must
    // exceed one window (the slices partition all 40 jobs — both
    // fitting inside one 8-window is unsatisfiable).
    let m1 = worker_member("store-grow", 1);
    let _ = list_materialization_jobs_as(&handle, 8, &m1).await; // join beat
    let beat0 = list_materialization_jobs_as(&handle, 8, &m0).await;
    let beat1 = list_materialization_jobs_as(&handle, 8, &m1).await;
    let mut union: std::collections::HashSet<Uuid> = beat0.iter().map(|j| j.job_id).collect();
    union.extend(beat1.iter().map(|j| j.job_id));
    assert!(
        union.len() > 8,
        "left: per-beat distinct listed jobs flat at the window size as \
         members grow / right: strictly grows per member (got {} distinct \
         across 2 members, window 8)",
        union.len()
    );
    Ok(())
}

// r[verify sched.materialize.listing-distribution]
/// live_041 — the steal horizon, SERVER-side (RULED CF-3: no client
/// steal lane exists): a member that misses its beat past
/// LISTING_STEAL_HORIZON has its owner slice served to the surviving
/// callers' normal listings; when it returns, the partition resumes.
///
/// Strawman disclosure (TI-4): the steal horizon does not exist
/// pre-fix, so no pre-fix red is runnable for THIS lane — the
/// pre-fix behavior is pinned by `rendezvous_listings_are_disjoint_
/// and_cover`'s recorded identical-heads red (the WO-S3-2 commit-A
/// protocol precedent). Aging is real-time (5.2 s sleep): load can
/// only make the silent owner MORE stale, so the assertion direction
/// cannot flake.
#[tokio::test]
async fn stale_owner_slice_enters_the_steal_horizon() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "rdv-steal", 12).await?;

    let m_a = worker_member("store-steal", 0);
    let m_b = worker_member("store-steal", 1);
    // Beat 0: both members announce; beat 1: fresh partition.
    let _ = list_materialization_jobs_as(&handle, 64, &m_a).await;
    let _ = list_materialization_jobs_as(&handle, 64, &m_b).await;
    let slice_a: std::collections::HashSet<Uuid> = list_materialization_jobs_as(&handle, 64, &m_a)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    let fresh_b: std::collections::HashSet<Uuid> = list_materialization_jobs_as(&handle, 64, &m_b)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    assert!(
        slice_a.is_disjoint(&fresh_b),
        "precondition: fresh members are partitioned"
    );
    assert!(
        !slice_a.is_empty(),
        "precondition: A owns part of the head (12 jobs across 2 members)"
    );

    // A goes silent past the steal horizon (5 s + slack); B's NEXT
    // normal listing — same RPC, no flag — includes A's unclaimed
    // slice.
    tokio::time::sleep(std::time::Duration::from_millis(5200)).await;
    let stolen_b: std::collections::HashSet<Uuid> = list_materialization_jobs_as(&handle, 64, &m_b)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    assert!(
        stolen_b.is_superset(&slice_a),
        "the stale owner's slice enters B's steal horizon (missing: {:?})",
        slice_a.difference(&stolen_b).collect::<Vec<_>>()
    );
    assert_eq!(
        stolen_b.len(),
        12,
        "B's listing = own slice UNION the stale owner's slice = the whole head"
    );

    // A returns: its next listing re-registers the contact and the
    // partition resumes — duplication is bounded by owner staleness
    // (exactly-one-LISTING is deliberately NOT the law; the RESET is).
    let returned_a: std::collections::HashSet<Uuid> =
        list_materialization_jobs_as(&handle, 64, &m_a)
            .await
            .iter()
            .map(|j| j.job_id)
            .collect();
    let post_b: std::collections::HashSet<Uuid> = list_materialization_jobs_as(&handle, 64, &m_b)
        .await
        .iter()
        .map(|j| j.job_id)
        .collect();
    assert!(
        returned_a.is_disjoint(&post_b),
        "partition resumes once the owner returns"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// bug_045 — listing cost envelope (sched.materialize.listing-cost)
// ---------------------------------------------------------------------------

/// Instrumentation pin (R16): the three cost-envelope counters are
/// wired to LIVE choke sites — a cold first beat in a multi-member
/// epoch must move all three (scores: the partition walk; fetches:
/// the head-window query; member touches: the contact-map scans).
/// This certifies the counters' own wiring, on the unfixed tree and
/// the fixed one alike (a cold beat legitimately scores, fetches, and
/// scans on both); the per-poll ZERO laws are the envelope tests'
/// proposition, not this pin's.
#[tokio::test]
async fn listing_cost_counters_move_at_the_choke_sites() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "cost-wire", 6).await?;

    let members: Vec<String> = (0..3).map(|w| worker_member("store-wire", w)).collect();
    // Age any seeding-time snapshot past its TTL so the measured
    // beats include a head-window refresh (post-close, joins alone
    // do not refetch a warm snapshot — by design).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let before = crate::actor::materialize::listing_cost_snapshot();
    for m in &members {
        let _ = list_materialization_jobs_as(&handle, 64, m).await;
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    assert!(
        after.scores_computed > before.scores_computed,
        "cold-beat partition work must pass through the counted scoring source"
    );
    assert!(
        after.snapshot_fetches > before.snapshot_fetches,
        "the head-window query must pass through the counted call site"
    );
    assert!(
        after.member_touches > before.member_touches,
        "membership scans must pass through the counted choke sites"
    );
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 — plan-level unit: a member JOIN scores EXACTLY one pair
/// per cached job (the joiner against each stored winner), and the
/// cache converges to the batch argmax after the poll's reconcile.
/// Proposition certified (R16): the join leg of the membership-event
/// maintenance bound, by operation count.
#[test]
fn member_join_scores_exactly_one_per_cached_job() {
    let jobs: Vec<Uuid> = (0..16).map(|_| Uuid::now_v7()).collect();
    let members: Vec<String> = (0..3)
        .map(|w| worker_member("store-plan-join", w))
        .collect();
    let mut plan = crate::actor::materialize::ListingPlan::default();
    for m in &members {
        plan.on_join(m); // empty cache: joins cost nothing yet
    }
    plan.reconcile_window(jobs.iter().copied(), members.iter().map(|m| m.as_str()));

    let joiner = worker_member("store-plan-join", 3);
    let before = crate::actor::materialize::listing_cost_snapshot();
    plan.on_join(&joiner);
    let after = crate::actor::materialize::listing_cost_snapshot();
    assert_eq!(
        after.scores_computed - before.scores_computed,
        16,
        "left: the join rescores window x members / right: exactly one \
         score per cached job (the joiner vs each stored winner)"
    );

    // Parity after the poll's reconcile (the arm always reconciles
    // after folding events).
    let all: Vec<&str> = members
        .iter()
        .map(|m| m.as_str())
        .chain(std::iter::once(joiner.as_str()))
        .collect();
    plan.reconcile_window(jobs.iter().copied(), all.iter().copied());
    for job in &jobs {
        assert_eq!(
            plan.owner(*job),
            crate::actor::materialize::rendezvous_owner(*job, all.iter().copied()),
            "cached owner must equal the batch argmax after the join"
        );
    }
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 — plan-level unit: an OWNER's TTL leave re-argmaxes ONLY
/// the departed member's jobs (exactly owned x survivors scores;
/// within the rule's owned x members bound); non-owner leaves are
/// free. Proposition certified (R16): the leave leg of the
/// membership-event maintenance bound, by operation count.
///
/// Disclosed (R16 pre-fix-inexpressible): no leave-granular red is
/// constructible on the unfixed tree — leaves are not events there;
/// every poll rescored the whole window regardless, which is exactly
/// the defect manifest `stable_epoch_polls_compute_zero_scores`
/// recorded true-red (delta = polls x window x members). This test is
/// the post-close structural witness at the event site, with
/// parameterized membership (no wall clock, no TTL sleeps).
#[test]
fn owner_leave_rescores_only_its_bucket() {
    let jobs: Vec<Uuid> = (0..24).map(|_| Uuid::now_v7()).collect();
    let members: Vec<String> = (0..4)
        .map(|w| worker_member("store-plan-leave", w))
        .collect();
    let mut plan = crate::actor::materialize::ListingPlan::default();
    for m in &members {
        plan.on_join(m);
    }
    plan.reconcile_window(jobs.iter().copied(), members.iter().map(|m| m.as_str()));

    let leaver = members[2].clone();
    let owned = jobs
        .iter()
        .filter(|j| plan.owner(**j) == Some(leaver.as_str()))
        .count() as u64;
    assert!(
        owned > 0,
        "precondition: the leaver owns part of the window"
    );
    let survivors: Vec<&str> = members
        .iter()
        .filter(|m| **m != leaver)
        .map(|m| m.as_str())
        .collect();

    let before = crate::actor::materialize::listing_cost_snapshot();
    plan.on_leave(&leaver, survivors.iter().copied());
    let after = crate::actor::materialize::listing_cost_snapshot();
    let delta = after.scores_computed - before.scores_computed;
    assert_eq!(
        delta,
        owned * survivors.len() as u64,
        "left: the leave rescores the whole window / right: exactly the \
         departed owner's bucket over the survivors"
    );
    assert!(
        delta <= owned * members.len() as u64,
        "the sched.materialize.listing-cost bound: <= owned x members"
    );
    for job in &jobs {
        assert_eq!(
            plan.owner(*job),
            crate::actor::materialize::rendezvous_owner(*job, survivors.iter().copied()),
            "cached owner must equal the batch argmax over the survivors"
        );
    }
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 — plan-level unit: the window reconcile scores ONLY jobs
/// ENTERING the window (cached jobs cost nothing; departed jobs are
/// evicted). Proposition certified (R16): the once-per-window-change
/// scoring law, by operation count.
#[test]
fn window_reconcile_scores_only_entering_jobs() {
    let jobs: Vec<Uuid> = (0..12).map(|_| Uuid::now_v7()).collect();
    let members: Vec<String> = (0..3)
        .map(|w| worker_member("store-plan-window", w))
        .collect();
    let mut plan = crate::actor::materialize::ListingPlan::default();
    for m in &members {
        plan.on_join(m);
    }
    plan.reconcile_window(jobs.iter().copied(), members.iter().map(|m| m.as_str()));

    // Same window: zero scoring.
    let before = crate::actor::materialize::listing_cost_snapshot();
    plan.reconcile_window(jobs.iter().copied(), members.iter().map(|m| m.as_str()));
    let after = crate::actor::materialize::listing_cost_snapshot();
    assert_eq!(
        after.scores_computed - before.scores_computed,
        0,
        "left: the reconcile rescores the standing window / right: cached \
         jobs cost nothing"
    );

    // 4 jobs leave, 4 enter: exactly 4 x members scores; the departed
    // are evicted.
    let entering: Vec<Uuid> = (0..4).map(|_| Uuid::now_v7()).collect();
    let next: Vec<Uuid> = jobs[4..].iter().copied().chain(entering).collect();
    let before = crate::actor::materialize::listing_cost_snapshot();
    plan.reconcile_window(next.iter().copied(), members.iter().map(|m| m.as_str()));
    let after = crate::actor::materialize::listing_cost_snapshot();
    assert_eq!(
        after.scores_computed - before.scores_computed,
        4 * members.len() as u64,
        "only the entering jobs are scored, once per member"
    );
    for gone in &jobs[..4] {
        assert_eq!(
            plan.owner(*gone),
            None,
            "jobs that left the window are evicted from the cache"
        );
    }
}

proptest::proptest! {
    #![proptest_config(proptest::prelude::ProptestConfig::with_cases(64))]
    // r[verify sched.materialize.listing-cost+2]
    /// bug_045 — THE parity pin (Q1 single-source law extended to the
    /// maintainer): across generated join/leave/refresh/window-change
    /// poll sequences, the cached owner of EVERY window job equals
    /// the batch argmax `rendezvous_owner` over the live membership.
    /// Proposition certified (R16): cached-owner ≡ batch-argmax under
    /// churn — the claim live_041's serving correctness rides on.
    /// Members are minted through the production composer
    /// (`Dns1123Label::sanitize(..).with_worker`); jobs are `now_v7`.
    /// Green-side by necessity (disclosed): the cache must exist to
    /// be compared.
    #[test]
    fn cached_owner_map_matches_batch_argmax_under_churn(
        polls in proptest::collection::vec(
            (
                proptest::option::of(0usize..6),  // joining worker
                proptest::bits::u8::ANY,          // TTL-leave mask (workers 0..6)
                proptest::option::of(proptest::bits::u32::ANY), // window change mask (jobs 0..24)
            ),
            1..32,
        ),
    ) {
        let jobs: Vec<Uuid> = (0..24).map(|_| Uuid::now_v7()).collect();
        let members: Vec<String> = (0..6)
            .map(|w| worker_member("store-churn", w))
            .collect();
        let mut plan = crate::actor::materialize::ListingPlan::default();
        let mut live: std::collections::BTreeSet<usize> = std::collections::BTreeSet::new();
        let mut window: Vec<Uuid> = Vec::new();

        for (join, leave_mask, window_mask) in polls {
            // The arm's event protocol per poll: prune leaves (all at
            // once), note the caller (join), fold leave events over
            // the post-prune membership (joiner included), fold the
            // join, then reconcile the window.
            let mut left: Vec<usize> = Vec::new();
            for w in 0..6 {
                if leave_mask & (1 << w) != 0 && live.remove(&w) {
                    left.push(w);
                }
            }
            let joined = match join {
                Some(w) => live.insert(w),
                None => false,
            };
            let live_members: Vec<&str> =
                live.iter().map(|w| members[*w].as_str()).collect();
            for w in &left {
                plan.on_leave(&members[*w], live_members.iter().copied());
            }
            if joined
                && let Some(w) = join
            {
                plan.on_join(&members[w]);
            }
            if let Some(mask) = window_mask {
                window = jobs
                    .iter()
                    .enumerate()
                    .filter(|(i, _)| mask & (1 << i) != 0)
                    .map(|(_, j)| *j)
                    .collect();
            }
            plan.reconcile_window(window.iter().copied(), live_members.iter().copied());

            for job in &window {
                proptest::prop_assert_eq!(
                    plan.owner(*job),
                    crate::actor::materialize::rendezvous_owner(
                        *job,
                        live_members.iter().copied()
                    ),
                    "cached owner != batch argmax after a poll's events"
                );
            }
        }
    }
}
// r[verify sched.materialize.listing-cost+2]
/// bug_045 envelope red (1) — the per-poll cost law's scoring half.
/// Proposition certified (R16): on a poll in a STABLE membership
/// epoch over a WARM snapshot, partition-scoring work is ZERO and no
/// member scan runs — the owner map is maintained per membership
/// event, never recomputed per poll. Structural witness: operation
/// counters (the repo's structural-over-wall-clock rule), never
/// wall-clock.
///
/// Pre-fix defect manifest (recorded verbatim in the close commit):
/// every identity-bearing poll in a >1 membership rescored the whole
/// window per member — delta = polls x window x members SipHashes,
/// all serialized on the single-threaded actor turn.
///
/// The member-touch half is asserted when no TTL refresh fired during
/// the measured polls (refresh-boundary beat work is membership-event
/// work, not per-poll work; under a CI stall past the snapshot TTL
/// the scoring assertion still binds unconditionally).
#[tokio::test]
async fn stable_epoch_polls_compute_zero_scores() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "cost-stable", 12).await?;

    let members: Vec<String> = (0..3).map(|w| worker_member("store-stable", w)).collect();
    // Warm-up: two beats per member — joins processed, snapshot warm.
    for _ in 0..2 {
        for m in &members {
            let _ = list_materialization_jobs_as(&handle, 64, m).await;
        }
    }

    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..10 {
        let served = list_materialization_jobs_as(&handle, 64, &members[0]).await;
        assert!(!served.is_empty(), "precondition: the member owns a slice");
    }
    let after = crate::actor::materialize::listing_cost_snapshot();

    let refreshes = after.snapshot_fetches - before.snapshot_fetches;
    assert_eq!(
        after.scores_computed - before.scores_computed,
        0,
        "left: every poll rescores the window per member (delta = polls x \
         window x members SipHashes per actor turn) / right: zero scoring \
         work on a stable-epoch poll over a warm snapshot"
    );
    if refreshes == 0 {
        assert_eq!(
            after.member_touches - before.member_touches,
            0,
            "left: per-poll membership scans (contact retain + owner-age \
             walks per row) / right: zero member touches on a warm-snapshot \
             stable-epoch poll"
        );
    }
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 envelope red (2) — the head-window fetch half. Proposition
/// certified (R16, scope stated exactly): the HEALTHY-path wall-clock
/// fetch-rate bound — identity-bearing polls within one beat share
/// ONE head-window fetch, budgeted `1 + elapsed` so a CI stall widens
/// the budget instead of flaking. This is deliberately NOT the
/// degraded-regime law (an elapsed-derived budget self-widens when
/// each fetch itself consumes ≥ TTL of wall clock — merged_bug_066):
/// failure-arm pacing, latency independence, and dirty-token
/// consumption are certified STRUCTURALLY — fetches per refresh
/// decision, never per wall-second — by
/// `failed_beat_charges_the_pacing_envelope`,
/// `slow_beat_does_not_convert_polls_into_serial_beats`, and
/// `dirty_edge_paces_through_a_failure_window`.
///
/// Pre-fix defect manifest: N fetches for N polls — the 512-row query
/// awaited in-turn on the single-threaded actor, once per poll per
/// worker fleet-wide.
#[tokio::test]
async fn listing_serves_polls_share_one_head_fetch_per_beat() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "cost-beat", 12).await?;

    let m0 = worker_member("store-beat", 0);
    let m1 = worker_member("store-beat", 1);
    // Warm-up beats: membership registered, snapshot warm.
    let _ = list_materialization_jobs_as(&handle, 64, &m0).await;
    let _ = list_materialization_jobs_as(&handle, 64, &m1).await;
    // Age the snapshot past its TTL so the measured window opens on a
    // refresh boundary: the law's "once" side is then observable
    // (exactly one fetch serves all eight polls on a healthy run).
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;

    let started = std::time::Instant::now();
    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..8 {
        let _ = list_materialization_jobs_as(&handle, 64, &m0).await;
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    let elapsed = started.elapsed();

    let delta = after.snapshot_fetches - before.snapshot_fetches;
    // 1 boundary refresh + 1 per full TTL window the measured polls
    // actually spanned (TTL = 1 s).
    let budget = 1 + elapsed.as_secs();
    assert!(
        delta >= 1,
        "the TTL-expired boundary poll must refresh the snapshot"
    );
    assert!(
        delta <= budget,
        "left: one head-window fetch PER POLL ({delta} fetches for 8 polls \
         in {elapsed:?}) / right: at most one fetch per snapshot TTL window \
         (budget {budget})"
    );
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 envelope red (3) — the membership-event maintenance bound.
/// Proposition certified (R16): a member JOIN rescores at most the
/// cached window — one score per cached job (the joiner against the
/// cached winner) — never window x members.
///
/// Pre-fix defect manifest: the join beat rescored every row against
/// every member (delta = window x (members+1) for the joiner's first
/// listing).
#[tokio::test]
async fn member_join_rescores_at_most_the_window() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    const WINDOW: usize = 12;
    seed_claimable_jobs(&handle, &store, "cost-join", WINDOW).await?;

    let members: Vec<String> = (0..3).map(|w| worker_member("store-join", w)).collect();
    for _ in 0..2 {
        for m in &members {
            let _ = list_materialization_jobs_as(&handle, 64, m).await;
        }
    }

    // The fourth worker's first identity-bearing listing IS its join.
    let joiner = worker_member("store-join", 3);
    let before = crate::actor::materialize::listing_cost_snapshot();
    let _ = list_materialization_jobs_as(&handle, 64, &joiner).await;
    let after = crate::actor::materialize::listing_cost_snapshot();

    let delta = after.scores_computed - before.scores_computed;
    assert!(
        delta <= WINDOW as u64,
        "left: the join beat rescores window x members ({delta} scores for \
         a {WINDOW}-job window, 4 members) / right: at most one score per \
         cached job (the joiner vs each cached winner)"
    );
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// bug_045 envelope red (4) — the snapshot's refresh law. Proposition
/// certified (R16): warm-snapshot polls do NOT fetch (at most one
/// fetch per TTL window, elapsed-budgeted), and a job created through
/// the production creation path enters the very next poll's listing —
/// the creation-dirty edge refreshes without waiting out the TTL.
///
/// Pre-fix defect manifest: every poll fetched the head window (the
/// warm-poll half is the red; the dirty half holds trivially pre-fix
/// because every poll re-queries).
#[tokio::test]
async fn snapshot_refreshes_once_per_ttl_and_on_creation_dirty() -> TestResult {
    let (test_db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "cost-dirty", 8).await?;
    let db = crate::db::SchedulerDb::new(test_db.pool.clone());

    let m0 = worker_member("store-dirty", 0);
    let m1 = worker_member("store-dirty", 1);
    let _ = list_materialization_jobs_as(&handle, 64, &m0).await;
    let _ = list_materialization_jobs_as(&handle, 64, &m1).await;
    // Open the measured window on a fresh beat (age the snapshot past
    // its TTL, let ONE listing absorb the boundary refresh) so the
    // warm-poll budget below is anchored, not racing the warm-up's
    // unknown snapshot age.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    let _ = list_materialization_jobs_as(&handle, 64, &m0).await;

    // Warm polls: serve from the beat snapshot.
    let started = std::time::Instant::now();
    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..5 {
        let _ = list_materialization_jobs_as(&handle, 64, &m0).await;
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    let warm_delta = after.snapshot_fetches - before.snapshot_fetches;
    let warm_budget = started.elapsed().as_secs();
    assert!(
        warm_delta <= warm_budget,
        "left: warm polls each fetch the head window ({warm_delta} fetches \
         for 5 polls) / right: zero fetches inside a warm TTL window \
         (budget {warm_budget})"
    );

    // Creation-dirty edge: mint a ninth job through the production
    // creation path (merge -> probe partition), with NO listing call
    // in between — db row count is the creation witness.
    let out = test_store_path("cost-dirty-late-out");
    let mut node = make_node("cost-dirty-late");
    node.expected_output_paths = vec![out.clone()];
    merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    barrier(&handle).await;
    store.state.substitutable.write().unwrap().push(out);
    let mut created = false;
    for _ in 0..16 {
        tick(&handle).await?;
        barrier(&handle).await;
        let (jobs, _) = db.count_materialization_rows().await?;
        if jobs >= 9 {
            created = true;
            break;
        }
    }
    assert!(created, "precondition: the probe partition minted job 9");

    // The very next poll lists the new job — without waiting out the
    // TTL (the dirty edge; on a stalled run the TTL may also have
    // expired, which only widens the same refresh edge).
    let served = list_materialization_jobs_as(&handle, 64, &m0).await;
    let m1_served = list_materialization_jobs_as(&handle, 64, &m1).await;
    let union: std::collections::HashSet<String> = served
        .iter()
        .chain(m1_served.iter())
        .map(|j| j.drv_hash.clone())
        .collect();
    assert!(
        union.contains("cost-dirty-late"),
        "left: the new job waits out the snapshot TTL (absent from the \
         post-creation beat) / right: the creation-dirty edge refreshes \
         the snapshot immediately"
    );
    Ok(())
}

/// bug_095 — the intake-shield pin (R16): certifies the interleaving
/// claim the companion-release docs now state — a REDELIVERED,
/// already-classified RetryLater report reaches neither the deferral
/// stamp (no second pacing window) nor the kind-blind requeue —
/// through the production report path (real claim mint, real report
/// intake, real redelivery; no companion called directly). The
/// load-bearing shield is `fold_report`'s intake gate
/// (rio-evidence-kernel): it AckIgnores any report for an inactive or
/// already-classified attempt. Regression PIN, not a red — the shield
/// holds today (disclosed; the DEFECT was the prose, quoted
/// before/after in the commit body).
#[tokio::test]
async fn redelivered_retry_later_after_classification_is_inert() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("maton-redeliver-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-redeliver");
    n.expected_output_paths = vec![out.clone()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    barrier(&handle).await;

    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-redeliver".into(),
            auth_intent: Some("maton-redeliver".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    let assignment = match outcome {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;

    let retry_later_payload = || crate::actor::pull::PullReportPayload {
        result: rio_proto::types::BuildResult::default(),
        peak_memory_bytes: 0,
        peak_cpu_cores: 0.0,
        node_name: None,
        hw_class: None,
        final_resources: None,
        final_line_count: 0,
        materialization_outcome: Some(rio_proto::types::MaterializationOutcome {
            outcome: Some(
                rio_proto::types::materialization_outcome::Outcome::RetryLater(
                    rio_proto::types::materialization_outcome::RetryLater {
                        detail: "upstream rate-limited".into(),
                        retry_after_secs: 1,
                        class: "rate_limited".into(),
                    },
                ),
            ),
        }),
    };

    // First delivery: classifies the attempt, stamps the 1 s deferral,
    // releases + requeues (the legitimate former-holder stamp).
    let first = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("maton-redeliver".into()),
            payload: retry_later_payload(),
            reply,
        })
        .await
        .expect("actor alive");
    assert!(first.is_ok(), "first report must consume: {first:?}");
    barrier(&handle).await;
    let drv = expect_drv(&handle, "maton-redeliver").await;
    assert_eq!(drv.status, DerivationStatus::Ready, "requeued once");

    // Let the pacing window LAPSE (the worker's 1 s hint is floored
    // to RETRY_LATER_DEFAULT_DEFER_SECS = 5 s), then REDELIVER the
    // identical report. If the redelivery re-ran the companion, it
    // would stamp a SECOND 5 s window (admission would refuse below)
    // and re-run the kind-blind requeue.
    tokio::time::sleep(std::time::Duration::from_millis(5300)).await;
    let second = handle
        .query_unchecked(|reply| ActorCommand::ReportPullOutcome {
            exec_id,
            auth_intent: Some("maton-redeliver".into()),
            payload: retry_later_payload(),
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        second.is_ok(),
        "the redelivery must be ACK'd inert (idempotent), got {second:?}"
    );
    barrier(&handle).await;

    // Zero charges either way (RetryLater is charge-free; the
    // redelivery must not mint anything).
    let charges: i64 =
        sqlx::query_scalar("SELECT count(*) FROM drv_attempts WHERE event_kind = 'attempt'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(charges, 0, "no ledger row from either delivery");

    // Node untouched by the redelivery: still Ready, still claimable.
    let drv = expect_drv(&handle, "maton-redeliver").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Ready,
        "the redelivery ran no kind-blind reset"
    );

    // THE stamp probe: the deferral lapsed BEFORE the redelivery — if
    // the redelivery had re-stamped defer_until, this claim would
    // answer NotYetReady for another second. It must DELIVER a fresh
    // attempt.
    let reclaim = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-redeliver".into(),
            auth_intent: Some("maton-redeliver".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-1-w0".into()),
            claim_nonce: None,
            confirm_only: false,
            executor_token_sha256: None,
            reply,
            resume_exec_id: None,
        })
        .await
        .expect("actor alive");
    let fresh = match reclaim {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!(
            "the redelivery must NOT have re-stamped the pacing window \
             (no second defer); claim after the lapse must deliver, got {other:?}"
        ),
    };
    assert_ne!(
        fresh.exec_id, assignment.exec_id,
        "the post-lapse claim is a FRESH attempt, not the classified one"
    );
    Ok(())
}

/// R14 parity pin (merged_bug_005 / live_046 riders): the store's
/// mirrored scheduler constants equal the real ones — asserted
/// THROUGH the exported store symbols, so the cross-crate cadence
/// derivations (the honest-beat futility interval, the eager-re-poll
/// horizon margin) can never drift silently (rio-store cannot import
/// rio-scheduler; the dependency runs this way).
#[test]
fn store_mirrored_listing_constants_match() {
    assert_eq!(
        rio_store::materialize::client::SCHEDULER_LISTING_MEMBER_TTL_SECS,
        crate::actor::materialize::LISTING_MEMBER_TTL.as_secs(),
    );
    assert_eq!(
        rio_store::materialize::client::SCHEDULER_LISTING_STEAL_HORIZON_SECS,
        crate::actor::materialize::LISTING_STEAL_HORIZON.as_secs(),
    );
}

// ── bug_090: the fail-fast park persists only an accepted release ──────

/// bug_090 R1: the fail-fast park is a RELEASE through the kinded
/// chokepoint, wired on the production path. PROPOSITION CERTIFIED:
/// after the arm-3 settled fail-fast, the persisted value and the
/// accepted edge are the same event — observed structurally through
/// the claim carrier (`exec_id == None` after settlement iff the
/// release chokepoint accepted; the carrier clear and the returned
/// target are the same `apply_validated_transition` event, so the
/// witness is the proposition one constructor earlier, not a proxy).
/// Production constructors only: real merge (prune fires), real pull
/// mint, real Unobtainable report through the consumption intake.
#[tokio::test]
async fn fail_fast_releases_the_claim_through_the_kinded_chokepoint() -> TestResult {
    use rio_auth::hmac::HmacSigner;

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let service_key = b"test-bug090-failfast-key-32bytes!".to_vec();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_cfg, p| {
            p.service_signer = Some(Arc::new(HmacSigner::from_key(service_key)));
        });
    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "bug090-tenant").await;

    // The pruned-root shape: root substitutable at merge → prune fires
    // → root marked + childless + origin=pruned job.
    let root_out = test_store_path("b90-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mut root = make_node("b90-root");
    root.expected_output_paths = vec![root_out.clone()];
    root.wanted_output_names = vec!["out".into()];
    let mut dep = make_node("b90-dep");
    dep.expected_output_paths = vec![test_store_path("b90-dep-out")];
    let build_id = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: Some(tenant),
            nodes: vec![root, dep],
            edges: vec![make_test_edge("b90-root", "b90-dep")],
            jwt_token: Some("harness-tenant-jwt".into()),
            ..Default::default()
        },
    )
    .await?;
    barrier(&handle).await;

    // Claim (the real pull mint stamps the exec carrier), then the
    // upstream entry vanishes and the worker confirms the absence.
    let assignment = match claim_materialization(&handle, "b90-root", "store-test-0").await {
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
        "b90-root",
        mat_unobtainable_outcome(
            vec![root_out.clone()],
            vec![],
            "upstream 404 on the pruned root's output",
        ),
    )
    .await
    .map_err(|e| anyhow::anyhow!("unobtainable report rejected: {e:?}"))?;
    barrier(&handle).await;

    // The claim carrier is RELEASED through the kinded chokepoint —
    // not latched through the terminal.
    let info = handle
        .debug_query_derivation("b90-root")
        .await?
        .expect("node still in DAG (reap runs on a later sweep)");
    assert_eq!(
        info.exec_id, None,
        "left: exec_id = Some(mat_exec) latched through the terminal (the \
         refused park left the claim carrier; the cancel epilogue \
         cross-correlated the settled exec) / right: None (released through \
         validate_transition_for_release)"
    );
    // The released Queued park is classified not-yet-dispatched by the
    // cancel cascade (pre-fix: memory stuck Assigned → the in-flight
    // arm cancelled it, an artifact of the divergence).
    assert_eq!(
        info.status,
        DerivationStatus::DependencyFailed,
        "the released park rides the cascade's not-yet-dispatched arm"
    );
    // PG and memory agree at every step (pre-fix PG was overwritten to
    // 'queued' over a refused edge mid-flight).
    let pg_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'b90-root'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(pg_status, "dependency_failed", "durable row matches memory");
    // The mat exec's drv_executions row carries the settled close's
    // stamp — the close's assignment-close owns the verdict; the
    // cancel-time epilogue no longer resolves this exec at all.
    let exec_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM drv_executions WHERE exec_id = $1")
            .bind(exec_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        exec_status.as_deref(),
        Some("succeeded"),
        "the settled close's stamp is the row's verdict"
    );
    // The build still fails with the resubmit-directing format (the
    // fail-fast's purpose is untouched by the release discipline).
    let st = query_status(&handle, build_id).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "the fail-fast still fails the interested build"
    );
    Ok(())
}

/// bug_090 R2: a no-edge arrival produces ZERO durable writes — the
/// persist-count delta IS the no-edge-no-write law. PROPOSITION
/// CERTIFIED: an already-parked (Queued) pruned root re-entering the
/// fail-fast helper persists nothing (pre-fix: one no-edge same-value
/// persist — the merged_bug_006 RefusedNewer feeder). Defense-in-depth
/// lane: production-minted entry state (a parked Queued root with live
/// interest), then the `pub(super)` helper invoked directly — the
/// sanctioned unit lane for a production-minted entry state.
#[tokio::test]
async fn already_parked_fail_fast_persists_nothing() -> TestResult {
    use std::sync::atomic::Ordering;

    let db = TestDb::new(&MIGRATOR).await;
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'queued') \
         RETURNING derivation_id",
    )
    .bind("b90-parked")
    .bind(test_drv_path("b90-parked"))
    .fetch_one(&db.pool)
    .await?;
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("b90-parked", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("b90-parked").expect("just injected");
        s.set_status_for_test(DerivationStatus::Queued);
        // Live interest keeps the helper's actionable guard open; the
        // build entry itself is absent (the cascade tolerates that —
        // its persists ride the UNCOUNTED batch path either way).
        s.interested_builds.insert(Uuid::new_v4());
    }

    let before = actor
        .test_counters
        .persist_status_calls
        .load(Ordering::SeqCst);
    actor
        .fail_fast_pruned_root(&DrvHash::from("b90-parked"), "already-parked re-entry")
        .await;
    let after = actor
        .test_counters
        .persist_status_calls
        .load(Ordering::SeqCst);
    assert_eq!(
        after - before,
        0,
        "left: 1 (a no-edge same-value persist — the merged_bug_006 \
         RefusedNewer feeder) / right: 0 (no edge, no write)"
    );
    Ok(())
}

// ── merged_bug_066: the beat charges its pacing per ATTEMPT ────────────

// r[verify sched.materialize.listing-cost+2]
/// merged_bug_066 R1: under a real PG failure (closed pool — every
/// query errors at acquire, the production-shaped connection
/// failure), fetch ATTEMPTS are bounded by the pacing envelope, not
/// by poll count. PROPOSITION CERTIFIED: fetches per refresh
/// decision — the rule's true quantity — with the failed attempt
/// charging the envelope (BeatToken spent into spend_failed).
/// Structural count; the elapsed guard only documents that the loop
/// stayed inside one TTL window.
#[tokio::test]
async fn failed_beat_charges_the_pacing_envelope() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
    seed_claimable_jobs(&handle, &store, "cost-errbeat", 4).await?;
    let m0 = worker_member("store-errbeat", 0);
    // Warm membership + snapshot.
    let _ = list_materialization_jobs_as(&handle, 64, &m0).await;
    // Age past the TTL so the measured window opens on a beat
    // boundary, then kill PG.
    tokio::time::sleep(std::time::Duration::from_millis(1100)).await;
    db.pool.close().await;

    let started = std::time::Instant::now();
    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..6 {
        let served = list_materialization_jobs_as(&handle, 64, &m0).await;
        assert!(served.is_empty(), "failed beats serve fail-closed empty");
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    let elapsed = started.elapsed();

    let delta = after.snapshot_fetches - before.snapshot_fetches;
    let budget = 1 + elapsed.as_secs();
    assert!(
        delta <= budget,
        "left: 6 (the Err arm zeroed the TTL — one 512-row attempt per \
         poll, serialized on the actor into the degraded PG) / right: 1 \
         (the failed attempt charges the envelope; budget {budget} over \
         {elapsed:?})"
    );
    assert!(delta >= 1, "the boundary poll attempts once");
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// merged_bug_066 R2: with injected beat latency ≥ TTL, polls queued
/// behind a completing beat share its snapshot. PROPOSITION
/// CERTIFIED: the fetch delta counts completions, not polls — the
/// pacing anchor samples at attempt COMPLETION, so query latency
/// cannot birth every snapshot expired. The latency hook is a
/// DISCLOSED harness alignment (r13-allow(opaque-consumer) shape: a
/// cfg(test) awaited delay between the query await and the spend —
/// exactly where production PG latency sits; 3 lines in the
/// handler).
#[tokio::test]
async fn slow_beat_does_not_convert_polls_into_serial_beats() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    actor
        .materialization_jobs
        .hydrated_listing_mut()
        .expect("always-leader actor starts hydrated")
        .2
        .test_beat_latency = Some(std::time::Duration::from_millis(1200));

    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..6 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        actor
            .handle_list_materialization_jobs(64, Some("w0".into()), tx)
            .await;
        let _ = rx.await;
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    let delta = after.snapshot_fetches - before.snapshot_fetches;
    // Beat 1 completes at ~1.2s; the 5 polls behind it land inside
    // the TTL measured FROM COMPLETION. ≤ 2 absorbs one scheduler
    // stall between adjacent polls; the pre-fix shape is 6 (every
    // poll a serial 1.2s beat — the budget must NOT scale with the
    // beats' own latency, which is exactly the self-widening escape).
    assert!(
        delta <= 2,
        "left: {delta} = one serial beat per poll (the pre-query stamp \
         births every snapshot expired once latency ≥ TTL) / right: 1 \
         (completion-sampled clock; ≤ 2 under harness stall)"
    );
    Ok(())
}

// r[verify sched.materialize.listing-cost+2]
/// merged_bug_066 R3: the dirty edge cannot re-open per-poll beating
/// during a failure window. PROPOSITION CERTIFIED: a creation
/// observed by a FAILED attempt is consumed by that attempt's charge
/// (spend_failed takes the token), so outage-time creations cost at
/// most one extra attempt each with the TTL leg as the retry floor.
#[tokio::test]
async fn dirty_edge_paces_through_a_failure_window() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    // One healthy beat to warm the pacing state.
    let (tx, rx) = tokio::sync::oneshot::channel();
    actor
        .handle_list_materialization_jobs(64, Some("w0".into()), tx)
        .await;
    let _ = rx.await;

    // PG dies; one job-creation event lands mid-window (the cfg(test)
    // view-seeding lane bumps the creation cursor exactly like the
    // production creation path does).
    db.pool.close().await;
    actor.materialization_jobs.insert(
        crate::state::DrvHash::from("dirty-window-drv"),
        crate::actor::materialize::JobViewEntry::test_unclaimed(Uuid::new_v4()),
    );

    let started = std::time::Instant::now();
    let before = crate::actor::materialize::listing_cost_snapshot();
    for _ in 0..4 {
        let (tx, rx) = tokio::sync::oneshot::channel();
        actor
            .handle_list_materialization_jobs(64, Some("w0".into()), tx)
            .await;
        let _ = rx.await;
    }
    let after = crate::actor::materialize::listing_cost_snapshot();
    let elapsed = started.elapsed();

    let delta = after.snapshot_fetches - before.snapshot_fetches;
    let budget = 2 + elapsed.as_secs();
    assert!(
        delta <= budget,
        "left: 4 (unconsumed creations re-beat per poll through the \
         failure window) / right: ≤ 2 (the failed attempt consumed the \
         dirty token; one TTL-paced retry; budget {budget} over \
         {elapsed:?})"
    );
    Ok(())
}

/// bug_060 pin (merged_bug_083's screen × merged_bug_014's probe —
/// the two licensed-by-the-stale-census edits both turn this red):
/// the MATERIALIZATION PROBE SHAPE — `kind = Materialization,
/// confirm_only = true, claim_nonce = fresh, executor_token_sha256 =
/// None`, exactly what the store's resume lane presents past full
/// slots (client.rs `confirm_only: probing`) — is SCREENED to
/// `NotYetReady` with ZERO durable side effects, through the
/// production actor command path against a hydrated, DeliverNew-
/// eligible job. Premise reachability (T3): the same shape with
/// `confirm_only = false` then DELIVERS — proving the probe WOULD
/// have minted had the screen not converted it (the NotYetReady is
/// the screen's work, not vacuity). No tracey marker: the
/// `sched.executor.confirm-fence` rule covers the KEYED BUILD lane's
/// fence screen, not this non-keyed kind-blind screen (re-verified at
/// execution — do not stamp a rule the test does not witness).
/// Strawman transcripts ((ddddd), quoted in the commit body) certify
/// the red-capability: (a) a `debug_assert!(!confirm_only)` in the
/// Materialization lane arm panics here; (b) gating the screen on
/// `kind != Materialization` mints — the attempt row appears and the
/// probe answer flips to Deliver.
#[tokio::test]
async fn materialization_probe_is_screened_not_minted() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // A claimable materialization job via the merge new_sub lane —
    // the DeliverNew-eligible premise.
    let out = test_store_path("maton-probe-screen-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut n = make_node("maton-probe-screen");
    n.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    let jobs = sdb(&db.pool)
        .list_claimable_materialization_jobs(16)
        .await?;
    assert_eq!(jobs.len(), 1, "the probe premise: one claimable job");

    // The probe shape (the store's standing oracle, merged_bug_014).
    let probe_nonce = Uuid::new_v4();
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-probe-screen".into(),
            auth_intent: Some("maton-probe-screen".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0".into()),
            resume_exec_id: None,
            claim_nonce: Some(probe_nonce),
            confirm_only: true,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::NotYetReady { .. })),
        "the probe must be screened to NotYetReady, got {outcome:?}"
    );

    // Zero durable side effects: no open-attempt row minted, no
    // confirm-fence row written (the NonKeyed lane writes none).
    let attempts: i64 = sqlx::query_scalar("SELECT count(*) FROM drv_executions")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(attempts, 0, "a screened probe must not mint an attempt");
    let fences: i64 = sqlx::query_scalar("SELECT count(*) FROM executor_confirm_fences")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(fences, 0, "the non-keyed lane writes no fence rows");

    // Premise reachability: the SAME shape minus confirm_only mints —
    // the screen, not job state, produced the NotYetReady above.
    let outcome = handle
        .query_unchecked(|reply| ActorCommand::PullAssignment {
            intent_id: "maton-probe-screen".into(),
            auth_intent: Some("maton-probe-screen".into()),
            kind: rio_evidence_kernel::pull::PullKind::Materialization,
            executor_instance: Some("store-replica-0".into()),
            resume_exec_id: None,
            claim_nonce: Some(Uuid::new_v4()),
            confirm_only: false,
            executor_token_sha256: None,
            reply,
        })
        .await
        .expect("actor alive");
    assert!(
        matches!(outcome, Ok(PullOutcome::Deliver(_))),
        "the claiming form of the same shape must deliver, got {outcome:?}"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Round-9 WO-S2-1 witness-gap delta (W9-N): the sweep's transaction bound
// ---------------------------------------------------------------------------

/// W9-N (round-9 B1 admissibility): the zero-interest cancel sweep is
/// O(1) write transactions for N jobs. The landed close (41e722dfc)
/// replaced the per-job fenced round-trip loop (live_053: 5,258
/// sequential cancels x 3.16ms = 16.6s inside one 134.65s Tick) with
/// ONE fenced sweep + ONE pin-release pass; the landed battery
/// certifies sweep SEMANTICS (rows cancelled, attempts closed, view
/// folds on the disposition) but nothing pinned the statement-count
/// bound itself -- the axis the close exists for, and the axis on
/// which the dead per-job shape would silently return.
///
/// Drives the REAL tick fn with N=40 zero-interest jobs (empty DAG ->
/// every entry takes the `None => true` node-absent arm) and counts
/// WRITE transactions via `pg_current_xact_id()` deltas: xid
/// assignment is immediate, global, and throttle-free (unlike
/// pg_stat_database counters, which lag per-backend up to 1s and can
/// both false-fail and false-pass a tight bound), and under nextest
/// each test owns its postgres instance so the counter is
/// test-private. Each probe forces one xid for its own transaction
/// (subtracted); the per-job regression shape is >= N fenced write
/// transactions, the batched form a small constant (the sweep tx;
/// the pin release is read-only when no pins exist).
// r[verify sched.admission.work-per-turn]
#[tokio::test]
async fn zero_interest_cancel_sweep_transaction_bound() -> TestResult {
    const N: usize = 40;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;

    // Seed N derivations in one batched tx (setup -- uncounted).
    let rows: Vec<crate::db::DerivationRow> = (0..N)
        .map(|i| crate::db::DerivationRow {
            drv_hash: format!("zi-bound-{i:03}"),
            drv_path: rio_test_support::fixtures::test_drv_path(&format!("zi-bound-{i:03}")),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            is_ca: false,
        })
        .collect();
    let mut tx = db.pool.begin().await?;
    let ids = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &rows).await?;
    tx.commit().await?;

    let mut actor = bare_actor(db.pool.clone());
    let generation = actor.serving_generation();
    for i in 0..N {
        let hash = format!("zi-bound-{i:03}");
        let derivation_id = ids.get(hash.as_str()).expect("just inserted").0;
        let created = sdb(&db.pool)
            .create_materialization_job_fenced(
                derivation_id,
                &hash,
                None,
                JobOrigin::Pruned,
                None,
                0.0,
                generation,
            )
            .await?;
        let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
            anyhow::bail!("job create must apply");
        };
        // Production view-entry constructor (R13); the DAG stays empty
        // so every entry is zero-interest via the node-absent arm.
        actor.materialization_jobs.insert(
            DrvHash::from(hash.as_str()),
            crate::actor::materialize::JobViewEntry::test_unclaimed(job_id),
        );
    }

    async fn current_xid(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar("SELECT pg_current_xact_id()::text::bigint")
            .fetch_one(pool)
            .await
            .expect("xid probe")
    }

    let authority = actor
        .dag_authority()
        .expect("direct-setup actor is authoritative");
    let xid_before = current_xid(&db.pool).await;
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;
    let xid_after = current_xid(&db.pool).await;
    // The after-probe consumed one xid itself.
    let write_txns = xid_after - xid_before - 1;
    eprintln!("zero-interest sweep of {N} jobs: {write_txns} write transactions");

    let cancelled: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE state = 'cancelled'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(cancelled, N as i64, "all {N} jobs resolved by ONE sweep");
    assert_eq!(
        actor.materialization_jobs.iter().count(),
        0,
        "every view entry folded on the Applied disposition"
    );
    assert!(
        write_txns < (N / 2) as i64,
        "the zero-interest sweep of {N} jobs issued {write_txns} write \
         transactions; the batched close is one fenced sweep (O(1)) -- \
         a count near {N} is the dead per-job round-trip shape (live_053)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Round-9 WO-S2-6 (bug_110): one PassClock per logical listing pass
// ---------------------------------------------------------------------------

/// **W9-AB (bug_110)** — *a backoff lapsing DURING the beat query is
/// served on this very poll, not withheld until the next one*. The
/// wave-8 disclosed-correction (faac1261e) re-pointed only the
/// prune/members reads to the post-await `beat_now`; the Phase-3
/// `claimability(now)` serve filter and the caller's contact stamp
/// stayed on the PRE-await clock — half the same-class reads. With
/// the listing pass on ONE [`PassClock`] (re-armed once at the beat
/// arm's completion), the serve filter evaluates at the pass clock:
/// a job whose `parked_until` expires during the query latency is
/// ClaimableNow when served.
#[tokio::test]
async fn backoff_lapsing_during_the_beat_query_is_served() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());

    // One durable claimable job + its view entry, parked until 400ms
    // from now — the backoff lapses INSIDE the 1200ms beat latency.
    let drv = insert_test_derivation_local(&db.pool, "beat-lapse").await?;
    let created = sdb(&db.pool)
        .create_materialization_job_fenced(
            drv,
            "beat-lapse",
            None,
            JobOrigin::Pruned,
            None,
            0.0,
            actor.serving_generation(),
        )
        .await?;
    let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("job create must apply");
    };
    let mut entry = crate::actor::materialize::JobViewEntry::test_unclaimed(job_id);
    entry.test_set_parked_until(Some(
        std::time::Instant::now() + std::time::Duration::from_millis(400),
    ));
    actor
        .materialization_jobs
        .insert(crate::state::DrvHash::from("beat-lapse"), entry);
    actor
        .materialization_jobs
        .hydrated_listing_mut()
        .expect("hydrated")
        .2
        .test_beat_latency = Some(std::time::Duration::from_millis(1200));

    let (tx, rx) = tokio::sync::oneshot::channel();
    actor
        .handle_list_materialization_jobs(64, Some("w0".into()), tx)
        .await;
    let served = rx.await?;
    assert_eq!(
        served.len(),
        1,
        "the backoff lapsed during the 1200ms beat (parked 400ms) — \
         the serve filter must evaluate at the pass clock, not the \
         pre-await one (bug_110): served={served:?}"
    );
    Ok(())
}

// r[verify sched.materialize.claimability-projection+1]
/// **W12-S9D (live061-R2, the race-window half)** — *the serve filter
/// reads the IN-MEMORY node face: a job whose node went terminal
/// in-memory is not served even while the PG status row still lags
/// non-terminal*. The durable predicate (W12-S9A) cannot see this
/// window: the actor transitions the node and only then persists the
/// batch — between the two, the beat snapshot still carries the row
/// and a view-only filter that reads claimability alone serves it
/// (the pre-fix shape). The claim against the in-memory-terminal node
/// answers `Gone` regardless (pull admission reads the same DAG), so
/// serving it is advertising a doomed mint — the live_061 burn. The
/// control node proves the filter does not over-exclude: a Ready
/// in-memory node with the same PG shape stays served, and a
/// DAG-ABSENT entry stays served too (absent rows are the
/// zero-interest sweep's 1-tick transient, not the listing's call —
/// the sweep owns them).
#[tokio::test]
async fn listing_excludes_in_memory_terminal_nodes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let generation = actor.serving_generation();

    // Three pending jobs, PG status 'created' (NON-terminal) for all —
    // the durable predicate passes every row; only the in-memory face
    // differs: zombie = Completed in-mem, control = Ready in-mem,
    // ghost = no DAG node at all.
    let mut job_ids = Vec::new();
    for hash in ["race-zombie", "race-control", "race-ghost"] {
        let drv = insert_test_derivation_local(&db.pool, hash).await?;
        let created = sdb(&db.pool)
            .create_materialization_job_fenced(
                drv,
                hash,
                None,
                JobOrigin::CacheOpportunity,
                None,
                0.0,
                generation,
            )
            .await?;
        let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
            anyhow::bail!("job create must apply for {hash}");
        };
        if hash != "race-ghost" {
            actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
                derivation_id: drv,
                ..crate::db::RecoveryDerivationRow::test_default(hash, "x86_64-linux")
            });
        }
        actor.materialization_jobs.insert(
            DrvHash::from(hash),
            crate::actor::materialize::JobViewEntry::test_unclaimed(job_id),
        );
        job_ids.push((hash, job_id));
    }
    // The race window: the zombie's node completes IN-MEMORY only
    // (persist_status_batch has not landed — PG still says 'created').
    actor
        .dag
        .node_mut("race-zombie")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Completed);

    let (tx, rx) = tokio::sync::oneshot::channel();
    actor
        .handle_list_materialization_jobs(64, Some("w0".into()), tx)
        .await;
    let served = rx.await?;
    let served_hashes: Vec<&str> = served.iter().map(|d| d.drv_hash.as_str()).collect();
    assert!(
        !served_hashes.contains(&"race-zombie"),
        "a job whose node is terminal IN-MEMORY must not be served — \
         its claim can only answer Gone (the persist-lag race window \
         the durable predicate cannot see); served={served_hashes:?}"
    );
    assert!(
        served_hashes.contains(&"race-control"),
        "a Ready in-memory node's job stays served (no over-exclusion); \
         served={served_hashes:?}"
    );
    assert!(
        served_hashes.contains(&"race-ghost"),
        "a DAG-absent entry stays served — absence is the zero-interest \
         sweep's transient, not a listing verdict; served={served_hashes:?}"
    );
    Ok(())
}

// r[verify sched.materialize.obsolescence]
/// **W12-S9B (live061-R1)** — *a pending job whose node COMPLETED by
/// other means resolves under the alphabet's own letter: `obsolete`,
/// not `cancelled`*. `JobState::Obsolete` ("the node produced by
/// other means while the job was open" — 078's CHECK has carried the
/// literal since birth) had NO WRITER for the system's entire life:
/// the zero-interest sweep folded the by-other-means face into
/// `cancelled` ("no live DAG-interested build remains" — FALSE here:
/// the interested build is live), so
/// `resolved_total{outcome="obsolete"}` was zero-forever and the
/// live_061 zombie class was forensically invisible. The model
/// (materializationJob.qnt `obsoleteOnProduced`) has demanded the
/// obsolete resolution all along — this is the conformance gap's
/// production half. Also pins: idempotent re-sweep (no double count,
/// at-most-once edge), and the attempt-close riding the same fenced
/// sweep.
#[tokio::test]
async fn node_completed_by_other_means_resolves_obsolete() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    rec.install().expect("install global debugging recorder");

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let generation = actor.serving_generation();

    // One pending job; its node is injected LIVE-INTERESTED and then
    // completes by other means (the store-probe shape: outputs found
    // locally while the job sat unclaimed).
    let drv = insert_test_derivation_local(&db.pool, "obsolete-fold").await?;
    let created = sdb(&db.pool)
        .create_materialization_job_fenced(
            drv,
            "obsolete-fold",
            None,
            JobOrigin::CacheOpportunity,
            None,
            0.0,
            generation,
        )
        .await?;
    let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("job create must apply");
    };
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id: drv,
        ..crate::db::RecoveryDerivationRow::test_default("obsolete-fold", "x86_64-linux")
    });
    // A LIVE interested build: the pre-fix sweep cancelled this row
    // through its node-terminal disjunct even though interest was
    // alive — the letter was wrong, not just late.
    let build_id = Uuid::new_v4();
    actor
        .dag
        .node_mut("obsolete-fold")
        .expect("just injected")
        .interested_builds
        .insert(build_id);
    actor
        .dag
        .node_mut("obsolete-fold")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Completed);
    actor.materialization_jobs.insert(
        DrvHash::from("obsolete-fold"),
        crate::actor::materialize::JobViewEntry::test_unclaimed(job_id),
    );

    let authority = actor
        .dag_authority()
        .expect("direct-setup actor is authoritative");
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;

    let state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE job_id = $1")
            .bind(job_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        state, "obsolete",
        "a pending job whose node COMPLETED by other means must resolve under \
         its own letter (the 078 alphabet's 'obsolete'; the model's \
         obsoleteOnProduced post-state) — 'cancelled' is the live_061 \
         conflation: interest is alive, nothing was cancelled"
    );
    assert_eq!(
        actor.materialization_jobs.iter().count(),
        0,
        "the view entry folds on the settled disposition"
    );

    // Idempotent re-entry: nothing pending, nothing re-counted.
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;

    // (ppppp): snapshot exactly once, at the end — both metrics fold
    // from the one drained snapshot.
    let snapshot = snap.snapshot();
    let by_outcome = {
        use metrics_util::debugging::DebugValue;
        let mut m = std::collections::BTreeMap::new();
        for (ck, _, _, v) in snapshot.into_vec() {
            let DebugValue::Counter(c) = v else { continue };
            let k = ck.key();
            let label = |which: &str| {
                k.labels()
                    .find(|l| l.key() == which)
                    .map(|l| l.value().to_owned())
                    .unwrap_or_default()
            };
            match k.name() {
                "rio_scheduler_materialization_jobs_resolved_total" => {
                    *m.entry(format!("resolved:{}", label("outcome")))
                        .or_insert(0u64) += c;
                }
                "rio_scheduler_materialization_view_node_skew_total" => {
                    *m.entry(format!("skew:{}", label("polarity")))
                        .or_insert(0u64) += c;
                }
                _ => {}
            }
        }
        m
    };
    assert_eq!(
        by_outcome.get("resolved:obsolete").copied().unwrap_or(0),
        1,
        "exactly one obsolete resolution counted (at-most-once; the idempotent \
         re-sweep adds nothing): {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("resolved:cancelled").copied().unwrap_or(0),
        0,
        "the by-other-means face no longer launders into 'cancelled': {by_outcome:?}"
    );
    Ok(())
}

// r[verify sched.materialize.obsolescence]
/// **W12-S9C (live061-R2, the detector half)** — *the view/node skew
/// detector FIRES on the planted zombie*. The metric was registered
/// (lib.rs) and emitted (housekeeping.rs) all along — live_061's
/// "never fired" was a DEAD DETECTION EDGE: both existing polarities
/// (split_release, claimed_no_attempt) quantify over Assigned/Running
/// nodes, and the zombie face (terminal node + pending job) had no
/// arm. The third polarity counts at sweep OBSERVATION, so a sustained
/// nonzero rate is the live_061 signature (a terminal edge minting
/// zombies) even when every row also resolves.
#[tokio::test]
async fn skew_detector_fires_on_terminal_node_pending_job() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    rec.install().expect("install global debugging recorder");

    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let generation = actor.serving_generation();

    let drv = insert_test_derivation_local(&db.pool, "skew-plant").await?;
    let created = sdb(&db.pool)
        .create_materialization_job_fenced(
            drv,
            "skew-plant",
            None,
            JobOrigin::CacheOpportunity,
            None,
            0.0,
            generation,
        )
        .await?;
    let crate::db::materialization::FencedJobCreate::Applied { job_id, .. } = created else {
        anyhow::bail!("job create must apply");
    };
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id: drv,
        ..crate::db::RecoveryDerivationRow::test_default("skew-plant", "x86_64-linux")
    });
    actor
        .dag
        .node_mut("skew-plant")
        .expect("just injected")
        .set_status_for_test(DerivationStatus::Completed);
    actor.materialization_jobs.insert(
        DrvHash::from("skew-plant"),
        crate::actor::materialize::JobViewEntry::test_unclaimed(job_id),
    );

    let authority = actor
        .dag_authority()
        .expect("direct-setup actor is authoritative");
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;

    // (ppppp): one snapshot.
    let polarities = crate::sla::metrics::counter_map_by(
        &snap,
        "rio_scheduler_materialization_view_node_skew_total",
        Some("polarity"),
    );
    assert_eq!(
        polarities.get("node_terminal_job_pending").copied(),
        Some(1),
        "the planted zombie (terminal node + pending job) must fire the skew \
         detector's third polarity — pre-fix the detector had no arm for this \
         face and stayed silent through the whole live_061 window: {polarities:?}"
    );
    Ok(())
}

/// **W12-S9B2** — *the moot sweep is bounded per tick (R17:
/// `MOOT_SWEEP_TICK_BOUND`, violable + testable)*: seeding BOUND+1
/// zero-interest rows resolves exactly BOUND on the first pass and the
/// remainder on the next (level-triggered; view entries leave only on
/// settled dispositions).
#[tokio::test]
async fn moot_sweep_is_bounded_per_tick() -> TestResult {
    use crate::actor::materialize::MOOT_SWEEP_TICK_BOUND;
    let n = MOOT_SWEEP_TICK_BOUND + 1;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let mut actor = bare_actor(db.pool.clone());
    let generation = actor.serving_generation();

    // N pending jobs in ONE batched tx (no derivations rows needed —
    // the moot closer keys on job state alone; the empty DAG routes
    // every row through the node-absent cancelled class).
    let drv_ids: Vec<Uuid> = (0..n).map(|_| Uuid::new_v4()).collect();
    let hashes: Vec<String> = (0..n).map(|i| format!("moot-bound-{i:05}")).collect();
    let rows: Vec<crate::db::materialization::NewJobRow<'_>> = drv_ids
        .iter()
        .zip(&hashes)
        .map(|(drv, hash)| crate::db::materialization::NewJobRow {
            derivation_id: *drv,
            drv_hash: hash,
            tenant_id: None,
            origin: JobOrigin::CacheOpportunity,
            priority: 0.0,
            carried_realized_paths: None,
        })
        .collect();
    let mut tx = db.pool.begin().await?;
    let created = crate::db::SchedulerDb::create_materialization_jobs_in_tx(
        &mut tx,
        &rows,
        generation.as_i64(),
    )
    .await?;
    tx.commit().await?;
    for (hash, jc) in hashes.iter().zip(&created) {
        actor.materialization_jobs.insert(
            DrvHash::from(hash.as_str()),
            crate::actor::materialize::JobViewEntry::test_unclaimed(jc.job_id),
        );
    }

    let authority = actor
        .dag_authority()
        .expect("direct-setup actor is authoritative");
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;
    let after_first: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE state = 'cancelled'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        after_first as usize, MOOT_SWEEP_TICK_BOUND,
        "the first pass resolves exactly the bound"
    );
    assert_eq!(
        actor.materialization_jobs.iter().count(),
        1,
        "the truncated remainder keeps its view entry for the next pass"
    );
    actor
        .tick_cancel_zero_interest_materialization(&authority)
        .await;
    let after_second: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE state = 'cancelled'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        after_second as usize, n,
        "the second pass drains the remainder (level-triggered)"
    );
    assert_eq!(actor.materialization_jobs.iter().count(), 0);
    Ok(())
}

/// Local single-derivation insert (the db/tests helper is `pub(super)`
/// to db::tests — this mirrors it for the actor battery).
async fn insert_test_derivation_local(pool: &sqlx::PgPool, hash: &str) -> anyhow::Result<Uuid> {
    let row = crate::db::DerivationRow {
        drv_hash: hash.into(),
        drv_path: rio_test_support::fixtures::test_drv_path(hash),
        pname: Some("test-pkg".into()),
        system: "x86_64-linux".into(),
        status: DerivationStatus::Created,
        required_features: vec![],
        expected_output_paths: vec![],
        output_names: vec!["out".into()],
        is_fixed_output: false,
        is_ca: false,
    };
    let mut tx = pool.begin().await?;
    let ids = crate::db::SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
    tx.commit().await?;
    Ok(ids.get(hash).expect("just inserted").0)
}

/// bug_110's structural pin: the listing handler reads time ONLY
/// through [`PassClock`] — zero raw `Instant::now()` mints inside the
/// handler region, exactly two `PassClock::arm()` sites (pass start +
/// the beat arm's completion re-arm). A third arm or a raw now() is a
/// second clock in the pass — the merged-half-sweep shape (faac1261e
/// re-pointed prune/members and left the serve filter + contact stamp
/// on the stale clock).
#[test]
fn listing_handler_reads_one_pass_clock() {
    let src = include_str!("../materialize.rs");
    let start = src
        .find("pub(super) async fn handle_list_materialization_jobs(")
        .expect("handler exists");
    // The handler region ends at the next fn item at the same impl depth.
    let end = src[start..]
        .find("\n    /// THE single job-creation helper")
        .expect("the next item's doc anchors the region");
    let handler = &src[start..start + end];
    assert_eq!(
        handler.matches("Instant::now()").count(),
        0,
        "the handler must not mint raw clocks — thread the PassClock"
    );
    assert_eq!(
        handler.matches("PassClock::arm()").count(),
        2,
        "exactly two arms: pass start + the beat completion re-arm"
    );
}

// r[verify sched.materialize.view-settlement]
/// W10-X (bug_120) — proposition: every successful PD-20 conversion
/// drives the disclosure tail with ZERO skew-WARNs. The park tail
/// already released the node (Assigned/Running → Queued|Ready — the
/// park's requeue companion), so the conversion's requeue at the
/// release chokepoint finds the node ALREADY at a released status.
/// Pre-fix `reset_after_attempt` conflated that benign idempotence
/// with genuine state-machine skew: the "invalid state for
/// reassignment, skipping" WARN fired on EVERY successful conversion
/// and the early return skipped `affected.extend` — no
/// `emit_progress` fan-out, so the build's dashboard view stayed
/// stale until the next unrelated event, while the WARN correlated
/// noise into the conversions alert lane.
///
/// Post-fix: already-at-a-released-status is the TYPED
/// `AlreadyReleased` outcome — no WARN (reserved for truly
/// unexpected statuses), and the disclosure tail still fans out (a
/// Progress event lands on the build's state ring after the
/// conversion tick).
#[tracing_test::traced_test]
#[tokio::test]
async fn conversion_requeue_is_a_disclosing_no_op() -> TestResult {
    use rio_proto::types::build_event::Event;
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let (store, store_client, store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |cfg, _| {
            cfg.materialization.max_attempts = 1;
            cfg.materialization.park_backoff_base_secs = 3600;
            cfg.materialization.park_backoff_cap_secs = 3600;
        });
    let _tasks = (store_task, actor_task);

    // Substitutable node → job; claim; infra-fail → PARKED (the park
    // tail releases the node).
    let out = test_store_path("w10x-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    let mut r = make_node("w10x");
    r.expected_output_paths = vec![out.clone()];
    r.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    let mut ev = merge_dag(&handle, b1, vec![r], vec![], false).await?;
    barrier(&handle).await;
    let assignment = match claim_materialization(&handle, "w10x", "store-test-0").await {
        Ok(PullOutcome::Deliver(a)) => *a,
        other => panic!("the claim must deliver, got {other:?}"),
    };
    let exec_id: Uuid = assignment.exec_id.parse()?;
    report_materialization_outcome(&handle, exec_id, "w10x", mat_infra_outcome("dead upstream"))
        .await
        .map_err(|e| anyhow::anyhow!("infra report rejected: {e:?}"))?;
    barrier(&handle).await;

    // Durable truth: the closure is produced + vouched — from-source
    // viable; the next tick converts (PD-20).
    let r_id = pg_derivation_id(&db.pool, "w10x").await?;
    let c_id = insert_pg_derivation(&db.pool, "w10x-child", "completed").await?;
    pg_edge(&db.pool, r_id, c_id).await?;
    pg_link(&db.pool, b1, c_id).await?;

    // Drain the ring so the fan-out assertion sees only post-tick
    // events, and let emit_progress's 250ms I-140 debounce window
    // lapse (the park flow just emitted Progress; the debounce is an
    // orthogonal rate policy — the claim under test is that the
    // chokepoint CALLS the fan-out at all).
    while ev.try_recv().is_ok() {}
    tokio::time::sleep(std::time::Duration::from_millis(300)).await;
    while ev.try_recv().is_ok() {}

    tick(&handle).await?;
    barrier(&handle).await;

    let job_state: String =
        sqlx::query_scalar("SELECT state FROM materialization_jobs WHERE drv_hash = 'w10x'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(job_state, "resolved_from_source", "the conversion fired");

    // (1) zero skew-WARNs: the conversion's requeue found the node
    // already released — benign idempotence, not state-machine skew.
    assert!(
        !logs_contain("invalid state for reassignment"),
        "left (pre-fix): EVERY successful conversion fired the \
         'invalid state for reassignment, skipping' WARN (the park tail \
         had already released the node; the chokepoint conflated benign \
         idempotence with skew) / right: already-released is a typed \
         no-op — the WARN is reserved for truly unexpected statuses"
    );

    // (2) the disclosure tail fires: a Progress event lands on the
    // build's state ring after the conversion tick.
    let mut progressed = false;
    for _ in 0..100 {
        match ev.try_recv() {
            Ok(e) => {
                if matches!(e.event, Some(Event::Progress(_))) {
                    progressed = true;
                    break;
                }
            }
            Err(tokio::sync::broadcast::error::TryRecvError::Empty) => {
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
            Err(_) => break,
        }
    }
    assert!(
        progressed,
        "left (pre-fix): the early return skipped affected.extend — no \
         emit_progress fan-out after the conversion; the build's view \
         stayed stale until the next unrelated event / right: the typed \
         AlreadyReleased outcome still drives the disclosure tail"
    );
    Ok(())
}

// ──────────────────────────────────────────────────────────────────────
// sh-002 row 4 (coalesce-outcomes) red-first batteries
//
// Hazard-M re-derived at 5f1ce214c (the per-CLAUDE.md narrowing record
// — verification grep + its output at the time of writing):
//   rg -n 'ActorCommand::ReportPullOutcome' rio-scheduler/src/actor/tests/
//   → tests/helpers.rs:1197
//     tests/pull.rs:461
//     tests/materialize.rs:599,700,799,908,1043,1871,5376,10625,10645
//   = 11 sites. Every one of them sends ONE report and `.await`s its
//   reply with the actor mailbox otherwise idle, so the
//   REPORT_OUTCOME_FLUSH_DEADLINE select! arm (trigger iv, sh-027 §3;
//   25ms in tests) fires for each — no helper-level Tick-after-send
//   shim is needed.
// ──────────────────────────────────────────────────────────────────────

/// Seed `tags.len()` substitutable single-output nodes (each its own
/// build), drive ONE dispatch-probe Tick to mint every job, claim each
/// and seed its output present, returning `(tag, output_path,
/// exec_id)` per node. One tick keeps the housekeeping interactions
/// out of the cross-node setup.
async fn seed_claimed_mat_jobs(
    handle: &ActorHandle,
    store: &rio_test_support::grpc::MockStore,
    tags: &[&str],
) -> anyhow::Result<Vec<(String, String, Uuid)>> {
    let mut outs = Vec::new();
    for tag in tags {
        let out = test_store_path(&format!("{tag}-out"));
        let mut n = make_node(tag);
        n.expected_output_paths = vec![out.clone()];
        merge_dag(handle, Uuid::new_v4(), vec![n], vec![], false).await?;
        store.state.substitutable.write().unwrap().push(out.clone());
        outs.push((tag.to_string(), out));
    }
    barrier(handle).await;
    tick(handle).await?;
    let mut claimed = Vec::new();
    for (tag, out) in outs {
        let assignment = match claim_materialization(handle, &tag, "store-sh002-0").await {
            Ok(PullOutcome::Deliver(a)) => *a,
            other => anyhow::bail!("the claim for {tag} must deliver, got {other:?}"),
        };
        let exec_id: Uuid = assignment.exec_id.parse()?;
        store.seed_with_content(&out, b"materialized");
        claimed.push((tag, out, exec_id));
    }
    Ok(claimed)
}

// r[verify sched.executor.report-idempotent]
/// sh-002 row-4 arm (e), the v3.1-amendment gate: a SINGLE Success
/// report resolves its reply WITHOUT a Tick — ack latency is decoupled
/// from `tick_interval`. Paused-time so no Tick can fire on its own.
/// This is the property the `tickIntervalSecs=600` materialization VM
/// fixture relies on ("never tick-driven"), and it is what the
/// `REPORT_OUTCOME_FLUSH_DEADLINE` select! arm (trigger iv, sh-027
/// §3) preserves once reports accumulate instead of consuming inline.
#[tokio::test]
async fn sh002_single_report_resolves_without_tick() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut claimed = seed_claimed_mat_jobs(&handle, &store, &["sh002-lone"]).await?;
    let (tag, out, exec_id) = claimed.remove(0);

    // The lone report: sent through the production intake, awaited
    // directly. The actor has no internal Tick timer — Tick is an
    // explicit `ActorCommand::Tick` from `main.rs` and none is sent
    // below — so the reply resolves iff the report's own actor turn
    // flushes it. The
    // bounded timeout converts a hang (the ack deferred to a Tick
    // that never comes) into a clean failure; it is wall-clock
    // because the report's own consumption issues real PG I/O
    // (paused-time auto-advance would race the socket reads and the
    // sqlx pool-acquire timer — observed: PoolTimedOut under
    // start_paused).
    let report = report_materialization_outcome(
        &handle,
        exec_id,
        &tag,
        mat_success_outcome(vec![out.clone()], vec![]),
    );
    tokio::time::timeout(std::time::Duration::from_secs(10), report)
        .await
        .map_err(|_| {
            anyhow::anyhow!(
                "a lone Success report's ack waited for a Tick (the \
                 REPORT_OUTCOME_FLUSH_DEADLINE arm — trigger iv — is missing): \
                 the reply must resolve within 250ms of the report's own actor turn"
            )
        })?
        .map_err(|e| anyhow::anyhow!("Success report rejected: {e:?}"))?;

    assert_eq!(
        expect_drv(&handle, &tag).await.status,
        DerivationStatus::Completed,
        "the lone report's flush ran the batched completion (the node \
         transitioned, not just acked)"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// sh-002 row-4 arm (a): N Success reports queued back-to-back drive
/// ONE `complete_ready_from_store_batch` call, not N per-item ones.
/// RED at 5f1ce214c: each report's `consume_materialization_outcome`
/// calls the batch helper inline with a `len=1` slice — the test
/// counter moves by 3. After the two-level accumulator the flush
/// drains all queued completions into one batched call.
#[tokio::test]
async fn sh002_queued_reports_coalesce_into_one_completion_batch() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let claimed = seed_claimed_mat_jobs(
        &handle,
        &store,
        &["sh002-batch-a", "sh002-batch-b", "sh002-batch-c"],
    )
    .await?;

    let before = handle.debug_counters().await?.complete_ready_batch_calls;

    // Send all three Success reports in one synchronous burst
    // (try_send never yields on a non-full channel, and the test
    // runtime is current-thread): the actor's biased select! drains
    // every queued mailbox command BEFORE the
    // REPORT_OUTCOME_FLUSH_DEADLINE arm (trigger iv, sh-027 §3) is
    // considered, so all three queue then drain into one batched
    // completion when the deadline fires.
    let tx = handle.command_sender();
    let mut rxs: Vec<tokio::sync::oneshot::Receiver<Result<(), PullRejection>>> = Vec::new();
    for (tag, out, exec_id) in &claimed {
        let mut payload = pull_payload(rio_proto::types::BuildResult::default());
        payload.materialization_outcome = Some(mat_success_outcome(vec![out.clone()], vec![]));
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        tx.try_send(ActorCommand::ReportPullOutcome {
            exec_id: *exec_id,
            auth_intent: Some(tag.clone()),
            payload,
            reply: rtx,
        })
        .map_err(|e| anyhow::anyhow!("mailbox try_send: {e}"))?;
        rxs.push(rrx);
    }

    // Ack-after-durable: every reply resolves Ok. Awaiting the FIRST
    // reply is what yields to the actor — by then all three are
    // queued, so the flush that resolves it batched all three.
    for (i, rx) in rxs.into_iter().enumerate() {
        let r = rx.await.map_err(|_| anyhow::anyhow!("reply {i} dropped"))?;
        assert!(r.is_ok(), "report {i} must ack Ok, got {r:?}");
    }
    for (tag, _, _) in &claimed {
        assert_eq!(
            expect_drv(&handle, tag).await.status,
            DerivationStatus::Completed,
            "every coalesced report's node completed"
        );
    }

    let delta = handle.debug_counters().await?.complete_ready_batch_calls - before;
    assert!(
        delta <= 1,
        "RED at base: 3 Success reports drove 3 per-item \
         complete_ready_from_store_batch(len=1) calls — the coalesced \
         flush must batch them into ONE call (got {delta})"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// sh-007c S6 (`report-pg-batch`): N Success reports queued
/// back-to-back drive ONE consumption-path `begin_fenced` transaction,
/// not N per-item ones. RED at base: each report's
/// `consume_materialization_outcome` calls
/// `close_materialization_attempt` (one fenced tx per item) — the
/// counter moves by 8. After the phased flush body the prefetch +
/// batch close+resolve runs ONE fenced tx for the whole batch.
#[tokio::test]
async fn flush_is_o1_pg_per_batch() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let claimed = seed_claimed_mat_jobs(
        &handle,
        &store,
        &[
            "sh007c-a", "sh007c-b", "sh007c-c", "sh007c-d", "sh007c-e", "sh007c-f", "sh007c-g",
            "sh007c-h",
        ],
    )
    .await?;

    let before = handle.debug_counters().await?.begin_fenced_calls;

    // Same coalesce shape as the sh-002 row-4 test: try_send all
    // reports in one synchronous burst so the actor's biased select!
    // drains every queued command before the
    // REPORT_OUTCOME_FLUSH_DEADLINE arm (trigger iv, sh-027 §3) is
    // considered; all eight queue then drain into ONE flush.
    let tx = handle.command_sender();
    let mut rxs: Vec<tokio::sync::oneshot::Receiver<Result<(), PullRejection>>> = Vec::new();
    for (tag, out, exec_id) in &claimed {
        let mut payload = pull_payload(rio_proto::types::BuildResult::default());
        payload.materialization_outcome = Some(mat_success_outcome(vec![out.clone()], vec![]));
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        tx.try_send(ActorCommand::ReportPullOutcome {
            exec_id: *exec_id,
            auth_intent: Some(tag.clone()),
            payload,
            reply: rtx,
        })
        .map_err(|e| anyhow::anyhow!("mailbox try_send: {e}"))?;
        rxs.push(rrx);
    }

    for (i, rx) in rxs.into_iter().enumerate() {
        let r = rx.await.map_err(|_| anyhow::anyhow!("reply {i} dropped"))?;
        assert!(r.is_ok(), "report {i} must ack Ok, got {r:?}");
    }
    for (tag, _, _) in &claimed {
        assert_eq!(
            expect_drv(&handle, tag).await.status,
            DerivationStatus::Completed,
            "every coalesced report's node completed"
        );
    }

    let delta = handle.debug_counters().await?.begin_fenced_calls - before;
    assert_eq!(
        delta, 1,
        "RED at base: 8 Success reports drove 8 per-item \
         close_materialization_attempt fenced transactions — the phased \
         flush must batch close+resolve into ONE fenced tx (got {delta})"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// sh-027 §3 (`s6-batch-tighten`): a 50-report burst coalesces to
/// flush batches with N̄≥20. RED at 59d532ff0 two ways: (a) the
/// `rio_scheduler_pull_outcome_flush_batch_size` histogram does not
/// exist; (b) with the histogram added but the retired mailbox-empty
/// trigger (sh-002 trigger iv) intact, the test harness's serial
/// `query_unchecked().await` per report leaves the mailbox empty after
/// every dequeue → 50 flushes of size 1 (max sample = 1.0, fails the
/// ≥20 gate). After the change: 50 try_sends in one synchronous burst,
/// the biased select! drains every queued command before the
/// `REPORT_OUTCOME_FLUSH_DEADLINE` arm is considered, so all 50 queue
/// then drain into ONE flush (sample = 50.0).
#[tokio::test]
async fn report_burst_coalesces_to_batch_ge_20() -> TestResult {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    // Thread-local: the current_thread runtime runs the actor task on
    // this same OS thread (precedent: parked_job_stalled_gauge_…).
    let _guard = metrics::set_default_local_recorder(&rec);

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tags: Vec<String> = (0..50).map(|i| format!("sh027-burst-{i:02}")).collect();
    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    let claimed = seed_claimed_mat_jobs(&handle, &store, &tag_refs).await?;

    // 50 reports in one synchronous burst (try_send never yields on a
    // non-full channel; the test runtime is current-thread).
    let tx = handle.command_sender();
    let mut rxs: Vec<tokio::sync::oneshot::Receiver<Result<(), PullRejection>>> = Vec::new();
    for (tag, out, exec_id) in &claimed {
        let mut payload = pull_payload(rio_proto::types::BuildResult::default());
        payload.materialization_outcome = Some(mat_success_outcome(vec![out.clone()], vec![]));
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        tx.try_send(ActorCommand::ReportPullOutcome {
            exec_id: *exec_id,
            auth_intent: Some(tag.clone()),
            payload,
            reply: rtx,
        })
        .map_err(|e| anyhow::anyhow!("mailbox try_send: {e}"))?;
        rxs.push(rrx);
    }
    for (i, rx) in rxs.into_iter().enumerate() {
        let r = rx.await.map_err(|_| anyhow::anyhow!("reply {i} dropped"))?;
        assert!(r.is_ok(), "report {i} must ack Ok, got {r:?}");
    }

    // Read the histogram samples — every flush since the recorder
    // installed (the seed_claimed_mat_jobs Tick may have flushed
    // empty/early; filter to nonzero samples).
    let mut samples: Vec<f64> = Vec::new();
    for (ck, _, _, v) in snap.snapshot().into_vec() {
        if ck.key().name() == "rio_scheduler_pull_outcome_flush_batch_size" {
            let DebugValue::Histogram(h) = v else {
                continue;
            };
            samples.extend(h.iter().map(|o| o.into_inner()));
        }
    }
    assert!(
        !samples.is_empty(),
        "RED(a) at 59d532ff0: rio_scheduler_pull_outcome_flush_batch_size does not exist"
    );
    let max = samples.iter().copied().fold(0.0_f64, f64::max);
    assert!(
        max >= 20.0,
        "RED(b) at 59d532ff0: the retired mailbox-empty trigger flushed \
         per-item (max batch size {max}); the deadline arm must coalesce \
         a 50-report burst to ≥20 (sh-027 §3 design target)"
    );
    // Tighter structural bound: the burst itself must be ONE flush of
    // exactly 50 (the biased select! drains every queued command
    // before the deadline arm is even considered, and 50 < BATCH_MAX).
    let burst_flushes: Vec<f64> = samples.iter().copied().filter(|&s| s > 1.0).collect();
    assert_eq!(
        burst_flushes.iter().copied().sum::<f64>(),
        50.0,
        "the 50-report burst must coalesce into flushes summing to 50 \
         (got samples {samples:?})"
    );
    assert!(
        burst_flushes.len() <= 3,
        "at most 1 threshold-flush + 1 deadline-flush + slack \
         (got {burst_flushes:?})"
    );
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// sh-027 §3 (`s6-batch-tighten`, phase-D): N batched Release-arm
/// reports drive ZERO per-item `companion_release` awaits — the
/// phase-D loop collects `DeferredRelease` and runs ONE
/// `companion_release_batch` after. RED at 59d532ff0: each
/// `apply_batched_companion` Release arm `.await`ed
/// `companion_release` inline → counter moves by N. Behaviour pin:
/// every claim is RELEASED (re-claimable) and the deferral stamps
/// (the bug_220 disposition-gated mutation) — the batch is
/// semantically identical to N × per-item, with one requeue
/// chokepoint instead of N.
#[tokio::test]
async fn phase_d_release_is_batched() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tags: Vec<String> = (0..6).map(|i| format!("sh027-rel-{i}")).collect();
    let tag_refs: Vec<&str> = tags.iter().map(String::as_str).collect();
    let claimed = seed_claimed_mat_jobs(&handle, &store, &tag_refs).await?;

    let before = handle.debug_counters().await?.companion_release_awaits;

    let retry_later = rio_proto::types::MaterializationOutcome {
        outcome: Some(
            rio_proto::types::materialization_outcome::Outcome::RetryLater(
                rio_proto::types::materialization_outcome::RetryLater {
                    detail: "upstream rate-limited".into(),
                    retry_after_secs: 60,
                    class: "rate_limited".into(),
                },
            ),
        ),
    };
    let tx = handle.command_sender();
    let mut rxs: Vec<tokio::sync::oneshot::Receiver<Result<(), PullRejection>>> = Vec::new();
    for (tag, _, exec_id) in &claimed {
        let mut payload = pull_payload(rio_proto::types::BuildResult::default());
        payload.materialization_outcome = Some(retry_later.clone());
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        tx.try_send(ActorCommand::ReportPullOutcome {
            exec_id: *exec_id,
            auth_intent: Some(tag.clone()),
            payload,
            reply: rtx,
        })
        .map_err(|e| anyhow::anyhow!("mailbox try_send: {e}"))?;
        rxs.push(rrx);
    }
    for (i, rx) in rxs.into_iter().enumerate() {
        let r = rx.await.map_err(|_| anyhow::anyhow!("reply {i} dropped"))?;
        assert!(r.is_ok(), "report {i} must ack Ok, got {r:?}");
    }

    let delta = handle.debug_counters().await?.companion_release_awaits - before;
    assert_eq!(
        delta, 0,
        "RED at 59d532ff0: 6 RetryLater reports drove 6 per-item \
         companion_release awaits — phase-D must collect DeferredRelease \
         and run ONE companion_release_batch (got {delta})"
    );
    // Behaviour pin: every node requeued to a released status — the
    // batch's `requeue_after_attempt(slice)` ran (the node-reset half;
    // the in-mem `release_claim_deferring` is the other half — pinned
    // independently by the bug_220 batteries).
    for (tag, _, _) in &claimed {
        let st = expect_drv(&handle, tag).await.status;
        assert!(
            matches!(st, DerivationStatus::Ready | DerivationStatus::Queued),
            "{tag}: the batched release must requeue to a released \
             status (got {st:?})"
        );
    }
    Ok(())
}

// r[verify sched.executor.report-idempotent]
/// sh-002 row-4 arm (c): a `LeaderLost` queued behind pending reports
/// drains every held reply with `Err(NotLeader)` — never a silent
/// `clear()`. RED at 5f1ce214c: there is no accumulator; each report
/// runs to completion inline (the unknown-exec ack-and-ignore arm
/// returns `Ok(())`), so the receivers below resolve `Ok` and the
/// `Err(NotLeader)` assertion fails.
#[tokio::test]
async fn sh002_leader_lost_drains_pending_reports_not_leader() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let tx = handle.command_sender();
    let mut rxs: Vec<tokio::sync::oneshot::Receiver<Result<(), PullRejection>>> = Vec::new();
    // Three reports for never-minted exec_ids (the cheap shape — no
    // PG setup needed; at base they hit the report-idempotent
    // unknown-exec arm and ack `Ok(())`). Queued WITH `LeaderLost`
    // behind them in one synchronous burst so every report's actor
    // turn sees a non-empty mailbox.
    for _ in 0..3 {
        let (rtx, rrx) = tokio::sync::oneshot::channel();
        tx.try_send(ActorCommand::ReportPullOutcome {
            exec_id: Uuid::new_v4(),
            auth_intent: None,
            payload: pull_payload(rio_proto::types::BuildResult::default()),
            reply: rtx,
        })
        .map_err(|e| anyhow::anyhow!("try_send: {e}"))?;
        rxs.push(rrx);
    }
    tx.try_send(ActorCommand::LeaderLost)
        .map_err(|e| anyhow::anyhow!("try_send LeaderLost: {e}"))?;
    barrier(&handle).await;

    for (i, rx) in rxs.into_iter().enumerate() {
        let r = rx
            .await
            .map_err(|_| anyhow::anyhow!("reply {i} dropped (a clear() would do this)"))?;
        assert!(
            matches!(r, Err(PullRejection::NotLeader)),
            "pending report {i} must drain Err(NotLeader) on LeaderLost \
             (RED at base: the inline intake already acked Ok), got {r:?}"
        );
    }
    Ok(())
}
