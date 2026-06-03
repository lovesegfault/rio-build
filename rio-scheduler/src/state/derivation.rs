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
use std::time::{Duration, Instant};

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
    // r[impl sched.state.machine+2]
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
}

// Hand-rolled (not the macro's `parse_err` form) — kept in this shape
// from the PD-D3 transitional window so the alphabet is explicit at
// the decode boundary. The walk-era 'substituting' decode arm was
// removed with migration 080: the migration's data step rewrites any
// leftover row to 'queued' BEFORE the narrowed CHECK lands, and both
// services migrate at startup before any status read, so the legacy
// string is unreachable post-080 (see `M_080`).
impl ::std::str::FromStr for DerivationStatus {
    type Err = TransitionError;
    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "created" => Ok(Self::Created),
            "queued" => Ok(Self::Queued),
            "ready" => Ok(Self::Ready),
            "assigned" => Ok(Self::Assigned),
            "running" => Ok(Self::Running),
            "completed" => Ok(Self::Completed),
            "failed" => Ok(Self::Failed),
            "poisoned" => Ok(Self::Poisoned),
            "dependency_failed" => Ok(Self::DependencyFailed),
            "cancelled" => Ok(Self::Cancelled),
            "skipped" => Ok(Self::Skipped),
            other => Err(TransitionError::UnknownStatus(other.to_string())),
        }
    }
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
    // r[impl sched.state.terminal-idempotent+2]
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
                // completes. (Failed is non-terminal; its Queued arm
                // is in the table below.)
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
            // Timeout-budget exhaustion observed by the controller's
            // DeadlineExceeded backstop (sched.termination.deadline-
            // exceeded+3): the disconnect already re-queued the node, so
            // the terminal Cancelled transition the cap mandates starts
            // from Ready — the Cancelled counterpart of the
            // Ready→Poisoned fleet-exhaust edge above.
            (Self::Ready, Self::Cancelled) => true,
            // CA early-cutoff: a CA dep completed with unchanged
            // output hash → this derivation would produce the same
            // output. Skip without running. Ready is allowed for
            // order-independence vs find_newly_ready (cascade may
            // race a prior Queued→Ready promotion — matches
            // DependencyFailed precedent at completion.rs).
            (Self::Queued | Self::Ready, Self::Skipped) => true,
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

    // r[impl sched.state.machine+2]
    /// Kind-aware transition validation for the PULL-MINT path only
    /// (PD-6, design §2.3: "one new transition edge `Queued → Assigned`
    /// is legal for materialization mints only").
    ///
    /// Build mints — and every (from, to) pair other than the single
    /// delta cell — delegate to the kind-blind [`Self::validate_transition`]
    /// table unchanged, so the as-built table (and its exhaustive test)
    /// stays byte-identical. Materialization mints additionally accept
    /// `Queued → Assigned`: materialization does not wait for deps (the
    /// store fetches from upstream; dep state is irrelevant to the
    /// claim), so the kernel's PD-6 Queued admission and this edge are
    /// the two halves of one decision — the kernel admits, the mint's
    /// in-memory transition must then accept (a rejection here would
    /// re-open the PDQ-6 stranded-mint window: durable rows committed
    /// for an attempt the actor refuses to track).
    pub fn validate_transition_for_mint(
        self,
        to: Self,
        kind: AttemptKind,
    ) -> Result<(), TransitionError> {
        if kind == AttemptKind::Materialization && self == Self::Queued && to == Self::Assigned {
            return Ok(());
        }
        self.validate_transition(to)
    }

    /// The RELEASE mirror of [`Self::validate_transition_for_mint`]
    /// (A2.5, merged_bug_318): a materialization claim admitted from
    /// `Queued` (the PD-6 dep-racing edge) must be able to RETURN to
    /// `Queued` when the attempt closes with deps still unbuilt.
    /// Exactly ONE kinded delta cell — `Assigned → Queued` for the
    /// materialization kind (the mint edge's inverse); the
    /// worker-lost path's `Running → Failed → Queued` second step is
    /// already legal in the kind-blind table (the resubmit edge).
    /// Build releases keep the as-built table byte-identically: the
    /// mint admits builds from `Ready` alone, so a build release
    /// never has a dep-blocked target.
    pub fn validate_transition_for_release(
        self,
        to: Self,
        kind: AttemptKind,
    ) -> Result<(), TransitionError> {
        if kind == AttemptKind::Materialization && self == Self::Assigned && to == Self::Queued {
            return Ok(());
        }
        self.validate_transition(to)
    }
}

/// Retry / failure-tracking sub-state of a [`DerivationState`].
///
/// Since the Phase-1b collapse (T-1b.13) this is the **fold-derived
/// cached dispatch view**: the budget counters and the per-executor
/// exclusion set are recomputed from `decide()` over the node's
/// committed attempt history (seeded by the carried legacy floor,
/// decision P5) whenever that history changes
/// (`DerivationState::refresh_retry_view_from_ledger` /
/// `DerivationState::rebuild_retry_view_from_ledger`); no code path
/// mutates the budget counters directly any more
/// (`sched.retry.counters-refine-history`). Two named carve-outs stay
/// actor-managed rather than fold-derived: [`Self::poisoned_at`]
/// (status metadata owned by the `derivations` row,
/// `sched.poison.ttl-persist`) and [`Self::backoff_until`] (pacing
/// state — the production jitter is applied at the failure site and
/// the clear-on-dispatch has no ledger event). The durable source of
/// truth for every verdict is the appending transaction's suffix read;
/// this view only feeds the dispatch-time readers (`hard_filter`'s
/// exclusion, the backoff defer), diagnostics, and the resubmit-bound
/// check.
#[derive(Debug, Clone, Default)]
pub struct RetryState {
    /// Number of retry attempts so far in the CURRENT poison cycle.
    /// Gated against `RetryPolicy::max_retries` at completion. Reset to
    /// 0 on resubmit-after-poison (fresh per-cycle budget). See
    /// [`Self::resubmit_cycles`] for the cross-cycle counter.
    pub count: u32,
    /// Number of poison→resubmit reset events. Gated against
    /// [`POISON_RESUBMIT_RETRY_LIMIT`]. Seeded in memory by `dag::merge`
    /// on each resubmit-reset (the carried prior + 1 — the documented
    /// in-memory seed of the new cycle); the `resubmit_reset` ledger row
    /// then carries the same index durably, so the bound survives leader
    /// failover via the fold (the legacy `M_051` mirror column is gone
    /// — dropped by `M_075`; the ledger is the sole durable source).
    /// Distinct from [`Self::count`]: a
    /// single counter cannot be both per-cycle-reset and
    /// cross-cycle-accumulated — when `count` served both roles, the
    /// `max_retries=2` cap was the permanent ceiling and the resubmit
    /// bound never fired (bug_152).
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
    /// Durable via the attempt ledger (the legacy `failed_builders`
    /// mirror column is gone — dropped by `M_075`).
    pub failed_builders: HashSet<ExecutorId>,
    /// Total TransientFailure/disconnect count (same-worker repeats
    /// counted). Drives poison threshold when
    /// `PoisonConfig::require_distinct_workers = false` (single-worker
    /// dev deployments). Fold-derived; floored at
    /// `failed_builders.len()` for mixed-era histories.
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

db_str_enum! {
    /// Row kind in the durable attempt ledger (`drv_attempts.event_kind`,
    /// migration 068): an observed attempt/charge event, or a reset event
    /// (resubmit reset, cache-hit clear, poison clear) that starts a new
    /// suffix for the fold.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum AttemptEventKind {
        Attempt = "attempt",
        Reset = "reset",
    }
    parse_err(_s) = &'static str:
        "invalid attempt event kind (must be 'attempt' or 'reset')";
}

