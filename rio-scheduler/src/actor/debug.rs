//! `cfg(test)` debug handlers + DAG injection helpers on [`DagActor`].
//! These bypass the state machine / dispatch path so tests can set up
//! preconditions directly.

#![cfg(test)]

use std::time::Instant;

use uuid::Uuid;

use crate::state::{DerivationStatus, ExecutorId};

use super::{DagActor, DebugCmd, DebugDerivationInfo};

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
    /// Dispatch a `cfg(test)` [`DebugCmd`].
    pub(super) fn handle_debug(&mut self, d: DebugCmd) {
        match d {
            DebugCmd::QueryDerivation { drv_hash, reply } => {
                let _ = reply.send(self.handle_debug_query_derivation(&drv_hash));
            }
            DebugCmd::ForceAssign {
                drv_hash,
                executor_id,
                reply,
            } => {
                let _ = reply.send(self.handle_debug_force_assign(&drv_hash, &executor_id));
            }
            DebugCmd::SetRunningBuild {
                executor_id,
                drv_hash,
                reply,
            } => {
                let _ = reply.send(self.handle_debug_set_running_build(&executor_id, &drv_hash));
            }
            DebugCmd::BackdateRunning {
                drv_hash,
                secs_ago,
                reply,
            } => {
                let _ = reply.send(self.handle_debug_backdate_running(&drv_hash, secs_ago));
            }
            DebugCmd::BackdateSubmitted {
                build_id,
                secs_ago,
                reply,
            } => {
                let _ = reply.send(self.handle_debug_backdate_submitted(build_id, secs_ago));
            }
            DebugCmd::ForcePoisoned {
                drv_hash,
                resubmit_cycles,
                reply,
            } => {
                let _ = reply.send(self.handle_debug_force_poisoned(&drv_hash, resubmit_cycles));
            }
            DebugCmd::ForceStatus {
                drv_hash,
                status,
                reply,
            } => {
                let ok = self.dag.node_mut(&drv_hash).is_some_and(|s| {
                    s.set_status_for_test(status);
                    true
                });
                let _ = reply.send(ok);
            }
            DebugCmd::SetOutputPaths {
                drv_hash,
                paths,
                reply,
            } => {
                let ok = self.dag.node_mut(&drv_hash).is_some_and(|s| {
                    s.output_paths = paths;
                    true
                });
                let _ = reply.send(ok);
            }
            DebugCmd::SetTopdownPruned {
                drv_hash,
                value,
                reply,
            } => {
                let ok = self.dag.node_mut(&drv_hash).is_some_and(|s| {
                    s.topdown_pruned = value;
                    true
                });
                let _ = reply.send(ok);
            }
            DebugCmd::ClearDrvContent { drv_hash, reply } => {
                let _ = reply.send(self.handle_debug_clear_drv_content(&drv_hash));
            }
            DebugCmd::TripBreaker { n, reply } => {
                let _ = reply.send(self.handle_debug_trip_breaker(n));
            }
            DebugCmd::BackdateHeartbeat {
                executor_id,
                secs_ago,
                reply,
            } => {
                let ok = self.executors.get_mut(&executor_id).is_some_and(|w| {
                    w.last_heartbeat = backdate(secs_ago);
                    true
                });
                let _ = reply.send(ok);
            }
            DebugCmd::SeedHwTable { factors, reply } => {
                self.sla_estimator
                    .seed_hw(crate::sla::hw::HwTable::from_map(factors));
                let _ = reply.send(());
            }
            DebugCmd::SwapDb { pool, reply } => {
                self.db = crate::db::SchedulerDb::new(pool);
                let _ = reply.send(());
            }
            DebugCmd::Counters { reply } => {
                let _ = reply.send(super::TestCountersSnapshot {
                    substitute_sem_permits: self.substitute_sem.available_permits(),
                    ..self.test_counters.snapshot()
                });
            }
            DebugCmd::SeedSchedHint {
                drv_hash,
                est_memory_bytes,
                est_disk_bytes,
                est_deadline_secs,
                floor,
                reply,
            } => {
                let ok = self.dag.node_mut(&drv_hash).is_some_and(|s| {
                    // Builder-style: any seeded field materializes a
                    // last_intent (other dimensions zero).
                    if est_memory_bytes.is_some()
                        || est_disk_bytes.is_some()
                        || est_deadline_secs.is_some()
                    {
                        let i = s.sched.last_intent.get_or_insert_with(Default::default);
                        if let Some(v) = est_memory_bytes {
                            i.mem_bytes = v;
                        }
                        if let Some(v) = est_disk_bytes {
                            i.disk_bytes = v;
                        }
                        if let Some(v) = est_deadline_secs {
                            i.deadline_secs = v;
                        }
                    }
                    if let Some(f) = floor {
                        s.sched.resource_floor = f;
                    }
                    true
                });
                let _ = reply.send(ok);
            }
        }
    }

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
            substitute_tried: s.substitute_tried,
            topdown_pruned: s.topdown_pruned,
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

    /// Set `worker.running_build = Some(drv_hash)` without any DAG-side
    /// transition. Unlike [`handle_debug_force_assign`], this does NOT
    /// require the drv to be in a transitionable state — for the
    /// heartbeat-reconcile safety-net test which needs a terminal
    /// (Poisoned) drv leaked into `running_build`.
    pub(super) fn handle_debug_set_running_build(
        &mut self,
        executor_id: &ExecutorId,
        drv_hash: &str,
    ) -> bool {
        let Some(w) = self.executors.get_mut(executor_id) else {
            return false;
        };
        w.running_build = Some(drv_hash.into());
        true
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

    /// Force a derivation into `Poisoned` with the given
    /// `resubmit_cycles`. For the I-169 resubmit-bound tests.
    pub(super) fn handle_debug_force_poisoned(
        &mut self,
        drv_hash: &str,
        resubmit_cycles: u32,
    ) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return false;
        };
        state.set_status_for_test(DerivationStatus::Poisoned);
        state.retry.resubmit_cycles = resubmit_cycles;
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
    /// `resource_floor=zeros`; callers override via struct-update on
    /// [`crate::db::RecoveryDerivationRow::test_default`].
    pub(crate) fn test_inject_ready_row(&mut self, row: crate::db::RecoveryDerivationRow) {
        let hash = row.drv_hash.clone();
        let state = crate::state::DerivationState::from_recovery_row(row, DerivationStatus::Ready)
            .expect("test_drv_path generates valid StorePath");
        self.dag.insert_recovered_node(state);
        // r[sched.admin.spawn-intents.probed-gate+2]: injected nodes
        // model "probed, not substitutable" so existing
        // compute_spawn_intents filter tests stay focused on
        // kind/system/feature logic. Reset via
        // [`Self::test_set_probed_generation`] to model the
        // SubstituteComplete cascade window.
        self.dag
            .node_mut(&hash)
            .expect("just inserted")
            .probed_generation = 1;
    }

    /// Set `sched.priority` on an already-injected node. For
    /// `compute_spawn_intents` priority-sort assertions — recovery
    /// rows default to priority 0.0.
    pub(crate) fn test_set_priority(&mut self, hash: &str, p: f64) {
        self.dag.node_mut(hash).expect("node exists").sched.priority = p;
    }

    /// Set `probed_generation` on an already-injected node. For
    /// `r[sched.admin.spawn-intents.probed-gate]` tests —
    /// [`Self::test_inject_ready_row`] stamps `1` so injected nodes are
    /// intent-eligible; reset to `0` to model the SubstituteComplete
    /// cascade window (Ready, not yet probed).
    pub(crate) fn test_set_probed_generation(&mut self, hash: &str, g: u64) {
        self.dag
            .node_mut(hash)
            .expect("node exists")
            .probed_generation = g;
    }

    /// Inject a Ready derivation with default fields. `is_fod` sets
    /// `is_fixed_output` so the kind boundary in `hard_filter` /
    /// `compute_spawn_intents` routes it to fetcher executors.
    pub(crate) fn test_inject_ready(
        &mut self,
        hash: &str,
        pname: Option<&str>,
        system: &str,
        is_fod: bool,
    ) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            pname: pname.map(String::from),
            is_fixed_output: is_fod,
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

    /// Inject a derivation directly at the given `status`. Same row
    /// defaults as [`Self::test_inject_ready`]; bypasses transition
    /// validation. For DAG-shape tests (forecast frontier, dep walks)
    /// that need Queued/Running nodes without merge+dispatch.
    pub(crate) fn test_inject_at(&mut self, hash: &str, system: &str, status: DerivationStatus) {
        let state = crate::state::DerivationState::from_recovery_row(
            crate::db::RecoveryDerivationRow::test_default(hash, system),
            status,
        )
        .expect("test_drv_path generates valid StorePath");
        self.dag.insert_recovered_node(state);
        self.dag
            .node_mut(hash)
            .expect("just inserted")
            .probed_generation = 1;
    }

    /// Add a DAG edge: `parent` depends on `child`.
    pub(crate) fn test_inject_edge(&mut self, parent: &str, child: &str) {
        self.dag.insert_recovered_edge(parent.into(), child.into());
    }

    /// Set up a Running dep for forecast-ETA tests: `last_intent.
    /// predicted.wall_secs = t_ref` and `running_since = now -
    /// elapsed_secs`. `cores` is the dispatched core count
    /// (`SolvedIntent.cores`). The drv MUST already be in the DAG.
    pub(crate) fn test_set_running_eta(
        &mut self,
        hash: &str,
        t_ref_secs: f64,
        elapsed_secs: u64,
        cores: u32,
    ) {
        let state = self.dag.node_mut(hash).expect("node exists");
        state.set_status_for_test(DerivationStatus::Running);
        state.running_since = Some(backdate(elapsed_secs));
        state.sched.last_intent = Some(crate::state::SolvedIntent {
            cores,
            predicted: Some(crate::sla::solve::SlaPrediction {
                wall_secs: Some(t_ref_secs),
                ..Default::default()
            }),
            ..Default::default()
        });
    }

    /// Inject a Ready non-FOD with a given `resource_floor.{mem,disk}_bytes`.
    /// For the D4 solve_intent_for-clamps-at-floor test — bypasses the
    /// disconnect→`bump_floor_or_count` dance.
    pub(crate) fn test_inject_ready_with_floor(
        &mut self,
        hash: &str,
        system: &str,
        floor_mem_bytes: u64,
        floor_disk_bytes: u64,
    ) {
        self.test_inject_ready_row(crate::db::RecoveryDerivationRow {
            floor_mem_bytes: floor_mem_bytes as i64,
            floor_disk_bytes: floor_disk_bytes as i64,
            ..crate::db::RecoveryDerivationRow::test_default(hash, system)
        });
    }
}
