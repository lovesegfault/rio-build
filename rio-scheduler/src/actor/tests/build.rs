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

    // Late WatchBuild → terminal event replayed within 2s, not hang.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            caller_tenant: None,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let (mut watch_rx, _) = reply_rx.await??;

    let event = tokio::time::timeout(Duration::from_secs(2), watch_rx.state.recv())
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
            caller_tenant: None,
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
    actor.handle_cleanup_terminal_build(build_id).await;

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

/// Terminal-build cleanup must not destroy a log buffer whose final flush
/// was deferred by the flusher (finalize guard could not read drv_logs — PG
/// outage at final-flush time): the retried flush's drain is that entry's
/// reaper, and discarding it here would lose the only copy of the log while
/// S3 is healthy. Unmarked buffers keep today's behavior (the discard bounds
/// a dropped-FlushRequest leak to ~60s).
///
/// Fixture note: the two drv_paths carry DISTINCT (valid, 32-char) store
/// hashes because `drv_log_hash` keys the buffers on the path's hash part —
/// the default `test_drv_path` fixture uses one shared `TEST_HASH` for every
/// name, which would silently collapse both buffers into one entry and make
/// the assertions vacuous (the fixture-collision trap).
/// r[verify obs.log.deferred-final-retry+4]
#[tokio::test]
async fn cleanup_skips_log_buffer_with_deferred_final_pending() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let bufs = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(bufs.clone()),
            ..Default::default()
        },
    );

    let build_id = Uuid::new_v4();
    let path_a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-r14clna-cleanup-deferred.drv";
    let path_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-r14clnb-cleanup-plain.drv";

    // Two sole-interest terminal drvs — both get reaped from the DAG by
    // handle_cleanup_terminal_build.
    for (hash, path) in [("r14clna", path_a), ("r14clnb", path_b)] {
        actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            drv_path: path.to_string(),
            ..crate::db::RecoveryDerivationRow::test_default(hash, "x86_64-linux")
        });
        let s = actor.dag.node_mut(hash).expect("just injected");
        s.set_status_for_test(DerivationStatus::Completed);
        s.interested_builds.insert(build_id);
    }

    // Both buffers stamped, populated, and sealed (the shape a terminal drv
    // leaves behind when its FlushRequest hasn't drained yet).
    let exec_a = Uuid::now_v7();
    let exec_b = Uuid::now_v7();
    for (path, exec) in [(path_a, exec_a), (path_b, exec_b)] {
        bufs.set_exec(path, exec, "worker-1");
        assert!(bufs.push_for(
            path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: path.to_string(),
                lines: vec![b"buffered line".to_vec()],
                first_line_number: 0,
                executor_id: "worker-1".into(),
            },
            "worker-1",
        ));
        bufs.seal(path);
    }
    // Only A's final flush was deferred (the flusher marked it).
    assert!(bufs.mark_final_pending(path_a, exec_a));

    actor.handle_cleanup_terminal_build(build_id).await;

    // Both DAG nodes reaped regardless.
    assert!(actor.dag.node("r14clna").is_none(), "A's node reaped");
    assert!(actor.dag.node("r14clnb").is_none(), "B's node reaped");

    // A's buffer survives for the flusher's retry…
    assert_eq!(
        bufs.exec_id(path_a),
        Some(exec_a),
        "deferred-final buffer must be left for the retried flush to drain"
    );
    assert!(
        bufs.read_since(path_a, 0).is_some_and(|l| l.len() == 1),
        "deferred-final buffer still holds its lines"
    );
    // …while B's is discarded exactly as before (bounds the
    // dropped-FlushRequest leak).
    assert_eq!(
        bufs.exec_id(path_b),
        None,
        "unmarked buffer is discarded at cleanup as before"
    );

    Ok(())
}

