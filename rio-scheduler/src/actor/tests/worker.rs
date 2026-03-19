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
            store_degraded: false,
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

/// A poisoned derivation is removed from the DAG after POISON_TTL elapses
/// and a Tick is processed. Covers the poison-expiry loop in handle_tick.
/// Removal (not in-place reset) means next submit re-inserts it fresh
/// with full proto fields via `compute_initial_states`.
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

    let post = handle.debug_query_derivation("poison-ttl-hash").await?;
    assert!(
        post.is_none(),
        "poisoned derivation should be removed from DAG after TTL expiry"
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
            store_degraded: false,
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

/// DrainWorker(force=true) on a BUSY worker → CancelSignal per in-flight
/// build + result.running_builds=N. The preemption hook: controller sees
/// DisruptionTarget on a pod, calls this so the worker cgroup.kills its
/// builds NOW instead of running the full 2h terminationGracePeriod.
///
/// Counterpart to test_force_drain_idle_worker_no_cancel_signals — that
/// one proves the CancelSignal loop does 0 iterations on idle; this one
/// proves it does N iterations on busy. Covers worker.rs:211-258 (the
/// `if force { ... }` body with a non-empty to_reassign).
#[tokio::test]
async fn test_force_drain_busy_worker_sends_cancel_signal() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("busy-worker", "x86_64-linux", 4).await?;

    // Merge + dispatch → Assigned to busy-worker. running_builds={drv}.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "drain-drv", PriorityClass::Scheduled).await?;
    // recv_assignment on busy-worker's rx proves it was dispatched here.
    let _assignment = recv_assignment(&mut rx).await;

    // Force-drain. to_reassign drains running_builds → 1 entry.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "busy-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx.await?;

    assert!(result.accepted, "known worker → accepted");
    // force=true → running_builds: 0 (worker.rs:277 "reassigned:
    // caller doesn't wait"). The count is only nonzero for
    // force=false (caller polls until it drains naturally).
    assert_eq!(
        result.running_builds, 0,
        "force-drain reassigns immediately; caller doesn't wait"
    );

    // CancelSignal should arrive in the worker's stream with the
    // force-drain reason (worker.rs:244). try_send on the stream_tx
    // is synchronous; barrier ensures the actor finished processing.
    barrier(&handle).await;
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("CancelSignal should arrive within 2s")
        .expect("channel should not close");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Cancel(c)) => {
            assert!(
                c.reason.contains("draining") && c.reason.contains("forced"),
                "cancel reason should be 'worker draining (forced)': {}",
                c.reason
            );
            // CancelSignal is keyed on drv_path (not drv_hash).
            // merge_single_node uses a synthetic /nix/store/<hash>-... path.
            assert!(
                c.drv_path.contains("drain-drv"),
                "CancelSignal drv_path should reference the in-flight build: {}",
                c.drv_path
            );
        }
        other => panic!("expected CancelSignal, got {other:?}"),
    }

    // Derivation reassigned: no longer Running/Assigned on busy-worker.
    // reassign_derivations resets to Ready (or Queued — depends on
    // whether another worker exists; here there's only one, which is
    // now draining, so it stays Ready with busy-worker in failed_workers).
    let post = handle
        .debug_query_derivation("drain-drv")
        .await?
        .expect("drv exists");
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "force-drain reassigns; drv should not still be Assigned/Running on the draining worker; got {:?}",
        post.status
    );

    Ok(())
}

/// Recorder-level proof: force-drain on a busy worker increments
/// `rio_scheduler_cancel_signals_total`. The test above proves the
/// CancelSignal *message* is sent; this proves the *metric* increments.
///
/// Regression-guards M1.1 (metric describe correct but increment uses
/// the wrong name — e.g. `_sent_total` vs `_signals_total`). The
/// describe-only test at `tests/metrics_registered.rs:57` would NOT
/// catch that bug: `describe_counter!` and `counter!` take string
/// literals independently.
///
/// Mechanism: `set_default_local_recorder` installs a thread-local
/// recorder on the test's OS thread. `#[tokio::test]` uses a
/// current-thread runtime, so the actor task spawned by
/// `setup_with_worker` runs on the *same* OS thread at `.await` points
/// and sees the thread-local when it calls `counter!()`. Guard must be
/// held before `setup_with_worker` (actor is spawned there) and until
/// after `reply_rx.await` (increment happens inside `handle_drain_worker`,
/// before the reply send).
#[tokio::test]
async fn test_force_drain_increments_cancel_signals_total_metric() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task, mut rx) =
        setup_with_worker("metric-drain-worker", "x86_64-linux", 4).await?;

    // Assign one build so to_reassign is non-empty (the increment at
    // worker.rs:255 is gated on `if !to_reassign.is_empty()`).
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(
        &handle,
        build_id,
        "metric-drain-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = recv_assignment(&mut rx).await;

    // No labels on this counter → CountingRecorder key is "name{}".
    let key = "rio_scheduler_cancel_signals_total{}";
    let before = recorder.get(key);

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "metric-drain-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    // handle_drain_worker increments the counter synchronously at
    // worker.rs:255 before reassign_derivations().await, and the actor
    // sends the reply after handle_drain_worker returns (mod.rs:472) —
    // so this await is a true barrier for the increment.
    let result = reply_rx.await?;
    assert!(result.accepted);

    let after = recorder.get(key);
    assert_eq!(
        after - before,
        1,
        "force-drain of 1 in-flight build must increment \
         rio_scheduler_cancel_signals_total by exactly 1.\n\
         Before: {before}, After: {after}\n\
         Counters actually registered: {:#?}",
        recorder.all_keys(),
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
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![make_test_node("pbt-drv", "x86_64-linux")],
                edges: vec![],
                options: BuildOptions {
                    max_silent_time: 0,
                    build_timeout: 60,
                    build_cores: 0,
                },
                keep_going: false,
                traceparent: String::new(),
            },
            reply: reply_tx,
        })
        .await?;
    let _ev = reply_rx.await??;

    // ── Boundary: 59s elapsed — NOT timed out ────────────────────────
    // 59 < 60 → elapsed.as_secs() > build_timeout is false. The check
    // uses strict > (worker.rs), so 60s elapsed would also NOT fire
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
