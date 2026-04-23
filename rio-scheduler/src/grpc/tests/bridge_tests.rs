//! `bridge_build_events` since-sequence replay + BuildEvent bridge tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395). Covers the
//! `bridge_build_events` function directly: broadcast-lag continue (I-144),
//! PG-backed replay + dedup watermark, post-subscribe dedup, PG-failure
//! fallthrough, half-open range query, and UUID-v7 build_id ordering.
//! These tests drive the bridge with a bare `broadcast::channel` rather
//! than the full actor so PG and ring-buffer state are precisely
//! controlled.

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
    // Capacity 1 + send 3 → rx (subscribed at channel creation) lags by 2.
    let (tx, rx) = broadcast::channel(1);
    for i in 1..=3u64 {
        let _ = tx.send(rio_proto::types::BuildEvent {
            build_id: "b".into(),
            sequence: i,
            timestamp: None,
            event: None,
        });
    }

    let mut stream = bridge_build_events("test-bridge", state_only(rx), None);

    // First poll: bridge's first recv() hits Lagged(2), continues, next
    // recv() returns seq=3 (oldest still in the cap-1 ring). NOT an Err.
    let first = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("bridge should not hang post-lag")
        .expect("stream should yield, not end");
    let ev = first.expect("post-lag event must be Ok, not DATA_LOSS");
    assert_eq!(
        ev.sequence, 3,
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
    let _ = tx.send(rio_proto::types::BuildEvent {
        build_id: "b".into(),
        sequence: 4,
        timestamp: None,
        event: None,
    });
    let next = tokio::time::timeout(Duration::from_secs(2), stream.next())
        .await
        .expect("post-lag stream should keep yielding")
        .expect("stream open")
        .expect("Ok event");
    assert_eq!(next.sequence, 4);
    assert_eq!(tx.receiver_count(), 1, "still subscribed after second send");
}

/// State events MUST survive log-channel flooding. Before the
/// state/log channel split, `Event::Log` and `DerivationEvent` shared
/// one `broadcast(4096)` ring; chatty parallel builds (chromium /
/// firefox / rustc) flooded it, the bridge's `Lagged` skip-and-continue
/// silently dropped `DerivationEvent::Completed`, and the gateway never
/// emitted `stop_activity` — repro JSON had 44 `start` / 34 `stop`.
///
/// Asserts: emitting >> ring-capacity Log events on the log channel
/// does NOT prevent a single `Derivation::Completed` on the state
/// channel from reaching the bridge output.
// r[verify gw.activity.stop-parity]
#[tokio::test]
async fn test_completed_event_survives_log_flood() {
    use rio_proto::types::build_event::Event;
    let build_id = Uuid::new_v4();

    // Log ring sized at the production LOG_EVENT_BUFFER_SIZE so the
    // flood actually lags it. State ring sized at 16 — irrelevant,
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

    // Flood the log channel well past its capacity so the log receiver
    // is guaranteed Lagged. This is what emit() routes Event::Log to.
    for _ in 0..6000 {
        let _ = log_tx.send(mk_log_event(build_id, 0));
    }
    // The state event under test: a per-derivation Completed.
    let _ = state_tx.send(rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        sequence: 1,
        timestamp: None,
        event: Some(Event::Derivation(rio_proto::types::DerivationEvent {
            derivation_path: "/nix/store/x.drv".into(),
            kind: rio_proto::types::DerivationEventKind::Completed as i32,
            output_paths: vec![],
            executor_id: String::new(),
            error_message: String::new(),
            failure_status: 0,
        })),
    });

    // Drain until we see the Completed. A 2s budget at 6001 events is
    // ample (in-process). Some Log events were evicted by Lagged —
    // count how many reached the bridge to assert the flood actually
    // overflowed the log ring (otherwise the test isn't proving the
    // split, just that 6000 < capacity).
    let mut log_seen = 0usize;
    let deadline = tokio::time::Instant::now() + Duration::from_secs(2);
    let saw_completed = loop {
        let Ok(Some(ev)) = tokio::time::timeout_at(deadline, stream.next()).await else {
            break false;
        };
        match ev.expect("Ok event").event {
            Some(Event::Log(_)) => log_seen += 1,
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
        "DerivationEvent::Completed must reach the bridge despite log flood"
    );
    assert!(
        log_seen < 6000,
        "log channel should have lagged (saw {log_seen}/6000); \
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
// since_sequence replay (PG event log + subscribe-first dedup)
// ===========================================================================

/// Minimal BuildEvent for replay tests. Prost-encoded (same as
/// emit_build_event does via encode_to_vec).
fn mk_event(build_id: Uuid, seq: u64) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        sequence: seq,
        timestamp: None,
        event: Some(Event::Cancelled(rio_proto::types::BuildCancelled {
            reason: format!("seq-{seq}"),
        })),
    }
}