/// A final FlushRequest that is still QUEUED (enqueued by the terminal
/// epilogue, not yet attempted by the flusher) must survive
/// CleanupTerminalBuild: the epilogue marks the entry final-pending at
/// enqueue time and cleanup leaves marked entries to the flusher. Marking
/// only at deferral time (round 14) left a final queued behind earlier
/// flusher stalls unprotected — during a slow PG outage each attempt burns
/// the ~30s pool-acquire timeout, so a final enqueued behind two or more
/// stalls is first attempted only after its build's cleanup has discarded
/// the sealed buffer, and the eventual attempt finds nothing to retain.
/// The enqueue-failure path keeps today's behavior: an un-enqueued final's
/// buffer is NOT marked, so cleanup still bounds that leak to ~60s.
///
/// Fixture note: the two drv_paths carry DISTINCT (valid, 32-char) store
/// hashes because `drv_log_hash` keys buffers on the hash part — a shared
/// hash would collapse both into one entry and make the assertions
/// vacuous. Buffers are seeded via set_exec + push_for (push() leaves
/// entries unstamped and the epilogue would skip them).
/// r[verify obs.log.deferred-final-retry+4]
#[tokio::test]
async fn epilogue_marks_enqueued_final_pending_and_cleanup_preserves_it() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let bufs = std::sync::Arc::new(crate::logs::LogBuffers::new());
    // Capacity-1 flush channel, receiver held but never drained: the
    // flusher-is-backed-up state (request enqueued, not yet attempted).
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(1);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(bufs.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    let build_id = Uuid::new_v4();
    let path_a = "/nix/store/cccccccccccccccccccccccccccccccc-r15qa-queued-final.drv";
    let path_b = "/nix/store/dddddddddddddddddddddddddddddddd-r15qb-dropped-enqueue.drv";

    // Two sole-interest terminal drvs, both reaped by this build's cleanup.
    for (hash, path) in [("r15qa", path_a), ("r15qb", path_b)] {
        actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            drv_path: path.to_string(),
            ..crate::db::RecoveryDerivationRow::test_default(hash, "x86_64-linux")
        });
        let s = actor.dag.node_mut(hash).expect("just injected");
        s.set_status_for_test(DerivationStatus::Completed);
        s.interested_builds.insert(build_id);
    }

    // Both buffers stamped and holding lines; the epilogue resolves the
    // exec from the buffer carrier (state.exec_id stays None).
    let exec_a = Uuid::now_v7();
    let exec_b = Uuid::now_v7();
    for (path, exec) in [(path_a, exec_a), (path_b, exec_b)] {
        bufs.set_exec(path, exec, "worker-1");
        assert!(bufs.push_for(
            path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: path.to_string(),
                lines: vec![b"buffered line".to_vec()],
                first_line_number: 0,
                executor_id: "worker-1".into(),
            },
            "worker-1",
        ));
    }

    // A's epilogue: seal + enqueue (fills the only slot) → marked pending.
    actor.terminal_log_epilogue(&DrvHash::from("r15qa"), "succeeded", &[build_id]);
    // B's epilogue: seal + enqueue fails (channel full) → NOT marked.
    actor.terminal_log_epilogue(&DrvHash::from("r15qb"), "succeeded", &[build_id]);

    assert!(
        bufs.final_pending(path_a),
        "successful enqueue must mark the entry final-pending at the epilogue"
    );
    assert!(
        !bufs.final_pending(path_b),
        "a final that was never handed to the flusher must NOT be marked \
         (cleanup stays its only bound)"
    );

    // Cleanup fires while A's request is still sitting in the queue.
    actor.handle_cleanup_terminal_build(build_id).await;

    // A is preserved for the flusher: entry, lines, stamp, seal all intact.
    assert_eq!(
        bufs.exec_id(path_a),
        Some(exec_a),
        "queued final's buffer must survive terminal cleanup"
    );
    assert!(
        bufs.read_since(path_a, 0).is_some_and(|l| l.len() == 1),
        "queued final's buffer still holds its lines"
    );
    assert!(
        bufs.is_sealed(path_a),
        "seal stays until the flusher drains"
    );
    // The request really is still queued — the exact window the fix covers.
    let queued = flush_rx.try_recv().expect("A's final must still be queued");
    assert_eq!(queued.exec_id, exec_a);
    assert_eq!(
        queued.lease_generation, 1,
        "epilogue must stamp the enqueueing tenure's lease generation \
         (the default test plumbing's LeaderState is at generation 1)"
    );
    // B keeps the pre-existing dropped-enqueue bound.
    assert_eq!(
        bufs.exec_id(path_b),
        None,
        "un-enqueued final's buffer is still discarded at cleanup"
    );

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
            caller_tenant: None,
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

