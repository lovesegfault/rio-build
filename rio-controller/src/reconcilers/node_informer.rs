//! Node-label → `hw_class` resolution and spot-exposure (λ\[h\])
//! accounting.
//!
//! ADR-023 §Hardware heterogeneity: builders are air-gapped from the
//! apiserver, so they report `spec.nodeName` (downward API) and the
//! controller joins to Node labels server-side. `hw_class` is the
//! operator's `[sla.hw_classes.$h]` key whose label conjunction
//! matches the Node — fetched via [`HwClassConfig::load`]
//! (`GetHwClassConfig` RPC) so the controller stamps the SAME `$h`
//! string the scheduler's `solve_intent_for` keys on, not a hardcoded
//! 4-label reconstruction that breaks the moment an operator's label
//! schema differs (bug_061).
//!
//! # No labels cache (campaign §4(a)2)
//!
//! Node labels are NOT cached in-process. The two consumers read them
//! straight from the apiserver:
//!
//! - **Per-flush LIST** ([`run`]): the λ exposure flush LISTs Nodes
//!   once per `EXPOSURE_FLUSH_SECS` and joins labels at that point.
//! - **Per-need GET** ([`run_pod_annotator`],
//!   [`run_spot_interrupt_watcher`]): one `GET /api/v1/nodes/{name}`
//!   per pod that needs an `rio.build/hw-class` stamp (≈ one per
//!   builder pod) and per `SpotInterrupted` event (rare). Node gone
//!   at lookup → `None`, the consumer skips (degraded, not broken).
//!
//! What survives is the per-spot-node **exposure cursor** (`name →
//! last-banked epoch`, M11): an accounting position ("exposure banked
//! up to T"), not a mirror of any apiserver state — it cannot be
//! recomputed from a LIST and is owned by [`run`]'s flush loop.

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::{Event, Node, Pod};
use kube::api::{ListParams, Patch, PatchParams};
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use parking_lot::RwLock;
use tracing::{debug, info, warn};

use crate::reconcilers::{AdminClient, admin_call};

/// Pod annotation the [`run_pod_annotator`] watcher stamps with the
/// node's matched `hw_class` (the operator's `[sla.hw_classes.$h]`
/// key). Builder reads it via downward-API (`RIO_HW_CLASS`) to key its
/// `hw_perf_samples` insert.
pub const ANNOT_HW_CLASS: &str = "rio.build/hw-class";

/// `karpenter.sh/capacity-type` — `spot` or `on-demand`. The exposure
/// flush reads it off each LISTed Node so on-demand nodes contribute
/// nothing to λ.
const LABEL_CAPACITY_TYPE: &str = "karpenter.sh/capacity-type";

