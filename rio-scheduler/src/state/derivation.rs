//! Derivation state machine: [`DerivationStatus`] transitions and
//! [`DerivationState`] (per-derivation DAG node).
//!
//! State machine: created → queued → ready → assigned → running →
//! completed|failed|poisoned. Poisoned has a 24h TTL (→ created).
//!
//! `Failed` is a **transient intermediate**, not a terminal state:
//! `handle_transient_failure` transitions Running → Failed → Ready
//! within a single call (sub-second) to record the retry attempt.
//! Terminal failure states are `Poisoned` (retry-exhausted) and
//! `DependencyFailed` (upstream failed). A derivation observed in
//! `Failed` is mid-retry, not stuck.

use std::collections::{HashMap, HashSet};
use std::time::Instant;

use uuid::Uuid;

use super::{DrvHash, ExecutorId, TransitionError, db_str_enum};

/// LRU cap for the [`EffectiveFeatures::derive`] strip-warn debounce.
/// Key cardinality is `|pname| × |reason|` (`reason ∈ {non_fod_fetcher,
/// fod_declared_features}`, `|reason| = 2`). Bounded by the live drv
/// set; 1024 matches the sibling `unroutable_features_warned` /
/// `cap_mismatch_warned` caps. Eviction re-arms the warn (fail-safe
/// over-emit).
const FEATURES_STRIPPED_WARNED_CAP: usize = 1024;

/// Process-wide once-per-`(pname, reason)` debounce for
/// `rio_scheduler_features_stripped_total`. A static, not a `DagActor`
/// field, because the chokepoint ([`EffectiveFeatures::derive`]) is a
/// constructor invariant on `DerivationState` — it has no actor handle.
/// Same `LruCache` shape as `unroutable_features_warned` /
/// `cap_mismatch_warned` / `forecast_dropped_warned` (the
/// `ONCE_PER_MISS` contract). Re-arms on eviction and on pod restart.
static FEATURES_STRIPPED_WARNED: std::sync::LazyLock<
    parking_lot::Mutex<lru::LruCache<(String, &'static str), ()>>,
> = std::sync::LazyLock::new(|| {
    parking_lot::Mutex::new(lru::LruCache::new(
        std::num::NonZeroUsize::new(FEATURES_STRIPPED_WARNED_CAP).unwrap(),
    ))
});

/// §13e + r35: derived feature set for a derivation. The biconditional
/// `is_fixed_output ⟺ ∋ fetcher` is enforced **at construction** in
/// BOTH directions, so producers cannot mint a value that violates the
/// FOD↔Fetcher airgap and consumers cannot read a stale raw set.
///
/// Why a newtype and not a sibling `Vec<String>` field (§nth-strike
/// STRIKE-3): three rounds of "the chokepoint isn't total" (§13e B1,
/// §13e B4, r35 `hard_filter`/`statically_eligible`/`pool_covers`).
/// Each fix added another caller of the free-fn `effective_features`;
/// each missed the next site. The newtype has no `From<Vec<String>>`
/// and no `pub` field, so the only way to obtain one is `derive` —
/// the bypass is impossible by construction, not by discipline.
///
/// Stored on [`DerivationState`] alongside the raw `required_features`
/// (kept for the diagnostic API echo: `InspectBuildDag`,
/// `dispatch.rs`'s `failed_builders` warn, the PG persist).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct EffectiveFeatures(Vec<String>);

impl EffectiveFeatures {
    /// Single chokepoint: FOD ⟹ `[fetcher]`; non-FOD ⟹ raw − `{fetcher}`.
    ///
    /// The forward override (FOD ⟹ `[fetcher]`) is the §13e fix: a
    /// misconfigured FOD declaring `requiredSystemFeatures: ["kvm"]`
    /// would otherwise route to a kvm node with no fetcher airgap,
    /// breaking ADR-019.
    ///
    /// The reverse strip (non-FOD ⟹ raw − `{fetcher}`) is the
    /// merged_bug_004 close: `fetcher` is a rio-internal routing tag,
    /// not a tenant-declarable system feature. A non-FOD with
    /// `requiredSystemFeatures: ["fetcher"]` would otherwise route to
    /// (and idle-mint) a fetcher node it doesn't need, spending the
    /// tenant's $/hr on a node-class that never builds it. Stripped
    /// here ⟹ `effective_features = []` ⟹ wire
    /// `SpawnIntent.required_features = []` ⟹ `pool_covers` Fetcher
    /// tuple `["fetcher"]` rejects via the ∅-guard ⟹ routes to a
    /// Builder Pool like any non-featured drv.
    ///
    /// `pname` is for warn/metric attribution only — it does not
    /// affect the derived set.
    // r[impl sched.sla.fod-feature-derivation+3]
    pub fn derive(is_fixed_output: bool, raw: &[String], pname: Option<&str>) -> Self {
        let out: Vec<String> = if is_fixed_output {
            vec![rio_common::k8s::FETCHER_FEATURE.to_string()]
        } else {
            raw.iter()
                .filter(|f| f.as_str() != rio_common::k8s::FETCHER_FEATURE)
                .cloned()
                .collect()
        };
        // Observability: the strip is silent — a tenant whose non-FOD
        // legitimately needs network and declares `["fetcher"]` will
        // run on an air-gapped builder and fail with `Connection
        // refused`, an opaque sandbox error that doesn't say "your
        // `fetcher` feature was stripped." Same for a FOD declaring
        // `["kvm"]` — it routes to a fetcher node (no kvm). Reject-
        // at-submit is disproportionate (the strip produces correct
        // routing); warn + count instead. Debounced once per
        // `(pname, reason)` so a 10K-drv DAG from one misconfigured
        // tenant emits once, not 10K times.
        //
        // Two inline-literal `counter!` calls (not `=> reason` with a
        // bound variable) so `labeled_metric_values_have_emit_sites`
        // can statically scan the `"reason" => "<literal>"` pairs.
        //
        // Fire only when something the tenant DECLARED was removed —
        // NOT when the chokepoint *added* a feature. A FOD with the
        // spec-correct `requiredSystemFeatures: []` derives to
        // `[fetcher]`; `out != raw` would warn on EVERY correctly
        // configured FOD (the common case), and the metric named for
        // a strip would count an addition. Same for `from_poisoned_row`
        // (always passes `raw=[]`): a recovered Poisoned FOD must not
        // count as a tenant misconfig.
        if raw.iter().any(|f| !out.contains(f)) {
            let reason: &'static str = if is_fixed_output {
                "fod_declared_features"
            } else {
                "non_fod_fetcher"
            };
            let key = (pname.unwrap_or("").to_string(), reason);
            if FEATURES_STRIPPED_WARNED.lock().put(key, ()).is_none() {
                if is_fixed_output {
                    ::metrics::counter!(
                        "rio_scheduler_features_stripped_total",
                        "reason" => "fod_declared_features",
                    )
                    .increment(1);
                } else {
                    ::metrics::counter!(
                        "rio_scheduler_features_stripped_total",
                        "reason" => "non_fod_fetcher",
                    )
                    .increment(1);
                }
                tracing::warn!(
                    declared = ?raw,
                    effective = ?out,
                    is_fod = is_fixed_output,
                    pname = pname.unwrap_or(""),
                    reason,
                    "stripped declared features at chokepoint — \
                     is_fixed_output ⟺ ∋ fetcher is enforced at construction; \
                     the declared set is preserved for diagnostics only",
                );
            }
        }
        Self(out)
    }

    /// Borrow the derived set. The only read path — there is no
    /// `Deref<Target=Vec<String>>` and no public field, so a future
    /// writer cannot `.push("kvm")` past the constructor invariant.
    pub fn as_slice(&self) -> &[String] {
        &self.0
    }

    /// Consume into the inner `Vec<String>` (for the wire
    /// `SpawnIntent.required_features` populate).
    pub fn into_vec(self) -> Vec<String> {
        self.0
    }
}

db_str_enum! {
    // r[impl sched.state.machine]
    /// State of a single derivation in the global DAG.
    ///
    /// The macro-generated [`ALL`](Self::ALL) const lists variants in
    /// the order the golden snapshot at
    /// `rio-scheduler/tests/golden/derivation_statuses.json` expects —
    /// the snapshot test, the exhaustive transition-table test, and the
    /// dashboard's cross-language cardinality check (vitest reads the
    /// same golden) all key on it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum DerivationStatus {
        Created = "created",
        Queued = "queued",
        Ready = "ready",
        Assigned = "assigned",
        Running = "running",
        /// Upstream substitution in flight: a background task is doing
        /// `QueryPathInfo` (which triggers store-side `try_substitute`)
        /// for this derivation's outputs. Dependents stay gated (NOT
        /// Completed/Skipped); the spawned task posts `SubstituteComplete`
        /// when done. r[sched.substitute.detached+5]
        Substituting = "substituting",
        Completed = "completed",
        Failed = "failed",
        Poisoned = "poisoned",
        /// A dependency of this derivation failed/poisoned. This derivation can
        /// never complete in the current build. Terminal (like Poisoned).
        /// Maps to Nix BuildStatus::DependencyFailed=10.
        DependencyFailed = "dependency_failed",
        /// Explicitly cancelled via CancelBuild (all interested builds cancelled)
        /// or DrainExecutor(force). Terminal but distinct from Poisoned: no
        /// implication of build defect, just scheduler/operator decision.
        /// No TTL reset — a cancelled build stays cancelled; retry means
        /// re-submitting. Worker's cgroup.kill SIGKILLs the daemon tree,
        /// cleanup is immediate (no 2h terminationGracePeriodSeconds wait).
        Cancelled = "cancelled",
        /// Terminal. CA early-cutoff: a CA dependency completed with
        /// byte-identical output (content-index match), so this derivation
        /// would produce the same output as already in the store. Skipped
        /// without running. Distinct from Completed for metrics
        /// (`rio_scheduler_ca_cutoff_saves_total`) and audit trail.
        /// Queued|Ready → Skipped (Ready is order-independent vs
        /// `find_newly_ready` — matches DependencyFailed precedent).
        Skipped = "skipped",
    }
    parse_err(other) = TransitionError: TransitionError::UnknownStatus(other.to_string());
}

impl DerivationStatus {
    /// Whether this is a terminal state (no further progress without external reset).
    pub fn is_terminal(self) -> bool {
        matches!(
            self,
            Self::Completed
                | Self::Poisoned
                | Self::DependencyFailed
                | Self::Cancelled
                | Self::Skipped
        )
    }

    /// Whether a resubmit of this derivation should reset it for re-dispatch.
    ///
    /// `Cancelled`: explicit cancel OR worker-side timeout (`BuildResultStatus::
    /// TimedOut` routes here, not to `Poisoned` — a timeout isn't a build
    /// defect, just needs more time or different conditions). Per the
    /// `Cancelled` doc-comment: "retry means re-submitting". Reset is
    /// defense-in-depth (reap now removes Cancelled nodes for terminal
    /// builds) — covers a resubmit during `TERMINAL_CLEANUP_DELAY` or a
    /// node shared with a still-active build at cancel time. Without reset,
    /// `merge()` adds interest but `compute_initial_states` only iterates
    /// `newly_inserted` — the resubmitted build would hang.
    ///
    /// `Failed`: transient-fail with no retry driver pending — resubmit retries.
    ///
    /// `DependencyFailed`: derived state — reset lets `compute_initial_states`
    /// re-evaluate `any_dep_terminally_failed` fresh. If the dep is still
    /// `Poisoned`, it goes back to `DependencyFailed` (same fast-fail). If the
    /// dep was `Cancelled` (reset by this same merge), it goes `Queued`/`Ready`.
    ///
    /// NOT retriable here: `Completed` (cache hit), `Poisoned` (handled at
    /// the [`DerivationState`] level — needs `retry_count` for the bounded
    /// check; see [`DerivationState::is_retriable_on_resubmit`]).
    pub fn is_retriable_on_resubmit(self) -> bool {
        matches!(
            self,
            Self::Cancelled | Self::Failed | Self::DependencyFailed
        )
    }

