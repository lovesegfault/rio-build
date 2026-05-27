//! `builder_nodes` registry (M_069) — §P0590 node-lineage audit.

use rio_test_support::TestDb;

use crate::db::SchedulerDb;

/// Row shape used by every assertion below.
type Row = (String, Option<String>, bool);

async fn rows(db: &TestDb) -> Vec<Row> {
    sqlx::query_as::<_, Row>(
        "SELECT node_name, retired_at::text, last_seen >= first_seen \
         FROM builder_nodes ORDER BY node_name",
    )
    .fetch_all(&db.pool)
    .await
    .unwrap()
}

/// The full lifecycle the scheduler drives: ack-upsert inserts with
/// `first_seen = last_seen`, the hung sweep retires by name, the TTL
/// sweep retires idle names, and a later ack-upsert refreshes
/// `last_seen` and clears `retired_at` (a reappearing name is no longer
/// retired).
#[tokio::test]
async fn builder_nodes_upsert_retire_and_resurrect() -> anyhow::Result<()> {
    let test_db = TestDb::new(&crate::MIGRATOR).await;
    let db = SchedulerDb::new(test_db.pool.clone());

    // Empty input: no-op, no rows.
    db.upsert_builder_nodes(&[]).await?;
    db.retire_builder_nodes(&[]).await?;
    assert!(rows(&test_db).await.is_empty());

    // First ack: two nodes appear (duplicates in one ack are deduped by
    // the INSERT ... SELECT DISTINCT).
    db.upsert_builder_nodes(&[
        "node-a".to_string(),
        "node-b".to_string(),
        "node-a".to_string(),
    ])
    .await?;
    let after_insert = rows(&test_db).await;
    assert_eq!(after_insert.len(), 2);
    assert!(
        after_insert
            .iter()
            .all(|(_, retired, seen_ok)| retired.is_none() && *seen_ok),
        "fresh rows: retired_at NULL and last_seen >= first_seen: {after_insert:?}"
    );

    // Hung sweep retires node-a; node-b is untouched.
    db.retire_builder_nodes(&["node-a".to_string()]).await?;
    let after_retire = rows(&test_db).await;
    assert!(after_retire[0].1.is_some(), "node-a retired");
    assert!(after_retire[1].1.is_none(), "node-b still live");

    // Re-issuing the retire (the sweep repeats while the node sits in
    // the hung window) keeps the ORIGINAL retired_at stamp.
    let first_stamp = after_retire[0].1.clone();
    db.retire_builder_nodes(&["node-a".to_string()]).await?;
    assert_eq!(
        rows(&test_db).await[0].1,
        first_stamp,
        "repeat retire must not move retired_at"
    );

    // Stale-TTL sweep: with a 0-second TTL everything not yet retired
    // (node-b) goes; node-a is already retired so it is not re-counted.
    let newly_retired = db.retire_stale_builder_nodes(0).await?;
    assert_eq!(newly_retired, 1, "only node-b is newly retired");
    assert!(rows(&test_db).await.iter().all(|(_, r, _)| r.is_some()));
    // A long TTL retires nothing further.
    assert_eq!(db.retire_stale_builder_nodes(24 * 3600).await?, 0);

    // node-a reappears in a later controller ack: last_seen refreshes
    // and retired_at clears; node-b stays retired.
    db.upsert_builder_nodes(&["node-a".to_string()]).await?;
    let after_resurrect = rows(&test_db).await;
    assert!(
        after_resurrect[0].1.is_none(),
        "re-acked node must be un-retired"
    );
    assert!(after_resurrect[1].1.is_some(), "absent node stays retired");

    Ok(())
}
