//! Dispatch: skip-ineligible, options propagation, interactive priority.

use super::*;

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Regression guard: without
/// propagation, all-zeros defaults would be sent.
#[tokio::test]
async fn test_build_options_propagated_to_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Submit with build_timeout=300, max_silent_time=60.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("opts-hash")],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 60,
                build_timeout: 300,
                build_cores: 4,
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // The pull payload carries the build's options.
    let assignment = pull_attempt(&handle, "opts-hash").await;
    let opts = assignment.build_options.expect("options should be set");
    assert_eq!(
        opts.build_timeout, 300,
        "build_timeout should propagate from build to worker"
    );
    assert_eq!(
        opts.max_silent_time, 60,
        "max_silent_time should propagate from build to worker"
    );
    assert_eq!(opts.build_cores, 4);
    Ok(())
}

/// The submitter's W3C traceparent is carried through `MergeDagRequest` →
/// stored on `DerivationState` → embedded in `WorkAssignment.traceparent`
/// at dispatch. This gives gateway→scheduler→worker trace continuity
/// despite the span context NOT crossing the mpsc channel to the actor.
///
/// Regression: before this fix, `dispatch.rs` called
/// `current_traceparent()` which read the actor's ORPHAN span (a fresh
/// root), so the worker's span belonged to a disjoint trace.
// r[verify obs.trace.w3c-traceparent]
// r[verify sched.trace.assignment-traceparent]
#[tokio::test]
async fn test_dispatch_carries_submitter_traceparent() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Known traceparent (W3C format: version-trace_id-span_id-flags).
    // The exact bytes don't matter for this test — we just verify it
    // flows through unchanged.
    let known_tp = "00-0af7651916cd43dd8448eb211c80319c-b7ad6b7169203331-01";

    let build_id = Uuid::new_v4();
    let _rx = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("trace-hash")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: known_tp.to_string(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    let assignment = pull_attempt(&handle, "trace-hash").await;
    assert_eq!(
        assignment.traceparent, known_tp,
        "WorkAssignment.traceparent must match the submitter's traceparent, \
         not the actor's orphan span"
    );
    Ok(())
}

/// Dedup: if a second submitter merges an already-present derivation,
/// the FIRST submitter's traceparent is preserved on the state. The
/// worker's span should chain back to whichever build first introduced
/// the derivation (operationally: the trace that will have waited longest).
#[tokio::test]
async fn test_dispatch_traceparent_first_submitter_wins_on_dedup() -> TestResult {
    let (db, handle, _task) = setup().await;

    let tp_first = "00-11111111111111111111111111111111-1111111111111111-01";
    let tp_second = "00-22222222222222222222222222222222-2222222222222222-01";

    // Helper: merge dedup-hash with a given traceparent (defaults otherwise).
    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("dedup-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First submit with tp_first.
    let _ = merge_dag_req(&handle, merge_with_tp(tp_first)).await?;
    // Second submit: SAME derivation, DIFFERENT traceparent (dedup hit).
    let _ = merge_dag_req(&handle, merge_with_tp(tp_second)).await?;

    // Pull the deduped node: the payload carries the stored traceparent.
    let assignment = pull_attempt(&handle, "dedup-hash").await;
    assert_eq!(
        assignment.traceparent, tp_first,
        "first submitter's traceparent should win on dedup (existing state not overwritten)"
    );
    drop(db);
    Ok(())
}

/// Dedup upgrade: if the existing node has traceparent="" (from
/// recovery or poison-reset), a live submitter's traceparent REPLACES
/// it. Recovery isn't a "submitter" — without this, a user's
/// STDERR_NEXT trace_id after failover never finds the worker span.
#[tokio::test]
async fn test_dedup_upgrades_empty_traceparent_from_recovery() -> TestResult {
    let (db, handle, _task) = setup().await;

    let merge_with_tp = |tp: &str| MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: None,
        priority_class: PriorityClass::Scheduled,
        nodes: vec![make_node("upgrade-hash")],
        edges: vec![],
        options: BuildOptions::default(),
        keep_going: false,
        traceparent: tp.to_string(),
        jti: None,
        jwt_token: None,
    };

    // First merge with EMPTY traceparent (simulates recovery:
    // from_recovery_row/from_poisoned_row set traceparent="").
    let _ = merge_dag_req(&handle, merge_with_tp("")).await?;

    // Second merge with a REAL traceparent — dedup hit, should upgrade.
    let live_tp = "00-33333333333333333333333333333333-3333333333333333-01";
    let _ = merge_dag_req(&handle, merge_with_tp(live_tp)).await?;

    let assignment = pull_attempt(&handle, "upgrade-hash").await;
    assert_eq!(
        assignment.traceparent, live_tp,
        "empty traceparent (recovery) should be upgraded by first live submitter"
    );
    drop(db);
    Ok(())
}

// -----------------------------------------------------------------------------
// C1: Leader generation — Arc<AtomicU64>, single-load consistency
// -----------------------------------------------------------------------------

/// The generation starts at 1, not 0. Proto-default is 0; a worker
/// receiving `generation=0` should interpret it as "field unset (old
/// scheduler version)" not "first generation."
///
/// Catches the off-by-one if someone changes `AtomicU64::new(1)` → `new(0)`
/// during a refactor. Without this, the bug would only surface as workers
/// treating EVERY first-leadership assignment as unset/stale.
#[tokio::test]
async fn test_generation_starts_at_one_not_zero() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Not dispatching anything — just reading the reader directly.
    // leader_generation() is the raw (not recovery-gated) value; the
    // heartbeat carries advertised_generation(), which equals it here
    // because the always-leader fixture constructs
    // recovery_complete = true.
    assert_eq!(
        handle.leader_generation(),
        1,
        "gen=0 is proto-default (unset); gen=1 is the real first generation"
    );
    assert_eq!(
        handle.advertised_generation(),
        1,
        "always-leader fixture has recovery complete, so the heartbeat advertises the real \
         first generation, not the 0 unset sentinel"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// PrefetchHint before WorkAssignment
// -----------------------------------------------------------------------------

/// Dispatch pins input-closure paths; terminal unpins.
/// Verifies the end-to-end pin → unpin lifecycle via scheduler_
/// live_pins row count.
// r[verify sched.gc.live-pins]
#[tokio::test]
async fn test_pin_unpin_live_inputs_lifecycle() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Two-node chain: child (leaf, no inputs) + parent (depends
    // on child). Parent's approx_input_closure = child's
    // expected_output_paths. The pull mint of PARENT should pin those.
    //
    // make_test_node defaults expected_output_paths=vec![]; set
    // explicitly so approx_input_closure has something to collect.
    let build_id = Uuid::new_v4();
    let child_out = test_store_path("x9-child-out");
    let mut child = make_node("x9-child");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_node("x9-parent");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![child, parent],
        vec![make_test_edge("x9-parent", "x9-child")],
        false,
    )
    .await?;

    // Child is pullable first (leaf → Ready immediately).
    let assignment_child = pull_attempt(&handle, "x9-child").await;
    assert!(assignment_child.drv_path.contains("x9-child"));

    // Child is leaf → approx_input_closure empty → no pin.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-child'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "leaf drv (no inputs) should not pin anything");

    // Complete child → parent becomes Ready → its pull mint pins.
    pull_complete_success_empty(&handle, "x9-child").await?;
    let assignment_parent = pull_attempt(&handle, "x9-parent").await;
    assert!(assignment_parent.drv_path.contains("x9-parent"));
    barrier(&handle).await;

    // Parent's input-closure = child's expected_output_paths
    // (1 path via make_test_node). Pin should be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 1, "parent pull mint should pin its 1 input path");

    // Complete parent → unpin.
    pull_complete_success_empty(&handle, "x9-parent").await?;
    barrier(&handle).await;

    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "completion should unpin");

    Ok(())
}

// -----------------------------------------------------------------------------
// CA recovery-resolve: fetch ATerm from store when drv_content empty
// -----------------------------------------------------------------------------

/// Build a CA-on-CA fixture: (child_node, parent_node, parent_aterm,
/// placeholder, child_modular_hash, realized_path).
///
/// Parent is floating-CA with one inputDrv = child. Child is
/// floating-CA with `ca_modular_hash` set (so `collect_ca_inputs`
/// picks it up). The placeholder is what `resolve_ca_inputs` will
/// replace with `realized_path` once the child's realisation is in PG.
fn ca_on_ca_fixture() -> (
    rio_proto::types::DerivationNode,
    rio_proto::types::DerivationNode,
    String,
    String,
    [u8; 32],
    String,
) {
    use crate::ca::resolve::downstream_placeholder;
    use rio_nix::store_path::StorePath;

    let child_path = test_drv_path("ca-child");
    let child_modular: [u8; 32] = [0xCA; 32];
    let realized_path = test_store_path("ca-child-realized-out");

    let placeholder = downstream_placeholder(&StorePath::parse(&child_path).unwrap(), "out");

    // Parent's ATerm: floating-CA output ("sha256" algo, empty hash,
    // empty path), one inputDrv = child, placeholder in env.DEP.
    let parent_aterm = format!(
        r#"Derive([("out","","sha256","")],[("{child_path}",["out"])],[],"x86_64-linux","/bin/sh",["-c","build"],[("DEP","{placeholder}"),("out",""),("system","x86_64-linux")])"#
    );

    let mut child = make_node("ca-child");
    child.is_content_addressed = true;
    child.needs_resolve = true;
    child.ca_modular_hash = child_modular.to_vec();
    // expected_output_paths can stay empty — parent's PrefetchHint
    // will be empty and skipped (leaf child → no hint anyway).

    let mut parent = make_node("ca-parent");
    parent.is_content_addressed = true;
    parent.needs_resolve = true;
    parent.drv_content = parent_aterm.clone().into_bytes();

    (
        child,
        parent,
        parent_aterm,
        placeholder,
        child_modular,
        realized_path,
    )
}

// -----------------------------------------------------------------------------
// maybe_resolve_ca gate-path passthrough coverage
// -----------------------------------------------------------------------------

// r[verify sched.ca.resolve+3]
/// IA passthrough: `state.ca.needs_resolve = false` → gate at
/// dispatch.rs:681 fails → `drv_content` returned unchanged. No
/// resolve fires, no ContentLookup, no PG query. The cheapest path —
/// every IA-with-IA-inputs dispatch takes it.
#[tokio::test]
async fn maybe_resolve_ca_ia_derivation_passthrough() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let original_content = b"dummy-ia-aterm-content".to_vec();
    let mut node = make_node("ia-drv");
    node.is_content_addressed = false; // explicit: IA
    node.needs_resolve = false; // explicit: no CA inputs either
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = pull_attempt(&handle, "ia-drv").await;

    assert_eq!(
        asgn.drv_content, original_content,
        "IA derivation → maybe_resolve_ca passthrough; drv_content unchanged"
    );
    Ok(())
}

// r[verify sched.dispatch.fod-substitute+2]
/// Dispatch-time substitution: a Ready IA derivation (FOD or non-FOD)
/// whose output becomes substitutable AFTER merge (so merge-time
/// `check_cached_outputs` missed it) is completed by
/// `batch_probe_cached_ready` without dispatching to a worker.
///
/// Pre-fix: the batch was FOD-only AND read only `missing_paths` (no
/// service-token, ignored `substitutable_paths`) → non-FODs relied on
/// merge-time `check_available` which truncates at 4096 → an 18k-drv
/// build's IA cache-hits dispatched to builders.
#[rstest::rstest]
#[case::fod(true)]
#[case::non_fod(false)]
#[tokio::test]
async fn dispatch_time_substitutable_completes(#[case] is_fod: bool) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let out = test_store_path("dispatch-sub-out");
    let mut n = make_node("dispatch-sub-drv");
    n.is_fixed_output = is_fod;
    n.system = "aarch64-linux".into();
    n.expected_output_paths = vec![out.clone()];
    let drv_path = n.drv_path.clone();
    let build_id = Uuid::new_v4();
    let mut ev_rx = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    // Merge-time saw nothing (substitutable not yet seeded) → node
    // stays Ready, stamped probed_generation=1. Seed; the next Tick
    // advances probe_generation and re-runs the ready-set sweep → the
    // batch probe sees it → spawns the substitute fetch.
    store.state.substitutable.write().unwrap().push(out.clone());
    tick(&handle).await?;
    settle_substituting(&handle, &[&make_node("dispatch-sub-drv").drv_hash]).await;
    tick(&handle).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "is_fod={is_fod}: should complete via dispatch-time substitution probe"
    );
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&out),
        "dispatch-time eager-fetch must call QueryPathInfo for the substitutable path; \
         qpi_calls={qpi:?}"
    );

    // Gateway visibility: both Substituting and Cached events must
    // reach interested builds (order is structurally guaranteed by
    // the actor — emit-after-transition + mailbox-ordered
    // SubstituteComplete — so this asserts presence only).
    use rio_proto::types::{DerivationEventKind, build_event::Event};
    let (mut got_substituting, mut got_cached) = (None, None);
    while let Ok(be) = ev_rx.try_recv() {
        if let Some(Event::Derivation(d)) = be.event
            && d.derivation_path == drv_path
        {
            match d.kind() {
                DerivationEventKind::Substituting => got_substituting = Some(d.clone()),
                DerivationEventKind::Cached => got_cached = Some(d.clone()),
                _ => {}
            }
        }
    }
    let s = got_substituting.expect("Substituting event must be emitted to interested builds");
    assert_eq!(s.output_paths, vec![out.clone()], "carries output paths");
    assert!(
        got_cached.is_some(),
        "Cached event (existing) must still arrive after substitution completes"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
/// `batch_probe_cached_ready` × wanted outputs: a Ready node whose only
/// missing output is one nothing wants must be completed inline by the
/// dispatch-time batch probe instead of staying Ready forever / being
/// dispatched to a builder. The node is aarch64 with only an x86_64
/// worker connected so it can never dispatch — with the all-declared
/// criterion it would sit Ready until the heat death of the universe
/// because P_debug is missing and unsubstitutable.
#[tokio::test]
async fn batch_probe_completes_on_missing_unwanted_output() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("bpw-out");
    let dbg = test_store_path("bpw-debug");
    let mut n = make_node("bpw-drv");
    n.system = "aarch64-linux".into();
    n.output_names = vec!["out".into(), "debug".into()];
    n.expected_output_paths = vec![out.clone(), dbg.clone()];
    n.wanted_output_names = vec!["out".into()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "bpw-drv").await.status,
        DerivationStatus::Ready,
        "precondition: nothing in store at merge time → Ready"
    );

    // P_out appears in the store AFTER merge (another build uploaded
    // it). P_debug stays missing and unsubstitutable. The next
    // The next Tick's ready-set sweep batch-probes the Ready node.
    store.seed_with_content(&out, b"out");
    tick(&handle).await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "bpw-drv").await.status,
        DerivationStatus::Completed,
        "all WANTED outputs present → completed inline by the batch \
         probe; the missing unwanted P_debug must not keep it Ready"
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Succeeded as i32);
    Ok(())
}

/// D16 / sched.evidence.settlement: a marked node with Broken closure
/// evidence (childless), a spent substitute_tried one-shot, live build
/// interest, and all live-wanted outputs PRESENT in the store must be
/// settled by the dispatch sweep (completed with its closure
/// re-verified, or fail-fasted) — not left Ready forever while
/// admit_pull refuses to mint for it.
///
/// As built this is the probe partition's no-action cell
/// (locally_present requires !substitute_tried; nothing else fires for
/// a present node), so the node sits Ready and the build hangs Active:
/// the limbo the settlement rule forbids.
// r[verify sched.evidence.settlement]
#[tokio::test]
async fn marked_broken_tried_present_node_settles() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("d16-out");
    // Nothing present/substitutable at merge time: the single-node
    // submission stays Ready for from-source dispatch, unmarked.
    let mut d1 = make_node("d16-d1");
    d1.expected_output_paths = vec![out.clone()];
    d1.wanted_output_names = vec!["out".into()];
    let build = Uuid::new_v4();
    // Hold the event receiver: the cfg(test) orphan-watcher grace is
    // ZERO, so a dropped receiver + >=2 Ticks auto-cancels the build
    // (housekeeping.rs ORPHAN_BUILD_GRACE caution).
    let _ev = merge_dag(&handle, build, vec![d1], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "d16-d1").await.status,
        DerivationStatus::Ready,
        "precondition: nothing available -> Ready"
    );

    // Assemble the D16 cell directly (the production assembly path is
    // >= 16 steps; the debug forcers pin the cell, production
    // reachability is owned by the model's canReachD16Cell witness):
    //   marked (childless => evidence Broken => must_substitute)
    //   + substitute_tried + Ready + live interest.
    assert!(handle.debug_set_topdown_pruned("d16-d1", true).await?);
    assert!(handle.debug_set_substitute_tried("d16-d1", true).await?);

    // Late ingest: the wanted output is now PRESENT.
    store.seed_with_content(&out, b"d16-out-content");

    // Drive the dispatch sweep. Pre-fix: the probe's all-present cell
    // skips tried nodes; nothing else touches it.
    tick(&handle).await?;
    tick(&handle).await?;
    settle_substituting(&handle, &["d16-d1"]).await; // post-fix: the
    // settlement spawns a verification walk; wait for it to land.
    tick(&handle).await?;

    let info = handle
        .debug_query_derivation("d16-d1")
        .await?
        .expect("exists");
    assert_ne!(
        info.status,
        DerivationStatus::Ready,
        "D16 limbo: marked+Broken+tried+present node was left Ready by the \
         dispatch sweep (and admit_pull refuses it) — the settlement rule is violated"
    );
    // The full settlement: present closure verified -> Completed, build done.
    assert_eq!(info.status, DerivationStatus::Completed);
    let st = query_status(&handle, build).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the interested build must complete once the node settles"
    );
    Ok(())
}

