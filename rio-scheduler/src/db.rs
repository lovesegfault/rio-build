//! PostgreSQL persistence for scheduler state.
//!
//! Synchronous writes: state transitions, assignment changes, build terminal status.
//! Async/batched: build_history EMA updates.
//!
//! UUIDs are bound natively via the sqlx `uuid` feature — no `::uuid` casts or
//! `.to_string()` conversions needed.

use std::collections::HashMap;

use sqlx::{PgConnection, PgPool, QueryBuilder};
use uuid::Uuid;

use crate::state::{BuildState, DerivationStatus};

/// Assignment lifecycle status (assignments table).
///
/// Only `Pending` and `Completed` are currently set from Rust. The schema
/// also supports `acknowledged`/`failed`/`cancelled` — reserved for phase2c
/// worker-ack and distinct failure reporting.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum AssignmentStatus {
    Pending,
    Acknowledged,
    Completed,
    Failed,
    Cancelled,
}

impl AssignmentStatus {
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Pending => "pending",
            Self::Acknowledged => "acknowledged",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Cancelled => "cancelled",
        }
    }

    pub fn is_terminal(self) -> bool {
        matches!(self, Self::Completed | Self::Failed | Self::Cancelled)
    }
}
/// Database operations for the scheduler.
#[derive(Debug, Clone)]
pub struct SchedulerDb {
    pool: PgPool,
}

/// EMA alpha for duration estimation updates.
const EMA_ALPHA: f64 = 0.3;

/// Row for [`SchedulerDb::batch_upsert_derivations`].
#[derive(Debug)]
pub struct DerivationRow {
    pub drv_hash: String,
    pub drv_path: String,
    pub pname: Option<String>,
    pub system: String,
    pub status: DerivationStatus,
    pub required_features: Vec<String>,
}

impl SchedulerDb {
    /// Create a new database handle from a connection pool.
    pub fn new(pool: PgPool) -> Self {
        Self { pool }
    }

    /// Get a reference to the underlying connection pool.
    pub fn pool(&self) -> &PgPool {
        &self.pool
    }

    // -----------------------------------------------------------------------
    // Build operations
    // -----------------------------------------------------------------------

    /// Insert a new build record.
    pub async fn insert_build(
        &self,
        build_id: Uuid,
        tenant_id: Option<&str>,
        priority_class: crate::state::PriorityClass,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO builds (build_id, tenant_id, requestor, status, priority_class)
            VALUES ($1, $2::uuid, '', 'pending', $3)
            "#,
        )
        .bind(build_id)
        .bind(tenant_id)
        .bind(priority_class.as_str())
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Delete a build row (best-effort cleanup after a failed merge).
    /// Cascade deletes build_derivations links. Used by handle_merge_dag
    /// rollback to clean up the orphan build row if DB persistence fails
    /// after insert_build succeeded.
    pub async fn delete_build(&self, build_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query("DELETE FROM builds WHERE build_id = $1")
            .bind(build_id)
            .execute(&self.pool)
            .await?;
        Ok(())
    }

