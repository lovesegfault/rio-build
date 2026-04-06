//! Autoscaling — shared types, predicates, and pure helpers.
//!
//! The two autoscaler loops (cluster-wide standalone vs WPS
//! per-class) live in the [`standalone`] and [`per_class`]
//! submodules. This module holds the surface they both consume:
//! stabilization state/timing, SSA patch body builders, and the
//! `is_wps_owned` ownership predicates that discriminate which
//! loop owns which pool.
//!
//! Split from the monolithic `scaling.rs` (P0381) along the
//! fault line P0234 introduced when it bolted per-class scaling
//! onto the cluster-wide loop — same shape as P0356's
//! `grpc/mod.rs` split.

pub mod component;
pub mod per_class;
pub mod standalone;

// Back-compat: `main.rs` constructs `scaling::Autoscaler` and
// `scaling::ScalingTiming`; the re-export keeps those paths
// stable so the split is transparent to callers.
pub use standalone::Autoscaler;

use std::time::{Duration, Instant};

use kube::ResourceExt;

use crate::crds::builderpool::BuilderPool;
use crate::crds::builderpoolset::BuilderPoolSet;
use crate::crds::fetcherpool::FetcherPool;

/// Stabilization window for scale-down. Long: avoid killing
/// workers right before the next burst. 10 min is the K8s HPA
/// default `--horizontal-pod-autoscaler-downscale-stabilization`;
/// we follow that convention as the DEFAULT. Configurable via
/// `RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS` so VM tests can observe
/// a full up→down cycle without waiting 10 minutes — production
/// deploys leave this at the default.
#[derive(Debug, Clone, Copy)]
pub struct ScalingTiming {
    /// How often to poll ClusterStatus. Granularity for everything
    /// else — polling faster than the up-window doesn't help.
    pub poll_interval: Duration,
    /// Stabilization window for scale-up. Desired must be stable
    /// (same value) for this long before we patch. Queue depth
    /// is high-confidence → react relatively fast.
    pub scale_up_window: Duration,
    /// Stabilization window for scale-down. Default 10 min (K8s
    /// HPA convention). Anti-flap: killing workers right before
    /// the next burst is the #1 autoscaler failure mode.
    pub scale_down_window: Duration,
    /// Minimum interval between any two patches. Anti-flap: a
    /// desired that wobbles around a boundary shouldn't cause
    /// rapid patch churn.
    pub min_scale_interval: Duration,
}

impl Default for ScalingTiming {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            scale_up_window: Duration::from_secs(30),
            scale_down_window: Duration::from_secs(600),
            min_scale_interval: Duration::from_secs(30),
        }
    }
}

/// Configuration error returned by `scale_one` for the caller
/// to surface via `BuilderPool.status.conditions`. Transient
/// errors (K8s API flake, STS not found) return `None` — they
/// log + retry next tick, no condition update.
#[derive(Debug, Clone)]
pub(crate) enum ScaleError {
    /// `spec.autoscaling.metric` is an unrecognized value.
    /// Carried value = the operator's metric string (for the
    /// condition message).
    UnknownMetric(String),
}

/// Per-pool stabilization state. Lives across poll iterations.
#[derive(Debug)]
pub(super) struct ScaleState {
    /// What we computed LAST iteration. If this iteration's
    /// desired differs, reset `stable_since`.
    pub(super) last_desired: i32,
    /// When `desired` last changed. Stabilization window starts here.
    pub(super) stable_since: Instant,
    /// Last actual patch time. Anti-flap check.
    pub(super) last_patch: Instant,
}

impl ScaleState {
    pub(super) fn new(initial: i32, min_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_desired: initial,
            stable_since: now,
            // Initialize to "long ago" so the first patch isn't
            // anti-flap-blocked. checked_sub → None on underflow;
            // unwrap_or(now) is a no-op on the FIRST iteration
            // since stable_since hasn't elapsed anyway. Uses the
            // TUNABLE min_interval — with the 3s VM-test override,
            // a 60s-ago init would work but 2× the actual config
            // value is clearer (exactly "past the anti-flap gate").
            last_patch: now.checked_sub(min_interval * 2).unwrap_or(now),
        }
    }
}

