//! The ledger-backed open-attempt view (pull-mode dispatch).
//!
//! An **open attempt** is an active `assignments` row (status ∈
//! {pending, acknowledged}) joined to its `drv_executions` row via
//! `exec_id` — both written by the pull transaction exactly as the
//! as-built assign path writes them — with terminality decided by the
//! `drv_attempts` fill (a row whose `termination_reason` is filled is
//! closed even if the assignment-close write has not landed yet).
//!
//! The unrestricted join would also match in-flight *stream*-mode
//! builds (the as-built dispatch path writes the same row pair), so
//! every pull-only consumer — the establishment sweep, the
//! `ListOpenAttempts` RPC, the open-attempts gauge, the controller's
//! synthesize-on-delete arm — reads through this module's
//! `dispatch_mode = 'pull'` filter. `source_node IS NOT NULL` is NOT
//! the discriminator (it is only populated when known).

use uuid::Uuid;

use super::SchedulerDb;

/// One open pull-mode attempt as read back from the view query.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct OpenAttemptRow {
    /// The DAG key (`derivations.derivation_id`).
    // Consumed by the establishment sweep's fill keying (lands later
    // in this wave); the RPC handler does not need it. allow (not
    // expect): the in-crate db tests already read it, so the lint only
    // fires on the non-test lib target.
    #[allow(dead_code)]
    pub derivation_id: Uuid,
    /// `derivations.drv_hash` — the intent id (the spawn-intent key).
    pub drv_hash: String,
    /// Full `.drv` store path.
    pub drv_path: String,
    /// Per-execution identifier minted by the pull transaction.
    pub exec_id: Uuid,
    /// Executor identity the attempt is bound to (`assignments.builder_id`).
    pub executor_id: String,
    /// Controller-authoritative node binding, when known (071).
    pub source_node: Option<String>,
    /// Lease generation the assignment row carries.
    pub generation: i64,
    /// `assignments.assigned_at` as epoch seconds (PG clock).
    // Consumed by the establishment sweep's window arithmetic (lands
    // later in this wave); same allow rationale as derivation_id.
    #[allow(dead_code)]
    pub assigned_at_epoch_secs: f64,
    /// Age of the assignment row in seconds (PG clock, non-negative).
    pub age_secs: f64,
    /// `drv_executions.dispatch_mode` — always `'pull'` for rows
    /// returned by [`SchedulerDb::list_open_pull_attempts`]; carried so
    /// the view-contract tests can assert the filter, not consulted by
    /// lib readers (the WHERE clause is the discriminator).
    #[allow(dead_code)]
    pub dispatch_mode: String,
}

impl SchedulerDb {
    /// Every open **pull-mode** attempt: active assignment ⋈ execution
    /// (`dispatch_mode = 'pull'`) with no terminal `drv_attempts` fill.
    ///
    /// Stream-mode rows (the `'stream'` default) are excluded by the
    /// `dispatch_mode` filter, never by executor-id heuristics, so the
    /// pull-only consumers cannot visit rows the as-built dispatch
    /// path wrote.
    pub(crate) async fn list_open_pull_attempts(&self) -> Result<Vec<OpenAttemptRow>, sqlx::Error> {
        let rows: Vec<OpenAttemptRow> = sqlx::query_as(
            "SELECT d.derivation_id, d.drv_hash, d.drv_path, \
                    a.exec_id, a.builder_id AS executor_id, a.generation, \
                    e.source_node, e.dispatch_mode, \
                    EXTRACT(EPOCH FROM a.assigned_at)::float8 AS assigned_at_epoch_secs, \
                    GREATEST(EXTRACT(EPOCH FROM (now() - a.assigned_at))::float8, 0.0) \
                        AS age_secs \
             FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             JOIN drv_executions e ON e.exec_id = a.exec_id \
             WHERE a.status IN ('pending', 'acknowledged') \
               AND e.dispatch_mode = 'pull' \
               AND NOT EXISTS ( \
                   SELECT 1 FROM drv_attempts t \
                   WHERE t.exec_id = a.exec_id \
                     AND t.termination_reason IS NOT NULL \
               ) \
             ORDER BY a.assigned_at, a.exec_id",
        )
        .fetch_all(&self.pool)
        .await?;
        Ok(rows)
    }
}
