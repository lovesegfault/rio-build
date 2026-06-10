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

use std::collections::{BTreeMap, HashMap, HashSet, VecDeque};
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
    /// (which `Self::from_literals` doesn't carry).
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
                    // merged_bug_236: birth the reasons × cells reaped
                    // series for the by-(cell) ICE alert — the cell
                    // axis is config-derived so it seeds here, on every
                    // load/refresh (absolute(0) is idempotent).
                    crate::observability::seed_reaped_cells(self.names().into_iter().flat_map(
                        |h| {
                            self.capacity_types_for(&h).into_iter().map(move |c| {
                                crate::reconcilers::nodeclaim_pool::Cell(h.clone(), c).to_string()
                            })
                        },
                    ));
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

/// One flush's full settlement (merged_bug_070): every node-second
/// that leaves a cursor exits through exactly one of BANK (`banked`,
/// headed for the append path), PENDING (the caller's retained
/// queue), or a COUNTED drop (`drops` — recorded by the caller via
/// [`record_exposure_drop`]). The pre-fix shape had two silent exits
/// (match-None consumption, absent-node forfeiture) that the leg's
/// own conservation claim said could not exist.
struct ExposureFlush {
    banked: Vec<(String, f64)>,
    drops: Vec<(ExposureDropReason, f64)>,
}

/// One λ exposure flush over a fresh Node LIST: for every spot node
/// matching a configured `$h`, bank `now − cursor` node-seconds
/// against that `hw_class` and advance the cursor; drop cursors for
/// nodes absent from the LIST. Pure — [`run`] feeds it the LIST.
///
/// Cursor (M11) discipline — what makes a 60s LIST a safe replacement
/// for the deleted Node watch + labels cache:
///
/// - **Seed** (merged_bug_070): a node first seen here starts its
///   cursor at `max(creationTimestamp, boot_epoch)` — creation time
///   for nodes born under this incarnation (pre-first-flush lifetime
///   banked exactly once), process boot for nodes that predate it. A
///   controller restart therefore CANNOT re-bank windows the previous
///   incarnation already shipped (pre-fix it re-banked every
///   surviving node's whole lifetime, un-keyed → denominator
///   inflation, λ biased LOW — the anti-conservative direction). The
///   pre-boot residual [last-shipped, boot] is forfeited to the
///   conservative side (λ reads marginally high).
/// - **No double-count**: the cursor advances to `now` on every flush
///   that sees the node, whether or not a `$h` matched, so
///   consecutive flushes bank disjoint slices and a late config load
///   does NOT retro-bank the unmatched window. The unmatched window
///   itself is a COUNTED `no_hw_class` drop (merged_bug_070 — it used
///   to vanish without a trace).
/// - **Absent node** (deleted between flushes): its cursor is dropped
///   without banking the final partial slice — a COUNTED
///   `absent_node` drop (merged_bug_070). This replaces the watch's
///   Delete-arm residual flush — an accepted under-count; λ reads
///   marginally high, the cost-conservative direction (the solver
///   under-prefers spot; never the phantom-exposure over-count that
///   biased it toward spot, bug `b81da271f`). The forfeit is one
///   flush period per node in the common case but grows with the gap
///   since the last successful LIST (a node deleted during an
///   N-window LIST-failure streak forfeits up to N windows) — the
///   counter makes that tail visible instead of pretending a bound.
/// - **Aggregated per hw_class** (one RPC per class per flush, not
///   per node): a 100-node spot fleet stays one `report_exposure`
///   await per class, preserving the bug_057 loop-starvation bound.
fn flush_spot_exposure(
    cursors: &mut HashMap<String, f64>,
    nodes: &[Node],
    config: &HwClassConfig,
    boot_epoch: f64,
    now_epoch: f64,
) -> ExposureFlush {
    let mut by_hw: HashMap<String, f64> = HashMap::new();
    let mut drops: Vec<(ExposureDropReason, f64)> = Vec::new();
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
        // Cursor, else seed at max(creationTimestamp, boot) (see the
        // Seed bullet), else (no creation timestamp — synthetic
        // objects only) start banking from now.
        let last = cursors
            .get(name)
            .copied()
            .or(created.map(|c| c.max(boot_epoch)))
            .unwrap_or(now_epoch);
        let secs = (now_epoch - last).max(0.0);
        cursors.insert(name.to_string(), now_epoch);
        if secs > 0.0 {
            match config.match_node(labels) {
                Some(hw) => *by_hw.entry(hw).or_default() += secs,
                // merged_bug_070(a): the cursor consumed this window
                // (by design — no retro-bank) so the seconds MUST
                // exit counted, not vanish.
                None => drops.push((ExposureDropReason::NoHwClass, secs)),
            }
        }
    }
    // merged_bug_070(c): absent nodes forfeit their final partial
    // slice — counted per node before the cursor drop.
    for (name, last) in cursors.iter() {
        if !seen.contains(name.as_str()) {
            let secs = (now_epoch - last).max(0.0);
            if secs > 0.0 {
                drops.push((ExposureDropReason::AbsentNode, secs));
            }
        }
    }
    cursors.retain(|name, _| seen.contains(name.as_str()));
    ExposureFlush {
        banked: by_hw.into_iter().collect(),
        drops,
    }
}

/// bug_363: `name → (hw_class, last_seen_epoch)` fallback map,
/// maintained by the 60 s exposure-flush LIST (which sees every node's
/// labels anyway) and consulted by the spot-interrupt watcher when the
/// per-need GET cannot resolve — the COMMON interrupt case is the node
/// disappearing moments after the event, which silently dropped the
/// numerator sample and biased λ LOW (toward spot) exactly when spot
/// was reclaiming. Pruned past `HW_FALLBACK_TTL_SECS` (2× the 3600 s
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
    /// Attribution succeeded but the `AppendInterruptSample` RPC
    /// failed (merged_bug_116): the sample existed and was lost in
    /// delivery — without this arm the conservation identity
    /// `observed = appended + Σ dropped{reason}` was false exactly
    /// when the scheduler was unreachable.
    AppendFailed,
}

impl SampleDropReason {
    fn label(self) -> &'static str {
        match self {
            Self::NodeGone => "node_gone",
            Self::NoHwClass => "no_hw_class",
            Self::GetError => "get_error",
            Self::AppendFailed => "append_failed",
        }
    }
}

/// merged_bug_070: why exposure node-seconds left a cursor without
/// being banked or queued. The ONLY consumer is
/// [`record_exposure_drop`] — a silent exit of the denominator leg is
/// unrepresentable (the numerator leg got the same treatment in
/// bug_363).
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ExposureDropReason {
    /// A spot node's labels match no configured `[sla.hw_classes.$h]`
    /// — the cursor advances by design (no retro-bank on late config
    /// load), so the window is consumed and must exit counted.
    NoHwClass,
    /// Node deleted between flushes: its final partial slice is
    /// forfeited with the cursor (the accepted under-count; grows
    /// past one flush period only across LIST-failure streaks).
    AbsentNode,
    /// Process shutdown: the pending (un-acked) queue is process
    /// memory with no drain — the WHOLE backlog is forfeited, one
    /// counted drop per slice.
    Shutdown,
    /// The slice's permanent counted exit ([`classify_append_status`]
    /// plus the strike budget): either the scheduler refused the
    /// slice's CONTENT or shape (request-disproving — re-sending the
    /// same bytes cannot succeed, exits in the observing pass) or repeated
    /// presentation-judging auth refusals exhausted the typed
    /// observation budget ([`AUTH_STRIKE_BUDGET`], merged_bug_013 — a
    /// persistent token misconfig exits counted and warn-disclosed at
    /// strike N, never on one rotation-skew observation). Counted
    /// here instead of recirculating forever (merged_bug_001-r6:
    /// pre-fix a permanently-refused slice was re-queued verbatim
    /// every pass, reported "pending" while de-facto dropped, and
    /// wedged every pass into budget exhaustion).
    Refused,
}

impl ExposureDropReason {
    fn label(self) -> &'static str {
        match self {
            Self::NoHwClass => "no_hw_class",
            Self::AbsentNode => "absent_node",
            Self::Shutdown => "shutdown",
            Self::Refused => "refused",
        }
    }
}

// r[impl ctrl.informer.exposure-recredit+4]
/// merged_bug_070: the one chokepoint for forfeited exposure
/// node-seconds — warn + counted
/// (`rio_controller_spot_exposure_dropped_seconds_total{reason}`,
/// incremented by whole seconds, sub-second residue rounds), the
/// denominator twin of [`record_sample_drop`]. Together they make the
/// leg's conservation identity total: every observed node-second is
/// banked, pending, or counted here — the pre-fix silent exits
/// (match-None consumption, absent-node forfeiture, whole-backlog
/// shutdown loss) are unrepresentable, and the refused exit
/// (merged_bug_001-r6) rides the same chokepoint so PENDING provably
/// means "deliverable by some future pass".
fn record_exposure_drop(reason: ExposureDropReason, secs: f64) {
    warn!(
        reason = reason.label(),
        secs, "spot-exposure: node-seconds forfeited (λ denominator under-counted)"
    );
    metrics::counter!(
        "rio_controller_spot_exposure_dropped_seconds_total",
        "reason" => reason.label()
    )
    .increment(secs.round().max(0.0) as u64);
}

// r[impl ctrl.informer.interrupt-sample-conservation+2]
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

/// merged_bug_116: an attributed spot-interrupt sample — constructible
/// ONLY from the classify-Ok path (the field is module-private and the
/// sole producer is [`attribute_interrupt`]), so every sample that
/// exists either appends or exits through the counted drop chokepoint.
pub(crate) struct InterruptSample {
    hw_class: String,
    event_uid: Option<String>,
}

/// The classify-Ok constructor: resolution outcome × fallback →
/// a deliverable sample or a typed drop.
fn attribute_interrupt(
    got: Result<Option<Option<String>>, ()>,
    fallback: Option<String>,
    event_uid: Option<String>,
) -> Result<InterruptSample, SampleDropReason> {
    classify_interrupt_resolution(got, fallback).map(|hw_class| InterruptSample {
        hw_class,
        event_uid,
    })
}

