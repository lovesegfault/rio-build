//! Dispatch: skip-ineligible, options propagation, interactive priority.

use super::*;

/// Dispatch should skip over derivations with no eligible worker (wrong
/// system or missing feature) instead of blocking the entire queue.
#[tokio::test]
async fn test_dispatch_skips_ineligible_derivation() -> TestResult {
    // Only x86_64 worker registered.
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("x86-only-worker", "x86_64-linux").await?;

    // Merge aarch64 derivation FIRST (goes to queue head), then x86_64.
    // With the old `None => break`, the aarch64 drv at head would block
    // the x86_64 drv from being dispatched.
    let build_arm = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_arm,
        vec![make_test_node("arm-hash", "aarch64-linux")],
        vec![],
        false,
    )
    .await?;

    let build_x86 = Uuid::new_v4();
    let p_x86 = test_drv_path("x86-hash");
    let _rx = merge_single_node(&handle, build_x86, "x86-hash", PriorityClass::Scheduled).await?;

    // x86_64 derivation should be dispatched despite aarch64 ahead of it.
    let dispatched_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(
        dispatched_path, p_x86,
        "x86_64 derivation should be dispatched even with ineligible aarch64 ahead in queue"
    );

    // aarch64 derivation should still be Ready (not stuck, not dispatched).
    let arm_info = expect_drv(&handle, "arm-hash").await;
    assert_eq!(
        arm_info.status,
        DerivationStatus::Ready,
        "aarch64 derivation should remain Ready (no eligible worker)"
    );
    Ok(())
}

/// Per-build BuildOptions (max_silent_time, build_timeout) must propagate
/// to the worker via WorkAssignment. Regression guard: without
/// propagation, all-zeros defaults would be sent.
#[tokio::test]
async fn test_build_options_propagated_to_worker() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("options-worker", "x86_64-linux").await?;

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

    // Worker should receive assignment with the build's options.
    let assignment = recv_assignment(&mut stream_rx).await;
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
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("trace-worker", "x86_64-linux").await?;

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

    let assignment = recv_assignment(&mut stream_rx).await;
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
    // P0537: delay capacity by not connecting the worker until after
    // both merges (zero-capacity workers are no longer expressible).
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

    // Now give capacity: connect worker → dispatch fires.
    let mut stream_rx = connect_executor(&handle, "dedup-worker", "x86_64-linux").await?;

    let assignment = recv_assignment(&mut stream_rx).await;
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
    // P0537: delay capacity by not connecting until after both merges.

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

    // Give capacity: connect worker → dispatch.
    let mut stream_rx = connect_executor(&handle, "upgrade-worker", "x86_64-linux").await?;

    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(
        assignment.traceparent, live_tp,
        "empty traceparent (recovery) should be upgraded by first live submitter"
    );
    drop(db);
    Ok(())
}

/// Interactive builds get a priority boost (+1e9 instead of the old
/// push_front). After a dependency completes, an Interactive build's
/// newly-ready derivation dispatches BEFORE already-queued Scheduled work.
///
/// Same observable behavior as the old push_front test; different
/// mechanism underneath (priority number vs queue position).
#[tokio::test]
async fn test_interactive_priority_boost() -> TestResult {
    // Worker with 1 slot so dispatch order is observable.
    let (_db, handle, _task, mut worker_rx) =
        setup_with_worker("prio-builder", "x86_64-linux").await?;

    // Build 1: Scheduled, 2 independent leaves (Q, R). Both queue immediately.
    // Only 1 dispatches (worker has 1 slot); the other stays queued.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_dag(
        &handle,
        build1,
        vec![make_node("prioQ"), make_node("prioR")],
        vec![],
        false,
    )
    .await?;

    // Build 2: Interactive, 2-node chain A → B. B is a leaf, A blocked.
    let p_prio_a = test_drv_path("prioA");
    let p_prio_b = test_drv_path("prioB");
    let build2 = Uuid::new_v4();
    let _rx2 = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build2,
            tenant_id: None,
            priority_class: PriorityClass::Interactive,
            nodes: vec![make_node("prioA"), make_node("prioB")],
            edges: vec![make_test_edge("prioA", "prioB")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // Worker connected before the merges, so the first assignment is one
    // of Q/R (NOT B — Interactive merged after Q/R were already
    // dispatched/queued). Complete until prioB is dispatched; then A
    // becomes newly-ready with INTERACTIVE_BOOST, and the NEXT dispatch
    // should be A (not a leftover Q/R).
    //
    // One-shot workers: each completion drains the worker; connect a
    // fresh one per iteration.
    let mut seen_paths = Vec::new();
    let mut wid = "prio-builder".to_string();
    for i in 0..4 {
        let Some(msg) = worker_rx.recv().await else {
            break;
        };
        let Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) = msg.msg else {
            continue;
        };
        let path = a.drv_path.clone();
        seen_paths.push(path.clone());
        complete_success(&handle, &wid, &path, &test_store_path("out")).await?;
        // Fresh worker for the next dispatch.
        wid = format!("prio-builder-{}", i + 1);
        worker_rx = connect_executor(&handle, &wid, "x86_64-linux").await?;
        // If we just completed B, the NEXT dispatch should be A (priority boost).
        if path == p_prio_b {
            // Regression guard: if INTERACTIVE_BOOST = 0, B is just
            // another leaf and dispatches FIFO after both Q and R. The
            // boost must have brought B forward past at least one
            // Scheduled leaf — otherwise the next-is-A assertion below
            // is vacuous (Q/R already drained, A is the only Ready).
            assert!(
                seen_paths.len() < 3,
                "prioB must dispatch before both Scheduled leaves drain \
                 (boost beats FIFO); history: {seen_paths:?}"
            );
            let next_a = recv_assignment(&mut worker_rx).await;
            assert_eq!(
                next_a.drv_path, p_prio_a,
                "Interactive newly-ready A should dispatch before queued Scheduled work. \
                 Dispatch history: {seen_paths:?}"
            );
            return Ok(());
        }
    }
    panic!("never dispatched prioB within 4 completions. Dispatch history: {seen_paths:?}");
}

// -----------------------------------------------------------------------------
// C1: Leader generation — Arc<AtomicU64>, single-load consistency
// -----------------------------------------------------------------------------

/// The generation in a WorkAssignment must equal the generation a
/// heartbeat would return at the same moment. Both read from the same
/// `Arc<AtomicU64>`. With no lease task running (no writer), both see
/// the init value.
///
/// This catches the previous design's hardcoded-1 bug: if
/// `HeartbeatResponse` and `WorkAssignment` used DIFFERENT generation
/// sources (one hardcoded, one from actor state), this test fails.
#[tokio::test]
async fn test_generation_consistent_between_heartbeat_and_assignment() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;

    let _ev = merge_single_node(&handle, Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;

    // Both sourced from the same Arc<AtomicU64>. No writer → both = 1.
    assert_eq!(assignment.generation, 1, "init value, no lease task");
    assert_eq!(handle.leader_generation(), 1);
    assert_eq!(
        assignment.generation,
        handle.leader_generation(),
        "WorkAssignment and HeartbeatResponse read the same atomic"
    );

    // The assignment_token embeds the generation too (format string).
    // Trailing suffix check — the token is "{executor_id}-{drv_hash}-{gen}",
    // drv_hash is variable-length so we can't split cleanly, but the
    // suffix is reliable.
    assert!(
        assignment.assignment_token.ends_with("-1"),
        "token embeds generation as suffix: {}",
        assignment.assignment_token
    );
    Ok(())
}

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
    // This IS the value HeartbeatResponse.generation would carry.
    assert_eq!(
        handle.leader_generation(),
        1,
        "gen=0 is proto-default (unset); gen=1 is the real first generation"
    );
    Ok(())
}

// -----------------------------------------------------------------------------
// PrefetchHint before WorkAssignment
// -----------------------------------------------------------------------------

