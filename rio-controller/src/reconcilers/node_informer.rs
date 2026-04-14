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
use k8s_openapi::api::core::v1::Node;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use tracing::{debug, info, warn};

/// Karpenter-set: `aws`, `intel`, `amd`. Unknown if label absent
/// (non-Karpenter node, or Karpenter < v0.33).
const LABEL_MANUFACTURER: &str = "karpenter.k8s.aws/instance-cpu-manufacturer";
/// Karpenter-set: instance generation number as string (`"7"` for
/// c7/m7/r7, etc.).
const LABEL_GENERATION: &str = "karpenter.k8s.aws/instance-generation";
/// rio-set via EC2NodeClass `spec.tags` → Karpenter propagates to
/// Node labels. `ebs` (gp3 root) or `nvme` (instance-store).
const LABEL_STORAGE: &str = "rio.build/storage";

/// Three labels that determine build-performance equivalence class.
/// Formatted as `"{manufacturer}-{generation}-{storage}"` for the
/// completion-sample `hw_class` column.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct HwClass {
    pub manufacturer: String,
    pub generation: String,
    pub storage: String,
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
pub struct NodeLabelCache(Arc<RwLock<HashMap<String, HwClass>>>);

impl NodeLabelCache {
    /// Formatted `hw_class` string for `node_name`, or `None` if the
    /// node is not (or no longer) in the cache.
    pub fn hw_class_of(&self, node_name: &str) -> Option<String> {
        self.0.read().get(node_name).map(HwClass::as_string)
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
        let hw = HwClass::from_node(node);
        self.0.write().insert(name, hw);
    }

    fn delete(&self, node: &Node) {
        if let Some(name) = &node.metadata.name {
            self.0.write().remove(name);
        }
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
pub async fn run(client: Client, cache: NodeLabelCache, shutdown: rio_common::signal::Token) {
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
                cache.delete(&node);
            }
            watcher::Event::Init | watcher::Event::InitDone => {}
        }
    }
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