    // r[impl sched.state.transitions]
    // r[impl sched.state.terminal-idempotent]
    // r[impl sched.state.poisoned-ttl]
    // r[impl sched.completion.idempotent]
    /// Validate a state transition.
    ///
    /// Returns `Ok(())` if the transition is valid, `Err` with a description otherwise.
    pub fn validate_transition(self, to: Self) -> Result<(), TransitionError> {
        // Idempotent no-ops
        if self == to {
            match self {
                Self::Completed | Self::Poisoned => return Ok(()),
                Self::DependencyFailed | Self::Cancelled | Self::Skipped => return Ok(()),
                _ => {
                    return Err(TransitionError::Invalid {
                        from: self,
                        to,
                        reason: "non-terminal self-transition is not allowed",
                    });
                }
            }
        }

        // Terminal -> non-terminal is rejected, with carve-outs:
        if self.is_terminal() && !to.is_terminal() {
            if self == Self::Poisoned && to == Self::Created {
                // 24h TTL expiry resets poisoned -> created
                return Ok(());
            }
            if matches!(self, Self::Completed | Self::Skipped)
                && matches!(to, Self::Ready | Self::Queued)
            {
                // I-047: output GC'd from store between completion and
                // a later build's merge. Reset to Ready (deps
                // available) or Queued (deps also reset). Skipped
                // carries real output_paths and unlocks dependents the
                // same as Completed, so a GC'd Skipped output needs
                // the same reset.
                return Ok(());
            }
            if matches!(self, Self::Poisoned | Self::DependencyFailed) && to == Self::Queued {
                // I-094 deferred re-probe: output is locally present
                // (cache-hit) but an inputDrv is in-flight, so the
                // closure-invariant fixed-point deferred this hit.
                // Prior failure history is moot; gate on the dep via
                // Queued so find_newly_ready picks it up when the dep
                // completes. Parallel to the →Substituting arm below.
                // (Failed is non-terminal; its Queued arm is in the
                // table below.)
                return Ok(());
            }
            if matches!(self, Self::Poisoned | Self::DependencyFailed) && to == Self::Substituting {
                // I-094 reprobe substitutable lane (see match arm
                // below). Substituting is non-terminal because a
                // failed fetch reverts to Ready/Queued.
                return Ok(());
            }
            return Err(TransitionError::TerminalToNonTerminal { from: self, to });
        }

        // Valid transitions
        let valid = match (self, to) {
            (Self::Created, Self::Completed) => true, // merge-time cache hit
            // Dispatch-time cache hit (I-067): a Ready FOD whose
            // output already exists in rio-store. Distinct from
            // Created→Completed: the merge-time check_cached_outputs
            // only checks newly_inserted, so a derivation that was
            // already in-DAG as Ready (e.g. stuck via I-062's
            // resource-fit, or reset via verify_preexisting_completed)
            // never gets re-checked there. batch_probe_cached_ready()
            // re-checks at dispatch and short-circuits the fetch.
            (Self::Ready, Self::Completed) => true,
            // Merge-time re-probe (I-099/I-094): a node that was
            // already in-DAG (inserted by an earlier build) but not
            // yet built, re-probed against the upstream cache when a
            // later build references it. If the output now exists
            // (e.g., upstream cache configured AFTER first insert),
            // skip directly to Completed regardless of current state.
            // Poisoned/DependencyFailed/Failed → Completed: prior
            // failure is moot — we have the output. Caller is
            // responsible for clearing poison fields and DB state.
            (Self::Queued, Self::Completed) => true,
            (Self::Poisoned, Self::Completed) => true,
            (Self::DependencyFailed, Self::Completed) => true,
            // Failed is symmetry-only: Failed is reset by dag.merge
            // today (`is_retriable_on_resubmit`), so a pre-existing
            // Failed node lands in newly_inserted and re-enters at
            // Created. Kept parallel to Poisoned/DependencyFailed for
            // the I-094 reprobe lane so the state machine and the
            // merge.rs callers (existing_reprobe match,
            // apply_cached_hits, deferred-reprobe stanza) agree —
            // defense-in-depth if `is_retriable_on_resubmit` ever
            // bounds Failed by retry-count.
            (Self::Failed, Self::Completed) => true,
            (Self::Created, Self::Queued) => true, // build accepted
            (Self::Queued, Self::Ready) => true,   // all deps complete
            // I-047 parent-side: a dep's output was GC'd and reset
            // (Completed→Ready/Queued above), so this node's Ready
            // verdict ("all deps' outputs available") no longer
            // holds. Demote to Queued; find_newly_ready re-promotes
            // when the reset dep re-completes. r[sched.merge.stale-
            // completed-verify]
            (Self::Ready, Self::Queued) => true,
            (Self::Queued, Self::DependencyFailed) => true, // dep poisoned, cascade
            (Self::Ready, Self::DependencyFailed) => true,  // dep poisoned, cascade
            (Self::Created, Self::DependencyFailed) => true, // dep poisoned before queue
            // I-047 stale-completed reset, dep terminally-failed (the
            // I-094 reprobe lane can leave a Poisoned dep under a
            // Completed parent; `revert_target_for` 3-way).
            (Self::Completed | Self::Skipped, Self::DependencyFailed) => true,
            (Self::Ready, Self::Assigned) => true, // worker selected
            (Self::Assigned, Self::Running) => true, // worker ack
            (Self::Assigned, Self::Ready) => true, // worker lost
            (Self::Running, Self::Completed) => true, // build succeeded
            (Self::Running, Self::Failed) => true, // retriable failure
            (Self::Running, Self::Poisoned) => true, // failed on 3+ workers
            (Self::Ready, Self::Poisoned) => true, // failed_builders exhausts fleet (I-065)
            (Self::Failed, Self::Ready) => true,   // retry scheduled
            // I-094 deferred re-probe: output present but inputDrv
            // in-flight; failure history moot, gate on dep via Queued.
            (Self::Failed, Self::Queued) => true,
            // Cancel: from any in-flight state. CancelBuild sends
            // CancelSignal to workers running sole-interest derivations;
            // DrainExecutor(force) cancels all a worker's in-flight.
            // Both require the derivation to be Assigned or Running —
            // if it's still Queued/Ready (not dispatched yet), just
            // remove build interest instead (handle_cancel_build's
            // existing orphan-removal path).
            (Self::Assigned, Self::Cancelled) => true, // cancel before worker ACK
            (Self::Running, Self::Cancelled) => true,  // cancel mid-build
            // CA early-cutoff: a CA dep completed with unchanged
            // output hash → this derivation would produce the same
            // output. Skip without running. Ready is allowed for
            // order-independence vs find_newly_ready (cascade may
            // race a prior Queued→Ready promotion — matches
            // DependencyFailed precedent at completion.rs).
            (Self::Queued | Self::Ready, Self::Skipped) => true,
            // r[impl sched.substitute.detached+5]
            // Detached upstream fetch: spawned from any pre-dispatch
            // state (merge-time) or Ready (dispatch-time). Completed →
            // fetch landed; Ready/Queued → fetch failed, fall through
            // to normal scheduling. Cancelled → all interested builds
            // cancelled mid-fetch (orphan task is benign — it still
            // populates the store, the SubstituteComplete is dropped).
            (Self::Created | Self::Queued | Self::Ready, Self::Substituting) => true,
            // I-094 reprobe, substitutable lane: poisoned/dep-failed/
            // failed node's output is upstream-substitutable on
            // resubmit — same "prior failure is moot" rationale as
            // the (Poisoned, Completed) arm above. DependencyFailed
            // and Failed are symmetry-only (both reset by dag.merge
            // today) but kept parallel to the →Completed arms above.
            (Self::Poisoned | Self::DependencyFailed | Self::Failed, Self::Substituting) => true,
            (Self::Substituting, Self::Completed | Self::Ready | Self::Queued) => true,
            // Fetch failed AND a dep is terminally-failed (I-094
            // reprobe of a node whose dep stayed Poisoned). Mirror
            // compute_initial_states' 3-way via `revert_target_for`.
            (Self::Substituting, Self::DependencyFailed) => true,
            (Self::Substituting, Self::Cancelled) => true,
            _ => false,
        };

        if valid {
            Ok(())
        } else {
            Err(TransitionError::Invalid {
                from: self,
                to,
                reason: "transition not in state machine",
            })
        }
    }
}

/// Retry / failure-tracking sub-state of a [`DerivationState`].
///
/// All fields are **in-memory only** unless otherwise noted: recovery
/// resets them to conservative defaults (won't spuriously poison after
/// restart). `failed_builders` is persisted; `failure_count` is
/// re-derived from it on recovery (same-worker repeats forgiven).
#[derive(Debug, Clone, Default)]
pub struct RetryState {
    /// Number of retry attempts so far in the CURRENT poison cycle.
    /// Gated against `RetryPolicy::max_retries` at completion. Reset to
    /// 0 on resubmit-after-poison (fresh per-cycle budget). See
    /// [`Self::resubmit_cycles`] for the cross-cycle counter.
    pub count: u32,
    /// Number of poison→resubmit reset events. Gated against
    /// [`POISON_RESUBMIT_RETRY_LIMIT`]. Incremented by `dag::merge` on
    /// each resubmit-reset; persisted (`M_051`) so the bound survives
    /// leader failover. Distinct from [`Self::count`]: a single counter
    /// cannot be both per-cycle-reset and cross-cycle-accumulated —
    /// when `count` served both roles, the `max_retries=2` cap was the
    /// permanent ceiling and the resubmit bound never fired (bug_152).
    pub resubmit_cycles: u32,
    /// Number of InfrastructureFailure re-dispatches so far. Separate
    /// from `count` because infra failures don't count toward the
    /// transient-failure budget (they're worker-local, not build-local)
    /// — but still bounded to prevent a misclassified deterministic
    /// failure from hot-looping forever.
    pub infra_count: u32,
    /// Number of `TimedOut` re-dispatches so far (I-200). Separate
    /// from `count` (timeouts don't eat the transient budget) and from
    /// `infra_count` (no time-window reset — a build that times out,
    /// gets promoted, and times out again an hour later on the larger
    /// class is still the same hung build). Bounded by
    /// `RetryPolicy::max_timeout_retries`; at the cap,
    /// `handle_timeout_failure` falls through to terminal Cancelled.
    pub timeout_count: u32,
    /// Timestamp of the most recent InfrastructureFailure that
    /// incremented `infra_count`. Drives the time-window reset
    /// (I-127): if the last infra failure was longer ago than
    /// `RetryPolicy::infra_retry_window_secs`, `infra_count` resets to
    /// 0 before the cap check — sparse failures over a long build
    /// don't accumulate toward poison.
    pub last_infra_failure_at: Option<Instant>,
    /// Number of `exempt_from_cap` infra-retry attempts so far
    /// (I-127's CONCURRENT_PUTPATH + D4's `floor_outcome.promoted`).
    /// Increments even when `infra_count` does not — the high-water
    /// terminal that keeps the cap-exemption from livelocking under a
    /// leaked store-side placeholder lock (the I-125a class). Bounded
    /// by `RetryPolicy::max_exempt_infra_retries`; no time-window
    /// reset (a stuck lock that persists across the window is exactly
    /// what this counter exists to catch).
    pub exempt_infra_count: u32,
    /// Workers that have failed building this derivation. Drives
    /// `best_executor()` exclusion + poison threshold in distinct mode.
    /// Persisted.
    pub failed_builders: HashSet<ExecutorId>,
    /// Total TransientFailure/disconnect count (same-worker repeats
    /// counted). Drives poison threshold when
    /// `PoisonConfig::require_distinct_workers = false` (single-worker
    /// dev deployments). Recovery initializes to `failed_builders.len()`.
    /// InfrastructureFailure does NOT increment this (T1's split).
    pub failure_count: u32,
    /// When the derivation entered the poisoned state (for TTL expiry).
    pub poisoned_at: Option<Instant>,
    /// Earliest time this derivation may be dispatched. Set by
    /// handle_transient_failure to implement the retry backoff —
    /// the derivation is Ready and in the queue, but dispatch_ready
    /// defers it if `Instant::now() < backoff_until`.
    ///
    /// Why not a timer-based requeue: timers need a scheduled task
    /// per deferred derivation + cleanup if the derivation
    /// transitions meanwhile (cancelled, DAG reload). Putting the
    /// deadline ON the state and checking in dispatch_ready is
    /// stateless — the existing defer-and-requeue pattern handles
    /// it. Cost: one Instant::now() comparison per Ready-pop for
    /// derivations that have backoff set (only transient-failures).
    ///
    /// Cleared on successful dispatch (assign_to_worker).
    pub backoff_until: Option<Instant>,
}

impl RetryState {
    /// Reset all failure-tracking fields. Call after a cache-hit
    /// transition from Poisoned/DependencyFailed/Failed to Completed
    /// (I-099/I-094) — the prior failures are moot once the output
    /// exists. Does NOT change `status`; caller transitions separately.
    pub fn clear(&mut self) {
        self.count = 0;
        self.resubmit_cycles = 0;
        self.infra_count = 0;
        self.timeout_count = 0;
        self.last_infra_failure_at = None;
        self.exempt_infra_count = 0;
        self.failed_builders.clear();
        self.failure_count = 0;
        self.poisoned_at = None;
    }
}

/// Content-addressed-derivation sub-state of a [`DerivationState`].
///
/// All fields except `is_ca` are **in-memory only**: recovered CA-on-CA
/// chains dispatch unresolved (collect_ca_inputs skips None) → worker
/// fails on placeholder → retry. The gateway recomputes on the NEXT
/// SubmitBuild that references the derivation. The one exception is
/// `modular_hash` for authoritative hook-fallback nodes: recovery
/// recomputes it from the persisted inline content
/// (`sched.recovery.inline-drv-ca-hash`) so post-failover completion
/// still registers the realisation.
#[derive(Debug, Clone, Default)]
pub struct CaState {
    /// Whether this derivation is content-addressed (fixed-output OR
    /// floating-CA). Drives CA early-cutoff: on completion the
    /// scheduler compares the output's nar_hash against the content
    /// index, skipping downstream builds on match. Set at gateway
    /// translate from `has_ca_floating_outputs() || is_fixed_output()`,
    /// propagated via proto `DerivationNode.is_content_addressed`.
    ///
    /// Distinct from `is_fixed_output`: a floating-CA derivation
    /// (`__contentAddressed = true` in Nix) is CA but not FOD (no
    /// predeclared hash — the output hash is computed post-build).
    /// Persisted.
    pub is_ca: bool,
    /// Whether this derivation needs dispatch-time placeholder
    /// resolution (ADR-018 Appendix B `shouldResolve`). Set at
    /// gateway translate from `has_ca_floating_outputs()` OR
    /// any-inputDrv-is-floating-CA (`ia.deferred`), propagated via
    /// proto `DerivationNode.needs_resolve`.
    ///
    /// Distinct from `is_ca`: an IA derivation with a floating-CA
    /// input has that input's placeholder embedded in env/args and
    /// needs resolve to rewrite it, even though the IA drv itself
    /// has a known output path. `is_ca` gates cutoff-compare;
    /// `needs_resolve` gates `maybe_resolve_ca`.
    pub needs_resolve: bool,
    /// For CA derivations: the modular derivation hash
    /// (`hashDerivationModulo` SHA-256). Realisations table PK half.
    /// Set at DAG merge from proto `DerivationNode.ca_modular_hash`
    /// (the gateway computes it post-BFS from the full drv_cache, and
    /// for the single-node `BasicDerivation` hook fallback from the
    /// lifted inline derivation). `None` for IA derivations. After a
    /// scheduler failover it is recomputed from the persisted
    /// authoritative inline content for hook-fallback nodes
    /// (`sched.recovery.inline-drv-ca-hash`); ordinary DAG-path CA
    /// nodes recover with `None` (lossy, see struct doc).
    ///
    /// Consumed by:
    /// - `collect_ca_inputs` ([`crate::actor`] dispatch) — this node
    ///   as a CA INPUT of a parent; `None` → skip, parent's resolve
    ///   is incomplete → worker fails on placeholder → retry.
    /// - `handle_success_completion` — this node's own
    ///   `(modular_hash, output_name)` for the `realisation_deps`
    ///   insert (the PARENT side of the junction).
    pub modular_hash: Option<[u8; 32]>,
    /// Realisation lookups from dispatch-time resolve. Consumed by
    /// `handle_success_completion` → `insert_realisation_deps` AFTER
    /// the parent's own realisation lands (the FK needs the parent's
    /// row in `realisations` to exist, which only happens post-build
    /// via `wopRegisterDrvOutput` — see resolve.rs's FK-ordering doc).
    ///
    /// Empty for IA derivations and for CA derivations whose resolve
    /// was a no-op (no CA inputs). Populated by `maybe_resolve_ca` in
    /// the dispatch path; consumed + drained at completion time.
    pub pending_realisation_deps: Vec<crate::ca::RealisationLookup>,
    /// CA cutoff-compare result: true iff EVERY output's nar_hash
    /// matched the content index on completion. Set by
    /// `handle_success_completion` (`r[sched.ca.cutoff-compare]`);
    /// consumed by `cascade_cutoff` via `find_cutoff_eligible_speculative` (`r[sched.ca.cutoff-propagate+2]`,
    /// P0252). Default `false` — only a positive all-match flips it.
    ///
    /// AND-fold semantics: a multi-output CA derivation with one
    /// matched and one missed output is `false`. The single-bool MVP
    /// doesn't distinguish which output matched; per-output
    /// granularity is a later refinement (downstream builds depend
    /// on specific outputs, so a partial match CAN skip some).
    ///
    /// **NOT persisted.** If the scheduler restarts between the
    /// compare (set) and the cascade (consume), the flag resets to
    /// `false` on recovery — downstream builds proceed normally
    /// (no cutoff). This is correctness-safe (rebuild > stale-skip)
    /// at the cost of one wasted build per affected derivation.
    /// The window is tight: the cascade runs in the SAME
    /// `handle_success_completion` call as the set, so the
    /// compare→propagate gap is a single actor-tick iteration.
    pub output_unchanged: bool,
}

// r[impl sched.sla.reactive-floor+2]
/// Per-dimension resource floor for the NEXT dispatch (D4).
///
/// Reactive promotion: an explicit resource-exhaustion signal
/// (controller-reported `OomKilled`/`EvictedDiskPressure`/
/// `DeadlineExceeded`, worker-reported `CgroupOom`/`TimedOut`) calls
/// `actor::floor::bump_floor_or_count` which doubles the
/// relevant dimension, capped at `Ceilings`. `solve_intent_for` clamps
/// its solved (mem, disk) at this floor before returning so the next
/// SpawnIntent is at least as large.
///
/// `Default` = zeros = no clamp (cold start). Persisted as
/// `derivations.floor_{mem,disk,deadline}_*` (`M_044`) so a scheduler
/// failover between OOM and retry doesn't reset to zero → re-OOM at
/// probe defaults.
///
/// No `cores` dimension: OOM/DiskPressure are mem/disk under-
/// provision; DeadlineExceeded is a wall-time bound, not a
/// parallelism bound. The SLA model owns core selection.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub struct ResourceFloor {
    pub mem_bytes: u64,
    pub disk_bytes: u64,
    pub deadline_secs: u32,
}

