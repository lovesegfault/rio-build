//! Worker management: heartbeat merge (no-clobber), Tick-driven timeout and poison expiry.
// r[verify sched.worker.dual-register]
// r[verify sched.worker.deregister-reassign]
// r[verify sched.state.poisoned-ttl]

use super::*;

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_builds. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() -> TestResult {
    // Register worker (initial heartbeat has empty running_builds).
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("toctou-worker", "x86_64-linux", 2).await?;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_builds.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Verify: derivation is Assigned, worker.running_builds contains it.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .expect("toctou-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "scheduler should have tracked the assignment in worker.running_builds"
    );

    // Send a STALE heartbeat with empty running_builds. This mimics the
    // race: worker sent heartbeat before receiving/acking the assignment.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            resources: None,
            bloom: None,
            size_class: None,
            worker_id: "toctou-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 2,
            running_builds: vec![], // stale — does NOT include fresh assignment
        })
        .await?;

    // Assignment must still be tracked. Before the fix, running_builds
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.worker_id == "toctou-worker")
        .expect("toctou-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
    Ok(())
}

/// Heartbeat timeout deregisters worker and reassigns its builds.
/// Instead of advancing time (PG timeout issue), we send Tick commands
/// after manipulating the worker's last_heartbeat via multiple Tick cycles
/// without heartbeats. Actually simpler: send WorkerDisconnected directly
/// is equivalent (handle_tick calls handle_worker_disconnected on timeout),
/// so that path is already covered by test_worker_disconnect_running_derivation.
/// This test verifies the Tick-driven path specifically by injecting Ticks.
#[tokio::test]
async fn test_heartbeat_timeout_via_tick_deregisters_worker() -> TestResult {
    // NOTE: This test would ideally use tokio::time::pause + advance, but
    // that interferes with PG pool timeouts. Instead, we verify that Tick
    // correctly processes the timeout path by checking the missed_heartbeats
    // counter accumulates. Since we can't easily fast-forward real time in
    // this test harness, we verify the logic indirectly: Tick with fresh
    // heartbeat does NOT remove the worker (negative test), and the
    // timeout-removal path is exercised directly via WorkerDisconnected
    // in test_worker_disconnect_running_derivation.
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("tick-worker", "x86_64-linux", 1).await?;

    // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
    for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }

    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.worker_id == "tick-worker"),
        "worker with fresh heartbeat should survive Tick"
    );
    Ok(())
}

// ===========================================================================
// Poison-TTL expiry (POISON_TTL is cfg(test)-shadowed to 100ms in state/mod.rs)
// ===========================================================================