/// Operator-configured hw-class definitions, fetched once via
/// `GetHwClassConfig`. Shared (`Arc`) between every consumer task
/// (exposure flush, annotator, spot-interrupt watcher, the
/// nodeclaim_pool reconciler) and the [`load`](Self::load) refresh.
/// The match is lazy ([`Self::match_node`] runs per lookup against
/// freshly-read Node labels) so config arriving late still resolves
/// correctly; before load completes `hw_class = None` everywhere —
/// annotator skips, λ-samples skip.
///
/// Stored as a sorted `Vec` so [`Self::match_node`] is deterministic
/// when a Node satisfies two overlapping conjunctions (lexicographic
/// `$h` wins).
#[derive(Clone, Default)]
pub struct HwClassConfig {
    classes: Arc<RwLock<Vec<HwClassResolved>>>,
    /// §13c-3: scheduler's boot-resolved global ceiling
    /// `(max_cores, max_mem)`, shipped over `GetHwClassConfig`.
    /// `None` until the first poll lands or against a pre-§13c-3
    /// scheduler (proto zero-default → filtered out by [`Self::set`]'s
    /// `>0` gate). The controller is air-gapped (no AWS API) so this
    /// is the ONLY source for the global cap. Refreshed every 300s by
    /// `hw_refresh`.
    global: Arc<RwLock<Option<(u32, u64)>>>,
}

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
        let cfg = self.classes.read();
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
        parking_lot::RwLockReadGuard::try_map(self.classes.read(), |cfg| {
            cfg.iter().find(|d| d.name == h)
        })
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

    /// §13c-2: per-class catalog ceiling for `h` (the largest real
    /// instance type matching `h`'s `requirements`, derived
    /// scheduler-side at boot from `describe_instance_types` and
    /// folded with any per-class config override before serialize —
    /// `min(catalog, cfg)`, each falling to the global when absent).
    /// `None` if `h` is unknown, config not yet loaded, OR the loaded
    /// entry's ceilings are zero (proto default — pre-R26 scheduler
    /// that doesn't ship them; the §13c-2 scheduler always ships
    /// nonzero — `validate()` rejects global=0 and `Some(0)`
    /// overrides). The §13b `cover_deficit` builds per-cell
    /// `SizingCfg` with `min(per_class, global)` so claims chunk at
    /// the class's actual instance-type ceiling instead of the global
    /// cap. Skew window: scheduler→controller `GetHwClassConfig`
    /// refreshes every 300s, so a freshly-booted scheduler's catalog
    /// reaches here within one refresh; an uncatalogued class is
    /// over-permitted to global in that window — bounded, self-heals.
    // r[impl scheduler.sla.ceiling.controller-mirror]
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

    /// Union of `provides_features` over hw-classes carrying a taint
    /// with key `taint_key`. §Partition-single-source (r31 bug_020):
    /// a Pool's pod must tolerate the taint iff *any* of the Pool's
    /// `features` routes a drv to a tainted hw-class — that routing
    /// keys on `features_compatible(required, provides_features)`, so
    /// the toleration consumer (`pool/pod.rs::wants_metal`) reads the
    /// same `provides_features` map. Adding a feature to a tainted
    /// class's `providesFeatures` cannot re-break the toleration ⇔
    /// routing equivalence — both sides read this. Unknown `taint_key`
    /// / config not yet loaded → empty.
    pub fn features_routing_to_taint(&self, taint_key: &str) -> std::collections::HashSet<String> {
        self.classes
            .read()
            .iter()
            .filter(|d| d.taints.iter().any(|t| t.key == taint_key))
            .flat_map(|d| d.provides_features.iter().cloned())
            .collect()
    }

    /// Dedup'd union of `taints` over hw-classes carrying a taint with
    /// key `taint_key` — the dual of [`Self::features_routing_to_taint`].
    /// §Partition-single-source (r33 bug_011): the pool-static
    /// toleration consumer (`pool/pod.rs`) reads this so a future second
    /// taint on a metal class (e.g. `rio.build/secure-boot`) routes its
    /// toleration automatically — same `[sla.hw_classes.$h].taints` map
    /// `cover::build_nodeclaim` reads to taint Nodes. Unknown
    /// `taint_key` / config not yet loaded → empty (caller falls back
    /// to the literal floor, mirroring `wants_metal`'s fail-OPEN).
    pub fn taints_routing_to(&self, taint_key: &str) -> Vec<rio_proto::types::NodeTaint> {
        let mut out: Vec<rio_proto::types::NodeTaint> = Vec::new();
        for d in self
            .classes
            .read()
            .iter()
            .filter(|d| d.taints.iter().any(|t| t.key == taint_key))
        {
            for t in &d.taints {
                if !out.contains(t) {
                    out.push(t.clone());
                }
            }
        }
        out
    }

    /// `[sla.hw_classes.$h].max_fleet_cores` for `h`. §13c
    /// `cover_deficit` clamps this class's per-tick mint at
    /// `min(global_remaining, cap − live_h − created_h)`. Unknown `h`
    /// / not loaded / unset → `None` (global-only).
    pub fn fleet_cap_for(&self, h: &str) -> Option<u32> {
        self.find(h).and_then(|d| d.max_fleet_cores)
    }

    /// `[sla.hw_classes.$h].capacity_types` for `h` as controller-side
    /// [`CapacityType`](crate::reconcilers::nodeclaim_pool::CapacityType)s.
    /// §13c: `all_cells`/`fallback_cell` iterate THIS so an od-only
    /// class structurally never produces a `(h, Spot)` cell. Unknown
    /// `h` / not loaded / empty (pre-§13c scheduler) → ALL.
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
        self.classes.read().iter().map(|d| d.name.clone()).collect()
    }

    /// Whether `h`'s `kubernetes.io/arch` label equals `arch`, OR is
    /// absent (an arch-agnostic hw-class matches any arch), OR `arch`
    /// is `None` (an arch-agnostic intent — `system="builtin"` FODs —
    /// matches any class; r35 B1). `false` if `h` is unknown / config
    /// not yet loaded. §13b cold-start fallback
    /// (`NodeClaimPoolConfig::fallback_cell`) uses this to pick a
    /// reference cell for hw-agnostic intents by `intent.system`.
    /// Mirrors the scheduler's `class_routes` `Option<&str>` arch
    /// semantics so the two ends of the placement⊇provisioning
    /// invariant cannot drift.
    pub fn matches_arch(&self, h: &str, arch: Option<&str>) -> bool {
        let Some(d) = self.find(h) else {
            return false;
        };
        d.labels
            .iter()
            .find(|(k, _)| k == crate::reconcilers::nodeclaim_pool::ARCH_LABEL)
            .is_none_or(|(_, v)| arch.is_none_or(|a| v == a))
    }

    /// Replace the config wholesale from a `GetHwClassConfigResponse`.
    /// Sorted by `$h` for deterministic [`Self::match_node`] on overlap.
    /// `pub(crate)` for tests that need per-class `max_cores`/`max_mem`
    /// (which [`Self::from_literals`] doesn't carry).
    pub(crate) fn set(
        &self,
        hw_classes: HashMap<String, rio_proto::types::HwClassLabels>,
        global: (u32, u64),
    ) {
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
        *self.classes.write() = v;
        // §13c-3: proto3 zero-default → filter so a pre-§13c-3
        // scheduler (or a malformed response) yields `None`, not
        // `Some((0, 0))` which would zero every claim's cap.
        *self.global.write() = (global.0 > 0 && global.1 > 0).then_some(global);
    }

    /// §13c-3: scheduler's boot-resolved global ceiling. `None` until
    /// the first `GetHwClassConfig` poll lands, or against a
    /// pre-§13c-3 scheduler (proto zero-default → `set()` filters).
    /// `cover_deficit` skips the tick on `None` — fail-closed; no
    /// claims are minted with an unknown cap. Same skew semantics as
    /// `ceilings_for=None` (≤300s, self-heals on next refresh).
    // r[impl scheduler.sla.global.controller-mirror]
    pub fn global_ceilings(&self) -> Option<(u32, u64)> {
        *self.global.read()
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
                    let r = r.into_inner();
                    let global = (r.global_max_cores, r.global_max_mem);
                    let hw_classes = r.hw_classes;
                    let requirements_nonempty = hw_classes
                        .values()
                        .filter(|d| !d.requirements.is_empty())
                        .count();
                    info!(
                        n = hw_classes.len(),
                        requirements_nonempty,
                        global_max_cores = global.0,
                        global_max_mem = global.1,
                        "GetHwClassConfig loaded"
                    );
                    self.set(hw_classes, global);
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
        Self {
            classes: Arc::new(RwLock::new(v)),
            global: Arc::default(),
        }
    }
}