/// Output of `solve_intent_for`: per-derivation `(cores, mem, disk,
/// deadline)` SpawnIntent shape + the dispatch-time SLA prediction
/// snapshot + the cost-routed nodeSelector. Stored on
/// [`SchedHint::last_intent`] at dispatch so `hard_filter` /
/// `build_assignment_proto` / `bump_floor_or_count` /
/// `record_build_sample` all read the SAME solve.
#[derive(Debug, Clone, Default)]
pub struct SolvedIntent {
    pub cores: u32,
    pub mem_bytes: u64,
    pub disk_bytes: u64,
    pub deadline_secs: u32,
    /// `None` on cold-start / probe / forced-cores override — the
    /// prediction-ratio histogram only sees model-backed dispatches.
    pub predicted: Option<crate::sla::solve::SlaPrediction>,
    /// ADR-023 §13a OR-of-ANDs `(h, cap)` targeting when `solve_full`
    /// ran; empty for the hw-agnostic path. The legacy single-cell
    /// `node_selector` map is gone — `compute_spawn_intents` leaves
    /// proto field 5 empty; the controller stamps the full
    /// `node_affinity` term-list onto
    /// `pod.spec.affinity.nodeAffinity.required…`
    /// (`r[ctrl.pool.node-affinity-from-intent]`).
    pub node_affinity: Vec<rio_proto::types::NodeSelectorTerm>,
    /// Operator's `[sla.hw_classes.$h]` keys parallel to
    /// `node_affinity` — `hw_class_names[i]` is the `h` whose label
    /// conjunction produced `node_affinity[i]`. Carried through to
    /// `SpawnIntent.hw_class_names` so the controller doesn't
    /// reverse-engineer `h` from a hardcoded label schema. Empty for
    /// the hw-agnostic path (same as `node_affinity`).
    pub hw_class_names: Vec<String>,
    /// ADR-023 §sizing: `headroom(fit.n_eff_ring)` — the variance-aware
    /// overlay-disk multiplier. Low `n_eff` (cold/noisy fit) → wide
    /// cushion; high `n_eff` → tight. Carried through
    /// `SpawnIntent.disk_headroom_factor` so the controller's
    /// `pod_ephemeral_request` and the NodeClaim disk floor agree
    /// without reimplementing the curve. Unfitted (probe/cold) → the
    /// flat 1.5× fallback.
    pub disk_headroom: f64,
}

/// Scheduling-hint sub-state of a [`DerivationState`].
///
/// Estimator outputs and critical-path priority. All fields are
/// **in-memory only** except `resource_floor` (persisted as
/// `derivations.floor_*`, `M_044`); the rest are recomputed at
/// next dispatch / `full_sweep`.
#[derive(Debug, Clone, Default)]
pub struct SchedHint {
    /// D4: per-dimension reactive floor. See [`ResourceFloor`].
    pub resource_floor: ResourceFloor,
    /// Dispatch-time `solve_intent_for` output. `hard_filter` reads
    /// `mem_bytes` (resource-fit), `build_assignment_proto` reads
    /// `cores` (`WorkAssignment.assigned_cores`), `bump_floor_or_count`
    /// reads `mem/disk/deadline` as the doubling base,
    /// `record_build_sample` reads `predicted` for actual-vs-predicted
    /// scoring.
    ///
    /// Populated at DISPATCH time (`dispatch_ready`), not merge time —
    /// the estimator refreshes on Tick, so a long-queued derivation
    /// picks up fresh history. `None` = never dispatched (cold start /
    /// recovery) — `hard_filter` treats it as "any worker fits".
    /// In-memory only.
    pub last_intent: Option<SolvedIntent>,
    /// Estimated build duration (from Estimator). Set at merge time;
    /// never updated after. The critical-path priority uses this;
    /// stale is fine (a build taking longer than estimated doesn't
    /// change the OPTIMAL schedule mid-execution — what's queued is
    /// queued).
    pub est_duration: f64,
    /// Critical-path priority: `est_duration + max(children's priority)`.
    /// Bottom-up: leaves have `priority = est_duration`; roots have
    /// the sum along the longest path. Higher = more urgent (dispatch
    /// first). Recomputed incrementally on completion via
    /// ancestor-walk. The ready queue uses this for BinaryHeap ordering.
    pub priority: f64,
}

/// In-memory state for a single derivation node in the global DAG.
#[derive(Debug, Clone)]
pub struct DerivationState {
    /// Unique hash identifying this derivation (store path for input-addressed, modular hash for CA).
    pub drv_hash: DrvHash,
    /// Store path of the .drv file. Private because the DAG maintains a
    /// `path_to_hash` reverse index keyed on this field — mutating it
    /// directly would silently corrupt that index. Read via `drv_path()`.
    drv_path: rio_nix::store_path::StorePath,
    /// Package name (for duration estimation).
    pub pname: Option<String>,
    /// Package version (drv.env `version`). ADR-023: feeds
    /// `build_samples.version` for version-distance sample weighting.
    /// `None` on recovery — best-effort sizing input, not persisted.
    pub version: Option<String>,
    /// drv.env `enableParallelBuilding`. ADR-023: when explicitly
    /// `Some(false)` → fix p̄=1 (no multi-core exploration). `None`
    /// means unknown (NOT false — historical stdenv default was unset).
    pub enable_parallel_building: Option<bool>,
    /// drv.env `enableParallelChecking`. ADR-023 §Model-staging:
    /// recorded into `build_samples` (migration 056) so a later p̄:=1
    /// seed can distinguish "compile scales, checkPhase serial". Not
    /// consulted by the solver yet — telemetry-only.
    pub enable_parallel_checking: Option<bool>,
    /// drv.env `preferLocalBuild`. ADR-023: `Some(true)` → trivially
    /// short, skip learning entirely.
    pub prefer_local_build: Option<bool>,
    /// Target system (e.g. "x86_64-linux").
    pub system: String,
    /// Declared `requiredSystemFeatures` from the proto node, verbatim.
    /// Diagnostic echo only — routing reads [`Self::effective_features`].
    /// Private: ALL writes must go through [`Self::set_required_features`]
    /// so `effective_features` re-derives atomically (the §13e+r35
    /// chokepoint). Read via [`Self::required_features`].
    required_features: Vec<String>,
    /// §13e + r35: derived feature set. The biconditional
    /// `is_fixed_output ⟺ ∋ fetcher` is enforced at construction (and
    /// re-derived on every [`Self::set_required_features`]) so
    /// producers cannot mint a value that violates the FOD↔Fetcher
    /// airgap. Private — read via [`Self::effective_features`] (no
    /// `Deref`, no `pub` field, no `From<Vec<String>>`; the only
    /// producer is [`EffectiveFeatures::derive`]).
    effective_features: EffectiveFeatures,
    /// Output names (e.g. ["out", "dev"]).
    pub output_names: Vec<String>,
    /// Whether this is a fixed-output derivation (fetchurl, etc.).
    pub is_fixed_output: bool,
    /// Content-addressed-derivation state (cutoff/resolve bookkeeping).
    pub ca: CaState,
    /// Current state machine status. Private: mutate only via `transition()`
    /// or `reset_to_ready()` to preserve invariants.
    status: DerivationStatus,
    /// Set of build IDs interested in this derivation.
    pub interested_builds: HashSet<Uuid>,
    /// Worker currently assigned/running this derivation.
    pub assigned_executor: Option<ExecutorId>,
    /// Per-execution identifier minted by `assign_to_worker` for the
    /// active assignment. UUIDv7 — keys the `drv_logs` PG row and the
    /// `logs/{drv_hash}/{exec_id}.log.zst` S3 blob. Mirrors the
    /// `LogBuffers` ring-buffer entry's `exec_id` (the flusher's read
    /// path) and `assignments.exec_id` (the recovery carrier). NOT the
    /// flusher's read path — the actor and the flusher are deliberately
    /// decoupled (see `logs/mod.rs` module header).
    ///
    /// `None` on construction. Set by `assign_to_worker`; cleared by
    /// `reset_to_ready` (worker disconnect, phantom drain, orphan
    /// reconcile, infra/timeout retry below cap, and `rollback_assignment` —
    /// that last one ALSO discards the `LogBuffers` entry, so neither
    /// carrier survives a failed dispatch) and by `transition()` on any
    /// terminal → non-terminal reset (I-094 reprobe, I-047 stale-output
    /// reset — the prior execution was already finalized at its
    /// terminal and must not be attributed to the node's next
    /// lifecycle). The reader is `terminal_log_epilogue` (which
    /// resolves once via `actor/event.rs::exec_id_for_terminal` and
    /// threads the value to the seal/flush/correlate steps); the
    /// resolution can run
    /// between a `reset_to_ready` and the next `assign_to_worker` when
    /// a poison-while-Ready path (I-065 fleet exhaustion,
    /// max_infra/timeout_retries cap) reaches `terminal_failure_epilogue`
    /// first, and falls back to the `LogBuffers` entry's stamped
    /// exec_id, which `reset_to_ready` does NOT clear (the lines and
    /// stamp are still needed by the periodic flusher in the
    /// reset→re-dispatch window).
    ///
    /// Recovery preserves the clear across leader failover: the recovery
    /// query only carries `assignments.exec_id` for currently-assigned
    /// drvs (`load_nonterminal_derivations`), so a reset drv's leaked
    /// `pending` assignments row cannot re-stamp this field on the new
    /// leader.
    pub exec_id: Option<Uuid>,
    /// Scheduling hints (estimator outputs, resource_floor, critical-path priority).
    pub sched: SchedHint,
    /// ATerm-serialized .drv content, inlined by the gateway for
    /// nodes that will actually dispatch (outputs missing from store).
    /// Empty = worker fetches from store via GetPath (fallback
    /// path, still works). Forwarded verbatim into WorkAssignment.
    /// Bounded at gRPC ingress by `rio_common::limits::MAX_DRV_CONTENT_BYTES`
    /// (1 MiB, the content-bound hook-fallback cap; the inline
    /// optimization producer stays ≤64 KiB per node).
    pub drv_content: Vec<u8>,
    /// Whether `drv_content` is the AUTHORITATIVE copy of the
    /// derivation (content-bound hook fallback: the bytes exist
    /// nowhere else). Set from the proto flag at merge time and
    /// re-derived from row presence on recovery. Gates the
    /// merge-time content protections
    /// (`sched.merge.authoritative-conflict`): a later submission can
    /// neither join this node with different authoritative bytes nor
    /// silently attach a conflicting verifiable identity to it.
    pub drv_content_authoritative: bool,
    /// `inputSrcs` from the derivation ATerm — already-built store
    /// paths this derivation reads (NOT in the DAG as child nodes).
    /// Parsed once at merge time so `approx_input_closure` can
    /// include them without re-parsing per dispatch. Empty when
    /// `drv_content` is empty/unparseable (recovered derivation,
    /// or gateway didn't inline) — prefetch falls back to DAG-
    /// children-only, same as before.
    pub input_srcs: Vec<String>,
    /// Retry / failure-tracking state.
    pub retry: RetryState,
    /// Realized output store paths (filled on completion).
    pub output_paths: Vec<String>,
    /// Expected output paths (from the proto node at merge time).
    /// Used for: cache-check (merge.rs), and prefetch-hint closure
    /// approximation (children's expected_output_paths = parent's
    /// inputs; see `approx_input_closure`).
    pub expected_output_paths: Vec<String>,
    /// Output NAMES any consumer actually references (∪ over parents'
    /// inputDrvs sets ∪ the root OutputsSpec). EMPTY = all declared
    /// outputs wanted. This is the STORED node-level union: it grows
    /// monotonically via union_wanted(), never shrinks, and is what the
    /// persistence upsert and recovery carry. Classification consults
    /// it only as the conservative fallback when [`effective_wanted`]
    /// cannot resolve a live-build union from [`Self::wanted_by_build`];
    /// everything else (assignment token, GC pins, prefetch, client
    /// output report) keeps using expected_output_paths.
    pub wanted_output_names: Vec<String>,
    /// Per-build wanted-output contributions: for each interested build,
    /// the `wanted_output_names` of THAT build's submission for this
    /// node (the per-submission sets that [`Self::wanted_output_names`]
    /// unions together). An entry whose value is the EMPTY Vec means
    /// that build wants ALL declared outputs (same sentinel as the
    /// union). A MISSING entry for an interested build means its
    /// contribution is UNKNOWN — recovery rebuilds `interested_builds`
    /// from `build_derivations`, which carries no per-build wanted data
    /// — and [`effective_wanted`] must then fall back to the stored
    /// node-level union instead of guessing.
    ///
    /// Entries follow `interested_builds` membership exactly: recorded
    /// where interest is recorded (`dag.merge`'s existing-node and
    /// new-node paths, plus the resubmit-reset carry-over), removed
    /// where interest is removed (`rollback_merge`,
    /// `remove_build_interest`, `remove_build_interest_and_reap`).
    /// In-memory only — never persisted, so the map starts empty after
    /// leader failover.
    pub wanted_by_build: HashMap<Uuid, Vec<String>>,
    /// Database UUID (set after insertion).
    pub db_id: Option<Uuid>,
    /// When the derivation entered Ready state (for assignment latency metric).
    pub(crate) ready_at: Option<Instant>,
    /// When the derivation entered Running state. For the backstop
    /// timeout: handle_tick checks this + est_duration × 3 (clamped
    /// to build_timeout + slack). A build that's been Running far
    /// longer than expected is likely stuck (worker heartbeating
    /// but the build wedged, or the worker's clock jumped).
    pub(crate) running_since: Option<Instant>,
    /// W3C traceparent of the submitting gRPC handler's span, captured
    /// at DAG-merge time. Embedded into `WorkAssignment.traceparent` at
    /// dispatch so the worker's build span chains back to the gateway's
    /// trace regardless of which code path (immediate merge, deferred
    /// completion/heartbeat) triggers dispatch. Empty for recovered
    /// derivations (no user trace). First submitter wins on dedup.
    pub traceparent: String,
    /// `DagActor.probe_generation` at the time of the last dispatch-
    /// time `FindMissingPaths` probe for this node. The batch pre-pass
    /// skips nodes whose `probed_generation == probe_generation` so the
    /// `truncate(DISPATCH_PROBE_BATCH_CAP)` window advances across
    /// inline `dispatch_ready` calls instead of re-probing the head.
    /// `probe_generation` advances once per `handle_tick` (1/s).
    pub probed_generation: u64,
    /// One-shot suppression for the detached-substitute lane: set on
    /// `SubstituteComplete{ok=false}` so subsequent dispatch passes
    /// skip substitution and route to a worker. Without this, a
    /// `FindMissingPaths` that says "substitutable" while
    /// `QueryPathInfo` says "no" loops at ~1/s and the drv never
    /// reaches `find_executor` (FMP HEAD probe vs. QPI GET drift).
    /// In-mem only — recovery resets `Substituting` to dep-walk anyway.
    pub substitute_tried: bool,
    /// Set when `check_roots_topdown` pruned this node's dep subgraph
    /// from the submission. Carries the invariant "this node MUST
    /// complete via substitution; building is invalid" — its
    /// `inputDrvs` were never merged/built/substituted, so a
    /// dispatched build would ENOENT on them. On
    /// `SubstituteComplete{ok=false}`, `handle_substitute_complete`
    /// fails every interested build instead of demoting to Ready
    /// (which would dispatch a doomed build); the dispatch-time probes
    /// take the same fail-fast arm via `must_substitute` — mark set
    /// AND closure evidence `Broken`, i.e. childless OR carrying the
    /// `closure_hole` breadcrumb (an un-produced child reaped out from
    /// under it — see that field) — when the flagged node's wanted
    /// outputs turn out missing and unsubstitutable; every
    /// children-keyed verdict (the dispatch guards, the reap-time
    /// hook, the walk-failure gate, and all clear sites) judges the
    /// same `ClosureEvidence`, treating a closure-holed survivor as
    /// childless-equivalent.
    /// r[sched.merge.substitute-topdown+10]. Persisted (`migrations/063`,
    /// stamped in the pruned merge's own transaction, OR-on-conflict,
    /// cleared only on a `Vouched` verdict — ≥1 child, all produced,
    /// no closure hole, and at recovery additionally a still-live
    /// build co-owning the parent to vouch for each produced child —
    /// or when the fail-fast consumes it) and restored by
    /// `from_recovery_row` — unlike `substitute_tried`, losing it
    /// across failover re-arms the doomed from-source dispatch.
    pub topdown_pruned: bool,
    /// Closure-hole breadcrumb: an un-produced child of this node
    /// (status not Completed/Skipped at removal time) was removed out
    /// from under it — by a terminal build's cleanup reap
    /// (`remove_build_interest_and_reap`), a poison-clear removal, or a
    /// recovery-time edge drop — so its current DAG children no longer
    /// represent its pruned input closure. Every
    /// children-keyed `topdown_pruned` verdict treats this as
    /// childless-equivalent: the reap-hook fail-fast arm fires for a
    /// holed survivor, and the walk-failure children gate, the
    /// completion-time clear, and the merge-time clear pass all refuse
    /// to trust (clear over) the truncated child set — erring toward
    /// the bounded resubmit-directing fail-fast, never the doomed
    /// from-source dispatch of a node whose closure may not be in the
    /// store.
    ///
    /// Persisted (`migrations/064`, OR-on-conflict — the merge-time row
    /// bind is always `false`, so only the explicit clears below ever
    /// drop the column): the in-memory breadcrumb is set by the reap on
    /// leaders and standbys alike, and the leader's survivor hook then
    /// writes it to PG best-effort (a crash or lease loss between the
    /// reap and that write loses the persisted copy — the accepted
    /// best-effort window). Recovery additionally sets it (in memory,
    /// best-effort in PG) in `load_dag_from_rows` when the edge load
    /// drops an un-produced terminal child of a recovered parent — the
    /// recovery-side analogue of the reap, covering the reap-then-
    /// failover window above. The poison-clear paths — admin
    /// `ClearPoison` (`handle_clear_poison`) and the poison-TTL sweep
    /// (`tick_process_expired_poisons`) — set it the same way (in
    /// memory, best-effort in PG) for the surviving parents of the
    /// Poisoned (by definition un-produced) child they remove; both run
    /// only on the leader by construction (admin leader guard / standby
    /// tick no-op). `from_recovery_row` restores it so the
    /// recovery-time produced-children gate never clears a holed
    /// flagged parent on the strength of the surviving produced
    /// children (the un-produced child's own row and edge may have been
    /// GC'd by `gc_orphan_terminal_derivations`, so the persisted
    /// breadcrumb is the only durable record of the truncation);
    /// `from_poisoned_row` keeps it false (poisoned restores are
    /// TTL-tracking stubs, never children-judged).
    ///
    /// The breadcrumb's lifecycle is decoupled from the node's own
    /// status: it records the relation between the node's child set
    /// and its pruned input closure, and the node completing or being
    /// skipped does not repair that relation — so terminal transitions
    /// do NOT drop it. It stays set and dormant: the children-keyed
    /// consumers either never evaluate a terminal node (the dispatch
    /// guards and the walk/reap fail-fast arms only see non-terminal
    /// ones) or err conservative on it (the stamp gates and the clear
    /// passes judge the holed child set Broken — a completed node
    /// later re-kept by a prune is stamped, not exempted), and the
    /// breadcrumb still guards the node when an in-tenure
    /// stale-Completed reset or a failover re-opens its lifecycle.
    /// Carried across a resubmit-reset (the truncation outlives the
    /// retry: the reset rebuilds the state via `try_from_node` but
    /// keeps the truncated edges without re-declaring them, so the
    /// breadcrumb rides along with the other carried fields); a
    /// `rollback_merge` restores the prior state wholesale. Cleared
    /// explicitly (memory + PG) only when a later full merge
    /// re-declares the node's edges (its child set is representative
    /// again; the heal pushes the PG clear for every re-declared edge
    /// parent) and when a Vouched-keyed clear pass consumes the
    /// `topdown_pruned` mark it qualifies (the batched
    /// `clear_topdown_pruned_by_hashes` drops both bits; the singular
    /// `clear_topdown_pruned_by_hash` — the lazy walk-failure clear and
    /// the fail-fast consume — is mark-only, so the fail-fast retains
    /// the breadcrumb for the directed resubmit it solicits). Known
    /// residual: a poison row already past its TTL when a new leader
    /// recovers is reset to `'created'` (and not loaded) before the
    /// recovery stamp query runs, so a parent whose only truncation
    /// evidence was that row recovers without the breadcrumb — the
    /// expired-at-load shape, reachable only when no leader is up to
    /// run the in-tenure sweep before the TTL lapses (same accepted
    /// class as the GC residual; see
    /// `load_parents_with_unproduced_terminal_children`).
    pub closure_hole: bool,
    /// Output paths that have already triggered a forgiven-seed-became-
    /// wanted DOWNGRADE of a substitute completion for this node
    /// (`handle_substitute_complete`) **within the current substitution
    /// chain**. A path in this set is never included in a later walk's
    /// forgivable set for the rest of that chain, no matter how the
    /// live effective wanted set shrinks (a build goes terminal) or
    /// re-grows (a new build merges) between walks. This is what keeps
    /// the downgrade → re-spawn chain monotone and terminating: each
    /// downgrade permanently consumes at least one of the node's
    /// finitely many declared outputs, so a chain can take at most
    /// |declared outputs| downgrades.
    ///
    /// The set's lifetime is the chain's, not the node's: it survives
    /// only the chain's own re-spawns (the immediate downgrade
    /// re-spawn and the deferred next-pass delta re-walk) and is
    /// cleared whenever the chain ends — the ok=true completion, the
    /// genuine-failure (ok=false) revert, the topdown fail-fast, an
    /// accepted worker-build verdict after a from-source routing, and
    /// every non-substitution completion (the inline store-batch
    /// completion, the merge-time cached-hit lane, the CA early-cutoff
    /// Skip). As a backstop, the stale-Completed/Skipped reset in
    /// `verify_preexisting_completed` clears it again at the moment a
    /// NEW chain begins, so no completion path missing from that list
    /// can leak the set across chains. A stale entry surviving into
    /// a LATER chain would veto forgiving a path no live build wants
    /// any more and demote a fully-substitutable node to a from-source
    /// build. A resubmit-reset rebuilds the node state from scratch
    /// (fresh empty set) and `rollback_merge` restores the removed
    /// node wholesale, so the set also never outlives the node
    /// lifetime it was accumulated in. In-mem only — never persisted;
    /// recovery resets `Substituting` nodes through the dep-walk anyway
    /// (same recovery semantics as `substitute_tried`).
    pub never_forgive_paths: HashSet<String>,
}