/// PrefetchHint arrives BEFORE the WorkAssignment on the stream.
/// Worker starts warming while still parsing the .drv — a few
/// seconds of head start on multi-minute fetches.
///
/// Setup: two-node chain (child → parent). Dispatch the parent;
/// the hint should contain the child's output path (parent's input).
/// The child itself is a leaf (no children → no hint → the first
/// message for it IS the assignment).
#[tokio::test]
async fn test_prefetch_hint_before_assignment() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;

    // Two-node chain: child (leaf) → parent (depends on child).
    // merge_dag edges point parent→child (parent's input is child's output).
    //
    // make_test_node leaves expected_output_paths EMPTY by default —
    // most tests don't care about it. approx_input_closure DOES care
    // (that's what it iterates). Populate explicitly.
    let child_out = rio_test_support::fixtures::test_store_path("child-out");
    let mut child = make_node("child");
    child.expected_output_paths = vec![child_out.clone()];
    let parent = make_node("parent");
    let edge = make_test_edge("parent", "child");

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![edge],
        false,
    )
    .await?;

    // First message: child is a leaf (no DAG children → no inputs
    // to prefetch → send_prefetch_hint early-returns). So the FIRST
    // thing we get is the child's Assignment.
    let first = rx.recv().await.expect("first message");
    match first.msg {
        Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => {
            assert!(
                a.drv_path.contains("child"),
                "leaf dispatches first (no deps), no hint precedes it: {a:?}"
            );
        }
        other => panic!("expected Assignment for leaf child, got {other:?}"),
    }

    // Complete the child so parent becomes ready. w1 drains; connect w2.
    complete_success_empty(&handle, "w1", "child").await?;
    let mut rx = connect_executor(&handle, "w2", "x86_64-linux").await?;

    // Parent has one child in the DAG (the completed "child"). Its
    // approx_input_closure = child's expected_output_paths.
    //
    // A fresh worker connecting with a non-empty ready queue gets the
    // on_worker_registered initial PrefetchHint AND the dispatch-time
    // hint (became-idle inline dispatch races ahead of PrefetchComplete
    // and assigns via cold-fallback). Drain ≥1 hint, then the assignment.
    let mut got_hint = None;
    let asgn = loop {
        match rx.recv().await.expect("msg").msg {
            Some(rio_proto::types::scheduler_message::Msg::Prefetch(h)) => got_hint = Some(h),
            Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) => break a,
            other => panic!("expected Prefetch or Assignment, got {other:?}"),
        }
    };
    let hint = got_hint.expect("at least one PrefetchHint before Assignment");
    assert_eq!(
        hint.store_paths,
        vec![child_out],
        "hint = child's output path (parent's direct input via DAG children)"
    );
    assert!(asgn.drv_path.contains("parent"));

    Ok(())
}

/// Two assignments within the same dispatch pass carry the same
/// generation. This is the single-load-per-assignment guarantee —
/// without a lease writer, it's trivially true (nothing changes
/// between loads), but it exercises the dispatch.rs load-once path.
///
/// The REAL torn-read test (concurrent lease fetch_add racing with
/// dispatch) lives with the lease task. This is the structural
/// precursor: proves the single load is what gets used throughout
/// send_assignment.
#[tokio::test]
async fn test_generation_single_load_within_assignment() -> TestResult {
    // P0537: two workers so the same dispatch_ready pass produces two
    // assignments (was one 4-slot worker).
    let (_db, handle, _task, mut rx) = setup_with_worker("w1", "x86_64-linux").await?;
    let mut rx2 = connect_executor(&handle, "w2", "x86_64-linux").await?;

    // Two independent derivations in the same dispatch pass.
    let _ev1 = merge_single_node(&handle, Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
    let _ev2 = merge_single_node(&handle, Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

    let a1 = recv_assignment(&mut rx).await;
    let a2 = recv_assignment(&mut rx2).await;

    // Same generation across both, and token suffix agrees with field.
    // If the load happened twice per assignment (e.g., once for token,
    // once for the field), a concurrent writer could split them —
    // token says "-1", field says 2. No writer here, so this asserts
    // STRUCTURAL consistency (same local used for both), not concurrent
    // safety (that's the lease task's job).
    assert_eq!(a1.generation, a2.generation);
    let expected_suffix = format!("-{}", a1.generation);
    assert!(
        a1.assignment_token.ends_with(&expected_suffix),
        "token suffix matches generation field: {} ends with {}",
        a1.assignment_token,
        expected_suffix
    );
    assert!(
        a2.assignment_token.ends_with(&expected_suffix),
        "same suffix on both assignments (single load per dispatch)"
    );
    Ok(())
}

/// Dispatch pins input-closure paths; terminal unpins.
/// Verifies the end-to-end pin → unpin lifecycle via scheduler_
/// live_pins row count.
// r[verify sched.gc.live-pins]
#[tokio::test]
async fn test_pin_unpin_live_inputs_lifecycle() -> TestResult {
    let (db, handle, _task, mut stream_rx) = setup_with_worker("w-x9", "x86_64-linux").await?;

    // Two-node chain: child (leaf, no inputs) + parent (depends
    // on child). Parent's approx_input_closure = child's
    // expected_output_paths. Dispatch of PARENT should pin those.
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

    // Child dispatches first (leaf → Ready immediately).
    let assignment_child = recv_assignment(&mut stream_rx).await;
    assert!(assignment_child.drv_path.contains("x9-child"));

    // Child is leaf → approx_input_closure empty → no pin.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-child'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 0, "leaf drv (no inputs) should not pin anything");

    // Complete child → parent becomes Ready → dispatched → pinned.
    // One-shot: w-x9 drains; connect a fresh worker for parent.
    complete_success_empty(&handle, "w-x9", "x9-child").await?;
    let mut stream_rx = connect_executor(&handle, "w-x9-2", "x86_64-linux").await?;
    // Parent dispatch sends PrefetchHint FIRST (child has expected_
    // output_paths set above), then Assignment. Drain both.
    let assignment_parent = loop {
        let msg = tokio::time::timeout(Duration::from_secs(2), stream_rx.recv())
            .await
            .expect("timeout")
            .expect("channel closed");
        if let Some(rio_proto::types::scheduler_message::Msg::Assignment(a)) = msg.msg {
            break a;
        }
        // Else: PrefetchHint, skip.
    };
    assert!(assignment_parent.drv_path.contains("x9-parent"));
    barrier(&handle).await;

    // Parent's input-closure = child's expected_output_paths
    // (1 path via make_test_node). Pin should be present.
    let count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'x9-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(count, 1, "parent dispatch should pin its 1 input path");

    // Complete parent → unpin.
    complete_success_empty(&handle, "w-x9-2", "x9-parent").await?;
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

// `recv_assignment` (helpers.rs) already skips Prefetch — alias kept so
// the CA-on-CA test bodies below stay readable at the old name.
use super::recv_assignment as recv_assignment_skip_prefetch;

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

// r[verify sched.ca.resolve+3]
/// Recovered CA-on-CA dispatch: scheduler restart cleared
/// `drv_content`, but the store has the `.drv` — `maybe_resolve_ca`
/// fetches it via `GetPath`, NAR-unwraps, and resolves placeholders.
///
/// Flow:
///   1. Seed MockStore with parent's ATerm bytes at its `.drv` path
///      (as a single-file NAR — same as `nix-store --dump` of a `.drv`).
///   2. Seed PG `realisations` with child's `(modular_hash, "out")` →
///      `realized_path`.
///   3. Merge CA-on-CA DAG (parent depends on child, both CA).
///   4. Child dispatches. Clear parent's `drv_content` (simulate
///      recovery).
///   5. Complete child → parent becomes Ready → `maybe_resolve_ca`
///      sees empty `drv_content` → fetches from MockStore → unwraps
///      NAR → `resolve_ca_inputs` rewrites placeholder.
///   6. Parent's `WorkAssignment.drv_content` contains the realized
///      path, not the placeholder.
#[tokio::test]
async fn recovered_ca_on_ca_dispatch_fetches_from_store() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let (child, parent, parent_aterm, placeholder, child_modular, realized_path) =
        ca_on_ca_fixture();

    // Seed MockStore: parent's ATerm wrapped in a single-file NAR
    // at its .drv store path. `seed_with_content` does the NAR wrap.
    store.seed_with_content(&parent.drv_path, parent_aterm.as_bytes());

    let mut rx = connect_executor(&handle, "ca-w", "x86_64-linux").await?;

    // Merge: child + parent, edge parent → child.
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![make_test_edge("ca-parent", "ca-child")],
        false,
    )
    .await?;

    // Child dispatches first (leaf → Ready immediately).
    let a1 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a1.drv_path.contains("ca-child"), "child dispatches first");

    // Seed PG realisations: child's (modular_hash, "out") → realized.
    // This is what `resolve_ca_inputs` queries to map placeholder →
    // realized path. Seeded AFTER merge so the child doesn't cache-hit
    // via check_cached_outputs' CA realisation lookup (GAP-3 fix) —
    // this test needs the child to actually dispatch and complete.
    sqlx::query(
        "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
         VALUES ($1, 'out', $2, $3)",
    )
    .bind(child_modular.as_slice())
    .bind(&realized_path)
    .bind([0u8; 32].as_slice())
    .execute(&_db.pool)
    .await?;

    // Clear parent's drv_content BEFORE completing child — actor
    // processes serially, so the clear lands before the completion
    // fires dispatch_ready for the parent.
    let cleared = handle.debug_clear_drv_content("ca-parent").await?;
    assert!(cleared, "parent should be in DAG");

    // Complete child → parent becomes Ready → dispatch fires →
    // maybe_resolve_ca sees empty drv_content → fetches from store.
    complete_success(
        &handle,
        "ca-w",
        "ca-child",
        &test_store_path("ca-child-out"),
    )
    .await?;
    let mut rx = connect_executor(&handle, "ca-w-2", "x86_64-linux").await?;

    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("ca-parent"));

    // The load-bearing assertions: drv_content was fetched + resolved.
    assert!(
        !a2.drv_content.is_empty(),
        "drv_content must be fetched from store, not left empty"
    );
    let text = std::str::from_utf8(&a2.drv_content).expect("ATerm is ASCII");
    assert!(
        !text.contains(&placeholder),
        "placeholder {placeholder:?} must be replaced post-fetch-and-resolve"
    );
    assert!(
        text.contains(&realized_path),
        "realized path {realized_path:?} must be present in resolved ATerm"
    );

    Ok(())
}

