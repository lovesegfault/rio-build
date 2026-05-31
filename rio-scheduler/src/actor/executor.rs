//! Requeue chokepoint for derivations whose executor is gone.
//!
//! The stream-era session plumbing that used to live here
//! (connect/disconnect/register, heartbeat reconcile, the per-executor
//! drain) was deleted with the executors map and the operator surfaces
//! that were its last readers: pull-mode executors hold no
//! scheduler-side connection state, per-executor drain has no
//! scheduler-side object (Job/pool-level draining is the successor),
//! and liveness belongs to kubelet/Job + `activeDeadlineSeconds`.
//! What remains is the reset-to-Ready requeue path shared by the
//! pull-attempt verdict arms and the establishment sweep. Periodic
//! `tick_*` housekeeping lives in [`super::housekeeping`].

use tracing::{info, warn};
use uuid::Uuid;

use crate::state::{DerivationStatus, DrvHash, ExecutorId};

use super::DagActor;

impl DagActor {
    /// E5's poison-threshold re-check as a `decide()` caller (Phase 1b,
    /// T-1b.8): fold the derivation's durable attempt suffix and report
    /// whether the verdict is a threshold poison. Read-only — this site
    /// appends nothing and charges nothing for the disconnect itself
    /// (the no-report establishment charge lands at the TTL sweep /
    /// backstop, T-1b.11, not here); the disconnect / force-drain /
    /// backstop rows it folds over were appended by their observation
    /// sites before this runs.
    ///
    /// Acts only on the threshold reason — exactly the check
    /// `PoisonConfig::is_poisoned` performed over the RAM counters —
    /// so the re-check stays a *threshold* re-check; every other budget
    /// is owned by its observation site's own collapsed verdict.
    ///
    /// On a read failure (or an uncommitted merge with no derivations
    /// row) the in-memory attempt history — the committed-suffix
    /// mirror — is the fallback fold input, so the re-check never goes
    /// silent on a PG blip (the as-built check was RAM-only and never
    /// touched PG).
    async fn reassign_threshold_recheck(&self, drv_hash: &DrvHash) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        let budget = self.decision_budget();
        let now = crate::db::attempts::epoch_now() as crate::retry_policy::AbsTime;
        let decision = match state.db_id {
            Some(derivation_id) => {
                let read: Result<crate::retry_policy::Decision, sqlx::Error> = async {
                    let mut conn = self.db.pool().acquire().await?;
                    let suffix = crate::db::SchedulerDb::load_attempt_suffix_one_in_tx(
                        &mut conn,
                        derivation_id,
                    )
                    .await?;
                    let history: Vec<crate::state::AttemptRecord> = suffix
                        .iter()
                        .map(crate::db::attempts::AttemptRow::to_record)
                        .collect();
                    Ok(crate::retry_policy::decide(&history, &budget, now))
                }
                .await;
                match read {
                    Ok(decision) => decision,
                    Err(e) => {
                        warn!(drv_hash = %drv_hash, error = %e,
                              "reassign re-check: suffix read failed; folding the in-memory \
                               attempt history instead");
                        crate::retry_policy::decide(state.attempt_history(), &budget, now)
                    }
                }
            }
            None => crate::retry_policy::decide(state.attempt_history(), &budget, now),
        };
        matches!(
            decision.verdict,
            crate::retry_policy::Verdict::Poison(crate::retry_policy::PoisonReason::Threshold)
        )
    }

    /// Reset a set of derivations to Ready and re-enqueue.
    ///
    /// The requeue chokepoint for "the executor that held this work is
    /// gone and the work is still wanted": the pull-attempt verdict
    /// arms (uncharged synthesized closes, infra/transient retries) and
    /// the establishment sweep all converge here.
    ///
    /// Leader-gated at the top, so it is the single chokepoint keeping
    /// a deposed leader from writing poison/Ready/terminal-log state
    /// from a stale DAG (r[sched.lease.standby-drops-writes+2]).
    ///
    /// `reset_to_ready()` handles both Assigned → Ready and Running →
    /// Failed → Ready. A derivation in any other state (Completed,
    /// Poisoned, DepFailed) is skipped with a warn — a stale caller
    /// (e.g. an establishment racing a late report) can produce it.
    ///
    /// A requeue through here does NOT bump `resource_floor` and does
    /// NOT record into `failed_builders`/`failure_count`/`retry_count`:
    /// the callers own their charge decisions (the attempt ledger row
    /// is appended at the observation site), and a bare "executor gone"
    /// is not a sizing signal — only the explicit OOM/disk-pressure
    /// classifications promote, at their own call sites.
    ///
    /// `lost_worker`: kept for the existing-poison-state check (3 prior
    /// REAL failures + 1 loss → poison instead of dispatching a 4th
    /// time) and for logging.
    // r[impl sched.reassign.no-promote-on-ephemeral-disconnect+5]
    pub(super) async fn reassign_derivations(
        &mut self,
        drv_hashes: &[DrvHash],
        lost_worker: Option<&ExecutorId>,
    ) {
        // r[impl sched.lease.standby-drops-writes+2]
        // Same defense-in-depth as the ProcessCompletion/CancelBuild arm
        // gates (mod.rs). A deposed leader processing a stale loss
        // against its stale DAG would otherwise:
        //   - poison branch: persist_poisoned + terminal_failure_epilogue
        //     → terminal_log_epilogue, which stamps the drv_executions row and
        //     pins the write-once build_derivations.exec_id for an execution
        //     the new leader is about to re-run (and, keep_going=false,
        //     fails/cancels the whole build from stale state);
        //   - reset branch: persist_status(Ready), racing the new leader's
        //     recovery.
        // The new leader's recovery + reconcile/orphan sweeps own the lost
        // worker's derivations; in-memory state here is cleared by LeaderLost.
        if !self.leader.is_leader() {
            if !drv_hashes.is_empty() {
                warn!(
                    drvs = drv_hashes.len(),
                    lost_worker = ?lost_worker,
                    "dropping reassign_derivations: not leader \
                     (new leader's recovery owns these derivations)"
                );
            }
            return;
        }
        let mut affected: std::collections::HashSet<Uuid> = Default::default();
        for drv_hash in drv_hashes {
            // Re-read existing poison state so 3 prior REAL failures
            // (recorded by handle_transient_failure) + this disconnect
            // → poison instead of dispatching a 4th time. Disconnect
            // itself never increments the count.
            //
            // E5, collapsed onto decide() (Phase 1b, T-1b.8): the
            // threshold re-check folds the durable attempt suffix
            // instead of reading the RAM
            // counters; verdict-identical on every single-tenure history
            // reachable today. Kept rather than deleted (decision P2,
            // the narrowed b09c5b312-X6 disposition): the backstop's
            // poison verdict no longer depends on this check since the
            // E8 collapse decides at its own site, but it remains the
            // requeue-time re-poison path and the post-failover
            // backstop for a lost persist_poisoned write.
            let should_poison = self.reassign_threshold_recheck(drv_hash).await;
            if should_poison {
                info!(drv_hash = %drv_hash, lost_worker = ?lost_worker,
                      "reassign: poison threshold reached, poisoning instead of retry");
                self.poison_and_cascade(
                    drv_hash,
                    "poison threshold reached on worker loss after prior failures",
                    None,
                    None,
                )
                .await;
                continue;
            }

            // A requeue does NOT bump `resource_floor` — only the
            // explicit OOM/disk-pressure classifications are sizing
            // signals, and they promote at their own call sites.
            if let Some(state) = self.dag.node_mut(drv_hash) {
                if let Err(e) = state.reset_to_ready() {
                    warn!(
                        drv_hash = %drv_hash, error = %e,
                        "invalid state for reassignment, skipping"
                    );
                    continue;
                }
                self.persist_status(drv_hash, DerivationStatus::Ready, None)
                    .await;
                affected.extend(self.get_interested_builds(drv_hash));
                self.push_ready(drv_hash.clone());
            }
        }
        // Dashboard: running count dropped; assigned_executors lost
        // this worker. Without emit_progress here, a quiet build shows
        // stale state until the next unrelated completion. Done at the
        // chokepoint so every caller (verdict requeue, establishment)
        // gets it for free and future callers can't repeat the
        // omission. `poison_and_cascade` emits its own events; only the
        // reset-to-Ready arm needs the explicit emit.
        for build_id in affected {
            self.emit_progress(build_id);
        }
    }
}
