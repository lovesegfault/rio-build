//! Node-label cache for `hw_class` join at completion-ingest.
//!
//! ADR-023 §Hardware heterogeneity: builders are air-gapped from the
//! apiserver, so they report `spec.nodeName` (downward API) and the
//! controller joins to Node labels server-side when ingesting build
//! completion samples. `hw_class = "{manufacturer}-{generation}-
//! {storage}"` from Karpenter-provisioned + rio.build labels.
//!
//! Node-gone-at-ingest → [`NodeLabelCache::hw_class_of`] returns
//! `None` and the sample's `hw_class` stays NULL. This is expected
//! for builds that finish after their node has been reaped (rare —
//! ephemeral builders typically outlive their single build).
//!
//! # Why a watcher, not per-ingest `Api::get`
//!
//! Completion-ingest is on the hot path (one per build). A cached
//! lookup is O(1) in-process; a per-ingest `GET /api/v1/nodes/{name}`
//! is an apiserver round-trip per build. The watch is one long-lived
//! connection that incrementally updates the cache.
//!
//! # Why not `reflector::store()`
//!
//! The reflector store caches full `Node` objects (status, conditions,
//! images list — kilobytes each). We need three label strings. A
//! hand-rolled `HashMap<String, HwClass>` is the lightweight version.

use std::collections::HashMap;
use std::sync::Arc;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use rio_proto::AdminServiceClient;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

/// Pod annotation the [`run_pod_annotator`] watcher stamps with the
/// node's [`HwClass::as_string`]. Builder reads it via downward-API
/// (`RIO_HW_CLASS`) to key its `hw_perf_samples` insert.
pub const ANNOT_HW_CLASS: &str = "rio.build/hw-class";

/// Karpenter-set: `aws`, `intel`, `amd`. Unknown if label absent
/// (non-Karpenter node, or Karpenter < v0.33).
const LABEL_MANUFACTURER: &str = "karpenter.k8s.aws/instance-cpu-manufacturer";
/// Karpenter-set: instance generation number as string (`"7"` for
/// c7/m7/r7, etc.).
const LABEL_GENERATION: &str = "karpenter.k8s.aws/instance-generation";
/// rio-set via EC2NodeClass `spec.tags` → Karpenter propagates to
/// Node labels. `ebs` (gp3 root) or `nvme` (instance-store).
const LABEL_STORAGE: &str = "rio.build/storage";

/// `karpenter.sh/capacity-type` — `spot` or `on-demand`. Cached so the
/// spot-interrupt watcher can skip on-demand nodes without re-fetching.
const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

/// Three labels that determine build-performance equivalence class.
/// Formatted as `"{manufacturer}-{generation}-{storage}"` for the
/// completion-sample `hw_class` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwClass {
    pub manufacturer: String,
    pub generation: String,
    pub storage: String,
}

/// Per-node cache value: `HwClass` + the bits the spot-interrupt
/// watcher needs (capacity type, creation epoch for node-seconds).
#[derive(Debug, Clone)]
struct NodeMeta {
    hw: HwClass,
    capacity_type: Option<String>,
    /// Unix-epoch seconds from `metadata.creationTimestamp`. `None` for
    /// nodes with no timestamp (shouldn't happen post-Create).
    created_at: Option<f64>,
}

impl HwClass {
    /// `"{manufacturer}-{generation}-{storage}"`, e.g. `"aws-7-ebs"`.
    pub fn as_string(&self) -> String {
        format!("{}-{}-{}", self.manufacturer, self.generation, self.storage)
    }

    /// Extract from a Node's labels. Missing labels default to
    /// `"unknown"` (manufacturer/generation) or `"ebs"` (storage —
    /// the conservative default; nvme is opt-in via EC2NodeClass).
    fn from_node(node: &Node) -> Self {
        let labels = node.metadata.labels.as_ref();
        let get = |k: &str| labels.and_then(|l| l.get(k)).cloned();
        Self {
            manufacturer: get(LABEL_MANUFACTURER).unwrap_or_else(|| "unknown".into()),
            generation: get(LABEL_GENERATION).unwrap_or_else(|| "unknown".into()),
            storage: get(LABEL_STORAGE).unwrap_or_else(|| "ebs".into()),
        }
    }
}

