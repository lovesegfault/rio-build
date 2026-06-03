//! The ledger-backed open-attempt view (pull-mode dispatch).
//!
//! An **open attempt** is an active `assignments` row (status ∈
//! {pending, acknowledged}) joined to its `drv_executions` row via
//! `exec_id` — both written by the pull transaction — with terminality
//! decided by the `drv_attempts` fill (a row whose `termination_reason`
//! is filled is closed even if the assignment-close write has not
//! landed yet).
//!
//! The pull transaction is the only `drv_executions` writer (the
//! stream dispatch path is deleted, and migration 076 dropped the
//! `dispatch_mode` coexistence discriminator with it), so the plain
//! assignment ⋈ execution join IS the open-attempt set; every consumer
//! — the establishment sweep, the `ListOpenAttempts` RPC, the
//! open-attempts gauge, the controller's synthesize-on-delete arm —
//! reads it unfiltered.

use uuid::Uuid;

use super::SchedulerDb;

/// One attempt resolved by `exec_id` (the `ReportOutcome` lookup).
#[derive(Debug, Clone, sqlx::FromRow)]
struct AttemptByExecRow {
    /// The DAG key (`derivations.derivation_id`).
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
    /// Work class of the execution (`drv_executions.attempt_kind`,
    /// read directly — the execution row is REQUIRED by the join, so
    /// absence is unrepresentable, never defaulted; the B2 security
    /// sweep's absence-as-verdict class). Routes the report intake:
    /// materialization attempts go to the consumption transaction,
    /// build attempts to the as-built completion path.
    pub attempt_kind: String,
}

/// The kind-typed attempt witness — the ONLY attempt-resolution shape
/// that reaches the actor (merged_bug_146 / bug_121's class). The kind
/// is parsed exactly once, here at the row boundary; a row whose
/// `attempt_kind` is outside the alphabet is a DECODE ERROR, never a
/// default. Cross-kind operations are uncompilable downstream: the
/// uncharged close takes `&BuildAttempt`, the consumption transaction
/// takes `&MatAttempt`.
#[derive(Debug, Clone)]
pub(crate) enum AttemptRef {
    Build(BuildAttempt),
    Materialization(MatAttempt),
}

/// The kind-independent facts of one resolved attempt.
#[derive(Debug, Clone)]
pub(crate) struct AttemptCore {
    /// The DAG key (`derivations.derivation_id`).
    pub derivation_id: Uuid,
    /// `derivations.drv_hash` — the intent id / DAG key string.
    pub drv_hash: String,
    /// Full `.drv` store path (what the completion path keys on).
    pub drv_path: String,
    /// Executor identity the attempt is bound to.
    pub executor_id: String,
    /// The assignment row is still active (pending/acknowledged).
    pub assignment_active: bool,
    /// Some `drv_attempts` row already exists for this exec.
    pub attempt_recorded: bool,
    /// A terminal `drv_attempts` fill exists for this exec.
    pub attempt_terminal: bool,
}

/// A resolved BUILD-kind attempt (witness arm).
#[derive(Debug, Clone)]
pub(crate) struct BuildAttempt {
    pub core: AttemptCore,
}

/// A resolved MATERIALIZATION-kind attempt (witness arm).
#[derive(Debug, Clone)]
pub(crate) struct MatAttempt {
    pub core: AttemptCore,
}

impl AttemptRef {
    /// The kind-independent facts (token binding, logging).
    pub(crate) fn core(&self) -> &AttemptCore {
        match self {
            Self::Build(b) => &b.core,
            Self::Materialization(m) => &m.core,
        }
    }

    /// Parse one row into the witness — the single kind-parse site.
    fn from_row(row: AttemptByExecRow) -> Result<Self, sqlx::Error> {
        let AttemptByExecRow {
            derivation_id,
            drv_hash,
            drv_path,
            executor_id,
            assignment_active,
            attempt_recorded,
            attempt_terminal,
            attempt_kind,
        } = row;
        let core = AttemptCore {
            derivation_id,
            drv_hash,
            drv_path,
            executor_id,
            assignment_active,
            attempt_recorded,
            attempt_terminal,
        };
        match attempt_kind.as_str() {
            "build" => Ok(Self::Build(BuildAttempt { core })),
            "materialization" => Ok(Self::Materialization(MatAttempt { core })),
            other => Err(sqlx::Error::Decode(
                format!(
                    "drv_executions.attempt_kind: value {other:?} not in the rust-side alphabet"
                )
                .into(),
            )),
        }
    }
}