/// Fail-safe preserved: store unreachable → dispatch still proceeds
/// with empty `drv_content`. Same degrade as before the fetch
/// existed — worker fails on placeholder, self-heals via retry after
/// a fresh `SubmitBuild` re-merges with inline `drv_content`.
///
/// Also covers `store_client = None` via the early `?` in
/// `fetch_drv_content_from_store` — this test uses the explicit
/// `fail_get_path` knob instead (closer to a real store outage).
#[tokio::test]
async fn recovered_ca_on_ca_dispatch_degrades_on_store_failure() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let (child, parent, _parent_aterm, _placeholder, child_modular, _realized_path) =
        ca_on_ca_fixture();

    let mut rx = connect_executor(&handle, "ca-w", "x86_64-linux").await?;

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![child, parent],
        vec![make_test_edge("ca-parent", "ca-child")],
        false,
    )
    .await?;

    let a1 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a1.drv_path.contains("ca-child"));

    // Seed the realisation AFTER merge (so the ONLY failure is the
    // store fetch, not a missing-realisation — we're testing the
    // fetch fallback specifically). Seeded post-merge so the child
    // doesn't cache-hit via check_cached_outputs' CA realisation
    // lookup (GAP-3 fix) — this test needs child to dispatch.
    sqlx::query(
        "INSERT INTO realisations (drv_hash, output_name, output_path, output_hash)
         VALUES ($1, 'out', $2, $3)",
    )
    .bind(child_modular.as_slice())
    .bind(test_store_path("irrelevant"))
    .bind([0u8; 32].as_slice())
    .execute(&_db.pool)
    .await?;

    // Clear parent's drv_content AND make GetPath fail. Order matters:
    // actor serializes, so both land before the completion's dispatch.
    let cleared = handle.debug_clear_drv_content("ca-parent").await?;
    assert!(cleared);
    store
        .faults
        .fail_get_path
        .store(true, std::sync::atomic::Ordering::SeqCst);

    complete_success(
        &handle,
        "ca-w",
        "ca-child",
        &test_store_path("ca-child-out"),
    )
    .await?;
    let mut rx = connect_executor(&handle, "ca-w-2", "x86_64-linux").await?;

    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("ca-parent"));

    // Fail-safe: store fetch failed → drv_content stays empty →
    // worker will fetch + fail on placeholder + retry. Same degrade
    // as before the store-fetch shortcut existed.
    assert!(
        a2.drv_content.is_empty(),
        "store fetch failed → drv_content must stay empty (degrade preserved)"
    );

    Ok(())
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
    let (_db, handle, _task, mut rx) = setup_with_worker("ia-w", "x86_64-linux").await?;

    let original_content = b"dummy-ia-aterm-content".to_vec();
    let mut node = make_node("ia-drv");
    node.is_content_addressed = false; // explicit: IA
    node.needs_resolve = false; // explicit: no CA inputs either
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = recv_assignment(&mut rx).await;

    assert_eq!(
        asgn.drv_content, original_content,
        "IA derivation → maybe_resolve_ca passthrough; drv_content unchanged"
    );
    Ok(())
}

// r[verify sched.ca.resolve+3]
/// FOD passthrough: `is_ca = true` BUT `needs_resolve = false` (FOD
/// output path is eval-time known; gateway doesn't set needs_resolve
/// unless an inputDrv is floating-CA). ADR-018 `shouldResolve` table:
/// FOD → only if ca-derivations feature enabled (optional optimization;
/// rio doesn't fire it unless inputs are actually CA).
#[tokio::test]
async fn maybe_resolve_ca_fixed_output_passthrough() -> TestResult {
    // FOD routing (ADR-019): FODs only dispatch to fetchers. The
    // default Builder-kind worker from setup_with_worker would never
    // receive the assignment (hard_filter rejects FOD→builder).
    let (_db, handle, _task) = setup().await;
    let mut rx = connect_executor_no_ack_kind(
        &handle,
        "fod-w",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "fod-w".into(),
            paths_fetched: 0,
        })
        .await?;

    let original_content = b"dummy-fod-aterm-content".to_vec();
    let mut node = make_node("fod-drv");
    node.is_content_addressed = true;
    node.is_fixed_output = true;
    node.needs_resolve = false; // gateway: FOD with no CA inputs → no resolve
    node.drv_content = original_content.clone();

    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;
    let asgn = recv_assignment(&mut rx).await;

    assert_eq!(
        asgn.drv_content, original_content,
        "FOD (is_ca && is_fixed_output) → passthrough; output path known at eval"
    );
    Ok(())
}

// r[verify sched.ca.resolve+3]
/// No-CA-inputs passthrough: floating-CA derivation whose children
/// are all IA → `collect_ca_inputs` returns `[]` → gate at
/// dispatch.rs:694 fails → passthrough. The common case: a CA
/// `mkDerivation` on IA stdenv. No resolve needed — no placeholder
/// in the ATerm because all input paths were known at eval time.
#[tokio::test]
async fn maybe_resolve_ca_no_ca_inputs_passthrough() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("noca-w", "x86_64-linux").await?;

    let original_content = b"floating-ca-with-ia-deps".to_vec();
    let mut parent = make_node("noca-parent");
    parent.is_content_addressed = true;
    parent.is_fixed_output = false;
    parent.needs_resolve = true; // floating-CA self — gate passes
    parent.drv_content = original_content.clone();

    // IA child — collect_ca_inputs skips it (is_ca=false).
    let child = make_node("noca-child");

    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![parent, child],
        vec![make_test_edge("noca-parent", "noca-child")],
        false,
    )
    .await?;

    // Child dispatches first (leaf).
    let a1 = recv_assignment(&mut rx).await;
    assert!(a1.drv_path.contains("noca-child"));
    complete_success_empty(&handle, "noca-w", &test_drv_path("noca-child")).await?;
    let mut rx = connect_executor(&handle, "noca-w-2", "x86_64-linux").await?;

    // Parent dispatches. collect_ca_inputs(parent) = [] (child is IA)
    // → ca_inputs.is_empty() gate → passthrough.
    let a2 = recv_assignment_skip_prefetch(&mut rx).await;
    assert!(a2.drv_path.contains("noca-parent"));
    assert_eq!(
        a2.drv_content, original_content,
        "floating-CA with only IA children → collect_ca_inputs=[] → \
         passthrough (no resolve, drv_content unchanged)"
    );
    Ok(())
}