/// `Event::Log` at the given seq. Log reuses the last persisted seq
/// (event.rs) — the dedup must NOT drop it (it's never in PG).
fn mk_log_event(build_id: Uuid, seq: u64) -> rio_proto::types::BuildEvent {
    use rio_proto::types::build_event::Event;
    rio_proto::types::BuildEvent {
        build_id: build_id.to_string(),
        sequence: seq,
        timestamp: None,
        event: Some(Event::Log(rio_proto::types::BuildLogBatch {
            derivation_path: "/nix/store/x".into(),
            lines: vec![b"log-line".to_vec()],
            first_line_number: 0,
            executor_id: String::new(),
        })),
    }
}

/// Insert one event into PG directly (bypassing the persister).
/// Tests control exact PG state to assert replay behavior.
async fn insert_event(pool: &sqlx::PgPool, build_id: Uuid, seq: u64) -> anyhow::Result<()> {
    use prost::Message;
    sqlx::query(
        "INSERT INTO build_event_log (build_id, sequence, event_bytes) VALUES ($1, $2, $3)",
    )
    .bind(build_id)
    .bind(seq as i64)
    .bind(mk_event(build_id, seq).encode_to_vec())
    .execute(pool)
    .await?;
    Ok(())
}

/// Drain N events from the bridge with a timeout. Collects just
/// sequences — that's what we assert on (order + gaps + dedup).
async fn collect_seqs(
    stream: &mut ReceiverStream<Result<rio_proto::types::BuildEvent, Status>>,
    n: usize,
) -> anyhow::Result<Vec<u64>> {
    let mut seqs = Vec::with_capacity(n);
    for _ in 0..n {
        let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream ended early"))??;
        seqs.push(ev.sequence);
    }
    Ok(seqs)
}

/// Core property: gateway reconnects with since_sequence=2 after
/// the actor has emitted seq 1..5. PG has all 5 (persister ran).
/// Broadcast ring ALSO has all 5 (cap 1024). Without dedup, the
/// gateway sees 3,4,5 from PG then 1..5 again from broadcast.
/// With dedup, exactly 3,4,5 once.
///
/// Test uses a bare broadcast channel (not the full actor) to
/// control exactly what's in the ring vs PG. The real subscribe-
/// first ordering is tested separately (test_bridge_no_gap_on_race).
#[tokio::test]
async fn test_bridge_replays_from_pg_and_dedups_broadcast() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: seq 1..5 persisted (simulates what emit_build_event did).
    for seq in 1..=5 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    // Broadcast: same 5 events still in the ring (1024 cap, they
    // haven't been pushed out). This is the DUPLICATE the dedup
    // protects against.
    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    for seq in 1..=5 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    // Gateway reconnects: saw up to seq=2 before disconnect.
    // last_seq=5 (actor's watermark at subscribe time).
    let mut stream = bridge_build_events(
        "test-replay",
        state_only(bcast_rx),
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 2,
            last_seq: 5,
        }),
    );

    // Expect exactly 3,4,5 — from PG, in order. Broadcast's 1..5
    // all skipped (seq ≤ last_seq=5).
    let seqs = collect_seqs(&mut stream, 3).await?;
    assert_eq!(seqs, vec![3, 4, 5], "PG replay fills (since, last_seq]");

    // And NO MORE. Post-subscribe events (seq > 5) would come next,
    // but we sent none. A 4th event = dedup failed (broadcast leak).
    let extra = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    assert!(
        extra.is_err(),
        "no 4th event — broadcast's 1..5 all deduped. Got: {extra:?}"
    );

    Ok(())
}