/// SSA field-manager for autoscaler's BuilderPool.status patches.
/// DIFFERENT from the reconciler's "rio-controller" — SSA splits
/// field ownership so the two don't clobber each other.
pub(super) const STATUS_MANAGER: &str = "rio-controller-autoscaler-status";

/// SSA field-manager for per-class (WPS child) StatefulSet
/// replica patches. Distinct from `rio-controller-autoscaler`
/// (standalone BuilderPool scaling) AND from `rio-controller-wps`
/// (the WPS reconciler's child-template sync). `kubectl get sts
/// -o yaml | grep managedFields` shows three managers on a WPS
/// child's STS, each owning its slice: reconciler owns the pod
/// template, this owns spec.replicas, and nothing else touches
/// either.
pub(crate) const WPS_AUTOSCALER_MANAGER: &str = "rio-controller-wps-autoscaler";

/// Build a K8s Condition for the "Scaling" type. Uses json!
/// instead of k8s_openapi's Condition struct because:
/// (1) Condition requires all fields including observedGeneration
/// which we don't track, (2) json! is easier to partial-patch.
///
/// `status`: "True" (scaled OK) / "False" (config error).
/// `reason`: CamelCase machine-readable (ScaledUp/UnknownMetric).
/// `message`: human-readable.
/// `prev`: existing condition of the same type (from the pool's
/// current `.status.conditions`). If its `status` matches, the
/// existing `lastTransitionTime` is preserved — K8s convention
/// is that `lastTransitionTime` changes only when `status`
/// transitions (True↔False), not on every write. Passing `None`
/// (first write, or caller doesn't have the prev) stamps now().
pub(super) fn scaling_condition(
    status: &str,
    reason: &str,
    message: &str,
    prev: Option<&serde_json::Value>,
) -> serde_json::Value {
    serde_json::json!({
        "type": "Scaling",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": transition_time(status, prev),
    })
}

/// Compute `lastTransitionTime` per K8s convention: preserve the
/// existing timestamp if `status` is unchanged, stamp now() on
/// an actual transition (or first write).
///
/// Without this, a reconciler that writes the same condition
/// every tick (ephemeral.rs's 10s requeue) makes
/// `lastTransitionTime` always read "~10s ago" — useless for
/// "when did the scheduler become unreachable."
pub(crate) fn transition_time(new_status: &str, prev: Option<&serde_json::Value>) -> String {
    // k8s_openapi re-exports jiff (kube 3.0's chrono replacement).
    // Timestamp::now() → Display is RFC3339 with offset (UTC Z).
    // K8s Condition.lastTransitionTime expects this format.
    if let Some(p) = prev
        && p.get("status").and_then(|s| s.as_str()) == Some(new_status)
        && let Some(ts) = p.get("lastTransitionTime").and_then(|t| t.as_str())
    {
        return ts.to_string();
    }
    k8s_openapi::jiff::Timestamp::now().to_string()
}

/// Find a condition by `type` in a `BuilderPool.status.conditions`
/// array. Used to read the existing condition before a rewrite so
/// `lastTransitionTime` can be preserved on non-transitions.
///
/// Returns `None` if the pool has no status, no conditions, or no
/// condition of the given type. Serializes via serde_json (the
/// k8s_openapi Condition struct → json::Value) so the output
/// plugs directly into `scaling_condition` / `transition_time`.
pub(crate) fn find_condition(pool: &BuilderPool, cond_type: &str) -> Option<serde_json::Value> {
    pool.status
        .as_ref()?
        .conditions
        .iter()
        .find(|c| c.type_ == cond_type)
        .and_then(|c| serde_json::to_value(c).ok())
}