/// Row shape for [`SchedulerDb::find_open_pull_attempt_by_drv_hash`]
/// (the same columns plus the exec_id key).
#[derive(Debug, Clone, sqlx::FromRow)]
struct AttemptByExecByHashRow {
    exec_id: Uuid,
    derivation_id: Uuid,
    drv_hash: String,
    drv_path: String,
    executor_id: String,
    assignment_active: bool,
    attempt_recorded: bool,
    attempt_terminal: bool,
    attempt_kind: String,
}

/// One open pull-mode attempt as read back from the view query.
#[derive(Debug, Clone, sqlx::FromRow)]
pub(crate) struct OpenAttemptRow {
    /// The DAG key (`derivations.derivation_id`).
    pub derivation_id: Uuid,
    /// `derivations.drv_hash` — the intent id (the spawn-intent key).
    pub drv_hash: String,
    /// Full `.drv` store path.
    pub drv_path: String,
    /// Per-execution identifier minted by the pull transaction.
    pub exec_id: Uuid,
    /// Executor identity the attempt is bound to (`assignments.builder_id`).
    pub executor_id: String,
    /// `derivations.system` — what the pod was spawned to build. The
    /// re-pointed `ListExecutors` surface reports it as the entry's
    /// `systems`.
    pub system: String,
    /// `derivations.is_fixed_output` — drives the entry's
    /// builder/fetcher `kind` on the re-pointed `ListExecutors`
    /// surface (one-shot pods build exactly the drv they pulled).
    pub is_fixed_output: bool,
    /// Controller-authoritative node binding, when known (071).
    pub source_node: Option<String>,
    /// Lease generation the assignment row carries.
    pub generation: i64,
    /// `assignments.assigned_at` as epoch seconds (PG clock).
    // The sweep's window math reads age_secs (PG-clock relative); this
    // absolute stamp feeds the re-pointed ListExecutors timestamps.
    pub assigned_at_epoch_secs: f64,
    /// Age of the assignment row in seconds (PG clock, non-negative).
    pub age_secs: f64,
    /// The deadline (seconds) this attempt was dispatched under,
    /// persisted by the pull mint (072). The establishment sweep's
    /// window anchor: the window may widen via the sweep-time re-solve
    /// but never shrink below this. `None` for rows minted before 072.
    pub deadline_secs: Option<f64>,
    /// Work class of the execution (`drv_executions.attempt_kind`,
    /// read directly — the execution row is required by the join). The
    /// establishment sweep branches on it: materialization attempts
    /// get the no-adopt materialization_infra arm.
    pub attempt_kind: String,
}

impl SchedulerDb {
    /// The mint's active-row upsert statement, shared between the
    /// production mint transaction and the statement-guard interleaving
    /// test (the test exercises THIS statement under a hand-held race —
    /// never a copy). Same active-row upsert discipline as the stream
    /// path's historical insert_assignment (assignments_active_uq is
    /// the arbiter). Returns `rows_affected`.
    ///
    /// **The statement guard (bug_261).** Under READ COMMITTED the
    /// begin-time floor read races a successor's claim+mint committing
    /// between the floor read and this upsert: the Rust-side compare
    /// passes on a stale floor, and an unguarded `DO UPDATE` would
    /// overwrite the successor's newer row (regressing `generation` and
    /// clobbering its `exec_id`). The
    /// `WHERE assignments.generation <= EXCLUDED.generation` predicate
    /// closes that TOCTOU for the destructive half: PostgreSQL
    /// evaluates the conflict-arm WHERE against the row's LATEST
    /// COMMITTED version (EvalPlanQual), so a mid-race lower-generation
    /// mint updates zero rows — the begin-time floor check is advisory;
    /// this guard is authoritative. Equal generation passes (`<=`, the
    /// same-epoch re-acquire keep). The fresh-INSERT-below-floor
    /// residual (no conflict row to evaluate against) is priced in
    /// `fence-invariant-map.md` and bounded in `fencedWrites.qnt`.
    // r[impl sched.lease.fence-statement-guard]
    pub(crate) async fn mint_assignment_upsert_in_tx(
        conn: &mut sqlx::PgConnection,
        derivation_id: Uuid,
        executor_id: &str,
        generation: i64,
        exec_id: Uuid,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
             VALUES ($1, $2, $3, 'pending', $4) \
             ON CONFLICT (derivation_id) WHERE status IN ('pending', 'acknowledged') \
             DO UPDATE SET \
                 builder_id = EXCLUDED.builder_id, \
                 generation = EXCLUDED.generation, \
                 status = 'pending', \
                 assigned_at = now(), \
                 completed_at = NULL, \
                 exec_id = EXCLUDED.exec_id \
             WHERE assignments.generation <= EXCLUDED.generation",
        )
        .bind(derivation_id)
        .bind(executor_id)
        .bind(generation)
        .bind(exec_id)
        .execute(&mut *conn)
        .await?;
        Ok(result.rows_affected())
    }
}

