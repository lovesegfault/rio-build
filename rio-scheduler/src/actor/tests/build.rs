//! Build lifecycle: CancelBuild, WatchBuild, terminal cleanup.
// r[verify sched.build.state]

use super::*;
use rio_proto::types::build_event::Event;

enum Terminalize {
    Success,
    PermanentFailure,
    Cancel,
}

// r[verify sched.watch.snapshot-first]
/// Late WatchBuild on an already-terminal build: the attach snapshot itself
/// reports the terminal state and outcome payload. Without it, a watcher
/// that subscribes after the terminal event was broadcast (possibly to zero
/// receivers) would hang forever — the property the deleted terminal-event
/// re-send / since_sequence replay used to provide.
#[rstest::rstest]
#[case::completed(Terminalize::Success)]
#[case::failed(Terminalize::PermanentFailure)]
#[case::cancelled(Terminalize::Cancel)]
#[tokio::test]
async fn test_watch_build_after_terminal_snapshot_reports_outcome(
    #[case] how: Terminalize,
) -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let original_rx =
        merge_single_node(&handle, build_id, "watch-hash", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    match how {
        Terminalize::Success => {
            pull_complete_success_empty(&handle, "watch-hash").await?;
        }
        Terminalize::PermanentFailure => {
            pull_complete_failure(
                &handle,
                "watch-hash",
                rio_proto::types::BuildResultStatus::PermanentFailure,
                "test permanent failure",
            )
            .await?;
        }
        Terminalize::Cancel => {
            let (tx, rx) = oneshot::channel();
            handle
                .send_unchecked(ActorCommand::CancelBuild {
                    build_id,
                    caller_tenant: None,
                    reason: "test cancel".into(),
                    reply: tx,
                })
                .await?;
            let _ = rx.await??;
        }
    }
    barrier(&handle).await;
    drop(original_rx);

    // Late WatchBuild → the snapshot reports the terminal outcome. No
    // event needs to arrive on the broadcast at all.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (_watch_rx, snapshot) = reply_rx.await??;

    let Some(Event::Snapshot(snap)) = snapshot.event else {
        panic!(
            "WatchBuild reply must carry a Snapshot event, got {:?}",
            snapshot.event
        );
    };
    match how {
        Terminalize::Success => {
            assert_eq!(
                snap.state,
                rio_proto::types::BuildState::Succeeded as i32,
                "snapshot reports Succeeded"
            );
            assert!(
                snap.error_message.is_empty() && snap.cancel_reason.is_empty(),
                "no failure payload on success: {snap:?}"
            );
        }
        Terminalize::PermanentFailure => {
            assert_eq!(
                snap.state,
                rio_proto::types::BuildState::Failed as i32,
                "snapshot reports Failed"
            );
            assert!(
                !snap.error_message.is_empty(),
                "failure payload populated: {snap:?}"
            );
        }
        Terminalize::Cancel => {
            assert_eq!(
                snap.state,
                rio_proto::types::BuildState::Cancelled as i32,
                "snapshot reports Cancelled"
            );
            assert!(
                !snap.cancel_reason.is_empty(),
                "cancel reason populated: {snap:?}"
            );
        }
    }
    Ok(())
}

