//! Assignment CRUD — `assignments` table.

#[cfg(test)]
use uuid::Uuid;

#[cfg(test)]
use super::{AssignmentStatus, SchedulerDb};
#[cfg(test)]
use crate::state::ExecutorId;

/// TEST FIXTURES ONLY. The production assignment lifecycle writes
/// exclusively through the fenced capability: the pull mint
/// (`mint_pull_attempt_fenced`) creates/refreshes active rows and
/// [`super::FencedTx::close_assignment`] /
/// [`SchedulerDb::close_assignment_fenced`] are the only closers —
/// exec_id-scoped, never derivation_id-keyed. The unfenced writers
/// below exist solely so db tests can seed historical row shapes;
/// `cfg(test)` keeps them out of the production surface entirely
/// (enforced by `db/tests/fence_coverage.rs`).
#[cfg(test)]
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
        exec_id: Uuid,
    ) -> Result<Uuid, sqlx::Error> {
        let row: (Uuid,) = sqlx::query_as(
            r#"
            INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id)
            VALUES ($1, $2, $3, 'pending', $4)
            ON CONFLICT (derivation_id) WHERE status IN ('pending', 'acknowledged')
            DO UPDATE SET
                builder_id = EXCLUDED.builder_id,
                generation = EXCLUDED.generation,
                status = 'pending',
                assigned_at = now(),
                completed_at = NULL,
                exec_id = EXCLUDED.exec_id
            RETURNING assignment_id
            "#,
        )
        .bind(derivation_id)
        .bind(executor_id.as_str())
        .bind(generation)
        .bind(exec_id)
        .fetch_one(&self.pool)
        .await?;

        Ok(row.0)
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