impl DerivationState {
    /// Create a new derivation state from a proto DerivationNode.
    ///
    /// Validates `node.drv_path` parses as a well-formed `StorePath`. The
    /// gRPC layer also validates upfront (returns INVALID_ARGUMENT), so this
    /// is belt-and-suspenders for when the actor is driven by something
    /// other than gRPC (tests, future admin APIs).
    pub fn try_from_node(
        node: &crate::domain::DerivationNode,
    ) -> Result<Self, rio_nix::store_path::StorePathError> {
        let drv_path = rio_nix::store_path::StorePath::parse(&node.drv_path)?;
        // Best-effort: parse inputSrcs from the inlined ATerm so
        // `approx_input_closure` covers shallow DAGs (drv with no
        // child nodes but many already-built inputs). Swallow parse
        // errors — `drv_content` is empty for store-hit nodes the
        // gateway didn't inline, and recovered nodes; both fall
        // back to DAG-children-only prefetch.
        let input_srcs: Vec<String> = std::str::from_utf8(&node.drv_content)
            .ok()
            .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok())
            .map(|d| d.input_srcs().iter().cloned().collect())
            .unwrap_or_default();
        Ok(Self {
            drv_hash: node.drv_hash.as_str().into(),
            drv_path,
            pname: (!node.pname.is_empty()).then(|| node.pname.clone()),
            version: node.version.clone(),
            enable_parallel_building: node.enable_parallel_building,
            enable_parallel_checking: node.enable_parallel_checking,
            prefer_local_build: node.prefer_local_build,
            system: node.system.clone(),
            required_features: node.required_features.clone(),
            effective_features: EffectiveFeatures::derive(
                node.is_fixed_output,
                &node.required_features,
                (!node.pname.is_empty()).then_some(node.pname.as_str()),
            ),
            output_names: node.output_names.clone(),
            is_fixed_output: node.is_fixed_output,
            ca: CaState {
                // r[impl sched.ca.detect]
                is_ca: node.is_content_addressed,
                needs_resolve: node.needs_resolve,
                // Decoded once at the proto→domain boundary. Gateway
                // sends 32 bytes for CA nodes it could compute the
                // modular hash for; `domain::DerivationNode::from`
                // maps non-32-byte (including empty) → None.
                // Belt-and-suspenders vs the gateway's own IA gate
                // (populate_ca_modular_hashes skips non-CA).
                modular_hash: node.ca_modular_hash,
                pending_realisation_deps: Vec::new(),
                output_unchanged: false,
            },
            status: DerivationStatus::Created,
            interested_builds: HashSet::new(),
            assigned_executor: None,
            exec_id: None,
            // est_duration/priority: placeholders — merge.rs sets them
            // via critical_path::compute_initial right after
            // try_from_node (SLA cache not in scope here). 0.0 is a
            // visible "not yet set" marker.
            sched: SchedHint::default(),
            drv_content: node.drv_content.clone(),
            drv_content_authoritative: node.drv_content_authoritative,
            input_srcs,
            retry: RetryState::default(),
            output_paths: Vec::new(),
            expected_output_paths: node.expected_output_paths.clone(),
            wanted_output_names: node.wanted_output_names.clone(),
            // The submitting build's contribution is recorded by
            // `dag.merge` (the build_id isn't known here).
            wanted_by_build: HashMap::new(),
            db_id: None,
            ready_at: None,
            running_since: None,
            traceparent: String::new(),
            probed_generation: 0,
            substitute_tried: false,
            topdown_pruned: false,
            closure_hole: false,
            never_forgive_paths: HashSet::new(),
        })
    }

    /// Current status (read-only). Use `transition()` etc. to mutate.
    pub fn status(&self) -> DerivationStatus {
        self.status
    }

    /// Reconstruct from a PG recovery row. Used by recover_from_pg().
    ///
    /// Lossy fields (can't persist Instant): `ready_at`, `running_since`,
    /// `poisoned_at`, `backoff_until` all reset to conservative
    /// defaults. `ready_at=Some(now)` for Ready (metric skew
    /// acceptable). `poisoned_at=None` — poisoned rows aren't
    /// loaded (load_nonterminal_derivations filters them). If one
    /// DOES slip through (race with status update), the None means
    /// the poison-TTL check never fires — the derivation stays
    /// poisoned forever until a new build re-merges it.
    ///
    /// `drv_content` is restored from the persisted row when the
    /// gateway marked it authoritative (content-bound hook fallback,
    /// `M_062`) — those bytes exist nowhere else, so the worker could
    /// not fetch them. For every other node it is empty and the
    /// worker fetches the `.drv` from the store via GetPath (fallback
    /// path, still supported in executor). When such a node is
    /// content-addressed, its CA modular hash is recomputed from the
    /// restored bytes so post-failover completion still registers the
    /// realisation (`sched.recovery.inline-drv-ca-hash`).
    ///
    /// Errors: `drv_path` doesn't parse as StorePath. Shouldn't
    /// happen (it was validated at merge time before persist) but
    /// be defensive against PG corruption / manual edits. On error,
    /// returns `(drv_hash, err)` so the caller can log without
    /// having cloned drv_hash up front.
    pub(crate) fn from_recovery_row(
        row: crate::db::RecoveryDerivationRow,
        status: DerivationStatus,
    ) -> Result<Self, (String, rio_nix::store_path::StorePathError)> {
        let drv_path = rio_nix::store_path::StorePath::parse(&row.drv_path)
            .map_err(|e| (row.drv_hash.clone(), e))?;
        let now = Instant::now();
        // §13e+r35: derive BEFORE the struct literal moves `row.pname` /
        // `row.required_features`. Same chokepoint as `try_from_node`.
        let effective_features = EffectiveFeatures::derive(
            row.is_fixed_output,
            &row.required_features,
            row.pname.as_deref(),
        );
        // Parse the persisted authoritative content once (when present):
        // it feeds both the prefetch-hint inputSrcs and — for CA nodes —
        // the modular-hash recompute below.
        let parsed_content = row
            .drv_content
            .as_deref()
            .and_then(|c| std::str::from_utf8(c).ok())
            .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok());
        // r[impl sched.recovery.inline-drv-ca-hash]
        // A recovered content-addressed hook-fallback node must regain
        // its CA modular hash, or its post-failover completion would
        // silently skip realisation registration (the consumable-result
        // guarantee of gw.hook.fallback-built-outputs). Recompute it
        // from the restored bytes — the same
        // `hash_derivation_modulo(…, |_| None)` computation SubmitBuild
        // ingress validated at admission — rather than persisting a
        // second column that could drift from the content. Failure
        // (pre-validation rows, manual edits) degrades to `None` with a
        // warning: recovery never fails and the build still completes,
        // only realisation registration is lost (now visibly).
        let modular_hash = if row.is_ca && row.drv_content.is_some() {
            match parsed_content.as_ref() {
                Some(drv) => rio_nix::derivation::hash_derivation_modulo(
                    drv,
                    &row.drv_path,
                    &|_| None,
                    &mut std::collections::HashMap::new(),
                )
                .map_err(|e| {
                    tracing::warn!(
                        drv_path = %row.drv_path,
                        error = %e,
                        "recovery: failed to recompute the CA modular hash from persisted drv_content"
                    );
                })
                .ok(),
                None => {
                    tracing::warn!(
                        drv_path = %row.drv_path,
                        "recovery: persisted authoritative drv_content did not parse; CA modular hash stays unset"
                    );
                    None
                }
            }
        } else {
            None
        };
        let recovered_input_srcs: Vec<String> = parsed_content
            .as_ref()
            .map(|d| d.input_srcs().iter().cloned().collect())
            .unwrap_or_default();
        Ok(Self {
            drv_hash: row.drv_hash.into(),
            drv_path,
            pname: row.pname,
            // ADR-023 sizing inputs not persisted — best-effort, lossy on
            // recovery (build_samples row written from a recovered build
            // gets NULL for these; the SLA fit tolerates sparse columns).
            version: None,
            enable_parallel_building: None,
            enable_parallel_checking: None,
            prefer_local_build: None,
            system: row.system,
            effective_features,
            required_features: row.required_features,
            output_names: row.output_names,
            is_fixed_output: row.is_fixed_output,
            ca: CaState {
                is_ca: row.is_ca,
                // Recomputed above for authoritative hook-fallback rows;
                // None for everything else.
                modular_hash,
                // Remaining CA fields lossy on recovery — see CaState doc.
                ..Default::default()
            },
            status,
            interested_builds: HashSet::new(), // populated by build_derivations join
            assigned_executor: row.assigned_builder_id.map(Into::into),
            // From the active `assignments` row, scoped by the JOIN to
            // currently-assigned drvs (`load_nonterminal_derivations`) —
            // NULL for a drv whose dispatch was reset, so recovery
            // preserves `reset_to_ready()`'s clear. Recovery re-stamps
            // the LogBuffers ring buffer from this; see `load_dag_from_rows`.
            exec_id: row.exec_id,
            sched: SchedHint {
                // M_044: persisted reactive floor. PG bigint → i64;
                // negatives (impossible by DEFAULT 0 + only-ever-doubled
                // writes) saturate to 0; deadline_secs saturates to u32.
                resource_floor: ResourceFloor {
                    mem_bytes: row.floor_mem_bytes.max(0) as u64,
                    disk_bytes: row.floor_disk_bytes.max(0) as u64,
                    deadline_secs: row.floor_deadline_secs.clamp(0, u32::MAX as i64) as u32,
                },
                // Remaining sched fields lossy; recomputed at next
                // dispatch / full_sweep — see SchedHint doc.
                ..Default::default()
            },
            // Authoritative inline derivation (hook fallback) is the
            // only copy anywhere — restore it so post-failover
            // dispatch still works; everything else stays empty and
            // the worker fetches the .drv from the store. The
            // inputSrcs prefetch hints come from the same parse as the
            // modular-hash recompute above. A row only ever holds
            // content when the creating submission marked it
            // authoritative (sched.persist.creation-scoped), so the
            // flag is re-derived from presence.
            drv_content_authoritative: row.drv_content.is_some(),
            input_srcs: recovered_input_srcs,
            drv_content: row.drv_content.unwrap_or_default(),
            retry: RetryState {
                count: row.retry_count.max(0) as u32,
                resubmit_cycles: row.resubmit_cycles.max(0) as u32,
                // failure_count: initialize from failed_builders.len() —
                // same-worker repeats are lost (in-mem only), conservative.
                failure_count: row.failed_builders.len() as u32,
                failed_builders: row.failed_builders.into_iter().map(Into::into).collect(),
                // Remaining retry fields in-memory only — recovery
                // resets to conservative defaults (see RetryState doc).
                ..Default::default()
            },
            output_paths: Vec::new(), // completed rows not loaded
            expected_output_paths: row.expected_output_paths,
            // Persisted with union-on-conflict (`migrations/062`,
            // `batch_upsert_derivations`). Pre-migration rows carry the
            // column DEFAULT '{}' = all declared outputs wanted, so
            // they recover with the old conservative criterion.
            wanted_output_names: row.wanted_output_names,
            // Per-build contributions are not persisted; recovered
            // interest has unknown contributions (missing entries), so
            // `effective_wanted` falls back to the union above.
            wanted_by_build: HashMap::new(),
            db_id: Some(row.derivation_id),
            // Instant fields: conservative defaults.
            // ready_at: Some(now) if Ready → dispatch_wait_seconds
            // metric skews (looks like instant dispatch) but
            // doesn't break anything.
            ready_at: (status == DerivationStatus::Ready).then_some(now),
            // running_since: Some(now) if Running → backstop
            // timeout resets. A build that was 1h into a 2h
            // estimate gets another full 6h backstop. Conservative
            // (won't spuriously cancel) at the cost of a possibly
            // stale build running longer.
            running_since: (status == DerivationStatus::Running).then_some(now),
            traceparent: String::new(), // recovered: no user trace
            probed_generation: 0,
            substitute_tried: false,
            // Persisted (`migrations/063`) precisely so it survives
            // failover: a pruned root recovered childless MUST keep the
            // "complete via substitution or fail fast" invariant — its
            // inputDrvs were never merged, so a from-source dispatch on
            // the new leader would ENOENT just like on the old one.
            topdown_pruned: row.topdown_pruned,
            // Persisted (`migrations/064`) for the same reason: the
            // un-produced child whose reap set the breadcrumb may have
            // been GC'd from PG since, so the surviving produced
            // children would otherwise launder the recovery-time clear
            // and re-arm the doomed from-source dispatch.
            closure_hole: row.closure_hole,
            never_forgive_paths: HashSet::new(),
        })
    }

    /// Construct from a `PoisonedDerivationRow` during recovery.
    /// Minimal — poisoned rows aren't dispatched, just TTL-tracked +
    /// resubmit-bound checked (`is_retriable_on_resubmit`).
    /// `elapsed_secs` comes from PG's `EXTRACT(EPOCH FROM (now() -
    /// poisoned_at))` so we compute `poisoned_at = Instant::now() -
    /// Duration::from_secs_f64(elapsed)` — approximate but good enough
    /// for a 24h TTL.
    pub(crate) fn from_poisoned_row(
        row: crate::db::PoisonedDerivationRow,
    ) -> Result<Self, (String, rio_nix::store_path::StorePathError)> {
        let drv_path = rio_nix::store_path::StorePath::parse(&row.drv_path)
            .map_err(|e| (row.drv_hash.clone(), e))?;
        let now = Instant::now();
        // Convert PG-computed elapsed seconds back to an Instant.
        // Clamp: negative/NaN → 0 (conservative full TTL), +inf → 1yr
        // (poisoned_at = -infinity::timestamp would make from_secs_f64
        // panic). Follows the `.max(0.0).min(MAX)` pattern from
        // `state/executor.rs`.
        //
        // Note: recovery.rs filters out rows with elapsed_secs >
        // POISON_TTL before calling this, so the checked_sub(elapsed)
        // → None → unwrap_or(now) path only fires for still-valid
        // poison on a recently-booted node — in which case treating
        // poisoned_at as "now" is a conservative approximation (slight
        // TTL extension from recovery time, bounded by node uptime).
        const MAX_ELAPSED_SECS: f64 = 365.0 * 86400.0;
        #[allow(clippy::manual_clamp)]
        let clamped = row.elapsed_secs.max(0.0).min(MAX_ELAPSED_SECS);
        let elapsed = std::time::Duration::from_secs_f64(clamped);
        let poisoned_at = now.checked_sub(elapsed).unwrap_or(now);
        // §13e+r35: recovered Poisoned has no persisted features
        // (`required_features = []`). FOD ⟹ `[fetcher]` still derives
        // correctly; non-FOD ⟹ `[]`.
        let effective_features =
            EffectiveFeatures::derive(row.is_fixed_output, &[], row.pname.as_deref());
        Ok(Self {
            drv_hash: row.drv_hash.into(),
            drv_path,
            pname: row.pname,
            version: None,
            enable_parallel_building: None,
            enable_parallel_checking: None,
            prefer_local_build: None,
            system: row.system,
            required_features: Vec::new(),
            effective_features,
            output_names: Vec::new(),
            is_fixed_output: row.is_fixed_output,
            ca: CaState::default(),
            status: DerivationStatus::Poisoned,
            interested_builds: HashSet::new(),
            assigned_executor: None,
            exec_id: None,
            sched: SchedHint::default(),
            drv_content: Vec::new(),
            drv_content_authoritative: false,
            input_srcs: Vec::new(),
            retry: RetryState {
                resubmit_cycles: row.resubmit_cycles.max(0) as u32,
                failure_count: row.failed_builders.len() as u32,
                failed_builders: row.failed_builders.into_iter().map(Into::into).collect(),
                poisoned_at: Some(poisoned_at),
                ..Default::default()
            },
            output_paths: Vec::new(),
            expected_output_paths: Vec::new(),
            wanted_output_names: Vec::new(),
            wanted_by_build: HashMap::new(),
            db_id: Some(row.derivation_id),
            ready_at: None,
            running_since: None,
            traceparent: String::new(),
            probed_generation: 0,
            substitute_tried: false,
            topdown_pruned: false,
            closure_hole: false,
            never_forgive_paths: HashSet::new(),
        })
    }

    /// Store path of the .drv file (read-only; DAG owns the reverse index).
    ///
    /// Callers using `&str` auto-deref via `StorePath::Deref<Target=str>`.
    pub fn drv_path(&self) -> &rio_nix::store_path::StorePath {
        &self.drv_path
    }

    /// §13e + r35: the derived feature set. The biconditional
    /// `is_fixed_output ⟺ ∋ fetcher` holds at every read because the
    /// only producer is [`EffectiveFeatures::derive`] (constructor +
    /// the `set_required_features` write-gate). EVERY routing
    /// consumer reads this — `passes_intent_filter`, `h_all`
    /// partition, `override_hash` memo key, `retain_hosting_cells`,
    /// `bypass_cells` cold-start, `hard_filter`/`rejection_reason`,
    /// `statically_eligible`, the wire `SpawnIntent.required_features`.
    /// The TWO intentional bypasses read the in-memory normalized set
    /// via [`Self::required_features`]: `actor/snapshot.rs::
    /// handle_inspect_build_dag` and `actor/dispatch.rs`'s
    /// `failed_builders` warn (operator-facing echo, post I-204
    /// soft-strip — pre §13e FOD↔fetcher derivation).
    pub fn effective_features(&self) -> &EffectiveFeatures {
        &self.effective_features
    }

    /// In-memory `requiredSystemFeatures`, post I-204 soft-strip (the
    /// verbatim declared set lives only in the `derivations.
    /// required_features` PG column — `set_required_features` mutates
    /// this in place). Diagnostic echo only — `InspectBuildDag` and the
    /// `dispatch.rs` `failed_builders` warn show the operator the
    /// pre-§13e-derivation set so they can spot a misrouted FOD.
    /// ROUTING reads [`Self::effective_features`] — a consumer reading
    /// this raw set is a §13e/r35 chokepoint bypass.
    pub fn required_features(&self) -> &[String] {
        &self.required_features
    }

    /// Atomic write-gate for `required_features`. ALL post-construction
    /// mutation of `required_features` MUST go through this — it
    /// re-derives `effective_features` so the biconditional
    /// `is_fixed_output ⟺ effective_features ∋ fetcher` cannot drift
    /// (§13e + r35 — the 4/4-validator-converged blocker:
    /// `apply_soft_features` mutated `required_features` AFTER
    /// construction, leaving a constructor-only `effective_features`
    /// permanently desynced and silently regressing I-204).
    ///
    /// The constructors (`try_from_node` / `from_recovery_row` /
    /// `from_poisoned_row`) call [`EffectiveFeatures::derive`]
    /// directly; there is NO direct write path to either field outside
    /// `state/derivation.rs`.
    pub(crate) fn set_required_features(&mut self, raw: Vec<String>) {
        self.required_features = raw;
        self.effective_features = EffectiveFeatures::derive(
            self.is_fixed_output,
            &self.required_features,
            self.pname.as_deref(),
        );
    }

    /// True iff this node can be checked against `FindMissingPaths`:
    /// every expected output path is known. Floating-CA
    /// (`expected_output_paths == [""]`) and nodes submitted without
    /// paths fail this — they cannot substitute by path and the
    /// dispatch-time probe never stamps their `probed_generation`.
    /// Shared by `batch_probe_cached_ready`, `ready_check_or_spawn`,
    /// and `r[sched.admin.spawn-intents.probed-gate]` — all three MUST
    /// agree or the gate dead-locks unprobeable nodes.
    pub fn output_paths_probeable(&self) -> bool {
        !self.expected_output_paths.is_empty()
            && self.expected_output_paths.iter().all(|p| !p.is_empty())
    }

    /// Union a newly-merged consumer's wanted set into this node's.
    /// Delegates to [`union_wanted_saturating`] — see it for the
    /// saturation algebra (empty = "all declared outputs wanted",
    /// `all ∪ X = all`, otherwise sorted deduplicated set union).
    pub fn union_wanted(&mut self, incoming: &[String]) {
        union_wanted_saturating(&mut self.wanted_output_names, incoming);
    }

    /// The derivation's `name` attribute, as encoded in the `.drv`
    /// store path (`{hash}-{name}.drv` → `{name}`). This is what
    /// Nix's `outputPathName` keys output-path name segments on —
    /// NOT `pname` (which omits the version suffix; `pname="hello"`
    /// vs `name="hello-2.10"`). Infallible: `drv_path` is a parsed
    /// `StorePath` so `name()` always exists; the `.drv` suffix is
    /// stripped if present (it always is for a real `.drv`, but
    /// `unwrap_or_else` keeps the method panic-free).
    pub fn drv_name(&self) -> &str {
        let n = self.drv_path.name();
        n.strip_suffix(".drv").unwrap_or(n)
    }

    /// Tenant IDs of all interested builds. Base iterator for
    /// [`Self::attributed_tenant`] (min) and the path-tenant upsert
    /// (collect-all). `filter_map` drops `None` (single-tenant mode;
    /// empty SSH-key comment → gateway sends "" → scheduler stores
    /// `None`).
    pub fn attributed_tenants<'a>(
        &'a self,
        builds: &'a std::collections::HashMap<Uuid, super::BuildInfo>,
    ) -> impl Iterator<Item = Uuid> + 'a {
        self.interested_builds
            .iter()
            .filter_map(|id| builds.get(id)?.tenant_id)
    }

    /// Minimum-UUID tenant among interested builds — the SLA model-key
    /// attribution shared by `solve_intent_for` / `model_key_for` /
    /// `record_build_sample` so the estimator's cache key matches the
    /// rows that fed it.
    ///
    /// `.min()` not `.next()`: `interested_builds` is a `HashSet`
    /// (RandomState iteration order). When a second tenant merges the
    /// same drv mid-build, `.next()` would let solve key on tenant_A
    /// and the completion sample land under tenant_B (or flip the
    /// SpawnIntent shape between controller polls). `.min()` is stable
    /// for a given set; cross-tenant dedup is rare enough that
    /// "smallest UUID wins" is fine — per-tenant key is a grouping
    /// dimension, not an accounting ledger.
    pub fn attributed_tenant(
        &self,
        builds: &std::collections::HashMap<Uuid, super::BuildInfo>,
    ) -> Option<Uuid> {
        self.attributed_tenants(builds).min()
    }

    /// Attempt to transition to a new status. Returns the old status on success.
    pub fn transition(
        &mut self,
        to: DerivationStatus,
    ) -> Result<DerivationStatus, TransitionError> {
        let from = self.status;
        from.validate_transition(to)?;

        // Idempotent no-ops: don't change anything
        if from == to {
            return Ok(from);
        }

        self.status = to;

        // Track ready_at for assignment latency metric
        if to == DerivationStatus::Ready {
            self.ready_at = Some(Instant::now());
        }
        // Track running_since for backstop timeout. Running → any
        // transition clears it (next Assigned → Running sets fresh).
        if to == DerivationStatus::Running {
            self.running_since = Some(Instant::now());
        } else if from == DerivationStatus::Running {
            self.running_since = None;
        }
        // r[impl sched.merge.exec-correlation+7]
        // A node leaving a terminal state (I-094 reprobe → Substituting/
        // Queued, I-047 stale-output reset → Ready/Queued) is starting a
        // fresh lifecycle. The terminal's epilogue already finalized the
        // prior execution's log and bd.exec_id correlation; carrying its
        // exec_id forward makes `exec_id_for_terminal` attribute that
        // finalized execution to whatever build next terminates the
        // reset node — a build that never observed it. Same contract as
        // `reset_to_ready`'s clear, applied at the chokepoint every
        // terminal-exit carve-out goes through (including the currently
        // uncalled Poisoned → Created one). The LogBuffers entry is
        // deliberately NOT touched: for a finalized execution it is
        // already gone (flush_final's drain), and for the
        // substitute-success known gap (un-finalized buffer on a reset
        // node) it must survive as the fallback carrier.
        if from.is_terminal() && !to.is_terminal() {
            self.exec_id = None;
        }

        Ok(from)
    }

    /// Worker-lost recovery. Transitions Assigned -> Ready, or Running -> Failed -> Ready.
    /// Clears `assigned_executor`. Returns error if not in Assigned or Running state.
    ///
    /// Running -> Ready is not a valid direct transition, so Running goes through
    /// Failed first (this is a failed build attempt that the caller should count
    /// as a retry).
    pub fn reset_to_ready(&mut self) -> Result<(), TransitionError> {
        match self.status {
            DerivationStatus::Assigned => {
                self.transition(DerivationStatus::Ready)?;
            }
            DerivationStatus::Running => {
                self.transition(DerivationStatus::Failed)?;
                self.transition(DerivationStatus::Ready)?;
            }
            _ => {
                return Err(TransitionError::Invalid {
                    from: self.status,
                    to: DerivationStatus::Ready,
                    reason: "reset_to_ready only valid from Assigned or Running",
                });
            }
        }
        self.assigned_executor = None;
        self.exec_id = None;
        Ok(())
    }

    /// If Assigned, transition to Running (intermediate step — the
    /// state machine requires Running before Completed/Poisoned/
    /// Failed; Assigned→X directly is invalid for those). No-op if
    /// already Running or past it. Returns true if the transition
    /// succeeded or wasn't needed; false if Assigned→Running was
    /// rejected (unexpected — that transition is always valid).
    pub fn ensure_running(&mut self) -> bool {
        if self.status() == DerivationStatus::Assigned {
            self.transition(DerivationStatus::Running).is_ok()
        } else {
            true
        }
    }

    // r[impl sched.merge.poisoned-resubmit-bounded+2]
    /// Whether a resubmit of THIS node should reset it for re-dispatch.
    ///
    /// Wraps [`DerivationStatus::is_retriable_on_resubmit`] (the
    /// unconditionally-retriable states) and adds the bounded `Poisoned`
    /// case: a `Poisoned` node resets iff `resubmit_cycles <
    /// POISON_RESUBMIT_RETRY_LIMIT`. An explicit client re-submission is
    /// retry intent — the operator presumably fixed the underlying cause
    /// (I-169: I-167's `?id=` patch poisoned, then 27k dependents
    /// re-derived `DependencyFailed` from the still-poisoned parent on
    /// every resubmit). `resubmit_cycles` is incremented on each reset
    /// (`dag::merge`) and persisted (`M_051`) so the bound accumulates
    /// across re-submissions and survives leader failover. At/above the
    /// limit, 24h TTL or `ClearPoison` are the only overrides.
    pub fn is_retriable_on_resubmit(&self) -> bool {
        self.status.is_retriable_on_resubmit()
            || (self.status == DerivationStatus::Poisoned
                && self.retry.resubmit_cycles < POISON_RESUBMIT_RETRY_LIMIT)
    }

    /// Test-only: directly set status bypassing state machine validation.
    /// For setting up test preconditions where the full transition chain
    /// would be verbose noise.
    #[cfg(test)]
    pub(crate) fn set_status_for_test(&mut self, status: DerivationStatus) {
        self.status = status;
    }
}

