//! Housekeeping and warm-gate coverage that survives the
//! session-machinery deletion: orphan-watcher, per-build timeout,
//! warm-gate registration hook. The stream session battery that used
//! to live here retired with the machinery it exercised.

use super::*;

/// Orphan-watcher: a watcher that reattaches before grace elapses
/// resets the timer. Covers the gateway WatchBuild-reconnect path —
/// a transient gateway blip must NOT cancel the build.
#[tokio::test]
async fn test_orphan_watcher_reattach_resets_timer() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let ev = merge_single_node(
        &handle,
        build_id,
        "orphan-reattach",
        PriorityClass::Scheduled,
    )
    .await?;

    // Drop watcher → first Tick stamps orphaned_since.
    drop(ev);
    handle.send_unchecked(ActorCommand::Tick).await?;

    // Reattach via WatchBuild before second tick.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let (rx2, _seq) = reply_rx.await??;

    // Second tick: receiver_count > 0 → orphaned_since reset, no cancel.
    handle.send_unchecked(ActorCommand::Tick).await?;
    // Third tick: still watched → still Active.
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "reattached build must stay Active"
    );

    // Now drop the reattached watcher and observe the next two ticks.
    // The reset's only observable effect is that this re-drop gets a
    // FRESH grace window: first post-re-drop tick stamps fresh →
    // Active. Without the reset (housekeeping.rs `orphaned_since =
    // None` deleted), `orphaned_since` is still Some(t1) from the
    // initial drop, grace=ZERO has elapsed, and this tick cancels.
    drop(rx2);
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "first tick after re-drop must only stamp, not cancel — \
         proves orphaned_since was reset on reattach"
    );
    // Second post-re-drop tick: grace=ZERO elapsed → cancel.
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "second tick after re-drop should cancel (grace=ZERO elapsed)"
    );

    Ok(())
}

/// Zero build_timeout = no overall timeout. Even with a wildly stale
/// submitted_at, Tick does NOT fail the build. Guards against an
/// accidental `>= 0` instead of `> 0` in the zero-check.
#[tokio::test]
async fn test_per_build_timeout_zero_means_unlimited() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    // merge_single_node uses BuildOptions::default() → build_timeout=0.
    let _ev = merge_single_node(&handle, build_id, "pbt0-drv", PriorityClass::Scheduled).await?;

    // Backdate far past any reasonable timeout. If the zero-check is
    // wrong (>=0 instead of >0), this would fire immediately.
    let ok = handle.debug_backdate_submitted(build_id, 100_000).await?;
    assert!(ok);

    handle.send_unchecked(ActorCommand::Tick).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build with build_timeout=0 should never time out; got state={}",
        status.state
    );

    Ok(())
}

// r[verify sched.assign.warm-gate]
/// Connect-then-empty-queue: a worker registering with an EMPTY
/// ready queue flips `warm=true` immediately (the short-circuit at
/// `state/executor.rs`-136 — "nothing queued → nothing to prefetch → gate
/// open now"). Proves: merge AFTER connect → Assignment arrives
/// WITHOUT a PrefetchComplete ACK round-trip.
#[tokio::test]
async fn on_worker_registered_empty_queue_flips_warm_immediately() -> TestResult {
    use rio_proto::types::scheduler_message::Msg;

    let (_db, handle, _task) = setup().await;

    // Connect FIRST — ready queue is empty. on_worker_registered's
    // short-circuit flips warm=true without sending a hint.
    let mut rx = connect_executor_no_ack(&handle, "empty-worker", "x86_64-linux").await?;
    barrier(&handle).await;

    // No PrefetchHint on the stream (nothing to hint for).
    assert!(
        rx.try_recv().is_err(),
        "empty queue at registration → no PrefetchHint sent"
    );

    // THEN merge. The worker is already warm (short-circuit) so
    // dispatch proceeds immediately — no ACK round-trip needed.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "empty-drv", PriorityClass::Scheduled).await?;

    // Assignment arrives WITHOUT any PrefetchComplete send. This is
    // the core assertion: if the short-circuit DIDN'T flip warm,
    // the derivation would stay Ready (warm-gate holds) and this
    // recv would timeout.
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout — short-circuit didn't flip warm (dispatch blocked)")
        .expect("channel open");
    match msg.msg {
        Some(Msg::Assignment(a)) => {
            assert_eq!(a.drv_path, test_drv_path("empty-drv"));
        }
        Some(Msg::Prefetch(_)) => {
            panic!("unexpected PrefetchHint — short-circuit should skip the hint for empty queue")
        }
        other => panic!("expected Assignment, got {other:?}"),
    }

    Ok(())
}