/// C3 / sched.merge.substitute-topdown: a stale ok=false walk verdict
/// consumed at the topdown fail-fast must not terminally fail a build
/// whose own (live effective) wanted outputs are PRESENT in the store —
/// the two-build dedup variant (the model's faithful C3 trace).
///
/// d1 declares two outputs: "out" (build B's narrow want; substitutable
/// at merge time, PRESENT later) and "wide" (build A's extra want;
/// never available). Build B's narrow pruned merge keeps {d1} (stamped,
/// childless) and spawns d1's walk (parked on the QPI gate). Build A's
/// wide full merge re-attaches d2 under d1 (its wide want blocks the
/// prune). d1's narrow output then becomes PRESENT (late ingest). A is
/// cancelled and reaped: d2 (sole-interest of A) is removed, d1 is
/// closure-holed -> evidence Broken. The parked walk's stale ok=false
/// verdict then arrives.
///
/// As built, handle_substitute_complete's Broken arm consumes the
/// verdict with NO presence re-check and fail-fasts every interested
/// build — wrongfully terminally failing build B although its wanted
/// output is present (a walk verdict is stale by the walk's own
/// duration). The settlement must re-probe the live effective wanted
/// set first and route the obtainable node to a verification walk.
// r[verify sched.merge.substitute-topdown+12]
// r[verify sched.evidence.settlement]
#[tokio::test]
async fn stale_walk_failure_does_not_fail_build_with_present_outputs() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park the detached fetch: QueryPathInfo waits on the gate, so d1
    // stays Substituting through the staging sequence and the injected
    // (stale) SubstituteComplete below is accepted.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // d1's narrow output: substitutable upstream at merge time (so
    // build B's prune fires and the walk spawns), PRESENT later.
    // The wide output is never available anywhere.
    let out = test_store_path("c3-d1-out");
    let wide = test_store_path("c3-d1-wide");
    store.state.substitutable.write().unwrap().push(out.clone());

    let mk_d1 = |wanted: Vec<String>| {
        let mut n = make_node("c3-d1");
        n.output_names = vec!["out".into(), "wide".into()];
        n.expected_output_paths = vec![out.clone(), wide.clone()];
        n.wanted_output_names = wanted;
        n
    };
    let mk_d2 = || {
        let mut n = make_node("c3-d2");
        n.expected_output_paths = vec![test_store_path("c3-d2-out")];
        n
    };

    // Build B (narrow): {d1 -> d2} wanting only d1's "out", which is
    // available upstream -> the topdown prune fires: keeps {d1}
    // (stamped, childless), drops d2, classifies d1 pending-substitute
    // -> the detached walk spawns and parks on the QPI gate.
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(
        &handle,
        build_b,
        vec![mk_d1(vec!["out".into()]), mk_d2()],
        vec![make_test_edge("c3-d1", "c3-d2")],
        false,
    )
    .await?;
    barrier(&handle).await;
    let d1 = expect_drv(&handle, "c3-d1").await;
    assert!(
        d1.topdown_pruned,
        "precondition: B's pruned merge stamps d1"
    );
    assert_eq!(
        d1.status,
        DerivationStatus::Substituting,
        "precondition: d1's detached fetch is parked on the QPI gate"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.total_derivations,
        1,
        "precondition: B took the roots-only prune path"
    );

    // Build A (wide): the duplicate submission {d1 -> d2}, wanting both
    // of d1's outputs. The wide want is missing and not substitutable,
    // so the prune cannot fire -> full merge: d2 enters the DAG, the
    // (d1, d2) edge is created, and the in-flight Substituting d1 is
    // left alone. A registers interest in both.
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag(
        &handle,
        build_a,
        vec![mk_d1(vec!["out".into(), "wide".into()]), mk_d2()],
        vec![make_test_edge("c3-d1", "c3-d2")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("c3-d2").await?.is_some(),
        "precondition: A's full merge brings d2 into the DAG"
    );

    // Late ingest: d1's narrow output becomes locally PRESENT (another
    // build/tenant uploaded it during the walk).
    store.seed_with_content(&out, b"c3-d1-out-content");

    // Cancel A and reap its sole-interest nodes: d2 is removed, d1
    // (shared with B) survives — childless and closure-holed.
    cancel_build(&handle, build_a).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build_a })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("c3-d2").await?.is_none(),
        "precondition: A's sole-interest d2 must be reaped"
    );
    let d1 = expect_drv(&handle, "c3-d1").await;
    assert!(
        d1.closure_hole,
        "precondition: the reap stamps the closure hole on the survivor"
    );
    assert_eq!(
        d1.status,
        DerivationStatus::Substituting,
        "precondition: the in-flight walk keeps d1 Substituting through the reap"
    );

    // Disarm the QPI gate so the (post-fix) settlement's verification
    // walk can proceed; the original parked walk stays parked (its
    // verdict is the injected one below).
    store
        .faults
        .query_path_info_gate_armed
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // The stale verdict: ok=false, fixed at the walk's own check time —
    // before "out" became present.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "c3-d1".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    // THE RED ASSERTION (C3): build B's wanted output IS present; the
    // stale verdict must not terminally fail it.
    let st = query_status(&handle, build_b).await?;
    assert_ne!(
        st.state,
        rio_proto::types::BuildState::Failed as i32,
        "C3 wrongful terminal failure: the stale ok=false walk verdict was \
         consumed at the topdown fail-fast with no presence re-check — build \
         B's wanted output is present in the store"
    );

    // Post-fix: the settlement re-probes the live effective wanted set,
    // sees the output present, and spawns the verification walk; wait
    // for it to land and assert the full settlement.
    settle_substituting(&handle, &["c3-d1"]).await;
    tick(&handle).await?;
    assert_eq!(
        expect_drv(&handle, "c3-d1").await.status,
        DerivationStatus::Completed,
        "settlement: present wanted output -> closure re-verified -> Completed"
    );
    let st = query_status(&handle, build_b).await?;
    assert_eq!(
        st.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build B must succeed once d1 settles"
    );
    Ok(())
}

/// I-163 Fix 3: `cluster_snapshot_cached()` reads the watch-channel
/// value the actor publishes on `Tick` — no mailbox round-trip. The
/// `fn` (not `async fn`) signature is the structural proof; this test
/// verifies the value is wired (Tick → publish → handle reads it) and
/// is stale-until-Tick (merge alone doesn't update it).
// r[verify sched.admin.snapshot-cached]
#[tokio::test]
async fn cluster_snapshot_cached_reflects_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "i163-snap",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // No Tick yet → watch holds the Default snapshot (all zeros).
    // The merged (Ready) node exists in actor state, but the cached
    // snapshot doesn't see it until Tick publishes.
    let pre = handle.cluster_snapshot_cached();
    assert_eq!(
        pre.queued_derivations, 0,
        "cached snapshot is Tick-published, not live; pre-Tick must be Default"
    );

    // Open the node's pull attempt so the post-Tick snapshot has a
    // running derivation (= one busy executor) to reflect.
    let _assignment = pull_attempt(&handle, "i163-snap").await;
    tick(&handle).await?;

    let post = handle.cluster_snapshot_cached();
    assert_eq!(
        post.total_executors, 1,
        "one open attempt = one busy executor"
    );
    assert_eq!(post.active_executors, 1);
    assert_eq!(post.draining_executors, 0);
    assert_eq!(post.running_derivations, 1);
    assert_eq!(
        post.queued_derivations, 0,
        "the pulled drv is no longer Ready"
    );
    Ok(())
}

// r[verify sched.sla.reactive-floor+3]
/// D4: `InfrastructureFailure(CgroupOom)` doubles
/// `resource_floor.mem_bytes` for both FOD and non-FOD (D2: same
/// reactive path). The floor feeds `solve_intent_for`'s mem clamp →
/// next SpawnIntent is at least the doubled value.
///
/// (TransientFailure does NOT bump — that's a build-determinism
/// signal. CgroupOom is the worker-reported sizing signal.)
#[rstest::rstest]
#[case::fod(rio_proto::types::ExecutorKind::Fetcher, true, "oom-fod")]
#[case::builder(rio_proto::types::ExecutorKind::Builder, false, "glibc-177")]
#[tokio::test]
async fn cgroup_oom_doubles_mem_floor(
    #[case] kind: rio_proto::types::ExecutorKind,
    #[case] is_fod: bool,
    #[case] tag: &str,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        // Zero backoff so the retry redispatches on the next Tick.
        c.retry_policy = crate::RetryPolicy {
            backoff_base_secs: 0.0,
            ..Default::default()
        };
        // 256 GiB ceiling so the 2GiB→4GiB double isn't clamped.
        c.sla = test_sla_config();
    });

    // Delivery is pull; the executor kind no longer routes anything.
    let _ = kind;

    let mut node = make_node(tag);
    node.is_fixed_output = is_fod;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    barrier(&handle).await;
    let first_asgn = pull_attempt(&handle, tag).await;
    assert!(first_asgn.drv_path.contains(tag));

    // Seed est_memory_bytes so the doubling has a base.
    handle
        .debug_seed_sched_hint(tag, Some(2 << 30), None, None, None)
        .await?;

    // Worker-reported CgroupOom → floor.mem doubled.
    pull_complete_failure(
        &handle,
        tag,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        &format!("{}; bumping resource floor", rio_proto::CGROUP_OOM_MSG),
    )
    .await?;
    tick(&handle).await?;

    assert_eq!(
        expect_drv(&handle, tag)
            .await
            .sched
            .resource_floor
            .mem_bytes,
        4 << 30,
        "CgroupOom → mem floor doubled (2GiB→4GiB)"
    );
    Ok(())
}

// ─── ADR-023 SlaEstimator → SpawnIntent ────────────────────────────────

/// `compute_spawn_intents` skips Ready nodes with `probed_generation==0`.
///
/// Pre-fix: `handle_substitute_complete{ok=true}` promotes dependents
/// Queued→Ready then defers their substitute-probe to the next Tick
/// (`dispatch_dirty=true`). A `GetSpawnIntents` poll in that ≤1s window
/// emitted intents → controller spawned Jobs → next Tick probed →
/// dependents went Substituting→Completed → `reap_stale_for_intents`
/// deleted the Pending Jobs 10s later. Fresh-cluster substitution
/// cascades hit this once per DAG layer.
///
/// `queued_by_system` is intentionally NOT gated — it must match
/// `ClusterSnapshot.queued_by_system` (snapshot.rs).
// r[verify sched.admin.spawn-intents.probed-gate+2]
#[tokio::test]
async fn spawn_intents_excludes_unprobed_ready() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig {
            sla: test_sla_config(),
            ..Default::default()
        },
        DagActorPlumbing {
            store_client: Some(store_client),
            ..Default::default()
        },
    );

    // `probeable`: has expected_output_paths → gate applies.
    // `unprobeable`: empty expected_output_paths (floating-CA / VM-test
    // SubmitBuild shape) → batch_probe_cached_ready never stamps it →
    // gate must NOT apply or it dead-locks at zero intents.
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        expected_output_paths: vec![test_store_path("probeable-out")],
        ..crate::db::RecoveryDerivationRow::test_default("probeable", "x86_64-linux")
    });
    actor.test_inject_ready("unprobeable", None, "x86_64-linux", false);
    actor.test_set_probed_generation("probeable", 0);
    actor.test_set_probed_generation("unprobeable", 0);

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        1,
        "probeable+gen=0 gated; unprobeable+gen=0 emits (gate exempt)"
    );
    assert_eq!(snap.intents[0].intent_id, "unprobeable");
    assert_eq!(
        snap.queued_by_system.get("x86_64-linux"),
        Some(&2),
        "queued_by_system counts both (matches ClusterSnapshot, gate-independent)"
    );

    actor.test_set_probed_generation("probeable", 1);
    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(snap.intents.len(), 2, "post-probe: both emit SpawnIntent");

    Ok(())
}

/// `handle_substitute_complete{ok=true}` calls `dispatch_ready` inline
/// (under [`BECAME_IDLE_INLINE_CAP`](crate::actor::BECAME_IDLE_INLINE_CAP))
/// so cascade-promoted dependents are probed in the same handler, not
/// deferred to the next Tick.
///
/// Pre-fix: only `dispatch_dirty=true` → dependent sat Ready+unprobed
/// for ≤1s. Combined with `r[sched.admin.spawn-intents.probed-gate]`,
/// that's correct but adds 1 Tick latency per cascade layer; this
/// keeps cold-start cascades tight.
// r[verify sched.dispatch.substitute-complete-inline+2]
#[tokio::test]
async fn substitute_complete_inline_probes_dependents() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let dep_out = test_store_path("sub-chain-dep-out");
    let tgt_out = test_store_path("sub-chain-tgt-out");
    let mut dep = make_node("sub-chain-dep");
    dep.expected_output_paths = vec![dep_out.clone()];
    let mut tgt = make_node("sub-chain-tgt");
    tgt.expected_output_paths = vec![tgt_out.clone()];

    // Seed ONLY dep so merge-time `check_cached_outputs` (which probes
    // Queued nodes too) misses tgt. Arm the QPI gate so dep's detached
    // fetch PARKS — we control SubstituteComplete arrival explicitly.
    store.state.substitutable.write().unwrap().push(dep_out);
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![dep, tgt],
        vec![make_test_edge("sub-chain-tgt", "sub-chain-dep")],
        false,
    )
    .await?;
    wait_for_status(&handle, "sub-chain-dep", DerivationStatus::Substituting).await;
    assert_eq!(
        expect_drv(&handle, "sub-chain-tgt").await.status,
        DerivationStatus::Queued,
        "precondition: tgt waits on dep"
    );

    // Seed tgt; explicitly post SubstituteComplete{dep,ok=true}. The
    // parked real fetch will later post a duplicate that the
    // `status!=Substituting` guard drops.
    store.state.substitutable.write().unwrap().push(tgt_out);
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "sub-chain-dep".into(),
            ok: true,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    let tgt_status = expect_drv(&handle, "sub-chain-tgt").await.status;
    assert_ne!(
        tgt_status,
        DerivationStatus::Ready,
        "SubstituteComplete must inline-probe promoted dependents \
         (got Ready ⇒ only dispatch_dirty=true, deferred to next Tick)"
    );
    Ok(())
}

/// `compute_spawn_intents` emits one SpawnIntent per Ready derivation,
/// with `intent_id == drv_hash` and `cores ≈ solve_tier(c_star)` for a
/// fitted key. Unfitted keys (no SlaEstimator entry) get probe defaults.
// r[verify sched.sla.intent-from-solve]
#[tokio::test]
async fn spawn_intent_from_sla_estimator() {
    use crate::sla::{solve, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // Seed a single tier so the test exercises the Feasible path the way
    // a configured deploy would (empty ladder → solve_tier BestEffort at
    // p̄ capped at max_cores).
    actor.sla_tiers = vec![solve::Tier {
        name: "normal".into(),
        p50: None,
        p90: Some(1200.0),
        p99: None,
    }];

    // Seed a fit for ("test-pkg", x86_64-linux, ""): Amdahl s=30 p=2000.
    // Against p90=1200, β=30-e^{-0.128}·1200≈-1026 → c*=2000/1026≈1.95
    // → ceil → 2 cores.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        // Asymptotic-n so z_q ≈ Φ⁻¹(0.9)=1.2816 and the closed-form
        // β math in the comment above holds; this test is about
        // SpawnIntent wiring, not small-n widening.
        n_eff_ring: RingNEff(1e6),
        fit_df: FitDf(1e6),
        n_distinct_c: 1_000_000,
        sum_w: 1e6,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });

    // "fitted" matches the seeded key; "cold" has no fit (different
    // pname). Both Ready, both non-FOD. test_inject_ready uses the
    // first arg verbatim as drv_hash.
    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());
    assert_eq!(
        snap.intents.len(),
        2,
        "one SpawnIntent per Ready derivation"
    );
    assert_eq!(snap.queued_by_system.get("x86_64-linux"), Some(&2));

    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .expect("intent_id == drv_hash");
    assert_eq!(
        fitted.cores, 2,
        "solve_tier c_star ≈ 1.95 → ceil 2 (got {})",
        fitted.cores
    );
    assert_eq!(fitted.disk_bytes, 10 << 30, "disk_p90 from fit");
    assert!(fitted.mem_bytes >= 6 << 30, "mem ≥ p90 (× headroom)");

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    // No SlaEstimator entry + no [sla] config → fallback probe (4c, 8Gi).
    assert_eq!(cold.cores, 4, "no fit → fallback probe cores");
    assert_eq!(cold.mem_bytes, 8 << 30);
}

/// `r[sched.sla.hw-class.ice-mask]`: `unfulfillable_cells` from the
/// controller marks the cell ICE; the read-time mask drops it from
/// `node_affinity` WITHOUT re-solving (memo unchanged); a successful
/// spawn ack clears the cell and the next dispatch sees it again.
///
/// R5B2: `_hw_ladder_exhausted_total` is gated on the ICE-edge
/// (`MemoEntry.ice_exhausted` rising), NOT on `was_miss`. ICE state is
/// read-time — explicitly NOT in `inputs_gen` — so `was_miss` is the
/// wrong granularity. Masking happens AFTER poll 1 here so `was_miss`
/// (true on poll 1) and all-masked (poll 4) are decoupled; the metric
/// must fire exactly once, on poll 4, and NOT on poll 5 (still
/// exhausted, no edge).
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn ice_mask_is_read_time() {
    use crate::sla::config::CapacityType;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    const LADDER: &str = "rio_scheduler_sla_hw_ladder_exhausted_total";
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);
    let ladder = |s: &metrics_util::debugging::Snapshotter| -> u64 {
        s.snapshot()
            .into_vec()
            .into_iter()
            .filter(|(ck, ..)| ck.key().name() == LADDER)
            .map(|(.., v)| match v {
                DebugValue::Counter(c) => c,
                _ => 0,
            })
            .sum()
    };

    // ── poll 1: was_miss=true, ICE clear → no ladder emit ──────────────
    // Precondition: solve_full fires, affinity over A' populated.
    let (hw, cost, g0) = actor.solve_inputs();
    let intent = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert!(
        !intent.node_affinity.is_empty(),
        "precondition: solve_full path active"
    );
    let n0 = intent.node_affinity.len();
    assert_eq!(
        ladder(&snap),
        0,
        "poll 1: was_miss=true but ICE clear → ladder MUST NOT fire \
         (was_miss is the wrong gate)"
    );

    // Mask one cell from A' via the controller's unfulfillable report.
    let masked: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);
    actor.handle_ack_spawned_intents(&[], &["intel-6:spot".into()], &[], &[], &[]);
    assert!(actor.ice.is_masked(&masked));

    // ── poll 2: read-time mask, A\{masked}, not exhausted ──────────────
    let intent2 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(actor.solve_inputs().2, g0, "ICE state is NOT in inputs_gen");
    let intel6_spot = |t: &rio_proto::types::NodeSelectorTerm| {
        t.match_expressions
            .iter()
            .any(|r| r.key == "rio.build/hw-class" && r.values == ["intel-6"])
            && t.match_expressions
                .iter()
                .any(|r| r.key == "karpenter.sh/capacity-type" && r.values == ["spot"])
    };
    if intent.node_affinity.iter().any(intel6_spot) {
        assert!(
            !intent2.node_affinity.iter().any(intel6_spot),
            "masked cell dropped from affinity"
        );
        assert_eq!(intent2.node_affinity.len(), n0 - 1);
    }
    assert_eq!(ladder(&snap), 0, "poll 2: partial mask → no ladder");

    // ── poll 3: clear → full A' returns from memo ──────────────────────
    actor.ice.clear(&masked);
    let intent3 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(
        intent3.node_affinity.len(),
        n0,
        "memo never overwritten — unmask restores full A'"
    );
    assert_eq!(ladder(&snap), 0, "poll 3: clear → no ladder");

    // ── poll 4: all-of-H masked → §Capacity backoff exit (b): falls
    // back to A (not empty affinity) and emits ladder_exhausted on the
    // RISING edge. `was_miss=false` here (memo hit at g0); the
    // `was_miss && exhausted` gate would have been silent.
    for h in actor.sla_config.hw_classes.keys() {
        for cap in CapacityType::ALL {
            actor.ice.mark(&(h.clone(), cap));
        }
    }
    let intent4 = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert!(
        !intent4.node_affinity.is_empty(),
        "all-masked → still emit A (best-effort reserved for envelope-infeasibility)"
    );
    assert_eq!(
        ladder(&snap),
        1,
        "poll 4: ICE-edge `false→true` → ladder fires exactly once \
         (bug_006: `was_miss`-gated would be 0 here since miss was poll 1)"
    );

    // ── poll 5: still all-masked → NO re-emit (no edge) ────────────────
    let _ = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    assert_eq!(
        ladder(&snap),
        0,
        "poll 5: still exhausted, prev=true → no edge → no re-emit"
    );
}