/// The wanted-output predicate family, canonically defined in
/// [`rio_common::wanted_outputs`] and re-exported here so every
/// existing `crate::state::{wanted_subset, verifiable_wanted_paths}`
/// import keeps resolving. The gateway's will-dispatch prediction and
/// DAG dedup call the same rio-common functions — the scheduler and
/// gateway sides of the demand-driven cache-hit criterion cannot drift.
pub use rio_common::wanted_outputs::{
    union_wanted_saturating, verifiable_wanted_paths, wanted_subset,
};

/// Effective wanted set for classification: the saturating union of the
/// wanted contributions ([`DerivationState::wanted_by_build`]) of LIVE
/// interested builds. A build is live iff its [`BuildInfo`](super::BuildInfo)
/// is present in `builds` and `!state().is_terminal()` — a missing entry
/// counts as terminal (terminal cleanup removes the `BuildInfo` and the
/// DAG interest in the same handler).
///
/// Returns `None` when the caller MUST fall back to the stored
/// node-level union ([`DerivationState::wanted_output_names`]):
/// - the node has zero live interested builds (all terminal, missing
///   from `builds`, or an empty interest set — recovered orphans), or
/// - any live interested build's contribution is unknown (no
///   `wanted_by_build` entry — post-failover, where recovery rebuilds
///   interest from `build_derivations` but contributions are not
///   persisted). A partial union would silently under-count that
///   build's wants, so the conservative union wins instead.
///
/// `Some(vec![])` means "ALL declared outputs wanted": at least one live
/// build contributed the empty all-wanted sentinel, which saturates the
/// union (same algebra as [`union_wanted_saturating`]). Note the
/// asymmetry with `None`: the empty Vec is an explicit per-build
/// contribution, never a fallback value.
///
/// Free function (not a `DerivationState` method) because liveness needs
/// the actor's `builds` map, which the node cannot see; it lives here
/// (not rio-common) because it needs [`BuildInfo`](super::BuildInfo).
pub fn effective_wanted(
    state: &DerivationState,
    builds: &HashMap<Uuid, super::BuildInfo>,
) -> Option<Vec<String>> {
    use super::BuildStateExt as _;

    // None = no live contribution folded yet. Folding starts from the
    // first live contribution (cloned) rather than an empty accumulator:
    // an empty Vec is the "all wanted" sentinel and would saturate the
    // union from the start.
    let mut effective: Option<Vec<String>> = None;
    for build_id in &state.interested_builds {
        let live = builds
            .get(build_id)
            .is_some_and(|b| !b.state().is_terminal());
        if !live {
            continue;
        }
        // A live interested build with an unknown contribution: a partial
        // union would under-count its wants — bail to the stored union.
        let contribution = state.wanted_by_build.get(build_id)?;
        match &mut effective {
            None => effective = Some(contribution.clone()),
            Some(acc) => union_wanted_saturating(acc, contribution),
        }
    }
    effective
}

