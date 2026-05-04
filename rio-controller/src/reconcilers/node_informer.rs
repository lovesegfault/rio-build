//! Node-label cache for `hw_class` join at completion-ingest.
//!
//! ADR-023 §Hardware heterogeneity: builders are air-gapped from the
//! apiserver, so they report `spec.nodeName` (downward API) and the
//! controller joins to Node labels server-side when ingesting build
//! completion samples. `hw_class` is the operator's
//! `[sla.hw_classes.$h]` key whose label conjunction matches the
//! Node — fetched via [`HwClassConfig::load`] (`GetHwClassConfig`
//! RPC) so the controller stamps the SAME `$h` string the scheduler's
//! `solve_intent_for` keys on, not a hardcoded 4-label reconstruction
//! that breaks the moment an operator's label schema differs (bug_061).
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
//! images list — kilobytes each). We need the labels map only. A
//! hand-rolled `HashMap<String, NodeMeta>` is the lightweight version.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::reconcilers::nodeclaim_pool::ffd::{parse_bytes, parse_cpu_millis};
use crate::reconcilers::{AdminClient, admin_call};

/// Pod annotation the [`run_pod_annotator`] watcher stamps with the
/// node's matched `hw_class` (the operator's `[sla.hw_classes.$h]`
/// key). Builder reads it via downward-API (`RIO_HW_CLASS`) to key its
/// `hw_perf_samples` insert.
pub const ANNOT_HW_CLASS: &str = "rio.build/hw-class";

/// `karpenter.sh/capacity-type` — `spot` or `on-demand`. Read from the
/// cached labels so the spot-interrupt watcher can skip on-demand nodes
/// without re-fetching.
const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

/// Operator-configured hw-class definitions, fetched once via
/// `GetHwClassConfig`. Shared (`Arc`) between [`NodeLabelCache`] and
/// the [`load`](Self::load) task. The match is lazy
/// ([`NodeLabelCache::hw_class_of`] etc. call [`Self::match_node`] on
/// each lookup) so config arriving AFTER nodes are cached still
/// resolves correctly; before load completes `hw_class = None`
/// everywhere — annotator skips, λ-samples skip.
///
/// Stored as a sorted `Vec` so [`Self::match_node`] is deterministic
/// when a Node satisfies two overlapping conjunctions (lexicographic
/// `$h` wins).
#[derive(Clone, Default)]
pub struct HwClassConfig(Arc<RwLock<Vec<HwClassResolved>>>);

/// One resolved `[sla.hw_classes.$h]` entry. Named struct (was a
/// 6-tuple pre-§13c; at 10 fields a tuple's positional accessors —
/// `(_, _, _, nc, ..)` — are a destructure-site bug magnet).
#[derive(Clone, Default)]
pub(crate) struct HwClassResolved {
    /// Operator's `$h` key.
    pub name: String,
    /// ANDed `(k, v)` Node-stamp labels.
    pub labels: Vec<(String, String)>,
    /// Karpenter instance-type `spec.requirements`.
    pub requirements: Vec<rio_proto::types::NodeSelectorRequirement>,
    /// EC2NodeClass name.
    pub node_class: String,
    /// Per-class capacity ceiling (cores).
    pub max_cores: u32,
    /// Per-class capacity ceiling (bytes).
    pub max_mem: u64,
    /// §13c: per-class Node taints.
    pub taints: Vec<rio_proto::types::NodeTaint>,
    /// §13c: `requiredSystemFeatures` this class hosts.
    pub provides_features: Vec<String>,
    /// §13c: per-class fleet-core sub-budget.
    pub max_fleet_cores: Option<u32>,
    /// §13c: capacity-types this class is permitted to provision
    /// (Karpenter label form: `"spot"` / `"on-demand"`). Empty ⇔ not
    /// shipped by scheduler ⇔ ALL.
    pub capacity_types: Vec<String>,
}

impl HwClassConfig {
    /// First `$h` (lexicographic) whose every `(k, v)` is satisfied by
    /// `labels`. `None` if no conjunction matches OR config is empty
    /// (not yet loaded).
    pub fn match_node(&self, labels: &BTreeMap<String, String>) -> Option<String> {
        let cfg = self.0.read();
        for d in cfg.iter() {
            if d.labels
                .iter()
                .all(|(k, v)| labels.get(k).is_some_and(|nv| nv == v))
            {
                // rio-store rejects `!is_hw_class_name(hw_class)`. The
                // operator's `$h` is the value written to
                // `hw_perf_samples.hw_class`; a config change that
                // violates the charset would silently break the
                // builder's AppendHwPerfSample. `SlaConfig::validate`
                // is the load-bearing check (bug_038); this is a
                // belt-and-suspenders fail-fast in tests.
                debug_assert!(
                    rio_common::limits::is_hw_class_name(&d.name),
                    "hw_class {:?} fails is_hw_class_name",
                    d.name
                );
                return Some(d.name.clone());
            }
        }
        None
    }

    /// `[sla.hw_classes.$h].labels` for `h` — the conjunction the
    /// scheduler's `cells_to_selector_terms` would emit. `None` if `h`
    /// is unknown OR config not yet loaded. The §13b `cover_deficit`
    /// stamps these on NodeClaim `metadata.labels` so the launched
    /// Node carries `rio.build/hw-band`/`storage` (these are NOT
    /// instance-type properties — see [`Self::requirements_for`]).
    pub fn labels_for(&self, h: &str) -> Option<Vec<(String, String)>> {
        self.find(h).map(|d| d.labels.clone())
    }