/// Regression: `ice.clear()` was wired to the Pending ack (`spawned`),
/// not the success edge (`registered_cells`). The all-masked fallback
/// re-emits the masked cell at `node_affinity[0]`, so each tick did
/// `clear(C)` then `mark(C)` — `step` never climbed past 0, defeating
/// backoff doubling. Now `spawned` is informational-only; only
/// `registered_cells` (or first heartbeat) clears.
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn ice_step_doubles_across_mark_without_clear() {
    use crate::sla::config::CapacityType;
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    let cell: crate::sla::config::Cell = ("intel-6".into(), CapacityType::Spot);

    // The controller's ack echoes back the intent it spawned a Job
    // for. Under the all-masked fallback, `node_affinity[0]` IS the
    // masked cell — exactly what the old `term_to_cell` clear-loop
    // would have parsed back out and cleared.
    let spawned = SpawnIntent {
        node_affinity: vec![NodeSelectorTerm {
            match_expressions: vec![
                NodeSelectorRequirement {
                    key: "rio.build/hw-class".into(),
                    operator: "In".into(),
                    values: vec!["intel-6".into()],
                },
                NodeSelectorRequirement {
                    key: "karpenter.sh/capacity-type".into(),
                    operator: "In".into(),
                    values: vec!["spot".into()],
                },
            ],
        }],
        ..Default::default()
    };

    // Three ticks of {spawned for cell, unfulfillable=cell, no
    // Registered signal}: step must climb 0→1→2. Old code: each tick
    // cleared (from `spawned`) before marking → step stuck at 0.
    for _ in 0..3 {
        actor.handle_ack_spawned_intents(
            std::slice::from_ref(&spawned),
            &["intel-6:spot".into()],
            &[],
            &[],
            &[],
        );
    }
    assert_eq!(
        actor.ice.step(&cell),
        Some(2),
        "spawned-ack must NOT clear; backoff doubles across consecutive marks"
    );

    // `registered_cells` IS the success signal → resets.
    actor.handle_ack_spawned_intents(&[], &[], &["intel-6:spot".into()], &[], &[]);
    assert_eq!(actor.ice.step(&cell), None, "registered_cells clears");
}

/// The pull-mint ICE clear keeps the bug_030 discipline now that the
/// heartbeat edge is gone: a successful pull is the success signal for
/// its intent, but a pod's `nodeAffinity` is an OR over A' — for
/// `|A'|>1` the pull identifies no single cell, so nothing is cleared
/// (over-clearing `cells[0]` would defeat `ice_step_doubles`); for
/// `|A'|==1` the single armed cell is cleared. `registered_cells`
/// (A18) stays the per-cell signal either way.
// r[verify sched.sla.hw-class.ice-mask]
#[tokio::test]
async fn pull_mint_ice_clear_only_at_single_cell() -> TestResult {
    use rio_proto::types::{NodeSelectorRequirement, NodeSelectorTerm, SpawnIntent};

    let term = |h: &str| NodeSelectorTerm {
        match_expressions: vec![
            NodeSelectorRequirement {
                key: "rio.build/hw-class".into(),
                operator: "In".into(),
                values: vec![h.into()],
            },
            NodeSelectorRequirement {
                key: "karpenter.sh/capacity-type".into(),
                operator: "In".into(),
                values: vec!["spot".into()],
            },
        ],
    };
    let ack =
        |intent_id: &str, hw: &[&str], unfulfillable: &[&str]| ActorCommand::AckSpawnedIntents {
            spawned: vec![SpawnIntent {
                intent_id: intent_id.into(),
                hw_class_names: hw.iter().map(|h| h.to_string()).collect(),
                node_affinity: hw.iter().map(|h| term(h)).collect(),
                ..Default::default()
            }],
            unfulfillable_cells: unfulfillable.iter().map(|c| c.to_string()).collect(),
            registered_cells: vec![],
            observed_instance_types: vec![],
            bound_intents: vec![],
        };
    let masked = |snap: &crate::actor::SpawnIntentsSnapshot, cell: &str| {
        snap.ice_masked_cells.iter().any(|c| c == cell)
    };
    async fn snapshot(handle: &ActorHandle) -> crate::actor::SpawnIntentsSnapshot {
        handle
            .query_unchecked(|reply| {
                ActorCommand::Admin(AdminQuery::GetSpawnIntents {
                    req: crate::actor::SpawnIntentsRequest::default(),
                    reply,
                })
            })
            .await
            .expect("actor alive")
    }

    let (_db, handle, _task) = setup().await;

    // |A'| = 2: arm both cells for "ice-multi" and mark h0 ICE.
    let _ev1 = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ice-multi",
        PriorityClass::Scheduled,
    )
    .await?;
    handle
        .send_unchecked(ack("ice-multi", &["h0", "h1"], &["h0:spot"]))
        .await?;
    barrier(&handle).await;
    assert!(
        masked(&snapshot(&handle).await, "h0:spot"),
        "precondition: h0:spot marked ICE before the pull"
    );

    let _assignment = pull_attempt(&handle, "ice-multi").await;
    assert!(
        masked(&snapshot(&handle).await, "h0:spot"),
        "|A'|>1: the pull identifies no single cell; the mask (and its backoff step) \
         must survive the mint"
    );

    // |A'| = 1: the single armed cell IS cleared by the pull.
    let _ev2 = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ice-single",
        PriorityClass::Scheduled,
    )
    .await?;
    handle
        .send_unchecked(ack("ice-single", &["h2"], &["h2:spot"]))
        .await?;
    barrier(&handle).await;
    assert!(
        masked(&snapshot(&handle).await, "h2:spot"),
        "precondition: h2:spot marked ICE before the pull"
    );
    let _assignment = pull_attempt(&handle, "ice-single").await;
    let snap = snapshot(&handle).await;
    assert!(
        !masked(&snap, "h2:spot"),
        "|A'|==1: the pull is the success edge for the single armed cell — cleared"
    );
    assert!(
        masked(&snap, "h0:spot"),
        "the multi-cell intent's mask is untouched by the other intent's pull"
    );
    Ok(())
}

/// `r[sched.sla.hw-class.epsilon-explore]`: ε_h coin is a pure
/// function of `drv_hash`; `h_explore ∈ H\A` (or `H\{argmin price}`
/// on miss / A=H) is pinned in `MemoEntry.pinned_explore` and carried
/// across `inputs_gen` churn. The resulting affinity is
/// `⊆ {h_explore}×{spot,od}`. The memo is never overwritten.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn epsilon_h_draws_outside_a() {
    let db = TestDb::new(&MIGRATOR).await;
    // Builders-only fixture: this test compares `in_a` (drv-routable
    // classes) against `h_all = cfg.hw_classes`. Featureless drvs
    // never route to fetcher cells (∅-guard), so fetcher-* in `h_all`
    // would make `in_a ⊊ h_all` trivially true.
    let mut actor = bare_actor_hw_builders_only(db.pool.clone());
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);

    // Baseline at ε=0 (bare_actor_hw default) so the memo is the
    // unrestricted solve, not an ε_h hit.
    let (hw, cost, g0) = actor.solve_inputs();
    let baseline = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    actor.sla_config.hw_explore_epsilon = 0.2; // max
    let in_a: std::collections::HashSet<String> = baseline
        .node_affinity
        .iter()
        .filter_map(|t| {
            t.match_expressions
                .iter()
                .find(|r| r.key == "rio.build/hw-class")
                .and_then(|r| r.values.first().cloned())
        })
        .collect();
    let h_all: std::collections::HashSet<String> =
        actor.sla_config.hw_classes.keys().cloned().collect();
    assert_ne!(in_a, h_all, "fixture: distinct factors ⇒ A⊊H");

    // ── Determinism (bug_049 regression) ───────────────────────────
    // Same (drv_hash, inputs_gen) → identical affinity across 10
    // calls. Before the fix, `rand::rng()` re-rolled per call →
    // selector-drift reap churn.
    let first = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
    for _ in 0..10 {
        let again = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g0);
        assert_eq!(
            again.node_affinity, first.node_affinity,
            "ε_h draw is deterministic for fixed (drv_hash, inputs_gen)"
        );
    }
    // Different inputs_gen → §Fourth-strike Option 2: ε_h pin is
    // carried across the staleness miss; seed no longer XORs
    // `inputs_gen`. Pass `g0+1` directly — equivalent to a real
    // hw/cost solve-relevant change. The Opt2-falsification contract
    // test asserts identity across REAL `inputs_gen` churn; this just
    // re-asserts determinism at the new gen (which it would have been
    // even pre-Opt2).
    let g1 = g0.wrapping_add(1);
    let after_bump = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g1);
    for _ in 0..10 {
        let again = actor.solve_intent_for(actor.dag.node("d").unwrap(), &hw, &cost, g1);
        assert_eq!(
            again.node_affinity, after_bump.node_affinity,
            "still deterministic at the new generation"
        );
    }
    // 200 distinct drv_hashes at ε=0.2 → ~40 expected hits; every hit
    // emits ≤2 terms over a single h ∉ A.
    let mut explore_hits = 0;
    for i in 0..200 {
        let dh = format!("d{i}");
        actor.test_inject_ready(&dh, Some("test-pkg"), "x86_64-linux", false);
        let intent = actor.solve_intent_for(actor.dag.node(dh.as_str()).unwrap(), &hw, &cost, g1);
        let hs: std::collections::HashSet<String> = intent
            .node_affinity
            .iter()
            .filter_map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "rio.build/hw-class")
                    .and_then(|r| r.values.first().cloned())
            })
            .collect();
        if hs.len() == 1 && hs != in_a {
            explore_hits += 1;
            let h = hs.into_iter().next().unwrap();
            assert!(!in_a.contains(&h), "h_explore={h} must be ∉ A; A={in_a:?}");
            // A' ⊆ {h_explore}×{spot,od}.
            assert!(intent.node_affinity.len() <= 2);
        }
    }
    // ε=0.2, 200 trials → ~40 expected; ≥10 with overwhelming prob.
    assert!(explore_hits >= 10, "ε_h fires: got {explore_hits}/200");
    // Memo unchanged across all draws (one ModelKey, one override).
    assert_eq!(actor.solve_cache.len(), 1);

    // ── Fallback branch: in_a=∅ OR A=H → H\{argmin price} ──────────
    // Regression: with the old `pool = H\in_a; if empty → H\{cheapest}`,
    // the in_a=∅ case (memo=None / BestEffort) gave pool=H and the
    // fallback never fired. Both `in_a.is_empty()` and `in_a==H` now
    // route directly to H\{cheapest}. Exercise via A=H (observable):
    // equalize hw factors + prices within τ → every spot cell in A.
    // Seed prices are cap-only (all h equal) → cheapest_h would be
    // HashMap-iteration order; set distinct prices within τ=0.15 so
    // cheapest is deterministic AND A still spans H.
    {
        use crate::sla::config::CapacityType::Spot;
        let mut ct = actor.cost_table.write();
        ct.set_price("intel-6", Spot, 0.0100, 1e9);
        ct.set_price("intel-7", Spot, 0.0105, 1e9);
        ct.set_price("intel-8", Spot, 0.0110, 1e9);
    }
    let cheapest = actor.cost_table.read().cheapest_h(&h_all).unwrap();
    assert_eq!(cheapest, "intel-6");
    let mut m = HashMap::new();
    for h in &h_all {
        m.insert(h.clone(), 1.0);
    }
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));
    // hw + cost both changed → re-derive (no bump call to forget).
    let (hw2, cost2, g2) = actor.solve_inputs();
    assert_ne!(g2, g0, "seed_hw + set_price → derived inputs_gen changed");
    actor.sla_config.hw_explore_epsilon = 0.0;
    let in_a2: std::collections::HashSet<_> = actor
        .solve_intent_for(actor.dag.node("d").unwrap(), &hw2, &cost2, g2)
        .node_affinity
        .iter()
        .filter_map(|t| {
            t.match_expressions
                .iter()
                .find(|r| r.key == "rio.build/hw-class")
                .and_then(|r| r.values.first().cloned())
        })
        .collect();
    assert_eq!(in_a2, h_all, "fixture: equal factors + prices in τ ⇒ A=H");
    actor.sla_config.hw_explore_epsilon = 0.2;
    let mut hits = 0;
    for i in 0..200 {
        let dh = format!("e{i}");
        actor.test_inject_ready(&dh, Some("test-pkg"), "x86_64-linux", false);
        let intent = actor.solve_intent_for(actor.dag.node(dh.as_str()).unwrap(), &hw2, &cost2, g2);
        let hs: std::collections::HashSet<String> = intent
            .node_affinity
            .iter()
            .filter_map(|t| {
                t.match_expressions
                    .iter()
                    .find(|r| r.key == "rio.build/hw-class")
                    .and_then(|r| r.values.first().cloned())
            })
            .collect();
        if hs.len() == 1 {
            hits += 1;
            let h = hs.into_iter().next().unwrap();
            assert_ne!(
                h, cheapest,
                "A=H fallback must draw from H\\{{argmin price={cheapest}}}"
            );
        }
    }
    assert!(hits >= 10, "ε_h fires under A=H: got {hits}/200");
}

/// `solve_full` wired into the actor: with `hw_cost_source` set,
/// `hw_classes` configured, a populated hw-factor table, and a fitted
/// key, `SpawnIntent.node_affinity` carries OR-of-ANDs `(h, cap)`
/// terms.
// r[verify sched.sla.hw-class.admissible-set]
#[tokio::test]
async fn spawn_intent_node_affinity_from_solve_full() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());

    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert!(
        !fitted.node_affinity.is_empty(),
        "solve_full populated affinity: {:?}",
        fitted.node_affinity
    );
    // Each term: h-label conjunction PLUS capacity-type.
    for term in &fitted.node_affinity {
        assert!(
            term.match_expressions
                .iter()
                .any(|r| r.key == "karpenter.sh/capacity-type")
        );
        assert!(
            term.match_expressions
                .iter()
                .any(|r| r.key == "rio.build/hw-class")
        );
    }

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    assert!(
        cold.node_affinity.is_empty(),
        "no fit → hw-agnostic intent_for path"
    );

    // Determinism: re-emit returns the SAME affinity (memoized; no
    // softmax re-roll). The controller's `reap_stale_for_intents` sees
    // the same fingerprint across re-polls.
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let fitted2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert_eq!(
        fitted.node_affinity, fitted2.node_affinity,
        "deterministic — memoized"
    );
}

/// `solve_full` gate predicates: each of `enableParallelBuilding=false`
/// / `--tier` override falls through to the hw-agnostic `intent_for`
/// path even with `hw_cost_source` set and a usable fit.
/// `required_features=["kvm"]` with NO hwClass providing kvm ALSO falls
/// through (h_all empties under the §13c partition →
/// `!h_all.is_empty()` gate fails). `--mem`-only override does NOT gate
/// it off — solve_full runs and the override overlays the result.
/// §13e: FOD no longer falls through — `effective_features = [fetcher]`
/// partitions `h_all` to `fetcher-*` and FOD participates in solve_full.
// r[verify sched.sla.hw-class.admissible-set]
// r[verify sched.sla.fod-feature-derivation+3]
#[tokio::test]
async fn solve_full_gate_skips_kvm_serial_and_override() {
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_hw(db.pool.clone());
    // Re-seed under pname "pkg" (bare_actor_hw seeds "test-pkg").
    seed_fit(&actor, "pkg");

    // Baseline (non-FOD, no features) — proves the fixture DOES route
    // through solve_full so the negative assertions below are
    // meaningful.
    actor.test_inject_ready("base", Some("pkg"), "x86_64-linux", false);
    // FOD with the same fitted pname — §13e: routes to fetcher-*.
    actor.test_inject_ready("fod", Some("pkg"), "x86_64-linux", true);
    // required_features non-empty (kvm pool).
    actor.test_inject_ready_with_features("kvm", Some("pkg"), "x86_64-linux", &["kvm"]);
    // enableParallelBuilding=false — set on the state directly
    // (RecoveryDerivationRow has no column for it).
    actor.test_inject_ready("serial", Some("pkg"), "x86_64-linux", false);
    actor
        .dag
        .node_mut("serial")
        .unwrap()
        .enable_parallel_building = Some(false);

    let snap = actor.compute_spawn_intents(&Default::default());
    let by_id = |id: &str| snap.intents.iter().find(|i| i.intent_id == id).unwrap();

    assert!(
        !by_id("base").node_affinity.is_empty(),
        "fixture sanity: baseline routes through solve_full"
    );
    let fod = by_id("fod");
    assert!(
        !fod.hw_class_names.is_empty()
            && fod.hw_class_names.iter().all(|h| h.starts_with("fetcher-")),
        "§13e: FOD routes through solve_full to fetcher-* cells; got {:?}",
        fod.hw_class_names
    );
    assert!(
        by_id("kvm").node_affinity.is_empty(),
        "no hwClass provides kvm → h_all=∅ → hw-agnostic fallthrough"
    );
    assert!(
        by_id("kvm").hw_class_names.is_empty(),
        "no hwClass provides kvm → hw_class_names=∅"
    );
    let serial = by_id("serial");
    assert!(
        serial.node_affinity.is_empty(),
        "enableParallelBuilding=false stays hw-agnostic"
    );
    assert_eq!(
        serial.cores, 1,
        "serial drv pinned to 1 core via intent_for (r[sched.sla.intent-from-solve])"
    );

    // ── override gating (deferred from 2b6001b7) ────────────────────
    // `tier` override → solve_full skipped (intent_for honors tier).
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "pkg".into(),
            tier: Some("normal".into()),
            ..Default::default()
        }]);
    let snap = actor.compute_spawn_intents(&Default::default());
    let base = snap.intents.iter().find(|i| i.intent_id == "base").unwrap();
    assert!(
        base.node_affinity.is_empty(),
        "tier override gates solve_full off"
    );

    // `mem`-only override → solve_full SKIPPED (bug_033: any override
    // field routes hw-agnostic intent_for; the post-solve overlay is
    // gone), mem honored by intent_for.
    actor
        .sla_estimator
        .seed_overrides(vec![crate::db::SlaOverrideRow {
            pname: "pkg".into(),
            mem_bytes: Some(32 << 30),
            ..Default::default()
        }]);
    let snap = actor.compute_spawn_intents(&Default::default());
    let base = snap.intents.iter().find(|i| i.intent_id == "base").unwrap();
    assert!(
        base.node_affinity.is_empty(),
        "mem-only override gates solve_full off (bug_033)"
    );
    assert_eq!(base.mem_bytes, 32 << 30, "forced_mem honored by intent_for");
}