// r[verify sched.watch.snapshot-first]
/// WatchBuild on an ACTIVE build returns a snapshot carrying the current
/// aggregate counts and the per-drv running set (with the exec_id minted at
/// dispatch), then the live broadcast continues from exactly that point —
/// the reconnecting-gateway property the deleted since_sequence replay used
/// to provide, now provided by state snapshot instead of event history.
#[tokio::test]
async fn test_watch_build_active_snapshot_counts_and_running_set() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _original = merge_single_node(
        &handle,
        build_id,
        "snap-active-hash",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // Open the pull attempt: the drv is dispatched with a minted exec_id.
    let assignment = pull_attempt(&handle, "snap-active-hash").await;
    barrier(&handle).await;

    // A "reconnecting" watcher attaches with no event history at all.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (mut watch_rx, snapshot) = reply_rx.await??;

    let Some(Event::Snapshot(snap)) = snapshot.event else {
        panic!(
            "WatchBuild reply must carry a Snapshot event, got {:?}",
            snapshot.event
        );
    };
    assert_eq!(
        snap.state,
        rio_proto::types::BuildState::Active as i32,
        "active build → Active snapshot"
    );
    assert_eq!(snap.total_derivations, 1);
    assert_eq!(snap.completed_derivations, 0);
    assert_eq!(
        snap.running_derivations, 1,
        "the dispatched drv counts as running: {snap:?}"
    );
    // The running set carries the exec_id the gateway needs to re-attach
    // the log tail for an execution whose Started event it never saw.
    assert_eq!(snap.running.len(), 1, "running set: {snap:?}");
    assert_eq!(
        snap.running[0].exec_id, assignment.exec_id,
        "snapshot exec_id matches the open attempt's"
    );

    // Snapshot + live continuity: the completion emitted AFTER the
    // snapshot arrives on the broadcast — no replay, no gap, no dedup.
    pull_complete_success_empty(&handle, "snap-active-hash").await?;
    let mut saw_completed = false;
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(200), watch_rx.state.recv()).await {
            Ok(Ok(event)) => {
                if matches!(event.event, Some(Event::Completed(_))) {
                    saw_completed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_completed,
        "events emitted after the snapshot arrive via the live broadcast"
    );
    Ok(())
}

/// Terminal build state should be cleaned up after TERMINAL_CLEANUP_DELAY
/// to prevent unbounded memory growth for long-running schedulers.
///
/// This test sends CleanupTerminalBuild directly (bypassing the delay)
/// since paused time interferes with PG pool timeouts. The delay
/// scheduling itself is trivially correct (tokio::time::sleep + try_send).
#[tokio::test]
async fn test_terminal_build_cleanup_after_delay() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Complete a build.
    let build_id = Uuid::new_v4();
    let drv_hash = "cleanup-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    pull_complete_success_empty(&handle, drv_hash).await?;

    // Build should be Succeeded and still queryable.
    let status = try_query_status(&handle, build_id).await?;
    assert!(status.is_ok(), "build should be queryable before cleanup");

    // Directly inject the cleanup command (bypassing the 60s delay).
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await?;

    // Build should now be gone (BuildNotFound).
    let status = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status, Err(ActorError::BuildNotFound(_))),
        "build should be cleaned up after delay, got {:?}",
        status
    );

    // DAG node should also be reaped (Completed + orphaned).
    let info = handle.debug_query_derivation(drv_hash).await?;
    assert!(
        info.is_none(),
        "orphaned+terminal DAG node should be reaped"
    );
    Ok(())
}

/// CancelBuild on an active build should clean up derivations and emit
/// BuildCancelled event.
#[tokio::test]
async fn test_cancel_build_active_drains_derivations() -> TestResult {
    let (_db, handle, _task) = setup().await;
    // No workers — derivation stays Ready (never assigned).

    let build_id = Uuid::new_v4();
    let mut event_rx =
        merge_single_node(&handle, build_id, "cancel-hash", PriorityClass::Scheduled).await?;

    // Send CancelBuild.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: reply_tx,
        })
        .await?;
    let cancelled = reply_rx.await??;
    assert!(cancelled, "CancelBuild should return true for active build");

    // Build should be Cancelled.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build should be Cancelled after CancelBuild"
    );

    // Should have received BuildCancelled event.
    let mut saw_cancelled = false;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Cancelled(_))
        ) {
            saw_cancelled = true;
        }
    }
    assert!(saw_cancelled, "BuildCancelled event should be emitted");

    // Second CancelBuild should be a no-op (idempotent: returns false).
    let (reply_tx2, reply_rx2) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "already cancelled".into(),
            reply: reply_tx2,
        })
        .await?;
    let re_cancelled = reply_rx2.await??;
    assert!(
        !re_cancelled,
        "CancelBuild on already-terminal build should return false"
    );
    Ok(())
}

/// WatchBuild during an active build should receive events as they happen.
/// (The after-completion case is tested separately.)
#[tokio::test]
async fn test_watch_build_receives_events() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _original = merge_single_node(
        &handle,
        build_id,
        "watch-events-hash",
        PriorityClass::Scheduled,
    )
    .await?;

    // WatchBuild on the active build.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (mut watch_rx, _snapshot) = reply_rx.await??;

    // Complete the build; watcher should see BuildCompleted.
    pull_complete_success_empty(&handle, "watch-events-hash").await?;

    let mut saw_completed = false;
    // Drain events with a timeout.
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(200), watch_rx.state.recv()).await {
            Ok(Ok(event)) => {
                if matches!(
                    event.event,
                    Some(rio_proto::types::build_event::Event::Completed(_))
                ) {
                    saw_completed = true;
                    break;
                }
            }
            _ => break,
        }
    }
    assert!(
        saw_completed,
        "WatchBuild subscriber should see BuildCompleted"
    );
    Ok(())
}

