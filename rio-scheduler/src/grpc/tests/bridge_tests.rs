//! `bridge_build_events` since-sequence replay + BuildEvent bridge tests.
//!
//! Split from the 1682L monolithic `grpc/tests.rs` (P0395). Covers the
//! `bridge_build_events` function directly: broadcast-lag DATA_LOSS,
//! PG-backed replay + dedup watermark, post-subscribe dedup, PG-failure
//! fallthrough, half-open range query, and UUID-v7 build_id ordering.
//! These tests drive the bridge with a bare `broadcast::channel` rather
//! than the full actor so PG and ring-buffer state are precisely
//! controlled.

use super::*;
use std::time::Duration;
use tokio_stream::StreamExt;

/// When a broadcast receiver lags (permanently misses events), the bridge
/// sends DATA_LOSS and stops. Without this, a missed BuildCompleted would
/// leave the client hanging forever.
#[tokio::test]
async fn test_bridge_build_events_lagged_sends_data_loss() {
    // Capacity 1 + send 3 before receiver subscribes → lag guaranteed.
    let (tx, _keepalive_rx) = broadcast::channel(1);
    let rx = tx.subscribe();
    // Fill the channel past capacity so rx is lagged.
    for i in 0..3u64 {
        let _ = tx.send(rio_proto::types::BuildEvent {
            build_id: format!("build-{i}"),
            sequence: i,
            timestamp: None,
            event: None,
        });
    }

    let mut stream = bridge_build_events("test-bridge", rx, None);
    // First poll: the bridge task's first recv() hits Lagged.
    let first = stream.next().await.expect("should yield one item");
    let status = first.expect_err("should be DATA_LOSS");
    assert_eq!(status.code(), tonic::Code::DataLoss);
    assert!(
        status.message().contains("missed"),
        "got: {}",
        status.message()
    );
    // Stream should then end (bridge task broke out of the loop).
    assert!(stream.next().await.is_none());
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
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _actor_task) = setup_actor(db.pool.clone());
    let grpc = SchedulerGrpc::new_for_tests(handle);

    let mk_req = |tag: &str| rio_proto::build_types::SubmitBuildRequest {
        tenant_name: String::new(),
        priority_class: String::new(),
        nodes: vec![make_test_node(tag, "x86_64-linux")],
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
        bcast_rx,
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
        bcast_rx,
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
        bcast_rx,
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

    let rows = crate::db::read_event_log(&db.pool, build_id, 2, 4).await?;
    let seqs: Vec<u64> = rows.iter().map(|(s, _)| *s).collect();
    assert_eq!(
        seqs,
        vec![3, 4],
        "(since, until] — excludes since, includes until"
    );
    Ok(())
}