    /// Update a build's status.
    pub async fn update_build_status(
        &self,
        build_id: Uuid,
        status: BuildState,
        error_summary: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        let now_col = match status {
            BuildState::Active => "started_at",
            BuildState::Succeeded | BuildState::Failed | BuildState::Cancelled => "finished_at",
            BuildState::Pending => "",
        };

        if now_col.is_empty() {
            sqlx::query("UPDATE builds SET status = $2 WHERE build_id = $1")
                .bind(build_id)
                .bind(status.as_str())
                .execute(&self.pool)
                .await?;
        } else if now_col == "started_at" {
            sqlx::query("UPDATE builds SET status = $2, started_at = now() WHERE build_id = $1")
                .bind(build_id)
                .bind(status.as_str())
                .execute(&self.pool)
                .await?;
        } else {
            sqlx::query(
                "UPDATE builds SET status = $2, finished_at = now(), error_summary = $3 WHERE build_id = $1",
            )
            .bind(build_id)
            .bind(status.as_str())
            .bind(error_summary)
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Derivation operations
    // -----------------------------------------------------------------------

    /// Update a derivation's status.
    pub async fn update_derivation_status(
        &self,
        drv_hash: &str,
        status: DerivationStatus,
        assigned_worker: Option<&str>,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            UPDATE derivations
            SET status = $2, assigned_worker_id = $3, updated_at = now()
            WHERE drv_hash = $1
            "#,
        )
        .bind(drv_hash)
        .bind(status.as_str())
        .bind(assigned_worker)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    /// Increment the retry count for a derivation.
    pub async fn increment_retry_count(&self, drv_hash: &str) -> Result<(), sqlx::Error> {
        sqlx::query(
            "UPDATE derivations SET retry_count = retry_count + 1, updated_at = now() WHERE drv_hash = $1",
        )
        .bind(drv_hash)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
    // -----------------------------------------------------------------------
    // Build-derivation mapping
    // -----------------------------------------------------------------------

    /// Link a build to a derivation.
    pub async fn insert_build_derivation(
        &self,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Batch operations (for persist_merge_to_db)
    // -----------------------------------------------------------------------

    /// Batch-upsert derivations. Returns a map `drv_hash -> derivation_id`.
    ///
    /// Uses QueryBuilder::push_values for multi-row INSERT. RETURNING includes
    /// drv_hash because PG doesn't guarantee RETURNING order matches input.
    pub async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
    ) -> Result<HashMap<String, Uuid>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        let mut qb = QueryBuilder::new(
            "INSERT INTO derivations (drv_hash, drv_path, pname, system, status, required_features) ",
        );
        qb.push_values(rows, |mut b, row| {
            b.push_bind(&row.drv_hash)
                .push_bind(&row.drv_path)
                .push_bind(&row.pname)
                .push_bind(&row.system)
                .push_bind(row.status.as_str())
                .push_bind(&row.required_features);
        });
        qb.push(
            " ON CONFLICT (drv_hash) DO UPDATE SET updated_at = now() \
             RETURNING drv_hash, derivation_id",
        );

        let result: Vec<(String, Uuid)> = qb.build_query_as().fetch_all(&mut *tx).await?;
        Ok(result.into_iter().collect())
    }

    /// Batch-insert build_derivations links.
    pub async fn batch_insert_build_derivations(
        tx: &mut PgConnection,
        build_id: Uuid,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(());
        }

        let mut qb = QueryBuilder::new("INSERT INTO build_derivations (build_id, derivation_id) ");
        qb.push_values(derivation_ids, |mut b, did| {
            b.push_bind(build_id).push_bind(did);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
        Ok(())
    }

    /// Batch-insert edges.
    pub async fn batch_insert_edges(
        tx: &mut PgConnection,
        edges: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if edges.is_empty() {
            return Ok(());
        }

        let mut qb = QueryBuilder::new("INSERT INTO derivation_edges (parent_id, child_id) ");
        qb.push_values(edges, |mut b, (p, c)| {
            b.push_bind(p).push_bind(c);
        });
        qb.push(" ON CONFLICT DO NOTHING");
        qb.build().execute(&mut *tx).await?;
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Assignment operations
    // -----------------------------------------------------------------------

    /// Create a new assignment record. Returns the assignment_id.
    pub async fn insert_assignment(
        &self,
        derivation_id: Uuid,
        worker_id: &str,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let row: (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO assignments (derivation_id, worker_id, generation, status)
            VALUES ($1, $2, $3, 'pending')
            RETURNING assignment_id
            "#,
        )
        .bind(derivation_id)
        .bind(worker_id)
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Update an assignment status.
    pub async fn update_assignment_status(
        &self,
        derivation_id: Uuid,
        status: AssignmentStatus,
    ) -> Result<(), sqlx::Error> {
        if status.is_terminal() {
            sqlx::query(
                r#"
                UPDATE assignments
                SET status = $2, completed_at = now()
                WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')
                "#,
            )
            .bind(derivation_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        } else {
            sqlx::query(
                r#"
                UPDATE assignments
                SET status = $2
                WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')
                "#,
            )
            .bind(derivation_id)
            .bind(status.as_str())
            .execute(&self.pool)
            .await?;
        }

        Ok(())
    }

    // -----------------------------------------------------------------------
    // Build history (async/batched)
    // -----------------------------------------------------------------------

    /// Read the full build_history table for estimator refresh.
    ///
    /// Returns `(pname, system, ema_duration_secs, ema_peak_memory_bytes)`.
    /// The estimator loads this into a HashMap at startup and refreshes
    /// on Tick (every 60s). A full table scan is fine: even 10k distinct
    /// (pname, system) pairs is ~1 MB and <10ms. The alternative —
    /// loading on-demand per estimate — would be an async PG roundtrip
    /// inside the single-threaded actor loop on every dispatch decision.
    /// Batch-load once, query in-memory.
    pub async fn read_build_history(
        &self,
    ) -> Result<Vec<(String, String, f64, Option<f64>)>, sqlx::Error> {
        sqlx::query_as(
            r#"
            SELECT pname, system, ema_duration_secs, ema_peak_memory_bytes
            FROM build_history
            "#,
        )
        .fetch_all(&self.pool)
        .await
    }

    /// Update the build history EMA for a (pname, system) pair.
    ///
    /// `peak_memory_bytes`/`output_size_bytes`: None means the worker
    /// had no signal (proc gone, build failed early). Must NOT drag
    /// the EMA toward zero — None → column unchanged.
    ///
    /// The COALESCE(blend, new, old) pattern handles all four
    /// old×new nullability combinations:
    ///   old=Some, new=Some → blend (normal EMA)
    ///   old=None, new=Some → blend is NULL (NULL*x), falls to new (first sample)
    ///   old=Some, new=None → blend is NULL (x+NULL), falls to new=NULL, falls to old (keep)
    ///   old=None, new=None → all NULL (still no signal)
    ///
    /// PG: any arithmetic with NULL yields NULL; COALESCE picks first non-NULL.
    pub async fn update_build_history(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
        peak_memory_bytes: Option<u64>,
        output_size_bytes: Option<u64>,
    ) -> Result<(), sqlx::Error> {
        // u64 → f64 for DOUBLE PRECISION binding. Precision loss at
        // ~2^53 bytes (~9 PB) is not a concern.
        let peak_mem = peak_memory_bytes.map(|b| b as f64);
        let out_size = output_size_bytes.map(|b| b as f64);

        sqlx::query(
            r#"
            INSERT INTO build_history
              (pname, system, ema_duration_secs,
               ema_peak_memory_bytes, ema_output_size_bytes,
               sample_count, last_updated)
            VALUES ($1, $2, $3, $5, $6, 1, now())
            ON CONFLICT (pname, system) DO UPDATE SET
                ema_duration_secs = build_history.ema_duration_secs * (1.0 - $4) + $3 * $4,
                ema_peak_memory_bytes = COALESCE(
                    build_history.ema_peak_memory_bytes * (1.0 - $4) + $5 * $4,
                    $5,
                    build_history.ema_peak_memory_bytes),
                ema_output_size_bytes = COALESCE(
                    build_history.ema_output_size_bytes * (1.0 - $4) + $6 * $4,
                    $6,
                    build_history.ema_output_size_bytes),
                sample_count = build_history.sample_count + 1,
                last_updated = now()
            "#,
        )
        .bind(pname)
        .bind(system)
        .bind(actual_duration_secs)
        .bind(EMA_ALPHA)
        .bind(peak_mem)
        .bind(out_size)
        .execute(&self.pool)
        .await?;

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_ema_alpha_range() {
        const { assert!(EMA_ALPHA > 0.0 && EMA_ALPHA < 1.0) };
    }

    #[test]
    fn test_derivation_status_roundtrip() -> anyhow::Result<()> {
        for status in [
            DerivationStatus::Created,
            DerivationStatus::Queued,
            DerivationStatus::Ready,
            DerivationStatus::Assigned,
            DerivationStatus::Running,
            DerivationStatus::Completed,
            DerivationStatus::Failed,
            DerivationStatus::Poisoned,
            DerivationStatus::DependencyFailed,
        ] {
            let s = status.as_str();
            let parsed: DerivationStatus = s.parse()?;
            assert_eq!(parsed, status);
        }
        Ok(())
    }

    #[test]
    fn test_build_state_roundtrip() -> anyhow::Result<()> {
        for state in [
            BuildState::Pending,
            BuildState::Active,
            BuildState::Succeeded,
            BuildState::Failed,
            BuildState::Cancelled,
        ] {
            let s = state.as_str();
            let parsed: BuildState = s.parse()?;
            assert_eq!(parsed, state);
        }
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Direct DB integration tests via TestDb
    // -----------------------------------------------------------------------

    use rio_test_support::TestDb;
    static MIGRATOR: sqlx::migrate::Migrator = sqlx::migrate!("../migrations");

    #[test]
    fn test_assignment_status_as_str_exhaustive() {
        assert_eq!(AssignmentStatus::Pending.as_str(), "pending");
        assert_eq!(AssignmentStatus::Acknowledged.as_str(), "acknowledged");
        assert_eq!(AssignmentStatus::Completed.as_str(), "completed");
        assert_eq!(AssignmentStatus::Failed.as_str(), "failed");
        assert_eq!(AssignmentStatus::Cancelled.as_str(), "cancelled");
    }

    #[test]
    fn test_assignment_status_is_terminal() {
        assert!(!AssignmentStatus::Pending.is_terminal());
        assert!(!AssignmentStatus::Acknowledged.is_terminal());
        assert!(AssignmentStatus::Completed.is_terminal());
        assert!(AssignmentStatus::Failed.is_terminal());
        assert!(AssignmentStatus::Cancelled.is_terminal());
    }

    /// Upsert a single derivation and return its db_id. Helper for tests
    /// that need a valid derivation_id foreign key.
    async fn insert_test_derivation(db: &SchedulerDb, drv_hash: &str) -> anyhow::Result<Uuid> {
        let mut tx = db.pool.begin().await?;
        let row = DerivationRow {
            drv_hash: drv_hash.into(),
            drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
        };
        let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[row]).await?;
        tx.commit().await?;
        Ok(*ids.get(drv_hash).expect("just inserted"))
    }

    #[tokio::test]
    async fn test_insert_build_derivation_idempotent() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(build_id, None, crate::state::PriorityClass::Scheduled)
            .await?;
        let drv_id = insert_test_derivation(&db, "aaa").await?;

        // Call twice — ON CONFLICT DO NOTHING should make the second call a no-op.
        db.insert_build_derivation(build_id, drv_id).await?;
        db.insert_build_derivation(build_id, drv_id).await?;

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(count.0, 1, "ON CONFLICT should prevent duplicate");
        Ok(())
    }

    /// BuildState::Pending has now_col="" → no timestamp column touched.
    #[tokio::test]
    async fn test_update_build_status_pending_no_timestamps() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(build_id, None, crate::state::PriorityClass::Scheduled)
            .await?;

        // Insert starts at 'pending'. Transition to Active then back to Pending
        // (unusual but valid at the DB layer — the state machine rejects it,
        // but this is testing the raw SQL branch).
        db.update_build_status(build_id, BuildState::Active, None)
            .await?;
        db.update_build_status(build_id, BuildState::Pending, None)
            .await?;

        // Query timestamp as Option<String> via text cast to avoid adding a
        // chrono dep just for test assertions.
        let (status, finished_at): (String, Option<String>) =
            sqlx::query_as("SELECT status, finished_at::text FROM builds WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await?;
        assert_eq!(status, "pending");
        // Pending transition does NOT set finished_at (the now_col="" branch).
        assert!(finished_at.is_none());
        Ok(())
    }

    /// Non-terminal status (Acknowledged) → completed_at stays NULL.
    #[tokio::test]
    async fn test_update_assignment_status_acknowledged_no_completed_at() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "bbb").await?;
        db.insert_assignment(drv_id, "worker-1", 1).await?;

        db.update_assignment_status(drv_id, AssignmentStatus::Acknowledged)
            .await?;

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await?;
        assert_eq!(status, "acknowledged");
        assert!(completed_at.is_none(), "non-terminal → completed_at NULL");
        Ok(())
    }

    /// Terminal status (Completed) → completed_at = now().
    #[tokio::test]
    async fn test_update_assignment_status_completed_sets_completed_at() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "ccc").await?;
        db.insert_assignment(drv_id, "worker-1", 1).await?;

        db.update_assignment_status(drv_id, AssignmentStatus::Completed)
            .await?;

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await?;
        assert_eq!(status, "completed");
        assert!(completed_at.is_some(), "terminal → completed_at set");
        Ok(())
    }

    /// EMA: first insert uses the duration directly; second update blends.
    /// ema = old * (1-ALPHA) + new * ALPHA = 10 * 0.7 + 20 * 0.3 = 13.
    #[tokio::test]
    async fn test_update_build_history_ema_accumulates() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        db.update_build_history("hello", "x86_64-linux", 10.0, None, None)
            .await?;
        let (ema1, count1): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        assert!((ema1 - 10.0).abs() < 0.001, "first insert: ema={ema1}");
        assert_eq!(count1, 1);

        db.update_build_history("hello", "x86_64-linux", 20.0, None, None)
            .await?;
        let (ema2, count2): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await?;
        let expected = 10.0 * (1.0 - EMA_ALPHA) + 20.0 * EMA_ALPHA;
        assert!(
            (ema2 - expected).abs() < 0.001,
            "second update: expected {expected}, got {ema2}"
        );
        assert_eq!(count2, 2);
        Ok(())
    }

    /// The COALESCE(blend, new, old) pattern for the nullable resource
    /// EMAs. All four old×new combinations in one test: the hard part
    /// is the `None` cases, where a naïve `old*0.7 + 0*0.3` would drag
    /// a real EMA toward zero on every build with a failed proc read.
    #[tokio::test]
    async fn test_update_build_history_memory_ema_none_is_no_signal() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let fetch = || async {
            sqlx::query_as::<_, (Option<f64>, Option<f64>)>(
                "SELECT ema_peak_memory_bytes, ema_output_size_bytes \
                 FROM build_history WHERE pname = 'mem' AND system = 'x86_64-linux'",
            )
            .fetch_one(&test_db.pool)
            .await
        };

        // 1. old=None, new=None → stays None.
        db.update_build_history("mem", "x86_64-linux", 1.0, None, None)
            .await?;
        let (mem, out) = fetch().await?;
        assert_eq!(mem, None, "None×None: mem should stay NULL");
        assert_eq!(out, None, "None×None: out should stay NULL");

        // 2. old=None, new=Some → initializes (blend is NULL, falls
        //    to $new).
        db.update_build_history("mem", "x86_64-linux", 1.0, Some(100_000_000), Some(5000))
            .await?;
        let (mem, out) = fetch().await?;
        assert!(
            mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
            "None×Some: mem should initialize to 100M, got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - 5000.0).abs() < 0.001),
            "None×Some: out should initialize to 5000, got {out:?}"
        );

        // 3. old=Some, new=None → KEEPS old (the important case — no
        //    signal doesn't drag). blend is NULL (x+NULL), falls to
        //    $new=NULL, falls to old.
        db.update_build_history("mem", "x86_64-linux", 1.0, None, None)
            .await?;
        let (mem, out) = fetch().await?;
        assert!(
            mem.is_some_and(|m| (m - 100_000_000.0).abs() < 0.001),
            "Some×None: mem should be UNCHANGED at 100M (no drag), got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - 5000.0).abs() < 0.001),
            "Some×None: out should be UNCHANGED at 5000 (no drag), got {out:?}"
        );

        // 4. old=Some, new=Some → normal EMA blend.
        db.update_build_history("mem", "x86_64-linux", 1.0, Some(200_000_000), Some(10_000))
            .await?;
        let (mem, out) = fetch().await?;
        let expect_mem = 100_000_000.0 * (1.0 - EMA_ALPHA) + 200_000_000.0 * EMA_ALPHA;
        let expect_out = 5000.0 * (1.0 - EMA_ALPHA) + 10_000.0 * EMA_ALPHA;
        assert!(
            mem.is_some_and(|m| (m - expect_mem).abs() < 0.001),
            "Some×Some: mem should blend to {expect_mem}, got {mem:?}"
        );
        assert!(
            out.is_some_and(|o| (o - expect_out).abs() < 0.001),
            "Some×Some: out should blend to {expect_out}, got {out:?}"
        );

        Ok(())
    }

    /// D3's estimator reads `ema_peak_memory_bytes` for D7's memory-bump
    /// classify(). Verify the write→read roundtrip works end to end.
    #[tokio::test]
    async fn test_build_history_memory_roundtrip_read() -> anyhow::Result<()> {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        db.update_build_history(
            "roundtrip",
            "aarch64-linux",
            42.0,
            Some(1_073_741_824),
            None,
        )
        .await?;

        let rows = db.read_build_history().await?;
        let row = rows
            .iter()
            .find(|(p, s, _, _)| p == "roundtrip" && s == "aarch64-linux")
            .expect("written row should be readable");

        assert!((row.2 - 42.0).abs() < 0.001, "duration: {}", row.2);
        assert!(
            row.3.is_some_and(|m| (m - 1_073_741_824.0).abs() < 0.001),
            "peak mem: {:?}",
            row.3
        );
        Ok(())
    }
}
