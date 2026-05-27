//! `builder_nodes` registry — §P0590 node-lineage audit (M_069).
//!
//! Scheduler-written from controller acknowledgements; **never read at
//! mint or verify time** (ADR-022 mount-admission credentials,
//! decision 3). All writes are best-effort: callers log-and-continue,
//! and a missed upsert only delays `first_seen`/`last_seen` by one
//! controller ack (~10 s).

use super::SchedulerDb;

impl SchedulerDb {
    /// Upsert the nodes named in a controller ack: insert with
    /// `first_seen = last_seen = now()`, or refresh `last_seen` (and
    /// clear `retired_at` — a name that reappears is no longer
    /// retired). `node_names` should be deduplicated by the caller; a
    /// duplicate only costs a redundant conflict-update.
    pub async fn upsert_builder_nodes(&self, node_names: &[String]) -> Result<(), sqlx::Error> {
        if node_names.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"
            INSERT INTO builder_nodes (node_name)
            SELECT DISTINCT unnest($1::text[])
            ON CONFLICT (node_name)
            DO UPDATE SET last_seen = now(), retired_at = NULL
            "#,
        )
        .bind(node_names)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stamp `retired_at` on the named nodes (the dead-node sweep's
    /// hung-node detections). Already-retired rows keep their original
    /// `retired_at`.
    pub async fn retire_builder_nodes(&self, node_names: &[String]) -> Result<(), sqlx::Error> {
        if node_names.is_empty() {
            return Ok(());
        }
        sqlx::query(
            r#"
            UPDATE builder_nodes
            SET retired_at = now()
            WHERE node_name = ANY($1) AND retired_at IS NULL
            "#,
        )
        .bind(node_names)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Stamp `retired_at` on nodes whose `last_seen` is older than
    /// `ttl_secs` — the "no builder pod has been bound here for a long
    /// time" complement to the hung-node sweep (a drained/scaled-down
    /// node never trips hung-node detection because it simply stops
    /// appearing in acks). Returns the number of rows newly retired.
    pub async fn retire_stale_builder_nodes(&self, ttl_secs: i64) -> Result<u64, sqlx::Error> {
        let res = sqlx::query(
            r#"
            UPDATE builder_nodes
            SET retired_at = now()
            WHERE retired_at IS NULL
              AND last_seen < now() - make_interval(secs => $1::double precision)
            "#,
        )
        .bind(ttl_secs as f64)
        .execute(&self.pool)
        .await?;
        Ok(res.rows_affected())
    }
}