/// FetcherPool variant of [`find_condition`]. Separate fn (not a
/// trait) because the status field paths differ only nominally —
/// genericizing over the two `*PoolStatus` structs would need a
/// trait both impl, which is more code than two 6-line functions.
pub(crate) fn find_fp_condition(pool: &FetcherPool, cond_type: &str) -> Option<serde_json::Value> {
    pool.status
        .as_ref()?
        .conditions
        .iter()
        .find(|c| c.type_ == cond_type)
        .and_then(|c| serde_json::to_value(c).ok())
}

/// Build the SSA patch body for `BuilderPool.status.{lastScaleTime,
/// conditions}`. Partial status — replicas/ready/desired are the
/// reconciler's fields, not ours.
///
/// Same apiVersion+kind requirement as sts_replicas_patch (and
/// the reconciler's status_patch).
pub(crate) fn wp_status_patch(conditions: &[serde_json::Value]) -> serde_json::Value {
    use kube::CustomResourceExt;
    let ar = BuilderPool::api_resource();
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "lastScaleTime": now,
            "conditions": conditions,
        },
    })
}

/// FetcherPool variant of [`wp_status_patch`]. Only the GVK differs;
/// the status fields (`lastScaleTime`, `conditions`) are the same
/// names in both Status structs.
pub(crate) fn fp_status_patch(conditions: &[serde_json::Value]) -> serde_json::Value {
    use kube::CustomResourceExt;
    let ar = FetcherPool::api_resource();
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "lastScaleTime": now,
            "conditions": conditions,
        },
    })
}

/// Build the SSA patch body for `StatefulSet.spec.replicas`.
///
/// apiVersion + kind are MANDATORY in SSA bodies: without them
/// the apiserver returns 400 "apiVersion must be set". Same
/// pattern as the reconcilers' status patches.
///
/// Extracted as a free fn so a unit test can assert the body
/// shape without spinning up a full mock-apiserver + mock
/// AdminService gRPC.
pub(crate) fn sts_replicas_patch(replicas: i32) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "spec": { "replicas": replicas },
    })
}

/// Queue depth relevant to a pool, given its `spec.systems`.
///
/// I-107: with multi-arch BuilderPools (one-arch-per-pool), the global
/// `queued_derivations` over-scales every pool to the cluster-wide
/// backlog — an aarch64 pool sits at ceiling while the queue is
/// x86-only. The scheduler now returns `queued_by_system` (Ready-only,
/// sum == scalar). We sum the entries whose key is in `systems`.
///
/// Backward compat: empty map (old scheduler) → fall back to the
/// scalar so a controller upgrade ahead of the scheduler still scales.
/// Empty `systems` (operator omitted it) → also fall back to the
/// scalar; nothing to filter on.
pub(crate) fn queued_for_systems(
    status: &rio_proto::types::ClusterStatusResponse,
    systems: &[String],
) -> u32 {
    if status.queued_by_system.is_empty() || systems.is_empty() {
        return status.queued_derivations;
    }
    systems
        .iter()
        .map(|s| status.queued_by_system.get(s).copied().unwrap_or(0))
        .sum()
}

/// Per-class queue depth relevant to a pool, given its `spec.systems`.
///
/// I-143: per-class analogue of [`queued_for_systems`]. Size-class
/// pools are per-arch (one BPS per arch), so an x86-64-tiny pool
/// reading the class-wide `queued` spawns at ceiling for an
/// aarch64-only backlog — half the cluster's builders idle-timeout
/// and respawn forever. Intersect `queued_by_system` with the pool's
/// `systems` to count only work this pool can actually build.
///
/// Backward compat: empty map (old scheduler) OR empty `systems` →
/// fall back to the class scalar. Same posture as I-107: a controller
/// upgrade ahead of the scheduler still scales (over-spawns rather
/// than never spawning).
///
/// `"builtin"` is always summed regardless of `systems` — every
/// executor advertises it unconditionally (rio-builder/main.rs), so
/// a `system="builtin"` FOD can land on any pool. Omitting it
/// from the spawn signal stalls cold-store bootstrap (bootstrap-tools
/// FODs queue with no fetcher spawned). Harmless for builder pools:
/// FODs never reach builders (kind-mismatch in `hard_filter`), so
/// `queued_by_system["builtin"]` is always 0 for builder classes.
// r[impl ctrl.pool.per-system-class-depth]
// r[impl ctrl.fetcherpool.spawn-builtin]
pub(crate) fn class_queued_for_systems(
    class: &rio_proto::types::SizeClassStatus,
    systems: &[String],
) -> u64 {
    const BUILTIN: &str = "builtin";
    if class.queued_by_system.is_empty() || systems.is_empty() {
        return class.queued;
    }
    systems
        .iter()
        .map(String::as_str)
        .chain((!systems.iter().any(|s| s == BUILTIN)).then_some(BUILTIN))
        .map(|s| class.queued_by_system.get(s).copied().unwrap_or(0))
        .sum()
}