/// I-163 Fix 1 + became-idle carve-out: steady-state Heartbeats set
/// `dispatch_dirty` (drained by `Tick`); a Heartbeat that flips
/// capacity 0→1 dispatches inline. At 290 workers × 169ms/dispatch
/// the unconditional inline path was ~5× actor capacity — the 0→1
/// edge is bounded by spawn rate, not heartbeat rate.
///
/// Shape: merge with no worker (Ready, deferred) → connect_no_ack
/// (registration heartbeat = 0→1 → inline dispatch, cold-fallback
/// places the node) → assert Assignment lands WITHOUT a Tick. Then a
/// SECOND heartbeat for the now-busy worker is steady-state (0→0) →
/// only dirty, no inline dispatch.
// r[verify sched.actor.dispatch-decoupled]
// r[verify sched.dispatch.became-idle-immediate]
#[tokio::test]
async fn heartbeat_sets_dirty_tick_dispatches() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge first: dispatch_ready runs at end-of-merge with zero
    // workers → deferred. ready_queue holds the node.
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "i163-hb", PriorityClass::Scheduled).await?;

    // Connected + Heartbeat. Registration heartbeat is a 0→1 capacity
    // edge → became_idle=true → inline dispatch_ready. Cold-fallback
    // places the node on the freshly-registered worker (warm-gate
    // sent a PrefetchHint but cold-fallback ignores warm).
    let mut rx = connect_executor_no_ack(&handle, "i163-w", "x86_64-linux").await?;
    let a = recv_assignment(&mut rx).await;
    assert!(
        a.drv_path.contains("i163-hb"),
        "registration heartbeat (0→1) must dispatch inline — \
         r[sched.dispatch.became-idle-immediate]"
    );

    // Second heartbeat: worker is now busy (running_build=Some) →
    // capacity 0→0 → became_idle=false → only dispatch_dirty set.
    // Structural assertion: a steady-state heartbeat must NOT call
    // `dispatch_ready` inline. The previous "no Assignment on rx"
    // check was vacuous — there's no idle capacity AND no queued
    // work, so it passes even if the heartbeat handler dispatches
    // inline. Counting `dispatch_ready` calls fails under the I-163
    // mutation (revert decoupling → heartbeat calls inline).
    let before = handle.debug_counters().await?;
    send_heartbeat_with(&handle, "i163-w", "x86_64-linux", |hb| {
        hb.running_build = Some(a.drv_path.clone());
    })
    .await?;
    barrier(&handle).await;
    let after_hb = handle.debug_counters().await?;
    assert_eq!(
        after_hb.dispatch_ready_calls, before.dispatch_ready_calls,
        "steady-state heartbeat (0→0, busy) must NOT call dispatch_ready inline — \
         r[sched.actor.dispatch-decoupled]"
    );
    // …and Tick drains the dirty flag (exactly one dispatch_ready).
    tick(&handle).await?;
    let after_tick = handle.debug_counters().await?;
    assert_eq!(
        after_tick.dispatch_ready_calls,
        before.dispatch_ready_calls + 1,
        "Tick must drain dispatch_dirty exactly once"
    );
    // Belt: still no spurious Assignment on the stream.
    while let Ok(m) = rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "steady-state heartbeat (0→0, busy) must not dispatch inline"
        );
    }
    Ok(())
}

/// I-163 Fix 2: deferred FODs are checked by the batch pre-pass (one
/// `FindMissingPaths`), and the drain loop SKIPS the per-FOD
/// `fod_outputs_in_store` for hashes the batch already covered. The
/// doc-comment on `fod_outputs_in_store` claimed this; the code
/// didn't honor it (211 deferred FODs × ~0.7ms RTT ≈ 150ms of the
/// 169ms/Heartbeat I-163 cost).
///
/// Shape: merge 5 Ready FODs with no fetcher (all defer); count
/// `FindMissingPaths` across one dispatch_ready. Want exactly 1 (the
/// batch). Pre-fix would be 1 + 5 = 6.
#[tokio::test]
async fn batch_checked_fods_skip_per_fod_rpc() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Builder, not fetcher: FODs route to fetchers (ADR-019), so all
    // 5 defer in the drain loop — that's the I-163 hot path.
    let _rx = connect_executor(&handle, "i163-builder", "x86_64-linux").await?;

    let nodes: Vec<_> = (0..5)
        .map(|i| {
            let mut n = make_node(&format!("i163-fod-{i}"));
            n.is_fixed_output = true;
            // batch pre-pass filters on !expected_output_paths.is_empty()
            n.expected_output_paths = vec![test_store_path(&format!("i163-fod-{i}-out"))];
            n
        })
        .collect();
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;

    // Merge ran check_cached_outputs (1 RPC) + dispatch_ready (1 batch
    // RPC). Reset the baseline; the assertion is on the NEXT
    // dispatch_ready in isolation.
    barrier(&handle).await;
    store.calls.find_missing_calls.store(0, Ordering::SeqCst);

    // Drive one dispatch_ready: Heartbeat (dirty) + Tick (drain).
    // send_heartbeat already chains the Tick.
    send_heartbeat(&handle, "i163-builder", "x86_64-linux").await?;
    barrier(&handle).await;

    let calls = store.calls.find_missing_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 1,
        "one dispatch_ready over 5 deferred FODs must issue exactly the batch \
         FindMissingPaths (got {calls}); >1 means the per-FOD fallback fired \
         for batch-checked hashes"
    );
    Ok(())
}

/// I-163 Fix 2, fail-open edge: when the batch RPC fails, the per-drv
/// fallback in the drain loop STILL fires for nodes the batch didn't
/// stamp (cascade-promoted). Nodes the batch DID stamp this gen are
/// `probed_generation`-gated in the per-drv path too, so a batch
/// failure for them defers retry to the next Tick (1/s) instead of
/// firing N sequential per-drv FMPs in the same pass.
#[tokio::test]
async fn batch_fod_fail_open_preserves_per_fod_fallback() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let _rx = connect_executor(&handle, "i163-fo-b", "x86_64-linux").await?;

    // fod1: leaf, Ready at merge → always in the batch snapshot.
    let mut fod1 = make_node("i163-fo-fod");
    fod1.is_fixed_output = true;
    fod1.expected_output_paths = vec![test_store_path("i163-fo-fod-out")];
    // fod2 depends on dep. dep's output is seeded AFTER merge so the
    // dispatch-time batch (not merge-time check_cached_outputs)
    // completes dep → cascade-promotes fod2 to Ready DURING the
    // dispatch pass, AFTER the batch's candidate snapshot — fod2 is
    // unstamped this gen → the per-drv `ready_check_or_spawn`
    // fallback fires for it.
    let dep_out = test_store_path("i163-fo-dep-out");
    let mut dep = make_node("i163-fo-dep");
    // FOD so it routes to fetcher (none connected → defers) instead
    // of dispatching to i163-fo-b during merge's inline dispatch_ready.
    dep.is_fixed_output = true;
    dep.expected_output_paths = vec![dep_out.clone()];
    let mut fod2 = make_node("i163-fo-fod2");
    fod2.is_fixed_output = true;
    fod2.expected_output_paths = vec![test_store_path("i163-fo-fod2-out")];
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![fod1, fod2, dep],
        vec![make_test_edge("i163-fo-fod2", "i163-fo-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // ── per-drv fallback path (test name half 2): seed dep present
    // NOW (post-merge), then drive one dispatch_ready. Batch sees
    // [fod1, dep] (both Ready); completes dep → fod2 promoted →
    // drain loop hits the `!batch_checked.contains(fod2)` branch →
    // `ready_check_or_spawn(fod2)` issues 1 per-drv FMP. Total = 2.
    // Mutation guard: delete the `ready_check_or_spawn` clause and
    // fod2 is never store-checked → ==1.
    store
        .state
        .paths
        .write()
        .unwrap()
        .insert(dep_out, Default::default());
    store.calls.find_missing_calls.store(0, Ordering::SeqCst);
    send_heartbeat(&handle, "i163-fo-b", "x86_64-linux").await?;
    barrier(&handle).await;
    let calls = store.calls.find_missing_calls.load(Ordering::SeqCst);
    assert_eq!(
        calls, 2,
        "cascade-promoted fod2 must hit the per-drv ready_check_or_spawn \
         fallback (1 batch + 1 per-drv); got {calls}"
    );
    assert_eq!(
        expect_drv(&handle, "i163-fo-dep").await.status,
        DerivationStatus::Completed
    );

    // ── fail-open gating path (test name half 1): arm the failure;
    // next gen's batch stamps fod1+fod2 (1 FMP, fails). batch_checked
    // returns empty BUT both have probed_generation == gen → per-drv
    // path's `probed_generation >= probe_gen` gate suppresses re-fire.
    // Exactly 1 proves a batch failure doesn't degrade to N
    // sequential per-drv FMPs in the same pass.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
    store.calls.find_missing_calls.store(0, Ordering::SeqCst);
    send_heartbeat(&handle, "i163-fo-b", "x86_64-linux").await?;
    barrier(&handle).await;
    assert_eq!(
        store.calls.find_missing_calls.load(Ordering::SeqCst),
        1,
        "batch-stamped nodes must not re-fire per-drv FMP within a generation"
    );
    Ok(())
}

// r[verify sched.dispatch.fod-substitute]
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
    // x86_64 builder; node is aarch64 → defers regardless of FOD-ness
    // (no system match), so merge-time dispatch can't assign it.
    let _rx = connect_executor(&handle, "sub-b", "x86_64-linux").await?;

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
    // Ready/deferred, stamped probed_generation=1. Seed; the next
    // send_heartbeat is NOT became_idle (worker already idle) → sets
    // dispatch_dirty → chained Tick advances probe_generation → batch
    // re-probes → spawns substitute fetch.
    store.state.substitutable.write().unwrap().push(out.clone());
    send_heartbeat(&handle, "sub-b", "x86_64-linux").await?;
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

/// I-163 Fix 3: `cluster_snapshot_cached()` reads the watch-channel
/// value the actor publishes on `Tick` — no mailbox round-trip. The
/// `fn` (not `async fn`) signature is the structural proof; this test
/// verifies the value is wired (Tick → publish → handle reads it) and
/// is stale-until-Tick (merge alone doesn't update it).
// r[verify sched.admin.snapshot-cached]
#[tokio::test]
async fn cluster_snapshot_cached_reflects_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    let _rx = connect_executor(&handle, "i163-snap-w", "x86_64-linux").await?;
    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "i163-snap",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // No Tick yet → watch holds the Default snapshot (all zeros).
    // connect_executor's PrefetchComplete dispatched the node, but
    // the cached snapshot doesn't see that until Tick publishes.
    let pre = handle.cluster_snapshot_cached();
    assert_eq!(
        pre.total_executors, 0,
        "cached snapshot is Tick-published, not live; pre-Tick must be Default"
    );

    tick(&handle).await?;

    let post = handle.cluster_snapshot_cached();
    assert_eq!(post.total_executors, 1);
    assert_eq!(post.active_executors, 1);
    // Node was assigned by PrefetchComplete's inline dispatch (still
    // runs inline — only Heartbeat moved to dirty-flag).
    assert_eq!(post.running_derivations, 1);
    Ok(())
}

