//! Kubernetes Lease-based leader election.
//!
//! When `lease_name` is configured, a background task acquires and
//! renews a `coordination.k8s.io/v1` Lease. On acquire, it derives
//! the generation from the lease's transition count (workers reject
// r[impl sched.lease.k8s-lease+2]
// r[impl sched.lease.generation-fence+2]
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
//! - Workers compare `WorkAssignment.generation` against
//!   `HeartbeatResponse.generation`. Once the new leader's
//!   generation reaches them via heartbeat, the old leader's
//!   assignments are stale and workers reject them.
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

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::serde_json::json;
use kube::api::{Api, Patch, PatchParams};
// tokio's Instant, not std's: identical semantics in production (a thin
// wrapper over std's monotonic clock), but it follows tokio's test clock
// under `start_paused`, which is what makes the lease loop's fence-check
// cadence testable end to end (see the loop-cadence test in `mod tests`).
use tokio::time::Instant;
use tracing::{debug, info, warn};

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
    /// `is_leader=true`. Also fired on a rebound — a holder change
    /// observed late on a still-leading round, AFTER
    /// [`LeaderState::on_rebound`] re-recorded the observed transition
    /// count and cleared `recovery_complete` — so consumers MUST
    /// tolerate a second call with no intervening [`on_lose`](Self::on_lose).
    fn on_acquire(&self);
    /// Called once per leader→standby transition (explicit lose OR local
    /// self-fence), AFTER [`LeaderState::on_lose`] has cleared `is_leader`
    /// and `recovery_complete`.
    fn on_lose(&self);
}

// TODO: use CLOCK_BOOTTIME for the self-fence clock instead of
// Instant::now() (= CLOCK_MONOTONIC on Linux). MONOTONIC does not advance
// during host suspend; a suspend-and-resume leaves `last_successful_renew`
// looking fresh while real time advanced past SELF_FENCE_AFTER — the leader
// resumes still believing it leads, until the next failed apiserver
// round-trip. BOOTTIME advances during suspend, so the self-fence fires
// immediately on resume. Low priority for k8s nodes (they don't suspend);
// worth doing before any bare-metal or laptop deployment of the scheduler.
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

/// The asymmetry margin between the leader's self-fence deadline and the
/// follower's steal threshold. The two deadlines are anchored at
/// different moments (the leader stamps `last_successful_renew` when its
/// renew RESPONSE arrives; the follower stamps `obs.at` when it OBSERVES
/// the rv change), so without a margin the follower's deadline can land
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
/// the apiserver COMMIT of its last renew, while production stamps
/// `last_successful_renew` at the RESPONSE arrival; that is sound
/// because renew attempts start exactly on interval ticks and the send
/// precedes the commit, so the anchoring renew's response can lag its
/// commit by at most the attempt deadline (RENEW_INTERVAL − RENEW_SLOP),
/// and that deadline stays strictly under the gap from SELF_FENCE_AFTER
/// up to the next RENEW_INTERVAL multiple — equivalently RENEW_SLOP >
/// SELF_FENCE_AFTER mod RENEW_INTERVAL — so the response lag cannot
/// push the firing tick past the commit-anchored bound (commit +
/// SELF_FENCE_AFTER + RENEW_INTERVAL) the model assumes; that arithmetic
/// premise is pinned by the const assert below. What remains of
/// the separation is a 1.5s one-sided clock-skew budget — far above NTP
/// drift on cloud nodes. A clock pause longer than that re-opens the
/// window, which is the impossibility result the generation fence
/// (r\[sched.lease.generation-fence+2\]) backstops. The model also shows
/// the bound is tight: one tick less separation and a dual-belief state
/// is reachable.
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

