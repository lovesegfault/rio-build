//! `cfg(test)` debug handlers + DAG injection helpers on [`DagActor`].
//! These bypass the state machine / dispatch path so tests can set up
//! preconditions directly.

#![cfg(test)]

use std::time::Instant;

use uuid::Uuid;

use crate::state::{DerivationStatus, ExecutorId};

use super::{DagActor, DebugDerivationInfo};

/// Backdate an Instant by `secs_ago` seconds. `checked_sub` is used
/// defensively: if `secs_ago` is absurd (e.g. `u64::MAX`) and
/// `Instant::now()` can't represent that far back, clamp to "now"
/// (effectively 0 elapsed). Tokio paused time can't mock `Instant`
/// — this is why the DebugBackdate* handlers exist at all.
pub(super) fn backdate(secs_ago: u64) -> Instant {
    Instant::now()
        .checked_sub(std::time::Duration::from_secs(secs_ago))
        .unwrap_or_else(Instant::now)
}

impl DagActor {
    pub(super) fn handle_debug_query_derivation(
        &self,
        drv_hash: &str,
    ) -> Option<DebugDerivationInfo> {
        self.dag.node(drv_hash).map(|s| DebugDerivationInfo {
            status: s.status(),
            assigned_executor: s.assigned_executor.as_ref().map(|w| w.to_string()),
            output_paths: s.output_paths.clone(),
            retry: s.retry.clone(),
            ca: s.ca.clone(),
            sched: s.sched.clone(),
        })
    }

