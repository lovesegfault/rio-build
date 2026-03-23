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

pub mod per_class;
pub mod standalone;

// Back-compat: `main.rs` constructs `scaling::Autoscaler` and
// `scaling::ScalingTiming`; the re-export keeps those paths
// stable so the split is transparent to callers.
pub use standalone::Autoscaler;

use std::time::{Duration, Instant};

use kube::ResourceExt;

use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::WorkerPoolSet;

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
/// to surface via `WorkerPool.status.conditions`. Transient
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

/// SSA field-manager for autoscaler's WorkerPool.status patches.
/// DIFFERENT from the reconciler's "rio-controller" — SSA splits
/// field ownership so the two don't clobber each other.
pub(super) const STATUS_MANAGER: &str = "rio-controller-autoscaler-status";

/// SSA field-manager for per-class (WPS child) StatefulSet
/// replica patches. Distinct from `rio-controller-autoscaler`
/// (standalone WorkerPool scaling) AND from `rio-controller-wps`
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
pub(super) fn scaling_condition(status: &str, reason: &str, message: &str) -> serde_json::Value {
    // k8s_openapi re-exports jiff (kube 3.0's chrono replacement).
    // Timestamp::now() → Display is RFC3339 with offset (UTC Z).
    // K8s Condition.lastTransitionTime expects this format.
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    serde_json::json!({
        "type": "Scaling",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": now,
    })
}

/// Build the SSA patch body for `WorkerPool.status.{lastScaleTime,
/// conditions}`. Partial status — replicas/ready/desired are the
/// reconciler's fields, not ours.
///
/// Same apiVersion+kind requirement as sts_replicas_patch (and
/// the reconciler's status_patch).
pub(crate) fn wp_status_patch(conditions: &[serde_json::Value]) -> serde_json::Value {
    use kube::CustomResourceExt;
    let ar = WorkerPool::api_resource();
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
/// `>0` (the CEL is on max_concurrent_builds, not target_value).
/// We clamp target to 1 here — target=0 is operator error ("scale
/// up on ANY queue") which clamping to 1 approximates.
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

pub(super) fn pool_key(pool: &WorkerPool) -> String {
    format!(
        "{}/{}",
        pool.namespace().unwrap_or_default(),
        pool.name_any()
    )
}

/// Is this WorkerPool a WPS child? Checks `ownerReferences` for
/// `kind=WorkerPoolSet` with `controller=true`. The WPS reconciler
/// sets this via `controller_owner_ref(&())` (see
/// `workerpoolset/builders.rs::build_child_workerpool`).
///
/// Used by `tick()` to skip WPS children in the standalone-pool
/// loop (those get per-class scaling via `scale_wps_class`
/// instead — two autoscalers on the same STS with different
/// signals would flap).
pub(crate) fn is_wps_owned(pool: &WorkerPool) -> bool {
    pool.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|or| or.kind == "WorkerPoolSet" && or.controller == Some(true))
}

/// Is this WorkerPool owned by a SPECIFIC WorkerPoolSet? Checks
/// `ownerReferences` for a controller entry whose UID matches
/// `wps.metadata.uid`. Stronger than `is_wps_owned` (which only
/// checks kind) — used by the prune path, where we must not prune
/// a DIFFERENT WPS's children that happen to share the namespace.
///
/// Returns false if `wps` has no UID (not from apiserver — should
/// not happen on a real reconcile; treated as "can't prove
/// ownership, don't prune").
pub(crate) fn is_wps_owned_by(pool: &WorkerPool, wps: &WorkerPoolSet) -> bool {
    let Some(wps_uid) = wps.metadata.uid.as_deref() else {
        return false;
    };
    pool.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|or| or.kind == "WorkerPoolSet" && or.controller == Some(true) && or.uid == wps_uid)
}

/// Result of looking up the WPS child pool for per-class scaling.
/// Captures the three distinct outcomes so `scale_wps_class` can
/// log at the right level (debug for not-yet-created, warn for a
/// name collision — operators should know about the latter).
#[derive(Debug)]
pub(crate) enum ChildLookup<'a> {
    /// Child found by name AND has a WPS controller ownerRef.
    /// Safe to scale per-class.
    Found(&'a WorkerPool),
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
/// two-key symmetry (name-match AND `is_wps_owned`) that prevents
/// the asymmetric-key flap: the standalone loop skips by ownerRef,
/// so the per-class loop must require ownerRef after name-match.
///
/// Pure fn so the gate is unit-testable without constructing an
/// Autoscaler + mock apiserver. The test at
/// `scale_wps_class_skips_name_collision_without_ownerref` is the
/// flap-regression check — with only name-match, that test fails.
pub(crate) fn find_wps_child<'a>(
    wps: &WorkerPoolSet,
    class_name: &str,
    pools: &'a [WorkerPool],
) -> ChildLookup<'a> {
    let child_name =
        crate::reconcilers::workerpoolset::builders::child_name_str(&wps.name_any(), class_name);
    let wps_ns = wps.namespace().unwrap_or_default();
    match pools
        .iter()
        .find(|p| p.name_any() == child_name && p.namespace().as_deref() == Some(&wps_ns))
    {
        None => ChildLookup::NotCreated,
        // r[impl ctrl.wps.autoscale] — ownerRef gate after name-match.
        Some(child) if is_wps_owned(child) => ChildLookup::Found(child),
        Some(_) => ChildLookup::NameCollision,
    }
}

#[cfg(test)]
mod tests;