// r[impl ctrl.informer.interrupt-sample-conservation+2]
/// The ONLY interrupt-sample appender (merged_bug_116): a delivery
/// failure routes through [`record_sample_drop`] with
/// [`SampleDropReason::AppendFailed`] — NO sample exit exists outside
/// the counted chokepoint, making the conservation identity
/// `observed = appended + Σ dropped{reason}` true over delivery too.
async fn deliver_interrupt_sample(admin: &mut AdminClient, node: &str, sample: InterruptSample) {
    let InterruptSample {
        hw_class,
        event_uid,
    } = sample;
    let r = admin_call(admin.append_interrupt_sample(
        rio_proto::types::AppendInterruptSampleRequest {
            hw_class: hw_class.clone(),
            kind: "interrupt".into(),
            value: 1.0,
            event_uid,
        },
    ))
    .await;
    match r {
        Ok(_) => debug!(%node, %hw_class, "spot-interrupt: sample appended"),
        Err(e) => {
            warn!(%node, error = %e, "spot-interrupt: append failed");
            record_sample_drop(node, SampleDropReason::AppendFailed);
        }
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
/// nothing double-counted; λ samples arrive late, not wrong). A
/// non-advancing flush window (`WindowGate::admit` → `None`; the gate
/// is module-private) is the same posture: banking deferred, cursors
/// untouched, nothing forfeited.
///
/// bug_022 (`ctrl.informer.cluster-identity-boundary`): startup
/// discloses the cluster identity axis — the single-cluster default
/// warns loudly (shared-PG absorption hazard), a non-empty id logs
/// positively. main.rs stays the ONE `cfg.cluster` read site; the
/// disclosure lives here because the informer is the exposure path's
/// activation.
pub async fn run(
    client: Client,
    config: HwClassConfig,
    mut admin: AdminClient,
    fallback: HwClassFallback,
    cluster: ClusterId,
    shutdown: rio_common::signal::Token,
) {
    // bug_022: activation disclosure FIRST — before any flush can
    // mint a uid under an undisclosed identity.
    disclose_cluster_identity(&cluster);

    let nodes: Api<Node> = Api::all(client);

    // merged_bug_070: process-boot epoch — the restart re-bank fence.
    // Cursor seeds clamp to this so a restarted controller never
    // re-banks windows its predecessor already shipped.
    let boot_epoch = now_epoch();

    // M11: per-spot-node exposure cursor (`name → last-banked epoch`).
    // An accounting position, not a cache — survives the §4(a)2
    // labels-cache deletion and is private to this loop.
    let mut cursors: HashMap<String, f64> = HashMap::new();
    // bug_150 + merged_bug_002 + merged_bug_001: per-(class, window)
    // slices whose append has not been ACKNOWLEDGED — the cursor
    // already advanced, so this queue is the only carrier of those
    // windows until a flush delivers them (consume-on-ack). Each
    // slice keeps its own deterministic
    // `exposure:{cluster}:{hw}:{window-slot}` [`EventUid`] across
    // retries so a commit-but-timeout redelivery dedups server-side
    // (`ON CONFLICT (event_uid)`, M_047) instead of double-banking
    // λ's denominator; the cluster axis makes cross-cluster absorbs
    // unconstructible in the shared-PG topology (ADR-023 §2.13), and
    // the grid-aligned slot makes same-cluster co-run twins CONVERGE
    // on one uid per logical window (the absorb becomes designed
    // at-most-once, not silent loss); windows are NEVER merged (a
    // merged value under an already-committed uid would be absorbed
    // and the fresh half lost).
    //
    // merged_bug_033: deliberately UNCAPPED. Memory is ~|H| slices
    // per minute (tens of bytes each — one per configured hw_class
    // per failed window), and a cap would mint a NEW forfeiture edge
    // the spec's counted-forfeiture enumeration does not name. The
    // drain is bounded per pass by [`ship_all`]'s one-rotation law
    // (each queued slice attempted at most once per pass), never by
    // dropping retriable slices — the only in-combinator exit is the
    // COUNTED `refused` drop for slices whose append provably cannot
    // succeed (merged_bug_001-r6).
    let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();

    // merged_bug_001: per-process window gate — window identity is
    // strictly monotone, so a clock step backward (or a same-slot
    // double tick) defers banking instead of re-minting an already-
    // shipped window under fresh seconds (absorbed → lost).
    let mut gate = WindowGate::default();

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
                // merged_bug_070(b): the pending queue is process
                // memory — shutdown forfeits the WHOLE backlog (one
                // window per failed flush per class; there is no
                // single-window bound). Counted per slice so the loss
                // is operator-visible and the spec's forfeiture
                // enumeration stays honest.
                for slice in unshipped.drain(..) {
                    record_exposure_drop(ExposureDropReason::Shutdown, slice.secs);
                }
                debug!("Node informer: shutdown");
                return;
            }
            _ = flush.tick() => {
                // merged_bug_033 arm-await sweep: no select-arm await
                // outlasts cancellation — the LIST races shutdown so
                // a hung apiserver cannot pin the loop past SIGTERM.
                match shutdown.run_until_cancelled(nodes.list(&ListParams::default())).await {
                    // Cancelled mid-LIST: nothing banked, nothing
                    // forfeited; the biased shutdown arm runs (and
                    // discloses the backlog) on the next iteration.
                    None => {}
                    Some(Ok(list)) => {
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
                        // r[impl ctrl.informer.exposure-recredit+4]
                        let now = now_epoch();
                        match gate.admit(now) {
                            Some(window) => {
                                let flush_out = flush_spot_exposure(
                                    &mut cursors, &list.items, &config, boot_epoch, now,
                                );
                                for (reason, secs) in flush_out.drops {
                                    record_exposure_drop(reason, secs);
                                }
                                queue_exposure_slices(
                                    &mut unshipped, flush_out.banked, &cluster, window,
                                );
                            }
                            // Banking deferred — LIST-failure-equivalent:
                            // cursors untouched (the next admitted window
                            // banks the full delta), the fallback refresh
                            // above already ran, and the retained queue
                            // below still ships. Nothing is forfeited; λ
                            // samples arrive late, not reduced.
                            None => {
                                warn!(
                                    now,
                                    "exposure flush window has not advanced; \
                                     banking deferred (cursors untouched)"
                                );
                            }
                        }
                        // Ship every pending slice independently —
                        // retained windows retry alongside this
                        // flush's fresh ones, each under its own uid.
                        // The closure is the ONLY production ship
                        // path (the former report_exposure body,
                        // folded in so no shipment exists outside the
                        // ship_all combinator): one
                        // AppendInterruptSample attempt per slice PER
                        // PASS (the one-rotation law), the outcome
                        // typed by [`classify_append_status`] —
                        // `Delivered` iff acknowledged (bug_150
                        // consume-on-ack; the combinator owns the
                        // recredit law), `Transient` re-credits to a
                        // later attempt, `Refused` exits through the
                        // counted-drop chokepoint inside the
                        // combinator. merged_bug_002 + merged_bug_001:
                        // the typed uid rides every retry, so the
                        // AMBIGUOUS failure (server committed, client
                        // timed out) redelivers into the M_047 absorb
                        // instead of double-banking — Ok⇒delivered is
                        // sound. Bounded per-RPC by [`admin_call`]'s
                        // timeout so a hung scheduler can't wedge the
                        // flush loop.
                        let pass = ship_all(
                            &mut unshipped,
                            &shutdown,
                            EXPOSURE_SHIP_PASS_BUDGET,
                            |slice: &PendingExposure| {
                                let mut admin = admin.clone();
                                let uid = slice.uid.clone();
                                let req = rio_proto::types::AppendInterruptSampleRequest {
                                    hw_class: slice.hw.clone(),
                                    kind: "exposure".into(),
                                    value: slice.secs,
                                    event_uid: Some(slice.uid.as_str().to_owned()),
                                };
                                async move {
                                    match admin_call(admin.append_interrupt_sample(req)).await {
                                        Ok(_) => ShipOutcome::Delivered,
                                        Err(e) => {
                                            let outcome = classify_append_status(e.code());
                                            match outcome {
                                                // Rare, high-signal: the uid and
                                                // tonic code name exactly what the
                                                // scheduler rejected (the chokepoint
                                                // warn carries the counted seconds).
                                                ShipOutcome::Refused => warn!(
                                                    error = %e,
                                                    code = ?e.code(),
                                                    %uid,
                                                    "spot-exposure: append refused \
                                                     (request-disproving); slice \
                                                     exits counted — re-sending the \
                                                     same bytes cannot succeed"
                                                ),
                                                // O(queue) per pass — log-cost
                                                // envelope keeps the per-attempt
                                                // lane at debug (auth-strike
                                                // transients included; the strike
                                                // EXHAUSTION exit warns in the
                                                // combinator); the caller's
                                                // Completed arm emits the O(1)
                                                // summary warn.
                                                _ => debug!(
                                                    error = %e,
                                                    %uid,
                                                    "spot-exposure: append failed; \
                                                     slice re-credited (or, at the \
                                                     auth-strike budget, exits \
                                                     counted in the combinator)"
                                                ),
                                            }
                                            outcome
                                        }
                                    }
                                }
                            },
                        )
                        .await;
                        // Closed exit alphabet — every variant's
                        // effect is explicit here (no wildcard arm):
                        // deferral and preemption keep slices
                        // PENDING; refused slices exited COUNTED
                        // inside the combinator; the shutdown arm
                        // below forfeits the rest (counted).
                        match pass {
                            ShipPass::Completed { retained, refused } => {
                                if retained > 0 || refused > 0 {
                                    warn!(
                                        retained,
                                        refused,
                                        queue_depth = unshipped.len(),
                                        "exposure drain pass completed with \
                                         residue: retained slices retry next \
                                         flush; refused slices exited counted"
                                    );
                                }
                            }
                            ShipPass::BudgetExhausted { remaining } => {
                                warn!(
                                    remaining,
                                    "exposure drain pass budget exhausted; \
                                     remainder stays pending until the next flush \
                                     (deferred, not dropped)"
                                );
                            }
                            ShipPass::Cancelled { requeued_in_flight } => {
                                debug!(
                                    requeued_in_flight,
                                    "exposure drain preempted by shutdown; the \
                                     shutdown arm will disclose the backlog"
                                );
                            }
                        }
                    }
                    Some(Err(e)) => {
                        warn!(error = %e, "node LIST failed; exposure flush skipped this round");
                    }
                }
            }
            _ = hw_refresh.tick() => {
                // merged_bug_033 arm-await sweep: load is internally
                // bounded (5 attempts, ≤ ~56s worst case) but still
                // raced against shutdown so the refresh cannot delay
                // the counted-disclosure arm past one backoff step.
                if shutdown.run_until_cancelled(config.load(&mut admin)).await.is_none() {
                    debug!("hw-class refresh preempted by shutdown");
                }
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
        // `event_uid` makes the INSERT idempotent: `.applied_objects()`
        // re-yields every still-extant Event on relist (controller
        // restart, apiserver restart, watch reconnect). Without dedup
        // each relist double-counts into λ's numerator → `solve_full`
        // biases away from spot.
        match attribute_interrupt(got, fb, ev.metadata.uid.clone()) {
            Ok(sample) => deliver_interrupt_sample(&mut admin, &node, sample).await,
            Err(reason) => record_sample_drop(&node, reason),
        }
    }
}

/// merged_bug_033: wall-clock budget for one [`ship_all`] pass.
/// Strictly less than the 60s flush period ([`EXPOSURE_FLUSH_SECS`])
/// so the drain arm cedes the select loop every period even under
/// saturation: worst-case arm occupancy ≈ budget + one in-flight
/// `ADMIN_RPC_TIMEOUT` ≈ 35s. SIGTERM latency is bounded by the
/// in-RPC `cancelled()` race inside the combinator (≈ instant), NOT
/// by this budget. Preempting the in-flight append is the ambiguous
/// commit-or-not case — `ship_all` re-queues the slice VERBATIM and
/// the redelivery dedups under the keyed (cluster, class, window)
/// identity (merged_bug_001), which is exactly what makes mid-RPC
/// preemption safe. merged_bug_001-r6: the budget is the pass's TIME
/// leg only — pass WORK is independently bounded by the one-rotation
/// law (attempts ≤ queue length at pass start; the budget bounds how
/// long the rotation may take, never how many same-pass attempts one
/// slice gets — fast failures cannot convert budget into a
/// `budget / failure_latency` retry hot-loop).
const EXPOSURE_SHIP_PASS_BUDGET: Duration = Duration::from_secs(30);

/// merged_bug_013 (R17 count-axis envelope, typed + violable): how
/// many presentation-judging auth refusals
/// (`judge_refusal(PerRequestService, ·) == JudgesPresentation`, the
/// `Unauthenticated | PermissionDenied` pair) one slice may absorb
/// before its permanent counted exit through
/// [`ExposureDropReason::Refused`]. Derivation: one HMAC rotation
/// skew ≈ kubelet Secret propagation lag (≤ ~2 min typical) over the
/// 60 s flush cadence ≈ 2-3 strikes; 16 ≈ sixteen minutes of
/// pure-skew passes — generous against any observed rotation, bounded
/// against a persistent token misconfig. Memory bound under
/// persistent misconfig: every slice exits by its 16th observation,
/// so queue depth ≤ BUDGET × per-flush slice mints (one slice per
/// configured hw class per flush). The exhaustion exit is
/// warn-disclosed with the observation count — the OQ-S5-2
/// disposition's loud-disclosure concern, preserved under the
/// supersession (see `classify_append_status`).
const AUTH_STRIKE_BUDGET: u32 = 16;

/// merged_bug_033: the CLOSED exit alphabet of one [`ship_all`] drain
/// pass. Every variant's effects are pinned by a dedicated test
/// (`Completed`: rotation finished — every slice queued at pass start
/// attempted exactly once, residue counts carried; `BudgetExhausted`:
/// remainder PENDING + zero drop ticks — deferral is not forfeiture;
/// `Cancelled`: in-flight slice re-queued verbatim, the caller's next
/// `select!` iteration reaches the counted-disclosure shutdown arm).
/// A new exit cannot be added without the compiler demanding its
/// effects at the caller's exhaustive match.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipPass {
    /// The rotation finished: every slice queued at pass start was
    /// attempted exactly once. `retained` transient slices stay
    /// QUEUED for the next flush; `refused` slices exited through the
    /// counted-drop chokepoint (merged_bug_001-r6).
    Completed { retained: usize, refused: usize },
    /// The per-pass budget expired; `remaining` slices stay QUEUED
    /// (pending, never dropped) for the next flush's pass.
    BudgetExhausted { remaining: usize },
    /// Shutdown preempted the pass; the in-flight slice (ambiguous
    /// commit-or-not) was re-queued verbatim — redelivery dedups
    /// under the keyed identity.
    Cancelled { requeued_in_flight: bool },
}

