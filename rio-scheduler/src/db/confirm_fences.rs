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
//! Scope note (disclosed at the work order; resweep merged_bug_011):
//! the fence covers the NOTHING-HELD answers — every keyed `Gone`
//! (live or confirm) and the confirm-only `NotYetReady` are
//! fence-written ahead of the reply. The MINTED exit-0 paths stay
//! OUTSIDE the fence, defended by the worker-abort close's transient
//! backoff window (the bug_282 `(Build, None)` arm holds fresh mints
//! until the window lapses), not by a fence row. Both residual
//! members of that family, named precisely:
//!   1. confirm-answers-Assignment: the confirm probe re-delivers a
//!      held attempt; the builder reports Cancelled and exits 0 — the
//!      report's consumption close settles the attempt.
//!   2. report-acked exits: a built/failed report is acked and the
//!      pod exits 0 — the ack itself proves the consumption close.
//!
//! Fencing those inside the report consumption tx (total
//! fence-over-all-exit-0) is deliberately out: hot-path transaction
//! risk for a window-defended residual (RULED S4-OQ4, accepted scope
//! note — flagged for adversarial review to challenge).

use super::SchedulerDb;

/// Fence rows older than this are garbage: any straggler pull has
/// long since timed out (client deadlines are seconds; the actor
/// mailbox holds nothing for hours). Swept by the attempt-ledger
/// housekeeping tick's rider ([`SchedulerDb::gc_confirm_fences`]).
pub(crate) const CONFIRM_FENCE_GC_SECS: f64 = 24.0 * 3600.0;

/// Proof that THIS pull observed a durable fence row for its token —
/// either by writing one ([`SchedulerDb::insert_confirm_fence`]) or by
/// reading one ([`SchedulerDb::confirm_fence_exists`]). Private unit
/// payload: the only constructors are those two functions, so the
/// keyed `Gone` license (`GoneLicense::Fenced`) cannot be built
/// without the fence on disk — an unfenced keyed clean exit does not
/// typecheck (merged_bug_011, keystone 1).
#[derive(Debug)]
#[must_use = "a fence witness exists to license a Gone answer"]
pub struct ConfirmFenceDurable(());

impl SchedulerDb {
    /// Durably record the exit-0 license for one executor token
    /// (idempotent: re-confirms upsert nothing). MUST complete before
    /// the licensing reply is sent — the write-ahead half of the
    /// fence. Returns the durable-fence witness.
    pub(crate) async fn insert_confirm_fence(
        &self,
        executor_token_sha256: &str,
        intent_id: &str,
    ) -> Result<ConfirmFenceDurable, sqlx::Error> {
        sqlx::query(
            "INSERT INTO executor_confirm_fences (executor_token_sha256, intent_id) \
             VALUES ($1, $2) \
             ON CONFLICT (executor_token_sha256) DO NOTHING",
        )
        .bind(executor_token_sha256)
        .bind(intent_id)
        .execute(&self.pool)
        .await?;
        Ok(ConfirmFenceDurable(()))
    }

    /// Whether this executor token has declared its exit (the
    /// `DeliverNew` screen's read): `Some(witness)` when the fence
    /// row exists — the witness licenses the screen's `Gone` answer.
    pub(crate) async fn confirm_fence_exists(
        &self,
        executor_token_sha256: &str,
    ) -> Result<Option<ConfirmFenceDurable>, sqlx::Error> {
        let row: Option<(i32,)> = sqlx::query_as(
            "SELECT 1 FROM executor_confirm_fences WHERE executor_token_sha256 = $1",
        )
        .bind(executor_token_sha256)
        .fetch_optional(&self.pool)
        .await?;
        Ok(row.map(|_| ConfirmFenceDurable(())))
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
