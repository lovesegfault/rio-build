//! Execution-lifecycle CRUD — `drv_executions` table.
//!
//! One row per execution attempt (UUIDv7 `exec_id`), created at
//! dispatch and stamped once at terminal. This is the log subsystem's
//! per-execution anchor: rio-store's latest-exec resolution
//! (`ORDER BY exec_id DESC`) and completeness predicate
//! (`status` ∈ terminal ∧ `final_line_count` covered by the chunk
//! manifest) read it. It deliberately duplicates `exec_id` /
//! `builder_id` / timestamps that also live on `assignments` —
//! `assignments` keeps one row per attempt with its own audit
//! semantics and a *different* status vocabulary (see
//! `rio_migrations::schema::EXEC_STATUS_SUCCEEDED`).
//!
//! The terminal UPDATE does not live here: `terminal_log_epilogue` is
//! a sync chokepoint that fires the write through `spawn_monitored`
//! (the `record_exec_correlation` pattern), so the SQL sits next to
//! that call in `actor/event.rs`.

use uuid::Uuid;

use super::SchedulerDb;
use crate::state::ExecutorId;

impl SchedulerDb {
    /// Create the lifecycle row for a freshly-minted execution.
    ///
    /// `drv_hash` is the `drv_log_hash()` 32-char form (the same value
    /// the `logs/{drv_hash}/…` S3 chunk keys use) —
    /// NOT the `derivations.drv_hash` DAG key. rio-store normalizes a
    /// reader's derivation argument through the same helper before
    /// querying this column.
    ///
    /// `ON CONFLICT DO NOTHING`: a daemon-transient retry keeps the
    /// same `exec_id` and re-runs the dispatch path — the second
    /// INSERT is a no-op and the original `started_at` is preserved.
    /// A scheduler re-dispatch mints a new `exec_id` → a new row.
    pub async fn insert_drv_execution(
        &self,
        exec_id: Uuid,
        drv_hash: &str,
        executor_id: &ExecutorId,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO drv_executions (exec_id, drv_hash, executor_id, started_at) \
             VALUES ($1, $2, $3, now()) \
             ON CONFLICT (exec_id) DO NOTHING",
        )
        .bind(exec_id)
        .bind(drv_hash)
        .bind(executor_id.as_str())
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Delete the lifecycle row for an execution whose `WorkAssignment`
    /// never reached the worker (`try_send` failed → the caller is
    /// rolling the whole dispatch back). The inverse of
    /// [`Self::insert_drv_execution`], called from
    /// `rollback_assignment` alongside `delete_latest_assignment` for
    /// the same reason: a row for an execution that never ran is
    /// misleading — it would sit at `status IS NULL` ("still running")
    /// until the 30-day TTL sweep.
    ///
    /// Guarded on `status IS NULL` so a rollback racing a terminal
    /// stamp (impossible today — the actor is single-threaded and the
    /// worker never saw the assignment — but cheap to keep monotone)
    /// cannot delete a finished execution's row.
    pub async fn delete_drv_execution(&self, exec_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM drv_executions WHERE exec_id = $1 AND status IS NULL")
            .bind(exec_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }
}