/// merged_bug_001-r6: the ship closure's CLOSED outcome alphabet —
/// the drain combinator's classification axis. `bool` made the
/// permanent-failure lane unrepresentable: every failure was
/// re-credited, so an InvalidArgument-class slice (or an HMAC
/// service-token refusal) recirculated forever — de-facto dropped
/// while reported "pending", wedging every pass into budget
/// exhaustion. Typing the axis in the closure's SIGNATURE makes every
/// ship path (production and test) state it or fail to compile.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum ShipOutcome {
    /// Append acknowledged — the slice is consumed (bug_150
    /// consume-on-ack).
    Delivered,
    /// Not-permanently-decided failure — the slice is re-credited
    /// VERBATIM (uid and value) for a later pass; the flush period is
    /// the retry pacing. `auth_strike` marks the presentation-judging
    /// auth refusals (merged_bug_013:
    /// `judge_refusal(PerRequestService, ·) == JudgesPresentation`):
    /// each strike advances the slice's typed observation budget
    /// ([`AUTH_STRIKE_BUDGET`]), and the budget's exhaustion — never a
    /// single observation — is the permanent exit. The field rides
    /// the VARIANT so rustc enumerates every constructor site
    /// (production and test) at the alphabet change.
    Transient { auth_strike: bool },
    /// Request-disproving failure ([`classify_append_status`]) — the
    /// scheduler refused the slice's CONTENT or shape, so re-sending
    /// the same bytes cannot succeed; the slice exits through
    /// [`record_exposure_drop`] with [`ExposureDropReason::Refused`]
    /// in the pass that observes the refusal.
    Refused,
}

/// merged_bug_001-r6: the ONE classification chokepoint mapping an
/// `AppendInterruptSample` error status to its [`ShipOutcome`].
/// EXHAUSTIVE over every [`tonic::Code`] variant — no wildcard arm,
/// so a tonic code addition is a compile error here (the census is
/// compiler-derived) — and fail-open toward RETENTION: a slice drops
/// only on proof of futility, because a wrongly-dropped slice is
/// permanent denominator loss while a wrongly-retained one costs one
/// queue slot per flush.
///
/// The futility axis is sourced from the exported authority
/// (merged_bug_013, `sec.authz.refusal-adjudication`):
/// `rio_proto::refusal::judge_refusal(PerRequestService, ·)` — this
/// transport is the per-request service-token regime
/// (`ServiceTokenInterceptor`, a fresh HMAC mint per send from the
/// rotating Secret). Arms may EXTEND the authority's ruling only on
/// codes it leaves `Undecided` (the append-specific
/// request-disproving residue below) — never contradict a
/// `JudgesPresentation` ruling.
///
/// Per-arm rationale:
///
/// - `InvalidArgument | Unimplemented` — `DisprovesRequest` per the
///   authority; the same bytes redeliver identically. Refused.
/// - `OutOfRange` — `Undecided` per the authority; extended here: the
///   validation gates at `append_interrupt_sample` emit it for
///   CONTENT this client cannot re-shape. Refused.
/// - `FailedPrecondition` — `Undecided` per the authority; extended
///   here: the server names a state the client cannot fix by
///   re-sending the same request. Refused.
/// - `Unauthenticated | PermissionDenied` — `JudgesPresentation` per
///   the authority: the refusal judges ONE presentation under ONE key
///   observation (kubelet Secret-rotation skew), so it cannot prove
///   futility of the next fresh mint. Transient WITH an auth strike —
///   the typed observation budget ([`AUTH_STRIKE_BUDGET`]) is the
///   permanent exit, never a single observation. Supersedes the
///   OQ-S5-2 counted-drop disposition (ruled 2026-06-10, round-6):
///   that ruling's own text made the rotation-window cost a mandatory
///   adversarial-review attack point, and the round-8 attack landed —
///   one skew pass counted-dropped the ENTIRE pending backlog
///   (including outage-retained slices the conservation law
///   preserves), while the eternal-recirculation wedge the drop
///   traded against was independently closed by this combinator's
///   one-rotation law. The disposition's real concern — loud
///   disclosure under a persistent token misconfig — is preserved by
///   the strike budget's counted, warn-disclosed exhaustion exit.
/// - `Unavailable | DeadlineExceeded | ResourceExhausted | Aborted |
///   Internal | Unknown | Cancelled | DataLoss` — transport,
///   leadership, load, or server-fault shapes that a later pass can
///   plausibly clear. Transient, no strike.
/// - `NotFound | AlreadyExists` — not emitted by this RPC today;
///   fail-open toward retention rather than guessing futility.
///   Transient, no strike.
/// - `Ok` — an error status carrying `Ok` is a transport anomaly,
///   not an acknowledgement; consume-on-ack (bug_150) forbids
///   consuming without an ack. Transient, no strike.
fn classify_append_status(code: tonic::Code) -> ShipOutcome {
    use tonic::Code;
    match code {
        Code::InvalidArgument
        | Code::OutOfRange
        | Code::Unimplemented
        | Code::FailedPrecondition => ShipOutcome::Refused,
        Code::Unauthenticated | Code::PermissionDenied => {
            ShipOutcome::Transient { auth_strike: true }
        }
        Code::Ok
        | Code::Cancelled
        | Code::Unknown
        | Code::DeadlineExceeded
        | Code::NotFound
        | Code::AlreadyExists
        | Code::ResourceExhausted
        | Code::Aborted
        | Code::Internal
        | Code::Unavailable
        | Code::DataLoss => ShipOutcome::Transient { auth_strike: false },
    }
}

// r[impl ctrl.informer.exposure-recredit+4]
// r[impl ctrl.informer.exposure-drain-budget+3]
/// merged_bug_033: drain the pending-exposure queue through ONE
/// combinator — every shipment in this module rides `ship_all` (the
/// production `ship` closure at the single [`run`] call site is the
/// folded former `report_exposure` body; no ship path exists outside
/// it), and the combinator takes the CancellationToken and the
/// per-pass budget BY CONSTRUCTION, so a select-arm body structurally
/// cannot contain an unbounded sequential await chain: the budget is
/// checked before every shipment (expiry → `BudgetExhausted`, the
/// remainder stays queued), and the in-flight ship is raced against
/// `shutdown.cancelled()` (`biased` — preemption re-queues the slice
/// verbatim and returns `Cancelled`, so the caller's loop reaches the
/// counted-disclosure shutdown arm within one RPC, at ANY backlog
/// depth).
///
/// merged_bug_001-r6, the ONE-ROTATION law: the pass snapshots the
/// queue length at start and attempts each queued slice AT MOST ONCE
/// — pass work is bounded by QUEUE SIZE, never by the
/// `budget / failure_latency` ratio (pre-fix, a fast-failing append
/// — connection refused, standby-UNAVAILABLE in ~ms against the 30s
/// budget — re-popped the same slice ~10^3-10^4 times per pass). The
/// inter-attempt spacing of any given slice is therefore the flush
/// period itself: the rotation IS the retry pacing, deterministic and
/// timer-free. The classification axis rides the closure's SIGNATURE
/// ([`ShipOutcome`]): `Delivered` consumes (bug_150 consume-on-ack);
/// `Transient` re-credits VERBATIM, uid and value, so however many
/// times delivery is ambiguous the server holds at most one row per
/// (cluster, class, window) — merged_bug_002: the retained slice
/// keeps its EXACT identity (no re-mint, no merge); the cursor
/// already advanced when the slice was banked, so this queue is the
/// ONLY carrier of the failed window — dropping a retriable slice
/// here would be permanent denominator loss. A `Transient` carrying
/// `auth_strike` additionally advances the slice's monotone strike
/// ledger (merged_bug_013), and the observation that reaches
/// [`AUTH_STRIKE_BUDGET`] exits the slice through the counted
/// chokepoint instead of re-crediting — warn-disclosed with the
/// count, and tallied in `refused` (a strike exit IS a refused exit;
/// the `Completed` alphabet is unchanged). `Refused` exits through
/// [`record_exposure_drop`] IN THIS PASS (the counted chokepoint —
/// the slice leaves the queue only via ack, counted drop, or stays).
/// A budget-deferred or preemption-requeued slice remains PENDING,
/// never a drop.
///
/// `ship` returns its future by VALUE (`FnMut(&_) -> Fut`, not an
/// async closure): the future owns everything it needs (the run()
/// closure clones the client and builds the request eagerly), so the
/// combinator awaits no slice-borrowing future — the lending shape
/// trips rustc's HRTB Send check on the spawned informer future (the
/// rust-lang/rust 102211 family; see `run_nodeclaim_pool`'s doc for
/// the house precedent).
async fn ship_all<Fut: Future<Output = ShipOutcome>>(
    unshipped: &mut VecDeque<PendingExposure>,
    shutdown: &rio_common::signal::Token,
    per_pass_budget: Duration,
    mut ship: impl FnMut(&PendingExposure) -> Fut,
) -> ShipPass {
    let start = tokio::time::Instant::now();
    // One rotation: exactly the slices queued at pass start get an
    // attempt; Transient push_backs land BEHIND the un-attempted
    // remainder, so they cannot be re-popped within this pass.
    let round = unshipped.len();
    let mut retained = 0usize;
    let mut refused = 0usize;
    for _ in 0..round {
        // Budget check FIRST: an expired pass defers the WHOLE
        // remainder (queued, pending — never dropped) to the next
        // flush, so the arm cedes the select loop every period no
        // matter how deep the backlog or how slow the scheduler.
        if start.elapsed() >= per_pass_budget {
            return ShipPass::BudgetExhausted {
                remaining: unshipped.len(),
            };
        }
        let Some(mut slice) = unshipped.pop_front() else {
            // Structurally unreachable: the rotation pops at most
            // `round` slices and only ever pushes back — the queue
            // cannot run dry before `round` pops. Complete defensively
            // rather than panic in the flush loop.
            break;
        };
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                // Preempting the in-flight append is the ambiguous
                // commit-or-not case — re-queue VERBATIM (uid and
                // all); the redelivery dedups under the keyed
                // (cluster, class, window) identity. The caller's
                // next select! iteration reaches the biased shutdown
                // arm, whose per-slice counted forfeiture closes the
                // pass-external drop exits.
                unshipped.push_back(slice);
                return ShipPass::Cancelled {
                    requeued_in_flight: true,
                };
            }
            outcome = ship(&slice) => match outcome {
                ShipOutcome::Delivered => {}
                ShipOutcome::Transient { auth_strike } => {
                    if auth_strike {
                        // Monotone strike ledger (merged_bug_013):
                        // one presentation-judging auth refusal = one
                        // strike; interleaved non-auth transients
                        // never touch it.
                        slice.auth_strikes = slice.auth_strikes.saturating_add(1);
                    }
                    if auth_strike && slice.auth_strikes >= AUTH_STRIKE_BUDGET {
                        // The budget's violation arm (R17): the
                        // permanent exit requires exactly N
                        // observations and is disclosed with the
                        // count — the persistent-misconfig
                        // disposition, typed (warn-lane and rare;
                        // per-strike transients stay debug in the
                        // ship closure).
                        warn!(
                            uid = %slice.uid,
                            strikes = slice.auth_strikes,
                            budget = AUTH_STRIKE_BUDGET,
                            "spot-exposure: auth (presentation-judging) refusals \
                             exhausted the strike budget; slice exits counted — \
                             every observation across the budget was refused \
                             (persistent service-token misconfig?)"
                        );
                        record_exposure_drop(ExposureDropReason::Refused, slice.secs);
                        refused += 1;
                    } else {
                        // The recredit law: retain VERBATIM (uid and
                        // value) for the next flush's pass (one
                        // attempt per slice per pass — never
                        // re-attempted within this one).
                        unshipped.push_back(slice);
                        retained += 1;
                    }
                }
                ShipOutcome::Refused => {
                    // The counted-drop chokepoint, IN the pass that
                    // observes the refusal — a request-disproving
                    // refusal cannot exit silently and cannot
                    // recirculate.
                    record_exposure_drop(ExposureDropReason::Refused, slice.secs);
                    refused += 1;
                }
            }
        }
    }
    ShipPass::Completed { retained, refused }
}

/// merged_bug_001 (Q2-round5): the cluster identity axis of every
/// exposure uid. `interrupt_samples` is multi-cluster — the scheduler
/// binds `[sla].cluster` per row and M_047's partial unique index is
/// table-GLOBAL in the shared-PG topology (ADR-023 §2.13;
/// db/history.rs "global-DB topology") — so an axis-free uid from two
/// clusters' informers collides and the `ON CONFLICT DO NOTHING`
/// absorb silently and permanently drops the second cluster's
/// denominator window. Carrying the axis in the TYPE means omitting
/// it is a compile error, not a review item.
///
/// Single normalizing constructor: trims; empty = the single-cluster
/// default, matching the scheduler's `[sla].cluster` `DEFAULT ''`
/// (043_sla_hardening). The value is Config-borne (`cluster` in
/// controller.toml, rendered by helm from the SAME values expression
/// as the scheduler's `[sla].cluster` — one source, two binaries).
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct ClusterId(String);

impl ClusterId {
    /// Normalize: trim. Empty (post-trim) = single-cluster default.
    pub fn new(raw: &str) -> Self {
        Self(raw.trim().to_string())
    }

    // r[impl ctrl.informer.cluster-identity-boundary+1]
    /// bug_022: `true` iff this is the empty (post-trim)
    /// single-cluster default — the value under which two deployments
    /// sharing one PG mint byte-identical `exposure::{hw}:{slot}`
    /// uids every window and silently absorb each other's
    /// λ-denominator evidence (M_047 dedup). The predicate the
    /// activation disclosure (`disclose_cluster_identity`, private to
    /// this module) and the config docs quantify over.
    pub fn is_single_cluster_default(&self) -> bool {
        self.0.is_empty()
    }
}

