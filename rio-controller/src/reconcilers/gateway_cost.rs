//! Gateway `pod-deletion-cost` annotator.
//!
//! sh-028: KEDA scale-down 7→6 picked a gateway pod with an active SSH
//! session; after `terminationGracePeriodSeconds` (3660 s) SIGKILL
//! landed and the user saw `Nix daemon disconnected unexpectedly`. The
//! ReplicaSet controller's only per-pod-load input is the
//! `controller.kubernetes.io/pod-deletion-cost` annotation (it sorts
//! ascending and evicts the lowest). The gateway already maintains the
//! EXACT signal — the `rio_gateway_connections_active` gauge on its
//! `/metrics` endpoint counts post-auth-handshake SSH connections (TCP
//! probes excluded; same lifecycle as `GatewayServer::active_conns`).
//!
//! Security review: this lives in the **controller** (not the gateway)
//! so the gateway keeps `automountServiceAccountToken: false` and gains
//! no kube client or `pods` RBAC. The controller already holds
//! cluster-wide `[get, list, patch]` on `pods` (the `rio.build/hw-class`
//! annotator and PodSnapshot inventory) and can reach every gateway
//! pod's `:9090/metrics` via the existing CNP egress rule — no new
//! attack surface on the SSH-ingress component.
//!
//! Best-effort and additive: a scrape or PATCH failure for one pod
//! degrades scale-down ordering for THAT pod this tick, never a crash.
//! Freestanding `spawn_monitored` interval loop (no CR to watch — same
//! shape as [`super::gc_schedule`] and
//! [`super::node_informer::run_pod_annotator`]).

use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, Patch, PatchParams};
use kube::{Client, ResourceExt};
use rio_lease::POD_DELETION_COST_ANNOTATION;
use serde_json::json;
use tracing::{debug, info, warn};

/// Label selector for gateway pods. Matches the chart's
/// `rio.selectorLabels "rio-gateway"` (`_helpers.tpl`).
pub const GATEWAY_SELECTOR: &str = "app.kubernetes.io/name=rio-gateway";

/// Gateway metrics port. Compiled-in everywhere
/// (`rio_gateway::config::Config::default().common.metrics_addr` =
/// `[::]:9090`; helm `gateway.yaml` `containerPort: 9090`).
pub const GATEWAY_METRICS_PORT: u16 = 9090;

/// The gauge whose value becomes the deletion cost. No labels, so the
/// `/metrics` line is exactly `<NAME> <value>`.
pub const CONNECTIONS_ACTIVE_METRIC: &str = "rio_gateway_connections_active";

/// Poll cadence. KEDA's `pollingInterval` (chart default 30 s) bounds
/// how often a scale-down decision is made, and the ReplicaSet
/// controller reads the annotation at evict time, not continuously —
/// 10 s keeps the stamped value fresh relative to that cadence without
/// churning the apiserver on every connect/disconnect.
pub const POLL_INTERVAL: Duration = Duration::from_secs(10);

/// Per-pod scrape budget (HTTP GET `/metrics`). Generous — `/metrics`
/// is sub-ms when healthy. A pod that takes >2s is unhealthy enough
/// that its annotation stays stale this tick; the next tick retries.
const SCRAPE_TIMEOUT: Duration = Duration::from_secs(2);

/// Merge-patch body for the deletion-cost annotation. Pure so the JSON
/// shape (and the int32-clamp) is unit-testable without a mock
/// apiserver — sibling shape to `rio_lease::leader_marks_patch`.
///
/// The annotation value is a string (all k8s annotations are), parsed
/// as int32 by the ReplicaSet controller; invalid values sort as 0. A
/// gateway with >2³¹ connections has bigger problems than scale-down
/// ordering, so clamp.
// r[impl ctrl.gateway.deletion-cost]
pub fn deletion_cost_patch(n: u64) -> serde_json::Value {
    let cost = n.min(i32::MAX as u64).to_string();
    json!({
        "metadata": {
            "annotations": { POD_DELETION_COST_ANNOTATION: cost }
        }
    })
}

