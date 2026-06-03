//! `bridge_build_events` BuildEvent bridge tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395). Covers the
//! `bridge_build_events` function directly: broadcast-lag continue (I-144),
//! snapshot-first attach ordering, the state/display ring split, and
//! UUID-v7 build_id ordering. These tests drive the bridge with a bare
//! `broadcast::channel` rather than the full actor so ring-buffer state
//! is precisely controlled.

use super::*;
use std::time::Duration;
use tokio_stream::StreamExt;

/// Pair a state receiver with a fresh empty log channel. Tests that
/// drive `bridge_build_events` with a bare `broadcast::channel` only
/// care about the state ring; the log ring is part of the signature.
fn state_only(state: broadcast::Receiver<rio_proto::types::BuildEvent>) -> BuildEventReceivers {
    let (_tx, log) = broadcast::channel(1);
    BuildEventReceivers { state, log }
}

/// I-144: when a broadcast receiver lags, the bridge MUST keep the
/// receiver alive (continue, not break). Breaking drops the receiver →
/// `receiver_count() == 0` → orphan-watcher (5-min grace) auto-cancels
/// a build the gateway is still actively watching. Under sustained
/// burst (large DAG, many concurrent drvs) the gateway re-lagged on
/// every reconnect and the build was orphan-cancelled at 1448/153821.
///
/// Asserts:
///   1. After Lagged, `tx.receiver_count() > 0` (the bridge didn't drop
///      its subscription — this is what orphan-watcher checks).
///   2. Post-lag events are forwarded (the gap is skipped, stream
///      continues — no DATA_LOSS, no break).
// r[verify sched.backstop.orphan-watcher]
#[tokio::test]
async fn test_bridge_build_events_lagged_keeps_receiver_alive() {
    let build_id = Uuid::new_v4();
    // Capacity 1 + send 3 → rx (subscribed at channel creation) lags by 2.
    let (tx, rx) = broadcast::channel(1);
    for i in 1..=3u64 {
        let _ = tx.send(mk_event(build_id, i));
    }

    let mut stream = bridge_build_events("test-bridge", state_only(rx), None);

    // First poll: bridge's first recv() hits Lagged(2) → the in-stream
    // ResyncRequired signal precedes the post-lag events
    // (test_bridge_lagged_emits_one_resync_per_streak owns the signal
    // contract; this test owns receiver liveness). Then event 3 (oldest
    // still in the cap-1 ring). NOT an Err.
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("bridge should not hang post-lag")
        .expect("stream should yield, not end");
    let signal = first.expect("post-lag signal must be Ok, not DATA_LOSS");
    assert!(is_resync(&signal), "Lagged surfaces the resync signal");
    let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("bridge should not hang post-lag")
        .expect("stream should yield, not end")
        .expect("post-lag event must be Ok, not DATA_LOSS");
    assert_eq!(
        event_tag(&ev),
        "seq-3",
        "oldest in-ring event after Lagged reposition"
    );

    // The bridge task is still alive holding the receiver. This is the
    // property orphan-watcher checks (executor.rs tick_check_orphaned_builds).
    assert_eq!(
        tx.receiver_count(),
        1,
        "I-144: bridge must hold the broadcast receiver across Lagged \
         so orphan-watcher doesn't see receiver_count()==0"
    );

    // Subsequent events flow normally (bridge loop didn't break).
    let _ = tx.send(mk_event(build_id, 4));
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("post-lag stream should keep yielding")
        .expect("stream open")
        .expect("Ok event");
    assert_eq!(event_tag(&next), "seq-4");
    assert_eq!(tx.receiver_count(), 1, "still subscribed after second send");
}