/// Queue depth relevant to a feature-gated ephemeral pool, given a
/// FEATURE-FILTERED `GetSizeClassStatus` response (the caller passed
/// `pool_features` so the scheduler already excluded derivations this
/// pool can't build).
///
/// I-176: when `features` is non-empty, sum [`class_queued_for_systems`]
/// across ALL classes — not just the pool's own `size_class`. Dispatch
/// overflow walks UP the class chain (`find_executor_with_overflow`),
/// so a `tiny`-classified kvm derivation routes to the `xlarge`-kvm
/// pool if that's the only kvm-capable pool. Reading only `xlarge`'s
/// count would leave `queued=0` → never spawn → derivation deadlocks
/// at `feature-missing` on the featureless tiny pool's spawn.
///
/// When `features` is empty: single-class lookup as before — a
/// featureless pool is one of N per-class pools, no cross-class
/// concern (overflow is handled by the LARGER featureless pool
/// spawning, which it does because its own class count is correct).
///
/// Returns `None` when `size_class` isn't in the response (scheduler
/// doesn't know that class) so the caller can fall through to
/// `cluster_status`. With non-empty `features` and a non-empty
/// response, returns the cross-class sum even if `size_class` itself
/// is absent — the feature filter makes every class entry relevant.
// r[impl ctrl.pool.per-feature-class-depth]
pub(crate) fn class_queued_for_pool(
    resp: &rio_proto::types::GetSizeClassStatusResponse,
    size_class: &str,
    systems: &[String],
    features: &[String],
) -> Option<u64> {
    if !features.is_empty() {
        // Cross-class sum. Empty response (size-classes off) → fall
        // through to cluster_status (None), same as the single-class
        // path's "class not found".
        if resp.classes.is_empty() {
            return None;
        }
        return Some(
            resp.classes
                .iter()
                .map(|c| class_queued_for_systems(c, systems))
                .sum(),
        );
    }
    resp.classes
        .iter()
        .find(|c| c.name == size_class)
        .map(|c| class_queued_for_systems(c, systems))
}

/// Compute desired replicas from queue metrics.
///
/// The formula: `ceil(queued / target)` gives how many workers
/// we'd need if each handles `target` queued items. Clamped to
/// `[min, max]`.
///
/// We use queued/target, not queued/active. Why: if active=0 (all
/// workers crashed), we still want to scale up based on queue;
/// queued/active would divide by zero.
///
/// Edge: `target=0` would divide-by-zero. CRD doesn't enforce
/// `>0` on target_value. We clamp target to 1 here — target=0 is
/// operator error ("scale up on ANY queue") which clamping to 1
/// approximates.
///
/// Edge: `queued=0` → desired=0 → clamped to min. Correct: empty
/// queue means scale DOWN to min, not to zero.
pub(crate) fn compute_desired(queued: u32, target: i32, min: i32, max: i32) -> i32 {
    // Defensive min>max swap. The CRD's CEL validation enforces
    // min ≤ max, but a CRD installed before the CEL rule existed
    // (or edited with `--validate=false`) could have min>max.
    // i32::clamp PANICS if min>max — the autoscaler would die
    // silently. Swap with a warn so operator notices.
    let (min, max) = if min > max {
        tracing::warn!(
            min,
            max,
            "compute_desired: min > max (pre-CEL CRD?); swapping"
        );
        (max, min)
    } else {
        (min, max)
    };

    let target = target.max(1) as u32;
    // ceil division. clippy prefers the std method over the
    // (a + b - 1) / b idiom — same semantics, clearer intent.
    let raw = queued.div_ceil(target);
    // i32 clamp. min/max from the CRD are i32 (K8s replica counts
    // are). Bound raw within i32 range BEFORE casting — raw as i32
    // would wrap negative when raw > 2^31 (queued near u32::MAX),
    // and a negative value would then clamp to `min` → autoscaler
    // scales DOWN under extreme load. Pathological but in u32 range.
    let raw = raw.min(i32::MAX as u32) as i32;
    raw.clamp(min, max)
}

