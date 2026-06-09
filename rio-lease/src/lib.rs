//! Kubernetes Lease-based leader election.
//!
//! When `lease_name` is configured, a background task acquires and
//! renews a `coordination.k8s.io/v1` Lease. On acquire, it derives
//! the generation from the lease's transition count (workers reject
// r[impl sched.lease.k8s-lease+2]
// r[impl sched.lease.generation-fence+3]
//! the old leader's stale-gen assignments once the post-recovery
//! generation reaches them via heartbeat) and sets `is_leader=true`
//! (dispatch_ready checks this).
//!
//! When NOT set (VM tests, single-scheduler deployments): no kube
//! dependency at runtime, `is_leader` defaults to `true`,
//! generation stays at 1. Zero behavior change for existing
//! deployments.
//!
//! # Election mechanics (see `election.rs`)
//!
//! All lease mutations go through `kube::Api::replace()` (PUT),
//! which requires the GET's `metadata.resourceVersion` — the
//! apiserver rejects with 409 if the object changed. Two racing
//! writers: exactly one wins. On top of that, standbys use a
//! local-monotonic "observed record" clock to decide staleness
//! (immune to cross-node clock skew — we never compare against
//! the lease's `renewTime`).
//!
//! This STILL isn't a linearizable fence — a partitioned leader
//! can keep dispatching until it self-fences at `SELF_FENCE_AFTER`.
//! That's acceptable because dispatch is idempotent:
//!
//! - DAG merge dedups by `drv_hash`. Two schedulers merging the
//!   same SubmitBuild both end up with the same DAG node.
//! - Assignment minting is fenced by the serving replica's lease
//!   generation against the durable claims floor
//!   (`leader_generation_claims`): a deposed leader's mint loses the
//!   fence race instead of reaching a pod. (Stream-era: workers
//!   compared generations from the removed heartbeat channel.)
//! - Worst case: a derivation dispatches twice (one from each
//!   leader), builds twice, produces the same output (deterministic
//!   builds). Wasteful but correct.
//!
//! # Lease TTL and renew cadence
//!
//! 15s TTL, renewed every 5s. The 3:1 ratio is the kubernetes
//! convention (see kube-controller-manager's defaults). The
//! thresholds the protocol acts on are `SELF_FENCE_AFTER` (11s —
//! two missed renew ticks plus one in-flight attempt) and
//! `STEAL_AFTER` (19s).

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::serde_json::json;
use kube::api::{Api, ListParams, Patch, PatchParams};
// tokio's Instant, not std's: identical semantics in production (a thin
// wrapper over std's monotonic clock), but it follows tokio's test clock
// under `start_paused`, which is what makes the lease loop's fence-check
// cadence testable end to end (see the loop-cadence test in `mod tests`).
use tokio::time::Instant;
use tracing::{debug, info, warn};

mod clock;
mod election;
pub use election::{Decision, ElectionResult, LeaderElection, Observed, decide};

/// Model-based tests: replay traces from
/// `docs/spec/models/leaderElection.qnt` against the election machinery.
/// A `#[cfg(test)]` module under `src/` (not `tests/mbt.rs`) because the
/// driver needs the `pub(crate)` `fetch_and_decide`/`act` split — those
/// stay crate-private so production code outside rio-lease can never
/// separate the GET from the PUT (that separation is how TOCTOU bugs get
/// written; the composition being the only public entry point is a
/// structural guarantee).
#[cfg(test)]
mod mbt_tests;

/// Callbacks fired on lease acquire/lose transitions.
///
/// `run_lease_loop` calls these synchronously from the renewal tick.
/// **They MUST NOT block** — a blocked hook stalls the renewal tick,
/// so the loop can neither renew nor self-fence while a standby steals
/// after `STEAL_AFTER` of observed staleness (dual-leader). Defer async
/// work through an order-preserving handoff — one queue drained by one
/// forwarder task, as the scheduler's `SchedulerLeaseHooks` does — NOT
/// one spawned task per call: independently spawned tasks can reorder,
/// and the same-tick `on_lose` → `on_acquire` pair (self-fence false
/// alarm) must reach the consumer in invocation order (the obligation
/// `sched.lease.hook-order` makes normative for the scheduler's actor
/// delivery).
///
/// Per-component metrics (`rio_{scheduler,controller}_lease_*_total`)
/// are emitted from the hook impl, not from `run_lease_loop`, so each
/// consumer names them per the `rio_{component}_` convention.
pub trait LeaseHooks: Clone + Send + 'static {
    /// Called once per standby→leader transition, AFTER
    /// [`LeaderState::on_acquire`] has written the generation and set
    /// `is_leader=true`.
    fn on_acquire(&self);
    /// Called once per leader→standby transition (explicit lose OR local
    /// self-fence), AFTER [`LeaderState::on_lose`] has cleared `is_leader`
    /// and `recovery_complete`.
    fn on_lose(&self);
    /// Called once per REBOUND — a holder change observed late on a
    /// still-leading round (`sched.lease.rebound`), AFTER
    /// [`LeaderState::on_rebound`] re-recorded the observed transition
    /// count and cleared `recovery_complete`. A rebound is a compressed
    /// lose→acquire pair whose standby interval was never locally
    /// observed; consumers decide per effect which halves to run (the
    /// scheduler routes it through its leadership-edge table's declared
    /// rebound policy). REQUIRED, no default: every implementor must
    /// choose — a silently-inherited acquire-only delivery is exactly
    /// how the cost-latch lose cell was skipped (merged_bug_212).
    /// Consumers MUST still tolerate it arriving with no intervening
    /// [`on_lose`](Self::on_lose).
    fn on_rebound(&self);
}

/// Lease TTL as written to the Lease object's `leaseDurationSeconds`.
/// Documentation for `kubectl describe lease` and any client-go
/// co-tenant — NOT the threshold either side of this protocol acts on.
/// The leader self-fences at [`SELF_FENCE_AFTER`] (earlier) and a
/// follower steals at [`STEAL_AFTER`] (later); the gap between them is
/// what closes the dual-belief window.
const LEASE_TTL: Duration = Duration::from_secs(15);

/// Renewal interval. LEASE_TTL / 3 per K8s convention.
///
/// `pub` so rio-scheduler can derive its bump-confirmation cap from the
/// same constants the loop runs on (`sched.recovery.bump-confirm`); the
/// compile-time asserts below keep the constants coupled.
pub const RENEW_INTERVAL: Duration = Duration::from_secs(5);

/// Slack between renew timeout and RENEW_INTERVAL. Each renew
/// attempt must return BEFORE the next interval tick would fire;
/// otherwise a hung apiserver burns multiple ticks on one call.
/// 3s deadline for a Lease GET+PUT is generous (healthy p99 <100ms)
/// while still giving 2 attempts before SELF_FENCE_AFTER.
const RENEW_SLOP: Duration = Duration::from_secs(2);

/// Per-phase budget for the split renew round-trip
/// (`sched.lease.cancelled-write`): the 3s belt
/// (`RENEW_INTERVAL − RENEW_SLOP`) tiled into a read phase and a write
/// phase. Bounding the phases SEPARATELY is load-bearing twice over: a
/// mutating request is only ever transmitted after a COMPLETED read —
/// so a truly-blind replica transmits nothing, its rv freezes, and
/// stealing still works — and a transmitted write always has a full
/// budget for its response, so "PUT sent but its response window was
/// eaten by a slow GET" is unrepresentable. Healthy p99 for either
/// phase is <100ms; 1.5s each is generous.
const RENEW_PHASE_DEADLINE: Duration = Duration::from_millis(1500);
const _: () = assert!(
    RENEW_PHASE_DEADLINE.as_millis() * 2 == RENEW_INTERVAL.as_millis() - RENEW_SLOP.as_millis(),
    "the two phase budgets must exactly tile the renew belt"
);

/// The asymmetry margin between the leader's self-fence deadline and the
/// follower's steal threshold. The two deadlines are anchored at
/// different moments, and BOTH sides carry their conservative-direction
/// premise as code: the leader's blind window is stamped from a
/// [`RenewAnchor`] minted BEFORE the attempt's await (anchor ≤ send ≤
/// commit — the window is never shorter than the model's), and the
/// follower's observed record follows the two-clock anchor discipline
/// in `election::decide()` — a fresh observation is STAMPED at the
/// GET's response instant while staleness is MEASURED against the
/// deciding GET's send instant, so the no-write span the steal acts on
/// is UNDERSTATED at both ends (a request-anchored stamp would
/// silently spend the 1.5s-per-side skew budget below on fetch
/// latency). Without a margin the follower's deadline can land
/// first with zero clock skew. The formal model
/// (`docs/spec/models/leaderElection.qnt`, the `leaderElectionAsymmetric`
/// regime) proves NeverDual —
/// no two replicas ever simultaneously believe they lead — exactly when
/// the separation exceeds the renew interval plus the round-trip clock
/// skew:
///
/// ```text
/// 2 × FENCE_MARGIN  ≥  RENEW_INTERVAL + 2 × clock_skew
///       8s          ≥       5s        + 2 × skew
/// ```
///
/// The renew-interval term is the victim's fence-check latency:
/// `run_lease_loop` evaluates `maybe_self_fence` at the top of every
/// tick, before starting the renew attempt (the error arm re-evaluates
/// when a failed attempt resolves — an additional, earlier opportunity,
/// never the only one), so the fence fires at most one RENEW_INTERVAL
/// after the deadline crossing. The model anchors the victim's fence at
/// the apiserver COMMIT of its last renew; production stamps
/// `last_successful_renew` with a [`RenewAnchor`] minted BEFORE the
/// attempt's await — anchor ≤ send ≤ commit — so the production anchor
/// is never later than the model's commit anchor and the model's bound
/// (commit + SELF_FENCE_AFTER + RENEW_INTERVAL) is conservative for
/// production with no arithmetic premise about response lag at all
/// (stamping a post-response reading has no API; see [`BlindClock`]).
/// What remains of the separation is a 1.5s one-sided clock-skew budget
/// — far above NTP drift on cloud nodes. Host suspend is caught at the
/// first post-resume fence check by the suspend-aware fence clock
/// (`clock::suspend_aware_now`, CLOCK_BOOTTIME on Linux) — including a
/// suspend that straddles an in-flight renew, whose pre-suspend anchor
/// preserves the blind window the response would otherwise erase; the
/// remaining pause classes — hypervisor-level VM pause (invisible to
/// BOOTTIME too), long stop-the-world stalls, and the resume-to-first-
/// tick gap — still re-open the window, which is the impossibility
/// result the generation fence (r\[sched.lease.generation-fence+3\])
/// backstops. The model also shows the bound is tight: one tick less
/// separation and a dual-belief state is reachable.
const FENCE_MARGIN: Duration = Duration::from_secs(4);

/// The leader self-fences after this long without a successful renew:
/// LEASE_TTL − FENCE_MARGIN. Two missed renew ticks plus one in-flight
/// attempt, instead of three. The idempotent same-epoch re-claim
/// (r\[sched.lease.generation-claim\]) is what makes the more frequent
/// false alarms free: a self-fence followed by a successful renew
/// re-acquires at the SAME generation and in-flight work survives.
///
/// `pub` for the same reason as [`RENEW_INTERVAL`].
pub const SELF_FENCE_AFTER: Duration = Duration::from_secs(11);

/// A follower steals after the lease's holder-authored content —
/// `(holderIdentity, renewTime bytes)` — has been observed unchanged
/// for this long: LEASE_TTL + FENCE_MARGIN. NOT resourceVersion-keyed:
/// the apiserver bumps the rv on every write, so foreign metadata
/// patches would reset an rv-keyed clock forever (merged_bug_180).
/// Failover after a real leader death takes up to this long plus one
/// renew interval.
const STEAL_AFTER: Duration = Duration::from_secs(19);

// The derivation and the NeverDual condition, enforced at compile time
// so no constant moves without the others. `Duration::as_secs` is
// const-stable.
const _: () = {
    assert!(
        SELF_FENCE_AFTER.as_secs() == LEASE_TTL.as_secs() - FENCE_MARGIN.as_secs(),
        "SELF_FENCE_AFTER must be LEASE_TTL - FENCE_MARGIN"
    );
    assert!(
        STEAL_AFTER.as_secs() == LEASE_TTL.as_secs() + FENCE_MARGIN.as_secs(),
        "STEAL_AFTER must be LEASE_TTL + FENCE_MARGIN"
    );
    // The model-verified NeverDual condition: the fence/steal separation
    // must exceed the renew interval — the victim's fence-check latency,
    // which the tick-time check at the top of run_lease_loop's loop body
    // bounds to one tick (the remainder is the clock-skew budget).
    assert!(
        2 * FENCE_MARGIN.as_secs() > RENEW_INTERVAL.as_secs(),
        "the fence/steal separation must exceed the renew interval"
    );
    // The tick-time fence check at the top of run_lease_loop's loop is
    // the premise of the NeverDual derivation (fence-check latency at
    // most one tick). It holds only if the loop body — bounded by the
    // renew attempt deadline RENEW_INTERVAL - RENEW_SLOP — stays
    // strictly inside the tick period, so MissedTickBehavior::Skip can
    // never drop a tick and stretch the check cadence. Whole-second
    // comparison on purpose, consistent with the neighboring asserts:
    // do not weaken to `> Duration::ZERO`.
    assert!(
        RENEW_SLOP.as_secs() > 0,
        "the renew attempt deadline must be strictly shorter than RENEW_INTERVAL"
    );
    // (The response-anchoring premise that used to be pinned here is
    // gone with response anchoring itself: the blind window is stamped
    // from a RenewAnchor minted before the attempt's await, which is
    // never later than the apiserver commit the model anchors at.)
    // The leader must get at least one renew attempt before fencing.
    assert!(
        SELF_FENCE_AFTER.as_secs() > RENEW_INTERVAL.as_secs(),
        "the leader must get at least one renew attempt before fencing"
    );
};

/// Verify cadence for the leader-marks reconcile: every Nth election
/// round (N renews ≈ 60s at the 5s interval) a clean-and-leading loop
/// re-reads its OWN Pod and compares the stored marks against its
/// leadership — re-dirtying on divergence. This converts the marks
/// machinery from edge-triggered-with-enumerated-writers to genuinely
/// level-triggered: ANY falsifier (a foreign sweep racing a re-acquire,
/// `kubectl label`, a future actor nobody enumerated) is repaired
/// within one verify interval plus one reconcile, instead of lingering
/// until the next leadership transition.
// r[impl sched.lease.marks-verify]
pub const MARKS_VERIFY_EVERY: u64 = 12;

/// Annotation key the leader stamps on its own Pod so the ReplicaSet
/// controller evicts the standby first during scale-down/RollingUpdate.
/// See `spawn_patch_leader_marks` (private) / `sched.lease.deletion-cost`.
pub const POD_DELETION_COST_ANNOTATION: &str = "controller.kubernetes.io/pod-deletion-cost";

/// Label key marking the pod that currently holds the lease. Consumed by
/// the `rio-scheduler-leader` Service selector (helm `scheduler.yaml`) so
/// ClusterIP clients that cannot retry a Trailers-Only `Unavailable`
/// (the dashboard's nginx upstream) only ever reach the leader.
///
/// The non-leader state is **label absent**, not `standby`: a Service
/// selector can only match presence+value, and absence is the safe
/// default for a pod that has never run the election loop.
pub const LEADER_ROLE_LABEL: &str = "rio.build/scheduler-role";

/// Value of [`LEADER_ROLE_LABEL`] on the current leader.
pub const LEADER_ROLE_LEADER: &str = "leader";

/// Lease configuration, built from the scheduler's loaded `Config`.
///
/// `Option` because it's entirely optional — `None` means non-K8s
/// mode (the common case for VM tests and dev). `from_parts()`
/// returns `None` unless `lease_name` is set.
#[derive(Debug, Clone)]
pub struct LeaseConfig {
    /// The Lease object's `.metadata.name`. Unique per scheduler
    /// deployment — two independent rio-build clusters in the same
    /// K8s namespace would use different names.
    pub lease_name: String,
    /// Namespace for the Lease. Usually the scheduler pod's own
    /// namespace (read from the downward API or the service-account
    /// mount).
    pub namespace: String,
    /// This replica's identity. Usually the pod name (HOSTNAME env,
    /// set by K8s). Written into `Lease.spec.holderIdentity` when
    /// we hold the lock — `kubectl get lease` shows who's leading.
    pub holder_id: String,
    /// Optional `(key, value)` label the lease loop reconciles onto its
    /// OWN Pod to mirror its leadership: present while leading, removed
    /// (merge-patch `null`) while not. Level-triggered — re-patched on
    /// every successful election round-trip until a patch lands, so a
    /// dropped PATCH or an in-place container restart converges within
    /// one tick. A leader-only Service selects on it (see
    /// [`LEADER_ROLE_LABEL`]). `None` (the default, and what the
    /// controller's nodeclaim-pool lease uses) → only the deletion-cost
    /// annotation is patched.
    pub leader_pod_label: Option<(String, String)>,
}

impl LeaseConfig {
    /// Build from layer-merged config fields. Returns `None` if
    /// `lease_name` is unset — the signal for "not running under
    /// K8s." Goes through the config loader like every other config knob
    /// (previously read `std::env::var` directly, bypassing the
    /// TOML/CLI layers — plan 21 Batch E).
    ///
    /// `lease_namespace = None` falls through to reading the
    /// in-cluster service-account namespace mount (standard K8s
    /// downward-API path). If that's ALSO missing (running locally
    /// against a remote cluster), defaults to "default" — probably
    /// wrong, but the operator will notice when the Lease doesn't
    /// appear where expected.
    ///
    /// `HOSTNAME` stays a raw env read: it's set by K8s (not us),
    /// not `RIO_`-prefixed, and has no TOML/CLI equivalent. If
    /// missing (non-K8s with lease_name manually set — weird but
    /// possible for testing), falls back to a UUID. Unique, just
    /// not human-readable in `kubectl get lease`.
    pub fn from_parts(lease_name: Option<String>, lease_namespace: Option<String>) -> Option<Self> {
        let lease_name = lease_name?;

        let namespace = lease_namespace.unwrap_or_else(|| {
            // The standard in-cluster namespace mount. Every pod
            // gets this via the service-account projected volume.
            std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "default".to_string())
        });

        let holder_id =
            std::env::var("HOSTNAME").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

        Some(Self {
            lease_name,
            namespace,
            holder_id,
            leader_pod_label: None,
        })
    }

    /// Reconcile `key: value` onto the leader's own Pod (present while
    /// leading, absent otherwise). See the `leader_pod_label` field doc
    /// and [`LEADER_ROLE_LABEL`]. Builder-style so
    /// `from_parts(...).map(|c| c.with_leader_pod_label(...))` keeps the
    /// `Option` chain at the call site.
    pub fn with_leader_pod_label(
        mut self,
        key: impl Into<String>,
        value: impl Into<String>,
    ) -> Self {
        self.leader_pod_label = Some((key.into(), value.into()));
        self
    }
}

/// Recovery-completion sentinel: no acquire-epoch has completed
/// recovery. Lives outside the realistic `leaseTransitions` range (the
/// apiserver count starts at 0 and increments per holder change), so it
/// can never compare equal to a recorded `acquired_transitions`.
const RECOVERY_NOT_COMPLETE: u64 = u64::MAX;

/// Shared leader state. Written by the lease task and by the embedding
/// service's recovery path (generation seed + epoch-keyed completion
/// stamp); readers span dispatch gating, heartbeats, and health.
///
/// The atomics — generation, acquired_transitions, is_leader, and the
/// epoch-keyed completion stamp behind `recovery_complete()` —
/// updated together on acquire/lose/rebound
/// transitions (acquired_transitions is written on acquire and on
/// rebound; `on_lose` deliberately leaves it recording the last such
/// edge). **All
/// writes and reads use SeqCst** to prevent reordering on weak memory
/// models (ARM). Previously is_leader/recovery_complete used Relaxed,
/// which allowed a reader on ARM to see `recovery_complete=true`
/// before `is_leader=true` during an acquire transition (the two
/// stores reordered). SeqCst gives a single total order across all
/// these atomics: if a reader sees the last store of a transition, it
/// sees all prior stores too.
///
/// Transitions go through [`on_acquire`](Self::on_acquire) /
/// [`on_rebound`](Self::on_rebound) / [`on_lose`](Self::on_lose) rather
/// than raw field stores — these encapsulate the multi-field update
/// order.
///
/// `Clone` is a cheap all-Arc clone; main.rs clones once for the
/// lease loop, once for the actor, and uses the per-field accessors for
/// the health-toggle polling loop.
#[derive(Clone)]
pub struct LeaderState {
    /// Generation counter. Derived from the Lease's transition count on
    /// each acquisition via [`on_acquire`](Self::on_acquire). Same Arc as
    /// DagActor.generation (see `generation_arc()` — this IS that
    /// Arc, cloned).
    generation: Arc<AtomicU64>,
    /// The `leaseTransitions` count observed at the most recent acquire
    /// edge or rebound. Unlike `generation` (a `fetch_max` that the
    /// PG-floor seed can saturate into a no-op), this is stored
    /// unconditionally on every [`on_acquire`](Self::on_acquire) and
    /// every [`on_rebound`](Self::on_rebound) — it is the holder-change
    /// signal the recovery TOCTOU gate compares when the generation
    /// cannot move. Only meaningful after the first acquire; `on_lose`
    /// does not touch it (it records the last acquire edge or rebound —
    /// the gate compares those edges, not the current lease state).
    acquired_transitions: Arc<AtomicU64>,
    /// Whether we currently hold the lease. dispatch_ready early-
    /// returns if false (standby schedulers merge DAGs but don't
    /// dispatch — state warm for fast takeover).
    is_leader: Arc<AtomicBool>,
    /// The `acquired_transitions` value recovery has completed FOR
    /// ([`RECOVERY_NOT_COMPLETE`] = none). `recovery_complete()` ⇔ this
    /// equals the currently recorded `acquired_transitions` — i.e. a
    /// completion is only valid while the acquire-epoch it was computed
    /// under is still the recorded one. dispatch_ready gates on BOTH
    /// is_leader AND that predicate. Set by handle_leader_acquired
    /// AFTER recover_from_pg finishes, with the transition count it
    /// snapshotted at recovery entry; reset to the sentinel by
    /// [`on_lose`](Self::on_lose) / [`on_rebound`](Self::on_rebound) /
    /// [`invalidate_recovery_completion`](Self::invalidate_recovery_completion)
    /// (the actor's `LeaderLost` handler, clearing a kept same-epoch
    /// completion whose DAG it is about to wipe) so re-acquire (or the
    /// rebound's re-fired hook) re-triggers recovery.
    /// Keying the completion to the epoch — rather than a bare bool —
    /// means a completion racing a concurrent lease transition can never
    /// ungate dispatch for an epoch it was not computed under, with no
    /// reliance on store ordering between the two writer tasks.
    ///
    /// Separate from is_leader because the lease loop sets
    /// is_leader IMMEDIATELY (non-blocking — must keep renewing),
    /// then fire-and-forgets LeaderAcquired. Recovery may take
    /// seconds; dispatch waits.
    recovery_completed_for: Arc<AtomicU64>,
    /// Renew rounds started: incremented by the lease loop immediately
    /// before each `try_acquire_or_renew()` attempt
    /// ([`begin_renew_round`](Self::begin_renew_round)). Paired with
    /// `last_leading_round` so the actor's recovery can require a
    /// post-claim apiserver round-trip that ended with this replica as
    /// the Lease holder before completing a recovery whose claim target
    /// the durable PG floor cannot vouch for
    /// (`sched.recovery.bump-confirm`). A confirmed round began
    /// strictly after every event that preceded its `begin_renew_round`
    /// call. Non-K8s/`always_leader` deployments never run the lease
    /// loop, so both counters legitimately stay 0 there — the actor's
    /// bump-confirmation consumer only runs off `LeaderAcquired`, which
    /// only the lease hooks send.
    renew_rounds_started: Arc<AtomicU64>,
    /// The highest round id whose attempt completed with
    /// `ElectionResult::Leading{..}` — see `renew_rounds_started`.
    /// 0 = no round has ever ended with this replica as the holder.
    last_leading_round: Arc<AtomicU64>,
    /// `Instant` of the last [`on_acquire`](Self::on_acquire). `None`
    /// when not leading. Exposed via [`leader_for`](Self::leader_for)
    /// and surfaced as `ListExecutorsResponse.leader_for_secs` so the
    /// controller's `orphan_reap_gate` can fail-closed during the
    /// post-failover partial-reconnect window: `self.executors` fills
    /// incrementally as workers reconnect (1-10s spread), and a
    /// non-empty PARTIAL list cannot prove absence. RwLock not atomic:
    /// `Instant` is 16B; reads are on a slow admin path,
    /// acquire/lose are rare.
    became_leader_at: Arc<parking_lot::RwLock<Option<Instant>>>,
    /// Cooperative step-down request (`sched.recovery.step-down`),
    /// stamped with the TENURE INSTANCE that issued it
    /// ([`acquired_instance`](Self::acquired_instance)), or
    /// [`STEP_DOWN_NONE`] when no request is pending. The lease loop
    /// consumes it at its next BELIEVING tick and only when the stamp
    /// still names the current instance — a request from an ended
    /// instance is dropped, never served against its successor. The
    /// stamp is the monotone per-acquire counter, NOT the lease
    /// transition count: the documented-frequent false-alarm-fence +
    /// same-epoch re-acquire pair re-acquires at the SAME transition
    /// count, and a count-keyed stamp would let recovery #1's stale
    /// demotion fire against a successor whose recovery #2 succeeded
    /// (merged_bug_128). Every acquire/rebound additionally CLEARS a
    /// pending request — a new instance starts clean; a failing re-run
    /// re-requests under its own instance. A tick on which this
    /// replica does not believe it leads leaves the request armed. In
    /// deployments with no lease loop (`always_leader`) the request is
    /// a recorded dead letter — there is no healthy peer a step-down
    /// could yield to; the operator signal is the recovery-failure
    /// counter/alert.
    step_down_for: Arc<AtomicU64>,
    /// Monotone tenure-instance counter: bumped by every
    /// [`on_acquire`](Self::on_acquire) and
    /// [`on_rebound`](Self::on_rebound). Unlike `acquired_transitions`
    /// (the lease's holder-change count, which a same-epoch re-acquire
    /// legitimately repeats), an instance value is never reused — it
    /// identifies one local tenure instance, which is what
    /// tenure-scoped requests must bind to.
    acquire_instance: Arc<AtomicU64>,
}

/// Sentinel for "no step-down request pending" in
/// [`LeaderState::step_down_for`]. `u64::MAX` can never collide with a
/// real instance: the per-acquire counter starts at 0 and bumps once
/// per acquire/rebound — bounded far below over any process lifetime.
const STEP_DOWN_NONE: u64 = u64::MAX;

impl Default for LeaderState {
    /// Non-K8s/test default: leader immediately, generation = 1,
    /// recovery_complete = true. Same as
    /// `always_leader(Arc::new(AtomicU64::new(1)))`.
    fn default() -> Self {
        Self::always_leader(Arc::new(AtomicU64::new(1)))
    }
}

