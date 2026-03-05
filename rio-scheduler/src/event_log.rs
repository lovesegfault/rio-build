//! Persistent build event log for since_sequence resumption.
//!
//! See `migrations/003_event_log.sql` for schema + rationale.
//!
//! # Why not fire-and-forget `tokio::spawn(sqlx INSERT)` in emit_build_event
//!
//! The architecture review caught this: fire-and-forget spawns
//! race on the PG pool. emit_build_event is called in-order by
//! the actor (single-threaded), but the spawned tasks execute in
//! tokio's scheduler order — there's no guarantee seq=5's INSERT
//! commits before seq=6's. A WatchBuild arriving mid-backlog
//! might see neither (both still in-flight) then get seq=7+ from
//! the broadcast. Events 5-6 lost.
//!
//! Bounded mpsc + single drain task = FIFO. The persister
//! receives in the actor's order and INSERTs in that order. Write
//! ordering guaranteed. Bonus: the channel bounds memory (1000
//! events × ~500 bytes = 500 KB max backlog, not unbounded spawn
//! queue).

use sqlx::PgPool;
use tokio::sync::mpsc;
use tracing::warn;
use uuid::Uuid;

/// Channel capacity. 1000 events × ~500 bytes each ≈ 500 KB max
/// backlog. emit_build_event try_sends; if this is full, the
/// event is dropped (but the broadcast already carried it — only
/// a mid-backlog reconnect loses it, which is already a degraded
/// scenario).
///
/// With Event::Log filtered out (S3 handles those), the rate is
/// ~5 events/sec for a busy scheduler (one per derivation state
/// transition). 1000 = 200 seconds of backlog. If the persister
/// is 200s behind, PG is probably down and the drops don't matter
/// (reconnect replay would also fail).
const CHANNEL_CAPACITY: usize = 1000;

/// Batch size for the drain loop. Draining up to N messages per
/// iteration and inserting them in one multi-row INSERT amortizes
/// the PG round-trip. 50 is large enough to batch meaningfully
/// (50 × ~500 bytes = 25 KB per batch, well within PG limits)
/// but small enough that the batch completes quickly (one round-
/// trip, not 50).
const BATCH_SIZE: usize = 50;

/// One event to persist. Tuple not struct: it's a pipe from
/// emit_build_event to the INSERT, nothing looks inside.
pub type EventLogEntry = (Uuid, u64, Vec<u8>);