// r[verify sched.sla.reactive-floor]
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

    let mut rx = connect_executor_kind(&handle, "w-1", "x86_64-linux", kind).await?;

    let mut node = make_node(tag);
    node.is_fixed_output = is_fod;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    barrier(&handle).await;
    let first_asgn = recv_assignment(&mut rx).await;
    assert!(first_asgn.drv_path.contains(tag));

    // Seed est_memory_bytes so the doubling has a base.
    handle
        .debug_seed_sched_hint(tag, Some(2 << 30), None, None, None)
        .await?;

    // Worker-reported CgroupOom → floor.mem doubled.
    complete_failure(
        &handle,
        "w-1",
        tag,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "cgroup OOM during build; bumping resource floor",
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

// r[verify sched.dispatch.unroutable-system]
/// Ready drv whose `system` is advertised by zero registered executors:
/// stays Ready (deferred), WARN fires once (edge-triggered, not per
/// tick). Connecting a matching executor dispatches it.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_unroutable_system_warn_then_dispatch() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Only an x86_64 builder registered.
    let _w = connect_executor(&handle, "x86-b1", "x86_64-linux").await?;

    // riscv drv has no advertising pool.
    let riscv = make_test_node("rv-unroutable", "riscv64-linux");
    let rv_hash = riscv.drv_hash.clone();
    // Hold the event receiver so the orphan-watcher doesn't auto-cancel.
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![riscv], vec![], false).await?;
    tick(&handle).await?;

    let d = expect_drv(&handle, &rv_hash).await;
    assert_eq!(
        d.status,
        DerivationStatus::Ready,
        "unroutable drv defers (not poisoned/failed)"
    );
    assert!(
        logs_contain("no registered executor advertises this system")
            && logs_contain("riscv64-linux"),
        "WARN fires when system first becomes unroutable"
    );

    // Edge-triggered: a second tick must NOT re-WARN.
    tick(&handle).await?;
    logs_assert(|lines| {
        let n = lines
            .iter()
            .filter(|l| l.contains("no registered executor advertises this system"))
            .count();
        if n == 1 {
            Ok(())
        } else {
            Err(format!("WARN must fire once across both ticks; got {n}"))
        }
    });

    // Connect a riscv builder → next tick dispatches. That same tick
    // sees riscv as routable, so `unroutable_warned.retain()` drops it.
    let mut rv_rx = connect_executor(&handle, "rv-b1", "riscv64-linux").await?;
    tick(&handle).await?;
    let assn = recv_assignment(&mut rv_rx).await;
    assert!(assn.drv_path.contains("rv-unroutable"));

    // Re-arm: riscv becomes unroutable again (only executor gone) → a
    // fresh riscv drv must trip a SECOND WARN.
    disconnect(&handle, "rv-b1").await?;
    let riscv2 = make_test_node("rv-unroutable-2", "riscv64-linux");
    let _ev2 = merge_dag(&handle, Uuid::new_v4(), vec![riscv2], vec![], false).await?;
    tick(&handle).await?;
    logs_assert(|lines| {
        let n = lines
            .iter()
            .filter(|l| l.contains("no registered executor advertises this system"))
            .count();
        if n == 2 {
            Ok(())
        } else {
            Err(format!(
                "WARN must re-arm after routable→unroutable; got {n}"
            ))
        }
    });

    Ok(())
}

// ─── ADR-023 SlaEstimator → SpawnIntent ────────────────────────────────

/// `compute_spawn_intents` emits one SpawnIntent per Ready derivation,
/// with `intent_id == drv_hash` and `cores ≈ solve_mvp(c_star)` for a
/// fitted key. Unfitted keys (no SlaEstimator entry) get probe defaults.
// r[verify sched.sla.intent-from-solve]
#[tokio::test]
async fn spawn_intent_from_sla_estimator() {
    use crate::sla::{solve, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    // Seed a single tier so the test exercises the Feasible path the way
    // a configured deploy would (empty ladder → solve_mvp BestEffort at
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
        n_eff: 10.0,
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
        prior_source: None,
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
        "solve_mvp c_star ≈ 1.95 → ceil 2 (got {})",
        fitted.cores
    );
    assert_eq!(fitted.disk_bytes, 10 << 30, "disk_p90 from fit");
    assert!(fitted.mem_bytes >= 6 << 30, "mem ≥ p90 (× headroom)");

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    // No SlaEstimator entry + no [sla] config → fallback probe (4c, 8Gi).
    assert_eq!(cold.cores, 4, "no fit → fallback probe cores");
    assert_eq!(cold.mem_bytes, 8 << 30);
}

/// ADR-023 §2.8 Pending-watch: an entry in `pending_intents` past
/// `hw_fallback_after_secs` marks its `(band, cap)` ICE-infeasible
/// and is dropped; an entry whose drv left Ready is dropped without
/// marking; a heartbeat with matching `intent_id` clears the entry.
#[tokio::test]
async fn pending_intent_timeout_marks_ice() {
    use crate::sla::cost::{Band, Cap};
    use std::time::{Duration, Instant};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_cfg(db.pool.clone(), DagActorConfig::default());
    actor.sla_config = crate::sla::config::SlaConfig {
        hw_fallback_after_secs: 120.0,
        ..test_sla_config()
    };

    // "stuck": Ready, backdated past max-jitter (1.2×120s).
    actor.test_inject_ready("stuck", Some("p"), "x86_64-linux", false);
    let old = Instant::now() - Duration::from_secs(200);
    actor
        .pending_intents
        .insert("stuck".into(), (Band::Mid, Cap::Spot, old));
    // "fresh": Ready, just emitted → kept.
    actor.test_inject_ready("fresh", Some("p"), "x86_64-linux", false);
    actor
        .pending_intents
        .insert("fresh".into(), (Band::Hi, Cap::Spot, Instant::now()));
    // "gone": no DAG node → dropped without marking.
    actor
        .pending_intents
        .insert("gone".into(), (Band::Lo, Cap::OnDemand, old));

    actor.tick_sweep_pending_intents(Instant::now());

    assert!(
        actor.ice.is_infeasible(Band::Mid, Cap::Spot),
        "stuck → marked"
    );
    assert!(
        !actor.ice.is_infeasible(Band::Lo, Cap::OnDemand),
        "gone → dropped without marking (drv left Ready)"
    );
    assert!(!actor.ice.is_infeasible(Band::Hi, Cap::Spot), "fresh kept");
    assert_eq!(actor.pending_intents.len(), 1, "only fresh remains");
    assert!(actor.pending_intents.contains_key("fresh"));

    // Heartbeat with intent_id="fresh" clears it (pod made it past
    // Pending). handle_heartbeat requires a stream entry first.
    let (tx, _rx) = tokio::sync::mpsc::channel(1);
    actor.handle_worker_connected(&"w0".into(), tx, next_stream_epoch_for("w0"), None);
    actor.handle_heartbeat(HeartbeatPayload {
        executor_id: "w0".into(),
        systems: vec!["x86_64-linux".into()],
        supported_features: vec![],
        running_build: None,
        resources: None,
        store_degraded: false,
        draining: false,
        kind: rio_proto::types::ExecutorKind::Builder,
        intent_id: Some("fresh".into()),
    });
    assert!(actor.pending_intents.is_empty(), "heartbeat clears entry");
}