// D3-retarget: the reap survivor pass survives (jobs-armed gate, B5); this
// test retires whole with the settlement arm in the consumption-component
// commit. The untried case (verification walk spawned at reap time) died
// with the walk spawner; the surviving tried case pins the reap-time
// fail-fast of a marked spent survivor for one more commit.
/// Reap-survivor settlement (third C3/D16 site): when a terminal
/// build's reap leaves a marked survivor holed with the verification
/// one-shot spent, the reap hook itself must settle the node
/// (fail-fast) at reap time — not leave it for a later sweep that may
/// never see it in this shape.
// r[verify sched.evidence.settlement]
// r[verify sched.merge.substitute-topdown+12]
#[rstest::rstest]
#[case::tried(true)]
#[tokio::test]
async fn reap_survivor_settles_at_reap_time(#[case] tried: bool) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park any detached fetch so build B's prune-merge walk never
    // completes on its own; the survivor is forced out of Substituting
    // below so the reap hook (not a walk verdict) is what settles it.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // d1's narrow output: substitutable at merge time (so B's prune
    // fires), PRESENT later (the reap-time re-probe must see it
    // obtainable). The wide output is never available.
    let out = test_store_path("reap-d1-out");
    let wide = test_store_path("reap-d1-wide");
    store.state.substitutable.write().unwrap().push(out.clone());

    let mk_d1 = |wanted: Vec<String>| {
        let mut n = make_node("reap-d1");
        n.output_names = vec!["out".into(), "wide".into()];
        n.expected_output_paths = vec![out.clone(), wide.clone()];
        n.wanted_output_names = wanted;
        n
    };
    let mk_d2 = || {
        let mut n = make_node("reap-d2");
        n.expected_output_paths = vec![test_store_path("reap-d2-out")];
        n
    };

    // Build B (narrow): pruned merge keeps {d1} (stamped, childless),
    // drops d2, parks d1's walk on the QPI gate. B stays interested.
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(
        &handle,
        build_b,
        vec![mk_d1(vec!["out".into()]), mk_d2()],
        vec![make_test_edge("reap-d1", "reap-d2")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        expect_drv(&handle, "reap-d1").await.topdown_pruned,
        "precondition: B's pruned merge stamps d1"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.total_derivations,
        1,
        "precondition: B took the roots-only prune path"
    );

    // Build A (wide): the duplicate submission {d1 -> d2}; the wide
    // want blocks the prune -> full merge -> d2 enters the DAG under d1.
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag(
        &handle,
        build_a,
        vec![mk_d1(vec!["out".into(), "wide".into()]), mk_d2()],
        vec![make_test_edge("reap-d1", "reap-d2")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("reap-d2").await?.is_some(),
        "precondition: A's full merge brings d2 into the DAG"
    );

    // Stage the survivor shape the reap hook must settle: d1 out of
    // Substituting (its walk verdict was "consumed elsewhere" — forced
    // Ready), the one-shot per case, the wanted output now PRESENT,
    // and the QPI gate disarmed so a (post-fix) verification walk can
    // run; the original parked walk stays parked.
    assert!(handle.debug_set_substitute_tried("reap-d1", tried).await?);
    assert!(
        handle
            .debug_force_status("reap-d1", DerivationStatus::Ready)
            .await?
    );
    store.seed_with_content(&out, b"present");
    store
        .faults
        .query_path_info_gate_armed
        .store(false, std::sync::atomic::Ordering::SeqCst);

    // Cancel A and reap its sole-interest nodes: d2 is removed, d1
    // survives — marked, closure-holed (Broken), Ready, with B's live
    // interest and a PRESENT wanted output.
    cancel_build(&handle, build_a).await?;
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: build_a })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("reap-d2").await?.is_none(),
        "precondition: A's sole-interest d2 must be reaped"
    );

    // THE RED ASSERTION: the hook must have acted at reap time (before
    // any tick/sweep runs).
    let status_after_reap = expect_drv(&handle, "reap-d1").await.status;
    assert_ne!(
        status_after_reap,
        DerivationStatus::Ready,
        "reap hook took no action on the marked+Broken survivor (pre-fix: untried \
         survivors are skipped entirely; the settlement obligation is violated at \
         this decision point)"
    );

    if tried {
        // One-shot spent => settlement-by-fail-fast. Pin the bound.
        let st = query_status(&handle, build_b).await?;
        assert_eq!(
            st.state,
            rio_proto::types::BuildState::Failed as i32,
            "tried survivor: the reap-time settlement takes the fail-fast"
        );
        assert!(
            st.error_summary.contains("resubmit"),
            "the fail-fast must direct resubmit; got {:?}",
            st.error_summary
        );
    } else {
        // Untried + present => verification walk spawned at reap time.
        // The walk runs against the locally-present MockStore output, so
        // it can land before this query runs — accept in-flight OR
        // already-completed (both prove the settlement acted at reap
        // time; the terminal assertions below pin the outcome).
        assert!(
            matches!(
                status_after_reap,
                DerivationStatus::Substituting | DerivationStatus::Completed
            ),
            "untried survivor with obtainable outputs: the reap-time settlement \
             spawns the verification walk; got {status_after_reap:?}"
        );
        settle_substituting(&handle, &["reap-d1"]).await;
        tick(&handle).await?;
        assert_eq!(
            expect_drv(&handle, "reap-d1").await.status,
            DerivationStatus::Completed,
            "the verification walk completes the survivor from the store"
        );
        let st = query_status(&handle, build_b).await?;
        assert_eq!(
            st.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "the surviving build succeeds once the survivor settles"
        );
    }
    Ok(())
}