/// Build a DAG with `n_ready` Ready parents, each depending on `paths_each`
/// children whose `expected_output_paths` they contribute to the closure.
/// Every child is shared by all parents below it (index-wise) so lower-index
/// paths get the highest frequency count. Every parent P(i) gets `i+1`
/// UUIDs inserted into `interested_builds` — so fan-in is P0<P1<...<P(n-1).
///
/// Returns the fully-populated DAG. No actor, no PG — a pure unit-test
/// fixture for `compute_initial_prefetch_paths`.
fn build_fanned_dag(n_ready: usize, paths_each: usize) -> crate::dag::DerivationDag {
    let mut dag = crate::dag::DerivationDag::new();

    // Children: each child C(j) has a single expected_output_path.
    // test_store_path(format!("child-{j:04}")) gives deterministic
    // lex ordering so the frequency-sort tie-break is predictable.
    let n_children = n_ready + paths_each - 1;
    let child_nodes: Vec<_> = (0..n_children)
        .map(|j| {
            let mut c = make_node(&format!("child-{j:04}"));
            c.expected_output_paths = vec![test_store_path(&format!("child-{j:04}-out"))];
            c
        })
        .collect();

    // Parents: P(i) depends on children C(i)..C(i+paths_each). The
    // sliding window means C(paths_each-1) is shared by paths_each
    // parents, C(0) by 1 parent, C(n_children-1) by 1 parent, etc.
    // Actually: C(j)'s parent-count = min(j+1, paths_each, n_ready,
    // n_children-j) — a trapezoidal distribution peaking in the middle.
    let parent_nodes: Vec<_> = (0..n_ready)
        .map(|i| make_node(&format!("parent-{i:04}")))
        .collect();
    let mut edges = Vec::with_capacity(n_ready * paths_each);
    for i in 0..n_ready {
        for j in i..i + paths_each {
            edges.push(make_test_edge(
                &format!("parent-{i:04}"),
                &format!("child-{j:04}"),
            ));
        }
    }

    // Single merge gets all nodes+edges in. build_id is one shared
    // UUID (every parent gets interested_builds.len()==1 from this);
    // we'll bump per-parent counts below via node_mut.
    let all_nodes: Vec<_> = parent_nodes.into_iter().chain(child_nodes).collect();
    // Arch#13 boundary shim: dag.merge takes domain types; the proto
    // fixtures convert via `From`. Full test-side migration is b03's
    // post-integration step — this is the only direct dag.merge call
    // outside dag/tests.rs.
    let all_nodes = crate::domain::nodes_from_proto(all_nodes);
    let edges = crate::domain::edges_from_proto(edges);
    dag.merge(Uuid::new_v4(), &all_nodes, &edges, "").unwrap();

    // Set statuses: parents → Ready, children → Completed.
    // `approx_input_closure` walks children; Completed children still
    // have their `expected_output_paths` set (persisted at merge time).
    for j in 0..n_children {
        dag.node_mut(&format!("child-{j:04}"))
            .unwrap()
            .set_status_for_test(DerivationStatus::Completed);
    }
    for i in 0..n_ready {
        let p = dag.node_mut(&format!("parent-{i:04}")).unwrap();
        p.set_status_for_test(DerivationStatus::Ready);
        // Fan-in: P(i) gets i ADDITIONAL UUIDs (merge already inserted
        // one). So P0.interested_builds.len()==1, P39.len()==40. The
        // fan-in sort picks P39 first, P0 last.
        for _ in 0..i {
            p.interested_builds.insert(Uuid::new_v4());
        }
    }

    dag
}