    /// Find the entry for `h` (under read lock). Internal — public
    /// accessors clone out the field they need so the lock doesn't
    /// escape.
    fn find(&self, h: &str) -> Option<parking_lot::MappedRwLockReadGuard<'_, HwClassResolved>> {
        parking_lot::RwLockReadGuard::try_map(self.0.read(), |cfg| cfg.iter().find(|d| d.name == h))
            .ok()
    }

    /// `[sla.hw_classes.$h].requirements` for `h` — the Karpenter
    /// instance-type selectors (`karpenter.k8s.aws/instance-generation
    /// In [7]`, `kubernetes.io/arch In [amd64]`, etc.). `None` if `h`
    /// is unknown OR config not yet loaded. The §13b `cover_deficit`
    /// reads this to build NodeClaim `spec.requirements`.
    pub fn requirements_for(
        &self,
        h: &str,
    ) -> Option<Vec<rio_proto::types::NodeSelectorRequirement>> {
        self.find(h).map(|d| d.requirements.clone())
    }

    /// `[sla.hw_classes.$h].node_class` for `h` — the EC2NodeClass
    /// name (`rio-default` / `rio-nvme` / `rio-metal`). `None` if `h`
    /// is unknown OR config not yet loaded.
    pub fn node_class_for(&self, h: &str) -> Option<String> {
        self.find(h).map(|d| d.node_class.clone())
    }

    /// `[sla.hw_classes.$h].{max_cores, max_mem}` for `h` — the
    /// per-class capacity ceilings. `None` if `h` is unknown, config
    /// not yet loaded, OR the loaded entry's ceilings are zero (proto
    /// default — pre-R26 scheduler that doesn't ship them). The §13b
    /// `cover_deficit` builds per-cell `SizingCfg` with
    /// `min(per_class, global)` so claims chunk at the class's actual
    /// instance-type ceiling instead of the global cap.
    pub fn ceilings_for(&self, h: &str) -> Option<(u32, u64)> {
        self.find(h)
            .map(|d| (d.max_cores, d.max_mem))
            .filter(|&(mc, mm)| mc > 0 && mm > 0)
    }

    /// `[sla.hw_classes.$h].taints` for `h`. §13c `cover::build_nodeclaim`
    /// chains these after `builder_taint()`. Unknown `h` / not loaded →
    /// empty.
    pub fn taints_for(&self, h: &str) -> Vec<rio_proto::types::NodeTaint> {
        self.find(h).map(|d| d.taints.clone()).unwrap_or_default()
    }

    /// `[sla.hw_classes.$h].provides_features` for `h`. Unknown `h` /
    /// not loaded → empty.
    pub fn provides_for(&self, h: &str) -> Vec<String> {
        self.find(h)
            .map(|d| d.provides_features.clone())
            .unwrap_or_default()
    }

    /// `[sla.hw_classes.$h].max_fleet_cores` for `h`. §13c
    /// `cover_deficit` clamps this class's per-tick mint at
    /// `min(global_remaining, cap − live_h − created_h)`. Unknown `h`
    /// / not loaded / unset → `None` (global-only).
    pub fn fleet_cap_for(&self, h: &str) -> Option<u32> {
        self.find(h).and_then(|d| d.max_fleet_cores)
    }

    /// `[sla.hw_classes.$h].capacity_types` for `h` as controller-side
    /// [`CapacityType`]s. §13c: `all_cells`/`fallback_cell` iterate
    /// THIS so an od-only class structurally never produces a
    /// `(h, Spot)` cell. Unknown `h` / not loaded / empty (pre-§13c
    /// scheduler) → ALL.
    pub fn capacity_types_for(
        &self,
        h: &str,
    ) -> Vec<crate::reconcilers::nodeclaim_pool::CapacityType> {
        use crate::reconcilers::nodeclaim_pool::CapacityType;
        self.find(h)
            .map(|d| {
                d.capacity_types
                    .iter()
                    .filter_map(|s| CapacityType::parse(s))
                    .collect::<Vec<_>>()
            })
            .filter(|v| !v.is_empty())
            .unwrap_or_else(|| vec![CapacityType::Spot, CapacityType::OnDemand])
    }

    /// All loaded hw-class names (sorted). §13b's `all_cells` derives
    /// the cell universe from this × `capacity_types_for(h)`.
    pub fn names(&self) -> Vec<String> {
        self.0.read().iter().map(|d| d.name.clone()).collect()
    }

    /// Whether `h`'s `kubernetes.io/arch` label equals `arch`, OR is
    /// absent (an arch-agnostic hw-class matches any arch). `false` if
    /// `h` is unknown / config not yet loaded. §13b cold-start fallback
    /// (`NodeClaimPoolConfig::fallback_cell`) uses this to pick a
    /// reference cell for hw-agnostic intents by `intent.system`.
    pub fn matches_arch(&self, h: &str, arch: &str) -> bool {
        let Some(d) = self.find(h) else {
            return false;
        };
        d.labels
            .iter()
            .find(|(k, _)| k == crate::reconcilers::nodeclaim_pool::ARCH_LABEL)
            .is_none_or(|(_, v)| v == arch)
    }

    /// Replace the config wholesale from a `GetHwClassConfigResponse`.
    /// Sorted by `$h` for deterministic [`Self::match_node`] on overlap.
    /// `pub(crate)` for tests that need per-class `max_cores`/`max_mem`
    /// (which [`Self::from_literals`] doesn't carry).
    pub(crate) fn set(&self, hw_classes: HashMap<String, rio_proto::types::HwClassLabels>) {
        let mut v: Vec<_> = hw_classes
            .into_iter()
            .map(|(h, def)| HwClassResolved {
                name: h,
                labels: def.labels.into_iter().map(|l| (l.key, l.value)).collect(),
                requirements: def.requirements,
                node_class: def.node_class,
                max_cores: def.max_cores,
                max_mem: def.max_mem,
                taints: def.taints,
                provides_features: def.provides_features,
                max_fleet_cores: def.max_fleet_cores,
                capacity_types: def.capacity_types,
            })
            .collect();
        v.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        *self.0.write() = v;
    }

    /// Fetch `GetHwClassConfig` with bounded backoff (5 attempts, 1→16s).
    /// Returns once populated OR after the final attempt fails — callers
    /// already hold a balanced channel from `connect_forever`, so the
    /// scheduler is reachable; failures here are leader-election /
    /// service-gate transients. Unpopulated → [`Self::match_node`]
    /// returns `None` everywhere (degraded, not broken: annotator skips,
    /// λ-samples skip, `hw-bench-needed` keys on `intent.hw_class_names`
    /// which the scheduler populates independently).
    pub async fn load(&self, admin: &mut AdminClient) {
        let mut delay = Duration::from_secs(1);
        for attempt in 1..=5 {
            match admin_call(admin.get_hw_class_config(())).await {
                Ok(r) => {
                    let hw_classes = r.into_inner().hw_classes;
                    let requirements_nonempty = hw_classes
                        .values()
                        .filter(|d| !d.requirements.is_empty())
                        .count();
                    info!(
                        n = hw_classes.len(),
                        requirements_nonempty, "GetHwClassConfig loaded"
                    );
                    self.set(hw_classes);
                    return;
                }
                Err(e) => {
                    warn!(attempt, error = %e, "GetHwClassConfig failed; retrying");
                    tokio::time::sleep(delay).await;
                    delay = (delay * 2).min(Duration::from_secs(16));
                }
            }
        }
        warn!(
            "GetHwClassConfig: gave up after 5 attempts; hw_class will \
             stay None until next periodic refresh (annotator/λ degraded)"
        );
    }

    /// Test-only constructor from `(h, [(k, v), …])` literals
    /// (requirements default empty, node_class `"rio-default"`,
    /// ceilings `(0, 0)` so [`Self::ceilings_for`] returns `None` →
    /// callers fall back to global caps).
    #[cfg(test)]
    pub fn from_literals(defs: &[(&str, &[(&str, &str)])]) -> Self {
        let mut v: Vec<_> = defs
            .iter()
            .map(|(h, conj)| HwClassResolved {
                name: (*h).to_string(),
                labels: conj
                    .iter()
                    .map(|(k, v)| ((*k).to_string(), (*v).to_string()))
                    .collect(),
                node_class: "rio-default".to_string(),
                ..Default::default()
            })
            .collect();
        v.sort_unstable_by(|a, b| a.name.cmp(&b.name));
        Self(Arc::new(RwLock::new(v)))
    }
}

/// Per-node cache value: full label map (matched lazily against
/// [`HwClassConfig`]) + the spot-exposure cursor.
#[derive(Debug, Clone)]
struct NodeMeta {
    /// All Node labels. [`HwClassConfig::match_node`] reads these on
    /// each lookup — config arriving after the node was cached still
    /// resolves. `capacity_type` is `labels.get(LABEL_CAPACITY_TYPE)`.
    labels: BTreeMap<String, String>,
    /// Epoch of the last exposure flush for this node (initialised to
    /// `metadata.creationTimestamp` on first sight). [`NodeLabelCache::spot_exposure`]
    /// and [`NodeLabelCache::drain_live_spot_exposure`] return the
    /// delta `now - last_exposure_at`, so live nodes contribute to the
    /// λ denominator every flush instead of only on Delete.
    last_exposure_at: Option<f64>,
}

impl NodeMeta {
    fn is_spot(&self) -> bool {
        self.labels.get(LABEL_CAPACITY_TYPE).map(String::as_str) == Some("spot")
    }
}

/// Node-name → labels cache + the [`HwClassConfig`] those labels are
/// matched against. Cheap to clone (`Arc` × 2); share one instance
/// between the informer task and consumers.
#[derive(Clone, Default)]
pub struct NodeLabelCache {
    nodes: Arc<RwLock<HashMap<String, NodeMeta>>>,
    config: HwClassConfig,
}

impl NodeLabelCache {
    /// Construct with a pre-populated [`HwClassConfig`]. `main.rs`
    /// loads the config once (retry-backoff) before spawning the
    /// informer/annotator tasks so the first watch relist already
    /// resolves correctly; lazy match means a late-loading config
    /// would also work, but loading-first avoids a window of
    /// `hw_class = None` λ-samples on cold start.
    pub fn with_config(config: HwClassConfig) -> Self {
        Self {
            nodes: Default::default(),
            config,
        }
    }

