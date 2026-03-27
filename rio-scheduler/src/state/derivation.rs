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

use std::collections::HashSet;
use std::time::Instant;

use uuid::Uuid;

use super::{DrvHash, ExecutorId, TransitionError};

// r[impl sched.state.machine]
/// State of a single derivation in the global DAG.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Hash)]
pub enum DerivationStatus {
    Created,
    Queued,
    Ready,
    Assigned,
    Running,
    Completed,
    Failed,
    Poisoned,
    /// A dependency of this derivation failed/poisoned. This derivation can
    /// never complete in the current build. Terminal (like Poisoned).
    /// Maps to Nix BuildStatus::DependencyFailed=10.
    DependencyFailed,
    /// Explicitly cancelled via CancelBuild (all interested builds cancelled)
    /// or DrainExecutor(force). Terminal but distinct from Poisoned: no
    /// implication of build defect, just scheduler/operator decision.
    /// No TTL reset — a cancelled build stays cancelled; retry means
    /// re-submitting. Worker's cgroup.kill SIGKILLs the daemon tree,
    /// cleanup is immediate (no 2h terminationGracePeriodSeconds wait).
    Cancelled,
    /// Terminal. CA early-cutoff: a CA dependency completed with
    /// byte-identical output (content-index match), so this derivation
    /// would produce the same output as already in the store. Skipped
    /// without running. Distinct from Completed for metrics
    /// (`rio_scheduler_ca_cutoff_saves_total`) and audit trail.
    /// Queued|Ready → Skipped (Ready is order-independent vs
    /// `find_newly_ready` — matches DependencyFailed precedent).
    Skipped,
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
    /// `Cancelled` doc-comment: "retry means re-submitting". Without reset,
    /// a `Cancelled` node stuck in the DAG (reap misses it —
    /// `cancel_build_derivations` removes interest BEFORE
    /// `remove_build_interest_and_reap`'s `was_interested` check) makes the
    /// resubmitted build hang: `merge()` adds interest but
    /// `compute_initial_states` only iterates `newly_inserted`.
    ///
    /// `Failed`: transient-fail with no retry driver pending — resubmit retries.
    ///
    /// `DependencyFailed`: derived state — reset lets `compute_initial_states`
    /// re-evaluate `any_dep_terminally_failed` fresh. If the dep is still
    /// `Poisoned`, it goes back to `DependencyFailed` (same fast-fail). If the
    /// dep was `Cancelled` (reset by this same merge), it goes `Queued`/`Ready`.
    ///
    /// NOT retriable: `Completed` (cache hit), `Poisoned` (failed on 3+
    /// workers, 24h TTL is the safety valve — use ClearPoison to override).
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

        // Terminal -> non-terminal is rejected (except poisoned -> created via TTL)
        if self.is_terminal() && !to.is_terminal() {
            if self == Self::Poisoned && to == Self::Created {
                // Allowed: 24h TTL expiry resets poisoned -> created
                return Ok(());
            }
            return Err(TransitionError::TerminalToNonTerminal { from: self, to });
        }

        // Valid transitions
        let valid = match (self, to) {
            (Self::Created, Self::Completed) => true,       // cache hit
            (Self::Created, Self::Queued) => true,          // build accepted
            (Self::Queued, Self::Ready) => true,            // all deps complete
            (Self::Queued, Self::DependencyFailed) => true, // dep poisoned, cascade
            (Self::Ready, Self::DependencyFailed) => true,  // dep poisoned, cascade
            (Self::Created, Self::DependencyFailed) => true, // dep poisoned before queue
            (Self::Ready, Self::Assigned) => true,          // worker selected
            (Self::Assigned, Self::Running) => true,        // worker ack
            (Self::Assigned, Self::Ready) => true,          // worker lost
            (Self::Running, Self::Completed) => true,       // build succeeded
            (Self::Running, Self::Failed) => true,          // retriable failure
            (Self::Running, Self::Poisoned) => true,        // failed on 3+ workers
            (Self::Failed, Self::Ready) => true,            // retry scheduled
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

    /// Convert to the wire/database string representation.
    pub fn as_str(self) -> &'static str {
        match self {
            Self::Created => "created",
            Self::Queued => "queued",
            Self::Ready => "ready",
            Self::Assigned => "assigned",
            Self::Running => "running",
            Self::Completed => "completed",
            Self::Failed => "failed",
            Self::Poisoned => "poisoned",
            Self::DependencyFailed => "dependency_failed",
            Self::Cancelled => "cancelled",
            Self::Skipped => "skipped",
        }
    }