// r[verify sched.assign.warm-gate]
/// Determinism: same DAG state → same PrefetchHint contents.
/// `HashMap` iteration is random; T1+T2's fan-in + frequency sort
/// makes the hint reproducible. Pre-T1+T2 this test is flaky (passes
/// ~1/N! of the time for N-element random iteration orderings).
///
/// Also asserts the FIRST path is the highest-frequency one — proving
/// the frequency sort actually fired (proves-nothing guard: a test
/// that only checks `a == b` would pass if both were empty or both
/// selected the same arbitrary set by accident).
#[test]
fn warm_gate_initial_hint_is_deterministic() {
    // 40 Ready parents (>MAX_READY_TO_SCAN=32), each with 4 child
    // paths in a sliding window → 43 unique children. Plus: we want
    // >100 unique paths so the cap is exercised. Use paths_each=5
    // and also attach per-parent UNIQUE paths below.
    //
    // Actually simpler: 40 parents × 5 children window = 44 unique
    // child paths. To exceed 100, bump paths_each to 70. That's 109
    // unique children with the middle-band children shared by up to
    // 40 parents (the sliding-window trapezoid). The top-fan-in
    // parents (P32..P39, interested_builds.len() 33..40) select into
    // the scan; their children are C32..C108 (overlap: C39..C101
    // appears in multiple). After the MAX_READY_TO_SCAN=32 cap, the
    // 32 highest-fan-in parents are P8..P39 (len 9..40).
    let n_ready = 40;
    let paths_each = 70;
    let dag_a = build_fanned_dag(n_ready, paths_each);
    let dag_b = build_fanned_dag(n_ready, paths_each);

    let hint_a = compute_initial_prefetch_paths(&dag_a);
    let hint_b = compute_initial_prefetch_paths(&dag_b);

    assert_eq!(
        hint_a, hint_b,
        "same DAG state must yield identical initial hint"
    );
    assert_eq!(hint_a.len(), 100, "cap at MAX_PREFETCH_PATHS");

    // Proves-nothing guard: highest-frequency path is FIRST. The 32
    // selected parents are P8..P39 (interested_builds.len() 9..40).
    // Each P(i) references C(i)..C(i+69). The intersection across all
    // 32 is C39..C77; within that band every child is referenced by
    // all 32 parents (frequency=32). Tie-break on path string gives
    // C39 first.
    //
    // Check the stronger property: the first path has the expected
    // maximum frequency, which proves T2's sort fired (not just T1's
    // ready-sort making the same arbitrary-cap happen twice).
    let expected_first = test_store_path("child-0039-out");
    assert_eq!(
        hint_a[0], expected_first,
        "highest-frequency path must be first (proves frequency sort fired)"
    );

    // Also check the fan-in sort fired: scanning only 32 of 40 Ready
    // nodes means low-fan-in parents (P0..P7) are excluded. P0's only
    // unique child is C0..C4 (no other parent in the scan references
    // C0..C7). If C0's path were present, T1's sort DIDN'T exclude P0.
    let p0_unique = test_store_path("child-0000-out");
    assert!(
        !hint_a.contains(&p0_unique),
        "lowest-fan-in parent P0 must be excluded by the MAX_READY_TO_SCAN \
         cap (proves fan-in sort fired)"
    );
}

/// Drain a watcher's BuildEvent receiver and count Progress events.
fn drain_progress_count(ev: &mut broadcast::Receiver<rio_proto::types::BuildEvent>) -> usize {
    use rio_proto::types::build_event::Event;
    let mut n = 0;
    while let Ok(e) = ev.try_recv() {
        if matches!(e.event, Some(Event::Progress(_))) {
            n += 1;
        }
    }
    n
}