/// A follower steals after observing the same resourceVersion for this
/// long: LEASE_TTL + FENCE_MARGIN. Failover after a real leader death
/// takes up to this long plus one renew interval.
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
    // The response-anchoring premise in FENCE_MARGIN's doc: production
    // stamps last_successful_renew at the renew RESPONSE, which can lag
    // the apiserver commit by up to the attempt deadline
    // (RENEW_INTERVAL - RENEW_SLOP). The tick-grid fence checks — the
    // only cadence the apiserver cannot delay; the error-arm re-check
    // only fires earlier — then stay within the commit-anchored bound
    // the model assumes (commit + SELF_FENCE_AFTER + RENEW_INTERVAL)
    // only while that deadline is strictly under the gap from
    // SELF_FENCE_AFTER up to the next tick multiple.
    assert!(
        RENEW_SLOP.as_secs() > SELF_FENCE_AFTER.as_secs() % RENEW_INTERVAL.as_secs(),
        "the renew attempt deadline must keep the response-anchored self-fence within the model's commit-anchored bound"
    );
    // The leader must get at least one renew attempt before fencing.
    assert!(
        SELF_FENCE_AFTER.as_secs() > RENEW_INTERVAL.as_secs(),
        "the leader must get at least one renew attempt before fencing"
    );
};

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
        })
    }
}

/// Recovery-completion sentinel: no acquire-epoch has completed
/// recovery. Lives outside the realistic `leaseTransitions` range (the
/// apiserver count starts at 0 and increments per holder change), so it
/// can never compare equal to a recorded `acquired_transitions`.
const RECOVERY_NOT_COMPLETE: u64 = u64::MAX;