// ---------------------------------------------------------------------------
// Build not-found paths
// ---------------------------------------------------------------------------

/// CancelBuild for a never-submitted build_id → BuildNotFound.
#[tokio::test]
async fn test_cancel_unknown_build_returns_not_found() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: Uuid::new_v4(),
            caller_tenant: None,
            reason: "test".into(),
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx.await?;
    assert!(
        matches!(result, Err(ActorError::BuildNotFound(_))),
        "unknown build → BuildNotFound, got {result:?}"
    );
    Ok(())
}

/// QueryBuildStatus for unknown build_id → BuildNotFound.
#[tokio::test]
async fn test_query_unknown_build_returns_not_found() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let result = try_query_status(&handle, Uuid::new_v4()).await?;
    assert!(
        matches!(result, Err(ActorError::BuildNotFound(_))),
        "unknown build → BuildNotFound"
    );
    Ok(())
}

/// BuildInputsResolved fires between BuildStarted and the first
/// dispatch-phase event. On a fresh single-node build with a worker
/// present, the merge-time event sequence is:
///   Started → InputsResolved → DerivationEvent::Started (dispatch fired)
///
/// This is the signal boundary: "store cache-check done, moving to
/// dispatch." Originally destined for the Build CRD's InputsResolved
/// condition; survives for gateway STDERR_NEXT (P0294 ripped the CRD).
#[tokio::test]
async fn test_inputs_resolved_fires_between_started_and_dispatch() -> TestResult {
    use rio_proto::types::build_event::Event;

    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "inputs-drv", PriorityClass::Scheduled).await?;
    // Open the pull attempt so the delivery-phase DrvStarted is emitted.
    let _assignment = pull_attempt(&handle, "inputs-drv").await;

    // Collect all merge-time + dispatch events. Single-node fresh
    // build with no cache hits: no DerivationCached events — the
    // sequence is tight. Drain until DerivationEvent::Started (the
    // first dispatch-phase event) OR timeout.
    let mut seq = Vec::new();
    loop {
        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event within 2s")?;
        let discriminant = match &ev.event {
            Some(Event::Started(_)) => "Started",
            Some(Event::InputsResolved(_)) => "InputsResolved",
            Some(Event::Derivation(d)) => match d.kind() {
                rio_proto::types::DerivationEventKind::Started => "DrvStarted",
                other => panic!("unexpected DerivationEvent kind: {other:?}"),
            },
            other => panic!("unexpected event in merge sequence: {other:?}"),
        };
        seq.push(discriminant);
        if discriminant == "DrvStarted" {
            break;
        }
    }

    // Precondition: we actually collected enough to assert ordering.
    // Without this, a "proves nothing" shortcut (e.g., the loop
    // breaking on the first iteration) would pass trivially.
    assert!(
        seq.len() >= 3,
        "expected ≥3 events (Started, InputsResolved, DrvStarted); got {seq:?}"
    );

    // Find positions by discriminant. Stream order IS event order —
    // the broadcast ring preserves emit order, and there are no
    // sequence numbers to cross-check anymore.
    let pos = |name: &str| {
        seq.iter()
            .position(|d| *d == name)
            .unwrap_or_else(|| panic!("{name} missing from sequence {seq:?}"))
    };
    let started_at = pos("Started");
    let resolved_at = pos("InputsResolved");
    let drv_at = pos("DrvStarted");

    assert!(
        started_at < resolved_at,
        "Started must precede InputsResolved: {seq:?}"
    );
    assert!(
        resolved_at < drv_at,
        "InputsResolved must precede first dispatch: {seq:?}"
    );

    Ok(())
}