/// SLA mode: `try_dispatch_one` writes `solve_intent_for().0` to
/// `sched.est_cores`, and `build_assignment_proto` forwards it as
/// `WorkAssignment.assigned_cores`.
// r[verify sched.sla.cores-reach-nix-build-cores]
#[tokio::test]
async fn work_assignment_carries_sla_cores() {
    use crate::sla::types::*;
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // Same Amdahl fit as `spawn_intent_from_sla_estimator` → c*≈1.95 → 2.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(30.0),
            p: RefSeconds(2000.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.1,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    let intent = {
        let state = actor.dag.node("fitted").unwrap();
        solve_intent(&actor, state)
    };
    let expected_cores = intent.cores;

    // The pull mint is the intent writer now: stamp the solve onto the
    // node exactly as `mint_and_deliver` does, then build the payload it
    // would deliver.
    actor.dag.node_mut("fitted").unwrap().sched.last_intent = Some(intent);
    let assignment = actor
        .build_assignment_proto(&"fitted".into(), &"fitted".into())
        .await
        .expect("payload for an injected Ready node");
    assert_eq!(
        assignment.assigned_cores,
        Some(expected_cores),
        "SLA mode: WorkAssignment.assigned_cores == solve_intent_for().cores"
    );
}

/// `solve_intent_for`'s fitted-path `deadline_secs` is wall-clock, not
/// ref-seconds. `t_at()` evaluates the ref-second fit; with a slow
/// hw_class (factor < 1) in the table the wall-clock budget on that
/// node is `ref / factor` — without de-normalization a build there is
/// killed at `ref_q99 × 5` < `wall_q99 × 5`. Reverting the
/// `/ hw.min_factor()` in `snapshot.rs` makes `slow > fast` fail.
// r[verify sched.sla.hw-ref-seconds]
#[tokio::test]
async fn solve_intent_deadline_denormalized_to_slowest_hw() {
    use crate::sla::{hw, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // probe deadline 1s so the `c.max(probe_deadline)` floor doesn't
    // mask the computed value.
    actor.sla_config.probe.deadline_secs = 1;
    // High-S Amdahl so c* clamps low and `T(c)` ≈ S regardless of how
    // many cores the solve picks — keeps the deadline arithmetic
    // independent of the cores decision.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "test-pkg".into(),
            system: "x86_64-linux".into(),
            tenant: String::new(),
        },
        fit: DurationFit::Amdahl {
            s: RefSeconds(1000.0),
            p: RefSeconds(10.0),
        },
        mem: MemFit::Independent {
            p90: MemBytes(6 << 30),
        },
        disk_p90: Some(DiskBytes(10 << 30)),
        sigma_resid: 0.0,
        log_residuals: Vec::new(),
        n_eff_ring: RingNEff(10.0),
        fit_df: FitDf(10.0),
        n_distinct_c: 5,
        sum_w: 10.0,
        span: 8.0,
        explore: ExploreState {
            distinct_c: 3,
            min_c: RawCores(1.0),
            max_c: RawCores(32.0),
            saturated: false,
            last_wall: WallSeconds(0.0),
        },
        t_min_ci: None,
        ci_computed_at: None,
        tier: None,
        hw_bias: Default::default(),
        alpha: crate::sla::alpha::UNIFORM,
        prior_source: None,
        is_fod: false,
    });
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d").unwrap();

    // Baseline: empty hw table → min_factor()=1.0 → deadline ≈ ref_q99×5.
    let baseline = solve_intent(&actor, state).deadline_secs;

    // Fast-only table (factor 2.0): wall = ref/2 → deadline ≈ baseline/2.
    let mut fast = HashMap::new();
    fast.insert("aws-8-nvme".into(), 2.0);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(fast));
    let fast_dl = solve_intent(&actor, state).deadline_secs;
    assert!(
        fast_dl < baseline,
        "factor>1 → deadline shrinks (was over-budget): {fast_dl} < {baseline}"
    );

    // Slow class added (factor 0.5): worst-case wall = ref/0.5 = 2×ref.
    // Deadline must budget for the slowest band → ≈ 2×baseline.
    let mut mixed = HashMap::new();
    mixed.insert("aws-8-nvme".into(), 2.0);
    mixed.insert("aws-4-slow".into(), 0.5);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(mixed));
    let slow_dl = solve_intent(&actor, state).deadline_secs;
    assert!(
        slow_dl > baseline && slow_dl > fast_dl,
        "factor<1 in table → deadline budgets slowest wall: {slow_dl} > {baseline}"
    );
    // Ratio check: slow/fast ≈ (1/0.5)/(1/2.0) = 4. Allow slack for the
    // p≠0 term in T(c) and integer truncation.
    let ratio = f64::from(slow_dl) / f64::from(fast_dl);
    assert!(
        (3.5..=4.5).contains(&ratio),
        "slow/fast deadline ratio ≈ min_factor inverse: {ratio}"
    );
}

// ---------------------------------------------------------------------------
// I-065 fleet-exhaustion: system/feature awareness
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// Detached-substitute completion: leader gate, one-shot suppress, progress
// ---------------------------------------------------------------------------

// r[verify sched.substitute.leader-gate]
/// `SubstituteComplete` posted by a detached fetch task that survived
/// lease loss must be a no-op on the standby — the ok=true branch
/// writes PG (`persist_status(Completed)` etc.) and would split-brain
/// `derivations.status` with the new leader. Pre-fix the handler had
/// no `is_leader()` gate.
#[tokio::test]
async fn substitute_complete_on_standby_is_noop() -> TestResult {
    use std::sync::Arc;
    use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // Wire a LeaderState we can flip from the test.
    let is_leader = Arc::new(AtomicBool::new(true));
    let leader =
        crate::lease::LeaderState::from_parts(Arc::new(AtomicU64::new(1)), is_leader.clone(), true);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), Some(store_client), |_, p| {
        p.leader = leader;
    });

    // Seed substitutable so the dispatch-time batch spawns a detached
    // fetch (Ready → Substituting). Arm the QPI gate so the detached
    // task PARKS (not fails) — we need status==Substituting when the
    // explicit ok=true arrives, otherwise the status guard at
    // dispatch.rs:874 shadows the leader gate under test and a
    // delete-the-leader-gate mutation would still pass.
    let out = test_store_path("sub-standby-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);
    let mut n = make_node("sub-standby");
    n.expected_output_paths = vec![out];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    // Wait for ENTERED Substituting (not exited — task is parked at
    // the QPI gate).
    wait_for_status(&handle, "sub-standby", DerivationStatus::Substituting).await;

    // Lease lost while status is STILL Substituting. The explicit
    // ok=true now exercises ONLY the leader gate — guard #2
    // (status!=Substituting) cannot fire. Pre-fix this wrote
    // `derivations.status = 'completed'` to PG.
    is_leader.store(false, Ordering::SeqCst);
    let before = handle.debug_counters().await?.persist_status_calls;
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "sub-standby".into(),
            ok: true,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status::text, assigned_builder_id FROM derivations WHERE drv_hash = $1",
    )
    .bind("sub-standby")
    .fetch_one(&db.pool)
    .await?;
    assert_ne!(
        row.0, "completed",
        "standby must not write Completed to PG (split-brain)"
    );
    assert_eq!(
        handle.debug_counters().await?.persist_status_calls,
        before,
        "standby SubstituteComplete must not call persist_status"
    );

    // Release the parked detached task; its eventual post is ALSO
    // dropped by the leader gate. Node stays Substituting (correct —
    // both messages dropped) so don't settle_substituting; just give
    // the task time to post and re-check.
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    for _ in 0..10 {
        tokio::task::yield_now().await;
        barrier(&handle).await;
    }
    assert_eq!(
        handle.debug_counters().await?.persist_status_calls,
        before,
        "released detached task's post must also be dropped on standby"
    );
    assert_eq!(
        expect_drv(&handle, "sub-standby").await.status,
        DerivationStatus::Substituting,
        "leader gate dropped BOTH messages; node stays Substituting"
    );
    Ok(())
}

