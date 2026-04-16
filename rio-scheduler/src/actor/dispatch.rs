//! Ready-queue dispatch: assign ready derivations to available workers.
// r[impl sched.overflow.up-only]

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use uuid::Uuid;

use tracing::{debug, error, info, warn};

use rio_proto::types::FindMissingPathsRequest;

use crate::state::{DerivationStatus, DrvHash, ExecutorId};

use super::DagActor;
#[cfg(test)]
use super::backdate;

/// Per-dispatch-pass accumulators threaded through
/// [`DagActor::try_dispatch_one`]. The drain loop in
/// [`DagActor::dispatch_ready`] previously closed over five outer
/// `mut` locals; collecting them here lets the loop body extract as a
/// method without 6-positional-`&mut` signatures.
#[derive(Default)]
struct DispatchTickCtx {
    /// Hashes the batch FOD pre-pass already checked (I-163).
    /// `try_dispatch_one` skips the per-FOD store RPC for these.
    batch_checked: HashSet<DrvHash>,
    /// Derivations that couldn't dispatch this pass (backoff not
    /// elapsed, no eligible worker, or assignment send failed).
    /// Re-pushed onto the ready queue at end of each cycle.
    deferred: Vec<DrvHash>,
    /// Per-TARGET-class deferral counts (operator gauge).
    class_deferred: HashMap<String, u64>,
    /// FOD deferral count. Separate from `class_deferred` — fetchers
    /// aren't size-classed (ADR-019).
    fod_deferred: u64,
    /// Ready drvs whose `system` is advertised by ZERO registered
    /// executors of the matching kind. Per-system count → gauge + a
    /// single WARN on first observation (operator action: add a pool).
    unroutable_systems: HashMap<String, u64>,
    /// Successful assign_to_worker calls (for the >1s debug log).
    n_assigned: u64,
}

/// Result of [`DagActor::try_dispatch_one`]. Only the outer drain
/// loop's `dispatched_any` flag depends on this — all other
/// accumulators are mutated through [`DispatchTickCtx`].
enum DispatchOutcome {
    /// Assigned to a worker, or short-circuited a Ready FOD from
    /// store. Triggers another drain cycle.
    Progressed,
    /// Stale entry / backoff / deferred / poisoned. May have pushed
    /// into `ctx.deferred`.
    NoProgress,
}

/// I-025 freeze detector: state machine that WARNs when derivations are
/// queued but zero streams of the matching kind exist for >60s.
///
/// The scheduler already surfaces this via metrics (`fod_queue_depth` +
/// `fetcher_utilization`), but metrics need a port-forward. A WARN lands
/// in `kubectl logs`. QA I-025: 20-minute freeze with zero ERROR/WARN is
/// operator-hostile — the scheduler knew, it just didn't say.
///
/// Rate-limit: `since` is reset on each WARN so we emit once/minute, not
/// once/dispatch-pass (~once/tick = every 10s would spam).
///
/// Free function (not `&mut self`) so the call site can borrow
/// `&mut self.fod_freeze_since` while also reading `self.executors`.
fn check_freeze(
    since: &mut Option<Instant>,
    frozen: bool,
    kind: &str,
    queue_depth: u64,
    stream_count: usize,
) {
    const WARN_AFTER: std::time::Duration = std::time::Duration::from_secs(60);
    match (frozen, *since) {
        (true, None) => *since = Some(Instant::now()),
        (true, Some(start)) if start.elapsed() > WARN_AFTER => {
            warn!(
                kind,
                queue_depth,
                stream_count,
                frozen_for_secs = start.elapsed().as_secs(),
                "derivations queued but zero {kind} streams — dispatch stuck. \
                 Worker gRPC bidi-streams may have disconnected. \
                 Run `rio-cli derivations --all-active --stuck` to diagnose. \
                 Workers are ephemeral Jobs — check controller reconcile: \
                 `kubectl get builderpool,fetcherpool -A` and `rio-cli executors`"
            );
            // Rate-limit: reset so we WARN once/minute, not once/pass.
            *since = Some(Instant::now());
        }
        (false, _) => *since = None,
        _ => {} // frozen but not yet 60s — keep counting
    }
}

impl DagActor {
    // -----------------------------------------------------------------------
    // Dispatch
    // -----------------------------------------------------------------------

    /// Dispatch ready derivations to available workers (FIFO).
    pub(super) async fn dispatch_ready(&mut self) {
        // Standby scheduler: merge DAGs (state warm for fast
        // takeover) but DON'T dispatch. The lease task flips this
        // on acquire/lose via LeaderState::on_acquire/on_lose.
        // SeqCst load: paired with SeqCst stores in LeaderState so
        // the three-field transition (generation, is_leader,
        // recovery_complete) is observably ordered even on ARM.
        // A one-pass lag on a single flag is still harmless (see
        // LeaderState struct doc). In non-K8s mode this is always
        // true — no-op check.
        if !self.leader.is_leader() {
            return;
        }
        // Also gate on recovery: don't dispatch until recover_from_
        // pg has rebuilt the DAG. Otherwise we'd dispatch from a
        // partial/empty DAG mid-recovery. SeqCst pairs with
        // handle_leader_acquired's SeqCst — sees all recovery
        // writes before proceeding (though actor is single-threaded
        // so this is belt-and-suspenders).
        if !self.leader.recovery_complete() {
            return;
        }

        // I-163: any caller reaching here (Tick, MergeDag,
        // ProcessCompletion, became_idle/PrefetchComplete carve-out)
        // is about to do the work the dirty flag represents. Clear it
        // so the NEXT Tick doesn't
        // redundantly re-dispatch when an inline caller already ran.
        // Cleared after the leader/recovery gates — a not-yet-leader
        // standby keeps the flag so the first post-recovery Tick
        // dispatches.
        self.dispatch_dirty = false;

        // I-140: per-phase timing. Same pattern as merge.rs phase!().
        // dispatch_ready is on the hot path (every heartbeat) so the
        // per-phase log is at trace level; only the >1s total is debug.
        let t_total = Instant::now();
        let mut t_phase = Instant::now();
        let mut n_popped = 0u64;
        macro_rules! phase {
            ($name:literal) => {
                tracing::trace!(elapsed = ?t_phase.elapsed(), phase = $name, "dispatch phase");
                t_phase = Instant::now();
            };
        }

        // I-067/I-070: batched pre-pass — short-circuit any Ready IA
        // derivation whose outputs are already in the store (locally
        // or upstream-substitutable). Was FOD-only; non-FODs relied on
        // merge-time check_available which truncates at 4096 paths, so
        // an 18k-drv build's non-FOD IA cache-hits dispatched to
        // builders. The per-drv check inside the dispatch loop below
        // is kept as a fallback for nodes promoted to Ready DURING
        // this pass (from the cascade each completion here triggers).
        //
        // I-163: returns the set of hashes the batch ALREADY checked
        // (regardless of outcome). The drain loop skips the per-drv
        // store-check for these — re-asking the store 200ms later for
        // the same 211 paths was the ~150ms dominant cost of the
        // 169ms/Heartbeat that saturated the actor at medium-mixed-32x
        // scale.
        let mut ctx = DispatchTickCtx {
            batch_checked: self.batch_probe_cached_ready().await,
            ..Default::default()
        };
        phase!("0-batch-ready-precheck");

        // Drain the queue, dispatching eligible derivations and deferring
        // ineligible ones. Deferring (instead of breaking on the first
        // ineligible derivation) prevents head-of-line blocking — an
        // aarch64 drv at queue head must not block all x86_64 dispatch.
        //
        // Keep cycling until a full pass with no dispatches AND no stale removals.
        // In practice this terminates quickly: each derivation is either
        // dispatched, deferred, or removed (stale) exactly once per pass.
        let mut dispatched_any = true;
        while dispatched_any {
            dispatched_any = false;

            while let Some(drv_hash) = self.ready_queue.pop() {
                n_popped += 1;
                if matches!(
                    self.try_dispatch_one(drv_hash, &mut ctx).await,
                    DispatchOutcome::Progressed
                ) {
                    dispatched_any = true;
                }
            }

            // Re-queue deferred derivations. push_ready recomputes their
            // priority (unchanged since we just popped them), so they
            // slot back into the same position. The old "push_front to
            // preserve order" doesn't apply — priority IS the order.
            for hash in std::mem::take(&mut ctx.deferred) {
                self.push_ready(hash);
            }
        }
        phase!("1-drain-loop");

        self.publish_dispatch_gauges(ctx.class_deferred, ctx.fod_deferred, ctx.unroutable_systems);
        phase!("2-gauges");
        let _ = &mut t_phase;
        let total = t_total.elapsed();
        if total >= std::time::Duration::from_secs(1) {
            debug!(
                elapsed = ?total,
                popped = n_popped,
                assigned = ctx.n_assigned,
                ready_queue = self.ready_queue.len(),
                "dispatch_ready total"
            );
        }
    }

