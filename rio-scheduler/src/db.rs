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

    /// Update the build history EMA for a (pname, system) pair.
    pub async fn update_build_history(
        &self,
        pname: &str,
        system: &str,
        actual_duration_secs: f64,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO build_history (pname, system, ema_duration_secs, sample_count, last_updated)
            VALUES ($1, $2, $3, 1, now())
            ON CONFLICT (pname, system) DO UPDATE SET
                ema_duration_secs = build_history.ema_duration_secs * (1.0 - $4) + $3 * $4,
                sample_count = build_history.sample_count + 1,
                last_updated = now()
            "#,
        )
        .bind(pname)
        .bind(system)
        .bind(actual_duration_secs)
        .bind(EMA_ALPHA)
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
    fn test_derivation_status_roundtrip() {
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
            let parsed: DerivationStatus = s.parse().unwrap();
            assert_eq!(parsed, status);
        }
    }

    #[test]
    fn test_build_state_roundtrip() {
        for state in [
            BuildState::Pending,
            BuildState::Active,
            BuildState::Succeeded,
            BuildState::Failed,
            BuildState::Cancelled,
        ] {
            let s = state.as_str();
            let parsed: BuildState = s.parse().unwrap();
            assert_eq!(parsed, state);
        }
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
    async fn insert_test_derivation(db: &SchedulerDb, drv_hash: &str) -> Uuid {
        let mut tx = db.pool.begin().await.unwrap();
        let row = DerivationRow {
            drv_hash: drv_hash.into(),
            drv_path: rio_test_support::fixtures::test_drv_path(drv_hash),
            pname: Some("test-pkg".into()),
            system: "x86_64-linux".into(),
            status: DerivationStatus::Created,
            required_features: vec![],
        };
        let ids = SchedulerDb::batch_upsert_derivations(&mut tx, &[row])
            .await
            .unwrap();
        tx.commit().await.unwrap();
        *ids.get(drv_hash).unwrap()
    }

    #[tokio::test]
    async fn test_insert_build_derivation_idempotent() {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(build_id, None, crate::state::PriorityClass::Scheduled)
            .await
            .unwrap();
        let drv_id = insert_test_derivation(&db, "aaa").await;

        // Call twice — ON CONFLICT DO NOTHING should make the second call a no-op.
        db.insert_build_derivation(build_id, drv_id).await.unwrap();
        db.insert_build_derivation(build_id, drv_id).await.unwrap();

        let count: (i64,) =
            sqlx::query_as("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await
                .unwrap();
        assert_eq!(count.0, 1, "ON CONFLICT should prevent duplicate");
    }

    /// BuildState::Pending has now_col="" → no timestamp column touched.
    #[tokio::test]
    async fn test_update_build_status_pending_no_timestamps() {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let build_id = Uuid::new_v4();
        db.insert_build(build_id, None, crate::state::PriorityClass::Scheduled)
            .await
            .unwrap();

        // Insert starts at 'pending'. Transition to Active then back to Pending
        // (unusual but valid at the DB layer — the state machine rejects it,
        // but this is testing the raw SQL branch).
        db.update_build_status(build_id, BuildState::Active, None)
            .await
            .unwrap();
        db.update_build_status(build_id, BuildState::Pending, None)
            .await
            .unwrap();

        // Query timestamp as Option<String> via text cast to avoid adding a
        // chrono dep just for test assertions.
        let (status, finished_at): (String, Option<String>) =
            sqlx::query_as("SELECT status, finished_at::text FROM builds WHERE build_id = $1")
                .bind(build_id)
                .fetch_one(&test_db.pool)
                .await
                .unwrap();
        assert_eq!(status, "pending");
        // Pending transition does NOT set finished_at (the now_col="" branch).
        assert!(finished_at.is_none());
    }

    /// Non-terminal status (Acknowledged) → completed_at stays NULL.
    #[tokio::test]
    async fn test_update_assignment_status_acknowledged_no_completed_at() {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "bbb").await;
        db.insert_assignment(drv_id, "worker-1", 1).await.unwrap();

        db.update_assignment_status(drv_id, AssignmentStatus::Acknowledged)
            .await
            .unwrap();

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await
        .unwrap();
        assert_eq!(status, "acknowledged");
        assert!(completed_at.is_none(), "non-terminal → completed_at NULL");
    }

    /// Terminal status (Completed) → completed_at = now().
    #[tokio::test]
    async fn test_update_assignment_status_completed_sets_completed_at() {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        let drv_id = insert_test_derivation(&db, "ccc").await;
        db.insert_assignment(drv_id, "worker-1", 1).await.unwrap();

        db.update_assignment_status(drv_id, AssignmentStatus::Completed)
            .await
            .unwrap();

        let (status, completed_at): (String, Option<String>) = sqlx::query_as(
            "SELECT status, completed_at::text FROM assignments WHERE derivation_id = $1",
        )
        .bind(drv_id)
        .fetch_one(&test_db.pool)
        .await
        .unwrap();
        assert_eq!(status, "completed");
        assert!(completed_at.is_some(), "terminal → completed_at set");
    }

    /// EMA: first insert uses the duration directly; second update blends.
    /// ema = old * (1-ALPHA) + new * ALPHA = 10 * 0.7 + 20 * 0.3 = 13.
    #[tokio::test]
    async fn test_update_build_history_ema_accumulates() {
        let test_db = TestDb::new(&MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());

        db.update_build_history("hello", "x86_64-linux", 10.0)
            .await
            .unwrap();
        let (ema1, count1): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await
        .unwrap();
        assert!((ema1 - 10.0).abs() < 0.001, "first insert: ema={ema1}");
        assert_eq!(count1, 1);

        db.update_build_history("hello", "x86_64-linux", 20.0)
            .await
            .unwrap();
        let (ema2, count2): (f64, i32) = sqlx::query_as(
            "SELECT ema_duration_secs, sample_count FROM build_history \
             WHERE pname = 'hello' AND system = 'x86_64-linux'",
        )
        .fetch_one(&test_db.pool)
        .await
        .unwrap();
        let expected = 10.0 * (1.0 - EMA_ALPHA) + 20.0 * EMA_ALPHA;
        assert!(
            (ema2 - expected).abs() < 0.001,
            "second update: expected {expected}, got {ema2}"
        );
        assert_eq!(count2, 2);
    }
}