    /// All variants, in the order the golden snapshot at
    /// `rio-test-support/golden/derivation_statuses.json` lists them.
    /// Used by the snapshot test (`status_snapshot` mod below), the
    /// exhaustive transition-table test, and indirectly by the
    /// dashboard's cross-language cardinality check (vitest reads the
    /// same golden). `cfg(test)` because the const itself is only
    /// useful where exhaustiveness matters — production code matches
    /// on the enum directly.
    #[cfg(test)]
    pub const ALL: [Self; 11] = [
        Self::Created,
        Self::Queued,
        Self::Ready,
        Self::Assigned,
        Self::Running,
        Self::Completed,
        Self::Failed,
        Self::Poisoned,
        Self::DependencyFailed,
        Self::Cancelled,
        Self::Skipped,
    ];
}

impl std::fmt::Display for DerivationStatus {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(self.as_str())
    }
}

impl std::str::FromStr for DerivationStatus {
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
    /// Target system (e.g. "x86_64-linux").
    pub system: String,
    /// Required system features the building worker must support.
    pub required_features: Vec<String>,
    /// Output names (e.g. ["out", "dev"]).
    pub output_names: Vec<String>,
    /// Whether this is a fixed-output derivation (fetchurl, etc.).
    pub is_fixed_output: bool,
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
    /// `None` for IA derivations AND for the `single_node_from_basic`
    /// fallback (no transitive closure to compute over).
    ///
    /// Consumed by:
    /// - `collect_ca_inputs` ([`crate::actor`] dispatch) — this node
    ///   as a CA INPUT of a parent; `None` → skip, parent's resolve
    ///   is incomplete → worker fails on placeholder → retry.
    /// - `handle_success_completion` — this node's own
    ///   `(modular_hash, output_name)` for the `realisation_deps`
    ///   insert (the PARENT side of the junction).
    pub ca_modular_hash: Option<[u8; 32]>,
    /// Realisation lookups from dispatch-time resolve. Consumed by
    /// `handle_success_completion` → `insert_realisation_deps` AFTER
    /// the parent's own realisation lands (the FK needs the parent's
    /// row in `realisations` to exist, which only happens post-build
    /// via `wopRegisterDrvOutput` — see resolve.rs's FK-ordering doc).
    ///
    /// Empty for IA derivations and for CA derivations whose resolve
    /// was a no-op (no CA inputs). Populated by `maybe_resolve_ca` in
    /// the dispatch path; consumed + drained at completion time.
    /// In-memory only: recovered derivations lose this (same lossy
    /// class as `ca_modular_hash`); the `realisation_deps` rows are
    /// best-effort cache, not correctness.
    pub pending_realisation_deps: Vec<crate::ca::RealisationLookup>,
    /// CA cutoff-compare result: true iff EVERY output's nar_hash
    /// matched the content index on completion. Set by
    /// `handle_success_completion` (`r[sched.ca.cutoff-compare]`);
    /// consumed by `cascade_cutoff` via `find_cutoff_eligible_speculative` (`r[sched.ca.cutoff-propagate]`,
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
    /// The window is tight: P0252's cascade runs in the SAME
    /// `handle_success_completion` call as the set (at
    /// `completion.rs:470`), so in practice the compare→propagate
    /// gap is a single actor-tick iteration, not a multi-second
    /// span. A restart in that window would have to land between
    /// line :448 and :476 — possible but rare. Persisting the flag
    /// (migration + persist_status touch) is not warranted for an
    /// optimization-only field with a zero-tick consumption window.
    pub ca_output_unchanged: bool,
    /// Current state machine status. Private: mutate only via `transition()`
    /// or `reset_to_ready()` to preserve invariants.
    status: DerivationStatus,
    /// Set of build IDs interested in this derivation.
    pub interested_builds: HashSet<Uuid>,
    /// Worker currently assigned/running this derivation.
    pub assigned_executor: Option<ExecutorId>,
    /// Size-class this derivation was dispatched to. Recorded at
    /// assign time so completion can check misclassification (actual
    /// duration > 2× the class cutoff). `None` = size-classes not
    /// configured, or never assigned.
    pub assigned_size_class: Option<String>,
    /// ATerm-serialized .drv content, inlined by the gateway for
    /// nodes that will actually dispatch (outputs missing from store).
    /// Empty = worker fetches from store via GetPath (fallback
    /// path, still works). Forwarded verbatim into WorkAssignment.
    /// ≤256 KB bound enforced at gRPC ingress.
    pub drv_content: Vec<u8>,
    /// Number of retry attempts so far.
    pub retry_count: u32,
    /// Number of InfrastructureFailure re-dispatches so far. Separate
    /// from `retry_count` because infra failures don't count toward
    /// the transient-failure budget (they're worker-local, not
    /// build-local) — but still bounded to prevent a misclassified
    /// deterministic failure from hot-looping forever. In-memory only:
    /// recovery resets to 0 (conservative — won't spuriously poison
    /// after restart).
    pub infra_retry_count: u32,
    /// Workers that have failed building this derivation. Drives
    /// `best_executor()` exclusion + poison threshold in distinct mode.
    pub failed_builders: HashSet<ExecutorId>,
    /// Total TransientFailure/disconnect count (same-worker repeats
    /// counted). Drives poison threshold when
    /// `PoisonConfig::require_distinct_workers = false` (single-worker
    /// dev deployments). In-memory only: recovery initializes to
    /// `failed_builders.len()` — same-worker repeats are "forgiven"
    /// across restart, which is conservative (won't spuriously poison).
    /// InfrastructureFailure does NOT increment this (T1's split).
    pub failure_count: u32,
    /// When the derivation entered the poisoned state (for TTL expiry).
    pub poisoned_at: Option<Instant>,
    /// Realized output store paths (filled on completion).
    pub output_paths: Vec<String>,
    /// Expected output paths (from the proto node at merge time).
    /// Used for: cache-check (merge.rs), transfer-cost scoring, and
    /// as the closure approximation for locality (children's
    /// expected_output_paths = parent's inputs).
    pub expected_output_paths: Vec<String>,
    /// Estimated build duration (from Estimator). Set at merge time;
    /// never updated after. The critical-path priority uses this;
    /// stale is fine (a build taking longer than estimated doesn't
    /// change the OPTIMAL schedule mid-execution — what's queued is
    /// queued).
    pub est_duration: f64,
    /// Sum of input_srcs nar_sizes from the proto. Passed to
    /// `Estimator::estimate()` for the closure-size-as-proxy fallback
    /// when there's no `build_history` entry. 0 = no-signal (empty
    /// srcs, or gateway's QueryPathInfo batch failed).
    ///
    /// Stored separately from `est_duration` even though it's only
    /// USED to compute est_duration: the estimator call happens in
    /// merge.rs AFTER try_from_node (estimator not in scope here),
    /// and merge.rs needs the raw value to pass through.
    pub input_srcs_nar_size: u64,
    /// Critical-path priority: `est_duration + max(children's priority)`.
    /// Bottom-up: leaves have `priority = est_duration`; roots have
    /// the sum along the longest path. Higher = more urgent (dispatch
    /// first). Recomputed incrementally on completion via
    /// ancestor-walk. The ready queue uses this for BinaryHeap ordering.
    pub priority: f64,
    /// Database UUID (set after insertion).
    pub db_id: Option<Uuid>,
    /// When the derivation entered Ready state (for assignment latency metric).
    pub(crate) ready_at: Option<Instant>,
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
    /// When the derivation entered Running state. For the backstop
    /// timeout: handle_tick checks this + est_duration × 3 (clamped
    /// to daemon_timeout + slack). A build that's been Running far
    /// longer than expected is likely stuck (worker heartbeating
    /// but daemon wedged, or the worker's clock jumped).
    pub(crate) running_since: Option<Instant>,
    /// W3C traceparent of the submitting gRPC handler's span, captured
    /// at DAG-merge time. Embedded into `WorkAssignment.traceparent` at
    /// dispatch so the worker's build span chains back to the gateway's
    /// trace regardless of which code path (immediate merge, deferred
    /// completion/heartbeat) triggers dispatch. Empty for recovered
    /// derivations (no user trace). First submitter wins on dedup.
    pub traceparent: String,
}