    /// One iteration of the dispatch drain loop: stale guards, backoff
    /// check, classify, FOD store short-circuit, executor placement,
    /// assign or defer or poison. Mutates `ctx` for deferral/count
    /// accumulators; returns whether progress was made (drives the
    /// outer `dispatched_any` cycle).
    async fn try_dispatch_one(
        &mut self,
        drv_hash: DrvHash,
        ctx: &mut DispatchTickCtx,
    ) -> DispatchOutcome {
        // Stale-entry guards: drop if not in DAG or not Ready.
        let Some(state) = self.dag.node(&drv_hash) else {
            return DispatchOutcome::NoProgress;
        };
        if state.status() != DerivationStatus::Ready {
            return DispatchOutcome::NoProgress;
        }
        // Retry backoff: if set and not yet elapsed, defer.
        // The derivation stays Ready + in queue (re-pushed
        // at the end of the pass with the other deferred).
        // Next dispatch pass re-checks — convergent without
        // timers. Cheap: one Instant::now() only for
        // derivations that failed transiently (backoff_until
        // is None for fresh ones).
        if let Some(deadline) = state.retry.backoff_until
            && Instant::now() < deadline
        {
            ctx.deferred.push(drv_hash);
            return DispatchOutcome::NoProgress;
        }

        // Classify by estimated duration + memory + CPU. None
        // if size_classes unconfigured (optional feature off —
        // no filter, all workers candidates).
        //
        // Also compute the SLA-solved memory for the resource-fit
        // filter (`r[sched.assign.resource-fit]`): same
        // `solve_intent_for` the snapshot uses, so the controller
        // spawns and dispatch accepts the SAME shape.
        //
        // `is_fixed_output` is captured into a local so the
        // `state` borrow ends here — `node_mut` below needs
        // exclusive access to `self.dag`.
        //
        // Read guard is dropped at the end of this block —
        // BEFORE `assign_to_worker().await`. parking_lot
        // guards aren't `Send`; the await point would be a
        // compile error anyway, but keeping the scope tight
        // is defensive.
        let (
            target_class,
            est_cores,
            est_memory_bytes,
            est_disk_bytes,
            est_deadline_secs,
            sla_predicted,
            is_fixed_output,
            system,
        ) = {
            let pname = state.pname.as_deref();
            let system = &state.system;
            let classes = self.sizing.size_classes.read();
            // Phase 6 deletes the whole classify() block; until then
            // (None, None) degrades it to duration-only.
            let target_class =
                crate::assignment::classify(state.sched.est_duration, None, None, &classes);
            // Skip for FODs: probe defaults (8 GiB) would reject
            // 2 GiB fetchers. I-062: 5 recurrences of fod_queue=2 +
            // fetcher_util=0 before the per-clause diagnostic exposed
            // this. FOD memory is the download buffer, not a compile
            // heap; resource-fit is the wrong gate.
            //
            // Skip when SpawnIntent emission is inactive: the symmetry
            // argument ("controller spawns and dispatch accepts the
            // SAME shape") only holds when the controller actually
            // receives intents and runs `apply_intent_resources`.
            // `compute_spawn_intents` emits intents iff
            // `sla_config.is_some()` (Static-mode gate). With it
            // false the
            // controller takes the `spawn_n` path (jobs.rs:244
            // `intents.is_empty()`) → fixed per-class `spec.resources`
            // → a cold-start `solve_intent_for` probe (12 GiB at helm
            // defaults) makes the resource-fit gate reject every
            // smaller worker. vm-lifecycle-recovery / vm-le-build-k3s
            // 2 GiB `tiny` pool flaked (not deterministic — gate passes
            // when `w.last_resources` is still None from the cgroup-
            // poll-vs-first-heartbeat race). The pre-ADR-023 path
            // (`bucketed_estimate`) returned None on cold start.
            let (est_cores, est_memory_bytes, est_disk_bytes, est_deadline_secs, sla_predicted) =
                if state.is_fixed_output || self.sla_config.is_none() || classes.is_empty() {
                    (None, None, None, None, None)
                } else {
                    let (cores, mem, disk, deadline, pred, _) = self.solve_intent_for(pname, state);
                    (Some(cores), Some(mem), Some(disk), Some(deadline), pred)
                };
            (
                target_class,
                est_cores,
                est_memory_bytes,
                est_disk_bytes,
                est_deadline_secs,
                sla_predicted,
                state.is_fixed_output,
                system.clone(),
            )
        };

        // Write the estimate onto the state BEFORE placement
        // so `hard_filter` (via find_executor_with_overflow →
        // best_executor) reads the fresh value. Refreshed each
        // dispatch pass — picks up estimator Tick updates.
        if let Some(state) = self.dag.node_mut(&drv_hash) {
            state.sched.est_cores = est_cores;
            state.sched.est_memory_bytes = est_memory_bytes;
            // D4: `bump_floor_or_count` reads est_{disk,deadline} as
            // `last_intent` for the doubling base.
            state.sched.est_disk_bytes = est_disk_bytes;
            // D7: same `solve_intent_for` value the snapshot stamps
            // onto SpawnIntent.deadline_secs (was P3's stopgap
            // `wall_secs as u32`; now the floor-clamped 5×p99).
            state.sched.est_deadline_secs = est_deadline_secs;
            // ADR-023 phase-7: capture the dispatch-time prediction so
            // completion can score actual-vs-predicted on the SAME
            // curve we sized against (the estimator may have refit by
            // then).
            state.sched.sla_predicted = sla_predicted;
        }

        // I-067: a Ready FOD whose output already exists in
        // rio-store should not dispatch — re-fetching is a
        // wasted round-trip at best, and a hash-mismatch
        // poison if upstream changed since the cached output
        // was produced (I-041). The merge-time
        // check_cached_outputs only checks newly_inserted, so
        // a FOD that was already in-DAG (e.g. stuck Ready via
        // I-062, or Completed→Ready via verify_preexisting_
        // completed) is never re-checked there. Re-check here.
        // I-163: skip the per-drv RPC if the batch pre-pass already
        // checked this hash. A node in `batch_checked` that's still
        // Ready here was found NOT-in-store by the batch (otherwise it
        // would have completed and the status guard above would have
        // dropped it) — no need to ask again. Only cascade-promoted
        // nodes (Ready AFTER the batch ran) hit the per-drv path.
        // Best-effort: store unreachable → dispatch as before.
        if !ctx.batch_checked.contains(&drv_hash) && self.ready_check_or_spawn(&drv_hash).await {
            return DispatchOutcome::Progressed;
        }

        // Try target class first, then overflow to larger
        // classes if no worker in target has capacity. A
        // "small" build CAN go to a "large" worker (just
        // wasteful); a "large" build CANNOT go to "small"
        // (would under-provision). So overflow walks UP only.
        let (eligible_worker, chosen_class) =
            self.find_executor_with_overflow(&drv_hash, target_class.as_deref());

        match eligible_worker {
            Some(executor_id) => {
                // Record what class we ACTUALLY routed to (may
                // be larger than target if we overflowed).
                // Misclassification detector reads this at
                // completion time.
                if let Some(state) = self.dag.node_mut(&drv_hash) {
                    state.sched.assigned_size_class = chosen_class.clone();
                }
                if let Some(class) = &chosen_class {
                    metrics::counter!(
                        "rio_scheduler_size_class_assignments_total",
                        "class" => class.clone()
                    )
                    .increment(1);
                }

                if self.assign_to_worker(&drv_hash, &executor_id).await {
                    ctx.n_assigned += 1;
                    DispatchOutcome::Progressed
                } else {
                    // Assignment send failed (worker stream full or
                    // disconnected). Defer — retrying immediately in
                    // the same pass would spin: the channel won't
                    // drain until we yield to the runtime.
                    ctx.deferred.push(drv_hash);
                    DispatchOutcome::NoProgress
                }
            }
            None => {
                // No eligible worker (even with overflow).
                //
                // I-065: if EVERY currently-registered worker of
                // the matching kind is in failed_builders, this
                // derivation can never dispatch on this fleet —
                // it would defer forever (poison threshold
                // counts failures, but with N workers you can't
                // exceed N). Poison now so the build fails
                // visibly instead of hanging silently.
                //
                // The "every registered worker" check (not
                // `failed_builders.len() >= total`) handles
                // worker replacement: failed_builders may hold
                // stale IDs that don't count against the
                // current fleet.
                if self.failed_builders_exhausts_fleet(&drv_hash, is_fixed_output) {
                    self.poison_and_cascade(&drv_hash).await;
                    return DispatchOutcome::NoProgress;
                }
                // I-056: distinguish "no capacity right now" (defer,
                // autoscaler handles it) from "no pool advertises this
                // system at all" (operator action — add the pool or
                // its `systems` entry). The latter sat silently Ready
                // for hours; surface it via gauge + a one-shot WARN.
                if !self.any_executor_advertises_system(&system, is_fixed_output) {
                    *ctx.unroutable_systems.entry(system).or_insert(0) += 1;
                }
                // Defer and track by TARGET class (not chosen —
                // chosen is None when there's no eligible).
                // FODs tracked separately: they have no class,
                // and the operator action is "scale fetchers"
                // not "scale class X builders".
                if is_fixed_output {
                    ctx.fod_deferred += 1;
                } else if let Some(class) = &target_class {
                    *ctx.class_deferred.entry(class.clone()).or_insert(0) += 1;
                }
                // I-056-style per-clause diagnostic: when there ARE
                // registered workers of the right kind but none
                // eligible, the freeze detectors above don't fire
                // (they key on stream_count==0), and the drv silently
                // defers forever. Dump per-worker rejection_reason at
                // INFO so `kubectl logs` names the gate. Sized for
                // small-N flake debugging — at scale (100s of workers)
                // the .map().collect() allocates per-tick; if that
                // becomes a problem, gate on a counter.
                if let Some(state) = self.dag.node(&drv_hash) {
                    let want_kind = if is_fixed_output {
                        rio_proto::types::ExecutorKind::Fetcher
                    } else {
                        rio_proto::types::ExecutorKind::Builder
                    };
                    let reasons: Vec<_> = self
                        .executors
                        .values()
                        .filter(|w| w.kind == want_kind && w.is_registered())
                        .map(|w| {
                            (
                                w.executor_id.as_ref().to_string(),
                                crate::assignment::rejection_reason(
                                    w,
                                    state,
                                    target_class.as_deref(),
                                ),
                            )
                        })
                        .collect();
                    if !reasons.is_empty() {
                        tracing::info!(
                            drv_hash = %drv_hash,
                            target_class = ?target_class,
                            ?reasons,
                            "no eligible executor; per-worker rejection reasons"
                        );
                    }
                }
                ctx.deferred.push(drv_hash);
                DispatchOutcome::NoProgress
            }
        }
    }

    /// Per-class deferral gauges + FOD queue depth + fetcher
    /// utilization + I-025 freeze-detector. Snapshot from one dispatch
    /// pass; next pass overwrites. ALL configured classes are zeroed
    /// first — gauges PERSIST in Prometheus until overwritten, so a
    /// class that was backed up (gauge=50) then cleared would stay at
    /// 50 forever otherwise.
    // r[impl sched.freeze-detector]
    // r[impl sched.dispatch.unroutable-system]
    fn publish_dispatch_gauges(
        &mut self,
        class_deferred: HashMap<String, u64>,
        fod_deferred: u64,
        unroutable_systems: HashMap<String, u64>,
    ) {
        for sc in self.sizing.size_classes.read().iter() {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => sc.name.clone()).set(0.0);
        }
        // Sum before class_deferred is consumed by the gauge loop below.
        // Feeds the I-025 builder-freeze check at the end of this fn.
        let class_total: u64 = class_deferred.values().sum();
        for (class, count) in class_deferred {
            metrics::gauge!("rio_scheduler_class_queue_depth", "class" => class).set(count as f64);
        }

