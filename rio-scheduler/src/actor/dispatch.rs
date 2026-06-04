//! Ready-queue dispatch: assign ready derivations to available workers.

use std::collections::{HashMap, HashSet, VecDeque};
use std::sync::Arc;
use std::time::Instant;

use rio_common::limits::MAX_SUBSTITUTE_CLOSURE;

use uuid::Uuid;

use tracing::{debug, error, info, warn};

use rio_proto::types::FindMissingPathsRequest;

use crate::dag::ClosureEvidence;
use crate::state::{
    BuildStateExt, DerivationStatus, DrvHash, ExecutorId, effective_wanted,
    verifiable_wanted_paths, wanted_subset,
};

use super::DagActor;
#[cfg(test)]
use super::backdate;

/// Per-dispatch-pass accumulators + the per-pass solve-input snapshot,
/// threaded through [`DagActor::try_dispatch_one`]. The drain loop in
/// [`DagActor::dispatch_ready`] previously closed over five outer
/// `mut` locals; collecting them here lets the loop body extract as a
/// method without 6-positional-`&mut` signatures.
struct DispatchTickCtx {
    // ── per-PASS (survive outer-loop iterations) ───────────────────
    /// ONE snapshot of the shared solve inputs for the whole pass —
    /// every drv `try_dispatch_one` solves sees the SAME `(hw, cost,
    /// inputs_gen)`. Mirrors `compute_spawn_intents` (snapshot.rs's
    /// `solve_inputs()` hoist, with the explicit TOCTOU rationale): a
    /// per-drv re-read meant two drvs in one pass could see different
    /// `cheapest_h` if `spot_price_poller` wrote between them. Also
    /// caps the §13c-2 `class_ceiling_uncatalogued` gauge at one emit
    /// per pass instead of `O(|hw_classes| × |Ready|)` (r33 bug_013).
    hw: crate::sla::hw::HwTable,
    cost: crate::sla::cost::CostTable,
    inputs_gen: u64,
    /// Hashes the batch FOD pre-pass already checked (I-163).
    /// `try_dispatch_one` skips the per-FOD store RPC for these.
    batch_checked: HashSet<DrvHash>,
    /// Successful assign_to_worker calls (for the >1s debug log).
    n_assigned: u64,
    // ── per-ITERATION (cleared at top of each outer `while
    //    dispatched_any` cycle; the FINAL iteration's values feed
    //    publish_dispatch_gauges) ──────────────────────────────────
    /// Derivations that couldn't dispatch this iteration (backoff not
    /// elapsed, no eligible worker, or assignment send failed).
    /// Re-pushed onto the ready queue at end of each cycle.
    deferred: Vec<DrvHash>,
    /// Per-kind deferral counts (operator gauge:
    /// `rio_scheduler_queue_depth{kind}`).
    kind_deferred: HashMap<rio_proto::types::ExecutorKind, u64>,
    /// Ready drvs whose `system` is advertised by ZERO registered
    /// executors of the matching kind. Per-system count → gauge + a
    /// single WARN on first observation (operator action: add a pool).
    unroutable_systems: HashMap<String, u64>,
}

/// I-025 freeze detector: state machine that WARNs when derivations are
/// queued but zero streams of the matching kind exist for >60s.
///
/// The scheduler already surfaces this via the `_queue_depth{kind}` and
/// `_utilization{kind}` gauges, but metrics need a port-forward. A WARN
/// lands in `kubectl logs`. QA I-025: 20-minute freeze with zero
/// ERROR/WARN is operator-hostile — the scheduler knew, it just didn't
/// say.
///
/// Rate-limit: `since` is reset on each WARN so we emit once/minute, not
/// once/dispatch-pass (~once/tick = every 10s would spam).
///
/// Free function (not `&mut self`) so the call site can borrow
/// `&mut self.freeze_{builders,fetchers}_since` while also reading
/// `self.executors`.
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
                 `kubectl get pool -A` and `rio-cli executors`"
            );
            // Rate-limit: reset so we WARN once/minute, not once/pass.
            *since = Some(Instant::now());
        }
        (false, _) => *since = None,
        _ => {} // frozen but not yet 60s — keep counting
    }
}

/// Outcome of [`DagActor::build_assignment_proto`].
pub(super) enum AssignmentProtoOutcome {
    /// Assignment constructed; send it.
    Ready(Box<rio_proto::types::WorkAssignment>),
    /// DAG node vanished (TOCTOU vs. concurrent cancel) — caller
    /// defers with the legacy NO-rollback semantics.
    NodeGone,
    /// The store could not vouch for a bare store-backed node's claims
    /// (`sched.dispatch.claims-derived`): transient — caller rolls the
    /// assignment back and sets the dispatch backoff.
    Unavailable(&'static str),
    /// The store's verified bytes disprove the recorded claims, or are
    /// content-bound garbage that can never parse: permanent — caller
    /// rolls back and poisons.
    Forged(String),
    /// Claims verification is STRUCTURALLY impossible for this node
    /// (`StoreEvidenceOutcome::StructurallyUnverifiable`): permanent
    /// for retries — caller rolls back and poisons with the carried
    /// remediation (generated from the typed reason, fix-discipline
    /// R6) instead of livelocking through backoff.
    PermanentlyUnverifiable(String),
    /// Direct input identities are missing AFTER the persisted-row
    /// read-through (`sched.dispatch.claims-derived+5`, bug_029):
    /// NOT instant permanence — the missing identity is a fact about
    /// CURRENT state (a deeper submission, an upload, or a mid-merge
    /// row can supply it at any time), so the caller rolls back and
    /// charges the node's own bounded unseeded-inputs budget; only
    /// exhaustion poisons, with the carried post-read-through
    /// remediation. Pre-fix this population routed through
    /// PermanentlyUnverifiable: a deploy failover (which erases every
    /// completed input's residency at once) instantly poisoned honest
    /// in-flight builds through the claims gate.
    UnseededInputs(String),
}

/// Closed outcome of [`DagActor::fetch_drv_content_from_store`]
/// (round-17 bug_030). Permanence is typed at the fetch site so the
/// consumers (the store-evidence chokepoint in merge.rs and the
/// dispatch-time CA resolve) cannot fold a deterministic content-bound
/// denial into transient store silence — the fold is what burned the
/// claims-unavailable budget and poisoned blaming store health for a
/// fact no retry can change.
pub(super) enum DrvFetch {
    /// NAR fetched and unwrapped to the raw ATerm bytes.
    Bytes(Vec<u8>),
    /// The store could not vouch either way: unconfigured client,
    /// transport failure, timeout, absent path, or a NAR that is not
    /// a single regular file. TRANSIENT — the store may answer
    /// differently later.
    Silence,
    /// The transfer was DENIED before any chunk flowed: the path's
    /// declared NAR size exceeds the derivation-text class cap
    /// ([`rio_common::limits::MAX_DRV_NAR_BYTES`]). CONTENT-BOUND and
    /// deterministic — the named path's contents cannot shrink on
    /// retry. Reachable for paths that bypassed store admission (the
    /// substitution ingest route, round-17 merged_063) or whose
    /// PathInfo declares a hostile size.
    Denied {
        /// Declared NAR size that tripped the cap.
        got: u64,
        /// The class cap it exceeded.
        limit: u64,
    },
}

/// `DrvHash` → owned `String` (the domain node synth wants `String`).
fn state_drv_hash_string(h: &DrvHash) -> String {
    h.as_str().to_string()
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
        #[cfg(test)]
        self.test_counters
            .dispatch_ready_calls
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);

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
        let batch_checked = self.batch_probe_cached_ready().await;
        // r33 bug_013: ONE solve-input snapshot for the WHOLE pass —
        // hoisted here from `try_dispatch_one`'s per-drv body. After
        // the batch store RPC so the snapshot is as fresh as the pass
        // will get. See [`DispatchTickCtx`]'s field doc for the TOCTOU
        // + gauge-spam rationale.
        let (hw, cost, inputs_gen) = self.solve_inputs();
        let mut ctx = DispatchTickCtx {
            hw,
            cost,
            inputs_gen,
            batch_checked,
            n_assigned: 0,
            deferred: Vec::new(),
            kind_deferred: HashMap::new(),
            unroutable_systems: HashMap::new(),
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
            // Per-ITERATION accumulators: only the final iteration's
            // counts are the true end-of-pass backlog. Without the
            // clear, iter1 dispatching ≥1 drv → iter2 re-pops the same
            // deferred set and re-increments → gauges report ~2× true.
            // `ctx.deferred` is already per-iteration via `mem::take`
            // below; these two were not.
            ctx.kind_deferred.clear();
            ctx.unroutable_systems.clear();

            while let Some(drv_hash) = self.ready_queue.pop() {
                n_popped += 1;
                if self.try_dispatch_one(drv_hash, &mut ctx).await {
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

        self.publish_dispatch_gauges(ctx.kind_deferred, ctx.unroutable_systems);
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
    /// check, SLA solve, store short-circuit, executor placement,
    /// assign or defer or poison. Mutates `ctx` for deferral/count
    /// accumulators; returns `true` if progress was made (assigned, or
    /// short-circuited a Ready FOD from store) — drives the outer
    /// `dispatched_any` cycle.
    async fn try_dispatch_one(&mut self, drv_hash: DrvHash, ctx: &mut DispatchTickCtx) -> bool {
        // Stale-entry guards: drop if not in DAG or not Ready.
        let Some(state) = self.dag.node(&drv_hash) else {
            return false;
        };
        if state.status() != DerivationStatus::Ready {
            return false;
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
            return false;
        }

        // SLA-solved (cores, mem, disk, deadline) for the resource-fit
        // filter (`r[sched.assign.resource-fit]`): same
        // `solve_intent_for` the snapshot uses, so the controller
        // spawns and dispatch accepts the SAME shape. D2: FODs go
        // through the identical pipeline — the `hard_filter` kind gate
        // routes them to fetchers.
        //
        // `want_kind`/`system` captured into locals so the `state`
        // borrow ends here — `node_mut` below needs exclusive access
        // to `self.dag`. The `(hw, cost, inputs_gen)` snapshot is
        // PER-PASS, threaded via `ctx` from `dispatch_ready` (r33
        // bug_013) — same pattern as `compute_spawn_intents`.
        let intent = self.solve_intent_for(state, &ctx.hw, &ctx.cost, ctx.inputs_gen);
        let want_kind = crate::state::kind_for_drv(state.is_fixed_output);
        let system = state.system.clone();

        // Write the intent onto the state BEFORE placement so
        // `hard_filter` (via find_executor → best_executor) reads the
        // fresh value. Refreshed each dispatch pass — picks up
        // estimator Tick updates. ADR-023 phase-7: completion scores
        // actual-vs-predicted on the curve captured here, not whatever
        // the estimator has refit to since.
        if let Some(state) = self.dag.node_mut(&drv_hash) {
            state.sched.last_intent = Some(intent);
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
            return true;
        }

        // r[impl sched.merge.substitute-topdown+15]
        // Fail-open carve-out: a topdown-pruned node whose closure
        // evidence is Broken — childless or closure-holed, see
        // `must_substitute` — must never be handed to a worker: its dep
        // closure was never merged (or the surviving children are a
        // reap-truncated view of it), so a from-source build is doomed
        // (ENOENT on inputDrvs). Reaching this point still Ready means
        // the dispatch-time probes produced no definitive verdict for
        // it this pass (store RPC failed / timed out — every definitive
        // outcome completes it inline, routes it to substitution, or
        // fail-fasts it). Defer it for this pass instead of letting
        // the generic fail-open dispatch pick it up; the next pass
        // re-probes. All other nodes keep the existing fail-open
        // behaviour.
        if self.must_substitute(&drv_hash) {
            debug!(%drv_hash,
                   "topdown-pruned node with broken closure evidence has no store \
                    verdict this pass; deferring instead of dispatching from source");
            ctx.deferred.push(drv_hash);
            return false;
        }

        // Intent-match (worker spawned FOR this drv) first, else
        // best_executor over the kind-matching pool.
        match self.find_executor(&drv_hash) {
            Some(executor_id) => {
                if self.assign_to_worker(&drv_hash, &executor_id).await {
                    ctx.n_assigned += 1;
                    true
                } else {
                    // Assignment send failed (worker stream full or
                    // disconnected). Defer — retrying immediately in
                    // the same pass would spin: the channel won't
                    // drain until we yield to the runtime.
                    ctx.deferred.push(drv_hash);
                    false
                }
            }
            None => {
                // No eligible worker.
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
                if self.failed_builders_exhausts_fleet(&drv_hash) {
                    self.poison_and_cascade(&drv_hash, "failed on every eligible worker")
                        .await;
                    return false;
                }
                // I-056: distinguish "no capacity right now" (defer,
                // autoscaler handles it) from "no pool advertises this
                // system at all" (operator action — add the pool or
                // its `systems` entry). The latter sat silently Ready
                // for hours; surface it via gauge + a one-shot WARN.
                if !self.any_executor_advertises_system(&system, want_kind) {
                    // r[impl sched.dispatch.unroutable-system+2]
                    // `system` is tenant-supplied (raw drv.platform());
                    // bucket so the Prometheus label cardinality is
                    // bounded by the Nix system-string shape, not
                    // tenant input. Real-but-unrouted systems
                    // (`aarch64-linux` with no aarch64 pool) stay
                    // visible by name; garbage (`fake-{uuid}`)
                    // collapses to one `unknown` series.
                    let label_sys = if Self::is_plausible_system(&system) {
                        system
                    } else {
                        "unknown".to_string()
                    };
                    *ctx.unroutable_systems.entry(label_sys).or_insert(0) += 1;
                }
                // Defer and track by kind.
                *ctx.kind_deferred.entry(want_kind).or_insert(0) += 1;
                // I-056-style per-clause diagnostic: when there ARE
                // registered workers of the right kind but none
                // eligible, the freeze detectors above don't fire
                // (they key on stream_count==0), and the drv silently
                // defers forever. Dump per-worker rejection_reason so
                // `RUST_LOG=rio_scheduler=debug` names the gate.
                //
                // debug!, not info!: under ADR-023's one-shot-pod
                // ramp-up, N drvs sit Ready while N pods register
                // serially → ~N² emissions (every deferred drv on
                // every dispatch_ready pass), each carrying an M-entry
                // vec. INFO floods kubectl logs and buries the
                // freeze-detector WARN. The same diagnostic is
                // available on demand via InspectBuildDag.
                if tracing::enabled!(tracing::Level::DEBUG)
                    && let Some(state) = self.dag.node(&drv_hash)
                {
                    let reasons: Vec<_> = self
                        .executors
                        .values()
                        .filter(|w| w.kind == want_kind && w.is_registered())
                        .map(|w| {
                            (
                                w.executor_id.as_ref().to_string(),
                                crate::assignment::rejection_reason(w, state),
                            )
                        })
                        .collect();
                    if !reasons.is_empty() {
                        tracing::debug!(
                            drv_hash = %drv_hash,
                            ?reasons,
                            "no eligible executor; per-worker rejection reasons"
                        );
                    }
                }
                ctx.deferred.push(drv_hash);
                false
            }
        }
    }

    /// Per-kind deferral gauges + utilization + I-025 freeze-detector.
    /// Snapshot from one dispatch pass; next pass overwrites. Both
    /// kinds emit a value every pass (zero is a legitimate value) so
    /// Prometheus doesn't persist stale nonzero.
    // r[impl sched.freeze-detector]
    // r[impl sched.dispatch.unroutable-system+2]
    fn publish_dispatch_gauges(
        &mut self,
        kind_deferred: HashMap<rio_proto::types::ExecutorKind, u64>,
        unroutable_systems: HashMap<String, u64>,
    ) {
        use rio_proto::types::ExecutorKind;
        for kind in [ExecutorKind::Builder, ExecutorKind::Fetcher] {
            let label = kind.as_str_name();
            let queued = kind_deferred.get(&kind).copied().unwrap_or(0);
            metrics::gauge!("rio_scheduler_queue_depth", "kind" => label).set(queued as f64);
            // I-048b: count only is_registered() executors. A heartbeat-
            // only zombie (stream_tx: None — race after scheduler
            // restart, fixed at the create-side in handle_heartbeat)
            // would inflate `total` here, hiding the freeze:
            // queue_depth>0 + util=0 + total>0 looks like "busy on
            // something else" when really nothing can dispatch.
            // Filtering by is_registered() makes the freeze detector
            // below fire on genuine no-stream-connected.
            let (busy, total) = self.executors.values().fold((0u32, 0u32), |(b, t), e| {
                if e.kind == kind && e.is_registered() {
                    (b + u32::from(e.running_build.is_some()), t + 1)
                } else {
                    (b, t)
                }
            });
            // No executors of this kind → emit 0.0 (not NaN). An
            // operator seeing queue_depth > 0 AND utilization == 0
            // with no executors registered knows the pool isn't
            // deployed.
            let util = if total > 0 {
                f64::from(busy) / f64::from(total)
            } else {
                0.0
            };
            metrics::gauge!("rio_scheduler_utilization", "kind" => label).set(util);

            // I-025 freeze detector: WARN if queue pressure + zero
            // streams >60s.
            let since = match kind {
                ExecutorKind::Builder => &mut self.freeze_builders_since,
                ExecutorKind::Fetcher => &mut self.freeze_fetchers_since,
            };
            check_freeze(
                since,
                queued > 0 && total == 0,
                label,
                queued,
                total as usize,
            );
        }

        // Unroutable-system gauge + edge-triggered WARN. Zero stale
        // labels first (gauges PERSIST in Prometheus until
        // overwritten), then set this pass's counts.
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

    /// Any registered executor of `kind` advertises `system`. Ignores
    /// busy/warm — distinguishes "no capacity right now" (transient,
    /// autoscaler handles it) from "no such pool exists" (operator
    /// action; the I-056 silent-stuck case).
    fn any_executor_advertises_system(
        &self,
        system: &str,
        kind: rio_proto::types::ExecutorKind,
    ) -> bool {
        self.executors
            .values()
            .any(|w| w.kind == kind && w.is_registered() && w.systems.iter().any(|s| s == system))
    }

    /// True if `system` matches the Nix `<arch>-<os>` shape (short,
    /// `[a-z0-9_-]` only). Used to bucket the tenant-supplied `system`
    /// label on `rio_scheduler_unroutable_ready` — anything outside
    /// this shape is collapsed to `"unknown"` so a tenant submitting
    /// `system = "fake-{uuid}"` can't mint unbounded Prometheus series.
    /// 32 covers the longest real Nix systems (`aarch64-unknown-linux-
    /// gnu` style is not used by Nix; the longest in nixpkgs lib.systems
    /// is well under).
    fn is_plausible_system(system: &str) -> bool {
        system.len() <= 32
            && system
                .chars()
                .all(|c| c.is_ascii_lowercase() || c.is_ascii_digit() || c == '-' || c == '_')
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
    /// Returns the set of hashes the drain loop must skip
    /// `ready_check_or_spawn` for (I-163). On success this is the
    /// batch-probed head (completed here or definitively found-missing
    /// one RPC ago) plus the truncated tail (deferred to next pass's
    /// batch). On RPC error/timeout this is the tail only — the
    /// stamped head is protected via `probed_generation`, so neither
    /// hits the per-drv fallback.
    // r[impl sched.dispatch.fod-substitute+2]
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
                    && s.output_paths_probeable()
            })
            .map(|(h, s)| (DrvHash::from(h), s.expected_output_paths.clone()))
            .collect();
        if candidates.is_empty() {
            return HashSet::new();
        }
        // Belt under the store-side 4096 cap. The truncated tail is
        // inserted into `checked` (so the drain loop skips the per-drv
        // `ready_check_or_spawn` fallback) but NOT stamped with
        // `probed_generation` — the next inline `dispatch_ready` (same
        // generation) batch-probes that window. Letting the tail fall
        // through to the per-drv path would be O(N) sequential 30s-
        // timeout RPCs in the actor (24h+ stall with a wide layer and
        // an unreachable store; I-139/I-140 invariant).
        let mut checked = HashSet::with_capacity(candidates.len());
        if candidates.len() > super::DISPATCH_PROBE_BATCH_CAP {
            for (h, _) in &candidates[super::DISPATCH_PROBE_BATCH_CAP..] {
                checked.insert(h.clone());
            }
            candidates.truncate(super::DISPATCH_PROBE_BATCH_CAP);
        }
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
        let auth = self.probe_substitute_auth(candidates.iter().map(|(h, _)| h));
        let probe = auth.mint();
        let probe_meta: Vec<(&'static str, &str)> =
            probe.iter().map(|(k, v)| (*k, v.as_str())).collect();

        let store_paths: Vec<String> = candidates
            .iter()
            .flat_map(|(_, p)| p.iter().cloned())
            .collect();
        // Deliberately NOT gated on `cache_breaker`: dispatch-time
        // probe failure degrades to cache-miss (per-drv fallback
        // retries), not StoreUnavailable. The breaker is for merge-time
        // admission only — here the call IS the work.
        let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
        Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
        let fmp_start = Instant::now();
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
                         dispatching fail-open (next pass batch-retries)"
                    );
                    // Tail already in `checked`; head protected via the
                    // probed_generation stamp at `ready_check_or_spawn`.
                    self.credit_heartbeats_for_stall(fmp_start.elapsed());
                    return checked;
                }
                Err(_) => {
                    debug!(
                        candidates = candidates.len(),
                        timeout = ?self.grpc_timeout,
                        "batched Ready store-check timed out; \
                         dispatching fail-open (next pass batch-retries)"
                    );
                    self.credit_heartbeats_for_stall(fmp_start.elapsed());
                    return checked;
                }
            };
        // Actor was unresponsive for the FMP duration — credit so the
        // queued Tick doesn't reap the fleet on a slow store.
        self.credit_heartbeats_for_stall(fmp_start.elapsed());

        // r[impl sched.substitute.detached+5]
        // Partition: locally-present (not in missing_paths) → complete
        // inline; substitutable → spawn detached fetch; truly-missing →
        // leave Ready (dispatches normally). The detached fetch runs
        // OUTSIDE the actor loop — before this, the awaited
        // eager_substitute_fetch blocked MergeDag/dispatch for >100s
        // when the closure walk pulled ghc-sized NARs.
        let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
        let substitutable: HashSet<String> = resp.substitutable_paths.into_iter().collect();
        let indeterminate: HashSet<String> = resp.indeterminate_paths.into_iter().collect();
        // I-139: collect-then-batch. The locally-present branch awaited
        // `complete_ready_from_store` per item (≥3 sequential PG RTTs
        // each); on warm-restart of a large closure ~all 2048 candidates
        // hit it → 12-30s actor stall → heartbeats missed → live workers
        // reaped. The Substituting branch already batched.
        let mut locally_present = Vec::new();
        let mut to_spawn = Vec::new();
        // Topdown-pruned roots with broken closure evidence (childless
        // or closure-holed) whose wanted set can neither complete
        // inline nor route to substitution: fail fast instead of
        // leaving them Ready (see the arm below).
        let mut to_fail_fast: Vec<DrvHash> = Vec::new();
        for (drv_hash, paths) in candidates {
            checked.insert(drv_hash.clone());
            let substitute_tried = self.dag.node(&drv_hash).is_some_and(|s| s.substitute_tried);
            // r[impl sched.merge.wanted-outputs+2]
            // Demand-driven completeness: only the WANTED outputs must
            // be present (→ complete inline) or present-or-
            // substitutable (→ detached fetch). A missing output
            // nothing consumes must not force a from-source dispatch.
            // The wanted slice is the LIVE effective wanted set
            // (`effective_wanted` over live interested builds'
            // contributions; a terminal build's wants stop counting),
            // falling back to the stored node-level union when it is
            // unavailable. `verifiable_wanted_paths` returns None for a
            // wanted set that resolves to no verifiable path; degrade
            // to all of `paths` then (and for a node that vanished from
            // the DAG mid-probe). The probe set and the `to_spawn` walk
            // seeds stay ALL expected paths (opportunistic completeness
            // — fetch the unwanted output too if the upstream has it).
            let wanted: Vec<String> = self
                .dag
                .node(&drv_hash)
                .and_then(|s| {
                    let eff = effective_wanted(s, &self.builds);
                    verifiable_wanted_paths(
                        &s.output_names,
                        &s.expected_output_paths,
                        eff.as_deref().unwrap_or(&s.wanted_output_names),
                    )
                    .map(|w| w.into_iter().map(str::to_owned).collect::<Vec<String>>())
                })
                .unwrap_or_else(|| paths.clone());
            if wanted.iter().all(|p| !missing.contains(p)) {
                // `substitute_tried` ⇒ the closure walk ingested the
                // seed (output) then failed on a ref — output-present
                // in PG does NOT imply closure-complete. FMP probes
                // output paths only, so "present" here can hide a
                // hole. Fall through to dispatch (build re-derives the
                // full closure) instead of marking Completed.
                if !substitute_tried {
                    locally_present.push(drv_hash);
                }
            } else if !substitute_tried
                && wanted.iter().all(|p| {
                    !missing.contains(p) || substitutable.contains(p) || indeterminate.contains(p)
                })
            {
                // r[impl sched.merge.substitute-probe-indeterminate]
                // Indeterminate treated optimistically — same as
                // merge.rs. The closure walk's failure path falls
                // through to build via `substitute_tried`.
                to_spawn.push((drv_hash, paths));
            } else if self.must_substitute(&drv_hash) {
                // r[impl sched.merge.substitute-topdown+15]
                // Truly missing (a wanted output is missing upstream and
                // not substitutable): every other node is left Ready and
                // dispatches from source. A topdown-pruned root whose
                // closure evidence is Broken (childless or closure-holed)
                // must not — its dep closure was never merged, so a
                // from-source dispatch is doomed (worker ENOENTs on
                // inputDrvs). This is the post-failover shape: the
                // recovered wanted union ('{}' = all declared) is wider
                // than the prune-time criterion, so an output the prune
                // never vouched for can be definitively missing here.
                // Fail fast with the resubmit-directing error instead.
                to_fail_fast.push(drv_hash);
            }
        }
        self.complete_ready_from_store_batch(&locally_present).await;
        self.spawn_substitute_fetches(to_spawn, auth).await;
        for drv_hash in &to_fail_fast {
            self.fail_fast_topdown_pruned_root(
                drv_hash,
                "wanted output(s) missing upstream and not substitutable at dispatch \
                 after deps were pruned",
            )
            .await;
        }
        checked
    }