/// Ready→Substituting and Substituting→Ready/Queued both flip
/// `build_summary`'s running count; both paths must `emit_progress`
/// so the dashboard sees the change. Pre-fix neither did — the next
/// per-drv event after `SUBSTITUTING` was `CACHED` or much-later
/// `STARTED`, with stale running/queued in between.
#[tokio::test]
async fn substitute_spawn_and_revert_emit_progress() -> TestResult {
    use rio_proto::types::build_event::Event;
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("sub-prog-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, Ordering::SeqCst);

    let mut n = make_node("sub-prog");
    n.expected_output_paths = vec![out];
    let build_id = Uuid::new_v4();
    let mut ev = merge_dag(&handle, build_id, vec![n], vec![], false).await?;
    settle_substituting(&handle, &["sub-prog"]).await;

    // Drain: must contain a Substituting DerivationEvent followed by a
    // Progress with running >= 1. The ok=false revert ALSO calls
    // emit_progress (running→0), but that fires within
    // PROGRESS_DEBOUNCE of the spawn-side emit in this fast test and
    // is correctly debounced — same `self.emit_progress(build_id)`
    // wiring, so observing the spawn side proves both callsites.
    let mut saw_substituting = false;
    let mut saw_progress_running = false;
    while let Ok(e) = ev.try_recv() {
        match e.event {
            Some(Event::Derivation(d))
                if d.kind == rio_proto::types::DerivationEventKind::Substituting as i32 =>
            {
                saw_substituting = true;
            }
            Some(Event::Progress(p)) if saw_substituting && p.running >= 1 => {
                saw_progress_running = true;
            }
            _ => {}
        }
    }
    assert!(
        saw_substituting,
        "merge → spawn_substitute_fetches must emit a Substituting DerivationEvent"
    );
    assert!(
        saw_progress_running,
        "spawn_substitute_fetches must emit_progress (running ≥ 1) after Substituting"
    );
    // The revert reached Ready/Queued and substitute_tried is set —
    // covers the ok=false branch ran (the emit_progress above is
    // wired identically there).
    let info = expect_drv(&handle, "sub-prog").await;
    assert!(matches!(
        info.status,
        DerivationStatus::Ready | DerivationStatus::Queued
    ));
    assert!(info.substitute_tried);
    Ok(())
}

// r[verify gw.activity.subst-progress]
/// `ActorCommand::SubstituteProgress` → `Event::SubstituteProgress` on
/// the build's LOG broadcast ring (display-only; state ring untouched).
/// Drives the actor directly — `walk_substitute_closure` is exercised
/// against real progress in the rio-store integration tests; here we
/// assert the scheduler-side relay wiring.
#[tokio::test]
async fn substitute_progress_emitted_on_log_channel() -> TestResult {
    use rio_proto::types::build_event::Event;

    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;
    let build_id = Uuid::new_v4();
    let mut state_ev = merge_dag(
        &handle,
        build_id,
        vec![make_node("sub-prog2")],
        vec![],
        false,
    )
    .await?;
    let mut log_ev = subscribe_log(&handle, build_id).await?;
    // Drain pre-existing events from both rings (BuildStarted etc.).
    while state_ev.try_recv().is_ok() {}
    while log_ev.try_recv().is_ok() {}

    handle
        .send_unchecked(ActorCommand::SubstituteProgress {
            drv_hash: "sub-prog2".into(),
            bytes_done: 7_000_000,
            bytes_expected: 10_000_000,
            upstream_uri: "https://cache.example.test".into(),
        })
        .await?;
    // Barrier: any debug query round-trips the actor mailbox.
    let _ = expect_drv(&handle, "sub-prog2").await;

    // Log ring: exactly the SubstituteProgress.
    let got = log_ev.try_recv().expect("SubstituteProgress on log ring");
    match got.event {
        Some(Event::SubstituteProgress(p)) => {
            assert_eq!(p.bytes_done, 7_000_000);
            assert_eq!(p.bytes_expected, 10_000_000);
            assert_eq!(p.upstream_uri, "https://cache.example.test");
            assert_eq!(p.derivation_path, test_drv_path("sub-prog2"));
        }
        other => panic!("expected SubstituteProgress, got {other:?}"),
    }
    // State ring: NO SubstituteProgress (display-only routing).
    while let Ok(e) = state_ev.try_recv() {
        assert!(
            !matches!(e.event, Some(Event::SubstituteProgress(_))),
            "SubstituteProgress must NOT route to state ring"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// I-139: locally-present completion is batched (no per-row PG awaits)
// ---------------------------------------------------------------------------

/// `batch_probe_cached_ready`'s locally-present branch must batch the
/// PG writes — pre-fix it awaited `complete_ready_from_store` per
/// item (≥3 sequential PG RTTs each); on warm-restart of a large
/// closure ~all 2048 candidates hit it → 12-30s actor stall.
/// Structural assertion via `persist_status_calls`: batch path uses
/// `persist_status_batch` (NOT counted), so 0 singular calls during
/// the dispatch pass means the batch helper was used.
#[tokio::test]
async fn batch_probe_locally_present_batches_pg() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    // Builder for an unrelated arch: heartbeat sets dispatch_dirty so
    // 50 leaves, all outputs seeded present → all hit the
    // locally-present branch on the first dispatch_ready.
    let mut nodes = Vec::with_capacity(50);
    {
        let mut paths = store.state.paths.write().unwrap();
        for i in 0..50 {
            let out = test_store_path(&format!("bp-{i}-out"));
            paths.insert(out.clone(), Default::default());
            let mut n = make_node(&format!("bp-{i}"));
            n.expected_output_paths = vec![out];
            nodes.push(n);
        }
    }
    // fail_find_missing during merge so check_cached_outputs +
    // merge's inline dispatch_ready don't complete them — we want
    // them Ready at the NEXT dispatch-time batch.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    barrier(&handle).await;
    store
        .faults
        .fail_find_missing
        .store(false, Ordering::SeqCst);

    let before = handle.debug_counters().await?.persist_status_calls;
    tick(&handle).await?;
    barrier(&handle).await;
    let after = handle.debug_counters().await?.persist_status_calls;

    for i in 0..50 {
        assert_eq!(
            expect_drv(&handle, &format!("bp-{i}")).await.status,
            DerivationStatus::Completed,
            "node bp-{i} must be Completed by the dispatch-time batch"
        );
    }
    assert_eq!(
        after, before,
        "locally-present completion must use persist_status_batch (0 singular calls); \
         pre-fix this was {} (one per node + one per newly-ready)",
        50
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// rollback_assignment is a complete inverse of record_assignment
// ---------------------------------------------------------------------------

// ---------------------------------------------------------------------------
// I-139/I-140: batch-probe truncated tail must NOT hit per-drv FMP fallback
// ---------------------------------------------------------------------------

// r[verify sched.dispatch.fod-substitute+2]
/// With > `DISPATCH_PROBE_BATCH_CAP` Ready leaves and the batch RPC
/// failing-open, the truncated tail must NOT fall through to the
/// per-drv `ready_check_or_spawn` (one inline-awaited FMP each =
/// O(N) sequential 30s timeouts in the actor). Pre-fix:
/// `find_missing_calls == 1 + tail` (tail × 30s ≈ 24h stall with an
/// unreachable store). Post-fix: 1 batch + 0 per-drv; tail dispatches
/// fail-open and is batch-probed next pass.
#[tokio::test]
async fn batch_probe_tail_never_per_drv_fmp() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // CAP+12 IA leaves with non-empty expected_output_paths so they're
    // batch candidates. No worker connected → all defer (we only care
    // about FMP call count, not assignment).
    let n = crate::actor::DISPATCH_PROBE_BATCH_CAP + 12;
    let nodes: Vec<_> = (0..n)
        .map(|i| {
            let tag = format!("bpt-{i}");
            let mut node = make_node(&tag);
            node.expected_output_paths = vec![test_store_path(&format!("bpt-{i}-out"))];
            node
        })
        .collect();
    // Batch FMP fails-open (Unavailable). Per-drv calls would also fail
    // — but the bug is they're MADE at all.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    let after_merge = store.calls.find_missing_calls.load(Ordering::SeqCst);
    // The ready-set sweep runs inline inside merge; count what THAT did.
    tick(&handle).await?;
    let total = store.calls.find_missing_calls.load(Ordering::SeqCst);
    // 1 batch FMP per sweep (merge's inline sweep + our explicit tick).
    // Neither may trigger per-drv tail calls — the per-drv fallback no
    // longer exists; the bug case was +12 EXTRA (one per tail node).
    let sweep_calls = total - after_merge;
    assert!(
        sweep_calls <= 2,
        "tick → ≤2 ready-set sweeps → ≤2 batch FMPs; \
         got {sweep_calls} (pre-fix: 1 batch + 12 per-drv tail = 13)"
    );
    assert!(
        total <= 8,
        "total FMPs across merge+tick bounded by batch passes, NOT by \
         tail size; got {total} (12 truncated nodes; pre-fix the tail \
         leaked to per-drv calls each pass)"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// SubstituteComplete{ok=false} 3-way revert (DependencyFailed branch)
// ---------------------------------------------------------------------------

// r[verify sched.state.transitions]
/// `handle_substitute_complete{ok=false}` on a node whose dep is
/// terminally-failed must revert to `DependencyFailed`, not `Queued`.
/// `Queued` with a `Poisoned` dep is stuck forever (find_newly_ready
/// never fires; poison_and_cascade already ran). Pre-fix: 2-way
/// `Ready|Queued` only.
#[tokio::test]
async fn substitute_fail_with_poisoned_dep_goes_dependency_failed() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // X depends on Y. Force Y Poisoned-at-limit; force X Substituting
    // (the I-094 reprobe lane reaches this state via DependencyFailed →
    // Substituting; here we set it directly to isolate the handler).
    let x_out = test_store_path("sfp-x-out");
    let mut x = make_node("sfp-x");
    x.expected_output_paths = vec![x_out];
    let build = Uuid::new_v4();
    merge_dag(
        &handle,
        build,
        vec![x, make_node("sfp-y")],
        vec![make_test_edge("sfp-x", "sfp-y")],
        false,
    )
    .await?;
    handle
        .debug_force_poisoned("sfp-y", POISON_RESUBMIT_RETRY_LIMIT)
        .await?;
    handle
        .debug_force_status("sfp-x", DerivationStatus::Substituting)
        .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "sfp-x").await.status,
        DerivationStatus::Substituting,
        "precondition"
    );

    // Detached fetch failed.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "sfp-x".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    let xs = expect_drv(&handle, "sfp-x").await;
    assert_eq!(
        xs.status,
        DerivationStatus::DependencyFailed,
        "dep Y Poisoned → X reverts to DependencyFailed (pre-fix: Queued, \
         stuck forever — never Ready since Y≠Completed, never cascaded \
         since cascade only fires on Y's TRANSITION to Poisoned)"
    );
    // Build completion check fired (terminal_failure_epilogue): build
    // terminates instead of hanging Active.
    let st = query_status(&handle, build).await?;
    assert_ne!(
        st.state,
        rio_proto::types::BuildState::Active as i32,
        "build must terminate, not hang Active"
    );
    Ok(())
}

/// `r[store.substitute.admission]` regression: with the store returning
/// `ResourceExhausted` for 20 s of wall-clock (modelling a saturated
/// per-replica admission gate that exhausts its 25 s bounded-wait), the
/// detached substitute-fetch's 8-attempt retry curve MUST absorb it —
/// zero demotions to build-from-source.
///
/// Reshaped from spike 0.1's `{2,8,20,60}s × 50` matrix into a single
/// 20 s × 10 regression. Spike proved the prior 5-attempt ~3.75 s
/// budget demoted 50/50 at hold ≥ 8 s; with `MAX_ATTEMPTS=8` (~31.75 s
/// budget) a 20 s hold is comfortably inside the window. 20 s (not the
/// spike's 8 s) so a future regression to 5 attempts fails this
/// cleanly; not 60 s because the test runs real-time (PG + paused
/// time don't compose) and 60 s would dominate the suite.
///
/// Real-time, so the test costs ~22 s wall-clock (20 s hold + ~2 s
/// drain). Polls until none are `Substituting`; capped at 60 s
/// (cumulative backoff to attempt 7 is ~31.75 s ±20 % jitter + slack).
// r[verify store.substitute.admission]
#[tokio::test]
async fn substitute_fetch_survives_store_backpressure() -> TestResult {
    use crate::state::DerivationStatus;
    const N: usize = 10;
    const HOLD: std::time::Duration = std::time::Duration::from_secs(20);

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // 10 leaf nodes, aarch64 so they can't dispatch to a worker. One
    // output each, all seeded substitutable BEFORE merge so merge-time
    // check_cached_outputs → spawn_substitute_fetches transitions all
    // 10 to Substituting and spawns 10 detached fetch tasks.
    let mut nodes = Vec::with_capacity(N);
    let mut hashes = Vec::with_capacity(N);
    let mut outs = Vec::with_capacity(N);
    for i in 0..N {
        let tag = format!("bp-n{i}");
        let out = test_store_path(&format!("bp-n{i}-out"));
        let mut n = make_node(&tag);
        n.system = "aarch64-linux".into();
        n.expected_output_paths = vec![out.clone()];
        hashes.push(n.drv_hash.clone());
        outs.push(out);
        nodes.push(n);
    }
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .extend(outs.iter().cloned());

    // Arm RE-until-deadline. Real Instant; the mock checks
    // `now() < until` on every QPI call.
    *store
        .faults
        .fail_qpi_resource_exhausted_until
        .write()
        .unwrap() = Some(std::time::Instant::now() + HOLD);

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, nodes, vec![], false).await?;
    barrier(&handle).await;

    // Poll until none are Substituting. Each task succeeds on its
    // first post-hold attempt (store returns the seeded path), so
    // resolution is at HOLD + (next backoff after HOLD) — for a 20 s
    // hold that's attempt 7 at ~15.75 s or attempt 8 at ~31.75 s
    // depending on jitter. 60 s cap is generous over max-jitter
    // cumulative (31.75 × 1.2 ≈ 38 s); loop exits early on success.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(60);
    loop {
        barrier(&handle).await;
        let mut any_sub = false;
        for h in &hashes {
            if expect_drv(&handle, h).await.status == DerivationStatus::Substituting {
                any_sub = true;
                break;
            }
        }
        if !any_sub {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached fetches did not resolve within 60 s (8-attempt budget \
             ~31.75 s ±20 % + drain); hung task?"
        );
        tokio::time::sleep(std::time::Duration::from_millis(100)).await;
    }
    *store
        .faults
        .fail_qpi_resource_exhausted_until
        .write()
        .unwrap() = None;

    // Zero demotions: every node Completed (substituted), none demoted
    // to Ready/Queued (build-from-source). Spike 0.1 proved 5 attempts
    // → 50/50 demoted at hold ≥ 8 s; this is the inverse assertion.
    for h in &hashes {
        let st = expect_drv(&handle, h).await.status;
        assert_eq!(
            st,
            DerivationStatus::Completed,
            "{h}: 20 s store backpressure must NOT demote to build-from-source \
             (8-attempt retry window covers it); got {st:?}"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// walk_substitute_closure (Option C: scheduler-side closure BFS)
// ---------------------------------------------------------------------------

/// Seed a path into MockStore with the given references. The seeded
/// `ValidatedPathInfo` is what `QueryPathInfo` / `BatchQueryPathInfo`
/// return, so `walk_substitute_closure` pushes `refs` onto its frontier.
fn seed_with_refs(store: &rio_test_support::grpc::MockStore, path: &str, refs: &[&str]) {
    let (nar, hash) = rio_test_support::fixtures::make_nar(path.as_bytes());
    let mut info = rio_test_support::fixtures::make_path_info(path, &nar, hash);
    info.references = refs
        .iter()
        .map(|r| rio_nix::store_path::StorePath::parse(r).unwrap())
        .collect();
    store.seed(info, nar);
}

// r[verify sched.substitute.detached+5]
/// `walk_substitute_closure` MUST walk transitively. Diamond:
/// A → [B, C]; B → [D]; C → [D]. All four end up in `state.paths`
/// (warm) so the layer-batched `BatchQueryPathInfo` fast-path covers
/// the whole closure. Assert ok=true and D visited once (dedup via
/// `visited`).
#[tokio::test]
async fn substitute_fetch_walks_closure_transitively() -> TestResult {
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let a = test_store_path("clo-a");
    let b = test_store_path("clo-b");
    let c = test_store_path("clo-c");
    let d = test_store_path("clo-d");
    seed_with_refs(&store, &a, &[&b, &c]);
    seed_with_refs(&store, &b, &[&d]);
    seed_with_refs(&store, &c, &[&d]);
    seed_with_refs(&store, &d, &[]);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![a.clone()],
        &Default::default(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        ok,
        "diamond closure with all refs present must return ok=true"
    );

    // Layer-batched: 3 BatchQPI calls (layer [A], layer [B,C], layer
    // [D]); D appears once because B and C's refs dedup. Zero per-path
    // QPIs because every layer was warm.
    let batches = store
        .calls
        .batch_qpi_calls
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        batches, 3,
        "warm closure: one BatchQPI per BFS layer (A; B,C; D)"
    );
    assert!(
        store.calls.qpi_calls.read().unwrap().is_empty(),
        "warm refs go through batch fast-path; no per-path QPI"
    );
    Ok(())
}

// r[verify sched.substitute.detached+5]
/// A reference miss MUST set ok=false (not silently truncate). A is
/// seeded substitutable (per-path QPI returns it with refs=[B]); B is
/// nowhere → batch returns None for B AND per-path QPI returns
/// NotFound → ok=false. This is the bug class the store-side
/// `ensure_references` couldn't surface (it returned `()`).
/// `start_paused` + in-process transport: B's NotFound now walks the
/// full retry ladder (~32 s of backoff) before the walk gives up.
#[tokio::test(start_paused = true)]
async fn substitute_fetch_ref_miss_sets_ok_false() -> TestResult {
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    let a = test_store_path("miss-a");
    let b = test_store_path("miss-b");
    seed_with_refs(&store, &a, &[&b]);
    // B is NOT seeded — both BatchQPI and QPI report it absent.

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![a.clone()],
        &Default::default(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        !ok,
        "missing transitive ref must set ok=false → revert (not Completed)"
    );
    // B fell through batch → per-path QPI tried → NotFound (recorded).
    assert!(
        store.calls.qpi_calls.read().unwrap().contains(&b),
        "absent ref must reach per-path QPI to attempt substitution"
    );
    Ok(())
}

/// A seed in `forgivable` (declared but unwanted) that the upstream
/// definitively misses must NOT fail the walk: the wanted seed
/// substitutes, the unwanted one is skipped with a log line, and the
/// walk returns ok=true. The unwanted seed is still ATTEMPTED
/// (opportunistic completeness — it stays in the seed list). The
/// returned `forgiven` set reports exactly the seeds that FAILED and
/// were forgiven — not the ones that substituted fine — so
/// `handle_substitute_complete` can re-check them against a wanted set
/// that grew mid-walk.
#[tokio::test]
async fn walk_forgives_unwanted_seed_not_found() -> TestResult {
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let out = test_store_path("fgvw-out");
    let dbg = test_store_path("fgvw-debug");
    // out: substitutable (per-path QPI materializes it). dbg: nowhere
    // → per-path SubstitutePath returns NotFound.
    store.state.substitutable.write().unwrap().push(out.clone());

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let forgivable: HashSet<String> = [dbg.clone()].into();
    let (ok, forgiven) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![out.clone(), dbg.clone()],
        &forgivable,
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        ok,
        "an unwanted seed the upstream misses must be forgiven → ok=true"
    );
    assert_eq!(
        forgiven,
        vec![dbg.clone()],
        "the forgiven set must report exactly the seeds whose failure \
         was forgiven (not the wanted seed that substituted fine)"
    );
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&dbg),
        "the unwanted seed must still be attempted (opportunistic \
         completeness); qpi_calls={qpi:?}"
    );
    Ok(())
}

/// The same scenario with an empty `forgivable` set (= every seed
/// wanted) keeps today's verdict: a seed miss fails the walk. The miss
/// is no longer accepted on the first occurrence, though — every path
/// in the walk was either HEAD-probed as available or named in a
/// narinfo the upstream just served, so a NotFound here contradicts an
/// earlier observation and must burn the full retry ladder before the
/// walk gives up. `start_paused` virtualizes the ~32 s of backoff;
/// in-process transport so auto-advance can't fire a timeout on a
/// kernel-side TCP handshake.
#[tokio::test(start_paused = true)]
async fn walk_fails_on_wanted_seed_not_found() -> TestResult {
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    let out = test_store_path("fgva-out");
    let dbg = test_store_path("fgva-debug");
    store.state.substitutable.write().unwrap().push(out.clone());

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![out.clone(), dbg.clone()],
        &HashSet::new(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        !ok,
        "every seed wanted (empty forgivable set) → a seed miss must \
         still fail the walk"
    );
    // Structural: the contradicted NotFound must consume the whole
    // retry budget before the walk demotes — a first-occurrence
    // demotion is the 235-path incident shape.
    assert_eq!(
        store
            .calls
            .qpi_attempts_by_path
            .read()
            .unwrap()
            .get(&dbg)
            .copied(),
        Some(crate::actor::SUBSTITUTE_FETCH_MAX_ATTEMPTS),
        "a wanted seed's NotFound must retry up to the attempt budget \
         before demoting, not give up on the first occurrence"
    );
    Ok(())
}

/// Sum every series of counter `name` in a debugging-recorder snapshot,
/// returning `(total, per-series labels)`. The labels let demotion
/// tests assert the `reason` value without hard-coding series order.
fn counter_series(
    snap: &metrics_util::debugging::Snapshotter,
    name: &str,
) -> (u64, Vec<Vec<(String, String)>>) {
    use metrics_util::debugging::DebugValue;
    let mut total = 0u64;
    let mut labels = Vec::new();
    for (ck, _, _, v) in snap.snapshot().into_vec() {
        if ck.key().name() != name {
            continue;
        }
        if let DebugValue::Counter(c) = v {
            total += c;
            labels.push(
                ck.key()
                    .labels()
                    .map(|l| (l.key().to_string(), l.value().to_string()))
                    .collect(),
            );
        }
    }
    (total, labels)
}

// r[verify sched.substitute.detached+5]
/// A wanted seed whose `SubstitutePath` returns `NotFound` twice and
/// then succeeds must be FETCHED, not demoted: every path in the walk
/// was HEAD-probed as available minutes earlier or named in a narinfo
/// the upstream just served, so a NotFound inside the walk is a
/// contradiction (auth/overload short-circuit, not a genuine miss) and
/// joins the same retry ladder as transient errors. The 2026-05 incident
/// demoted 235 such paths to from-source builds on their FIRST NotFound;
/// all 235 substituted fine 80 seconds later.
#[tokio::test(start_paused = true)]
async fn walk_retries_not_found_then_succeeds() -> TestResult {
    use std::sync::atomic::Ordering;
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    let out = test_store_path("nfrt-out");
    // Substitutable (the GET would succeed) but the first 2 attempts
    // short-circuit with NotFound before reaching the upstream.
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .fail_qpi_not_found_per_path_n
        .store(2, Ordering::SeqCst);

    let rec = metrics_util::debugging::DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, forgiven) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![out.clone()],
        &HashSet::new(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        ok,
        "a NotFound that clears on retry must not demote the derivation"
    );
    assert!(forgiven.is_empty(), "nothing was forgiven — it was fetched");
    // Structural: 2 NotFounds + 1 success = 3 attempts.
    assert_eq!(
        store
            .calls
            .qpi_attempts_by_path
            .read()
            .unwrap()
            .get(&out)
            .copied(),
        Some(3),
        "expected 2 NotFound retries + 1 success"
    );
    // The path was ultimately fetched (success arm reached once).
    assert!(
        store.calls.qpi_calls.read().unwrap().contains(&out),
        "the path must reach the success arm after the NotFounds clear"
    );
    let (demotions, _) = counter_series(&snap, "rio_scheduler_substitute_demotions_total");
    assert_eq!(
        demotions, 0,
        "a recovered NotFound must not count as a demotion"
    );
    let (failures, _) = counter_series(&snap, "rio_scheduler_substitute_fetch_failures_total");
    assert_eq!(
        failures, 0,
        "a recovered NotFound must not count as a fetch failure"
    );
    Ok(())
}