/// Node-name → `HwClass` cache. Cheap to clone (`Arc`); share one
/// instance between the informer task and consumers.
#[derive(Clone, Default)]
pub struct NodeLabelCache(Arc<RwLock<HashMap<String, NodeMeta>>>);

impl NodeLabelCache {
    /// Formatted `hw_class` string for `node_name`, or `None` if the
    /// node is not (or no longer) in the cache.
    pub fn hw_class_of(&self, node_name: &str) -> Option<String> {
        self.0.read().get(node_name).map(|m| m.hw.as_string())
    }

    /// `Some((hw_class, node_seconds))` for `node_name` IF it's a spot
    /// node with a creation timestamp. Feeds the exposure half of λ\[h\]
    /// — on-demand nodes contribute neither numerator nor denominator
    /// (their λ is 0 by definition).
    pub fn spot_exposure(&self, node_name: &str, now_epoch: f64) -> Option<(String, f64)> {
        let g = self.0.read();
        let m = g.get(node_name)?;
        if m.capacity_type.as_deref() != Some("spot") {
            return None;
        }
        let secs = (now_epoch - m.created_at?).max(0.0);
        Some((m.hw.as_string(), secs))
    }

    /// Current cache size. For the `rio_controller_node_cache_size`
    /// gauge and tests.
    pub fn len(&self) -> usize {
        self.0.read().len()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.0.read().is_empty()
    }

    fn apply(&self, node: &Node) {
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let labels = node.metadata.labels.as_ref();
        let meta = NodeMeta {
            hw: HwClass::from_node(node),
            capacity_type: labels.and_then(|l| l.get(LABEL_CAPACITY_TYPE)).cloned(),
            created_at: node
                .metadata
                .creation_timestamp
                .as_ref()
                .map(|t| t.0.as_second() as f64),
        };
        self.0.write().insert(name, meta);
    }

    fn delete(&self, node: &Node) -> Option<NodeMeta> {
        node.metadata
            .name
            .as_ref()
            .and_then(|n| self.0.write().remove(n))
    }
}

/// Run the Node informer. Returns on `shutdown.cancelled()` or if
/// the watch stream ends (shouldn't — `default_backoff()` retries
/// indefinitely).
///
/// `spawn_monitored("node-informer", run(...))` from main.rs. Panics
/// are logged; the controller keeps reconciling. Consumers see an
/// empty/stale cache → `hw_class_of` returns `None` → samples ingest
/// with NULL `hw_class` (degraded, not broken).
pub async fn run(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminServiceClient<Channel>,
    shutdown: rio_common::signal::Token,
) {
    let nodes: Api<Node> = Api::all(client);

    // Raw event stream (not `.applied_objects()`): we need Delete to
    // evict cache entries. `default_backoff()`: exponential retry on
    // watch connection loss (apiserver restart, network blip) — same
    // as disruption.rs.
    let mut stream = watcher(nodes, watcher::Config::default())
        .default_backoff()
        .boxed();

    info!("Node informer started");

    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("Node informer: shutdown");
                return;
            }
            next = stream.next() => match next {
                Some(Ok(ev)) => ev,
                Some(Err(e)) => {
                    warn!(error = %e, "Node informer: stream error");
                    continue;
                }
                None => {
                    warn!("Node informer: stream ended (unexpected)");
                    return;
                }
            },
        };

        // Apply/InitApply → upsert; Delete → remove. Init/InitDone
        // are relist bookends — we don't buffer-and-swap (a node
        // deleted during a watch-gap lingers until process restart;
        // Karpenter node names are IP-derived and not reused, so the
        // worst case is a bounded stale-entry leak, not a wrong join).
        match event {
            watcher::Event::Apply(node) | watcher::Event::InitApply(node) => {
                cache.apply(&node);
            }
            watcher::Event::Delete(node) => {
                // ADR-023 phase-13: spot-node lifetime → λ denominator.
                // Compute BEFORE delete (cache still has created_at).
                if let Some(name) = &node.metadata.name
                    && let Some((hw, secs)) = cache.spot_exposure(name, now_epoch())
                {
                    report_exposure(&mut admin, hw, secs).await;
                }
                cache.delete(&node);
            }
            watcher::Event::Init | watcher::Event::InitDone => {}
        }
    }
}

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Builder/fetcher pod label selector — same value `disruption.rs`
/// filters on. Re-declared here (not re-exported) to keep this module
/// dependency-free of `builderpool`.
const POOL_LABEL: &str = "rio.build/pool";