/// Poison detection config. Replaces the former `POISON_THRESHOLD` const.
///
/// `require_distinct_workers` toggles between HashSet semantics
/// (`failed_builders.len()` — default, current behavior) and a flat
/// counter (`failure_count` — any N failures poison, regardless of
/// worker; for single-worker dev deployments where 3 distinct workers
/// will never exist).
///
/// `#[serde(default)]` on the struct → absent keys fall through to
/// `Default::default()`, so `[poison] threshold = 5` leaves
/// `require_distinct_workers = true` (unchanged). Matches the
/// `Config` precedent. Serialize + PartialEq are
/// for the TOML-roundtrip tests in main.rs (`assert_eq!(cfg.poison,
/// PoisonConfig::default())`).
#[derive(
    Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize, schemars::JsonSchema,
)]
#[serde(default)]
pub struct PoisonConfig {
    /// Failures before poison. Default 3 (the former POISON_THRESHOLD).
    pub threshold: u32,
    /// Whether failures must be on distinct workers. Default `true`
    /// (HashSet semantics — matches prior behavior). `false` = any N
    /// failures poison regardless of worker.
    pub require_distinct_workers: bool,
}

impl Default for PoisonConfig {
    fn default() -> Self {
        Self {
            threshold: 3,
            require_distinct_workers: true,
        }
    }
}

impl PoisonConfig {
    /// Check whether a derivation has reached the poison threshold.
    /// Centralizes the distinct-vs-flat-count branch so the 3 callers
    /// (completion/worker/recovery) stay in lockstep.
    pub fn is_poisoned(&self, state: &DerivationState) -> bool {
        let count = if self.require_distinct_workers {
            state.retry.failed_builders.len() as u32
        } else {
            state.retry.failure_count
        };
        count >= self.threshold
    }
}

/// Max `resubmit_cycles` at which a `Poisoned` node still resets on
/// explicit resubmit. At/above this, the node stays `Poisoned` and the
/// build fail-fasts (24h TTL or `ClearPoison` to override).
/// `resubmit_cycles` is incremented on each reset and persisted
/// (`M_051`) so this accumulates across re-submissions and scheduler
/// restarts: two poison cycles before the node sticks. See
/// [`DerivationState::is_retriable_on_resubmit`].
pub const POISON_RESUBMIT_RETRY_LIMIT: u32 = 2;

/// Poison TTL: duration after which a poisoned derivation is reset to created.
/// 24h in production. Short in tests so poison-expiry can be observed without
/// clock manipulation (`std::time::Instant` can't be faked).
#[cfg(not(test))]
pub const POISON_TTL: std::time::Duration = std::time::Duration::from_secs(24 * 60 * 60);
#[cfg(test)]
pub const POISON_TTL: std::time::Duration = std::time::Duration::from_millis(100);

#[cfg(test)]
mod tests {
    use super::*;

    fn dummy_node() -> crate::domain::DerivationNode {
        rio_test_support::fixtures::make_derivation_node("h", "x86_64-linux").into()
    }

    /// `try_from_node` parses `inputSrcs` from inlined ATerm
    /// `drv_content` so `approx_input_closure` covers shallow DAGs.
    /// Best-effort: empty/malformed content → empty `input_srcs`,
    /// node creation still succeeds.
    #[test]
    fn try_from_node_parses_input_srcs() {
        let mut node = dummy_node();
        // Minimal valid ATerm with two inputSrcs (3rd Derive field).
        node.drv_content = br#"Derive([("out","/nix/store/abc-out","","")],[],["/nix/store/abc-gcc","/nix/store/abc-glibc"],"x86_64-linux","/bin/sh",[],[("out","/nix/store/abc-out")])"#.to_vec();
        let state = DerivationState::try_from_node(&node).unwrap();
        assert_eq!(
            state.input_srcs,
            vec!["/nix/store/abc-gcc", "/nix/store/abc-glibc"],
        );

        // Empty drv_content (gateway didn't inline / store-hit) → empty,
        // not an error.
        let empty = DerivationState::try_from_node(&dummy_node()).unwrap();
        assert!(empty.input_srcs.is_empty());

        // Malformed ATerm → empty, not an error.
        let mut bad = dummy_node();
        bad.drv_content = b"not a derivation".to_vec();
        let bad_state = DerivationState::try_from_node(&bad).unwrap();
        assert!(bad_state.input_srcs.is_empty());
    }

    /// `wanted_subset` filters the (output_names ↔ expected_output_paths)
    /// parallel arrays down to the wanted subset. Empty wanted = all declared
    /// (pre-migration rows, the BasicDerivation fallback, and `^*` roots must
    /// keep today's conservative all-outputs behaviour).
    #[test]
    fn wanted_subset_filters_and_empty_means_all() {
        let mut s = DerivationState::try_from_node(&dummy_node()).unwrap();
        s.output_names = vec!["out".into(), "dev".into(), "debug".into()];
        s.expected_output_paths = vec![
            "/nix/store/aaaa-glibc".into(),
            "/nix/store/bbbb-glibc-dev".into(),
            "/nix/store/cccc-glibc-debug".into(),
        ];
        fn wanted_paths(s: &DerivationState) -> Vec<&String> {
            wanted_subset(
                &s.output_names,
                &s.expected_output_paths,
                &s.wanted_output_names,
            )
            .collect()
        }

        s.wanted_output_names = vec![];
        assert_eq!(
            wanted_paths(&s),
            vec![
                "/nix/store/aaaa-glibc",
                "/nix/store/bbbb-glibc-dev",
                "/nix/store/cccc-glibc-debug"
            ],
            "empty wanted set must mean ALL declared outputs"
        );

        s.wanted_output_names = vec!["out".into(), "dev".into()];
        assert_eq!(
            wanted_paths(&s),
            vec!["/nix/store/aaaa-glibc", "/nix/store/bbbb-glibc-dev"],
            "wanted subset must exclude the unwanted -debug path"
        );

        // A wanted name with no matching declared output (defensive) is ignored.
        s.wanted_output_names = vec!["out".into(), "nonexistent".into()];
        assert_eq!(wanted_paths(&s), vec!["/nix/store/aaaa-glibc"]);
    }

    // r[verify sched.merge.wanted-outputs+2]
    /// `union_wanted` semantics: empty is the "all outputs wanted"
    /// sentinel, so the union of "all" with anything saturates to "all"
    /// (stays/becomes empty). Non-empty ∪ non-empty is a sorted,
    /// deduplicated set union — the wanted set only ever grows.
    #[test]
    fn union_wanted_grows_and_empty_saturates() {
        let mut s = DerivationState::try_from_node(&dummy_node()).unwrap();

        // {out} ∪ {dev} = {dev, out} (sorted union).
        s.wanted_output_names = vec!["out".into()];
        s.union_wanted(&["dev".into()]);
        assert_eq!(s.wanted_output_names, vec!["dev", "out"]);

        // Re-union of an already-present name is a no-op (no duplicates).
        s.union_wanted(&["out".into()]);
        assert_eq!(s.wanted_output_names, vec!["dev", "out"]);

        // {} ∪ {out} = {} — "all" absorbs any subset.
        s.wanted_output_names = vec![];
        s.union_wanted(&["out".into()]);
        assert_eq!(s.wanted_output_names, Vec::<String>::new());

        // {out} ∪ {} = {} — a consumer wanting all saturates the union.
        s.wanted_output_names = vec!["out".into()];
        s.union_wanted(&[]);
        assert_eq!(s.wanted_output_names, Vec::<String>::new());
    }