impl SchedulerDb {
    /// The fenced pull-mint transaction (the durable half of
    /// `PullAssignment`'s Deliver arm): write/refresh the active
    /// `assignments` row and insert the `drv_executions` row
    /// (`source_node` when known) through the [`super::FencedTx`]
    /// capability — committed only at-or-above the durable claims
    /// floor. Returns [`super::FencedOutcome::Fenced`] when the fence
    /// (begin-time floor or the upsert's own statement guard) refused;
    /// nothing written in that case.
    // r[impl sched.executor.pull-transaction+2]
    // r[impl sched.lease.generation-fence+3]
    // The argument list mirrors the row pair this single transaction
    // writes (same precedent as the other multi-column writers).
    #[allow(clippy::too_many_arguments)]
    pub(crate) async fn mint_pull_attempt_fenced(
        &self,
        derivation_id: Uuid,
        executor_id: &crate::state::ExecutorId,
        serving_generation: i64,
        exec_id: Uuid,
        log_hash: &str,
        source_node: Option<&str>,
        deadline_secs: Option<f64>,
        attempt_kind: crate::state::AttemptKind,
    ) -> Result<super::FencedOutcome, sqlx::Error> {
        let mut tx = match self.begin_fenced(serving_generation).await? {
            super::FencedBegin::Fenced { .. } => return Ok(super::FencedOutcome::Fenced),
            super::FencedBegin::Open(ftx) => ftx,
        };
        let upserted = Self::mint_assignment_upsert_in_tx(
            tx.conn(),
            derivation_id,
            executor_id.as_str(),
            serving_generation,
            exec_id,
        )
        .await?;
        if upserted == 0 {
            // The statement guard refused: a newer-generation active
            // row exists (the begin-time floor read lost the race).
            // Drop without commit = rollback — the drv_executions row
            // must not exist for a mint that never owned the
            // assignment.
            return Ok(super::FencedOutcome::Fenced);
        }
        // The execution lifecycle row carries the controller-
        // authoritative source attribution (071), the dispatched-
        // deadline anchor for the establishment window (072), and the
        // work class (078, substitution-replacement: 'build' for build
        // pulls — value-identical to the column default — and
        // 'materialization' for store-replica claims; the kind
        // partition's durable key).
        sqlx::query(
            "INSERT INTO drv_executions \
                 (exec_id, drv_hash, executor_id, started_at, source_node, deadline_secs, \
                  attempt_kind) \
             VALUES ($1, $2, $3, now(), $4, $5, $6) \
             ON CONFLICT (exec_id) DO NOTHING",
        )
        .bind(exec_id)
        .bind(log_hash)
        .bind(executor_id.as_str())
        .bind(source_node)
        .bind(deadline_secs)
        .bind(attempt_kind.as_str())
        .execute(tx.conn())
        .await?;
        tx.commit().await?;
        Ok(super::FencedOutcome::Applied(1))
    }