    /// Operator's `[sla.hw_classes.$h]` key matching `node_name`'s
    /// labels, or `None` if the node is not (or no longer) cached, no
    /// `$h` matches, or config is not yet loaded.
    pub fn hw_class_of(&self, node_name: &str) -> Option<String> {
        let g = self.nodes.read();
        self.config.match_node(&g.get(node_name)?.labels)
    }

    /// `Some((hw_class, node_seconds_since_last_flush))` for
    /// `node_name` IF it's a spot node with a creation timestamp AND
    /// matches a configured `$h`. Feeds the exposure half of λ\[h\] —
    /// on-demand nodes contribute neither numerator nor denominator
    /// (their λ is 0 by definition); unmatched nodes (non-builder
    /// NodePool) have no `$h` to key on.
    ///
    /// Returns the slice since the last
    /// [`Self::drain_live_spot_exposure`] (or since creation if never
    /// drained) so the Delete arm reports only the final residual,
    /// not the full lifetime — the periodic flush has already banked
    /// the rest.
    pub fn spot_exposure(&self, node_name: &str, now_epoch: f64) -> Option<(String, f64)> {
        let g = self.nodes.read();
        let m = g.get(node_name)?;
        if !m.is_spot() {
            return None;
        }
        let hw = self.config.match_node(&m.labels)?;
        let secs = (now_epoch - m.last_exposure_at?).max(0.0);
        Some((hw, secs))
    }

    /// For every live spot node, sum `now - last_exposure_at` per
    /// `hw_class` and advance `last_exposure_at = now`. Called from
    /// [`run`]'s periodic flush so the λ denominator includes
    /// still-running (right-censored) nodes — without this, a single
    /// early interrupt in a fresh N-node burst inflates λ ≈N× until
    /// the survivors are eventually deleted.
    ///
    /// Controller restart re-seeds `last_exposure_at = created_at`
    /// (in-process state), so the first post-restart flush re-reports
    /// the pre-restart slice once. Accepted: restarts are rare and
    /// the 24h EMA halflife dampens the duplicate; the alternative
    /// (seed `= now`) would *under*-count on cold start, which is the
    /// bias direction this fix exists to eliminate.
    pub fn drain_live_spot_exposure(&self, now_epoch: f64) -> Vec<(String, f64)> {
        let mut by_hw: HashMap<String, f64> = HashMap::new();
        let mut g = self.nodes.write();
        for m in g.values_mut() {
            if !m.is_spot() {
                continue;
            }
            let Some(last) = m.last_exposure_at else {
                continue;
            };
            let secs = (now_epoch - last).max(0.0);
            m.last_exposure_at = Some(now_epoch);
            if secs > 0.0
                && let Some(hw) = self.config.match_node(&m.labels)
            {
                *by_hw.entry(hw).or_default() += secs;
            }
        }
        by_hw.into_iter().collect()
    }

    /// Evict every cached entry whose name is NOT in `seen`, returning
    /// each evicted spot entry's final exposure slice. Called on
    /// `InitDone` after a watch-gap relist: the raw `watcher` does NOT
    /// synthesize `Delete` for nodes gone during the gap, so without
    /// this they linger and feed [`Self::drain_live_spot_exposure`]
    /// ~60 phantom node-seconds/min forever — biasing λ\[h\] low and
    /// `solve_full` toward spot. Mirrors the `Delete` arm's residual
    /// flush so the evicted node's last slice still lands in the
    /// denominator.
    pub fn prune_absent(&self, seen: &HashSet<String>, now_epoch: f64) -> Vec<(String, f64)> {
        // Aggregate by hw_class (mirrors `drain_live_spot_exposure`):
        // the caller awaits one `report_exposure` RPC per returned
        // entry inside a single `select!` arm, so per-NODE tuples turn
        // a 100-node spot-storm relist into 100 sequential awaits ×
        // ADMIN_RPC_TIMEOUT against a degraded scheduler — ~500s of
        // watch-loop starvation. The RPC payload carries no per-node
        // identity (only hw_class/kind/value, event_uid: None) and
        // exposure sums into λ's denominator, so aggregation is
        // lossless (bug_057).
        let mut by_hw: HashMap<String, f64> = HashMap::new();
        let config = &self.config;
        self.nodes.write().retain(|name, m| {
            if seen.contains(name) {
                return true;
            }
            if m.is_spot()
                && let Some(last) = m.last_exposure_at
                && let Some(hw) = config.match_node(&m.labels)
            {
                *by_hw.entry(hw).or_default() += (now_epoch - last).max(0.0);
            }
            false
        });
        by_hw.into_iter().collect()
    }

    /// Current cache size. For the `rio_controller_node_cache_size`
    /// gauge and tests.
    pub fn len(&self) -> usize {
        self.nodes.read().len()
    }

    /// `len() == 0`. Clippy `len_without_is_empty`.
    pub fn is_empty(&self) -> bool {
        self.nodes.read().is_empty()
    }

    fn apply(&self, node: &Node) {
        let Some(name) = node.metadata.name.clone() else {
            return;
        };
        let created_at = node
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second() as f64);
        let mut g = self.nodes.write();
        // Preserve last_exposure_at across re-apply (watch relist,
        // label-change Modify) so the periodic flush doesn't re-count
        // the already-banked slice. New entry → seed from created_at.
        let last_exposure_at = g.get(&name).and_then(|m| m.last_exposure_at).or(created_at);
        g.insert(
            name,
            NodeMeta {
                labels: node.metadata.labels.clone().unwrap_or_default(),
                last_exposure_at,
            },
        );
    }

    fn delete(&self, node: &Node) -> Option<NodeMeta> {
        node.metadata
            .name
            .as_ref()
            .and_then(|n| self.nodes.write().remove(n))
    }
}

/// Per-pod `(cores, mem_bytes, disk_bytes)` request, keyed by
/// `metadata.name`. One entry per node so [`PodRequestedCache::sum_for`]
/// is `O(pods_on_node)`, not `O(all_pods)`.
type NodePodRequests = HashMap<String, (u32, u64, u64)>;

/// [`PodRequestedCache`] inner: per-node pod requests + the
/// `intent_id → node_name` index for FFD's already-bound short-circuit.
#[derive(Default)]
struct PodRequestedInner {
    by_node: HashMap<String, NodePodRequests>,
    /// Bound pods carrying `INTENT_ID_ANNOTATION`: `intent_id →
    /// (pod_name, node_name)`. FFD short-circuits an already-bound
    /// intent directly into `placeable` (no fit-check) — its own pod's
    /// `(c,m,d)` is in `sum_for(node)` so a fit-check would
    /// double-count and evict it. `pod_name` is in the value so
    /// [`prune_absent`](Self::prune_absent) and
    /// [`delete`](Self::delete) prune at per-pod granularity (not
    /// per-node — a sibling pod surviving on the same node mustn't
    /// keep a deleted pod's binding alive).
    bound_intent: HashMap<String, (String, String)>,
}

/// `spec.nodeName` → Σ pod `resources.requests` cache for the §13b FFD
/// sim's `LiveNode.requested`. Hand-rolled (NOT `reflector::store`) for
/// the same reason as [`NodeLabelCache`]: a full-object Pod reflector
/// caches kilobytes (status, conditions, container statuses) per pod;
/// we need three integers. Label-selected on `rio.build/pool` so the
/// watch scope matches [`run_pod_annotator`]'s — only builder/fetcher
/// pods, not the cluster's full pod set.
///
/// Cheap to clone (`Arc`); share one instance between the watcher task
/// and the `NodeClaimPoolReconciler`.
#[derive(Clone, Default)]
pub struct PodRequestedCache(Arc<RwLock<PodRequestedInner>>);

impl PodRequestedCache {
    /// `Σ (cores, mem, disk)` over pods bound to `node_name`. `(0,0,0)`
    /// for an unknown node (no pods scheduled, or watcher not yet
    /// caught up).
    pub fn sum_for(&self, node_name: &str) -> (u32, u64, u64) {
        let g = self.0.read();
        g.by_node.get(node_name).map_or((0, 0, 0), |pods| {
            pods.values().fold((0, 0, 0), |(c, m, d), &(pc, pm, pd)| {
                (c + pc, m + pm, d + pd)
            })
        })
    }

