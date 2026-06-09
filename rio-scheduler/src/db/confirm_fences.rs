//! The confirm-exit fence (merged_bug_145, migration 097).
//!
//! One row per confirm-exited executor pod, keyed by the SHA-256 hex
//! of the raw executor token — the durable half of the builder's
//! exit-0 license. Written when a `confirm_only` pull is answered
//! "nothing held" (NotYetReady/Gone), BEFORE the reply (write-ahead:
//! no clean-exit answer without the fence on disk). Read by the
//! `DeliverNew` admission: a fenced token's mint is screened to Gone —
//! the late abandoned pull that would otherwise open an attempt
//! against a `Succeeded` Job (invisible to the establishment sweep,
//! which reaps against FAILED pods).
//!
//! Key provenance (merged_bug_078): the hash these functions accept is
//! minted ONLY by the gRPC credential layer's `ConfirmFenceKey` — the
//! SHA-256 of exactly the carrier bytes that VERIFIED, never of
//! whichever carrier was merely present — and reaches the actor
//! through the hash-or-nothing command conduit. No other layer sees
//! raw token bytes, so no other derivation site exists.
//!
//! Scope note (disclosed at the work order): the fence covers the
//! nothing-held confirm path. The minted-confirm path (the confirm
//! answers Assignment → the builder reports Cancelled → exits 0) is
//! defended by the worker-abort close's transient backoff window (the
//! bug_282 (Build, None) arm holds fresh mints until the window
//! lapses), not by this fence.

use super::SchedulerDb;

/// Fence rows older than this are garbage: any straggler pull has
/// long since timed out (client deadlines are seconds; the actor
/// mailbox holds nothing for hours). Swept by the attempt-ledger
/// housekeeping tick's rider ([`SchedulerDb::gc_confirm_fences`]).
pub(crate) const CONFIRM_FENCE_GC_SECS: f64 = 24.0 * 3600.0;

impl SchedulerDb {
    /// Durably record the exit-0 license for one executor token
    /// (idempotent: re-confirms upsert nothing). MUST complete before
    /// the confirm reply is sent — the write-ahead half of the fence.
    pub(crate) async fn insert_confirm_fence(
        &self,
        executor_token_sha256: &str,
        intent_id: &str,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            "INSERT INTO executor_confirm_fences (executor_token_sha256, intent_id) \
             VALUES ($1, $2) \
             ON CONFLICT (executor_token_sha256) DO NOTHING",
        )
        .bind(executor_token_sha256)
        .bind(intent_id)
        .execute(&self.pool)
        .await?;
        Ok(())
    }

    /// Whether this executor token has declared its exit (the
    /// `DeliverNew` screen's read).
    pub(crate) async fn confirm_fence_exists(
        &self,
        executor_token_sha256: &str,
    ) -> Result<bool, sqlx::Error> {
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM executor_confirm_fences WHERE executor_token_sha256 = $1",
        )
        .bind(executor_token_sha256)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.is_some())
    }

    /// Delete fences older than `horizon_secs` (the housekeeping
    /// rider). Returns rows deleted.
    pub(crate) async fn gc_confirm_fences(
        &self,
        horizon_secs: f64,
        batch: i64,
    ) -> Result<u64, sqlx::Error> {
        let result = sqlx::query(
            "DELETE FROM executor_confirm_fences \
             WHERE executor_token_sha256 IN ( \
                 SELECT executor_token_sha256 FROM executor_confirm_fences \
                 WHERE confirmed_at < now() - make_interval(secs => $1) \
                 LIMIT $2)",
        )
        .bind(horizon_secs)
        .bind(batch)
        .execute(&self.pool)
        .await?;
        Ok(result.rows_affected())
    }
}