    /// Backfill the execution row's controller-authoritative node
    /// attribution when the pull mint lost the race against the
    /// binding ack (AD2c): NULL-only — an attribution already present
    /// is never overwritten — and idempotent. Callers pass only
    /// controller-reported nodes (`ReportAttemptOutcome.node_name` /
    /// the spawn-ack binding), never worker-supplied identity.
    pub(crate) async fn fill_open_execution_source_node(
        &self,
        exec_id: Uuid,
        source_node: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE drv_executions SET source_node = $2 \
             WHERE exec_id = $1 AND source_node IS NULL",
        )
        .bind(exec_id)
        .bind(source_node)
        .execute(&self.pool)
        .await
        .map(|_| ())
    }

    /// Resolve one attempt by its `exec_id` (the `ReportOutcome`
    /// idempotency key): the assignment row that carries this exec, its
    /// derivation, whether the assignment is still active, and whether
    /// any `drv_attempts` classification already exists for it. `None`
    /// when no assignment carries the exec — never pulled, or already
    /// superseded by a newer attempt (the active-row upsert re-keyed
    /// the row) — and equally when the assignment has NO
    /// `drv_executions` row: an attempt whose kind is unknowable is
    /// not resolvable (deny-by-default; pre-fix the LEFT JOIN +
    /// COALESCE defaulted it to the build lane).
    pub(crate) async fn find_attempt_by_exec_id(
        &self,
        exec_id: Uuid,
    ) -> Result<Option<AttemptRef>, sqlx::Error> {
        let row: Option<AttemptByExecRow> = sqlx::query_as(
            "SELECT d.derivation_id, d.drv_hash, d.drv_path, \
                    a.builder_id AS executor_id, \
                    (a.status IN ('pending', 'acknowledged')) AS assignment_active, \
                    EXISTS (SELECT 1 FROM drv_attempts t WHERE t.exec_id = a.exec_id) \
                        AS attempt_recorded, \
                    EXISTS (SELECT 1 FROM drv_attempts t \
                            WHERE t.exec_id = a.exec_id \
                              AND t.termination_reason IS NOT NULL) AS attempt_terminal, \
                    e.attempt_kind \
             FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             JOIN drv_executions e ON e.exec_id = a.exec_id \
             WHERE a.exec_id = $1 \
             ORDER BY a.assigned_at DESC \
             LIMIT 1",
        )
        .bind(exec_id)
        .fetch_optional(&self.pool)
        .await?;
        row.map(AttemptRef::from_row).transpose()
    }

    /// Resolve the OPEN pull-mode attempt for one derivation (the
    /// `intent_id` arm of `ReportAttemptOutcome`'s identity
    /// resolution): the active assignment for that drv joined to its
    /// execution row.
    pub(crate) async fn find_open_pull_attempt_by_drv_hash(
        &self,
        drv_hash: &str,
    ) -> Result<Option<(Uuid, AttemptRef)>, sqlx::Error> {
        let row: Option<AttemptByExecByHashRow> = sqlx::query_as(
            "SELECT a.exec_id, d.derivation_id, d.drv_hash, d.drv_path, \
                    a.builder_id AS executor_id, \
                    (a.status IN ('pending', 'acknowledged')) AS assignment_active, \
                    EXISTS (SELECT 1 FROM drv_attempts t WHERE t.exec_id = a.exec_id) \
                        AS attempt_recorded, \
                    EXISTS (SELECT 1 FROM drv_attempts t \
                            WHERE t.exec_id = a.exec_id \
                              AND t.termination_reason IS NOT NULL) AS attempt_terminal, \
                    e.attempt_kind \
             FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             JOIN drv_executions e ON e.exec_id = a.exec_id \
             WHERE d.drv_hash = $1 \
               AND a.status IN ('pending', 'acknowledged') \
             ORDER BY a.assigned_at DESC \
             LIMIT 1",
        )
        .bind(drv_hash)
        .fetch_optional(&self.pool)
        .await?;
        row.map(|r| {
            let exec_id = r.exec_id;
            AttemptRef::from_row(AttemptByExecRow {
                derivation_id: r.derivation_id,
                drv_hash: r.drv_hash,
                drv_path: r.drv_path,
                executor_id: r.executor_id,
                assignment_active: r.assignment_active,
                attempt_recorded: r.attempt_recorded,
                attempt_terminal: r.attempt_terminal,
                attempt_kind: r.attempt_kind,
            })
            .map(|a| (exec_id, a))
        })
        .transpose()
    }

    /// Every open attempt: active assignment ⋈ execution with no
    /// terminal `drv_attempts` fill.
    ///
    /// The pull transaction is the only execution writer, so the plain
    /// join needs no discriminator — terminality (the `drv_attempts`
    /// fill) is the only filter.
    pub(crate) async fn list_open_pull_attempts(&self) -> Result<Vec<OpenAttemptRow>, sqlx::Error> {
        let rows: Vec<OpenAttemptRow> = sqlx::query_as(
            "SELECT d.derivation_id, d.drv_hash, d.drv_path, d.system, d.is_fixed_output, \
                    a.exec_id, a.builder_id AS executor_id, a.generation, \
                    e.source_node, e.deadline_secs, \
                    e.attempt_kind, \
                    EXTRACT(EPOCH FROM a.assigned_at)::float8 AS assigned_at_epoch_secs, \
                    GREATEST(EXTRACT(EPOCH FROM (now() - a.assigned_at))::float8, 0.0) \
                        AS age_secs \
             FROM assignments a \
             JOIN derivations d ON d.derivation_id = a.derivation_id \
             JOIN drv_executions e ON e.exec_id = a.exec_id \
             WHERE a.status IN ('pending', 'acknowledged') \
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

impl SchedulerDb {
    /// Recovery view-rebuild load (Phase B, T-4.3): every unresolved
    /// materialization job plus the holder of its open MATERIALIZATION
    /// attempt (if any). Unlike `list_claimable_materialization_jobs`
    /// this does NOT filter parked or claimed jobs — the in-memory view
    /// must mirror ALL unresolved state so pull admission answers
    /// correctly from the very first post-failover claim (a parked job
    /// answers NotYetReady, a held attempt re-delivers to its holder,
    /// and nothing answers the stranding `Gone`).
    ///
    /// **Kind discipline (bug_266).** The holder join carries the kind
    /// conjunct: only an assignment whose execution row is
    /// `attempt_kind = 'materialization'` resolves as `claimed_by`. A
    /// live BUILD attempt on the same derivation (the stale-reset lane
    /// creates jobs while a build attempt is open) is the SLOT holder
    /// but never the JOB holder — pre-084 the kind-blind join stamped
    /// the builder onto the job and recovery rebuilt a Claimed view
    /// nobody held. Relocated here (the single sanctioned home for
    /// `assignments` joins — `assignments-join-policy` in
    /// nix/misc-checks.nix) from db/materialization.rs.
    pub(crate) async fn load_unresolved_materialization_jobs(
        &self,
    ) -> Result<Vec<RecoveredJobRow>, sqlx::Error> {
        sqlx::query_as(
            "SELECT j.job_id, j.drv_hash, j.carried_realized_paths, \
                    EXTRACT(EPOCH FROM (j.park_until - now()))::float8 AS park_remaining_secs, \
                    EXTRACT(EPOCH FROM (now() - j.park_began_at))::float8 AS park_began_secs_ago, \
                    a.builder_id AS claimed_by \
               FROM materialization_jobs j \
               LEFT JOIN assignments a ON a.derivation_id = j.derivation_id \
                                      AND a.status IN ('pending', 'acknowledged') \
                                      AND EXISTS ( \
                                          SELECT 1 FROM drv_executions e \
                                           WHERE e.exec_id = a.exec_id \
                                             AND e.attempt_kind = 'materialization') \
              WHERE j.state = 'pending'",
        )
        .fetch_all(&self.pool)
        .await
    }
}

/// One unresolved job as the recovery view rebuild loads it (T-4.3).
#[derive(Debug, sqlx::FromRow)]
pub(crate) struct RecoveredJobRow {
    pub job_id: Uuid,
    pub drv_hash: String,
    /// Realized-path carrier (migration 082); `None` = no carrier.
    pub carried_realized_paths: Option<Vec<String>>,
    /// Seconds until the park expires; `None` or non-positive = not
    /// parked (or the park already lapsed).
    pub park_remaining_secs: Option<f64>,
    /// Seconds since the most recent park began (migration 083, the
    /// failover-exact dwell anchor); `None` = never parked or parked
    /// pre-083 (the dwell gate treats it as unmet — conservative).
    pub park_began_secs_ago: Option<f64>,
    /// The open MATERIALIZATION attempt's holder identity; `None` =
    /// unclaimed (a build-kind slot holder is not a job holder).
    pub claimed_by: Option<String>,
}