impl LeaderState {
    /// Shared `is_leader` Arc. SeqCst loads only; writes go through
    /// [`on_acquire`](Self::on_acquire)/[`on_lose`](Self::on_lose).
    pub fn is_leader_arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_leader)
    }

    /// Shared `generation` Arc.
    pub fn generation_arc(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// Current generation. `Acquire` load — pairs with the `SeqCst`
    /// `fetch_max` in [`on_acquire`](Self::on_acquire) so a reader
    /// observing the new generation also sees all prior stores.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether we currently hold the lease. `SeqCst` load — see the
    /// struct doc for the multi-field ordering rationale.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }

    /// The `leaseTransitions` count recorded by the most recent
    /// [`on_acquire`](Self::on_acquire) or
    /// [`on_rebound`](Self::on_rebound). `SeqCst` load — same
    /// discipline as [`is_leader`](Self::is_leader) /
    /// [`recovery_complete`](Self::recovery_complete). Changes on every
    /// acquire edge or rebound that follows a holder change, even when
    /// the generation `fetch_max` is a no-op (the saturated
    /// post-lease-deletion regime) — which is exactly what the recovery
    /// TOCTOU gate needs.
    pub fn acquired_transitions(&self) -> u64 {
        self.acquired_transitions.load(Ordering::SeqCst)
    }

    /// Whether the actor's `recover_from_pg` has completed for the
    /// CURRENTLY recorded acquire-epoch: the completion stamp equals
    /// `acquired_transitions`. Two `SeqCst` loads (stamp first, then
    /// count) — pairs with
    /// [`set_recovery_complete`](Self::set_recovery_complete),
    /// [`invalidate_recovery_completion`](Self::invalidate_recovery_completion),
    /// [`on_lose`](Self::on_lose) and [`on_rebound`](Self::on_rebound).
    /// A stamp from an epoch a later transition has already replaced
    /// can never compare equal; a stamp loaded just before a concurrent
    /// transition lands errs only toward `false` for one read — the
    /// conservative direction (and the same benign window the previous
    /// boolean flag had).
    pub fn recovery_complete(&self) -> bool {
        let completed_for = self.recovery_completed_for.load(Ordering::SeqCst);
        completed_for != RECOVERY_NOT_COMPLETE
            && completed_for == self.acquired_transitions.load(Ordering::SeqCst)
    }

    /// Mark recovery complete for the acquire-epoch
    /// `transitions_at_entry` — the `acquired_transitions()` value the
    /// caller snapshotted when that recovery began. `SeqCst` store — the
    /// actor calls this AFTER `recover_from_pg` returns (success or
    /// fail-empty); pairs with the `SeqCst` loads in `dispatch_ready` so
    /// dispatch sees all recovery writes before proceeding. The store is
    /// unconditional but stale-safe by construction against the
    /// transitions that move the count: if a rebound or a re-acquire at
    /// a different count landed after the snapshot, the recorded
    /// `acquired_transitions` no longer equals the stamp and
    /// `recovery_complete()` stays false. A bare lose does NOT move the
    /// count — `on_lose` clears the stamp but leaves
    /// `acquired_transitions` alone — so a completion stored after it
    /// compares equal again: dispatch stays gated by `dispatch_ready`'s
    /// independent `is_leader` check, the heartbeat keeps advertising
    /// that raced completion's generation until the scheduler actor
    /// processes the queued loss (invalidating the orphaned completion)
    /// or, at the latest, until this replica's next acquire edge (see
    /// `GenerationReader::advertised` for the duration bound and for
    /// the claimed-path vs proceeded-unclaimed scoping of the ledger
    /// guarantee), and
    /// a later re-acquire at the SAME count is the deliberate same-epoch
    /// keep. The completion writers (the actor, serially; the controller
    /// once at startup) never race each other, so no compare-and-set is
    /// needed. The one shape the stamp cannot distinguish is an epoch
    /// that left and came back to the SAME count (the count-coincidence
    /// ABA) — the same residual the recovery TOCTOU gate already prices.
    pub fn set_recovery_complete(&self, transitions_at_entry: u64) {
        self.recovery_completed_for
            .store(transitions_at_entry, Ordering::SeqCst);
    }

    /// Invalidate any recorded recovery completion: store
    /// `RECOVERY_NOT_COMPLETE` into the stamp, touching nothing else.
    /// Called by the scheduler actor when it processes a `LeaderLost`
    /// and destroys the persisted state a previously-recorded
    /// completion certified — the stamp must not outlive the state it
    /// certifies ("state wiped ⇒ recovery not complete"). Deliberately
    /// does NOT touch `is_leader` or `acquired_transitions`: the lease
    /// loop owns those, and a same-count re-acquire may already be live
    /// by the time the queued loss is drained. Idempotent with the
    /// lease loop's own clears in [`on_lose`](Self::on_lose) /
    /// [`on_rebound`](Self::on_rebound); the follow-up
    /// `LeaderAcquired`'s recovery records a fresh completion. Why
    /// `on_lose`'s clear is not sufficient: a kept same-epoch recovery
    /// can re-stamp the completion after `on_lose` ran but before the
    /// actor processes the queued `LeaderLost` — this is the
    /// invalidation for exactly that orphaned stamp. No-clobber
    /// argument: within the scheduler the actor is the sole writer of
    /// completions, so this call can only ever kill a completion that
    /// certified the state being wiped — a newer legitimate completion
    /// is necessarily written later, by the same actor task, in the
    /// follow-up `LeaderAcquired`'s recovery.
    pub fn invalidate_recovery_completion(&self) {
        self.recovery_completed_for
            .store(RECOVERY_NOT_COMPLETE, Ordering::SeqCst);
    }

    /// Allocate the id of a renew round that is about to start. Called
    /// by the lease loop immediately before each
    /// `try_acquire_or_renew()` attempt; returns this round's id (ids
    /// start at 1). `SeqCst` — same discipline as the neighbouring
    /// atomics, and the round id must be taken before the attempt's
    /// apiserver I/O begins for the post-claim ordering argument in
    /// `sched.recovery.bump-confirm` to hold.
    pub fn begin_renew_round(&self) -> u64 {
        self.renew_rounds_started.fetch_add(1, Ordering::SeqCst) + 1
    }

    /// Number of renew rounds the lease loop has started. A reader that
    /// snapshots this before an event and later observes
    /// [`last_leading_round`](Self::last_leading_round) above the
    /// snapshot knows an apiserver round-trip that began after the
    /// snapshot ended with this replica as the Lease holder.
    pub fn renew_rounds_started(&self) -> u64 {
        self.renew_rounds_started.load(Ordering::SeqCst)
    }

    /// Record that round `round` completed with this replica as the
    /// Lease holder. `fetch_max` so a stale completion can never
    /// regress the recorded round.
    pub fn confirm_leading_round(&self, round: u64) {
        self.last_leading_round.fetch_max(round, Ordering::SeqCst);
    }

    /// The highest round id whose attempt completed with this replica
    /// as the Lease holder (0 = never). See
    /// [`renew_rounds_started`](Self::renew_rounds_started).
    pub fn last_leading_round(&self) -> u64 {
        self.last_leading_round.load(Ordering::SeqCst)
    }

    /// Elapsed since this replica acquired leadership, or `None` when
    /// not leading. Populates `ListExecutorsResponse.leader_for_secs`.
    // r[impl sched.admin.list-executors-leader-age+3]
    pub fn leader_for(&self) -> Option<Duration> {
        self.became_leader_at.read().map(|t| t.elapsed())
    }

    /// Monotonically raise generation to at least `target`. `Release`
    /// `fetch_max` — defensive against a Lease transition-count reset
    /// (`kubectl delete lease` recreates the Lease at
    /// `leaseTransitions = 0`, so the lease-derived generation regresses
    /// while PG's high-water persists). Returns the previous value.
    pub fn seed_generation_from(&self, target: u64) -> u64 {
        self.generation.fetch_max(target, Ordering::Release)
    }

    /// Request a cooperative step-down (`sched.recovery.step-down`)
    /// on behalf of the tenure INSTANCE identified by `for_instance` —
    /// the [`acquired_instance`](Self::acquired_instance) value the
    /// requesting consumer recorded at its tenure's entry. The loop
    /// serves the request at its next believing tick IF that instance
    /// is still current: it releases the lease (holder-guarded,
    /// bounded by the renew deadline), runs the full lose-edge effects
    /// (`on_lose` + the consumer hook + leader-marks reconciliation),
    /// then resumes candidacy on the following tick. A request whose
    /// instance has ended by service time is dropped — its issuer no
    /// longer exists and the successor instance never asked to step
    /// down; the same-count re-acquire is an ended instance too
    /// (merged_bug_128). The durable generation claim is NOT released
    /// — an unserved claim is a documented harmless over-claim (the
    /// floor only grows). With no lease loop (`always_leader`
    /// deployments) the request is a dead letter: the tenure stays
    /// incomplete and the operator signal is the caller's failure
    /// counter.
    pub fn request_step_down(&self, for_instance: u64) {
        self.step_down_for.store(for_instance, Ordering::Release);
    }

    /// The current tenure-instance value. Consumers that may later
    /// request a tenure-scoped action
    /// ([`request_step_down`](Self::request_step_down)) record this at
    /// tenure entry; the loop compares against it at service time.
    /// Monotone and never reused — a false-alarm fence followed by a
    /// same-epoch re-acquire yields a NEW instance at the SAME
    /// `acquired_transitions` count.
    pub fn acquired_instance(&self) -> u64 {
        self.acquire_instance.load(Ordering::SeqCst)
    }

    /// Consume a pending step-down request at a believing loop tick.
    /// Returns `true` — serve the step-down — only when the pending
    /// request's instance stamp equals `current_instance`. A stale
    /// stamp (its instance ended; an acquire or rebound installed a
    /// successor — including the same-count re-acquire) is cleared and
    /// dropped with a warning; acquire/rebound additionally clear the
    /// slot eagerly, so this stale-drop arm is the backstop for a
    /// request racing the edge. `false` with no request pending is the
    /// steady state. Never called on a disbelieving tick — the caller
    /// guards on belief FIRST, so a self-fence at the tick top leaves
    /// the request armed for the next believing tick of the SAME
    /// instance (a re-acquire starts clean instead).
    pub fn take_step_down_request(&self, current_instance: u64) -> bool {
        let pending = self.step_down_for.load(Ordering::Acquire);
        if pending == STEP_DOWN_NONE {
            return false;
        }
        // compare_exchange, not swap: a racing re-request for a newer
        // instance must never be clobbered by this consume.
        if self
            .step_down_for
            .compare_exchange(pending, STEP_DOWN_NONE, Ordering::AcqRel, Ordering::Acquire)
            .is_err()
        {
            // A racer replaced the request between load and consume;
            // the next tick re-evaluates the fresh one.
            return false;
        }
        if pending == current_instance {
            return true;
        }
        warn!(
            requested_for_instance = pending,
            current_instance,
            "dropping a stale step-down request — the tenure instance that issued it \
             has ended (a later acquire or rebound installed a successor, possibly at \
             the same lease transition count) and the successor never asked to step down"
        );
        false
    }

    /// Is a step-down request pending (non-consuming)? Observability
    /// and tests only — the loop consumes through
    /// [`take_step_down_request`](Self::take_step_down_request).
    pub fn step_down_pending(&self) -> bool {
        self.step_down_for.load(Ordering::Acquire) != STEP_DOWN_NONE
    }

    /// Construct from pre-existing shared Arcs plus the initial
    /// recovery-completion state. Test fixtures that need to drive the
    /// flags from outside the lease loop (e.g. rio-scheduler's actor
    /// recovery tests) keep a clone of the returned `LeaderState` and
    /// observe/drive completion through the `recovery_complete()` /
    /// `set_recovery_complete()` / `on_*` methods. Not `#[cfg(test)]`:
    /// cross-crate test callers compile this crate without `--cfg test`.
    pub fn from_parts(
        generation: Arc<AtomicU64>,
        is_leader: Arc<AtomicBool>,
        recovery_complete: bool,
    ) -> Self {
        // Test fixtures: `became_leader_at` mirrors `is_leader` —
        // Some(now) when leading, None when standby. Avoids a fourth
        // param at every from_parts callsite.
        let became_leader_at = is_leader.load(Ordering::SeqCst).then(Instant::now);
        Self {
            generation,
            acquired_transitions: Arc::new(AtomicU64::new(0)),
            is_leader,
            // "Already complete" = complete for the fixture's initial
            // acquire-epoch (acquired_transitions starts at 0).
            recovery_completed_for: Arc::new(AtomicU64::new(if recovery_complete {
                0
            } else {
                RECOVERY_NOT_COMPLETE
            })),
            renew_rounds_started: Arc::new(AtomicU64::new(0)),
            last_leading_round: Arc::new(AtomicU64::new(0)),
            became_leader_at: Arc::new(parking_lot::RwLock::new(became_leader_at)),
            step_down_for: Arc::new(AtomicU64::new(STEP_DOWN_NONE)),
            acquire_instance: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Non-K8s mode: leader immediately, generation stays at 1.
    /// This is what VM tests and single-scheduler deployments see.
    ///
    /// recovery_complete=true: no lease acquisition → no recovery
    /// trigger. Empty DAG at startup (same as Phase 3a). Single-
    /// instance deployments don't failover so PG recovery isn't
    /// meaningful.
    pub fn always_leader(generation: Arc<AtomicU64>) -> Self {
        Self {
            generation,
            acquired_transitions: Arc::new(AtomicU64::new(0)),
            is_leader: Arc::new(AtomicBool::new(true)),
            // Complete for the (only) epoch 0: no lease loop, no
            // recovery to wait for.
            recovery_completed_for: Arc::new(AtomicU64::new(0)),
            renew_rounds_started: Arc::new(AtomicU64::new(0)),
            last_leading_round: Arc::new(AtomicU64::new(0)),
            // Non-K8s/test mode reports a real (small but growing) age
            // so consumers reading `leader_for_secs` don't see 0
            // forever (which the controller treats as "young leader,
            // fail-closed").
            became_leader_at: Arc::new(parking_lot::RwLock::new(Some(Instant::now()))),
            step_down_for: Arc::new(AtomicU64::new(STEP_DOWN_NONE)),
            acquire_instance: Arc::new(AtomicU64::new(0)),
        }
    }

    /// K8s mode: NOT leader until the lease loop acquires. If the
    /// loop never acquires (another replica holds it), we stay
    /// standby forever — correct.
    ///
    /// recovery_complete=false: acquisition triggers recovery.
    /// dispatch_ready gates on this AND is_leader.
    pub fn pending(generation: Arc<AtomicU64>) -> Self {
        Self {
            generation,
            acquired_transitions: Arc::new(AtomicU64::new(0)),
            is_leader: Arc::new(AtomicBool::new(false)),
            // No epoch has completed recovery yet: the first
            // acquisition triggers recovery before dispatch ungates.
            recovery_completed_for: Arc::new(AtomicU64::new(RECOVERY_NOT_COMPLETE)),
            renew_rounds_started: Arc::new(AtomicU64::new(0)),
            last_leading_round: Arc::new(AtomicU64::new(0)),
            became_leader_at: Arc::new(parking_lot::RwLock::new(None)),
            step_down_for: Arc::new(AtomicU64::new(STEP_DOWN_NONE)),
            acquire_instance: Arc::new(AtomicU64::new(0)),
        }
    }

    /// Acquire transition: derive the generation from the lease's
    /// transition count, set is_leader=true. Returns the new generation.
    ///
    /// The generation IS `lease_transitions + 1`. `leaseTransitions`
    /// counts holder *changes* — the lease's creator is transition 0, the
    /// first thief is 1 — and the rv-guarded PUT bumps it atomically with
    /// the holder change, so two replicas that both believe they lead can
    /// never have acquired at the same count. The `+ 1` maps the creator
    /// onto the generation floor of 1 (the `AtomicU64::new(1)` init and
    /// the non-K8s `always_leader` value): creator → 1, first thief → 2.
    /// Without it the creator (`fetch_max(0)` → stays 1) and the first
    /// thief (`fetch_max(1)` → stays 1) would collide at the floor.
    ///
    /// `fetch_max`, not a store or an increment: the recovery path's
    /// [`seed_generation_from`](Self::seed_generation_from) may already
    /// have raised the generation past `transitions + 1` after a
    /// `kubectl delete lease` reset the transition count while PG's
    /// high-water persisted. A local increment is what produced the
    /// generation collision `StaleLeaderHasStaleGeneration` falsifies in
    /// `docs/spec/models/leaderElection.qnt` — a leader deposed before
    /// persisting its generation to PG left the high-water stale and its
    /// successor seeded from the same value.
    ///
    /// A consequence of deriving from the holder-change count: a replica
    /// that self-fences on a connectivity blip and then successfully
    /// renews (nobody stole in between) re-acquires at the SAME
    /// generation. That is the correct epoch semantics — its in-flight
    /// assignments are still its own; bumping would spuriously invalidate
    /// them. If somebody DID steal during the blind window, the re-acquire
    /// is itself a steal (transitions bumps) or a 409 (we observe the new
    /// holder first) — either way the holder change is what increments
    /// the epoch.
    ///
    /// In the post-lease-deletion regime (the PG floor seeded the
    /// generation past `lease_transitions + 1` via
    /// [`seed_generation_from`](Self::seed_generation_from)), the
    /// generation `fetch_max` here is a no-op even on a holder change.
    /// The raw `lease_transitions` value is therefore recorded
    /// unconditionally in `acquired_transitions` — the recovery TOCTOU
    /// gate keys on that recorded count (plus `is_leader`), not on the
    /// generation, to detect holder changes that land mid-recovery.
    ///
    /// SeqCst on all stores: the generation and acquired_transitions
    /// writes happen-before the is_leader write in the total order. A
    /// reader seeing is_leader=true (SeqCst load) sees the new
    /// generation and the new transition count. Recovery completion is
    /// NOT recorded here — that's the actor's job after recover_from_pg
    /// finishes (and a completion stamped under a previous epoch only
    /// reads as complete again if this acquire records the SAME
    /// transition count — the same-epoch re-acquire, which is exactly
    /// the case whose in-flight work is still valid).
    ///
    /// A holder change observed only after we are already Leading again
    /// (no standby interval ever observed locally) goes through
    /// [`on_rebound`](Self::on_rebound) instead — same count recording
    /// and generation derivation, different recovery-completion
    /// handling.
    // r[impl sched.lease.generation-fence+3]
    pub fn on_acquire(&self, lease_transitions: u64) -> u64 {
        let target = lease_transitions.saturating_add(1);
        // fetch_max returns the PREVIOUS value; the new value is the max
        // of the two.
        let new_gen = self
            .generation
            .fetch_max(target, Ordering::SeqCst)
            .max(target);
        // Unconditional store — that is the point: it changes on every
        // acquire that follows a holder change, even when the
        // generation fetch_max above is a no-op.
        self.acquired_transitions
            .store(lease_transitions, Ordering::SeqCst);
        // A new tenure INSTANCE begins: bump the never-reused instance
        // counter and clear any pending step-down — the request's
        // issuer ended with the previous instance, and a recovery that
        // fails again under this one re-requests with the new stamp
        // (merged_bug_128: the same-count re-acquire must not inherit
        // its failed predecessor's demotion).
        self.acquire_instance.fetch_add(1, Ordering::SeqCst);
        self.step_down_for.store(STEP_DOWN_NONE, Ordering::SeqCst);
        self.is_leader.store(true, Ordering::SeqCst);
        // AFTER is_leader=true: a reader seeing `leader_for().is_some()`
        // also sees is_leader=true (RwLock acquire/release pairs with
        // the SeqCst store above).
        *self.became_leader_at.write() = Some(Instant::now());
        new_gen
    }

    /// Rebound transition: a holder change observed late, on a
    /// still-leading round. The lease loop calls this when a round
    /// resolves `Leading` while we already believe we lead but the
    /// observed `leaseTransitions` count differs from the recorded one —
    /// a foreign term (or a delete/recreate) began and ended entirely
    /// inside our observation gap, so neither an acquire nor a lose edge
    /// ever fired locally. Returns the new generation.
    ///
    /// Store order differs from [`on_acquire`](Self::on_acquire), and it
    /// is load-bearing: the completion stamp is cleared FIRST, then the
    /// transition count is recorded, then the generation `fetch_max`
    /// runs. Nearly every path into an acquire edge already has
    /// `recovery_complete()` false (`pending()`, the lose arm, the
    /// self-fence). The exception is a completion that raced a bare
    /// lose (see `set_recovery_complete`): the stamp still matches the
    /// unmoved count, so a later re-steal at a different count enters
    /// `on_acquire` with the predicate true and raises the generation
    /// before its `acquired_transitions` store invalidates the stamp
    /// (transient once the actor's lost-handler clears the orphaned
    /// completion — see `handle_leader_lost`). That
    /// raise-before-invalidate window is observable only as the
    /// one-heartbeat lose→re-acquire straddle already priced at the
    /// scheduler's `advertised()`, and the count store that ends it
    /// happens-before `on_acquire`'s `is_leader` store, so dispatch
    /// keeps its backstop. A rebound has neither shield: it runs on a
    /// still-leading round, so without the clear-first order a reader
    /// could pair a still-true predicate with the raised generation
    /// while `is_leader` is true. Clearing the stamp first means a
    /// heartbeat reader cannot make that pairing, except in the
    /// already-accepted two-load-straddle case documented at the
    /// scheduler's `advertised()` — preserving
    /// `sched.lease.claim-before-advertise`. An in-flight recovery that
    /// completes AFTER this rebound stamps the PRE-rebound transition
    /// count, which the moved `acquired_transitions` no longer matches —
    /// so the late completion cannot ungate dispatch either (the only
    /// exception is an observed count that lands back exactly on the
    /// recorded one: the count-coincidence ABA priced at the recovery
    /// gate's entry-snapshot comment in rio-scheduler).
    ///
    /// `is_leader` is NOT touched: we hold the Lease at this
    /// observation, there is no moment of believed non-leadership to
    /// publish (and no lose hook fires — see the loop arm).
    /// `became_leader_at` IS reset: the foreign tenure may have shuffled
    /// workers, so restarting `leader_for()` (read by `leader_for_secs`
    /// and the controller's `orphan_reap_gate`) re-closes the
    /// fail-closed grace window — the conservative direction.
    ///
    /// The count coincidence — an observed value that lands exactly back
    /// on the recorded one — is undetectable here by construction; that
    /// residual is priced at the recovery gate's entry-snapshot comment
    /// in rio-scheduler.
    // r[impl sched.lease.rebound+4]
    // r[impl sched.lease.generation-fence+3]
    pub fn on_rebound(&self, lease_transitions: u64) -> u64 {
        self.recovery_completed_for
            .store(RECOVERY_NOT_COMPLETE, Ordering::SeqCst);
        self.acquired_transitions
            .store(lease_transitions, Ordering::SeqCst);
        // Same instance discipline as on_acquire: a rebound installs a
        // successor tenure instance, so a step-down filed by the
        // pre-rebound instance must not demote it (merged_bug_128).
        self.acquire_instance.fetch_add(1, Ordering::SeqCst);
        self.step_down_for.store(STEP_DOWN_NONE, Ordering::SeqCst);
        let target = lease_transitions.saturating_add(1);
        let new_gen = self
            .generation
            .fetch_max(target, Ordering::SeqCst)
            .max(target);
        *self.became_leader_at.write() = Some(Instant::now());
        new_gen
    }

    /// Lose transition: clear is_leader, clear the recovery-completion
    /// stamp.
    ///
    /// SeqCst on both stores: is_leader=false happens-before the stamp
    /// clear in the total order. A reader seeing the cleared stamp
    /// (SeqCst) also sees is_leader=false.
    /// Generation is NOT touched — the NEW leader derives its own
    /// from the lease's transition count on acquire; we don't know
    /// (and don't need to know) what that will be.
    /// `acquired_transitions` is NOT touched either — it records the
    /// last acquire edge or rebound, and the recovery TOCTOU gate
    /// compares those edges, not the current lease state.
    pub fn on_lose(&self) {
        self.is_leader.store(false, Ordering::SeqCst);
        self.recovery_completed_for
            .store(RECOVERY_NOT_COMPLETE, Ordering::SeqCst);
        *self.became_leader_at.write() = None;
    }
}

/// The lease loop. Spawn this via `spawn_monitored` in main.rs.
///
/// Never returns (barring panic). On each tick:
/// - `try_acquire_or_renew`: creates the Lease if it doesn't
///   exist, or updates `renewTime` if we hold it, or returns
///   "not leading" if someone else holds it.
/// - On acquire transition (was standby, now leading): derive the
///   generation from the lease's transition count, flip `is_leader`,
///   fire-and-forget `LeaderAcquired` to the actor. The actor's
///   `handle_leader_acquired` runs recovery then sets
///   `recovery_complete=true`. CRITICAL: this loop does NOT
///   block on recovery — it keeps renewing the lease every 5s
///   regardless. A loop blocked on a slow recovery could neither
///   renew nor self-fence while a standby steals after
///   `STEAL_AFTER` of observed staleness → dual-leader.
/// - On lose transition (was leading, now not): flip `is_leader`,
///   clear `recovery_complete` (re-acquire re-triggers recovery).
///   DON'T touch the generation — the NEW leader's steal bumps the
///   lease's transition count, which is where generations come from.
///   We don't know the new gen.
/// - On a rebound (still leading, but the observed `leaseTransitions`
///   count differs from the recorded one — a holder change landed
///   entirely inside our observation gap): re-record the count,
///   re-derive the generation, clear `recovery_complete`, and re-fire
///   the on-acquire hook so the consumer re-runs recovery. No lose
///   hook fires (`sched.lease.rebound`).
///
/// On K8s API error (apiserver restarting, network blip): log
/// warn and retry next tick. Don't crash — a transient API hiccup
/// shouldn't kill the scheduler. If the error persists past
/// `SELF_FENCE_AFTER`, the local self-fence flips `is_leader`; a
/// standby steals after `STEAL_AFTER` of observed staleness and
/// takes over, which is exactly the desired behavior for "this
/// replica's K8s connectivity is broken."
///
/// `hooks`: per-component callbacks (metrics + actor notification).
/// Called synchronously on the transition edge — see [`LeaseHooks`]
/// for the non-blocking constraint.
pub async fn run_lease_loop<H: LeaseHooks>(
    cfg: LeaseConfig,
    state: LeaderState,
    hooks: H,
    shutdown: rio_common::signal::Token,
) {
    // kube client from in-cluster config. If this fails (not in
    // a pod, or service account not mounted), log and exit the
    // loop — spawn_monitored logs the task death. The scheduler
    // keeps running with `is_leader=false` → never dispatches →
    // effectively a standby. Not useful but not broken either;
    // the OTHER replica (with working kube access) leads.
    let client = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "kube client init failed; lease loop exiting (this replica will never lead)");
            return;
        }
    };

    run_lease_loop_with_client(
        client,
        cfg,
        state,
        hooks,
        shutdown,
        clock::suspend_aware_now,
    )
    .await;
}

/// The lease loop proper, with the [`kube::Client`] injected. Everything
/// [`run_lease_loop`] documents happens here; the public wrapper only
/// constructs the in-cluster client. Split so tests can drive the real
/// loop against an in-process mock apiserver
/// (`rio_test_support::kube_mock::MockApiServer`) under a paused clock —
/// the fence-check-cadence test in `mod tests` is the consumer.
///
/// `fence_now`: the self-fence blind-time clock, injected like the kube
/// client so tests control it. Production ([`run_lease_loop`]) passes
/// [`clock::suspend_aware_now`] (CLOCK_BOOTTIME on Linux — advances
/// across host suspend); paused-clock tests pass a tokio-Instant-anchored
/// closure so the measurement follows the virtual clock; the fence-jump
/// test adds a controlled offset to simulate suspend.
/// THE observe-completed-read body for a held lease while believing
/// (`sched.lease.rebound`): every consumer that records a believed-held
/// observation routes through here — the Leading renew round, the
/// own-commit evidence arm, and both acquire edges (whose just-recorded
/// count makes the comparison a structural no-op) — so the
/// transitions-vs-recorded rebound comparison is fused to recording the
/// observation. The fusion is a MACHINE property, not a convention:
/// `LeaseStanding::on_observed_held` demands the [`ObservedHeld`]
/// witness and this function is its only production minting site, so
/// "a sibling consumer of the same facts skipped the rebound law" does
/// not typecheck (merged_bug_114).
///
/// The rebound rationale (hoisted from the Completed arm, where it
/// applies identically): steady state is `observed_transitions ==
/// recorded` — renews never bump the count, and a foreign holder still
/// present at the next successful round resolves through the lose edge
/// instead — so an unequal value here means a holder change (foreign
/// term + vacate, or delete/recreate) landed entirely inside our
/// observation gap. Synthesize the missing transition: re-derive local
/// state and re-fire the rebound hook so the consumer re-runs recovery
/// against the post-term state. Deliberately NO `on_lose()`: a
/// synthesized lose would force a pointless wipe of state the
/// immediately-following re-recovery rebuilds, while `on_rebound`'s own
/// `recovery_complete` clear already gates dispatch during the
/// re-recovery. The leader MARKS, by contrast, MUST be re-dirtied: a
/// moved count means a foreign term ran to completion inside the gap,
/// and a foreign holder's reconcile GUARANTEES a sweep stripped our
/// label and zeroed our cost. The count-coincidence ABA (the observed
/// value lands back exactly on the recorded one) remains the accepted
/// residual, bounded for the marks half by the periodic verify
/// (`sched.lease.marks-verify`). Equal counts stay a silent no-op (a
/// log every 5s would be noisy).
// r[impl sched.lease.rebound+4]
fn observe_held_while_believing<H: LeaseHooks>(
    state: &LeaderState,
    standing: &mut LeaseStanding,
    marks_dirty: &DirtyGen,
    hooks: &H,
    holder_id: &str,
    observed_transitions: u64,
) {
    let recorded = state.acquired_transitions();
    if observed_transitions != recorded {
        // The rebound re-dirty: a guaranteed-foreign-sweep falsifier
        // handled at its edge; the verify pass bounds the rest.
        marks_dirty.mark();
        let new_gen = state.on_rebound(observed_transitions);
        warn!(
            recorded,
            observed = observed_transitions,
            generation = new_gen,
            holder = %holder_id,
            "lease transition count moved while still leading — \
             unobserved holder change inside our observation gap; \
             re-running recovery"
        );
        hooks.on_rebound();
    }
    standing.on_observed_held(ObservedHeld(()));
}

/// Machine witness for the believing→not-believing lose edge on a
/// COMPLETED round (`sched.lease.holder-evidenced-lose`): the lose-edge
/// body is reachable only through a value of this type, and the two
/// constructors are the two evidence shapes the signed decision admits.
/// A bare first 409 has NO constructor — its only arm is the one-round
/// deferral, which runs no lose edge. (Self-fence and cooperative
/// step-down are separate, non-completed-round edges with their own
/// laws.)
///
/// SIGNED 2026-06-08 (owner, bughunt-4 fix-wave §5-S Q3): holder-
/// evidenced lose — a 409 defers one round; the lose edge requires the
/// next GET to name a DIFFERENT holder (typed evidence on the edge).
/// Cost accepted: ~seconds of deferred step-down in true-loss cases,
/// vs eliminating spurious DAG/outbox wipes on own-write races.
/// Formal: leaderElection.qnt `loseRequiresHolderEvidence` invariant +
/// falsify twin (lease-085-blind-conflict-lose).
enum CompletedLoseEvidence {
    /// A completed read resolved Standby: the apiserver named another
    /// holder (or the observed record says the holder is fresh and it
    /// is not us). Direct holder evidence.
    AnotherHolderObserved,
    /// Two consecutive believing rounds bounced our CAS: the one-round
    /// deferral the signed decision grants is exhausted. Bounded
    /// indirect evidence — retaining belief longer would erode the
    /// NeverDual fence/steal separation.
    ConflictDeferralExhausted,
}