/// bug_125: `Event::Log` reuses the last persisted seq (event.rs Log
/// arm) and is never written to PG. After a reconnect-with-replay,
/// `dedup_watermark = last_seq` and a fresh Log arrives at `seq =
/// last_seq` — the dedup MUST NOT drop it. Log now arrives on a
/// separate channel that the bridge never dedups, so this is
/// structural; the test pins that the split is wired and a Log at the
/// watermark seq still reaches the client.
#[tokio::test]
async fn test_bridge_log_event_at_watermark_seq_not_deduped() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: seq 1..3 persisted (DerivationStarted/Completed/etc.).
    for seq in 1..=3 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    // State ring: the same 3 persisted events (still in buffer) — all
    // Phase-1 duplicates, all deduped.
    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    for seq in 1..=3 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }
    // Log ring: a Log at seq=3 (emit() reuses the last persisted seq).
    let (log_tx, log_rx) = broadcast::channel(16);
    log_tx.send(mk_log_event(build_id, 3))?;

    // Gateway reconnects: saw nothing (since=0), actor's watermark is 3.
    let mut stream = bridge_build_events(
        "test-log-dedup",
        BuildEventReceivers {
            state: bcast_rx,
            log: log_rx,
        },
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 3,
        }),
    );

    // Phase 1: PG replay yields seq 1,2,3. Phase 2: state-ring 1..3 all
    // deduped (seq ≤ 3); Log@3 from log-ring passes (no dedup applied
    // to log channel). 4 events total.
    let mut events = Vec::new();
    for _ in 0..4 {
        let ev = tokio::time::timeout(Duration::from_secs(2), stream.next())
            .await?
            .ok_or_else(|| anyhow::anyhow!("stream ended early"))??;
        events.push(ev);
    }
    assert_eq!(
        events.iter().map(|e| e.sequence).collect::<Vec<_>>(),
        vec![1, 2, 3, 3],
        "PG replay 1..3 then Log@3 from log channel"
    );
    use rio_proto::types::build_event::Event;
    assert!(
        matches!(events[3].event, Some(Event::Log(_))),
        "4th event is the Log (not a deduped duplicate of the persisted seq=3)"
    );
    // The 3 persisted state-ring duplicates were skipped — no 5th event.
    let extra = tokio::time::timeout(Duration::from_millis(100), stream.next()).await;
    assert!(
        extra.is_err(),
        "persisted-event state-ring duplicates still deduped: {extra:?}"
    );

    Ok(())
}

/// Post-subscribe events (seq > last_seq) flow through normally.
/// This is what the gateway sees AFTER the replay catches up.
#[tokio::test]
async fn test_bridge_post_subscribe_events_pass_dedup() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: seq 1,2 (pre-subscribe history).
    for seq in 1..=2 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    // Broadcast ring: the same 1,2 (still in buffer) PLUS 3,4
    // which arrived AFTER subscribe (seq > last_seq=2).
    for seq in 1..=4 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    let mut stream = bridge_build_events(
        "test-post-sub",
        state_only(bcast_rx),
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 2,
        }),
    );

    // PG replay: 1,2. Then broadcast: 1,2 skipped (≤2), 3,4 pass.
    let seqs = collect_seqs(&mut stream, 4).await?;
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4],
        "replay then live: PG gives 1,2; broadcast dedups 1,2, passes 3,4"
    );
    Ok(())
}

/// PG down → replay fails → fall through to broadcast WITHOUT dedup.
/// A double is better than a hole — if we deduped without having
/// actually delivered from PG, the gateway would miss events.
#[tokio::test]
async fn test_bridge_pg_failure_falls_through_no_dedup() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();
    // Close the pool → read_event_log fails immediately.
    // TestDb::Drop uses a fresh admin connection so this is safe.
    db.pool.close().await;

    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    // Broadcast has 1,2,3. With dedup (last_seq=3) they'd ALL be
    // skipped. Without dedup (PG failed) they all pass — the
    // gateway gets SOMETHING instead of silence.
    for seq in 1..=3 {
        bcast_tx.send(mk_event(build_id, seq))?;
    }

    let mut stream = bridge_build_events(
        "test-pg-fail",
        state_only(bcast_rx),
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 3,
        }),
    );

    // PG failed → dedup_watermark stays 0 → all 3 pass.
    let seqs = collect_seqs(&mut stream, 3).await?;
    assert_eq!(
        seqs,
        vec![1, 2, 3],
        "PG failure → no dedup → broadcast delivers (safety net)"
    );
    Ok(())
}