impl DerivationState {
    /// Create a new derivation state from a proto DerivationNode.
    ///
    /// Validates `node.drv_path` parses as a well-formed `StorePath`. The
    /// gRPC layer also validates upfront (returns INVALID_ARGUMENT), so this
    /// is belt-and-suspenders for when the actor is driven by something
    /// other than gRPC (tests, future admin APIs).
    pub fn try_from_node(
        node: &rio_proto::dag::DerivationNode,
    ) -> Result<Self, rio_nix::store_path::StorePathError> {
        let drv_path = rio_nix::store_path::StorePath::parse(&node.drv_path)?;
        Ok(Self {
            drv_hash: node.drv_hash.as_str().into(),
            drv_path,
            pname: (!node.pname.is_empty()).then(|| node.pname.clone()),
            system: node.system.clone(),
            required_features: node.required_features.clone(),
            output_names: node.output_names.clone(),
            is_fixed_output: node.is_fixed_output,
            // r[impl sched.ca.detect]
            is_ca: node.is_content_addressed,
            needs_resolve: node.needs_resolve,
            // Gateway sends 32 bytes for CA nodes it could compute
            // the modular hash for, empty otherwise (IA, or
            // BasicDerivation fallback with no transitive closure).
            // try_into rejects non-32-byte (including empty) →
            // None. Belt-and-suspenders vs the gateway's own IA
            // gate (populate_ca_modular_hashes skips non-CA).
            ca_modular_hash: node.ca_modular_hash.as_slice().try_into().ok(),
            pending_realisation_deps: Vec::new(),
            ca_output_unchanged: false,
            status: DerivationStatus::Created,
            interested_builds: HashSet::new(),
            assigned_executor: None,
            assigned_size_class: None,
            drv_content: node.drv_content.clone(),
            retry_count: 0,
            infra_retry_count: 0,
            failed_builders: HashSet::new(),
            failure_count: 0,
            poisoned_at: None,
            output_paths: Vec::new(),
            expected_output_paths: node.expected_output_paths.clone(),
            // Placeholder — merge.rs sets this from the Estimator right
            // after try_from_node (try_from_node doesn't have estimator
            // access). 0.0 is a visible "not yet set" marker.
            est_duration: 0.0,
            input_srcs_nar_size: node.input_srcs_nar_size,
            priority: 0.0,
            db_id: None,
            ready_at: None,
            backoff_until: None,
            running_since: None,
            traceparent: String::new(),
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
    pub fn from_recovery_row(
        row: crate::db::RecoveryDerivationRow,
        status: DerivationStatus,
    ) -> Result<Self, (String, rio_nix::store_path::StorePathError)> {
        let drv_path = rio_nix::store_path::StorePath::parse(&row.drv_path)
            .map_err(|e| (row.drv_hash.clone(), e))?;
        let now = Instant::now();
        Ok(Self {
            drv_hash: row.drv_hash.into(),
            drv_path,
            pname: row.pname,
            system: row.system,
            required_features: row.required_features,
            output_names: row.output_names,
            is_fixed_output: row.is_fixed_output,
            is_ca: row.is_ca,
            // Lossy on recovery: not persisted. Conservative false →
            // dispatch unresolved → worker fails on placeholder →
            // retry. Same degradation class as ca_modular_hash below.
            needs_resolve: false,
            // Lossy on recovery: not persisted. Recovered CA-on-CA
            // chains dispatch unresolved (collect_ca_inputs skips
            // None) → worker fails on placeholder → retry. Same
            // degradation as drv_content=empty below. The gateway
            // recomputes on the NEXT SubmitBuild that references
            // this derivation (DAG merge sees the fresh proto).
            ca_modular_hash: None,
            pending_realisation_deps: Vec::new(),
            ca_output_unchanged: false,
            status,
            interested_builds: HashSet::new(), // populated by build_derivations join
            assigned_executor: row.assigned_builder_id.map(Into::into),
            assigned_size_class: None, // lossy; misclassification detector skips None
            drv_content: Vec::new(),   // worker fetches from store
            retry_count: row.retry_count.max(0) as u32,
            // In-memory only — recovery resets to 0 (conservative).
            infra_retry_count: 0,
            // failure_count: initialize from failed_builders.len() —
            // same-worker repeats are lost (in-mem only), conservative.
            failure_count: row.failed_builders.len() as u32,
            failed_builders: row.failed_builders.into_iter().map(Into::into).collect(),
            poisoned_at: None, // poisoned rows not loaded; if one slips through, stays poisoned
            output_paths: Vec::new(), // completed rows not loaded
            expected_output_paths: row.expected_output_paths,
            est_duration: 0.0,      // recomputed by full_sweep
            input_srcs_nar_size: 0, // lossy; only used as estimator input (proxy)
            priority: 0.0,          // recomputed by full_sweep
            db_id: Some(row.derivation_id),
            // Instant fields: conservative defaults.
            // ready_at: Some(now) if Ready → assignment_latency
            // metric skews (looks like instant dispatch) but
            // doesn't break anything.
            ready_at: (status == DerivationStatus::Ready).then_some(now),
            // running_since: Some(now) if Running → backstop
            // timeout resets. A build that was 1h into a 2h
            // estimate gets another full 6h backstop. Conservative
            // (won't spuriously cancel) at the cost of a possibly
            // stale build running longer.
            running_since: (status == DerivationStatus::Running).then_some(now),
            backoff_until: None,        // any in-flight backoff is forgiven
            traceparent: String::new(), // recovered: no user trace
        })
    }

    /// Construct from a `PoisonedDerivationRow` during recovery.
    /// Minimal — poisoned rows aren't dispatched, just TTL-tracked.
    /// `elapsed_secs` comes from PG's `EXTRACT(EPOCH FROM (now() -
    /// poisoned_at))` so we compute `poisoned_at = Instant::now() -
    /// Duration::from_secs_f64(elapsed)` — approximate but good enough
    /// for a 24h TTL.
    pub fn from_poisoned_row(
        row: crate::db::PoisonedDerivationRow,
    ) -> Result<Self, (String, rio_nix::store_path::StorePathError)> {
        let drv_path = rio_nix::store_path::StorePath::parse(&row.drv_path)
            .map_err(|e| (row.drv_hash.clone(), e))?;
        let now = Instant::now();
        // Convert PG-computed elapsed seconds back to an Instant.
        // Clamp: negative/NaN → 0 (conservative full TTL), +inf → 1yr
        // (poisoned_at = -infinity::timestamp would make from_secs_f64
        // panic). Follows the `.max(0.0).min(MAX)` pattern from
        // worker.rs:207.
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
        Ok(Self {
            drv_hash: row.drv_hash.into(),
            drv_path,
            pname: row.pname,
            system: row.system,
            required_features: Vec::new(),
            output_names: Vec::new(),
            is_fixed_output: false,
            is_ca: false,
            needs_resolve: false,
            ca_modular_hash: None,
            pending_realisation_deps: Vec::new(),
            ca_output_unchanged: false,
            status: DerivationStatus::Poisoned,
            interested_builds: HashSet::new(),
            assigned_executor: None,
            assigned_size_class: None,
            drv_content: Vec::new(),
            retry_count: 0,
            infra_retry_count: 0,
            failure_count: row.failed_builders.len() as u32,
            failed_builders: row.failed_builders.into_iter().map(Into::into).collect(),
            poisoned_at: Some(poisoned_at),
            output_paths: Vec::new(),
            expected_output_paths: Vec::new(),
            est_duration: 0.0,
            input_srcs_nar_size: 0,
            priority: 0.0,
            db_id: Some(row.derivation_id),
            ready_at: None,
            running_since: None,
            backoff_until: None,
            traceparent: String::new(),
        })
    }

    /// Store path of the .drv file (read-only; DAG owns the reverse index).
    ///
    /// Callers using `&str` auto-deref via `StorePath::Deref<Target=str>`.
    pub fn drv_path(&self) -> &rio_nix::store_path::StorePath {
        &self.drv_path
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

    /// Test-only: directly set status bypassing state machine validation.
    /// For setting up test preconditions where the full transition chain
    /// would be verbose noise.
    #[cfg(test)]
    pub(crate) fn set_status_for_test(&mut self, status: DerivationStatus) {
        self.status = status;
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
/// `size_classes` precedent in `Config`. Serialize + PartialEq are
/// for the TOML-roundtrip tests in main.rs (`assert_eq!(cfg.poison,
/// PoisonConfig::default())`).
#[derive(Debug, Clone, PartialEq, Eq, serde::Serialize, serde::Deserialize)]
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
            state.failed_builders.len() as u32
        } else {
            state.failure_count
        };
        count >= self.threshold
    }
}

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

    fn dummy_node() -> rio_proto::dag::DerivationNode {
        rio_test_support::fixtures::make_derivation_node("h", "x86_64-linux")
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
        assert!(!ia_state.is_ca, "input-addressed drv → is_ca=false");

        // Content-addressed: is_ca = true. Proto field set by the
        // gateway from `is_fixed_output() || has_ca_floating_outputs()`;
        // scheduler doesn't recompute, just propagates.
        let mut ca_node = dummy_node();
        ca_node.is_content_addressed = true;
        let ca_state = DerivationState::try_from_node(&ca_node)?;
        assert!(ca_state.is_ca, "CA drv → is_ca=true propagated from proto");

        // needs_resolve propagates independently of is_ca: the
        // ia.deferred case (IA drv with floating-CA input) has
        // is_ca=false but needs_resolve=true.
        let mut deferred = dummy_node();
        deferred.needs_resolve = true;
        let deferred_state = DerivationState::try_from_node(&deferred)?;
        assert!(
            deferred_state.needs_resolve,
            "needs_resolve=true propagated from proto"
        );
        assert!(
            !deferred_state.is_ca,
            "ia.deferred: needs_resolve independent of is_ca"
        );

        Ok(())
    }

    #[test]
    fn test_derivation_valid_transitions() {
        use DerivationStatus::*;

        let valid_transitions = [
            (Created, Completed),        // cache hit
            (Created, Queued),           // build accepted
            (Queued, Ready),             // all deps complete
            (Ready, Assigned),           // worker selected
            (Assigned, Running),         // worker ack
            (Assigned, Ready),           // worker lost
            (Running, Completed),        // build succeeded
            (Running, Failed),           // retriable failure
            (Running, Poisoned),         // failed on 3+ workers
            (Failed, Ready),             // retry scheduled
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
        // Terminal: no resurrect.
        assert!(Skipped.validate_transition(Created).is_err());
        assert!(Skipped.validate_transition(Ready).is_err());
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

        // Terminal -> non-terminal (except poisoned -> created)
        assert!(Completed.validate_transition(Created).is_err());
        assert!(Completed.validate_transition(Running).is_err());
        assert!(Completed.validate_transition(Ready).is_err());
        assert!(Completed.validate_transition(Queued).is_err());

        // Skip states
        assert!(Created.validate_transition(Running).is_err());
        assert!(Created.validate_transition(Ready).is_err());
        assert!(Created.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Assigned).is_err());
        assert!(Queued.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Running).is_err());
        assert!(Ready.validate_transition(Completed).is_err());

        // Running -> Ready is NOT valid (must go through Failed)
        assert!(Running.validate_transition(Ready).is_err());
        assert!(Running.validate_transition(Queued).is_err());
        assert!(Running.validate_transition(Assigned).is_err());

        // Failed can only go to Ready
        assert!(Failed.validate_transition(Running).is_err());
        assert!(Failed.validate_transition(Completed).is_err());
        assert!(Failed.validate_transition(Queued).is_err());

        // Poisoned can only go to Created (TTL expiry) or stay Poisoned
        assert!(Poisoned.validate_transition(Ready).is_err());
        assert!(Poisoned.validate_transition(Running).is_err());
        assert!(Poisoned.validate_transition(Failed).is_err());

        // DependencyFailed is terminal: can't go anywhere except self
        assert!(DependencyFailed.validate_transition(Ready).is_err());
        assert!(DependencyFailed.validate_transition(Queued).is_err());
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
            expected_output_paths: vec![],
            output_names: vec!["out".into()],
            is_fixed_output: false,
            // is_ca=true: this IS a CA derivation, so the flag
            // mattered pre-restart. Prove it's false post-restart
            // regardless.
            is_ca: true,
            failed_builders: vec![],
        };
        let state = DerivationState::from_recovery_row(row, DerivationStatus::Queued).unwrap();
        assert!(state.is_ca, "precondition: recovered as CA");
        assert!(
            !state.ca_output_unchanged,
            "recovery MUST reset ca_output_unchanged to false (NOT persisted — \
             compare result from a prior scheduler instance is stale)"
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
            (Created, Completed), // cache hit
            (Created, Queued),
            (Queued, Ready),
            (Ready, Assigned),
            (Assigned, Running),
            (Assigned, Ready), // worker lost
            (Running, Completed),
            (Running, Failed),
            (Running, Poisoned),
            (Failed, Ready),
            // DependencyFailed cascade
            (Created, DependencyFailed),
            (Queued, DependencyFailed),
            (Ready, DependencyFailed),
            // Poison TTL reset
            (Poisoned, Created),
            // Cancel from in-flight
            (Assigned, Cancelled),
            (Running, Cancelled),
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

        for from in DerivationStatus::ALL {
            for to in DerivationStatus::ALL {
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
        };
        let state = DerivationState::from_poisoned_row(row).unwrap();
        // Clamp caps at 1yr, checked_sub(1yr) on most boxes → None →
        // poisoned_at = now. This is a panic guard, not correctness
        // — recovery.rs filters expired rows before calling here so
        // a +inf elapsed would never reach this in practice.
        assert!(state.poisoned_at.is_some());
    }
}

#[cfg(test)]
mod status_snapshot {
    //! Cross-language DerivationStatus enforcement. The golden file at
    //! `rio-test-support/golden/derivation_statuses.json` is the single
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
        let golden = include_str!("../../../rio-test-support/golden/derivation_statuses.json");
        assert_eq!(
            json.trim(),
            golden.trim(),
            "\nDerivationStatus {{as_str, is_terminal}} set drifted from golden.\n\
             If you added/reclassified a variant, update IN ORDER:\n\
               (1) rio-test-support/golden/derivation_statuses.json\n\
               (2) rio-dashboard/src/lib/graphLayout.ts — STATUS_CLASS + SORT_RANK + TERMINAL\n\
               (3) rio-dashboard/src/lib/__tests__/graphLayout.test.ts — intended-set asserts\n\
               (4) docs/src/components/scheduler.md — PG CHECK constraint list\n\
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