/// Pod-watcher: stamp `rio.build/hw-class` on each builder pod once
/// `spec.nodeName` resolves.
///
/// ADR-023 phase-10: builders are air-gapped from the apiserver, so
/// they can't read Node labels themselves. The builder reads the
/// stamped annotation via downward-API (`RIO_HW_CLASS`) to key its
/// `hw_perf_samples` microbench insert. Stamps once (skip if the
/// annotation already exists) — pod annotations are sticky and the
/// hw_class can't change for a scheduled pod.
///
/// `spawn_monitored("hw-class-annotator", run_pod_annotator(...))`
/// from main.rs. Same degraded-not-broken contract as [`run`]: if the
/// watcher dies, builders skip the bench (`RIO_HW_CLASS` empty) and
/// the hw_class stays at `factor=1.0` until ≥3 pods report.
///
/// TODO: gate the stamp on `hw_class NOT IN (SELECT hw_class FROM
/// hw_perf_factors)` so well-sampled classes skip the ~5s bench. Needs
/// a PG handle here (controller is currently apiserver-only); deferred
/// — the bench is cheap and runs concurrent with cold-start anyway.
pub async fn run_pod_annotator(
    client: Client,
    cache: NodeLabelCache,
    shutdown: rio_common::signal::Token,
) {
    let pods: Api<Pod> = Api::all(client.clone());
    let cfg = watcher::Config::default().labels(POOL_LABEL);
    let mut stream = watcher(pods, cfg)
        .default_backoff()
        .applied_objects()
        .boxed();

    info!("hw-class pod annotator started (label={POOL_LABEL})");

    loop {
        let pod = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("hw-class annotator: shutdown");
                return;
            }
            next = stream.next() => match next {
                Some(Ok(p)) => p,
                Some(Err(e)) => { warn!(error = %e, "hw-class annotator: stream error"); continue; }
                None => { warn!("hw-class annotator: stream ended (unexpected)"); return; }
            },
        };
        let Some((name, ns, hw)) = hw_class_patch_target(&pod, &cache) else {
            continue;
        };
        let pods_ns: Api<Pod> = Api::namespaced(client.clone(), &ns);
        let patch = serde_json::json!({
            "metadata": { "annotations": { ANNOT_HW_CLASS: hw } }
        });
        if let Err(e) = pods_ns
            .patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            warn!(%name, %ns, error = %e, "hw-class annotator: patch failed");
        }
    }
}

