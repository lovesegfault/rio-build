//! Worker selection.
//!
//! `best_executor()`: hard-filter on system/features/kind, then the
//! warm-gate (prefer workers that ACKed PrefetchComplete). With one-shot
//! pods, all candidates that pass the filter are equivalent — no
//! per-worker locality state to discriminate on. First match wins.

use std::collections::HashMap;

use serde::{Deserialize, Serialize};

use rio_proto::types::ExecutorKind;

use crate::dag::DerivationDag;
use crate::state::{DerivationState, DrvHash, ExecutorId, ExecutorState};

/// One soft-feature entry (`[[soft_features]]` in scheduler.toml).
/// `name` is stripped from `requiredSystemFeatures` at DAG-insert
/// (`r[sched.dispatch.soft-features]`).
///
/// D6: legacy `floor_hint` removed — SLA `solve_intent_for` +
/// `resource_floor` doubling own initial sizing now; the I-213 firefox
/// case (`big-parallel` → xlarge) is handled by the SLA model fitting on
/// prior firefox samples + reactive doubling on cold start.
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(deny_unknown_fields)]
pub struct SoftFeature {
    pub name: String,
}

/// First clause of [`hard_filter`] that rejects, or `None` if it
/// accepts. Used by `InspectBuildDag` to answer "why is this Ready drv
/// not dispatching?" without a trace build (I-062: 4 stuck-FOD
/// recurrences with every visible field clean — the rejection was in
/// something the diagnostic API didn't expose).
///
/// MUST mirror [`hard_filter`]'s clauses exactly; `hard_filter` is
/// implemented as `rejection_reason(..).is_none()` so the two cannot
/// drift. The reason strings are stable identifiers (CLI/dashboard
/// match on them), not prose — keep them terse.
pub fn rejection_reason(w: &ExecutorState, drv: &DerivationState) -> Option<&'static str> {
    if drv.is_fixed_output != (w.kind == ExecutorKind::Fetcher) {
        return Some("kind-mismatch");
    }
    // has_capacity()'s constituents, individually so the reason names
    // the SPECIFIC gate. Order matches has_capacity()'s short-circuit.
    if w.is_draining() {
        return Some("draining");
    }
    if w.store_degraded {
        return Some("store-degraded");
    }
    if !w.is_registered() {
        // stream_tx.is_none() OR systems.is_empty(). The diagnostic
        // already exposes has_stream and systems separately; one
        // reason here is fine since either being false means "not
        // ready to dispatch to".
        return Some("not-registered");
    }
    // I-095: stream_tx is Some but the receiver was dropped (gRPC
    // stream gone). ExecutorDisconnected is queued in the mailbox
    // but dispatch_ready may run first — without this gate, every
    // ready drv in the pass picks this executor, try_send fails
    // "channel closed", drv resets to Ready, next drv picks it
    // again. Observed: 100+ WARN/tick, actor saturated, RPCs time
    // out. Distinct from not-registered so the gauge accounting in
    // handle_executor_disconnected (which keys on is_registered())
    // stays correct.
    if w.stream_tx.as_ref().is_some_and(|tx| tx.is_closed()) {
        return Some("stream-closed");
    }
    if w.running_build.is_some() {
        return Some("at-capacity");
    }
    // r[impl sched.sla.intent-match]
    // ADR-023: a worker spawned for intent X (= drv_hash X) is
    // RESERVED for X. Without this, the FIFO drain pops Y first, finds
    // this worker via the regular best_executor walk, and gives it Y —
    // its pod resources were sized for X, so Y likely OOMs or wastes
    // headroom. Stale intents (X not Ready) are cleared at heartbeat
    // (`handle_heartbeat`), so a worker with `Some(intent_id)` here
    // always has a live reservation.
    if w.intent_id
        .as_deref()
        .is_some_and(|id| id != drv.drv_hash.as_ref())
    {
        return Some("intent-reserved");
    }
    // System: any-match (multi-arch executor); features: all-match.
    if !w.systems.iter().any(|s| s == &drv.system) {
        return Some("system-mismatch");
    }
    if !drv
        .required_features
        .iter()
        .all(|f| w.supported_features.contains(f))
    {
        return Some("feature-missing");
    }
    if drv.retry.failed_builders.contains(&w.executor_id) {
        return Some("failed-on");
    }
    if let Some(intent) = &drv.sched.last_intent
        && let Some(r) = &w.last_resources
        && r.memory_total_bytes != 0
        && r.memory_total_bytes < intent.mem_bytes
    {
        return Some("resource-fit");
    }
    None
}