        // FOD queue depth + fetcher utilization (ADR-019 observability).
        // Zero is a legitimate value (no FODs queued), emitted
        // explicitly so Prometheus doesn't persist stale nonzero.
        metrics::gauge!("rio_scheduler_fod_queue_depth").set(fod_deferred as f64);
        // I-048b: count only is_registered() fetchers. A heartbeat-only
        // zombie (stream_tx: None — race after scheduler restart, fixed
        // at the create-side in handle_heartbeat) would inflate `total`
        // here, hiding the freeze: fod_queue>0 + util=0 + total>0 looks
        // like "fetchers busy on something else" when really nothing
        // can dispatch. Filtering by is_registered() makes the freeze
        // detector below fire on genuine no-stream-connected.
        let (busy, total) = self.executors.values().fold((0u32, 0u32), |(b, t), e| {
            if e.kind == rio_proto::types::ExecutorKind::Fetcher && e.is_registered() {
                (b + u32::from(e.running_build.is_some()), t + 1)
            } else {
                (b, t)
            }
        });
        // No fetchers → emit 0.0 (not NaN). An operator seeing
        // fod_queue_depth > 0 AND fetcher_utilization == 0 with no
        // fetchers registered knows the FetcherPool isn't deployed.
        let util = if total > 0 {
            f64::from(busy) / f64::from(total)
        } else {
            0.0
        };
        metrics::gauge!("rio_scheduler_fetcher_utilization").set(util);

        // I-025 freeze detector: WARN if queue pressure + zero streams >60s.
        check_freeze(
            &mut self.fod_freeze_since,
            fod_deferred > 0 && total == 0,
            "fetcher",
            fod_deferred,
            total as usize,
        );
        let builder_stream_count = self
            .executors
            .values()
            .filter(|e| e.kind == rio_proto::types::ExecutorKind::Builder && e.is_registered())
            .count();
        check_freeze(
            &mut self.builder_freeze_since,
            class_total > 0 && builder_stream_count == 0,
            "builder",
            class_total,
            builder_stream_count,
        );

