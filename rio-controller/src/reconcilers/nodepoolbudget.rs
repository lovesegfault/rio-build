//! Shared vCPU budget across Karpenter NodePools.
//!
//! Karpenter's `NodePool.spec.limits` is per-pool; there is no native
//! cross-pool limit (kubernetes-sigs/karpenter#1747). This reconciler
//! gives a label-selected set of NodePools a shared budget by patching
//! each `spec.limits.cpu = used + headroom`, where `headroom = budget −
//! Σused`. Every governed pool sees the same headroom, so any pool can
//! absorb a burst up to the aggregate budget.
//!
//! Freeze-on-exhaustion: when `Σused ≥ budget`, headroom is 0 and each
//! pool's limit equals its current usage. Karpenter stops provisioning;
//! we never set `limit < used` (no forced scale-down → no mid-build
//! node reclaim). Overshoot is bounded by one Karpenter provisioning
//! batch before the next tick clamps.
//!
//! Like [`super::gc_schedule`] this is a periodic task, not a
//! `Controller::run` — NodePool is a foreign CRD we don't own.

use std::collections::BTreeMap;
use std::time::Duration;

use kube::api::{Api, DynamicObject, ListParams, Patch, PatchParams};
use kube::core::{ApiResource, GroupVersionKind};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, warn};

/// Reconcile interval. Karpenter updates `status.resources` within
/// seconds of node Ready; 30s keeps overshoot bounded without
/// hammering the apiserver (≤ a handful of NodePools per tick).
const TICK: Duration = Duration::from_secs(30);

/// fieldManager for the merge patch. Distinct from helm's manager so
/// `kubectl get nodepool -o yaml --show-managed-fields` shows who
/// last set `spec.limits.cpu`.
const FIELD_MANAGER: &str = "rio-controller-budget";

/// Figment-loaded config (`RIO_NODEPOOL_BUDGET__*`). `cpu_millicores
/// = 0` → reconciler not spawned (see gate in main.rs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodePoolBudgetConfig {
    /// Shared budget in millicores. 0 = disabled.
    pub cpu_millicores: u64,
    /// Label selector for governed NodePools. Ungoverned pools
    /// (e.g., rio-general, control-plane) are untouched.
    pub selector: String,
}

impl Default for NodePoolBudgetConfig {
    fn default() -> Self {
        Self {
            cpu_millicores: 0,
            selector: "rio.build/karpenter-budget=shared".into(),
        }
    }
}

// r[impl ctrl.nodepoolbudget]
/// Main loop. `main.rs` spawns via `spawn_monitored("nodepool-budget",
/// ...)` when `cfg.cpu_millicores > 0`. Returns on shutdown.
pub async fn run(
    client: kube::Client,
    cfg: NodePoolBudgetConfig,
    shutdown: rio_common::signal::Token,
) {
    info!(
        budget_millicores = cfg.cpu_millicores,
        selector = %cfg.selector,
        "NodePool budget reconciler starting"
    );
    let ar = ApiResource::from_gvk(&GroupVersionKind::gvk("karpenter.sh", "v1", "NodePool"));
    let api: Api<DynamicObject> = Api::all_with(client, &ar);

    let mut interval = tokio::time::interval(TICK);
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }
        if let Err(e) = tick_once(&api, &cfg).await {
            warn!(error = %e, "NodePool budget tick failed");
        }
    }
    info!("NodePool budget reconciler stopped");
}

/// One tick: list, compute, patch-if-changed.
async fn tick_once(api: &Api<DynamicObject>, cfg: &NodePoolBudgetConfig) -> anyhow::Result<()> {
    let pools = api
        .list(&ListParams::default().labels(&cfg.selector))
        .await?;

    let mut used = BTreeMap::new();
    let mut current_limit = BTreeMap::new();
    for pool in &pools.items {
        let Some(name) = pool.metadata.name.clone() else {
            continue;
        };
        let u = pool
            .data
            .get("status")
            .and_then(|s| s.get("resources"))
            .and_then(|r| r.get("cpu"))
            .and_then(extract_cpu_millis)
            .unwrap_or(0);
        let lim = pool
            .data
            .get("spec")
            .and_then(|s| s.get("limits"))
            .and_then(|l| l.get("cpu"))
            .and_then(extract_cpu_millis);
        used.insert(name.clone(), u);
        current_limit.insert(name, lim);
    }

    let total_used: u64 = used.values().sum();
    let headroom = cfg.cpu_millicores.saturating_sub(total_used);
    metrics::gauge!("rio_controller_nodepool_budget_used_millicores").set(total_used as f64);
    metrics::gauge!("rio_controller_nodepool_budget_headroom_millicores").set(headroom as f64);

    let new_limits = compute_limits(&used, cfg.cpu_millicores);
    let pp = PatchParams {
        field_manager: Some(FIELD_MANAGER.into()),
        ..Default::default()
    };
    let mut patched = 0u32;
    for (name, new) in &new_limits {
        if current_limit.get(name).copied().flatten() == Some(*new) {
            continue;
        }
        let body = serde_json::json!({ "spec": { "limits": { "cpu": format!("{new}m") } } });
        if let Err(e) = api.patch(name, &pp, &Patch::Merge(&body)).await {
            warn!(pool = %name, error = %e, "NodePool budget patch failed");
            continue;
        }
        debug!(pool = %name, new_limit_millicores = new, "patched NodePool spec.limits.cpu");
        patched += 1;
    }

    info!(
        pools = used.len(),
        total_used,
        budget = cfg.cpu_millicores,
        headroom,
        patched,
        "NodePool budget reconciled"
    );
    Ok(())
}