/// bug_372: dedup watermark must equal "highest seq PG actually
/// returned", not `r.last_seq`. When the persister dropped the
/// terminal under backpressure, PG returns Ok with rows missing at
/// the tail; `handle_watch_build`'s safety-net resend at
/// `seq=last_seq` would otherwise be suppressed by `N <= N` and the
/// gateway loops `EofWithoutTerminal` forever.
#[tokio::test]
async fn test_bridge_watermark_tracks_pg_max_not_last_seq() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // PG: only seq 1..5 persisted. Persister dropped 6..10 under
    // backpressure (simulated by not inserting them).
    for seq in 1..=5 {
        insert_event(&db.pool, build_id, seq).await?;
    }

    // Broadcast: handle_watch_build's terminal resend at seq=10
    // (= last_seq). This is the ONLY copy of the terminal.
    let (bcast_tx, bcast_rx) = broadcast::channel(16);
    bcast_tx.send(mk_event(build_id, 10))?;

    // Gateway reconnects with since=0; actor's last_seq=10.
    let mut stream = bridge_build_events(
        "test-watermark",
        state_only(bcast_rx),
        Some(EventReplay {
            pool: db.pool.clone(),
            build_id,
            since: 0,
            last_seq: 10,
        }),
    );

    // Expect 1..5 from PG, then 10 from broadcast (NOT deduped:
    // watermark = max-seen = 5, and 10 > 5). At b62291b8: watermark
    // would be 10, broadcast's seq=10 deduped → only 1..5.
    let seqs = collect_seqs(&mut stream, 6).await?;
    assert_eq!(
        seqs,
        vec![1, 2, 3, 4, 5, 10],
        "PG-dropped tail must NOT suppress broadcast resend at last_seq"
    );
    Ok(())
}

/// bug_375 structural assertion: `read_event_log` returns a stream
/// (compile-time-checked by the `pin!` + `next().await` consumption
/// pattern in `bridge_build_events`); rows arrive incrementally so
/// the mpsc(256) backpressure can apply.
#[tokio::test]
async fn test_read_event_log_streams_rows() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();
    for seq in 1..=200 {
        insert_event(&db.pool, build_id, seq).await?;
    }
    // Consume the first row WITHOUT collecting the rest. If the
    // function `.fetch_all()`'d internally this would still work,
    // but the type signature (`impl Stream`) is the structural
    // guarantee — the row-by-row forward in `bridge_build_events`
    // is what propagates backpressure to the PG cursor.
    let mut s = std::pin::pin!(crate::db::read_event_log(&db.pool, build_id, 0, 200));
    let (first_seq, _) = s.next().await.expect("at least one row")?;
    assert_eq!(first_seq, 1);
    Ok(())
}

/// `read_event_log` range is half-open `(since, until]`. Boundary
/// check: since=2, until=4 → returns 3,4 (not 2, not 5).
#[tokio::test]
async fn test_read_event_log_half_open_range() -> anyhow::Result<()> {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();
    for seq in 1..=5 {
        insert_event(&db.pool, build_id, seq).await?;
    }
    // Noise: another build, same seq range. Scoping check.
    for seq in 1..=5 {
        insert_event(&db.pool, Uuid::new_v4(), seq).await?;
    }

    let rows: Vec<_> = crate::db::read_event_log(&db.pool, build_id, 2, 4)
        .collect::<Vec<_>>()
        .await
        .into_iter()
        .collect::<Result<_, _>>()?;
    let seqs: Vec<u64> = rows.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        vec![3, 4],
        "(since, until] — excludes since, includes until"
    );
    Ok(())
}
