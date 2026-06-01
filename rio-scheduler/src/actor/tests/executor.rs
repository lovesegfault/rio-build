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
            reply: reply_tx,
        })
        .await?;
    let (rx2, _snapshot) = reply_rx.await??;

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