/// State events MUST survive display-channel flooding. Before the
/// state/display channel split, display-only events and
/// `DerivationEvent` shared
/// one `broadcast(4096)` ring; chatty parallel builds (chromium /
/// firefox / rustc) flooded it, the bridge's `Lagged` skip-and-continue
/// silently dropped `DerivationEvent::Completed`, and the gateway never
/// emitted `stop_activity` — repro JSON had 44 `start` / 34 `stop`.
///
/// Asserts: emitting >> ring-capacity SubstituteProgress events on the
/// display channel does NOT prevent a single `Derivation::Completed`
/// on the state channel from reaching the bridge output.
// r[verify gw.activity.stop-parity]
#[tokio::test]
async fn test_completed_event_survives_display_flood() {
    use rio_proto::types::build_event::Event;
    let build_id = Uuid::new_v4();

    // Display ring sized at the production LOG_EVENT_BUFFER_SIZE so
    // the flood actually lags it. State ring sized at 16 — irrelevant,
    // only one state event is sent.
    let (state_tx, state_rx) = broadcast::channel(16);
    let (log_tx, log_rx) = broadcast::channel(crate::actor::LOG_EVENT_BUFFER_SIZE);
    let mut stream = bridge_build_events(
        "test-log-flood",
        BuildEventReceivers {
            state: state_rx,
            log: log_rx,
        },
        None,
    );

    // Flood the display channel well past its capacity so its receiver
    // is guaranteed Lagged. This is what emit() routes
    // Event::SubstituteProgress to.
    for _ in 0..6000 {
        let _ = log_tx.send(mk_display_event(build_id, 0));
    }
    // The state event under test: a per-derivation Completed.
    let _ = state_tx.send(rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        timestamp: None,
        event: Some(Event::Derivation(rio_proto::types::DerivationEvent {
            derivation_path: "/nix/store/x.drv".into(),
            kind: rio_proto::types::DerivationEventKind::Completed as i32,
            output_paths: vec![],
            executor_id: String::new(),
            error_message: String::new(),
            failure_status: 0,
            exec_id: String::new(),
        })),
    });

    // Drain until we see the Completed. A 2s budget at 6001 events is
    // ample (in-process). Some display events were evicted by Lagged —
    // count how many reached the bridge to assert the flood actually
    // overflowed the display ring (otherwise the test isn't proving the
    // split, just that 6000 < capacity).
    let mut log_seen = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let saw_completed = loop {
        let Ok(Some(ev)) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break false;
        };
        match ev.expect("Ok event").event {
            Some(Event::SubstituteProgress(_)) => log_seen += 1,
            Some(Event::Derivation(d))
                if d.kind == rio_proto::types::DerivationEventKind::Completed as i32 =>
            {
                break true;
            }
            other => panic!("unexpected event: {other:?}"),
        }
    };

    assert!(
        saw_completed,
        "DerivationEvent::Completed must reach the bridge despite the display flood"
    );
    assert!(
        log_seen < 6000,
        "display channel should have lagged (saw {log_seen}/6000); \
         if all logs arrived, LOG_EVENT_BUFFER_SIZE >= 6000 and the test \
         isn't exercising the split"
    );
}