// r[impl ctrl.informer.cluster-identity-boundary+1]
/// bug_022: the activation disclosure. The informer IS the exposure
/// path's activation, and this is the only boundary present in EVERY
/// topology — the helm `rio.clusterIdentity` render gate covers
/// chart-driven installs where the shared-capable PG path
/// (`externalSecrets.enabled`) is render-visible; this disclosure
/// covers the residual the chart cannot see (manual-secret external
/// PG, out-of-chart installs). Empty (single-cluster default) → ONE
/// loud warn naming the cross-deployment absorption hazard;
/// non-empty → ONE positive info disclosure, so the operator can
/// read the live axis value off the boot log either way.
fn disclose_cluster_identity(cluster: &ClusterId) {
    if cluster.is_single_cluster_default() {
        warn!(
            "cluster identity is the single-cluster default (\"\"); exposure uids render \
             exposure::{{hw}}:{{slot}} — if this scheduler's PG is shared with ANY other \
             deployment, λ-denominator evidence is being silently absorbed cross-cluster \
             (M_047); set [cluster] in controller.toml / scheduler.sla.cluster in helm"
        );
    } else {
        info!(cluster = %cluster.0, "exposure uid cluster identity");
    }
}

/// merged_bug_001: one logical exposure window — the grid slot START
/// (epoch seconds, always a multiple of [`EXPOSURE_FLUSH_SECS`]).
/// Obtainable ONLY from [`WindowGate::admit`] (no public constructor),
/// so every uid's window component is grid-aligned and strictly
/// monotone BY CONSTRUCTION — a non-advancing window is
/// unrepresentable, not checked per call site. Grid alignment is what
/// makes two surge-overlapped informers of ONE cluster converge on
/// IDENTICAL uids for the same wall window (their process-local flush
/// instants differ; their slots do not), turning the co-run collision
/// into the designed at-most-once absorb instead of double-banking.
#[derive(Debug, Clone, Copy, PartialEq, Eq, PartialOrd, Ord)]
struct WindowId(u64);

impl WindowId {
    /// Slot start in epoch seconds (digits-only in the rendered uid —
    /// the parse-unambiguity anchor).
    fn slot_secs(self) -> u64 {
        self.0
    }
}

/// merged_bug_001: per-process monotonic window admission. `admit`
/// returns `Some` iff the computed grid slot STRICTLY exceeds the
/// last admitted one (first admit always succeeds) — a clock step
/// backward or a same-slot double tick yields `None` and the flush
/// round defers banking (LIST-failure-equivalent: cursors untouched,
/// nothing forfeited, λ samples arrive late not reduced). Pre-fix,
/// window identity was raw `now_epoch()` seconds: a backward step
/// re-minted an already-shipped second under fresh seconds, and the
/// redelivery was absorbed by M_047 — counted as delivered, lost
/// forever.
#[derive(Debug, Default)]
struct WindowGate {
    last: Option<WindowId>,
}

impl WindowGate {
    /// Admit `now_epoch` into a fresh window, or refuse (`None`) if
    /// the grid slot has not strictly advanced. Non-finite or
    /// negative epochs (the `now_epoch()` `unwrap_or(0.0)` arm) clamp
    /// to slot 0 — admissible at most once, refused thereafter.
    fn admit(&mut self, now_epoch: f64) -> Option<WindowId> {
        let secs = if now_epoch.is_finite() && now_epoch > 0.0 {
            now_epoch
        } else {
            0.0
        };
        let slot = WindowId((secs as u64 / EXPOSURE_FLUSH_SECS) * EXPOSURE_FLUSH_SECS);
        match self.last {
            Some(prev) if slot <= prev => None,
            _ => {
                self.last = Some(slot);
                Some(slot)
            }
        }
    }
}

/// merged_bug_001 (Q2-round5 SIGNED vehicle): the deterministic
/// exposure idempotency key, rendered `exposure:{cluster}:{hw}:{slot}`
/// — the cluster axis lives in the uid FORMAT, not in schema (M_047
/// is checksum-frozen; no DDL). [`EventUid::new`] is the ONLY
/// producer and demands every identity axis at construction; a
/// `format!`-minted uid for the M_047-constrained column is
/// untypecheckable ([`PendingExposure::uid`] is `EventUid`, not
/// `String`).
///
/// Parse-unambiguity: `hw` is server-charset-gated `[a-z0-9-]`
/// (`AppendInterruptSample` rejects anything else) and the slot is
/// digits, so the rightmost two `:`-segments bind uniquely and the
/// residual prefix is the cluster — a `:` in a cluster name stays
/// unambiguous, and the empty single-cluster default renders
/// `exposure::{hw}:{slot}`: visible, and disjoint from every
/// non-empty cluster AND from the retired pre-cluster format
/// `exposure:{hw}:{epoch}` (no dedup seam at the cut; the unshipped
/// queue is process memory, so no pre-fix slice ever redelivers —
/// old-format rows simply age out of the λ window).
///
/// bug_022: disjointness is between DISTINCT cluster values; two
/// deployments BOTH at the empty default mint IDENTICAL uids — the
/// shared-PG topology therefore requires distinct non-empty ids,
/// enforced at render by the helm external-secrets gate
/// (`rio.clusterIdentity`) and disclosed at activation by the
/// informer warn ([`disclose_cluster_identity`]).
#[derive(Debug, Clone, PartialEq, Eq)]
struct EventUid(String);

impl EventUid {
    /// Mint the uid for one (cluster, class, window) slice. The only
    /// uid producer — all three axes are demanded by type.
    fn new(cluster: &ClusterId, hw: &str, window: WindowId) -> Self {
        Self(format!(
            "exposure:{}:{}:{}",
            cluster.0,
            hw,
            window.slot_secs()
        ))
    }

    /// Wire form for `AppendInterruptSampleRequest.event_uid`.
    fn as_str(&self) -> &str {
        &self.0
    }
}

impl std::fmt::Display for EventUid {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        f.write_str(&self.0)
    }
}

/// One un-acknowledged exposure shipment: a single `(hw_class,
/// window)` slice with its deterministic idempotency key
/// (merged_bug_002, cluster-scoped + grid-aligned by merged_bug_001).
#[derive(Debug, Clone, PartialEq)]
struct PendingExposure {
    hw: String,
    /// `exposure:{cluster}:{hw}:{window-slot}` — deterministic per
    /// (cluster, class, window), carried VERBATIM across retries.
    /// Deterministic (not minted-per-send) so the redelivery of a
    /// slice whose first append committed-but-timed-out collides with
    /// its own committed row — and ONLY with its own row: the cluster
    /// axis scopes the key in the shared-PG topology, and the
    /// grid-aligned slot makes same-cluster co-run twins collide BY
    /// DESIGN (at-most-once per logical window).
    uid: EventUid,
    secs: f64,
    /// merged_bug_013: presentation-judging auth refusals absorbed so
    /// far (`Transient { auth_strike: true }` observations) — MONOTONE
    /// (never reset by interleaved non-auth transients or fresh
    /// passes). At [`AUTH_STRIKE_BUDGET`] the slice exits counted
    /// through [`ExposureDropReason::Refused`]; below it the slice is
    /// retained exactly like every other transient.
    auth_strikes: u32,
}