/// The own-commit evidence baseline (`sched.lease.cancelled-write`):
/// what the last COMPLETED read observed of the lease's holder-authored
/// content. Three-state (merged_bug_164): a completed read that
/// observed ABSENCE — the 404 before a Create — is a recorded
/// observation, distinct from "no completed read yet". The old
/// two-state `Option` conflated them, so the round that first proved a
/// Create committed (holder=us after a witnessed 404) could never
/// consume the ledger: absence→present-naming-us IS content movement
/// (only our POST installs our holderIdentity; a racing creator's
/// 409-winning lease carries theirs).
enum ContentBaseline {
    /// No completed read yet: loop start, or a Completed round on the
    /// Create path (the POST left no read facts and Leading already
    /// answered the ledger wholesale) — the next read re-baselines.
    NoCompletedRead,
    /// A completed read observed no lease object (404).
    Absent,
    /// A completed read observed a lease with these holder-authored
    /// `renewTime` bytes (None when the field itself was absent).
    Present { renew_time: Option<String> },
}

pub(crate) async fn run_lease_loop_with_client<H: LeaseHooks>(
    client: kube::Client,
    cfg: LeaseConfig,
    state: LeaderState,
    hooks: H,
    shutdown: rio_common::signal::Token,
    fence_now: impl Fn() -> Duration + Send + 'static,
) {
    // Clone for leader-mark (pod-deletion-cost annotation + leader
    // label) patching. LeaderElection::new takes ownership (wraps the
    // client in Api<Lease>).
    let pod_patch_client = client.clone();

    let mut election = LeaderElection::new(
        client,
        &cfg.namespace,
        cfg.lease_name.clone(),
        cfg.holder_id.clone(),
        LEASE_TTL,
        STEAL_AFTER,
    );

    info!(
        lease = %cfg.lease_name,
        namespace = %cfg.namespace,
        holder = %cfg.holder_id,
        ttl_secs = LEASE_TTL.as_secs(),
        self_fence_secs = SELF_FENCE_AFTER.as_secs(),
        steal_after_secs = STEAL_AFTER.as_secs(),
        "lease loop starting"
    );

    let mut standing = LeaseStanding::new();
    // Level-triggered pod-marks reconciliation. `true` means "this
    // pod's leader marks (deletion-cost annotation + optional leader
    // label) may not match its actual leadership — re-patch on the
    // next reachable election round-trip". The label half is
    // LOAD-BEARING for the dashboard data path (the leader-only
    // Service selects on it), so a single dropped PATCH must not leave
    // it wrong until the next leadership transition. Set on:
    //   - startup (init true): an in-place container restart keeps the
    //     pod object and its labels, so a previous incarnation's marks
    //     may still be there. A genuinely fresh pod pays one no-op
    //     PATCH (cost "0" ≡ absent label ≡ removing nothing).
    //   - every acquire/lose transition, including the self-fence
    //     (which cannot patch — the apiserver is unreachable — so the
    //     flag IS the deferred debt the old `owe_cost_clear` carried).
    //   - implicitly kept set by a failed PATCH: the spawned task
    //     clears only THROUGH the mark snapshot the loop took at its
    //     spawn ([`DirtyGen::clear_through`]), so a failure simply
    //     never clears — and any dirtying event that lands after the
    //     snapshot (a transition, a rebound, a verify divergence)
    //     survives the clear by arithmetic, with no per-edge ordering
    //     premise.
    // Arc: the detached patch task owns a clone and clears it on
    // success.
    let marks_dirty = Arc::new(DirtyGen::new_dirty());
    // Single-flight slot for the marks PATCH task: owned by the loop
    // (which takes it before spawning) plus the one in-flight task,
    // which releases it on completion via a Drop guard whose release
    // runs AFTER the task's dirty-flag decision (end of scope) — a
    // freed slot always means the flag already reflects that task's
    // outcome.
    let marks_patch_in_flight = Arc::new(AtomicBool::new(false));
    // R3: the in-flight marks task's handle (reconcile or verify; the
    // single-flight slot means at most one). Aborted at loop exit so a
    // parked round-trip cannot outlive the loop and write marks (or
    // sweep a successor's) after shutdown; SlotRelease's Drop runs on
    // abort, so the slot is released either way.
    let mut inflight_marks: Option<tokio::task::JoinHandle<()>> = None;
    // Blind-window clock on the injected fence clock (production:
    // CLOCK_BOOTTIME — advances across host suspend, so a suspend
    // straddling SELF_FENCE_AFTER fences at the first post-resume
    // tick). Stamped only with attempt-START anchors; see BlindClock.
    let mut blind = BlindClock::starting_at(RenewAnchor::mint(&fence_now));
    // r[impl sched.lease.cancelled-write+2]
    // The cancelled-write ledger and its evidence cursor: `unconfirmed`
    // records the oldest transmitted-but-unanswered mutating act;
    // `content_baseline` is what the last COMPLETED read observed of
    // the holder-authored content — three-state, so a read that
    // observed ABSENCE (the 404 before a Create) is a recorded
    // baseline, distinct from "no completed read yet" (merged_bug_164;
    // see ContentBaseline). Keyed on protocol-authored content, never
    // on resourceVersion: a foreign metadata patch moves the rv
    // without any write of OURS committing (merged_bug_180).
    let mut unconfirmed: Option<UnconfirmedPut> = None;
    let mut content_baseline = ContentBaseline::NoCompletedRead;
    // Q3 deferral flag: a believing renew 409 has been observed and its
    // resolution is owed to the NEXT completed read. Set only by the
    // deferral arm; cleared by every resolving arm (acquire, lose,
    // still-leading observe, standby observe). One round deep by
    // construction — a second consecutive believing 409 exhausts it.
    let mut conflict_deferred = false;
    let mut interval = tokio::time::interval(RENEW_INTERVAL);
    // Skip: if one renewal is slow (apiserver busy), don't fire
    // twice immediately. SELF_FENCE_AFTER is 11s; we have slack.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Stateful loop (was_leading, last_successful_renew are
    // cross-tick): not spawn_periodic. biased; inlined per
    // r[common.task.periodic-biased].
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }

        // Tick-time fence check, BEFORE the renew attempt. The error arm
        // below re-evaluates when a failed attempt resolves, but that
        // evaluation can lag the tick by up to the attempt deadline; the
        // NeverDual derivation on FENCE_MARGIN assumes the fence-check
        // latency is at most one tick, and this call is what delivers it.
        if maybe_self_fence(
            &state,
            &mut standing,
            &marks_dirty,
            blind.blind_for(fence_now()),
        ) {
            hooks.on_lose();
        }

        // r[impl sched.recovery.step-down+2]
        // Cooperative step-down: a
        // consumer that cannot serve its tenure (failed recovery)
        // requested a local demotion. Belief is evaluated FIRST and the
        // request is consumed only inside a believing tick — a
        // lose/self-fence that landed at the tick top leaves the
        // request armed for the next believing tick of the SAME tenure
        // instance. Consumption is INSTANCE-keyed (merged_bug_128):
        // `take_step_down_request` serves the request only when its
        // stamp names the current per-acquire instance and drops a
        // stale one — and on_acquire/on_rebound clear the slot
        // outright, so a false-alarm fence followed by a same-count
        // re-acquire starts its successor instance CLEAN. A recovery
        // that fails again under the successor re-requests with the
        // new stamp; recovery #1's demotion can never fire against a
        // successor whose recovery #2 succeeded.
        // The release is holder-guarded and 409/404-tolerant like the
        // shutdown release; on success the round observably resolved
        // not-leading (standing.on_observed_not_leading()); on failure the
        // apiserver state is unknown — believe-clear only, hold kept
        // (the self-fence posture), so a later shutdown still
        // releases. Either way the full lose-edge effects run
        // (state, hook, marks) and the NEXT tick resumes candidacy —
        // try_acquire_or_renew steals/acquires normally.
        if standing.believes() && state.take_step_down_request(state.acquired_instance()) {
            warn!(
                holder = %cfg.holder_id,
                "cooperative step-down requested (tenure cannot serve); releasing the lease"
            );
            let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
            match tokio::time::timeout(deadline, election.step_down()).await {
                Ok(Ok(())) => {
                    info!("step-down: lease released; resuming candidacy next tick");
                    standing.on_observed_not_leading();
                }
                Ok(Err(e)) => {
                    warn!(error = %e,
                          "step-down release failed; the next replica will steal in {}s",
                          STEAL_AFTER.as_secs());
                    standing.on_self_fence();
                }
                Err(_) => {
                    warn!(
                        ?deadline,
                        "step-down release timed out; the next replica will steal in {}s",
                        STEAL_AFTER.as_secs()
                    );
                    standing.on_self_fence();
                }
            }
            state.on_lose();
            hooks.on_lose();
            marks_dirty.mark();
            continue;
        }

        let renew_deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        // Round id BEFORE the attempt starts: a consumer that snapshots
        // `renew_rounds_started` and later sees `last_leading_round`
        // above it knows the confirming round began after the snapshot
        // (sched.recovery.bump-confirm).
        let round = state.begin_renew_round();
        // The blind-window anchor for this attempt: minted BEFORE the
        // await, so anchor <= send <= apiserver commit. A suspend that
        // straddles the in-flight round-trip is therefore part of the
        // stamped window — the response's arrival time never enters the
        // blind clock (there is no API for it).
        let attempt_anchor = RenewAnchor::mint(&fence_now);
        match election
            .renew_phased(RENEW_PHASE_DEADLINE, RENEW_PHASE_DEADLINE)
            .await
        {
            election::RenewOutcome::Completed { result, facts } => {
                // Successful round-trip (apiserver answered). Even
                // Standby/Conflict restart the blind window — we KNOW
                // the apiserver answered, we just don't hold the lease.
                // The clock tracks "am I blind", not "am I leader".
                blind.stamp(attempt_anchor);
                let is_conflict = matches!(result, ElectionResult::Conflict);
                // Leading/Standby answer the unconfirmed question
                // wholesale: their facts describe the post-resolution
                // state, so nothing of ours is left in doubt. A 409
                // does NOT — it proves only that the rv moved between
                // our GET and PUT, and the mover may be our own zombie
                // commit from the cancelled-write ledger (or a foreign
                // metadata patch). The ledger survives a Conflict so
                // the NEXT completed read can consume it as own-commit
                // evidence (sched.lease.holder-evidenced-lose).
                if !is_conflict {
                    unconfirmed = None;
                }
                content_baseline = match facts {
                    Some(f) => ContentBaseline::Present {
                        renew_time: f.renew_time,
                    },
                    // Completed Create: the POST left no read facts,
                    // and Leading just answered the ledger wholesale —
                    // the next read re-baselines. (The pre-POST 404 is
                    // stale the instant the Create succeeds.)
                    None => ContentBaseline::NoCompletedRead,
                };
                // Conflict on renew = the CAS bounced; WHO holds is
                // unknown until the next read (the deferral arm below).
                // Conflict on steal = another standby raced us → we
                // were never leading, nothing to defer. Leading carries
                // the lease's transition count so the acquire arm can
                // derive the generation from it; None ⇔ not leading.
                let leading_transitions = match result {
                    ElectionResult::Leading { transitions } => Some(transitions),
                    ElectionResult::Standby | ElectionResult::Conflict => None,
                };
                let now_leading = leading_transitions.is_some();

                // Edge detection on (leading?, was_leading). Binding
                // the transition count in the acquire pattern makes
                // "acquired without a transition count to derive the
                // generation from" unrepresentable — the arm cannot
                // execute without a value to hand to on_acquire.
                match (leading_transitions, standing.believes()) {
                    (Some(transitions), false) => {
                        // ---- Acquire transition ----
                        // on_acquire: write the generation FIRST, then
                        // set is_leader, both SeqCst. A reader seeing
                        // is_leader=true also sees the new generation.
                        // The other order would let dispatch run with
                        // is_leader=true but OLD generation for one
                        // pass — harmless (workers compare heartbeat
                        // gen, not assignment gen, for staleness) but
                        // conceptually wrong.
                        //
                        // The generation derives from the lease's
                        // transition count (see on_acquire's doc).
                        let new_gen = state.on_acquire(transitions);
                        info!(
                            generation = new_gen,
                            holder = %cfg.holder_id,
                            "acquired leadership"
                        );

                        // r[impl sched.lease.deletion-cost+3]
                        // The leader marks (pod-deletion-cost=1 so K8s
                        // kills the standby first on scale-down, plus
                        // the optional leader label the
                        // rio-scheduler-leader Service selects on) no
                        // longer match this pod — the reconcile arm
                        // after the edge match patches them this same
                        // tick.
                        marks_dirty.mark();

                        // r[impl sched.lease.non-blocking-acquire+2]
                        // Fire the per-component on-acquire hook
                        // (metrics + actor notification). The hook MUST
                        // NOT block — see LeaseHooks doc.
                        //
                        // NON-BLOCKING IS LOAD-BEARING: if a hook
                        // awaited recovery, a slow recovery for a
                        // large DAG would stall the renewal tick —
                        // the blocked loop can neither renew nor
                        // self-fence while a standby steals after
                        // STEAL_AFTER of observed staleness →
                        // dual-leader. Hooks spawn; the separate
                        // recovery-completion gate lets the loop keep
                        // renewing regardless.
                        hooks.on_acquire();
                        // Record the believed-held observation THROUGH
                        // the funnel (merged_bug_114): on_acquire just
                        // stored this count, so the rebound comparison
                        // no-ops by construction — and a consumer that
                        // skipped the funnel would not typecheck (the
                        // ObservedHeld witness has no other minting
                        // site).
                        observe_held_while_believing(
                            &state,
                            &mut standing,
                            &marks_dirty,
                            &hooks,
                            &cfg.holder_id,
                            transitions,
                        );
                        // An acquire resolves any pending deferral —
                        // a new belief episode starts clean.
                        conflict_deferred = false;
                    }
                    (None, true) => {
                        // ---- Lose-or-defer ----
                        // r[impl sched.lease.holder-evidenced-lose]
                        // The lose edge demands a CompletedLoseEvidence
                        // witness (see its doc + the SIGNED Q3 block):
                        // a completed read naming another holder, or an
                        // exhausted one-round 409 deferral. A bare
                        // first 409 constructs neither — it defers.
                        let evidence: Option<CompletedLoseEvidence> = if !is_conflict {
                            // Standby resolution: the read named a
                            // fresh foreign holder (or an empty/stale
                            // one we chose not to steal) — direct
                            // holder evidence either way: the lease
                            // provably no longer resolves to us.
                            Some(CompletedLoseEvidence::AnotherHolderObserved)
                        } else if conflict_deferred {
                            Some(CompletedLoseEvidence::ConflictDeferralExhausted)
                        } else {
                            None
                        };
                        match evidence {
                            Some(evidence) => {
                                // ---- Lose transition ----
                                // Stop dispatching. The generation is
                                // the NEW leader's concern — it derives
                                // its own from the lease's transition
                                // count on acquire. on_lose clears both
                                // is_leader and recovery_complete
                                // (SeqCst): if we re-acquire, recovery
                                // runs again — the other replica's
                                // actions may have changed PG.
                                state.on_lose();
                                match evidence {
                                    CompletedLoseEvidence::AnotherHolderObserved => warn!(
                                        holder = %cfg.holder_id,
                                        "lost leadership (a completed read named another holder)"
                                    ),
                                    CompletedLoseEvidence::ConflictDeferralExhausted => warn!(
                                        holder = %cfg.holder_id,
                                        "lost leadership (two consecutive renew 409s — the \
                                         one-round holder-evidence deferral is exhausted)"
                                    ),
                                }

                                // r[impl sched.lease.standby-tick-noop+2]
                                // Symmetric with on_acquire above: fire
                                // the per-component on-lose hook
                                // (metrics + actor notification). Same
                                // non-blocking constraint. is_leader is
                                // already false (above) so the
                                // consumer's tick early-returns
                                // regardless; this just lets it drop
                                // stale state and zero leader-only
                                // gauges.
                                hooks.on_lose();

                                // The leader marks must be cleared —
                                // we're standby now: K8s should prefer
                                // to kill us over the new leader, and
                                // the leader-only Service must stop
                                // routing to us (the dashboard's RPCs
                                // would land here as Trailers-Only
                                // Unavailable otherwise). The reconcile
                                // arm after the edge match patches this
                                // tick.
                                marks_dirty.mark();
                                standing.on_observed_not_leading();
                                conflict_deferred = false;
                            }
                            None => {
                                // ---- One-round deferral ----
                                // Belief, the hold, and the ledger all
                                // survive: nothing is wiped on a CAS
                                // bounce that may be our own write
                                // committing. No hooks, no marks — no
                                // transition happened. The NEXT
                                // completed read resolves: holder=us →
                                // ordinary renew (or own-commit
                                // evidence on the act-failed path);
                                // holder=other → lose WITH evidence;
                                // another 409 → exhausted, lose.
                                conflict_deferred = true;
                                warn!(
                                    holder = %cfg.holder_id,
                                    "renew 409 while believing: rv moved but the holder is \
                                     unknown — deferring the lose edge one round for holder \
                                     evidence (own zombie commit and foreign metadata writes \
                                     are non-lose rv-movers)"
                                );
                            }
                        }
                    }
                    (Some(transitions), true) => {
                        // ---- Still leading ----
                        // THE observe-completed-read body: the rebound
                        // comparison and the believed-held observation
                        // are one fused operation (see the funnel's doc
                        // for the full rationale).
                        observe_held_while_believing(
                            &state,
                            &mut standing,
                            &marks_dirty,
                            &hooks,
                            &cfg.holder_id,
                            transitions,
                        );
                        // A still-leading resolution clears a pending
                        // deferral — the 409's question is answered.
                        conflict_deferred = false;
                    }
                    // Steady state: still standby while someone else
                    // holds (or a 409 raced our steal — we were never
                    // leading, nothing to defer). No log — 5s interval
                    // would be noisy.
                    (None, false) => {
                        standing.on_observed_not_leading();
                        conflict_deferred = false;
                    }
                }
                // r[impl sched.recovery.bump-confirm+3]
                // Confirm AFTER the edge-detection match: when an
                // acquire edge and a confirmation land in the same
                // round, on_acquire's stores are already visible by
                // the time the confirmation is observable. Standby/
                // Conflict rounds (and the error/timeout arm below)
                // are never confirmed.
                if now_leading {
                    state.confirm_leading_round(round);
                }
            }
            election::RenewOutcome::FetchedActFailed {
                facts,
                put_transmitted,
                error,
            } => {
                match &error {
                    Some(e) => warn!(
                        error = %e,
                        put_transmitted,
                        "lease act phase failed after a completed read; retrying next tick"
                    ),
                    None => warn!(
                        deadline = ?RENEW_PHASE_DEADLINE,
                        put_transmitted,
                        "lease act phase timed out after a completed read; retrying next tick"
                    ),
                }

                // r[impl sched.lease.cancelled-write+2]
                // Own-commit evidence: the read completed, so we have a
                // fresh holder/content view even though the write phase
                // died. If the lease names US and the holder-authored
                // `renewTime` bytes moved since our last completed
                // read, some write of OURS committed (only our own
                // renew/steal writes `renewTime` while leaving us as
                // holder) — consume the ledger and stamp the blind
                // clock at the LEDGER's anchor (anchor ≤ send ≤ commit;
                // never the observing read's time). Frozen content
                // stamps NOTHING — even under foreign rv churn: an
                // annotation patch moves the rv without any write of
                // OURS committing, so it is not evidence
                // (merged_bug_180; the foreign-rv companion test pins
                // this direction alongside the frozen-rv one).
                if let Some(f) = &facts {
                    // Content movement against the three-state baseline
                    // (merged_bug_164): Present compares the bytes;
                    // Absent→a lease naming us is our POST committing
                    // (the outer holder_is_us carries the naming
                    // requirement); NoCompletedRead can prove nothing.
                    let content_moved = match &content_baseline {
                        ContentBaseline::Present { renew_time: prev } => prev != &f.renew_time,
                        ContentBaseline::Absent => true,
                        ContentBaseline::NoCompletedRead => false,
                    };
                    if f.holder_is_us
                        && content_moved
                        && let Some(led) = unconfirmed.take()
                    {
                        blind.stamp(led.anchor);
                        // merged_bug_122: the stamp is unconditional
                        // (the evidence is real and the ledger is
                        // consumed) but the ACQUIRE is fence-gated. A
                        // ledger anchor already past SELF_FENCE_AFTER
                        // at consumption time makes the acquire
                        // provably futile — the trailing
                        // maybe_self_fence in this same arm would
                        // re-fence it before the next await, churning
                        // hooks/marks/recovery once per round of a
                        // slow-commit brownout.
                        let anchor_age = blind.blind_for(fence_now());
                        if anchor_age > SELF_FENCE_AFTER {
                            info!(
                                ?anchor_age,
                                "own-commit evidence consumed, but its anchor is already \
                                 past the fence deadline — staying fenced (an acquire \
                                 here would be re-fenced this same arm)"
                            );
                        } else if !standing.believes() {
                            // A self-fence inside the mid-band window
                            // already ran the lose edge; the evidence
                            // proves the apiserver still (or again)
                            // names us holder, so re-enter through the
                            // ordinary acquire edge with the FETCHED
                            // transition count — the same-count case is
                            // the documented same-epoch re-acquire
                            // (generation fetch_max no-op, in-flight
                            // work survives). The bump-confirmation is
                            // deliberately NOT run here: confirmation
                            // stays a completed-round property
                            // (sched.recovery.bump-confirm), and
                            // pre-fix this regime fenced outright —
                            // strictly worse than a gated recovery.
                            let new_gen = state.on_acquire(f.transitions);
                            info!(
                                generation = new_gen,
                                holder = %cfg.holder_id,
                                "own-commit evidence (holder=us, renewTime moved) restored \
                                 leadership belief after an abandoned write"
                            );
                            marks_dirty.mark();
                            hooks.on_acquire();
                            // Counts as a Leading observation: the read
                            // is apiserver-authoritative about the hold.
                            // Routed THROUGH the funnel (merged_bug_114)
                            // — the rebound comparison no-ops on the
                            // just-recorded count, and the ObservedHeld
                            // witness keeps a funnel-skipping sibling
                            // consumer untypeable.
                            observe_held_while_believing(
                                &state,
                                &mut standing,
                                &marks_dirty,
                                &hooks,
                                &cfg.holder_id,
                                f.transitions,
                            );
                        } else {
                            // Still believing: this is a completed read
                            // of a lease we hold — the SAME facts the
                            // Leading renew round consumes, so it routes
                            // through the SAME observe-completed-read
                            // body. A foreign term that completed inside
                            // the observation gap (moved count) rebounds
                            // here exactly as it would on a Completed
                            // round; pre-fix this leg skipped the
                            // comparison and the term was never repaired
                            // (no round Completes in the reads-complete/
                            // acts-fail regime, and the evidence
                            // re-stamps defeat the self-fence).
                            observe_held_while_believing(
                                &state,
                                &mut standing,
                                &marks_dirty,
                                &hooks,
                                &cfg.holder_id,
                                f.transitions,
                            );
                        }
                    }
                    content_baseline = ContentBaseline::Present {
                        renew_time: f.renew_time.clone(),
                    };
                } else {
                    // 404→Create round whose POST died unanswered: the
                    // read OBSERVED absence — that is a baseline
                    // observation (merged_bug_164), and it is exactly
                    // what lets the next completed read prove the
                    // Create committed (holder=us against Absent).
                    content_baseline = ContentBaseline::Absent;
                }

                // Record THIS round's transmitted write as in doubt —
                // kept at the OLDEST anchor: an existing unconsumed
                // entry is never overwritten (if several writes are in
                // doubt, the blind window must cover the eldest).
                if put_transmitted && unconfirmed.is_none() {
                    unconfirmed = Some(UnconfirmedPut {
                        anchor: attempt_anchor,
                    });
                }

                // The fence still arbitrates: evidence (above) is the
                // only thing that stamps on this path, so a regime
                // where writes stop committing fences on the same
                // schedule as a full outage.
                if maybe_self_fence(
                    &state,
                    &mut standing,
                    &marks_dirty,
                    blind.blind_for(fence_now()),
                ) {
                    hooks.on_lose();
                }
            }
            election::RenewOutcome::FetchFailed { error } => {
                // No fresh view of the Lease object at all — and,
                // because a mutating request is only ever transmitted
                // after a completed read (renew_phased's phase order),
                // provably nothing was sent: no ledger entry, the rv
                // freezes for stealers, and this replica is exactly as
                // blind as it looks.
                match &error {
                    Some(e) => {
                        warn!(error = %e, "lease renew failed (apiserver error); retrying next tick");
                    }
                    None => {
                        warn!(deadline = ?RENEW_PHASE_DEADLINE, "lease read phase TIMED OUT (apiserver hung?); retrying next tick");
                    }
                }
                //
                // Local self-fence: if SELF_FENCE_AFTER has elapsed
                // since the last SUCCESSFUL round-trip, flip is_leader
                // locally. SELF_FENCE_AFTER is 2×FENCE_MARGIN earlier
                // than any follower's steal threshold (STEAL_AFTER), so
                // by the time a replica that CAN reach the apiserver
                // steals, we have already stopped believing — that
                // ordering is the neverDual proof in the
                // leaderElectionAsymmetric regime of
                // docs/spec/models/leaderElection.qnt. This is the
                // SECOND fence evaluation in the tick: the tick-time
                // check at the top of the loop body is what bounds the
                // fence-check latency to one tick; this one just fires
                // earlier when a failed attempt resolves before the
                // next tick.
                //
                // The old "DON'T flip — apiserver down for EVERYONE"
                // argument is wrong once elapsed > the fence deadline.
                // In the symmetric-partition case (nobody reaches the
                // apiserver) flipping costs nothing: workers can't be
                // scheduled anyway. In the asymmetric case (WE are
                // partitioned, peer is not) NOT flipping makes us a
                // stale-assignment noise generator. Worker-side
                // generation fence (r[sched.lease.generation-fence+3])
                // saves correctness either way; this fence saves ops
                // sanity.
                // attempt_anchor is dropped on this path: a failed
                // round-trip stamps nothing and the window keeps aging.
                if maybe_self_fence(
                    &state,
                    &mut standing,
                    &marks_dirty,
                    blind.blind_for(fence_now()),
                ) {
                    // Self-fence is a lose-transition: same on-lose
                    // hook as the explicit lose arm above.
                    hooks.on_lose();
                }
            }
        }

        // r[impl sched.lease.deletion-cost+3]
        // Level-triggered leader-marks reconcile — hoisted AFTER the
        // whole outcome match (merged_bug_122) so EVERY arm services
        // the dirt it minted this tick: the Completed edges, the
        // tick-top and in-arm self-fences, and the evidence-acquire
        // leg of the acts-fail arm — whose own doc names the regime
        // where no round Completes, which is exactly why a spawn that
        // lived only inside the Completed arm left that leg's dirt
        // structurally unserviceable. Polarity reads the post-arm
        // leadership state. A transition on THIS tick patches the
        // post-transition state (and the self-fence false-alarm pair —
        // fence at the top of the tick, then a successful renew —
        // never patches the label off). At most one leader-marks PATCH
        // is in flight at a time; `marks_dirty` persists across
        // skipped ticks and is cleared only by a completed patch whose
        // written polarity still matches the current desire, so the
        // first tick after a release retries with the then-current
        // polarity. Steady state spawns nothing. Detached because the
        // lease loop MUST NOT block on the pod PATCH (same constraint
        // as the hooks); each attempt is bounded by the renew deadline
        // so a wedged PATCH cannot hold the single-flight slot
        // forever.
        let now_leading = state.is_leader();
        if let Some(task) = maybe_spawn_leader_marks(
            &pod_patch_client,
            &cfg,
            now_leading,
            &marks_dirty,
            &marks_patch_in_flight,
            renew_deadline,
        ) {
            inflight_marks = Some(task);
        }

        // r[impl sched.lease.marks-verify]
        // Bounded-cadence verification: every MARKS_VERIFY_EVERY
        // rounds a clean-and-leading loop re-reads its OWN Pod and
        // compares the stored marks against its leadership, re-dirtying
        // on divergence — the level-triggered closure over falsifiers
        // nobody enumerated (a foreign sweep racing a re-acquire,
        // kubectl, a future actor). Shares the marks single-flight
        // slot, so verify and reconcile never interleave; the dirty
        // short-circuit inside the gate means a divergence found here
        // is repaired by the ordinary reconcile on the NEXT round-trip.
        if now_leading
            && round.is_multiple_of(MARKS_VERIFY_EVERY)
            && let Some(task) = maybe_spawn_verify_leader_marks(
                &pod_patch_client,
                &cfg,
                &marks_dirty,
                &marks_patch_in_flight,
                renew_deadline,
            )
        {
            inflight_marks = Some(task);
        }
    }

    // R3: abort the in-flight marks task BEFORE the graceful release —
    // the loop's lifetime owns it, and a parked reconcile surviving the
    // loop could land marks writes (or sweep the successor) long after
    // this replica stopped mattering. Abort-safety is pinned by
    // aborting_inflight_marks_task_releases_slot_and_keeps_dirty.
    if let Some(task) = inflight_marks.take() {
        task.abort();
    }

    // r[impl sched.lease.graceful-release+2]
    // Graceful release: on shutdown, release the lease so the next
    // replica acquires on its next poll tick (one RENEW_INTERVAL, 5s)
    // instead of waiting out the steal threshold (19s). Gate on the
    // HOLD — acquired and not since observed superseded — never on
    // belief: a local self-fence clears belief while the Lease may
    // still name us at the apiserver, and fence-then-SIGTERM is
    // precisely when the release matters most (without it the
    // successor waits out the full steal threshold). step_down()
    // itself is holder-guarded and 409/404-tolerant, so a stale hold
    // costs one harmless round-trip. The skip remains for "standby
    // all along" and "supersession already observed" (on_observed
    // clears the hold). Any error is non-fatal: we're shutting down
    // regardless, and the steal threshold is the fallback.
    if standing.should_release_on_shutdown() {
        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        match tokio::time::timeout(deadline, election.step_down()).await {
            Ok(Ok(())) => info!("released lease on shutdown"),
            Ok(Err(e)) => warn!(
                error = %e,
                "step_down failed; the next replica will steal in {}s",
                STEAL_AFTER.as_secs()
            ),
            Err(_) => warn!(
                ?deadline,
                "step_down timed out (apiserver hung?); the next replica will steal in {}s",
                STEAL_AFTER.as_secs()
            ),
        }
    }
    debug!("lease loop exited");
}