/// UUID v7 build_ids are time-ordered: two submissions ~apart in time
/// produce lexicographically ordered IDs. This is the property we rely
/// on for S3 log key prefix-scanning and PG index locality.
///
/// We don't assert strict monotonicity within the same millisecond —
/// v7's counter field handles that, but testing it requires contriving
/// >1 call per ms which is flaky. Instead: sleep > 1ms between
/// submissions and assert lexicographic order. This tests the property
/// we actually care about (chronological ordering at human timescales),
/// not the RFC's intra-ms counter edge case.
#[tokio::test(flavor = "multi_thread", worker_threads = 2)]
async fn test_build_ids_are_time_ordered_v7() -> anyhow::Result<()> {
    let (_db, grpc, _handle, _actor_task) = setup_grpc().await;

    let mk_req = |tag: &str| rio_proto::types::SubmitBuildRequest {
        tenant_name: String::new(),
        priority_class: String::new(),
        nodes: vec![make_node(tag)],
        edges: vec![],
        max_silent_time: 0,
        build_timeout: 0,
        build_cores: 0,
        keep_going: false,
    };

    // First submission.
    let mut s1 = grpc
        .submit_build(tonic::Request::new(mk_req("v7-first")))
        .await?
        .into_inner();
    let id1 = s1.next().await.expect("first event").expect("ok").build_id;

    // > 1ms gap guarantees a different v7 timestamp prefix. 2ms is
    // plenty; tokio's time granularity is ~1ms on most systems.
    tokio::time::sleep(Duration::from_millis(2)).await;

    // Second submission.
    let mut s2 = grpc
        .submit_build(tonic::Request::new(mk_req("v7-second")))
        .await?
        .into_inner();
    let id2 = s2.next().await.expect("first event").expect("ok").build_id;

    // v7 IDs sort lexicographically by creation time. The string
    // representation is the canonical UUID format (8-4-4-4-12 hex
    // with lowercase a-f), and lex-order on that matches timestamp
    // order for v7 (the timestamp is in the high bits).
    assert!(
        id1 < id2,
        "v7 build_ids should be time-ordered: {id1} should sort before {id2}"
    );

    // Also verify they parse as v7 (version nibble = 7). The version
    // is the first nibble of the third hyphen-delimited group.
    let parse = |s: &str| -> Uuid { s.parse().expect("valid UUID") };
    assert_eq!(
        parse(&id1).get_version_num(),
        7,
        "build_id should be UUID v7"
    );
    assert_eq!(
        parse(&id2).get_version_num(),
        7,
        "build_id should be UUID v7"
    );

    Ok(())
}

// ===========================================================================
// Snapshot-first attach + bridge helpers
// ===========================================================================

/// Minimal BuildEvent for bridge tests. The `n` lands in the Cancelled
/// reason (`"seq-{n}"`) so tests can identify which event came through —
/// see [`event_tag`].
fn mk_event(build_id: Uuid, n: u64) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        timestamp: None,
        event: Some(Event::Cancelled(rio_proto::types::BuildCancelled {
            reason: format!("seq-{n}"),
        })),
    }
}

/// The identifying tag [`mk_event`] embedded in this event.
fn event_tag(ev: &rio_proto::types::BuildEvent) -> &str {
    use rio_proto::types::build_event::Event;
    match &ev.event {
        Some(Event::Cancelled(c)) => &c.reason,
        other => panic!("expected a mk_event Cancelled, got {other:?}"),
    }
}

/// `Event::SubstituteProgress` — display-only, routed via the log ring.
fn mk_display_event(build_id: Uuid, _n: u64) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        timestamp: None,
        event: Some(Event::SubstituteProgress(
            rio_proto::types::SubstituteProgress {
                derivation_path: "/nix/store/x".into(),
                ..Default::default()
            },
        )),
    }
}

/// A `BuildEvent::Snapshot` shaped like what `handle_watch_build` returns
/// for an active build.
fn mk_snapshot(build_id: Uuid) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        timestamp: None,
        event: Some(Event::Snapshot(rio_proto::types::BuildSnapshot {
            state: rio_proto::types::BuildState::Active as i32,
            total_derivations: 3,
            completed_derivations: 1,
            running_derivations: 1,
            queued_derivations: 1,
            ..Default::default()
        })),
    }
}

// r[verify sched.watch.snapshot-first]
/// The bridge delivers the snapshot-first attach message BEFORE anything
/// from the broadcast rings — a watcher always learns current state, then
/// gets the live event flow, in that order. This ordering (plus the actor
/// computing the snapshot atomically with the subscription) is the whole
/// gap-free reconnect guarantee: no sequence numbers, no PG replay, no
/// dedup.
#[tokio::test]
async fn test_bridge_sends_snapshot_first() {
    use rio_proto::types::build_event::Event;
    let build_id = Uuid::new_v4();

    // The state ring already has a queued live event at attach time
    // (sent after subscribe but before the bridge task started). The
    // snapshot must still be delivered ahead of it.
    let (state_tx, state_rx) = broadcast::channel(16);
    let _ = state_tx.send(mk_event(build_id, 1));

    let mut stream = bridge_build_events(
        "test-snapshot-first",
        state_only(state_rx),
        Some(Box::new(mk_snapshot(build_id))),
    );

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("first message within 2s")
        .expect("stream open")
        .expect("Ok event");
    assert!(
        matches!(first.event, Some(Event::Snapshot(_))),
        "snapshot is the stream's first message, got {:?}",
        first.event
    );

    let second = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("second message within 2s")
        .expect("stream open")
        .expect("Ok event");
    assert!(
        matches!(second.event, Some(Event::Cancelled(_))),
        "queued broadcast event follows the snapshot, got {:?}",
        second.event
    );
}