/// Cancellation of an `Assigned`/`Running` derivation MUST run the same
/// log-finalization sequence as success and permanent-failure
/// terminals: seal the ring buffer (block late `LogBatch` from
/// recreating it), enqueue a final flush (`status="cancelled"`,
/// `is_complete=true`, `.partial`→`.log.zst` swap), and record the
/// exec→build correlation (`build_derivations.exec_id`).
///
/// Exercises `cancel_build_derivations` directly (the chokepoint shared
/// by all four callers: user cancel, per-build wall-clock timeout,
/// fail-fast, top-down substitute fail).
///
/// Pre-fix: `cancel_build_derivations` transitioned to `Cancelled`,
/// sent `CancelSignal`, persisted — but never called the log-finalize
/// sequence. The worker's `CompletionReport(Cancelled)` is a no-op
/// early-return at `process_completion`. Net effect: the periodic
/// flusher's `.partial` row stays `is_complete=false`/`status=NULL`
/// for the 30-day TTL, the `.partial` blob is never replaced, and
/// `bd.exec_id` stays NULL → dashboard shows the "approximate" banner
/// for a log that was actually streamed.
///
/// r[verify sched.merge.exec-correlation+7]
/// r[verify obs.log.exec-keyed]
#[rstest::rstest]
#[case::running(DerivationStatus::Running)]
#[case::assigned(DerivationStatus::Assigned)]
#[tokio::test]
async fn cancel_running_drv_finalizes_log(#[case] from_status: DerivationStatus) -> TestResult {
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

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
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
    let drv_path = test_drv_path("can-drv");
    log_buffers.set_exec(&drv_path, exec_id, "worker-1");

    // Cancel the build — the drv is sole-interest Assigned/Running, so
    // it lands in `to_cancel` (CancelSignal + transition Cancelled +
    // log epilogue).
    actor
        .cancel_build_derivations(build_id, "test cancel")
        .await;

    // (1) Buffer sealed — late LogBatch from the still-streaming worker
    // (CancelSignal try_send may have dropped) is now rejected at
    // push_for instead of recreating an entry the flusher drained. The
    // class of late batch this drops includes the worker's
    // `rio: result cancelled` footer — it is sent only after the
    // CancelSignal's cgroup.kill lands, which is after this seal. A log
    // cancelled in flight therefore normally has no footer (this arm
    // only — the reset-arm finalize cannot drop the prior worker's
    // already-buffered footer; cancel_reset_drv_finalizes_prior_exec_log
    // pins the opposite). drv_logs.status is the outcome of record. If
    // the cancel path ever stops sealing first, that tradeoff is being
    // renegotiated — update terminal_log_epilogue's sequencing doc and
    // the observability spec's cancelled-footer paragraph to match.
    assert!(
        log_buffers.is_sealed(&drv_path),
        "cancel must seal the buffer so late LogBatch can't recreate it"
    );
    // (2) Final flush enqueued with status="cancelled".
    let req = flush_rx.try_recv().expect(
        "cancel must enqueue a final FlushRequest so the periodic \
         flusher's .partial row is finalized (is_complete=true, status set, \
         .partial blob replaced) instead of lingering for the 30-day TTL",
    );
    assert_eq!(
        req.exec_id, exec_id,
        "request pins the exec being cancelled"
    );
    assert_eq!(req.drv_path, drv_path);
    assert_eq!(req.status.as_deref(), Some("cancelled"));
    // (3) Exec correlation recorded — dashboard fetches the exact log
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

/// A `Ready`/`Substituting` drv whose prior execution was reset
/// (`reset_to_ready()` on worker disconnect / transient failure: clears
/// `state.exec_id`, retains the stamped `LogBuffers` entry) lands in
/// `cancel_build_derivations`' `to_depfail` / `to_cancel_substituting`
/// arm when the build is cancelled before re-dispatch. That arm MUST
/// finalize the retained execution's log the same way the `to_cancel`
/// (Assigned/Running) arm does — seal, flush (`status="cancelled"`,
/// `is_complete=true`, `.partial` swap), correlate — or the prior exec's
/// `drv_logs` row stays `is_complete=false`/`status=NULL` for the 30-day
/// TTL as the drv's latest (and final) execution.
///
/// The never-dispatched sibling pins the gate's other half: it has no
/// exec_id from either carrier and no buffer, so the epilogue is
/// skipped entirely — no FlushRequest, no seal tombstone.
///
/// Pre-fix: the `to_depfail`/`to_cancel_substituting` arms skipped the
/// epilogue on the (false) claim that those drvs "have no exec_id and
/// no buffer, so the call would be a guaranteed no-op".
///
/// r[verify sched.merge.exec-correlation+7]
/// r[verify obs.log.exec-keyed]
#[rstest::rstest]
#[case::ready(DerivationStatus::Ready, DerivationStatus::DependencyFailed)]
#[case::substituting(DerivationStatus::Substituting, DerivationStatus::Cancelled)]
#[tokio::test]
async fn cancel_reset_drv_finalizes_prior_exec_log(
    #[case] from_status: DerivationStatus,
    #[case] expected_terminal: DerivationStatus,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed the rows record_exec_correlation's UPDATE targets — same
    // pattern as cancel_running_drv_finalizes_log.
    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready') \
         RETURNING derivation_id",
    )
    .bind("rst-drv")
    .bind(test_drv_path("rst-drv"))
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

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    // The reset drv: `state.exec_id: None` (test_default — the shape
    // reset_to_ready() leaves), no assigned executor, but a LogBuffers
    // entry still stamped with the prior execution's exec_id.
    let exec_id = uuid::Uuid::now_v7();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("rst-drv", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("rst-drv").expect("just injected");
        s.set_status_for_test(from_status);
        s.interested_builds.insert(build_id);
    }
    let drv_path = test_drv_path("rst-drv");
    log_buffers.set_exec(&drv_path, exec_id, "old-worker");
    assert!(log_buffers.push_for(
        &drv_path,
        &rio_proto::types::BuildLogBatch {
            derivation_path: drv_path.clone(),
            lines: vec![b"line from the reset execution".to_vec()],
            first_line_number: 0,
            executor_id: "old-worker".into(),
        },
        "old-worker",
    ));
    // The lost worker's parting footer lands in the retained buffer
    // AFTER the reset: the entry is still stamped to "old-worker" and
    // not yet sealed, so push_for accepts it (force-drain sends the
    // CancelSignal and resets the drv before the worker's footer batch
    // arrives). This is the case observability.typ's cancelled-footer
    // paragraph describes — the reset-arm finalize cannot drop it.
    assert!(log_buffers.push_for(
        &drv_path,
        &rio_proto::types::BuildLogBatch {
            derivation_path: drv_path.clone(),
            lines: vec![b"rio: result   cancelled after 4s".to_vec()],
            first_line_number: 1,
            executor_id: "old-worker".into(),
        },
        "old-worker",
    ));

    // The never-dispatched sibling: same build, no buffer, no exec_id
    // from either carrier. Must NOT produce a FlushRequest or a seal.
    //
    // Fixture validity: `test_drv_path` stamps the SAME 32-char hash
    // into every path, and `LogBuffers` keys on `drv_log_hash(path)`
    // (the hash component) — the default path would alias the sibling
    // onto rst-drv's stamped buffer entry and the gate would (correctly,
    // for that key) fire for it. Give the sibling a distinct store-path
    // hash so its buffer lookup is genuinely empty.
    let nd_path = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-rst-nd.drv".to_string();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        drv_path: nd_path.clone(),
        ..crate::db::RecoveryDerivationRow::test_default("rst-nd", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("rst-nd").expect("just injected");
        s.set_status_for_test(DerivationStatus::Queued);
        s.interested_builds.insert(build_id);
    }

    actor
        .cancel_build_derivations(build_id, "test cancel")
        .await;

    // (1) Exactly one FlushRequest — for the reset drv's prior exec,
    // with the scheduler's cancel disposition.
    let req = flush_rx.try_recv().expect(
        "cancel must finalize the retained prior-exec log of a reset \
         Ready/Substituting drv instead of leaving its drv_logs row at \
         is_complete=false for the 30-day TTL",
    );
    assert_eq!(
        req.exec_id, exec_id,
        "request pins the retained buffer's stamp"
    );
    assert_eq!(req.drv_path, drv_path);
    assert_eq!(req.status.as_deref(), Some("cancelled"));
    assert!(
        flush_rx.try_recv().is_err(),
        "the never-dispatched sibling has no exec to flush"
    );

    // (2) Buffer sealed for the reset drv only — the gate keeps
    // never-dispatched drvs out of the epilogue entirely.
    assert!(log_buffers.is_sealed(&drv_path));
    assert!(
        !log_buffers.is_sealed(&nd_path),
        "never-dispatched drvs must not accumulate seal tombstones"
    );

    // (2b) The seal blocks future pushes; it does not strip what the
    // prior worker already pushed. The buffer the flusher will drain
    // still ends with that worker's footer — a status='cancelled' log
    // MAY carry a `rio: result` line that disagrees with the row
    // (observability.typ's cancelled-footer paragraph; drv_logs.status
    // is authoritative).
    let retained = log_buffers
        .read_since(&drv_path, 0)
        .expect("entry survives the seal until the flusher drains it");
    assert_eq!(
        retained.last().map(|(_, l)| l.as_slice()),
        Some(b"rio: result   cancelled after 4s".as_slice()),
        "reset-arm finalize must not strip the prior worker's already-buffered footer"
    );

    // (3) Both drvs reached the right terminal (the epilogue didn't
    // perturb the transitions).
    assert_eq!(
        actor.dag.node("rst-drv").expect("still in DAG").status(),
        expected_terminal
    );
    assert_eq!(
        actor.dag.node("rst-nd").expect("still in DAG").status(),
        DerivationStatus::DependencyFailed
    );

    // (4) Exec correlation recorded for the reset drv (spawned write;
    // established 10ms × 100 poll pattern).
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
        "cancel of a reset drv must record bd.exec_id so the dashboard \
         fetches the exact partial log this build observed"
    );
    Ok(())
}