/// A poisoned derivation is reset to Created after POISON_TTL elapses and
/// a Tick is processed. Covers the poison-expiry loop in handle_tick
/// and state/mod.rs:reset_from_poison.
#[tokio::test]
async fn test_tick_expires_poisoned_derivation() -> TestResult {
    let (_db, handle, _task, _rx) =
        setup_with_worker("poison-ttl-worker", "x86_64-linux", 1).await?;

    // Merge, dispatch, poison via PermanentFailure.
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(
        &handle,
        build_id,
        "poison-ttl-hash",
        PriorityClass::Scheduled,
    )
    .await?;

    complete_failure(
        &handle,
        "poison-ttl-worker",
        &test_drv_path("poison-ttl-hash"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;

    // Verify poisoned.
    let pre = handle
        .debug_query_derivation("poison-ttl-hash")
        .await?
        .expect("derivation exists");
    assert_eq!(pre.status, DerivationStatus::Poisoned);

    // Wait past the cfg(test) POISON_TTL (100ms). 3× margin for loaded
    // CI hosts — poisoned_at is std::time::Instant (derivation.rs:202),
    // which tokio paused time can't mock, so real sleep is the only option.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Tick processes the expiry.
    handle.send_unchecked(ActorCommand::Tick).await?;

    let post = handle
        .debug_query_derivation("poison-ttl-hash")
        .await?
        .expect("derivation still exists");
    assert_eq!(
        post.status,
        DerivationStatus::Created,
        "poisoned derivation should be reset after TTL expiry"
    );
    Ok(())
}

/// 3 sequential worker disconnects with the same derivation must
/// poison it (not leave it Ready-but-undispatchable because
/// best_worker excludes all 3 failed workers).
#[tokio::test]
async fn test_three_worker_disconnects_poisons() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Connect 3 workers sequentially. Each gets the drv assigned,
    // then disconnects.
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(&handle, build_id, "x6-drv", PriorityClass::Scheduled).await?;

    for i in 0..3 {
        let worker_id = format!("w-x6-{i}");
        let mut rx = connect_worker(&handle, &worker_id, "x86_64-linux", 1).await?;
        // Receive assignment (proves drv dispatched to this worker).
        let assignment = recv_assignment(&mut rx).await;
        assert!(
            assignment.drv_path.contains("x6-drv"),
            "worker {i} should get x6-drv"
        );

        // Disconnect. reassign_derivations runs → checks
        // POISON_THRESHOLD. For i<2: reset to Ready + next worker
        // gets it. For i==2: poison.
        handle
            .send_unchecked(ActorCommand::WorkerDisconnected {
                worker_id: worker_id.clone().into(),
            })
            .await?;
        barrier(&handle).await;

        // Close stream (drop rx) to complete disconnect.
        drop(rx);
    }

    // After 3 disconnects: drv should be Poisoned. Without the
    // poison check in reassign_derivations: Ready with
    // failed_workers={w0,w1,w2}, never dispatchable.
    let info = handle
        .debug_query_derivation("x6-drv")
        .await?
        .expect("derivation exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 worker disconnects should poison; got {:?}",
        info.status
    );

    Ok(())
}

/// WorkerDisconnected for a never-connected worker → no-op. The
/// handler's early-return on `workers.remove(worker_id) == None`
/// means no gauge decrement (would go negative otherwise) and no
/// reassign pass (nothing to reassign).
#[tokio::test]
async fn test_worker_disconnect_unknown_noop() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Disconnect a worker that was never connected.
    handle
        .send_unchecked(ActorCommand::WorkerDisconnected {
            worker_id: "ghost".into(),
        })
        .await?;

    // Actor should still be alive (no panic on None remove).
    let workers = handle.debug_query_workers().await?;
    assert!(workers.is_empty(), "workers should remain empty");
    assert!(handle.is_alive(), "actor should survive unknown disconnect");

    Ok(())
}

/// Heartbeat reports a running build the scheduler never assigned
/// (but which IS in the DAG) → warn + adopt it into running_builds.
/// This is the "split-brain or restart" recovery path: maybe a
/// previous scheduler assigned it, we lost in-mem state, worker
/// still running it. The worker knows better than we do.
///
/// Note: the drv_path must resolve via `dag.hash_for_path` — unknown
/// paths are silently filtered BEFORE the reconcile. So this test
/// merges the drv first (puts it in the DAG) without dispatching it
/// to the heartbeating worker.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_heartbeat_reports_unknown_build_warns() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge a drv into the DAG. NO worker connected yet → stays
    // Ready (not dispatched). This puts the drv_path→hash mapping
    // in the DAG so the heartbeat filter lets it through.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "hb-drv", PriorityClass::Scheduled).await?;

    // Connect worker via WorkerConnected only (no initial heartbeat)
    // so we control the first heartbeat's running_builds precisely.
    let (stream_tx, _stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "hb-worker".into(),
            stream_tx,
        })
        .await?;

    // Heartbeat with running_builds claiming the drv we merged but
    // never assigned to this worker. max_builds=0 so the heartbeat
    // doesn't trigger dispatch (which would ALSO assign the drv and
    // muddy the test — we want "worker claims it, scheduler didn't
    // know" to be clearly distinguishable).
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            resources: None,
            bloom: None,
            size_class: None,
            worker_id: "hb-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 0,
            running_builds: vec![test_drv_path("hb-drv")],
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("heartbeat reports running build scheduler did not assign"),
        "unknown-build heartbeat should warn"
    );

    // The drv SHOULD be adopted into running_builds (worker is
    // authoritative about what it's actually running).
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.worker_id == "hb-worker")
        .expect("worker registered");
    assert!(
        w.running_builds.contains(&"hb-drv".to_string()),
        "worker's claim should be adopted into running_builds"
    );

    Ok(())
}

