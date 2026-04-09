//! K8s `Condition` helpers shared by the Job-mode reconcilers.
//!
//! Both the builder and fetcher pool reconcilers write a
//! `SchedulerUnreachable` condition every tick; preserving
//! `lastTransitionTime` across non-transitions is the K8s
//! convention these helpers implement. Lived in `scaling/` until
//! the cleanup pass — nothing here is about scaling.

use rio_crds::common::PoolStatusCommon;

/// Compute `lastTransitionTime` per K8s convention: preserve the
/// existing timestamp if `status` is unchanged, stamp now() on
/// an actual transition (or first write).
///
/// Without this, a reconciler that writes the same condition
/// every tick (the 10s `JOB_REQUEUE`) makes `lastTransitionTime`
/// always read "~10s ago" — useless for "when did the scheduler
/// become unreachable."
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

/// Find a condition by `type` in a pool's `status.conditions` array.
/// Used to read the existing condition before a rewrite so
/// `lastTransitionTime` can be preserved on non-transitions.
///
/// Generic over the embedding status struct via `AsRef` so both
/// `BuilderPoolStatus` and `FetcherPoolStatus` (which flatten
/// [`PoolStatusCommon`]) feed into one function — previously two
/// near-identical 6-line copies.
///
/// Returns `None` if the pool has no status, no conditions, or no
/// condition of the given type. Serializes via serde_json (the
/// k8s_openapi Condition struct → json::Value) so the output
/// plugs directly into `transition_time`.
pub(crate) fn find_condition<S: AsRef<PoolStatusCommon>>(
    status: Option<&S>,
    cond_type: &str,
) -> Option<serde_json::Value> {
    status?
        .as_ref()
        .conditions
        .iter()
        .find(|c| c.type_ == cond_type)
        .and_then(|c| serde_json::to_value(c).ok())
}