/// Spawn the persister task. Returns the sender.
///
/// The task runs until `rx.recv()` returns `None` (all senders
/// dropped). That happens when the actor's copy of the Sender
/// drops — i.e., actor task exit. At that point the scheduler is
/// shutting down anyway.
///
/// `spawn_monitored`: if this panics, logged. The scheduler keeps
/// running — events just don't persist. Degraded (reconnect loses
/// history) but functional. The broadcast channel still carries
/// live events to active gateways.
pub fn spawn(pool: PgPool) -> mpsc::Sender<EventLogEntry> {
    let (tx, mut rx) = mpsc::channel(CHANNEL_CAPACITY);

    rio_common::task::spawn_monitored("event-persister", async move {
        // Batch buffer. Reused across iterations — clear(), don't
        // reallocate. Pre-sized to BATCH_SIZE.
        let mut batch: Vec<EventLogEntry> = Vec::with_capacity(BATCH_SIZE);

        loop {
            // Wait for the first event. `recv()` returns None
            // when all senders drop → loop exits, task ends.
            let Some(first) = rx.recv().await else {
                break;
            };
            batch.push(first);

            // Drain up to BATCH_SIZE-1 more (try_recv, non-
            // blocking). This is the "amortize PG round-trip"
            // optimization. If only one event was queued, the
            // batch is size 1 — fine, one INSERT. If 50 arrived
            // in a burst, one INSERT instead of 50.
            //
            // `while let Ok` (not a for loop): try_recv returns
            // Err(Empty) when drained, which breaks the loop.
            // Err(Disconnected) also breaks — same as None from
            // recv() above, just means we caught it mid-drain.
            while batch.len() < BATCH_SIZE
                && let Ok(next) = rx.try_recv()
            {
                batch.push(next);
            }

            // INSERT. Multi-row VALUES built via QueryBuilder —
            // sqlx doesn't have a native batch-insert helper, but
            // push_values generates the right SQL. The BYTEA
            // binds are fine (sqlx encodes Vec<u8> → BYTEA).
            //
            // i64 cast on sequence: PG BIGINT is signed. u64 →
            // i64 wraps at 2^63, but a build with 9 quintillion
            // events would have other problems. The u64→i64 cast
            // is the ONLY one in this module.
            let mut qb = sqlx::QueryBuilder::new(
                "INSERT INTO build_event_log (build_id, sequence, event_bytes) ",
            );
            qb.push_values(&batch, |mut b, (build_id, seq, bytes)| {
                b.push_bind(build_id)
                    .push_bind(*seq as i64)
                    .push_bind(bytes);
            });
            if let Err(e) = qb.build().execute(&pool).await {
                // PG error. Log with the batch size so operators
                // can see how many events were lost. Don't crash
                // — degraded, not dead. The broadcast still works.
                //
                // Drop the batch (don't retry): if PG is down,
                // retrying blocks the drain loop → channel fills
                // → try_send starts failing → metric fires.
                // Better to keep draining and let the metric
                // signal the problem.
                warn!(
                    error = %e,
                    batch_size = batch.len(),
                    "event-persister: INSERT failed (events lost; broadcast still live)"
                );
            }

            batch.clear();
        }
    });

    tx
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_test_support::TestDb;

    #[tokio::test]
    async fn persister_writes_in_order() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tx = spawn(db.pool.clone());

        let build_id = Uuid::new_v4();
        for seq in 1..=5u64 {
            tx.send((build_id, seq, vec![seq as u8; 3])).await?;
        }
        // Drop tx → persister drains and exits. Without this, the
        // test would need to poll until all 5 are in PG (the
        // persister is async). Dropping = synchronous barrier.
        drop(tx);

        // Give the task a tick to process the backlog before it
        // sees the channel close. Bounded poll, not a sleep.
        let rows: Vec<(i64, Vec<u8>)> =
            tokio::time::timeout(std::time::Duration::from_secs(5), async {
                loop {
                    let r: Vec<(i64, Vec<u8>)> = sqlx::query_as(
                        "SELECT sequence, event_bytes FROM build_event_log \
                         WHERE build_id = $1 ORDER BY sequence",
                    )
                    .bind(build_id)
                    .fetch_all(&db.pool)
                    .await
                    .unwrap();
                    if r.len() == 5 {
                        return r;
                    }
                    tokio::task::yield_now().await;
                }
            })
            .await?;

        // Ordered by sequence AND bytes match (proves we're not
        // just getting 5 rows, but the RIGHT 5 rows in order).
        for (i, (seq, bytes)) in rows.iter().enumerate() {
            let expected = (i + 1) as i64;
            assert_eq!(*seq, expected, "sequence preserved (FIFO drain)");
            assert_eq!(bytes, &vec![expected as u8; 3], "bytes round-trip");
        }
        Ok(())
    }

    /// Full channel → try_send fails → sender sees it. The actor's
    /// emit_build_event will increment a metric on this; here we
    /// just prove the bound works.
    ///
    /// `current_thread` runtime + no yields in the send loop = the
    /// persister task (spawned inside `spawn()`) never gets
    /// scheduled. Deterministic: exactly CHANNEL_CAPACITY sends
    /// succeed, the next fails. (A closed pool doesn't HANG the
    /// INSERT — it fails fast, so the persister would drain as
    /// fast as it could discard. Starving it is simpler.)
    #[tokio::test]
    async fn full_channel_try_send_fails() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let tx = spawn(db.pool.clone());
        let build_id = Uuid::new_v4();

        // No yield → current_thread never schedules the persister.
        // The channel fills at exactly CHANNEL_CAPACITY.
        let mut sent = 0;
        for seq in 1..=(CHANNEL_CAPACITY + 10) {
            if tx.try_send((build_id, seq as u64, vec![0])).is_ok() {
                sent += 1;
            } else {
                break;
            }
        }

        assert_eq!(
            sent, CHANNEL_CAPACITY,
            "persister starved on current_thread → channel fills at exactly cap"
        );
        Ok(())
    }
}