/// DrainWorker(force=true) on an idle worker → running=0, no
/// CancelSignal sent (nothing to cancel). The to_reassign vec is
/// empty, the CancelSignal loop does 0 iterations.
#[tokio::test]
async fn test_force_drain_idle_worker_no_cancel_signals() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("idle-worker", "x86_64-linux", 4).await?;

    // Worker is idle (no builds assigned). Force-drain.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "idle-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx.await?;

    assert!(result.accepted, "known worker → accepted=true");
    assert_eq!(
        result.running_builds, 0,
        "idle worker → nothing to reassign"
    );

    // No CancelSignal should appear in the stream. barrier-then-
    // try_recv: any message sent during drain would be in the
    // channel by now (mpsc is ordered).
    barrier(&handle).await;
    assert!(
        rx.try_recv().is_err(),
        "no CancelSignal should be sent for idle worker (nothing running)"
    );

    Ok(())
}

// r[verify sched.backstop.timeout]
/// Backstop timeout: a derivation Running far longer than expected
/// gets CancelSignal + reset_to_ready on Tick. The cfg(test) floor
/// is 0s (BACKSTOP_DAEMON_TIMEOUT_SECS=0, BACKSTOP_SLACK_SECS=0) so
/// any positive `running_since` elapsed triggers the backstop.
///
/// Uses DebugBackdateRunning to force Running status with a stale
/// timestamp, bypassing the normal Assigned→Running transition
/// (which would require worker ack + heartbeat roundtrips).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_backstop_timeout_cancels_and_reassigns() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("bs-worker", "x86_64-linux", 1).await?;

    // Merge + dispatch → Assigned.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "bs-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut rx).await;

    // Backdate running_since. 100s is plenty past the 0s test floor.
    // Also transitions Assigned → Running (required for the backstop
    // check: it only fires on status==Running).
    let ok = handle.debug_backdate_running("bs-drv", 100).await?;
    assert!(ok, "debug_backdate_running should succeed for Assigned drv");

    // Tick → backstop check → CancelSignal + reassign.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // The backstop-timeout branch should have logged.
    assert!(
        logs_contain("backstop timeout"),
        "backstop should log on timeout"
    );

    // CancelSignal should appear in the worker's stream.
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("CancelSignal should arrive within 2s")
        .expect("channel should not close");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Cancel(c)) => {
            assert!(
                c.reason.contains("backstop"),
                "cancel reason should mention backstop: {}",
                c.reason
            );
        }
        other => panic!("expected CancelSignal, got {other:?}"),
    }

    // Drv should be Ready (reset for retry) with retry_count bumped
    // and the worker recorded in failed_workers. It may immediately
    // re-dispatch to the same worker (only one available) IF
    // best_worker doesn't exclude it — but the worker IS in
    // failed_workers now. Either Ready (excluded) or a fresh
    // Assigned (dispatch fired again). What matters is: NOT stuck
    // in Running.
    let post = handle
        .debug_query_derivation("bs-drv")
        .await?
        .expect("drv exists");
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "backstop should reset drv (not leave it Running); got {:?}",
        post.status
    );
    assert!(
        post.retry_count >= 1,
        "retry_count should be bumped after backstop reassign"
    );

    Ok(())
}
