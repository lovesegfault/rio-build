//! Build lifecycle: CancelBuild, WatchBuild, terminal cleanup.
// r[verify sched.build.state]

use super::*;
use rio_proto::types::build_event::Event;

enum Terminalize {
    Success,
    PermanentFailure,
    Cancel,
}

/// Late WatchBuild on an already-terminal build immediately replays the
/// terminal event (Completed/Failed/Cancelled). Without re-send: if the
/// original event was sent to zero receivers (submit subscriber
/// disconnected before completion), a late WatchBuild would hang forever.
#[rstest::rstest]
#[case::completed(Terminalize::Success)]
#[case::failed(Terminalize::PermanentFailure)]
#[case::cancelled(Terminalize::Cancel)]
#[tokio::test]
async fn test_watch_build_after_terminal_replays_event(#[case] how: Terminalize) -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("watch-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let original_rx =
        merge_single_node(&handle, build_id, "watch-hash", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    match how {
        Terminalize::Success => {
            complete_success_empty(&handle, "watch-w", &test_drv_path("watch-hash")).await?;
        }
        Terminalize::PermanentFailure => {
            complete_failure(
                &handle,
                "watch-w",
                &test_drv_path("watch-hash"),
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
                    reason: "test cancel".into(),
                    reply: tx,
                })
                .await?;
            let _ = rx.await??;
        }
    }
    barrier(&handle).await;
    drop(original_rx);

    // Late WatchBuild → terminal event replayed within 2s, not hang.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let (mut watch_rx, _) = reply_rx.await??;

    let event = tokio::time::timeout(Duration::from_secs(2), watch_rx.recv())
        .await
        .expect("WatchBuild on terminal build should not hang")
        .expect("should receive an event");
    let ok = match how {
        Terminalize::Success => matches!(event.event, Some(Event::Completed(_))),
        Terminalize::PermanentFailure => matches!(event.event, Some(Event::Failed(_))),
        Terminalize::Cancel => matches!(event.event, Some(Event::Cancelled(_))),
    };
    assert!(
        ok,
        "late WatchBuild should replay terminal; got {:?}",
        event.event
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
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("cleanup-worker", "x86_64-linux").await?;

    // Complete a build.
    let build_id = Uuid::new_v4();
    let drv_hash = "cleanup-hash";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    complete_success_empty(&handle, "cleanup-worker", &drv_path).await?;

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
    let (_db, handle, _task, _rx) =
        setup_with_worker("watch-events-worker", "x86_64-linux").await?;

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
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let (mut watch_rx, _last_seq) = reply_rx.await??;

    // Complete the build; watcher should see BuildCompleted.
    complete_success_empty(
        &handle,
        "watch-events-worker",
        &test_drv_path("watch-events-hash"),
    )
    .await?;

    let mut saw_completed = false;
    // Drain events with a timeout.
    for _ in 0..10 {
        match tokio::time::timeout(Duration::from_millis(200), watch_rx.recv()).await {
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

/// emit_build_event persists state-machine events but filters out
/// Event::Log — a chatty rustc would flood PG otherwise. Log lines
/// are already durable via the S3 LogFlusher; only Started/
/// Completed/Derivation* matter for since_sequence replay.
///
/// Unit test on a bare DagActor (not running): we control the
/// persister channel directly, call emit_build_event, then drain
/// try_recv to see what got through the filter.
#[tokio::test]
async fn test_emit_build_event_filters_log_from_persister() -> TestResult {
    use rio_proto::types::build_event::Event;
    use rio_proto::types::{BuildCancelled, BuildLogBatch};

    let db = TestDb::new(&MIGRATOR).await;
    // Small channel (not the production 1000) — 3 events expected,
    // 10 gives headroom if the filter breaks.
    let (tx, mut rx) = mpsc::channel::<crate::event_log::EventLogEntry>(10);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            event_persist_tx: Some(tx),
            ..Default::default()
        },
    );

    let build_id = Uuid::new_v4();

    // 1. State event → persisted.
    actor.events.emit(
        build_id,
        Event::Cancelled(BuildCancelled {
            reason: "test".into(),
        }),
    );
    // 2. Log event → FILTERED. Default::default() — only the
    // discriminant matters for the filter.
    actor
        .events
        .emit(build_id, Event::Log(BuildLogBatch::default()));
    // 3. State event → persisted. seq=2 (Log did NOT consume a seq).
    actor.events.emit(
        build_id,
        Event::Cancelled(BuildCancelled {
            reason: "again".into(),
        }),
    );

    // Drain. try_recv — the channel is synchronous (no persister
    // task running), so everything sent is already queued.
    let mut received = Vec::new();
    while let Ok(entry) = rx.try_recv() {
        received.push(entry);
    }

    assert_eq!(
        received.len(),
        2,
        "Log filtered, two Cancelled persisted. Got: {received:?}"
    );
    assert_eq!(received[0].0, build_id);
    assert_eq!(received[0].1, 1, "first Cancelled at seq=1");
    assert_eq!(
        received[1].1, 2,
        "second Cancelled at seq=2 — Log MUST NOT consume a seq \
         (broadcast carries last-persisted seq; gateway tracker overwrites). \
         Consuming a seq diverges in-memory from PG MAX(sequence) → \
         since_sequence replay guard misfires after failover."
    );

    // Bytes decode back to the same event (proves encode is right
    // — read_event_log in db/recovery.rs (re-exported as
    // db::read_event_log) will decode these).
    use prost::Message;
    let decoded = rio_proto::types::BuildEvent::decode(&received[0].2[..])?;
    assert!(matches!(decoded.event, Some(Event::Cancelled(_))));
    assert_eq!(decoded.sequence, 1);

    Ok(())
}

/// handle_cleanup_terminal_build fires a GC DELETE for the
/// persisted event log. Fire-and-forget — the actor doesn't wait
/// on PG. Test: persist some rows, run cleanup, poll PG until
/// they're gone.
///
/// Gated on event_persist_tx.is_some() — without a persister, the
/// cleanup doesn't touch PG (most tests use the None path). This
/// test sets the persister AND writes rows directly (bypassing
/// the persister task; we're testing the DELETE, not the INSERT).
#[tokio::test]
async fn test_cleanup_terminal_build_gc_deletes_event_log() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Dummy channel — just needs is_some() for the gate. Never read.
    let (tx, _rx) = mpsc::channel::<crate::event_log::EventLogEntry>(1);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            event_persist_tx: Some(tx),
            ..Default::default()
        },
    );

    let build_id = Uuid::new_v4();
    let other_build = Uuid::new_v4();

    // Insert rows for TWO builds. Cleanup should only delete ours.
    for (id, seq) in [(build_id, 1), (build_id, 2), (other_build, 1)] {
        sqlx::query(
            "INSERT INTO build_event_log (build_id, sequence, event_bytes) VALUES ($1, $2, $3)",
        )
        .bind(id)
        .bind(seq as i64)
        .bind(vec![0u8])
        .execute(&db.pool)
        .await?;
    }

    // build_id isn't in self.builds → is_terminal=true (already
    // removed). The cleanup path short-circuits to "fine".
    actor.handle_cleanup_terminal_build(build_id);

    // DELETE is fire-and-forget spawn. Poll until it lands.
    let remaining: i64 = tokio::time::timeout(Duration::from_secs(5), async {
        loop {
            let n: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM build_event_log WHERE build_id = $1")
                    .bind(build_id)
                    .fetch_one(&db.pool)
                    .await
                    .unwrap();
            if n == 0 {
                return n;
            }
            tokio::task::yield_now().await;
        }
    })
    .await?;

    assert_eq!(remaining, 0, "target build's rows deleted");

    // Other build's rows untouched (DELETE is scoped by build_id).
    let other_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM build_event_log WHERE build_id = $1")
            .bind(other_build)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(other_count, 1, "unrelated build's rows survive");

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

    let (_db, handle, _task, _stream_rx) = setup_with_worker("inputs-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "inputs-drv", PriorityClass::Scheduled).await?;

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
        seq.push((ev.sequence, discriminant));
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

    // Find positions by discriminant.
    let pos = |name: &str| {
        seq.iter()
            .position(|(_, d)| *d == name)
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

    // Sequence numbers are monotonic — emit_build_event bumps seq
    // per call; InputsResolved consumed a seq between them.
    assert!(
        seq[started_at].0 < seq[resolved_at].0 && seq[resolved_at].0 < seq[drv_at].0,
        "sequence numbers must be strictly increasing: {seq:?}"
    );

    Ok(())
}

/// InputsResolved also fires on the all-cached fast path — "resolved
/// to zero work" is still resolved. No worker needed: with no worker
/// AND no cache hits, a fresh node would sit Created forever (dispatch
/// is a no-op). So we observe this via sequence alone: Started →
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

    let (_db, handle, _task, _stream_rx) = setup_with_worker("prog-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let mut events =
        merge_single_node(&handle, build_id, "prog-drv", PriorityClass::Scheduled).await?;

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
        vec!["prog-w"],
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
    // P0537: one build per worker → N workers for N concurrent
    // assignments. The original single high-capacity worker is no
    // longer expressible.
    let mut rxs = Vec::with_capacity(N);
    for i in 0..N {
        rxs.push(connect_executor(&handle, &format!("batch-w-{i:03}"), "x86_64-linux").await?);
    }

    // 100 independent nodes (no edges) — all become Ready on merge
    // and dispatch one-per-worker.
    let build_id = Uuid::new_v4();
    let nodes: Vec<_> = (0..N)
        .map(|i| make_node(&format!("batch-{i:03}")))
        .collect();
    let _ev = merge_dag(&handle, build_id, nodes, vec![], false).await?;

    // Drain dispatches so the worker streams don't back up.
    // recv_assignment skips PrefetchHint for us.
    for rx in &mut rxs {
        let _ = recv_assignment(rx).await;
    }

    // Cancel the build. With batched PG writes this returns quickly.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
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
    let (_db, handle, _task, mut rx) = setup_with_worker("reap-w", "x86_64-linux").await?;

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
    let _ = recv_assignment(&mut rx).await;

    let (tx, rrx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "reap test".into(),
            reply: tx,
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

/// `cancel_signals_total` counts only signals that landed on the executor
/// stream (Ok on `try_send`), not the candidate-list length.
///
/// Three sole-interest dispatched nodes; one worker's stream_tx is
/// dropped before CancelBuild so its `try_send` fails. Expect
/// `signals_total += 2`, `dropped_total += 1`. Previously
/// `signals_total += to_cancel.len() == 3`.
#[tokio::test]
async fn test_cancel_signals_total_counts_delivered_only() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task) = setup().await;
    let mut rx0 = connect_executor(&handle, "csig-w0", "x86_64-linux").await?;
    let mut rx1 = connect_executor(&handle, "csig-w1", "x86_64-linux").await?;
    let mut rx2 = connect_executor(&handle, "csig-w2", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![
            make_node("csig-a"),
            make_node("csig-b"),
            make_node("csig-c"),
        ],
        vec![],
        false,
    )
    .await?;
    let _ = recv_assignment(&mut rx0).await;
    let _ = recv_assignment(&mut rx1).await;
    let _ = recv_assignment(&mut rx2).await;

    // Close one worker's stream so its try_send Errs (channel closed).
    // The actor still has the executor record + stream_tx; only the
    // receiver end is gone.
    drop(rx2);

    let before_signals = recorder.get("rio_scheduler_cancel_signals_total{}");
    let before_dropped = recorder.get("rio_scheduler_cancel_signal_dropped_total{}");

    let (tx, rrx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "csig test".into(),
            reply: tx,
        })
        .await?;
    assert!(rrx.await??);

    assert_eq!(
        recorder.get("rio_scheduler_cancel_signals_total{}") - before_signals,
        2,
        "signals_total counts delivered (Ok on try_send) only — was to_cancel.len()=3 before"
    );
    assert_eq!(
        recorder.get("rio_scheduler_cancel_signal_dropped_total{}") - before_dropped,
        1,
        "closed-stream worker contributes to dropped_total"
    );
    Ok(())
}