/// InputsResolved also fires on the all-cached fast path — "resolved
/// to zero work" is still resolved. No worker needed: with no worker
/// AND no cache hits, a fresh node would sit Created forever (dispatch
/// is a no-op). So we observe this via stream order alone: Started →
/// InputsResolved → (no dispatch, build waits). The test just checks
/// InputsResolved arrives even when dispatch_ready() is a no-op.
#[tokio::test]
async fn test_inputs_resolved_fires_without_worker() -> TestResult {
    use rio_proto::types::build_event::Event;

    // No worker: dispatch_ready() is a no-op. The event must still fire.
    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "noworker-drv", PriorityClass::Scheduled).await?;

    let mut saw_started = false;
    let mut saw_resolved = false;
    // Two recv()s suffice: fresh node, no cache-hit events, no
    // dispatch events. Merge emits exactly Started → InputsResolved.
    for _ in 0..2 {
        let ev = tokio::time::timeout(Duration::from_secs(2), events.recv())
            .await
            .expect("event within 2s")?;
        match ev.event {
            Some(Event::Started(_)) => saw_started = true,
            Some(Event::InputsResolved(_)) => {
                assert!(
                    saw_started,
                    "InputsResolved arrived before Started (ordering bug)"
                );
                saw_resolved = true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    }
    assert!(saw_resolved, "InputsResolved never fired");

    Ok(())
}

/// BuildProgress fires on dispatch carrying the assigned worker.
///
/// Dispatch emits DerivationStarted → Progress (in that order, same
/// interested_builds loop iteration). The Progress snapshot reflects
/// the post-assign state: running=1, the worker is in assigned_executors.
/// critical_path_remaining_secs is Some (always populated — even if
/// the estimator gave 0).
#[tokio::test]
async fn test_progress_event_on_dispatch_carries_worker() -> TestResult {
    use rio_proto::types::build_event::Event;

    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "prog-drv", PriorityClass::Scheduled).await?;
    // Open the pull attempt: the mint emits DrvStarted then Progress.
    let _assignment = pull_attempt(&handle, "prog-drv").await;

    // Drain until Progress. Single-node fresh build with worker:
    // Started → InputsResolved → DrvStarted → Progress. The Progress
    // is the one emit_progress() fires inside the dispatch loop.
    let mut saw_drv_started = false;
    let progress = loop {
        let ev = tokio::time::timeout(Duration::from_secs(5), events.recv())
            .await
            .expect("event within 5s")?;
        match ev.event {
            Some(Event::Started(_)) | Some(Event::InputsResolved(_)) => {}
            Some(Event::Derivation(d)) => {
                // DrvStarted should precede Progress (emit order in
                // dispatch.rs). Assert we see it first.
                assert_eq!(d.kind(), rio_proto::types::DerivationEventKind::Started);
                saw_drv_started = true;
            }
            Some(Event::Progress(p)) => break p,
            other => panic!("unexpected event before Progress: {other:?}"),
        }
    };

    // Precondition: DrvStarted actually arrived BEFORE Progress. If
    // dispatch's emit order ever flips, this catches it — the
    // dashboard relies on Progress reflecting post-assign state, so
    // ordering matters.
    assert!(
        saw_drv_started,
        "DerivationStarted must precede Progress (dispatch emit order)"
    );

    // The Progress snapshot reflects one running drv on prog-w.
    assert_eq!(progress.running, 1);
    assert_eq!(progress.queued, 0);
    assert_eq!(progress.total, 1);
    assert_eq!(
        progress.assigned_executors,
        vec!["prog-drv"],
        "dispatch sets assigned_executor before emitting; Progress must carry it"
    );
    assert!(
        progress.critical_path_remaining_secs.is_some(),
        "critpath always Some — scheduler always has an estimate (even if 0)"
    );

    Ok(())
}