/// Parse the [`CONNECTIONS_ACTIVE_METRIC`] gauge from a Prometheus
/// text-format body. The gauge has no labels, so the production line
/// is exactly `rio_gateway_connections_active <value>` — a full
/// text-format parser would be overkill (and another dep). Tolerates
/// the float encoding `metrics-exporter-prometheus` emits for gauges
/// (`42` or `42.0`). `None` on absent/malformed — caller skips the
/// pod this tick.
pub fn parse_connections_active(body: &str) -> Option<u64> {
    for line in body.lines() {
        let Some(rest) = line.strip_prefix(CONNECTIONS_ACTIVE_METRIC) else {
            continue;
        };
        // Next byte must be whitespace — `<NAME>_foo` (a hypothetical
        // longer metric) must NOT match.
        let rest = rest.strip_prefix(' ').or_else(|| rest.strip_prefix('\t'))?;
        // Gauge values render as float; truncate (connections are
        // integral by construction).
        return rest.trim().parse::<f64>().ok().map(|f| f.max(0.0) as u64);
    }
    None
}

/// The annotator loop: every [`POLL_INTERVAL`], LIST gateway pods in
/// `namespace`, scrape each Running pod's `:9090/metrics`, and
/// merge-PATCH `pod-deletion-cost` when the scraped value differs from
/// the pod's current annotation. PATCH-on-change (not every tick)
/// keeps apiserver write rate proportional to connection churn, not
/// the poll cadence.
///
/// `spawn_monitored("gateway-cost-annotator", run(...))` from
/// `main.rs`, gated on `cfg.gateway_namespace` non-empty (helm sets it
/// from the downward-API `metadata.namespace`; non-k8s `cargo run`
/// leaves it empty → annotator not spawned).
pub async fn run(client: Client, namespace: String, shutdown: rio_common::signal::Token) {
    let pods: Api<Pod> = Api::namespaced(client, &namespace);
    // One pooled client for all scrapes. `timeout` here is the
    // per-request budget — reqwest applies it to connect+read, so no
    // separate `tokio::time::timeout` wrapper (and no timeout-census
    // entry) is needed.
    let http = reqwest::Client::builder()
        .timeout(SCRAPE_TIMEOUT)
        .build()
        .expect("reqwest client build (no TLS, default resolver)");
    info!(
        %namespace, selector = GATEWAY_SELECTOR, interval = ?POLL_INTERVAL,
        "gateway deletion-cost annotator started \
         (pod-deletion-cost = scraped rio_gateway_connections_active)"
    );
    let mut tick = tokio::time::interval(POLL_INTERVAL);
    loop {
        tokio::select! {
            _ = shutdown.cancelled() => return,
            _ = tick.tick() => {}
        }
        let listed = match pods
            .list(&ListParams::default().labels(GATEWAY_SELECTOR))
            .await
        {
            Ok(l) => l,
            Err(e) => {
                warn!(error = %e, "gateway-cost: pod LIST failed; retrying next tick");
                continue;
            }
        };
        for pod in listed {
            annotate_one(&pods, &http, &pod).await;
        }
    }
}