/// One λ exposure flush over a fresh Node LIST: for every spot node
/// matching a configured `$h`, bank `now − cursor` node-seconds
/// against that `hw_class` and advance the cursor; drop cursors for
/// nodes absent from the LIST. Pure — [`run`] feeds it the LIST.
///
/// Cursor (M11) discipline — what makes a 60s LIST a safe replacement
/// for the deleted Node watch + labels cache:
///
/// - **Seed**: a node first seen here starts its cursor at
///   `metadata.creationTimestamp`, so its pre-first-flush lifetime is
///   banked exactly once. Controller restart re-seeds the same way →
///   the first post-restart flush re-reports the pre-restart slice
///   once (accepted bias, unchanged from the watch-cache behavior;
///   the 24h EMA halflife dampens it).
/// - **No double-count**: the cursor advances to `now` on every flush
///   that sees the node, whether or not a `$h` matched, so
///   consecutive flushes bank disjoint slices and a late config load
///   does NOT retro-bank the unmatched window.
/// - **Absent node** (deleted between flushes): its cursor is dropped
///   WITHOUT banking the final partial slice (≤ one flush period).
///   This replaces the watch's Delete-arm residual flush — an
///   accepted under-count; λ reads marginally high, the
///   cost-conservative direction (the solver under-prefers spot;
///   never the phantom-exposure over-count that biased it toward
///   spot, bug `b81da271f`).
/// - **Aggregated per hw_class** (one RPC per class per flush, not
///   per node): a 100-node spot fleet stays one `report_exposure`
///   await per class, preserving the bug_057 loop-starvation bound.
fn flush_spot_exposure(
    cursors: &mut HashMap<String, f64>,
    nodes: &[Node],
    config: &HwClassConfig,
    now_epoch: f64,
) -> Vec<(String, f64)> {
    let mut by_hw: HashMap<String, f64> = HashMap::new();
    let mut seen: HashSet<&str> = HashSet::with_capacity(nodes.len());
    for node in nodes {
        let Some(name) = node.metadata.name.as_deref() else {
            continue;
        };
        seen.insert(name);
        let Some(labels) = node.metadata.labels.as_ref() else {
            continue;
        };
        // On-demand nodes contribute neither numerator nor denominator
        // (their λ is 0 by definition) and get no cursor.
        if labels.get(LABEL_CAPACITY_TYPE).map(String::as_str) != Some("spot") {
            continue;
        }
        let created = node
            .metadata
            .creation_timestamp
            .as_ref()
            .map(|t| t.0.as_second() as f64);
        // Cursor, else seed from creationTimestamp, else (no creation
        // timestamp — synthetic objects only) start banking from now.
        let last = cursors.get(name).copied().or(created).unwrap_or(now_epoch);
        let secs = (now_epoch - last).max(0.0);
        cursors.insert(name.to_string(), now_epoch);
        if secs > 0.0
            && let Some(hw) = config.match_node(labels)
        {
            *by_hw.entry(hw).or_default() += secs;
        }
    }
    cursors.retain(|name, _| seen.contains(name.as_str()));
    by_hw.into_iter().collect()
}

/// bug_363: `name → (hw_class, last_seen_epoch)` fallback map,
/// maintained by the 60 s exposure-flush LIST (which sees every node's
/// labels anyway) and consulted by the spot-interrupt watcher when the
/// per-need GET cannot resolve — the COMMON interrupt case is the node
/// disappearing moments after the event, which silently dropped the
/// numerator sample and biased λ LOW (toward spot) exactly when spot
/// was reclaiming. Pruned past [`HW_FALLBACK_TTL_SECS`] (2× the 3600 s
/// Event TTL bound — entries older than any Event that could still
/// reference them).
pub type HwClassFallback = std::sync::Arc<std::sync::RwLock<HashMap<String, (String, f64)>>>;