/// A drv that went terminal (its execution finalized: buffer drained,
/// `bd.exec_id` written for the builds that observed it) and was then
/// reset out of that terminal — I-094 reprobe (`Poisoned →
/// Substituting`/`Queued`), I-047 stale-output reset (`Completed →
/// Ready`) — retains the finalized execution's `state.exec_id` but has
/// NO LogBuffers entry. Cancelling the resubmitting build before
/// re-dispatch lands it in the `to_depfail`/`to_cancel_substituting`
/// arm; the finalization gate MUST NOT fire: there is nothing left to
/// finalize, and running the epilogue would durably write `bd.exec_id`
/// for an execution this build never observed (suppressing the
/// dashboard's "approximate" banner) plus enqueue a FlushRequest that
/// `flush_final`'s staleness guard immediately drops.
///
/// Counterpart to `cancel_reset_drv_finalizes_prior_exec_log`, which
/// pins the inverse shape (`state.exec_id: None`, buffer PRESENT →
/// gate fires). Together they pin the gate to the LogBuffers carrier.
///
/// The stale `state.exec_id` is set directly on the DAG node, NOT via
/// `RecoveryDerivationRow.exec_id` — that field's recovery semantics
/// are scoped to currently-assigned drvs and the fixture must model
/// "stale by any means", not one specific writer.
///
/// r[verify sched.merge.exec-correlation+7]
#[rstest::rstest]
#[case::substituting(DerivationStatus::Substituting, DerivationStatus::Cancelled)]
#[case::ready(DerivationStatus::Ready, DerivationStatus::DependencyFailed)]
#[tokio::test]
async fn cancel_reprobed_drv_skips_finalized_exec(
    #[case] from_status: DerivationStatus,
    #[case] expected_terminal: DerivationStatus,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    let derivation_id: Uuid = sqlx::query_scalar(
        "INSERT INTO derivations (drv_hash, drv_path, pname, system, status) \
         VALUES ($1, $2, 'pkg', 'x86_64-linux', 'ready') \
         RETURNING derivation_id",
    )
    .bind("rpb-drv")
    .bind(test_drv_path("rpb-drv"))
    .fetch_one(&db.pool)
    .await?;
    let build_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(build_id)
        .execute(&db.pool)
        .await?;
    // bd row: fixture realism (an interested build always has one) and
    // FK target for cancel_build_derivations' status writes. NOT read
    // back by this test — see assertion (1)'s comment.
    sqlx::query("INSERT INTO build_derivations (build_id, derivation_id) VALUES ($1, $2)")
        .bind(build_id)
        .bind(derivation_id)
        .execute(&db.pool)
        .await?;

    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let mut actor = DagActor::new(
        SchedulerDb::new(db.pool.clone()),
        DagActorConfig::default(),
        DagActorPlumbing {
            log_buffers: Some(log_buffers.clone()),
            log_flush_tx: Some(flush_tx),
            ..Default::default()
        },
    );

    // The reprobed drv: stale `state.exec_id` from the finalized prior
    // execution, NO LogBuffers entry (flush_final drained it at the
    // prior terminal). Single drv in this test — no drv_log_hash
    // aliasing risk (LogBuffers keys on the store-path hash component,
    // which `test_drv_path` stamps identically for every name).
    let stale_exec = uuid::Uuid::now_v7();
    actor.test_inject_ready_row(crate::db::RecoveryDerivationRow {
        derivation_id,
        ..crate::db::RecoveryDerivationRow::test_default("rpb-drv", "x86_64-linux")
    });
    {
        let s = actor.dag.node_mut("rpb-drv").expect("just injected");
        s.set_status_for_test(from_status);
        s.exec_id = Some(stale_exec);
        s.interested_builds.insert(build_id);
    }
    let drv_path = test_drv_path("rpb-drv");
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        None,
        "precondition: the prior execution's buffer was already drained"
    );

    actor
        .cancel_build_derivations(build_id, "test cancel")
        .await;

    // (1) No FlushRequest — there is no buffer to finalize, and a
    // request pinning the stale exec would be dropped by flush_final's
    // staleness guard anyway. THIS IS THE LOAD-BEARING ASSERTION for
    // the durable damage too: trigger_log_flush and
    // record_exec_correlation are both unconditionally inside the same
    // gated terminal_log_epilogue call, so "no FlushRequest" proves the
    // bd.exec_id UPDATE was never issued. (A direct SELECT for
    // bd.exec_id IS NULL would need a flat sleep to wait out the
    // spawned UPDATE — a negative proven by sleeping can only
    // false-pass under load, so it is deliberately omitted.)
    assert!(
        flush_rx.try_recv().is_err(),
        "cancel of a reprobed drv with no retained buffer must not \
         enqueue a FlushRequest for the already-finalized execution"
    );
    // (2) No seal tombstone for a buffer that doesn't exist.
    assert!(
        !log_buffers.is_sealed(&drv_path),
        "no seal tombstone may be left for a drv with no buffer entry"
    );
    // (3) The transition itself still happens — skipping the epilogue
    // must not skip the terminal.
    assert_eq!(
        actor.dag.node("rpb-drv").expect("still in DAG").status(),
        expected_terminal
    );
    Ok(())
}