    /// `intent_id → node_name` for bound pods carrying
    /// `INTENT_ID_ANNOTATION`. FFD short-circuits already-bound
    /// intents to `placeable` instead of fit-checking them (which
    /// would double-count their own pod's reservation against them).
    pub fn bound_intents(&self) -> HashMap<String, String> {
        self.0
            .read()
            .bound_intent
            .iter()
            .map(|(id, (_, node))| (id.clone(), node.clone()))
            .collect()
    }

    /// Upsert `pod`'s requests under its `spec.nodeName`. Pending pods
    /// (no `nodeName`) are ignored — they haven't reserved capacity on
    /// any node. Terminal-phase pods (`Succeeded`/`Failed`) are evicted
    /// — kube-scheduler's NodeResourcesFit excludes them; with
    /// `JOB_TTL_SECS=600s` they'd otherwise inflate `sum_for` for ~60
    /// FFD ticks per build completion. A `nodeName` change (rare;
    /// binding is normally immutable, but a relist after
    /// delete+recreate of a same-named pod would look like one) evicts
    /// the old-node entry first.
    fn apply(&self, pod: &Pod) {
        if matches!(
            pod.status.as_ref().and_then(|s| s.phase.as_deref()),
            Some("Succeeded" | "Failed")
        ) {
            return self.delete(pod);
        }
        let terminating = pod.metadata.deletion_timestamp.is_some();
        let Some(name) = pod.metadata.name.as_ref() else {
            return;
        };
        let Some(node) = pod.spec.as_ref().and_then(|s| s.node_name.as_ref()) else {
            return;
        };
        let intent_id = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION))
            .cloned();
        let req = pod_requests(pod);
        let mut g = self.0.write();
        // Evict from any other node first (nodeName change / stale).
        for (n, pods) in g.by_node.iter_mut() {
            if n != node {
                pods.remove(name);
            }
        }
        g.by_node
            .entry(node.clone())
            .or_default()
            .insert(name.clone(), req);
        // A terminating pod (deletionTimestamp set, phase still Running
        // for the grace period) must not (re-)claim the binding — the
        // retry's NEW pod may already hold it. Without this guard, a
        // late kubelet MODIFIED for the old pod overwrites the fresh
        // entry, then the (correctly pod-name-guarded) terminal-phase
        // delete removes it — the §Verifier-one-step-removed sibling of
        // mb_034's delete-guard. The `by_node` insert above stays
        // unconditional: a terminating pod still holds node resources
        // until gone.
        if let Some(id) = intent_id.filter(|_| !terminating) {
            g.bound_intent.insert(id, (name.clone(), node.clone()));
        }
    }

    /// Evict `pod` from its node's entry and the bound-intent index.
    /// Node entries are NOT garbage-collected when they empty — node
    /// count is bounded and the empty `HashMap` is ~48 bytes.
    fn delete(&self, pod: &Pod) {
        let Some(name) = pod.metadata.name.as_ref() else {
            return;
        };
        let intent_id = pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION));
        let mut g = self.0.write();
        for pods in g.by_node.values_mut() {
            pods.remove(name);
        }
        // Only evict the binding if THIS pod is the one recorded. A
        // retry of the same drv (`intent_id == drv_hash`) may have
        // already bound a NEW pod via [`apply`](Self::apply); a late
        // delete for the old pod must not wipe the fresh entry.
        if let Some(id) = intent_id
            && g.bound_intent.get(id).map(|(p, _)| p.as_str()) == Some(name)
        {
            g.bound_intent.remove(id);
        }
    }

    /// Evict every cached pod whose name is NOT in `seen`. Called on
    /// `InitDone` after a watch-gap relist (mirrors
    /// [`NodeLabelCache::prune_absent`]): the raw watcher synthesizes
    /// no Delete for pods gone during the gap, so without this their
    /// requests would inflate `sum_for` until controller restart —
    /// `free()` under-reports → FFD under-places → `cover_deficit`
    /// over-provisions.
    fn prune_absent(&self, seen: &HashSet<String>) {
        let mut g = self.0.write();
        for pods in g.by_node.values_mut() {
            pods.retain(|name, _| seen.contains(name));
        }
        g.bound_intent.retain(|_, (pod, _)| seen.contains(pod));
    }
}

/// `Σ containers[].resources.requests` as `(cores, mem, disk)`. Whole
/// cores (truncated millicores) so the unit matches `SpawnIntent.cores`
/// and `LiveNode.allocatable.0`. Missing keys → 0.
fn pod_requests(pod: &Pod) -> (u32, u64, u64) {
    let Some(spec) = pod.spec.as_ref() else {
        return (0, 0, 0);
    };
    spec.containers.iter().fold((0, 0, 0), |(c, m, d), ctr| {
        let q = |k: &str| {
            ctr.resources
                .as_ref()
                .and_then(|r| r.requests.as_ref())
                .and_then(|r| r.get(k))
                .map(|q| q.0.as_str())
        };
        (
            c + q("cpu").map_or(0, |s| (parse_cpu_millis(s) / 1000) as u32),
            m + q("memory").map_or(0, parse_bytes),
            d + q("ephemeral-storage").map_or(0, parse_bytes),
        )
    })
}