// r[verify sched.substitute.detached+5]
/// A wanted seed that returns `NotFound` on EVERY attempt exhausts the
/// retry ladder and only then demotes: ok=false, exactly
/// `SUBSTITUTE_FETCH_MAX_ATTEMPTS` attempts, and ONE
/// `substitute_demotions_total{reason=not_found}` increment (the page-
/// worthy "a derivation and its build-time closure are about to be
/// compiled from source because a download failed" event).
#[tokio::test(start_paused = true)]
async fn walk_demotes_after_not_found_retries_exhausted() -> TestResult {
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    // Not seeded anywhere → the mock's SubstitutePath returns a plain
    // "path not found" NotFound on every attempt.
    let out = test_store_path("nfex-out");

    let rec = metrics_util::debugging::DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![out.clone()],
        &HashSet::new(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(!ok, "a NotFound on every attempt must still demote");
    assert_eq!(
        store
            .calls
            .qpi_attempts_by_path
            .read()
            .unwrap()
            .get(&out)
            .copied(),
        Some(crate::actor::SUBSTITUTE_FETCH_MAX_ATTEMPTS),
        "the demotion must come AFTER the full retry budget, not on the \
         first NotFound"
    );
    let (demotions, labels) = counter_series(&snap, "rio_scheduler_substitute_demotions_total");
    assert_eq!(
        demotions, 1,
        "exactly one demotion for one exhausted path; labels={labels:?}"
    );
    assert_eq!(
        labels,
        vec![vec![("reason".to_string(), "not_found".to_string())]],
        "a plain path-not-found after retries must be reason=not_found"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
/// A forgivable (unwanted) seed is forgiven on its FIRST failure of any
/// kind — it must not burn the 8-attempt retry ladder first. The path
/// is still attempted once (opportunistic completeness) but a transient
/// error on something nobody consumes is not worth ~32 s of backoff
/// serialized into the walk's per-path loop.
#[tokio::test(start_paused = true)]
async fn walk_forgives_unwanted_seed_without_retrying() -> TestResult {
    use std::sync::atomic::Ordering;
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    let dbg = test_store_path("fgv1-debug");
    // Unavailable — TRANSIENT. Pre-change this routed to the retry
    // ladder and the seed was only forgiven by the post-loop
    // exhaustion arm after 8 attempts.
    store
        .faults
        .fail_query_path_info
        .store(true, Ordering::SeqCst);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let forgivable: HashSet<String> = [dbg.clone()].into();
    let (ok, forgiven) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![dbg.clone()],
        &forgivable,
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(ok, "a forgivable seed's transient failure must be forgiven");
    assert_eq!(
        forgiven,
        vec![dbg.clone()],
        "the forgiven set must record the seed"
    );
    assert_eq!(
        store
            .calls
            .qpi_attempts_by_path
            .read()
            .unwrap()
            .get(&dbg)
            .copied(),
        Some(1),
        "a forgivable seed must be forgiven on the FIRST failure — \
         attempted once, zero retries"
    );
    Ok(())
}

/// A NON-TRANSIENT error (generic `Err(e)`, here `Internal`) is
/// forgiven iff the seed is forgivable: the dedicated first-failure
/// forgiveness arm catches it for a forgivable seed; a non-forgivable
/// seed falls through to the generic `Err(e)` arm and demotes
/// immediately (no retry ladder). Single-seed A/B because the mock's
/// fault knob is global (every QPI fails) — the wanted-seed-succeeds-
/// while-unwanted-fails composition is covered by the NotFound pair
/// above and the end-to-end test below.
#[tokio::test]
async fn walk_forgives_unwanted_seed_non_transient_error() -> TestResult {
    use std::sync::atomic::Ordering;
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // Internal — non-transient, not NotFound → decided on the FIRST
    // attempt either way (no retry ladder).
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, Ordering::SeqCst);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    for (tag, forgivable_dbg, expect_ok) in [("fgve-f", true, true), ("fgve-w", false, false)] {
        let dbg = test_store_path(&format!("{tag}-debug"));
        let forgivable: HashSet<String> = if forgivable_dbg {
            [dbg.clone()].into()
        } else {
            HashSet::new()
        };
        let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
            &client,
            vec![dbg.clone()],
            &forgivable,
            &auth,
            &shutdown,
            |_, _, _| {},
        )
        .await;
        assert_eq!(
            ok, expect_ok,
            "{tag}: a non-transient error on a seed must be forgiven \
             iff the seed is forgivable"
        );
        // Structural: exactly one attempt — the first-failure
        // forgiveness arm (forgivable) or the generic error arm
        // (non-forgivable) fired, not the retry ladder / exhaust
        // fallthrough.
        assert_eq!(
            store
                .calls
                .qpi_attempts_by_path
                .read()
                .unwrap()
                .get(&dbg)
                .copied(),
            Some(1),
            "{tag}: Internal is non-transient → no retries → decided \
             on the first attempt"
        );
    }
    Ok(())
}

/// The RETRY-EXHAUST fallthrough (after `SUBSTITUTE_FETCH_MAX_ATTEMPTS`
/// transient errors) demotes a non-forgivable seed with
/// `reason=exhausted`. `start_paused` virtualizes the ~32 s of backoff
/// between the 8 attempts; the per-seed attempt count is asserted to
/// be exactly `SUBSTITUTE_FETCH_MAX_ATTEMPTS` so a spurious
/// auto-advance through an in-flight RPC (which would surface as a
/// non-transient `DeadlineExceeded` and route to the WRONG arm) fails
/// the test instead of silently passing via the error gate.
///
/// This used to be an A/B pair proving the fallthrough had its own
/// forgiveness gate; a forgivable seed is now forgiven on its FIRST
/// failure and never reaches the fallthrough — that side lives in
/// `walk_forgives_unwanted_seed_without_retrying`.
#[tokio::test(start_paused = true)]
async fn walk_demotes_after_transient_retries_exhausted() -> TestResult {
    use std::sync::atomic::Ordering;
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    // Unavailable — transient → the retry ladder runs to exhaustion.
    store
        .faults
        .fail_query_path_info
        .store(true, Ordering::SeqCst);

    let rec = metrics_util::debugging::DebuggingRecorder::new();
    let snap = rec.snapshotter();
    let _g = metrics::set_default_local_recorder(&rec);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let dbg = test_store_path("fgvx-w-debug");
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![dbg.clone()],
        &HashSet::new(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(!ok, "exhausted retries on a wanted seed must fail the walk");
    // Structural: all attempts reached the store — the transient
    // ladder ran to exhaustion and the post-loop fallthrough made
    // the decision, not the NotFound or generic-error arm.
    assert_eq!(
        store
            .calls
            .qpi_attempts_by_path
            .read()
            .unwrap()
            .get(&dbg)
            .copied(),
        Some(crate::actor::SUBSTITUTE_FETCH_MAX_ATTEMPTS),
        "every attempt must reach the store before the exhaust \
         fallthrough fires"
    );
    let (demotions, labels) = counter_series(&snap, "rio_scheduler_substitute_demotions_total");
    assert_eq!(demotions, 1, "one exhausted path = one demotion");
    assert_eq!(
        labels,
        vec![vec![("reason".to_string(), "exhausted".to_string())]],
        "transient retries exhausted must be reason=exhausted"
    );
    Ok(())
}

/// Forgiveness is scoped to unwanted SEEDS only. A path discovered via
/// the reference BFS (a runtime reference of a successfully fetched
/// seed) is never forgivable — its absence is a hole in a closure the
/// walk is about to declare complete (`SubstituteComplete{ok=true}` →
/// `Substituting → Completed` → a dependent ENOENTs at exec time). The
/// unwanted seed's miss is forgiven AND the wanted seed's missing ref
/// still fails the walk, in the same run. `start_paused` + in-process
/// transport: the non-forgivable reference's NotFound now walks the
/// full retry ladder (~32 s of backoff) before the walk gives up.
#[tokio::test(start_paused = true)]
async fn walk_forgiveness_does_not_extend_to_references() -> TestResult {
    let (store, client) = rio_test_support::grpc::spawn_mock_store_inproc().await?;
    let out = test_store_path("fgvr-out");
    let dbg = test_store_path("fgvr-debug");
    let r = test_store_path("fgvr-ref");
    // out: warm with a reference to r. r: nowhere. dbg: nowhere.
    seed_with_refs(&store, &out, &[&r]);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let forgivable: HashSet<String> = [dbg.clone()].into();
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![out.clone(), dbg.clone()],
        &forgivable,
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        !ok,
        "a missing reference-BFS-discovered path must fail the walk \
         even when an unwanted seed's miss was forgiven"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// End-to-end forgiveness: a derivation classified as
/// pending-substitute whose UNWANTED seed fails the upstream GET must
/// still complete (the walk forgives the unwanted seed); the same
/// derivation with an empty wanted set (= all wanted) must demote to
/// build-from-source (today's behaviour preserved).
///
/// P_out is substitutable (probe + GET agree). P_debug is
/// indeterminate at probe time (treated optimistically →
/// pending_substitute) but the GET fails. The forgiven arm fails it
/// with the natural NotFound (the HEAD-says-maybe / GET-misses
/// divergence that condemns a whole closure to a from-source rebuild
/// over an output nothing consumes). The all-wanted arm fails it with
/// a per-path non-retryable Internal instead: a non-forgivable
/// NotFound now burns the full ~32 s retry ladder in real time before
/// demoting (the actor-driven test can't virtualize the clock), and
/// the retry-then-demote path is structurally covered by
/// `walk_demotes_after_not_found_retries_exhausted`. What this arm
/// uniquely covers — the wanted-set plumbing that makes the seed
/// non-forgivable, and the SubstituteComplete{ok=false} → Ready revert
/// — is error-kind-agnostic.
#[tokio::test]
async fn substitute_walk_forgives_unwanted_seed_end_to_end() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    for (tag, wanted, expect) in [
        // P_debug unwanted → its GET miss is forgiven → Completed.
        (
            "fgv-w",
            vec!["out".to_string()],
            DerivationStatus::Completed,
        ),
        // Empty wanted = all wanted → P_debug's GET failure is fatal →
        // demoted to Ready for a from-source dispatch.
        ("fgv-a", vec![], DerivationStatus::Ready),
    ] {
        let out = test_store_path(&format!("{tag}-out"));
        let dbg = test_store_path(&format!("{tag}-debug"));
        store.state.substitutable.write().unwrap().push(out.clone());
        store.state.indeterminate.write().unwrap().push(dbg.clone());
        if tag == "fgv-a" {
            store
                .faults
                .fail_qpi_internal_paths
                .write()
                .unwrap()
                .insert(dbg.clone());
        }

        let mut n = make_node(tag);
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted;
        let build_id = Uuid::new_v4();
        merge_dag(&handle, build_id, vec![n], vec![], false).await?;
        settle_substituting(&handle, &[tag]).await;

        assert_eq!(
            expect_drv(&handle, tag).await.status,
            expect,
            "{tag}: unwanted-seed fetch failure must be forgiven only \
             when the seed is outside a non-empty wanted set"
        );
        // Opportunistic completeness: the unwanted seed is still
        // ATTEMPTED (it stays in the seed list) — forgiveness changes
        // the verdict, not the fetch. `qpi_attempts_by_path` (recorded
        // on RPC entry) rather than `qpi_calls` (recorded after the
        // fault-injection block, which the fgv-a arm's injected
        // Internal short-circuits).
        assert!(
            store
                .calls
                .qpi_attempts_by_path
                .read()
                .unwrap()
                .contains_key(&dbg),
            "{tag}: the unwanted seed must still be attempted"
        );
    }
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// An UNRESOLVABLE wanted set (non-empty but matching no declared
/// output name — a `drv^bogus` root the gateway didn't validate) must
/// not invert the forgiveness gate. The wanted subset resolves to
/// nothing, so the complement "expected − wanted" would be EVERY
/// declared path — and a forgivable set of everything turns a total
/// fetch failure into `ok=true` with zero outputs present: the node
/// Completes vacuously and its dependents dispatch against missing
/// inputs. The conservative branch for an unresolvable wanted set is
/// "nothing is forgivable" — every seed failure fails the walk, the
/// pre-feature behaviour.
///
/// The node falls through the merge-time classification (the
/// `verifiable_wanted_paths` guard skips it), seeds Ready, and reaches
/// the substitute path via `batch_probe_cached_ready`'s all-declared
/// fallback — the one route where the spawn-time complement used to
/// run unguarded.
#[tokio::test]
async fn substitute_walk_unresolvable_wanted_set_forgives_nothing() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("fgv-bogus-out");
    // Indeterminate at probe time (treated optimistically → routed to
    // the detached fetch) but the GET fails — the only seed, and the
    // walk must NOT forgive it. Internal (non-retryable) rather than
    // the natural NotFound: a non-forgivable NotFound now burns the
    // full ~32 s retry ladder in real time before demoting, and what
    // this test covers — the conservative empty-forgivable-set branch
    // for an unresolvable wanted set — is decided before the walk ever
    // sees the error.
    store.state.indeterminate.write().unwrap().push(out.clone());
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, Ordering::SeqCst);

    let mut n = make_node("fgv-bogus");
    n.output_names = vec!["out".into()];
    n.expected_output_paths = vec![out.clone()];
    n.wanted_output_names = vec!["bogus".into()];
    merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    settle_substituting(&handle, &["fgv-bogus"]).await;

    assert_eq!(
        expect_drv(&handle, "fgv-bogus").await.status,
        DerivationStatus::Ready,
        "an unresolvable wanted set must forgive NOTHING — the only \
         seed's fetch failure must fail the walk and demote to a \
         from-source dispatch, not Complete the node with zero outputs \
         present"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// The forgivable set is snapshotted at spawn time, but a detached
/// fetch can run for minutes — a second build merging during that
/// window can grow the node's wanted union to include an output the
/// in-flight walk is about to forgive. `handle_substitute_complete`
/// MUST re-check the walk's forgiven set against the node's CURRENT
/// wanted set and downgrade a stale `ok=true` to a revert; otherwise
/// the node Completes without an output build B positively wants.
///
/// The QPI gate parks the detached walk before its first fetch so the
/// second merge lands deterministically inside the spawn→complete
/// window.
#[tokio::test]
async fn substitute_complete_recheck_forgiven_against_grown_wanted_set() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("fgv-race-out");
    let dbg = test_store_path("fgv-race-debug");
    // out: substitutable (probe + GET agree). dbg: indeterminate at
    // probe time but the GET definitively misses — the seed the walk
    // forgives against the spawn-time wanted set {out}.
    store.state.substitutable.write().unwrap().push(out.clone());
    store.state.indeterminate.write().unwrap().push(dbg.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    let mk = |wanted: &[&str]| {
        let mut n = make_node("fgv-race");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Build A wants only {out} → forgivable = {P_debug} at spawn time.
    // The detached walk parks at the QPI gate.
    merge_dag(&handle, Uuid::new_v4(), vec![mk(&["out"])], vec![], false).await?;
    wait_for_status(&handle, "fgv-race", DerivationStatus::Substituting).await;

    // Build B merges mid-fetch and wants {debug} → the union grows to
    // {debug, out} while the walk still holds the spawn-time snapshot.
    merge_dag(&handle, Uuid::new_v4(), vec![mk(&["debug"])], vec![], false).await?;
    barrier(&handle).await;

    // Release the walk: P_out substitutes, P_debug GETs NotFound and is
    // forgiven against the STALE forgivable set → the task posts
    // ok=true with forgiven=[P_debug].
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    settle_substituting(&handle, &["fgv-race"]).await;

    assert_ne!(
        expect_drv(&handle, "fgv-race").await.status,
        DerivationStatus::Completed,
        "a seed forgiven against the spawn-time wanted set that became \
         wanted mid-fetch must downgrade the completion to a revert — \
         build B would otherwise observe a Completed node missing an \
         output it asked for"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// Once a forgiven seed has triggered a downgrade (it became wanted
/// mid-fetch), it must NEVER be treated as forgivable again in later
/// walks of the substitution chain that recorded it — even if the build
/// that wanted it later goes terminal mid-chain. Without this, the walk
/// chain could oscillate forgivable↔wanted indefinitely under build
/// churn (the live effective wanted set shrinks when a build goes
/// terminal and re-grows when a new one merges); with it, every
/// downgrade permanently consumes one of the node's finitely many
/// declared outputs for the rest of that chain, so the chain is
/// bounded. (Once the chain ends, the bookkeeping is dropped — the
/// chain-scoping tests below cover that half.)
///
/// Staging: build B wants {out}; walk 1 forgives P_debug's failure;
/// build C (wanting {debug}) merges mid-fetch → the completion is
/// downgraded (P_debug is now wanted by a live build) and P_debug's
/// forgiveness is spent. C is then cancelled, so no live build wants
/// P_debug anymore. The next probe re-spawns the walk: P_debug must NOT
/// be forgivable (it already triggered a downgrade), so its genuine
/// failure fails the walk and the node demotes for a from-source
/// dispatch — it must NOT complete.
#[tokio::test]
async fn substitute_downgrade_never_forgives_the_same_path_twice() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("nfg-out");
    let dbg = test_store_path("nfg-debug");
    // P_out: substitutable (probe + GET agree; the mock GET does not
    // materialize it locally, so every later probe still sees it as
    // missing-but-substitutable and a re-walk is spawned each pass).
    // P_debug: indeterminate at probe time; its GET always fails with a
    // non-retryable Internal.
    store.state.substitutable.write().unwrap().push(out.clone());
    store.state.indeterminate.write().unwrap().push(dbg.clone());
    store
        .faults
        .fail_qpi_internal_paths
        .write()
        .unwrap()
        .insert(dbg.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    let mk = |wanted: &[&str]| {
        let mut n = make_node("nfg-drv");
        // aarch64 with only an x86_64 worker connected: the node can
        // never dispatch from source, so the final state isolates the
        // walk verdict.
        n.system = "aarch64-linux".into();
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Build B wants only {out} → pending-substitute → walk 1 spawned
    // with forgivable = {P_debug}, parked at the QPI gate.
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(&handle, build_b, vec![mk(&["out"])], vec![], false).await?;
    wait_for_status(&handle, "nfg-drv", DerivationStatus::Substituting).await;

    // Build C merges mid-fetch and wants {debug}.
    let build_c = Uuid::new_v4();
    let _ev_c = merge_dag(&handle, build_c, vec![mk(&["debug"])], vec![], false).await?;
    barrier(&handle).await;

    // Release walk 1: P_out substitutes, P_debug fails and is forgiven
    // against the spawn-time forgivable set → the task posts ok=true
    // with forgiven=[P_debug] → the handler downgrades (P_debug is now
    // wanted by the live build C); that downgrade spends P_debug's
    // forgiveness for this node.
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    settle_substituting(&handle, &["nfg-drv"]).await;
    assert_eq!(
        expect_drv(&handle, "nfg-drv").await.status,
        DerivationStatus::Ready,
        "precondition: the downgraded completion reverts to Ready for \
         re-substitution of the delta"
    );

    // Build C goes terminal: nothing live wants P_debug anymore.
    let (cancel_tx, cancel_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: build_c,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: cancel_tx,
        })
        .await?;
    assert!(cancel_rx.await??, "cancel C");

    // Walk 1's successful fetch materialized P_out in the local store —
    // a re-probe right now would legitimately complete the node for the
    // live demand ({out} present) without ever re-walking. Simulate a GC
    // of P_out between the walks (it stays substitutable upstream) so
    // the next probe must re-spawn the walk and the spent forgiveness is
    // what decides the verdict.
    store.state.paths.write().unwrap().remove(&out);

    // The next dispatch pass re-probes the Ready node and re-spawns the
    // walk. P_debug already triggered a downgrade, so it must NOT be
    // forgivable — its (genuine) failure must fail the walk and demote
    // the node, NOT complete it without the output.
    tick(&handle).await?;
    settle_substituting(&handle, &["nfg-drv"]).await;

    assert_eq!(
        expect_drv(&handle, "nfg-drv").await.status,
        DerivationStatus::Ready,
        "a path that already triggered a downgrade must never be \
         forgiven again for this node — the re-spawned walk must fail on \
         P_debug and demote, not complete the node without it"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B keeps building from source; the node must not complete behind \
         a forgiveness that was already spent"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.merge.substitute-topdown+12]
// r[verify sched.substitute.detached+5]
/// Downgraded completion (a forgiven seed became wanted mid-fetch) on a
/// topdown-pruned CHILDLESS root: the dependency closure was dropped
/// from the submission, so the generic revert (childless ⇒ vacuously
/// Ready, `substitute_tried` left unset) re-arms exactly the doomed
/// dispatch the topdown fail-fast arm exists to prevent — the next pass
/// probes, finds the now-wanted path definitively missing, and routes
/// the root to a worker that ENOENTs on its never-scheduled inputDrvs.
///
/// The downgrade must instead re-attempt the delta with the corrected
/// forgivable set (and fail-fast with the resubmit-directing error if
/// it is genuinely missing), or take the fail-fast arm directly. It
/// must NOT leave the node dispatchable from source.
///
/// Staged like `test_topdown_pruned_flag_ignored_after_full_merge_adds_
/// deps`: `debug_force_status`/`debug_set_topdown_pruned`/
/// `debug_set_output_paths` + an injected `SubstituteComplete` (the
/// actor only checks `status == Substituting`, so an injected message
/// is indistinguishable from the spawned task's).
#[tokio::test]
async fn substitute_downgrade_on_topdown_pruned_childless_root_does_not_dispatch_from_source()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("fnw-td-out");
    let dbg = test_store_path("fnw-td-debug");
    let mk = |wanted: &[&str]| {
        let mut n = make_node("fnw-td");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Build A wants only {out}. No store lanes are seeded yet, so the
    // merge classifies nothing as substitutable and R seeds Ready.
    let build_a = Uuid::new_v4();
    merge_dag(&handle, build_a, vec![mk(&["out"])], vec![], false).await?;

    // Stage: an earlier topdown prune left R mid-fetch — Substituting,
    // topdown_pruned, childless, output_paths stashed by the spawn.
    assert!(
        handle
            .debug_force_status("fnw-td", DerivationStatus::Substituting)
            .await?
    );
    assert!(handle.debug_set_topdown_pruned("fnw-td", true).await?);
    assert!(
        handle
            .debug_set_output_paths("fnw-td", vec![out.clone(), dbg.clone()])
            .await?
    );

    // Build B merges mid-fetch and wants {debug}: the union grows to
    // {debug, out} while the (staged) walk still holds the spawn-time
    // forgivable snapshot.
    let build_b = Uuid::new_v4();
    merge_dag(&handle, build_b, vec![mk(&["debug"])], vec![], false).await?;

    // At any post-downgrade re-attempt the now-wanted P_debug is
    // definitively missing: GETs fail hard (Internal — no retry
    // ladder) and FMP keeps reporting it missing-not-substitutable.
    // P_out stays fetchable so a corrected re-walk fails on the delta,
    // not on the output the first walk already handled.
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .fail_qpi_internal_paths
        .write()
        .unwrap()
        .insert(dbg.clone());

    // The staged walk posts ok=true with the seed it forgave against
    // the stale {out}-only wanted set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "fnw-td".into(),
            ok: true,
            forgiven: vec![dbg.clone()],
        })
        .await?;
    settle_substituting(&handle, &["fnw-td"]).await;
    tick(&handle).await?;

    let r = expect_drv(&handle, "fnw-td").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "downgraded substitute completion left a topdown-pruned childless root \
         dispatchable from source (status={:?}, substitute_tried={}); its dep \
         closure was never scheduled, so a worker dispatch ENOENTs on inputDrvs",
        r.status,
        r.substitute_tried
    );
    // Both interested builds must get the crisp resubmit-directing
    // fail-fast (directly, or after a corrected re-walk finds the delta
    // genuinely missing) — not hang Active behind a doomed dispatch.
    for (label, build_id) in [("A", build_a), ("B", build_b)] {
        let s = query_status(&handle, build_id).await?;
        assert_eq!(
            s.state,
            rio_proto::types::BuildState::Failed as i32,
            "build {label} must be failed by the topdown fail-fast after the \
             downgrade (the now-wanted delta is unfetchable), not left \
             Active/dispatched; got state={} error={:?}",
            s.state,
            s.error_summary
        );
        assert!(
            s.error_summary.contains("resubmit"),
            "build {label} should carry the resubmit-directing fail-fast error; \
             got {:?}",
            s.error_summary
        );
    }
    Ok(())
}

// r[verify sched.merge.substitute-topdown+12]
/// Fail-open carve-out: when the dispatch-time store probe errors out
/// (RPC failure / timeout), every other Ready node keeps the existing
/// fail-open behaviour and dispatches — but a CHILDLESS topdown-pruned
/// node must not: without a definitive verdict it can neither be
/// completed inline, routed to substitution, nor fail-fasted, and a
/// from-source dispatch is the known-likely-doomed case the guard
/// exists to prevent. It must be deferred for this pass (left
/// Ready/Queued for the next probe), with no WorkAssignment sent.
#[tokio::test]
async fn topdown_pruned_childless_node_not_dispatched_when_probe_fails_open() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Merge while the store is healthy (nothing substitutable) so the
    // node simply seeds Ready; no worker yet, so nothing dispatches.
    let mut node = make_node("tdfo-r");
    node.expected_output_paths = vec![test_store_path("tdfo-r-out")];
    let build_id = Uuid::new_v4();
    // Hold the event receiver: the test-build orphan watcher (zero
    // grace) auto-cancels an unwatched Active build on the second Tick.
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Stage the post-prune / post-failover shape (childless + flagged),
    // then break the probe: FindMissingPaths now returns Unavailable.
    assert!(handle.debug_set_topdown_pruned("tdfo-r", true).await?);
    store
        .faults
        .fail_find_missing
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // A puller is available — the fail-open path has somewhere to go if
    // the carve-out is missing.
    tick(&handle).await?;
    tick(&handle).await?;

    // No from-source delivery for the flagged childless node on a
    // fail-open pass: a pull for it must not deliver.
    let pull = try_pull_attempt(&handle, "tdfo-r").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "a childless topdown-pruned node must not be dispatched from source \
         on a fail-open (probe error) pass; got {pull:?}"
    );
    // The node is merely deferred — still schedulable once the store
    // answers again — and the build is still alive.
    let d = expect_drv(&handle, "tdfo-r").await;
    assert!(
        matches!(d.status, DerivationStatus::Ready | DerivationStatus::Queued),
        "deferred node should stay Ready/Queued for the next probe; got {:?}",
        d.status
    );
    assert_eq!(
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "fail-open deferral must not fail the build"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+12]
/// Two childless topdown-pruned roots, both sole-interest of the SAME
/// build, both with a wanted output definitively missing and not
/// substitutable, land in `to_fail_fast` together in one dispatch pass
/// (the post-failover multi-target shape). The first iteration's
/// `cancel_build_derivations` already terminalizes the second node
/// (sole-interest, not yet dispatched ⇒ DependencyFailed, persisted,
/// interest stripped). The second iteration must NOT resurrect it:
/// pre-fix it re-parked the node to Queued (DependencyFailed→Queued is
/// a valid reprobe edge) and overwrote the terminal PG row, leaving a
/// non-terminal zero-interest dep-less orphan that no reap collects and
/// every recovery reloads. The build must fail exactly once with the
/// resubmit-directing error and both nodes must stay terminal.
#[tokio::test]
async fn topdown_fail_fast_does_not_resurrect_node_terminalized_by_prior_iteration() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // One build, two dep-less targets. Nothing is substitutable, so the
    // merge seeds both Ready (no prune: the submission has no edges).
    let mk = |tag: &str| {
        let mut n = make_node(tag);
        n.expected_output_paths = vec![test_store_path(&format!("{tag}-out"))];
        n
    };
    let build_id = Uuid::new_v4();
    // Hold the event receiver: the test-build orphan watcher (zero
    // grace) auto-cancels an unwatched Active build on the second Tick.
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![mk("tdm-a"), mk("tdm-b")],
        vec![],
        false,
    )
    .await?;

    // Stage the post-failover shape: both roots carry the restored
    // pruned marker and are childless.
    assert!(handle.debug_set_topdown_pruned("tdm-a", true).await?);
    assert!(handle.debug_set_topdown_pruned("tdm-b", true).await?);

    // Drive a fresh Tick: it advances the probe generation, and that
    // batch probe finds both wanted outputs missing and unsubstitutable
    // → both roots take the fail-fast in ONE pass.
    tick(&handle).await?;
    barrier(&handle).await;

    // The build fails exactly once, with the resubmit-directing error.
    let s = query_status(&handle, build_id).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "build must fail fast; got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s.error_summary
    );
    assert_eq!(
        recorder.get("rio_scheduler_topdown_substitute_fail_total{}"),
        1,
        "the fail-fast must run once for the build's first root and skip the \
         node the cancel already terminalized"
    );

    // Neither node may be resurrected: both stay terminal in memory and
    // in PG (the cancel's DependencyFailed verdict is the record of why
    // the build failed). Pre-fix one of them ended Queued with zero
    // interested builds — a permanent orphan.
    for tag in ["tdm-a", "tdm-b"] {
        let d = expect_drv(&handle, tag).await;
        assert!(
            d.status.is_terminal(),
            "{tag} must stay terminal after the fail-fast pass; got {:?}",
            d.status
        );
        let (pg_status,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = $1")
                .bind(tag)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(
            pg_status, "dependency_failed",
            "{tag}'s PG row must keep the terminal verdict, not be overwritten \
             back to a schedulable status"
        );
    }
    // No spurious from-source delivery of either node happened: a pull
    // for either must not deliver.
    for tag in ["tdm-a", "tdm-b"] {
        let pull = try_pull_attempt(&handle, tag).await;
        assert!(
            !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
            "neither failed-fast root may be dispatched from source; {tag} got {pull:?}"
        );
    }
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// Downgraded completion (a forgiven seed became wanted mid-fetch) on a
/// node whose dependency is Poisoned (the I-094 reprobe lane):
/// `revert_target_for` says DependencyFailed and the generic revert's
/// DependencyFailed arm runs `terminal_failure_epilogue` — terminally
/// failing EVERY interested build: the one whose entire wanted subset
/// was just successfully fetched AND the one whose newly-wanted seed
/// has never had a single real attempt (forgivable seeds are forgiven
/// on their FIRST failure). That contradicts the downgrade's stated
/// intent — "the next pass re-substitutes the delta" — because there is
/// no next pass out of DependencyFailed.
///
/// The newly-wanted path must get a real substitution attempt (a
/// re-spawned walk with the corrected forgivable set) before any
/// terminal verdict; the build whose wanted subset was fully fetched
/// must not be failed by the downgrade itself.
#[tokio::test]
async fn substitute_downgrade_with_poisoned_dep_reattempts_delta_before_failing_builds()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let p_out = test_store_path("fnw-dep-out");
    let p_dbg = test_store_path("fnw-dep-debug");
    let mk_p = |wanted: &[&str]| {
        let mut n = make_node("fnw-dep-p");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![p_out.clone(), p_dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Build A submits P→C; C is the dep that will be terminally failed.
    let build_a = Uuid::new_v4();
    let mut c = make_node("fnw-dep-c");
    c.expected_output_paths = vec![test_store_path("fnw-dep-c-out")];
    merge_dag(
        &handle,
        build_a,
        vec![mk_p(&["out"]), c],
        vec![make_test_edge("fnw-dep-p", "fnw-dep-c")],
        false,
    )
    .await?;
    // Build B wants only {out} — its entire wanted subset is what the
    // staged walk fetched.
    let build_b = Uuid::new_v4();
    merge_dag(&handle, build_b, vec![mk_p(&["out"])], vec![], false).await?;

    // Stage the I-094 shape: C terminally failed while P is mid-walk
    // (Substituting), with P's spawn-time output_paths stashed.
    assert!(handle.debug_force_poisoned("fnw-dep-c", 0).await?);
    assert!(
        handle
            .debug_force_status("fnw-dep-p", DerivationStatus::Substituting)
            .await?
    );
    assert!(
        handle
            .debug_set_output_paths("fnw-dep-p", vec![p_out.clone(), p_dbg.clone()])
            .await?
    );

    // Build D merges mid-fetch and wants {debug}: the union grows while
    // the walk still holds the spawn-time forgivable snapshot.
    let build_d = Uuid::new_v4();
    merge_dag(&handle, build_d, vec![mk_p(&["debug"])], vec![], false).await?;

    // The delta IS fetchable upstream — a corrected re-walk succeeds.
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(p_out.clone());
        subs.push(p_dbg.clone());
    }

    // The staged walk posts ok=true, having forgiven P_debug against
    // the stale {out}-only wanted set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "fnw-dep-p".into(),
            ok: true,
            forgiven: vec![p_dbg.clone()],
        })
        .await?;
    settle_substituting(&handle, &["fnw-dep-p"]).await;
    barrier(&handle).await;

    // No interested build may be terminally failed on the heels of the
    // downgrade: B's entire wanted subset was fetched, and D's newly-
    // wanted output is fetchable — a corrected re-walk completes both.
    for (label, build_id) in [
        ("B (wanted subset fully fetched)", build_b),
        ("D (newly-wanted delta, never attempted)", build_d),
    ] {
        let s = query_status(&handle, build_id).await?;
        assert_ne!(
            s.state,
            rio_proto::types::BuildState::Failed as i32,
            "build {label} was terminally failed by the downgrade (error={:?}) — \
             terminal_failure_epilogue ran before the newly-wanted path got a \
             single real substitution attempt",
            s.error_summary
        );
    }
    // The downgrade must not hand the node to the DependencyFailed
    // terminal arm before the delta has been re-attempted.
    let p = expect_drv(&handle, "fnw-dep-p").await;
    assert_ne!(
        p.status,
        DerivationStatus::DependencyFailed,
        "the downgraded completion took the DependencyFailed terminal arm \
         instead of re-attempting the newly-wanted delta"
    );
    // And the delta must actually have been attempted: the re-spawned
    // walk's fetch for P_debug reached the store.
    assert!(
        store.calls.qpi_calls.read().unwrap().contains(&p_dbg),
        "the newly-wanted path must get a real substitution attempt (no fetch \
         for it ever reached the store)"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// Same chain-scoping property as the test above, but chain 1 ends
/// through the OTHER non-substitution completion path: after the
/// downgrade, the build that wanted the trigger path is cancelled, so
/// the next pass finds every live-wanted output already present and
/// completes the node inline from the store — the delta re-walk never
/// runs. The spent-forgiveness bookkeeping must not survive that
/// completion into a later substitution chain.
#[tokio::test]
async fn substitute_inline_store_completion_clears_spent_forgiveness() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("nfg-ic-out");
    let dbg = test_store_path("nfg-ic-debug");
    store.state.substitutable.write().unwrap().push(out.clone());
    store.state.indeterminate.write().unwrap().push(dbg.clone());
    store
        .faults
        .fail_qpi_internal_paths
        .write()
        .unwrap()
        .insert(dbg.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    let mk = |wanted: &[&str]| {
        let mut n = make_node("nfg-ic-drv");
        // aarch64 with only an x86_64 worker connected: the node can
        // never dispatch from source, so the final state isolates the
        // walk verdict (mirrors the oscillation test).
        n.system = "aarch64-linux".into();
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Chain 1: B wants {out} → walk 1 (forgivable={P_debug}) parks at
    // the gate; C (wanting {debug}) merges mid-fetch; release → the
    // completion is downgraded and P_debug's forgiveness is spent.
    // (Keep the event receivers alive — the orphan-watcher cancels
    // unwatched Active builds in tests.)
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(&handle, build_b, vec![mk(&["out"])], vec![], false).await?;
    wait_for_status(&handle, "nfg-ic-drv", DerivationStatus::Substituting).await;
    let build_c = Uuid::new_v4();
    let _ev_c = merge_dag(&handle, build_c, vec![mk(&["debug"])], vec![], false).await?;
    barrier(&handle).await;
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    settle_substituting(&handle, &["nfg-ic-drv"]).await;
    assert_eq!(
        expect_drv(&handle, "nfg-ic-drv").await.status,
        DerivationStatus::Ready,
        "precondition: the downgraded completion reverts to Ready"
    );

    // C goes terminal before any re-walk: the only live demand is B's
    // {out}, which walk 1 already fetched — the next pass completes the
    // node inline from the store; the delta re-walk never runs.
    let (cancel_tx, cancel_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: build_c,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: cancel_tx,
        })
        .await?;
    assert!(cancel_rx.await??, "cancel C");
    tick(&handle).await?;
    assert_eq!(
        expect_drv(&handle, "nfg-ic-drv").await.status,
        DerivationStatus::Completed,
        "precondition: every live-wanted output is present locally — the \
         node completes inline from the store (chain 1 is over)"
    );

    // Later: P_out is GC'd and build D (wanting only {out}) merges. The
    // stale-Completed verify spawns a NEW chain; nothing live wants
    // P_debug, so it must be forgiven and the node must complete by
    // substitution.
    store.state.paths.write().unwrap().remove(&out);
    let build_d = Uuid::new_v4();
    let _ev_d = merge_dag(&handle, build_d, vec![mk(&["out"])], vec![], false).await?;
    settle_substituting(&handle, &["nfg-ic-drv"]).await;

    assert_eq!(
        expect_drv(&handle, "nfg-ic-drv").await.status,
        DerivationStatus::Completed,
        "spent forgiveness must not survive an inline store completion \
         into a later substitution chain: P_debug is unwanted now, its \
         absence must be forgiven, and the node must complete by \
         substitution instead of demoting to a from-source dispatch"
    );
    assert_eq!(
        query_status(&handle, build_d).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build D's wanted output was substituted; it must complete"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.substitute.detached+5]
/// Same chain-scoping property as the two tests above, but chain 1 ends
/// through the merge-time cached-hit lane: after the downgrade the
/// trigger-wanting build is cancelled, and a NEW build re-merges the
/// same drv while the node is parked Ready — the merge re-probe sees
/// every live-wanted output already present locally and
/// `apply_cached_hits` completes the node during the merge itself; no
/// dispatch pass and no further walk ever runs. The spent-forgiveness
/// bookkeeping must not survive that completion (or the
/// stale-Completed reset that opens the next chain) into a later
/// substitution chain.
#[tokio::test]
async fn substitute_merge_cached_hit_completion_clears_spent_forgiveness() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("nfg-mc-out");
    let dbg = test_store_path("nfg-mc-debug");
    store.state.substitutable.write().unwrap().push(out.clone());
    store.state.indeterminate.write().unwrap().push(dbg.clone());
    store
        .faults
        .fail_qpi_internal_paths
        .write()
        .unwrap()
        .insert(dbg.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    let mk = |wanted: &[&str]| {
        let mut n = make_node("nfg-mc-drv");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        n
    };

    // Chain 1: B wants {out} → walk 1 (forgivable={P_debug}) parks at
    // the gate; C (wanting {debug}) merges mid-fetch; release → the
    // completion is downgraded and P_debug's forgiveness is spent.
    // (Keep the event receivers alive — the orphan-watcher cancels
    // unwatched Active builds in tests.)
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(&handle, build_b, vec![mk(&["out"])], vec![], false).await?;
    wait_for_status(&handle, "nfg-mc-drv", DerivationStatus::Substituting).await;
    let build_c = Uuid::new_v4();
    let _ev_c = merge_dag(&handle, build_c, vec![mk(&["debug"])], vec![], false).await?;
    barrier(&handle).await;
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    settle_substituting(&handle, &["nfg-mc-drv"]).await;
    assert_eq!(
        expect_drv(&handle, "nfg-mc-drv").await.status,
        DerivationStatus::Ready,
        "precondition: the downgraded completion reverts to Ready"
    );

    // C goes terminal before any dispatch pass runs: the only live
    // demand left is B's {out}, which walk 1 already materialized in
    // the local store.
    let (cancel_tx, cancel_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: build_c,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: cancel_tx,
        })
        .await?;
    assert!(cancel_rx.await??, "cancel C");

    // Chain 1 ends in the MERGE path: build E (also wanting only {out})
    // re-merges the drv while the node is parked Ready. The re-probe
    // classifies it as a cached hit (every live-wanted output is
    // locally present) and `apply_cached_hits` completes it during the
    // merge — no dispatch pass, no walk, no worker.
    let build_e = Uuid::new_v4();
    let _ev_e = merge_dag(&handle, build_e, vec![mk(&["out"])], vec![], false).await?;
    assert_eq!(
        expect_drv(&handle, "nfg-mc-drv").await.status,
        DerivationStatus::Completed,
        "precondition: the re-merge completes the node through the \
         merge-time cached-hit lane (chain 1 is over)"
    );

    // Later: P_out is GC'd and build D (wanting only {out}) merges. The
    // stale-Completed verify resets the node and spawns a NEW chain;
    // nothing live wants P_debug, so it must be forgiven and the node
    // must complete by substitution.
    store.state.paths.write().unwrap().remove(&out);
    let build_d = Uuid::new_v4();
    let _ev_d = merge_dag(&handle, build_d, vec![mk(&["out"])], vec![], false).await?;
    settle_substituting(&handle, &["nfg-mc-drv"]).await;

    assert_eq!(
        expect_drv(&handle, "nfg-mc-drv").await.status,
        DerivationStatus::Completed,
        "spent forgiveness must not survive a merge-time cached-hit \
         completion into a later substitution chain: P_debug is unwanted \
         now, its absence must be forgiven, and the node must complete \
         by substitution instead of demoting to a from-source dispatch"
    );
    assert_eq!(
        query_status(&handle, build_d).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build D's wanted output was substituted; it must complete"
    );
    Ok(())
}

// r[verify sched.substitute.detached+5]
/// Cold path: A is NOT in `state.paths` (batch returns None) but IS
/// in `state.substitutable` (per-path QPI materializes it with
/// refs=[B]); B is seeded warm. Asserts the absent→QPI→push-refs arm
/// works and returns ok=true.
#[tokio::test]
async fn substitute_fetch_cold_seed_pushes_refs() -> TestResult {
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let a = test_store_path("cold-a");
    let b = test_store_path("cold-b");
    // A: substitutable-only — batch sees None, QPI materializes it.
    // MockStore's substitutable lane returns `..Default::default()` so
    // refs=[] — instead, seed A in `paths` AFTER the first batch via
    // direct seeding so the QPI arm reads refs=[B]. Simpler: A goes in
    // `paths` (warm) with refs=[B]; B is substitutable-only (cold).
    seed_with_refs(&store, &a, &[&b]);
    store.state.substitutable.write().unwrap().push(b.clone());

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![a.clone()],
        &Default::default(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;
    assert!(
        ok,
        "cold ref substituted via per-path QPI must return ok=true"
    );
    // B: batch=None (not in `paths`) → QPI hit (substitutable lane).
    assert!(
        store.calls.qpi_calls.read().unwrap().contains(&b),
        "cold ref B must reach per-path QPI (batch is local-only)"
    );
    Ok(())
}

// r[verify gw.activity.subst-progress]
/// Aggregate-progress invariants: across all `on_progress` emits,
/// `done <= expected` AND `done` is monotone non-decreasing — even when
/// (a) some paths return Ok(Some) without ever emitting Progress
/// (store-side moka hit / `AlreadyComplete`) and (b) within-path `done`
/// regresses (multi-upstream fallback or retry restarting at 0).
///
/// Pre-fix: A's no-callback `done_base += 1000` left `expected_total=0`;
/// B's first emit was `(1050, 100)` — 1050%. C's `[(160,200),(60,200)]`
/// emitted `(prev+160,…)` then `(prev+60,…)` — backward.
#[tokio::test]
async fn walk_substitute_closure_progress_monotone_and_bounded() -> TestResult {
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let a = test_store_path("aggprog-a");
    let b = test_store_path("aggprog-b");
    let c = test_store_path("aggprog-c");
    // A: nar_size=1000, NO ticks → store-side cache-hit shape (callback
    //    never fires). `done_base += 1000` must also grow `expected`.
    // B: nar_size=100, one tick (50,100) → first callback after A's
    //    no-callback completion; this is where >100% surfaced pre-fix.
    // C: nar_size=200, ticks (160,200) then (60,200) → within-stream
    //    `done` regression (multi-upstream fallback shape).
    {
        let mut t = store.state.subst_progress_ticks.write().unwrap();
        t.insert(a.clone(), (1000, vec![]));
        t.insert(b.clone(), (100, vec![(50, 100)]));
        t.insert(c.clone(), (200, vec![(160, 200), (60, 200)]));
    }
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .extend([a.clone(), b.clone(), c.clone()]);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let mut emits: Vec<(u64, u64)> = Vec::new();
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![a, b, c],
        &Default::default(),
        &auth,
        &shutdown,
        |done, expected, _| emits.push((done, expected)),
    )
    .await;
    assert!(ok);
    assert_eq!(emits.len(), 3, "B emits once, C emits twice; A emits zero");

    let mut prev_done = 0u64;
    for (done, expected) in &emits {
        assert!(
            done <= expected,
            "emit ({done}, {expected}) has done>expected → renders >100%"
        );
        assert!(
            *done >= prev_done,
            "emit done={done} < prev {prev_done} → bar jumps backward"
        );
        prev_done = *done;
    }
    Ok(())
}

// r[verify sched.substitute.detached+5]
/// merged_001: a hostile upstream returning > `MAX_SUBSTITUTE_CLOSURE`
/// references on a SINGLE path must trip the per-path cap check
/// immediately — not after the next BFS layer. Without the per-insert-
/// block check, a 10k-refs/node × 10k-node layer reaches ~100M strings
/// before the once-per-layer top-of-loop check fires. Proof of bound:
/// only the seed layer is batch-probed (1 BatchQPI), and the 60k-ref
/// frontier never reaches the per-path QPI loop — i.e. `visited` is
/// capped at `MAX_SUBSTITUTE_CLOSURE + one path's references`.
#[tokio::test]
async fn walk_closure_hostile_refs_bounds_memory() -> TestResult {
    use rio_common::limits::MAX_SUBSTITUTE_CLOSURE;
    let (store, client, _task) = rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let a = test_store_path("hostile-a");
    let n_refs = MAX_SUBSTITUTE_CLOSURE + 10_000;
    let refs: Vec<String> = (0..n_refs)
        .map(|i| test_store_path(&format!("hostile-ref-{i:05}")))
        .collect();
    let ref_strs: Vec<&str> = refs.iter().map(String::as_str).collect();
    seed_with_refs(&store, &a, &ref_strs);

    let shutdown = rio_common::signal::Token::new();
    let auth = crate::actor::dispatch::SubstituteAuth::Jwt(Vec::new());
    let (ok, _) = crate::actor::dispatch::walk_substitute_closure(
        &client,
        vec![a.clone()],
        &Default::default(),
        &auth,
        &shutdown,
        |_, _, _| {},
    )
    .await;

    assert!(!ok, "hostile-ref path must trip closure cap → ok=false");
    assert_eq!(
        store
            .calls
            .batch_qpi_calls
            .load(std::sync::atomic::Ordering::SeqCst),
        1,
        "cap check must fire after the first layer's refs, before the \
         60k-ref frontier reaches a second BatchQPI"
    );
    assert!(
        store.calls.qpi_calls.read().unwrap().is_empty(),
        "no per-path QPI for hostile refs — early return bounds memory"
    );
    Ok(())
}

/// merged_bug_017: SolveCache is keyed on `model_key_hash` and bounded
/// by "|live SlaEstimator keys| × |overrides|" — but the bound only
/// holds if LRU eviction propagates. Without the `on_evict` hook, the
/// solve_cache entry for an evicted fit is orphaned forever
/// (`solve_intent_for` short-circuits on `cached(k) == None` before
/// reaching `get_or_insert_with`).
#[tokio::test]
async fn solve_cache_evicted_with_lru() {
    use crate::sla::solve::model_key_hash;
    let db = TestDb::new(&MIGRATOR).await;
    let mut cfg = test_hw_sla_config();
    cfg.max_keys_per_tenant = 3;
    let mut actor = bare_actor_cfg(
        db.pool.clone(),
        DagActorConfig {
            sla: cfg,
            ..Default::default()
        },
    );
    actor.sla_tiers = actor.sla_config.solve_tiers();
    actor.cost_table.write().set_resolved_global((
        actor.sla_config.max_cores.unwrap() as u32,
        actor.sla_config.max_mem.unwrap(),
    ));
    actor.sla_ceilings = crate::sla::solve::Ceilings::from_resolved(
        &actor.sla_config,
        actor.cost_table.read().resolved_global(),
    );
    let mut m = std::collections::HashMap::new();
    m.insert("intel-6".into(), 1.0);
    m.insert("intel-7".into(), 1.4);
    m.insert("intel-8".into(), 2.0);
    actor
        .sla_estimator
        .seed_hw(crate::sla::hw::HwTable::from_map(m));

    // Fill LRU to cap and populate solve_cache for each.
    for p in ["p0", "p1", "p2"] {
        seed_fit(&actor, p);
        actor.test_inject_ready(p, Some(p), "x86_64-linux", false);
        let _ = solve_intent(&actor, actor.dag.node(p).unwrap());
    }
    assert_eq!(actor.solve_cache.len(), 3, "solve_cache one per fit");

    // cap+1 insert via the SAME `on_evict` wiring `maybe_refresh_estimator`
    // uses (housekeeping.rs). LRU evicts the least-recently-used fit;
    // solve_cache must drop its memo.
    let solve_cache = std::sync::Arc::clone(&actor.solve_cache);
    let p3 = make_fit("p3");
    actor.sla_estimator.insert(&p3.key.clone(), p3, |k| {
        solve_cache.remove_model_key(model_key_hash(k))
    });
    assert_eq!(
        actor.solve_cache.len(),
        2,
        "evicted ModelKey's solve_cache entry dropped via on_evict"
    );

    // Steady-state: solving for the surviving fits + p3 stays ≤ cap.
    seed_fit(&actor, "p3");
    actor.test_inject_ready("p3", Some("p3"), "x86_64-linux", false);
    let _ = solve_intent(&actor, actor.dag.node("p3").unwrap());
    assert!(
        actor.solve_cache.len() <= 3,
        "solve_cache bounded by max_keys_per_tenant"
    );
}

/// merged_bug_028: `inputs_gen` was an `AtomicU64` whose `bump()` was
/// unconditional every 60s, so ε_h re-rolled before Karpenter could
/// provision an explore node. Third-strike redesign: `inputs_gen` is
/// **derived** from `(HwTable, CostTable)` solve-relevant projection;
/// nobody bumps. merged_bug_018 / fourth-strike Option 1: that
/// projection hashed bit-exact f64 EMA *state* (diverging) instead of
/// the converging *signal* solve reads — quantize. This asserts the
/// projection's stability under noise-band perturbation, NOT just
/// bit-identical re-inserts.
// r[verify sched.sla.hw-class.epsilon-explore+6]
#[tokio::test]
async fn inputs_gen_stable_across_noop_refresh() -> TestResult {
    use crate::sla::solve::SolveInputs;
    let db = TestDb::new(&MIGRATOR).await;
    let sdb = SchedulerDb::new(db.pool.clone());
    let est = crate::sla::SlaEstimator::for_test(&test_hw_sla_config());
    let mut cost = crate::sla::cost::CostTable::default();
    let derive = |est: &crate::sla::SlaEstimator, cost: &crate::sla::cost::CostTable| {
        SolveInputs {
            hw: &est.hw_table(),
            cost,
        }
        .inputs_gen()
    };
    let seed_hw = |pod: String, tenant: String, alu: f64| {
        sqlx::query(
            "INSERT INTO hw_perf_samples \
             (hw_class, pod_id, submitting_tenant, factor) \
             VALUES ('intel-7', $1, $2, jsonb_build_object('alu', $3::float8))",
        )
        .bind(pod)
        .bind(tenant)
        .bind(alu)
        .execute(&db.pool)
    };

    // 10 refreshes, no hw_perf rows → solve_relevant_hash unchanged →
    // derived inputs_gen identical.
    let g0 = derive(&est, &cost);
    for _ in 0..10 {
        est.refresh(&sdb, &[], |_| {}).await?;
    }
    assert_eq!(derive(&est, &cost), g0, "no-op refresh → inputs_gen stable");

    // PREFIX: ≥FLEET_MEDIAN_MIN_TENANTS (5) distinct submitting_tenant
    // rows — else hw.rs cross_tenant_median pins factor=[1.0;K] and the
    // noise-band assertions below are vacuous. pod_ids 0→5 crosses
    // HW_MIN_PODS → trust bool flips → key enters hash → inputs_gen
    // changes. factor[0] = median(5× 1.4) = 1.4 → bucket 140.
    for i in 0..5 {
        seed_hw(format!("pod-{i}"), format!("t{i}"), 1.4).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    let g1 = derive(&est, &cost);
    assert_ne!(g1, g0, "0→5 pods crosses trust threshold → derived change");

    // merged_bug_011 regression: pod_ids 5→6, factor bit-identical →
    // inputs_gen UNCHANGED. The old `content_hash` hashed raw `pod_ids`
    // and would have changed here.
    seed_hw("pod-x".into(), "t0".into(), 1.4).await?;
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_eq!(
        derive(&est, &cost),
        g1,
        "pod_ids 5→6 within trusted, factor unchanged → inputs_gen UNCHANGED"
    );

    // (a) merged_bug_018: 5 NEW tenants at alu=1.401 (NOT bit-identical)
    // → median of 10 tenant-medians = xs[5] = 1.401. Bit-exact hash
    // would change; quantized (1.401·100).round()=140=(1.4·100).round()
    // → UNCHANGED. This is the noise-band assertion the prior test
    // (bit-identical 1.4 re-insert) missed.
    for i in 5..10 {
        seed_hw(format!("pod-{i}"), format!("t{i}"), 1.401).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_eq!(
        derive(&est, &cost),
        g1,
        "factor 1.4→1.401 within 1% bucket → inputs_gen UNCHANGED"
    );

    // (b) cost-side: λ hashed as the converging `lambda_for` quotient,
    // NOT diverging (num, den) sums. Steady 600s exposure with
    // interrupts at LAMBDA_SEED rate → λ̂ stays exactly SEED (the
    // Gamma-Poisson `(a·s + b·s)/(a+b)=s` identity) regardless of how
    // (num, den, node_count) grow per tick. First tick adds the λ key
    // → new baseline g2; second tick → (num,den) bit-change, λ̂ doesn't.
    let seed_tick = |at: f64| {
        sqlx::query(
            "INSERT INTO interrupt_samples (cluster, hw_class, kind, value, at) \
             VALUES ('', 'intel-7', 'exposure', 600, to_timestamp($1)), \
                    ('', 'intel-7', 'interrupt', $2, to_timestamp($1))",
        )
        .bind(at)
        .bind(600.0 * crate::sla::cost::LAMBDA_SEED)
        .execute(&db.pool)
    };
    seed_tick(1000.0).await?;
    cost.refresh_lambda(&sdb).await?;
    let g2 = derive(&est, &cost);
    assert_ne!(g2, g1, "λ key entered → cost-side hash changes");
    seed_tick(1600.0).await?;
    cost.refresh_lambda(&sdb).await?;
    assert_eq!(
        derive(&est, &cost),
        g2,
        "steady refresh_lambda → λ̂ converges, (num,den) diverge → UNCHANGED"
    );

    // (c) Real change: factor moves >1% (out of bucket). Clear and
    // re-seed 5 tenants at alu=1.5 → bucket 150 ≠ 140 → CHANGED.
    sqlx::query("DELETE FROM hw_perf_samples")
        .execute(&db.pool)
        .await?;
    for i in 0..5 {
        seed_hw(format!("pod-c{i}"), format!("t{i}"), 1.5).await?;
    }
    est.refresh(&sdb, &[], |_| {}).await?;
    assert_ne!(
        derive(&est, &cost),
        g2,
        "factor 1.401→1.5 crosses 1% bucket → inputs_gen CHANGED"
    );
    Ok(())
}

/// `compute_spawn_intents` carries `IceBackoff::masked_cells()` in the
/// snapshot. Pre-R24B6a `admin/spawn_intents.rs` hardcoded `vec![]`, so
/// the controller's `cover_deficit` mask never received the scheduler's
/// accumulated ladder and rediscovered ICE per cell on every controller
/// restart (mb_001).
#[tokio::test]
async fn compute_spawn_intents_carries_ice_masked_cells() -> TestResult {
    use crate::sla::config::CapacityType;
    let db = TestDb::new(&MIGRATOR).await;
    let actor = bare_actor_sla(db.pool.clone());
    actor
        .ice
        .mark(&("hi-ebs-x86".to_string(), CapacityType::Spot));
    let snap = actor.compute_spawn_intents(&Default::default());
    assert!(
        snap.ice_masked_cells
            .contains(&"hi-ebs-x86:spot".to_string()),
        "masked cell flows to snapshot via cell_label: got {:?}",
        snap.ice_masked_cells
    );
    Ok(())
}

// r[verify sched.build.terminal-status-settled+2]
/// A dispatch-time store hit that fans out to a resident terminal build
/// must not mutate its served accounting and must not emit a
/// post-terminal `BuildProgress` to its watchers.
///
/// Staging: B1 builds X and succeeds (terminal, resident for the cleanup
/// window, interest retained on X). B2 re-merges X while X's output is
/// missing from the store, so the stale-Completed verify resets X to
/// Ready. The output then appears in the store and the next dispatch
/// sweep's batched Ready probe completes X as cached, fanning out to
/// every interested build — including terminal B1. B1's served
/// accounting (cached_derivations), its live event stream, and its
/// WatchBuild snapshot must all stay settled: a post-`BuildCompleted`
/// `BuildProgress` recomputed from the still-mutating DAG would reach
/// watchers with totals the finished build no longer describes.
#[tokio::test]
async fn test_terminal_build_frozen_on_dispatch_store_hit() -> TestResult {
    use rio_proto::types::build_event::Event;

    // Setup: mock store. B1's broadcast receiver is held for the whole
    // test so every event B1's watchers would see is observable.
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), |_, _| {});

    let x_out = test_store_path("tfz-x-out");

    // B1: single node X. Nothing in the store at merge time → X Ready.
    let b1 = Uuid::new_v4();
    let mut x = make_node("tfz-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut b1_events = merge_dag(&handle, b1, vec![x.clone()], vec![], false).await?;
    barrier(&handle).await;

    // Build X via the pull surface → B1 Succeeded.
    pull_complete_success(&handle, "tfz-x", &x_out).await?;
    wait_for_status(&handle, "tfz-x", DerivationStatus::Completed).await;
    let settled = query_status(&handle, b1).await?;
    assert_eq!(
        settled.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "precondition: B1 finished"
    );
    assert_eq!(
        settled.cached_derivations, 0,
        "precondition: B1 built X from source — no cache hits"
    );

    // Drain B1's stream up to (and including) its BuildCompleted. From
    // here on, no BuildProgress may follow for B1.
    let mut saw_completed = false;
    while let Ok(ev) = b1_events.try_recv() {
        if matches!(ev.event, Some(Event::Completed(_))) {
            saw_completed = true;
        }
    }
    assert!(saw_completed, "precondition: B1's BuildCompleted observed");

    // B2 re-merges X (+ a sibling root Z so B2 stays live). x_out is NOT
    // in the mock store → the stale-Completed verify resets X to Ready.
    // B1 (terminal, resident) keeps its interest in X.
    let b2 = Uuid::new_v4();
    let mut z = make_node("tfz-z");
    z.expected_output_paths = vec![test_store_path("tfz-z-out")];
    merge_dag(&handle, b2, vec![x, z], vec![], false).await?;
    barrier(&handle).await;
    let xs = expect_drv(&handle, "tfz-x").await;
    assert!(
        matches!(
            xs.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "precondition: stale-Completed verify must reset X (its output is \
         missing from the store); got {:?}",
        xs.status
    );

    // The output appears in the store → the next dispatch sweep's
    // batched Ready probe completes X as cached and fans out to ALL
    // interested builds — including terminal B1.
    store.seed_with_content(&x_out, b"x");
    tick(&handle).await?;
    barrier(&handle).await;

    // Sanity: the fan-out ran — the live build counted the store hit.
    let s2 = query_status(&handle, b2).await?;
    assert!(
        s2.cached_derivations >= 1,
        "the live build counts the dispatch-time store hit; got {}",
        s2.cached_derivations
    );

    // 1. The terminal build's served accounting is frozen.
    let after = query_status(&handle, b1).await?;
    assert_eq!(
        after.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "B1 stays terminal"
    );
    assert_eq!(
        after.cached_derivations, 0,
        "a terminal build's cached_derivations must not drift after the \
         terminal transition"
    );

    // 2. No post-terminal BuildProgress reached B1's live stream. The
    //    per-drv DerivationCached fan-out event is allowed (it is a fact
    //    about the derivation, not aggregate progress of the finished
    //    build); aggregate Progress is not.
    let mut post_terminal_progress = 0usize;
    while let Ok(ev) = b1_events.try_recv() {
        if matches!(ev.event, Some(Event::Progress(_))) {
            post_terminal_progress += 1;
        }
    }
    assert_eq!(
        post_terminal_progress, 0,
        "no BuildProgress after BuildCompleted may be emitted to a \
         terminal build's watchers (it would show totals shrunk by DAG \
         mutations the finished build no longer describes)"
    );

    // 3. The WatchBuild snapshot — what a re-attaching watcher would see
    //    — also reports the settled accounting, not the mutated DAG's.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id: b1,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (_rx, snapshot) = reply_rx.await??;
    let Some(Event::Snapshot(snap)) = snapshot.event else {
        panic!(
            "WatchBuild reply must carry a Snapshot, got {:?}",
            snapshot.event
        );
    };
    assert_eq!(
        snap.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "snapshot reports the settled terminal state"
    );
    assert_eq!(
        snap.cached_derivations, 0,
        "snapshot serves the settled accounting, not the mutated DAG's"
    );
    Ok(())
}