/// `handle_ack_spawned_intents` is idempotent: a re-ack of an
/// already-armed intent (the controller acks the FULL still-Pending
/// set every tick) MUST NOT reset `armed_at` — `or_insert`, not
/// `insert`. Otherwise a pod stuck Pending forever never crosses
/// `hw_fallback_after_secs` because each tick re-arms it at `now`.
/// After the ICE-sweep removes a timed-out entry, a subsequent ack
/// is a fresh insert at the new time.
#[tokio::test]
async fn ack_spawned_intents_reack_preserves_armed_at() {
    use crate::sla::cost::{self, Band, Cap};
    use std::time::{Duration, Instant};
    let db = TestDb::new(&MIGRATOR).await;
    let actor = bare_actor_cfg(db.pool.clone(), DagActorConfig::default());

    let sel = cost::selector_for(Band::Mid, Cap::Spot);
    let intent = rio_proto::types::SpawnIntent {
        intent_id: "x".into(),
        node_selector: sel.into_iter().collect(),
        ..Default::default()
    };

    actor.handle_ack_spawned_intents(std::slice::from_ref(&intent));
    let t1 = actor.pending_intents.get("x").unwrap().2;

    // Re-ack after measurable elapsed time. `Instant::now()` inside
    // `handle_ack_spawned_intents` would yield > t1 if it overwrote.
    std::thread::sleep(Duration::from_millis(5));
    actor.handle_ack_spawned_intents(std::slice::from_ref(&intent));
    let t1_again = actor.pending_intents.get("x").unwrap().2;
    assert_eq!(
        t1, t1_again,
        "re-ack preserves armed_at (or_insert, not insert)"
    );
    assert!(Instant::now() > t1, "sleep was observable");

    // Simulate ICE-sweep removing the timed-out entry.
    actor.pending_intents.remove("x");
    std::thread::sleep(Duration::from_millis(5));
    actor.handle_ack_spawned_intents(std::slice::from_ref(&intent));
    let t3 = actor.pending_intents.get("x").unwrap().2;
    assert!(
        t3 > t1,
        "post-removal ack arms fresh: t3={t3:?} > t1={t1:?}"
    );
}

/// `solve_full` wired into the actor: with `hw_cost_source` set, a
/// populated hw-factor table, and a fitted key, `SpawnIntent.
/// node_selector` carries `rio.build/hw-band` + `karpenter.sh/
/// capacity-type` and the Pending-watch ledger is armed.
// r[verify sched.sla.solve-per-band-cap]
#[tokio::test]
async fn spawn_intent_node_selector_from_solve_full() {
    use crate::sla::{cost, hw, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_cfg(db.pool.clone(), DagActorConfig::default());
    actor.sla_config = crate::sla::config::SlaConfig {
        hw_cost_source: Some(cost::HwCostSource::Static),
        // temp=0 → greedy argmin → deterministic pick.
        hw_softmax_temp: 0.0,
        ..test_sla_config()
    };
    actor.sla_ceilings = actor.sla_config.ceilings();
    actor.sla_tiers = vec![crate::sla::solve::Tier {
        name: "normal".into(),
        p50: None,
        p90: Some(1200.0),
        p99: None,
    }];
    // hw table with one Mid-band class only (so h_dagger picks it for
    // every band → only Mid is feasible, Hi/Lo fall back to factor=1.0
    // via empty match … actually h_dagger returns ("",1.0) for bands
    // with no hw_class, which is still feasible). To make the assertion
    // tight, populate one class per band so all 6 cells are candidates
    // and the argmin picks the cheapest spot.
    let mut m = HashMap::new();
    m.insert("aws-7-ebs-mid".into(), 1.0);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(m));
    // cost_table left at seed defaults — spot < on-demand for every
    // band, Lo cheapest. With only Mid in hw table, h_dagger for Hi/Lo
    // returns ("",1.0); all bands are feasible at factor=1.0 so argmin
    // by E[cost] picks (Lo, Spot).

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
        n_eff: 10.0,
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
        prior_source: None,
    });

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    actor.test_inject_ready("cold", Some("never-seen"), "x86_64-linux", false);

    let snap = actor.compute_spawn_intents(&Default::default());

    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert!(
        fitted.node_selector.contains_key("rio.build/hw-band"),
        "solve_full populated band selector: {:?}",
        fitted.node_selector
    );
    assert!(
        fitted
            .node_selector
            .contains_key("karpenter.sh/capacity-type"),
        "solve_full populated capacity selector: {:?}",
        fitted.node_selector
    );
    assert!(
        !actor.pending_intents.contains_key("fitted"),
        "compute_spawn_intents is read-only: Pending-watch NOT armed on emit"
    );

    let cold = snap.intents.iter().find(|i| i.intent_id == "cold").unwrap();
    assert!(
        cold.node_selector.is_empty(),
        "no fit → band-agnostic intent_for path"
    );
    assert!(!actor.pending_intents.contains_key("cold"));

    // Ack arms it; band-agnostic ack is ignored.
    actor.handle_ack_spawned_intents(&snap.intents);
    assert!(
        actor.pending_intents.contains_key("fitted"),
        "Pending-watch armed on controller ack"
    );
    assert!(
        !actor.pending_intents.contains_key("cold"),
        "band-agnostic selector → ack ignored (no ICE ladder)"
    );

    // Selector pin: a re-emit reuses the acked (band, cap) — overwrite
    // the entry to a sentinel cell and assert the next snapshot
    // returns THAT, not a fresh solve_full pick.
    actor.pending_intents.insert(
        "fitted".into(),
        (
            cost::Band::Hi,
            cost::Cap::OnDemand,
            std::time::Instant::now(),
        ),
    );
    let snap2 = actor.compute_spawn_intents(&Default::default());
    let fitted2 = snap2
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert_eq!(
        fitted2
            .node_selector
            .get("rio.build/hw-band")
            .map(String::as_str),
        Some("hi"),
        "re-emit reuses pinned selector, not fresh softmax roll"
    );
    assert_eq!(
        fitted2
            .node_selector
            .get("karpenter.sh/capacity-type")
            .map(String::as_str),
        Some("on-demand"),
    );

    // Gate: hw_cost_source unset → solve_full skipped even with hw table.
    actor.sla_config.hw_cost_source = None;
    actor.pending_intents.clear();
    let snap = actor.compute_spawn_intents(&Default::default());
    let fitted = snap
        .intents
        .iter()
        .find(|i| i.intent_id == "fitted")
        .unwrap();
    assert!(
        fitted.node_selector.is_empty(),
        "hw_cost_source=None → band-agnostic"
    );
}