/// Scrape + conditionally PATCH one pod. Split for readability and so
/// the per-pod failure surface is one `debug!`/`warn!` site, never a
/// `?` that aborts the tick's remaining pods.
async fn annotate_one(pods: &Api<Pod>, http: &reqwest::Client, pod: &Pod) {
    let name = pod.name_any();
    // Running pods with an assigned podIP only — Pending/Succeeded/
    // Failed have nothing to scrape, and a freshly-scheduled pod with
    // no IP is the BEST eviction candidate (cost 0 by absence) anyway.
    let Some(ip) = pod
        .status
        .as_ref()
        .filter(|s| s.phase.as_deref() == Some("Running"))
        .and_then(|s| s.pod_ip.as_deref())
    else {
        return;
    };
    let current = pod
        .annotations()
        .get(POD_DELETION_COST_ANNOTATION)
        .map(String::as_str);
    let url = format!("http://{ip}:{GATEWAY_METRICS_PORT}/metrics");
    let body = match http
        .get(&url)
        .send()
        .await
        .and_then(|r| r.error_for_status())
    {
        Ok(r) => match r.text().await {
            Ok(t) => t,
            Err(e) => {
                debug!(%name, %url, error = %e, "gateway-cost: scrape body read failed");
                return;
            }
        },
        Err(e) => {
            // Connection refused during cold-start / draining is
            // expected churn — debug, not warn.
            debug!(%name, %url, error = %e, "gateway-cost: scrape failed");
            return;
        }
    };
    let Some(n) = parse_connections_active(&body) else {
        warn!(
            %name, metric = CONNECTIONS_ACTIVE_METRIC,
            "gateway-cost: metric absent from /metrics — gauge unregistered? \
             (sh-028 sub-finding); deletion-cost not stamped this tick"
        );
        return;
    };
    let desired = n.min(i32::MAX as u64).to_string();
    if current == Some(desired.as_str()) {
        return;
    }
    let patch = deletion_cost_patch(n);
    if let Err(e) = pods
        .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
        .await
    {
        warn!(%name, error = %e, "gateway-cost: pod-deletion-cost PATCH failed");
    } else {
        debug!(%name, cost = n, "gateway-cost: patched pod-deletion-cost");
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    // r[verify ctrl.gateway.deletion-cost]
    /// sh-028 RED-first: the annotation body is the connection count,
    /// stringified (k8s annotations are strings; the ReplicaSet
    /// controller parses as int32). RED at base: `deletion_cost_patch`
    /// did not exist — no per-pod-load signal reached the ReplicaSet
    /// controller, so KEDA scale-down picked a busy gateway replica.
    #[test]
    fn deletion_cost_patch_body_is_int_string() {
        assert_eq!(
            deletion_cost_patch(42)["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
            json!("42"),
            "annotation value is the connection count as a decimal string"
        );
        // int32 clamp: overflow would parse as invalid → sort as 0
        // (= "evict me first"), the OPPOSITE of what >2B connections
        // should mean.
        assert_eq!(
            deletion_cost_patch(u64::MAX)["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
            json!(i32::MAX.to_string()),
        );
        // 0 → "0", not absent — an idle pod IS the best candidate.
        // Absence sorts as 0 anyway, but an explicit "0" makes
        // `kubectl get pod -o yaml` show the annotator ran.
        assert_eq!(
            deletion_cost_patch(0)["metadata"]["annotations"][POD_DELETION_COST_ANNOTATION],
            json!("0"),
        );
    }

    /// The text-format parse is line-exact: name match must be whole-
    /// token (a hypothetical `…_active_peak` must NOT match), HELP/
    /// TYPE comments are skipped, and the float encoding the
    /// prometheus exporter emits for gauges is tolerated.
    #[test]
    fn parse_connections_active_is_line_exact() {
        let body = "\
# HELP rio_gateway_connections_active Authenticated SSH connections currently open.\n\
# TYPE rio_gateway_connections_active gauge\n\
rio_gateway_connections_total{result=\"new\"} 99\n\
rio_gateway_connections_active 7\n\
rio_gateway_channels_active 12\n";
        assert_eq!(parse_connections_active(body), Some(7));
        // float encoding (metrics-exporter-prometheus renders gauges
        // as f64).
        assert_eq!(
            parse_connections_active("rio_gateway_connections_active 42.0\n"),
            Some(42)
        );
        // Whole-token: a longer name with the same prefix must NOT
        // match — the strip_prefix(' ') gate.
        assert_eq!(
            parse_connections_active("rio_gateway_connections_active_peak 99\n"),
            None
        );
        // Absent → None (caller skips the pod this tick).
        assert_eq!(parse_connections_active("rio_other_gauge 1\n"), None);
    }
}