db_str_enum! {
    /// Outcome classification of one attempt-ledger row
    /// (`drv_attempts.outcome_class`, migration 068; alphabet expanded
    /// by 079). This is the
    /// `classify()` alphabet: the CHECK constraint in the migration and
    /// this enum MUST stay in lockstep — extending the alphabet is a new
    /// migration plus a variant here, verified by the
    /// `outcome_class_alphabet_matches_check_constraint` test.
    ///
    /// The materialization classes ARE in the alphabet (the
    /// substitution-replacement campaign's typed carve-out, superseding
    /// the old structural carve-out that kept `substitution` out): they
    /// enter the fold but are partitioned by attempt kind so they feed
    /// exactly one budget (max_materialization_attempts) and are
    /// invisible to every build budget. See `rio_retry_kernel::decide`'s
    /// kind partition.
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
    pub enum OutcomeClass {
        /// E1 — worker-reported `TransientFailure` (build ran, exited
        /// non-zero) or `Unspecified`.
        Transient = "transient",
        /// E2 — worker-reported `InfrastructureFailure`, non-exempt.
        Infra = "infra",
        /// E2/E6 — infra failure exempt from the non-exempt cap
        /// (floor-promoted CgroupOom/OOMKilled, CONCURRENT_PUTPATH).
        ExemptInfra = "exempt_infra",
        /// E4/E7 — worker `TimedOut` or controller `DeadlineExceeded`.
        Timeout = "timeout",
        /// E3 — one of the seven permanent failure statuses.
        Permanent = "permanent",
        /// A dependent swept to `DependencyFailed` by an ancestor's
        /// terminal failure (no execution of its own).
        Cascade = "cascade",
        /// E8 — the scheduler-side backstop timer fired for a Running
        /// build with no report.
        Backstop = "backstop",
        /// E5 — executor loss released the execution (pull-era: the
        /// charge-free synthesized-verdict / establishment-sweep
        /// closure; stream-era: disconnect / heartbeat timeout /
        /// force-drain); classification not yet established
        /// (first installment of a two-installment attempt).
        Disconnected = "disconnected",
        /// A `disconnected` attempt whose classifying report never
        /// arrived: established by the correlation-TTL sweep (or the
        /// backstop) as an unreported executor crash.
        ExecutorCrash = "executor_crash",
        /// E9 — dispatch-time fleet-exhaust verdict marker (not a
        /// charge; the fold treats it as a no-op event).
        FleetExhaust = "fleet_exhaust",
        /// Reset row: `dag::merge` resubmit reset of a retriable
        /// terminal node (carries the new `resubmit_cycle`).
        ResubmitReset = "resubmit_reset",
        /// Reset row: cache-hit retry-state reset (output turned up
        /// in the store / re-probe found it substitutable).
        CacheHitClear = "cache_hit_clear",
        /// Reset row: admin `ClearPoison` or the poison-TTL expiry.
        PoisonCleared = "poison_cleared",
        /// Substitution-replacement: a materialization attempt confirmed
        /// a live-wanted path absent upstream after the full per-path
        /// retry ladder. A verdict consumed by the four-arm routing
        /// (sched.materialize.routing) — never retried by a budget.
        MaterializationUnobtainable = "materialization_unobtainable",
        /// Substitution-replacement: a materialization attempt hit
        /// infrastructure failure (upstream 5xx/timeout/store-internal/
        /// no-tenant-context) or its executing replica crashed
        /// (establishment-written). Counts toward
        /// max_materialization_attempts and toward NOTHING else —
        /// invisible to every build budget.
        MaterializationInfra = "materialization_infra",
        /// Reset row, materialization lane (migration 085): written at
        /// job creation — one fresh budget window per job. Cuts the
        /// materialization-lane suffix exactly as the build resets cut
        /// the build lane (the cut is `(attempt_kind, event_kind)`;
        /// this class is row data, never the cut predicate).
        MaterializationReset = "materialization_reset",
    }
    parse_err(_s) = &'static str:
        "invalid outcome class (not in the migration-066 alphabet)";
}

db_str_enum! {
    /// Which party observed/reported the event behind an attempt-ledger
    /// row (`drv_attempts.reporting_party`, migration 068).
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum ReportingParty {
        /// Worker `CompletionReport`.
        Worker = "worker",
        /// Controller `ReportExecutorTermination`.
        Controller = "controller",
        /// Scheduler-side observation (disconnect, backstop, sweep,
        /// dispatch-time verdict, TTL expiry).
        Scheduler = "scheduler",
        /// Admin RPC (ClearPoison).
        Admin = "admin",
    }
    parse_err(_s) = &'static str:
        "invalid reporting party (must be worker/controller/scheduler/admin)";
}

db_str_enum! {
    /// Work class of an execution/attempt (`drv_executions.attempt_kind`,
    /// migration 078): a from-source build or a store-executed
    /// materialization (substitution-replacement campaign, design §2.5).
    /// The db mirror of [`rio_retry_kernel::AttemptKind`]; the string
    /// alphabet stays in lockstep with the 078 CHECK constraint,
    /// verified by the `attempt_kind_alphabet_matches_check_constraint`
    /// test. Kind is keyed on this column and ONLY this column — never
    /// derived from an executor-id prefix (the newtypes.rs convention).
    #[derive(Debug, Clone, Copy, PartialEq, Eq, Default)]
    pub enum AttemptKind {
        /// A from-source build attempt (the as-built work class; the
        /// column DEFAULT, so every existing writer is untouched).
        #[default]
        Build = "build",
        /// A store-executed materialization attempt.
        Materialization = "materialization",
    }
    parse_err(_s) = &'static str:
        "invalid attempt kind (must be 'build' or 'materialization')";
}

db_str_enum! {
    /// Origin of a materialization job (`materialization_jobs.origin`,
    /// migration 078): which classification demanded the job
    /// (substitution-replacement campaign, design §2.1). The alphabet is
    /// forward-complete — `stale_reset`/`reprobe` creation sites are
    /// Phase B work — and stays in lockstep with the 078 CHECK
    /// constraint, verified by the
    /// `materialization_job_alphabets_match_check_constraints` test.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JobOrigin {
        /// A top-down prune kept the node on substitution evidence
        /// (design §2.1 row 1).
        Pruned = "pruned",
        /// Merge classification found the node's outputs upstream
        /// (the new_sub lane, design §2.1 row 2).
        CacheOpportunity = "cache_opportunity",
        /// The stale-Completed verify demanded re-materialization
        /// (creation site is Phase B — PD-18; literal reserved now).
        StaleReset = "stale_reset",
        /// The reprobe lane (creation site is Phase B — PD-17; literal
        /// reserved now).
        Reprobe = "reprobe",
    }
    parse_err(_s) = &'static str:
        "invalid materialization-job origin (not in the migration-078 alphabet)";
}