/// The two leadership facts the lease loop used to conflate in one
/// `was_leading` bool, split so each consumer reads the fact it
/// actually means (bug_387's class — a gate reading belief where it
/// needs the hold — is unrepresentable: the fields are private, the
/// fence path has no API that touches the hold, and the release gate
/// has no API that reads belief):
///
/// - **believes** — "do I currently think I lead". The edge-detection
///   input. Cleared by the local self-fence, which writes NOTHING to
///   the apiserver.
/// - **held_unsuperseded** — "did I acquire this lease, and have I not
///   since observed a completed round resolving not-leading". The
///   graceful-release gate. NOT cleared by the self-fence: fencing is
///   a local belief change and the Lease may still name us at the
///   apiserver — exactly the case the shutdown release exists for
///   (fence during an outage, then SIGTERM: without the release the
///   successor waits out the full steal threshold).
struct LeaseStanding {
    believes: bool,
    held_unsuperseded: bool,
}

/// Witness that a believed-held observation passed through THE
/// observe-completed-read body (`observe_held_while_believing`) — the
/// fused rebound-comparison + observation. The only production minting
/// site is that function, which is what upgrades its doc's "a sibling
/// consumer skipping the rebound law is unwritable" from convention to
/// structure (merged_bug_114): `LeaseStanding::on_observed_held`
/// demands this value, and a consumer that did not run the funnel has
/// no way to construct it. (Tests and the kani standing proofs mint
/// through a cfg-gated driver that delegates to the same production
/// transitions.)
struct ObservedHeld(());

impl LeaseStanding {
    /// Loop start: never led, never held.
    fn new() -> Self {
        Self {
            believes: false,
            held_unsuperseded: false,
        }
    }

    /// The edge-detection read (`run_lease_loop`'s old `was_leading`).
    fn believes(&self) -> bool {
        self.believes
    }

    /// A COMPLETED round resolved BELIEVED-HELD. Demands the
    /// [`ObservedHeld`] witness, minted only by
    /// `observe_held_while_believing` — so recording a held
    /// observation without running the fused rebound comparison does
    /// not typecheck in production code (merged_bug_114; the funnel
    /// doc carries the full claim).
    fn on_observed_held(&mut self, _witness: ObservedHeld) {
        self.believes = true;
        self.held_unsuperseded = true;
    }

    /// A COMPLETED election round resolved not-leading: clears both
    /// facts — including the stale hold of a fenced-then-superseded
    /// sequence, which preserves the legitimate release skip (an
    /// observed supersession means there is nothing of ours to
    /// release).
    fn on_observed_not_leading(&mut self) {
        self.believes = false;
        self.held_unsuperseded = false;
    }

    /// Test/proof driver: the production polarity pair as one call.
    /// DELEGATES to the production methods (the kani standing proofs
    /// and the unit tests exercise the real transitions, not a shim) —
    /// the witness mint here is the cfg-gated escape hatch production
    /// code does not have.
    #[cfg(any(test, kani))]
    fn on_observed(&mut self, now_leading: bool) {
        if now_leading {
            self.on_observed_held(ObservedHeld(()));
        } else {
            self.on_observed_not_leading();
        }
    }

    /// Local self-fence: stop believing. The apiserver state is
    /// UNKNOWN — that is what fencing means — so the hold is untouched
    /// (deliberately no API to clear it from this path).
    fn on_self_fence(&mut self) {
        self.believes = false;
    }

    /// The shutdown release gate: release iff we acquired and never
    /// observed our supersession. Reads ONLY the hold — belief has no
    /// API here.
    fn should_release_on_shutdown(&self) -> bool {
        self.held_unsuperseded
    }
}

/// The standing algebra under CBMC: over every event sequence (bounded
/// length, the production loop's only two mutators), the self-fence
/// never changes the hold, and the release gate is exactly "the last
/// completed round resolved leading". Joins the `decide_pure` pattern
/// (election.rs): pure bools, no loops in the functions under proof,
/// bounded driver loop.
#[cfg(kani)]
mod lease_standing_proofs {
    use super::LeaseStanding;

    const MAX_EVENTS: usize = 8;

    /// r[verify sched.lease.graceful-release+2]
    /// Fence events are invisible to the hold: folding any sequence
    /// with its SelfFence events removed yields the same
    /// `should_release_on_shutdown` verdict.
    #[kani::proof]
    #[kani::unwind(9)]
    fn lease_standing_fence_never_clears_held() {
        let observed: [Option<bool>; MAX_EVENTS] = kani::any();
        let fence_at: [bool; MAX_EVENTS] = kani::any();

        let mut with_fences = LeaseStanding::new();
        let mut without_fences = LeaseStanding::new();
        let mut i = 0;
        while i < MAX_EVENTS {
            if let Some(now_leading) = observed[i] {
                with_fences.on_observed(now_leading);
                without_fences.on_observed(now_leading);
            }
            if fence_at[i] {
                with_fences.on_self_fence();
                // without_fences: the fence removed.
            }
            i += 1;
        }
        assert!(
            with_fences.should_release_on_shutdown() == without_fences.should_release_on_shutdown(),
            "a local self-fence must never change the release verdict"
        );
    }

    /// r[verify sched.lease.graceful-release+2]
    /// The release gate is exactly "acquired and not since observed
    /// superseded": it equals the polarity of the LAST completed round
    /// (false when no round ever completed), regardless of interleaved
    /// fences; and belief never exceeds the hold (a believer is always
    /// a holder, never the reverse... the fence opens exactly the
    /// holder-not-believer gap the release exists for).
    #[kani::proof]
    #[kani::unwind(9)]
    fn lease_standing_release_gate_iff_acquired_unsuperseded() {
        let observed: [Option<bool>; MAX_EVENTS] = kani::any();
        let fence_at: [bool; MAX_EVENTS] = kani::any();

        let mut standing = LeaseStanding::new();
        let mut last_round: Option<bool> = None;
        let mut i = 0;
        while i < MAX_EVENTS {
            if let Some(now_leading) = observed[i] {
                standing.on_observed(now_leading);
                last_round = Some(now_leading);
            }
            if fence_at[i] {
                standing.on_self_fence();
            }
            i += 1;
        }
        assert!(
            standing.should_release_on_shutdown() == last_round.unwrap_or(false),
            "release iff the last completed round resolved leading"
        );
        assert!(
            !standing.believes() || standing.should_release_on_shutdown(),
            "belief never exceeds the hold"
        );
    }
}

/// A blind-window anchor minted at the START of a renew attempt — on
/// the suspend-aware fence clock, strictly BEFORE the attempt's await.
/// `anchor ≤ send ≤ apiserver commit`, so a blind window stamped from
/// it is never shorter than one anchored at the commit (the event the
/// leader-election model anchors at).
///
/// No other constructor exists: the single mint site is the line above
/// the attempt's `await` in `run_lease_loop_with_client`, which makes
/// "stamp the blind clock from a post-response reading" — the
/// suspend-straddle zombie-leader bug — unrepresentable rather than
/// merely avoided. The mint is infallible and cheap; the value is
/// consumed by [`BlindClock::stamp`] (or dropped when the attempt
/// fails, leaving the window aging — correct: a failed round-trip is
/// blind time).
struct RenewAnchor(Duration);

impl RenewAnchor {
    /// Mint an anchor from the injected fence clock. Call ONLY before
    /// the renew attempt's await (see the type doc).
    fn mint(fence_now: &impl Fn() -> Duration) -> Self {
        Self(fence_now())
    }
}

/// The self-fence blind-window clock: remembers the anchor of the last
/// successful renew round-trip and answers "how long have I been
/// blind". The only write API consumes a [`RenewAnchor`], so the value
/// stored is always an attempt-START reading; the saturating subtraction
/// lives here so no caller ever computes the window by hand.
struct BlindClock {
    last: Duration,
}

impl BlindClock {
    /// Initialize at loop start: the node has never completed a
    /// round-trip, so the window starts aging from "now" (an anchor
    /// minted before any await — the loop-init mint site).
    fn starting_at(anchor: RenewAnchor) -> Self {
        Self { last: anchor.0 }
    }

    /// Record a successful round-trip: the blind window restarts at the
    /// attempt's START. Consumes the anchor — each mint stamps at most
    /// once.
    fn stamp(&mut self, anchor: RenewAnchor) {
        self.last = anchor.0;
    }

    /// The blind window as of `now`. Saturating: both readings come
    /// from the same non-decreasing fence clock, so a zero from an
    /// out-of-order reading is purely defensive.
    fn blind_for(&self, now: Duration) -> Duration {
        now.saturating_sub(self.last)
    }
}

/// Ledger entry for a transmitted-but-unconfirmed lease write
/// (`sched.lease.cancelled-write`): the act phase failed AFTER a
/// mutating request may have left this process. "Cancelled" is not
/// "discarded" — the apiserver may commit the request after our
/// deadline, and a committed renew/steal bumps the rv every standby's
/// steal clock keys on. While unconsumed the entry keeps the OLDEST
/// in-doubt anchor (if several writes are in doubt, the blind window
/// must cover the eldest); it is consumed ONLY by own-commit evidence
/// — a later COMPLETED read observing this replica as holder with a
/// moved rv proves some write of ours committed — and the blind clock
/// then stamps at THIS entry's anchor, never the observing read's
/// time: anchor ≤ send ≤ commit, so the stamped window is never
/// shorter than one anchored at the commit, preserving the NeverDual
/// separation arithmetic (`FENCE_MARGIN`). A completed read with an
/// UNCHANGED rv stamps nothing — read-stamping is the regression the
/// model's falsify twin guards (it would let a read-only replica
/// believe past every steal).
struct UnconfirmedPut {
    anchor: RenewAnchor,
}

/// Generation-counted dirty flag for the leader-marks reconcile
/// (bug_181's structural close — replaces the `AtomicBool` whose clear
/// could clobber a concurrent re-dirty).
///
/// Two monotone counters: `marked` bumps on every dirtying event,
/// `cleared` advances only *through a snapshot the clearing task took
/// at spawn*. Dirty ⇔ `marked > cleared`. A mark that lands after a
/// task's snapshot is `> snap ≥ cleared` and therefore survives that
/// task's clear **by arithmetic** — there is no per-edge ordering
/// premise ("every edge writes `is_leader` before the flag") to
/// maintain, document, or violate. The rebound edge — which by design
/// never writes `is_leader` — was exactly the dirtying site the old
/// premise missed; any future site (a new edge, a verify pass, an
/// actor) is race-proof the moment it calls [`mark`](Self::mark).
///
/// The count-coincidence ABA of the bool (clear erases an unseen mark)
/// is unrepresentable: `clear_through` is `fetch_max`, so clears never
/// regress, and a clear can only settle marks that existed at its own
/// snapshot.
pub(crate) struct DirtyGen {
    /// Total dirtying events. Starts at 1 (construction is the first
    /// dirtying event: a fresh loop owes one reconcile — see the
    /// init-dirty comment in `run_lease_loop_with_client`).
    marked: AtomicU64,
    /// Highest settled mark. `cleared ≤ marked` always (clears go
    /// through snapshots of `marked`).
    cleared: AtomicU64,
}

impl DirtyGen {
    /// Construct in the dirty state (one un-cleared mark): the loop
    /// always owes its first reconcile.
    fn new_dirty() -> Self {
        Self {
            marked: AtomicU64::new(1),
            cleared: AtomicU64::new(0),
        }
    }

    /// Construct in the clean state. Test-only: production reaches
    /// clean exclusively through a completed reconcile's
    /// [`clear_through`](Self::clear_through).
    #[cfg(any(test, kani))]
    fn new_clean() -> Self {
        Self {
            marked: AtomicU64::new(0),
            cleared: AtomicU64::new(0),
        }
    }

    /// Record a dirtying event — any site, any task, any time. The
    /// mark survives every clear whose snapshot predates it.
    fn mark(&self) {
        self.marked.fetch_add(1, Ordering::SeqCst);
    }

    /// Snapshot the mark counter for a clear-through. Taken by the
    /// loop at task spawn (before the task's round-trip), so the clear
    /// can settle exactly the marks the task's patch could have
    /// reflected.
    fn snapshot(&self) -> u64 {
        self.marked.load(Ordering::SeqCst)
    }

    /// Are there unsettled marks?
    fn is_dirty(&self) -> bool {
        self.marked.load(Ordering::SeqCst) > self.cleared.load(Ordering::SeqCst)
    }

    /// Settle every mark up to `snap` (a value from
    /// [`snapshot`](Self::snapshot)). `fetch_max`: concurrent or
    /// out-of-order clears never regress the settled watermark, and a
    /// mark later than `snap` stays dirty by arithmetic.
    fn clear_through(&self, snap: u64) {
        self.cleared.fetch_max(snap, Ordering::SeqCst);
    }
}

/// The DirtyGen algebra under CBMC: over every bounded interleaving of
/// marks and snapshot/clear pairs, a mark that lands after a snapshot
/// is never settled by that snapshot's clear. Joins the `decide_pure` /
/// `LeaseStanding` pattern: no loops in the functions under proof,
/// bounded driver loop. (rio-lease has no `expectedHarnesses` pin —
/// the kani driver's "Complete - N" line is the count of record.)
#[cfg(kani)]
mod dirty_gen_proofs {
    use super::DirtyGen;

    const MAX_EVENTS: usize = 8;

    /// r[verify sched.lease.rebound+4]
    /// For every event sequence (mark / snapshot / clear-through-last-
    /// snapshot), after any clear: if a mark happened after the
    /// snapshot that clear used, the flag is still dirty.
    #[kani::proof]
    #[kani::unwind(9)]
    fn dirty_gen_mark_after_snapshot_never_cleared() {
        // 0 = mark, 1 = take snapshot, 2 = clear through the last
        // taken snapshot (no-op when none taken yet).
        let events: [u8; MAX_EVENTS] = kani::any();
        let dg = DirtyGen::new_clean();
        let mut snap: Option<u64> = None;
        let mut marked_since_snap = false;
        let mut i = 0;
        while i < MAX_EVENTS {
            match events[i] % 3 {
                0 => {
                    dg.mark();
                    marked_since_snap = true;
                }
                1 => {
                    snap = Some(dg.snapshot());
                    marked_since_snap = false;
                }
                _ => {
                    if let Some(s) = snap {
                        dg.clear_through(s);
                        assert!(
                            !marked_since_snap || dg.is_dirty(),
                            "a mark after the snapshot must survive the clear"
                        );
                    }
                }
            }
            i += 1;
        }
    }
}

/// Local self-fence: if we believed we were leading but haven't
/// had a successful apiserver round-trip in over [`SELF_FENCE_AFTER`],
/// flip `is_leader=false` locally. SELF_FENCE_AFTER is 2×FENCE_MARGIN
/// ahead of any follower's steal threshold, so we stop believing
/// BEFORE anyone who can reach the apiserver steals — that ordering is
/// the `neverDual` proof in the `leaderElectionAsymmetric` regime of
/// `docs/spec/models/leaderElection.qnt`. The only
/// world where we're still the rightful leader is one where NOBODY can
/// reach the apiserver — in which case dispatch is pointless anyway.
///
/// Extracted from `run_lease_loop`'s error arm so it can be unit-
/// tested without spawning the full loop (paused time + real TCP
/// mocks cause spurious deadline-exceeded; see lang-gotchas).
///
/// Takes the full [`LeaseStanding`] but can only clear BELIEF —
/// [`LeaseStanding::on_self_fence`] is the sole mutation it has an API
/// for; the graceful-release hold is structurally out of reach.
///
/// Returns `true` if the fence fired (for test assertions).
// r[impl sched.lease.self-fence+2]
fn maybe_self_fence(
    state: &LeaderState,
    standing: &mut LeaseStanding,
    marks_dirty: &DirtyGen,
    blind_for: Duration,
) -> bool {
    if standing.believes() && blind_for > SELF_FENCE_AFTER {
        warn!(
            blind_for = ?blind_for,
            self_fence_after_secs = SELF_FENCE_AFTER.as_secs(),
            "LOCAL SELF-FENCE: no successful renew in > SELF_FENCE_AFTER, stepping down locally"
        );
        state.on_lose();
        standing.on_self_fence();
        // No spawn_patch_leader_marks here: the apiserver is
        // unreachable. Mark the pod's leader marks dirty so the FIRST
        // reachable round-trip in run_lease_loop's Ok arm reconciles
        // them (cost=0, leader label removed — unless we re-acquired by
        // then, in which case it re-asserts the leader marks). While we
        // stay partitioned we cannot remove our own stale label; the
        // new holder's peer sweep (sweep_peer_leader_marks) is what
        // bounds its lifetime, and until that holder's first successful
        // reconcile any request landing on us fails with this
        // self-fence's un-retryable UNAVAILABLE. The deferred local
        // reconcile still matters: in a symmetric partition there is no
        // reachable holder to sweep us, and our own first reachable
        // round-trip is what clears the marks (and the cost tie) then.
        marks_dirty.mark();
        true
    } else {
        false
    }
}

/// Merge-patch body for [`spawn_patch_leader_marks`]. Pure so the JSON
/// shape (and the merge-patch-`null`-removes-the-label encoding) is
/// unit-testable without a mock apiserver.
///
/// - `leading=true`  → `pod-deletion-cost: "1"`, label = its value.
/// - `leading=false` → `pod-deletion-cost: "0"`, label = `null`
///   (RFC 7396: a `null` member in a JSON merge patch REMOVES the key,
///   i.e. `kubectl label pod foo key-`). Absence — not `standby` — is
///   the non-leader state, so a leader-only Service selector never
///   matches a demoted pod.
/// - `label=None` → no `labels` key at all (the controller's
///   nodeclaim-pool lease has no leader Service).
fn leader_marks_patch(
    leading: bool,
    label: Option<&(String, String)>,
) -> k8s_openapi::serde_json::Value {
    // The annotation value is a string (all k8s annotations are),
    // parsed as int32 by the ReplicaSet controller. Invalid values
    // sort as 0.
    let cost = if leading { "1" } else { "0" };
    match label {
        Some((key, value)) => {
            // Computed before the json! call: `if` expressions in the
            // macro's value position confuse its tt-munching.
            let label_value = if leading {
                json!(value)
            } else {
                k8s_openapi::serde_json::Value::Null
            };
            json!({
                "metadata": {
                    "annotations": { POD_DELETION_COST_ANNOTATION: cost },
                    "labels": { key.as_str(): label_value }
                }
            })
        }
        None => json!({
            "metadata": {
                "annotations": { POD_DELETION_COST_ANNOTATION: cost }
            }
        }),
    }
}