/// Cancelling a large build must not stall the actor on sequential PG
/// writes. Before the batch fix, `cancel_build_derivations` issued
/// 2×N PG round-trips (persist_status + unpin) inside the actor loop;
/// a 100-drv cancel would block heartbeats for ~200 RTTs. After the
/// fix, it's 2 round-trips total — the actor returns to the command
/// loop fast enough for a following heartbeat to process within a
/// tight timeout.
///
/// Shape of the test: connect N workers so all N independent
/// derivations dispatch (they must be Assigned/Running to enter
/// the `to_cancel` set — Ready derivations are handled by
/// `remove_build_interest` which was never the bottleneck). Then
/// cancel; then assert a heartbeat processes within 5s. With
/// batched writes this is trivial (<100ms); with N+1 writes
/// against even a local PG it's borderline and against a network-
/// latency PG it blows through entirely.
#[tokio::test]
async fn test_cancel_large_build_does_not_stall_actor() -> TestResult {
    const N: usize = 100;
    let (_db, handle, _task) = setup().await;

    // 100 independent nodes (no edges) — all become Ready on merge.
    let build_id = Uuid::new_v4();
    let nodes: Vec<_> = (0..N)
        .map(|i| make_node(&format!("batch-{i:03}")))
        .collect();
    let _ev = merge_dag(&handle, build_id, nodes, vec![], false).await?;

    // Open a pull attempt for every node so all N are in flight
    // (Assigned/Running) and land in the cancel's `to_cancel` set.
    for i in 0..N {
        let _ = pull_attempt(&handle, &format!("batch-{i:03}")).await;
    }

    // Cancel the build. With batched PG writes this returns quickly.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "batch-cancel test".into(),
            reply: reply_tx,
        })
        .await?;
    // The reply oneshot is sent only AFTER handle_cancel_build (incl.
    // all PG writes) returns, so timing reply_rx itself IS the
    // load-bearing assertion. 5s is >50× the expected ~100ms for 2
    // batched local-PG round-trips — generous slack budget per
    // ci-failure-patterns.md "Wall-clock gate under load". Mutation-
    // verified: reverting persist_status_batch/unpin_best_effort_batch
    // to per-item loops blows through this on a network-latency PG.
    let cancelled = tokio::time::timeout(Duration::from_secs(5), reply_rx)
        .await
        .expect(
            "cancel should complete within 5s with batched PG writes (was 2×N RTTs before)",
        )??;
    assert!(cancelled);

    // Functional check: build is Cancelled and actor is responsive.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Cancelled as i32);

    Ok(())
}

/// CancelBuild reaps sole-interest DAG nodes after cleanup.
///
/// `cancel_build_derivations` strips build interest BEFORE
/// `handle_cleanup_terminal_build` calls `remove_build_interest_and_reap`.
/// The previous `was_interested` guard saw the empty set and reaped
/// nothing — every cancelled build leaked its DAG nodes for the
/// process lifetime. Only `complete_build` (which skips
/// `cancel_build_derivations`) actually reaped.
#[tokio::test]
async fn test_cancel_reaps_dag_nodes() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 3 sole-interest nodes: dispatched (Assigned/Running) + 2 queued
    // behind it (Queued → DependencyFailed). Covers both transition
    // arms of cancel_build_derivations.
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![
            make_node("reap-a"),
            make_node("reap-b"),
            make_node("reap-c"),
        ],
        vec![
            make_test_edge("reap-b", "reap-a"),
            make_test_edge("reap-c", "reap-a"),
        ],
        false,
    )
    .await?;
    let _ = pull_attempt(&handle, "reap-a").await;

    let (tx, rrx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "reap test".into(),
            reply: tx,
            caller_tenant: None,
        })
        .await?;
    assert!(rrx.await??);

    // Inject cleanup directly (bypass TERMINAL_CLEANUP_DELAY per
    // test_terminal_build_cleanup_after_delay precedent).
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id })
        .await?;
    barrier(&handle).await;

    for h in ["reap-a", "reap-b", "reap-c"] {
        assert!(
            handle.debug_query_derivation(h).await?.is_none(),
            "cancelled sole-interest node {h:?} must be reaped"
        );
    }
    Ok(())
}