/// Scaling decision.
pub(super) enum Decision {
    Patch(Direction),
    Wait(WaitReason),
}

#[derive(Clone, Copy)]
pub(super) enum Direction {
    Up,
    Down,
}

impl Direction {
    pub(super) fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

pub(super) enum WaitReason {
    /// desired == current. Nothing to do.
    NoChange,
    /// desired changed this tick — reset window.
    DesiredChanged,
    /// Window hasn't elapsed yet.
    Stabilizing,
    /// Too soon since last patch.
    AntiFlap,
}

impl WaitReason {
    pub(super) fn as_str(&self) -> &'static str {
        match self {
            Self::NoChange => "no_change",
            Self::DesiredChanged => "desired_changed",
            Self::Stabilizing => "stabilizing",
            Self::AntiFlap => "anti_flap",
        }
    }
}

/// Check stabilization windows. Updates `state` in place.
///
/// Separate fn for testability — all the timing logic is here,
/// no K8s or scheduler interaction.
pub(super) fn check_stabilization(
    state: &mut ScaleState,
    current: i32,
    desired: i32,
    timing: ScalingTiming,
) -> Decision {
    let now = Instant::now();

    // Desired changed → reset window. Even if it changed BACK
    // to what it was 2 ticks ago, reset: we want "stable for
    // N seconds" which means no changes in that window.
    if desired != state.last_desired {
        state.last_desired = desired;
        state.stable_since = now;
        return Decision::Wait(WaitReason::DesiredChanged);
    }

    // At target. No-op. (After the change check: if we just
    // reached target THIS tick, the change check fires; if we
    // were ALREADY at target, this fires.)
    if desired == current {
        return Decision::Wait(WaitReason::NoChange);
    }

    // Direction + window. Both tunable: VM tests shorten both
    // to observe a full up→down cycle; production uses 30s up
    // / 600s down (K8s HPA convention for downscale anti-flap).
    let (direction, window) = if desired > current {
        (Direction::Up, timing.scale_up_window)
    } else {
        (Direction::Down, timing.scale_down_window)
    };

    if now.duration_since(state.stable_since) < window {
        return Decision::Wait(WaitReason::Stabilizing);
    }

    // Anti-flap. Prevents oscillation when desired wobbles
    // across a boundary (queue at exactly target*N).
    if now.duration_since(state.last_patch) < timing.min_scale_interval {
        return Decision::Wait(WaitReason::AntiFlap);
    }

    Decision::Patch(direction)
}

pub(super) fn pool_key(pool: &BuilderPool) -> String {
    format!(
        "{}/{}",
        pool.namespace().unwrap_or_default(),
        pool.name_any()
    )
}

/// FetcherPool key. Prefixed with `fp:` so the autoscaler's `states`
/// HashMap can hold both BuilderPool and FetcherPool entries without
/// collision when an operator names them identically (e.g., both
/// `default` in their respective namespaces — `ns/default` would
/// collide; `fp:ns/default` doesn't).
pub(super) fn fp_pool_key(pool: &FetcherPool) -> String {
    format!(
        "fp:{}/{}",
        pool.namespace().unwrap_or_default(),
        pool.name_any()
    )
}