/// bug_363: prune horizon for [`HwClassFallback`].
const HW_FALLBACK_TTL_SECS: f64 = 2.0 * 3600.0;

/// bug_363: why an interrupt sample could not be attributed. The ONLY
/// consumer of a failed resolution is [`record_sample_drop`] — the
/// silent `debug!` skip is unrepresentable.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(crate) enum SampleDropReason {
    /// Node 404 and no fallback entry.
    NodeGone,
    /// Node exists (or fallback hit was possible) but no
    /// `[sla.hw_classes.$h]` matches its labels.
    NoHwClass,
    /// Node GET failed (apiserver error) and no fallback entry.
    GetError,
}

impl SampleDropReason {
    fn label(self) -> &'static str {
        match self {
            Self::NodeGone => "node_gone",
            Self::NoHwClass => "no_hw_class",
            Self::GetError => "get_error",
        }
    }
}

// r[impl ctrl.informer.interrupt-sample-conservation]
/// bug_363: the one chokepoint for dropped interrupt samples — warn +
/// counted (`rio_controller_spot_interrupt_dropped_total{reason}`), so
/// an attribution gap is operator-visible instead of a `debug!` line.
fn record_sample_drop(node: &str, reason: SampleDropReason) {
    warn!(
        %node,
        reason = reason.label(),
        "spot-interrupt: sample dropped (λ numerator under-counted)"
    );
    metrics::counter!(
        "rio_controller_spot_interrupt_dropped_total",
        "reason" => reason.label()
    )
    .increment(1);
}

/// bug_363: pure resolution decision — GET outcome × fallback. Unit
/// table in `informer_tests`; the IO wrapper below is thin.
fn classify_interrupt_resolution(
    got: Result<Option<Option<String>>, ()>,
    fallback: Option<String>,
) -> Result<String, SampleDropReason> {
    match got {
        // Node present: its labels are authoritative. No match ⇒
        // NoHwClass even if the (older) fallback knew one — config or
        // labels changed and the stale class would mis-attribute.
        Ok(Some(Some(hw))) => Ok(hw),
        Ok(Some(None)) => Err(SampleDropReason::NoHwClass),
        // Node gone / GET failed: the fallback (last LIST observation)
        // is the best remaining evidence.
        Ok(None) => fallback.ok_or(SampleDropReason::NodeGone),
        Err(()) => fallback.ok_or(SampleDropReason::GetError),
    }
}

/// Per-need Node GET + `[sla.hw_classes.$h]` match — the §4(a)2
/// replacement for the deleted labels-cache lookup. `None` if the
/// node is gone (404), the GET failed (logged), no `$h` matches, or
/// config is not yet loaded; callers skip (degraded, not broken).
async fn node_hw_class(nodes: &Api<Node>, config: &HwClassConfig, name: &str) -> Option<String> {
    match nodes.get_opt(name).await {
        Ok(Some(node)) => config.match_node(node.metadata.labels.as_ref()?),
        Ok(None) => {
            debug!(node = %name, "node gone; no hw_class");
            None
        }
        Err(e) => {
            warn!(node = %name, error = %e, "node GET failed; no hw_class");
            None
        }
    }
}

/// Run the spot-exposure flush loop + the periodic [`HwClassConfig`]
/// refresh. Returns on `shutdown.cancelled()`.
///
/// §4(a)2: replaces the Node watch + labels cache. λ's denominator
/// must include censored (still-running) observations — every
/// `EXPOSURE_FLUSH_SECS` this LISTs all Nodes and banks each spot
/// node's slice since its cursor (see `flush_spot_exposure`),
/// bounding the right-censoring bias to ≤60 node-seconds per node.
///
/// `spawn_monitored("node-informer", run(...))` from main.rs. Panics
/// are logged; the controller keeps reconciling. A failed LIST skips
/// that flush — cursors are untouched, so the next successful flush
/// banks the full delta since the last successful one (nothing lost,
/// nothing double-counted; λ samples arrive late, not wrong).
pub async fn run(
    client: Client,
    config: HwClassConfig,
    mut admin: AdminClient,
    fallback: HwClassFallback,
    shutdown: rio_common::signal::Token,
) {
    let nodes: Api<Node> = Api::all(client);

    // M11: per-spot-node exposure cursor (`name → last-banked epoch`).
    // An accounting position, not a cache — survives the §4(a)2
    // labels-cache deletion and is private to this loop.
    let mut cursors: HashMap<String, f64> = HashMap::new();

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

    info!("Node informer started (per-flush LIST, no watch)");

    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("Node informer: shutdown");
                return;
            }
            _ = flush.tick() => {
                match nodes.list(&ListParams::default()).await {
                    Ok(list) => {
                        // bug_363: refresh the interrupt watcher's
                        // hw_class fallback from the same LIST (every
                        // node's labels are in hand anyway); prune
                        // entries past the TTL.
                        {
                            let now = now_epoch();
                            let mut fb = fallback.write().expect("fallback map lock");
                            for node in &list.items {
                                if let Some(name) = node.metadata.name.as_deref()
                                    && let Some(labels) = node.metadata.labels.as_ref()
                                    && let Some(hw) = config.match_node(labels)
                                {
                                    fb.insert(name.to_owned(), (hw, now));
                                }
                            }
                            fb.retain(|_, (_, seen)| now - *seen < HW_FALLBACK_TTL_SECS);
                        }
                        for (hw, secs) in
                            flush_spot_exposure(&mut cursors, &list.items, &config, now_epoch())
                        {
                            report_exposure(&mut admin, hw, secs).await;
                        }
                    }
                    Err(e) => {
                        warn!(error = %e, "node LIST failed; exposure flush skipped this round");
                    }
                }
            }
            _ = hw_refresh.tick() => {
                config.load(&mut admin).await;
            }
        }
    }
}

