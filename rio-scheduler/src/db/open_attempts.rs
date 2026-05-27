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

/// One attempt resolved by `exec_id` (the `ReportOutcome` lookup).
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct AttemptByExecRow {
    /// The DAG key (`derivations.derivation_id`).
    #[allow(dead_code)]
    pub derivation_id: Uuid,
    /// `derivations.drv_hash` — the intent id / DAG key string.
    pub drv_hash: String,
    /// Full `.drv` store path (what the completion path keys on).
    pub drv_path: String,
    /// Executor identity the attempt is bound to.
    pub executor_id: String,
    /// The assignment row is still active (pending/acknowledged).
    pub assignment_active: bool,
    /// Some `drv_attempts` row already exists for this exec
    /// (a worker-reported classification).
    pub attempt_recorded: bool,
    /// A terminal `drv_attempts` fill exists for this exec
    /// (establishment or the controller's second installment).
    pub attempt_terminal: bool,
}

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
    /// The fenced pull-mint transaction (the durable half of
    /// `PullAssignment`'s Deliver arm): write/refresh the active
    /// `assignments` row and insert the `drv_executions` row
    /// (`dispatch_mode = 'pull'`, `source_node` when known) in ONE
    /// transaction that commits only if `serving_generation` is not
    /// below the durable claims floor (GREATEST over
    /// `leader_generation_claims` and `assignments` — the same arms
    /// `max_known_generation` reads). Returns `Ok(true)` when the
    /// transaction committed and `Ok(false)` when the fence aborted it
    /// (nothing written).
    // r[impl sched.executor.pull-transaction]
    pub(crate) async fn mint_pull_attempt_fenced(
        &self,
        derivation_id: Uuid,
        executor_id: &crate::state::ExecutorId,
        serving_generation: i64,
        exec_id: Uuid,
        log_hash: &str,
        source_node: Option<&str>,
    ) -> Result<bool, sqlx::Error> {
        let mut tx = self.pool.begin().await?;
        // The floor read runs on the transaction's connection so the
        // commit happens at-or-after any claim row visible here; a
        // below-floor serving generation aborts with no writes.
        let floor: Option<i64> = sqlx::query_scalar(
            "SELECT GREATEST( \
                 (SELECT MAX(generation) FROM assignments), \
                 (SELECT MAX(generation) FROM leader_generation_claims))",
        )
        .fetch_one(&mut *tx)
        .await?;
        if floor.is_some_and(|f| serving_generation < f) {
            tx.rollback().await?;
            return Ok(false);
        }
        // Same active-row upsert discipline as the stream path's
        // insert_assignment (assignments_active_uq is the arbiter).
        sqlx::query(
            "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
             VALUES ($1, $2, $3, 'pending', $4) \
             ON CONFLICT (derivation_id) WHERE status IN ('pending', 'acknowledged') \
             DO UPDATE SET \
                 builder_id = EXCLUDED.builder_id, \
                 generation = EXCLUDED.generation, \
                 status = 'pending', \
                 assigned_at = now(), \
                 completed_at = NULL, \
                 exec_id = EXCLUDED.exec_id",
        )
        .bind(derivation_id)
        .bind(executor_id.as_str())
        .bind(serving_generation)
        .bind(exec_id)
        .execute(&mut *tx)
        .await?;
        // The execution lifecycle row carries the pull discriminator
        // and the controller-authoritative source attribution (071).
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, dispatch_mode, source_node) \
             VALUES ($1, $2, $3, now(), 'pull', $4) \
             ON CONFLICT (exec_id) DO NOTHING",
        )
        .bind(exec_id)
        .bind(log_hash)
        .bind(executor_id.as_str())
        .bind(source_node)
        .execute(&mut *tx)
        .await?;
        tx.commit().await?;
        Ok(true)
    }

    /// Resolve one attempt by its `exec_id` (the `ReportOutcome`
    /// idempotency key): the assignment row that carries this exec, its
    /// derivation, whether the assignment is still active, and whether
    /// any `drv_attempts` classification already exists for it. `None`
    /// when no assignment carries the exec — never pulled, or already
    /// superseded by a newer attempt (the active-row upsert re-keyed
    /// the row).
    pub(crate) async fn find_attempt_by_exec_id(
        &self,
        exec_id: Uuid,
    ) -> Result<Option<AttemptByExecRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT d.derivation_id, d.drv_hash, d.drv_path, \
                    a.builder_id AS executor_id, \
                    (a.status IN ('pending', 'acknowledged')) AS assignment_active, \
                    EXISTS (SELECT 1 FROM drv_attempts t WHERE t.exec_id = a.exec_id) \
                        AS attempt_recorded, \
                    EXISTS (SELECT 1 FROM drv_attempts t \
                            WHERE t.exec_id = a.exec_id \
                              AND t.termination_reason IS NOT NULL) AS attempt_terminal \
             FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             WHERE a.exec_id = $1 \
             ORDER BY a.assigned_at DESC \
             LIMIT 1",
        )
        .bind(exec_id)
        .fetch_optional(&self.pool)
        .await
    }

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