/// `solve_full` gate predicates: each of FOD / `required_features` /
/// `enableParallelBuilding=false` / `--tier` override falls through to
/// the band-agnostic `intent_for` path even with `hw_cost_source` set
/// and a usable fit. The first three would otherwise emit a
/// `rio.build/hw-band` selector that no fetcher / metal NodePool
/// carries → permanently Pending. `--mem`-only override does NOT gate
/// it off — solve_full runs and the override overlays the result.
// r[verify sched.sla.solve-per-band-cap]
#[tokio::test]
async fn solve_full_gate_skips_fod_kvm_serial_and_override() {
    use crate::sla::{cost, hw, types::*};
    let db = TestDb::new(&MIGRATOR).await;
    let mut actor = bare_actor_sla(db.pool.clone());
    actor.sla_config = crate::sla::config::SlaConfig {
        hw_cost_source: Some(cost::HwCostSource::Static),
        hw_softmax_temp: 0.0,
        ..test_sla_config()
    };
    actor.sla_ceilings = actor.sla_config.ceilings();
    actor.sla_tiers = vec![crate::sla::solve::Tier {
        name: "normal".into(),
        p50: None,
        p90: Some(1200.0),
        p99: None,
    }];
    let mut m = HashMap::new();
    m.insert("aws-7-ebs-mid".into(), 1.0);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(m));
    // One fitted key reused by every variant below.
    actor.sla_estimator.seed(FittedParams {
        key: ModelKey {
            pname: "pkg".into(),
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
        n_eff: 10.0,
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
        prior_source: None,
    });

    // Baseline (non-FOD, no features) — proves the fixture DOES route
    // through solve_full so the negative assertions below are
    // meaningful.
    actor.test_inject_ready("base", Some("pkg"), "x86_64-linux", false);
    // FOD with the same fitted pname.
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
        by_id("base")
            .node_selector
            .contains_key("rio.build/hw-band"),
        "fixture sanity: baseline routes through solve_full"
    );
    assert!(
        by_id("fod").node_selector.is_empty(),
        "FOD must not get hw-band selector (fetcher NodePool has none)"
    );
    assert!(
        by_id("kvm").node_selector.is_empty(),
        "required_features must not get hw-band selector (metal pool has none)"
    );
    let serial = by_id("serial");
    assert!(
        serial.node_selector.is_empty(),
        "enableParallelBuilding=false stays band-agnostic"
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
        base.node_selector.is_empty(),
        "tier override gates solve_full off"
    );

    // `mem`-only override → solve_full RUNS (selector populated), mem
    // overlaid post-solve.
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
        base.node_selector.contains_key("rio.build/hw-band"),
        "mem-only override does NOT gate solve_full off"
    );
    assert_eq!(
        base.mem_bytes,
        32 << 30,
        "forced_mem overlays solve_full result"
    );
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
        n_eff: 10.0,
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
        prior_source: None,
    });

    actor.test_inject_ready("fitted", Some("test-pkg"), "x86_64-linux", false);
    let expected_cores = {
        let state = actor.dag.node("fitted").unwrap();
        actor.solve_intent_for(state).cores
    };
    actor.push_ready("fitted".to_string().into());

    let mut rx = bare_connect_builder(&mut actor, "w-sla");
    actor.dispatch_ready().await;

    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.assigned_cores,
        Some(expected_cores),
        "SLA mode: WorkAssignment.assigned_cores == solve_intent_for().cores"
    );
    assert_eq!(
        actor
            .dag
            .node("fitted")
            .unwrap()
            .sched
            .last_intent
            .as_ref()
            .map(|i| i.cores),
        Some(expected_cores),
        "last_intent.cores persisted on state for build_assignment_proto"
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
        n_eff: 10.0,
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
        prior_source: None,
    });
    actor.test_inject_ready("d", Some("test-pkg"), "x86_64-linux", false);
    let state = actor.dag.node("d").unwrap();

    // Baseline: empty hw table → min_factor()=1.0 → deadline ≈ ref_q99×5.
    let baseline = actor.solve_intent_for(state).deadline_secs;

    // Fast-only table (factor 2.0): wall = ref/2 → deadline ≈ baseline/2.
    let mut fast = HashMap::new();
    fast.insert("aws-8-nvme".into(), 2.0);
    actor.sla_estimator.seed_hw(hw::HwTable::from_map(fast));
    let fast_dl = actor.solve_intent_for(state).deadline_secs;
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
    let slow_dl = actor.solve_intent_for(state).deadline_secs;
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

/// Connect+heartbeat+warm a builder against a bare (unspawned) actor.
/// `resources=None` so the resource-fit gate is bypassed.
fn bare_connect_builder(
    actor: &mut DagActor,
    id: &str,
) -> mpsc::Receiver<rio_proto::types::SchedulerMessage> {
    let (tx, rx) = mpsc::channel(8);
    actor.handle_worker_connected(&id.into(), tx, next_stream_epoch_for(id), None);
    actor.handle_heartbeat(HeartbeatPayload {
        executor_id: id.into(),
        systems: vec!["x86_64-linux".into()],
        supported_features: vec![],
        running_build: None,
        resources: None,
        store_degraded: false,
        draining: false,
        kind: rio_proto::types::ExecutorKind::Builder,
        intent_id: None,
    });
    actor.handle_prefetch_complete(&id.into(), 0);
    rx
}

/// A worker heartbeating `intent_id == drv_hash` gets THAT drv even when
/// it isn't FIFO-first. Proves the `find_executor` intent match preempts
/// pick-from-queue. On miss (stale intent), the worker falls through to
/// FIFO.
// r[verify sched.sla.intent-match]
#[tokio::test]
async fn heartbeat_intent_id_prefers_precomputed_drv() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Two Ready drvs: "a" merged first (FIFO head), "b" second.
    let _rx = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("a"), make_node("b")],
        vec![],
        false,
    )
    .await?;

    // Worker spawned for "b" (intent_id = drv_hash). Without the
    // intent-match it would get "a" (FIFO).
    let mut rx = connect_executor_with_intent(&handle, "w-b", "b").await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let asg = recv_assignment(&mut rx).await;
    assert_eq!(
        asg.drv_path,
        test_drv_path("b"),
        "intent_id=b → worker gets b, not FIFO-head a"
    );

    // Stale-intent fallback: worker for "gone" (no such drv) → FIFO
    // pick-from-queue → gets "a".
    let mut rx2 = connect_executor_with_intent(&handle, "w-stale", "gone").await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let asg2 = recv_assignment(&mut rx2).await;
    assert_eq!(
        asg2.drv_path,
        test_drv_path("a"),
        "no intent match → falls through to pick-from-queue"
    );
    Ok(())
}

/// Connect a builder with `intent_id` set. Local helper for the
/// ADR-023 intent-match tests above.
async fn connect_executor_with_intent(
    handle: &ActorHandle,
    executor_id: &str,
    intent_id: &str,
) -> anyhow::Result<mpsc::Receiver<rio_proto::types::SchedulerMessage>> {
    let (tx, rx) = mpsc::channel(16);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: executor_id.into(),
            stream_tx: tx,
            stream_epoch: next_stream_epoch_for(executor_id),
            auth_intent: None,
        })
        .await?;
    send_heartbeat_with(handle, executor_id, "x86_64-linux", |hb| {
        hb.intent_id = Some(intent_id.into());
    })
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: executor_id.into(),
            paths_fetched: 0,
        })
        .await?;
    Ok(rx)
}

// ---------------------------------------------------------------------------
// I-065 fleet-exhaustion: system/feature awareness
// ---------------------------------------------------------------------------

// r[verify sched.dispatch.fleet-exhaust]
/// I-065 on the system/features axis: when every statically-eligible
/// worker is in `failed_builders`, poison — even if other-system or
/// feature-lacking workers of the same kind exist.
///
/// Pre-fix `failed_builders_exhausts_fleet` filtered the fleet by
/// `kind && is_registered()` only. An x86 drv that transient-failed on
/// both x86 builders deferred forever (find_executor: failed-on /
/// system-mismatch → None; exhausts_fleet: aarch64 not in failed set
/// → false; is_poisoned: 2 < threshold=3 → false; defer).
#[rstest::rstest]
#[case::system("aarch64-linux", &[])] // padding mismatched on system
#[case::features("x86_64-linux", &[])] // padding mismatched on features (drv needs kvm)
#[tokio::test]
async fn test_fleet_exhaustion_static_eligibility(
    #[case] pad_system: &str,
    #[case] pad_features: &[&str],
) -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.poison = crate::PoisonConfig {
            threshold: 3,
            require_distinct_workers: true,
        };
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    // 2 statically-eligible x86+kvm builders. The drv will fail on both.
    let _b1 = connect_executor_with(&handle, "b1", "x86_64-linux", true, |hb| {
        hb.supported_features = vec!["kvm".into()];
    })
    .await?;
    let _b2 = connect_executor_with(&handle, "b2", "x86_64-linux", true, |hb| {
        hb.supported_features = vec!["kvm".into()];
    })
    .await?;
    // 2 padding builders that ARE kind-matching+registered but NOT
    // statically-eligible: wrong system (case::system) or lacking kvm
    // (case::features). Pre-fix these counted toward "the fleet" and
    // kept exhausts_fleet false.
    let pad_feats: Vec<String> = pad_features.iter().map(|s| s.to_string()).collect();
    let _p1 = connect_executor_with(&handle, "pad1", pad_system, true, |hb| {
        hb.supported_features = pad_feats.clone();
    })
    .await?;
    let _p2 = connect_executor_with(&handle, "pad2", pad_system, true, |hb| {
        hb.supported_features = pad_feats.clone();
    })
    .await?;

    // x86 drv requiring kvm.
    let mut node = make_node("fe-drv");
    node.required_features = vec!["kvm".into()];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // Fail on both eligible workers.
    fail_on_workers(
        &handle,
        "fe-drv",
        rio_proto::types::BuildResultStatus::TransientFailure,
        &["b1", "b2"],
    )
    .await?;
    tick(&handle).await?;

    let info = expect_drv(&handle, "fe-drv").await;
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "every statically-eligible (kind+system+features) worker in \
         failed_builders → poison; pre-fix this stayed Ready forever \
         because {pad_system}/{pad_features:?} padding kept the kind-only \
         fleet non-exhausted"
    );
    assert_eq!(
        recorder.get("rio_scheduler_poison_fleet_exhausted_total{}"),
        1,
        "fleet-exhausted counter incremented once"
    );
    Ok(())
}