    /// `effective_wanted` — the wanted set scoped to LIVE interested
    /// builds, with the documented fallbacks:
    /// 1. a single live build's narrow contribution is returned as-is
    ///    (and two live builds' contributions union);
    /// 2. a live build contributing the empty all-wanted sentinel
    ///    saturates the result to `Some(vec![])`;
    /// 3. a terminal build's contribution stops counting;
    /// 4. a live build with an UNKNOWN contribution (no `wanted_by_build`
    ///    entry — post-failover recovery) → `None` (caller falls back to
    ///    the stored node-level union);
    /// 5. zero live interested builds (terminal, missing `BuildInfo`, or
    ///    empty interest set) → `None`.
    #[test]
    fn effective_wanted_live_union_and_fallbacks() -> anyhow::Result<()> {
        use std::collections::HashMap;

        use super::super::{BuildInfo, BuildOptions, BuildState, PriorityClass};

        let mk_build = |bid| {
            BuildInfo::new_pending(
                bid,
                None,
                PriorityClass::Scheduled,
                false,
                BuildOptions::default(),
                HashSet::new(),
            )
        };
        let live_out = Uuid::new_v4();
        let live_dev = Uuid::new_v4();
        let live_all = Uuid::new_v4();
        let live_unknown = Uuid::new_v4();
        let done = Uuid::new_v4();

        let mut builds: HashMap<Uuid, BuildInfo> = HashMap::new();
        for bid in [live_out, live_dev, live_all, live_unknown] {
            builds.insert(bid, mk_build(bid));
        }
        let mut terminal = mk_build(done);
        terminal.transition(BuildState::Active)?;
        terminal.transition(BuildState::Succeeded)?;
        builds.insert(done, terminal);

        let mut s = DerivationState::try_from_node(&dummy_node())?;

        // 1. Single live build → its own contribution, verbatim.
        s.interested_builds = [live_out].into();
        s.wanted_by_build = [(live_out, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            Some(vec!["out".to_string()]),
            "single live build → its own contribution"
        );

        // 1b. Two live builds → saturating union of their contributions.
        s.interested_builds = [live_out, live_dev].into();
        s.wanted_by_build = [
            (live_out, vec!["out".to_string()]),
            (live_dev, vec!["dev".to_string()]),
        ]
        .into();
        assert_eq!(
            effective_wanted(&s, &builds),
            Some(vec!["dev".to_string(), "out".to_string()]),
            "two live builds → union of their contributions"
        );

        // 2. A live build contributing the empty sentinel saturates.
        s.interested_builds = [live_out, live_all].into();
        s.wanted_by_build = [(live_out, vec!["out".to_string()]), (live_all, vec![])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            Some(vec![]),
            "a live build wanting all saturates the live union to all"
        );

        // 3. A terminal build's (wider) contribution stops counting.
        s.interested_builds = [live_out, done].into();
        s.wanted_by_build = [(live_out, vec!["out".to_string()]), (done, vec![])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            Some(vec!["out".to_string()]),
            "a terminal build's all-wanted contribution must stop counting"
        );

        // 4. A live build whose contribution is unknown → None (fallback),
        //    even though another live build's contribution is known.
        s.interested_builds = [live_out, live_unknown].into();
        s.wanted_by_build = [(live_out, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            None,
            "an unknown contribution of a live build forces the stored-union fallback"
        );

        // 5. Zero live interested builds → None.
        // 5a. Only a terminal build is interested.
        s.interested_builds = [done].into();
        s.wanted_by_build = [(done, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            None,
            "no live interested builds → fallback to the stored union"
        );
        // 5b. The interested build has no BuildInfo at all (terminal
        //     cleanup already removed it) — treated as terminal.
        let gone = Uuid::new_v4();
        s.interested_builds = [gone].into();
        s.wanted_by_build = [(gone, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            None,
            "an interested build with no BuildInfo entry counts as terminal"
        );
        // 5c. Empty interest set (recovered orphan) → None.
        s.interested_builds = HashSet::new();
        s.wanted_by_build = HashMap::new();
        assert_eq!(effective_wanted(&s, &builds), None);

        Ok(())
    }

    /// `drv_name()` strips the `.drv` suffix from the store-path
    /// name segment. For a stdenv package, this yields the full
    /// `${pname}-${version}` (what `output_path_name` keys on), NOT
    /// the bare `pname` — the distinction the CA-cutoff cascade gets
    /// wrong if it keys on `pname` (bug_006).
    #[test]
    fn drv_name_strips_suffix_and_keeps_version() -> anyhow::Result<()> {
        let node: crate::domain::DerivationNode =
            rio_test_support::fixtures::make_derivation_node("hello-2.10", "x86_64-linux").into();
        let state = DerivationState::try_from_node(&node)?;
        assert_eq!(
            state.drv_name(),
            "hello-2.10",
            "drv_name = {{hash}}-{{name}}.drv → {{name}}; for stdenv this is \
             ${{pname}}-${{version}}, not bare pname"
        );
        Ok(())
    }

    // r[verify sched.ca.detect]
    /// Proto `is_content_addressed` → `DerivationState.is_ca` at
    /// DAG-merge time (try_from_node is what `dag.merge()` calls
    /// per-node). Both flag values.
    #[test]
    fn ca_drv_sets_is_ca_flag() -> anyhow::Result<()> {
        // Input-addressed (default): is_ca = false.
        let ia_node = dummy_node();
        assert!(!ia_node.is_content_addressed, "fixture precondition");
        let ia_state = DerivationState::try_from_node(&ia_node)?;
        assert!(!ia_state.ca.is_ca, "input-addressed drv → is_ca=false");

        // Content-addressed: is_ca = true. Proto field set by the
        // gateway from `is_fixed_output() || has_ca_floating_outputs()`;
        // scheduler doesn't recompute, just propagates.
        let mut ca_node = dummy_node();
        ca_node.is_content_addressed = true;
        let ca_state = DerivationState::try_from_node(&ca_node)?;
        assert!(
            ca_state.ca.is_ca,
            "CA drv → is_ca=true propagated from proto"
        );

        // needs_resolve propagates independently of is_ca: the
        // ia.deferred case (IA drv with floating-CA input) has
        // is_ca=false but needs_resolve=true.
        let mut deferred = dummy_node();
        deferred.needs_resolve = true;
        let deferred_state = DerivationState::try_from_node(&deferred)?;
        assert!(
            deferred_state.ca.needs_resolve,
            "needs_resolve=true propagated from proto"
        );
        assert!(
            !deferred_state.ca.is_ca,
            "ia.deferred: needs_resolve independent of is_ca"
        );

        Ok(())
    }

    #[test]
    fn test_derivation_valid_transitions() {
        use DerivationStatus::*;

        let valid_transitions = [
            (Created, Completed),        // cache hit
            (Ready, Completed),          // dispatch-time FOD store-hit (I-067)
            (Created, Queued),           // build accepted
            (Queued, Ready),             // all deps complete
            (Ready, Assigned),           // worker selected
            (Assigned, Running),         // worker ack
            (Assigned, Ready),           // worker lost
            (Running, Completed),        // build succeeded
            (Running, Failed),           // retriable failure
            (Running, Poisoned),         // failed on 3+ workers
            (Ready, Poisoned),           // failed_builders exhausts fleet (I-065)
            (Failed, Ready),             // retry scheduled
            (Completed, Ready),          // output GC'd; re-dispatch (I-047)
            (Completed, Queued),         // output GC'd + dep also reset (I-047)
            (Skipped, Ready),            // output GC'd; re-dispatch (I-047)
            (Skipped, Queued),           // output GC'd + dep also reset (I-047)
            (Poisoned, Queued),          // I-094 deferred re-probe (output present, dep in-flight)
            (Failed, Queued),            // I-094 deferred re-probe
            (DependencyFailed, Queued),  // I-094 deferred re-probe
            (Poisoned, Created),         // 24h TTL expiry
            (Queued, DependencyFailed),  // dep poisoned cascade
            (Ready, DependencyFailed),   // dep poisoned cascade
            (Created, DependencyFailed), // dep poisoned before queue
            // Cancel: only from in-flight states. Queued/Ready
            // derivations are handled by orphan-removal instead
            // (handle_cancel_build's existing path).
            (Assigned, Cancelled),            // CancelSignal before worker ACK
            (Running, Cancelled),             // CancelSignal mid-build (cgroup.kill)
            (Queued, Skipped),                // CA early-cutoff cascade
            (Ready, Skipped),                 // CA cutoff after find_newly_ready promoted
            (Poisoned, Substituting),         // I-094 reprobe substitutable lane
            (DependencyFailed, Substituting), // symmetry with (DependencyFailed, Completed)
        ];

        for (from, to) in valid_transitions {
            assert!(
                from.validate_transition(to).is_ok(),
                "expected {from} -> {to} to be valid"
            );
        }
    }

    #[test]
    fn test_derivation_idempotent_transitions() {
        use DerivationStatus::*;

        // completed -> completed is no-op
        assert!(Completed.validate_transition(Completed).is_ok());
        // poisoned -> poisoned is no-op
        assert!(Poisoned.validate_transition(Poisoned).is_ok());
        // dependency_failed -> dependency_failed is no-op
        assert!(
            DependencyFailed
                .validate_transition(DependencyFailed)
                .is_ok()
        );
        // cancelled -> cancelled is no-op (duplicate CancelSignal or
        // late completion report after cgroup.kill)
        assert!(Cancelled.validate_transition(Cancelled).is_ok());
        // skipped -> skipped is no-op (cascade re-visits via diamond DAG)
        assert!(Skipped.validate_transition(Skipped).is_ok());
    }

    #[test]
    fn test_cancelled_is_terminal_no_resurrect() {
        use DerivationStatus::*;
        // Cancelled is terminal: no TTL reset like Poisoned. A
        // cancelled build stays cancelled; retry = re-submit.
        assert!(Cancelled.is_terminal());
        assert!(Cancelled.validate_transition(Created).is_err());
        assert!(Cancelled.validate_transition(Ready).is_err());
        // Cancel from NON-in-flight states: invalid. Queued/Ready
        // orphans are just removed from ready_queue, not transitioned.
        assert!(Queued.validate_transition(Cancelled).is_err());
        assert!(Ready.validate_transition(Cancelled).is_err());
        assert!(Created.validate_transition(Cancelled).is_err());
    }

    // r[verify sched.preempt.never-running]
    /// Skipped is terminal and only reachable from pre-dispatch
    /// states. Running builds are NEVER killed for CA cutoff —
    /// wasted CPU but correct output. Assigned is also excluded:
    /// once a worker is picked, let it run.
    #[test]
    fn test_skipped_is_terminal_never_from_running() {
        use DerivationStatus::*;
        assert!(Skipped.is_terminal());
        // Terminal: no resurrect to Created/Completed. Ready/Queued ARE
        // valid (I-047 GC reset — Skipped carries output_paths and
        // unlocks dependents same as Completed).
        assert!(Skipped.validate_transition(Created).is_err());
        assert!(Skipped.validate_transition(Ready).is_ok());
        assert!(Skipped.validate_transition(Queued).is_ok());
        assert!(Skipped.validate_transition(Completed).is_err());
        // r[sched.preempt.never-running]: in-flight states can NOT
        // transition to Skipped. CA cutoff only touches Queued/Ready.
        assert!(Running.validate_transition(Skipped).is_err());
        assert!(Assigned.validate_transition(Skipped).is_err());
        // Pre-Queued: Created can't skip (hasn't even entered the
        // DAG flow yet — cache-check happens at merge).
        assert!(Created.validate_transition(Skipped).is_err());
    }

    #[test]
    fn test_derivation_invalid_transitions() {
        use DerivationStatus::*;

        // Terminal -> non-terminal (except the documented carve-outs
        // validated below)
        assert!(Completed.validate_transition(Created).is_err());
        assert!(Completed.validate_transition(Running).is_err());
        // completed -> ready/queued IS valid (output GC'd; I-047)
        assert!(Completed.validate_transition(Ready).is_ok());
        assert!(Completed.validate_transition(Queued).is_ok());

        // Skip states
        assert!(Created.validate_transition(Running).is_err());
        assert!(Created.validate_transition(Ready).is_err());
        assert!(Created.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Running).is_err());
        // ready -> completed IS valid (FOD output already in store; I-067)
        assert!(Ready.validate_transition(Completed).is_ok());

        // Running -> Ready is NOT valid (must go through Failed)
        assert!(Running.validate_transition(Ready).is_err());
        assert!(Running.validate_transition(Queued).is_err());
        assert!(Running.validate_transition(Assigned).is_err());

        // Failed can go to Ready (retry), Queued (I-094 deferred), or
        // Completed/Substituting (I-094/I-099 re-probe cache-hit;
        // symmetry-only — Failed is reset by dag.merge today, but the
        // state machine and the merge.rs reprobe callers must agree).
        assert!(Failed.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Completed).is_ok());
        assert!(Failed.validate_transition(Substituting).is_ok());
        assert!(Failed.validate_transition(Queued).is_ok());

        // Poisoned can go to Created (TTL), Queued (I-094 deferred),
        // Completed/Substituting (I-094 reprobe), or stay Poisoned.
        // NOT Ready (must gate on dep), Running, Failed.
        assert!(Poisoned.validate_transition(Ready).is_err());
        assert!(Poisoned.validate_transition(Running).is_err());
        assert!(Poisoned.validate_transition(Failed).is_err());
        assert!(Poisoned.validate_transition(Queued).is_ok());

        // DependencyFailed is terminal: can't go anywhere except
        // self/Completed/Substituting/Queued (re-probe lanes).
        assert!(DependencyFailed.validate_transition(Ready).is_err());
        assert!(DependencyFailed.validate_transition(Queued).is_ok());
        assert!(DependencyFailed.validate_transition(Created).is_err());
        // Assigned/Running cannot cascade to DependencyFailed (already started)
        assert!(Assigned.validate_transition(DependencyFailed).is_err());
        assert!(Running.validate_transition(DependencyFailed).is_err());

        // Non-terminal self-transitions are invalid
        assert!(Created.validate_transition(Created).is_err());
        assert!(Queued.validate_transition(Queued).is_err());
        assert!(Ready.validate_transition(Ready).is_err());
        assert!(Assigned.validate_transition(Assigned).is_err());
        assert!(Running.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Failed).is_err());
    }

    // r[verify sched.state.transitions]
    /// Failed→Completed/Substituting symmetry: `existing_reprobe`
    /// includes `Failed`, and `apply_cached_hits` /
    /// `spawn_substitute_fetches` attempt these transitions on a
    /// re-probe hit. Today `Failed` is reset by `dag.merge`
    /// (`is_retriable_on_resubmit`), so the path is unreachable; the
    /// arms are kept parallel to Poisoned/DependencyFailed so the
    /// state machine and the I-094 reprobe-lane callers agree —
    /// defense-in-depth if `is_retriable_on_resubmit` ever bounds
    /// `Failed` by retry-count (which would silently activate the
    /// "re-probe hit on Failed dropped with warn!" gap).
    #[test]
    fn test_failed_reprobe_transitions_symmetry() {
        use DerivationStatus::*;
        assert!(Failed.validate_transition(Completed).is_ok());
        assert!(Failed.validate_transition(Substituting).is_ok());
        // Parallel: the existing I-094 lanes these mirror.
        assert!(Poisoned.validate_transition(Completed).is_ok());
        assert!(Poisoned.validate_transition(Substituting).is_ok());
        assert!(DependencyFailed.validate_transition(Completed).is_ok());
        assert!(DependencyFailed.validate_transition(Substituting).is_ok());
    }

    #[test]
    fn test_reset_to_ready() -> anyhow::Result<()> {
        let node = dummy_node();

        // Assigned -> Ready: direct valid transition
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Assigned);
        state.assigned_executor = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_executor.is_none());