/// Live spot-node exposure flush cadence (seconds). See
/// `flush_spot_exposure`.
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
/// §4(a)2: the node's labels come from a per-need GET
/// (`node_hw_class`), not a cache. The GET happens only AFTER the
/// pure `annotation_target` pre-check passes, so the apiserver cost
/// is one GET per pod that actually needs a stamp (≈ one per builder
/// pod lifetime), not one per watch event.
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
    config: HwClassConfig,
    shutdown: rio_common::signal::Token,
) {
    let pods: Api<Pod> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client.clone());
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
        // Pure pre-check first: no apiserver round-trip for pods that
        // are already stamped or not yet scheduled.
        let Some((name, ns, node_name)) = annotation_target(&pod) else {
            continue;
        };
        // Per-need GET; node gone / unmatched / config unloaded → skip
        // (don't stamp a bogus value).
        let Some(hw) = node_hw_class(&nodes, &config, &node_name).await else {
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
/// we watch the Node event so `involvedObject.name` is the Node
/// `metadata.name`), resolve the referenced node's hw_class via a
/// per-need GET (`node_hw_class`, §4(a)2 — interrupts are rare, so
/// one GET per event is negligible), and append
/// `interrupt_samples(hw_class, kind='interrupt', value=1)` via
/// `AdminService.AppendInterruptSample`.
///
/// The exposure half (`kind='exposure', value=node_seconds`) is
/// emitted from [`run`]: a periodic flush banks live node-seconds
/// every `EXPOSURE_FLUSH_SECS`. Live nodes MUST contribute — counting
/// only completed lifetimes is the right-censoring bias that spikes
/// λ at burst onset.
///
/// `field_selector` narrows the watch server-side so we don't churn
/// on the cluster's full event firehose. Karpenter's interruption
/// controller emits the `SpotInterrupted` event on BOTH the NodeClaim
/// and the Node; we watch the **Node** event so `involvedObject.name`
/// is the Node `metadata.name` — the name the GET resolves. The
/// NodeClaim event's `involvedObject.name` is the NodeClaim name
/// (`{nodepool}-{hash}` on EKS), which is not a Node name.
pub async fn run_spot_interrupt_watcher(
    client: Client,
    config: HwClassConfig,
    mut admin: AdminClient,
    fallback: HwClassFallback,
    shutdown: rio_common::signal::Token,
) {
    let events: Api<Event> = Api::all(client.clone());
    let nodes: Api<Node> = Api::all(client);
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
        // metadata.name.
        let Some(node) = ev.involved_object.name else {
            continue;
        };
        // bug_363: resolution returns Result — the Err arm's only
        // consumer is the counted recorder; a silent skip no longer
        // typechecks as "handled".
        let got = match nodes.get_opt(&node).await {
            Ok(Some(n)) => Ok(Some(
                n.metadata
                    .labels
                    .as_ref()
                    .and_then(|labels| config.match_node(labels)),
            )),
            Ok(None) => Ok(None),
            Err(e) => {
                debug!(%node, error = %e, "spot-interrupt: node GET failed");
                Err(())
            }
        };
        let fb = fallback
            .read()
            .expect("fallback map lock")
            .get(&node)
            .map(|(hw, _)| hw.clone());
        let hw_class = match classify_interrupt_resolution(got, fb) {
            Ok(hw) => hw,
            Err(reason) => {
                record_sample_drop(&node, reason);
                continue;
            }
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
/// informer's flush loop (every caller is inside that loop).
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

/// `Some((pod_name, namespace, node_name))` if `pod` is a candidate
/// for the `rio.build/hw-class` stamp: scheduled (`spec.nodeName`
/// set) and not already annotated. Pure pre-check — the per-need
/// node GET (`node_hw_class`) runs only after this passes, so
/// already-stamped and Pending pods cost no apiserver round-trip.
fn annotation_target(pod: &Pod) -> Option<(String, String, String)> {
    let already = pod
        .metadata
        .annotations
        .as_ref()
        .is_some_and(|a| a.contains_key(ANNOT_HW_CLASS));
    if already {
        return None;
    }
    let node = pod.spec.as_ref()?.node_name.clone()?;
    let name = pod.metadata.name.clone()?;
    let ns = pod.metadata.namespace.clone()?;
    Some((name, ns, node))
}

#[cfg(test)]
mod tests {

    // r[verify ctrl.informer.interrupt-sample-conservation]
    /// bug_363's resolution table: present-node labels are
    /// authoritative; a gone/unreadable node falls back to the flush
    /// map; only a genuinely unattributable event drops — and the drop
    /// is TYPED (the silent debug!-skip shape no longer exists; the
    /// recorded pre-fix red: a 404'd node's sample was skipped even
    /// though the 60 s flush had just observed its hw_class).
    #[test]
    fn interrupt_resolution_classifies_every_cell() {
        use super::{SampleDropReason, classify_interrupt_resolution};
        let hw = || Some("mid-ebs-x86".to_string());

        // Present + match ⇒ attributed (fallback irrelevant).
        assert_eq!(
            classify_interrupt_resolution(Ok(Some(hw())), None),
            Ok("mid-ebs-x86".into())
        );
        // Present + no match ⇒ NoHwClass even with a stale fallback
        // (labels/config changed; the old class would mis-attribute).
        assert_eq!(
            classify_interrupt_resolution(Ok(Some(None)), hw()),
            Err(SampleDropReason::NoHwClass)
        );
        // Gone + fallback ⇒ attributed via the flush map (the common
        // reclaim case — the node is deleted moments after the Event).
        assert_eq!(
            classify_interrupt_resolution(Ok(None), hw()),
            Ok("mid-ebs-x86".into())
        );
        // Gone + no fallback ⇒ typed drop.
        assert_eq!(
            classify_interrupt_resolution(Ok(None), None),
            Err(SampleDropReason::NodeGone)
        );
        // GET error + fallback ⇒ attributed; without ⇒ typed drop.
        assert_eq!(
            classify_interrupt_resolution(Err(()), hw()),
            Ok("mid-ebs-x86".into())
        );
        assert_eq!(
            classify_interrupt_resolution(Err(()), None),
            Err(SampleDropReason::GetError)
        );
    }
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

    fn labels_of(node: &Node) -> BTreeMap<String, String> {
        node.metadata.labels.clone().unwrap_or_default()
    }

    #[test]
    fn matches_arch_absent_label_is_agnostic() {
        use crate::reconcilers::nodeclaim_pool::ARCH_LABEL;
        let cfg = HwClassConfig::from_literals(&[
            ("x86", &[(ARCH_LABEL, "amd64"), ("k", "v")]),
            ("arm", &[(ARCH_LABEL, "arm64")]),
            ("agnostic", &[("k", "v")]),
        ]);
        assert!(cfg.matches_arch("x86", Some("amd64")));
        assert!(!cfg.matches_arch("x86", Some("arm64")));
        assert!(cfg.matches_arch("arm", Some("arm64")));
        // No arch label → matches any.
        assert!(cfg.matches_arch("agnostic", Some("amd64")));
        assert!(cfg.matches_arch("agnostic", Some("arm64")));
        // r35 B1: arch=None (arch-unmappable intent — `builtin` FOD) →
        // matches any class. The intent is arch-agnostic; the class's
        // arch label is irrelevant.
        assert!(cfg.matches_arch("x86", None));
        assert!(cfg.matches_arch("arm", None));
        assert!(cfg.matches_arch("agnostic", None));
        // Unknown h → false (even with arch=None — unknown class is
        // never a candidate).
        assert!(!cfg.matches_arch("nope", Some("amd64")));
        assert!(!cfg.matches_arch("nope", None));
        assert!(!HwClassConfig::default().matches_arch("x86", Some("amd64")));
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

    /// The label match every per-need GET and per-flush LIST runs: a
    /// node's labels resolve to the operator's `$h` key; an unmatched
    /// label set resolves to `None`. Successor of the deleted cache's
    /// `hw_class_of_matches_operator_config` — the lookup-by-name half
    /// is gone with the cache (the GET reads the apiserver directly).
    #[test]
    fn match_node_matches_operator_config() {
        let cfg = band_config();
        assert_eq!(
            cfg.match_node(&labels_of(&node(
                "ip-10-0-1-5",
                &[("rio.build/hw-band", "7"), ("unrelated", "x")],
            ))),
            Some("intel-7".into())
        );
        assert_eq!(cfg.match_node(&BTreeMap::new()), None);
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
        assert_eq!(
            cfg.match_node(&labels_of(&node(
                "full",
                &[
                    ("karpenter.k8s.aws/instance-cpu-manufacturer", "amd"),
                    ("rio.build/storage", "nvme"),
                    ("karpenter.k8s.aws/instance-generation", "6"),
                ],
            ))),
            Some("amd-nvme-mid".into())
        );
        // Partial match (one label of the conjunction missing) → None.
        assert_eq!(
            cfg.match_node(&labels_of(&node(
                "partial",
                &[("karpenter.k8s.aws/instance-cpu-manufacturer", "amd")],
            ))),
            None
        );
    }

    /// Unloaded config → `match_node` returns `None` (annotator skips,
    /// λ-samples skip — degraded, not broken). Once config loads, the
    /// SAME labels resolve — and because every per-need GET / per-flush
    /// LIST re-matches against current config, there is no stale-cache
    /// window to invalidate (successor of the deleted cache's
    /// lazy-match test).
    #[test]
    fn match_none_until_config_loaded() {
        let cfg = HwClassConfig::default();
        let labels = labels_of(&node("n", &[("rio.build/hw-band", "7")]));
        assert_eq!(cfg.match_node(&labels), None, "config empty → no match");
        // Config arrives: the same labels now resolve.
        cfg.set(
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
            (192, 1536 << 30),
        );
        assert_eq!(cfg.match_node(&labels), Some("intel-7".into()));
        assert_eq!(cfg.ceilings_for("intel-7"), Some((64, 256 << 30)));
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
            (192, 1536 << 30),
        );
        assert_eq!(cfg.ceilings_for("arm"), Some((64, 128 << 30)));
        assert_eq!(cfg.ceilings_for("old"), None, "zero ceilings → None");
        assert_eq!(cfg.ceilings_for("nope"), None, "unknown → None");
    }

    /// §13c-3 RED-FIRST: `global_ceilings()` returns the resolved
    /// global from `set()`, `None` until set, `None` on the proto
    /// zero-default (pre-§13c-3 scheduler).
    // r[verify scheduler.sla.global.controller-mirror]
    #[test]
    fn global_ceilings_set_and_filter() {
        let cfg = HwClassConfig::default();
        assert_eq!(cfg.global_ceilings(), None, "before first poll → None");

        cfg.set(Default::default(), (64, 256 << 30));
        assert_eq!(
            cfg.global_ceilings(),
            Some((64, 256 << 30)),
            "set() with non-zero global → Some"
        );

        // Pre-§13c-3 scheduler ships proto zero-default → filtered.
        cfg.set(Default::default(), (0, 0));
        assert_eq!(
            cfg.global_ceilings(),
            None,
            "proto zero-default (old scheduler) → None"
        );
        cfg.set(Default::default(), (64, 0));
        assert_eq!(cfg.global_ceilings(), None, "half-zero → None");
    }

    /// Overlapping conjunctions → deterministic (lexicographic `$h`).
    #[test]
    fn match_node_deterministic_on_overlap() {
        let cfg = HwClassConfig::from_literals(&[("zz", &[("k", "v")]), ("aa", &[("k", "v")])]);
        let labels: BTreeMap<_, _> = [("k".into(), "v".into())].into();
        assert_eq!(cfg.match_node(&labels), Some("aa".into()));
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

    fn od_node(name: &str, created: i64) -> Node {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        use k8s_openapi::jiff::Timestamp;
        let mut n = node(
            name,
            &[
                (LABEL_CAPACITY_TYPE, "on-demand"),
                ("rio.build/hw-band", "7"),
            ],
        );
        n.metadata.creation_timestamp = Some(Time(Timestamp::from_second(created).unwrap()));
        n
    }

    fn sorted(mut v: Vec<(String, f64)>) -> Vec<(String, f64)> {
        v.sort_by(|a, b| a.0.cmp(&b.0));
        v
    }

    /// §4(a)2 gate (M11 no-double-count): right-censoring fix carried
    /// over from the watch cache — live spot nodes contribute exposure
    /// on every flush, each flush banks only the delta since the
    /// previous one (cursor advance), and on-demand nodes contribute
    /// nothing. Port of the deleted cache's
    /// `drain_live_spot_exposure_banks_incremental_deltas`.
    #[test]
    fn flush_banks_incremental_deltas() {
        let cfg = band_config();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let nodes = vec![
            spot_node("a", "7", 1000),
            spot_node("b", "7", 1000),
            spot_node("c", "6", 1020),
            // On-demand node: must NOT appear in flush output.
            od_node("od", 1000),
        ];

        // First flush at t=1060: cursors seed from creationTimestamp →
        // a+b → 2×60=120s for intel-7, c → 40s.
        let d = sorted(flush_spot_exposure(&mut cursors, &nodes, &cfg, 1060.0));
        assert_eq!(d, vec![("intel-6".into(), 40.0), ("intel-7".into(), 120.0)]);

        // Second flush at t=1120 over the SAME LIST: deltas only (60s
        // each), not cumulative-from-created — the cursor advanced.
        let d = sorted(flush_spot_exposure(&mut cursors, &nodes, &cfg, 1120.0));
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 120.0)]);

        // On-demand node never grew a cursor.
        assert!(!cursors.contains_key("od"));
    }

    /// §4(a)2 gate (capacity-type / match gating): spot nodes whose
    /// labels match no configured `$h` advance their cursor WITHOUT
    /// banking — a late config load does not retro-bank the unmatched
    /// window (same semantics as the deleted cache). Port of
    /// `spot_exposure_gates_on_capacity_type`'s config-empty half.
    #[test]
    fn flush_unmatched_window_is_dropped_not_retro_banked() {
        let unloaded = HwClassConfig::default();
        let loaded = band_config();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let nodes = vec![spot_node("a", "7", 1000)];

        // Flush at t=1060 with config not yet loaded: nothing banked,
        // but the cursor still advances to 1060.
        let d = flush_spot_exposure(&mut cursors, &nodes, &unloaded, 1060.0);
        assert!(d.is_empty(), "unmatched spot node banks nothing");
        assert_eq!(cursors.get("a").copied(), Some(1060.0));

        // Config loads; flush at t=1120 banks ONLY 1060..1120 — the
        // unmatched 1000..1060 window is dropped, not retro-banked.
        let d = flush_spot_exposure(&mut cursors, &nodes, &loaded, 1120.0);
        assert_eq!(d, vec![("intel-7".into(), 60.0)]);
    }

    /// §4(a)2 gate (the recorded accepted under-count): a node absent
    /// from the LIST has its cursor dropped WITHOUT banking the final
    /// partial slice. This is the deleted watch cache's Delete-arm /
    /// `prune_absent` residual flush, deliberately forfeited (≤ one
    /// flush period per node, λ reads marginally high — the
    /// cost-conservative direction). Successor of
    /// `prune_absent_evicts_nodes_missing_from_relist`, with the
    /// assertion inverted to match the recorded semantics change.
    #[test]
    fn flush_drops_absent_node_cursors_without_banking() {
        let cfg = band_config();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let all = vec![
            spot_node("a", "7", 1000),
            spot_node("b", "7", 1000),
            spot_node("b2", "7", 1000),
            spot_node("c", "6", 1020),
            od_node("od", 1000),
        ];

        // Flush at t=1060 sees everything; all cursors advance.
        let _ = flush_spot_exposure(&mut cursors, &all, &cfg, 1060.0);
        assert_eq!(cursors.len(), 4, "four spot nodes tracked");

        // b, b2, od deleted between flushes → the t=1090 LIST has only
        // {a, c}. Their 1060..1090 residuals are forfeited (NOT in the
        // output) and their cursors are dropped.
        let survivors = vec![spot_node("a", "7", 1000), spot_node("c", "6", 1020)];
        let d = sorted(flush_spot_exposure(&mut cursors, &survivors, &cfg, 1090.0));
        assert_eq!(
            d,
            vec![("intel-6".into(), 30.0), ("intel-7".into(), 30.0)],
            "only survivors bank; absent nodes' residuals are forfeited"
        );
        assert_eq!(cursors.len(), 2);
        assert!(!cursors.contains_key("b"));
        assert!(!cursors.contains_key("b2"));

        // Next flush at t=1120: still only the survivors — no phantom
        // node-seconds from the departed nodes (the b81da271f hazard
        // the watch needed `prune_absent` for cannot exist here).
        let d = sorted(flush_spot_exposure(&mut cursors, &survivors, &cfg, 1120.0));
        assert_eq!(d, vec![("intel-6".into(), 30.0), ("intel-7".into(), 30.0)]);
    }

    /// A re-listed node (same LIST contents, later flush) must NOT
    /// re-seed its cursor from creationTimestamp — the cursor map is
    /// the no-double-count authority (M11). Port of the re-apply half
    /// of the deleted cache's incremental-deltas test.
    #[test]
    fn flush_preserves_cursor_across_relists() {
        let cfg = band_config();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let nodes = vec![spot_node("a", "7", 1000)];
        let d = flush_spot_exposure(&mut cursors, &nodes, &cfg, 1060.0);
        assert_eq!(d, vec![("intel-7".into(), 60.0)]);
        // Same node, fresh LIST objects (a relist): banks 30s, not 110s.
        let relisted = vec![spot_node("a", "7", 1000)];
        let d = flush_spot_exposure(&mut cursors, &relisted, &cfg, 1090.0);
        assert_eq!(d, vec![("intel-7".into(), 30.0)]);
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

    /// The pure pre-check that gates the per-need GET: stamp candidates
    /// are scheduled, not-yet-annotated pods. The "node missing" and
    /// "no `$h` matches" skip arms moved into `node_hw_class` (the
    /// GET half) — their match logic is covered by the `match_node`
    /// tests above. Successor of the deleted `patch_target_stamps_once`.
    #[test]
    fn annotation_target_stamps_once() {
        // Scheduled, not yet annotated → candidate (carries node name
        // for the GET).
        let p = pod("rb-abc", "rio", Some("ip-10-0-1-5"), &[]);
        assert_eq!(
            annotation_target(&p),
            Some(("rb-abc".into(), "rio".into(), "ip-10-0-1-5".into()))
        );
        // Already annotated → skip (sticky), costing no GET.
        let p = pod(
            "rb-abc",
            "rio",
            Some("ip-10-0-1-5"),
            &[(ANNOT_HW_CLASS, "intel-7")],
        );
        assert_eq!(annotation_target(&p), None);
        // Pending (no nodeName yet) → skip, costing no GET.
        let p = pod("rb-pending", "rio", None, &[]);
        assert_eq!(annotation_target(&p), None);
    }
}