    /// Force Ready→Assigned (or Failed→Ready→Assigned) bypassing
    /// backoff + failed_builders exclusion. For retry/poison tests
    /// that need to drive multiple completion cycles without waiting
    /// for real backoff. Clears `backoff_until`.
    pub(super) fn handle_debug_force_assign(
        &mut self,
        drv_hash: &str,
        executor_id: &ExecutorId,
    ) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        // If not already Ready, try to get there. Assigned/Running →
        // reset_to_ready, Failed → transition Ready, Ready → no-op.
        let prepped = match state.status() {
            DerivationStatus::Ready => true,
            DerivationStatus::Assigned | DerivationStatus::Running => {
                state.reset_to_ready().is_ok()
            }
            DerivationStatus::Failed => state.transition(DerivationStatus::Ready).is_ok(),
            _ => false, // terminal or pre-Ready: can't force
        };
        if !prepped {
            return false;
        }
        state.retry.backoff_until = None;
        state.assigned_executor = Some(executor_id.clone());
        let assigned = state.transition(DerivationStatus::Assigned).is_ok();
        // Set worker's running build so subsequent complete_failure
        // finds a consistent state.
        if let Some(w) = self.executors.get_mut(executor_id) {
            w.running_build = Some(drv_hash.into());
        }
        assigned
    }

    /// Force to Running with `running_since` backdated. Used by
    /// backstop-timeout tests (`handle_tick` checks Running +
    /// `running_since > threshold`). The cfg(test) backstop floor is
    /// 0s so any `secs_ago > 0` triggers the backstop on Tick.
    pub(super) fn handle_debug_backdate_running(&mut self, drv_hash: &str, secs_ago: u64) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        // Transition to Running if not already there. Assigned →
        // Running is a valid transition; Ready/Created would fail
        // (need Assigned first). DebugForceAssign → Assigned, then
        // this → Running is the typical test sequence.
        let running = match state.status() {
            DerivationStatus::Running => true,
            DerivationStatus::Assigned => state.transition(DerivationStatus::Running).is_ok(),
            _ => false,
        };
        if running {
            state.running_since = Some(backdate(secs_ago));
        }
        running
    }

    /// Backdate `submitted_at` for per-build-timeout tests.
    /// `handle_tick`'s `r[sched.timeout.per-build]` check uses
    /// `submitted_at.elapsed()` — a `std::time::Instant`, which tokio
    /// paused time cannot mock (and paused time breaks PG pool
    /// timeouts anyway).
    pub(super) fn handle_debug_backdate_submitted(
        &mut self,
        build_id: Uuid,
        secs_ago: u64,
    ) -> bool {
        let Some(build) = self.builds.get_mut(&build_id) else {
            return false;
        };
        build.submitted_at = backdate(secs_ago);
        true
    }

    /// Force a derivation into `Poisoned` with the given `retry_count`.
    /// For the I-169 resubmit-bound tests.
    pub(super) fn handle_debug_force_poisoned(&mut self, drv_hash: &str, retry_count: u32) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        state.set_status_for_test(DerivationStatus::Poisoned);
        state.retry.count = retry_count;
        state.retry.poisoned_at = Some(std::time::Instant::now());
        true
    }

    /// Clear `drv_content` to simulate post-recovery state (DAG
    /// reloaded from PG, drv_content not persisted). For CA
    /// recovery-fetch tests.
    pub(super) fn handle_debug_clear_drv_content(&mut self, drv_hash: &str) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        state.drv_content.clear();
        true
    }

    /// Trip the cache-check circuit breaker directly. For CA
    /// cutoff-compare breaker-gate tests — bypasses the
    /// N-failing-SubmitBuild dance.
    pub(super) fn handle_debug_trip_breaker(&mut self, n: u32) -> bool {
        for _ in 0..n {
            let _ = self.cache_breaker.record_failure();
        }
        self.cache_breaker.is_open()
    }

    /// Inject a derivation directly into the DAG at `Ready` status.
    /// Bypasses MergeDag + PG persist. The row defaults are
    /// `is_fixed_output=false`, `required_features=[]`,
    /// `size_class_floor=None`; callers override via struct-update on
    /// [`crate::db::RecoveryDerivationRow::test_default`].
    pub(crate) fn test_inject_ready_row(&mut self, row: crate::db::RecoveryDerivationRow) {
        let state = crate::state::DerivationState::from_recovery_row(row, DerivationStatus::Ready)
            .expect("test_drv_path generates valid StorePath");
        self.dag.insert_recovered_node(state);
    }

    /// Inject a Ready non-FOD with default fields.
    pub(crate) fn test_inject_ready(&mut self, hash: &str, pname: Option<&str>, system: &str) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            pname: pname.map(String::from),
            ..crate::db::RecoveryDerivationRow::test_default(hash, system)
        });
    }

    /// Inject a Ready non-FOD with `required_features` populated.
    /// For the I-176 feature-filter snapshot tests.
    pub(crate) fn test_inject_ready_with_features(
        &mut self,
        hash: &str,
        pname: Option<&str>,
        system: &str,
        required_features: &[&str],
    ) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            pname: pname.map(String::from),
            required_features: required_features.iter().map(|s| s.to_string()).collect(),
            ..crate::db::RecoveryDerivationRow::test_default(hash, system)
        });
    }

    /// Inject a Ready non-FOD with a given `size_class_floor`. For the
    /// I-187 snapshot-honors-floor test — bypasses the
    /// disconnect→`promote_size_class_floor` dance.
    pub(crate) fn test_inject_ready_with_floor(
        &mut self,
        hash: &str,
        system: &str,
        floor: Option<&str>,
    ) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            size_class_floor: floor.map(String::from),
            ..crate::db::RecoveryDerivationRow::test_default(hash, system)
        });
    }

    /// Inject a Ready FOD with a given `size_class_floor`. For
    /// `compute_fod_size_class_snapshot` tests (P0556) — bypasses the
    /// merge+fail-to-promote dance.
    pub(crate) fn test_inject_ready_fod(&mut self, hash: &str, system: &str, floor: Option<&str>) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            is_fixed_output: true,
            size_class_floor: floor.map(String::from),
            ..crate::db::RecoveryDerivationRow::test_default(hash, system)
        });
    }

    /// Seed the Estimator directly, bypassing the Tick refresh path.
    /// Pairs with [`Self::test_inject_ready`].
    pub(crate) fn test_refresh_estimator(&mut self, rows: Vec<crate::db::BuildHistoryRow>) {
        self.estimator.refresh(rows);
    }
}