/// SubmitBuild's bridge passes `first: None` — the stream carries only
/// broadcast events, with nothing prepended.
#[tokio::test]
async fn test_bridge_no_first_message_passthrough() {
    let build_id = Uuid::new_v4();
    let (state_tx, state_rx) = broadcast::channel(16);
    let _ = state_tx.send(mk_event(build_id, 1));

    let mut stream = bridge_build_events("test-no-first", state_only(state_rx), None);

    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("first message within 2s")
        .expect("stream open")
        .expect("Ok event");
    assert!(
        matches!(
            first.event,
            Some(rio_proto::types::build_event::Event::Cancelled(_))
        ),
        "no snapshot prepended on the SubmitBuild path, got {:?}",
        first.event
    );
}

/// Helper: next stream item, unwrapped.
async fn next_ev(
    stream: &mut tokio_stream::wrappers::ReceiverStream<
        Result<rio_proto::types::BuildEvent, tonic::Status>,
    >,
) -> rio_proto::types::BuildEvent {
    tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("stream should not hang")
        .expect("stream should yield")
        .expect("Ok event")
}

fn is_resync(ev: &rio_proto::types::BuildEvent) -> bool {
    matches!(
        ev.event,
        Some(rio_proto::types::build_event::Event::ResyncRequired(_))
    )
}

// r[verify gw.resync.loss-signal]
/// A state-channel Lagged streak emits exactly ONE in-stream
/// `ResyncRequired` before the post-lag events; a successful forward
/// re-arms the signal so the NEXT streak emits exactly one more.
#[tokio::test]
async fn test_bridge_lagged_emits_one_resync_per_streak() {
    let build_id = Uuid::new_v4();
    // Capacity 1 + send 3 → the receiver (subscribed at creation) lags
    // by 2 on its first recv.
    let (tx, rx) = broadcast::channel(1);
    for i in 1..=3u64 {
        let _ = tx.send(mk_event(build_id, i));
    }

    let mut stream = bridge_build_events("test-bridge-resync", state_only(rx), None);

    // Streak 1: ResyncRequired FIRST, then the oldest in-ring event.
    let first = next_ev(&mut stream).await;
    assert!(
        is_resync(&first),
        "the Lagged streak must surface an in-stream ResyncRequired before \
         post-lag events (got {:?})",
        first.event
    );
    let ev = next_ev(&mut stream).await;
    assert_eq!(event_tag(&ev), "seq-3", "post-lag event follows the signal");

    // A successful forward re-arms the signal.
    let _ = tx.send(mk_event(build_id, 4));
    let ev = next_ev(&mut stream).await;
    assert_eq!(
        event_tag(&ev),
        "seq-4",
        "no spurious resync between streaks"
    );

    // Streak 2: flood again → exactly one more ResyncRequired.
    let _ = tx.send(mk_event(build_id, 5));
    let _ = tx.send(mk_event(build_id, 6));
    let ev = next_ev(&mut stream).await;
    assert!(
        is_resync(&ev),
        "a NEW Lagged streak after a successful forward must re-emit the \
         signal (got {:?})",
        ev.event
    );
    let ev = next_ev(&mut stream).await;
    assert_eq!(event_tag(&ev), "seq-6");
}