        // Unroutable-system gauge + edge-triggered WARN. Zero stale
        // labels first (same persist-until-overwritten reason as
        // class_queue_depth above), then set this pass's counts.
        for sys in self.unroutable_warned.iter() {
            metrics::gauge!("rio_scheduler_unroutable_ready", "system" => sys.clone()).set(0.0);
        }
        for (sys, count) in &unroutable_systems {
            metrics::gauge!("rio_scheduler_unroutable_ready", "system" => sys.clone())
                .set(*count as f64);
            if !self.unroutable_warned.contains(sys) {
                warn!(
                    system = %sys, ready = count,
                    "no registered executor advertises this system; Ready drvs \
                     unroutable until a pool with `systems` containing it exists"
                );
            }
        }
        // Retain only systems still unroutable so the WARN re-arms once
        // a system becomes routable and later regresses, AND so the
        // zeroing loop above stops emitting for long-gone systems.
        self.unroutable_warned
            .retain(|s| unroutable_systems.contains_key(s));
        self.unroutable_warned
            .extend(unroutable_systems.into_keys());
    }

    /// Any registered executor of the matching kind advertises
    /// `system`. Ignores busy/warm/class — distinguishes "no capacity
    /// right now" (transient, autoscaler handles it) from "no such
    /// pool exists" (operator action; the I-056 silent-stuck case).
    fn any_executor_advertises_system(&self, system: &str, is_fixed_output: bool) -> bool {
        let want_kind = if is_fixed_output {
            rio_proto::types::ExecutorKind::Fetcher
        } else {
            rio_proto::types::ExecutorKind::Builder
        };
        self.executors.values().any(|w| {
            w.kind == want_kind && w.is_registered() && w.systems.iter().any(|s| s == system)
        })
    }

    /// I-067: best-effort store check for a Ready IA derivation's
    /// outputs (was FOD-only; generalised per the >4096 cap-gap).
    ///
    /// I-070: batched form — collect every unprobed Ready node's
    /// expected outputs, ONE `FindMissingPaths`, then
    /// [`Self::complete_ready_from_store`] each whose outputs are all
    /// present. Fail-open: store unreachable → no-op (per-drv
    /// fallback in the dispatch loop covers it next pass).
    ///
    /// Iterates the full DAG, not just `ready_queue` — `ready_queue` is
    /// a heap (no peek-iter without drain) and stale entries in it are
    /// harmless (the inner-loop status guard drops them after this
    /// completes them). Full-DAG scan is O(nodes) but the actor is
    /// single-threaded so there's no contention; for a 1085-node merge
    /// the scan is sub-ms vs. ~25s of sequential RPCs it replaces.
    ///
    /// Returns the set of hashes that were CHECKED (regardless of
    /// outcome). The drain loop skips `ready_check_or_spawn` for these
    /// (I-163) — they were either completed here or definitively
    /// found-missing one RPC ago. Empty set on fail-open paths (no
    /// store / RPC error / timeout): the per-drv fallback then runs as
    /// before, so the fail-open semantics are unchanged.
    // r[impl sched.dispatch.fod-substitute]
    async fn batch_probe_cached_ready(&mut self) -> HashSet<DrvHash> {
        let Some(store) = &self.store_client else {
            return HashSet::new();
        };
        let probe_gen = self.probe_generation;
        // Candidate set: (drv_hash, output_paths). Collected up-front
        // so the FindMissingPaths borrow doesn't hold &self.dag across
        // the .await (and so the completion loop can take &mut self).
        // Floating-CA (`expected_output_paths == [""]`) is excluded by
        // the `!is_empty()` + path-known check; the realisations lane
        // at merge-time handles those.
        let mut candidates: Vec<(DrvHash, Vec<String>)> = self
            .dag
            .iter_nodes()
            .filter(|(_, s)| {
                s.status() == DerivationStatus::Ready
                    && s.probed_generation < probe_gen
                    && !s.expected_output_paths.is_empty()
                    && s.expected_output_paths.iter().all(|p| !p.is_empty())
            })
            .map(|(h, s)| (DrvHash::from(h), s.expected_output_paths.clone()))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }
        // Belt under the store-side 4096 cap. The truncated tail keeps
        // probed_generation < probe_gen, so the per-drv
        // ready_check_or_spawn fallback (same drain loop) still probes
        // them — sequentially, but bounded to one FMP each.
        candidates.truncate(super::DISPATCH_PROBE_BATCH_CAP);
        for (h, _) in &candidates {
            if let Some(s) = self.dag.node_mut(h) {
                s.probed_generation = probe_gen;
            }
        }

        // Tenant context for the upstream-substitution probe: any
        // tenant that wants any candidate (substitution is content-
        // addressed; whose upstream we use is irrelevant to the
        // result). Without this the store sees tenant_id=None and
        // substitutable_paths stays empty — the pre-fix behaviour
        // that dispatched FODs already in cache.nixos.org.
        let probe = self.probe_tenant_meta(candidates.iter().map(|(h, _)| h));
        let probe_meta: Vec<(&'static str, &str)> =
            probe.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let store_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, p)| p.iter().cloned())
            .collect();
        let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
        let resp =
            match tokio::time::timeout(self.grpc_timeout, store.clone().find_missing_paths(req))
                .await
            {
                Ok(Ok(r)) => r.into_inner(),
                Ok(Err(e)) => {
                    debug!(
                        candidates = candidates.len(),
                        error = %e,
                        "batched Ready store-check FindMissingPaths failed; \
                         per-drv fallback will retry"
                    );
                    return HashSet::new();
                }
                Err(_) => {
                    debug!(
                        candidates = candidates.len(),
                        timeout = ?self.grpc_timeout,
                        "batched Ready store-check timed out; per-drv fallback will retry"
                    );
                    return HashSet::new();
                }
            };

        // r[impl sched.substitute.detached]
        // Partition: locally-present (not in missing_paths) → complete
        // inline; substitutable → spawn detached fetch; truly-missing →
        // leave Ready (dispatches normally). The detached fetch runs
        // OUTSIDE the actor loop — before this, the awaited
        // eager_substitute_fetch blocked MergeDag/dispatch for >100s
        // when the closure walk pulled ghc-sized NARs.
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let mut checked = HashSet::with_capacity(candidates.len());
        let mut to_spawn = Vec::new();
        for (drv_hash, paths) in candidates {
            checked.insert(drv_hash.clone());
            if paths.iter().all(|p| !missing.contains(p)) {
                self.complete_ready_from_store(&drv_hash).await;
            } else if paths
                .iter()
                .all(|p| !missing.contains(p) || substitutable.contains(p))
            {
                to_spawn.push((drv_hash, paths));
            }
        }
        self.spawn_substitute_fetches(to_spawn, probe).await;
        checked
    }

    /// Mint `(x-rio-service-token, x-rio-probe-tenant-id)` metadata
    /// for the dispatch-time store calls. Empty when no
    /// `service_signer` (dev mode) or no candidate has a known tenant
    /// (single-tenant mode / recovered orphan).
    fn probe_tenant_meta<'a>(
        &self,
        drv_hashes: impl Iterator<Item = &'a DrvHash>,
    ) -> Vec<(&'static str, String)> {
        let Some(signer) = &self.service_signer else {
            return Vec::new();
        };
        let Some(tid) = drv_hashes
            .filter_map(|h| self.dag.node(h))
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id)
        else {
            return Vec::new();
        };
        let claims = rio_auth::hmac::ServiceClaims {
            caller: "rio-scheduler".to_string(),
            expiry_unix: (std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0))
                + 60,
        };
        vec![
            (rio_proto::SERVICE_TOKEN_HEADER, signer.sign(&claims)),
            (rio_proto::PROBE_TENANT_ID_HEADER, tid.to_string()),
        ]
    }

    fn inject_probe_meta(md: &mut tonic::metadata::MetadataMap, meta: &[(&'static str, &str)]) {
        for (k, v) in meta {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(*v) {
                md.insert(*k, mv);
            }
        }
    }

    // r[impl sched.substitute.detached]
    /// Transition each candidate to `Substituting` and spawn a
    /// background task that triggers store-side `try_substitute` (via
    /// `QueryPathInfo`) for its output paths, then posts
    /// [`ActorCommand::SubstituteComplete`] back into the mailbox.
    ///
    /// Detaches the upstream NAR fetch from the actor event loop:
    /// before this, `eager_substitute_fetch` was awaited inline and a
    /// single ghc-sized closure walk blocked `MergeDag` for >100s
    /// (`"actor command exceeded 1s","cmd":"MergeDag","elapsed":"135s"`).
    ///
    /// Candidates whose transition is rejected (vanished, wrong status)
    /// are skipped — they fall through to normal scheduling.
    /// `tenant_meta` is the owned form of either the gateway-forwarded
    /// JWT (merge-time) or the scheduler-minted service-token +
    /// `probe-tenant-id` pair (dispatch-time).
    pub(super) async fn spawn_substitute_fetches(
        &mut self,
        candidates: Vec<(DrvHash, Vec<String>)>,
        tenant_meta: Vec<(&'static str, String)>,
    ) {
        if candidates.is_empty() {
            return;
        }
        let Some(store) = self.store_client.clone() else {
            return;
        };
        let Some(weak_tx) = self.self_tx.clone() else {
            return;
        };
        struct Spawned {
            hash: DrvHash,
            drv_path: String,
            output_paths: Vec<String>,
            interested: HashSet<Uuid>,
        }
        let mut spawned: Vec<Spawned> = Vec::with_capacity(candidates.len());
        for (drv_hash, paths) in candidates {
            let Some(state) = self.dag.node_mut(&drv_hash) else {
                continue;
            };
            if let Err(e) = state.transition(DerivationStatus::Substituting) {
                debug!(%drv_hash, %e, "spawn_substitute: transition rejected; falling through");
                continue;
            }
            let drv_path = state.drv_path().to_string();
            let interested = state.interested_builds.clone();
            let output_paths = paths.clone();
            let store = store.clone();
            let weak_tx = weak_tx.clone();
            let meta = tenant_meta.clone();
            let h = drv_hash.clone();
            let sem = self.substitute_sem.clone();
            let shutdown = self.shutdown.clone();
            rio_common::task::spawn_monitored("substitute-fetch", async move {
                // Bound in-flight QPIs across ALL spawned tasks. The
                // task is already spawned (so the actor returned), but
                // it parks here until a slot is free — Substituting
                // status keeps dependents gated meanwhile.
                let _permit = sem.acquire_owned().await;
                let meta_ref: Vec<(&'static str, &str)> =
                    meta.iter().map(|(k, v)| (*k, v.as_str())).collect();
                let mut ok = true;
                'paths: for p in &paths {
                    for attempt in 0..super::SUBSTITUTE_FETCH_MAX_ATTEMPTS {
                        if shutdown.is_cancelled() {
                            ok = false;
                            break 'paths;
                        }
                        let mut c = store.clone();
                        match rio_proto::client::query_path_info_opt(
                            &mut c,
                            p,
                            super::SUBSTITUTE_FETCH_TIMEOUT,
                            &meta_ref,
                        )
                        .await
                        {
                            Ok(Some(_)) => continue 'paths,
                            Ok(None) => {
                                warn!(path = %p, "detached substitute fetch: NotFound \
                                       (upstream HEAD probe lied?); demoting to cache-miss");
                                metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                                    .increment(1);
                                ok = false;
                                continue 'paths;
                            }
                            Err(e) if rio_common::grpc::is_transient(e.code()) => {
                                debug!(path = %p, attempt, error = %e,
                                       "substitute fetch transient error; retrying");
                                metrics::counter!("rio_scheduler_substitute_fetch_retries_total")
                                    .increment(1);
                                if attempt + 1 == super::SUBSTITUTE_FETCH_MAX_ATTEMPTS {
                                    break;
                                }
                                tokio::select! {
                                    _ = shutdown.cancelled() => { ok = false; break 'paths; }
                                    _ = tokio::time::sleep(
                                        super::SUBSTITUTE_FETCH_BACKOFF.duration(attempt)
                                    ) => {}
                                }
                            }
                            Err(e) => {
                                warn!(path = %p, error = %e,
                                      "detached substitute fetch failed; demoting to cache-miss");
                                metrics::counter!("rio_scheduler_substitute_fetch_failures_total")
                                    .increment(1);
                                ok = false;
                                continue 'paths;
                            }
                        }
                    }
                    // Exhausted retries on transient errors.
                    warn!(path = %p, attempts = super::SUBSTITUTE_FETCH_MAX_ATTEMPTS,
                          "detached substitute fetch exhausted retries; demoting to cache-miss");
                    metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
                    ok = false;
                }
                if let Some(tx) = weak_tx.upgrade() {
                    let _ = tx
                        .send(super::ActorCommand::SubstituteComplete { drv_hash: h, ok })
                        .await;
                }
            });
            spawned.push(Spawned {
                hash: drv_hash,
                drv_path,
                output_paths,
                interested,
            });
        }
        if !spawned.is_empty() {
            debug!(
                count = spawned.len(),
                "detached upstream substitute fetch spawned"
            );
            metrics::counter!("rio_scheduler_substitute_spawned_total")
                .increment(spawned.len() as u64);
            let hashes: Vec<&str> = spawned.iter().map(|s| s.hash.as_str()).collect();
            self.persist_status_batch(&hashes, DerivationStatus::Substituting)
                .await;
            for s in &spawned {
                let event = rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::substituting(
                        s.drv_path.clone(),
                        s.output_paths.clone(),
                    ),
                );
                for &build_id in &s.interested {
                    self.events.emit(build_id, event.clone());
                }
            }
        }
    }

    // r[impl sched.substitute.detached]
    /// Handle a [`ActorCommand::SubstituteComplete`] posted by a
    /// detached fetch task. `ok=true` → output now in rio-store with
    /// its full reference closure (store-side `ensure_references`
    /// walked it), so `Substituting → Completed` is safe even if
    /// inputDrvs aren't yet Completed in the DAG. `ok=false` → revert
    /// to `Ready`/`Queued` for normal scheduling.
    pub(super) async fn handle_substitute_complete(&mut self, drv_hash: &DrvHash, ok: bool) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        if state.status() != DerivationStatus::Substituting {
            debug!(%drv_hash, status = ?state.status(),
                   "SubstituteComplete: not Substituting (cancelled/re-merged); dropping");
            return;
        }
        if ok {
            // complete_ready_from_store does Substituting→Completed
            // (valid transition) + the full post-completion machinery
            // (output_paths, persist, upsert_path_tenants, promote_
            // newly_ready, per-build events + completion check).
            self.complete_ready_from_store(drv_hash).await;
            // promote_newly_ready pushed dependents to ready_queue;
            // mark dirty so the next Tick dispatches them (this
            // handler runs outside dispatch_ready's drain loop).
            self.dispatch_dirty = true;
            return;
        }
        let to = if self.dag.all_deps_completed(drv_hash) {
            DerivationStatus::Ready
        } else {
            DerivationStatus::Queued
        };
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        if let Err(e) = state.transition(to) {
            warn!(%drv_hash, %e, "SubstituteComplete fail: revert rejected");
            return;
        }
        self.persist_status(drv_hash, to, None).await;
        if to == DerivationStatus::Ready {
            self.push_ready(drv_hash.clone());
            self.dispatch_dirty = true;
        }
    }

    /// Returns `true` only when `FindMissingPaths` definitively says all
    /// `expected_output_paths` are present. Any uncertainty (no paths to
    /// check, no store_client, RPC error, timeout) returns `false` so the
    /// caller proceeds to dispatch as before — fail-open.
    ///
    /// Fallback for the cascade tail: [`Self::batch_probe_cached_ready`]
    /// at the top of `dispatch_ready` covers every IA node that was
    /// Ready at pass start (one RPC). This per-drv check fires only for
    /// nodes promoted to Ready DURING the pass (via `find_newly_ready`
    /// from a completion above) — typically zero, occasionally a
    /// handful. Deferred nodes (no worker capacity) re-check each Tick
    /// via the batch, not here; the answer can flip to `true` mid-queue
    /// (an earlier dispatch on another scheduler/build uploaded it).
    async fn ready_check_or_spawn(&mut self, drv_hash: &DrvHash) -> bool {
        let probe_gen = self.probe_generation;
        let (paths, mut store) = {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                return false;
            };
            // Already probed this generation (by the batch or a prior
            // per-drv call) — same gate as `batch_probe_cached_ready`
            // so a fail-open empty `batch_checked` doesn't trigger N
            // sequential per-drv FMPs for nodes the batch just stamped.
            if state.probed_generation >= probe_gen {
                return false;
            }
            // Floating-CA: output path unknown until built → nothing
            // to ask FindMissingPaths. Guard so an empty-paths edge
            // case can't fall through to "all present".
            if state.expected_output_paths.is_empty()
                || state.expected_output_paths.iter().any(String::is_empty)
            {
                return false;
            }
            state.probed_generation = probe_gen;
            let Some(store) = &self.store_client else {
                return false;
            };
            (state.expected_output_paths.clone(), store.clone())
        };
        // r[impl sched.dispatch.fod-substitute] — same probe-tenant
        // wiring as batch_probe_cached_ready.
        let probe = self.probe_tenant_meta(std::iter::once(drv_hash));
        let probe_meta: Vec<(&'static str, &str)> =
            probe.iter().map(|(k, v)| (*k, v.as_str())).collect();
        let mut req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: paths.clone(),
        });
        Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
        match tokio::time::timeout(self.grpc_timeout, store.find_missing_paths(req)).await {
            Ok(Ok(r)) => {
                let resp = r.into_inner();
                if resp.missing_paths.is_empty() {
                    self.complete_ready_from_store(drv_hash).await;
                    return true;
                }
                // r[impl sched.substitute.detached] — spawn instead of
                // awaiting eager_substitute_fetch in the actor loop.
                let sub: HashSet<String> = resp.substitutable_paths.into_iter().collect();
                if resp.missing_paths.iter().all(|p| sub.contains(p)) {
                    self.spawn_substitute_fetches(vec![(drv_hash.clone(), paths)], probe)
                        .await;
                    return true;
                }
                false
            }
            Ok(Err(e)) => {
                debug!(drv_hash = %drv_hash, error = %e,
                       "Ready store-check FindMissingPaths failed; will dispatch");
                false
            }
            Err(_) => {
                debug!(drv_hash = %drv_hash, timeout = ?self.grpc_timeout,
                       "Ready store-check FindMissingPaths timed out; will dispatch");
                false
            }
        }
    }

    /// I-067: complete a Ready IA derivation whose output is already in
    /// store, without dispatching to a worker.
    ///
    /// Dispatch-time analogue of the merge-time `cached_hits` block in
    /// `handle_merge`, with the post-completion machinery from
    /// `handle_success_completion` (newly-ready cascade + per-build
    /// progress + completion check) since dependents are already in
    /// the DAG. Skips worker-result-only steps: no executor running-
    /// build clear, no `record_durations`, no critical-path accuracy
    /// metric, no CA realisation insert (input-addressed:
    /// `expected_output_paths` IS the realised path).
    async fn complete_ready_from_store(&mut self, drv_hash: &DrvHash) {
        let (drv_path, output_paths, interested) = {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                return;
            };
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "store-hit Ready→Completed rejected; dispatching instead");
                return;
            }
            state.output_paths = state.expected_output_paths.clone();
            (
                state.drv_path().to_string(),
                state.output_paths.clone(),
                state.interested_builds.clone(),
            )
        };

        info!(drv_hash = %drv_hash, "output already in store; skipping dispatch");
        metrics::counter!("rio_scheduler_cache_hits_total", "source" => "dispatch").increment(1);
        self.persist_status(drv_hash, DerivationStatus::Completed, None)
            .await;
        self.upsert_path_tenants_for(drv_hash).await;
        self.promote_newly_ready(drv_hash).await;

        let event = rio_proto::types::build_event::Event::Derivation(
            rio_proto::types::DerivationEvent::cached(drv_path, output_paths),
        );
        for build_id in interested {
            self.events.emit(build_id, event.clone());
            // I-103: dispatch-time short-circuit is "completed without
            // assignment" → counts as cached (matches the original
            // LIST_BUILDS_SELECT NOT EXISTS heuristic).
            if let Some(b) = self.builds.get_mut(&build_id) {
                b.cached_count += 1;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            self.events.emit_progress_with(build_id, &summary);
            self.check_build_completion(build_id).await;
        }
    }

    /// Find a worker for this derivation, starting at `target_class` and
    /// overflowing to progressively larger classes if needed.
    /// I-065: has `failed_builders` excluded EVERY currently-registered
    /// worker of the matching kind?
    ///
    /// Live example: 2-builder cluster, `diffutils.drv` accumulates
    /// `failed_builders=[b0,b1]`. `hard_filter`'s `!contains()` rejects
    /// both → defer forever. `PoisonConfig.threshold=3` never reached.
    /// The build hangs `[Active]` with no log signal.
    ///
    /// Predicate is "every kind-matching registered worker is in the
    /// failed set", not `failed_builders.len() >= total`. The latter
    /// over-counts stale IDs: b0 fails, b0 is replaced by b2, b1 fails
    /// → set={b0,b1} len=2, total=2 → would poison, but b2 was never
    /// tried.
    ///
    /// Returns false (don't poison) when zero workers of that kind are
    /// registered — that's "no workers connected", a transient that the
    /// freeze detector + autoscaler handle. Poisoning then would brick
    /// builds during a deployment rollout.
    pub(super) fn failed_builders_exhausts_fleet(
        &self,
        drv_hash: &DrvHash,
        is_fixed_output: bool,
    ) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        if state.retry.failed_builders.is_empty() {
            return false;
        }
        let want_kind = if is_fixed_output {
            rio_proto::types::ExecutorKind::Fetcher
        } else {
            rio_proto::types::ExecutorKind::Builder
        };
        let mut fleet = self
            .executors
            .values()
            .filter(|w| w.kind == want_kind && w.is_registered());
        // `all()` on an empty iterator is vacuously true — peek first.
        let Some(first) = fleet.next() else {
            return false;
        };
        let exhausted = std::iter::once(first)
            .chain(fleet)
            .all(|w| state.retry.failed_builders.contains(&w.executor_id));
        if exhausted {
            warn!(
                drv_hash = %drv_hash,
                kind = ?want_kind,
                failed_on = state.retry.failed_builders.len(),
                "failed_builders excludes every registered worker; poisoning \
                 (would otherwise defer forever — see I-065)"
            );
            metrics::counter!("rio_scheduler_poison_fleet_exhausted_total").increment(1);
        }
        exhausted
    }

    ///
    /// Returns `(executor_id, class_actually_used)`. Both None if nobody
    /// can take it (wrong system, all full, no workers).
    ///
    /// Overflow direction: small → large only. A slow build on a small
    /// worker would dominate that worker's single slot; a fast build on
    /// a large worker is just slightly wasteful. scheduler.md:178.
    fn find_executor_with_overflow(
        &self,
        drv_hash: &DrvHash,
        target_class: Option<&str>,
    ) -> (Option<ExecutorId>, Option<String>) {
        let Some(drv_state) = self.dag.node(drv_hash) else {
            return (None, None);
        };

        // r[impl sched.sla.intent-match]
        // ADR-023: a worker that heartbeated `intent_id == drv_hash` was
        // spawned FOR this derivation (controller stamped the SpawnIntent
        // on its pod resources). Prefer it over best_executor — its
        // (cores, mem, disk) were sized by `solve_intent_for` for this
        // exact drv. Re-check `can_build` (system/feature) so a pool
        // misconfig doesn't bypass the airgap/feature gates; on miss
        // (drv re-planned, scheduler restarted, intent stale) fall
        // through to pick-from-queue.
        if let Some(w) = self.executors.values().find(|w| {
            w.intent_id.as_deref() == Some(drv_hash.as_ref())
                && w.has_capacity()
                && w.can_build(&drv_state.system, &drv_state.required_features)
        }) {
            return (Some(w.executor_id.clone()), w.size_class.clone());
        }

        // r[impl sched.dispatch.no-fod-fallback]
        // r[impl sched.fod.size-class-reactive]
        // FOD overflow walks FETCHER classes only (I-170) — never the
        // builder size_classes chain below. If no fetcher class has a
        // free executor the FOD queues; the scheduler NEVER sends a
        // FOD to a builder under pressure (kind-mismatch in
        // hard_filter is the absolute boundary). A queued FOD is
        // preferable to a builder with internet access.
        //
        // D4: class-name `size_class_floor` removed; the reactive
        // `resource_floor` is applied inside `solve_intent_for`.
        // Phase 6 (D2) deletes this whole FOD branch — FODs go through
        // the same intent-match path as builds. Empty config = no
        // class filter (original behavior).
        if drv_state.is_fixed_output {
            if self.sizing.fetcher_classes.is_empty() {
                let w = crate::assignment::best_executor(&self.executors, drv_state, None);
                return (w, None);
            }
            for class in &self.sizing.fetcher_classes {
                if let Some(w) =
                    crate::assignment::best_executor(&self.executors, drv_state, Some(class))
                {
                    return (Some(w), Some(class.clone()));
                }
            }
            return (None, None);
        }

        // No classification configured → single best_executor call with
        // no filter. Fast path for deployments without size-classes.
        let Some(target) = target_class else {
            let w = crate::assignment::best_executor(&self.executors, drv_state, None);
            return (w, None);
        };

        // Build the overflow chain: target class, then all classes with
        // cutoff > target's cutoff, sorted ascending. If target cutoff
        // is 30s, chain is [small(30), medium(300), large(3600)].
        //
        // We don't cache this chain because classify() is called fresh
        // per-dispatch anyway (est_duration can change between ticks
        // via estimator refresh) and the sort is 2-4 elements.
        //
        // Read guard lives through the chain walk — no `.await` in
        // this fn, so it's safe. best_executor is sync.
        let classes = self.sizing.size_classes.read();
        // r[impl sched.builder.size-class-reactive]
        // D4: class-name `size_class_floor` clamp removed; the
        // reactive `resource_floor` is applied inside
        // `solve_intent_for` (mem/disk bytes) which feeds
        // `est_memory_bytes` for `hard_filter`'s resource-fit gate.
        // Phase 6 deletes this whole overflow chain.
        let start_cutoff = crate::assignment::cutoff_for(target, &classes);
        let mut chain: Vec<(&str, f64)> = classes
            .iter()
            .filter(|c| {
                // Target itself (== cutoff) or larger, raised to
                // floor. start_cutoff=None shouldn't happen (target
                // came FROM classify which reads the same config)
                // but be defensive: if None, include everything.
                start_cutoff.is_none_or(|t| c.cutoff_secs >= t)
            })
            .map(|c| (c.name.as_str(), c.cutoff_secs))
            .collect();
        chain.sort_by(|a, b| a.1.total_cmp(&b.1));

        // Walk the chain: first class with an available worker wins.
        for (class, _) in chain {
            if let Some(w) =
                crate::assignment::best_executor(&self.executors, drv_state, Some(class))
            {
                return (Some(w), Some(class.to_string()));
            }
        }

        (None, None)
    }

    /// Transition a derivation to Assigned and send it to the worker.
    /// Returns `true` if the assignment was sent, `false` if it failed
    /// (caller should defer the derivation, not retry immediately).
    ///
    /// Phases (each a sub-method below): transition → record (PG +
    /// in-mem) → send (with rollback) → emit. Split so the rollback
    /// inverse-of-record relationship is auditable side-by-side.
    pub(super) async fn assign_to_worker(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
    ) -> bool {
        if !self.transition_to_assigned(drv_hash, executor_id) {
            return false;
        }

        // Single atomic load. The lease task may fetch_add the
        // generation between the DB insert and the WorkAssignment send
        // below (there's an await in between). Without this snapshot,
        // the two reads could see DIFFERENT generations — the PG row
        // says "assigned under gen N" but the worker receives "gen
        // N+1." The worker then rejects its own assignment as stale.
        // Loading once and reusing closes the tear.
        //
        // Acquire pairs with the lease task's Release fetch_add. Sees
        // the generation AND any writes the lease task did before it
        // (is_leader=true, which dispatch_ready checked at loop top).
        let generation = self.leader.generation();

        self.record_assignment(drv_hash, executor_id, generation)
            .await;

        // PrefetchHint BEFORE WorkAssignment: the worker starts
        // warming its FUSE cache while still parsing the .drv. A few
        // seconds of head-start on a multi-minute fetch is the win.
        // Best-effort: try_send, failure logs debug not warn. If only
        // the HINT fails, the build still works (on-demand FUSE).
        self.send_prefetch_hint(executor_id, drv_hash);

        // Resolve CA inputs + construct the WorkAssignment proto.
        // None means the DAG node disappeared between the Ready
        // check and here (TOCTOU vs. concurrent cancel) — treat as
        // assignment failure so the caller defers.
        let Some(assignment) = self
            .build_assignment_proto(drv_hash, executor_id, generation)
            .await
        else {
            return false;
        };

        if !self.try_send_assignment(drv_hash, executor_id, assignment) {
            self.rollback_assignment(drv_hash, executor_id).await;
            return false;
        }

        self.emit_assignment_started(drv_hash, executor_id);
        debug!(drv_hash = %drv_hash, executor_id = %executor_id, "assigned derivation to worker");
        metrics::counter!("rio_scheduler_assignments_total").increment(1);
        true
    }

    /// Phase 1 of [`assign_to_worker`](Self::assign_to_worker):
    /// Ready→Assigned transition + dispatch_wait metric + clear
    /// backoff. Returns `false` on TOCTOU (caller defers).
    fn transition_to_assigned(&mut self, drv_hash: &DrvHash, executor_id: &ExecutorId) -> bool {
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return true; // node gone — let downstream phases handle
        };
        // Transition FIRST so a rejected transition doesn't pollute
        // the dispatch_wait metric or clear ready_at.
        if let Err(e) = state.transition(DerivationStatus::Assigned) {
            // Not in Ready state (TOCTOU vs. the dispatch_ready
            // pre-check). Caller defers; next dispatch pass drops it
            // via the status != Ready guard.
            warn!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                current = ?state.status(),
                error = %e,
                "Ready->Assigned transition rejected in assign_to_worker (TOCTOU)"
            );
            metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "assigned")
                .increment(1);
            return false;
        }
        // Record dispatch wait (Ready -> Assigned time). Fed from
        // `ready_at` (set on transition→Ready in DerivationState).
        if let Some(ready_at) = state.ready_at.take() {
            metrics::histogram!("rio_scheduler_dispatch_wait_seconds")
                .record(ready_at.elapsed().as_secs_f64());
        }
        // Clear retry-backoff: dispatch_ready wouldn't have let us
        // here unless honored. Next failure recomputes from the
        // (incremented) retry_count.
        state.retry.backoff_until = None;
        state.assigned_executor = Some(executor_id.clone());
        true
    }

    /// Phase 2 of [`assign_to_worker`](Self::assign_to_worker): record
    /// the assignment everywhere except the worker stream — PG status,
    /// PG `assignments` row, in-mem `worker.running_build`, GC
    /// `scheduler_live_pins`. All best-effort (log+continue). Inverse
    /// is [`rollback_assignment`](Self::rollback_assignment).
    // r[impl sched.gc.live-pins]
    async fn record_assignment(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        generation: u64,
    ) {
        self.persist_status(drv_hash, DerivationStatus::Assigned, Some(executor_id))
            .await;

        // PG BIGINT is signed; cast at THIS boundary, not at the
        // proto-encode sites (hotter). Best-effort: log+continue.
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, executor_id, generation as i64)
                .await
        {
            error!(drv_hash = %drv_hash, executor_id = %executor_id, error = %e,
                   "failed to insert assignment record");
        }

        // has_capacity() (running_build.is_none()) was checked by
        // hard_filter, so this never overwrites a live assignment.
        if let Some(worker) = self.executors.get_mut(executor_id) {
            debug_assert!(
                worker.running_build.is_none(),
                "assign_to_worker called for busy executor (hard_filter gap?)"
            );
            worker.running_build = Some(drv_hash.clone());
        }

        // Auto-pin input-closure paths to scheduler_live_pins so GC's
        // mark CTE protects them. Same closure approximation as
        // send_prefetch_hint. Best-effort; 24h grace is fallback.
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if !input_paths.is_empty()
            && let Err(e) = self.db.pin_live_inputs(drv_hash, &input_paths).await
        {
            debug!(drv_hash = %drv_hash, error = %e,
                   "failed to pin live inputs (best-effort; grace period is fallback)");
        }
    }

    /// Phase 3a of [`assign_to_worker`](Self::assign_to_worker):
    /// `try_send` the proto onto the worker's bidi stream. `false` if
    /// the channel is full/closed (caller rolls back).
    ///
    /// If the worker has no `stream_tx` (or vanished from the map),
    /// returns `true` WITHOUT sending — preserves pre-refactor behavior
    /// where the if-let chain fell through. The actor is
    /// single-threaded so an executor selected by `best_executor` can't
    /// disappear before this point; the fall-through is unreachable in
    /// practice but kept verbatim. A `debug_assert!` flags it in tests.
    fn try_send_assignment(
        &self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        assignment: rio_proto::types::WorkAssignment,
    ) -> bool {
        let Some(tx) = self
            .executors
            .get(executor_id)
            .and_then(|w| w.stream_tx.as_ref())
        else {
            debug_assert!(
                false,
                "selected executor {executor_id} has no stream_tx at send time"
            );
            return true;
        };
        let msg = rio_proto::types::SchedulerMessage {
            msg: Some(rio_proto::types::scheduler_message::Msg::Assignment(
                assignment,
            )),
        };
        if let Err(e) = tx.try_send(msg) {
            warn!(executor_id = %executor_id, drv_hash = %drv_hash, error = %e,
                  "failed to send assignment to worker");
            return false;
        }
        true
    }

    /// Phase 3b of [`assign_to_worker`](Self::assign_to_worker):
    /// inverse of [`record_assignment`](Self::record_assignment) +
    /// [`transition_to_assigned`](Self::transition_to_assigned). Clears
    /// `worker.running_build`, resets state to Ready, unpins, deletes
    /// the PG assignments row, emits progress so the dashboard sees the
    /// rollback. Do NOT re-queue here — channel is still full; caller's
    /// `ctx.deferred` handles that next pass.
    async fn rollback_assignment(&mut self, drv_hash: &DrvHash, executor_id: &ExecutorId) {
        // Worker tracking (set in record_assignment). Without this the
        // worker appears busy → phantom capacity leak.
        if let Some(worker) = self.executors.get_mut(executor_id)
            && worker.running_build.as_ref() == Some(drv_hash)
        {
            worker.running_build = None;
        }
        // Assigned -> Ready. Caller (dispatch_ready) defers; next pass
        // retries.
        if let Some(state) = self.dag.node_mut(drv_hash)
            && let Err(e) = state.reset_to_ready()
        {
            // Already transitioned to Assigned, can't reset. Orphaned
            // in Assigned with no worker building. Heartbeat reconcile
            // may eventually catch this — visible hang until then.
            error!(
                drv_hash = %drv_hash,
                executor_id = %executor_id,
                current = ?state.status(),
                error = %e,
                "reset_to_ready failed after assignment send failure; derivation orphaned in Assigned"
            );
            metrics::counter!("rio_scheduler_transition_rejected_total", "to" => "ready_reset")
                .increment(1);
        }
        // PG cleanup (inverse of record_assignment):
        //   - unpin: pin_live_inputs wrote scheduler_live_pins rows;
        //     leak until terminal cleanup if not undone.
        //   - delete_latest_assignment: insert_assignment wrote a
        //     'pending' row; misleading on recovery.
        self.unpin_best_effort(drv_hash).await;
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self.db.delete_latest_assignment(db_id).await
        {
            warn!(drv_hash = %drv_hash, error = %e,
                  "delete_latest_assignment failed during try_send rollback");
        }
        // Was Assigned (counted in running), now Ready (queued).
        for build_id in self.get_interested_builds(drv_hash) {
            self.emit_progress(build_id);
        }
    }

    /// Phase 4 of [`assign_to_worker`](Self::assign_to_worker): emit
    /// `DerivationStarted` + progress to interested gateways.
    fn emit_assignment_started(&mut self, drv_hash: &DrvHash, executor_id: &ExecutorId) {
        let drv_path = self.dag.path_or_hash_fallback(drv_hash);
        for build_id in self.get_interested_builds(drv_hash) {
            self.events.emit(
                build_id,
                rio_proto::types::build_event::Event::Derivation(
                    rio_proto::types::DerivationEvent::started(
                        drv_path.clone(),
                        executor_id.to_string(),
                    ),
                ),
            );
            // Progress snapshot: running count +1, worker set changed.
            // Critpath unchanged on dispatch (no completion) — but the
            // dashboard also uses Progress for running/queued columns.
            self.emit_progress(build_id);
        }
    }

    /// Construct the [`WorkAssignment`] proto for `drv_hash` →
    /// `executor_id`: CA-input resolve, HMAC token sign, build-options
    /// lookup. Side-effect: stashes `pending_realisation_deps` on the
    /// node so `handle_success_completion` can write the realisation FK
    /// rows post-build.
    ///
    /// Returns `None` if the DAG node is gone (TOCTOU vs. concurrent
    /// cancel) — caller treats that as assignment failure.
    ///
    /// [`WorkAssignment`]: rio_proto::types::WorkAssignment
    async fn build_assignment_proto(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        generation: u64,
    ) -> Option<rio_proto::types::WorkAssignment> {
        // CA input resolution: rewrite placeholder paths in
        // env/args/builder to realized output paths before
        // dispatch. Fires when gateway set needs_resolve (ADR-018
        // Appendix B: floating-CA self OR ia.deferred — IA drv
        // with a floating-CA input).
        //
        // `maybe_resolve_ca` returns the (possibly rewritten)
        // drv_content PLUS the realisation lookups performed. On
        // resolve error (missing realisation, PG blip) it logs and
        // returns the original unresolved bytes + empty lookups —
        // the worker's build fails on the placeholder path, which
        // is the correct signal (retry after the realisation lands).
        //
        // The resolve runs in its OWN scoped borrow of `self.dag`
        // (node() + collect_ca_inputs both &-borrow) so the lookups
        // can be stashed via node_mut() below before the main
        // WorkAssignment construction takes its own & borrow.
        let (drv_content_to_send, resolve_lookups) = {
            let state = self.dag.node(drv_hash)?;
            self.maybe_resolve_ca(drv_hash, state).await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        if !resolve_lookups.is_empty()
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.ca.pending_realisation_deps = resolve_lookups;
        }

        let state = self.dag.node(drv_hash)?;
        let build_opts = self.build_options_for_derivation(drv_hash);

        // Assignment token: HMAC-signed if configured, else
        // legacy format-string. The store verifies signed
        // tokens on PutPath (prevents arbitrary-path upload
        // from a compromised worker). Unsigned tokens are
        // accepted by a store with hmac_verifier=None (dev).
        //
        // Expiry: 2× build_timeout (or 2× daemon_timeout
        // default if timeout=0). A worker legitimately
        // uploading after completion is well within that
        // window. Prevents replay from a leaked token later.
        let assignment_token = if let Some(signer) = &self.hmac_signer {
            let timeout_secs = if build_opts.build_timeout > 0 {
                build_opts.build_timeout
            } else {
                // Match rio-builder's DEFAULT_DAEMON_TIMEOUT.
                // Can't reference the const cross-crate, so
                // duplicate the value. 7200s = 2h.
                7200
            };
            // Clamp BEFORE saturating_mul: a client sending
            // build_timeout=u64::MAX would get saturating_mul
            // → u64::MAX → expiry_unix = u64::MAX = immortal
            // token. A leaked immortal token defeats the
            // replay-prevention purpose of expiry entirely.
            // 7 days max: well above any real build duration.
            const MAX_HMAC_TIMEOUT_SECS: u64 = 7 * 86400;
            let timeout_secs = timeout_secs.min(MAX_HMAC_TIMEOUT_SECS);
            let expiry_unix = std::time::SystemTime::now()
                .duration_since(std::time::UNIX_EPOCH)
                .map(|d| d.as_secs())
                .unwrap_or(0)
                .saturating_add(timeout_secs.saturating_mul(2));
            signer.sign(&rio_auth::hmac::AssignmentClaims {
                executor_id: executor_id.to_string(),
                drv_hash: drv_hash.to_string(),
                expected_outputs: state.expected_output_paths.clone(),
                // Floating-CA: output path is computed post-build
                // from the NAR hash, so expected_output_paths is
                // [""] here. Store skips the path-in-claims check
                // when is_ca is set (verify-on-put still hashes
                // the NAR independently; threat model holds).
                // Fixed-output CA (FOD) has a known path → treat
                // as IA for the membership check.
                is_ca: state.ca.is_ca && !state.is_fixed_output,
                expiry_unix,
            })
        } else {
            // Legacy unsigned: format-string. Store with
            // hmac_verifier=None accepts this.
            format!("{executor_id}-{drv_hash}-{generation}")
        };

        Some(rio_proto::types::WorkAssignment {
            drv_path: state.drv_path().to_string(),
            // Forward what the gateway inlined (or empty → worker
            // fetches from store). Gateway only inlines for nodes
            // whose outputs are MISSING (will-dispatch), so cache
            // hits don't bloat this. Worker already handles both
            // paths (executor/mod.rs:241 branches on is_empty).
            // For CA-depends-on-CA derivations, this is the
            // RESOLVED ATerm (placeholders replaced by realized
            // paths) — see maybe_resolve_ca above.
            drv_content: drv_content_to_send,
            output_names: state.output_names.clone(),
            build_options: Some(build_opts),
            assignment_token,
            generation,
            is_fixed_output: state.is_fixed_output,
            traceparent: state.traceparent.clone(),
            // Intent for this drv (matches the SpawnIntent that spawned the
            // pod). Builder clamps to cgroup cpu.max so a wildcard worker
            // (different intent) still gets ground-truth. Populating this
            // makes resolve_build_opts override client `--cores N`, closing
            // the model-corruption vector where the build runs at N but
            // telemetry records cgroup limit.
            assigned_cores: state.sched.est_cores,
            assigned_mem_bytes: None,
            assigned_disk_bytes: None,
        })
    }

    /// Send a PrefetchHint for the chosen worker to warm its FUSE
    /// cache. Best-effort: `try_send`, failure logs at debug.
    ///
    /// Paths come from [`approx_input_closure`] (DAG children's
    /// expected outputs), truncated to `MAX_PREFETCH_PATHS`. Under
    /// ephemeral one-build-per-pod the worker's cache is always
    /// empty, so the full closure is always sent — no per-worker
    /// filtering. Empty closure (leaf derivation) = don't send.
    ///
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn send_prefetch_hint(&self, executor_id: &ExecutorId, drv_hash: &DrvHash) {
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if input_paths.is_empty() {
            // Leaf derivation (no DAG children). Nothing to prefetch.
            // Common for .drv fetches and source tarballs.
            return;
        }

        let Some(worker) = self.executors.get(executor_id.as_str()) else {
            return;
        };

        // Cap: bound message size. A derivation with 200 deps ×
        // 3 outputs = 600 paths × ~80 bytes = 48 KB. Fine for gRPC
        // but let's not surprise anyone with a 1 MB hint for a
        // pathological case. 100 covers the 95th percentile; the
        // rest fetch on-demand (we cap by truncating, not by
        // "pick the best 100" — that would need per-path nar_size
        // which we don't have).
        let mut to_prefetch = input_paths;
        if to_prefetch.len() > super::MAX_PREFETCH_PATHS {
            to_prefetch.truncate(super::MAX_PREFETCH_PATHS);
        }

        if to_prefetch.is_empty() {
            return;
        }

        let hint_len = to_prefetch.len();
        let hint = rio_proto::types::PrefetchHint {
            store_paths: to_prefetch,
        };
        let msg = rio_proto::types::SchedulerMessage {
            msg: Some(rio_proto::types::scheduler_message::Msg::Prefetch(hint)),
        };

        // try_send: if the channel is full, drop the hint. The
        // assignment that follows uses the SAME channel — if it's
        // full, that assignment also fails and reset_to_ready cleans
        // up. If only this fails (race: channel had 1 slot, hint
        // lost, assignment fit), the build works without prefetch.
        // debug not warn: this is a hint, not a contract.
        if let Some(tx) = &worker.stream_tx {
            match tx.try_send(msg) {
                Ok(()) => {
                    metrics::counter!("rio_scheduler_prefetch_hints_sent_total").increment(1);
                    metrics::counter!("rio_scheduler_prefetch_paths_sent_total")
                        .increment(hint_len as u64);
                }
                Err(e) => {
                    debug!(
                        executor_id = %executor_id,
                        drv_hash = %drv_hash,
                        error = %e,
                        "prefetch hint dropped (channel full; assignment may also fail)"
                    );
                }
            }
        }
    }

    /// If this derivation is CA-floating with CA inputs, resolve
    /// placeholder paths to realized output paths before dispatch.
    /// Returns `(drv_content_bytes, realisation_lookups)`: the
    /// (possibly rewritten) ATerm plus every
    /// `(dep_modular_hash, dep_output_name) → realized_path` lookup
    /// the resolve performed. Caller stashes lookups on
    /// `DerivationState.ca.pending_realisation_deps` for the
    /// completion-time `insert_realisation_deps` call (the FK needs
    /// the parent's OWN realisation row to exist first).
    ///
    /// ADR-018 Appendix B: resolve fires when `needs_resolve` is set
    /// by the gateway — floating-CA self (`has_ca_floating_outputs`)
    /// OR any inputDrv is floating-CA (`ia.deferred`: an IA drv
    /// depending on a CA input has the CA placeholder embedded in
    /// its env/args). Fixed-output CA with no CA inputs doesn't need
    /// resolve — its output path AND its inputs' paths are all
    /// eval-time known.
    ///
    /// The resolve step queries the `realisations` table for each CA
    /// input's `(modular_hash, output_name)` → `output_path`, then
    /// string-replaces placeholders through the ATerm. Each lookup
    /// is staged for `realisation_deps` INSERT (rio's derived build
    /// trace, per ADR-018:45) — though the actual INSERT is deferred
    /// to completion time (the FK needs the parent's OWN realisation
    /// to exist, which only happens post-build).
    ///
    /// Error handling: resolve failure (missing realisation, PG blip)
    /// logs and returns the original unresolved bytes + empty lookups.
    /// The worker's build will then fail on the placeholder path not
    /// existing (`/1ril1qzj...` is not a real store path), triggering
    /// the normal retry-with-backoff. This is correct: a missing
    /// realisation means the input's `wopRegisterDrvOutput` hasn't
    /// landed yet (race), and retry-after-backoff gives it time to.
    async fn maybe_resolve_ca(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> (Vec<u8>, Vec<crate::ca::RealisationLookup>) {
        // Gate: ADR-018 Appendix B `shouldResolve`. Gateway computes
        // `needs_resolve = has_ca_floating_outputs() || any inputDrv
        // is floating-CA` at translate time. Covers both floating-CA
        // self AND ia.deferred (IA with CA inputs — the CA input's
        // placeholder is embedded in this drv's env/args and needs
        // rewriting to the realized path).
        if !state.ca.needs_resolve {
            return (state.drv_content.clone(), Vec::new());
        }

        // Build the input lists: walk DAG children, split into CA
        // and IA. For CA children we need the MODULAR hash (the
        // `realisations` table key, plumbed by the gateway via
        // `DerivationNode.ca.modular_hash`). For IA children we need
        // the `expected_output_paths` (deterministic, computed at
        // gateway submit time from the parsed `.drv`).
        //
        // Nix's `tryResolve` (derivations.cc:1206-1234) iterates ALL
        // inputDrvs regardless of addressing mode, adding each output
        // path to `inputSrcs`. CA outputs come from realisations; IA
        // outputs are concrete and already in the DAG.
        //
        // Floating-CA with NO inputs at all (rare: a leaf CA drv with
        // only fixed srcs) doesn't need resolve — nothing to collapse
        // into inputSrcs. Short-circuit before the ATerm parse.
        let ca_inputs = self.collect_ca_inputs(drv_hash);
        let ia_inputs = self.collect_ia_inputs(drv_hash);
        if ca_inputs.is_empty() && ia_inputs.is_empty() {
            return (state.drv_content.clone(), Vec::new());
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG, drv_content not persisted). The store
        // has the ATerm — fetch it. Workers do the same when the
        // inline is empty (build_types.proto:231: "Empty = fallback;
        // worker fetches via GetPath"). ~10-50ms round-trip, once
        // per recovered floating-CA dispatch.
        //
        // Checked AFTER the both-empty short-circuit: a recovered
        // floating-CA with no DAG inputs doesn't need resolve and
        // doesn't need the fetch — worker fetches the unresolved
        // `.drv` from the store itself (same path it always does
        // when `drv_content` is empty). Any floating-CA WITH inputs
        // (CA or IA) needs the scheduler-side fetch so
        // `resolve_ca_inputs` can parse `inputDrvs` and serialize
        // the resolved `BasicDerivation` form.
        //
        // The same lossy-on-recovery pattern still applies to
        // `ca_modular_hash` (see `collect_ca_inputs`'s skip-on-None)
        // and `pending_realisation_deps` (best-effort cache,
        // reconstituted here on each resolve).
        //
        // r[impl sched.ca.resolve+2]
        let drv_content = if state.drv_content.is_empty() {
            match self.fetch_drv_content_from_store(drv_hash, state).await {
                Some(bytes) => bytes,
                None => {
                    // Store unreachable or .drv not found — dispatch
                    // unresolved (worker fails on placeholder,
                    // self-heals via retry after a fresh SubmitBuild
                    // re-merges with inline drv_content). Same
                    // degrade as before P0408.
                    warn!(
                        drv_hash = %drv_hash,
                        "recovered CA-on-CA dispatch: drv_content empty + store fetch failed; \
                         dispatching unresolved (worker will fail on placeholder)"
                    );
                    return (state.drv_content.clone(), Vec::new());
                }
            }
        } else {
            state.drv_content.clone()
        };

        match crate::ca::resolve_ca_inputs(&drv_content, &ca_inputs, &ia_inputs, self.db.pool())
            .await
        {
            Ok(resolved) => {
                debug!(
                    drv_hash = %drv_hash,
                    n_ca_inputs = ca_inputs.len(),
                    n_ia_inputs = ia_inputs.len(),
                    n_lookups = resolved.lookups.len(),
                    "CA resolve: rewrote placeholders + collapsed inputSrcs for dispatch"
                );
                (resolved.drv_content, resolved.lookups)
            }
            Err(e) => {
                // Swallow-to-warn for ALL ResolveError variants,
                // including `Db` (transient PG blip). Rationale:
                // the unresolved dispatch → worker fails on the
                // placeholder path → retry-with-backoff fires →
                // next dispatch re-runs resolve. For
                // `RealisationMissing`, the backoff gives the
                // input's `wopRegisterDrvOutput` time to land
                // (race). For `Db`, the backoff IS the retry-PG
                // mechanism — the wasted worker cycle (~seconds
                // to fail on ENOENT) is acceptable vs adding a
                // defer-and-requeue path here (would need a timer
                // to re-dispatch, which `backoff_until` already
                // provides on the FAILURE path). Slot-wasteful
                // but correct; profiling can drive a `Db → defer`
                // split if the waste proves measurable.
                warn!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "CA resolve failed; dispatching unresolved (worker will fail on placeholder)"
                );
                // Return the (possibly fetched-from-store) bytes
                // unresolved. If the fetch succeeded but resolve
                // failed, the worker at least skips its own GetPath.
                (drv_content, Vec::new())
            }
        }
    }

    /// Fetch a derivation's ATerm bytes from the store via `GetPath`.
    ///
    /// The store returns NAR-framed bytes; a `.drv` is a single
    /// regular file, so [`rio_nix::nar::extract_single_file`] unwraps
    /// it to the raw ATerm. This is the same path the worker takes
    /// when `WorkAssignment.drv_content` is empty
    /// ([`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`]).
    ///
    /// Returns `None` on any failure: store unconfigured
    /// (`store_client = None`, test mode), `GetPath` error, timeout,
    /// not-found, or NAR unwrap failure. Callers treat `None` as
    /// "degrade to the pre-P0408 behavior" — dispatch unresolved,
    /// worker fails on placeholder, retry-with-backoff self-heals.
    ///
    /// Hard 2s timeout + 1 MiB NAR cap: a `.drv` is ~1-50 KB ASCII.
    /// A larger-than-1-MiB blob means something is badly wrong (the
    /// path isn't a `.drv`, or the store returned a closure NAR).
    /// Either way, bail — resolve can't parse a non-ATerm.
    async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &DrvHash,
        state: &crate::state::DerivationState,
    ) -> Option<Vec<u8>> {
        /// `.drv` NAR cap. ~1-50 KB typical; 1 MiB is ~20× any
        /// real-world `.drv`. Avoids pulling a multi-GB closure if
        /// the store path was mis-resolved.
        const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
        /// End-to-end `GetPath` + stream-drain timeout. ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let mut client = self.store_client.as_ref()?.clone();
        let drv_path = state.drv_path().to_string();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            &drv_path,
            FETCH_TIMEOUT,
            MAX_DRV_NAR_SIZE,
            None,
            &[],
        )
        .await;

        let nar = match result {
            Ok(Some((_info, nar))) => nar,
            Ok(None) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    "recovered CA resolve: .drv not found in store"
                );
                return None;
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return None;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → None.
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => Some(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                None
            }
        }
    }

    /// Collect CA inputs for resolve. Walks the DAG children (deps)
    /// and returns a `CaResolveInput` for each child with
    /// `is_ca = true` AND a populated `ca_modular_hash`.
    ///
    /// Children with `is_ca && ca_modular_hash.is_none()` are
    /// skipped — the gateway couldn't compute the modular hash
    /// (BasicDerivation fallback, or recovered state where the
    /// hash wasn't persisted). The parent's resolve is incomplete
    /// for that input → worker fails on the placeholder path →
    /// retry-with-backoff. The next SubmitBuild referencing this
    /// child re-merges the proto with a fresh `ca_modular_hash`.
    fn collect_ca_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::CaResolveInput> {
        // DAG children = dependencies (must complete before this drv).
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if !child.ca.is_ca {
                continue;
            }
            let Some(modular_hash) = child.ca.modular_hash else {
                // Gateway didn't populate (BasicDerivation fallback
                // OR recovered state). Skip — resolve is incomplete
                // for this input, worker fails on placeholder,
                // retry-with-backoff handles it. debug not warn:
                // recovered chains hit this legitimately; the
                // scheduler-restart-mid-CA-chain case is expected
                // to degrade to worker-retry, not spam logs.
                debug!(
                    drv_hash = %drv_hash,
                    child = %child_hash,
                    "collect_ca_inputs: child is CA but ca_modular_hash unset; \
                     resolve incomplete, worker will fail on placeholder"
                );
                continue;
            };
            inputs.push(crate::ca::CaResolveInput {
                drv_path: child.drv_path().to_string(),
                modular_hash,
                output_names: child.output_names.clone(),
            });
        }
        inputs
    }

    /// Collect IA (input-addressed) inputs for resolve. Walks the
    /// DAG children and returns an [`IaResolveInput`] for each child
    /// with `is_ca = false` AND non-empty `expected_output_paths`.
    ///
    /// IA output paths are deterministic — the gateway computed them
    /// at submit time from the parsed `.drv` and plumbed them via
    /// `DerivationNode.expected_output_paths`. No store RPC needed.
    /// This is the same field [`approx_input_closure`] reads for the
    /// prefetch hint, so the data is already live.
    ///
    /// Children with empty `expected_output_paths` (recovered state
    /// where the paths weren't persisted, or a proto without the
    /// field) are skipped — `resolve_ca_inputs` will log and skip
    /// the `inputSrcs` add for that input. The worker's FUSE layer
    /// on-demand-fetches regardless, so builds don't break; only
    /// resolved-drv-hash compat with Nix is affected.
    ///
    /// [`IaResolveInput`]: crate::ca::IaResolveInput
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn collect_ia_inputs(&self, drv_hash: &DrvHash) -> Vec<crate::ca::IaResolveInput> {
        let children = self.dag.get_children(drv_hash);
        let mut inputs = Vec::new();
        for child_hash in children {
            let Some(child) = self.dag.node(&child_hash) else {
                continue;
            };
            if child.ca.is_ca {
                // CA child with a modular hash — handled by
                // collect_ca_inputs via realisation lookup. But a CA
                // child WITHOUT a modular hash (recovered state,
                // BasicDerivation fallback) that HAS completed can
                // still contribute its realized output_paths here:
                // the resolve doesn't need the realisation table when
                // we already have the concrete path in-memory.
                if child.ca.modular_hash.is_some() || child.output_paths.is_empty() {
                    continue;
                }
                // Fall through: CA child, no modular hash, but
                // output_paths is populated (completed). Treat as IA
                // for the purpose of inputSrcs collection — the
                // realized path is just as concrete as an IA
                // expected_output_path.
            }
            // Prefer realized output_paths (filled on completion) over
            // expected_output_paths (filled at merge). For IA children
            // the two are equivalent; for the CA-no-hash fallthrough
            // above, only output_paths is usable (expected is [""]
            // for floating-CA).
            let paths = if !child.output_paths.is_empty() {
                &child.output_paths
            } else {
                &child.expected_output_paths
            };
            if paths.is_empty() {
                // Recovered node or proto without the field. Skip;
                // resolve_ca_inputs logs and skips the inputSrcs add.
                continue;
            }
            inputs.push(crate::ca::IaResolveInput {
                drv_path: child.drv_path().to_string(),
                output_names: child.output_names.clone(),
                output_paths: paths.clone(),
            });
        }
        inputs
    }

    // -----------------------------------------------------------------------
    // Queue priority helpers
    // -----------------------------------------------------------------------
    //
    // Pure DAG lookup helpers (`path_for_hash`, `hash_for_path`,
    // `path_or_hash_fallback`, `db_id_for_path`) live on
    // [`crate::dag::DerivationDag`]; the helpers below stay on
    // `DagActor` because they cross-reference `self.builds`.

    /// Whether any interested build for this derivation is interactive (IFD).
    /// Interactive derivations get a priority boost in the queue.
    fn should_prioritize(&self, drv_hash: &DrvHash) -> bool {
        self.get_interested_builds(drv_hash).iter().any(|build_id| {
            self.builds
                .get(build_id)
                .is_some_and(|b| b.priority_class.is_interactive())
        })
    }

    /// Compute the effective queue priority for a derivation: its
    /// critical-path priority + interactive boost if applicable.
    ///
    /// All queue pushes go through this. Replaces the old `push_front`/
    /// `push_back` split — interactive is now a number, not a position.
    ///
    /// Returns 0.0 if the node isn't in the DAG (stale hash). The
    /// caller probably shouldn't be pushing it, but 0.0 = lowest
    /// priority = harmless (stale entries get skipped on pop anyway
    /// if status != Ready).
    pub(super) fn queue_priority(&self, drv_hash: &DrvHash) -> f64 {
        let base = self
            .dag
            .node(drv_hash)
            .map(|n| n.sched.priority)
            .unwrap_or(0.0);
        if self.should_prioritize(drv_hash) {
            base + crate::queue::INTERACTIVE_BOOST
        } else {
            base
        }
    }

    /// Push a derivation onto the ready queue with its computed priority.
    /// Centralizes the priority lookup so call sites are simple.
    pub(super) fn push_ready(&mut self, drv_hash: DrvHash) {
        let prio = self.queue_priority(&drv_hash);
        self.ready_queue.push(drv_hash, prio);
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // check_freeze state machine. `backdate` (from actor/mod.rs) lets us
    // construct Instants in the past without waiting or mocking the clock.

    // r[verify sched.freeze-detector]
    #[test]
    fn check_freeze_starts_timer_on_first_freeze() {
        let mut since = None;
        check_freeze(&mut since, true, "fetcher", 41, 0);
        assert!(since.is_some(), "frozen=true with None → timer started");
    }

    #[test]
    fn check_freeze_thaw_resets_to_none() {
        let mut since = Some(backdate(30));
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "frozen=false → reset to None");

        // Also resets even if we were past the WARN threshold.
        let mut since = Some(backdate(120));
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "thaw wins regardless of elapsed");
    }

    #[test]
    fn check_freeze_keeps_counting_before_threshold() {
        let start = backdate(30);
        let mut since = Some(start);
        check_freeze(&mut since, true, "fetcher", 41, 0);
        assert_eq!(
            since,
            Some(start),
            "frozen but under 60s → unchanged (keep counting)"
        );
    }

    #[test]
    fn check_freeze_resets_timer_after_warn() {
        // Past the 60s threshold: the WARN fires and `since` is reset
        // to ~now for rate-limiting (once/minute, not once/pass).
        let start = backdate(61);
        let mut since = Some(start);
        check_freeze(&mut since, true, "fetcher", 41, 0);
        // Timer was reset: new Instant, strictly after the old one.
        let new = since.expect("still frozen → still Some");
        assert!(new > start, "rate-limit reset: new timer > old start");
        // And the reset is recent (within the last second — the call just happened).
        assert!(
            new.elapsed() < std::time::Duration::from_secs(1),
            "reset to ~now"
        );
    }

    #[test]
    fn check_freeze_noop_when_never_frozen() {
        let mut since = None;
        check_freeze(&mut since, false, "fetcher", 0, 5);
        assert!(since.is_none(), "never frozen → stays None");
    }
}