/// Hard filter: executor-kind, capacity, system/feature match,
/// not-previously-failed, resource-fit. The SAME predicate that both
/// warm-pass and cold-fallback use in [`best_executor`] — extracted so
/// the two passes can't drift. Implemented via [`rejection_reason`] so
/// the diagnostic and the filter cannot diverge.
fn hard_filter(w: &ExecutorState, drv: &DerivationState) -> bool {
    // r[impl sched.dispatch.fod-to-fetcher]
    // r[impl sched.assign.resource-fit]
    // Clause-by-clause logic lives in `rejection_reason` (above) so the
    // dispatch path and the diagnostic cannot drift. The kind gate is
    // first (cheap boolean, airgap boundary: a builder NEVER sees a
    // FOD, a fetcher NEVER sees arbitrary code). Resource-fit: three
    // unknown-ceiling cases (est=None, no heartbeat resources,
    // memory_total_bytes==0 = "unknown" per builder cgroup.rs:667)
    // all pass.
    rejection_reason(w, drv).is_none()
}

/// Select the best worker for a derivation.
///
/// Hard filter first (kind, capacity, system/feature, resource-fit),
/// then score the survivors, return the lowest. `None` if nobody passes
/// filter — caller defers the derivation.
pub fn best_executor(
    workers: &HashMap<ExecutorId, ExecutorState>,
    drv: &DerivationState,
) -> Option<ExecutorId> {
    // r[impl sched.assign.warm-gate]
    // --- Hard filter: warm-gate two-pass ---
    // First pass: warm workers only. This is the normal path once a
    // worker has ACKed its initial PrefetchHint (PrefetchComplete).
    let warm_candidates: Vec<&ExecutorState> = workers
        .values()
        .filter(|w| w.warm && hard_filter(w, drv))
        .collect();

    // Fallback: no warm worker passes → relax the gate. Single-
    // worker clusters, mass scale-up where ALL workers are fresh —
    // can't deadlock. The gate is an optimization, not a correctness
    // constraint. Cold workers still go through the SAME hard_filter
    // (capacity/features/kind) — we're only relaxing `warm`.
    let candidates: Vec<&ExecutorState> = if !warm_candidates.is_empty() {
        warm_candidates
    } else {
        let cold: Vec<&ExecutorState> = workers.values().filter(|w| hard_filter(w, drv)).collect();
        if !cold.is_empty() {
            tracing::debug!(
                cold_count = cold.len(),
                drv_system = %drv.system,
                "warm-gate fallback: no warm worker passes filter, dispatching cold"
            );
            metrics::counter!("rio_scheduler_warm_gate_fallback_total").increment(1);
        }
        cold
    };

    if candidates.is_empty() {
        return None;
    }

    // All candidates passed hard_filter + warm-gate. One-shot workers
    // are equivalent at this point (no per-worker locality state); pick
    // the first. HashMap iteration order is effectively random.
    Some(candidates[0].executor_id.clone())
}

