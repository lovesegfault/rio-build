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
            _ => "",
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

    /// Insert a new derivation, returning the assigned derivation_id as a string.
    /// Uses ON CONFLICT to handle deduplication by drv_hash.
    pub async fn upsert_derivation(
        &self,
        drv_hash: &str,
        drv_path: &str,
        pname: Option<&str>,
        system: &str,
        status: DerivationStatus,
        required_features: &[String],
    ) -> Result<Uuid, sqlx::Error> {
        let row: (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO derivations (drv_hash, drv_path, pname, system, status, required_features)
            VALUES ($1, $2, $3, $4, $5, $6)
            ON CONFLICT (drv_hash) DO UPDATE SET updated_at = now()
            RETURNING derivation_id
            "#,
        )
        .bind(drv_hash)
        .bind(drv_path)
        .bind(pname)
        .bind(system)
        .bind(status.as_str())
        .bind(required_features)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

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
    // Edge operations
    // -----------------------------------------------------------------------

    /// Insert a derivation edge.
    pub async fn insert_edge(&self, parent_id: Uuid, child_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO derivation_edges (parent_id, child_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(parent_id)
        .bind(child_id)
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
    #[allow(clippy::type_complexity)]
    pub async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[(
            String,
            String,
            Option<String>,
            String,
            DerivationStatus,
            Vec<String>,
        )],
    ) -> Result<HashMap<String, Uuid>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        let mut qb = QueryBuilder::new(
            "INSERT INTO derivations (drv_hash, drv_path, pname, system, status, required_features) ",
        );
        qb.push_values(rows, |mut b, (hash, path, pname, sys, status, feats)| {
            b.push_bind(hash)
                .push_bind(path)
                .push_bind(pname)
                .push_bind(sys)
                .push_bind(status.as_str())
                .push_bind(feats);
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
}