/// Is this BuilderPool a WPS child? Checks `ownerReferences` for
/// `kind=BuilderPoolSet` with `controller=true`. The WPS reconciler
/// sets this via `controller_owner_ref(&())` (see
/// `builderpoolset/builders.rs::build_child_builderpool`).
///
/// Used by `tick()` to skip WPS children in the standalone-pool
/// loop (those get per-class scaling via `scale_wps_class`
/// instead — two autoscalers on the same STS with different
/// signals would flap).
pub(crate) fn is_wps_owned(pool: &BuilderPool) -> bool {
    pool.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|or| or.kind == "BuilderPoolSet" && or.controller == Some(true))
}

/// Is this BuilderPool owned by a SPECIFIC BuilderPoolSet? Checks
/// `ownerReferences` for a controller entry whose UID matches
/// `wps.metadata.uid`. Stronger than `is_wps_owned` (which only
/// checks kind) — used by the prune path, where we must not prune
/// a DIFFERENT WPS's children that happen to share the namespace.
///
/// Returns false if `wps` has no UID (not from apiserver — should
/// not happen on a real reconcile; treated as "can't prove
/// ownership, don't prune").
pub(crate) fn is_wps_owned_by(pool: &BuilderPool, wps: &BuilderPoolSet) -> bool {
    let Some(wps_uid) = wps.metadata.uid.as_deref() else {
        return false;
    };
    pool.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|or| or.kind == "BuilderPoolSet" && or.controller == Some(true) && or.uid == wps_uid)
}

/// Result of looking up the WPS child pool for per-class scaling.
/// Captures the three distinct outcomes so `scale_wps_class` can
/// log at the right level (debug for not-yet-created, warn for a
/// name collision — operators should know about the latter).
#[derive(Debug)]
pub(crate) enum ChildLookup<'a> {
    /// Child found by name AND has a WPS controller ownerRef.
    /// Safe to scale per-class.
    Found(&'a BuilderPool),
    /// No pool matches `{wps}-{class}` in the WPS namespace.
    /// Reconciler hasn't created it yet — skip this tick.
    NotCreated,
    /// A pool matches the name but is NOT WPS-owned. This is a
    /// name collision: likely a manually-created standalone pool
    /// named `{wps}-{class}` before the WPS was stood up. Scaling
    /// it per-class would flap against the standalone loop (both
    /// autoscalers patching the same STS with different signals).
    NameCollision,
}

/// Find the WPS child pool to scale for a class. Enforces the
/// two-key symmetry (name-match AND `is_wps_owned_by`) that prevents
/// the asymmetric-key flap: the standalone loop skips by ownerRef,
/// so the per-class loop must require ownerRef after name-match.
///
/// UID-matching (`is_wps_owned_by`), not kind-only (`is_wps_owned`):
/// the prune path (builderpoolset/mod.rs:252) and cleanup
/// (builderpoolset/mod.rs:405) use UID-matching for the reason at
/// L349-357 — two WPS in the same namespace must not scale each
/// other's children. A kind-only check would return `Found` for
/// a different WPS's child with a colliding name.
///
/// Pure fn so the gate is unit-testable without constructing an
/// Autoscaler + mock apiserver. The test at
/// `scale_wps_class_skips_name_collision_without_ownerref` is the
/// flap-regression check — with only name-match, that test fails.
pub(crate) fn find_wps_child<'a>(
    wps: &BuilderPoolSet,
    class_name: &str,
    pools: &'a [BuilderPool],
) -> ChildLookup<'a> {
    let child_name =
        crate::reconcilers::builderpoolset::builders::child_name_str(&wps.name_any(), class_name);
    let wps_ns = wps.namespace().unwrap_or_default();
    match pools
        .iter()
        .find(|p| p.name_any() == child_name && p.namespace().as_deref() == Some(&wps_ns))
    {
        None => ChildLookup::NotCreated,
        // r[impl ctrl.wps.autoscale] — ownerRef gate after name-match.
        Some(child) if is_wps_owned_by(child, wps) => ChildLookup::Found(child),
        Some(_) => ChildLookup::NameCollision,
    }
}

#[cfg(test)]
mod tests;