// r[impl sched.lease.deletion-cost+3]
/// Detached PATCH of the leader marks on our own Pod:
///
/// - `controller.kubernetes.io/pod-deletion-cost`: K8s's ReplicaSet
///   controller sorts pods by this value (ascending) when deciding
///   which to evict during scale-down --- including RollingUpdate surge
///   reconciliation. Leader sets cost=1, standby cost=0, so K8s kills
///   the standby first.
/// - The optional leader label (`cfg.leader_pod_label`, e.g.
///   [`LEADER_ROLE_LABEL`]`=`[`LEADER_ROLE_LEADER`]): present on the
///   leader, absent on the standby — the `rio-scheduler-leader` Service
///   selects on it, so its endpoints converge to the current leader on
///   the holder's first successful reconcile after acquiring. When
///   leading, the task also strips the label from any other pod still
///   carrying it (the peer sweep, [`sweep_peer_leader_marks`]) — a
///   partitioned ex-leader cannot remove its own.
///
/// One PATCH for both: they flip on the same transitions, and a single
/// merge patch removes the partial-failure window (cost updated but
/// label not).
///
/// Level-triggered, not fire-and-forget: the caller
/// ([`maybe_spawn_leader_marks`]) spawns this while `marks_dirty` is
/// set AND no other marks PATCH is in flight — at most one is in
/// flight at a time, and dirtiness persists across the ticks the
/// single-flight guard skips. On success the task clears the marks
/// only THROUGH the [`DirtyGen`] snapshot the caller took at spawn:
/// any dirtying event that lands after the snapshot — a transition
/// edge, a rebound (which by design never writes `is_leader`), a
/// verify divergence — stays dirty **by arithmetic**, so no polarity
/// re-check or per-edge store-ordering premise exists to get wrong
/// (bug_181: the old bool's clear clobbered a concurrent rebound
/// re-dirty exactly because the rebound edge has no `is_leader` write
/// for a re-check to observe). A failed or timed-out patch clears
/// nothing. Either way the first tick after the slot frees retries
/// with the then-current state instead of leaving the marks wrong
/// until the next leadership transition — the label is load-bearing
/// for the dashboard data path (leader-only Service), so "wrong until
/// the next transition" would be an unbounded outage, not a cosmetic
/// blip.
///
/// `tokio::spawn` because the lease loop MUST NOT block. A slow
/// apiserver PATCH would stall the renew tick — the blocked loop can
/// neither renew nor self-fence while a standby steals after
/// `STEAL_AFTER` of observed staleness — dual-leader. Same constraint
/// as the LeaderAcquired actor send (see `run_lease_loop`). Each call
/// is bounded by `patch_timeout` (the loop passes its renew deadline):
/// with single-flight, one wedged PATCH would otherwise block every
/// further reconcile attempt.
///
/// Merge patch (not Apply): we only touch one annotation key and one
/// label key; Apply would need a fieldManager and a fuller object
/// shape. Merge is `kubectl annotate --overwrite` / `kubectl label
/// --overwrite` semantics, and a `null` value removes the key.
fn spawn_patch_leader_marks(
    client: kube::Client,
    cfg: &LeaseConfig,
    leading: bool,
    marks_dirty: Arc<DirtyGen>,
    marks_snapshot: u64,
    patch_in_flight: Arc<AtomicBool>,
    patch_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    let namespace = cfg.namespace.clone();
    let pod_name = cfg.holder_id.clone();
    let lease_name = cfg.lease_name.clone();
    let label = cfg.leader_pod_label.clone();
    tokio::spawn(async move {
        // Release the single-flight slot on every exit — success,
        // failure, timeout, or panic. A slot that stayed taken would
        // block every further reconcile attempt. Drop runs at the end
        // of this scope, i.e. AFTER the dirty-flag decision below — a
        // freed slot always means the flag already reflects this
        // task's outcome.
        struct SlotRelease(Arc<AtomicBool>);
        impl Drop for SlotRelease {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _slot = SlotRelease(patch_in_flight);

        let pods: Api<Pod> = Api::namespaced(client.clone(), &namespace);
        let leases: Api<Lease> = Api::namespaced(client, &namespace);
        let patch = leader_marks_patch(leading, label.as_ref());
        match tokio::time::timeout(
            patch_timeout,
            pods.patch(&pod_name, &PatchParams::default(), &Patch::Merge(&patch)),
        )
        .await
        {
            Ok(Ok(_)) => {
                debug!(%pod_name, leading, "patched pod leader marks");
                // Peer sweep — only while leading with a label
                // configured: strip the leader marks off any OTHER pod
                // still carrying the label (a partitioned ex-leader
                // cannot remove its own). The single-flight slot covers
                // own patch + list + peer patches; a transition during
                // that window is picked up by the next round-trip
                // (level-triggered, nothing lost). A failed sweep keeps
                // the reconcile dirty exactly like a failed own-pod
                // patch.
                let sweep_ok = match (leading, label.as_ref()) {
                    (true, Some(l)) => {
                        sweep_peer_leader_marks(
                            &pods,
                            &leases,
                            &lease_name,
                            &pod_name,
                            l,
                            patch_timeout,
                        )
                        .await
                    }
                    _ => true,
                };
                // Single-flight makes the apiserver's last-applied
                // marks the ones THIS task wrote; a clean own patch +
                // sweep settles exactly the marks that existed at the
                // caller's spawn snapshot. Any dirtying event that
                // landed after the snapshot — a transition edge, a
                // rebound (no `is_leader` write to observe), a verify
                // divergence — is `> marks_snapshot` and stays dirty by
                // arithmetic; an incomplete sweep simply clears
                // nothing, so the next round-trip retries the whole
                // reconcile.
                if sweep_ok {
                    marks_dirty.clear_through(marks_snapshot);
                }
            }
            Ok(Err(e)) => {
                // marks_dirty stays set → the first tick after the slot
                // frees retries (the retry contract in run_lease_loop's
                // reconcile comment). A PERSISTENT failure (RBAC
                // missing `patch pods` — 403) repeats this warning on
                // every retry — the steady warn stream is the
                // production operator signal. CI catches it too: the
                // helm-rendered scheduler runs under an RBAC-enforced
                // ServiceAccount in the lease-enabled VM scenarios, so
                // a persistent 403 fails the vm-dashboard-k3s
                // EndpointSlice wait (exactly one ready endpoint) and
                // the leader-election scenario's
                // deletion-cost assertion.
                // While it keeps failing: scale-down ordering is
                // arbitrary (deletion-cost half) and the leader-only
                // Service has no or stale endpoints, i.e. the dashboard
                // is down (label half).
                warn!(
                    %pod_name, leading, error = %e,
                    "failed to patch pod leader marks (deletion-cost + leader label); \
                     retrying on the next election round-trip"
                );
            }
            Err(_) => {
                // Per-call timeout. With single-flight, a wedged PATCH
                // would otherwise hold the slot and block every further
                // reconcile attempt, so the bound is required for
                // liveness — and it is a liveness bound only: dropping
                // the request future resets the HTTP/2 stream but does
                // not prove the apiserver discarded it, so a
                // cancelled-but-applied-late patch can in principle
                // overwrite a successor until this pod's next edge
                // (strictly narrower than the unbounded reordering race
                // single-flight removed). marks_dirty stays set → the
                // first tick after the slot frees retries.
                warn!(
                    %pod_name, leading, timeout = ?patch_timeout,
                    "pod leader-marks PATCH timed out; \
                     retrying on the next election round-trip"
                );
            }
        }
    })
}

/// Pure marks comparison for the verify pass: do the pod's stored
/// leader marks match this leadership polarity?
///
/// - Annotation half: leading requires `pod-deletion-cost: "1"`;
///   not-leading accepts `"0"` OR an absent annotation (a fresh pod has
///   none — absence ≡ cost 0, the same equivalence the init-dirty
///   comment in `run_lease_loop` documents).
/// - Label half (when configured): leading requires `key=value`
///   present; not-leading requires the key ABSENT (absence — not
///   `standby` — is the non-leader state, per `leader_marks_patch`).
fn marks_match(
    meta: &kube::api::ObjectMeta,
    leading: bool,
    label: Option<&(String, String)>,
) -> bool {
    let cost = meta
        .annotations
        .as_ref()
        .and_then(|a| a.get(POD_DELETION_COST_ANNOTATION))
        .map(String::as_str);
    let cost_ok = if leading {
        cost == Some("1")
    } else {
        matches!(cost, None | Some("0"))
    };
    let label_ok = match label {
        None => true,
        Some((key, value)) => {
            let stored = meta
                .labels
                .as_ref()
                .and_then(|l| l.get(key))
                .map(String::as_str);
            if leading {
                stored == Some(value.as_str())
            } else {
                stored.is_none()
            }
        }
    };
    cost_ok && label_ok
}

// r[impl sched.lease.marks-verify]
/// Detached verify pass: GET our OWN Pod (timeout-bounded; RBAC `get
/// pods` — granted alongside the existing `patch pods`/`list pods`
/// verbs), compare the stored marks against the leadership polarity
/// captured at spawn, and on divergence re-dirty the marks + warn —
/// the repair itself reuses the level-triggered reconcile on the next
/// round-trip. A GET failure changes nothing (the next verify interval
/// retries); a polarity gone stale mid-flight at worst re-dirties
/// ([`DirtyGen::mark`]), which is always safe (one no-op PATCH) — and
/// a mark is never erased by a concurrent reconcile's clear (the
/// clear-through arithmetic), so the verify's verdict cannot be lost.
fn spawn_verify_leader_marks(
    client: kube::Client,
    cfg: &LeaseConfig,
    leading: bool,
    marks_dirty: Arc<DirtyGen>,
    patch_in_flight: Arc<AtomicBool>,
    call_timeout: Duration,
) -> tokio::task::JoinHandle<()> {
    let namespace = cfg.namespace.clone();
    let pod_name = cfg.holder_id.clone();
    let label = cfg.leader_pod_label.clone();
    tokio::spawn(async move {
        // Same slot-release discipline as the reconcile task.
        struct SlotRelease(Arc<AtomicBool>);
        impl Drop for SlotRelease {
            fn drop(&mut self) {
                self.0.store(false, Ordering::SeqCst);
            }
        }
        let _slot = SlotRelease(patch_in_flight);

        let pods: Api<Pod> = Api::namespaced(client, &namespace);
        match tokio::time::timeout(call_timeout, pods.get(&pod_name)).await {
            Ok(Ok(pod)) => {
                if !marks_match(&pod.metadata, leading, label.as_ref()) {
                    warn!(
                        %pod_name, leading,
                        "leader-marks verify found divergence (external strip/foreign \
                         sweep?); re-dirtying for the next reconcile"
                    );
                    marks_dirty.mark();
                }
            }
            Ok(Err(e)) => {
                debug!(%pod_name, error = %e, "leader-marks verify GET failed; next interval retries");
            }
            Err(_) => {
                debug!(%pod_name, timeout = ?call_timeout, "leader-marks verify GET timed out; next interval retries");
            }
        }
    })
}

/// Spawn gate for the verify pass: spawns [`spawn_verify_leader_marks`]
/// iff the marks are CLEAN and no marks task is in flight, taking the
/// same single-flight slot as the reconcile. Dirty marks need no
/// verification — the reconcile already owes a patch; a taken slot
/// means a reconcile/verify is mid-flight and this round skips
/// (level-triggered: the next multiple retries).
///
/// Always called with `leading == true` (the loop gates on
/// `now_leading`): a standby's marks are converged by its own lose-edge
/// reconcile and swept by the live holder; verifying them too would
/// double the fleet's GET load for marks nobody routes on.
fn maybe_spawn_verify_leader_marks(
    client: &kube::Client,
    cfg: &LeaseConfig,
    marks_dirty: &Arc<DirtyGen>,
    patch_in_flight: &Arc<AtomicBool>,
    call_timeout: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    if !marks_dirty.is_dirty() && !patch_in_flight.swap(true, Ordering::SeqCst) {
        Some(spawn_verify_leader_marks(
            client.clone(),
            cfg,
            true,
            Arc::clone(marks_dirty),
            Arc::clone(patch_in_flight),
            call_timeout,
        ))
    } else {
        None
    }
}

/// Pure target selection for the peer sweep: every labeled pod except
/// our own AND except the current Lease holder. The own-name exclusion
/// is the one mistake that could strip the real leader's label when WE
/// are it; the holder exclusion closes the same mistake for a peer
/// that re-acquired while our reconcile was in flight (sweeping the
/// live holder downs the leader-only Service until the victim's own
/// next reconcile). Pinned by its own unit tests.
fn peer_sweep_targets(
    labeled_pod_names: Vec<String>,
    own_name: &str,
    lease_holder: Option<&str>,
) -> Vec<String> {
    labeled_pod_names
        .into_iter()
        .filter(|name| name != own_name && Some(name.as_str()) != lease_holder)
        .collect()
}

/// Peer sweep: while leading, strip the leader marks off any OTHER pod
/// still carrying the leader label — a partitioned ex-leader cannot
/// remove its own (its self-fence only defers its own reconcile), so
/// the new holder is the only replica that can bound that stale
/// label's lifetime. Runs inside the single in-flight reconcile task,
/// after the own-pod patch. Returns `false` if the holder read, the
/// list, or any peer patch failed; the caller then keeps the marks
/// dirty so the whole reconcile (self + sweep) retries on the next
/// successful round-trip.
///
/// HOLDER-AWARE: the sweep first GETs the Lease (same reconcile pass,
/// timeout-bounded) and spares the CURRENT holder — a peer that
/// re-acquired while this reconcile was in flight must not have its
/// fresh label stripped (that would down the leader-only Service until
/// the victim's own next reconcile; with the verify cadence that is
/// bounded, but the right bound is zero). A failed holder read fails
/// the sweep — keep dirty, retry — rather than sweeping blind.
///
/// The peer is demoted with the same body a standby writes for itself
/// ([`leader_marks_patch`] with `leading=false`): label removed AND
/// deletion-cost `"0"` — the cost reset for a non-leader is deliberate,
/// not incidental (a stale cost=1 on an ex-leader would tie it with the
/// real leader at the next scale-down).
///
/// Residuals, structurally: the stale-label bound holds on the new
/// holder's first SUCCESSFUL reconcile and only while it can reach the
/// apiserver; a pre-partition own-patch landing after the sweep is made
/// unreachable by the per-call timeout (far below the steal threshold);
/// the holder read and the peer patches are not one transaction, so a
/// holder change BETWEEN them can still strip a just-re-acquired
/// leader (the read-then-patch TOCTOU) — that residual is now BOUNDED
/// by the verify cadence (`sched.lease.marks-verify`): the victim
/// re-discovers the strip within MARKS_VERIFY_EVERY rounds instead of
/// waiting for its next leadership transition; in a symmetric
/// partition there is no holder to sweep and nothing can be scheduled
/// anyway.
async fn sweep_peer_leader_marks(
    pods: &Api<Pod>,
    leases: &Api<Lease>,
    lease_name: &str,
    own_name: &str,
    label: &(String, String),
    call_timeout: Duration,
) -> bool {
    // r[impl sched.lease.deletion-cost+3]
    // The holder read that makes the sweep spare the live holder.
    let holder = match tokio::time::timeout(call_timeout, leases.get_opt(lease_name)).await {
        Ok(Ok(lease)) => lease
            .and_then(|l| l.spec)
            .and_then(|spec| spec.holder_identity),
        Ok(Err(e)) => {
            warn!(error = %e, "peer-sweep holder read failed; keeping leader marks dirty to retry");
            return false;
        }
        Err(_) => {
            warn!(
                timeout = ?call_timeout,
                "peer-sweep holder read timed out; keeping leader marks dirty to retry"
            );
            return false;
        }
    };
    let (key, value) = label;
    let lp = ListParams::default().labels(&format!("{key}={value}"));
    let labeled = match tokio::time::timeout(call_timeout, pods.list(&lp)).await {
        Ok(Ok(list)) => list,
        Ok(Err(e)) => {
            warn!(error = %e, "peer-sweep pod list failed; keeping leader marks dirty to retry");
            return false;
        }
        Err(_) => {
            warn!(
                timeout = ?call_timeout,
                "peer-sweep pod list timed out; keeping leader marks dirty to retry"
            );
            return false;
        }
    };
    let names = labeled
        .items
        .into_iter()
        .filter_map(|p| p.metadata.name)
        .collect();
    let mut all_ok = true;
    for peer in peer_sweep_targets(names, own_name, holder.as_deref()) {
        let patch = leader_marks_patch(false, Some(label));
        match tokio::time::timeout(
            call_timeout,
            pods.patch(&peer, &PatchParams::default(), &Patch::Merge(&patch)),
        )
        .await
        {
            Ok(Ok(_)) => debug!(%peer, "stripped stale leader marks off peer pod"),
            Ok(Err(e)) => {
                warn!(
                    %peer, error = %e,
                    "failed to strip stale leader marks off peer pod; \
                     keeping leader marks dirty to retry"
                );
                all_ok = false;
            }
            Err(_) => {
                warn!(
                    %peer, timeout = ?call_timeout,
                    "peer leader-marks PATCH timed out; keeping leader marks dirty to retry"
                );
                all_ok = false;
            }
        }
    }
    all_ok
}

/// Spawn gate for the leader-marks reconcile: spawns
/// [`spawn_patch_leader_marks`] iff the marks are dirty AND no other
/// marks PATCH is in flight, taking the single-flight slot atomically.
/// Returns the spawned task's handle (`None` when it skipped) — the
/// lease loop ignores it; tests await it to observe the task's
/// dirty-flag decision deterministically.
///
/// Single-flight is what couples "the marks the apiserver last
/// applied" to "the polarity the completing task compares against the
/// desire": with two patches of opposite polarity in flight, the
/// apiserver's application order and the handlers' completion order
/// are independent, so the flag could end up clear while the pod
/// stores stale marks. While a patch is outstanding the dirty flag
/// simply stays set; the first tick after the slot frees retries with
/// the then-current polarity (the level-triggered contract is
/// unchanged).
pub(crate) fn maybe_spawn_leader_marks(
    client: &kube::Client,
    cfg: &LeaseConfig,
    leading: bool,
    marks_dirty: &Arc<DirtyGen>,
    patch_in_flight: &Arc<AtomicBool>,
    patch_timeout: Duration,
) -> Option<tokio::task::JoinHandle<()>> {
    // Short-circuit keeps the swap (which takes the slot) from firing
    // on non-dirty ticks.
    if marks_dirty.is_dirty() && !patch_in_flight.swap(true, Ordering::SeqCst) {
        // The clear-through snapshot: taken AFTER the slot is held and
        // BEFORE the task runs, so the spawned task can settle exactly
        // the marks its patch could have reflected — anything later
        // survives its clear by arithmetic.
        let marks_snapshot = marks_dirty.snapshot();
        Some(spawn_patch_leader_marks(
            client.clone(),
            cfg,
            leading,
            Arc::clone(marks_dirty),
            marks_snapshot,
            Arc::clone(patch_in_flight),
            patch_timeout,
        ))
    } else {
        None
    }
}

// r[verify sched.lease.k8s-lease+2]
// r[verify sched.lease.generation-fence+3]
#[cfg(test)]
mod tests {
    use super::*;

    /// from_parts returns None when lease_name unset — the signal
    /// for "non-K8s mode." This is how VM tests stay unaffected.
    /// Previously `from_env()` read `std::env::var("RIO_LEASE_NAME")`
    /// directly (bypassing the config loader); now the scheduler's
    /// Config passes the merged value through.
    /// BlindClock algebra: the window grows with `now`, restarts at a
    /// stamped anchor's mint time (not at stamp-call time), and is
    /// saturating for out-of-order readings. RenewAnchor's single-mint
    /// discipline is compile-level (no other constructor; consumed by
    /// stamp), so no runtime case exists to test.
    #[test]
    fn blind_clock_window_algebra() {
        let mut blind = BlindClock::starting_at(RenewAnchor(Duration::from_secs(10)));
        assert_eq!(blind.blind_for(Duration::from_secs(10)), Duration::ZERO);
        assert_eq!(
            blind.blind_for(Duration::from_secs(25)),
            Duration::from_secs(15),
            "the window ages with now"
        );
        // Saturating: a reading older than the anchor is zero, not a
        // panic or an underflow.
        assert_eq!(blind.blind_for(Duration::from_secs(9)), Duration::ZERO);

        // Stamping an anchor minted at t=20 restarts the window at 20 —
        // however late the stamp call happens (the suspend-straddle
        // property in one line: the response's arrival time is not an
        // input).
        blind.stamp(RenewAnchor(Duration::from_secs(20)));
        assert_eq!(
            blind.blind_for(Duration::from_secs(33)),
            Duration::from_secs(13)
        );
    }

    #[test]
    fn from_parts_none_when_unset() {
        assert!(
            LeaseConfig::from_parts(None, None).is_none(),
            "no lease_name → None → non-K8s mode"
        );
        // Namespace alone doesn't trigger K8s mode — lease_name is
        // the gate.
        assert!(
            LeaseConfig::from_parts(None, Some("rio-prod".into())).is_none(),
            "namespace without lease_name → still None"
        );
    }

    #[test]
    fn from_parts_reads_all_three() {
        // HOSTNAME is still a raw env read (K8s sets it, not us).
        // The jail serializes env access across parallel tests.
        rio_test_support::Jail::expect_with(|jail| {
            jail.set_env("HOSTNAME", "rio-scheduler-0");

            let cfg = LeaseConfig::from_parts(
                Some("rio-scheduler-leader".into()),
                Some("rio-prod".into()),
            )
            .expect("lease_name set → Some");
            assert_eq!(cfg.lease_name, "rio-scheduler-leader");
            assert_eq!(cfg.namespace, "rio-prod");
            assert_eq!(cfg.holder_id, "rio-scheduler-0");
            Ok(())
        });
    }

    /// Namespace None → read serviceaccount mount → fall back to
    /// "default" when that's also missing (non-K8s host running
    /// tests has no /var/run/secrets/kubernetes.io mount).
    #[test]
    fn from_parts_namespace_fallback() {
        let cfg = LeaseConfig::from_parts(Some("lease".into()), None).unwrap();
        // On a dev/CI host with no serviceaccount mount, the
        // read_to_string fails → "default". On the off chance the
        // CI runner IS a pod with a mount, the namespace will be
        // something else — just check it's non-empty.
        assert!(!cfg.namespace.is_empty());
    }

    // HOSTNAME fallback to UUID is a one-liner
    // (`.unwrap_or_else(|| Uuid::new_v4())`). Not worth a test —
    // the UUID crate tests itself.

    // r[verify sched.admin.list-executors-leader-age+3]
    /// `leader_for()` tracks acquire/lose. `pending` → None;
    /// `on_acquire` → Some; `on_lose` → None; `always_leader`/
    /// `Default` → Some (non-K8s mode reports a real age so the
    /// controller's `leader_for_secs` gate isn't permanently 0).
    #[test]
    fn leader_for_tracks_acquire_lose() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        assert!(state.leader_for().is_none(), "pending → None");
        state.on_acquire(0);
        assert!(state.leader_for().is_some(), "on_acquire → Some(elapsed)");
        state.on_lose();
        assert!(state.leader_for().is_none(), "on_lose → None");

        assert!(
            LeaderState::default().leader_for().is_some(),
            "Default (always_leader) → Some so non-K8s mode reports real age"
        );
    }

    #[test]
    fn leader_state_always_leader() {
        let gen_arc = Arc::new(AtomicU64::new(1));
        let state = LeaderState::always_leader(Arc::clone(&gen_arc));
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "non-K8s mode: immediately leader"
        );
        assert_eq!(state.generation.load(Ordering::Acquire), 1);
        // Same Arc: writes through gen_arc are visible through state.
        gen_arc.fetch_add(1, Ordering::Release);
        assert_eq!(state.generation.load(Ordering::Acquire), 2);
    }

    #[test]
    fn leader_state_pending_starts_false() {
        let gen_arc = Arc::new(AtomicU64::new(1));
        let state = LeaderState::pending(gen_arc);
        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "K8s mode: NOT leader until lease loop acquires"
        );
    }

    /// The generation IS the lease's transition count plus one — derived
    /// from the apiserver-CAS-guarded `leaseTransitions` field, not from a
    /// local increment. Two replicas that both believe they lead can never
    /// share a generation, because the transition count is bumped
    /// atomically with the holder change (the generation-collision
    /// counterexample documented in `docs/spec/models/leaderElection.qnt`'s
    /// StaleLeaderHasStaleGeneration archaeology is exactly two local
    /// increments seeded from the same stale high-water mark).
    // r[verify sched.lease.generation-fence+3]
    #[test]
    fn generation_derives_from_lease_transitions() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));

        // The lease creator: leaseTransitions=0 → generation 1. Matches
        // the non-K8s always_leader floor ("generation 1" = "the first
        // and only leader there has ever been").
        assert_eq!(state.on_acquire(0), 1, "creator: transitions 0 → gen 1");
        assert_eq!(state.generation(), 1);

        // A re-acquire at the same transition count (self-fence false
        // alarm: we fenced, connectivity returned, our renew succeeded,
        // nobody stole in between) is the SAME leadership epoch — the
        // generation must not move, or every in-flight assignment would
        // be spuriously invalidated.
        state.on_lose();
        assert_eq!(
            state.on_acquire(0),
            1,
            "re-acquire without a holder change keeps the epoch"
        );

        // The first thief: leaseTransitions=1 → generation 2. Distinct
        // from the creator's even if the creator never persisted
        // anything to PG.
        let thief = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        assert_eq!(thief.on_acquire(1), 2, "first steal: transitions 1 → gen 2");

        // The PG high-water seed (kubectl-delete-lease defense) can have
        // raised the generation past the lease's transition count;
        // fetch_max keeps the larger. A recreated Lease restarts
        // transitions at 0 but PG remembers generation 7 was used.
        let recovered = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        recovered.seed_generation_from(8);
        assert_eq!(
            recovered.on_acquire(0),
            8,
            "PG seed past the (reset) transition count wins"
        );
    }

    /// Confirmation-round bookkeeping: round ids increase per
    /// `begin_renew_round`, confirming records the round, and a stale
    /// (lower) confirmation never regresses `last_leading_round`.
    // r[verify sched.recovery.bump-confirm+3]
    #[test]
    fn confirmation_rounds_are_monotone() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        assert_eq!(state.renew_rounds_started(), 0, "no rounds yet");
        assert_eq!(state.last_leading_round(), 0, "nothing confirmed yet");

        let r1 = state.begin_renew_round();
        let r2 = state.begin_renew_round();
        assert!(r2 > r1, "round ids must increase");
        assert_eq!(state.renew_rounds_started(), 2);

        state.confirm_leading_round(r2);
        assert_eq!(state.last_leading_round(), r2);
        // A late confirmation of an earlier round must not regress.
        state.confirm_leading_round(r1);
        assert_eq!(
            state.last_leading_round(),
            r2,
            "confirming an old round never regresses the recorded round"
        );
    }

    /// A recovery completion computed before a rebound must not mark
    /// recovery complete after the rebound. Models the actor finishing
    /// `handle_leader_acquired` (TOCTOU gate already passed on the
    /// pre-rebound snapshots) while the lease loop's `on_rebound` lands
    /// concurrently between the gate's loads and the completion call:
    /// the completion is stamped with the pre-rebound acquire-epoch, so
    /// it cannot ungate dispatch for the post-rebound one — regardless
    /// of store order between the two tasks. (Red-first: with the
    /// previous unconditional boolean store(true) the first assertion
    /// failed.) The same-epoch re-acquire deliberately still completes:
    /// a lose + re-acquire at the SAME count is the same epoch, and its
    /// in-flight recovery result is still valid — that is the recovery
    /// gate's documented keep case, preserved by keying on the epoch
    /// rather than a session counter.
    // r[verify sched.lease.rebound+4]
    #[test]
    fn stale_completion_does_not_override_rebound_clear() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(2);
        // The actor snapshots its recovery-entry state — including the
        // acquire-epoch (transition count) — when it starts processing
        // LeaderAcquired.
        let transitions_at_entry = state.acquired_transitions();

        // The lease loop observes a holder change that began and ended
        // inside our observation gap — a rebound to a different count.
        // Recovery must re-run before dispatch ungates.
        state.on_rebound(4);

        // The in-flight (pre-rebound) recovery now finishes and stamps
        // the epoch it was computed under. The stamp is stale — it must
        // NOT make recovery_complete() true.
        state.set_recovery_complete(transitions_at_entry);
        assert!(
            !state.recovery_complete(),
            "a completion computed before the rebound must not ungate dispatch"
        );

        // Control: the rebound's own recovery (entered after the
        // rebound, so stamped with the new epoch) completing DOES
        // ungate.
        state.set_recovery_complete(state.acquired_transitions());
        assert!(state.recovery_complete());

        // Same-epoch keep: a lose + re-acquire at the SAME count while
        // a recovery is in flight is the same epoch — its completion
        // remains valid (the gate's documented keep case).
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(3);
        let entry = state.acquired_transitions();
        state.on_lose();
        state.on_acquire(3);
        state.set_recovery_complete(entry);
        assert!(
            state.recovery_complete(),
            "a same-epoch re-acquire keeps the in-flight completion valid"
        );
    }

    /// `acquired_transitions` records the raw transition count of the
    /// most recent acquire edge, unconditionally — including in the
    /// saturated regime where the PG-floor seed makes the generation
    /// `fetch_max` a no-op. This is the holder-change signal the
    /// scheduler's recovery TOCTOU gate compares; the generation alone
    /// cannot serve once it is seeded past `lease_transitions + 1`.
    #[test]
    fn acquired_transitions_tracks_acquire_edges() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(0);
        assert_eq!(state.acquired_transitions(), 0, "creator records 0");

        // Saturated regime: the seed raises the generation past any
        // value the lease can derive; subsequent acquires still record
        // their transition count even though the generation is frozen.
        state.seed_generation_from(8);
        state.on_lose();
        state.on_acquire(2);
        assert_eq!(state.generation(), 8, "generation fetch_max is a no-op");
        assert_eq!(
            state.acquired_transitions(),
            2,
            "acquire edge recorded despite the frozen generation"
        );

        // Same-epoch re-acquire (no holder change): the recorded count
        // does not move.
        state.on_lose();
        state.on_acquire(2);
        assert_eq!(state.generation(), 8);
        assert_eq!(
            state.acquired_transitions(),
            2,
            "same transition count re-recorded verbatim"
        );
    }

    /// `on_rebound` — the holder-change-observed-late transition: it
    /// re-records the observed transition count, re-derives the
    /// generation via `fetch_max` (a no-op in the saturated regime),
    /// clears `recovery_complete` so recovery re-runs, refreshes
    /// `leader_for()`, and never touches `is_leader`.
    // r[verify sched.lease.rebound+4]
    #[test]
    fn on_rebound_records_count_and_reruns_recovery() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(2); // gen 3, recorded transitions 2
        state.set_recovery_complete(state.acquired_transitions());

        let new_gen = state.on_rebound(4);
        assert_eq!(new_gen, 5, "generation re-derives from the observed count");
        assert_eq!(state.generation(), 5);
        assert_eq!(
            state.acquired_transitions(),
            4,
            "the observed count is re-recorded"
        );
        assert!(
            !state.recovery_complete(),
            "a rebound clears recovery_complete so recovery re-runs"
        );
        assert!(state.is_leader(), "a rebound is not a loss of leadership");
        assert!(
            state.leader_for().is_some(),
            "leader_for refreshes rather than clears"
        );

        // Saturated regime: the PG-floor seed already raised the
        // generation past anything the count derives — the fetch_max is
        // a no-op but the count and the recovery flag still move.
        state.seed_generation_from(10);
        state.set_recovery_complete(state.acquired_transitions());
        let saturated_gen = state.on_rebound(6);
        assert_eq!(saturated_gen, 10, "saturated regime: generation unchanged");
        assert_eq!(state.acquired_transitions(), 6);
        assert!(!state.recovery_complete());
        assert!(state.is_leader());
    }

    // ---- Renewal timeout + self-fence (remediation 08) -----------

    use super::election::LeaderElection;
    use rio_test_support::kube_mock::ApiServerVerifier;

    /// Apiserver accepts the connection but never responds. The
    /// renew timeout must fire within RENEW_INTERVAL - RENEW_SLOP.
    /// Without the timeout wrapper at `run_lease_loop`'s callsite,
    /// `try_acquire_or_renew` would hang until the outer tokio::test
    /// timeout — proving the bug.
    ///
    /// Hang injection: hold the verifier WITHOUT calling `.run()`.
    /// The tower-test mock's Handle stays alive (request can queue)
    /// but nobody ever calls `next_request()` to pull + respond →
    /// the client's GET pends forever. Calling `.run(vec![])` would
    /// NOT work: the spawned task drops the Handle on return, which
    /// makes the mock return `ServiceError::Closed` immediately.
    #[tokio::test]
    async fn renew_timeout_fires_on_hung_apiserver() {
        // _verifier binding keeps the tower-test Handle alive for
        // the whole test body. No drop-bomb on ApiServerVerifier
        // itself (only on the VerifierGuard returned by .run()).
        let (client, _verifier) = ApiServerVerifier::new();

        let mut election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            LEASE_TTL,
            STEAL_AFTER,
        );

        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        let started = Instant::now();
        let result = tokio::time::timeout(deadline, election.try_acquire_or_renew()).await;

        assert!(
            result.is_err(),
            "timeout should fire (apiserver hung), got {result:?}"
        );
        // Prove it was OUR timeout, not some inner kube-rs deadline.
        // If kube-rs had a default request timeout < 3s, this would
        // complete early with Ok(Err(_)) and the elapsed check would
        // catch it.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
            "timeout fired at {elapsed:?}, expected ~{deadline:?}"
        );
    }

    /// step_down on a hung apiserver must time out, not hang shutdown.
    /// Same hang-injection as renew_timeout_fires_on_hung_apiserver:
    /// hold the verifier without `.run()` so the GET pends forever.
    /// Without the timeout wrapper at `run_lease_loop`'s shutdown
    /// branch, `main.rs`'s `h.await` would block until SIGKILL.
    // r[verify sched.lease.graceful-release+2]
    #[tokio::test]
    async fn step_down_timeout_fires_on_hung_apiserver() {
        let (client, _verifier) = ApiServerVerifier::new();
        let election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            LEASE_TTL,
            STEAL_AFTER,
        );

        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        let started = Instant::now();
        let result = tokio::time::timeout(deadline, election.step_down()).await;

        assert!(result.is_err(), "step_down should time out, got {result:?}");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
            "fired at {elapsed:?}, expected ~{deadline:?}"
        );
    }

    /// Self-fence fires when `last_successful_renew` is older than
    /// SELF_FENCE_AFTER and we believed we were leading. Simulates
    /// the state after 3+ failed renew ticks (5s each, fence at 11s).
    #[test]
    fn self_fence_flips_is_leader_after_fence_deadline_of_failures() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.set_recovery_complete(state.acquired_transitions());

        let mut standing = LeaseStanding::new();
        standing.on_observed(true);
        let marks_dirty = DirtyGen::new_clean();
        // 20s blind > SELF_FENCE_AFTER (11s). The predicate is clock-free
        // (takes the blind duration directly) since the boottime move.
        let blind_for = Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut standing, &marks_dirty, blind_for);

        assert!(fired, "self-fence should fire past SELF_FENCE_AFTER");
        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "self-fence should flip is_leader=false"
        );
        assert!(
            !state.recovery_complete(),
            "self-fence should clear recovery_complete (re-acquire re-runs recovery)"
        );
        assert!(
            !standing.believes(),
            "belief should flip so next tick is edge-free"
        );
        assert!(
            standing.should_release_on_shutdown(),
            "the fence is local: the hold survives for the shutdown release"
        );
    }

    /// Self-fence does NOT fire within SELF_FENCE_AFTER. One or two
    /// transient apiserver blips should not cause step-down — the
    /// lease may still be validly held (the original "DON'T flip"
    /// comment's reasoning is correct for the FIRST few failures).
    /// 10s is SELF_FENCE_AFTER − 1s: the boundary's negative side.
    #[test]
    fn self_fence_does_not_flip_before_fence_deadline() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.set_recovery_complete(state.acquired_transitions());

        let mut standing = LeaseStanding::new();
        standing.on_observed(true);
        let marks_dirty = DirtyGen::new_clean();
        // 10s blind < SELF_FENCE_AFTER (11s). Two failed ticks, lease
        // still validly held as far as we know.
        let blind_for = Duration::from_secs(10);

        let fired = maybe_self_fence(&state, &mut standing, &marks_dirty, blind_for);

        assert!(!fired, "within SELF_FENCE_AFTER → no self-fence");
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "within SELF_FENCE_AFTER → still leader (transient blip)"
        );
        assert!(state.recovery_complete());
        assert!(standing.believes());
    }

    /// The fence predicate is strict `>`: blind-time EXACTLY at
    /// SELF_FENCE_AFTER does not fire — the same boundary choice
    /// `decide_pure` documents for the steal threshold. Trivially
    /// expressible now that the predicate takes the duration directly.
    #[test]
    fn self_fence_does_not_fire_at_exact_deadline() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.set_recovery_complete(state.acquired_transitions());

        let mut standing = LeaseStanding::new();
        standing.on_observed(true);
        let marks_dirty = DirtyGen::new_clean();

        let fired = maybe_self_fence(&state, &mut standing, &marks_dirty, SELF_FENCE_AFTER);

        assert!(
            !fired,
            "blind_for == SELF_FENCE_AFTER must NOT fire (strict >)"
        );
        assert!(state.is_leader.load(Ordering::Relaxed));
        assert!(standing.believes());
    }

    /// Self-fence is gated on `was_leading`. A standby that has
    /// NEVER held the lease should not "step down" — it has nothing
    /// to step down from. Avoids spurious lease_lost_total increments
    /// from a standby whose apiserver connectivity is flaky.
    #[test]
    fn self_fence_no_op_when_not_leading() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        // is_leader already false, recovery_complete already false.

        let mut standing = LeaseStanding::new();
        let marks_dirty = DirtyGen::new_clean();
        let blind_for = Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut standing, &marks_dirty, blind_for);

        assert!(!fired, "not leading → no fence even past TTL");
        assert!(!state.is_leader.load(Ordering::Relaxed));
        assert!(!standing.believes());
    }

    /// Self-fence sets `marks_dirty` so the lease loop's next reachable
    /// round-trip reconciles our pod's leader marks (deletion-cost
    /// annotation + leader label) — it cannot patch from inside the
    /// fence, the apiserver is unreachable. Without the deferred
    /// reconcile, an ex-leader keeps cost=1 tied with the new leader
    /// (peer's cost=1 patch doesn't touch OUR pod) so the next
    /// RollingUpdate evicts arbitrarily — defeating
    /// `r[sched.lease.deletion-cost]` — AND keeps the leader label, so
    /// the leader-only Service routes to two pods. Regression:
    /// maybe_self_fence previously consumed the `was_leading` edge
    /// without arranging the deferred patch.
    // r[verify sched.lease.deletion-cost+3]
    #[test]
    fn self_fence_sets_marks_dirty() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.set_recovery_complete(state.acquired_transitions());

        let mut standing = LeaseStanding::new();
        standing.on_observed(true);
        let marks_dirty = DirtyGen::new_clean();
        let blind_for = Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut standing, &marks_dirty, blind_for);
        assert!(fired);
        assert!(
            marks_dirty.is_dirty(),
            "self-fence must mark the pod's leader marks dirty (apiserver unreachable \
             now, so the lease loop reconciles them on the next reachable round-trip)"
        );

        // No-fire path leaves the flag untouched (a standby that never
        // led has no marks to reconcile).
        let standby = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let mut standing = LeaseStanding::new();
        let marks_dirty = DirtyGen::new_clean();
        let fired = maybe_self_fence(
            &standby,
            &mut standing,
            &marks_dirty,
            Duration::from_secs(20),
        );
        assert!(!fired);
        assert!(!marks_dirty.is_dirty(), "no-fire → flag untouched");
    }

    // ---- Leader-marks patch body (deletion-cost + leader label) ----
    //
    // The detached PATCH itself has no mock-apiserver test (it is a
    // tokio::spawn off the lease loop); the JSON body is the part that
    // encodes the set/remove semantics, so that's what's pinned here.
    // The dirty-flag plumbing (set on every transition, cleared only by
    // a successful patch of a still-current polarity, retried every
    // round-trip otherwise) stays inline in `run_lease_loop` /
    // `spawn_patch_leader_marks`; the vm-dashboard-k3s EndpointSlice
    // wait and the leader-election VM scenario's deletion-cost
    // assertion are the end-to-end coverage for it.

    /// Acquire: cost=1 and the label is present with its value.
    // r[verify sched.lease.deletion-cost+3]
    #[test]
    fn leader_marks_patch_sets_label_on_acquire() {
        let label = (
            LEADER_ROLE_LABEL.to_string(),
            LEADER_ROLE_LEADER.to_string(),
        );
        let p = leader_marks_patch(true, Some(&label));
        assert_eq!(
            p["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
            json!("1")
        );
        assert_eq!(
            p["metadata"]["labels"][LEADER_ROLE_LABEL],
            json!(LEADER_ROLE_LEADER),
            "leader label must be present with its value on acquire"
        );
    }

    /// Lose / demote: cost=0 and the label key is EXPLICITLY null.
    /// RFC 7396 merge-patch semantics: a null member removes the key.
    /// An absent `labels` key here would leave the stale label in
    /// place — the leader-only Service would keep routing to the
    /// ex-leader.
    #[test]
    fn leader_marks_patch_nulls_label_on_lose() {
        let label = (
            LEADER_ROLE_LABEL.to_string(),
            LEADER_ROLE_LEADER.to_string(),
        );
        let p = leader_marks_patch(false, Some(&label));
        assert_eq!(
            p["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
            json!("0")
        );
        let labels = p["metadata"]["labels"]
            .as_object()
            .expect("labels map present on demote");
        assert!(
            labels.contains_key(LEADER_ROLE_LABEL),
            "label key must be PRESENT (set to null) so the merge patch removes it"
        );
        assert!(
            labels[LEADER_ROLE_LABEL].is_null(),
            "label value must be null (= remove) on lose, not a 'standby' string"
        );
    }

    /// No label configured (the controller's nodeclaim-pool lease):
    /// the patch only carries the deletion-cost annotation — no
    /// `labels` key at all, so we never touch another component's pod
    /// labels.
    #[test]
    fn leader_marks_patch_omits_labels_when_unconfigured() {
        for leading in [true, false] {
            let p = leader_marks_patch(leading, None);
            assert!(
                p["metadata"].get("labels").is_none(),
                "no leader_pod_label configured → no labels key in the patch"
            );
            assert_eq!(
                p["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
                json!(if leading { "1" } else { "0" })
            );
        }
    }

    /// `with_leader_pod_label` round-trips through `LeaseConfig`, and
    /// `from_parts` defaults to None (the controller path is unchanged).
    #[test]
    fn lease_config_leader_pod_label_default_and_builder() {
        // HOSTNAME may or may not be set in the test environment;
        // from_parts falls back to a UUID either way and this test
        // only cares about the label field.
        let cfg = LeaseConfig::from_parts(Some("lease".into()), Some("ns".into())).unwrap();
        assert_eq!(cfg.leader_pod_label, None, "default: no label management");
        let cfg = cfg.with_leader_pod_label(LEADER_ROLE_LABEL, LEADER_ROLE_LEADER);
        assert_eq!(
            cfg.leader_pod_label,
            Some((
                LEADER_ROLE_LABEL.to_string(),
                LEADER_ROLE_LEADER.to_string()
            ))
        );
    }

    // ---- Marks reconcile: apply order vs completion order ----

    use rio_test_support::kube_mock::RequestPark;

    /// `LeaseConfig` for driving the marks reconcile directly: pod name
    /// "us", leader label configured (the deployment shape the
    /// `rio-scheduler-leader` Service depends on).
    fn marks_test_cfg() -> LeaseConfig {
        LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        }
        .with_leader_pod_label(LEADER_ROLE_LABEL, LEADER_ROLE_LEADER)
    }

    /// Polarity a leader-marks patch body writes: `true` ⇔ leader marks
    /// (deletion-cost "1"; the label, when configured, present).
    fn body_polarity(body: &k8s_openapi::serde_json::Value) -> bool {
        body["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION] == json!("1")
    }

    /// A canned 200 body for a pod PATCH response.
    fn pod_ok(name: &str) -> k8s_openapi::serde_json::Value {
        json!({"apiVersion": "v1", "kind": "Pod", "metadata": {"name": name}})
    }

    /// Opposite-polarity marks reconciles racing must not end with
    /// `marks_dirty` clear while the apiserver's last-applied marks
    /// disagree with the pod's leadership: the flag's final value is
    /// decided by handler completion order, the pod's stored marks by
    /// apiserver application order, and nothing couples the two unless
    /// at most one patch is in flight. The label half is load-bearing
    /// for the leader-only Service, so a silently-stale outcome is a
    /// dashboard outage until the next leadership transition. Pre-fix
    /// (no single-flight gate), the lose-while-in-flight schedule below
    /// ended with the flag clear over leader marks stored on a
    /// non-leader.
    // r[verify sched.lease.deletion-cost+3]
    #[tokio::test]
    async fn marks_reconcile_single_flight_couples_flag_to_stored_polarity() {
        let (client, mut park) = RequestPark::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));
        let is_leader = Arc::new(AtomicBool::new(true));
        // Generous bound: nothing in this schedule relies on the
        // timeout firing (the timed-out leg has its own test).
        let patch_timeout = Duration::from_secs(60);

        // The test's model of the apiserver: bodies in the order the
        // test applies them; the last entry is what the pod stores.
        let mut applied: Vec<k8s_openapi::serde_json::Value> = Vec::new();

        // Leader + dirty → task A (leader polarity) spawns; its PATCH
        // parks at the mock.
        let handle_a =
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout)
                .expect("dirty + free slot must spawn");
        let req_a = park.next().await;
        assert!(
            req_a.path.contains("/pods/us"),
            "own-pod PATCH endpoint, got {}",
            req_a.path
        );

        // Lose edge while A is still in flight: desire flips, dirty set.
        is_leader.store(false, Ordering::SeqCst);
        marks_dirty.mark();

        // Single-flight: the opposite-polarity drive is skipped while A
        // holds the slot — a concurrent pair of patches is exactly what
        // decouples the apiserver's last write from the flag's final
        // value.
        assert!(
            maybe_spawn_leader_marks(
                &client,
                &cfg,
                false,
                &marks_dirty,
                &in_flight,
                patch_timeout,
            )
            .is_none(),
            "second reconcile must be skipped while a patch is in flight"
        );
        assert!(
            park.try_next().await.is_none(),
            "no second PATCH may reach the apiserver while A is in flight"
        );

        // A completes: applied at the apiserver, then its handler runs
        // with a now-stale polarity → flag stays dirty, slot freed.
        applied.push(req_a.body.clone());
        req_a.respond_ok(pod_ok("us"));
        // A wrote the leader polarity, so its task also runs the peer
        // sweep inside the same single-flight slot (holder read + own
        // patch + list); answer the holder read, then the
        // label-selected LIST with no peers, so A's handler can reach
        // its flag decision.
        let req_holder = park.next().await;
        assert!(
            req_holder.path.contains("/leases/rio-sched"),
            "holder-aware sweep reads the Lease first, got {}",
            req_holder.path
        );
        req_holder.respond_ok(serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": "rio-sched", "namespace": "default",
                          "resourceVersion": "5" },
            "spec": { "holderIdentity": "us", "leaseTransitions": 1 },
        }));
        let req_list = park.next().await;
        assert!(
            req_list.path.contains("labelSelector="),
            "peer-sweep LIST endpoint, got {}",
            req_list.path
        );
        req_list.respond_ok(json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {"resourceVersion": "1"},
            "items": []
        }));
        handle_a.await.expect("patch task A");
        assert!(
            marks_dirty.is_dirty(),
            "stale-polarity completion must leave the marks dirty"
        );
        assert!(
            !in_flight.load(Ordering::SeqCst),
            "completed task must release the single-flight slot"
        );

        // The retry spawns with the CURRENT polarity and converges.
        let handle_c = maybe_spawn_leader_marks(
            &client,
            &cfg,
            false,
            &marks_dirty,
            &in_flight,
            patch_timeout,
        )
        .expect("dirty + freed slot must spawn the retry");
        let req_c = park.next().await;
        applied.push(req_c.body.clone());
        req_c.respond_ok(pod_ok("us"));
        handle_c.await.expect("patch task C");

        // The structural invariant the bug violated: flag clear with no
        // patch in flight ⇒ the apiserver's last-applied polarity equals
        // the pod's leadership. Exactly two PATCHes total (A + retry).
        assert!(
            !marks_dirty.is_dirty(),
            "converged reconcile clears the flag"
        );
        assert!(!in_flight.load(Ordering::SeqCst));
        assert!(
            park.try_next().await.is_none(),
            "no further requests after convergence"
        );
        assert_eq!(
            applied.len(),
            2,
            "exactly A and the retry reached the apiserver"
        );
        let stored = applied.last().expect("at least one applied patch");
        assert_eq!(
            body_polarity(stored),
            is_leader.load(Ordering::SeqCst),
            "marks_dirty is clear with nothing in flight, so the last-applied \
             marks must match the pod's leadership"
        );
    }

    /// A failed PATCH must release the single-flight slot and leave the
    /// marks dirty, so the next round-trip can retry.
    #[tokio::test]
    async fn marks_patch_failure_releases_slot_and_keeps_dirty() {
        let (client, mut park) = RequestPark::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));
        let patch_timeout = Duration::from_secs(60);

        let handle =
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout)
                .expect("dirty + free slot must spawn");
        park.next()
            .await
            .respond_status(403, "Forbidden", "rbac: missing patch on pods");
        handle.await.expect("patch task");

        assert!(marks_dirty.is_dirty(), "failed patch keeps the marks dirty");
        assert!(
            !in_flight.load(Ordering::SeqCst),
            "failed patch releases the slot"
        );
        // The freed slot is genuinely usable: the next drive spawns.
        assert!(
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout,)
                .is_some(),
            "retry after a failure must be able to take the slot"
        );
    }

    /// A PATCH that never gets an answer must end at the per-call
    /// timeout: slot released, marks still dirty, retry possible. The
    /// bound is what keeps single-flight from turning one wedged call
    /// into a permanently stalled reconcile.
    #[tokio::test(start_paused = true)]
    async fn marks_patch_timeout_releases_slot_and_keeps_dirty() {
        let (client, mut park) = RequestPark::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));
        // Same bound the loop passes: its renew deadline.
        let patch_timeout = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);

        let handle =
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout)
                .expect("dirty + free slot must spawn");
        // Park the request and never answer it; the call must end via
        // its own timeout once the (paused) clock passes the bound.
        let parked = park.next().await;
        tokio::time::advance(patch_timeout + Duration::from_millis(100)).await;
        handle.await.expect("patch task ends via its timeout");

        assert!(
            marks_dirty.is_dirty(),
            "timed-out patch keeps the marks dirty"
        );
        assert!(
            !in_flight.load(Ordering::SeqCst),
            "timed-out patch releases the slot"
        );
        assert!(
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout,)
                .is_some(),
            "retry after a timeout must be able to take the slot"
        );
        drop(parked);
    }

    /// bug_181 red: a rebound's re-dirty landing while a reconcile is
    /// in flight must survive that reconcile's clear. The rebound edge
    /// dirties the marks WITHOUT writing `is_leader` (by design —
    /// pinned by `on_rebound_records_count_and_reruns_recovery`), so a
    /// completion-side polarity re-check premised on "every edge writes
    /// `is_leader` before the dirty flag" cannot see it: the in-flight
    /// task's clear erases the rebound's mark even though the foreign
    /// term's reconcile provably swept our stored marks. The dirtying
    /// site this test stands in for is the rebound arm's re-dirty in
    /// `run_lease_loop_with_client` (the `transitions != recorded`
    /// branch).
    // r[verify sched.lease.rebound+4]
    // r[verify sched.lease.deletion-cost+3]
    #[tokio::test]
    async fn rebound_redirty_survives_inflight_reconcile_clear() {
        let (client, mut park) = RequestPark::new();
        // No leader label configured → no peer-sweep legs; the own-pod
        // PATCH is the only request.
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));
        let patch_timeout = Duration::from_secs(60);

        // Leader + dirty → reconcile spawns; its PATCH parks.
        let handle =
            maybe_spawn_leader_marks(&client, &cfg, true, &marks_dirty, &in_flight, patch_timeout)
                .expect("dirty + free slot must spawn");
        let req = park.next().await;
        assert!(req.path.contains("/pods/us"));

        // The rebound re-dirty lands while the PATCH is in flight.
        // is_leader is NOT written — that is the rebound's contract.
        marks_dirty.mark();

        // The task completes successfully and runs its clear decision.
        req.respond_ok(pod_ok("us"));
        handle.await.expect("patch task");

        assert!(
            marks_dirty.is_dirty(),
            "a rebound re-dirty landing mid-reconcile must survive the \
             completing task's clear (the foreign term's sweep stripped \
             our stored marks; a clean flag here is an unbounded stale-\
             marks outage)"
        );
        assert!(!in_flight.load(Ordering::SeqCst));
    }

    // ---- merged_bug_303: cancelled lease writes are not discarded ----

    /// Test-side apiserver lease body for the parked-request loop
    /// tests: holder + transitions + rv, the three fields the loop's
    /// decision and evidence logic read.
    fn park_lease_json(
        holder: Option<&str>,
        transitions: u64,
        rv: u64,
    ) -> k8s_openapi::serde_json::Value {
        park_lease_json_rt(holder, transitions, rv, rv)
    }

    /// Like [`park_lease_json`] but with the protocol-authored
    /// `renewTime` controlled separately from the apiserver's
    /// `resourceVersion`: `rt` is folded into a synthetic micro-time
    /// so equal `rt` values model "no protocol write since" while a
    /// moving `rv` with frozen `rt` models foreign non-protocol
    /// writes (annotation/label patches).
    fn park_lease_json_rt(
        holder: Option<&str>,
        transitions: u64,
        rv: u64,
        rt: u64,
    ) -> k8s_openapi::serde_json::Value {
        json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "rio-sched",
                "namespace": "default",
                "resourceVersion": rv.to_string(),
            },
            "spec": {
                "holderIdentity": holder,
                "leaseTransitions": transitions,
                "leaseDurationSeconds": 15,
                "renewTime": format!("2026-01-01T00:00:{:02}.{:06}Z", rt % 60, rt),
            },
        })
    }

    /// merged_bug_303 (the mid-band livelock): an apiserver that
    /// ANSWERS reads promptly but is too slow to answer writes inside
    /// the belt — while still COMMITTING them — must not depose the
    /// leader forever. Pre-fix, the single composition deadline
    /// stamped nothing on Err(Elapsed): the leader self-fenced at 11s
    /// and stayed out indefinitely, while its committed-but-cancelled
    /// PUTs kept bumping the rv and re-anchoring every standby's
    /// steal clock — an unbounded leaderless livelock. Post-fix the
    /// loop records each transmitted-then-abandoned write as an
    /// UnconfirmedPut and consumes it with own-commit evidence (a
    /// completed read observing holder==us with a moved rv), stamping
    /// the blind clock at the LEDGER's anchor — the leader keeps
    /// leading exactly because its writes keep committing.
    // r[verify sched.lease.cancelled-write+2]
    #[tokio::test(start_paused = true)]
    async fn mid_band_committed_writes_keep_the_leader_leading() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Round 1 (healthy): the apiserver answers both phases. The
        // loop renews a lease it already holds server-side and takes
        // the acquire edge.
        let mut rv: u64 = 10;
        let get = park.next().await;
        assert!(get.path.contains("/leases/rio-sched"));
        get.respond_ok(park_lease_json(Some("us"), 3, rv));
        let put = park.next().await;
        rv += 1;
        put.respond_ok(park_lease_json(Some("us"), 3, rv));
        settle().await;
        assert!(state.is_leader(), "healthy round acquires");
        // The init-dirty marks reconcile fires after the first leading
        // round; answer its own-pod PATCH so the flag settles clean and
        // no further marks traffic interleaves with the lease rounds.
        let patch = park.next().await;
        assert!(patch.path.contains("/pods/us"), "marks reconcile PATCH");
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Mid-band rounds: GET answered promptly; the PUT is never
        // answered (the write deadline elapses via auto-advance — the
        // paused clock jumps to the next armed timer whenever every
        // task is parked) but COMMITS server-side: the next GET serves
        // the bumped rv. Dropping the parked request models the
        // response the client never saw. 12 rounds = 60s of sustained
        // mid-band; the tick cadence stays exactly RENEW_INTERVAL
        // because the test never advances the clock by hand.
        for round in 0..12 {
            // The hoisted marks machinery (merged_bug_122) services
            // leading acts-fail rounds too: the bounded-cadence verify
            // re-reads its own Pod every MARKS_VERIFY_EVERY rounds.
            // Absorb that traffic so the lease choreography stays
            // aligned — it is exactly the falsifier-closure coverage
            // the hoist exists to extend to this regime.
            let get = loop {
                let req = park.next().await;
                if req.path.contains("/pods/us") {
                    req.respond_ok(pod_ok("us"));
                    continue;
                }
                break req;
            };
            assert!(
                get.path.contains("/leases/rio-sched"),
                "round {round}: read phase"
            );
            get.respond_ok(park_lease_json(Some("us"), 3, rv));
            let put = park.next().await;
            rv += 1;
            drop(put);
        }
        settle().await;

        assert!(
            state.is_leader(),
            "a leader whose cancelled writes provably COMMIT (holder==us, \
             rv moving) must keep leading — fencing it forever while its \
             own PUTs re-anchor every standby's steal clock is the \
             unbounded leaderless livelock"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            0,
            "no lose edge may fire while own-commit evidence holds"
        );

        shutdown.cancel();
        // Graceful-release leg: answer its GET with a 404 so the loop
        // exits cleanly (no lease to release).
        if let Some(req) = park.try_next().await {
            req.respond_status(404, "NotFound", "gone");
        }
        loop_task.await.expect("lease loop exits");
    }

    /// The own-commit evidence arm consumes the SAME completed-read
    /// facts as the Leading renew round — including the transition
    /// count — so its still-believing leg must run the SAME rebound
    /// law (`sched.lease.rebound`): a foreign term that completed
    /// entirely inside the observation gap (moved count, holder back
    /// to us) re-derives the generation, clears recovery_complete, and
    /// fires the rebound hook. Pre-fix the leg only stamped the blind
    /// clock and recorded the observation: in the reads-complete/
    /// acts-fail regime the evidence re-stamps defeated the self-fence
    /// and no round ever Completed, so the foreign term was NEVER
    /// repaired — recovery_complete stayed true against PG state the
    /// term mutated, indefinitely.
    // r[verify sched.lease.rebound+4]
    #[tokio::test(start_paused = true)]
    async fn evidence_arm_rebounds_on_moved_count_while_believing() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Healthy acquire round at transitions=3.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 10));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, 11));
        settle().await;
        assert!(state.is_leader(), "healthy round acquires");
        let gen_at_acquire = state.generation();
        // Settle the init-dirty marks reconcile so the parked queue
        // carries lease traffic only.
        let patch = park.next().await;
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Mid-band round A: read completes (content unchanged), the
        // write is abandoned — the unconfirmed-write ledger is minted.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        drop(put);
        settle().await;

        // Mid-band round B: the read completes with the rv moved AND
        // the transition count moved — a foreign term ran to completion
        // entirely inside our observation gap and the lease came back
        // to us. The evidence arm consumes the ledger (we stay leader)
        // and its believing leg MUST rebound.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 9, 12));
        let put = park.next().await;
        drop(put);
        settle().await;

        assert!(state.is_leader(), "evidence keeps the leader leading");
        assert_eq!(
            hooks.rebounds.lock().expect("rebounds").len(),
            1,
            "the evidence arm's believing leg must run the rebound law on a moved count"
        );
        assert_eq!(
            state.generation(),
            gen_at_acquire + 6,
            "the rebound re-derives the generation from the moved count (3 -> 9)"
        );
        assert!(
            !state.recovery_complete(),
            "the rebound clears recovery_complete so recovery re-runs against post-term state"
        );

        shutdown.cancel();
        if let Some(req) = park.try_next().await {
            req.respond_status(404, "NotFound", "gone");
        }
        loop_task.await.expect("lease loop exits");
    }

    /// Shared choreography for the Q3 holder-evidenced-lose tests:
    /// healthy acquire (round 1), a transmitted-then-dropped renew that
    /// arms the cancelled-write ledger (round 2), then a renew 409
    /// (round 3) — the ambiguous CAS bounce whose rv-mover may be our
    /// own round-2 zombie commit. Returns after the 409 response is
    /// delivered and the loop has settled.
    async fn drive_to_believing_409(park: &mut RequestPark, state: &LeaderState) {
        // Round 1 (healthy): acquire.
        let get = park.next().await;
        assert!(get.path.contains("/leases/rio-sched"));
        get.respond_ok(park_lease_json(Some("us"), 3, 10));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, 11));
        settle().await;
        assert!(state.is_leader(), "healthy round acquires");
        let patch = park.next().await;
        assert!(patch.path.contains("/pods/us"), "marks reconcile PATCH");
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Round 2 (zombie): the read completes, the renew PUT is
        // transmitted and dropped — it may still commit server-side.
        // The ledger arms at this round's anchor.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        drop(put);
        settle().await;
        assert!(state.is_leader(), "an act failure alone never loses");

        // Round 3 (the ambiguous 409): the GET still serves the
        // pre-zombie view; the zombie commits inside the GET→PUT
        // window, so the PUT bounces. The 409 proves rv movement —
        // NOT a holder change.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        put.respond_status(409, "Conflict", "the object has been modified");
        settle().await;
    }

    /// Q3 (bughunt-4 §5-S, SIGNED): a renew 409 while believing is a
    /// CAS bounce, not holder evidence — our own zombie commit from a
    /// cancelled write (round 2 here) and a foreign metadata-only patch
    /// both move the rv while we remain holder. The first believing 409
    /// must DEFER one round instead of running the lose edge; the next
    /// completed read naming us resolves the deferral as a renew. The
    /// old immediate-lose wiped the DAG/outbox and bounced the leader
    /// marks on what was provably our own write.
    // r[verify sched.lease.holder-evidenced-lose]
    #[tokio::test(start_paused = true)]
    async fn believing_409_defers_for_holder_evidence_then_renews() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        drive_to_believing_409(&mut park, &state).await;

        // THE Q3 ASSERTION: the ambiguous 409 deferred — belief, the
        // hold, and the ledger survive; no lose edge ran.
        assert!(
            state.is_leader(),
            "a believing 409 with no holder evidence must defer, not lose \
             (the rv-mover may be our own zombie commit)"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            0,
            "no lose edge may run on a bare CAS bounce"
        );

        // Round 4 (resolution): the next GET shows the zombie DID
        // commit — rv and renewTime moved, holder is still us. The
        // deferred round resolves as an ordinary renew.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 12));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, 13));
        settle().await;

        assert!(state.is_leader(), "holder=us resolution renews");
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            0,
            "deferred-renew: the spurious failover is gone"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "belief was never dropped, so no second acquire edge fires"
        );

        shutdown.cancel();
        if let Some(req) = park.try_next().await {
            req.respond_status(404, "NotFound", "gone");
        }
        loop_task.await.expect("lease loop exits");
    }

    /// Q3 bound: the deferral is ONE round. A second consecutive
    /// believing 409 exhausts it and runs the lose edge — that bound is
    /// what keeps the NeverDual fence/steal separation intact (an
    /// unbounded deferral under every-round CAS bounces would retain
    /// belief past a standby's steal).
    // r[verify sched.lease.holder-evidenced-lose]
    #[tokio::test(start_paused = true)]
    async fn second_consecutive_believing_409_loses_with_deferral_exhausted() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        drive_to_believing_409(&mut park, &state).await;
        assert!(
            state.is_leader(),
            "first believing 409 defers (red against the immediate-lose law)"
        );

        // Round 4: ANOTHER 409 — the deferral is exhausted; the lose
        // edge runs with bounded evidence (two consecutive completed
        // rounds bounced our CAS).
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        put.respond_status(409, "Conflict", "the object has been modified");
        settle().await;

        assert!(
            !state.is_leader(),
            "the second consecutive believing 409 exhausts the deferral and loses"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "exactly one lose edge, at deferral exhaustion"
        );

        shutdown.cancel();
        for _ in 0..3 {
            if let Some(req) = park.try_next().await {
                req.respond_status(404, "NotFound", "gone");
            }
        }
        loop_task.await.expect("lease loop exits");
    }

    /// Q3 evidence path: a deferred 409 whose next completed read names
    /// a DIFFERENT holder loses WITH holder evidence — the typed lose
    /// edge the signed decision demands.
    // r[verify sched.lease.holder-evidenced-lose]
    #[tokio::test(start_paused = true)]
    async fn deferred_409_resolving_to_foreign_holder_loses_with_evidence() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        drive_to_believing_409(&mut park, &state).await;
        assert!(state.is_leader(), "first believing 409 defers");

        // Round 4: the GET names another holder — the rv-mover was a
        // genuine steal. The lose edge runs, with evidence.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("other"), 4, 12));
        settle().await;

        assert!(
            !state.is_leader(),
            "a completed read naming another holder is the evidence the lose edge requires"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "exactly one lose edge, with holder evidence"
        );

        shutdown.cancel();
        for _ in 0..3 {
            if let Some(req) = park.try_next().await {
                req.respond_status(404, "NotFound", "gone");
            }
        }
        loop_task.await.expect("lease loop exits");
    }

    /// merged_bug_122 (acquire-before-fence): consuming own-commit
    /// evidence whose ledger anchor is ALREADY past the self-fence
    /// deadline must not take the acquire edge — the very next
    /// statement block in the same arm would fence it again, a futile
    /// acquire→instant-fence flap that churns hooks, marks, and
    /// recovery every round of a slow-commit brownout. The stamp is
    /// unconditional (the evidence is real); only the acquire is
    /// fence-gated.
    // r[verify sched.lease.cancelled-write+2]
    // r[verify sched.lease.self-fence+2]
    #[tokio::test(start_paused = true)]
    async fn stale_anchor_evidence_consumption_stays_fenced() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Round 1 (healthy): acquire; settle the marks PATCH.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 10));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, 11));
        settle().await;
        let patch = park.next().await;
        patch.respond_ok(pod_ok("us"));
        settle().await;
        assert!(state.is_leader());

        // Round 2 (t=5s): the renew PUT is transmitted and dropped —
        // the ledger arms at THIS anchor and keeps it (oldest-wins).
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        drop(put);
        settle().await;

        // Rounds 3-4 (t=10s, 15s): same acts-fail shape, frozen
        // content — nothing stamps, the round-2 anchor ages.
        for _ in 0..2 {
            let get = park.next().await;
            get.respond_ok(park_lease_json(Some("us"), 3, 11));
            let put = park.next().await;
            drop(put);
            settle().await;
        }
        // The blind window (anchored at round 2, t=5s) crosses
        // SELF_FENCE_AFTER=11s at t=16s; the t=20s tick-top fences.
        // Round 5 (t=20s): the round-2 zombie is finally visible —
        // holder=us, renewTime moved. Evidence consumes the ledger at
        // its 15s-old anchor.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 12));
        let put = park.next().await;
        drop(put);
        settle().await;

        assert!(
            !state.is_leader(),
            "a 15s-old anchor is past the fence deadline — the replica stays fenced"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "consuming evidence with a stale anchor must NOT take the acquire \
             edge (the same arm's trailing fence would instantly undo it — \
             the acquire/instant-fence flap)"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "exactly one lose edge (the legitimate fence) — no flap churn"
        );

        shutdown.cancel();
        for _ in 0..3 {
            if let Some(req) = park.try_next().await {
                req.respond_status(404, "NotFound", "gone");
            }
        }
        loop_task.await.expect("lease loop exits");
    }

    /// merged_bug_122 (marks half, the report's bug_230 leg): the
    /// evidence-acquire leg mints `marks_dirty` but the leader-marks
    /// spawns lived only inside the `Completed` arm — in the
    /// reads-complete/acts-fail regime (where no round Completes, by
    /// the arm's own doc) the dirt was structurally unserviceable.
    /// The spawns are hoisted after the outcome match so EVERY arm
    /// services the dirt it (or the tick-top fence) minted.
    // r[verify sched.lease.deletion-cost+3]
    #[tokio::test(start_paused = true)]
    async fn evidence_acquire_services_marks_in_the_acts_fail_regime() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Round 1 (healthy): acquire; settle the marks PATCH clean.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 10));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, 11));
        settle().await;
        let patch = park.next().await;
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Rounds 2-3 (t=5s, 10s): the READ phase dies — nothing
        // stamps, nothing transmits, the blind window ages from the
        // round-1 stamp.
        for _ in 0..2 {
            let get = park.next().await;
            drop(get);
            settle().await;
        }

        // t=15s tick top: blind=15s > 11s — the legitimate self-fence
        // fires (and marks the strip dirt). Round 4 (same tick): the
        // read completes again; the PUT is transmitted and dropped —
        // the ledger arms at t=15s.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 11));
        let put = park.next().await;
        drop(put);
        settle().await;
        assert!(!state.is_leader(), "the fence fired before round 4");
        // Absorb the fence's strip PATCH if the marks dirt is being
        // serviced on non-Completed arms (the post-fix behavior).
        if let Some(req) = park.try_next().await {
            assert!(req.path.contains("/pods/us"));
            req.respond_ok(pod_ok("us"));
            settle().await;
        }

        // Round 5 (t=20s): the round-4 zombie committed — holder=us,
        // renewTime moved. Evidence consumes at the t=15s anchor
        // (5s old, inside the fence deadline) and re-acquires.
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, 12));
        let put = park.next().await;
        drop(put);
        settle().await;

        assert!(state.is_leader(), "fresh-anchor evidence re-acquires");
        assert_eq!(hooks.acquires.lock().expect("acquires lock").len(), 2);

        // THE marks assertion: the acquire minted label/cost dirt, and
        // the acts-fail regime never Completes a round — the dirt must
        // be serviced by THIS arm, this tick.
        let marks = park.try_next().await;
        assert!(
            marks.as_ref().is_some_and(|r| r.path.contains("/pods/us")),
            "the evidence-acquire round must service the marks dirt it \
             minted (no Completed round will come in this regime)"
        );
        if let Some(req) = marks {
            req.respond_ok(pod_ok("us"));
        }

        shutdown.cancel();
        for _ in 0..3 {
            if let Some(req) = park.try_next().await {
                req.respond_status(404, "NotFound", "gone");
            }
        }
        loop_task.await.expect("lease loop exits");
    }

    /// The safety companion (sched.lease.cancelled-write's second
    /// half): when the abandoned writes DON'T commit — the rv the
    /// reads serve stays frozen — there is no own-commit evidence, the
    /// blind clock must never be stamped, and the leader must
    /// self-fence on schedule. A bare completed read with an unchanged
    /// rv stamps NOTHING (read-stamping is the regression the model's
    /// falsify twin guards: it would let a read-only replica believe
    /// past every steal).
    // r[verify sched.lease.cancelled-write+2]
    // r[verify sched.lease.self-fence+2]
    #[tokio::test(start_paused = true)]
    async fn rv_frozen_act_failures_still_fence_on_schedule() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Healthy acquire round.
        let rv: u64 = 10;
        let get = park.next().await;
        get.respond_ok(park_lease_json(Some("us"), 3, rv));
        let put = park.next().await;
        put.respond_ok(park_lease_json(Some("us"), 3, rv + 1));
        settle().await;
        assert!(state.is_leader(), "healthy round acquires");
        // Settle the init-dirty marks reconcile (own-pod PATCH) before
        // the frozen rounds so the parked queue carries lease traffic
        // only.
        let patch = park.next().await;
        assert!(patch.path.contains("/pods/us"), "marks reconcile PATCH");
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Frozen mid-band: reads answer with an UNMOVED rv (the writes
        // never commit); writes are abandoned (their deadline elapses
        // via auto-advance). The blind window grows from the healthy
        // round's anchor; SELF_FENCE_AFTER is 11s, so the fence fires
        // by the third frozen round.
        let mut fenced_at: Option<u32> = None;
        for round in 0..5u32 {
            let get = park.next().await;
            get.respond_ok(park_lease_json(Some("us"), 3, rv + 1));
            let put = park.next().await;
            drop(put);
            settle().await;
            if fenced_at.is_none() && !state.is_leader() {
                fenced_at = Some(round);
            }
        }

        assert!(
            !state.is_leader(),
            "with the rv frozen there is no own-commit evidence: the \
             leader must fence and stay fenced"
        );
        assert!(
            fenced_at.is_some_and(|r| r <= 2),
            "the fence must fire within the SELF_FENCE_AFTER schedule \
             (by the third frozen round), got {fenced_at:?}"
        );
        assert!(
            !hooks.loses.lock().expect("loses lock").is_empty(),
            "the self-fence runs the lose edge"
        );

        shutdown.cancel();
        if let Some(req) = park.try_next().await {
            req.respond_status(404, "NotFound", "gone");
        }
        loop_task.await.expect("lease loop exits");
    }

    /// merged_bug_180, evidence direction: own-commit evidence keys on
    /// holder-authored spec content (`renewTime` bytes changing while
    /// the holder stays us), never on raw `resourceVersion` movement —
    /// the apiserver bumps rv on EVERY object write, including
    /// annotation/label patches by non-protocol tooling. A write-dead
    /// leader under a periodic foreign mutator must still fence on
    /// schedule: rv churn that the protocol did not author is not
    /// evidence that OUR write committed.
    // r[verify sched.lease.cancelled-write+2]
    #[tokio::test(start_paused = true)]
    async fn foreign_rv_movement_is_not_own_commit_evidence() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Healthy acquire round (renewTime rt=100 written by our PUT).
        let get = park.next().await;
        get.respond_ok(park_lease_json_rt(Some("us"), 3, 10, 100));
        let put = park.next().await;
        put.respond_ok(park_lease_json_rt(Some("us"), 3, 11, 101));
        settle().await;
        assert!(state.is_leader(), "healthy round acquires");
        let patch = park.next().await;
        patch.respond_ok(pod_ok("us"));
        settle().await;

        // Write-dead regime under a foreign mutator: every read shows
        // a FRESH rv (annotation patches) but the protocol-authored
        // renewTime is frozen — none of OUR abandoned writes commit.
        // There is no own-commit evidence; the fence must fire on the
        // same schedule as the frozen-rv regime.
        let mut fenced_at: Option<u32> = None;
        for round in 0..5u32 {
            let get = park.next().await;
            get.respond_ok(park_lease_json_rt(
                Some("us"),
                3,
                12 + u64::from(round),
                101,
            ));
            let put = park.next().await;
            drop(put);
            settle().await;
            if fenced_at.is_none() && !state.is_leader() {
                fenced_at = Some(round);
            }
        }

        assert!(
            !state.is_leader(),
            "foreign rv churn with a frozen renewTime is NOT own-commit \
             evidence: the write-dead leader must fence"
        );
        assert!(
            fenced_at.is_some_and(|r| r <= 2),
            "the fence must fire on the SELF_FENCE_AFTER schedule, got {fenced_at:?}"
        );

        shutdown.cancel();
        if let Some(req) = park.try_next().await {
            req.respond_status(404, "NotFound", "gone");
        }
        loop_task.await.expect("lease loop exits");
    }

    /// merged_bug_180, liveness direction: the standby steal clock keys
    /// observation identity on holder-authored spec content
    /// (`holderIdentity` + `renewTime` bytes), never on the object
    /// version counter — so a periodic non-protocol Lease write
    /// (annotation/label patch under the steal cadence) cannot
    /// indefinitely block stealing a genuinely dead leader's lease.
    /// The observation was recorded while the apiserver was at one rv;
    /// by the deciding read the foreign mutator has pushed the rv far
    /// past it — and the decision must not care, because the dead
    /// leader's CONTENT never moved. (The pre-fix red was captured by
    /// driving the lease loop through the mock apiserver with the rv
    /// bumped every tick: the rv-keyed clock reset each round and the
    /// standby never stole. The green is pinned at the composition
    /// grain because a paused-clock loop cannot age the std-Instant
    /// observation record — the same constraint the suspend test
    /// documents.)
    // r[verify sched.lease.k8s-lease+2]
    #[tokio::test]
    async fn foreign_metadata_patches_do_not_block_stealing_a_dead_leader() {
        use crate::election::{ElectionResult, LeaderElection};
        let (client, verifier) = rio_test_support::kube_mock::ApiServerVerifier::new();
        let lease_with_rt = |rv: &str| {
            serde_json::json!({
                "apiVersion": "coordination.k8s.io/v1",
                "kind": "Lease",
                "metadata": { "name": "rio-sched", "namespace": "default",
                              "resourceVersion": rv },
                "spec": { "holderIdentity": "dead-leader", "leaseTransitions": 2,
                          "leaseDurationSeconds": 15,
                          "renewTime": "2026-01-01T00:00:00.000100Z" },
            })
            .to_string()
        };
        let guard = verifier.run(vec![
            // The deciding GET: the foreign mutator has pushed the rv
            // far past wherever it was when we started observing; the
            // dead leader's spec content is byte-identical.
            rio_test_support::kube_mock::Scenario::ok(
                http::Method::GET,
                "/leases/rio-sched",
                lease_with_rt("105"),
            ),
            // The steal PUT succeeds (rv-guarded on the GET's rv).
            rio_test_support::kube_mock::Scenario::ok(
                http::Method::PUT,
                "/leases/rio-sched",
                serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": { "name": "rio-sched", "namespace": "default",
                                  "resourceVersion": "106" },
                    "spec": { "holderIdentity": "us", "leaseTransitions": 3,
                              "leaseDurationSeconds": 15 },
                })
                .to_string(),
            ),
        ]);

        let mut election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            LEASE_TTL,
            STEAL_AFTER,
        );
        // The content-keyed observation, recorded STEAL_AFTER+1s ago.
        // The identity bytes are the parse-normalized rendering — the
        // same pipeline fetch_and_decide feeds decide() — so the
        // comparison is literal-format-agnostic.
        let rt_bytes = {
            let mt: k8s_openapi::apimachinery::pkg::apis::meta::v1::MicroTime =
                k8s_openapi::serde_json::from_str("\"2026-01-01T00:00:00.000100Z\"")
                    .expect("micro-time literal parses");
            mt.0.to_string()
        };
        election.observed = crate::election::test_observed(
            "dead-leader",
            Some(&rt_bytes),
            std::time::Instant::now() - STEAL_AFTER - Duration::from_secs(1),
        );

        let result = election
            .try_acquire_or_renew()
            .await
            .expect("steal round-trip");
        assert_eq!(
            result,
            ElectionResult::Leading { transitions: 3 },
            "a periodic non-protocol Lease write must not indefinitely block \
             stealing a dead leader's lease — observation identity keys on \
             holder-authored spec content, not the rv"
        );

        guard.verified().await;
    }

    // ---- Peer sweep: stale leader label on another pod ----
    //
    // ApiServerVerifier is already imported by the renewal-timeout
    // tests above; only the scenario types are new here.

    use rio_test_support::kube_mock::{Method, Scenario};

    /// Pure target selection: never our own pod, never the current
    /// Lease holder, every other labeled pod. The own-name exclusion is
    /// the one mistake that could strip the real leader's label when WE
    /// are it; the holder exclusion is the same mistake for a peer that
    /// re-acquired mid-reconcile.
    // r[verify sched.lease.deletion-cost+3]
    #[test]
    fn peer_sweep_targets_excludes_own_name_and_holder() {
        assert_eq!(
            peer_sweep_targets(
                vec!["other-1".into(), "us".into(), "other-2".into()],
                "us",
                None
            ),
            vec!["other-1".to_string(), "other-2".to_string()]
        );
        assert!(peer_sweep_targets(vec!["us".into()], "us", None).is_empty());
        assert!(peer_sweep_targets(vec![], "us", None).is_empty());
        // The current holder is spared even when labeled.
        assert_eq!(
            peer_sweep_targets(vec!["b".into(), "us".into(), "old".into()], "us", Some("b")),
            vec!["old".to_string()]
        );
        // Holder == us: the own-name arm already covers it; no
        // double-exclusion surprises.
        assert_eq!(
            peer_sweep_targets(vec!["us".into(), "old".into()], "us", Some("us")),
            vec!["old".to_string()]
        );
        // An UNHELD lease (holder None, e.g. graceful vacate mid-sweep)
        // spares nobody extra.
        assert_eq!(
            peer_sweep_targets(vec!["old".into()], "us", None),
            vec!["old".to_string()]
        );
    }

    /// Pure marks comparison driving the verify pass: all four
    /// (leading, stored-marks) quadrants plus the absent-annotation and
    /// no-label-configured arms.
    // r[verify sched.lease.marks-verify]
    #[test]
    fn marks_match_quadrants() {
        let label = ("role".to_owned(), "leader".to_owned());
        let meta = |cost: Option<&str>, lbl: Option<&str>| kube::api::ObjectMeta {
            annotations: cost.map(|c| {
                [(POD_DELETION_COST_ANNOTATION.to_owned(), c.to_owned())]
                    .into_iter()
                    .collect()
            }),
            labels: lbl.map(|v| [("role".to_owned(), v.to_owned())].into_iter().collect()),
            ..Default::default()
        };
        // Converged leader.
        assert!(marks_match(
            &meta(Some("1"), Some("leader")),
            true,
            Some(&label)
        ));
        // Converged standby: cost 0 + label absent; and the fresh pod
        // (everything absent) is equivalent.
        assert!(marks_match(&meta(Some("0"), None), false, Some(&label)));
        assert!(marks_match(&meta(None, None), false, Some(&label)));
        // Stripped leader: label gone (the external-strip divergence).
        assert!(!marks_match(&meta(Some("1"), None), true, Some(&label)));
        // Zeroed leader cost.
        assert!(!marks_match(
            &meta(Some("0"), Some("leader")),
            true,
            Some(&label)
        ));
        // Stale standby still carrying leader marks.
        assert!(!marks_match(
            &meta(Some("1"), Some("leader")),
            false,
            Some(&label)
        ));
        // No label configured: annotation half alone decides.
        assert!(marks_match(&meta(Some("1"), None), true, None));
        assert!(!marks_match(&meta(None, None), true, None));
    }

    /// PodList response body whose items all carry the leader label.
    fn labeled_pod_list(names: &[&str]) -> String {
        let items: Vec<_> = names
            .iter()
            .map(|n| {
                json!({"metadata": {"name": n, "namespace": "default",
                       "labels": {LEADER_ROLE_LABEL: LEADER_ROLE_LEADER}}})
            })
            .collect();
        json!({
            "apiVersion": "v1",
            "kind": "PodList",
            "metadata": {"resourceVersion": "1"},
            "items": items
        })
        .to_string()
    }

    /// While leading, the reconcile strips the leader marks off any
    /// other pod still carrying the label. This is the deterministic
    /// stand-in for the asymmetric-partition aftermath: a self-fenced
    /// ex-leader cannot remove its own label, so the new holder's sweep
    /// is what bounds the stale endpoint's lifetime. The peer PATCH
    /// must carry the demote body (label removed via merge-patch null,
    /// cost "0"), and the flag clears only after own patch + sweep all
    /// landed.
    // r[verify sched.lease.deletion-cost+3]
    #[tokio::test]
    async fn leading_reconcile_sweeps_stale_peer_label() {
        let (client, verifier) = ApiServerVerifier::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));

        let guard = verifier.run(vec![
            Scenario::ok(Method::PATCH, "/pods/us", pod_ok("us").to_string()),
            // The holder-aware sweep reads the Lease first (holder =
            // us: nobody extra is spared — the original intent of this
            // test is unchanged).
            Scenario::ok(
                Method::GET,
                "/leases/rio-sched",
                serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": { "name": "rio-sched", "namespace": "default",
                                  "resourceVersion": "5" },
                    "spec": { "holderIdentity": "us", "leaseTransitions": 1 },
                })
                .to_string(),
            ),
            Scenario::ok(
                Method::GET,
                "labelSelector=",
                labeled_pod_list(&["us", "old-leader"]),
            ),
            Scenario {
                body_contains: Some(r#""rio.build/scheduler-role":null"#),
                ..Scenario::ok(
                    Method::PATCH,
                    "/pods/old-leader",
                    pod_ok("old-leader").to_string(),
                )
            },
        ]);

        let handle = maybe_spawn_leader_marks(
            &client,
            &cfg,
            true,
            &marks_dirty,
            &in_flight,
            Duration::from_secs(60),
        )
        .expect("dirty + free slot must spawn");
        handle.await.expect("reconcile task");
        guard.verified().await;

        assert!(
            !marks_dirty.is_dirty(),
            "own patch + sweep all landed → flag clears"
        );
        assert!(!in_flight.load(Ordering::SeqCst), "slot released");
    }

    /// A peer PATCH that fails (here: the pod vanished, 404) keeps the
    /// marks dirty so the whole reconcile — self + sweep — retries on
    /// the next successful round-trip.
    #[tokio::test]
    async fn failed_peer_sweep_keeps_marks_dirty() {
        let (client, verifier) = ApiServerVerifier::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));

        let guard = verifier.run(vec![
            Scenario::ok(Method::PATCH, "/pods/us", pod_ok("us").to_string()),
            // The holder-aware sweep reads the Lease first (holder =
            // us: nobody extra is spared — the original intent of this
            // test is unchanged).
            Scenario::ok(
                Method::GET,
                "/leases/rio-sched",
                serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": { "name": "rio-sched", "namespace": "default",
                                  "resourceVersion": "5" },
                    "spec": { "holderIdentity": "us", "leaseTransitions": 1 },
                })
                .to_string(),
            ),
            Scenario::ok(
                Method::GET,
                "labelSelector=",
                labeled_pod_list(&["us", "old-leader"]),
            ),
            Scenario::k8s_error(
                Method::PATCH,
                "/pods/old-leader",
                404,
                "NotFound",
                "pods \"old-leader\" not found",
            ),
        ]);

        let handle = maybe_spawn_leader_marks(
            &client,
            &cfg,
            true,
            &marks_dirty,
            &in_flight,
            Duration::from_secs(60),
        )
        .expect("dirty + free slot must spawn");
        handle.await.expect("reconcile task");
        guard.verified().await;

        assert!(
            marks_dirty.is_dirty(),
            "incomplete sweep must keep the marks dirty"
        );
        assert!(
            !in_flight.load(Ordering::SeqCst),
            "slot released even when the sweep failed"
        );
    }

    // ---- The loop's fence-check cadence (the NeverDual premise) ----

    use std::sync::Mutex;

    use rio_test_support::kube_mock::{MockApiServer, MockBehavior};

    /// Hooks that record the (virtual) instant of every acquire/lose
    /// callback. Clone is an Arc clone — the test body and the loop task
    /// observe the same vectors.
    #[derive(Clone, Default)]
    struct RecordingHooks {
        acquires: Arc<Mutex<Vec<Instant>>>,
        loses: Arc<Mutex<Vec<Instant>>>,
        rebounds: Arc<Mutex<Vec<Instant>>>,
    }

    impl LeaseHooks for RecordingHooks {
        fn on_acquire(&self) {
            self.acquires
                .lock()
                .expect("acquires lock")
                .push(Instant::now());
        }
        fn on_lose(&self) {
            self.loses.lock().expect("loses lock").push(Instant::now());
        }
        fn on_rebound(&self) {
            self.rebounds
                .lock()
                .expect("rebounds lock")
                .push(Instant::now());
        }
    }

    /// Let every task that is ready at the current (paused) instant run
    /// to quiescence: tick → fence check → request → mock handler →
    /// response → match arm. Same advance-then-yield driving pattern as
    /// `rio_common::task`'s periodic-task tests; the count is generous —
    /// once everything is quiescent the extra passes are no-ops.
    async fn settle() {
        for _ in 0..32 {
            tokio::task::yield_now().await;
        }
    }

    /// The NeverDual derivation on [`FENCE_MARGIN`] prices the victim's
    /// fence-check latency at one [`RENEW_INTERVAL`]: a blind leader must
    /// evaluate `maybe_self_fence` within one tick of the deadline
    /// crossing. The error-arm evaluation alone does NOT deliver that
    /// bound — a renew attempt that hangs resolves only at its deadline
    /// (`RENEW_INTERVAL − RENEW_SLOP` after the tick), so the evaluation
    /// can lag the tick by the attempt deadline. This drives the real
    /// loop against the mock apiserver under a paused clock and pins the
    /// bound structurally (constants-derived, never wall-clock).
    // r[verify sched.lease.self-fence+2]
    #[tokio::test(start_paused = true)]
    async fn fence_check_latency_bounded_by_one_tick() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state,
            hooks.clone(),
            shutdown.clone(),
            // tokio-Instant anchor: under start_paused this measurement
            // follows the virtual clock exactly as the old ambient one did.
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: the interval's first tick fires immediately; the mock is
        // Healthy, so the attempt creates the Lease and the acquire arm
        // runs. t_acquire (recorded at the on_acquire hook) is a valid
        // proxy for the blind clock's stamped anchor ONLY because this
        // scripted schedule makes the t=0 acquire the last successful
        // round-trip AND no virtual time elapses between that attempt's
        // anchor mint and its hook — under start_paused the two are the
        // same instant. If the schedule ever gains another Healthy
        // tick, the anchor must move to that tick.
        settle().await;
        let t_acquire = {
            let acquires = hooks.acquires.lock().expect("acquires lock");
            assert_eq!(
                acquires.len(),
                1,
                "the immediate first tick must acquire exactly once"
            );
            acquires[0]
        };

        // The FailFast@+5s/+10s → Hang@+15s choreography is load-bearing
        // for what this test proves: an all-Hang schedule has the +10s
        // attempt resolve at +13s (already past SELF_FENCE_AFTER), so the
        // error-arm evaluation alone fires within the bound and the test
        // could not distinguish a loop without the tick-time check. The
        // fast failures keep the last under-deadline evaluation at +10s,
        // and the hung +15s attempt delays the next error-arm evaluation
        // to +18s — past the bound unless the tick-time check fires at
        // +15s.
        mock.set_behavior(MockBehavior::FailFast);
        tokio::time::advance(RENEW_INTERVAL).await; // tick at +5s
        settle().await;
        tokio::time::advance(RENEW_INTERVAL).await; // tick at +10s
        settle().await;

        // Strictly BEFORE advancing into the +15s tick: that tick's
        // attempt must never resolve.
        mock.set_behavior(MockBehavior::Hang);
        tokio::time::advance(RENEW_INTERVAL).await; // tick at +15s
        settle().await;

        // Let the +15s attempt's deadline (RENEW_INTERVAL − RENEW_SLOP
        // past the tick) expire, plus a little slack so the timeout's
        // wakeup is unambiguously due.
        tokio::time::advance(RENEW_INTERVAL - RENEW_SLOP).await;
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;

        // Stop the loop. By now it is parked on the next interval tick
        // and the biased shutdown arm wins; the join may let tokio
        // auto-advance the paused clock, which is benign — every
        // assertion below is on instants recorded before this point.
        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");

        let acquires = hooks.acquires.lock().expect("acquires lock");
        let loses = hooks.loses.lock().expect("loses lock");
        assert_eq!(acquires.len(), 1, "exactly one acquire over the run");
        assert_eq!(
            loses.len(),
            1,
            "exactly one lose over the run (the self-fence)"
        );
        let gap = loses[0] - t_acquire;
        assert!(
            gap > SELF_FENCE_AFTER,
            "self-fence fired {gap:?} after the last successful renew — \
             before the SELF_FENCE_AFTER deadline ({SELF_FENCE_AFTER:?})"
        );
        assert!(
            gap <= SELF_FENCE_AFTER + RENEW_INTERVAL,
            "self-fence fired {gap:?} after the last successful renew; the \
             NeverDual derivation prices the fence-check latency at one \
             RENEW_INTERVAL past SELF_FENCE_AFTER ({:?})",
            SELF_FENCE_AFTER + RENEW_INTERVAL
        );
        // election.rs's Observed/decide clock stays on std (real) time
        // inside this paused-clock test — harmless here because the node
        // under test is the lease's creator/holder and never exercises
        // the steal-aging path that clock feeds.
    }

    /// The cooperative step-down self-heal loop
    /// (`sched.recovery.step-down`): a consumer that cannot serve its
    /// tenure (failed recovery) calls `request_step_down`; the NEXT
    /// loop tick consumes the request — releases the lease and fires
    /// the FULL lose-edge effects (leader state cleared, `on_lose`
    /// hook delivered) — and the FOLLOWING tick resumes candidacy and
    /// re-acquires. Pre-fix there was no such API: a broken tenure
    /// held the lease until a real lose/fence, serving nothing.
    // r[verify sched.recovery.step-down+2]
    #[tokio::test(start_paused = true)]
    async fn step_down_consumed_within_one_tick_then_candidacy_resumes() {
        let (client, _mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let observer = state.clone();
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state,
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: immediate first tick acquires (Healthy mock).
        settle().await;
        assert_eq!(
            hooks.acquires.lock().expect("acquires").len(),
            1,
            "the immediate first tick must acquire"
        );
        assert!(observer.is_leader(), "leading after the first tick");

        // The consumer (e.g. a failed recovery) requests a step-down
        // mid-tenure, stamped with the tenure it serves. Nothing
        // happens until the next tick — the request is a stamped cell,
        // not a reentrant call.
        observer.request_step_down(observer.acquired_instance());
        assert!(observer.is_leader(), "request alone demotes nothing");

        // +5s tick: the request is consumed — release + full
        // lose-edge effects, exactly once.
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            hooks.loses.lock().expect("loses").len(),
            1,
            "the step-down request is consumed within one renew tick \
             (full lose-edge: on_lose hook delivered)"
        );
        assert!(!observer.is_leader(), "leader state cleared on step-down");
        assert!(
            !observer.step_down_pending(),
            "consume-once: the loop's take leaves no residue"
        );

        // +10s tick: candidacy resumed — the loop re-acquires the
        // freshly released lease (sole candidate).
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            hooks.acquires.lock().expect("acquires").len(),
            2,
            "candidacy resumes on the following tick (re-acquire)"
        );
        assert!(observer.is_leader(), "leading again after re-acquire");

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// A step-down request carries the tenure that issued it and is
    /// dropped — never served — once that tenure has ended. Pre-fix the
    /// request was a bare flag: a tenure-N failure request lingered and
    /// demoted the healthy successor tenure N+1 that a rebound had just
    /// installed (the request's issuer no longer exists; the successor
    /// never asked to step down).
    // r[verify sched.recovery.step-down+2]
    #[tokio::test(start_paused = true)]
    async fn stale_tenure_step_down_request_cannot_demote_a_rebounded_tenure() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let observer = state.clone();
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state,
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: acquire via the create path — tenure = transitions 0.
        settle().await;
        assert!(observer.is_leader(), "t=0 acquire");
        let tenure_at_entry = observer.acquired_transitions();

        // A foreign term completes entirely inside the observation gap:
        // the lease comes back to us with the transition count bumped.
        // The next tick rebounds to the successor tenure.
        let rv: u64 = mock
            .resource_version()
            .expect("lease exists")
            .parse()
            .expect("numeric rv");
        mock.seed(serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": "rio-sched", "namespace": "default",
                          "resourceVersion": (rv + 1).to_string() },
            "spec": { "holderIdentity": "us", "leaseTransitions": 7 },
        }));
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            hooks.rebounds.lock().expect("rebounds").len(),
            1,
            "the moved count rebounds to the successor tenure"
        );

        // The DEAD tenure's failed recovery requests a step-down — for
        // the tenure it was serving, which has since ended.
        observer.request_step_down(tenure_at_entry);

        // The request must be dropped, not served against the healthy
        // successor.
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert!(
            observer.is_leader(),
            "a stale-tenure step-down request must not demote the healthy successor tenure"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses").len(),
            0,
            "no lose-edge runs for a dropped stale request"
        );
        assert!(
            !observer.step_down_pending(),
            "the stale request is cleared, not left to linger"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// merged_bug_128: a false-alarm fence followed by a same-epoch
    /// re-acquire installs a NEW tenure INSTANCE at the SAME transition
    /// count — and the re-acquire clears the failed predecessor's
    /// step-down request. The successor's recovery re-runs; if IT
    /// fails, it re-requests under its own instance and THAT serves.
    /// (This test previously pinned the retired count-keyed law — the
    /// request "surviving" the pair and demoting the successor — which
    /// is exactly the stale demotion the instance stamp exists to
    /// drop: recovery #1's failure must not fire against a successor
    /// whose recovery #2 succeeded.)
    // r[verify sched.recovery.step-down+2]
    #[tokio::test(start_paused = true)]
    async fn same_epoch_reacquire_drops_the_stale_request_and_a_fresh_one_serves() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let observer = state.clone();
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let jump_ms = Arc::new(AtomicU64::new(0));
        let fence_now = {
            let a = Instant::now();
            let jump_ms = jump_ms.clone();
            move || a.elapsed() + Duration::from_millis(jump_ms.load(Ordering::SeqCst))
        };
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state,
            hooks.clone(),
            shutdown.clone(),
            fence_now,
        ));

        // t=0: acquire (create path, tenure 0).
        settle().await;
        assert!(observer.is_leader(), "t=0 acquire");

        // The tenure's recovery fails and requests the step-down...
        observer.request_step_down(observer.acquired_instance());

        // ...but before the next tick a fence-clock jump (false alarm:
        // local only, the apiserver is healthy throughout) fences at the
        // tick top. The same tick's renew succeeds and re-acquires at
        // the SAME transition count — a NEW tenure instance, which
        // clears the failed predecessor's request.
        jump_ms.store(12_000, Ordering::SeqCst);
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            hooks.loses.lock().expect("loses").len(),
            1,
            "the false-alarm fence runs its lose edge"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires").len(),
            2,
            "the same tick's successful renew re-acquires (same-epoch re-claim)"
        );
        assert!(
            !observer.step_down_pending(),
            "the re-acquire cleared the failed predecessor's request — the \
             successor instance starts clean"
        );

        // Next believing tick: NOTHING serves — the successor tenure
        // never asked to step down (its recovery may well succeed).
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            mock.holder(),
            Some("us".to_string()),
            "the stale request must not release the successor's lease \
             (recovery #1's demotion firing against a successor whose \
             recovery #2 succeeded is the merged_bug_128 failover)"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses").len(),
            1,
            "no second lose edge — the stale demotion is gone"
        );

        // The successor's OWN recovery fails too: it re-requests under
        // its own instance, and that request serves normally.
        observer.request_step_down(observer.acquired_instance());
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            mock.holder(),
            None,
            "a request filed by the CURRENT instance serves at the next believing tick"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses").len(),
            2,
            "the served step-down runs its own lose edge"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// merged_bug_128 unit pair, defense (a): the acquire/rebound edges
    /// CLEAR a pending request — a new instance starts clean.
    #[test]
    fn acquire_and_rebound_clear_a_pending_step_down() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(7);
        state.request_step_down(state.acquired_instance());
        assert!(state.step_down_pending());
        // False-alarm fence + same-count re-acquire.
        state.on_lose();
        state.on_acquire(7);
        assert!(
            !state.step_down_pending(),
            "the same-count re-acquire installs a new instance and clears the request"
        );
        // Rebound clears too.
        state.request_step_down(state.acquired_instance());
        state.on_rebound(9);
        assert!(
            !state.step_down_pending(),
            "a rebound installs a successor instance and clears the request"
        );
    }

    /// merged_bug_128 unit pair, defense (b): a stale stamp filed AFTER
    /// the successor's edge (racing the clear) is dropped by the
    /// instance comparison — the monotone counter never repeats, so the
    /// same-count collision that defeated the transition-count stamp is
    /// unrepresentable.
    #[test]
    fn stale_instance_stamp_is_dropped_even_at_the_same_transition_count() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        state.on_acquire(7);
        let stale_instance = state.acquired_instance();
        state.on_lose();
        state.on_acquire(7); // same count, NEW instance
        // The racer files with the instance it recorded at ITS entry —
        // after the successor's clear ran.
        state.request_step_down(stale_instance);
        assert!(
            !state.take_step_down_request(state.acquired_instance()),
            "a stale instance stamp must not serve against the successor, \
             even though the transition count is identical"
        );
        assert!(
            !state.step_down_pending(),
            "the stale request is consumed-and-dropped, not left armed"
        );
    }

    /// merged_bug_164: a completed read that observed ABSENCE (the 404
    /// before a Create) is a baseline observation, not a missing one.
    /// The old two-state Option baseline nulled itself on the
    /// 404→Create round while still minting the ledger entry — so the
    /// round that first PROVED the Create committed (holder=us after a
    /// witnessed 404) evaluated `None.is_some_and(..) == false` and the
    /// conclusive own-commit evidence could never fire: belief entry
    /// for a committed-but-unanswered POST was structurally
    /// unreachable. Absence→present-naming-us IS content movement.
    // r[verify sched.lease.cancelled-write+2]
    #[tokio::test(start_paused = true)]
    async fn committed_create_with_lost_response_is_own_commit_evidence() {
        let (client, mut park) = RequestPark::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Round 1: the read witnesses ABSENCE (404), the loop POSTs a
        // Create, and the response NEVER ARRIVES — the act deadline
        // elapses (the parked request is held alive so the act times
        // out instead of erroring; its eventual drop lands in an
        // already-cancelled call). The ledger arms, and the baseline
        // must record the observed absence.
        let get = park.next().await;
        assert!(get.path.contains("/leases/rio-sched"));
        get.respond_status(404, "NotFound", "no lease yet");
        let post_hold = park.next().await;
        assert_eq!(
            post_hold.method,
            http::Method::POST,
            "the 404 read routes to the Create act"
        );
        // Let the act deadline elapse — the POST "response" is lost.
        tokio::time::advance(RENEW_PHASE_DEADLINE + Duration::from_millis(100)).await;
        settle().await;
        assert!(!state.is_leader(), "no response, no belief yet");

        // Round 2: the read completes and the lease NAMES US — only
        // our POST installs our holderIdentity (a racing creator's
        // 409-winning lease would carry theirs). That is conclusive
        // own-commit evidence against the Absent baseline; the renew
        // PUT dying (timeout, same as the POST) changes nothing.
        // The hoisted marks service (merged_bug_122) fires the
        // init-dirty reconcile after round 1 — a non-Completed round —
        // instead of waiting for a Completed one. Absorb it.
        let init_marks = park.next().await;
        assert!(
            init_marks.path.contains("/pods/us"),
            "init-dirty marks reconcile"
        );
        init_marks.respond_ok(pod_ok("us"));
        let get = park.next().await;
        assert!(get.path.contains("/leases/rio-sched"), "round 2 read phase");
        get.respond_ok(park_lease_json(Some("us"), 0, 1));
        let put_hold = park.next().await;
        tokio::time::advance(RENEW_PHASE_DEADLINE + Duration::from_millis(100)).await;
        settle().await;
        drop(post_hold);
        drop(put_hold);

        assert!(
            state.is_leader(),
            "holder=us after a witnessed 404 proves the Create committed — \
             the evidence must consume the ledger and take the acquire edge"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "exactly one acquire edge, from the consumed Create evidence"
        );

        shutdown.cancel();
        for _ in 0..3 {
            if let Some(req) = park.try_next().await {
                req.respond_status(404, "NotFound", "gone");
            }
        }
        loop_task.await.expect("lease loop exits");
    }

    /// The suspend-blindness regression the boottime fence clock fixes:
    /// a fence-clock jump (host suspend) past SELF_FENCE_AFTER must fence
    /// at the FIRST post-resume tick-time check — before any hung renew
    /// attempt resolves. With the old ambient (monotonic; tokio-virtual
    /// here) measurement the jump is invisible and the zombie leader
    /// persists until the hung attempt's error arm much later.
    // r[verify sched.lease.self-fence+2]
    #[tokio::test(start_paused = true)]
    async fn fence_fires_at_first_tick_after_fence_clock_jump() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        // Injected fence clock: tokio-virtual base plus a controllable
        // jump, simulating CLOCK_BOOTTIME advancing across a suspend the
        // monotonic clock never sees.
        let jump_ms = Arc::new(AtomicU64::new(0));
        let fence_now = {
            let a = Instant::now();
            let jump_ms = jump_ms.clone();
            move || a.elapsed() + Duration::from_millis(jump_ms.load(Ordering::SeqCst))
        };
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            fence_now,
        ));

        // t=0: the immediate first tick acquires healthily.
        settle().await;
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "t=0 acquire must succeed"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "exactly one acquire at t=0"
        );

        // "Suspend": the apiserver becomes unreachable AND the fence
        // clock jumps 12s (> SELF_FENCE_AFTER = 11s) while the tokio
        // virtual clock — the monotonic view — advances only one tick.
        mock.set_behavior(MockBehavior::Hang);
        jump_ms.store(12_000, Ordering::SeqCst);
        tokio::time::advance(RENEW_INTERVAL).await; // first post-resume tick
        settle().await;

        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "the first post-jump tick-time fence check must fence the zombie leader"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "exactly one lose at the first post-jump tick (before the hung attempt resolves)"
        );

        // Let the hung attempt's deadline expire: the error-arm re-check
        // sees was_leading already false — still exactly one lose.
        tokio::time::advance(RENEW_INTERVAL - RENEW_SLOP).await;
        settle().await;
        tokio::time::advance(Duration::from_millis(100)).await;
        settle().await;
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "no second lose from the error-arm re-check"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// Rider R1 of merged_bug_138: a rebound (still-leading round whose
    /// observed leaseTransitions moved — a foreign term ended entirely
    /// inside our observation gap) GUARANTEES a foreign reconcile swept
    /// our leader marks, so the rebound MUST re-dirty them. Pre-fix the
    /// arm stored nothing ("leadership polarity is unchanged") and the
    /// label stayed missing until the next leadership transition — a
    /// dashboard outage with no bound.
    // r[verify sched.lease.rebound+4]
    #[tokio::test(start_paused = true)]
    async fn rebound_marks_leader_marks_dirty() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = marks_test_cfg();
        mock.seed_pod("us", pod_ok("us"));
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0 acquire; the init-dirty reconcile patches our pod's marks.
        settle().await;
        assert!(state.is_leader.load(Ordering::Relaxed), "t=0 acquire");
        let pod = mock.pod("us").expect("pod seeded");
        assert_eq!(
            pod["metadata"]["labels"][LEADER_ROLE_LABEL],
            json!(LEADER_ROLE_LEADER),
            "the acquire reconcile must have asserted the leader label"
        );

        // A foreign term lands ENTIRELY inside our observation gap: the
        // foreign holder's reconcile swept our marks (label gone, cost
        // zeroed), then the lease came back to us with the transition
        // count bumped. Out-of-band seeds model both.
        mock.seed_pod("us", pod_ok("us")); // marks swept by the foreign reconcile
        let rv: u64 = mock
            .resource_version()
            .expect("lease exists")
            .parse()
            .expect("numeric rv");
        let transitions = 7;
        mock.seed(serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": "rio-sched", "namespace": "default",
                          "resourceVersion": (rv + 1).to_string() },
            "spec": { "holderIdentity": "us", "leaseTransitions": transitions },
        }));

        // Next tick: still-leading round observes the moved count — the
        // rebound. It must re-dirty the marks; the same tick's reconcile
        // re-asserts the label.
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert_eq!(
            hooks.rebounds.lock().expect("rebounds lock").len(),
            1,
            "the rebound must fire the rebound hook"
        );
        let pod = mock.pod("us").expect("pod still stored");
        assert_eq!(
            pod["metadata"]["labels"][LEADER_ROLE_LABEL],
            json!(LEADER_ROLE_LEADER),
            "the rebound must re-dirty the leader marks so the swept label is re-asserted \
             (a foreign term guarantees a foreign sweep ran)"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop exits");
    }

    /// Rider R2 of merged_bug_138: the peer sweep must spare the
    /// CURRENT Lease holder. Pre-fix the sweep stripped every labeled
    /// pod except our own — including a pod that just re-acquired the
    /// lease (our reconcile completing late, after a fence): stripping
    /// the real holder's label downs the leader-only Service until that
    /// victim's own next reconcile.
    // r[verify sched.lease.deletion-cost+3]
    #[tokio::test]
    async fn peer_sweep_spares_current_lease_holder() {
        let (client, verifier) = ApiServerVerifier::new();
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));

        let guard = verifier.run(vec![
            Scenario::ok(Method::PATCH, "/pods/us", pod_ok("us").to_string()),
            // The holder-aware sweep reads the Lease FIRST: the current
            // holder is B — B's label must survive the sweep.
            Scenario::ok(
                Method::GET,
                "/leases/rio-sched",
                serde_json::json!({
                    "apiVersion": "coordination.k8s.io/v1",
                    "kind": "Lease",
                    "metadata": { "name": "rio-sched", "namespace": "default",
                                  "resourceVersion": "9" },
                    "spec": { "holderIdentity": "b", "leaseTransitions": 3 },
                })
                .to_string(),
            ),
            Scenario::ok(
                Method::GET,
                "labelSelector=",
                labeled_pod_list(&["us", "b", "old-leader"]),
            ),
            Scenario {
                body_contains: Some(r#""rio.build/scheduler-role":null"#),
                ..Scenario::ok(
                    Method::PATCH,
                    "/pods/old-leader",
                    pod_ok("old-leader").to_string(),
                )
            },
        ]);

        let handle = maybe_spawn_leader_marks(
            &client,
            &cfg,
            true,
            &marks_dirty,
            &in_flight,
            Duration::from_secs(60),
        )
        .expect("dirty + free slot must spawn");
        handle.await.expect("reconcile task");
        guard.verified().await;

        assert!(
            !marks_dirty.is_dirty(),
            "own patch + holder-aware sweep all landed → flag clears"
        );
    }

    /// The structural close of merged_bug_138: a bounded-cadence verify
    /// pass re-reads our own pod every MARKS_VERIFY_EVERY rounds and
    /// re-dirties on divergence — so ANY falsifier (foreign sweep,
    /// kubectl, future actor) is bounded to one verify interval plus
    /// one reconcile, instead of "until the next leadership
    /// transition". Pre-fix: steady state spawned nothing, ever.
    // r[verify sched.lease.marks-verify]
    #[tokio::test(start_paused = true)]
    async fn nth_renew_verify_redirties_on_external_strip() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = marks_test_cfg();
        mock.seed_pod("us", pod_ok("us"));
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // Acquire; the init reconcile converges the marks.
        settle().await;
        assert!(state.is_leader.load(Ordering::Relaxed));
        assert_eq!(
            mock.pod("us").expect("pod")["metadata"]["labels"][LEADER_ROLE_LABEL],
            json!(LEADER_ROLE_LEADER),
            "marks converged after acquire"
        );

        // External strip: kubectl/foreign actor removes the label and
        // zeroes the cost, with NO leadership transition — the
        // edge-writer enumeration sees nothing.
        mock.seed_pod("us", pod_ok("us"));

        // Advance MARKS_VERIFY_EVERY healthy rounds: the verify pass
        // must GET our pod, observe the divergence, re-dirty, and the
        // following reconcile re-asserts the label.
        for _ in 0..(MARKS_VERIFY_EVERY + 1) {
            tokio::time::advance(RENEW_INTERVAL).await;
            settle().await;
        }
        let verify_gets = mock
            .requests()
            .iter()
            .filter(|(m, p)| *m == http::Method::GET && p.contains("/pods/us"))
            .count();
        assert!(
            verify_gets >= 1,
            "the bounded-cadence verify must GET our own pod at least once \
             in MARKS_VERIFY_EVERY healthy rounds (got {verify_gets} GETs)"
        );
        assert_eq!(
            mock.pod("us").expect("pod")["metadata"]["labels"][LEADER_ROLE_LABEL],
            json!(LEADER_ROLE_LEADER),
            "an externally-stripped leader label must be re-asserted within one \
             verify interval + one reconcile"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop exits");
    }

    /// Rider R3 of merged_bug_138: the abort contract the loop's exit
    /// path relies on — aborting an in-flight marks task releases the
    /// single-flight slot (SlotRelease's Drop runs on abort) and leaves
    /// `marks_dirty` set, so nothing wedges and nothing lies. The loop-
    /// level wiring (keep the JoinHandle, abort before step_down) is a
    /// compile-level close: pre-fix the handle was dropped (`let _ =`)
    /// and no abort site existed; a black-box loop test cannot
    /// distinguish abort from the task's own timeout under paused-time
    /// auto-advance, so the contract is pinned here and the wiring by
    /// the abort call's existence.
    #[tokio::test(start_paused = true)]
    async fn aborting_inflight_marks_task_releases_slot_and_keeps_dirty() {
        let (client, mock) = MockApiServer::new();
        mock.seed_pod("us", pod_ok("us"));
        mock.set_behavior(MockBehavior::Park);
        let cfg = marks_test_cfg();
        let marks_dirty = Arc::new(DirtyGen::new_dirty());
        let in_flight = Arc::new(AtomicBool::new(false));

        let handle = maybe_spawn_leader_marks(
            &client,
            &cfg,
            true,
            &marks_dirty,
            &in_flight,
            Duration::from_secs(600), // far beyond the test: abort, not timeout, ends it
        )
        .expect("dirty + free slot must spawn");
        assert!(in_flight.load(Ordering::SeqCst), "slot taken");

        // The own-pod PATCH is in flight (parked).
        while !mock
            .requests()
            .iter()
            .any(|(m, p)| *m == http::Method::PATCH && p.contains("/pods/us"))
        {
            tokio::task::yield_now().await;
        }

        // What the loop's exit path now does.
        handle.abort();
        assert!(handle.await.expect_err("aborted").is_cancelled());

        assert!(
            !in_flight.load(Ordering::SeqCst),
            "SlotRelease must run on abort — a held slot would wedge every \
             future reconcile of a (hypothetical) successor user of the Arcs"
        );
        assert!(
            marks_dirty.is_dirty(),
            "an aborted reconcile proved nothing — the marks stay dirty"
        );
        let sweep_issued = mock
            .requests()
            .iter()
            .any(|(m, p)| *m == http::Method::GET && p.contains("labelSelector"));
        assert!(
            !sweep_issued,
            "the aborted task must not reach its sweep round-trips"
        );
    }

    /// The release-gate half of bug_387: a self-fence is a LOCAL belief
    /// change — the apiserver may still name us holder — so a fence
    /// followed by SIGTERM must still release the lease gracefully.
    /// Pre-fix, the gate read the belief bool the fence had cleared and
    /// skipped step_down exactly when it mattered most.
    // r[verify sched.lease.graceful-release+2]
    #[tokio::test(start_paused = true)]
    async fn shutdown_after_self_fence_still_releases_lease() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: acquire healthily.
        settle().await;
        assert!(state.is_leader.load(Ordering::Relaxed), "t=0 acquire");
        assert_eq!(mock.holder().as_deref(), Some("us"));

        // Outage: fail fast until the self-fence fires (+15s tick:
        // blind 15s > SELF_FENCE_AFTER 11s).
        mock.set_behavior(MockBehavior::FailFast);
        for _ in 0..3 {
            tokio::time::advance(RENEW_INTERVAL).await;
            settle().await;
        }
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "the self-fence must have fired during the outage"
        );
        assert!(!state.is_leader.load(Ordering::Relaxed));
        assert_eq!(
            mock.holder().as_deref(),
            Some("us"),
            "the fence is local: the apiserver still names us"
        );

        // Recovery + SIGTERM with NO further election round (no tick):
        // the graceful release must still run — we acquired and never
        // observed our supersession.
        mock.set_behavior(MockBehavior::Healthy);
        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
        assert_eq!(
            mock.holder(),
            None,
            "shutdown after a self-fence must still release the lease \
             (the local fence does not un-hold it at the apiserver)"
        );
    }

    /// The legitimate-skip half of the same gate: once a completed
    /// round OBSERVES our supersession (Standby resolution while
    /// another replica holds), the hold is genuinely gone and shutdown
    /// must NOT touch the lease.
    // r[verify sched.lease.graceful-release+2]
    #[tokio::test(start_paused = true)]
    async fn shutdown_after_observed_supersession_skips_release() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        settle().await;
        assert!(state.is_leader.load(Ordering::Relaxed), "t=0 acquire");

        // Fence during an outage...
        mock.set_behavior(MockBehavior::FailFast);
        for _ in 0..3 {
            tokio::time::advance(RENEW_INTERVAL).await;
            settle().await;
        }
        assert_eq!(hooks.loses.lock().expect("loses lock").len(), 1);

        // ...meanwhile the standby stole the lease (out-of-band write,
        // rv above anything we have seen)...
        mock.seed(serde_json::json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": { "name": "rio-sched", "namespace": "default",
                          "resourceVersion": "50" },
            "spec": { "holderIdentity": "other", "leaseTransitions": 3 },
        }));

        // ...and one healthy round OBSERVES the supersession (Standby).
        mock.set_behavior(MockBehavior::Healthy);
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert!(!state.is_leader.load(Ordering::Relaxed));

        let requests_before_shutdown = mock.requests().len();
        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
        assert_eq!(
            mock.holder().as_deref(),
            Some("other"),
            "an observed supersession means nothing of ours to release"
        );
        let tail = mock.requests().split_off(requests_before_shutdown);
        assert!(
            !tail
                .iter()
                .any(|(m, p)| *m == http::Method::PUT && p.contains("/leases/")),
            "no lease PUT after shutdown: the release is skipped, got {tail:?}"
        );
    }

    /// The in-flight-straddle half of the suspend-blindness class
    /// (bug_096): a suspend that straddles an IN-FLIGHT renew must still
    /// fence. The renew is sent before the suspend, the apiserver
    /// answers, and the response is read after resume — with the blind
    /// window anchored at the RESPONSE, the post-resume stamp erases the
    /// entire suspended interval and the zombie leader survives; with
    /// the anchor minted BEFORE the attempt's await (anchor <= send <=
    /// commit), the stamp preserves the blind time and the next
    /// tick-time check fences.
    // r[verify sched.lease.self-fence+2]
    #[tokio::test(start_paused = true)]
    async fn fence_fires_after_suspend_straddles_inflight_renew() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let jump_ms = Arc::new(AtomicU64::new(0));
        let fence_now = {
            let a = Instant::now();
            let jump_ms = jump_ms.clone();
            move || a.elapsed() + Duration::from_millis(jump_ms.load(Ordering::SeqCst))
        };
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            fence_now,
        ));

        // t=0: the immediate first tick acquires healthily.
        settle().await;
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "t=0 acquire must succeed"
        );
        let requests_after_acquire = mock.requests().len();

        // Park the apiserver: the +5s tick's renew round-trip is held
        // in flight (sent, unanswered).
        mock.set_behavior(MockBehavior::Park);
        tokio::time::advance(RENEW_INTERVAL).await; // tick at +5s, renew in flight
        settle().await;

        // "Suspend" WHILE the round-trip is in flight: the fence clock
        // jumps 12s (> SELF_FENCE_AFTER = 11s); the tokio virtual clock
        // — the monotonic view, which also drives the attempt timeout —
        // does not advance, exactly like CLOCK_MONOTONIC across a host
        // suspend.
        jump_ms.store(12_000, Ordering::SeqCst);

        // Resume: the parked round-trip completes successfully (the
        // request log proves the round actually happened — the red/
        // green difference must be about anchoring, not about a failed
        // attempt, whose un-stamped error arm would fence even
        // pre-fix). Repeated release+settle drains the GET, the PUT,
        // and the detached marks PATCH the Ok arm spawns.
        for _ in 0..4 {
            mock.release_parked();
            settle().await;
        }
        assert!(
            mock.requests().len() > requests_after_acquire,
            "the parked renew round-trip must have been issued"
        );
        assert_eq!(
            mock.holder().as_deref(),
            Some("us"),
            "the parked renew must have completed successfully (still holder)"
        );
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "no second acquire edge: the round resolved Leading-steady"
        );

        // First post-resume tick: the blind window (anchored before the
        // await) now spans the suspend — the tick-time fence check MUST
        // fire before the next attempt starts.
        tokio::time::advance(RENEW_INTERVAL).await;
        settle().await;
        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "the first post-resume tick-time fence check must fence the zombie leader \
             (a stamp from the post-suspend response erased the blind window)"
        );
        assert_eq!(
            hooks.loses.lock().expect("loses lock").len(),
            1,
            "exactly one lose: the suspend-straddled renew must not reset the blind window"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// The bump-confirmation wiring (`sched.recovery.bump-confirm`)
    /// rests on two loop-level properties: the round id is taken BEFORE
    /// the attempt's apiserver I/O starts, and only rounds that resolve
    /// `Leading` are confirmed. A regression on either turns the
    /// scheduler-side confirmation vacuous (it could observe a
    /// confirmation from a round that began before its claim, or from a
    /// round that did not end with us holding the Lease). Drives the
    /// real loop against the mock apiserver under a paused clock.
    // r[verify sched.recovery.bump-confirm+3]
    #[tokio::test(start_paused = true)]
    async fn leading_rounds_confirm_failed_rounds_do_not() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: the immediate first tick creates the Lease and acquires;
        // that round must be confirmed.
        settle().await;
        assert!(state.is_leader(), "healthy first tick acquires");
        let confirmed_at_acquire = state.last_leading_round();
        assert!(
            confirmed_at_acquire >= 1,
            "the acquiring round must be confirmed"
        );

        // A snapshot taken between ticks is only ever exceeded by a
        // strictly later Leading round: the next healthy renew's round
        // id is above the snapshot and becomes the confirmed round.
        let snapshot = state.renew_rounds_started();
        tokio::time::advance(RENEW_INTERVAL).await; // healthy renew at +5s
        settle().await;
        assert!(
            state.renew_rounds_started() > snapshot,
            "the renew tick consumed a round id"
        );
        assert!(
            state.last_leading_round() > snapshot,
            "a Leading round that began after the snapshot confirms above it"
        );

        // Failed and hung rounds consume round ids but never confirm.
        let confirmed_before_failures = state.last_leading_round();
        mock.set_behavior(MockBehavior::FailFast);
        tokio::time::advance(RENEW_INTERVAL).await; // FailFast at +10s
        settle().await;
        mock.set_behavior(MockBehavior::Hang);
        tokio::time::advance(RENEW_INTERVAL).await; // Hang at +15s
        settle().await;
        // Let the hung attempt's deadline expire.
        tokio::time::advance(RENEW_INTERVAL - RENEW_SLOP).await;
        settle().await;
        assert!(
            state.renew_rounds_started() >= confirmed_before_failures + 2,
            "failed/hung rounds still consume round ids"
        );
        assert_eq!(
            state.last_leading_round(),
            confirmed_before_failures,
            "failed/hung rounds must never be confirmed"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// Rounds that resolve `Standby` (another replica holds the Lease)
    /// are never confirmed: `last_leading_round` only ever names rounds
    /// in which WE were the holder, which is exactly the evidence the
    /// scheduler's bump-confirmation consumes.
    // r[verify sched.recovery.bump-confirm+3]
    #[tokio::test(start_paused = true)]
    async fn standby_rounds_never_confirm() {
        let (client, mock) = MockApiServer::new();
        // A live foreign holder: freshly observed, so decide() stays
        // Standby for every tick this test advances through (well under
        // STEAL_AFTER).
        mock.seed(json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "rio-sched",
                "namespace": "default",
                "resourceVersion": "100",
            },
            "spec": {
                "holderIdentity": "other-replica",
                "leaseTransitions": 3,
                "leaseDurationSeconds": 15,
            },
        }));
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        settle().await; // tick at t=0
        tokio::time::advance(RENEW_INTERVAL).await; // +5s
        settle().await;
        tokio::time::advance(RENEW_INTERVAL).await; // +10s
        settle().await;

        assert!(!state.is_leader(), "foreign holder: we stay standby");
        assert!(
            state.renew_rounds_started() >= 3,
            "standby rounds still consume round ids"
        );
        assert_eq!(
            state.last_leading_round(),
            0,
            "standby rounds must never be confirmed"
        );

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }

    /// A holder change that lands entirely inside this replica's
    /// observation gap — a foreign term that ended in a graceful vacate,
    /// or a delete/recreate — produces no acquire or lose edge: the next
    /// successful round resolves `Leading` while `was_leading` is already
    /// true. The loop must treat the moved `leaseTransitions` count as a
    /// late-observed holder change (a "rebound"): re-record the count,
    /// re-derive the generation, clear `recovery_complete`, and re-fire
    /// the acquire hook so the consumer re-runs recovery against the
    /// post-change state — without ever firing the lose hook. A renew
    /// that observes the SAME count must not rebound (steady state); a
    /// delete/recreate whose count lands back exactly on the recorded
    /// value is equally undetectable — that count coincidence is the
    /// accepted residual (see the recovery gate's entry-snapshot comment
    /// in rio-scheduler).
    // r[verify sched.lease.rebound+4]
    #[tokio::test(start_paused = true)]
    async fn still_leading_rounds_rebound_on_moved_transition_count() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
            leader_pod_label: None,
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
            {
                let a = Instant::now();
                move || a.elapsed()
            },
        ));

        // t=0: the immediate first tick creates the Lease (creator:
        // transitions 0) and acquires. Emulate the actor finishing its
        // recovery so the rebound's clear is observable.
        settle().await;
        assert!(state.is_leader(), "healthy first tick acquires");
        assert_eq!(state.acquired_transitions(), 0, "creator records 0");
        state.set_recovery_complete(state.acquired_transitions());

        // A healthy renew that observes the same transition count must
        // NOT rebound — the steady state. (A delete/recreate whose count
        // lands back exactly on the recorded value is indistinguishable
        // from this arm; that coincidence is the accepted residual.)
        tokio::time::advance(RENEW_INTERVAL).await; // +5s
        settle().await;
        assert_eq!(
            hooks.acquires.lock().expect("acquires lock").len(),
            1,
            "a renew with an unchanged count must not re-fire the acquire hook"
        );
        assert!(
            state.recovery_complete(),
            "a renew with an unchanged count must not clear recovery_complete"
        );

        // Out of band: a foreign term ran and gracefully vacated inside
        // our observation gap — the Lease now has an empty holder and a
        // moved resourceVersion; the count our next steal derives will
        // differ from the recorded one.
        mock.seed(json!({
            "apiVersion": "coordination.k8s.io/v1",
            "kind": "Lease",
            "metadata": {
                "name": "rio-sched",
                "namespace": "default",
                "resourceVersion": "50",
            },
            "spec": {
                "holderIdentity": null,
                "leaseTransitions": 0,
                "leaseDurationSeconds": 15,
            },
        }));

        // The next tick GETs an empty holder and steals it back
        // (transitions 0 → 1) while `was_leading` is still true: the
        // rebound path.
        tokio::time::advance(RENEW_INTERVAL).await; // +10s
        settle().await;
        assert_eq!(
            state.acquired_transitions(),
            1,
            "the rebound re-records the observed transition count"
        );
        assert_eq!(
            state.generation(),
            2,
            "the generation re-derives from the observed count"
        );
        assert!(
            !state.recovery_complete(),
            "the rebound clears recovery_complete so recovery re-runs"
        );
        assert!(state.is_leader(), "a rebound never clears is_leader");
        {
            let acquires = hooks.acquires.lock().expect("acquires lock");
            let loses = hooks.loses.lock().expect("loses lock");
            let rebounds = hooks.rebounds.lock().expect("rebounds lock");
            assert_eq!(
                acquires.len(),
                1,
                "the rebound fires its OWN hook, not a second acquire"
            );
            assert_eq!(rebounds.len(), 1, "the rebound fires the rebound hook");
            assert_eq!(loses.len(), 0, "the rebound must not fire the lose hook");
        }

        // The 404-recreate cousin: an operator deletes the Lease
        // outright; our next tick re-creates it at transitions 0 — a
        // second rebound (0 differs from the recorded 1).
        state.set_recovery_complete(state.acquired_transitions());
        assert!(mock.clear(), "lease deleted out of band");
        tokio::time::advance(RENEW_INTERVAL).await; // +15s
        settle().await;
        assert_eq!(
            state.acquired_transitions(),
            0,
            "the recreate's transition count is re-recorded"
        );
        assert_eq!(
            state.generation(),
            2,
            "the generation never regresses (fetch_max)"
        );
        assert!(
            !state.recovery_complete(),
            "the 404-recreate rebound re-runs recovery"
        );
        {
            let acquires = hooks.acquires.lock().expect("acquires lock");
            let loses = hooks.loses.lock().expect("loses lock");
            let rebounds = hooks.rebounds.lock().expect("rebounds lock");
            assert_eq!(acquires.len(), 1, "still exactly one acquire edge");
            assert_eq!(
                rebounds.len(),
                2,
                "the 404-recreate rebound fires the rebound hook"
            );
            assert_eq!(loses.len(), 0, "still no lose edge");
        }

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }
}