db_str_enum! {
    /// State of a materialization job (`materialization_jobs.state`,
    /// migration 078). "Claimed" is deliberately NOT a job state: a
    /// claim is an open attempt (assignments + drv_executions rows);
    /// the job row is untouched until consumption resolves it.
    #[derive(Debug, Clone, Copy, PartialEq, Eq)]
    pub enum JobState {
        /// Unresolved: claimable by the store executor (unless parked).
        Pending = "pending",
        /// Consumption confirmed the live wanted set present.
        ResolvedSuccess = "resolved_success",
        /// Consumption took the fail-fast arm (a live-wanted path
        /// confirmed missing-and-unsubstitutable).
        ResolvedUnobtainable = "resolved_unobtainable",
        /// Consumption routed the node from-source (the Vouched/Pending
        /// arms of the four-arm routing).
        ResolvedFromSource = "resolved_from_source",
        /// The node produced by other means while the job was open.
        Obsolete = "obsolete",
        /// No live DAG-interested build remains.
        Cancelled = "cancelled",
    }
    parse_err(_s) = &'static str:
        "invalid materialization-job state (not in the migration-078 alphabet)";
}

/// In-memory mirror of one `drv_attempts` row — the per-node attempt
/// history entry that Phase 1b's `decide()` will fold. Field-for-field
/// the row minus `derivation_id` (implicit from the owning node), with
/// the timestamps as epoch seconds (the ledger is PG-authoritative; the
/// in-memory list is a read-through cache of committed rows).
#[derive(Debug, Clone, PartialEq)]
pub struct AttemptRecord {
    /// Ledger primary key (UUIDv7, minted at append).
    pub attempt_id: Uuid,
    /// Attempt event or reset event.
    pub event_kind: AttemptEventKind,
    /// Outcome classification (the `classify()` alphabet).
    pub outcome_class: OutcomeClass,
    /// Execution this attempt corresponds to, when one was dispatched.
    pub exec_id: Option<Uuid>,
    /// Executor that ran (or was assigned) the attempt.
    pub executor_id: Option<ExecutorId>,
    /// Work class of the attempt's execution (substitution-replacement,
    /// joined from `drv_executions.attempt_kind` at suffix load; rows
    /// without an execution are [`AttemptKind::Build`]). Keys the
    /// retry-fold kind partition: materialization-kind records are
    /// invisible to every build budget.
    pub attempt_kind: AttemptKind,
    /// Controller-authoritative source node (071, AD2c) — stamped only
    /// for pull-mode attempts. The retry fold keys the row's
    /// exclusion/budget contribution on this and ONLY this (decision
    /// P12): a row without it charges flat counters but contributes no
    /// exclusion key.
    pub source_node: Option<String>,
    /// Second-installment classification detail (controller reason,
    /// `unreported`, `force_drain`, …). `None` until established.
    pub termination_reason: Option<String>,
    /// Who observed the event.
    pub reporting_party: ReportingParty,
    /// E2's `exempt_from_cap` (floor-promoted or CONCURRENT_PUTPATH).
    pub exempt: bool,
    /// `FloorOutcome::promoted` at append time.
    pub floor_promoted: bool,
    /// `FloorOutcome::at_cap` at append time.
    pub floor_at_cap: bool,
    /// Worker/controller error message, where the path carries one.
    pub error_msg: Option<String>,
    /// `CompletionReport.final_line_count` for report-bearing failures.
    pub final_line_count: Option<i64>,
    /// Resubmit cycle index this row belongs to (reset rows carry the
    /// new cycle).
    pub resubmit_cycle: i32,
    /// When the event occurred (epoch seconds, append-site clock).
    pub occurred_at_epoch_secs: f64,
    /// When the row was committed (epoch seconds, PG clock). `0.0` for
    /// records appended in-memory before a load round-trip.
    pub recorded_at_epoch_secs: f64,
}

/// Content-addressed-derivation sub-state of a [`DerivationState`].
///
/// All fields except `is_ca` are **in-memory only**: recovered CA-on-CA
/// chains dispatch unresolved (collect_ca_inputs skips None) → worker
/// fails on placeholder → retry. The gateway recomputes on the NEXT
/// SubmitBuild that references the derivation.
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
    /// (the gateway computes it post-BFS from the full drv_cache).
    /// `None` for IA derivations AND for the single-node
    /// `BasicDerivation` fallback (no transitive closure to compute over).
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

// r[impl sched.sla.reactive-floor+3]
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
    /// active assignment. UUIDv7 — keys the `drv_executions` PG row and
    /// rio-store's `logs/{drv_hash}/{exec_id}/...` chunk objects.
    /// Mirrors `assignments.exec_id` (the recovery carrier).
    ///
    /// `None` on construction. Set by `assign_to_worker`; cleared by
    /// `reset_to_ready` (worker disconnect, phantom drain, orphan
    /// reconcile, infra/timeout retry below cap, and `rollback_assignment`)
    /// and by `transition()` on any
    /// terminal → non-terminal reset (I-094 reprobe, I-047 stale-output
    /// reset — the prior execution was already finalized at its
    /// terminal and must not be attributed to the node's next
    /// lifecycle). The reader is `terminal_log_epilogue` (which
    /// resolves once via `actor/event.rs::exec_id_for_terminal` and
    /// threads the value to the correlate/stamp steps).
    ///
    /// Recovery preserves the clear across leader failover: the recovery
    /// query only carries `assignments.exec_id` for currently-assigned
    /// drvs (`load_nonterminal_derivations`), so a reset drv's leaked
    /// `pending` assignments row cannot re-stamp this field on the new
    /// leader.
    pub exec_id: Option<Uuid>,
    /// Work class of the OPEN attempt (set with `exec_id` at the mint
    /// bookkeeping, cleared in lockstep with it at every clear site,
    /// recovered from the `drv_executions.attempt_kind` join riding the
    /// same assignment-row guard). The display projection
    /// (`rio_evidence_kernel::pull::display_class`) keys the kinded
    /// running surface on this: BUILD entries get the builder display
    /// (log tail + build activity), MATERIALIZATION entries get the
    /// substitution display, and the running-count aggregates exclude
    /// materialization-claimed nodes (owner decision Q10, bug_144).
    pub open_attempt_kind: Option<AttemptKind>,
    /// Scheduling hints (estimator outputs, resource_floor, critical-path priority).
    pub sched: SchedHint,
    /// ATerm-serialized .drv content, inlined by the gateway for
    /// nodes that will actually dispatch (outputs missing from store).
    /// Empty = worker fetches from store via GetPath (fallback
    /// path, still works). Forwarded verbatim into WorkAssignment.
    /// ≤256 KB bound enforced at gRPC ingress.
    pub drv_content: Vec<u8>,
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
    /// In-memory mirror of this derivation's `drv_attempts` suffix (the
    /// rows since the last reset event). Append-only; entries are pushed
    /// only AFTER the owning appending transaction commits (the ledger
    /// is PG-authoritative, this is a read-through cache), and the
    /// two-installment classification update mirrors `fill_termination`.
    /// Populated from the ledger at recovery. NOT consulted by any
    /// decision in Phase 1a — the RAM counters in [`Self::retry`] stay
    /// authoritative until the Phase-1b collapse.
    attempt_history: Vec<AttemptRecord>,
    /// Realized output store paths (filled on completion).
    pub output_paths: Vec<String>,
    /// Expected output paths (from the proto node at merge time).
    /// Used for: cache-check (merge.rs), and prefetch-hint closure
    /// approximation (children's expected_output_paths = parent's
    /// inputs; see `approx_input_closure`).
    pub expected_output_paths: Vec<String>,
    /// Per-build wanted-output contributions: for each interested build,
    /// the wanted output names of THAT build's submission for this
    /// node. An entry whose value is the EMPTY Vec means that build
    /// wants ALL declared outputs. A
    /// MISSING entry for an interested build means its contribution is
    /// UNKNOWN — [`effective_wanted`]'s conservative-absent arm then
    /// saturates the union to all-declared width (T-D2.3/PD-D5).
    ///
    /// **A droppable cache of `build_wanted_outputs`** (the durable
    /// relation): rebuilt from it at recovery
    /// (`load_wanted_for_live_builds`), never reconciled, never written
    /// back. Entries follow `interested_builds` membership exactly:
    /// recorded where interest is recorded (`dag.merge`'s existing-node
    /// and new-node paths, plus the resubmit-reset carry-over), removed
    /// where interest is removed (`rollback_merge`,
    /// `remove_build_interest`, `remove_build_interest_and_reap`).
    pub wanted_by_build: HashMap<Uuid, Vec<String>>,
    /// Database UUID (set after insertion).
    pub db_id: Option<Uuid>,
    /// When the derivation entered Ready state (for assignment latency metric).
    pub(crate) ready_at: Option<Instant>,
    /// When the derivation entered Running state. For the backstop
    /// timeout: handle_tick checks this + est_duration × 3 (clamped
    /// to daemon_timeout + slack). A build that's been Running far
    /// longer than expected is likely stuck (worker pod alive but
    /// daemon wedged, or the worker's clock jumped).
    pub(crate) running_since: Option<Instant>,
    /// W3C traceparent of the submitting gRPC handler's span, captured
    /// at DAG-merge time. Embedded into `WorkAssignment.traceparent` at
    /// dispatch so the worker's build span chains back to the gateway's
    /// trace regardless of which code path (immediate merge, deferred
    /// completion) triggers dispatch. Empty for recovered
    /// derivations (no user trace). First submitter wins on dedup.
    pub traceparent: String,
    /// `DagActor.probe_generation` at the time of the last dispatch-
    /// time `FindMissingPaths` probe for this node. The batch pre-pass
    /// skips nodes whose `probed_generation == probe_generation` so the
    /// `truncate(DISPATCH_PROBE_BATCH_CAP)` window advances across
    /// inline `dispatch_ready` calls instead of re-probing the head.
    /// `probe_generation` advances once per `handle_tick` (1/s).
    pub probed_generation: u64,
}

