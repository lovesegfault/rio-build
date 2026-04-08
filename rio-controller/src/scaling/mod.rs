//! Scaling helpers — queue-depth lookups, condition utilities,
//! BPS ownership predicates, and the [`component`] scaler.
//!
//! Shared surface the Job reconcilers and the ComponentScaler
//! both consume.

pub mod component;

use crate::crds::builderpool::BuilderPool;
use crate::crds::builderpoolset::BuilderPoolSet;
use crate::crds::fetcherpool::FetcherPool;

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
/// plugs directly into `transition_time`.
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

/// Is this BuilderPool owned by a SPECIFIC BuilderPoolSet? Checks
/// `ownerReferences` for a controller entry whose UID matches
/// `wps.metadata.uid`. Used by the prune path, where we must not
/// prune a DIFFERENT WPS's children that happen to share the
/// namespace.
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

#[cfg(test)]
mod tests;