        // Running -> Failed -> Ready: goes through Failed
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Running);
        state.assigned_executor = Some("w1".into());
        assert!(state.reset_to_ready().is_ok());
        assert_eq!(state.status(), DerivationStatus::Ready);
        assert!(state.assigned_executor.is_none());

        // Invalid source states rejected
        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Queued);
        assert!(state.reset_to_ready().is_err());

        let mut state = DerivationState::try_from_node(&node)?;
        state.set_status_for_test(DerivationStatus::Completed);
        assert!(state.reset_to_ready().is_err());
        Ok(())
    }

    #[test]
    fn test_from_poisoned_row_invalid_drv_path() {
        // Malformed drv_path (not a store path) → Err((hash, StorePathError)).
        // Covers the error branch that recovery.rs logs-and-skips.
        let row = crate::db::PoisonedDerivationRow {
            derivation_id: uuid::Uuid::new_v4(),
            drv_hash: "somehash".into(),
            drv_path: "not-a-store-path".into(),
            pname: None,
            system: "x86_64-linux".into(),
            failed_builders: vec![],
            elapsed_secs: 100.0,
            is_fixed_output: false,
            resubmit_cycles: 0,
        };
        let err = DerivationState::from_poisoned_row(row).unwrap_err();
        assert_eq!(
            err.0, "somehash",
            "error tuple returns drv_hash for logging"
        );
    }

    // r[verify sched.ca.cutoff-compare]
    /// `ca_output_unchanged` resets to `false` on recovery. This is
    /// the DOCUMENTED behavior (see the field's doc-comment), not a
    /// bug: the compare→cascade window is a single actor tick, so
    /// restart-loss costs one wasted build, never a stale-skip.
    ///
    /// This test proves recovery doesn't LEAK a stale `true` from
    /// some other path (e.g., if `from_recovery_row` used `Default`
    /// and someone changed the default, or if PG grew a column
    /// someone wired through without reading the doc-comment).
    #[test]
    fn ca_output_unchanged_resets_on_recovery() {
        let row = crate::db::RecoveryDerivationRow {
            derivation_id: uuid::Uuid::new_v4(),
            drv_hash: "cahash".into(),
            drv_path: rio_test_support::fixtures::test_drv_path("ca-recover"),
            pname: Some("ca-recover".into()),
            system: "x86_64-linux".into(),
            status: "queued".into(),
            required_features: vec![],
            assigned_builder_id: None,
            retry_count: 0,
            resubmit_cycles: 0,
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            wanted_output_names: vec![],
            is_fixed_output: false,
            // is_ca=true: this IS a CA derivation, so the flag
            // mattered pre-restart. Prove it's false post-restart
            // regardless.
            is_ca: true,
            topdown_pruned: false,
            closure_hole: false,
            failed_builders: vec![],
            floor_mem_bytes: 0,
            floor_disk_bytes: 0,
            floor_deadline_secs: 0,
            drv_content: None,
            exec_id: None,
        };
        let state = DerivationState::from_recovery_row(row, DerivationStatus::Queued).unwrap();
        assert!(state.ca.is_ca, "precondition: recovered as CA");
        assert!(
            !state.ca.output_unchanged,
            "recovery MUST reset ca_output_unchanged to false (NOT persisted — \
             compare result from a prior scheduler instance is stale)"
        );
    }

    // r[verify sched.merge.exec-correlation+7]
    /// A terminal state's execution was finalized by that terminal's
    /// epilogue. Every terminal → non-terminal reset carve-out in
    /// `validate_transition` must drop the finalized execution's `exec_id`
    /// so `exec_id_for_terminal` cannot attribute it to the node's next
    /// lifecycle. Non-terminal → terminal must NOT clear: the epilogue runs
    /// after the transition and still needs the carrier.
    #[test]
    fn transition_out_of_terminal_clears_exec_id() {
        use DerivationStatus::*;
        let mk = |status: DerivationStatus| {
            let mut s = DerivationState::from_recovery_row(
                crate::db::RecoveryDerivationRow::test_default("exid", "x86_64-linux"),
                Ready,
            )
            .unwrap();
            s.set_status_for_test(status);
            s.exec_id = Some(uuid::Uuid::now_v7());
            s
        };
        // The terminal-exit reset carve-outs (validate_transition), both
        // source halves of each. (Poisoned, Created) is a valid carve-out
        // with no live production caller (the poison-TTL sweep removes the
        // node instead of transitioning it) — pinned anyway because the
        // predicate is on terminality, not on the lane.
        for (from, to) in [
            (Poisoned, Substituting),         // I-094 substitutable reprobe
            (DependencyFailed, Substituting), // I-094 substitutable reprobe
            (Poisoned, Queued),               // I-094 deferred reprobe
            (DependencyFailed, Queued),       // I-094 deferred reprobe
            (Completed, Ready),               // I-047 stale-output reset
            (Skipped, Queued),                // I-047 stale-output reset
            (Poisoned, Created),              // carve-out only, no live caller
        ] {
            let mut s = mk(from);
            s.transition(to)
                .unwrap_or_else(|e| panic!("{from:?}→{to:?}: {e}"));
            assert_eq!(
                s.exec_id, None,
                "{from:?}→{to:?} must clear the finalized execution's exec_id"
            );
        }
        // Entering a terminal keeps the carrier — the epilogue reads it
        // after the transition.
        let mut s = mk(Running);
        let kept = s.exec_id;
        s.transition(Cancelled).unwrap();
        assert_eq!(
            s.exec_id, kept,
            "Running→Cancelled must keep exec_id for the epilogue"
        );
        // Terminal → terminal (I-094 reprobe cache-hit) keeps it too — the
        // guard is specifically about *leaving* terminality.
        let mut s = mk(Poisoned);
        let kept = s.exec_id;
        s.transition(Completed).unwrap();
        assert_eq!(
            s.exec_id, kept,
            "Poisoned→Completed is not a lifecycle reset"
        );
    }

    // r[verify sched.state.machine]
    /// Exhaustive (from, to) cartesian product over all 11 states.
    /// Every pair is explicitly Ok or Err — a mutant that flips ONE
    /// arm's outcome breaks exactly one assertion. Complements
    /// `test_derivation_valid_transitions` (positive list) and
    /// `test_derivation_invalid_transitions` (negative samples) with
    /// full-coverage table: 11×11 = 121 cases.
    ///
    /// Cargo-mutants baseline: 30 candidate mutations in
    /// `validate_transition`. Without this test, deleting or
    /// inverting a single match arm would only be caught if the
    /// specific (from, to) pair happened to be in the sample tests.
    #[test]
    fn validate_transition_exhaustive() {
        use DerivationStatus::*;
        // Valid transitions (the full allowed set). Terminal
        // self-transitions are idempotent (Ok). Everything not in
        // this set MUST be Err.
        let valid: std::collections::HashSet<(DerivationStatus, DerivationStatus)> = [
            // Happy path
            (Created, Completed),          // cache hit
            (Ready, Completed),            // dispatch-time FOD store-hit (I-067)
            (Queued, Completed),           // merge-time re-probe (I-099)
            (Poisoned, Completed),         // merge-time re-probe unpoisons (I-094)
            (DependencyFailed, Completed), // merge-time re-probe (I-099)
            (Failed, Completed),           // merge-time re-probe (I-094 symmetry)
            (Created, Queued),
            (Queued, Ready),
            (Ready, Queued), // I-047 parent-side: dep output GC'd
            (Ready, Assigned),
            (Assigned, Running),
            (Assigned, Ready), // worker lost
            (Running, Completed),
            (Running, Failed),
            (Running, Poisoned),
            (Ready, Poisoned),
            (Failed, Ready),
            // DependencyFailed cascade
            (Created, DependencyFailed),
            (Queued, DependencyFailed),
            (Ready, DependencyFailed),
            // Poison TTL reset
            (Poisoned, Created),
            // Output GC'd between completion and later merge (I-047)
            (Completed, Ready),
            (Completed, Queued),           // dep also reset
            (Completed, DependencyFailed), // dep terminally-failed (revert_target_for)
            (Skipped, Ready),
            (Skipped, Queued),
            (Skipped, DependencyFailed),
            // I-094 deferred re-probe (output present, dep in-flight)
            (Poisoned, Queued),
            (Failed, Queued),
            (DependencyFailed, Queued),
            // Cancel from in-flight
            (Assigned, Cancelled),
            (Running, Cancelled),
            // CA early-cutoff
            (Queued, Skipped),
            (Ready, Skipped),
            // Detached upstream fetch (r[sched.substitute.detached+5])
            (Created, Substituting),
            (Queued, Substituting),
            (Ready, Substituting),
            (Poisoned, Substituting), // I-094 reprobe substitutable lane
            (DependencyFailed, Substituting),
            (Failed, Substituting), // I-094 symmetry
            (Substituting, Completed),
            (Substituting, Ready),
            (Substituting, Queued),
            (Substituting, DependencyFailed), // fetch failed + dep terminally-failed
            (Substituting, Cancelled),
            // Terminal self-transitions (idempotent)
            (Completed, Completed),
            (Poisoned, Poisoned),
            (DependencyFailed, DependencyFailed),
            (Cancelled, Cancelled),
            (Skipped, Skipped),
        ]
        .into_iter()
        .collect();

        for &from in DerivationStatus::ALL {
            for &to in DerivationStatus::ALL {
                let result = from.validate_transition(to);
                if valid.contains(&(from, to)) {
                    assert!(
                        result.is_ok(),
                        "expected {from} -> {to} to be VALID (in state machine)"
                    );
                } else {
                    assert!(
                        result.is_err(),
                        "expected {from} -> {to} to be INVALID (not in state machine)"
                    );
                }
            }
        }
    }

    #[test]
    fn test_from_poisoned_row_infinity_elapsed_does_not_panic() {
        // poisoned_at = '-infinity'::timestamp in PG → EXTRACT returns
        // +inf. from_secs_f64(+inf) panics. The clamp guards this
        // (requires manual DB corruption, but a panic here bricks
        // scheduler startup entirely — disproportionate).
        let row = crate::db::PoisonedDerivationRow {
            derivation_id: uuid::Uuid::new_v4(),
            drv_hash: "infhash".into(),
            drv_path: rio_test_support::fixtures::test_drv_path("inf"),
            pname: None,
            system: "x86_64-linux".into(),
            failed_builders: vec![],
            elapsed_secs: f64::INFINITY,
            is_fixed_output: false,
            resubmit_cycles: 0,
        };
        let state = DerivationState::from_poisoned_row(row).unwrap();
        // Clamp caps at 1yr, checked_sub(1yr) on most boxes → None →
        // poisoned_at = now. This is a panic guard, not correctness
        // — recovery.rs filters expired rows before calling here so
        // a +inf elapsed would never reach this in practice.
        assert!(state.retry.poisoned_at.is_some());
    }

    /// `attributed_tenant()` is deterministic across `HashSet`
    /// iteration order: same set → same tenant, regardless of
    /// insertion order. Regression for the `.next()` form, which
    /// returned hash-bucket order (RandomState) and let solve key on
    /// tenant_A while the completion sample landed under tenant_B.
    #[test]
    fn attributed_tenant_deterministic_min() -> anyhow::Result<()> {
        use super::super::{BuildInfo, BuildOptions, PriorityClass};
        let t_hi = Uuid::from_u128(0xffff_ffff_ffff_ffff_ffff_ffff_ffff_ffff);
        let t_lo = Uuid::from_u128(0x1);
        let (b_hi, b_lo) = (Uuid::new_v4(), Uuid::new_v4());
        let mk = |bid, tid| {
            BuildInfo::new_pending(
                bid,
                Some(tid),
                PriorityClass::Scheduled,
                false,
                BuildOptions::default(),
                HashSet::new(),
            )
        };
        let builds: std::collections::HashMap<_, _> =
            [(b_hi, mk(b_hi, t_hi)), (b_lo, mk(b_lo, t_lo))].into();

        let mut s = DerivationState::try_from_node(&dummy_node())?;
        // Insert hi-tenant build first, lo second.
        s.interested_builds.insert(b_hi);
        s.interested_builds.insert(b_lo);
        assert_eq!(s.attributed_tenant(&builds), Some(t_lo), "min, not first");

        // Reverse insertion order — same answer.
        let mut s2 = DerivationState::try_from_node(&dummy_node())?;
        s2.interested_builds.insert(b_lo);
        s2.interested_builds.insert(b_hi);
        assert_eq!(
            s2.attributed_tenant(&builds),
            Some(t_lo),
            "insertion order irrelevant"
        );
        Ok(())
    }

    /// `M_062`: a recovery row carrying authoritative inline drv_content
    /// hydrates both the bytes and the re-parsed inputSrcs; a row
    /// without it keeps the empty defaults (worker fetches from store).
    // r[verify sched.recovery.inline-drv-durability]
    #[test]
    fn from_recovery_row_hydrates_authoritative_drv_content() {
        let aterm = br#"Derive([("out","/nix/store/abc-out","","")],[],["/nix/store/abc-gcc","/nix/store/abc-glibc"],"x86_64-linux","/bin/sh",[],[("out","/nix/store/abc-out")])"#;
        let row = crate::db::RecoveryDerivationRow {
            drv_content: Some(aterm.to_vec()),
            ..crate::db::RecoveryDerivationRow::test_default("durable-drv", "x86_64-linux")
        };
        let state =
            DerivationState::from_recovery_row(row, DerivationStatus::Ready).expect("row hydrates");
        assert_eq!(state.drv_content, aterm.to_vec(), "bytes restored");
        assert_eq!(
            state.input_srcs,
            vec!["/nix/store/abc-gcc", "/nix/store/abc-glibc"],
            "inputSrcs re-parsed from the restored content"
        );

        let bare = DerivationState::from_recovery_row(
            crate::db::RecoveryDerivationRow::test_default("plain-drv", "x86_64-linux"),
            DerivationStatus::Ready,
        )
        .expect("row hydrates");
        assert!(bare.drv_content.is_empty(), "no persisted content → empty");
        assert!(bare.input_srcs.is_empty());
    }

    /// A recovered CA node carrying authoritative inline content regains
    /// its modular hash (recomputed from the bytes — identical to what
    /// ingress validated); rows without content, non-CA rows, and
    /// unparseable content stay `None`.
    // r[verify sched.recovery.inline-drv-ca-hash]
    #[test]
    fn from_recovery_row_recomputes_ca_modular_hash_for_authoritative_content() {
        let aterm = r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","")])"#;
        let row = crate::db::RecoveryDerivationRow {
            drv_content: Some(aterm.as_bytes().to_vec()),
            is_ca: true,
            expected_output_paths: vec![String::new()],
            ..crate::db::RecoveryDerivationRow::test_default("ca-durable", "x86_64-linux")
        };
        let expected = rio_nix::derivation::hash_derivation_modulo(
            &rio_nix::derivation::Derivation::parse(aterm).unwrap(),
            &row.drv_path,
            &|_| None,
            &mut std::collections::HashMap::new(),
        )
        .unwrap();
        let state =
            DerivationState::from_recovery_row(row, DerivationStatus::Ready).expect("hydrates");
        assert_eq!(
            state.ca.modular_hash,
            Some(expected),
            "modular hash recomputed from the restored bytes"
        );

        // CA without persisted content: stays None (ordinary DAG-path CA
        // node — the documented lossy posture).
        let bare_ca = DerivationState::from_recovery_row(
            crate::db::RecoveryDerivationRow {
                is_ca: true,
                ..crate::db::RecoveryDerivationRow::test_default("ca-bare", "x86_64-linux")
            },
            DerivationStatus::Ready,
        )
        .expect("hydrates");
        assert_eq!(bare_ca.ca.modular_hash, None);

        // Content on a non-CA row: no hash.
        let ia = DerivationState::from_recovery_row(
            crate::db::RecoveryDerivationRow {
                drv_content: Some(b"Derive-not-ca".to_vec()),
                ..crate::db::RecoveryDerivationRow::test_default("ia-durable", "x86_64-linux")
            },
            DerivationStatus::Ready,
        )
        .expect("hydrates");
        assert_eq!(ia.ca.modular_hash, None);

        // Unparseable content on a CA row: degrades to None, no panic.
        let broken = DerivationState::from_recovery_row(
            crate::db::RecoveryDerivationRow {
                drv_content: Some(b"not-an-aterm".to_vec()),
                is_ca: true,
                ..crate::db::RecoveryDerivationRow::test_default("ca-broken", "x86_64-linux")
            },
            DerivationStatus::Ready,
        )
        .expect("hydrates");
        assert_eq!(broken.ca.modular_hash, None);
    }
}

#[cfg(test)]
mod status_snapshot {
    //! Cross-language DerivationStatus enforcement. The golden file at
    //! `rio-scheduler/tests/golden/derivation_statuses.json` is the single
    //! source of truth — both this Rust-side snapshot test AND
    //! rio-dashboard's vitest (`graphLayout.test.ts` cross-language
    //! describe block) compare against it. A 12th variant added here
    //! without plumbing to the dashboard's STATUS_CLASS/SORT_RANK/
    //! TERMINAL mirrors breaks both checks (loudly, not silently-gray).

    use super::DerivationStatus;

    /// Serialize `ALL` as the golden's `[{status, terminal}]` shape.
    /// Manual formatting (not serde_json) because (a) avoids a dev-dep,
    /// and (b) the golden is hand-formatted for git-diff readability
    /// (one status per line, fixed key order) — matching that exactly
    /// with to_string_pretty would need a custom Serialize impl anyway.
    fn emit() -> String {
        let mut out = String::from("[\n");
        for (i, s) in DerivationStatus::ALL.iter().enumerate() {
            let sep = if i + 1 < DerivationStatus::ALL.len() {
                ","
            } else {
                ""
            };
            out.push_str(&format!(
                "  {{\"status\": \"{}\", \"terminal\": {}}}{}\n",
                s.as_str(),
                s.is_terminal(),
                sep
            ));
        }
        out.push(']');
        out
    }

    // r[verify sched.state.transitions]
    /// The canonical `{as_str, is_terminal}` set matches the golden
    /// snapshot that rio-dashboard's vitest also reads. Adding a 12th
    /// variant to `DerivationStatus` (or changing `is_terminal`'s
    /// classification) drifts `emit()` away from the golden — this test
    /// fails with a diff-friendly multi-line mismatch and a checklist
    /// of everywhere the new variant needs plumbing.
    #[test]
    fn derivation_status_snapshot_is_current() {
        let json = emit();
        let golden = include_str!("../../tests/golden/derivation_statuses.json");
        assert_eq!(
            json.trim(),
            golden.trim(),
            "\nDerivationStatus {{as_str, is_terminal}} set drifted from golden.\n\
             If you added/reclassified a variant, update IN ORDER:\n\
               (1) rio-scheduler/tests/golden/derivation_statuses.json\n\
               (2) rio-dashboard/src/lib/graphLayout.ts — STATUS_CLASS + SORT_RANK + TERMINAL\n\
               (3) rio-dashboard/src/lib/__tests__/graphLayout.test.ts — intended-set asserts\n\
               (4) docs/spec/components/scheduler.typ — PG CHECK constraint list\n\
               (5) this const: DerivationStatus::ALL (and the exhaustive match below)\n\
             ── emitted ──\n{json}\n── golden ──\n{golden}"
        );
    }

    /// Positive control: `ALL` is truly exhaustive. A 12th variant
    /// without an `ALL` entry compiles (arrays don't enforce
    /// exhaustiveness) — this exhaustive match forces a compile error
    /// on the new variant, and the .len() assert catches the inverse
    /// (an `ALL` entry without an enum variant is already a compile
    /// error, so this direction is belt-and-braces).
    #[test]
    fn all_const_is_exhaustive() {
        // Exhaustive match: adding a 12th variant without a match arm
        // here is a compile error — the cheapest possible "did you
        // remember to update ALL?" reminder.
        #[allow(clippy::match_same_arms)]
        fn _witness(s: DerivationStatus) -> usize {
            match s {
                DerivationStatus::Created => 0,
                DerivationStatus::Queued => 1,
                DerivationStatus::Ready => 2,
                DerivationStatus::Assigned => 3,
                DerivationStatus::Running => 4,
                DerivationStatus::Substituting => 5,
                DerivationStatus::Completed => 6,
                DerivationStatus::Failed => 7,
                DerivationStatus::Poisoned => 8,
                DerivationStatus::DependencyFailed => 9,
                DerivationStatus::Cancelled => 10,
                DerivationStatus::Skipped => 11,
            }
        }
        assert_eq!(DerivationStatus::ALL.len(), 12);
        // Each ALL[i] round-trips through the witness at its own index.
        // Catches accidental duplicates or order drift (the golden
        // expects ALL's order).
        for (i, s) in DerivationStatus::ALL.iter().enumerate() {
            assert_eq!(_witness(*s), i, "ALL[{i}]={s} at wrong index");
        }
    }
}