/// ADR-023 phase-13 λ\[h\] self-calibration: watch `core/v1.Event` for
/// `reason=SpotInterrupted` (Karpenter posts one on the NodeClaim when
/// AWS sends the 2-minute spot interruption notice), resolve the
/// referenced node's hw_class via [`NodeLabelCache`], and append
/// `interrupt_samples(hw_class, kind='interrupt', value=1)` via
/// `AdminService.AppendInterruptSample`.
///
/// The exposure half (`kind='exposure', value=node_seconds`) is
/// emitted from [`run`]'s Node-DELETE arm — every spot-node teardown
/// (interrupted or not) contributes its lifetime to the denominator.
///
/// `field_selector` narrows the watch server-side so we don't churn
/// on the cluster's full event firehose. Karpenter's event is on the
/// NodeClaim object; `involvedObject.kind=NodeClaim` + `reason=
/// SpotInterrupted` is the tightest filter the apiserver supports.
pub async fn run_spot_interrupt_watcher(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminServiceClient<Channel>,
    shutdown: rio_common::signal::Token,
) {
    let events: Api<Event> = Api::all(client);
    let cfg =
        watcher::Config::default().fields("reason=SpotInterrupted,involvedObject.kind=NodeClaim");
    let mut stream = watcher(events, cfg)
        .default_backoff()
        .applied_objects()
        .boxed();

    info!("spot-interrupt watcher started");

    loop {
        let ev = tokio::select! {
            biased;
            _ = shutdown.cancelled() => return,
            next = stream.next() => match next {
                Some(Ok(e)) => e,
                Some(Err(e)) => { warn!(error = %e, "spot-interrupt: stream error"); continue; }
                None => { warn!("spot-interrupt: stream ended (unexpected)"); return; }
            },
        };
        // NodeClaim → Node: Karpenter sets the NodeClaim's
        // `status.nodeName` once the node registers; the Event's
        // `involvedObject.name` IS the NodeClaim name, which Karpenter
        // also uses as the Node name. Fall back to a direct cache
        // lookup by that name.
        let Some(node) = ev.involved_object.name else {
            continue;
        };
        let Some(hw_class) = cache.hw_class_of(&node) else {
            debug!(%node, "spot-interrupt: node not in cache; skipping");
            continue;
        };
        let r = admin
            .append_interrupt_sample(rio_proto::types::AppendInterruptSampleRequest {
                hw_class: hw_class.clone(),
                kind: "interrupt".into(),
                value: 1.0,
            })
            .await;
        match r {
            Ok(_) => debug!(%node, %hw_class, "spot-interrupt: sample appended"),
            Err(e) => warn!(%node, error = %e, "spot-interrupt: append failed"),
        }
    }
}

/// Append `kind='exposure'` for a deleted spot node. Best-effort: a
/// failed RPC drops one denominator sample (λ reads slightly high
/// until the next exposure lands).
async fn report_exposure(admin: &mut AdminServiceClient<Channel>, hw_class: String, secs: f64) {
    if let Err(e) = admin
        .append_interrupt_sample(rio_proto::types::AppendInterruptSampleRequest {
            hw_class,
            kind: "exposure".into(),
            value: secs,
        })
        .await
    {
        warn!(error = %e, "spot-exposure: append failed");
    }
}