/// Shared leader state. The lease task writes; actor + health read.
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
    /// [`on_lose`](Self::on_lose) / [`on_rebound`](Self::on_rebound) so
    /// re-acquire (or the rebound's re-fired hook) re-triggers recovery.
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
}

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
    /// independent `is_leader` check, the heartbeat may briefly
    /// advertise that raced completion's generation (already claimed in
    /// the ledger, so a successor seeds above it and the executor fence
    /// is a `fetch_max` floor — see `GenerationReader::advertised`), and
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
    // r[impl sched.admin.list-executors-leader-age]
    pub fn leader_for(&self) -> Option<Duration> {
        self.became_leader_at.read().map(|t| t.elapsed())
    }

    /// Monotonically raise generation to at least `target`. `Release`
    /// `fetch_max` — defensive against Lease annotation reset
    /// (`kubectl delete lease` zeros the annotation; PG's high-water
    /// persists). Returns the previous value.
    pub fn seed_generation_from(&self, target: u64) -> u64 {
        self.generation.fetch_max(target, Ordering::Release)
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
    // r[impl sched.lease.generation-fence+2]
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
    /// runs. Every path into an acquire edge already has
    /// `recovery_complete()` false (`pending()`, the lose arm, the
    /// self-fence), so `on_acquire` can never raise the generation while
    /// the predicate is true — a rebound is the first writer that could.
    /// Clearing the stamp first means a heartbeat reader cannot pair a
    /// still-true predicate with the rebound-raised generation, except
    /// in the already-accepted two-load-straddle case documented at the
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
    // r[impl sched.lease.rebound]
    // r[impl sched.lease.generation-fence+2]
    pub fn on_rebound(&self, lease_transitions: u64) -> u64 {
        self.recovery_completed_for
            .store(RECOVERY_NOT_COMPLETE, Ordering::SeqCst);
        self.acquired_transitions
            .store(lease_transitions, Ordering::SeqCst);
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

    run_lease_loop_with_client(client, cfg, state, hooks, shutdown).await;
}

/// The lease loop proper, with the [`kube::Client`] injected. Everything
/// [`run_lease_loop`] documents happens here; the public wrapper only
/// constructs the in-cluster client. Split so tests can drive the real
/// loop against an in-process mock apiserver
/// (`rio_test_support::kube_mock::MockApiServer`) under a paused clock —
/// the fence-check-cadence test in `mod tests` is the consumer.
pub(crate) async fn run_lease_loop_with_client<H: LeaseHooks>(
    client: kube::Client,
    cfg: LeaseConfig,
    state: LeaderState,
    hooks: H,
    shutdown: rio_common::signal::Token,
) {
    // Clone for pod-deletion-cost patching. LeaderElection::new
    // takes ownership (wraps the client in Api<Lease>).
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

    let mut was_leading = false;
    let mut owe_cost_clear = false;
    let mut last_successful_renew = Instant::now();
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
            &mut was_leading,
            &mut owe_cost_clear,
            last_successful_renew,
        ) {
            hooks.on_lose();
        }

        let renew_deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        // Round id BEFORE the attempt starts: a consumer that snapshots
        // `renew_rounds_started` and later sees `last_leading_round`
        // above it knows the confirming round began after the snapshot
        // (sched.recovery.bump-confirm).
        let round = state.begin_renew_round();
        match tokio::time::timeout(renew_deadline, election.try_acquire_or_renew()).await {
            Ok(Ok(result)) => {
                // Successful round-trip (apiserver answered). Even
                // Standby/Conflict reset the self-fence clock — we
                // KNOW the apiserver state, we just don't hold the
                // lease. The clock tracks "am I blind", not "am I
                // leader".
                last_successful_renew = Instant::now();
                // Conflict on renew = someone stole since our GET
                // → unambiguous lose. Conflict on steal = another
                // standby raced us → we were never leading. Both
                // map to now_leading=false; was_leading edge-
                // detection below distinguishes the lose case.
                //
                // Leading carries the lease's transition count so the
                // acquire arm can derive the generation from it;
                // None ⇔ not leading.
                let leading_transitions = match result {
                    ElectionResult::Leading { transitions } => Some(transitions),
                    ElectionResult::Standby | ElectionResult::Conflict => None,
                };
                let now_leading = leading_transitions.is_some();

                // r[impl sched.lease.deletion-cost]
                // Deferred deletion-cost clear: if we self-fenced
                // while the apiserver was unreachable, the cost=0
                // patch was owed but skipped (no connectivity).
                // This is the first reachable observation since —
                // pay the debt now. Level-triggered (not edge):
                // `was_leading` was already flipped false by the
                // self-fence so the lose-transition arm below will
                // never see the edge. If we re-acquired before
                // anyone else (`now_leading`), the acquire arm
                // sets cost=1 anyway, so just drop the debt.
                if std::mem::take(&mut owe_cost_clear) && !now_leading {
                    spawn_patch_deletion_cost(
                        pod_patch_client.clone(),
                        cfg.namespace.clone(),
                        cfg.holder_id.clone(),
                        0,
                    );
                }

                // Edge detection on (leading?, was_leading). Binding
                // the transition count in the acquire pattern makes
                // "acquired without a transition count to derive the
                // generation from" unrepresentable — the arm cannot
                // execute without a value to hand to on_acquire.
                match (leading_transitions, was_leading) {
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

                        // r[impl sched.lease.deletion-cost]
                        // Annotate our own Pod with pod-deletion-cost=1.
                        // K8s's ReplicaSet controller sorts by this when
                        // picking which pod to kill during scale-down
                        // (incl. RollingUpdate). Leader gets the higher
                        // cost → k8s kills the standby first → no
                        // leadership churn on rollout. Fire-and-forget:
                        // the lease loop MUST NOT block (see below).
                        // Patch failure is non-fatal — without the
                        // annotation, k8s picks arbitrarily (rollout
                        // still works, just with possible double-churn).
                        spawn_patch_deletion_cost(
                            pod_patch_client.clone(),
                            cfg.namespace.clone(),
                            cfg.holder_id.clone(),
                            1,
                        );

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
                    }
                    (None, true) => {
                        // ---- Lose transition ----
                        // Someone else acquired (we couldn't renew in
                        // time). Stop dispatching. The generation is
                        // the NEW leader's concern — it derives its
                        // own from the lease's transition count on
                        // acquire. on_lose clears both is_leader and
                        // recovery_complete (SeqCst): if we
                        // re-acquire, recovery runs again — the
                        // other replica's actions may have changed PG.
                        state.on_lose();
                        warn!(
                            holder = %cfg.holder_id,
                            "lost leadership (another replica acquired)"
                        );

                        // r[impl sched.lease.standby-tick-noop+2]
                        // Symmetric with on_acquire above: fire the
                        // per-component on-lose hook (metrics + actor
                        // notification). Same non-blocking constraint.
                        // is_leader is already false (above) so the
                        // consumer's tick early-returns regardless;
                        // this just lets it drop stale state and zero
                        // leader-only gauges.
                        hooks.on_lose();

                        // Clear deletion cost — we're standby now, K8s
                        // should prefer to kill us over the new leader.
                        spawn_patch_deletion_cost(
                            pod_patch_client.clone(),
                            cfg.namespace.clone(),
                            cfg.holder_id.clone(),
                            0,
                        );
                        owe_cost_clear = false;
                    }
                    (Some(transitions), true) => {
                        // ---- Still leading ----
                        // Steady state is transitions == the count
                        // recorded at our last acquire edge or rebound:
                        // renews never bump the count, and a foreign
                        // holder still present at our next successful
                        // round resolves through the lose edge instead —
                        // so an unequal value here means a holder change
                        // (foreign term + vacate, or delete/recreate)
                        // landed entirely inside our observation gap.
                        // Synthesize the missing transition: re-derive
                        // local state and re-fire the acquire hook so
                        // the consumer re-runs recovery against the
                        // post-term state. Deliberately NO on_lose():
                        // a synthesized lose would force a pointless
                        // wipe of state the immediately-following
                        // re-recovery rebuilds and (if the full lose
                        // edge were synthesized) an is_leader=false
                        // blip, while adding nothing to dispatch
                        // gating — on_rebound's own recovery_complete
                        // clear already gates dispatch during the
                        // re-recovery. Hook delivery is ordered
                        // (sched.lease.hook-order), so skipping the
                        // lose is about avoiding wasted work, not a
                        // reordering hazard. No
                        // spawn_patch_deletion_cost either — the cost
                        // annotation is already 1 from the original
                        // acquire. The count-coincidence ABA (the
                        // observed value lands back exactly on the
                        // recorded one) remains the accepted residual —
                        // see the recovery gate's entry-snapshot comment
                        // in rio-scheduler. Equal counts stay a silent
                        // no-op (a log every 5s would be noisy).
                        // r[impl sched.lease.rebound]
                        let recorded = state.acquired_transitions();
                        if transitions != recorded {
                            let new_gen = state.on_rebound(transitions);
                            warn!(
                                recorded,
                                observed = transitions,
                                generation = new_gen,
                                holder = %cfg.holder_id,
                                "lease transition count moved while still leading — \
                                 unobserved holder change inside our observation gap; \
                                 re-running recovery"
                            );
                            hooks.on_acquire();
                        }
                    }
                    // Steady state: still standby while someone else
                    // holds. No log — 5s interval would be noisy.
                    (None, false) => {}
                }

                was_leading = now_leading;
                // r[impl sched.recovery.bump-confirm+2]
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
            outcome @ (Ok(Err(_)) | Err(_)) => {
                // Either apiserver returned an error (Ok(Err)) or
                // our timeout fired before it answered (Err(Elapsed)).
                // Both mean: no fresh view of the Lease object.
                match &outcome {
                    Ok(Err(e)) => {
                        warn!(error = %e, "lease renew failed (apiserver error); retrying next tick");
                    }
                    Err(_) => {
                        warn!(deadline = ?renew_deadline, "lease renew TIMED OUT (apiserver hung?); retrying next tick");
                    }
                    Ok(Ok(_)) => unreachable!(),
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
                // generation fence (r[sched.lease.generation-fence+2])
                // saves correctness either way; this fence saves ops
                // sanity.
                if maybe_self_fence(
                    &state,
                    &mut was_leading,
                    &mut owe_cost_clear,
                    last_successful_renew,
                ) {
                    // Self-fence is a lose-transition: same on-lose
                    // hook as the explicit lose arm above.
                    hooks.on_lose();
                }
            }
        }
    }

    // r[impl sched.lease.graceful-release]
    // Graceful release: on shutdown, release the lease so the next
    // replica acquires on its next poll tick (one RENEW_INTERVAL, 5s)
    // instead of waiting out the steal threshold (19s). Gate on
    // was_leading to skip the apiserver round-trip when we were
    // standby all along. Any error is non-fatal: we're shutting down
    // regardless, and the steal threshold is the fallback.
    if was_leading {
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
/// Returns `true` if the fence fired (for test assertions).
// r[impl sched.lease.self-fence+2]
fn maybe_self_fence(
    state: &LeaderState,
    was_leading: &mut bool,
    owe_cost_clear: &mut bool,
    last_successful_renew: Instant,
) -> bool {
    if *was_leading && last_successful_renew.elapsed() > SELF_FENCE_AFTER {
        warn!(
            blind_for = ?last_successful_renew.elapsed(),
            self_fence_after_secs = SELF_FENCE_AFTER.as_secs(),
            "LOCAL SELF-FENCE: no successful renew in > SELF_FENCE_AFTER, stepping down locally"
        );
        state.on_lose();
        *was_leading = false;
        // No spawn_patch_deletion_cost here: can't reach apiserver.
        // Record the debt so the FIRST reachable round-trip in
        // run_lease_loop's Ok arm patches cost=0. The peer's cost=1
        // doesn't clear OURS — without this deferred clear we'd stay
        // tied at cost=1 with the new leader and the next
        // RollingUpdate picks arbitrarily.
        *owe_cost_clear = true;
        true
    } else {
        false
    }
}

// r[impl sched.lease.deletion-cost]
/// Fire-and-forget PATCH on our own Pod's `controller.kubernetes.io/
/// pod-deletion-cost` annotation. K8s's ReplicaSet controller sorts
/// pods by this value (ascending) when deciding which to evict during
/// scale-down --- including RollingUpdate surge reconciliation. Leader
/// sets cost=1, standby sets cost=0, k8s kills the standby first.
///
/// `tokio::spawn` because the lease loop MUST NOT block. A slow
/// apiserver PATCH would stall the renew tick — the blocked loop can
/// neither renew nor self-fence while a standby steals after
/// `STEAL_AFTER` of observed staleness — dual-leader. Same constraint
/// as the LeaderAcquired actor send (see `run_lease_loop`).
///
/// Merge patch (not Apply): we only touch one annotation key; Apply
/// would need a fieldManager and a fuller object shape. Merge is
/// `kubectl annotate --overwrite` semantics.
fn spawn_patch_deletion_cost(client: kube::Client, namespace: String, pod_name: String, cost: i32) {
    tokio::spawn(async move {
        let pods: Api<Pod> = Api::namespaced(client, &namespace);
        // The annotation value is a string (all k8s annotations are),
        // parsed as int32 by the ReplicaSet controller. Invalid
        // values sort as 0.
        let patch = json!({
            "metadata": {
                "annotations": {
                    "controller.kubernetes.io/pod-deletion-cost": cost.to_string()
                }
            }
        });
        match pods
            .patch(&pod_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => debug!(%pod_name, cost, "patched pod-deletion-cost"),
            Err(e) => {
                // Non-fatal: rollout still works, k8s just picks
                // arbitrarily. RBAC missing `patch pods` is the
                // likely cause — 403 Forbidden. VM tests don't
                // catch this (k3s admin kubeconfig bypasses RBAC).
                warn!(
                    %pod_name, cost, error = %e,
                    "failed to patch pod-deletion-cost (rollout still works, \
                     k8s will pick arbitrarily during scale-down)"
                );
            }
        }
    });
}

// r[verify sched.lease.k8s-lease+2]
// r[verify sched.lease.generation-fence+2]
#[cfg(test)]
mod tests {
    use super::*;

    /// from_parts returns None when lease_name unset — the signal
    /// for "non-K8s mode." This is how VM tests stay unaffected.
    /// Previously `from_env()` read `std::env::var("RIO_LEASE_NAME")`
    /// directly (bypassing the config loader); now the scheduler's
    /// Config passes the merged value through.
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

    // r[verify sched.admin.list-executors-leader-age]
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
    // r[verify sched.lease.generation-fence+2]
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
    // r[verify sched.recovery.bump-confirm+2]
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
    // r[verify sched.lease.rebound]
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
    // r[verify sched.lease.rebound]
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
    // r[verify sched.lease.graceful-release]
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

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        // 20s ago > SELF_FENCE_AFTER (11s). Same back-dated Observed
        // pre-seeding as election.rs's steal-test fixtures.
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

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
            !was_leading,
            "was_leading should flip so next tick is edge-free"
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

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        // 10s ago < SELF_FENCE_AFTER (11s). Two failed ticks, lease
        // still validly held as far as we know.
        let last_renew = Instant::now() - Duration::from_secs(10);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

        assert!(!fired, "within SELF_FENCE_AFTER → no self-fence");
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "within SELF_FENCE_AFTER → still leader (transient blip)"
        );
        assert!(state.recovery_complete());
        assert!(was_leading);
    }

    /// Self-fence is gated on `was_leading`. A standby that has
    /// NEVER held the lease should not "step down" — it has nothing
    /// to step down from. Avoids spurious lease_lost_total increments
    /// from a standby whose apiserver connectivity is flaky.
    #[test]
    fn self_fence_no_op_when_not_leading() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        // is_leader already false, recovery_complete already false.

        let mut was_leading = false;
        let mut owe_cost_clear = false;
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

        assert!(!fired, "not leading → no fence even past TTL");
        assert!(!state.is_leader.load(Ordering::Relaxed));
        assert!(!was_leading);
    }

    /// Self-fence sets `owe_cost_clear` so the lease loop's first
    /// reachable round-trip clears our pod-deletion-cost annotation.
    /// Without the deferred clear, an ex-leader keeps cost=1 tied with
    /// the new leader (peer's cost=1 patch doesn't touch OUR pod) and
    /// the next RollingUpdate evicts arbitrarily — defeating
    /// `r[sched.lease.deletion-cost]`. Regression: maybe_self_fence
    /// previously consumed the `was_leading` edge without arranging
    /// the deferred patch.
    // r[verify sched.lease.deletion-cost]
    #[test]
    fn self_fence_sets_owe_cost_clear() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.set_recovery_complete(state.acquired_transitions());

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);
        assert!(fired);
        assert!(
            owe_cost_clear,
            "self-fence must record the owed cost=0 patch (apiserver unreachable now, \
             so the lease loop pays it on first reachable round-trip)"
        );

        // No-fire path leaves the flag untouched (a standby that never
        // led has no cost to clear).
        let standby = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let mut was_leading = false;
        let mut owe_cost_clear = false;
        let fired = maybe_self_fence(
            &standby,
            &mut was_leading,
            &mut owe_cost_clear,
            Instant::now() - Duration::from_secs(20),
        );
        assert!(!fired);
        assert!(!owe_cost_clear, "no-fire → no debt recorded");
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
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state,
            hooks.clone(),
            shutdown.clone(),
        ));

        // t=0: the interval's first tick fires immediately; the mock is
        // Healthy, so the attempt creates the Lease and the acquire arm
        // runs. t_acquire is a valid proxy for `last_successful_renew`
        // ONLY because this scripted schedule makes the t=0 acquire the
        // last successful round-trip — if the schedule ever gains
        // another Healthy tick, the anchor must move to that tick.
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

    /// The bump-confirmation wiring (`sched.recovery.bump-confirm`)
    /// rests on two loop-level properties: the round id is taken BEFORE
    /// the attempt's apiserver I/O starts, and only rounds that resolve
    /// `Leading` are confirmed. A regression on either turns the
    /// scheduler-side confirmation vacuous (it could observe a
    /// confirmation from a round that began before its claim, or from a
    /// round that did not end with us holding the Lease). Drives the
    /// real loop against the mock apiserver under a paused clock.
    // r[verify sched.recovery.bump-confirm+2]
    #[tokio::test(start_paused = true)]
    async fn leading_rounds_confirm_failed_rounds_do_not() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
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
    // r[verify sched.recovery.bump-confirm+2]
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
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
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
    // r[verify sched.lease.rebound]
    #[tokio::test(start_paused = true)]
    async fn still_leading_rounds_rebound_on_moved_transition_count() {
        let (client, mock) = MockApiServer::new();
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let cfg = LeaseConfig {
            lease_name: "rio-sched".into(),
            namespace: "default".into(),
            holder_id: "us".into(),
        };
        let hooks = RecordingHooks::default();
        let shutdown = rio_common::signal::Token::new();
        let loop_task = tokio::spawn(run_lease_loop_with_client(
            client,
            cfg,
            state.clone(),
            hooks.clone(),
            shutdown.clone(),
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
            assert_eq!(acquires.len(), 2, "the rebound re-fires the acquire hook");
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
            assert_eq!(
                acquires.len(),
                3,
                "the 404-recreate rebound re-fires the acquire hook"
            );
            assert_eq!(loses.len(), 0, "still no lose edge");
        }

        shutdown.cancel();
        loop_task.await.expect("lease loop task exits cleanly");
    }
}