/// `handle_cancel_build` records `build_duration_seconds`.
///
/// Previously it open-coded `transition + db.update_build_status`
/// instead of calling `transition_build`, so cancelled builds bumped
/// `builds_total{outcome="cancelled"}` but never the duration histogram
/// — `histogram_count` ≠ `sum(builds_total)` and percentiles biased high.
#[tokio::test]
async fn test_cancel_records_duration_histogram() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task) = setup().await;
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "cdur-h", PriorityClass::Scheduled).await?;

    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "duration test".into(),
            reply: tx,
            caller_tenant: None,
        })
        .await?;
    assert!(rx.await??);

    assert!(
        recorder.histogram_touched("rio_scheduler_build_duration_seconds"),
        "cancelled build must record into build_duration_seconds"
    );
    assert_eq!(
        recorder.get("rio_scheduler_builds_total{outcome=cancelled}"),
        1,
        "builds_total{{outcome=cancelled}} should increment exactly once"
    );
    Ok(())
}

/// Cancellation of an `Assigned`/`Running` derivation MUST record the
/// exec→build correlation (`build_derivations.exec_id`) just like the
/// success and permanent-failure terminals.
///
/// Exercises `cancel_build_derivations` directly (the chokepoint shared
/// by all four callers: user cancel, per-build wall-clock timeout,
/// fail-fast, top-down substitute fail).
///
/// Pre-fix: `cancel_build_derivations` transitioned to `Cancelled`,
/// sent `CancelSignal`, persisted — but never recorded the
/// correlation. Net effect: `bd.exec_id` stays NULL → the dashboard
/// falls back to the latest-exec log instead of the exact execution
/// this build observed.
///
/// r[verify sched.merge.exec-correlation+7]
#[rstest::rstest]
#[case::running(DerivationStatus::Running)]
#[case::assigned(DerivationStatus::Assigned)]
#[tokio::test]
async fn cancel_running_drv_records_exec_correlation(
    #[case] from_status: DerivationStatus,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed the rows record_exec_correlation's UPDATE targets — same
    // pattern as exec_correlation_falls_back_to_buffer_exec_id.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'running') \
         RETURNING derivation_id",
    )
    .bind("can-drv")
    .bind(test_drv_path("can-drv"))
    .fetch_one(&db.pool)
    .await?;
    let build_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(derivation_id)
        .execute(&db.pool)
        .await?;

    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );

    // Inject a drv with state.exec_id and assigned_executor — the shape
    // assign_to_worker produces. test_inject_ready_row injects at Ready;
    // promote to Assigned/Running via test helper and add the build
    // interest the collect-loop filters on.
    let exec_id = uuid::Uuid::now_v7();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        exec_id: Some(exec_id),
        assigned_builder_id: Some("worker-1".into()),
        ..crate::db::RecoveryDerivationRow::test_default("can-drv", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("can-drv").expect("just injected");
        s.set_status_for_test(from_status);
        s.interested_builds.insert(build_id);
    }

    // Cancel the build — the drv is sole-interest Assigned/Running, so
    // it lands in `to_cancel` (CancelSignal + transition Cancelled).
    actor
        .cancel_build_derivations(build_id, "test cancel")
        .await;

    // Exec correlation recorded — dashboard fetches the exact log
    // observed by this build instead of the latest-exec fallback.
    // Spawned write; poll PG (established 10ms × 100 pattern).
    let mut got: Option<Uuid> = None;
    for _ in 0..100 {
        got = sqlx::query_scalar(
            "SELECT exec_id FROM build_derivations \
             WHERE build_id = $1 AND derivation_id = $2",
        )
        .bind(build_id)
        .bind(derivation_id)
        .fetch_one(&db.pool)
        .await?;
        if got.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        got,
        Some(exec_id),
        "cancel must record bd.exec_id so the dashboard fetches the \
         exact log instead of the latest-exec fallback"
    );
    Ok(())
}