/// Approximate input closure: the derivation's DAG children's
/// expected output paths.
///
/// This is what the derivation NEEDS as inputs — its dependencies'
/// outputs. Not perfect (misses `input_srcs` and transitive closure),
/// but covers the bulk of what the worker's FUSE will actually fetch.
///
/// Used by dispatch.rs for [`PrefetchHint`] — tell the chosen worker
/// to warm these before the build starts.
///
/// Cheap: DAG iteration only, no store RPCs, no ATerm parse. The
/// scheduler has all this state in memory already (populated at
/// merge time). For a derivation with 20 dependencies each with
/// 2 outputs: 40 string clones, ~1μs.
///
/// [`PrefetchHint`]: rio_proto::types::PrefetchHint
pub(crate) fn approx_input_closure(dag: &DerivationDag, drv_hash: &DrvHash) -> Vec<String> {
    dag.get_children(drv_hash)
        .into_iter()
        .filter_map(|child| dag.node(&child))
        .flat_map(|child| {
            // Prefer REALIZED output_paths (populated at completion time
            // from the worker's BuildResult.built_outputs) over
            // expected_output_paths (populated at merge time from the
            // proto). For a floating-CA child, expected_output_paths is
            // `[""]` (the path is unknown pre-build) but output_paths
            // has the actual realized path once the child completes.
            // For IA children, expected_output_paths is correct and
            // output_paths is empty until completion — fall through.
            if child.output_paths.is_empty() {
                child.expected_output_paths.iter()
            } else {
                child.output_paths.iter()
            }
        })
        // Filter empties: a floating-CA child that hasn't completed yet
        // has expected_output_paths=[""] and output_paths=[]. The ""
        // would be a no-op PrefetchHint entry; cleaner to drop it here.
        .filter(|p| !p.is_empty())
        .cloned()
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::state::SolvedIntent;
    use rio_test_support::fixtures::make_derivation_node;

    fn make_worker(id: &str, running: u32) -> ExecutorState {
        let mut w = ExecutorState::new(id.into());
        w.systems = vec!["x86_64-linux".into()];
        // running>0 → Some (busy), running==0 → None (idle).
        w.running_build = (running > 0).then(|| "run-0".into());
        // Tests default to warm — warm-gate coverage lives in the
        // dedicated tests below that flip `warm=false` explicitly.
        w.warm = true;
        // has_capacity needs an OPEN stream_tx (I-095: is_closed()
        // is now part of the gate). forget(rx) keeps the channel
        // open for the test's lifetime — a few dozen tiny receivers
        // leaked per test process; nextest runs each test in its own
        // process so they don't accumulate.
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        std::mem::forget(rx);
        w.stream_tx = Some(tx);
        w
    }

    /// Like `make_worker` but with a CLOSED stream_tx — the receiver
    /// is dropped, so `tx.is_closed()` is true. Models the window
    /// between gRPC stream drop and ExecutorDisconnected processing.
    fn make_worker_closed_stream(id: &str) -> ExecutorState {
        let mut w = make_worker(id, 0);
        let (tx, rx) = tokio::sync::mpsc::channel(1);
        drop(rx);
        w.stream_tx = Some(tx);
        w
    }

    fn make_drv() -> DerivationState {
        DerivationState::try_from_node(&make_derivation_node("test-drv", "x86_64-linux").into())
            .unwrap()
    }

    fn workers_map(ws: Vec<ExecutorState>) -> HashMap<ExecutorId, ExecutorState> {
        ws.into_iter().map(|w| (w.executor_id.clone(), w)).collect()
    }

    // r[verify sched.dispatch.fod-to-fetcher]
    /// 4-cell matrix: `is_fixed_output × executor.kind`. The XOR check
    /// in `hard_filter` must reject both cross-kind routings. The
    /// `true×Builder` rejection is the airgap boundary: even an idle
    /// builder fails the filter, so there's no FOD→builder path under
    /// any load condition.
    #[test]
    fn hard_filter_kind_matrix() {
        let mk_worker = |kind| {
            let mut w = make_worker("e", 0);
            w.kind = kind;
            w
        };
        let mk_drv = |is_fod| {
            let mut d = make_drv();
            d.is_fixed_output = is_fod;
            d
        };

        // FOD → Fetcher: passes (rest of filter permitting)
        assert!(
            hard_filter(&mk_worker(ExecutorKind::Fetcher), &mk_drv(true)),
            "FOD must route to fetcher"
        );
        // FOD → Builder: rejected (airgap boundary)
        assert!(
            !hard_filter(&mk_worker(ExecutorKind::Builder), &mk_drv(true)),
            "FOD must NOT route to builder (airgap)"
        );
        // non-FOD → Fetcher: rejected (arbitrary code on open egress)
        assert!(
            !hard_filter(&mk_worker(ExecutorKind::Fetcher), &mk_drv(false)),
            "non-FOD must NOT route to fetcher"
        );
        // non-FOD → Builder: passes (rest of filter permitting)
        assert!(
            hard_filter(&mk_worker(ExecutorKind::Builder), &mk_drv(false)),
            "non-FOD must route to builder"
        );
    }

    // r[verify sched.dispatch.fod-builtin-any-arch]
    /// `system="builtin"` FOD matches a fetcher of EITHER arch
    /// (every executor appends `"builtin"` to its advertised
    /// `systems` at startup). An arch-specific FOD (`system=
    /// "x86_64-linux"`, e.g. `pkgs.fetchgit` inheriting stdenv's
    /// system) matches only fetchers advertising that arch.
    #[test]
    fn hard_filter_builtin_matches_any_fetcher() {
        let mk_fetcher = |systems: &[&str]| {
            let mut w = make_worker("f", 0);
            w.kind = ExecutorKind::Fetcher;
            w.systems = systems.iter().map(|s| s.to_string()).collect();
            w
        };
        let mk_fod = |system: &str| {
            let mut d = DerivationState::try_from_node(&make_derivation_node("fod", system).into())
                .unwrap();
            d.is_fixed_output = true;
            d
        };

        let arm = mk_fetcher(&["aarch64-linux", "builtin"]);
        let x86 = mk_fetcher(&["x86_64-linux", "builtin"]);

        // builtin FOD: both arches eligible (overflow to either).
        assert!(hard_filter(&arm, &mk_fod("builtin")));
        assert!(hard_filter(&x86, &mk_fod("builtin")));
        // x86_64-linux FOD: only x86 fetcher; arm rejects.
        assert!(hard_filter(&x86, &mk_fod("x86_64-linux")));
        assert!(!hard_filter(&arm, &mk_fod("x86_64-linux")));
        assert_eq!(
            rejection_reason(&arm, &mk_fod("x86_64-linux")),
            Some("system-mismatch")
        );
    }

    /// Kind check runs BEFORE capacity — a full builder still rejects
    /// FODs for the right reason (wrong kind), not by accident (no
    /// capacity). Pins the ordering so a refactor that puts kind LAST
    /// doesn't silently accept FODs on an idle misconfigured builder.
    #[test]
    fn hard_filter_kind_check_precedes_capacity() {
        let mut fetcher_full = make_worker("f", 1); // at capacity
        fetcher_full.kind = ExecutorKind::Fetcher;
        let mut fod = make_drv();
        fod.is_fixed_output = true;
        // Kind matches → falls through to capacity check → fails there.
        assert!(!hard_filter(&fetcher_full, &fod));

        let mut builder_idle = make_worker("b", 0); // idle
        builder_idle.kind = ExecutorKind::Builder;
        // Kind mismatch → rejected regardless of idle capacity.
        assert!(!hard_filter(&builder_idle, &fod));
    }

    /// `rejection_reason` names each clause; `hard_filter` is its
    /// `is_none()`. Exhaustive over the reasons the diagnostic surfaces.
    /// ExecutorState isn't Clone (stream_tx), so each case rebuilds.
    #[test]
    fn rejection_reason_per_clause() {
        let drv = make_drv();

        // Baseline: idle builder, non-FOD drv, system match → ACCEPT.
        let w = make_worker("w", 0);
        assert_eq!(rejection_reason(&w, &drv), None);
        assert!(hard_filter(&w, &drv));

        // kind-mismatch
        let mut fetcher = make_worker("w", 0);
        fetcher.kind = ExecutorKind::Fetcher;
        assert_eq!(rejection_reason(&fetcher, &drv), Some("kind-mismatch"));

        // draining (admin) — checked before capacity, so a busy
        // draining worker reports "draining", not "at-capacity".
        let mut draining = make_worker("w", 1);
        draining.draining = true;
        assert_eq!(rejection_reason(&draining, &drv), Some("draining"));

        // draining_hb (worker SIGTERM via heartbeat) — same gate via
        // is_draining()'s OR.
        let mut draining_hb = make_worker("w", 0);
        draining_hb.draining_hb = true;
        assert_eq!(rejection_reason(&draining_hb, &drv), Some("draining"));

        // store-degraded
        let mut degraded = make_worker("w", 0);
        degraded.store_degraded = true;
        assert_eq!(rejection_reason(&degraded, &drv), Some("store-degraded"));

        // not-registered (no stream)
        let mut no_stream = make_worker("w", 0);
        no_stream.stream_tx = None;
        assert_eq!(rejection_reason(&no_stream, &drv), Some("not-registered"));

        // stream-closed (I-095): stream_tx Some but receiver dropped.
        // Distinct from not-registered — is_registered() stays true
        // (gauge accounting), but dispatch must skip.
        let closed = make_worker_closed_stream("w");
        assert!(closed.is_registered(), "is_registered ignores is_closed()");
        assert!(!closed.has_capacity(), "has_capacity gates on is_closed()");
        assert_eq!(rejection_reason(&closed, &drv), Some("stream-closed"));
        assert!(!hard_filter(&closed, &drv));

        // at-capacity
        let busy = make_worker("w", 1);
        assert_eq!(rejection_reason(&busy, &drv), Some("at-capacity"));

        // system-mismatch
        let mut aarch_drv = make_drv();
        "aarch64-linux".clone_into(&mut aarch_drv.system);
        assert_eq!(rejection_reason(&w, &aarch_drv), Some("system-mismatch"));

        // feature-missing
        let mut feat_drv = make_drv();
        feat_drv.required_features = vec!["kvm".into()];
        assert_eq!(rejection_reason(&w, &feat_drv), Some("feature-missing"));

        // failed-on
        let mut failed_drv = make_drv();
        failed_drv.retry.failed_builders.insert("w".into());
        assert_eq!(rejection_reason(&w, &failed_drv), Some("failed-on"));

        // resource-fit
        let mut tight = make_worker("w", 0);
        tight.last_resources = Some(rio_proto::types::ResourceUsage {
            memory_total_bytes: 4 << 30,
            ..Default::default()
        });
        let mut big_drv = make_drv();
        big_drv.sched.last_intent = Some(SolvedIntent {
            mem_bytes: 8 << 30,
            ..Default::default()
        });
        assert_eq!(rejection_reason(&tight, &big_drv), Some("resource-fit"));

        // hard_filter == rejection_reason.is_none() for every case
        // above. Spot-check one of each polarity.
        assert!(!hard_filter(&busy, &drv));
        assert!(hard_filter(&tight, &drv)); // last_intent=None → fits
    }

    #[test]
    fn no_candidates_returns_none() {
        let workers = workers_map(vec![make_worker("full", 2)]); // at capacity
        assert_eq!(best_executor(&workers, &make_drv()), None);
    }

    #[test]
    fn failed_worker_excluded() {
        // Two workers, both idle + capable. drv has worker-a in
        // failed_builders → best_executor MUST pick worker-b. Without
        // the exclusion, a 2-worker cluster with one broken worker
        // would oscillate: fail → reassign to same worker → fail
        // → ... until poison threshold. With exclusion: second
        // attempt goes to worker-b, succeeds.
        let workers = workers_map(vec![make_worker("worker-a", 0), make_worker("worker-b", 0)]);
        let mut drv = make_drv();
        drv.retry.failed_builders.insert("worker-a".into());

        let chosen = best_executor(&workers, &drv);
        assert_eq!(
            chosen,
            Some("worker-b".into()),
            "worker-a excluded via failed_builders → worker-b is the only candidate"
        );

        // ALL workers failed → None (caller defers). This is where
        // poison detection kicks in (failed_builders.len() >= 3 →
        // handle_transient_failure poisons).
        drv.retry.failed_builders.insert("worker-b".into());
        assert_eq!(
            best_executor(&workers, &drv),
            None,
            "all workers in failed_builders → nobody eligible"
        );
    }

    /// When `worker_count < poison_threshold` and ALL workers have failed
    /// this derivation, `best_executor` returns None (everyone fails the
    /// `failed_builders` exclusion in `hard_filter`). This is CORRECT at
    /// the assignment layer — `best_executor` has no business poisoning.
    ///
    /// The starvation this used to cause (derivation stuck in Ready
    /// forever, never poisoned because `len=2 < threshold=3`, never
    /// dispatched because all workers excluded) is FIXED at two layers
    /// (I-065): `handle_transient_failure` poisons immediately on the
    /// failure-report path; `dispatch_ready` poisons as a backstop for
    /// paths that bypass it (worker disconnect, recovery reconcile).
    /// Both call `failed_builders_exhausts_fleet`, which is kind-aware
    /// — the original check (`self.executors.keys().all(...)`) counted
    /// fetchers against a builder drv and never tripped on mixed
    /// clusters. See actor/{dispatch.rs,completion.rs}.
    #[test]
    fn all_workers_failed_below_threshold_poisons_upstream() {
        // 2-worker cluster — below the default poison_threshold of 3.
        let workers = workers_map(vec![make_worker("worker-a", 0), make_worker("worker-b", 0)]);
        let mut drv = make_drv();

        // Both workers failed. failed_builders.len() == 2 < 3 →
        // PoisonConfig::is_poisoned alone would NOT poison, but
        // handle_transient_failure's worker_count clamp now does.
        drv.retry.failed_builders.insert("worker-a".into());
        drv.retry.failed_builders.insert("worker-b".into());

        // best_executor still returns None (both excluded via
        // failed_builders) — correct. The caller
        // (handle_transient_failure) poisons BEFORE this state is
        // reachable in the dispatch loop, so the None here is never
        // observed in production.
        assert_eq!(
            best_executor(&workers, &drv),
            None,
            "all workers in failed_builders → best_executor correctly returns \
             None; handle_transient_failure poisons upstream via \
             worker_count clamp so this Ready-but-undispatchable state \
             is never reached"
        );

        // Sanity: adding a third worker makes dispatch possible again
        // (relevant for the case where a fresh worker connects BEFORE
        // the 2nd failure lands — no poison, dispatch resumes).
        let mut workers3 = workers;
        let c = make_worker("worker-c", 0);
        workers3.insert(c.executor_id.clone(), c);
        assert_eq!(
            best_executor(&workers3, &drv),
            Some("worker-c".into()),
            "fresh worker not in failed_builders → dispatch resumes"
        );
    }

    #[test]
    fn single_candidate_short_circuits() {
        let workers = workers_map(vec![make_worker("only", 0)]);
        let result = best_executor(&workers, &make_drv());
        assert_eq!(result.as_deref(), Some("only"));
    }

    // r[verify sched.assign.resource-fit]
    // ADR-020 §5: worker.memory_total_bytes >= last_intent.mem_bytes
    // as a hard filter. Uses `hard_filter` directly (not
    // `best_executor`) to isolate the resource-fit arm from the
    // warm-gate — the filter's pass/fail is what's under test.
    #[test]
    fn resource_fit_filter() {
        const GIB: u64 = 1 << 30;

        // Plan T3 case matrix. Worker memory via last_resources;
        // drv estimate via last_intent.mem_bytes.
        let mk_worker = |mem_total: u64| {
            let mut w = make_worker("w", 0);
            w.last_resources = Some(rio_proto::types::ResourceUsage {
                memory_total_bytes: mem_total,
                ..Default::default()
            });
            w
        };
        let mk_drv = |est: Option<u64>| {
            let mut d = make_drv();
            d.sched.last_intent = est.map(|m| SolvedIntent {
                mem_bytes: m,
                ..Default::default()
            });
            d
        };

        // Worker 8Gi, drv est 6Gi → fits (6 ≤ 8).
        assert!(
            hard_filter(&mk_worker(8 * GIB), &mk_drv(Some(6 * GIB))),
            "8Gi worker must fit 6Gi drv"
        );

        // Worker 8Gi, drv est 12Gi → doesn't fit.
        assert!(
            !hard_filter(&mk_worker(8 * GIB), &mk_drv(Some(12 * GIB))),
            "8Gi worker must REJECT 12Gi drv"
        );

        // Worker 64Gi, drv est 6Gi → fits (overflow routing: a small
        // drv CAN go to a big pod if that's what's idle).
        assert!(
            hard_filter(&mk_worker(64 * GIB), &mk_drv(Some(6 * GIB))),
            "64Gi worker must fit 6Gi drv (overflow routing)"
        );

        // Worker 0 (cgroup memory.max=max → unlimited), drv 128Gi
        // → fits. rio-builder cgroup.rs:667 unwrap_or(0) → 0 means
        // "unknown ceiling", never "zero memory".
        assert!(
            hard_filter(&mk_worker(0), &mk_drv(Some(128 * GIB))),
            "memory_total_bytes=0 (unlimited cgroup) must fit any drv"
        );

        // Drv est=None (cold start: no history) → any worker fits,
        // even an 8Gi one. The manifest omits cold-start drvs too
        // (controller uses operator floor) — consistent with "no
        // estimate means no constraint".
        assert!(
            hard_filter(&mk_worker(8 * GIB), &mk_drv(None)),
            "est=None (cold start) must fit any worker"
        );
    }

    // r[verify sched.assign.resource-fit]
    // Worker with last_resources=None (no heartbeat resources yet)
    // → unknown ceiling → fits. Separate from the 0-ceiling case:
    // this is "heartbeat didn't carry resources" (proto field
    // absent), not "heartbeat said unlimited". Both mean
    // always-fits, but via different match arms.
    #[test]
    fn resource_fit_no_heartbeat_resources_fits() {
        let w = make_worker("fresh", 0); // last_resources defaults to None
        assert!(w.last_resources.is_none(), "precondition: no resources");
        let mut d = make_drv();
        d.sched.last_intent = Some(SolvedIntent {
            mem_bytes: 128 << 30, // 128Gi
            ..Default::default()
        });

        assert!(
            hard_filter(&w, &d),
            "worker with no heartbeat resources must be treated as unknown-fits"
        );
    }

    // r[verify sched.assign.warm-gate]
    // Warm-gate: warm worker wins over cold even when cold is
    // otherwise better. With zero warm workers, falls back to cold
    // and increments the fallback metric.
    #[test]
    fn warm_gate_prefers_warm_worker() {
        // P0537: both idle (single-build). Warm-gate must pick warm
        // even when cold is otherwise indistinguishable.
        let mut warm = make_worker("warm", 0);
        warm.warm = true;
        let mut cold = make_worker("cold", 0);
        cold.warm = false;

        let workers = workers_map(vec![warm, cold]);

        // Warm-gate first-pass: only "warm" passes. Cold is
        // filtered out. "warm" is the ONLY candidate → picked.
        let result = best_executor(&workers, &make_drv());
        assert_eq!(
            result.as_deref(),
            Some("warm"),
            "warm-gate first-pass must pick warm over cold"
        );
    }

    // r[verify sched.assign.warm-gate]
    // r[verify obs.metric.scheduler]
    #[test]
    fn warm_gate_fallback_when_no_warm_workers() {
        use rio_test_support::metrics::CountingRecorder;
        let recorder = CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);

        // All-cold cluster (fresh scale-up or single-worker). The gate
        // MUST fall back — the scheduler can't deadlock.
        let mut a = make_worker("a", 1);
        a.warm = false;
        let mut b = make_worker("b", 0);
        b.warm = false;

        let workers = workers_map(vec![a, b]);

        let before = recorder.get("rio_scheduler_warm_gate_fallback_total{}");

        // With zero warm candidates, fallback uses cold workers with
        // the SAME hard_filter (capacity/features) then scores.
        // "b" has lower load (0.0 vs 0.25) → wins.
        let result = best_executor(&workers, &make_drv());
        assert_eq!(
            result.as_deref(),
            Some("b"),
            "fallback must dispatch cold and still prefer lower-load"
        );
        // P0299 EC: fallback path increments fallback_total. Without
        // this assert, a refactor that removed the metric (or moved
        // the increment to a branch that doesn't fire) would be
        // silent — the PICK assert above doesn't prove the counter.
        assert_eq!(
            recorder.get("rio_scheduler_warm_gate_fallback_total{}") - before,
            1,
            "fallback path must increment rio_scheduler_warm_gate_fallback_total exactly once"
        );
    }

    #[test]
    fn warm_gate_is_per_worker_not_global() {
        // Warm-gate per spec: a second worker registering while the
        // first is still warming does not delay builds the second
        // (already warm) worker can take. Prove: cold+warm both
        // eligible → warm wins, cold is NOT blocking dispatch.
        let mut cold = make_worker("cold-still-warming", 0);
        cold.warm = false;
        let warm = make_worker("warm-ready", 0);

        let workers = workers_map(vec![cold, warm]);

        let result = best_executor(&workers, &make_drv());
        assert_eq!(
            result.as_deref(),
            Some("warm-ready"),
            "a cold worker's presence must not block dispatch to a warm one"
        );
    }

    /// I-095 regression: a closed-channel executor must NOT be picked.
    /// Before the fix, dispatch_ready would pick it for every ready
    /// drv in the pass (try_send fails → reset_to_ready → next drv
    /// picks it again), saturating the actor. With the fix, hard_filter
    /// rejects it so best_executor returns the live one (or None).
    #[test]
    fn best_executor_skips_closed_stream() {
        let drv = make_drv();

        // Only candidate has a closed channel → no executor.
        let dead = make_worker_closed_stream("dead");
        let map = workers_map(vec![dead]);
        assert_eq!(best_executor(&map, &drv), None);

        // Closed + live → live is picked. Loop to guard against
        // accidental ordering-dependence (HashMap iteration).
        for _ in 0..10 {
            let dead = make_worker_closed_stream("dead");
            let live = make_worker("live", 0);
            let map = workers_map(vec![dead, live]);
            assert_eq!(best_executor(&map, &drv).as_deref(), Some("live"));
        }
    }
}