/// Pod-watcher: maintain [`PodRequestedCache`] from Applied/Deleted
/// events on `rio.build/pool`-labelled pods. Same selector as
/// [`run_pod_annotator`] so the apiserver watch scope is identical.
///
/// `spawn_monitored("pod-requested-cache", ...)` from main.rs. Same
/// degraded-not-broken contract as [`run`]: if the watcher dies,
/// `sum_for` returns stale/zero and `LiveNode::free()` over-reports —
/// FFD double-places, `cover_deficit` under-provisions next tick.
/// Observable as `ffd_unplaced_cores` low with builder pods Pending.
pub async fn run_pod_requested_cache(
    client: Client,
    cache: PodRequestedCache,
    shutdown: rio_common::signal::Token,
) {
    let pods: Api<Pod> = Api::all(client);
    let cfg = watcher::Config::default().labels(POOL_LABEL);
    let mut stream = watcher(pods, cfg).default_backoff().boxed();

    // Names seen between Init and InitDone (relist prune; see
    // `PodRequestedCache::prune_absent`).
    let mut relist_seen: Option<HashSet<String>> = None;

    info!("pod-requested cache started (label={POOL_LABEL})");

    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("pod-requested cache: shutdown");
                return;
            }
            next = stream.next() => match next {
                Some(Ok(ev)) => ev,
                Some(Err(e)) => { warn!(error = %e, "pod-requested cache: stream error"); continue; }
                None => { warn!("pod-requested cache: stream ended (unexpected)"); return; }
            },
        };
        match event {
            watcher::Event::Apply(pod) => cache.apply(&pod),
            watcher::Event::InitApply(pod) => {
                if let Some(seen) = relist_seen.as_mut()
                    && let Some(name) = &pod.metadata.name
                {
                    seen.insert(name.clone());
                }
                cache.apply(&pod);
            }
            watcher::Event::Delete(pod) => cache.delete(&pod),
            watcher::Event::Init => relist_seen = Some(HashSet::new()),
            watcher::Event::InitDone => {
                if let Some(seen) = relist_seen.take() {
                    cache.prune_absent(&seen);
                }
            }
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
pub async fn run(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminClient,
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

    // Live-exposure flush cadence. λ's denominator must include
    // censored (still-running) observations; flushing every minute
    // bounds the right-censoring bias to ≤60 node-seconds per node.
    let mut flush = tokio::time::interval(Duration::from_secs(EXPOSURE_FLUSH_SECS));
    flush.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // HwClassConfig periodic refresh. main.rs calls `load()` once at
    // startup, but if the scheduler is mid-rollout that can return
    // empty `requirements` (live B11: cover_deficit emitted NodeClaims
    // with only the capacity-type req → Karpenter picked arbitrary
    // arch). `load()` already retry-backoffs internally, so a single
    // call per tick suffices; the `Arc<RwLock>` inside `HwClassConfig`
    // means downstream readers (`cover_deficit`, `match_node`) see the
    // refreshed config without re-clone.
    let mut hw_refresh = tokio::time::interval(Duration::from_secs(HW_CONFIG_REFRESH_SECS));
    hw_refresh.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);

    // Names seen between Init and InitDone. `Some` only during a
    // relist; `None` during steady-state Apply/Delete.
    let mut relist_seen: Option<HashSet<String>> = None;

    info!("Node informer started");

    loop {
        let event = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("Node informer: shutdown");
                return;
            }
            _ = flush.tick() => {
                for (hw, secs) in cache.drain_live_spot_exposure(now_epoch()) {
                    report_exposure(&mut admin, hw, secs).await;
                }
                continue;
            }
            _ = hw_refresh.tick() => {
                cache.config.load(&mut admin).await;
                continue;
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
        // bracket a relist: Init opens a seen-set, InitApply both
        // upserts AND records the name, InitDone prunes any cached
        // entry NOT re-seen. The raw watcher synthesizes no Delete
        // for nodes gone during a watch gap; before the periodic
        // drain_live_spot_exposure flush this was a harmless stale-
        // entry leak (passive lookup), but now a leaked spot entry
        // would feed phantom node-seconds into λ's denominator until
        // controller restart.
        match event {
            watcher::Event::Apply(node) => {
                cache.apply(&node);
            }
            watcher::Event::InitApply(node) => {
                if let Some(seen) = relist_seen.as_mut()
                    && let Some(name) = &node.metadata.name
                {
                    seen.insert(name.clone());
                }
                cache.apply(&node);
            }
            watcher::Event::Delete(node) => {
                // ADR-023 phase-13: final exposure slice → λ
                // denominator. Compute BEFORE delete (cache still has
                // last_exposure_at). Periodic flush above has already
                // banked the bulk of this node's lifetime.
                if let Some(name) = &node.metadata.name
                    && let Some((hw, secs)) = cache.spot_exposure(name, now_epoch())
                {
                    report_exposure(&mut admin, hw, secs).await;
                }
                cache.delete(&node);
            }
            watcher::Event::Init => {
                relist_seen = Some(HashSet::new());
            }
            watcher::Event::InitDone => {
                if let Some(seen) = relist_seen.take() {
                    for (hw, secs) in cache.prune_absent(&seen, now_epoch()) {
                        report_exposure(&mut admin, hw, secs).await;
                    }
                }
            }
        }
    }
}

/// Live spot-node exposure flush cadence (seconds). See
/// [`NodeLabelCache::drain_live_spot_exposure`].
const EXPOSURE_FLUSH_SECS: u64 = 60;

/// `HwClassConfig::load` re-fetch cadence. Covers the scheduler-
/// rollout race where the startup load got stale/empty `requirements`.
const HW_CONFIG_REFRESH_SECS: u64 = 300;

fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map(|d| d.as_secs_f64())
        .unwrap_or(0.0)
}

/// Builder/fetcher pod label selector — same value `disruption.rs`
/// filters on. Re-declared here (not re-exported) to keep this module
/// dependency-free of `pool`.
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
/// watcher dies, builders' downward-API volume stays empty,
/// `hw_class::resolve` returns `None` after its 30s bound, and the
/// hw_class stays at `factor=1.0` until ≥3 pods report. The volume
/// (NOT env-var) form means a late stamp still reaches a running pod.
///
/// TODO: gate the stamp on `EXISTS(SELECT 1 FROM hw_perf_samples WHERE
/// hw_class=$1)` so well-sampled classes skip the ~5s bench. Deferred:
/// rio-controller has NO PG access today (apiserver-only — every other
/// reconciler talks to the scheduler/store via gRPC, never PG
/// directly). Adding a PgPool here means: config plumbing
/// (`DATABASE_URL`), helm secret mount, IRSA policy, and a connection
/// the controller otherwise doesn't need — >50 LoC of plumbing for a
/// ~5s saving that already runs concurrent with the ~30s cold-start.
/// If this becomes worth it, route through a new
/// `SchedulerAdmin.HwClassSampled(hw_class) -> bool` RPC instead
/// (controller already holds that channel).
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
/// `reason=SpotInterrupted` (Karpenter emits this on BOTH the NodeClaim
/// and the Node when AWS sends the 2-minute spot interruption notice;
/// we watch the Node event because [`NodeLabelCache`] is keyed by Node
/// name), resolve the referenced node's hw_class, and append
/// `interrupt_samples(hw_class, kind='interrupt', value=1)` via
/// `AdminService.AppendInterruptSample`.
///
/// The exposure half (`kind='exposure', value=node_seconds`) is
/// emitted from [`run`]: a periodic flush banks live node-seconds
/// every `EXPOSURE_FLUSH_SECS`, and the Node-DELETE arm appends
/// the final residual slice. Live nodes MUST contribute — counting
/// only completed lifetimes is the right-censoring bias that spikes
/// λ at burst onset.
///
/// `field_selector` narrows the watch server-side so we don't churn
/// on the cluster's full event firehose. Karpenter's interruption
/// controller emits the `SpotInterrupted` event on BOTH the NodeClaim
/// and the Node; we watch the **Node** event so `involvedObject.name`
/// is the Node `metadata.name` — the same key [`NodeLabelCache`] is
/// indexed by. The NodeClaim event's `involvedObject.name` is the
/// NodeClaim name (`{nodepool}-{hash}` on EKS), which would always
/// miss the IP-derived-Node-name-keyed cache.
pub async fn run_spot_interrupt_watcher(
    client: Client,
    cache: NodeLabelCache,
    mut admin: AdminClient,
    shutdown: rio_common::signal::Token,
) {
    let events: Api<Event> = Api::all(client);
    let cfg = watcher::Config::default().fields("reason=SpotInterrupted,involvedObject.kind=Node");
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
        // involvedObject.kind=Node ⇒ involvedObject.name IS the Node
        // metadata.name — the cache key.
        let Some(node) = ev.involved_object.name else {
            continue;
        };
        let Some(hw_class) = cache.hw_class_of(&node) else {
            debug!(%node, "spot-interrupt: node not in cache; skipping");
            continue;
        };
        // `event_uid` makes the INSERT idempotent: `.applied_objects()`
        // re-yields every still-extant Event on relist (controller
        // restart, apiserver restart, watch reconnect). Without dedup
        // each relist double-counts into λ's numerator → `solve_full`
        // biases away from spot.
        let r = admin_call(admin.append_interrupt_sample(
            rio_proto::types::AppendInterruptSampleRequest {
                hw_class: hw_class.clone(),
                kind: "interrupt".into(),
                value: 1.0,
                event_uid: ev.metadata.uid.clone(),
            },
        ))
        .await;
        match r {
            Ok(_) => debug!(%node, %hw_class, "spot-interrupt: sample appended"),
            Err(e) => warn!(%node, error = %e, "spot-interrupt: append failed"),
        }
    }
}