/// `pool.limit = used + (budget − Σused)`. Freeze: `saturating_sub`
/// makes headroom 0 when over budget, so `limit == used` (Karpenter
/// can't grow, won't shrink).
pub(crate) fn compute_limits(used: &BTreeMap<String, u64>, budget: u64) -> BTreeMap<String, u64> {
    let total: u64 = used.values().sum();
    let headroom = budget.saturating_sub(total);
    used.iter()
        .map(|(k, &v)| (k.clone(), v + headroom))
        .collect()
}

/// Read a Quantity-ish JSON value as millicores. Karpenter writes
/// `status.resources.cpu` as a Quantity string; helm-rendered
/// `spec.limits.cpu` may be a bare int (cores) depending on YAML
/// quoting.
fn extract_cpu_millis(v: &serde_json::Value) -> Option<u64> {
    if let Some(s) = v.as_str() {
        return Some(parse_cpu_millis(s));
    }
    if let Some(n) = v.as_u64() {
        return Some(n * 1000);
    }
    v.as_f64().map(|f| (f * 1000.0).round() as u64)
}

/// `"64"` → 64000, `"64000m"` → 64000, `"1.5"` → 1500. Malformed → 0.
/// Only the `m` (milli) suffix is handled — Karpenter never emits
/// binary-SI (`Ki`/`Mi`) for CPU.
pub(crate) fn parse_cpu_millis(q: &str) -> u64 {
    if let Some(m) = q.strip_suffix('m') {
        return m.parse().unwrap_or(0);
    }
    q.parse::<f64>()
        .map(|c| (c * 1000.0).round() as u64)
        .unwrap_or(0)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_cpu_millis_forms() {
        assert_eq!(parse_cpu_millis("64"), 64_000);
        assert_eq!(parse_cpu_millis("136000m"), 136_000);
        assert_eq!(parse_cpu_millis("1.5"), 1_500);
        assert_eq!(parse_cpu_millis("0"), 0);
        assert_eq!(parse_cpu_millis("0m"), 0);
        assert_eq!(parse_cpu_millis("garbage"), 0);
        assert_eq!(parse_cpu_millis(""), 0);
    }

    #[test]
    fn extract_handles_json_types() {
        use serde_json::json;
        assert_eq!(extract_cpu_millis(&json!("512")), Some(512_000));
        assert_eq!(extract_cpu_millis(&json!("512000m")), Some(512_000));
        assert_eq!(extract_cpu_millis(&json!(512)), Some(512_000));
        assert_eq!(extract_cpu_millis(&json!(1.5)), Some(1_500));
        assert_eq!(extract_cpu_millis(&json!(null)), None);
        assert_eq!(extract_cpu_millis(&json!({})), None);
    }

    // r[verify ctrl.nodepoolbudget]
    /// Under budget: every pool gets `used + headroom`. The headroom is
    /// shared, so each pool's limit minus its used equals the same value.
    #[test]
    fn compute_limits_shared_headroom() {
        let used = BTreeMap::from([
            ("a".into(), 10_000_000u64), // 10000 cores
            ("b".into(), 5_000_000),
        ]);
        let out = compute_limits(&used, 30_000_000);
        // headroom = 30M − 15M = 15M
        assert_eq!(out["a"], 25_000_000);
        assert_eq!(out["b"], 20_000_000);
        assert_eq!(
            out["a"] - used["a"],
            out["b"] - used["b"],
            "headroom shared"
        );
    }

    /// Over budget → freeze: each limit equals its current usage,
    /// never less (no forced scale-down).
    #[test]
    fn compute_limits_freeze_on_exhaustion() {
        let used = BTreeMap::from([("a".into(), 20_000_000u64), ("b".into(), 15_000_000)]);
        let out = compute_limits(&used, 30_000_000);
        assert_eq!(out["a"], 20_000_000, "freeze at used");
        assert_eq!(out["b"], 15_000_000, "freeze at used");
        for (k, &lim) in &out {
            assert!(lim >= used[k], "never below used");
        }
    }

    /// A pool with no nodes yet (used=0) still gets the full headroom.
    #[test]
    fn compute_limits_empty_pool() {
        let used = BTreeMap::from([("warm".into(), 1_000_000u64), ("cold".into(), 0)]);
        let out = compute_limits(&used, 30_000_000);
        assert_eq!(out["cold"], 29_000_000);
        assert_eq!(out["warm"], 30_000_000);
    }

    #[test]
    fn config_default_disabled() {
        let d = NodePoolBudgetConfig::default();
        assert_eq!(d.cpu_millicores, 0, "0 = disabled gate");
        assert_eq!(d.selector, "rio.build/karpenter-budget=shared");
    }
}