// r[verify sched.timeout.per-build]
/// Per-build overall timeout: a build with `build_timeout=60` whose
/// `submitted_at` is 61s ago transitions to Failed on Tick. Same build
/// at 59s elapsed does NOT fail (boundary check).
///
/// Uses DebugBackdateSubmitted (same pattern as DebugBackdateRunning
/// above): `submitted_at` is `std::time::Instant`, which tokio paused
/// time cannot mock. And paused time breaks PG pool timeouts anyway
/// (see comment at test_heartbeat_timeout_via_tick_deregisters_worker).
///
/// No worker connected — derivation stays Ready, never Assigned. This
/// isolates the per-build-timeout from the backstop-timeout above: the
/// backstop only fires for status==Running, so a Ready derivation with
/// a stale BUILD proves the per-build check fires independently. The
/// plan's exit criterion "existing backstop test still passes unchanged
/// — proves independence" is satisfied by the backstop test above not
/// being touched; this test adds the converse (per-build fires without
/// backstop).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_per_build_timeout_fails_build_on_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    // No worker — derivation stays Ready. Keeps the backstop check
    // (status==Running only) out of the picture.

    let build_id = Uuid::new_v4();
    let _ev = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("pbt-drv")],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 0,
                build_timeout: 60,
                build_cores: 0,
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // ── Boundary: 59s elapsed — NOT timed out ────────────────────────
    // 59 < 60 → elapsed.as_secs() > build_timeout is false. The check
    // uses strict `>`, so 60s elapsed would also NOT fire
    // (elapsed().as_secs() truncates to 60, and 60 > 60 is false).
    // 59 gives a comfortable margin below; 61 is unambiguously past.
    let ok = handle.debug_backdate_submitted(build_id, 59).await?;
    assert!(ok, "debug_backdate_submitted should find the build");

    handle.send_unchecked(ActorCommand::Tick).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active at 59s < 60s timeout"
    );
    assert!(
        status.error_summary.is_empty(),
        "error_summary should be empty before timeout; got {:?}",
        status.error_summary
    );

    // ── Timeout: 61s elapsed — Failed with timeout reason ────────────
    let ok = handle.debug_backdate_submitted(build_id, 61).await?;
    assert!(ok);

    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    assert!(
        logs_contain("per-build timeout exceeded"),
        "handle_tick should warn on per-build timeout"
    );

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should be Failed after per-build timeout; got state={}",
        status.state
    );
    assert!(
        status.error_summary.contains("build_timeout 60s exceeded"),
        "error_summary should contain the timeout reason; got {:?}",
        status.error_summary
    );

    Ok(())
}

// r[verify sched.backstop.orphan-watcher]
/// Orphan-watcher sweep: an Active build whose `build_events` channel
/// has zero receivers past `ORPHAN_BUILD_GRACE` is auto-cancelled.
/// I-112/I-036 backstop for gateway crash (gateway can't send P0331's
/// CancelBuild). cfg(test) grace is ZERO so two ticks suffice: tick 1
/// stamps `orphaned_since`, tick 2 cancels.
///
/// Three phases:
///   1. Watcher held → Tick does NOT cancel (receiver_count > 0).
///   2. Watcher dropped → Tick stamps orphaned_since, build still Active.
///   3. Second Tick past grace → cancelled.
///
/// Phase 1 is the load-bearing negative case: without it, a regression
/// that ignores `receiver_count` and cancels every Active build on tick
/// would still pass phases 2+3.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_orphan_watcher_cancels_unwatched_build_on_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let ev = merge_single_node(&handle, build_id, "orphan-sweep", PriorityClass::Scheduled).await?;

    // ── Phase 1: watcher held → Tick is a no-op ──────────────────────
    handle.send_unchecked(ActorCommand::Tick).await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "watched build must stay Active across ticks"
    );

    // ── Phase 2: drop watcher → first Tick stamps orphaned_since ─────
    drop(ev);
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "first orphan tick only stamps; grace not yet elapsed"
    );

    // ── Phase 3: second Tick past grace → cancelled ──────────────────
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    assert!(
        logs_contain("orphan-watcher"),
        "expected orphan-watcher warn log"
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build should be Cancelled after orphan grace; got state={}",
        status.state
    );

    Ok(())
}