/// Queue this flush's fresh per-class slices as individually-keyed
/// shipments (merged_bug_002). Every uid is minted through
/// [`EventUid::new`] from a [`WindowGate`]-admitted window
/// (merged_bug_001) — this is the SOLE mint site, and the typed
/// constructor demands the cluster axis and the grid slot. Slices are
/// NEVER merged across windows — the uid keys an exact committed
/// value server-side, so merging would change the value under an
/// already-committed key and the `ON CONFLICT` absorb would silently
/// drop the fresh half. A retained class with no fresh slice this
/// round (its nodes deleted mid-outage) still retries: it simply
/// stays queued.
fn queue_exposure_slices(
    unshipped: &mut VecDeque<PendingExposure>,
    fresh: Vec<(String, f64)>,
    cluster: &ClusterId,
    window: WindowId,
) {
    for (hw, secs) in fresh {
        let uid = EventUid::new(cluster, &hw, window);
        unshipped.push_back(PendingExposure {
            hw,
            uid,
            secs,
            auth_strikes: 0,
        });
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
    // r[verify ctrl.informer.exposure-recredit+4]
    /// bug_150 + merged_bug_002: a failed exposure append re-credits
    /// the slice; across an outage spanning N windows, total banked
    /// exposure is conserved as N DISTINCT keyed slices — never a
    /// merged sum (merging would change the value under window 1's
    /// already-possibly-committed uid, and the server's `ON CONFLICT`
    /// absorb would silently drop window 2's seconds). (Recorded red
    /// on the bug_150 pre-fix shape via strawman reversal of the
    /// settle law: the second flush shipped 60.0 total — the failed
    /// window was consumed by the cursor advance and never re-banked.)
    #[tokio::test(start_paused = true)]
    async fn failed_exposure_slice_recredits_to_next_flush() {
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        // Flush 1 (t=1000 → slot 960): fresh 60s for m6id; the
        // ship_all pass FAILS the append.
        let w1 = gate.admit(1000.0).expect("first admit");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w1);
        assert_eq!(unshipped.len(), 1);
        assert_eq!(
            (unshipped[0].hw.as_str(), unshipped[0].secs),
            ("m6id", 60.0)
        );
        // Zero-time-advance closure (merged_bug_001-r6 wrong-witness
        // kill): the recredit is a property of the ROTATION — the
        // budget arm provably cannot be the bounder, because no
        // virtual time ever elapses.
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |_| std::future::ready(ShipOutcome::Transient { auth_strike: false }),
        )
        .await;
        assert_eq!(unshipped.len(), 1, "failed slice re-credited, not consumed");
        // Flush 2 (t=1060 → slot 1020): fresh 60s again. BOTH windows
        // ship — as two slices under their own uids, total conserved.
        let w2 = gate.admit(1060.0).expect("next slot admits");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w2);
        assert_eq!(
            unshipped.iter().map(|s| s.secs).sum::<f64>(),
            120.0,
            "the failed window must be re-credited, not consumed"
        );
        assert_eq!(
            unshipped.len(),
            2,
            "windows stay distinct slices, never merged"
        );
        assert_ne!(
            unshipped[0].uid, unshipped[1].uid,
            "each window keys its own committed row"
        );
        let mut shipped: Vec<f64> = Vec::new();
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |s: &PendingExposure| {
                shipped.push(s.secs);
                std::future::ready(ShipOutcome::Delivered)
            },
        )
        .await;
        assert_eq!(shipped.iter().sum::<f64>(), 120.0, "both windows shipped");
        assert!(unshipped.is_empty(), "delivered slices leave no residue");
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// A retained class with NO fresh slice this round (its nodes were
    /// deleted mid-outage) still retries — retention is queue
    /// membership, independent of fresh production.
    #[tokio::test(start_paused = true)]
    async fn retained_class_without_fresh_slice_still_retries() {
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        let w1 = gate.admit(500.0).expect("first admit");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 45.0)], &cluster, w1);
        // Zero-time-advance closure (merged_bug_001-r6 wrong-witness
        // kill): retention is the rotation's law, not the budget's.
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |_| std::future::ready(ShipOutcome::Transient { auth_strike: false }),
        )
        .await;
        // Next flush: NO fresh slices — retention is queue
        // membership, independent of fresh production.
        let w2 = gate.admit(560.0).expect("next slot admits");
        queue_exposure_slices(&mut unshipped, vec![], &cluster, w2);
        let mut attempted: Vec<(String, f64)> = Vec::new();
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |s: &PendingExposure| {
                attempted.push((s.hw.clone(), s.secs));
                std::future::ready(ShipOutcome::Delivered)
            },
        )
        .await;
        assert_eq!(attempted, vec![("m6id".into(), 45.0)]);
        assert!(unshipped.is_empty());
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_002 red: the retry of a slice whose append
    /// committed-but-timed-out MUST collide with its own committed row
    /// — which requires the uid to be deterministic and carried
    /// VERBATIM across settle. `left:` the pre-fix wire sent
    /// `event_uid: None` on every attempt (each retry inserted a new
    /// row; a 1-hour brownout at 60s windows over-counted the
    /// denominator ~30×, λ biased LOW → solver over-prefers spot);
    /// `right:` retry uid == original uid (re-verified under
    /// [`EventUid`] equality — merged_bug_001 made the key
    /// cluster-scoped and grid-aligned, `exposure:{cluster}:{hw}:{slot}`).
    #[tokio::test(start_paused = true)]
    async fn retried_slice_carries_identical_uid() {
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        let w = gate.admit(1767225613.0).expect("first admit");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w);
        let original = unshipped[0].clone();
        assert_eq!(
            original.uid.as_str(),
            "exposure:prod-eu:m6id:1767225600",
            "deterministic per-(cluster, class, window) key"
        );
        // Ambiguous failure → the combinator's recredit law retains;
        // the retried slice is byte-identical (uid AND value).
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |_| std::future::ready(ShipOutcome::Transient { auth_strike: false }),
        )
        .await;
        assert_eq!(
            unshipped[0], original,
            "retry must collide with its own committed row server-side"
        );
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_001 red R1: the uid carries the cluster axis and a
    /// grid-aligned window slot. Two clusters, same hw + same instant
    /// ⇒ uids DIFFER (cross-cluster absorbs unconstructible in the
    /// shared-PG topology); one cluster, two gate instances (the
    /// rollout surge twin) at different in-window instants ⇒ uids
    /// IDENTICAL (co-run double-banking becomes the designed
    /// at-most-once absorb). Recorded red against the pre-fix mint
    /// (`exposure:{hw}:{epoch}` — no cluster axis, no convergence):
    /// `left: "exposure:mid-ebs-x86:1767225613" /
    ///  right: "exposure:mid-ebs-x86:1767225647"`.
    #[test]
    fn exposure_uid_is_cluster_scoped_and_grid_aligned() {
        // Two clusters, same hw_class, same instant: DIFFERENT uids.
        let east = ClusterId::new("prod-east");
        let west = ClusterId::new("prod-west");
        let mut gate_e = WindowGate::default();
        let mut gate_w = WindowGate::default();
        let we = gate_e.admit(1767225613.2).expect("first admit");
        let ww = gate_w.admit(1767225613.2).expect("first admit");
        let mut qe: VecDeque<PendingExposure> = VecDeque::new();
        let mut qw: VecDeque<PendingExposure> = VecDeque::new();
        queue_exposure_slices(&mut qe, vec![("mid-ebs-x86".into(), 60.0)], &east, we);
        queue_exposure_slices(&mut qw, vec![("mid-ebs-x86".into(), 60.0)], &west, ww);
        assert_ne!(
            qe[0].uid, qw[0].uid,
            "cluster axis must scope the dedup key (shared-PG topology)"
        );
        assert_eq!(
            qe[0].uid.as_str(),
            "exposure:prod-east:mid-ebs-x86:1767225600"
        );
        assert_eq!(
            qw[0].uid.as_str(),
            "exposure:prod-west:mid-ebs-x86:1767225600"
        );

        // Surge twin: ONE cluster, two processes (two gates), flushes
        // at …+13.2 and …+47.9 of the same wall window: IDENTICAL.
        let mut twin_a = WindowGate::default();
        let mut twin_b = WindowGate::default();
        let wa = twin_a.admit(1767225613.2).expect("first admit");
        let wb = twin_b.admit(1767225647.9).expect("first admit");
        let mut qa: VecDeque<PendingExposure> = VecDeque::new();
        let mut qb: VecDeque<PendingExposure> = VecDeque::new();
        queue_exposure_slices(&mut qa, vec![("mid-ebs-x86".into(), 60.0)], &east, wa);
        queue_exposure_slices(&mut qb, vec![("mid-ebs-x86".into(), 47.9)], &east, wb);
        assert_eq!(
            qa[0].uid, qb[0].uid,
            "same logical window must converge on one uid"
        );

        // The empty single-cluster default renders visibly and
        // disjoint from every non-empty cluster.
        let mut gate_d = WindowGate::default();
        let wd = gate_d.admit(1767225613.2).expect("first admit");
        let mut qd: VecDeque<PendingExposure> = VecDeque::new();
        queue_exposure_slices(
            &mut qd,
            vec![("mid-ebs-x86".into(), 60.0)],
            &ClusterId::new("  "),
            wd,
        );
        assert_eq!(qd[0].uid.as_str(), "exposure::mid-ebs-x86:1767225600");
    }

    /// `MakeWriter` into a shared buffer (the rio-common task.rs
    /// pattern) — `fmt::TestWriter` goes to stdout, no good for
    /// asserting on the emitted bytes.
    #[derive(Clone, Default)]
    struct LogBuf(std::sync::Arc<std::sync::Mutex<Vec<u8>>>);

    impl std::io::Write for LogBuf {
        fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
            self.0.lock().unwrap().extend_from_slice(buf);
            Ok(buf.len())
        }
        fn flush(&mut self) -> std::io::Result<()> {
            Ok(())
        }
    }

    impl<'a> tracing_subscriber::fmt::MakeWriter<'a> for LogBuf {
        type Writer = Self;
        fn make_writer(&'a self) -> Self::Writer {
            self.clone()
        }
    }

    /// Capture the lines `f` emits through a scoped JSON subscriber
    /// (drop-guard local — no global recorder races).
    fn captured_lines(f: impl FnOnce()) -> Vec<String> {
        use tracing_subscriber::layer::SubscriberExt as _;
        let buf = LogBuf::default();
        let subscriber = tracing_subscriber::registry().with(
            tracing_subscriber::fmt::layer()
                .json()
                .with_writer(buf.clone()),
        );
        let guard = tracing::subscriber::set_default(subscriber);
        f();
        drop(guard);
        let bytes = std::mem::take(&mut *buf.0.lock().unwrap());
        String::from_utf8(bytes)
            .unwrap()
            .lines()
            .map(String::from)
            .collect()
    }

    // r[verify ctrl.informer.cluster-identity-boundary+1]
    /// bug_022 red R-F: the activation disclosure EMITS — exactly one
    /// WARN naming the single-cluster default at `ClusterId::new("")`,
    /// and zero WARNs + one positive INFO carrying the axis value at a
    /// non-empty id. Recorded red (strawman disclosure site present
    /// but empty — pre-fix the informer emitted NOTHING at
    /// activation): `panic: expected warn line not emitted; captured:
    /// []`. Certifies: the disclosure duty itself, at emission — not
    /// a classification proxy.
    #[test]
    fn informer_activation_warns_on_single_cluster_default() {
        let lines = captured_lines(|| disclose_cluster_identity(&ClusterId::new("")));
        let warns: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("\"WARN\"") && l.contains("single-cluster default"))
            .collect();
        assert_eq!(
            warns.len(),
            1,
            "expected warn line not emitted; captured: {lines:?}"
        );

        let lines = captured_lines(|| disclose_cluster_identity(&ClusterId::new("prod-eu")));
        assert!(
            !lines.iter().any(|l| l.contains("\"WARN\"")),
            "non-empty id must not warn; captured: {lines:?}"
        );
        let infos: Vec<&String> = lines
            .iter()
            .filter(|l| l.contains("\"INFO\"") && l.contains("prod-eu"))
            .collect();
        assert_eq!(
            infos.len(),
            1,
            "positive disclosure must carry the axis value; captured: {lines:?}"
        );
    }

    // r[verify ctrl.informer.cluster-identity-boundary+1]
    /// bug_022 red R-G: the predicate the warn and the docs quantify
    /// over — empty and whitespace-only ids are the single-cluster
    /// default (trim law pinned); non-empty ids are not. Recorded red
    /// (new surface, disclosed): `error[E0599]: no method named
    /// `is_single_cluster_default` found for struct
    /// `node_informer::ClusterId``.
    #[test]
    fn cluster_id_classifies_the_default() {
        assert!(ClusterId::new("").is_single_cluster_default());
        assert!(
            ClusterId::new("  ").is_single_cluster_default(),
            "trim law: whitespace-only normalizes to the default"
        );
        assert!(!ClusterId::new("prod-eu").is_single_cluster_default());
        assert!(
            !ClusterId::new(" prod-eu ").is_single_cluster_default(),
            "trim law: padding does not change a real id's class"
        );
    }

    // r[verify ctrl.informer.cluster-identity-boundary+1]
    /// merged_bug_067 R-3C: the Rust constructor agrees with the
    /// cross-boundary golden fixture CELL-FOR-CELL over the FULL
    /// alphabet (including the non-helm-settable whitespace forms) —
    /// `ClusterId::new(raw)` yields exactly `normalized` (in-module
    /// private access) and `is_single_cluster_default()` matches the
    /// fixture's flag. The SAME committed bytes drive helm fragment
    /// 39's leg (i) over the helm-settable subset, so the two
    /// languages' trim ∘ classify predicates cannot drift (the
    /// derivation_statuses.json precedent; the fixture's `_doc` field
    /// carries the scope split — each side certifies its full
    /// reachable input set). Subsumes-but-keeps
    /// `cluster_id_classifies_the_default` (the round-6 point pins).
    #[test]
    fn cluster_identity_normalization_golden() {
        #[derive(serde::Deserialize)]
        struct Fixture {
            cases: Vec<Case>,
        }
        #[derive(serde::Deserialize)]
        struct Case {
            raw: String,
            normalized: String,
            single_cluster_default: bool,
            helm_settable: bool,
        }
        let fixture: Fixture = serde_json::from_str(include_str!(
            "../../tests/golden/cluster_identity_normalization.json"
        ))
        .expect("golden fixture parses");
        assert!(
            fixture.cases.len() >= 6,
            "the fixture covers the alphabet (defaults, padding, interior space)"
        );
        assert!(
            fixture.cases.iter().any(|c| !c.helm_settable),
            "the Rust leg quantifies past the helm-settable subset"
        );
        for case in &fixture.cases {
            let id = ClusterId::new(&case.raw);
            assert_eq!(
                id.0, case.normalized,
                "ClusterId::new({:?}) must normalize per the one law",
                case.raw
            );
            assert_eq!(
                id.is_single_cluster_default(),
                case.single_cluster_default,
                "classification of {:?} must match the fixture",
                case.raw
            );
        }
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_001 red R2: window identity is strictly monotone per
    /// process — a clock step backward or a same-slot double tick is
    /// REFUSED (banking deferred), never re-minted under fresh
    /// seconds. Recorded red (characterization, against the pre-fix
    /// mint — the old path constructed a fresh-seconds uid for a
    /// non-advanced epoch, the absorb-loss precondition):
    /// `left: 2 / right: 1` — "a non-advanced window must not mint a
    /// fresh-seconds uid (the absorb-loss precondition is
    /// constructible)".
    #[test]
    fn window_gate_refuses_non_advancing_windows() {
        let mut gate = WindowGate::default();
        assert_eq!(
            gate.admit(1767225613.0).map(WindowId::slot_secs),
            Some(1767225600),
            "first admit lands on the grid slot start"
        );
        assert_eq!(
            gate.admit(1767225608.0),
            None,
            "clock step backward (−5s) is refused"
        );
        assert_eq!(
            gate.admit(1767225633.0),
            None,
            "same slot (+20s) is refused — co-run/double-tick dedup"
        );
        assert_eq!(
            gate.admit(1767225660.0).map(WindowId::slot_secs),
            Some(1767225660),
            "the next grid slot admits"
        );
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_001 red R3: a gate-deferred window forfeits NOTHING
    /// — cursors untouched, zero drop ticks, the retained queue
    /// intact (it still ships). Extends the conservation family
    /// below: deferral is the LIST-failure posture, not a fourth
    /// forfeiture.
    #[tokio::test(start_paused = true)]
    async fn conservation_holds_across_deferred_window() {
        use metrics_util::debugging::DebuggingRecorder;
        let cfg = band_config();
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        let nodes = vec![spot_node("a", "7", 1000)];

        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);

        // Round 1 (t=1060 → slot 1020): banks one slice; its append
        // fails transiently → retained (the combinator's recredit
        // law). Zero-time-advance closure (merged_bug_001-r6
        // wrong-witness kill, R-D2): the gate-None conservation below
        // is certified free of the paused-clock recredit scaffolding.
        let w1 = gate.admit(1060.0).expect("first admit");
        let out = flush_spot_exposure(&mut cursors, &nodes, &cfg, 0.0, 1060.0);
        assert!(out.drops.is_empty());
        queue_exposure_slices(&mut unshipped, out.banked, &cluster, w1);
        assert_eq!(unshipped.len(), 1);
        ship_all(
            &mut unshipped,
            &rio_common::signal::Token::new(),
            EXPOSURE_SHIP_PASS_BUDGET,
            |_| std::future::ready(ShipOutcome::Transient { auth_strike: false }),
        )
        .await;
        assert_eq!(unshipped.len(), 1, "failed slice retained");
        let cursors_before = cursors.clone();
        let queued_before = unshipped.clone();

        // Round 2 fires in the SAME slot (t=1070 — double tick /
        // backward step): the gate refuses, and the run() arm skips
        // the whole banking leg — cursors untouched, the retained
        // queue intact, and NOT ONE drop tick (deferral is pending,
        // never forfeiture).
        assert_eq!(gate.admit(1070.0), None, "same slot must defer banking");
        assert_eq!(
            cursors, cursors_before,
            "deferred window leaves cursors untouched (next admitted \
             window banks the full delta)"
        );
        assert_eq!(
            unshipped, queued_before,
            "retained queue intact across the deferral"
        );
        assert!(
            rec.snapshotter().snapshot().into_vec().is_empty(),
            "a deferred window forfeits nothing — zero drop ticks"
        );
    }

    /// Production-minted backlog for the ship_all tests: `n` windows
    /// of one class, every uid through the gate + queue constructors
    /// (witness provenance — no hand-rolled wire shapes).
    fn minted_backlog(n: u64) -> VecDeque<PendingExposure> {
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        for i in 0..n {
            let w = gate
                .admit(1000.0 + 60.0 * i as f64)
                .expect("each tick lands in a fresh slot");
            queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w);
        }
        unshipped
    }

    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_001-r6 red R-A: each queued slice gets AT MOST ONE
    /// attempt per pass — the rotation, not the budget, bounds pass
    /// work (the closure advances ZERO virtual time, so the budget
    /// arm provably cannot be the bounder; the round-5 recredit
    /// witnesses proved one-attempt-per-slice only via paused-clock
    /// engineering, 5 virtual s/attempt against a 5s budget).
    /// Recorded red against the pre-fix combinator (bool alphabet,
    /// fail 5× then deliver): `left: 6 / right: 1` — the slice
    /// delivered within ONE pass after 6 same-pass attempts.
    /// Certifies: the attempts bound itself, with the budget provably
    /// not the bounder.
    #[tokio::test(start_paused = true)]
    async fn drain_pass_attempts_each_queued_slice_exactly_once() {
        let mut unshipped = minted_backlog(1);
        let shutdown = rio_common::signal::Token::new();
        let mut attempts = 0u32;
        let pass = ship_all(&mut unshipped, &shutdown, EXPOSURE_SHIP_PASS_BUDGET, |_| {
            attempts += 1;
            // Would deliver on the 6th same-pass attempt — under the
            // one-rotation law that attempt can never happen in THIS
            // pass.
            let outcome = if attempts > 5 {
                ShipOutcome::Delivered
            } else {
                ShipOutcome::Transient { auth_strike: false }
            };
            std::future::ready(outcome)
        })
        .await;
        assert_eq!(
            attempts, 1,
            "one attempt per queued slice per pass (the flush period is the retry pacing)"
        );
        assert_eq!(
            pass,
            ShipPass::Completed {
                retained: 1,
                refused: 0
            },
            "the transient slice is retained, the rotation completes"
        );
        assert_eq!(unshipped.len(), 1, "retained for the NEXT flush's pass");
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_001-r6 red R-B: a permanently-refused slice exits
    /// COUNTED within its pass — queue empty, pass residue carried in
    /// `Completed`, and the whole seconds land on
    /// `rio_controller_spot_exposure_dropped_seconds_total{reason="refused"}`.
    /// Recorded red (DISCLOSED STRAWMAN — `Refused` is
    /// unrepresentable in the old bool alphabet, mapped to `false`,
    /// with 5 virtual s/attempt terminating the otherwise-unbounded
    /// pre-fix loop at a 12s budget): `left: 1 / right: 0` queue
    /// residue, and the reason="refused" series did not exist.
    /// Certifies: the conservation identity's refused leg
    /// (in == delivered + retained + counted).
    #[tokio::test(start_paused = true)]
    async fn refused_slice_exits_counted_within_its_pass() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let mut unshipped = minted_backlog(2);
        let shutdown = rio_common::signal::Token::new();
        let pass = ship_all(
            &mut unshipped,
            &shutdown,
            EXPOSURE_SHIP_PASS_BUDGET,
            |s: &PendingExposure| {
                // Slice 1 (slot 960) is permanently refused; slice 2
                // delivers. Zero virtual time advances.
                let outcome = if s.uid.as_str().ends_with(":960") {
                    ShipOutcome::Refused
                } else {
                    ShipOutcome::Delivered
                };
                std::future::ready(outcome)
            },
        )
        .await;
        assert_eq!(
            unshipped.len(),
            0,
            "a permanently-refused slice must exit its pass counted, not recirculate"
        );
        assert_eq!(
            pass,
            ShipPass::Completed {
                retained: 0,
                refused: 1
            }
        );
        // Snapshot EXACTLY ONCE (DebuggingRecorder drains on
        // snapshot) and query the materialized Vec.
        let snapshot = rec.snapshotter().snapshot().into_vec();
        let refused_secs = snapshot.into_iter().find_map(|(k, _, _, v)| {
            let key = k.key();
            (key.name() == "rio_controller_spot_exposure_dropped_seconds_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "refused"))
            .then_some(v)
        });
        match refused_secs {
            Some(DebugValue::Counter(n)) => assert_eq!(
                n, 60,
                "the refused slice's whole seconds exit through the counted chokepoint"
            ),
            other => panic!("refused drop uncounted: no reason=refused series ({other:?})"),
        }
    }

    // r[verify sec.authz.refusal-adjudication]
    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_013 red R-1C: ONE drain pass under HMAC rotation
    /// skew retains the ENTIRE pending backlog — including
    /// outage-retained slices — with uids verbatim, zero drop ticks,
    /// and one strike per slice; the post-skew pass then delivers
    /// everything (Σ in == Σ delivered — the conservation law's
    /// PENDING leg closes the loop). Production-minted population
    /// (R13: `ClusterId::new` + `WindowGate::admit` +
    /// `queue_exposure_slices`), real `tonic::Status::unauthenticated`
    /// classified through the production `classify_append_status`.
    /// TRUE RED at 83e596f0c (via the disclosed (ddddd) classifier
    /// strawman): `left: Completed { retained: 0, refused: 3 }, queue
    /// empty, reason="refused" counter ticked +180 / right: retained:
    /// 3`. Certifies: one skew observation cannot consume the
    /// accumulated denominator backlog.
    #[tokio::test(start_paused = true)]
    async fn auth_skew_pass_retains_entire_backlog() {
        use metrics_util::debugging::DebuggingRecorder;
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        // Three windows banked across a simulated prior scheduler
        // outage (three flush ticks, no delivery) — the backlog the
        // conservation law exists to preserve.
        let mut unshipped = minted_backlog(3);
        let uids_before: Vec<String> = unshipped
            .iter()
            .map(|s| s.uid.as_str().to_owned())
            .collect();
        let total_in: f64 = unshipped.iter().map(|s| s.secs).sum();
        let shutdown = rio_common::signal::Token::new();
        // The skew pass: every append answered with a REAL auth-layer
        // refusal, classified through the production chokepoint.
        let pass = ship_all(&mut unshipped, &shutdown, EXPOSURE_SHIP_PASS_BUDGET, |_| {
            let status = tonic::Status::unauthenticated("hmac verify failed: unknown key id");
            std::future::ready(classify_append_status(status.code()))
        })
        .await;
        assert_eq!(
            pass,
            ShipPass::Completed {
                retained: 3,
                refused: 0
            },
            "a skew pass retains the whole backlog — zero counted drops"
        );
        let uids_after: Vec<String> = unshipped
            .iter()
            .map(|s| s.uid.as_str().to_owned())
            .collect();
        assert_eq!(
            uids_before, uids_after,
            "slices retained VERBATIM (uids intact)"
        );
        assert!(
            unshipped.iter().all(|s| s.auth_strikes == 1),
            "exactly one strike per slice per skew pass"
        );
        // The post-skew pass (fresh mint verifies): the outage-retained
        // backlog delivers whole.
        let mut delivered = 0.0f64;
        let pass2 = ship_all(
            &mut unshipped,
            &shutdown,
            EXPOSURE_SHIP_PASS_BUDGET,
            |s: &PendingExposure| {
                delivered += s.secs;
                std::future::ready(ShipOutcome::Delivered)
            },
        )
        .await;
        assert_eq!(
            pass2,
            ShipPass::Completed {
                retained: 0,
                refused: 0
            }
        );
        assert_eq!(
            delivered, total_in,
            "Σ in == Σ delivered — the conservation loop closes after the skew"
        );
        assert!(unshipped.is_empty());
        // Snapshot EXACTLY ONCE (DebuggingRecorder drains on
        // snapshot): the reason="refused" series must not exist.
        let snapshot = rec.snapshotter().snapshot().into_vec();
        let refused_series = snapshot.into_iter().any(|(k, _, _, _)| {
            let key = k.key();
            key.name() == "rio_controller_spot_exposure_dropped_seconds_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "refused")
        });
        assert!(
            !refused_series,
            "the drop counter must not tick under rotation skew"
        );
    }

    // r[verify sec.authz.refusal-adjudication]
    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_013 red R-1D (R17's violating red for
    /// `AUTH_STRIKE_BUDGET`): a slice carried to `AUTH_STRIKE_BUDGET -
    /// 1` strikes through the production path (15 skew passes, one
    /// attempt each — the one-rotation law is the pacing) survives
    /// each pass; the budget-reaching observation exits it through
    /// the counted chokepoint, tallied in `refused`, with the slice's
    /// whole seconds on `reason="refused"`. Red pre-fix: the strike
    /// axis is unrepresentable in the old alphabet — DISCLOSED
    /// STRAWMAN mapping (the pre-fix arm counted-drops at strike 1;
    /// this red's retention-until-budget law fails immediately at
    /// 83e596f0c: `left: Completed { retained: 0, refused: 1 } at
    /// pass 1 / right: retained: 1 through pass 15`). Certifies: the
    /// permanent exit requires exactly N observations and is
    /// disclosed with the count — the persistent-misconfig
    /// disposition, typed.
    #[tokio::test(start_paused = true)]
    async fn auth_strike_budget_is_violable() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);
        let mut unshipped = minted_backlog(1);
        let shutdown = rio_common::signal::Token::new();
        // Alternate the auth pair across passes — both codes are one
        // strike each (the adversarial mixed-code population).
        let skew = |pass_n: u32| {
            let status = if pass_n.is_multiple_of(2) {
                tonic::Status::unauthenticated("hmac verify failed: unknown key id")
            } else {
                tonic::Status::permission_denied("service caller not allowed")
            };
            classify_append_status(status.code())
        };
        for pass_n in 1..AUTH_STRIKE_BUDGET {
            let pass = ship_all(&mut unshipped, &shutdown, EXPOSURE_SHIP_PASS_BUDGET, |_| {
                std::future::ready(skew(pass_n))
            })
            .await;
            assert_eq!(
                pass,
                ShipPass::Completed {
                    retained: 1,
                    refused: 0
                },
                "pass {pass_n}: below the budget the slice is retained"
            );
            assert_eq!(
                unshipped[0].auth_strikes, pass_n,
                "the strike ledger is monotone, one per observation"
            );
        }
        // The budget observation: strike 16 exits counted.
        let pass = ship_all(&mut unshipped, &shutdown, EXPOSURE_SHIP_PASS_BUDGET, |_| {
            std::future::ready(skew(AUTH_STRIKE_BUDGET))
        })
        .await;
        assert_eq!(
            pass,
            ShipPass::Completed {
                retained: 0,
                refused: 1
            },
            "the strike exit counts in `refused` — the Completed alphabet is unchanged"
        );
        assert!(
            unshipped.is_empty(),
            "the budget-reaching observation is the permanent exit"
        );
        // Snapshot EXACTLY ONCE: the slice's whole seconds landed on
        // the counted chokepoint.
        let snapshot = rec.snapshotter().snapshot().into_vec();
        let refused_secs = snapshot.into_iter().find_map(|(k, _, _, v)| {
            let key = k.key();
            (key.name() == "rio_controller_spot_exposure_dropped_seconds_total"
                && key
                    .labels()
                    .any(|l| l.key() == "reason" && l.value() == "refused"))
            .then_some(v)
        });
        match refused_secs {
            Some(DebugValue::Counter(n)) => {
                assert_eq!(n, 60, "exactly the exiting slice's whole seconds, once");
            }
            other => panic!("strike exit uncounted: no reason=refused series ({other:?})"),
        }
    }

    // r[verify sec.authz.refusal-adjudication]
    /// merged_bug_001-r6 red R-C, rewritten for merged_bug_013 (R-1B):
    /// the classification chokepoint is total over all 17
    /// `tonic::Code` variants, fail-open toward retention (drop only
    /// on proof of futility), and CANNOT CONTRADICT the exported
    /// refusal authority: for every code,
    /// `judge_refusal(PerRequestService, c) == JudgesPresentation ⟹
    /// classify == Transient { auth_strike: true }` and
    /// `== DisprovesRequest ⟹ classify == Refused` (append-specific
    /// extensions live only on `Undecided` codes). rustc's exhaustive
    /// matches keep both ends total; this table pins each arm's
    /// VALUE. TRUE RED at 83e596f0c (via the disclosed (ddddd)
    /// classifier strawman — the pre-fix arm expressed in the new
    /// alphabet): `left: Refused / right: Transient { auth_strike:
    /// true }` for both auth rows. Certifies: the consumer agrees
    /// with the authority cell-for-cell, by test — a divergent
    /// per-consumer fatal set cannot re-ship.
    #[test]
    fn classify_append_status_total_over_tonic_codes() {
        use rio_proto::refusal::{CredentialRegime, RefusalJudgment, judge_refusal};
        use tonic::Code;
        let refused = [
            Code::InvalidArgument,
            Code::OutOfRange,
            Code::Unimplemented,
            Code::FailedPrecondition,
        ];
        let auth_strike = [Code::Unauthenticated, Code::PermissionDenied];
        let transient = [
            Code::Ok,
            Code::Cancelled,
            Code::Unknown,
            Code::DeadlineExceeded,
            Code::NotFound,
            Code::AlreadyExists,
            Code::ResourceExhausted,
            Code::Aborted,
            Code::Internal,
            Code::Unavailable,
            Code::DataLoss,
        ];
        assert_eq!(
            refused.len() + auth_strike.len() + transient.len(),
            17,
            "every tonic::Code variant is pinned exactly once"
        );
        for code in refused {
            assert_eq!(
                classify_append_status(code),
                ShipOutcome::Refused,
                "{code:?} disproves the request — counted exit in the observing pass"
            );
        }
        for code in auth_strike {
            assert_eq!(
                classify_append_status(code),
                ShipOutcome::Transient { auth_strike: true },
                "{code:?} judges the presentation — retained under the strike budget"
            );
        }
        for code in transient {
            assert_eq!(
                classify_append_status(code),
                ShipOutcome::Transient { auth_strike: false },
                "{code:?} retains (fail-open toward retention; consume only on ack)"
            );
        }
        // The consumer-agreement law: classify may EXTEND the
        // authority only on `Undecided` codes — never contradict a
        // per-request ruling.
        for code in refused.into_iter().chain(auth_strike).chain(transient) {
            match judge_refusal(CredentialRegime::PerRequestService, code) {
                RefusalJudgment::JudgesPresentation => assert_eq!(
                    classify_append_status(code),
                    ShipOutcome::Transient { auth_strike: true },
                    "{code:?}: a presentation-judgment must ride the strike budget"
                ),
                RefusalJudgment::DisprovesRequest => assert_eq!(
                    classify_append_status(code),
                    ShipOutcome::Refused,
                    "{code:?}: a request-disproof must exit counted"
                ),
                RefusalJudgment::Undecided => {}
            }
        }
    }

    mod ship_all_props {
        use std::collections::VecDeque;

        use proptest::prelude::*;

        use super::super::{
            AUTH_STRIKE_BUDGET, EXPOSURE_SHIP_PASS_BUDGET, PendingExposure, ShipOutcome, ShipPass,
            ship_all,
        };
        use super::minted_backlog;

        fn outcome(cell: u8) -> ShipOutcome {
            match cell % 4 {
                0 => ShipOutcome::Delivered,
                1 => ShipOutcome::Transient { auth_strike: false },
                2 => ShipOutcome::Transient { auth_strike: true },
                _ => ShipOutcome::Refused,
            }
        }

        proptest! {
            /// merged_bug_001-r6 red R-E, extended for merged_bug_013:
            /// under ARBITRARY queue sizes × ARBITRARY multi-pass
            /// outcome scripts drawn from the FULL alphabet
            /// ({Delivered, Transient{auth_strike: false},
            /// Transient{auth_strike: true}, Refused}) — per pass,
            /// attempts ≤ |queue at pass start|; across the whole run
            /// the conservation identity holds with the strike-exit
            /// leg (Σ secs in == Σ delivered + Σ still-queued + Σ
            /// counted, where counted = request-disproving exits +
            /// strike-budget exits), and the surviving queue equals an
            /// independent per-uid strike model's prediction (slices
            /// exit exactly at their AUTH_STRIKE_BUDGET-th strike,
            /// never on interleaved non-auth transients). Paused-clock
            /// runtime with zero-advance closures: the budget arm
            /// structurally cannot exit. Certifies: the pass
            /// envelope's work leg and the strike ledger's exit law
            /// over the whole outcome alphabet, not a hand-picked
            /// script.
            #[test]
            fn ship_all_pass_work_bounded_by_queue(
                queue_n in 0u64..6,
                script in proptest::collection::vec(0u8..4, 0..120)
            ) {
                let rt = tokio::runtime::Builder::new_current_thread()
                    .enable_time()
                    .start_paused(true)
                    .build()
                    .expect("current-thread test runtime");
                let checked: Result<(), TestCaseError> = rt.block_on(async {
                    let mut unshipped: VecDeque<PendingExposure> = minted_backlog(queue_n);
                    let total_in: f64 = unshipped.iter().map(|s| s.secs).sum();
                    // Independent model: uid → strikes so far. The
                    // closure mirrors every observation; the model
                    // predicts which slices survive.
                    let mut model: std::collections::HashMap<String, u32> = unshipped
                        .iter()
                        .map(|s| (s.uid.as_str().to_owned(), 0u32))
                        .collect();
                    let mut cursor = 0usize;
                    let mut delivered_secs = 0.0f64;
                    let mut counted_secs = 0.0f64;
                    while !unshipped.is_empty() && cursor < script.len() {
                        let initial = unshipped.len();
                        let mut attempts = 0usize;
                        let pass = ship_all(
                            &mut unshipped,
                            &rio_common::signal::Token::new(),
                            EXPOSURE_SHIP_PASS_BUDGET,
                            |s: &PendingExposure| {
                                // Past-script attempts drain Delivered
                                // (the healed-scheduler tail).
                                let o = if cursor < script.len() {
                                    outcome(script[cursor])
                                } else {
                                    ShipOutcome::Delivered
                                };
                                cursor += 1;
                                attempts += 1;
                                let uid = s.uid.as_str().to_owned();
                                match o {
                                    ShipOutcome::Delivered => {
                                        delivered_secs += s.secs;
                                        model.remove(&uid);
                                    }
                                    ShipOutcome::Transient { auth_strike: false } => {}
                                    ShipOutcome::Transient { auth_strike: true } => {
                                        let strikes = model
                                            .get_mut(&uid)
                                            .expect("attempted slice is modeled");
                                        *strikes += 1;
                                        if *strikes >= AUTH_STRIKE_BUDGET {
                                            counted_secs += s.secs;
                                            model.remove(&uid);
                                        }
                                    }
                                    ShipOutcome::Refused => {
                                        counted_secs += s.secs;
                                        model.remove(&uid);
                                    }
                                }
                                std::future::ready(o)
                            },
                        )
                        .await;
                        prop_assert!(attempts <= initial, "attempts {} > initial {}", attempts, initial);
                        prop_assert!(
                            matches!(pass, ShipPass::Completed { .. }),
                            "zero-advance closures: the rotation, not the budget, ends the pass"
                        );
                    }
                    let queued_after: f64 = unshipped.iter().map(|s| s.secs).sum();
                    prop_assert_eq!(
                        total_in,
                        delivered_secs + queued_after + counted_secs,
                        "Σ in == Σ delivered + Σ still-queued + Σ counted (incl. strike exits)"
                    );
                    let mut surviving: Vec<String> =
                        unshipped.iter().map(|s| s.uid.as_str().to_owned()).collect();
                    surviving.sort();
                    let mut predicted: Vec<String> = model.keys().cloned().collect();
                    predicted.sort();
                    prop_assert_eq!(
                        surviving,
                        predicted,
                        "the queue survives exactly the model's prediction"
                    );
                    for s in &unshipped {
                        prop_assert_eq!(
                            s.auth_strikes,
                            model[s.uid.as_str()],
                            "per-slice strike ledger matches the independent model"
                        );
                        prop_assert!(
                            s.auth_strikes < AUTH_STRIKE_BUDGET,
                            "no surviving slice sits at or past the budget"
                        );
                    }
                    Ok(())
                });
                checked?;
            }
        }
    }

    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_033 red R6: a drain pass is bounded by the
    /// wall-clock budget — the remainder DEFERS (stays queued, zero
    /// drop ticks; deferral is not forfeiture). Recorded red at the
    /// pre-budget extraction (the whole backlog shipped serially in
    /// one arm body): `left: 10 / right: 3` — "budget 12s at
    /// 5s/attempt = 3 attempts".
    #[tokio::test(start_paused = true)]
    async fn ship_all_stops_at_pass_budget_and_defers_remainder() {
        use metrics_util::debugging::DebuggingRecorder;
        let mut unshipped = minted_backlog(10);
        assert_eq!(unshipped.len(), 10);

        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);

        let shutdown = rio_common::signal::Token::new();
        let mut attempts = 0u32;
        let pass = ship_all(&mut unshipped, &shutdown, Duration::from_secs(12), |_| {
            attempts += 1;
            async {
                // Paused clock: each shipment costs 5 virtual
                // seconds and fails transiently (the
                // stalled-scheduler shape). This is the DEADLINE
                // witness — the time-advancing closure is the budget
                // arm's own legitimate domain (merged_bug_001-r6:
                // the attempts-bound witnesses advance zero time).
                tokio::time::advance(Duration::from_secs(5)).await;
                ShipOutcome::Transient { auth_strike: false }
            }
        })
        .await;
        assert_eq!(attempts, 3, "budget 12s at 5s/attempt = 3 attempts");
        assert_eq!(
            pass,
            ShipPass::BudgetExhausted { remaining: 10 },
            "the pass exits through the budget arm with the full backlog pending"
        );
        assert_eq!(
            unshipped.len(),
            10,
            "deferral is not forfeiture: every slice still queued"
        );
        assert!(
            rec.snapshotter().snapshot().into_vec().is_empty(),
            "zero drop ticks — budget deferral is PENDING, never a drop"
        );
    }

    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_033 red R7: shutdown preempts the IN-FLIGHT ship —
    /// the combinator returns within one poll of cancellation, the
    /// preempted slice re-queues under an IDENTICAL EventUid (the
    /// WO-1 coupling: the aborted append is exactly the ambiguous
    /// commit-or-not the deterministic uid redelivers into), and the
    /// rest go unattempted. Recorded red at the pre-cancel
    /// extraction: `Err(Elapsed(()))` — the combinator never returns
    /// while a ship hangs.
    #[tokio::test]
    async fn ship_all_preempts_in_flight_ship_on_cancellation() {
        let mut unshipped = minted_backlog(10);
        let in_flight_uid = unshipped[1].uid.clone();

        let shutdown = rio_common::signal::Token::new();
        let token = shutdown.clone();
        let mut calls = 0u32;
        let pass = tokio::time::timeout(
            Duration::from_secs(1),
            ship_all(&mut unshipped, &shutdown, EXPOSURE_SHIP_PASS_BUDGET, |_| {
                calls += 1;
                let cancel_now = calls == 2;
                let token = token.clone();
                async move {
                    if cancel_now {
                        // Slice 2 hangs forever; shutdown fires
                        // mid-flight.
                        token.cancel();
                        std::future::pending::<ShipOutcome>().await
                    } else {
                        ShipOutcome::Delivered
                    }
                }
            }),
        )
        .await
        .expect("ship_all must return promptly under cancellation");
        assert_eq!(
            pass,
            ShipPass::Cancelled {
                requeued_in_flight: true
            },
            "preemption exits through the Cancelled arm"
        );
        assert_eq!(calls, 2, "slices after the preempted one are unattempted");
        assert_eq!(
            unshipped.len(),
            9,
            "slice 1 delivered; in-flight slice 2 re-queued; 3..10 untouched"
        );
        assert_eq!(
            unshipped.back().expect("requeued slice").uid,
            in_flight_uid,
            "the in-flight slice re-queues under an IDENTICAL EventUid"
        );
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    // r[verify ctrl.informer.exposure-drain-budget+3]
    /// merged_bug_033 red R8: conservation across preemption — every
    /// node-second entering a cancelled pass is either ACKED or still
    /// QUEUED (the cancel-requeue is PENDING, not a drop; the counted
    /// whole-backlog forfeiture belongs to the caller's shutdown arm
    /// alone).
    #[tokio::test]
    async fn cancelled_pass_conserves_every_slice() {
        let mut unshipped = minted_backlog(6);
        let total_in: f64 = unshipped.iter().map(|s| s.secs).sum();

        let shutdown = rio_common::signal::Token::new();
        let token = shutdown.clone();
        let mut calls = 0u32;
        let mut acked = 0.0f64;
        let pass = tokio::time::timeout(
            Duration::from_secs(1),
            ship_all(
                &mut unshipped,
                &shutdown,
                EXPOSURE_SHIP_PASS_BUDGET,
                |s: &PendingExposure| {
                    calls += 1;
                    let cancel_now = calls == 4;
                    if !cancel_now {
                        acked += s.secs;
                    }
                    let token = token.clone();
                    async move {
                        if cancel_now {
                            token.cancel();
                            std::future::pending::<ShipOutcome>().await
                        } else {
                            ShipOutcome::Delivered
                        }
                    }
                },
            ),
        )
        .await
        .expect("conservation pass must return under cancellation");
        assert_eq!(
            pass,
            ShipPass::Cancelled {
                requeued_in_flight: true
            }
        );
        let queued_after: f64 = unshipped.iter().map(|s| s.secs).sum();
        assert_eq!(
            total_in,
            acked + queued_after,
            "Σ in == Σ acked + Σ queued-after-Cancelled (nothing vanishes \
             across preemption)"
        );
    }

    mod window_gate_props {
        use proptest::prelude::*;

        use super::super::{EXPOSURE_FLUSH_SECS, WindowGate};

        proptest! {
            /// Rp (merged_bug_001 formal disposition): the gate's two
            /// laws under ARBITRARY clocks — NaN, ±inf, 0.0, negative,
            /// backward steps, repeats: every ADMITTED `WindowId`
            /// strictly increases and is ≡ 0 mod
            /// `EXPOSURE_FLUSH_SECS`. (kani n/a — f64 floor-division
            /// domain; this proptest + R2 pin the laws inside the
            /// normal nextest gate.)
            #[test]
            fn window_gate_monotone_and_grid_under_arbitrary_epochs(
                epochs in proptest::collection::vec(proptest::num::f64::ANY, 1..64)
            ) {
                let mut gate = WindowGate::default();
                let mut last: Option<u64> = None;
                for e in epochs {
                    if let Some(w) = gate.admit(e) {
                        let slot = w.slot_secs();
                        prop_assert_eq!(slot % EXPOSURE_FLUSH_SECS, 0, "grid law");
                        if let Some(prev) = last {
                            prop_assert!(slot > prev, "monotone law: {} -> {}", prev, slot);
                        }
                        last = Some(slot);
                    }
                }
            }
        }
    }

    // r[verify ctrl.informer.interrupt-sample-conservation+2]
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
        let d = sorted(flush_spot_exposure(&mut cursors, &nodes, &cfg, 0.0, 1060.0).banked);
        assert_eq!(d, vec![("intel-6".into(), 40.0), ("intel-7".into(), 120.0)]);

        // Second flush at t=1120 over the SAME LIST: deltas only (60s
        // each), not cumulative-from-created — the cursor advanced.
        let d = sorted(flush_spot_exposure(&mut cursors, &nodes, &cfg, 0.0, 1120.0).banked);
        assert_eq!(d, vec![("intel-6".into(), 60.0), ("intel-7".into(), 120.0)]);

        // On-demand node never grew a cursor.
        assert!(!cursors.contains_key("od"));
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// §4(a)2 gate (capacity-type / match gating): spot nodes whose
    /// labels match no configured `$h` advance their cursor WITHOUT
    /// banking — a late config load does not retro-bank the unmatched
    /// window (same semantics as the deleted cache). merged_bug_070(a)
    /// red: the consumed window MUST exit through the counted
    /// chokepoint — `left:` pre-fix it vanished (cursor advanced,
    /// nothing banked, nothing counted — a silent third forfeiture
    /// the leg's own conservation claim denied); `right:` a
    /// `no_hw_class` drop carries the 60 seconds.
    #[test]
    fn flush_unmatched_window_is_dropped_counted_not_retro_banked() {
        let unloaded = HwClassConfig::default();
        let loaded = band_config();
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let nodes = vec![spot_node("a", "7", 1000)];

        // Flush at t=1060 with config not yet loaded: nothing banked,
        // the cursor still advances to 1060 — and the consumed window
        // is COUNTED.
        let out = flush_spot_exposure(&mut cursors, &nodes, &unloaded, 0.0, 1060.0);
        assert!(out.banked.is_empty(), "unmatched spot node banks nothing");
        assert_eq!(cursors.get("a").copied(), Some(1060.0));
        assert_eq!(
            out.drops,
            vec![(ExposureDropReason::NoHwClass, 60.0)],
            "the consumed window exits counted, not silently"
        );

        // Config loads; flush at t=1120 banks ONLY 1060..1120 — the
        // unmatched 1000..1060 window stays dropped (no retro-bank),
        // and nothing further drops.
        let out = flush_spot_exposure(&mut cursors, &nodes, &loaded, 0.0, 1120.0);
        assert_eq!(out.banked, vec![("intel-7".into(), 60.0)]);
        assert!(out.drops.is_empty());
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// §4(a)2 gate (the recorded accepted under-count): a node absent
    /// from the LIST has its cursor dropped without banking the final
    /// partial slice — and merged_bug_070(c) red: the forfeited
    /// residual MUST exit through the counted chokepoint, one
    /// `absent_node` drop per departed node (`left:` pre-fix the
    /// residual vanished uncounted — and under a LIST-failure streak
    /// the forfeit grows past the doc's claimed one-window bound with
    /// no trace; `right:` counted, bound honest). λ reads marginally
    /// high — the cost-conservative direction (the solver
    /// under-prefers spot; never the phantom-exposure over-count of
    /// bug `b81da271f`). Successor of
    /// `prune_absent_evicts_nodes_missing_from_relist`.
    #[test]
    fn flush_drops_absent_node_cursors_counted() {
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
        let _ = flush_spot_exposure(&mut cursors, &all, &cfg, 0.0, 1060.0);
        assert_eq!(cursors.len(), 4, "four spot nodes tracked");

        // b, b2, od deleted between flushes → the t=1090 LIST has only
        // {a, c}. Their 1060..1090 residuals are forfeited — COUNTED —
        // and their cursors are dropped.
        let survivors = vec![spot_node("a", "7", 1000), spot_node("c", "6", 1020)];
        let out = flush_spot_exposure(&mut cursors, &survivors, &cfg, 0.0, 1090.0);
        assert_eq!(
            sorted(out.banked),
            vec![("intel-6".into(), 30.0), ("intel-7".into(), 30.0)],
            "only survivors bank"
        );
        assert_eq!(
            out.drops,
            vec![
                (ExposureDropReason::AbsentNode, 30.0),
                (ExposureDropReason::AbsentNode, 30.0),
            ],
            "each departed node's residual exits counted (od never had \
             a cursor — on-demand contributes nothing)"
        );
        assert_eq!(cursors.len(), 2);
        assert!(!cursors.contains_key("b"));
        assert!(!cursors.contains_key("b2"));

        // Next flush at t=1120: still only the survivors — no phantom
        // node-seconds from the departed nodes (the b81da271f hazard
        // the watch needed `prune_absent` for cannot exist here).
        let out = flush_spot_exposure(&mut cursors, &survivors, &cfg, 0.0, 1120.0);
        assert_eq!(
            sorted(out.banked),
            vec![("intel-6".into(), 30.0), ("intel-7".into(), 30.0)]
        );
        assert!(out.drops.is_empty());
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_070(d) red: a controller RESTART must not re-bank
    /// windows the previous incarnation already shipped. Cursor seeds
    /// clamp to `max(creationTimestamp, boot_epoch)` — `left:` pre-fix
    /// the seed was bare creationTimestamp, so the first post-restart
    /// flush re-banked every surviving spot node's WHOLE lifetime
    /// (un-keyed appends → denominator inflation → λ biased LOW → the
    /// solver over-prefers spot, the exact polarity the absent-node
    /// design note says the leg never takes); `right:` the
    /// post-restart flush banks only [boot, now].
    #[test]
    fn flush_restart_seeds_at_boot_not_creation() {
        let cfg = band_config();
        // Fresh process (empty cursors = restart), node born at 1000,
        // process boot at 2000, first flush at 2060.
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let nodes = vec![spot_node("a", "7", 1000)];
        let out = flush_spot_exposure(&mut cursors, &nodes, &cfg, 2000.0, 2060.0);
        assert_eq!(
            out.banked,
            vec![("intel-7".into(), 60.0)],
            "only the post-boot slice banks (pre-fix: 1060s — the \
             node's whole lifetime re-banked on every restart)"
        );
        // A node born AFTER boot still seeds from creation (its
        // pre-first-flush lifetime is this incarnation's to bank).
        let mut cursors: HashMap<String, f64> = HashMap::new();
        let young = vec![spot_node("y", "7", 2030)];
        let out = flush_spot_exposure(&mut cursors, &young, &cfg, 2000.0, 2060.0);
        assert_eq!(out.banked, vec![("intel-7".into(), 30.0)]);
    }

    // r[verify ctrl.informer.exposure-recredit+4]
    /// merged_bug_070(b) red: shutdown forfeits the WHOLE pending
    /// backlog — the spec's old "at most one pending window" bound was
    /// false (each failed flush queues another window; the carrier is
    /// process memory with no drain). `left:` two pending windows
    /// vanished silently at shutdown while the doc claimed ≤1 could;
    /// `right:` every queued slice exits through the counted
    /// chokepoint (the run-loop shutdown arm drains
    /// `unshipped` → `record_exposure_drop(Shutdown, …)` per slice),
    /// and the spec enumerates the forfeiture honestly.
    #[test]
    fn shutdown_backlog_is_counted_per_slice() {
        let cluster = ClusterId::new("prod-eu");
        let mut gate = WindowGate::default();
        let mut unshipped: VecDeque<PendingExposure> = VecDeque::new();
        // Two failed windows accumulate (the brownout shape).
        let w1 = gate.admit(1000.0).expect("first admit");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w1);
        let w2 = gate.admit(1060.0).expect("next slot admits");
        queue_exposure_slices(&mut unshipped, vec![("m6id".into(), 60.0)], &cluster, w2);
        assert_eq!(unshipped.len(), 2, "backlog: one slice per window");
        // The shutdown arm's exact mapping: one counted drop per
        // slice, seconds preserved.
        let drops: Vec<(ExposureDropReason, f64)> = unshipped
            .drain(..)
            .map(|s| (ExposureDropReason::Shutdown, s.secs))
            .collect();
        assert_eq!(
            drops,
            vec![
                (ExposureDropReason::Shutdown, 60.0),
                (ExposureDropReason::Shutdown, 60.0),
            ],
            "the WHOLE backlog is forfeited and counted — not one window"
        );
        assert!(unshipped.is_empty());
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
        let d = flush_spot_exposure(&mut cursors, &nodes, &cfg, 0.0, 1060.0).banked;
        assert_eq!(d, vec![("intel-7".into(), 60.0)]);
        // Same node, fresh LIST objects (a relist): banks 30s, not 110s.
        let relisted = vec![spot_node("a", "7", 1000)];
        let d = flush_spot_exposure(&mut cursors, &relisted, &cfg, 0.0, 1090.0).banked;
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

    // r[verify ctrl.informer.interrupt-sample-conservation+2]
    /// merged_bug_116: a delivery failure exits through the counted
    /// chokepoint. Recorded red (pre-fix): the append-Err arm only
    /// warned — observed = appended + Σ dropped was false exactly when
    /// the scheduler was unreachable.
    #[tokio::test]
    async fn append_failure_is_a_counted_drop() {
        use metrics_util::debugging::{DebugValue, DebuggingRecorder};
        let (mock, addr, _server) = rio_test_support::grpc::spawn_mock_admin()
            .await
            .expect("mock admin");
        let channel = tonic::transport::Endpoint::try_from(format!("http://{addr}"))
            .expect("endpoint")
            .connect()
            .await
            .expect("connect");
        let mut admin = rio_proto::AdminServiceClient::with_interceptor(
            channel,
            rio_auth::hmac::ServiceTokenInterceptor::new(None, "rio-controller"),
        );

        let dropped = |rec: &DebuggingRecorder, reason: &str| {
            rec.snapshotter()
                .snapshot()
                .into_vec()
                .into_iter()
                .find_map(|(k, _, _, v)| {
                    let key = k.key();
                    (key.name() == "rio_controller_spot_interrupt_dropped_total"
                        && key
                            .labels()
                            .any(|l| l.key() == "reason" && l.value() == reason))
                    .then_some(v)
                })
        };

        let rec = DebuggingRecorder::new();
        let _g = ::metrics::set_default_local_recorder(&rec);

        // Successful delivery: appended, nothing dropped.
        let ok = attribute_interrupt(Ok(Some(Some("mid-ebs-x86".into()))), None, None)
            .expect("attributed");
        deliver_interrupt_sample(&mut admin, "node-1", ok).await;
        assert_eq!(mock.interrupt_samples.read().unwrap().len(), 1);
        assert!(dropped(&rec, "append_failed").is_none());

        // Programmed failure: the sample exits through the chokepoint.
        mock.fail_next_append
            .store(true, std::sync::atomic::Ordering::SeqCst);
        let lost = attribute_interrupt(Ok(Some(Some("mid-ebs-x86".into()))), None, None)
            .expect("attributed");
        deliver_interrupt_sample(&mut admin, "node-1", lost).await;
        match dropped(&rec, "append_failed") {
            Some(DebugValue::Counter(n)) => assert_eq!(
                n, 1,
                "append failure must be a counted drop (conservation identity)"
            ),
            other => panic!("append failure uncounted: no append_failed drop series ({other:?})"),
        }
    }
}