// r[verify sched.dispatch.fleet-exhaust]
/// Negative: a statically-eligible worker NOT in `failed_builders` keeps
/// the fleet non-exhausted (defer, don't poison). Guards against the
/// fix over-filtering.
#[tokio::test]
async fn test_fleet_exhaustion_spare_eligible_defers() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.poison.threshold = 3;
        c.retry_policy.max_retries = 10;
    });
    let _db = db;

    let _b1 = connect_executor(&handle, "b1", "x86_64-linux").await?;
    let _b2 = connect_executor(&handle, "b2", "x86_64-linux").await?;
    // b3 is statically eligible AND not in failed_builders → fleet not
    // exhausted; drv should defer (or dispatch to b3), NOT poison.
    let _b3 = connect_executor(&handle, "b3", "x86_64-linux").await?;

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "fe-spare",
        PriorityClass::Scheduled,
    )
    .await?;
    fail_on_workers(
        &handle,
        "fe-spare",
        rio_proto::types::BuildResultStatus::TransientFailure,
        &["b1", "b2"],
    )
    .await?;
    tick(&handle).await?;

    let info = expect_drv(&handle, "fe-spare").await;
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "b3 statically-eligible and untried → fleet NOT exhausted"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// queue_depth / unroutable_ready gauge exactness under active dispatch
// ---------------------------------------------------------------------------

// r[verify obs.metric.scheduler]
/// `queue_depth` / `unroutable_ready` gauges report the FINAL-iteration
/// deferral count, not the sum across outer-loop iterations. Regression
/// for the per-iteration accumulator never being cleared: with 1 idle
/// worker + N ready, iter1 dispatches 1 + defers N-1 → iter2 re-defers
/// N-1 → exit. Gauge must read N-1, not 2×(N-1).
#[rstest::rstest]
#[case::queue_depth(
    &["x86_64-linux"; 5],
    "rio_scheduler_queue_depth{kind=EXECUTOR_KIND_BUILDER}",
    4.0
)]
#[case::unroutable(
    // 1 x86 (dispatches → dispatched_any=true → 2nd iteration) + 3 riscv
    // (no advertiser → unroutable). Pre-fix gauge read 6.0.
    &["x86_64-linux", "riscv64-linux", "riscv64-linux", "riscv64-linux"],
    "rio_scheduler_unroutable_ready{system=riscv64-linux}",
    3.0
)]
#[tokio::test]
async fn test_queue_depth_gauge_exact_under_active_dispatch(
    #[case] systems: &[&str],
    #[case] gauge_key: &str,
    #[case] expected: f64,
) -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (_db, handle, _task) = setup().await;

    let mut rx = connect_executor(&handle, "qd-w1", "x86_64-linux").await?;
    let nodes: Vec<_> = systems
        .iter()
        .enumerate()
        .map(|(i, sys)| make_test_node(&format!("qd-{i}"), sys))
        .collect();
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, vec![], false).await?;
    let _asgn = recv_assignment(&mut rx).await; // exactly 1 dispatched

    let got = recorder.gauge_value(gauge_key).unwrap_or_else(|| {
        panic!(
            "gauge {gauge_key} not set; gauges={:?}",
            recorder.gauge_names()
        )
    });
    assert_eq!(
        got,
        expected,
        "{gauge_key}: 1 worker, {} ready → 1 dispatched + {expected} deferred; \
         pre-fix this read {} (double-counted across outer-loop iterations)",
        systems.len(),
        2.0 * expected
    );
    Ok(())
}

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
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(1)),
        is_leader.clone(),
        Arc::new(AtomicBool::new(true)),
    );
    let (handle, _task) = setup_actor_configured(db.pool.clone(), Some(store_client), |_, p| {
        p.leader = leader;
    });

    // Seed substitutable so the dispatch-time batch spawns a detached
    // fetch (Ready → Substituting). Arm a permanent QPI failure so the
    // task posts ok=false (we don't depend on the value — the test
    // sends its own SubstituteComplete{ok=true} after lease loss).
    let out = test_store_path("sub-standby-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, Ordering::SeqCst);
    let mut n = make_node("sub-standby");
    n.expected_output_paths = vec![out];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    settle_substituting(&handle, &["sub-standby"]).await;

    // Lease lost. The detached task may have already posted; either way
    // we now send an explicit ok=true as the standby. Pre-fix this
    // wrote `derivations.status = 'completed'` to PG.
    is_leader.store(false, Ordering::SeqCst);
    let before = handle.debug_counters().await?.persist_status_calls;
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "sub-standby".into(),
            ok: true,
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
    Ok(())
}

// r[verify sched.substitute.detached]
/// `SubstituteComplete{ok=false}` sets `substitute_tried` so the next
/// dispatch pass falls through to a worker instead of re-spawning the
/// fetch every Tick (~1/s livelock when FMP HEAD says substitutable
/// but QPI GET says no).
#[tokio::test]
async fn substitute_ok_false_suppresses_respawn() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    // Fetcher so the FOD has somewhere to dispatch on fall-through.
    let mut frx = connect_executor_kind(
        &handle,
        "sub-fall-f",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;

    let out = test_store_path("sub-fall-out");
    store.state.substitutable.write().unwrap().push(out.clone());
    // Permanent (non-transient) QPI failure → detached task posts
    // ok=false on the first attempt.
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, Ordering::SeqCst);

    let mut n = make_node("sub-fall");
    n.is_fixed_output = true;
    n.expected_output_paths = vec![out];
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![n], vec![], false).await?;
    // Spawn → Substituting → task fails → ok=false → Ready.
    settle_substituting(&handle, &["sub-fall"]).await;

    let info = expect_drv(&handle, "sub-fall").await;
    assert!(
        info.substitute_tried,
        "ok=false must set substitute_tried (one-shot fall-through)"
    );
    let qpi_before = store.calls.qpi_calls.read().unwrap().len();

    // Next dispatch pass: partition's `!substitute_tried` guard skips
    // the spawn lane → drv stays Ready → drains to find_executor →
    // dispatched to the fetcher. Pre-fix: re-spawned every tick,
    // qpi_calls grows, never Assigned.
    tick(&handle).await?;
    let a = recv_assignment(&mut frx).await;
    assert!(a.drv_path.contains("sub-fall"));
    assert_eq!(
        store.calls.qpi_calls.read().unwrap().len(),
        qpi_before,
        "no re-spawn after ok=false (substitute_tried suppression)"
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
    // Tick drains, but it can't take any of the x86_64 nodes (so no
    // record_assignment → no singular persist_status(Assigned) noise).
    let _rx = connect_executor(&handle, "bp-hb", "aarch64-linux").await?;

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
    send_heartbeat(&handle, "bp-hb", "aarch64-linux").await?;
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

/// `rollback_assignment` must persist `Ready` to PG — `record_
/// assignment` wrote `status=Assigned`+`assigned_executor`; without
/// the inverse, a scheduler crash in the deferred window reloads
/// `Assigned` and `reset_orphan_to_ready` charges a spurious
/// retry/poison for an assignment that never reached the worker.
#[tokio::test]
async fn rollback_assignment_persists_ready_to_pg() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Connect a worker but immediately drop its rx so the channel
    // closes — `try_send_assignment` fails → `rollback_assignment`.
    let rx = connect_executor(&handle, "rb-w", "x86_64-linux").await?;
    drop(rx);
    let _ev =
        merge_single_node(&handle, Uuid::new_v4(), "rb-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    let info = expect_drv(&handle, "rb-drv").await;
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "in-mem reset_to_ready ran"
    );
    let row: (String, Option<String>) = sqlx::query_as(
        "SELECT status::text, assigned_builder_id FROM derivations WHERE drv_hash = $1",
    )
    .bind("rb-drv")
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        row.0, "ready",
        "rollback_assignment must persist Ready to PG (was: {})",
        row.0
    );
    assert_eq!(
        row.1, None,
        "rollback_assignment must clear assigned_builder_id in PG"
    );
    Ok(())
}