    /// Resolve auth for dispatch-time store calls. `Jwt(vec![])`
    /// (no-auth) when no `service_signer` (dev mode) or no candidate
    /// has a known tenant (single-tenant mode / recovered orphan);
    /// otherwise `Service{signer, tenant_id}` so the detached
    /// closure-walk task can re-mint tokens past the original 60s.
    #[allow(clippy::extra_unused_lifetimes)] // 'a only in impl-Trait arg
    fn probe_substitute_auth<'a>(
        &self,
        drv_hashes: impl Iterator<Item = &'a DrvHash>,
    ) -> SubstituteAuth {
        let tid = drv_hashes
            .filter_map(|h| self.dag.node(h))
            .flat_map(|s| s.interested_builds.iter())
            .filter_map(|bid| self.builds.get(bid))
            .find_map(|b| b.tenant_id);
        self.substitute_auth_for_tenant(tid)
    }

    /// Build a [`SubstituteAuth`] for a known `tenant_id`. `Service`
    /// when both `service_signer` and `tenant_id` are present (so
    /// `walk_substitute_closure` can re-mint past the original
    /// expiry); `Jwt(vec![])` (no-auth) otherwise — dev mode or
    /// single-tenant. Used by both `probe_substitute_auth`
    /// (dispatch-time, derives `tenant_id` from the DAG) and
    /// merge-time (`MergeDagRequest.tenant_id` is already in hand).
    pub(super) fn substitute_auth_for_tenant(&self, tenant_id: Option<Uuid>) -> SubstituteAuth {
        match (&self.service_signer, tenant_id) {
            (Some(signer), Some(tid)) => SubstituteAuth::Service {
                signer: signer.clone(),
                tenant_id: tid,
            },
            _ => SubstituteAuth::Jwt(Vec::new()),
        }
    }

    fn inject_probe_meta(md: &mut tonic::metadata::MetadataMap, meta: &[(&'static str, &str)]) {
        for (k, v) in meta {
            if let Ok(mv) = tonic::metadata::MetadataValue::try_from(*v) {
                md.insert(*k, mv);
            }
        }
    }

    // r[impl sched.substitute.detached+5]
    /// Transition each candidate to `Substituting` and spawn a
    /// background task that triggers store-side `try_substitute` (via
    /// `QueryPathInfo`) for its output paths AND their transitive
    /// reference closure, then posts
    /// [`ActorCommand::SubstituteComplete`] back into the mailbox.
    ///
    /// Detaches the upstream NAR fetch from the actor event loop:
    /// before this, `eager_substitute_fetch` was awaited inline and a
    /// single ghc-sized closure walk blocked `MergeDag` for >100s
    /// (`"actor command exceeded 1s","cmd":"MergeDag","elapsed":"135s"`).
    ///
    /// Candidates whose transition is rejected (vanished, wrong status)
    /// are skipped — they fall through to normal scheduling.
    /// `auth` is the `(signer, tenant_id)` pair (both merge- and
    /// dispatch-time, via `substitute_auth_for_tenant`) so the spawned
    /// task can re-mint a fresh service token AFTER acquiring
    /// `substitute_sem`, once per BFS layer, and every
    /// `SUBSTITUTE_REMINT_PATHS` / `SUBSTITUTE_REMINT_INTERVAL` inside
    /// the per-path loop: a token minted at spawn-time would expire
    /// while parked on the semaphore or mid-way through a wide cold
    /// closure walk (later QPIs → `NotFound` → spurious `ok=false`).
    pub(super) async fn spawn_substitute_fetches(
        &mut self,
        candidates: Vec<(DrvHash, Vec<String>)>,
        auth: SubstituteAuth,
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
            // Live effective wanted set for the forgivable complement
            // below (terminal builds' contributions excluded; stored-
            // union fallback on None) — computed before the `node_mut`
            // borrow since it needs `self.builds`.
            let eff = self
                .dag
                .node(&drv_hash)
                .and_then(|s| effective_wanted(s, &self.builds));
            let Some(state) = self.dag.node_mut(&drv_hash) else {
                continue;
            };
            let from = match state.transition(DerivationStatus::Substituting) {
                Ok(f) => f,
                Err(e) => {
                    debug!(%drv_hash, %e, "spawn_substitute: transition rejected; falling through");
                    continue;
                }
            };
            // Stash the realized paths now so SubstituteComplete →
            // complete_ready_from_store_batch doesn't have to recompute
            // them. Dispatch-time callers pass `expected_output_paths`
            // (no-op semantically); the verify_preexisting_completed
            // reprobe lane passes the REALIZED floating-CA path which
            // would otherwise be lost (cleared at merge.rs reset, then
            // clobbered with `[""]` at the IA-only assignment below).
            state.output_paths = paths.clone();
            // I-094 reprobe substitutable lane: failure history is moot
            // — we're fetching the upstream-built output. Mirror the
            // poison-clear in apply_cached_hits. Clearing here (not on
            // SubstituteComplete{ok=true}) means a later fetch failure
            // demotes via `revert_target_for` (Ready/Queued/
            // DependencyFailed) and may get one more dispatch attempt
            // — acceptable, since substitutability is evidence the
            // world changed (Hydra/another tenant built it).
            //
            // Round-17 merged_bug_073: the gate is the shared revival
            // population (`is_revival_resettable`), not a per-site
            // subset — WIDENED from {Poisoned, DependencyFailed} to
            // include Failed, whose →Substituting arm the FSM has
            // always allowed; the old gate silently kept a
            // Failed-origin substitution's stale history. Observable
            // delta is confined to post-substitution-failure retry
            // budgets (one more dispatch attempt possible — the same
            // acceptability argument above). The PG tier resets below,
            // outside the node borrow.
            let revive = from.is_revival_resettable();
            if revive {
                state.retry.clear();
            }
            // r[impl sched.merge.wanted-outputs+2]
            // The forgivable seed subset: declared output paths whose
            // name is OUTSIDE the (non-empty) wanted set. The walk
            // still attempts them (opportunistic completeness) but
            // their failure must not demote the derivation to a
            // from-source build. Computed from the LIVE effective
            // wanted set (`effective_wanted` over live interested
            // builds' contributions, stored-union fallback) — the same
            // source the cache-hit classification uses — so a path is
            // only forgiven when NO LIVE build wants it; a terminal
            // build's wants stop pinning seeds as unforgivable. Empty
            // wanted set = all wanted = nothing forgivable (today's
            // behaviour). Only paths positively identifiable as
            // unwanted (declared in `expected_output_paths` but outside
            // the wanted subset) qualify; a seed that matches no
            // declared path (the realized floating-CA path from the
            // reprobe lane) is never forgiven, and neither is a path
            // that already triggered a forgiven-seed-became-wanted
            // downgrade (`never_forgive_paths` — see
            // `handle_substitute_complete`'s termination argument).
            //
            // The complement MUST be taken against the *verifiable*
            // wanted subset. A wanted set that resolves to no
            // verifiable path (`drv^bogus`) yields an EMPTY wanted set
            // here — and the complement of nothing is every declared
            // path, which forgives every seed failure and lets the
            // walk return ok=true having fetched NOTHING. On `None`
            // nothing is positively identifiable as unwanted, so
            // nothing is forgivable: every seed failure fails the walk
            // (the conservative pre-feature behaviour).
            let forgivable: HashSet<String> = match verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&state.wanted_output_names),
            ) {
                Some(wanted) => {
                    let wanted: HashSet<&str> = wanted.into_iter().collect();
                    state
                        .expected_output_paths
                        .iter()
                        .filter(|p| {
                            !p.is_empty()
                                && !wanted.contains(p.as_str())
                                && paths.contains(*p)
                                && !state.never_forgive_paths.contains(p.as_str())
                        })
                        .cloned()
                        .collect()
                }
                None => HashSet::new(),
            };
            let drv_path = state.drv_path().to_string();
            let interested = state.interested_builds.clone();
            // Best-effort PG tier of the revival reset (the SAME
            // population as the in-memory clear above — round-17
            // merged_bug_073) so recovery doesn't resurrect the moot
            // history. After last use of `state` so the &mut self.dag
            // borrow ends before &self.db. `clear_revival_history`
            // leaves `status` alone: PG keeps the origin status until
            // SubstituteComplete persists the outcome, and a failover
            // in the window recovers the origin with CLEAN history —
            // the I-094 re-probe lane re-discovers substitutability.
            // (The old `clear_poison` call's status='created' flip
            // bought nothing: both flows converge on re-probe.)
            if revive && let Err(e) = self.db.clear_revival_history(&drv_hash).await {
                warn!(%drv_hash, error = %e,
                      "failed to clear revival history in PG after re-probe substitutable hit");
            }
            let output_paths = paths.clone();
            let store = store.clone();
            let weak_tx = weak_tx.clone();
            let auth = auth.clone();
            let h = drv_hash.clone();
            let sem = self.substitute_sem.clone();
            let shutdown = self.shutdown.clone();
            rio_common::task::spawn_monitored("substitute-fetch", async move {
                // Bound in-flight closure walks across ALL spawned
                // tasks. The task is already spawned (so the actor
                // returned), but it parks here until a slot is free —
                // Substituting status keeps dependents gated meanwhile.
                // `auth.mint()` is deferred to inside the walk (per
                // layer) so time parked here doesn't eat token expiry.
                let _permit = sem.acquire_owned().await;
                // r[impl gw.activity.subst-progress]
                // Progress emits post back into the actor mailbox.
                // `try_send` (via the weak upgrade) — if the actor is
                // gone or its mailbox full, drop the emit (display-
                // only; SubstituteComplete below uses `send` so the
                // state transition is never lost to backpressure).
                let progress_tx = weak_tx.clone();
                let progress_hash = h.clone();
                let on_progress = move |done: u64, expected: u64, upstream: &str| {
                    if let Some(tx) = progress_tx.upgrade() {
                        let _ = tx.try_send(super::ActorCommand::SubstituteProgress {
                            drv_hash: progress_hash.clone(),
                            bytes_done: done,
                            bytes_expected: expected,
                            upstream_uri: upstream.to_string(),
                        });
                    }
                };
                let (ok, forgiven) = walk_substitute_closure(
                    &store,
                    paths,
                    &forgivable,
                    &auth,
                    &shutdown,
                    on_progress,
                )
                .await;
                if let Some(tx) = weak_tx.upgrade() {
                    let _ = tx
                        .send(super::ActorCommand::SubstituteComplete {
                            drv_hash: h,
                            ok,
                            forgiven,
                        })
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
            // build_summary counts Substituting as running — emit a
            // progress snapshot so the queued/running flip is visible
            // (matches `emit_assignment_started`). Dedup builds across
            // all spawned drvs; emit once per build.
            let interested_builds: HashSet<Uuid> = spawned
                .iter()
                .flat_map(|s| s.interested.iter().copied())
                .collect();
            for build_id in interested_builds {
                self.emit_progress(build_id);
            }
        }
    }

    // r[impl sched.substitute.detached+5]
    /// Handle a [`ActorCommand::SubstituteComplete`] posted by a
    /// detached fetch task. `ok=true` → output now in rio-store with
    /// its full reference closure ([`walk_substitute_closure`] walked
    /// it), so `Substituting → Completed` is safe even if inputDrvs
    /// aren't yet Completed in the DAG. `ok=false` → revert to
    /// `Ready`/`Queued` for normal scheduling.
    ///
    /// `forgiven` is the set of seeds the walk forgave against the
    /// wanted set *as snapshotted at spawn time*; see the re-check
    /// below for why it must be re-evaluated here.
    pub(super) async fn handle_substitute_complete(
        &mut self,
        drv_hash: &DrvHash,
        ok: bool,
        forgiven: &[String],
    ) {
        // r[impl sched.substitute.leader-gate]
        // `on_lose` only flips atomics; the detached `substitute-fetch`
        // task survives lease loss and posts here on the standby. The
        // ok=true branch writes PG (`persist_status(Completed)` /
        // `complete_ready_from_store`) → split-brain on
        // `derivations.status`. Same gate as `dispatch_ready` — the new
        // leader's recovery owns this drv (resets Substituting via the
        // dep-walk).
        if !self.leader.is_leader() {
            debug!(%drv_hash, ok,
                   "SubstituteComplete on standby (lease lost mid-fetch); dropping");
            return;
        }
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        if state.status() != DerivationStatus::Substituting {
            debug!(%drv_hash, status = ?state.status(),
                   "SubstituteComplete: not Substituting (cancelled/re-merged); dropping");
            return;
        }
        let topdown_pruned = state.topdown_pruned;
        // r[impl sched.merge.wanted-outputs+2]
        // The walk's forgiveness verdict was computed against the
        // wanted set as of SPAWN time. A build that merged during the
        // (potentially minutes-long) detached fetch can have made a
        // seed the walk forgave wanted by a live build — completing
        // now would hand that build a node missing an output it wants.
        // The re-check is evaluated against the LIVE effective wanted
        // set (`effective_wanted`, computed at this call site because
        // liveness needs `self.builds`, which the node cannot see),
        // falling back to the stored node-level union — same source as
        // the spawn-time forgivable complement. Downgrade to a revert
        // WITHOUT setting `substitute_tried`: the next dispatch pass
        // re-probes and re-spawns the walk with the corrected
        // forgivable set, so the delta is re-substituted (and a genuine
        // miss then fails the walk → `substitute_tried` → from-source
        // build). Two revert targets cannot wait for "the next pass" —
        // a topdown-pruned root with broken closure evidence (childless
        // or closure-holed) and a node whose dep is terminally failed —
        // so for those the walk is re-spawned immediately below instead
        // (see the downgrade re-spawn ahead of the generic revert).
        //
        // The trigger paths (forgiven at spawn time AND wanted now) are
        // collected — not just detected — so the downgrade can record
        // them in `never_forgive_paths` below.
        let forgiven_now_wanted_paths: Vec<String> = if ok && !forgiven.is_empty() {
            let eff = effective_wanted(state, &self.builds);
            wanted_subset(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&state.wanted_output_names),
            )
            .filter(|p| forgiven.contains(p))
            .cloned()
            .collect()
        } else {
            Vec::new()
        };
        let forgiven_now_wanted = !forgiven_now_wanted_paths.is_empty();
        let ok = ok && !forgiven_now_wanted;
        if forgiven_now_wanted {
            // Spend the trigger paths' forgiveness BEFORE any re-spawn
            // (immediate or next-pass): a path that has triggered a
            // downgrade once is never forgivable again for this node,
            // no matter how the live effective wanted set later shrinks
            // (the wanting build goes terminal) or re-grows. This is
            // the monotone step the walk chain's termination argument
            // rests on — see the downgrade re-spawn comment below.
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.never_forgive_paths
                    .extend(forgiven_now_wanted_paths.iter().cloned());
            }
            info!(%drv_hash,
                  "substitute walk forgave a seed that became wanted \
                   mid-fetch; reverting for re-substitution of the delta");
        }
        if ok {
            // The substitution chain ends in success — the spent-
            // forgiveness bookkeeping is scoped to the chain, so clear
            // it. Safe: no re-spawn follows the ok=true arm, and a NEW
            // chain only starts after an external reset (re-merge of a
            // failed node, stale-Completed verify, recovery), each of
            // which is bounded by the same |declared outputs| argument
            // again — clearing here cannot re-open an unbounded
            // downgrade loop.
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.never_forgive_paths.clear();
            }
            // complete_ready_from_store does Substituting→Completed
            // (valid transition) + the full post-completion machinery
            // (output_paths, persist, upsert_path_tenants, promote_
            // newly_ready, per-build events + completion check).
            self.complete_ready_from_store(drv_hash).await;
            // r[impl sched.dispatch.substitute-complete-inline]
            // promote_newly_ready pushed dependents to ready_queue at
            // probed_generation=0. Probe inline so the cascade
            // doesn't wait one Tick per layer; share the
            // BECAME_IDLE_INLINE_CAP budget — fresh-cluster
            // substitution can post thousands of these in a burst,
            // and uncapped inline dispatch is the I-163 storm shape.
            // Past the cap, `r[sched.admin.spawn-intents.probed-gate]`
            // still suppresses spurious intents; the dependent just
            // waits ≤1 Tick.
            if self.became_idle_inline_this_tick < super::BECAME_IDLE_INLINE_CAP {
                self.became_idle_inline_this_tick += 1;
                self.dispatch_ready().await;
            } else {
                self.dispatch_dirty = true;
            }
            return;
        }
        // r[impl sched.merge.substitute-topdown+15]
        // Topdown-pruned root: the dep subgraph was dropped from this
        // submission, so a build dispatch cannot succeed (worker
        // ENOENTs on inputDrvs). Fail every interested build with a
        // resubmit-directing error instead of demoting to Ready
        // (vacuously all_deps_completed → would dispatch). keep_going
        // is irrelevant: a topdown-pruned graph is roots-only by
        // construction; there is no other work to "keep going" with,
        // and leaving the build Active would hang it.
        //
        // The marked node's routing is keyed on its closure evidence
        // (`DerivationDag::closure_evidence`):
        //
        //  - Vouched (≥1 child, all produced, no closure hole): the
        //    "deps were dropped" invariant is moot — lazily clear the
        //    flag (memory + best-effort PG) and fall through to the
        //    normal revert. This backstops the stamp being conditional
        //    and the other clear sites (post-reconciliation,
        //    completion-time, recovery-time) having missed it.
        //  - Pending (children present but not all produced): a live
        //    flag alongside unbuilt children is normal (kept by
        //    design — they can still be reaped unbuilt). Suppress the
        //    fail-fast, keep the mark, and fall through to the normal
        //    Ready/Queued handling instead of collaterally failing a
        //    build whose node IS buildable once those children land.
        //  - Broken (childless or closure-holed): the child set must
        //    not vouch for a from-source dispatch — a closure-holed
        //    node's surviving children (left by a reap, a poison-clear
        //    removal, or a recovery edge-drop) are not representative
        //    of the pruned input closure, so neither the lazy clear nor
        //    the suppression may trust them. Take the fail-fast arm below
        //    (the bounded resubmit-directing outcome, never the doomed
        //    from-source dispatch a Ready revert would produce).
        //
        // The fail-fast arm additionally gates on
        // `!forgiven_now_wanted`: that downgrade means the fetch did
        // NOT definitively fail — it forgave a seed that has since
        // become wanted. Failing the build here would be premature;
        // fall through to the downgrade re-spawn below, which re-walks
        // immediately with the corrected forgivable set. A genuine
        // failure on that second walk lands back here with
        // `forgiven_now_wanted = false` and fails the build then.
        let evidence = self.dag.closure_evidence(drv_hash);
        if topdown_pruned && evidence == ClosureEvidence::Vouched {
            debug!(%drv_hash,
                   "topdown-pruned root's children are all produced; \
                    invariant moot, clearing flag (memory + PG)");
            if let Some(s) = self.dag.node_mut(drv_hash) {
                s.topdown_pruned = false;
            }
            // Best-effort PG counterpart of the in-memory clear:
            // this lazy clear backstops the children-produced-later
            // case (the completion-time clear in
            // `clear_topdown_pruned_for_produced_parents` is the
            // primary site), and the column is what a failover
            // restores — left set, a new leader would resurrect the
            // mark onto a node whose closure IS produced and a
            // later walk failure would wrongly fail-fast. Same
            // error posture as the fail-fast's clear: warn and
            // continue, never fail the handler.
            if let Err(e) = self.db.clear_topdown_pruned_by_hash(drv_hash).await {
                warn!(%drv_hash, error = %e,
                      "failed to clear persisted topdown_pruned after lazy clear (continuing)");
            }
        } else if topdown_pruned && evidence == ClosureEvidence::Pending {
            debug!(%drv_hash,
                   "topdown-pruned root has unbuilt DAG children; \
                    suppressing the fail-fast but keeping the mark \
                    (children can still be reaped unbuilt)");
        } else if topdown_pruned && !forgiven_now_wanted {
            self.fail_fast_topdown_pruned_root(
                drv_hash,
                "upstream substitute fetch failed after deps were pruned",
            )
            .await;
            return;
        }
        // 3-way revert (NOT 2-way Ready|Queued): the I-094 reprobe lane
        // can transition a node whose dep is Poisoned directly →
        // Substituting; on fetch-fail it must go DependencyFailed, not
        // Queued (Queued with a Poisoned dep is stuck forever — see
        // `revert_target_for`).
        let to = self.dag.revert_target_for(drv_hash);
        // Must-substitute judgment for the downgrade re-spawn's topdown
        // leg below: the flag now survives merges that add unbuilt
        // children, so "flagged" no longer implies "childless" — the
        // doomed-from-source shape this leg must catch is a marked node
        // whose closure evidence is Broken (childless OR closure-holed,
        // same predicate as the dispatch guards). A genuine failure on
        // such a node takes the fail-fast arm before reaching here;
        // this leg only matters for the forgiven-now-wanted downgrade.
        // Evaluated before `node_mut` because the `&mut` borrow is held
        // at the use site.
        let must_sub = self.must_substitute(drv_hash);
        let Some(state) = self.dag.node_mut(drv_hash) else {
            return;
        };
        // r[impl sched.merge.wanted-outputs+2]
        // Downgrade re-spawn: two revert targets must not wait for
        // "the next pass" to re-substitute the delta.
        //
        //  - `DependencyFailed` (a dep is Poisoned — the I-094 lane):
        //    the arm below runs `terminal_failure_epilogue`, terminally
        //    failing every interested build — including the build whose
        //    wanted subset WAS fully fetched and the build whose newly
        //    wanted seed never had a single real attempt (forgivable
        //    seeds are forgiven on their first failure). There is no
        //    "next pass" out of DependencyFailed.
        //  - a topdown-pruned root with broken closure evidence
        //    (childless or closure-holed — `must_substitute`): its dep
        //    closure was dropped from the submission and the current
        //    child set cannot vouch for it, so plain Ready without
        //    `substitute_tried` is a doomed from-source dispatch
        //    (worker ENOENTs on inputDrvs) if the next probe finds the
        //    now-wanted path definitively missing — the exact hazard
        //    the fail-fast arm above exists to prevent.
        //
        // For both, re-spawn the walk NOW: `spawn_substitute_fetches`
        // recomputes the forgivable set from the CURRENT effective
        // wanted set, so the newly-wanted delta gets a real,
        // retry-laddered attempt before any terminal verdict. If that
        // walk genuinely fails it lands back here with
        // `forgiven_now_wanted = false` and takes the normal arms
        // (fail-fast / epilogue). Terminates: every downgrade first
        // adds its trigger paths to `never_forgive_paths` (above), and
        // a path in that set is excluded from every later walk's
        // forgivable set within this chain — so it can never be
        // reported forgiven (and trigger another downgrade) again,
        // regardless of how the live effective wanted set shrinks (a
        // build goes terminal) and re-grows (a new build merges)
        // between walks. The trigger paths were forgivable, hence not
        // yet in the set, so within one chain the set only grows
        // between re-spawns and the chain takes at most |declared
        // outputs| downgrades. The set does NOT persist past the
        // chain: it is cleared at every chain ending (the ok=true
        // completion, the genuine-failure revert below, the topdown
        // fail-fast above, a worker-build verdict after a from-source
        // routing, any non-substitution completion — inline
        // store-batch, merge-time cached-hit, CA-cutoff Skip — or the
        // node being reset/removed, including the stale-Completed
        // reset that opens the next chain) — only this re-spawn and
        // the deferred next-pass delta re-walk retain it. A NEW chain
        // requires an external event (a later dispatch-pass or
        // merge-time classification spawning a fresh walk), which
        // re-establishes the same per-chain bound rather than
        // re-opening this one.
        //
        // The in-memory revert to `to` only satisfies the transition
        // table (there is no Substituting→Substituting); PG keeps
        // `Substituting`, which the re-spawn re-persists (one nuance:
        // for the DependencyFailed/Poisoned-origin arm the re-spawn's
        // best-effort `clear_poison` transiently writes status
        // 'created' to PG before that re-persist lands). The ordinary
        // Ready/Queued downgrade keeps today's behaviour (revert; the
        // next pass re-probes and re-substitutes — and a from-source
        // dispatch there is safe because the node's deps ARE in the
        // DAG and Completed). Falls through to that plain revert if the
        // store/self handles are gone (shutdown) — pre-fix behaviour.
        if forgiven_now_wanted
            && (to == DerivationStatus::DependencyFailed || must_sub)
            && !state.output_paths.is_empty()
            && self.store_client.is_some()
            && self.self_tx.is_some()
        {
            if let Err(e) = state.transition(to) {
                warn!(%drv_hash, %e, "SubstituteComplete downgrade: revert-for-respawn rejected");
                return;
            }
            let paths = state.output_paths.clone();
            info!(%drv_hash, revert_target = ?to,
                  "downgraded substitute completion would land in a \
                   terminal/fail-fast arm; re-spawning the walk with the \
                   corrected forgivable set");
            let auth = self.probe_substitute_auth(std::iter::once(drv_hash));
            self.spawn_substitute_fetches(vec![(drv_hash.clone(), paths)], auth)
                .await;
            return;
        }
        // One-shot fall-through: FMP said substitutable, QPI said no.
        // Next dispatch pass skips substitution and routes to a worker
        // — without this the partition re-includes it every Tick (~1/s
        // livelock; never reaches `find_executor`).
        //
        // EXCEPT for the forgiven-seed-became-wanted downgrade: there
        // the fetch did not definitively fail a wanted path (it never
        // tried the now-wanted one as wanted), so the next pass MUST
        // re-substitute with the corrected forgivable set. Bounded:
        // the trigger path is in `never_forgive_paths`, so that second
        // walk either succeeds or fails the now-unforgivable seed →
        // lands back here with `forgiven_now_wanted = false` → sets the
        // one-shot flag. (The hazardous DependencyFailed /
        // topdown-must-substitute targets never reach here — they
        // re-spawned above.)
        if !forgiven_now_wanted {
            state.substitute_tried = true;
        }
        // Chain-scope bookkeeping: the only way this chain continues
        // past this arm is that deferred delta re-substitution — the
        // downgrade reverting to Ready/Queued WITHOUT the one-shot
        // flag, so the next pass re-walks and `never_forgive_paths`
        // must survive into that walk. Every other way out ends the
        // chain (a genuine failure routes to a from-source build via
        // the one-shot flag; DependencyFailed is terminal), so the
        // spent-forgiveness set is cleared: leaking it into a LATER
        // substitution chain for this node (a stale-Completed reset, a
        // resubmit re-probe) would veto forgiving a path no live build
        // wants any more and turn a fully-substitutable node into a
        // from-source dispatch.
        if !(forgiven_now_wanted
            && matches!(to, DerivationStatus::Ready | DerivationStatus::Queued))
        {
            state.never_forgive_paths.clear();
        }
        if let Err(e) = state.transition(to) {
            warn!(%drv_hash, %e, "SubstituteComplete fail: revert rejected");
            return;
        }
        self.persist_status(drv_hash, to, None).await;
        match to {
            DerivationStatus::Ready => {
                self.push_ready(drv_hash.clone());
                self.dispatch_dirty = true;
            }
            DerivationStatus::DependencyFailed => {
                // Cascade + per-build completion-check so the interested
                // build terminates instead of hanging Active.
                self.terminal_failure_epilogue(
                    drv_hash,
                    "substitute fetch failed and a dependency is terminally failed",
                    rio_proto::types::BuildResultStatus::DependencyFailed,
                )
                .await;
            }
            _ => {}
        }
        // build_summary counts Substituting as running; revert flips
        // running→queued. Mirror `rollback_assignment` /
        // `emit_assignment_started` so the dashboard sees the demote.
        for build_id in self.get_interested_builds(drv_hash) {
            self.emit_progress(build_id);
        }
    }

    // r[impl sched.merge.substitute-topdown+15]
    /// Topdown-pruned fail-fast: the node's dep subgraph was dropped
    /// from its submission, so a from-source build dispatch cannot
    /// succeed (the worker ENOENTs on inputDrvs that were never
    /// merged). Fail every interested build with a resubmit-directing
    /// error and park the node instead of leaving it dispatchable.
    ///
    /// `cause` is the user-facing reason spliced into the build's
    /// error summary ("topdown-pruned root <hash>: <cause>; resubmit
    /// to re-probe or full-merge").
    ///
    /// Callers (all gate on `topdown_pruned` plus closure evidence the
    /// classifier judges `Broken` — childless or closure-holed, the
    /// `must_substitute` predicate — so a reap-truncated child set is
    /// treated exactly like an empty one at every site; unbuilt
    /// children (`Pending`) suppress the fail-fast but no longer shed
    /// the mark; only a `Vouched` child set — all produced, no hole —
    /// clears it):
    ///  - `handle_substitute_complete` on a failed/downgraded detached
    ///    fetch (`SubstituteComplete{ok=false}` after the
    ///    closure-evidence gate), the original home of this block;
    ///  - the dispatch-time probes (`batch_probe_cached_ready`,
    ///    `ready_check_or_spawn`) when a marked node with Broken
    ///    evidence can neither complete inline nor be routed to
    ///    substitution — the post-failover shape, where the recovered
    ///    (wider) wanted union contains an output that is genuinely
    ///    missing upstream. Pre-fix that outcome left the node Ready
    ///    and dispatched it from source — the doomed dispatch this arm
    ///    exists to prevent;
    ///  - the reap-time survivor re-evaluation in
    ///    `handle_cleanup_terminal_build` (leader-gated), which
    ///    additionally requires `substitute_tried` (the node's own walk
    ///    already failed) and a non-`Substituting` status (a walk in
    ///    flight keeps its chance) for a survivor the reap left
    ///    childless or closure-holed — nothing children-keyed would
    ///    ever re-evaluate it otherwise (`find_newly_ready` only fires
    ///    on completions).
    pub(super) async fn fail_fast_topdown_pruned_root(&mut self, drv_hash: &DrvHash, cause: &str) {
        // A prior iteration of the same dispatch pass may already have
        // settled this node: `batch_probe_cached_ready` collects the
        // whole `to_fail_fast` layer up front, and the first node's
        // `cancel_build_derivations` transitions every other
        // sole-interest not-yet-dispatched node of that build —
        // including later entries of the list — to DependencyFailed,
        // persists it, and strips the build's interest. Re-running the
        // park here would resurrect that terminal verdict
        // (DependencyFailed→Queued is a valid reprobe edge) and leave a
        // non-terminal, zero-interest, dep-less orphan behind in memory
        // and PG that no reap collects and every recovery reloads.
        // Equally nothing to do when the node vanished or no build is
        // interested any more — the verdicts are already settled. (The
        // SubstituteComplete{ok=false} caller never trips this: it only
        // reaches the helper for a node it just observed Substituting
        // with live interest.)
        let actionable = self
            .dag
            .node(drv_hash)
            .is_some_and(|s| !s.status().is_terminal() && !s.interested_builds.is_empty());
        if !actionable {
            debug!(%drv_hash,
                   "topdown fail-fast: node already terminal/orphaned (settled by an \
                    earlier iteration of this pass); skipping");
            return;
        }
        warn!(%drv_hash, cause,
              "topdown-pruned root cannot complete via substitution; \
               deps were dropped from DAG — failing build (resubmit \
               will re-probe or full-merge)");
        metrics::counter!("rio_scheduler_topdown_substitute_fail_total").increment(1);
        let msg =
            format!("topdown-pruned root {drv_hash}: {cause}; resubmit to re-probe or full-merge");
        // Queued (not Ready): zero DAG deps → vacuous Ready would
        // re-dispatch on the next Tick. cancel_build_derivations
        // strips interest below; with zero remaining interest the
        // node is reaped on the next sweep.
        let parked = if let Some(s) = self.dag.node_mut(drv_hash) {
            s.substitute_tried = true;
            // The chain ends here (terminal fail-fast): drop the
            // chain-scoped spent-forgiveness bookkeeping so it
            // cannot leak into a later substitution chain for this
            // node (e.g. after a resubmit re-probes it).
            s.never_forgive_paths.clear();
            // The fail-fast CONSUMES the pruned marker (here and, best-
            // effort, in PG below). The flag can be stale: a merge
            // transaction that committed but whose build activation
            // failed leaves it on shared rows; a node stamped while its
            // existing children were invisible (recovery drops edges to
            // completed children) reads as "childless" again after the
            // next failover; and a genuinely pruned leaf is never
            // cleared by a children-adding merge. Left in place, a
            // stale flag re-arms this fail-fast after EVERY failover
            // and wrongfully terminal-fails builds for a node that
            // could build from source. Clearing loses nothing: after
            // the park there is no surviving interest, and a
            // resubmitted genuinely-pruned root either re-prunes
            // (re-stamped — the retained breadcrumb below keeps a
            // reap-truncated survivor set from vouching) or full-merges
            // (children all produced ⇒ cleared).
            s.topdown_pruned = false;
            // Deliberately do NOT clear `closure_hole`: the directed
            // resubmit this fail-fast solicits goes through the
            // resubmit-reset, which keeps the (possibly truncated)
            // child edges and carries the breadcrumb, and the
            // re-pruning merge's stamp gates need it to avoid reading
            // produced survivors as Vouched — erasing it here would
            // launder that resubmit into the doomed from-source
            // dispatch one lifecycle step later (round-23 bug_006). An
            // unmarked node's hole has no consumer that can mis-fire
            // (every consumer also requires the mark); a later full
            // merge that re-declares the edges heals it.
            if let Err(e) = s.transition(DerivationStatus::Queued) {
                warn!(%drv_hash, %e, "topdown fail-fast: transition to Queued rejected");
            }
            true
        } else {
            false
        };
        // Persist + PG flag clear only for a node the block above
        // actually parked — never for one the early return skipped or
        // that vanished between the check and the mutation.
        if parked {
            self.persist_status(drv_hash, DerivationStatus::Queued, None)
                .await;
            // Best-effort PG counterpart of the in-memory mark clear
            // above (mark-only: the persisted closure_hole breadcrumb
            // is deliberately left set, mirroring the retention above).
            // A failure costs at most one more wrongful fail-fast cycle
            // after a later failover — never fail the actor command
            // over it.
            if let Err(e) = self.db.clear_topdown_pruned_by_hash(drv_hash).await {
                warn!(%drv_hash, error = %e,
                      "failed to clear persisted topdown_pruned after fail-fast (continuing)");
            }
        }
        for build_id in self.get_interested_builds(drv_hash) {
            if let Some(build) = self.builds.get_mut(&build_id) {
                build.error_summary.get_or_insert_with(|| msg.clone());
                build
                    .failed_derivation
                    .get_or_insert_with(|| drv_hash.to_string());
            }
            self.cancel_build_derivations(
                build_id,
                &format!("build {build_id}: topdown-pruned root: {cause}"),
            )
            .await;
            if let Err(e) = self.transition_build_to_failed(build_id).await {
                error!(%build_id, error = %e,
                       "failed to persist build-failed after topdown fail-fast");
            }
        }
    }

    // r[impl gw.activity.subst-progress]
    /// Relay byte-progress from a detached substitute fetch to every
    /// interested build via [`Event::SubstituteProgress`]. Display-only
    /// (routed through the log broadcast ring, not persisted, reuses
    /// last seq); the gateway translates to `actCopyPath` +
    /// `resProgress`. Non-leader emits are fine — this is read-only
    /// (no DAG/PG mutation).
    pub(super) fn handle_substitute_progress(
        &mut self,
        drv_hash: &DrvHash,
        bytes_done: u64,
        bytes_expected: u64,
        upstream_uri: String,
    ) {
        let Some(state) = self.dag.node(drv_hash) else {
            return;
        };
        let drv_path = state.drv_path().to_string();
        let interested = state.interested_builds.clone();
        let event = rio_proto::types::build_event::Event::SubstituteProgress(
            rio_proto::types::SubstituteProgress {
                derivation_path: drv_path,
                bytes_done,
                bytes_expected,
                upstream_uri,
            },
        );
        for build_id in interested {
            self.events.emit(build_id, event.clone());
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
        // Live effective wanted set (terminal builds' contributions
        // excluded; stored-union fallback on None) — computed before
        // the `node_mut` borrow below since it needs `self.builds`.
        let eff = self
            .dag
            .node(drv_hash)
            .and_then(|s| effective_wanted(s, &self.builds));
        let (paths, wanted, substitute_tried, mut store) = {
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
            if !state.output_paths_probeable() {
                return false;
            }
            state.probed_generation = probe_gen;
            let substitute_tried = state.substitute_tried;
            // r[impl sched.merge.wanted-outputs+2]
            // Demand-driven completeness: the probe set stays ALL
            // expected paths, but the present/substitutable verdicts
            // below are evaluated over the WANTED subset only — the
            // live effective wanted set computed above, with the stored
            // node-level union as the fallback.
            // `verifiable_wanted_paths` returns None for a wanted set
            // that resolves to no verifiable path; degrade to all
            // expected paths then — same shape as
            // `batch_probe_cached_ready`.
            let wanted: Vec<String> = verifiable_wanted_paths(
                &state.output_names,
                &state.expected_output_paths,
                eff.as_deref().unwrap_or(&state.wanted_output_names),
            )
            .map(|w| w.into_iter().map(str::to_owned).collect())
            .unwrap_or_else(|| state.expected_output_paths.clone());
            let Some(store) = &self.store_client else {
                return false;
            };
            (
                state.expected_output_paths.clone(),
                wanted,
                substitute_tried,
                store.clone(),
            )
        };
        // r[impl sched.dispatch.fod-substitute+2] — same probe-tenant
        // wiring as batch_probe_cached_ready.
        let auth = self.probe_substitute_auth(std::iter::once(drv_hash));
        let probe = auth.mint();
        let probe_meta: Vec<(&'static str, &str)> =
            probe.iter().map(|(k, v)| (*k, v.as_str())).collect();
        // Deliberately NOT gated on `cache_breaker`: per-drv fallback;
        // failure = cache-miss → dispatch normally.
        let mut req = tonic::Request::new(FindMissingPathsRequest {
            store_paths: paths.clone(),
        });
        Self::inject_probe_meta(req.metadata_mut(), &probe_meta);
        let fmp_start = Instant::now();
        match tokio::time::timeout(self.grpc_timeout, store.find_missing_paths(req)).await {
            Ok(Ok(r)) => {
                self.credit_heartbeats_for_stall(fmp_start.elapsed());
                let resp = r.into_inner();
                // Demand-driven completeness: only the WANTED outputs
                // must be present / present-or-substitutable. The
                // missing set is keyed off the full probe (all
                // expected paths); intersect it with the wanted
                // subset before deciding.
                let missing: HashSet<String> = resp.missing_paths.into_iter().collect();
                if wanted.iter().all(|p| !missing.contains(p)) {
                    // Same partial-closure gate as
                    // `batch_probe_cached_ready`: substitute_tried ⇒
                    // walk ingested seed then failed; output-present
                    // doesn't imply closure-complete. Fall through.
                    if substitute_tried {
                        return false;
                    }
                    self.complete_ready_from_store(drv_hash).await;
                    return true;
                }
                // r[impl sched.substitute.detached+5] — spawn instead of
                // awaiting eager_substitute_fetch in the actor loop.
                // r[impl sched.merge.substitute-probe-indeterminate]
                let sub: HashSet<String> = resp.substitutable_paths.into_iter().collect();
                let ind: HashSet<String> = resp.indeterminate_paths.into_iter().collect();
                if !substitute_tried
                    && wanted
                        .iter()
                        .all(|p| !missing.contains(p) || sub.contains(p) || ind.contains(p))
                {
                    self.spawn_substitute_fetches(vec![(drv_hash.clone(), paths)], auth)
                        .await;
                    return true;
                }
                // r[impl sched.merge.substitute-topdown+15]
                // Truly missing → the caller dispatches from source. A
                // topdown-pruned root with broken closure evidence
                // (childless or closure-holed) must not be (its dep
                // closure was never merged) — same fail-fast as the
                // batch pre-pass above; `true` = handled, don't dispatch.
                if self.must_substitute(drv_hash) {
                    self.fail_fast_topdown_pruned_root(
                        drv_hash,
                        "wanted output(s) missing upstream and not substitutable at dispatch \
                         after deps were pruned",
                    )
                    .await;
                    return true;
                }
                false
            }
            Ok(Err(e)) => {
                self.credit_heartbeats_for_stall(fmp_start.elapsed());
                debug!(drv_hash = %drv_hash, error = %e,
                       "Ready store-check FindMissingPaths failed; will dispatch");
                false
            }
            Err(_) => {
                self.credit_heartbeats_for_stall(fmp_start.elapsed());
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
        self.complete_ready_from_store_batch(std::slice::from_ref(drv_hash))
            .await;
    }

    /// Batched [`complete_ready_from_store`](Self::complete_ready_from_store):
    /// transition + `output_paths` set in-mem first (no await), then one
    /// `persist_status_batch(Completed)`, one `upsert_path_tenants_for_
    /// batch`, one batched newly-ready promote, then per-BUILD (not
    /// per-drv) summary/counts/completion-check. I-139: the per-item
    /// variant in `batch_probe_cached_ready`'s locally-present branch
    /// was 3 sequential PG awaits × ≤2048 candidates → 12-30s actor
    /// stall on warm-restart of a large closure.
    async fn complete_ready_from_store_batch(&mut self, hashes: &[DrvHash]) {
        if hashes.is_empty() {
            return;
        }
        struct Done {
            hash: DrvHash,
            drv_path: String,
            output_paths: Vec<String>,
            interested: HashSet<Uuid>,
        }
        let mut ok: Vec<Done> = Vec::with_capacity(hashes.len());
        for drv_hash in hashes {
            let Some(state) = self.dag.node_mut(drv_hash) else {
                continue;
            };
            if let Err(e) = state.transition(DerivationStatus::Completed) {
                warn!(drv_hash = %drv_hash, error = %e,
                      "store-hit Ready→Completed rejected; dispatching instead");
                continue;
            }
            // An inline store completion ends any substitution chain
            // that left this node Ready (the forgiven-now-wanted
            // downgrade reverts to Ready for a delta re-walk; if the
            // wanting build goes terminal first, the next pass lands
            // here instead of re-walking). The spent-forgiveness set is
            // chain-scoped — clear it so a LATER chain (e.g. a stale-
            // Completed reset after GC) starts with a clean slate
            // instead of vetoing forgiveness of a path no live build
            // wants any more.
            state.never_forgive_paths.clear();
            // IA-only convenience: `expected_output_paths` IS the
            // realised path. Non-destructive when a path is already
            // known — the floating-CA reprobe→re-substitute lane
            // arrives here with `output_paths` set to the realized
            // path by `spawn_substitute_fetches`; clobbering it with
            // `expected_output_paths == [""]` would drop GC retention
            // and emit `[""]` to clients.
            if state.output_paths.is_empty() {
                state.output_paths = state.expected_output_paths.clone();
            }
            ok.push(Done {
                hash: drv_hash.clone(),
                drv_path: state.drv_path().to_string(),
                output_paths: state.output_paths.clone(),
                interested: state.interested_builds.clone(),
            });
        }
        if ok.is_empty() {
            return;
        }
        info!(
            count = ok.len(),
            "outputs already in store; skipping dispatch"
        );
        metrics::counter!("rio_scheduler_cache_hits_total", "source" => "dispatch")
            .increment(ok.len() as u64);

        let ok_hashes: Vec<DrvHash> = ok.iter().map(|d| d.hash.clone()).collect();
        let ok_refs: Vec<&str> = ok_hashes.iter().map(|h| h.as_str()).collect();
        self.persist_status_batch(&ok_refs, DerivationStatus::Completed)
            .await;
        self.upsert_path_tenants_for_batch(&ok_hashes).await;

        // Batched promote: dedup find_newly_ready across all completed
        // hashes, transition + push_ready in-mem, then one
        // persist_status_batch(Ready). Same shape as the
        // ca_cutoff_cascade batched-promote.
        let mut newly_ready: Vec<DrvHash> = Vec::new();
        let mut seen_ready: HashSet<DrvHash> = HashSet::new();
        for h in &ok_hashes {
            for ready_hash in self.dag.find_newly_ready(h) {
                if !seen_ready.insert(ready_hash.clone()) {
                    continue;
                }
                if let Some(s) = self.dag.node_mut(&ready_hash)
                    && s.transition(DerivationStatus::Ready).is_ok()
                {
                    newly_ready.push(ready_hash.clone());
                    self.push_ready(ready_hash);
                }
            }
        }
        let ready_refs: Vec<&str> = newly_ready.iter().map(|h| h.as_str()).collect();
        self.persist_status_batch(&ready_refs, DerivationStatus::Ready)
            .await;

        // Same completion-time topdown_pruned re-evaluation as the
        // worker path (`promote_newly_ready_batch`): substitution
        // success and dispatch-time store hits also turn parents'
        // children produced, and this path promotes through its own
        // inline loop above instead of that helper.
        self.clear_topdown_pruned_for_produced_parents(&ok_hashes)
            .await;

        // Per-build (not per-drv): emit one cached event per (drv,
        // interested-build), then a single summary scan + counts +
        // completion-check per distinct build. I-103: dispatch-time
        // short-circuit counts as cached.
        let mut cached_per_build: HashMap<Uuid, u32> = HashMap::new();
        for d in &ok {
            let event = rio_proto::types::build_event::Event::Derivation(
                rio_proto::types::DerivationEvent::cached(
                    d.drv_path.clone(),
                    d.output_paths.clone(),
                ),
            );
            for &build_id in &d.interested {
                self.events.emit(build_id, event.clone());
                *cached_per_build.entry(build_id).or_default() += 1;
            }
        }
        for (build_id, n) in cached_per_build {
            // r[impl sched.build.terminal-status-settled+2]
            // Dispatch-time store hits can fan out to resident terminal
            // builds that retained interest on the shared node; their
            // served accounting and progress are frozen at the terminal
            // transition (the wrapper below also skips the Progress).
            if let Some(b) = self.builds.get_mut(&build_id)
                && !b.state().is_terminal()
            {
                b.cached_count += n;
            }
            // I-140: one build_summary scan shared, not two.
            let summary = self.dag.build_summary(build_id);
            self.update_build_counts_with(build_id, &summary).await;
            self.emit_progress_with(build_id, &summary);
            self.check_build_completion(build_id).await;
        }
    }

    /// I-065: has `failed_builders` excluded EVERY currently-registered
    /// statically-eligible **non-draining** worker (matching kind +
    /// system + features)?
    ///
    /// Predicate is "every statically-eligible non-draining worker is
    /// in the failed set", not `failed_builders.len() >= total`. The
    /// latter over-counts stale IDs: b0 fails, b0 is replaced by b2,
    /// b1 fails → set={b0,b1} len=2, total=2 → would poison, but b2
    /// was never tried. The fleet filter MUST match the
    /// static-eligibility subset of `rejection_reason` — a kind-only
    /// filter let an x86 drv that failed on every x86 worker defer
    /// forever in a multi-arch cluster because aarch64 workers (which
    /// `find_executor` rejects on system-mismatch) kept the fleet
    /// "non-exhausted".
    ///
    /// Draining workers are excluded: under one-shot semantics
    /// (I-188; the only mode), a just-failed worker is `draining=true`
    /// but still in `self.executors` at completion-time. Counting it
    /// meant `poolSize=1` poisoned on the FIRST transient failure
    /// (fleet={E1}, failed={E1} → exhausted), bypassing `max_retries`
    /// and `poison_config.threshold`. Excluding it lets the
    /// empty-fleet guard below return `false` → re-queue → controller
    /// spawns a fresh `executor_id ∉ failed_builders`. Under one-shot
    /// this function therefore returns `false` in practice (failed
    /// workers drain; fresh workers ∉ failed_builders); it remains as
    /// defense-in-depth for any future path where a worker fails
    /// without draining. Poison-on-repeated-failure flows through
    /// `PoisonConfig::is_poisoned(threshold)` instead.
    ///
    /// Returns false (don't poison) when zero statically-eligible
    /// non-draining workers are registered — that's "no pool connected
    /// for this system/features", a transient that the freeze
    /// detector, unroutable-system gauge, and autoscaler handle.
    /// Poisoning then would brick builds during a deployment rollout.
    // r[impl sched.dispatch.fleet-exhaust+2]
    pub(super) fn failed_builders_exhausts_fleet(&self, drv_hash: &DrvHash) -> bool {
        let Some(state) = self.dag.node(drv_hash) else {
            return false;
        };
        if state.retry.failed_builders.is_empty() {
            return false;
        }
        let mut fleet = self
            .executors
            .values()
            .filter(|w| !w.is_draining() && crate::assignment::statically_eligible(w, state));
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
                system = %state.system,
                // §13e + r35: intentional bypass — the operator triaging
                // an unroutable drv needs to see what the tenant
                // DECLARED. `effective_features` is the routing artifact.
                declared_features = ?state.required_features(),
                effective_features = ?state.effective_features().as_slice(),
                failed_on = state.retry.failed_builders.len(),
                "failed_builders excludes every statically-eligible worker \
                 (kind+system+features); poisoning (would otherwise defer \
                 forever — see I-065)"
            );
            metrics::counter!("rio_scheduler_poison_fleet_exhausted_total").increment(1);
        }
        exhausted
    }

    /// Find a worker for this derivation: intent-match (ADR-023)
    /// first, else `best_executor` over the kind-matching pool. `None`
    /// if nobody can take it (wrong system, all full, no workers).
    fn find_executor(&self, drv_hash: &DrvHash) -> Option<ExecutorId> {
        let drv_state = self.dag.node(drv_hash)?;

        // r[impl sched.sla.intent-match]
        // ADR-023: a worker that heartbeated `intent_id == drv_hash` was
        // spawned FOR this derivation (controller stamped the SpawnIntent
        // on its pod resources). Prefer it over best_executor — its
        // (cores, mem, disk) were sized by `solve_intent_for` for this
        // exact drv. Re-check `rejection_reason` (kind/system/feature/
        // capacity) so a pool misconfig doesn't bypass the airgap/
        // feature gates; on miss (drv re-planned, scheduler restarted,
        // intent stale) fall through to pick-from-queue.
        if let Some(w) = self.executors.values().find(|w| {
            w.intent_id.as_deref() == Some(drv_hash.as_ref())
                && crate::assignment::rejection_reason(w, drv_state).is_none()
        }) {
            return Some(w.executor_id.clone());
        }

        // D2: builds and FODs share this path. The kind boundary in
        // `hard_filter` (`r[sched.dispatch.fod-to-fetcher]`) routes
        // FODs to fetcher executors and non-FODs to builders; the
        // resource-fit clause (fed by `last_intent.mem_bytes` ←
        // `solve_intent_for` ← `resource_floor`) handles sizing.
        crate::assignment::best_executor(&self.executors, drv_state)
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
        // Fresh attempt → clean log buffer. Clears any stale partial
        // from a transient-failure predecessor (whose lines would
        // otherwise prefix this attempt's with overlapping numbers)
        // and any stale seal from a poison-clear (which would silently
        // drop this attempt's pushes). No-op for first dispatch.
        self.discard_log_buffer(drv_hash);

        // Mint a fresh per-execution identifier. UUIDv7 — time-sortable,
        // keys the `drv_logs` row and the `logs/{drv_hash}/{exec_id}.*`
        // S3 blobs. Stamped on the ring-buffer entry (flusher carrier)
        // and `DerivationState` (actor carrier); persisted to
        // `assignments.exec_id` in `record_assignment` (recovery
        // carrier); sent in `WorkAssignment.exec_id` (worker echo).
        // Minted AFTER discard so the buffer entry it stamps is fresh,
        // and BEFORE record_assignment so the PG row is consistent
        // with the in-memory state.
        let exec_id = Uuid::now_v7();
        self.set_log_exec(drv_hash, exec_id, executor_id);
        if let Some(state) = self.dag.node_mut(drv_hash) {
            state.exec_id = Some(exec_id);
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

        self.record_assignment(drv_hash, executor_id, generation, exec_id)
            .await;

        // PrefetchHint BEFORE WorkAssignment: the worker starts
        // warming its FUSE cache while still parsing the .drv. A few
        // seconds of head-start on a multi-minute fetch is the win.
        // Best-effort: try_send, failure logs debug not warn. If only
        // the HINT fails, the build still works (on-demand FUSE).
        self.send_prefetch_hint(executor_id, drv_hash);

        // Derive claims + resolve CA inputs + construct the proto.
        let assignment = match self
            .build_assignment_proto(drv_hash, executor_id, generation)
            .await
        {
            AssignmentProtoOutcome::Ready(a) => *a,
            // Node disappeared between the Ready check and here
            // (TOCTOU vs. concurrent cancel) — legacy no-rollback
            // semantics: caller defers.
            AssignmentProtoOutcome::NodeGone => return false,
            // r[impl sched.dispatch.claims-derived+5]
            // The store could not vouch for a bare store-backed node's
            // claims — STORE SILENCE only; the cause population is the
            // `SilenceReason` enum (merge.rs), nothing else routes
            // here. The other outcomes route per the
            // build_assignment_proto match (the defining site):
            // content-bound structural reasons take the
            // PermanentlyUnverifiable poison arm; UNSEEDED INPUTS take
            // the bounded UnseededInputs deferral arm — NOT poison
            // (claims-derived+5; round-17 merged_bug_090 site 1
            // re-trued the pre-+3 "unseedable inputs poison" sentence
            // that contradicted that arm). Transient,
            // store-trust posture:
            // roll the assignment back AND set the dispatch backoff
            // ourselves — `rollback_assignment` resets to Ready
            // without one, and a store outage would otherwise hot-loop
            // assign → fetch-fail → rollback on every dispatch pass.
            AssignmentProtoOutcome::Unavailable(reason) => {
                metrics::counter!("rio_scheduler_dispatch_claims_unavailable_total").increment(1);
                self.rollback_assignment(drv_hash, executor_id).await;
                // r[impl sched.dispatch.claims-derived+5]
                // Store silence is a transient verdict (post-+3 the
                // unseeded-inputs arm below defers too, on its own
                // budget), and it
                // is bounded by its OWN budget (charge(); cap = the
                // existing max_infra_retries — no new knob): a
                // persistently silent store on a deterministic input
                // must converge to a visible poison, not retry
                // forever. The charge deliberately does NOT touch
                // retry.count — silence is not a build failure, and
                // borrowing that counter polluted the transient build
                // budget (merged_bug_010). Failover forgives: the
                // counter is in-memory, a fresh leader re-probes.
                let cap = self.retry_policy.max_infra_retries;
                let decision = match self.dag.node_mut(drv_hash) {
                    Some(state) => state
                        .retry
                        .charge(crate::state::FailureClass::ClaimsUnavailable, cap),
                    None => return false,
                };
                match decision {
                    crate::state::ChargeDecision::Backoff(attempt) => {
                        warn!(
                            drv_hash = %drv_hash,
                            executor_id = %executor_id,
                            reason,
                            attempt,
                            cap,
                            "claims derivation unavailable; assignment rolled back with backoff"
                        );
                        if let Some(state) = self.dag.node_mut(drv_hash) {
                            let backoff = self.retry_policy.backoff_duration(attempt);
                            state.retry.backoff_until = Some(std::time::Instant::now() + backoff);
                        }
                    }
                    crate::state::ChargeDecision::Exhausted => {
                        warn!(
                            drv_hash = %drv_hash,
                            executor_id = %executor_id,
                            reason,
                            cap,
                            "claims derivation unavailable budget exhausted; poisoning"
                        );
                        let msg = format!(
                            "the store could not vouch for this derivation's claims \
                             after {cap} dispatch attempts (last reason: {reason}); \
                             verify the .drv is uploaded and the store is healthy, \
                             then clear the poison or resubmit"
                        );
                        self.poison_and_cascade(drv_hash, &msg).await;
                        for build_id in self.get_interested_builds(drv_hash) {
                            self.record_failure_evidence(build_id, drv_hash).await;
                        }
                    }
                }
                return false;
            }
            // Claims forgery (the verified bytes contradict the
            // recorded claims) or content-bound unparseable bytes:
            // PERMANENT. No token is ever signed; the node is poisoned
            // through the terminal-failure machinery and every
            // interested build records the failure evidence at source.
            AssignmentProtoOutcome::Forged(detail) => {
                metrics::counter!("rio_scheduler_dispatch_claims_forgery_total").increment(1);
                warn!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    detail = %detail,
                    "dispatch claims forged; poisoning the derivation"
                );
                self.rollback_assignment(drv_hash, executor_id).await;
                let msg = format!("dispatch claims derivation failed permanently: {detail}");
                self.poison_and_cascade(drv_hash, &msg).await;
                for build_id in self.get_interested_builds(drv_hash) {
                    self.record_failure_evidence(build_id, drv_hash).await;
                }
                return false;
            }
            // r[impl sched.dispatch.claims-derived+5]
            // Structurally unverifiable: PERMANENT for retries of this
            // submission shape — surface a visible poison carrying the
            // generated remediation instead of livelocking through
            // backoff (pre-fix: deterministic re-verification forever).
            AssignmentProtoOutcome::PermanentlyUnverifiable(remediation) => {
                metrics::counter!("rio_scheduler_dispatch_claims_unverifiable_total").increment(1);
                warn!(
                    drv_hash = %drv_hash,
                    executor_id = %executor_id,
                    remediation = %remediation,
                    "dispatch claims structurally unverifiable; poisoning with remediation"
                );
                self.rollback_assignment(drv_hash, executor_id).await;
                let msg = format!(
                    "dispatch claims verification is structurally impossible: {remediation}"
                );
                self.poison_and_cascade(drv_hash, &msg).await;
                for build_id in self.get_interested_builds(drv_hash) {
                    self.record_failure_evidence(build_id, drv_hash).await;
                }
                return false;
            }
            // r[impl sched.dispatch.claims-derived+5]
            // Post-read-through unseeded inputs (bug_029): bounded
            // backoff on the node's OWN budget, exactly the
            // claims-unavailable shape — because the blocking fact is
            // mutable state (residency erased by reap/failover; rows
            // that may land mid-merge), not content. Pre-fix this
            // population instant-poisoned: a deploy failover erased
            // every completed input's residency at once and the
            // claims gate poisoned every in-flight dependent honest
            // build it touched. Exhaustion converges to the SAME
            // visible poison, but only after the budget proves the
            // identity is genuinely not arriving.
            AssignmentProtoOutcome::UnseededInputs(remediation) => {
                metrics::counter!("rio_scheduler_dispatch_claims_unseeded_total").increment(1);
                self.rollback_assignment(drv_hash, executor_id).await;
                let cap = self.retry_policy.max_infra_retries;
                let decision = match self.dag.node_mut(drv_hash) {
                    Some(state) => state
                        .retry
                        .charge(crate::state::FailureClass::UnseededInputs, cap),
                    None => return false,
                };
                match decision {
                    crate::state::ChargeDecision::Backoff(attempt) => {
                        warn!(
                            drv_hash = %drv_hash,
                            executor_id = %executor_id,
                            attempt,
                            cap,
                            "claims inputs unseeded after row read-through; \
                             assignment rolled back with backoff"
                        );
                        if let Some(state) = self.dag.node_mut(drv_hash) {
                            let backoff = self.retry_policy.backoff_duration(attempt);
                            state.retry.backoff_until = Some(std::time::Instant::now() + backoff);
                        }
                    }
                    crate::state::ChargeDecision::Exhausted => {
                        warn!(
                            drv_hash = %drv_hash,
                            executor_id = %executor_id,
                            cap,
                            "unseeded-inputs budget exhausted; poisoning with remediation"
                        );
                        let msg = format!(
                            "dispatch claims verification could not seed the \
                             derivation's input identities after {cap} attempts: \
                             {remediation}"
                        );
                        self.poison_and_cascade(drv_hash, &msg).await;
                        for build_id in self.get_interested_builds(drv_hash) {
                            self.record_failure_evidence(build_id, drv_hash).await;
                        }
                    }
                }
                return false;
            }
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
    // r[impl sched.gc.live-pins+2]
    async fn record_assignment(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        generation: u64,
        exec_id: Uuid,
    ) {
        self.persist_status(drv_hash, DerivationStatus::Assigned, Some(executor_id))
            .await;

        // PG BIGINT is signed; cast at THIS boundary, not at the
        // proto-encode sites (hotter). Best-effort: log+continue.
        if let Some(state) = self.dag.node(drv_hash)
            && let Some(db_id) = state.db_id
            && let Err(e) = self
                .db
                .insert_assignment(db_id, executor_id, generation as i64, exec_id)
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
        // Discard the ring-buffer entry that `set_log_exec` just
        // created — without this, a failed dispatch leaks an empty
        // stamped entry that the periodic flusher would skip (zero
        // lines) but never reap. Idempotent; the entry is empty (the
        // worker never started streaming because try_send failed).
        self.discard_log_buffer(drv_hash);
        // Assigned -> Ready. Caller (dispatch_ready) defers; next pass
        // retries. `reset_to_ready` also clears `exec_id`.
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
        //   - persist_status(Ready): record_assignment wrote
        //     status=Assigned + assigned_executor; without this a
        //     scheduler crash in the (potentially long) deferred window
        //     reloads `Assigned` and `reset_orphan_to_ready` charges a
        //     spurious retry/poison for an assignment that never
        //     reached the worker.
        //   - unpin: pin_live_inputs wrote scheduler_live_pins rows;
        //     leak until terminal cleanup if not undone.
        //   - delete_latest_assignment: insert_assignment wrote a
        //     'pending' row; misleading on recovery.
        self.persist_status(drv_hash, DerivationStatus::Ready, None)
            .await;
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
    /// `executor_id`: claims derivation, CA-input resolve, HMAC token
    /// sign, build-options lookup. Side-effect: stashes
    /// `pending_realisation_deps` on the node so
    /// `handle_success_completion` can write the realisation FK rows
    /// post-build.
    ///
    /// [`WorkAssignment`]: rio_proto::types::WorkAssignment
    async fn build_assignment_proto(
        &mut self,
        drv_hash: &DrvHash,
        executor_id: &ExecutorId,
        generation: u64,
    ) -> AssignmentProtoOutcome {
        // === Claims derivation (sched.dispatch.claims-derived+5) ======
        // r[impl sched.dispatch.claims-derived+5]
        // Decide the byte-bound source of every value the token will
        // sign and the worker will obey, BEFORE any of it is used.
        // Unsigned dev mode mints no claims — nothing to derive (the
        // store accepts unsigned only when its own verifier is off).
        //
        // Ingress-byte-bound nodes (inline / authoritative): commits
        // 13/16 bound the recorded values to the bytes at SubmitBuild —
        // sign recorded. Store-backed nodes already at
        // `path_bound_bytes` (a prior dispatch derived them, or the
        // merge-time store-evidence check verified them; recovery
        // restores the persisted rank): sign recorded. EVERY other
        // store-backed node: fetch the .drv the declared path names,
        // re-derive its text content-address in the actor, and run the
        // parsed derivation against the RECORDED claims through the
        // same identity validator SubmitBuild ingress applies — the
        // resolve-need is then derived from those verified bytes,
        // never from the submitter's `needs_resolve` echo.
        //
        // Computed cost bound: this gate performs exactly ONE store
        // GetPath per FIRST dispatch of a bare store-backed node — the
        // node's own `.drv`, never a closure walk (input digests come
        // from the InputFormSeed over resident DAG children, zero
        // fetches). A verified or stripped node is raised to
        // `path_bound_bytes`, so re-dispatch skips the fetch entirely.
        // Contrast with the store-side deriver-proof read-through,
        // which is the O(closure) surface and carries its own budget.
        let verified_bytes: Option<Vec<u8>> = if self.hmac_signer.is_some() {
            let verdict = {
                let Some(state) = self.dag.node(drv_hash) else {
                    return AssignmentProtoOutcome::NodeGone;
                };
                if !state.drv_content.is_empty() {
                    let source = if state.drv_content_authoritative {
                        "authoritative"
                    } else {
                        "inline"
                    };
                    metrics::counter!(
                        "rio_scheduler_dispatch_claims_source_total",
                        "source" => source
                    )
                    .increment(1);
                    None
                } else if state.evidence >= crate::state::DefinitionEvidence::PathBoundBytes {
                    metrics::counter!(
                        "rio_scheduler_dispatch_claims_source_total",
                        "source" => "store"
                    )
                    .increment(1);
                    None
                } else {
                    // Bare store-backed below path-bound standing:
                    // synthesize the RECORDED claims (pre-resolve:
                    // deferred/floating slots still carry their
                    // ingress-shape empty paths) and verify against the
                    // store's own bytes. Sibling hash seeds come from
                    // the DAG children — for a dispatched node its
                    // dependencies are resident and Completed.
                    let node = crate::domain::DerivationNode {
                        drv_hash: state_drv_hash_string(drv_hash),
                        drv_path: state.drv_path().to_string(),
                        pname: String::new(),
                        system: state.system.clone(),
                        output_names: state.output_names.clone(),
                        expected_output_paths: state.expected_output_paths.clone(),
                        is_fixed_output: state.is_fixed_output,
                        is_content_addressed: state.ca.is_ca,
                        ca_modular_hash: state.ca.modular_hash,
                        // The synth is a verification INPUT; preserved
                        // stripped claims are not part of the claimed
                        // identity being verified.
                        ca_modular_hash_stripped: None,
                        drv_content: Vec::new(),
                        drv_content_authoritative: false,
                        required_features: Vec::new(),
                        wanted_output_names: Vec::new(),
                        explicitly_requested: false,
                        needs_resolve: false,
                        version: None,
                        enable_parallel_building: None,
                        enable_parallel_checking: None,
                        prefer_local_build: None,
                    };
                    // Input-form seeds only: the constructor owns the
                    // not-floating predicate (sched.merge.input-form-seed)
                    // — a Completed floating child's recorded hash is
                    // the masked published form and would steer the
                    // verification onto wrong derived paths (wrongful
                    // Forged for honest parents, wrongful Verified for
                    // crafted ones). Excluded children make the input
                    // unseedable instead, which is the fail-closed
                    // direction.
                    //
                    // TODO: round-15 C3c7 (slipped to follow-up) — these
                    // seeds are rank-blind: a non-floating child whose
                    // recorded hash is still a submitter echo
                    // (UnverifiedClaim) seeds the parent's verification
                    // with an unverified value (merged_bug_039's
                    // value-trust half). The fix is a
                    // min_rank=PathBoundBytes floor here plus a store
                    // read-through for sub-floor/unseedable children —
                    // but the M_068-backed digests (`prove_drv_modulo`)
                    // have NO read RPC on StoreService today, and a
                    // floor without the fallback livelocks honest bare
                    // closures (cache-hit children never re-verify).
                    // Minting that read surface is trusted-plane design
                    // (auth posture for a scheduler-privileged read,
                    // walk-budget-over-wire), deferred wholesale per the
                    // round-15 plan §4.3.1 slip clause. Residual until
                    // then: a forged child echo steers THIS node's
                    // verification toward Contradicts/Unverifiable —
                    // bounded backoff + noisy rejection, never a forged
                    // Verified for a path the store's bytes don't derive
                    // (the parent's own bytes are still text-CA-bound).
                    let seed = super::merge::InputFormSeed::from_dag_children(&self.dag, drv_hash);
                    Some(self.check_store_evidence(&node, &seed).await)
                }
            };
            match verdict {
                None => None,
                Some(super::merge::StoreEvidenceOutcome::Verified(def)) => {
                    metrics::counter!(
                        "rio_scheduler_dispatch_claims_source_total",
                        "source" => "store"
                    )
                    .increment(1);
                    // The recorded claims are now PROVEN byte-derived:
                    // raise the node's standing so re-dispatch skips
                    // the re-fetch. Best-effort persist — a lost write
                    // degrades to re-derivation after failover.
                    // r[impl sched.dispatch.claims-derived+5]
                    // The resolve flag is recorded HERE, in the same
                    // node_mut block as the rank raise, from the
                    // byte-derived fact the classification site
                    // computed — every later read (maybe_resolve_ca)
                    // consults recorded state only, so a forged echo
                    // cannot steer post-verification dispatch.
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        state.evidence = crate::state::DefinitionEvidence::PathBoundBytes;
                        state.ca.needs_resolve = def.needs_resolve;
                        // Verified edge: consecutive-silence budget resets.
                        state.retry.reset_claims_unavailable();
                    }
                    if let Err(e) = self
                        .db
                        .persist_evidence_rank(
                            drv_hash.as_str(),
                            crate::state::DefinitionEvidence::PathBoundBytes,
                            // M_071: the byte-derived resolve flag rides
                            // the raise — one statement, so the
                            // persisted rank can never outlive a lossy
                            // re-derivation of the flag (bug_053).
                            Some(def.needs_resolve),
                        )
                        .await
                    {
                        debug!(drv_hash = %drv_hash, error = %e,
                               "evidence-rank persist failed (best-effort)");
                    }
                    Some(def.bytes)
                }
                Some(super::merge::StoreEvidenceOutcome::Contradicts(detail)) => {
                    return AssignmentProtoOutcome::Forged(detail);
                }
                Some(super::merge::StoreEvidenceOutcome::UnparseableVerified) => {
                    return AssignmentProtoOutcome::Forged(
                        "the store's text-CA-bound bytes at the declared path do not \
                         parse as a derivation (content-bound: refetching reproduces \
                         them)"
                            .into(),
                    );
                }
                // r[impl sched.dispatch.claims-derived+5]
                // Three-way permanence contract (the merged_bug_019
                // deploy-blocker fix; fix-discipline R1 — consequences
                // derived from the variant's typed permanence):
                //
                // TRANSIENT silence → backoff. The ONLY arm allowed to
                // retry, and it is bounded by its own budget.
                Some(super::merge::StoreEvidenceOutcome::StoreSilence(reason)) => {
                    return AssignmentProtoOutcome::Unavailable(reason.as_str());
                }
                // PERMANENT structural impossibility → visible poison
                // with remediation generated from the typed reason.
                // Backoff cannot resolve it: pre-fix this arm
                // livelocked (deterministic re-verification, identical
                // result, forever). Restricted BY TYPE to content-bound
                // reasons (claims-derived+5): missing input identity
                // is the UnseededInputs arm below.
                Some(super::merge::StoreEvidenceOutcome::StructurallyUnverifiable(reason)) => {
                    return AssignmentProtoOutcome::PermanentlyUnverifiable(reason.remediation());
                }
                // r[impl sched.dispatch.claims-derived+5]
                // Post-read-through unseeded inputs → BOUNDED BACKOFF
                // (the bug_029 kill): the chokepoint already consulted
                // the persisted rows, but residency/rows are state
                // that can still change under this node (deeper
                // submission, upload, mid-merge row). The caller
                // charges the dedicated budget; exhaustion poisons
                // with this remediation.
                Some(super::merge::StoreEvidenceOutcome::UnseededInputs { missing, .. }) => {
                    return AssignmentProtoOutcome::UnseededInputs(
                        super::merge::unseeded_remediation(&missing),
                    );
                }
                // Strip-resolvable: the bytes ARE the store's text-CA
                // object and the identity verifies EXCEPT the declared
                // modular hash, which can never be recomputed (floating
                // store-backed input). Exact ingress-STRIP parity
                // (ingress-inline-drv-binding+1): an unverifiable claim
                // is NO claim — clear it (memory + row), raise the node
                // to path_bound_bytes on the verified bytes, and
                // proceed. Pre-fix this arm livelocked 100% of bare
                // CA-chain / deferred-IA dispatches under signing.
                Some(super::merge::StoreEvidenceOutcome::VerifiedExceptDeclaredHash(def)) => {
                    metrics::counter!(
                        "rio_scheduler_dispatch_claims_source_total",
                        "source" => "store"
                    )
                    .increment(1);
                    metrics::counter!("rio_scheduler_dispatch_claims_stripped_total").increment(1);
                    info!(
                        drv_hash = %drv_hash,
                        "declared modular hash unverifiable against store bytes; \
                         stripped (an unverifiable claim is no claim) and \
                         proceeding on the verified bytes"
                    );
                    if let Some(state) = self.dag.node_mut(drv_hash) {
                        // MOVE, never destroy (M_070): the preserved
                        // claim is what lets a settled row formed from
                        // this node match a byte-equal resubmission
                        // after reap (merged_bug_038). take() keeps an
                        // earlier preserved value when the live hash is
                        // already None (re-strip idempotence).
                        if let Some(stripped) = state.ca.modular_hash.take() {
                            state.ca.modular_hash_stripped = Some(stripped);
                        }
                        state.evidence = crate::state::DefinitionEvidence::PathBoundBytes;
                        // r[impl sched.dispatch.claims-derived+5]
                        // Same record-at-raise as the Verified arm:
                        // the strip raises rank on these bytes, so the
                        // byte-derived resolve flag rides the raise.
                        state.ca.needs_resolve = def.needs_resolve;
                        // Verified-modulo-strip edge: budget resets too.
                        state.retry.reset_claims_unavailable();
                    }
                    if let Err(e) = self
                        .db
                        .persist_evidence_rank_and_strip_modular_hash(
                            drv_hash.as_str(),
                            crate::state::DefinitionEvidence::PathBoundBytes,
                            // M_071: same one-statement pairing as the
                            // plain raise arm above.
                            Some(def.needs_resolve),
                        )
                        .await
                    {
                        debug!(drv_hash = %drv_hash, error = %e,
                               "stripped-evidence persist failed (best-effort)");
                    }
                    Some(def.bytes)
                }
            }
        } else {
            None
        };
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
        let (drv_content_to_send, resolve_lookups, resolved_output_paths) = {
            let Some(state) = self.dag.node(drv_hash) else {
                return AssignmentProtoOutcome::NodeGone;
            };
            self.maybe_resolve_ca(drv_hash, state, verified_bytes.as_deref())
                .await
        };

        // Stash lookups for handle_success_completion's
        // insert_realisation_deps (the FK needs the parent's own
        // realisation row to exist, which only happens post-build).
        // Empty vec → no-op; non-empty only for CA-on-CA chains
        // that actually resolved.
        //
        // Deferred-IA: record the post-resolve computed paths
        // (index-aligned with output_names) in the CLAIM field so the
        // HMAC `expected_outputs` claim below carries the real path,
        // not `""`. NEVER written to `expected_output_paths` — the
        // round-16 bug_094 fix: that field's ingress shape (empty slot
        // = path unknown until resolution) is the contract of the
        // byte-derived resolve probe (`child_unknown`, merge.rs) and
        // the recovery degrade; the old per-slot overwrite destroyed
        // the emptiness signal, so a bare store-backed FOD parent
        // dispatching after its deferred-IA child recorded a STICKY
        // `needs_resolve=false` at its PathBoundBytes raise and then
        // failed deterministically on the un-rewritten placeholder.
        // Floating-CA leaves resolved_output_paths empty → claim falls
        // back to expected (its HMAC path is `is_ca` instead).
        let mut persist_claim: Option<Vec<String>> = None;
        if (!resolve_lookups.is_empty() || !resolved_output_paths.is_empty())
            && let Some(state) = self.dag.node_mut(drv_hash)
        {
            state.ca.pending_realisation_deps = resolve_lookups;
            if !resolved_output_paths.is_empty() {
                // Owner method (round-17 bug_033): arity-total over the
                // omitted-[] ingress shape — the resize-then-merge lives
                // in ONE place so a resolved path can never be silently
                // dropped against a short expected list again.
                let claim = state
                    .merge_resolved_claim_paths(resolved_output_paths)
                    .to_vec();
                persist_claim = Some(claim);
            }
        }
        // M_075 write-through (round-17 merged_bug_099): the claim vec
        // persists at its sole set site so a leader failover keeps the
        // surviving worker's GC pin and completion path-binding. The
        // exact in-memory vec is written — resolved slots carry real
        // paths, still-unresolved slots keep the "" sentinel (the
        // completion gate's accepted_unresolved_slot cell distinguishes
        // them; a floating-CA node never reaches this set site, so its
        // column stays NULL and the fallback-to-expected behaviour is
        // untouched). Best-effort: on write failure the pre-M_075
        // behaviour (re-resolve at next dispatch) is the fallback.
        if let Some(claim) = persist_claim
            && let Err(e) = self.db.persist_claim_output_paths(drv_hash, &claim).await
        {
            warn!(%drv_hash, error = %e,
                  "failed to persist dispatch-resolved claim paths \
                   (GC pin and binding fall back to re-resolve after failover)");
        }

        let Some(state) = self.dag.node(drv_hash) else {
            return AssignmentProtoOutcome::NodeGone;
        };
        let build_opts = self.build_options_for_derivation(drv_hash);

        // Assignment token: HMAC-signed if configured, else
        // legacy format-string. The store verifies signed
        // tokens on PutPath (prevents arbitrary-path upload
        // from a compromised worker). Unsigned tokens are
        // accepted by a store with hmac_verifier=None (dev).
        //
        // Expiry: 2× the effective build timeout (the
        // assignment's BuildOptions.build_timeout, or the
        // worker's default when unset). A worker legitimately
        // uploading after completion is well within that
        // window. Prevents replay from a leaked token later.
        let assignment_token = if let Some(signer) = &self.hmac_signer {
            let timeout_secs = if build_opts.build_timeout > 0 {
                build_opts.build_timeout
            } else {
                // Match rio-builder's DEFAULT_BUILD_TIMEOUT.
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
                expected_outputs: state.claim_output_paths().to_vec(),
                // Floating-CA: output path is computed post-build
                // from the NAR hash, so expected_output_paths is
                // [""] here. Store skips the path-in-claims check
                // when is_ca is set (verify-on-put still hashes
                // the NAR independently; threat model holds).
                // Fixed-output CA (FOD) has a known path → treat
                // as IA for the membership check.
                is_ca: state.ca.is_ca && !state.is_fixed_output,
                // Signed FOD marker (persisted on the node, so recovered
                // assignments still carry it): the store rejects
                // descriptor-less uploads under a FOD-flagged token, so
                // a worker cannot skip the content⇔path verification by
                // simply omitting its `fixed:` descriptor
                // (sec.authz.ca-path-derived). Always signed — the
                // store-side rejection is a core guarantee, not an
                // opt-in.
                is_fixed_output: state.is_fixed_output,
                expiry_unix,
                // Tenant attribution for hw_perf_samples.submitting_tenant (M_054).
                // Phase 2 of the bug_011 two-phase rollout (Phase 1 = fb096e50f);
                // safe to set unconditionally since fb096e50f's `skip_serializing_if`
                // + `#[serde(default)]` cover both rolling-upgrade skew directions.
                tenant: state.attributed_tenant(&self.builds).map(|u| u.to_string()),
            })
        } else {
            // Legacy unsigned: format-string. Store with
            // hmac_verifier=None accepts this.
            format!("{executor_id}-{drv_hash}-{generation}")
        };

        AssignmentProtoOutcome::Ready(Box::new(rio_proto::types::WorkAssignment {
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
            // makes resolve_build_opts override client `--cores N`. The
            // telemetry side (build_samples.cpu_limit_cores) records this
            // same value at completion — see record_build_sample.
            assigned_cores: state.sched.last_intent.as_ref().map(|i| i.cores),
            // mem/disk are display-only on the worker side — the `rio:
            // builder` banner renders them; runtime limits come from the
            // pod's cgroup, which the spawn intent already shaped. Populate
            // from the same `SolvedIntent` so the banner shows what was
            // actually fitted instead of `?`. Absent (no intent — wildcard
            // worker, or pre-ADR-023 path) → banner renders `?`.
            assigned_mem_bytes: state.sched.last_intent.as_ref().map(|i| i.mem_bytes),
            assigned_disk_bytes: state.sched.last_intent.as_ref().map(|i| i.disk_bytes),
            // Per-execution identifier minted in `assign_to_worker` and
            // stored on `DerivationState`. The worker echoes this in the
            // log header (display-only). Empty only on the unreachable
            // path where the dag node lost its exec_id between assign and
            // proto-build — the actor is single-threaded so that can't
            // happen, but the field is non-Option in the proto.
            exec_id: state.exec_id.map(|u| u.to_string()).unwrap_or_default(),
        }))
    }

    /// Send a PrefetchHint for the chosen worker to warm its FUSE
    /// cache. Best-effort: `try_send`, failure logs at debug.
    ///
    /// Paths come from [`approx_input_closure`] (DAG children's
    /// expected outputs ∪ the drv's own `inputSrcs`), truncated to
    /// `MAX_PREFETCH_PATHS`. Under ephemeral one-build-per-pod the
    /// worker's cache is always empty, so the full set is always
    /// sent — no per-worker filtering. Empty = don't send.
    ///
    /// [`approx_input_closure`]: crate::assignment::approx_input_closure
    fn send_prefetch_hint(&self, executor_id: &ExecutorId, drv_hash: &DrvHash) {
        let input_paths = crate::assignment::approx_input_closure(&self.dag, drv_hash);
        if input_paths.is_empty() {
            // No DAG children AND no parsed inputSrcs (drv_content
            // empty/unparseable). Nothing to prefetch.
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
        verified_bytes: Option<&[u8]>,
    ) -> (
        Vec<u8>,
        Vec<crate::ca::RealisationLookup>,
        Vec<(String, String)>,
    ) {
        // Gate: the RECORDED resolve flag, single-source
        // (sched.dispatch.claims-derived+5). Every writer derived it
        // from bytes through the shared oracle predicate
        // (`rio_nix::derivation::should_resolve`): the gateway's
        // post-BFS pass for ingress-bound nodes (normalized again at
        // SubmitBuild from the validated inline bytes), the claims
        // derivation's record-at-raise for store-backed nodes, the
        // merge-time store-evidence stamp for evidence-created nodes,
        // and recovery's expected-paths degrade. The submitter's echo
        // is structurally out of reach here — the local re-derivation
        // this read used to do (a clause-dropping copy of the
        // predicate, merged_bug_035) is gone.
        let needs_resolve = state.ca.needs_resolve;
        if !needs_resolve {
            return (
                verified_bytes
                    .map(|b| b.to_vec())
                    .unwrap_or_else(|| state.drv_content.clone()),
                Vec::new(),
                Vec::new(),
            );
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
            return (
                verified_bytes
                    .map(|b| b.to_vec())
                    .unwrap_or_else(|| state.drv_content.clone()),
                Vec::new(),
                Vec::new(),
            );
        }

        // No drv_content → recovered derivation (scheduler restart,
        // DAG reloaded from PG; only authoritative hook-fallback
        // content is persisted — M_062 — everything else is
        // refetched). The store
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
        // The lossy-on-recovery pattern still applies to
        // `pending_realisation_deps` (best-effort cache, reconstituted
        // here on each resolve); `ca_modular_hash` and `needs_resolve`
        // are no longer lossy — recovery restores BOTH from their
        // persisted columns (sched.persist.ca-modular-hash,
        // sched.recovery.deferred-resolve+1 / M_071 verbatim restore).
        //
        // r[impl sched.ca.resolve+3]
        let drv_content = if let Some(bytes) = verified_bytes {
            // Claims derivation already fetched + text-CA-verified the
            // bytes — resolve over THOSE, no second fetch.
            bytes.to_vec()
        } else if state.drv_content.is_empty() {
            match self
                .fetch_drv_content_from_store(drv_hash.as_str(), state.drv_path())
                .await
            {
                DrvFetch::Bytes(bytes) => bytes,
                DrvFetch::Silence => {
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
                    return (state.drv_content.clone(), Vec::new(), Vec::new());
                }
                DrvFetch::Denied { got, limit } => {
                    // Same unresolved degrade — the worker's own fetch
                    // applies the same class cap and will fail the
                    // build with its content-bound InvalidDerivation
                    // classification — but the log must be truthful:
                    // this is a deterministic denial, NOT a store
                    // outage (round-17 bug_030's fold, kept out).
                    warn!(
                        drv_hash = %drv_hash,
                        got,
                        limit,
                        "recovered CA-on-CA dispatch: .drv NAR exceeds the derivation-text \
                         class cap (content-bound, not store health); dispatching unresolved"
                    );
                    return (state.drv_content.clone(), Vec::new(), Vec::new());
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
                (
                    resolved.drv_content,
                    resolved.lookups,
                    resolved.output_paths,
                )
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
                (drv_content, Vec::new(), Vec::new())
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
    /// The outcome is CLOSED ([`DrvFetch`]) so the two consumers — the
    /// merge/dispatch store-evidence chokepoint and the dispatch-time
    /// CA resolve — derive their consequence from the variant's typed
    /// permanence instead of collapsing every failure into one shape:
    /// transient failures (store unconfigured, transport, timeout,
    /// not-found, NAR shape) are [`DrvFetch::Silence`]; the
    /// over-class-cap denial is [`DrvFetch::Denied`], content-bound
    /// and deterministic (round-17 bug_030).
    ///
    /// Hard 2s idle timeout + the shared derivation-text NAR cap
    /// ([`rio_common::limits::MAX_DRV_NAR_BYTES`], 16 MiB): legitimate
    /// `.drv`s reach ~10 MiB at nixpkgs scale (huge env blocks,
    /// `exportReferencesGraph` users — the cap's own sizing note), and
    /// every other derivation-text fetch site (store admission,
    /// gateway BFS, worker glue fetch) admits up to that bound. A
    /// private lower cap here deterministically failed every
    /// (1,16] MiB `.drv`'s claims verification as "store silence"
    /// (round-17 bug_030). The class cap still bounds mis-resolution:
    /// a closure NAR behind a mis-resolved path is rejected by the
    /// collector's leading `Info.nar_size` pre-check, byte-free.
    ///
    /// Shared by the dispatch-time CA resolve (this module) and the
    /// merge-time store-evidence check
    /// (`sched.merge.store-evidence-displacement+3`) — hence the
    /// path-taking signature: the merge-side caller verifies
    /// non-resident settled rows, which have no `DerivationState`.
    pub(super) async fn fetch_drv_content_from_store(
        &self,
        drv_hash: &str,
        drv_path: &str,
    ) -> DrvFetch {
        /// Per-chunk idle bound for `GetPath` (initial RPC + each
        /// stream.message() — I-211, not whole-call). ~10-50 ms
        /// typical; 2 s covers a slow store without blocking
        /// dispatch for long. On timeout we degrade to unresolved
        /// dispatch (same as store-unconfigured).
        const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

        let Some(client) = self.store_client.as_ref() else {
            return DrvFetch::Silence;
        };
        let mut client = client.clone();

        let result = rio_proto::client::get_path_nar(
            &mut client,
            drv_path,
            FETCH_TIMEOUT,
            rio_common::limits::NarSizeCap::derivation(),
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
                return DrvFetch::Silence;
            }
            // Content-bound denial: the path's DECLARED NAR size
            // exceeds the derivation-text class cap, reported by the
            // collector's leading `Info.nar_size` pre-check before any
            // chunk flows. Deterministic — a retry cannot shrink the
            // named path's contents — so this must NOT be folded into
            // store silence (round-17 bug_030: that fold burned the
            // claims-unavailable budget and poisoned blaming store
            // health for a content-bound fact).
            Err(rio_proto::client::NarCollectError::SizeExceeded { got, limit }) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    got,
                    limit,
                    "drv fetch denied: NAR exceeds the derivation-text class cap"
                );
                return DrvFetch::Denied { got, limit };
            }
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    drv_path = %drv_path,
                    error = %e,
                    "recovered CA resolve: GetPath failed"
                );
                return DrvFetch::Silence;
            }
        };

        // NAR unwrap: .drv is a single regular file. Anything else
        // (directory, symlink, corrupt NAR) → silence: the store may
        // answer differently later (the genuine text-CA object
        // replacing a corrupt one).
        match rio_nix::nar::extract_single_file(&nar) {
            Ok(bytes) => DrvFetch::Bytes(bytes),
            Err(e) => {
                debug!(
                    drv_hash = %drv_hash,
                    error = %e,
                    "recovered CA resolve: NAR unwrap failed (not a single regular file)"
                );
                DrvFetch::Silence
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
        // Any interested build is interactive (IFD) → priority boost
        // dwarfing any critical-path value.
        let interactive = self.get_interested_builds(drv_hash).iter().any(|id| {
            self.builds
                .get(id)
                .is_some_and(|b| b.priority_class.is_interactive())
        });
        if interactive {
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

// r[impl sched.substitute.detached+5]
/// Auth source for the detached substitute closure walk.
///
/// `Service` holds `(signer, tenant_id)` and re-mints a fresh
/// `SUBSTITUTE_FETCH_TIMEOUT`-expiry token on every `mint()` so a long
/// closure walk or time parked on `substitute_sem` can't outlive the
/// token (a 60s spawn-time token expired mid-walk → later QPIs
/// `NotFound` → spurious `ok=false`). Both merge-time and dispatch-time
/// callers now use `Service` (via `substitute_auth_for_tenant`): the
/// gateway JWT is single-shot and a wide cold closure can outlive its
/// ~65 min expiry. `Jwt` remains only for the dev/no-signer/no-tenant
/// fallback (`vec![]` — no-auth) where re-minting is meaningless.
#[derive(Clone)]
pub(super) enum SubstituteAuth {
    Jwt(Vec<(&'static str, String)>),
    Service {
        signer: Arc<rio_auth::hmac::HmacSigner>,
        tenant_id: Uuid,
    },
}

impl SubstituteAuth {
    pub(super) fn mint(&self) -> Vec<(&'static str, String)> {
        match self {
            SubstituteAuth::Jwt(m) => m.clone(),
            SubstituteAuth::Service { signer, tenant_id } => {
                let claims = rio_auth::hmac::ServiceClaims {
                    caller: "rio-scheduler".to_string(),
                    expiry_unix: std::time::SystemTime::now()
                        .duration_since(std::time::UNIX_EPOCH)
                        .map(|d| d.as_secs())
                        .unwrap_or(0)
                        + super::SUBSTITUTE_FETCH_TIMEOUT.as_secs(),
                };
                vec![
                    (rio_proto::SERVICE_TOKEN_HEADER, signer.sign(&claims)),
                    (rio_proto::PROBE_TENANT_ID_HEADER, tenant_id.to_string()),
                ]
            }
        }
    }
}

/// `reason` label values for `rio_scheduler_substitute_demotions_total`,
/// derived from the FINAL error that demoted a non-forgivable path
/// (attempts within one path's retry ladder can fail with different
/// codes; the last one is what the walk gave up on). A small fixed set
/// — never the raw store message or the path.
///
/// - `not_found`: the final attempt was a plain path-not-found — no
///   configured upstream produced the path, or the store skipped
///   substitution entirely (no upstreams configured for the tenant /
///   the replica's HTTP client failed at boot). The skip cases are
///   counted store-side in `rio_store_substitute_skipped_total`; the
///   bare message gives the scheduler no way to tell them apart from a
///   genuine all-upstreams miss.
/// - `not_found_infra`: the final attempt's NotFound message indicates
///   the request never reached an upstream (auth chain / substituter
///   config) — fix the infrastructure, the path is probably fine. The
///   substrings are pinned against rio-store's `substitute_path_impl`
///   refusal messages by `demotion_reason_classifies_store_messages`.
/// - `error`: a non-transient, non-NotFound gRPC error (no retry).
/// - `exhausted`: the retry budget ran out on a transient error.
fn demotion_reason(e: &tonic::Status) -> &'static str {
    match e.code() {
        tonic::Code::NotFound => {
            let m = e.message();
            if m.contains("no tenant context") || m.contains("substituter not configured") {
                "not_found_infra"
            } else {
                "not_found"
            }
        }
        c if rio_common::grpc::is_transient(c) => "exhausted",
        _ => "error",
    }
}

/// Bump the demotion counters for a non-forgivable path whose failure
/// is about to set `ok = false` — the derivation and its build-time
/// closure are about to be compiled from source because a download
/// failed. The caller emits the `error!` (the message differs per
/// arm); this owns the two counters so no demotion site can increment
/// one and forget the other.
fn record_demotion(e: &tonic::Status) {
    metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
    metrics::counter!(
        "rio_scheduler_substitute_demotions_total",
        "reason" => demotion_reason(e)
    )
    .increment(1);
}

/// `MAX_SUBSTITUTE_CLOSURE` overshoot guard for [`walk_substitute_closure`].
/// Re-checked after every per-path `references` push so a hostile
/// upstream can overshoot by at most one path's `MAX_REFERENCES` (10k),
/// not `layer_width × MAX_REFERENCES` (~100M strings before the next
/// top-of-loop check).
fn closure_cap_exceeded(visited: usize) -> bool {
    if visited > MAX_SUBSTITUTE_CLOSURE {
        warn!(
            visited,
            cap = MAX_SUBSTITUTE_CLOSURE,
            "substitute closure walk exceeded MAX_SUBSTITUTE_CLOSURE; \
             demoting to cache-miss (hostile upstream reference chain?)"
        );
        metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
        true
    } else {
        false
    }
}

/// BFS over `info.references` from `seeds`, issuing `QueryPathInfo`
/// (which triggers store-side `try_substitute`) for every node. Returns
/// `true` iff every node in the closure is present in the store on
/// return — the contract `handle_substitute_complete{ok=true}` relies
/// on for `Substituting → Completed`.
///
/// The store substitutes ONE path per call (no closure walk), so this
/// is the only place the runtime closure can be completed. Without it,
/// `compute_input_closure`'s `BatchQueryPathInfo` (local-only) drops a
/// transitive ref from the JIT allowlist → ENOENT at exec time (e.g.
/// `rustc-wrapper` execs `rustc-1.94.0`).
///
/// **Layer-batched fast-path:** each BFS layer is first probed via
/// `BatchQueryPathInfo` (local-only, one PG `= ANY()` round-trip — the
/// I-110 pattern from `compute_input_closure`). Refs already in PG
/// contribute their `references` to the next layer without a per-path
/// RPC; only the absent subset gets a substituting `QueryPathInfo`. For
/// an 800-path closure that's mostly warm, this is O(depth) ≈ 10 batch
/// RPCs instead of 800 unary ones. A `BatchQueryPathInfo` error falls
/// through to per-path QPI for the whole layer (correctness over
/// throughput).
///
/// A non-transient `Err`, a retry-exhausted path (NotFound and
/// transient errors share the backoff ladder — a NotFound inside the
/// walk contradicts the HEAD probe / narinfo that named the path, so
/// it is retried rather than believed on first occurrence), or the
/// `MAX_SUBSTITUTE_CLOSURE` cap → `false`. Self-references and
/// diamonds dedup via `visited`.
///
/// **`forgivable`** is the subset of `seeds` whose failure must NOT
/// fail the walk: the declared-but-unwanted output paths (no consumer's
/// `inputDrvs` names them and the root didn't ask for them). The seeds
/// stay ALL expected output paths — opportunistic completeness: fetch
/// the `-debug` output if the upstream has it — but when the upstream
/// definitively misses one, condemning the derivation (and its whole
/// build-time closure) to a from-source rebuild over an output nothing
/// consumes is the incident this gate exists to prevent. A failed
/// WANTED seed and a failed reference-BFS-discovered path (a runtime
/// reference of something already fetched — its absence is a hole in a
/// closure we are about to declare complete) keep failing the walk;
/// only unwanted seeds are in `forgivable` by construction. A
/// forgivable seed is forgiven on its FIRST failure of any kind — it
/// does not consume the retry budget (the per-path loop is serial, so
/// ~32 s of backoff on an output nobody consumes delays every path
/// behind it).
///
/// **Residual hole risk.** If a WANTED output runtime-references an
/// unwanted sibling output, forgiving that sibling can leave a hole in
/// the wanted output's runtime closure even though the walk returns
/// `ok=true`. The no-hole guarantee only holds when the forgiven
/// error is a **NotFound**, and only against an upstream that
/// maintains its closure invariant: an upstream that served the wanted
/// output has every path in that output's closure, so a true miss on
/// the sibling proves the wanted output does not reference it. A
/// forgiven **transient or non-transient error** carries no such proof
/// — the upstream may well have the sibling (a 500, a timeout, a flaky
/// connection) and the walk still completes with the reference
/// unsatisfied; first-failure forgiveness widens this slightly (a
/// transient blip that one retry would have cleared is now forgiven
/// instead of retried). Accepted
/// because (a) it requires the rare wanted→unwanted-sibling reference
/// direction (`-debug`/`-doc` outputs reference their `out`, not the
/// reverse), and (b) the alternative — failing the walk — is a
/// GUARANTEED from-source rebuild of the derivation and its entire
/// build-time closure, versus a POSSIBLE FUSE ENOENT on one path in
/// one dependent's build, whose retry re-queries the path and
/// re-triggers substitution.
///
/// Returns `(ok, forgiven)`: `forgiven` is the subset of `forgivable`
/// seeds that actually FAILED and were forgiven (not the ones that
/// substituted fine). `forgivable` is a snapshot of the wanted set at
/// spawn time; a build that merges during the (potentially minutes-
/// long) walk can grow the node's wanted set to include a forgiven
/// seed. `handle_substitute_complete` re-checks `forgiven` against the
/// node's CURRENT wanted set and downgrades a stale `ok=true` to a
/// revert.
pub(super) async fn walk_substitute_closure(
    store: &rio_proto::store::store_service_client::StoreServiceClient<tonic::transport::Channel>,
    seeds: Vec<String>,
    forgivable: &HashSet<String>,
    auth: &SubstituteAuth,
    shutdown: &rio_common::signal::Token,
    mut on_progress: impl FnMut(u64, u64, &str),
) -> (bool, Vec<String>) {
    let mut visited: HashSet<String> = seeds.iter().cloned().collect();
    let mut frontier: VecDeque<String> = seeds.into_iter().collect();
    let mut ok = true;
    let mut forgiven: Vec<String> = Vec::new();
    // Aggregate across the closure for `r[gw.activity.subst-progress]`.
    // `done_base` accumulates completed-path nar_sizes; the per-path
    // callback adds its in-flight `done` on top. `expected` grows as
    // each path's narinfo is read (first progress emit OR Ok(Some)).
    // `seen_expected` tracks which paths have already contributed to
    // `expected` so a retry after a transient error doesn't double-count.
    //
    // `expected_total` grows as the BFS discovers references — nom's
    // denominator increases mid-stream. Inherent to walk-and-discover
    // (we don't pre-fetch all narinfos), not a bug; the invariants
    // below keep it from rendering as >100%.
    let mut done_base: u64 = 0;
    let mut expected_total: u64 = 0;
    let mut seen_expected: HashSet<String> = HashSet::new();
    while !frontier.is_empty() {
        if shutdown.is_cancelled() {
            return (false, forgiven);
        }
        if closure_cap_exceeded(visited.len()) {
            return (false, forgiven);
        }
        // Re-mint per layer: dispatch-time `Service` tokens are
        // short-lived; minting once at spawn meant a ghc-sized walk or
        // time on `substitute_sem` outlived the token. `Jwt` is a
        // cheap clone. Only the per-path QPIs need tenant context (for
        // store-side `try_substitute_on_miss → tenant_upstreams`).
        let mut meta_owned = auth.mint();
        let mut paths_since_mint = 0usize;
        let mut mint_at = Instant::now();
        // Layer-batched fast-path: drain the current frontier into one
        // BatchQueryPathInfo. Present refs (warm in PG) push their
        // references; absent refs go to per-path QPI to trigger
        // substitution. Batch error → treat the whole layer as absent
        // (per-path QPI then handles each, including retry).
        //
        // `&[]` metadata: BatchQPI is local-only and rejects end-user
        // tenant JWTs (`reject_end_user_tenant`); the merge-time `Jwt`
        // path carries one, so passing `meta` here →
        // `PermissionDenied` → debug! + per-path fallback (fast-path
        // never fired). The call needs no tenant.
        let layer: Vec<String> = frontier.drain(..).collect();
        let mut absent: Vec<String> = Vec::new();
        let mut c = store.clone();
        match rio_proto::client::batch_query_path_info(
            &mut c,
            layer.clone(),
            super::SUBSTITUTE_FETCH_TIMEOUT,
            &[],
        )
        .await
        {
            Ok(entries) => {
                for (path, info) in entries {
                    match info {
                        Some(info) => {
                            for r in &info.references {
                                if visited.insert(r.to_string()) {
                                    frontier.push_back(r.to_string());
                                }
                            }
                            if closure_cap_exceeded(visited.len()) {
                                return (false, forgiven);
                            }
                        }
                        None => absent.push(path),
                    }
                }
            }
            Err(e) => {
                debug!(error = %e, layer = layer.len(),
                       "substitute closure: batch probe failed; \
                        falling through to per-path QPI");
                absent = layer;
            }
        }
        // Per-path QPI for the absent subset — triggers store-side
        // try_substitute. Same retry/error handling as before; on
        // success, push references for the next layer.
        'paths: for p in absent {
            // Per-path high-water mark for the in-flight `done`.
            // `do_substitute` may iterate upstreams (or the outer
            // attempt loop may retry), each restarting at `done=0`;
            // emitting raw `done` would make the bar jump backward.
            // The hwm makes the per-path contribution monotone.
            let mut in_flight_hwm: u64 = 0;
            // Final error of the retry ladder, for the post-loop
            // exhaustion arm: the demotion log and the
            // `substitute_demotions_total{reason}` label need to know
            // whether the budget was burned by NotFounds (the upstream
            // kept contradicting the probe) or by transient errors
            // (the store never answered).
            let mut last_err: Option<tonic::Status> = None;
            // Per-path re-mint cadence: the per-layer mint above is
            // not enough — this serial loop can run >30 min on a wide
            // cold layer (hundreds of paths × admission-wait + retry
            // backoff each), outliving a `Service` token. Re-mint
            // every `SUBSTITUTE_REMINT_PATHS` paths OR
            // `SUBSTITUTE_REMINT_INTERVAL` elapsed, whichever first.
            // Expired-token QPI surfaces as `NotFound`/
            // `Unauthenticated` (NON-transient) → spurious `ok=false`
            // → demote to build-from-source.
            if paths_since_mint >= super::SUBSTITUTE_REMINT_PATHS
                || mint_at.elapsed() >= super::SUBSTITUTE_REMINT_INTERVAL
            {
                meta_owned = auth.mint();
                paths_since_mint = 0;
                mint_at = Instant::now();
            }
            paths_since_mint += 1;
            let meta: Vec<(&'static str, &str)> =
                meta_owned.iter().map(|(k, v)| (*k, v.as_str())).collect();
            for attempt in 0..super::SUBSTITUTE_FETCH_MAX_ATTEMPTS {
                if shutdown.is_cancelled() {
                    return (false, forgiven);
                }
                let mut c = store.clone();
                // r[impl gw.activity.subst-progress]
                // Per-path progress: store streams (done, expected,
                // upstream) per ~1 MiB. First emit for `p` adds its
                // `expected` to the closure aggregate; subsequent emits
                // (and retries) only update the in-flight hwm on top of
                // `done_base`. `seen_expected` keys on path so a retry
                // after a transient error doesn't re-add `expected`.
                let path_progress = |done: u64, expected: u64, upstream: &str| {
                    if seen_expected.insert(p.clone()) {
                        expected_total = expected_total.saturating_add(expected);
                    }
                    in_flight_hwm = in_flight_hwm.max(done);
                    on_progress(
                        done_base.saturating_add(in_flight_hwm),
                        expected_total,
                        upstream,
                    );
                };
                match rio_proto::client::substitute_path_with_progress(
                    &mut c,
                    &p,
                    super::SUBSTITUTE_FETCH_TIMEOUT,
                    &meta,
                    path_progress,
                )
                .await
                {
                    Ok(info) => {
                        // Store-side cache-hit / AlreadyComplete returns
                        // here without ever calling `path_progress`, so
                        // `expected_total` must learn `nar_size` here too
                        // — otherwise `done_base` outgrows it and the
                        // next path's emit shows >100%.
                        if seen_expected.insert(p.clone()) {
                            expected_total = expected_total.saturating_add(info.nar_size);
                        }
                        done_base = done_base.saturating_add(info.nar_size);
                        for r in &info.references {
                            if visited.insert(r.to_string()) {
                                frontier.push_back(r.to_string());
                            }
                        }
                        if closure_cap_exceeded(visited.len()) {
                            return (false, forgiven);
                        }
                        continue 'paths;
                    }
                    // An unwanted seed is forgiven on its FIRST
                    // failure of ANY kind: not a failure (no metric),
                    // the walk continues, the closure is still
                    // complete for every output anything consumes.
                    // Checked before the retry ladder — burning ~32 s
                    // of serialized backoff on an output nobody
                    // consumes delays every path behind it in the
                    // layer. The path was still ATTEMPTED once
                    // (opportunistic completeness — it stays in the
                    // seed list). `store_msg` for the same reason as
                    // the fatal arms below: "no tenant context" /
                    // "substituter not configured" mean the request
                    // never reached the upstream — a forgiven skip
                    // that should have been a fetch.
                    Err(e) if forgivable.contains(&p) => {
                        info!(path = %p, code = ?e.code(), store_msg = e.message(),
                              "unwanted output not substituted; continuing without it");
                        forgiven.push(p.clone());
                        continue 'paths;
                    }
                    // A NotFound inside the walk is always a
                    // contradiction: every path here was either
                    // HEAD-probed as available minutes earlier (a
                    // seed) or named in a narinfo the upstream just
                    // served (a reference), so "the upstream doesn't
                    // have it" disagrees with an observation the
                    // store/upstream made moments ago. The genuinely-
                    // not-on-any-upstream case never enters the walk.
                    // Treat it like a transient error: retry through
                    // the same backoff ladder. The 2026-05 incident
                    // demoted 235 paths to from-source builds on
                    // first-occurrence NotFounds that the store never
                    // actually checked against the upstream; all 235
                    // substituted fine 80 seconds later.
                    Err(e)
                        if e.code() == tonic::Code::NotFound
                            || rio_common::grpc::is_transient(e.code()) =>
                    {
                        debug!(path = %p, attempt, code = ?e.code(), store_msg = e.message(),
                               "substitute fetch retryable error; retrying");
                        metrics::counter!("rio_scheduler_substitute_fetch_retries_total")
                            .increment(1);
                        let exhausted = attempt + 1 == super::SUBSTITUTE_FETCH_MAX_ATTEMPTS;
                        last_err = Some(e);
                        if exhausted {
                            break;
                        }
                        tokio::select! {
                            _ = shutdown.cancelled() => return (false, forgiven),
                            _ = tokio::time::sleep(
                                super::SUBSTITUTE_FETCH_BACKOFF.duration(attempt)
                            ) => {}
                        }
                    }
                    Err(e) => {
                        // Non-transient, non-NotFound: retrying won't
                        // change the answer. error! (not warn!) — this
                        // event means a derivation and its build-time
                        // closure are about to be compiled from source
                        // because a download failed; it should page.
                        error!(path = %p, error = %e, store_msg = e.message(),
                               reason = demotion_reason(&e),
                               "detached substitute fetch failed; demoting to cache-miss");
                        record_demotion(&e);
                        ok = false;
                        continue 'paths;
                    }
                }
            }
            // Retry ladder exhausted: every attempt failed with a
            // retryable error (NotFound or transient) on a
            // non-forgivable path — every other arm `continue 'paths`
            // out of the attempt loop, so `last_err` is always the
            // final attempt's error here. The consequence is "compile
            // the derivation (and its build closure) from source", so
            // the WHY must survive into the log: `store_msg` is
            // rio-store's own reason for the FINAL attempt — "no
            // tenant context on request" / "substituter not
            // configured" mean that request never reached
            // cache.nixos.org (fix the auth chain / config); a bare
            // "path not found" means no configured upstream produced
            // it — or the store skipped substitution entirely (no
            // upstreams configured for the tenant / no HTTP client on
            // the replica; rio_store_substitute_skipped_total carries
            // the store-side cause). Indistinguishable before
            // 2026-05-23.
            let store_msg = last_err
                .as_ref()
                .map(|e| e.message().to_owned())
                .unwrap_or_default();
            let reason = last_err.as_ref().map_or("exhausted", demotion_reason);
            error!(path = %p, attempts = super::SUBSTITUTE_FETCH_MAX_ATTEMPTS,
                   store_msg, reason,
                   "detached substitute fetch exhausted retries; demoting to cache-miss");
            match last_err {
                Some(e) => record_demotion(&e),
                // Unreachable in practice; keep the failure counted
                // rather than silently losing the demotion.
                None => {
                    metrics::counter!("rio_scheduler_substitute_fetch_failures_total").increment(1);
                    metrics::counter!(
                        "rio_scheduler_substitute_demotions_total",
                        "reason" => "exhausted"
                    )
                    .increment(1);
                }
            }
            ok = false;
        }
    }
    (ok, forgiven)
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Pin `demotion_reason`'s message-substring classifier against the
    /// THREE NotFound shapes rio-store's `substitute_path_impl`
    /// actually produces (rio-store/src/grpc/queries.rs). The reason
    /// label is the only alertable signal that distinguishes "the
    /// upstream really missed" from "the request never reached the
    /// upstream"; if the store re-words a refusal message and the
    /// substring no longer matches, the infra case silently collapses
    /// into `not_found` — this test breaks instead.
    #[test]
    fn demotion_reason_classifies_store_messages() {
        let p = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x";
        for (status, want) in [
            // queries.rs: the no-substituter refusal.
            (
                tonic::Status::not_found(format!(
                    "path not found (substituter not configured on this store replica): {p}"
                )),
                "not_found_infra",
            ),
            // queries.rs: the no-tenant refusal.
            (
                tonic::Status::not_found(format!(
                    "path not found (no tenant context on request — substitution \
                     requires x-rio-tenant-token or x-rio-probe-tenant-id + \
                     x-rio-service-token): {p}"
                )),
                "not_found_infra",
            ),
            // queries.rs: the bare terminal. Reached on a genuine
            // all-upstreams miss, but ALSO when the store skipped
            // substitution entirely (no upstreams configured for the
            // tenant / no HTTP client on the replica) — same message
            // either way; the store-side cause is only visible in
            // rio_store_substitute_skipped_total.
            (
                tonic::Status::not_found(format!("path not found: {p}")),
                "not_found",
            ),
            // Transient code → the ladder ran out of budget.
            (
                tonic::Status::unavailable("connection refused"),
                "exhausted",
            ),
            // Non-transient, non-NotFound → no retry.
            (tonic::Status::internal("boom"), "error"),
        ] {
            assert_eq!(
                demotion_reason(&status),
                want,
                "code={:?} msg={:?}",
                status.code(),
                status.message()
            );
        }
    }

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