/// `Some((pod_name, namespace, hw_class))` if `pod` should be stamped:
/// it has a `spec.nodeName`, that node is in `cache`, and the
/// annotation isn't already set. Pure for unit-testability.
fn hw_class_patch_target(pod: &Pod, cache: &NodeLabelCache) -> Option<(String, String, String)> {
    let already = pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(ANNOT_HW_CLASS));
    if already {
        return None;
    }
    let node = pod.spec.as_ref()?.node_name.as_deref()?;
    let hw = cache.hw_class_of(node)?;
    let name = pod.metadata.name.clone()?;
    let ns = pod.metadata.namespace.clone()?;
    Some((name, ns, hw))
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::BTreeMap;

    fn node(name: &str, labels: &[(&str, &str)]) -> Node {
        let mut n = Node::default();
        n.metadata.name = Some(name.into());
        n.metadata.labels = Some(
            labels
                .iter()
                .map(|(k, v)| ((*k).into(), (*v).into()))
                .collect::<BTreeMap<_, _>>(),
        );
        n
    }

    #[test]
    fn hw_class_of_returns_formatted() {
        let cache = NodeLabelCache::default();
        cache.apply(&node(
            "ip-10-0-1-5",
            &[
                (LABEL_MANUFACTURER, "aws"),
                (LABEL_GENERATION, "7"),
                (LABEL_STORAGE, "ebs"),
            ],
        ));
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), Some("aws-7-ebs".into()));
        assert_eq!(cache.hw_class_of("missing"), None);
    }

    #[test]
    fn hw_class_defaults_on_missing_labels() {
        let cache = NodeLabelCache::default();
        // No labels at all → all defaults.
        cache.apply(&node("bare", &[]));
        assert_eq!(
            cache.hw_class_of("bare"),
            Some("unknown-unknown-ebs".into())
        );
        // Partial: only manufacturer set.
        cache.apply(&node("partial", &[(LABEL_MANUFACTURER, "intel")]));
        assert_eq!(
            cache.hw_class_of("partial"),
            Some("intel-unknown-ebs".into())
        );
    }

    #[test]
    fn delete_evicts_entry() {
        let cache = NodeLabelCache::default();
        let n = node("ip-10-0-1-5", &[(LABEL_STORAGE, "nvme")]);
        cache.apply(&n);
        assert_eq!(cache.len(), 1);
        cache.delete(&n);
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), None);
        assert!(cache.is_empty());
    }

    #[test]
    fn spot_exposure_gates_on_capacity_type() {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let cache = NodeLabelCache::default();
        let mut spot = node(
            "spot-n",
            &[(LABEL_CAPACITY_TYPE, "spot"), (LABEL_GENERATION, "7")],
        );
        spot.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&spot);
        let mut od = node("od-n", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&od);
        // Spot node → exposure with hw_class + uptime.
        let (hw, secs) = cache.spot_exposure("spot-n", 1100.0).unwrap();
        assert_eq!(hw, "unknown-7-ebs");
        assert!((secs - 100.0).abs() < 1e-6);
        // On-demand → no exposure (λ=0 by definition).
        assert!(cache.spot_exposure("od-n", 1100.0).is_none());
        // Unknown node → None.
        assert!(cache.spot_exposure("missing", 1100.0).is_none());
    }

    fn pod(name: &str, ns: &str, node: Option<&str>, annots: &[(&str, &str)]) -> Pod {
        let mut p = Pod::default();
        p.metadata.name = Some(name.into());
        p.metadata.namespace = Some(ns.into());
        if !annots.is_empty() {
            p.metadata.annotations = Some(
                annots
                    .iter()
                    .map(|(k, v)| ((*k).into(), (*v).into()))
                    .collect(),
            );
        }
        if let Some(n) = node {
            p.spec = Some(k8s_openapi::api::core::v1::PodSpec {
                node_name: Some(n.into()),
                ..Default::default()
            });
        }
        p
    }

    #[test]
    fn patch_target_stamps_once() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("ip-10-0-1-5", &[(LABEL_STORAGE, "nvme")]));
        // Scheduled, not yet annotated → stamp.
        let p = pod("rb-abc", "rio", Some("ip-10-0-1-5"), &[]);
        assert_eq!(
            hw_class_patch_target(&p, &cache),
            Some(("rb-abc".into(), "rio".into(), "unknown-unknown-nvme".into()))
        );
        // Already annotated → skip (sticky).
        let p = pod(
            "rb-abc",
            "rio",
            Some("ip-10-0-1-5"),
            &[(ANNOT_HW_CLASS, "unknown-unknown-nvme")],
        );
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Pending (no nodeName yet) → skip.
        let p = pod("rb-pending", "rio", None, &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Node not in cache (informer race / non-Karpenter) → skip.
        let p = pod("rb-unk", "rio", Some("ip-10-0-9-9"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
    }

    #[test]
    fn apply_upserts_on_label_change() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("n", &[(LABEL_STORAGE, "ebs")]));
        assert_eq!(cache.hw_class_of("n"), Some("unknown-unknown-ebs".into()));
        // Relabel (e.g., operator manually patched) → Modify event →
        // apply() again → cache reflects new value.
        cache.apply(&node("n", &[(LABEL_STORAGE, "nvme")]));
        assert_eq!(cache.hw_class_of("n"), Some("unknown-unknown-nvme".into()));
        assert_eq!(cache.len(), 1, "upsert, not duplicate");
    }
}
