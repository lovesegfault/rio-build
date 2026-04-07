//! Assignment CRUD — `assignments` table.

use uuid::Uuid;

use super::{AssignmentStatus, SchedulerDb};
use crate::state::ExecutorId;

impl SchedulerDb {
    /// Create a new assignment record. Returns the assignment_id.
    ///
    /// Idempotent against the `assignments_active_uq` partial unique
    /// index: if an active (pending/acknowledged) assignment already
    /// exists for this derivation, the existing row is updated to the
    /// new worker/generation rather than erroring. This happens when
    /// the scheduler re-dispatches after a worker-reported failure
    /// before the completion handler has transitioned the prior
    /// assignment to a terminal status — a race, not a logic bug.
    pub async fn insert_assignment(
        &self,
        derivation_id: Uuid,
        executor_id: &ExecutorId,
        generation: i64,
    ) -> Result<Uuid, sqlx::Error> {
        let row: (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO assignments (derivation_id, builder_id, generation, status)
            VALUES ($1, $2, $3, 'pending')
            ON CONFLICT (derivation_id) WHERE status IN ('pending', 'acknowledged')
            DO UPDATE SET
                builder_id = EXCLUDED.builder_id,
                generation = EXCLUDED.generation,
                status = 'pending',
                assigned_at = now(),
                completed_at = NULL
            RETURNING assignment_id
            "#,
        )
        .bind(derivation_id)
        .bind(executor_id.as_str())
        .bind(generation)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
    }

    /// Delete the in-progress assignment for a derivation.
    /// Called from dispatch.rs on try_send failure to clean up the
    /// PG row that insert_assignment wrote (no worker ever got the
    /// assignment, so the row is misleading on recovery).
    ///
    /// Only deletes `pending`/`acknowledged` rows — terminal rows
    /// are audit-valuable even for stale derivations.
    pub async fn delete_latest_assignment(&self, derivation_id: Uuid) -> Result<(), sqlx::Error> {
        sqlx::query!(
            "DELETE FROM assignments \
             WHERE derivation_id = $1 AND status IN ('pending', 'acknowledged')",
            derivation_id,
        )
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Update an assignment status. Terminal statuses
    /// (`Completed`/`Failed`/`Cancelled`) also stamp
    /// `completed_at = now()`; `Pending` leaves it alone.
    pub async fn update_assignment_status(
        &self,
        derivation_id: Uuid,
        status: AssignmentStatus,
    ) -> Result<(), sqlx::Error> {
        match status {
            AssignmentStatus::Completed
            | AssignmentStatus::Failed
            | AssignmentStatus::Cancelled => {
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
            }
            AssignmentStatus::Pending => {
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
        }

        Ok(())
    }
}