/// The realized output paths a stale-Completed reset is about to
/// destroy — captured AT the destruction site as a `#[must_use]`
/// carrier so no reset arm can drop them silently (merged_bug_257:
/// the `!deps_ok` exit lost the floating-CA carrier and the node
/// later re-dispatched from source). The only producer is
/// [`DerivationState::take_realized_paths`]; consumers either route
/// the paths onto a materialization job or discard them EXPLICITLY
/// for the from-source lane.
#[must_use = "the realized paths were just destroyed in memory — route them onto a \
              materialization job or call discard_for_from_source() explicitly"]
pub struct RealizedPathCarrier {
    paths: Vec<String>,
}

impl RealizedPathCarrier {
    /// Consume the carrier into its paths (the job-creation lane).
    pub fn into_paths(self) -> Vec<String> {
        self.paths
    }

    /// Explicit discard: the reset routes from-source (not every
    /// wanted missing path is substitutable), where the re-dispatch
    /// itself reproduces the outputs — the carrier has no consumer.
    pub fn discard_for_from_source(self) {}
}

impl DerivationState {
    /// THE stale-reset destruction site (merged_bug_257): clear
    /// `output_paths`, returning the non-empty, still-wanted realized
    /// paths as a carrier the caller MUST route or explicitly discard.
    /// Replaces the bare `output_paths.clear()` so capture and
    /// destruction are one step.
    pub fn take_realized_paths(
        &mut self,
        unwanted: &std::collections::HashSet<String>,
    ) -> RealizedPathCarrier {
        let paths = self
            .output_paths
            .iter()
            .filter(|p| !p.is_empty() && !unwanted.contains(p.as_str()))
            .cloned()
            .collect();
        self.output_paths.clear();
        RealizedPathCarrier { paths }
    }

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
            open_attempt_kind: None,
            // est_duration/priority: placeholders — merge.rs sets them
            // via critical_path::compute_initial right after
            // try_from_node (SLA cache not in scope here). 0.0 is a
            // visible "not yet set" marker.
            sched: SchedHint::default(),
            drv_content: node.drv_content.clone(),
            input_srcs,
            retry: RetryState::default(),
            attempt_history: Vec::new(),
            output_paths: Vec::new(),
            expected_output_paths: node.expected_output_paths.clone(),
            // The submitting build's contribution is recorded by
            // `dag.merge` (the build_id isn't known here).
            wanted_by_build: HashMap::new(),
            db_id: None,
            ready_at: None,
            running_since: None,
            traceparent: String::new(),
            probed_generation: 0,
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
    /// `drv_content` is empty — worker fetches from store via
    /// GetPath (fallback path, still supported in executor).
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
                // Remaining CA fields lossy on recovery — see CaState doc.
                ..Default::default()
            },
            status,
            interested_builds: HashSet::new(), // populated by build_derivations join
            assigned_executor: row.assigned_builder_id.map(Into::into),
            // From the active `assignments` row, scoped by the JOIN to
            // currently-assigned drvs (`load_nonterminal_derivations`) —
            // NULL for a drv whose dispatch was reset, so recovery
            // preserves `reset_to_ready()`'s clear.
            exec_id: row.exec_id,
            open_attempt_kind: row
                .attempt_kind
                .as_deref()
                .and_then(|k| k.parse::<AttemptKind>().ok()),
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
            drv_content: Vec::new(), // worker fetches from store
            input_srcs: Vec::new(),  // unparsed (no drv_content); DAG-children-only prefetch
            // Construction-time placeholder: recovery re-derives the
            // retry view from the attempt-ledger fold via
            // `rebuild_retry_view_from_ledger` once the suffix is
            // loaded, so budgets survive failover
            // (`sched.retry.failover-budget`). The attempt ledger is the
            // only failure-history record (migration 075 dropped the
            // mirror columns); a node with no attempt rows recovers the
            // default (empty) retry state.
            retry: RetryState::default(),
            // Populated from the attempt ledger by the recovery load
            // (`load_attempt_suffix`) after construction.
            attempt_history: Vec::new(),
            output_paths: Vec::new(), // completed rows not loaded
            expected_output_paths: row.expected_output_paths,
            // Per-build contributions: rebuilt from the durable
            // `build_wanted_outputs` relation by the recovery loader
            // (`load_wanted_for_live_builds`) after construction; a
            // live build with no relation row saturates
            // `effective_wanted` to all-declared width (the
            // conservative-absent arm, T-D2.3/PD-D5).
            wanted_by_build: HashMap::new(),
            db_id: Some(row.derivation_id),
            // Instant fields: conservative defaults.
            // ready_at: Some(now) if Ready (informational; the
            // dispatch-wait histogram retired with the placement layer)
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
            open_attempt_kind: None,
            sched: SchedHint::default(),
            drv_content: Vec::new(),
            input_srcs: Vec::new(),
            // Construction-time placeholder carrying only the row-owned
            // `poisoned_at`; the recovery load re-derives the counters
            // (exclusion set, resubmit bound, budgets) from the
            // attempt-ledger fold via `rebuild_retry_view_from_ledger`,
            // which preserves this row-derived `poisoned_at`
            // (`sched.poison.ttl-persist`).
            retry: RetryState {
                poisoned_at: Some(poisoned_at),
                ..Default::default()
            },
            // Populated from the attempt ledger by the recovery load
            // (`load_attempt_suffix`) after construction.
            attempt_history: Vec::new(),
            output_paths: Vec::new(),
            expected_output_paths: Vec::new(),
            wanted_by_build: HashMap::new(),
            db_id: Some(row.derivation_id),
            ready_at: None,
            running_since: None,
            traceparent: String::new(),
            probed_generation: 0,
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
        Ok(self.apply_validated_transition(from, to))
    }

    // r[impl sched.state.machine+2]
    /// [`Self::transition`] for the pull-mint path: kind-aware
    /// validation ([`DerivationStatus::validate_transition_for_mint`] —
    /// the PD-6 `Queued → Assigned` materialization edge is the only
    /// delta), identical post-validation bookkeeping. Build-kind mints
    /// behave byte-identically to [`Self::transition`].
    pub fn transition_for_mint(
        &mut self,
        to: DerivationStatus,
        kind: AttemptKind,
    ) -> Result<DerivationStatus, TransitionError> {
        let from = self.status;
        from.validate_transition_for_mint(to, kind)?;
        Ok(self.apply_validated_transition(from, to))
    }

    /// The post-validation bookkeeping shared by [`Self::transition`]
    /// and [`Self::transition_for_mint`]. The caller has already
    /// validated `from → to`; this applies the status write and the
    /// derived-field maintenance, returning the old status.
    fn apply_validated_transition(
        &mut self,
        from: DerivationStatus,
        to: DerivationStatus,
    ) -> DerivationStatus {
        // Idempotent no-ops: don't change anything
        if from == to {
            return from;
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
        // r[impl sched.merge.exec-correlation+8]
        // A node leaving a terminal state (I-094 reprobe → Queued,
        // I-047 stale-output reset → Ready/Queued) is starting a
        // fresh lifecycle. The terminal's epilogue already stamped the
        // prior execution's row and bd.exec_id correlation; carrying its
        // exec_id forward makes `exec_id_for_terminal` attribute that
        // finalized execution to whatever build next terminates the
        // reset node — a build that never observed it. Same contract as
        // `reset_to_ready`'s clear, applied at the chokepoint every
        // terminal-exit carve-out goes through (including the currently
        // uncalled Poisoned → Created one).
        if from.is_terminal() && !to.is_terminal() {
            self.exec_id = None;
            self.open_attempt_kind = None;
        }

        from
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
        self.open_attempt_kind = None;
        Ok(())
    }

    /// Kinded attempt-release reset (A2.5, merged_bug_318): the
    /// chokepoint every requeue-after-attempt goes through.
    ///
    /// - `Build` delegates to [`Self::reset_to_ready`] BYTE-IDENTICALLY
    ///   (build mints admit from `Ready` alone, so `Ready` is always
    ///   the correct release target).
    /// - `Materialization` targets the DEP-DERIVED status the node
    ///   promises everywhere else: `Ready` iff `deps_completed`, else
    ///   `Queued` (the PD-6 dep-racing claim was admitted from
    ///   `Queued`; forcing `Ready` made the node from-source
    ///   dispatchable against inputs that do not exist).
    ///
    /// Returns the released-to status for the caller's persist.
    pub fn reset_after_attempt(
        &mut self,
        kind: AttemptKind,
        deps_completed: bool,
    ) -> Result<DerivationStatus, TransitionError> {
        if kind == AttemptKind::Build {
            self.reset_to_ready()?;
            return Ok(DerivationStatus::Ready);
        }
        let target = if deps_completed {
            DerivationStatus::Ready
        } else {
            DerivationStatus::Queued
        };
        fn step(
            this: &mut DerivationState,
            to: DerivationStatus,
            kind: AttemptKind,
        ) -> Result<(), TransitionError> {
            let from = this.status;
            from.validate_transition_for_release(to, kind)?;
            this.apply_validated_transition(from, to);
            Ok(())
        }
        match self.status {
            DerivationStatus::Assigned => step(self, target, kind)?,
            DerivationStatus::Running => {
                step(self, DerivationStatus::Failed, kind)?;
                step(self, target, kind)?;
            }
            _ => {
                return Err(TransitionError::Invalid {
                    from: self.status,
                    to: target,
                    reason: "reset_after_attempt only valid from Assigned or Running",
                });
            }
        }
        self.assigned_executor = None;
        self.exec_id = None;
        self.open_attempt_kind = None;
        Ok(target)
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

    // r[impl sched.merge.poisoned-resubmit-bounded+4]
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
    /// (`dag::merge`) and persisted as the `resubmit_reset` attempt-
    /// ledger row so the bound accumulates across re-submissions and
    /// survives leader failover. At/above the
    /// limit, 24h TTL or `ClearPoison` are the only overrides.
    pub fn is_retriable_on_resubmit(&self) -> bool {
        self.status.is_retriable_on_resubmit()
            || (self.status == DerivationStatus::Poisoned
                && self.retry.resubmit_cycles < POISON_RESUBMIT_RETRY_LIMIT)
    }

    /// Append one committed attempt-ledger row's in-memory mirror to
    /// this node's attempt history. Call ONLY after the owning appending
    /// transaction has committed (the ledger is PG-authoritative; an
    /// uncommitted row must not be visible here).
    pub(crate) fn push_attempt_record(&mut self, record: AttemptRecord) {
        self.attempt_history.push(record);
    }

    /// Replace the in-memory attempt history wholesale. Recovery-load
    /// only (the suffix loaded from `drv_attempts` after a failover).
    pub(crate) fn set_attempt_history(&mut self, history: Vec<AttemptRecord>) {
        self.attempt_history = history;
    }

    /// Mirror a successful reason-only fill (the unified pod-terminal
    /// report's second installment) onto the in-memory record for
    /// `exec_id`: sets `termination_reason` if it is still empty and
    /// touches nothing else (class and floor flags keep the values the
    /// classifying append wrote; `source_node` is adopted only when the
    /// record does not already carry one — mirroring the
    /// `COALESCE(source_node, …)` the durable fill applies). Returns
    /// whether a record was updated.
    pub(crate) fn fill_attempt_termination_reason(
        &mut self,
        exec_id: Uuid,
        termination_reason: &str,
        source_node: Option<&str>,
    ) -> bool {
        for record in self.attempt_history.iter_mut().rev() {
            if record.exec_id == Some(exec_id) {
                if record.termination_reason.is_none() {
                    record.termination_reason = Some(termination_reason.to_string());
                    if record.source_node.is_none() {
                        record.source_node = source_node.map(str::to_owned);
                    }
                    return true;
                }
                return false;
            }
        }
        false
    }

    /// The in-memory attempt history (the committed suffix mirror).
    /// Consulted by the Phase-1b collapsed sites' uncommitted-merge
    /// fallback (no `derivations` row to read the suffix from yet) and,
    /// from T-1b.13, by the fold-derived dispatch-time view; the
    /// authoritative read for a verdict stays the appending
    /// transaction's suffix SELECT.
    pub(crate) fn attempt_history(&self) -> &[AttemptRecord] {
        &self.attempt_history
    }

    /// The node-keyed exclusion set (AD2): every
    /// `source_node` carried by an attempt record in the current
    /// suffix that the fold actually excluded (it appears in the
    /// cached `failed_builders` view — which since decision P12 is
    /// keyed by source node only). This is
    /// what the spawn intent advertises as `excluded_nodes`;
    /// identity-less rows contribute nothing, so an unattributed
    /// history yields an empty list.
    pub(crate) fn excluded_source_nodes(&self) -> Vec<String> {
        let mut nodes: Vec<String> = self
            .attempt_history
            .iter()
            .filter_map(|r| r.source_node.as_deref())
            .filter(|n| self.retry.failed_builders.contains(&ExecutorId::from(*n)))
            .map(str::to_owned)
            .collect();
        nodes.sort_unstable();
        nodes.dedup();
        nodes
    }

    // r[impl sched.retry.recovery-projection+3]
    // r[impl sched.retry.failover-budget]
    /// Rebuild the in-memory retry view from the attempt-ledger fold
    /// over [`Self::attempt_history`]. Recovery calls this
    /// once per loaded node after the suffix load, so the recovered
    /// view is the same fold the live appending transactions
    /// compute — budgets and the placement exclusion survive a leader
    /// failover (`sched.retry.failover-budget`) instead of the
    /// pre-ledger selective forgiveness.
    ///
    /// `poisoned_at` (and the poisoned status) stay derivations-row
    /// owned (`sched.poison.ttl-persist`): whatever the row constructor
    /// set is preserved, not overwritten by the fold's approximation.
    /// `backoff_until` IS adopted from the fold here — recovery has no
    /// in-memory pacing state to preserve, and restoring the
    /// deterministic deadline keeps the pre-failover backoff honored
    /// after a leader change. The live refresh after an append uses
    /// [`Self::refresh_retry_view_from_ledger`] instead, which preserves
    /// the actor-managed (jittered) value.
    pub(crate) fn rebuild_retry_view_from_ledger(
        &mut self,
        budget: &crate::retry_policy::Budget,
        now_epoch_secs: crate::retry_policy::AbsTime,
    ) {
        let decision = crate::retry_policy::decide(&self.attempt_history, budget, now_epoch_secs);
        let c = decision.counters;
        let now = Instant::now();
        // Epoch-seconds → Instant conversions. Past timestamps clamp at
        // the process epoch when the node booted more recently than the
        // event (Instant cannot represent times before boot): the
        // clamped anchor reads as "just now", which errs on the strict
        // side (the 300 s window has not elapsed) and self-corrects at
        // the next counted event.
        let to_past_instant = |t: crate::retry_policy::AbsTime| -> Instant {
            now.checked_sub(Duration::from_secs(now_epoch_secs.saturating_sub(t)))
                .unwrap_or(now)
        };
        self.retry = RetryState {
            count: c.count,
            resubmit_cycles: c.resubmit_cycles,
            infra_count: c.infra_count,
            timeout_count: c.timeout_count,
            last_infra_failure_at: c.last_infra_failure_at.map(to_past_instant),
            exempt_infra_count: c.exempt_infra_count,
            failed_builders: c.failed_builders.iter().cloned().map(Into::into).collect(),
            failure_count: c.failure_count,
            poisoned_at: self.retry.poisoned_at,
            backoff_until: c.backoff_until.map(|t| {
                if t > now_epoch_secs {
                    now + Duration::from_secs(t - now_epoch_secs)
                } else {
                    to_past_instant(t)
                }
            }),
        };
    }

    /// Refresh the cached dispatch view (`self.retry`) from the
    /// attempt-ledger fold after the in-memory attempt history changed
    /// (an append or a
    /// two-installment classification committed). Same computation as
    /// [`Self::rebuild_retry_view_from_ledger`], but the actor-managed
    /// `backoff_until` carve-out is preserved instead of being replaced
    /// by the fold's deterministic (no-jitter) deadline: the production
    /// jitter is applied at the failure site and the clear-on-dispatch
    /// has no ledger event to fold from, so the live pacing value must
    /// not be clobbered by a refresh. (`poisoned_at` is preserved by the
    /// rebuild itself.)
    pub(crate) fn refresh_retry_view_from_ledger(
        &mut self,
        budget: &crate::retry_policy::Budget,
        now_epoch_secs: crate::retry_policy::AbsTime,
    ) {
        let backoff_until = self.retry.backoff_until;
        self.rebuild_retry_view_from_ledger(budget, now_epoch_secs);
        self.retry.backoff_until = backoff_until;
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
/// **The conservative-absent arm (T-D2.3/PD-D5, DQ-2):** a live
/// interested build with NO `wanted_by_build` entry (the legacy shape:
/// a build whose contributions predate `build_wanted_outputs`, or whose
/// rows were purged) contributes `{}` — ALL declared outputs — which
/// SATURATES the union to maximal width. Never a narrower
/// stale snapshot, never a vacuous set; divergence from the exact union is strictly in
/// the widening direction. The degradation is observable:
/// [`note_wanted_width_saturated`] (counter + rate-limited warn).
///
/// Returns `None` only when the node has zero live interested builds
/// (all terminal, missing from `builds`, or an empty interest set —
/// recovered orphans): callers treat that as all-declared too (the
/// conservative branch).
///
/// `Some(vec![])` means "ALL declared outputs wanted": a live build
/// contributed the empty all-wanted sentinel (or the conservative
/// arm fired), which saturates the union (same algebra as
/// [`union_wanted_saturating`]).
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
        // The conservative-absent arm: a live interested build with an
        // unknown contribution saturates the union to all-declared
        // (MAXIMAL width — never under-counted, never the stored
        // union). Observable via counter + rate-limited warn.
        let Some(contribution) = state.wanted_by_build.get(build_id) else {
            note_wanted_width_saturated(build_id);
            return Some(Vec::new());
        };
        match &mut effective {
            None => effective = Some(contribution.clone()),
            Some(acc) => union_wanted_saturating(acc, contribution),
        }
    }
    effective
}

/// DQ-2 observability: the conservative-absent arm fired — a live
/// build's wanted contributions are unknown and the effective width
/// degraded to all-declared. Counter always; warn rate-limited (the
/// arm can fire per classification pass).
pub fn note_wanted_width_saturated(build_id: &Uuid) {
    metrics::counter!("rio_scheduler_wanted_width_saturated_total").increment(1);
    use std::sync::atomic::{AtomicI64, Ordering};
    static LAST_WARN_EPOCH: AtomicI64 = AtomicI64::new(0);
    let now = std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs() as i64)
        .unwrap_or(0);
    let last = LAST_WARN_EPOCH.load(Ordering::Relaxed);
    if now - last >= 10
        && LAST_WARN_EPOCH
            .compare_exchange(last, now, Ordering::Relaxed, Ordering::Relaxed)
            .is_ok()
    {
        tracing::warn!(
            build_id = %build_id,
            "wanted contributions unknown for live build; degrading to \
             all-declared width (rate-limited; see \
             rio_scheduler_wanted_width_saturated_total)"
        );
    }
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
/// (the `resubmit_reset` ledger row — the `M_051` mirror column was
/// dropped by `M_075`) so this accumulates across re-submissions and
/// scheduler restarts: two poison cycles before the node sticks. See
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
        fn wanted_paths<'a>(s: &'a DerivationState, wanted: &'a [String]) -> Vec<&'a String> {
            wanted_subset(&s.output_names, &s.expected_output_paths, wanted).collect()
        }

        assert_eq!(
            wanted_paths(&s, &[]),
            vec![
                "/nix/store/aaaa-glibc",
                "/nix/store/bbbb-glibc-dev",
                "/nix/store/cccc-glibc-debug"
            ],
            "empty wanted set must mean ALL declared outputs"
        );

        assert_eq!(
            wanted_paths(&s, &["out".into(), "dev".into()]),
            vec!["/nix/store/aaaa-glibc", "/nix/store/bbbb-glibc-dev"],
            "wanted subset must exclude the unwanted -debug path"
        );

        // A wanted name with no matching declared output (defensive) is ignored.
        assert_eq!(
            wanted_paths(&s, &["out".into(), "nonexistent".into()]),
            vec!["/nix/store/aaaa-glibc"]
        );
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
        terminal.transition_terminal(crate::state::SettledBuild {
            counts: crate::state::SettledCounts {
                total: 0,
                completed: 0,
                cached: 0,
                failed: 0,
            },
            outcome: crate::state::TerminalOutcome::Succeeded {
                output_paths: vec![],
            },
        })?;
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

        // 4. A live build whose contribution is unknown SATURATES the
        //    union to all-declared (the conservative-absent arm,
        //    T-D2.3/PD-D5/DQ-2 — maximal width, never the stored
        //    union), even though another live build's contribution is
        //    known.
        s.interested_builds = [live_out, live_unknown].into();
        s.wanted_by_build = [(live_out, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            Some(vec![]),
            "an unknown contribution of a live build saturates to all-declared"
        );

        // 5. Zero live interested builds → None (callers treat as
        //    all-declared — the conservative branch).
        // 5a. Only a terminal build is interested.
        s.interested_builds = [done].into();
        s.wanted_by_build = [(done, vec!["out".to_string()])].into();
        assert_eq!(
            effective_wanted(&s, &builds),
            None,
            "no live interested builds → None (callers degrade to all-declared)"
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
            (Assigned, Cancelled), // CancelSignal before worker ACK
            (Running, Cancelled),  // CancelSignal mid-build (cgroup.kill)
            (Queued, Skipped),     // CA early-cutoff cascade
            (Ready, Skipped),      // CA cutoff after find_newly_ready promoted
        ];

        for (from, to) in valid_transitions {
            assert!(
                from.validate_transition(to).is_ok(),
                "expected {from} -> {to} to be valid"
            );
        }
    }

    /// The decode boundary rejects unknown literals (the PD-D3
    /// transitional 'substituting' arm was removed with migration 080
    /// — the state is unrepresentable post-080, so the legacy literal
    /// is an unknown like any other now).
    #[test]
    fn unknown_status_literal_errors() {
        assert!("bogus-status".parse::<DerivationStatus>().is_err());
        assert!("substituting".parse::<DerivationStatus>().is_err());
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
        // Cancel from NON-dispatched states: invalid for build-cancel
        // orphans (Queued/Created are just removed from the ready
        // queue, not transitioned). Ready IS allowed — the
        // controller-observed timeout cap (DeadlineExceeded backstop)
        // takes its terminal Cancelled transition from Ready because
        // the disconnect already re-queued the node (the Cancelled
        // counterpart of the Ready→Poisoned fleet-exhaust edge).
        assert!(Queued.validate_transition(Cancelled).is_err());
        assert!(Ready.validate_transition(Cancelled).is_ok());
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
        // PD-6 (Phase B): the kind-blind table keeps rejecting
        // Queued→Assigned (the line above is the as-built pin); the
        // MATERIALIZATION-MINT kinded form accepts exactly that edge —
        // and only for the materialization kind (build mints stay
        // byte-identical to the kind-blind table).
        assert!(
            Queued
                .validate_transition_for_mint(Assigned, AttemptKind::Materialization)
                .is_ok()
        );
        assert!(
            Queued
                .validate_transition_for_mint(Assigned, AttemptKind::Build)
                .is_err()
        );
        assert!(Queued.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Running).is_err());
        // ready -> completed IS valid (FOD output already in store; I-067)
        assert!(Ready.validate_transition(Completed).is_ok());

        // Running -> Ready is NOT valid (must go through Failed)
        assert!(Running.validate_transition(Ready).is_err());
        assert!(Running.validate_transition(Queued).is_err());
        assert!(Running.validate_transition(Assigned).is_err());

        // Failed can go to Ready (retry), Queued (I-094 deferred), or
        // Completed (I-094/I-099 re-probe cache-hit; symmetry-only —
        // Failed is reset by dag.merge today, but the state machine
        // and the merge.rs reprobe callers must agree).
        assert!(Failed.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Completed).is_ok());
        assert!(Failed.validate_transition(Queued).is_ok());

        // Poisoned can go to Created (TTL), Queued (I-094 deferred),
        // Completed (I-094 reprobe), or stay Poisoned.
        // NOT Ready (must gate on dep), Running, Failed.
        assert!(Poisoned.validate_transition(Ready).is_err());
        assert!(Poisoned.validate_transition(Running).is_err());
        assert!(Poisoned.validate_transition(Failed).is_err());
        assert!(Poisoned.validate_transition(Queued).is_ok());

        // DependencyFailed is terminal: can't go anywhere except
        // self/Completed/Queued (re-probe lanes).
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
    /// Failed→Completed symmetry: `existing_reprobe` includes
    /// `Failed`, and `apply_cached_hits` attempts the transition on a
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
        // Parallel: the existing I-094 lanes these mirror.
        assert!(Poisoned.validate_transition(Completed).is_ok());
        assert!(DependencyFailed.validate_transition(Completed).is_ok());
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
            elapsed_secs: 100.0,
            is_fixed_output: false,
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
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            // is_ca=true: this IS a CA derivation, so the flag
            // mattered pre-restart. Prove it's false post-restart
            // regardless.
            is_ca: true,
            floor_mem_bytes: 0,
            floor_disk_bytes: 0,
            floor_deadline_secs: 0,
            exec_id: None,
            attempt_kind: None,
        };
        let state = DerivationState::from_recovery_row(row, DerivationStatus::Queued).unwrap();
        assert!(state.ca.is_ca, "precondition: recovered as CA");
        assert!(
            !state.ca.output_unchanged,
            "recovery MUST reset ca_output_unchanged to false (NOT persisted — \
             compare result from a prior scheduler instance is stale)"
        );
    }

    // r[verify sched.merge.exec-correlation+8]
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
            (Poisoned, Queued),         // I-094 deferred reprobe
            (DependencyFailed, Queued), // I-094 deferred reprobe
            (Completed, Ready),         // I-047 stale-output reset
            (Skipped, Queued),          // I-047 stale-output reset
            (Poisoned, Created),        // carve-out only, no live caller
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

    // r[verify sched.state.machine+2]
    /// Exhaustive (from, to) cartesian product over all 11 states.
    /// Every pair is explicitly Ok or Err — a mutant that flips ONE
    /// arm's outcome breaks exactly one assertion. Complements
    /// `test_derivation_valid_transitions` (positive list) and
    /// `test_derivation_invalid_transitions` (negative samples) with
    /// full-coverage table: 11×11 = 121 cases (the count is true
    /// again now the walk-era Substituting variant is gone).
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
            // Timeout-cap terminal observed by the controller backstop
            // after the disconnect already re-queued (D1 / the
            // deadline-exceeded rule's Cancelled-at-cap clause).
            (Ready, Cancelled),
            // CA early-cutoff
            (Queued, Skipped),
            (Ready, Skipped),
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

    // r[verify sched.state.machine+2]
    /// Exhaustive (from, to, kind) product over all 12 statuses × 2
    /// attempt kinds for the KINDED mint validation (PD-6, Phase B):
    /// the kinded form agrees with the kind-blind table on EVERY cell
    /// except exactly one — (Queued, Assigned, Materialization), the
    /// dep-racing materialization claim edge. The as-built exhaustive
    /// test above stays untouched (it pins the kind-blind table, which
    /// is byte-identical); this test pins the wrapper's delta to that
    /// single cell so a future edit that widens the kinded form breaks
    /// here, not in production.
    #[test]
    fn validate_transition_for_mint_exhaustive() {
        use DerivationStatus as S;
        for &from in S::ALL {
            for &to in S::ALL {
                let blind = from.validate_transition(to).is_ok();
                for kind in [AttemptKind::Build, AttemptKind::Materialization] {
                    let kinded = from.validate_transition_for_mint(to, kind).is_ok();
                    let is_delta_cell = from == S::Queued
                        && to == S::Assigned
                        && kind == AttemptKind::Materialization;
                    if is_delta_cell {
                        assert!(
                            kinded,
                            "(Queued, Assigned, Materialization) is the ONE legal kinded delta"
                        );
                        assert!(
                            !blind,
                            "the kind-blind table must keep rejecting Queued -> Assigned"
                        );
                    } else {
                        assert_eq!(
                            kinded, blind,
                            "kinded({from} -> {to}, {kind:?}) must agree with the kind-blind \
                             table everywhere except the single delta cell"
                        );
                    }
                }
            }
        }
    }

    // r[verify sched.state.machine+2]
    /// The RELEASE mirror of the mint table (A2.5, merged_bug_318):
    /// exhaustive (from, to, kind) product — the kinded release agrees
    /// with the kind-blind table on EVERY cell except exactly one,
    /// (Assigned, Queued, Materialization): the dep-racing claim's
    /// return edge (Failed → Queued is already blind-legal — the
    /// resubmit edge). A build release never has a dep-blocked target
    /// (build mints admit from Ready alone), so the build column is
    /// byte-identical everywhere.
    #[test]
    fn validate_transition_for_release_exhaustive() {
        use DerivationStatus as S;
        for &from in S::ALL {
            for &to in S::ALL {
                let blind = from.validate_transition(to).is_ok();
                for kind in [AttemptKind::Build, AttemptKind::Materialization] {
                    let kinded = from.validate_transition_for_release(to, kind).is_ok();
                    let is_delta_cell = from == S::Assigned
                        && to == S::Queued
                        && kind == AttemptKind::Materialization;
                    if is_delta_cell {
                        assert!(
                            kinded,
                            "(Assigned, Queued, Materialization) is the ONE legal kinded release delta"
                        );
                        assert!(
                            !blind,
                            "the kind-blind table must keep rejecting Assigned -> Queued"
                        );
                    } else {
                        assert_eq!(
                            kinded, blind,
                            "release({from} -> {to}, {kind:?}) must agree with the kind-blind                              table everywhere except the two delta cells"
                        );
                    }
                }
            }
        }
    }

    /// `reset_after_attempt` decision table (A2.5): Build delegates to
    /// `reset_to_ready` byte-identically; Materialization targets the
    /// dep-derived status — Ready iff deps completed, else Queued —
    /// from both Assigned and Running (via Failed), clearing the
    /// attempt bookkeeping either way.
    #[test]
    fn reset_after_attempt_table() {
        use DerivationStatus as S;
        let mk = |status: S| {
            let mut st = DerivationState::try_from_node(&dummy_node()).expect("dummy node");
            st.set_status_for_test(status);
            st.assigned_executor = Some("w".into());
            st.exec_id = Some(uuid::Uuid::now_v7());
            st.open_attempt_kind = Some(AttemptKind::Build);
            st
        };
        for (from, kind, deps, want) in [
            (S::Assigned, AttemptKind::Build, false, S::Ready),
            (S::Assigned, AttemptKind::Build, true, S::Ready),
            (S::Running, AttemptKind::Build, false, S::Ready),
            (S::Assigned, AttemptKind::Materialization, true, S::Ready),
            (S::Assigned, AttemptKind::Materialization, false, S::Queued),
            (S::Running, AttemptKind::Materialization, true, S::Ready),
            (S::Running, AttemptKind::Materialization, false, S::Queued),
        ] {
            let mut st = mk(from);
            let got = st.reset_after_attempt(kind, deps).expect("legal release");
            assert_eq!(got, want, "({from}, {kind:?}, deps={deps})");
            assert_eq!(st.status(), want);
            assert!(st.assigned_executor.is_none() && st.exec_id.is_none());
            assert!(st.open_attempt_kind.is_none());
        }
        // Illegal sources reject for both kinds.
        for kind in [AttemptKind::Build, AttemptKind::Materialization] {
            assert!(mk(S::Ready).reset_after_attempt(kind, true).is_err());
            assert!(mk(S::Completed).reset_after_attempt(kind, true).is_err());
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
            elapsed_secs: f64::INFINITY,
            is_fixed_output: false,
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
}

#[cfg(test)]
mod status_snapshot {
    //! Cross-language DerivationStatus enforcement. The golden file at
    //! `rio-scheduler/tests/golden/derivation_statuses.json` is the single
    //! source of truth — both this Rust-side snapshot test AND
    //! rio-dashboard's vitest (`graphLayout.test.ts` cross-language
    //! describe block) compare against it. A new variant added here
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
    /// snapshot that rio-dashboard's vitest also reads. Adding a
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
    /// (or any new variant)
    /// without an `ALL` entry compiles (arrays don't enforce
    /// exhaustiveness) — this exhaustive match forces a compile error
    /// on the new variant, and the .len() assert catches the inverse
    /// (an `ALL` entry without an enum variant is already a compile
    /// error, so this direction is belt-and-braces).
    #[test]
    fn all_const_is_exhaustive() {
        // Exhaustive match: adding a new variant without a match arm
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
                DerivationStatus::Completed => 5,
                DerivationStatus::Failed => 6,
                DerivationStatus::Poisoned => 7,
                DerivationStatus::DependencyFailed => 8,
                DerivationStatus::Cancelled => 9,
                DerivationStatus::Skipped => 10,
            }
        }
        assert_eq!(DerivationStatus::ALL.len(), 11);
        // Each ALL[i] round-trips through the witness at its own index.
        // Catches accidental duplicates or order drift (the golden
        // expects ALL's order).
        for (i, s) in DerivationStatus::ALL.iter().enumerate() {
            assert_eq!(_witness(*s), i, "ALL[{i}]={s} at wrong index");
        }
    }
}