/// Append `kind='exposure'` node-seconds for one `hw_class`.
/// Best-effort: a failed/timed-out RPC drops one denominator sample
/// (λ reads slightly high until the next flush lands). Bounded by
/// [`admin_call`]'s timeout so a hung scheduler can't wedge the Node-
/// informer's watch loop (every caller is inside that loop).
async fn report_exposure(admin: &mut AdminClient, hw_class: String, secs: f64) {
    if let Err(e) = admin_call(admin.append_interrupt_sample(
        rio_proto::types::AppendInterruptSampleRequest {
            hw_class,
            kind: "exposure".into(),
            value: secs,
            // Timer-driven, no K8s Event → NULL uid (unconstrained by
            // the M_047 partial unique index).
            event_uid: None,
        },
    ))
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

    /// Two-class config keyed on `rio.build/hw-band` only — covers the
    /// "operator's label schema is arbitrary" case (bug_061: a single
    /// non-Karpenter label, NOT the hardcoded 4-tuple).
    fn band_config() -> HwClassConfig {
        HwClassConfig::from_literals(&[
            ("intel-7", &[("rio.build/hw-band", "7")]),
            ("intel-6", &[("rio.build/hw-band", "6")]),
        ])
    }

    fn band_cache() -> NodeLabelCache {
        NodeLabelCache::with_config(band_config())
    }

    #[test]
    fn matches_arch_absent_label_is_agnostic() {
        use crate::reconcilers::nodeclaim_pool::ARCH_LABEL;
        let cfg = HwClassConfig::from_literals(&[
            ("x86", &[(ARCH_LABEL, "amd64"), ("k", "v")]),
            ("arm", &[(ARCH_LABEL, "arm64")]),
            ("agnostic", &[("k", "v")]),
        ]);
        assert!(cfg.matches_arch("x86", "amd64"));
        assert!(!cfg.matches_arch("x86", "arm64"));
        assert!(cfg.matches_arch("arm", "arm64"));
        // No arch label → matches any.
        assert!(cfg.matches_arch("agnostic", "amd64"));
        assert!(cfg.matches_arch("agnostic", "arm64"));
        // Unknown h → false.
        assert!(!cfg.matches_arch("nope", "amd64"));
        assert!(!HwClassConfig::default().matches_arch("x86", "amd64"));
    }

    #[test]
    fn labels_for_returns_conjunction() {
        let cfg = band_config();
        assert_eq!(
            cfg.labels_for("intel-7"),
            Some(vec![("rio.build/hw-band".into(), "7".into())])
        );
        assert_eq!(cfg.labels_for("nope"), None);
        // Empty (not yet loaded) → None for any h.
        assert_eq!(HwClassConfig::default().labels_for("intel-7"), None);
    }

    #[test]
    fn hw_class_of_matches_operator_config() {
        let cache = band_cache();
        cache.apply(&node(
            "ip-10-0-1-5",
            &[("rio.build/hw-band", "7"), ("unrelated", "x")],
        ));
        assert_eq!(cache.hw_class_of("ip-10-0-1-5"), Some("intel-7".into()));
        assert_eq!(cache.hw_class_of("missing"), None);
    }

    /// bug_061 contract: the matched value IS the operator's `$h` key,
    /// not a controller-side `"{mfg}-{gen}-{storage}-{band}"`
    /// reconstruction. A node satisfying a multi-label conjunction
    /// returns the `$h` string as-is; a node missing one label of the
    /// conjunction returns `None` (no `"unknown"` fill-in).
    #[test]
    fn hw_class_is_operator_key_not_reconstruction() {
        let cfg = HwClassConfig::from_literals(&[(
            "amd-nvme-mid",
            &[
                ("karpenter.k8s.aws/instance-cpu-manufacturer", "amd"),
                ("rio.build/storage", "nvme"),
            ],
        )]);
        let cache = NodeLabelCache::with_config(cfg);
        cache.apply(&node(
            "full",
            &[
                ("karpenter.k8s.aws/instance-cpu-manufacturer", "amd"),
                ("rio.build/storage", "nvme"),
                ("karpenter.k8s.aws/instance-generation", "6"),
            ],
        ));
        assert_eq!(cache.hw_class_of("full"), Some("amd-nvme-mid".into()));
        // Partial match (one label of the conjunction missing) → None.
        cache.apply(&node(
            "partial",
            &[("karpenter.k8s.aws/instance-cpu-manufacturer", "amd")],
        ));
        assert_eq!(cache.hw_class_of("partial"), None);
    }

    /// Unloaded config → `hw_class_of` returns `None` (annotator
    /// skips, λ-samples skip — degraded, not broken). Late config load
    /// resolves already-cached nodes (lazy match).
    #[test]
    fn hw_class_none_until_config_loaded_then_lazy_match() {
        let cache = NodeLabelCache::default();
        cache.apply(&node("n", &[("rio.build/hw-band", "7")]));
        assert_eq!(cache.hw_class_of("n"), None, "config empty → no match");
        // Config arrives: already-cached node now resolves.
        cache.config.set(
            [(
                "intel-7".into(),
                rio_proto::types::HwClassLabels {
                    labels: vec![rio_proto::types::NodeLabelMatch {
                        key: "rio.build/hw-band".into(),
                        value: "7".into(),
                    }],
                    node_class: "rio-default".into(),
                    max_cores: 64,
                    max_mem: 256 << 30,
                    ..Default::default()
                },
            )]
            .into(),
        );
        assert_eq!(cache.hw_class_of("n"), Some("intel-7".into()));
        assert_eq!(cache.config.ceilings_for("intel-7"), Some((64, 256 << 30)));
    }

    /// `ceilings_for`: per-class capacity ceilings; `None` for unknown
    /// `h` or zero values (proto default → pre-R26 scheduler).
    #[test]
    fn ceilings_for_filters_zero_and_unknown() {
        let cfg = HwClassConfig::default();
        cfg.set(
            [
                (
                    "arm".into(),
                    rio_proto::types::HwClassLabels {
                        max_cores: 64,
                        max_mem: 128 << 30,
                        ..Default::default()
                    },
                ),
                ("old".into(), rio_proto::types::HwClassLabels::default()),
            ]
            .into(),
        );
        assert_eq!(cfg.ceilings_for("arm"), Some((64, 128 << 30)));
        assert_eq!(cfg.ceilings_for("old"), None, "zero ceilings → None");
        assert_eq!(cfg.ceilings_for("nope"), None, "unknown → None");
    }

    /// Overlapping conjunctions → deterministic (lexicographic `$h`).
    #[test]
    fn match_node_deterministic_on_overlap() {
        let cfg = HwClassConfig::from_literals(&[("zz", &[("k", "v")]), ("aa", &[("k", "v")])]);
        let labels: BTreeMap<_, _> = [("k".into(), "v".into())].into();
        assert_eq!(cfg.match_node(&labels), Some("aa".into()));
    }

    #[test]
    fn delete_evicts_entry() {
        let cache = band_cache();
        let n = node("ip-10-0-1-5", &[("rio.build/hw-band", "7")]);
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
        let cache = band_cache();
        let mut spot = node(
            "spot-n",
            &[(LABEL_CAPACITY_TYPE, "spot"), ("rio.build/hw-band", "7")],
        );
        spot.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&spot);
        let mut od = node(
            "od-n",
            &[
                (LABEL_CAPACITY_TYPE, "on-demand"),
                ("rio.build/hw-band", "7"),
            ],
        );
        od.metadata.creation_timestamp = Some(Time(Timestamp::from_second(1000).unwrap()));
        cache.apply(&od);
        // Spot node → exposure with hw_class + uptime.
        let (hw, secs) = cache.spot_exposure("spot-n", 1100.0).unwrap();
        assert_eq!(hw, "intel-7");
        assert!((secs - 100.0).abs() < 1e-6);
        // On-demand → no exposure (λ=0 by definition).
        assert!(cache.spot_exposure("od-n", 1100.0).is_none());
        // Unknown node → None.
        assert!(cache.spot_exposure("missing", 1100.0).is_none());
        // Spot but no matching $h (config empty) → None (no key for λ).
        let bare = NodeLabelCache::default();
        bare.apply(&spot);
        assert!(bare.spot_exposure("spot-n", 1100.0).is_none());
    }

    fn spot_node(name: &str, band: &str, created: i64) -> Node {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let mut n = node(
            name,
            &[(LABEL_CAPACITY_TYPE, "spot"), ("rio.build/hw-band", band)],
        );
        n.metadata.creation_timestamp = Some(Time(Timestamp::from_second(created).unwrap()));
        n
    }

    /// Right-censoring fix: live spot nodes contribute exposure on
    /// every periodic drain, not only on Delete. Drain returns the
    /// per-hw_class delta since last drain and advances the cursor;
    /// Delete-arm `spot_exposure` then sees only the residual.
    #[test]
    fn drain_live_spot_exposure_banks_incremental_deltas() {
        let cache = band_cache();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand node: must NOT appear in drain output.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // First drain at t=1060: a+b → 2×60=120s for intel-7, c → 40s.
        let mut d = cache.drain_live_spot_exposure(1060.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 40.0), ("intel-7".into(), 120.0)]);

        // Second drain at t=1120: deltas only (60s each), not
        // cumulative-from-created.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 120.0)]);

        // Re-apply (watch relist / label Modify) MUST preserve
        // last_exposure_at — no double-count of the banked slice.
        cache.apply(&spot_node("a", "7", 1000));
        let (_, secs) = cache.spot_exposure("a", 1150.0).unwrap();
        assert!((secs - 30.0).abs() < 1e-6, "residual since last drain");

        // Delete-arm sees only the residual since last drain, not
        // full lifetime (periodic flush already banked 1000..1120).
        let (_, secs) = cache.spot_exposure("c", 1150.0).unwrap();
        assert!((secs - 30.0).abs() < 1e-6);
    }

    /// Watch-gap relist: a spot node deleted during the gap gets no
    /// Delete event. `prune_absent` (called on InitDone) must evict it
    /// — returning its final exposure slice — so it stops feeding
    /// `drain_live_spot_exposure` phantom node-seconds.
    #[test]
    fn prune_absent_evicts_nodes_missing_from_relist() {
        let cache = band_cache();
        cache.apply(&spot_node("a", "7", 1000));
        cache.apply(&spot_node("b", "7", 1000));
        cache.apply(&spot_node("b2", "7", 1000));
        cache.apply(&spot_node("c", "6", 1020));
        // On-demand: evicted but contributes no exposure slice.
        let mut od = node("od", &[(LABEL_CAPACITY_TYPE, "on-demand")]);
        od.metadata.creation_timestamp = spot_node("x", "7", 1000).metadata.creation_timestamp;
        cache.apply(&od);

        // Periodic flush at t=1060 advances all cursors to 1060.
        let _ = cache.drain_live_spot_exposure(1060.0);

        // Relist at t=1090 saw only {a, c}. b, b2, od deleted during
        // the gap → prune. b+b2 residuals (1060..1090 each) are
        // returned AGGREGATED by hw_class (bug_057: per-node tuples
        // would emit 2 entries × 30.0, defeating the watch-loop
        // bound); od is not spot → no exposure slice.
        let seen: HashSet<String> = ["a", "c"].into_iter().map(String::from).collect();
        let evicted = cache.prune_absent(&seen, 1090.0);
        assert_eq!(
            evicted,
            vec![("intel-7".into(), 60.0)],
            "two same-hw_class spot nodes → ONE aggregated entry (Σsecs)"
        );
        assert_eq!(cache.len(), 2);
        assert!(cache.hw_class_of("b").is_none());
        assert!(cache.hw_class_of("b2").is_none());
        assert!(cache.hw_class_of("od").is_none());

        // Next periodic drain at t=1120: only survivors contribute —
        // no phantom 60s from b/b2.
        let mut d = cache.drain_live_spot_exposure(1120.0);
        d.sort_by(|a, b| a.0.cmp(&b.0));
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 60.0)]);
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
        let cache = band_cache();
        cache.apply(&node("ip-10-0-1-5", &[("rio.build/hw-band", "7")]));
        // Scheduled, not yet annotated → stamp.
        let p = pod("rb-abc", "rio", Some("ip-10-0-1-5"), &[]);
        assert_eq!(
            hw_class_patch_target(&p, &cache),
            Some(("rb-abc".into(), "rio".into(), "intel-7".into()))
        );
        // Already annotated → skip (sticky).
        let p = pod(
            "rb-abc",
            "rio",
            Some("ip-10-0-1-5"),
            &[(ANNOT_HW_CLASS, "intel-7")],
        );
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Pending (no nodeName yet) → skip.
        let p = pod("rb-pending", "rio", None, &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Node not in cache (informer race / non-Karpenter) → skip.
        let p = pod("rb-unk", "rio", Some("ip-10-0-9-9"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
        // Cached node but no $h matches (e.g. non-builder NodePool, or
        // config not yet loaded) → skip — don't stamp a bogus value.
        cache.apply(&node("nomatch", &[("rio.build/hw-band", "9")]));
        let p = pod("rb-nm", "rio", Some("nomatch"), &[]);
        assert_eq!(hw_class_patch_target(&p, &cache), None);
    }

    fn pod_req(name: &str, node: Option<&str>, cpu: &str, mem: &str, disk: &str) -> Pod {
        use k8s_openapi::api::core::v1::{Container, PodSpec, ResourceRequirements};
        use k8s_openapi::apimachinery::pkg::api::resource::Quantity;
        let mut p = Pod::default();
        p.metadata.name = Some(name.into());
        p.metadata.namespace = Some("rio".into());
        p.spec = Some(PodSpec {
            node_name: node.map(str::to_owned),
            containers: vec![Container {
                name: "c".into(),
                resources: Some(ResourceRequirements {
                    requests: Some(
                        [
                            ("cpu".into(), Quantity(cpu.into())),
                            ("memory".into(), Quantity(mem.into())),
                            ("ephemeral-storage".into(), Quantity(disk.into())),
                        ]
                        .into(),
                    ),
                    ..Default::default()
                }),
                ..Default::default()
            }],
            ..Default::default()
        });
        p
    }

    /// F6: cache sums `Σ pod.resources.requests` per `spec.nodeName`;
    /// delete drops the pod's contribution; pods with no `nodeName`
    /// (Pending) contribute nothing.
    #[test]
    fn pod_requested_cache_sums_by_node() {
        let cache = PodRequestedCache::default();
        cache.apply(&pod_req("a", Some("n1"), "4", "8Gi", "10Gi"));
        cache.apply(&pod_req("b", Some("n1"), "2", "4Gi", "5Gi"));
        cache.apply(&pod_req("c", Some("n2"), "1500m", "2Gi", "1Gi"));
        // Pending pod (no nodeName) → not tracked.
        cache.apply(&pod_req("p", None, "8", "1Gi", "1Gi"));
        let gi = 1u64 << 30;
        assert_eq!(cache.sum_for("n1"), (6, 12 * gi, 15 * gi));
        // 1500m → 1 whole core (truncated, matching SpawnIntent.cores unit).
        assert_eq!(cache.sum_for("n2"), (1, 2 * gi, gi));
        assert_eq!(cache.sum_for("unknown"), (0, 0, 0));
        // Delete b → n1 drops to a's request.
        cache.delete(&pod_req("b", Some("n1"), "2", "4Gi", "5Gi"));
        assert_eq!(cache.sum_for("n1"), (4, 8 * gi, 10 * gi));
        // Re-apply a (watch relist / Modify) → upsert, not double-count.
        cache.apply(&pod_req("a", Some("n1"), "4", "8Gi", "10Gi"));
        assert_eq!(cache.sum_for("n1"), (4, 8 * gi, 10 * gi));
        // nodeName change (rare, but Modify with new node) → moves.
        cache.apply(&pod_req("c", Some("n1"), "1500m", "2Gi", "1Gi"));
        assert_eq!(cache.sum_for("n2"), (0, 0, 0));
        assert_eq!(cache.sum_for("n1"), (5, 10 * gi, 11 * gi));
    }

    /// F6: relist gap — a pod deleted while the watch was disconnected
    /// gets no Delete event; `prune_absent` evicts it on InitDone so
    /// `sum_for` doesn't carry phantom requests.
    #[test]
    fn pod_requested_cache_prune_absent() {
        let cache = PodRequestedCache::default();
        cache.apply(&pod_req("a", Some("n1"), "4", "8Gi", "10Gi"));
        cache.apply(&pod_req("b", Some("n1"), "2", "4Gi", "5Gi"));
        // Relist saw only {a}.
        let seen: HashSet<String> = ["a".into()].into();
        cache.prune_absent(&seen);
        let gi = 1u64 << 30;
        assert_eq!(cache.sum_for("n1"), (4, 8 * gi, 10 * gi));
    }

    #[test]
    fn apply_upserts_on_label_change() {
        let cache = band_cache();
        cache.apply(&node("n", &[("rio.build/hw-band", "6")]));
        assert_eq!(cache.hw_class_of("n"), Some("intel-6".into()));
        // Relabel (e.g., operator manually patched) → Modify event →
        // apply() again → cache reflects new value.
        cache.apply(&node("n", &[("rio.build/hw-band", "7")]));
        assert_eq!(cache.hw_class_of("n"), Some("intel-7".into()));
        assert_eq!(cache.len(), 1, "upsert, not duplicate");
    }

    fn pod_phase(mut p: Pod, phase: &str) -> Pod {
        p.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some(phase.into()),
            ..Default::default()
        });
        p
    }

    /// mb_019(B): Succeeded/Failed pods are evicted (kube-scheduler's
    /// NodeResourcesFit excludes terminal-phase pods; with
    /// `JOB_TTL_SECS=600s` they'd otherwise inflate `sum_for` for ~60
    /// FFD ticks per build completion → FFD under-places →
    /// cover_deficit mints redundant NodeClaims).
    #[test]
    fn pod_requested_excludes_terminal_phases() {
        let cache = PodRequestedCache::default();
        let p = pod_req("p", Some("n1"), "4", "8Gi", "10Gi");
        cache.apply(&p);
        let gi = 1u64 << 30;
        assert_eq!(cache.sum_for("n1"), (4, 8 * gi, 10 * gi));
        // phase=Succeeded → evicted (kube-scheduler NodeResourcesFit
        // would also exclude it).
        cache.apply(&pod_phase(p, "Succeeded"));
        assert_eq!(cache.sum_for("n1"), (0, 0, 0), "Succeeded → evicted");
        // Failed too.
        let f = pod_phase(pod_req("f", Some("n1"), "2", "4Gi", "5Gi"), "Failed");
        cache.apply(&f);
        assert_eq!(cache.sum_for("n1"), (0, 0, 0), "Failed → not counted");
        // Running stays.
        let r = pod_phase(pod_req("r", Some("n1"), "2", "4Gi", "5Gi"), "Running");
        cache.apply(&r);
        assert_eq!(cache.sum_for("n1"), (2, 4 * gi, 5 * gi));
    }

    /// bug_069: a bound pod's intent_id → node_name is indexed so FFD
    /// can short-circuit it directly to `placeable` (its own pod's
    /// (c,m,d) is in `sum_for(node)`; fit-checking would double-count
    /// and evict it → orphan-reap loop).
    #[test]
    fn pod_requested_indexes_bound_intent() {
        use crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION;
        let cache = PodRequestedCache::default();
        let mut p = pod_req("rb-x", Some("n1"), "4", "8Gi", "10Gi");
        p.metadata.annotations = Some([(INTENT_ID_ANNOTATION.into(), "abc".into())].into());
        cache.apply(&p);
        assert_eq!(cache.bound_intents().get("abc"), Some(&"n1".into()));
        // Succeeded → evicted from index too.
        cache.apply(&pod_phase(p.clone(), "Succeeded"));
        assert!(cache.bound_intents().is_empty());
        // delete() also evicts.
        cache.apply(&p);
        cache.delete(&p);
        assert!(cache.bound_intents().is_empty());
    }

    /// mb_034: a per-pod entry was pruned at per-node granularity, so
    /// pod-a (intent X) deleted during a watch-gap stayed in
    /// `bound_intent` as long as a sibling pod-b survived on the same
    /// node — FFD then short-circuited X's retry to a slot that's gone
    /// from `sum_for`.
    #[test]
    fn prune_absent_evicts_stale_bound_intent_with_surviving_sibling() {
        use crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION;
        let cache = PodRequestedCache::default();
        let with_intent = |name, id| {
            let mut p = pod_req(name, Some("n1"), "4", "8Gi", "10Gi");
            p.metadata.annotations = Some([(INTENT_ID_ANNOTATION.into(), id)].into());
            p
        };
        cache.apply(&with_intent("pod-a", "X".into()));
        cache.apply(&with_intent("pod-b", "Y".into()));
        // Relist sees only pod-b (pod-a deleted during watch gap).
        cache.prune_absent(&HashSet::from(["pod-b".into()]));
        let b = cache.bound_intents();
        assert_eq!(b.get("X"), None, "X's pod gone → binding pruned");
        assert_eq!(b.get("Y"), Some(&"n1".into()), "sibling Y survives");
    }

    /// A1: late delete(pod-a) for intent X must not wipe a fresh
    /// apply(pod-b, X, n2) — the retry's binding survives.
    #[test]
    fn delete_preserves_newer_binding_for_same_intent() {
        use crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION;
        let cache = PodRequestedCache::default();
        let with_intent = |name, node, id| {
            let mut p = pod_req(name, Some(node), "4", "8Gi", "10Gi");
            p.metadata.annotations = Some([(INTENT_ID_ANNOTATION.into(), id)].into());
            p
        };
        let a = with_intent("pod-a", "n1", "X".into());
        cache.apply(&a);
        cache.apply(&with_intent("pod-b", "n2", "X".into()));
        cache.delete(&a);
        assert_eq!(cache.bound_intents().get("X"), Some(&"n2".into()));
    }

    /// bug_023: a late MODIFIED for a terminating pod-a (deletionTimestamp
    /// set, phase still Running during grace) must not overwrite retry
    /// pod-b's fresh binding. The mb_034 fix guarded `delete()` by
    /// pod-name; `apply()` was the unguarded sibling. Without the
    /// `!terminating` filter, step (3) clobbers the entry back to
    /// `(pod-a,n1)`, then step (4)'s pod-a terminal-phase delete passes
    /// its own guard and removes it — leaving X unindexed and FFD's
    /// bound short-circuit blind to pod-b.
    #[test]
    fn apply_terminating_preserves_newer_binding() {
        use crate::reconcilers::pool::jobs::INTENT_ID_ANNOTATION;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        let cache = PodRequestedCache::default();
        let with_intent = |name, node, id: &str| {
            let mut p = pod_req(name, Some(node), "4", "8Gi", "10Gi");
            p.metadata.annotations = Some([(INTENT_ID_ANNOTATION.into(), id.into())].into());
            p
        };
        // (1) pod-a Running on n1.
        let mut a = with_intent("pod-a", "n1", "X");
        cache.apply(&a);
        // (2) pod-b (retry) binds n2 → bound_intent[X]=(pod-b,n2).
        cache.apply(&with_intent("pod-b", "n2", "X"));
        // (3) Late MODIFIED for pod-a: deletionTimestamp set, phase
        //     still Running (grace period). Must NOT clobber.
        a.metadata.deletion_timestamp = Some(Time(k8s_openapi::jiff::Timestamp::now()));
        cache.apply(&a);
        assert_eq!(
            cache.bound_intents().get("X"),
            Some(&"n2".into()),
            "terminating pod-a must not reclaim X from pod-b"
        );
        // (4) pod-a terminal phase → delete. Guard checks recorded pod
        //     == "pod-a"; it's "pod-b" → no-op. X stays bound to n2.
        a.status = Some(k8s_openapi::api::core::v1::PodStatus {
            phase: Some("Failed".into()),
            ..Default::default()
        });
        cache.apply(&a);
        assert_eq!(cache.bound_intents().get("X"), Some(&"n2".into()));
        // by_node still tracks pod-a's resources during grace (it holds
        // node capacity until gone) — only the intent binding is gated.
        assert_eq!(
            cache.sum_for("n1"),
            (0, 0, 0),
            "post-terminal pod-a evicted"
        );
    }
}
