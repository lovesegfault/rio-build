//! ADR-023 §13b @alg-pool: forecast-driven NodeClaim provisioning.
//!
//! Replaces the 12 static `band×storage×arch` Karpenter NodePools with
//! ONE inert shim pool (`limits:{cpu:0}`) plus this reconciler creating
//! NodeClaims directly. Per tick (`TICK = 10s`, the GetSpawnIntents poll
//! cadence):
//!
//! 1. Poll `AdminService.GetSpawnIntents` for the scheduler's per-drv
//!    `(cores, mem, disk)` forecast. ⊥ on RPC failure → after
//!    `BOT_TICKS_BEFORE_CONSOLIDATE_ONLY` consecutive ⊥-ticks, switch to
//!    consolidate-only (don't grow the fleet on stale data).
//! 2. LIST the owned NodeClaims and the `rio.build/pool` pods (one
//!    label-selected Pod LIST per tick → `pods::PodSnapshot`: per-node
//!    requested sums + the bound-intent index), then FFD-simulate
//!    placing the intents onto live (Registered + in-flight) NodeClaims
//!    with the same MostAllocated bin-select that
//!    `kube-scheduler-packed` uses.
//! 3. Cover the unplaced deficit per `(hw_class, capacity_type)` cell
//!    with 1×anchor + N×bulk NodeClaims, capped at
//!    `max_node_claims_per_cell_per_tick` and `max_fleet_cores`.
//! 4. Reap idle Registered claims via windowed-rate break-even.
//! 5. Reap unhealthy (OA2 wedge-clustered dead nodes) and ICE-stuck
//!    claims.
//! 6. Persist `CellSketches` (lead-time quantile sketches + idle-gap log) to PG.
//!
//! Lease-gated: only the leader replica runs `reconcile_once`. The
//! lease makes rolling-upgrade surge safe for THIS reconciler — the
//! surge replica idles until the old one releases. Controller stays
//! `replicas: 1` (the Pool reconciler and gc_schedule are NOT
//! lease-gated; two replicas would double-spawn Jobs — see
//! controller.yaml `replicas: 1` rationale).

mod consolidate;
mod cover;
pub(crate) mod ffd;
mod health;
#[cfg(test)]
mod lifecycle_tests;
mod pods;
pub mod sketch;
mod wedge;

use std::collections::{BTreeMap, HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use k8s_openapi::api::core::v1::Pod;
use kube::api::{Api, ListParams, PostParams};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use rio_crds::karpenter::NodeClaim;
use rio_crds::pool::{ExecutorKind, Pool};
use rio_lease::LeaderState;
use rio_proto::types::{
    AckSpawnedIntentsRequest, GetSpawnIntentsRequest, GetSpawnIntentsResponse,
    ListOpenAttemptsRequest, SpawnIntent,
};

use crate::reconcilers::node_informer::HwClassConfig;
use crate::reconcilers::pool;
use crate::reconcilers::{AdminClient, admin_call};

pub use consolidate::{HOLD_OPEN_ANNOTATION, IdleGapEvent};
pub use cover::{NODEPOOL_LABEL, SHIM_NODEPOOL};
pub use ffd::{
    ARCH_LABEL, CAPACITY_TYPE_LABEL, HW_CLASS_LABEL, LiveNode, Placement, a_open, cells_of,
    system_to_arch,
};
pub use sketch::{CapacityType, Cell, CellSketches, CellState};

/// Scheduler-bound evidence from kube-only observation
/// (merged_bug_007). The producer edges are consume-once, so a value
/// of this type must never be dropped — `#[must_use]` turns the old
/// `let _ =` discard into a deny-warnings error; ticks that cannot
/// ship it merge it into the reconciler's buffer instead.
#[must_use = "kube-only evidence is consume-once: merge it into pending_evidence (shipped from the buffer, cleared only on Ack-Ok)"]
#[derive(Debug, Default)]
pub(crate) struct PendingSchedulerEvidence {
    /// Cells whose NodeClaim reached `Registered=True` (the ICE-clear
    /// signal). `BTreeSet`: dedup across merged ticks + deterministic
    /// wire order.
    pub(crate) registered_cells: std::collections::BTreeSet<Cell>,
    /// Per-cell instance types Karpenter resolved (CostTable feed).
    /// Deduped by full tuple on merge.
    pub(crate) observed_types: Vec<rio_proto::types::ObservedInstanceType>,
    /// Cells whose NodeClaim was reaped for ICE (the ICE-mark signal,
    /// bug_082). The producers are consume-once edges — `record_reap`
    /// fires at the instant the claim is deleted, `detect_vanished`
    /// removes its tracking entry in the same retain that emits — so a
    /// mark that misses its Ack is gone forever unless it lives here.
    /// Commit-on-Ack like the sibling planes; `BTreeSet` dedups within
    /// a request (the scheduler ladder steps once per entry). A mark
    /// re-delivered after an ambiguous Ack timeout advances the ladder
    /// one extra step — strictly conservative against re-minting into
    /// a cell that provably ICE'd, and strictly better than losing the
    /// mark.
    pub(crate) ice_cells: std::collections::BTreeSet<Cell>,
}

impl PendingSchedulerEvidence {
    /// Merge another tick's evidence (dedup all three planes).
    pub(crate) fn merge(&mut self, other: PendingSchedulerEvidence) {
        self.registered_cells.extend(other.registered_cells);
        for o in other.observed_types {
            if !self.observed_types.iter().any(|e| {
                e.cell == o.cell
                    && e.instance_type == o.instance_type
                    && e.cores == o.cores
                    && e.mem_bytes == o.mem_bytes
            }) {
                self.observed_types.push(o);
            }
        }
        self.ice_cells.extend(other.ice_cells);
    }
}

/// Reconcile interval. Matches the Pool reconciler's `GetSpawnIntents`
/// poll cadence so the scheduler's `compute_spawn_intents` snapshot is
/// no staler here than in the legacy spawn path.
pub(crate) const TICK: Duration = Duration::from_secs(10);

/// Consecutive ⊥ ticks (scheduler unreachable / `Unavailable`) before
/// the loop drops into consolidate-only. ADR §13b: don't grow the fleet
/// on stale data, but DO keep reaping idle/unhealthy nodes — those reads
/// are kube-only.
const BOT_TICKS_BEFORE_CONSOLIDATE_ONLY: u8 = 5;

/// Unix-epoch seconds of `t`. Condition `lastTransitionTime` and
/// `creationTimestamp` are RFC3339; comparing in epoch-seconds keeps
/// arithmetic in `f64` throughout. The tick path threads one
/// `SystemTime` from [`NodeClaimPoolReconciler::tick`] (sampled once
/// per tick in `run()`, injectable by the lifecycle-invariants suite)
/// and derives epoch-seconds through this at the top of each body.
fn epoch_secs(t: std::time::SystemTime) -> f64 {
    t.duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// `(kind, systems, features)` extracted from a Builder/Fetcher Pool.
/// The provisioner's coverage filter ([`pool_covers`]) checks intents
/// against the union of these — the SAME `(kind, systems, features)`
/// tuple the placer (`pool/jobs::queued_for_pool`) sends per-Pool to
/// `GetSpawnIntents{filter_features: true}`.
type PoolCoverage = (rio_proto::types::ExecutorKind, Vec<String>, Vec<String>);

/// §13d Pool axis (r31 bug_019): does ANY configured Builder or Fetcher
/// Pool place a Job for `intent`? Mirrors the scheduler's
/// `passes_intent_filter` — a 3-axis (`kind`, `systems`, `features`)
/// predicate. `systems=[]` means no system filter (a Pool with no
/// `systems` constraint accepts every arch), and `features` is checked
/// via the SAME `features_compatible` predicate
/// (`pool/jobs::queued_for_pool` sends `filter_features=true` per
/// Pool). The `retain_hosting_cells` chokepoint validates the *cell*
/// axis (some hwClass hosts the intent); this validates the *Pool*
/// axis (some Pool consumes the intent). An intent that's
/// cell-routable but Pool-uncovered would mint a NodeClaim that
/// FFD-places (`reserved`) → `reap_idle` skips forever — a
/// permanently-idle on-demand metal node with no Job ever spawned.
///
/// r35 merged_bug_004 residual (B2): the `kind` axis is checked
/// EXPLICITLY, not assumed redundant with the features axis. The
/// `kind`/`features` redundancy holds only when the scheduler-side
/// FOD↔fetcher biconditional (`EffectiveFeatures::derive`, B0) holds —
/// but the controller has no view into whether that invariant held on
/// the wire. A scheduler regression that re-leaks `fetcher` into a
/// `kind=Builder` intent would otherwise mint a fetcher NodeClaim that
/// the placer never Job-places (the placer DOES check `intent.kind ==
/// pool.spec.kind`).
///
/// §13e: callers MUST build `coverage` from `effective_features(spec)`,
/// not `spec.features` — a Fetcher Pool's declared features are `[]`
/// (CEL-enforced) but its effective features are `[fetcher]`, and FOD
/// intents carry `required_features=[fetcher]`. Building from
/// `spec.features` would drop every FOD intent here (silent — the
/// `no_pool_covers` warn fires, but FFD never sees the demand and no
/// fetcher NodeClaim is minted).
///
/// Extracted for unit testability — the predicate is pure and the
/// failure mode (silent over-provisioning) doesn't surface in any
/// fast test path.
pub(super) fn pool_covers(intent: &SpawnIntent, coverage: &[PoolCoverage]) -> bool {
    coverage.iter().any(|(kind, systems, features)| {
        i32::from(*kind) == intent.kind
            && (systems.is_empty() || systems.contains(&intent.system))
            && rio_common::k8s::features_compatible(&intent.required_features, features)
    })
}

/// Label selector for NodeClaims this reconciler owns. Stamped at
/// `create()` time so `list_live_nodeclaims` and the consolidator never
/// touch claims from the rio-general NodePool (or any operator-managed
/// NodePool).
///
/// > ⚠ Note (§13e): the value `"builder"` is HISTORICAL — this is the
/// > *reconciler-ownership* selector, not a role label. Post-§13e the
/// > same reconciler owns fetcher NodeClaims too (it covers both
/// > Builder and Fetcher Pools — see `reconcile_once`). Renaming the
/// > value to `"shim"` is a label-value migration that requires
/// > `kubectl label nodeclaims rio.build/nodeclaim-pool- --all` on
/// > deploy; deferred to a follow-up.
pub const OWNER_LABEL: &str = "rio.build/nodeclaim-pool=builder";

/// `rio.build/node-role` label key/value stamped on every rio-minted
/// **builder** NodeClaim. The legacy band-loop NodePool template stamped
/// this; B3 deleted those NodePools, and builder pod affinity still
/// requires `node-role In [builder]` (helm `builder.nodeSelector`), so
/// `cover::build_nodeclaim` must stamp it directly. §13e: fetcher
/// NodeClaims get `(rio.build/node-role, "fetcher")` instead —
/// `cover::build_nodeclaim` branches on `provides_features ∋ fetcher`
/// (the same map the scheduler routes against). The role label is for
/// operator queries/dashboards only; the per-intent affinity matches
/// `rio.build/fetcher` (the §13e taint+label key from `hw.labels`).
pub const NODE_ROLE_LABEL: (&str, &str) = ("rio.build/node-role", "builder");

/// `intent_id` set FFD-placed on a `Registered=True` NodeClaim. `None`
/// = no FFD tick has published yet (first ~10s after start, or standby
/// replica whose lease-gated reconciler never runs).
type PlaceableSet = Option<Arc<HashSet<String>>>;

/// Receiver-side of the placeable-gate channel, held in [`super::Ctx`]
/// so the `pool/jobs` reconciler can read it. ADR-023 §13b: Jobs spawn
/// only for intents the FFD sim placed on a Registered node —
/// structurally closes the spawn-intent fan-out (1226 Ready intents →
/// 1226 Pending Jobs → Karpenter thrash) that the §13a
/// `intents.retain(|i| i.ready)` gate could not.
///
/// `watch` semantics: the Pool reconciler reads the latest snapshot
/// each tick (no event-per-publish). Staleness is bounded by the 10s
/// tick cadence on both sides **while the lease is held and the
/// scheduler is reachable**. During a scheduler outage the producer
/// (`reconcile_once`) early-returns on a ⊥ tick and, after
/// `BOT_TICKS_BEFORE_CONSOLIDATE_ONLY` consecutive ⊥-ticks, switches
/// to `consolidate_only` — neither path republishes, so the last-good
/// set persists for the duration. That staleness is benign by
/// construction: the only consumer (`pool/jobs::reconcile`) fetches
/// its own intent list from the SAME scheduler before calling
/// [`PlaceableGate::retain`], so when the producer is in
/// `consolidate_only` the consumer has `intents=[]` and
/// `scheduler_err.is_some()` — the stale set has nothing to filter and
/// `queued_known=None` keeps `reap_excess_pending` fail-closed. The
/// stale set is observable only across the recovery edge (≤1 tick) or
/// under a one-sided RPC flap, where it admits a bounded subset of
/// previously-confirmed-placeable IDs that self-corrects on the next
/// successful FFD tick. Lease loss is the contrasting case and DOES
/// reset to `None` (see [`ControllerLeaseHooks`]): there the stale set
/// belongs to a *different replica's* FFD state and would drive
/// `reap_excess_pending` against the new leader's Jobs.
/// `Arc<HashSet>` so `borrow().clone()` is O(1).
#[derive(Clone)]
pub struct PlaceableGate(tokio::sync::watch::Receiver<PlaceableSet>);

impl PlaceableGate {
    /// Retain only intents whose `intent_id` is in the last-published
    /// placeable set. Returns whether the gate is **armed** (a value has
    /// been published). `false` ⇔ no FFD tick has run yet — caller
    /// treats `queued` as unknown so `reap_excess_pending` stays
    /// fail-closed (a standby replica whose lease-gated reconciler never
    /// publishes would otherwise see `queued=0` and reap the leader's
    /// Pending Jobs).
    // r[impl ctrl.nodeclaim.placeable-gate+5]
    pub fn retain(&self, intents: &mut Vec<SpawnIntent>) -> bool {
        match self.0.borrow().clone() {
            Some(set) => {
                intents.retain(|i| set.contains(&i.intent_id));
                true
            }
            None => {
                intents.clear();
                false
            }
        }
    }

    /// Test-only: gate seeded with `ids` (armed).
    #[cfg(test)]
    pub fn from_ids<I: IntoIterator<Item = &'static str>>(ids: I) -> Self {
        let set: HashSet<String> = ids.into_iter().map(str::to_owned).collect();
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(set)));
        Self(rx)
    }

    /// Test-only: unarmed (no publish yet).
    #[cfg(test)]
    pub fn unarmed() -> Self {
        let (_tx, rx) = tokio::sync::watch::channel(None);
        Self(rx)
    }
}

/// Construct a placeable-gate channel pair. The sender is held by
/// [`NodeClaimPoolReconciler`]; the receiver wraps into [`PlaceableGate`]
/// in `Ctx`. Initial value `None` (unarmed) so the first Pool-reconcile
/// tick before the first FFD tick is fail-closed.
pub fn placeable_channel() -> (tokio::sync::watch::Sender<PlaceableSet>, PlaceableGate) {
    let (tx, rx) = tokio::sync::watch::channel(None);
    (tx, PlaceableGate(rx))
}

/// Layered-config-loaded config. Scalars via `RIO_NODECLAIM_POOL__*` env;
/// `lead_time_seed` via the `[nodeclaim_pool]` table in
/// `/etc/rio/controller.toml` (helm `rio-controller-config` ConfigMap) —
/// the RIO_ env layer yields bare strings, so nested map fields
/// cannot load from env.
#[derive(Debug, Clone, Serialize, Deserialize, schemars::JsonSchema)]
#[serde(default)]
pub struct NodeClaimPoolConfig {
    /// PostgreSQL URL for [`CellSketches`] persist/load. Same DB as
    /// store/scheduler (migration 059 lives there). Required —
    /// controller doesn't otherwise hold a PG handle.
    pub database_url: String,
    /// Lease object name for leader election. `None` → non-K8s mode
    /// (always-leader, see [`rio_lease::LeaseConfig::from_parts`]).
    pub lease_name: Option<String>,
    /// Lease namespace. `None` → in-cluster service-account mount.
    pub lease_namespace: Option<String>,
    /// `[sla].referenceHwClass` — the cold-start fallback cell for
    /// hw-agnostic intents (`fit=None` → `hw_class_names=[]`). See
    /// [`Self::fallback_cell`]. Helm: `sla.referenceHwClass` (same key
    /// the scheduler reads for ref-second normalization, so the
    /// controller's cold-start probes land on the normalization
    /// anchor).
    pub reference_hw_class: String,
    /// §13b deficit-cover budget cap (sum of `allocatable.cpu` across
    /// all owned NodeClaims, Registered + in-flight). Helm:
    /// `sla.maxFleetCores`.
    pub max_fleet_cores: u32,
    /// §13b per-cell-per-tick NodeClaim create cap. Prevents one cell's
    /// burst from monopolizing a tick's budget. Helm:
    /// `sla.maxNodeClaimsPerCellPerTick`.
    pub max_node_claims_per_cell_per_tick: u32,
    /// §13b lead-time Schmitt clamp ceiling (seconds). Helm:
    /// `sla.maxLeadTime`.
    pub max_lead_time: f64,
    /// §13b consolidator hold-open threshold (seconds) for
    /// `rio.build/hold-open`-annotated NodeClaims. `None` =
    /// 2×`consolidate_after()` per ADR. NOT a ceiling: when set below
    /// the NA break-even, `consolidate::hold_open_threshold` clamps
    /// to ≥ `na` (r38 merged_004 — annotation cannot lower the
    /// threshold). Helm: `scheduler.sla.maxConsolidationTime`
    /// (rendered by `controller.yaml` and `scheduler.yaml`).
    /// r37 merged_010: the model floor `max(boot_median/2, min)` wins
    /// when this is set below it (`max_t.max(floor)` clamp in
    /// `consolidate_after`).
    pub max_consolidation_time: Option<f64>,
    /// r35 bug_050: per-hw-class-prefix `consolidate_after` floor
    /// (seconds). §13e routed Fetcher Pools through `nodeclaim_pool`,
    /// silently dropping Karpenter's `consolidateAfter: 10m` to the
    /// NA-model floor `boot_median/2` (~15s) — a fetcher node burns a
    /// boot every 15s of lull instead of holding 10min. The NA model is
    /// correct (it observed the right λ); the *policy floor* was lost
    /// in the migration. Keys are class names or `<prefix>*` globs
    /// matched against `cell.0` (most-specific exact match wins, then
    /// longest prefix). Default `{"fetcher-*": 600.0, "*": 300.0}`:
    /// fetchers restore the pre-§13e Karpenter behavior; builders get a
    /// 300s floor (`boot_median/2 ≈ 9s` is below the ~18s boot cost it's
    /// supposed to amortize — reaping there is strictly dominated; see
    /// [`Self::default`]). Helm:
    /// `karpenter.nodeclaimPool.minConsolidationTime`.
    ///
    /// `BTreeMap` (not `HashMap`) so the default serializes in a stable
    /// key order into the frozen config-schema snapshot; the lookup in
    /// [`Self::min_consolidation_time_for`] is order-independent.
    pub min_consolidation_time: BTreeMap<String, f64>,
    /// `(hw_class:cap)` → seed lead-time seconds, written by
    /// `xtask k8s probe-boot`. Seeds the lead-time sketch on cold start.
    /// Helm: `sla.leadTimeSeed`.
    pub lead_time_seed: HashMap<String, f64>,
    /// Seed lead-time (seconds) for cells absent from
    /// [`Self::lead_time_seed`]. The seed feeds [`Self::seed_for`] —
    /// `health::classify` uses it as a TIMEOUT, not just a floor, so
    /// 0 would reap every NodeClaim before it can register (~18s
    /// real boot). Helm: `sla.defaultLeadTimeSeed`.
    pub default_lead_time_seed: f64,
    /// Quantile-sketch active→shadow rotation interval (seconds). After
    /// `2×halflife` a sample has aged out entirely. Helm: not surfaced;
    /// 6h default per ADR §13b.
    pub sketch_halflife_secs: u64,
    /// Per-NodeClaim `resources.requests.ephemeral-storage` ceiling.
    /// Helm: derived from `karpenter.dataVolumeSize` × allocatable
    /// fraction (kubelet reserve ≈10%). nvme cells get instance-store
    /// (much larger) so this only binds ebs cells.
    pub max_node_disk: u64,
    /// `karpenter.k8s.aws/instance-size NotIn` values appended to
    /// every NodeClaim's `spec.requirements` — the metal partition
    /// (I-205). Helm: `karpenter.metalSizes`. Empty (kwok/vmtest) → no
    /// instance-size requirement emitted.
    pub metal_sizes: Vec<String>,
    /// FUSE-cache budget added to every builder pod's
    /// `ephemeral-storage` request (the `fuse-cache` emptyDir). Single
    /// source for ALL Builder-pool callers via
    /// [`pool::pod::BUILDER_FUSE_CACHE`] — the NodeClaim's
    /// `ephemeral-storage` floor and the pod's actual request both read
    /// this so FFD/cover/stamp agree (§Simulator-shares-accounting).
    /// Helm: `poolDefaults.fuseCacheBytes` (50Gi prod). Default is the
    /// controller-config fallback `pool::pod::BUILDER_FUSE_CACHE_BYTES`
    /// (8Gi).
    pub fuse_cache_bytes: u64,
    /// FUSE-cache budget for FETCHER pods (the `fuse-cache` emptyDir
    /// sizeLimit and the matching `ephemeral-storage` addend). A FOD's
    /// FUSE cache only ever holds the fetch script's input closure
    /// (curl/git/JDK-class toolchains), not the downloaded artifact
    /// (that lands in the overlay emptyDir, sized from `disk_bytes`,
    /// which grows via the reactive disk floor on eviction) — so this
    /// is a small static bound, not the builder budget. Single source
    /// for all Fetcher callers via [`pool::pod::FETCHER_FUSE_CACHE`]
    /// (§Simulator-shares-accounting). This dimension has NO eviction
    /// escalation path: a FOD whose input closure exceeds it retries to
    /// exhaustion, so raise it here rather than relying on retry. Helm:
    /// `poolDefaults.fetcherFuseCacheBytes`. Default is
    /// `pool::pod::FETCHER_FUSE_CACHE_BYTES` (4Gi).
    pub fetcher_fuse_cache_bytes: u64,
    /// `true` ⟺ the `kube-build-scheduler` Deployment is rendered
    /// (`buildScheduler.enabled`). r40 bug_018: builder pods get
    /// `schedulerName=kube-build-scheduler` only when BOTH the
    /// NodeClaim CRD is present (Karpenter installed) AND this flag is
    /// set — otherwise the pod targets a scheduler that doesn't exist
    /// and the default kube-scheduler ignores it (permanent Pending,
    /// zero alerts: the `KubeBuildScheduler*` alerts are gated off by
    /// the same `buildScheduler.enabled` toggle). Helm:
    /// `buildScheduler.enabled`. Default: `true` (matches the chart's
    /// default).
    pub kube_build_scheduler_enabled: bool,
}

impl NodeClaimPoolConfig {
    /// `lead_time_seed[cell]` (seconds), or
    /// [`Self::default_lead_time_seed`] for cells absent from the map.
    /// `health::classify` uses this as a TIMEOUT (`2×seed`), so the
    /// default must be non-zero — a 0 default would reap every
    /// NodeClaim of an unseeded cell at the next 10s tick (well before
    /// ~18s real boot completes).
    ///
    /// r40 bug_024: routes through [`Cell::parse`] so `"h:od"` and
    /// `"h:on-demand"` are equivalent — the same normalization
    /// `CellSketches::seed()`, the scheduler's `cell_key_serde`, and
    /// `nix/tests/helm/18-metal-feature-routing.sh §5` already apply.
    /// Without this, an operator who writes `"metal-x86:on-demand": 600.0`
    /// (matching the Karpenter `capacityTypes` vocabulary two lines
    /// away) passes helm-lint, seeds the sketch, and STILL falls to
    /// `default_lead_time_seed=30s` here — `health::classify` reaps
    /// every ~600s metal boot at the `2×30=60s` timeout, the exact
    /// `RioNodeclaimPoolBootTimeoutLoop` this seed exists to prevent.
    ///
    /// Precedence: the canonical `"<h>:od"` key wins over the alias
    /// `"<h>:on-demand"` when both are present (e.g. layered helm values
    /// where one overlay uses the Karpenter form). The `Display`-form
    /// `.get()` runs first; the `Cell::parse` scan is the fallback.
    /// Same precedence as [`CellSketches::seed`] (r42 bug_018).
    pub fn seed_for(&self, cell: &Cell) -> f64 {
        self.lead_time_seed
            .get(&cell.to_string())
            .copied()
            .or_else(|| {
                self.lead_time_seed
                    .iter()
                    .find_map(|(k, v)| (Cell::parse(k).as_ref() == Some(cell)).then_some(*v))
            })
            .unwrap_or(self.default_lead_time_seed)
    }

    /// r35 bug_050: `min_consolidation_time[cell]` — the operator floor
    /// for `consolidate::consolidate_after`. `None` when no entry
    /// matches `cell.0`. Lookup precedence: exact hw-class match wins,
    /// then the longest matching `<prefix>*` glob. (The longest-prefix
    /// rule keeps overlapping globs deterministic regardless of map
    /// iteration order.)
    pub fn min_consolidation_time_for(&self, cell: &Cell) -> Option<f64> {
        if let Some(&v) = self.min_consolidation_time.get(&cell.0) {
            return Some(v);
        }
        self.min_consolidation_time
            .iter()
            .filter_map(|(k, &v)| {
                k.strip_suffix('*')
                    .filter(|p| cell.0.starts_with(p))
                    .map(|p| (p.len(), v))
            })
            .max_by_key(|&(len, _)| len)
            .map(|(_, v)| v)
    }

    /// Cold-start fallback cell for an hw-agnostic intent
    /// (`hw_class_names=[]`, i.e. `fit=None`): the
    /// `(referenceHwClass, <first cap>)` cell when its
    /// `kubernetes.io/arch` label matches `intent.system` (or is absent
    /// — arch-agnostic hw-class) AND its per-class `max_cores`/`max_mem`
    /// host the intent, else the first (sorted) hw-class satisfying
    /// both. `None` ⇔ `system` unmappable, no configured hw-class hosts
    /// that arch at that size, OR every hosting cell is in `masked` —
    /// the caller (`cover::assign_to_cells`) re-evaluates with
    /// `masked=∅` to attribute the drop
    /// (`rio_controller_nodeclaim_intent_dropped_total{reason=
    /// no_hosting_class|all_cells_ice_masked}` — the two need different
    /// operator actions; see `cover::DropTally`).
    ///
    /// Capacity-type is the FIRST listed for the class (Spot for
    /// default classes — cold-start probes are uniform `probe.cpu`-
    /// shaped and bounded by `max_node_claims_per_cell_per_tick`;
    /// on-demand fallback would defeat the §13b cost model. OnDemand
    /// for od-only classes like metal — §13c). **Consequence:** a
    /// *structural* spot failure (account quota, missing
    /// `AWSServiceRoleForEC2Spot`, region exhaustion) ICE-masks every
    /// cold-start fallback cell, the build never completes, the
    /// estimator never gets a fit, and `hw_class_names` stays `[]`
    /// forever — the system has no automatic spot→od escape valve. That
    /// is deliberate: spot ICE is normally transient (60s–`maxLeadTime`
    /// backoff) and pre-empting to od burns money for nothing. The
    /// escape valve for a structural failure is the operator's call,
    /// surfaced by `RioNodeclaimPoolAllCellsIceMasked`: fix the cloud,
    /// or set `capacityTypes: [on-demand]` on the affected class.
    ///
    /// §13d STRIKE-7 (r30 mb_012): `provides_features` IS filtered here.
    /// The pre-r30 doc claimed "post-§13c kvm intents always get
    /// `hw_class_names=[metal-*]` from the scheduler so they never reach
    /// fallback empty" — that was wrong for the cold-start (`fit=None`)
    /// path, which emitted `hw_class_names=[]` unconditionally; and a
    /// featured intent CAN legitimately ship `[]` when no class hosts
    /// it (`reference_hw_class_for_system` returns `None`). The
    /// scheduler-side `None/None` arm now emits cells for featured
    /// non-FOD intents (so this is mostly a backstop), but the consumer
    /// MUST NOT treat a feature-carrying `[]` intent as
    /// unconstrained-agnostic. Bidirectional ∅-guard
    /// (`features_compatible`) also closes the inverse leak: a
    /// featureless intent must NOT land on a kvm-tainted metal cell —
    /// the metal node carries `rio.build/kvm:NoSchedule` and the
    /// featureless pod has no toleration → wasted on-demand metal Node.
    ///
    /// `masked` cells are skipped — when the reference cell is
    /// ICE-masked, the next arch-matching cell is returned so
    /// cold-start probes don't silently strand on a cell
    /// `cover_deficit` then `continue`s.
    pub fn fallback_cell(
        &self,
        i: &SpawnIntent,
        hw: &HwClassConfig,
        masked: &HashSet<Cell>,
    ) -> Option<Cell> {
        // r35 B1 (§13e B5): featured arch-unmappable intent (`builtin`
        // FODs) routes by feature alone. The `i.required_features.
        // is_empty()` guard preserves the original early-return for
        // featureless unmappable systems (no class to route to is the
        // right answer there). `matches_arch(h, None)` treats a missing
        // intent arch as pass-through — the feature filter is what
        // narrows the candidate set. Symmetric with the scheduler's
        // `reference_hw_class_for_system` so the §13d
        // placement⊇provisioning invariant holds at both ends.
        let arch = ffd::system_to_arch(&i.system);
        if arch.is_none() && i.required_features.is_empty() {
            return None;
        }
        let candidate = |h: &str| {
            // Per-class ceiling filter: an hw-agnostic intent (override
            // bypass-path with `--cores=N`) may carry `cores >
            // class.max_cores`. Routing it to that cell would hit
            // `cover::sizing`'s exceeds_cell_cap drop — better to find
            // a cell that CAN host it (or `None` → caller's
            // `no_hosting_class` metric, which is the right operator
            // signal: "no class for this arch+size+feature").
            let (cls_c, cls_m) = hw.ceilings_for(h).unwrap_or((u32::MAX, u64::MAX));
            let cap = *hw.capacity_types_for(h).first()?;
            let c = Cell(h.into(), cap);
            (hw.matches_arch(h, arch)
                && !masked.contains(&c)
                && i.cores <= cls_c
                && i.mem_bytes <= cls_m
                && rio_common::k8s::features_compatible(&i.required_features, &hw.provides_for(h)))
            .then_some(c)
        };
        candidate(&self.reference_hw_class).or_else(|| hw.names().iter().find_map(|h| candidate(h)))
    }

    /// All configured cells (`hw_classes[h] × capacity_types_for(h)`),
    /// for round-robin iteration and per-cell gauges. §13c: per-hwClass
    /// capacity-types so an od-only class structurally never produces a
    /// `(h, Spot)` cell. Derived from the loaded [`HwClassConfig`] (not
    /// from `lead_time_seed` keys — those may be a subset).
    pub fn all_cells(&self, hw: &HwClassConfig) -> Vec<Cell> {
        hw.names()
            .into_iter()
            .flat_map(|h| {
                hw.capacity_types_for(&h)
                    .into_iter()
                    .map(move |c| Cell(h.clone(), c))
            })
            .collect()
    }
}

impl Default for NodeClaimPoolConfig {
    fn default() -> Self {
        Self {
            database_url: String::new(),
            lease_name: None,
            lease_namespace: None,
            reference_hw_class: String::new(),
            // Matches helm `sla.maxFleetCores` / `maxNodeClaimsPerCellPerTick`
            // / `maxLeadTime` defaults.
            max_fleet_cores: 10_000,
            max_node_claims_per_cell_per_tick: 8,
            max_lead_time: 600.0,
            max_consolidation_time: None,
            // r35 bug_050: Karpenter `consolidateAfter: 10m` parity for
            // fetcher cells.
            //
            // `*: 300.0` builder floor: the NA-model floor `boot_median/2`
            // (~9s for an 18s-boot builder) is BELOW the boot cost it's
            // supposed to amortize — reaping at 9s when the next build
            // arrives at t=15s burns a ~30s reprovision (boot + tick lag)
            // to save 6s of idle (~$0.0002 at $0.10/node-hr). 300s covers
            // two gaps: (1) the inter-build dispatch gap in a sequential
            // chain (output upload + next pod schedule + image pull,
            // ~60s ≈ 3×boot_median); (2) `>-<` DAG bottlenecks — a
            // wide→narrow→wide build graph idles the fleet during the
            // narrow phase, but the §13b forecast frontier is 1
            // dep-layer deep so the wide-again layer's Queued drvs emit
            // no intents and the threshold drops to the floor.
            // Observed bottlenecks are mostly <5min. ~$0.0083/node-hr
            // idle (vs $0.0167 at Karpenter's 10m). The NA model still
            // RAISES the threshold above the floor when arrival pressure
            // justifies it, but only for cells packing ~1 intent/node
            // (r38 bug_022) — for bin-packed cells the floor is the
            // threshold, which is fine: DAG-shaped demand is
            // deterministic, not stochastic, and a hazard model can't
            // see it. Targeted fix is a backlog-aware floor (scheduler
            // aggregates Queued cores per cell into
            // `GetSpawnIntentsResponse`; controller raises floor while
            // nonzero) so this stops being blind insurance.
            //
            // Lookup precedence: longest prefix glob wins, so `fetcher-*`
            // (len 8) overrides `*` (len 0) for fetcher cells.
            min_consolidation_time: BTreeMap::from([
                ("fetcher-*".into(), 600.0),
                ("*".into(), 300.0),
            ]),
            lead_time_seed: HashMap::new(),
            // Matches helm `sla.defaultLeadTimeSeed` default. Non-zero
            // so an unseeded cell's `health::classify` timeout (2×seed)
            // covers the ~18s real boot.
            default_lead_time_seed: 30.0,
            sketch_halflife_secs: 6 * 3600,
            // §13c-3: per-NodeClaim cores/mem ceilings come from the
            // scheduler's resolved global over `GetHwClassConfig`
            // (see `HwClassConfig::global_ceilings`); the controller
            // is air-gapped and cannot self-derive. Disk is the only
            // local ceiling (derived from `karpenter.dataVolumeSize`).
            // ≈ 500Gi `dataVolumeSize` × 90% allocatable.
            max_node_disk: 450 * (1 << 30),
            metal_sizes: Vec::new(),
            fuse_cache_bytes: pool::pod::BUILDER_FUSE_CACHE_BYTES,
            fetcher_fuse_cache_bytes: pool::pod::FETCHER_FUSE_CACHE_BYTES,
            // r40 bug_018: matches the chart's `buildScheduler.enabled`
            // default. When `false`, `pool/jobs::build_job` does NOT
            // stamp `schedulerName=kube-build-scheduler` even with the
            // NodeClaim CRD present — the second scheduler isn't
            // deployed, so a pod targeting it would Pending forever.
            kube_build_scheduler_enabled: true,
        }
    }
}

/// Per-component lease hooks. `LeaseHooks: Clone + Send + Sync` and
/// methods are sync, so transition work that needs `&mut self`/
/// `.await` (reload sketches, unarm gate) flows via `Arc<AtomicBool>`
/// flags the run loop checks at the top of each tick.
#[derive(Clone, Default)]
pub struct ControllerLeaseHooks {
    /// Set on `on_acquire`; run loop reloads `CellSketches` from PG
    /// and clears `recorded_boot`/`prev_idle`/`inflight_created` so a
    /// long-running standby that wins the lease doesn't `persist()`
    /// stale startup-time sketches over the previous leader's
    /// accumulated samples.
    ///
    /// bug_346: a monotone ACQUISITION EPOCH, not a boolean — each
    /// `on_acquire` increments it. The reconciler tracks two cursors
    /// against it (`edge_seen_epoch`, `reloaded_epoch`), so
    /// edge-actions fire EXACTLY once per acquisition by construction:
    /// the per-tick "is the flag still set?" re-execution that
    /// disabled idle consolidation for a whole PG outage (the
    /// `prev_idle.clear()` ran every Err tick) is unwritable — there
    /// is no flag to keep re-reading, only an epoch you have either
    /// seen or not. Reload retry keeps its latch-on-Ok-only shape via
    /// `reloaded_epoch` (advanced only on a successful load; persist
    /// gated until it catches up). A re-acquire mid-Err-loop is a NEW
    /// epoch and re-fires the edge once (new tenure).
    acquire_epoch: Arc<std::sync::atomic::AtomicU64>,
    /// Set on `on_lose`; run loop `placeable_tx.send_replace(None)`
    /// so an ex-leader's `PlaceableGate` doesn't stay armed with a
    /// stale set (whose stale `queued` would `reap_excess_pending` the
    /// new leader's Jobs).
    lose: Arc<std::sync::atomic::AtomicBool>,
}

impl rio_lease::LeaseHooks for ControllerLeaseHooks {
    fn on_acquire(&self) {
        self.acquire_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        metrics::counter!("rio_controller_lease_acquired_total").increment(1);
    }
    fn on_lose(&self) {
        self.lose.store(true, std::sync::atomic::Ordering::SeqCst);
        metrics::counter!("rio_controller_lease_lost_total").increment(1);
    }
    fn on_rebound(&self) {
        // Compound semantics for the controller's two cells: the lose
        // flag disarms the PlaceableGate (the pre-term tenure's stale
        // `queued` set must not `reap_excess_pending` the post-term
        // world), and the epoch bump re-fires the acquire edge actions
        // (sketch reload, `prev_idle` clear, `pending_evidence` reset)
        // exactly once — the foreign term may have moved PG and
        // cluster state under us. Order is irrelevant here (the run
        // loop reads both flags at the same tick top); each is its own
        // atomic.
        self.lose.store(true, std::sync::atomic::Ordering::SeqCst);
        self.acquire_epoch
            .fetch_add(1, std::sync::atomic::Ordering::SeqCst);
        metrics::counter!("rio_controller_lease_rebound_total").increment(1);
    }
}

/// The reconciler. Constructed in `main.rs` after PG connect; `run()`
/// is `spawn_monitored` and never returns until shutdown.
pub struct NodeClaimPoolReconciler {
    nodeclaims: Api<NodeClaim>,
    /// Cluster-scoped `Pool` API for the §13d Pool-coverage filter
    /// (r31 bug_019). `reconcile_once` lists per tick (cheap — Pools
    /// are ~5 small objects) and drops intents no configured Builder
    /// Pool will Job-place, BEFORE `ffd::simulate` so the FFD set,
    /// the deficit, and the cover are all consistent. Fail-OPEN on
    /// transient apiserver error: skipping the filter for one tick
    /// over-provisions; an empty coverage set would drop EVERY intent
    /// → ZERO Jobs.
    pools: Api<Pool>,
    admin: AdminClient,
    pg: sqlx::PgPool,
    leader: LeaderState,
    cfg: NodeClaimPoolConfig,
    /// `[sla.hw_classes.$h]` → label conjunction, fetched via
    /// `GetHwClassConfig` in main.rs and shared with the
    /// `node_informer`. `cover_deficit` reads
    /// [`HwClassConfig::labels_for`] to build NodeClaim
    /// `spec.requirements`.
    hw_config: HwClassConfig,
    /// Label-selected (`rio.build/pool`) Pod API for the per-tick
    /// `pods::PodSnapshot` LIST (§4(a)1 — replaces the watch-fed
    /// `PodRequestedCache`). [`Self::list_pool_pods`] derives the
    /// per-node `requested` sums and the bound-intent index from one
    /// LIST per tick; [`Self::list_live_nodeclaims`] post-fills
    /// `LiveNode.requested` from it so `free()` reflects what's
    /// already bound.
    pods: Api<Pod>,
    /// Publish side of [`PlaceableGate`]. Written once per successful
    /// FFD tick with the `intent_id`s placed on `Registered=True`
    /// nodes; the `pool/jobs` reconciler reads it via `Ctx.placeable`.
    /// NOT republished on ⊥-ticks or in `consolidate_only` (intentional
    /// — see [`PlaceableGate`] for why the resulting staleness is
    /// benign); reset to `None` on lease loss.
    placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    /// Lease-transition flags from [`ControllerLeaseHooks`]. Checked
    /// at the top of each run-loop tick so acquire/lose edges do real
    /// state work (`LeaseHooks` methods are sync; can't `.await`).
    hooks: ControllerLeaseHooks,
    sketches: CellSketches,
    /// NodeClaim names whose `Registered=True` boot time has already
    /// been recorded into `sketches`. Edge-detector state for
    /// [`CellSketches::observe_registered`]; pruned to live names each
    /// tick. In-memory only — `observe_registered`'s recency-gate
    /// (`now − Registered.transition < 3×TICK`) means a restart
    /// re-records ONLY recently-registered nodes; stale registrations
    /// are recorded-only (so they don't re-edge later) without pushing
    /// the cell to `report_unfulfillable`'s ICE-clear.
    recorded_boot: HashSet<String>,
    /// merged_bug_007: scheduler-bound evidence produced by kube-only
    /// observation on ticks that cannot ship it (⊥ pre-threshold +
    /// consolidate-only). The producing edges are CONSUME-ONCE
    /// (`recorded_boot` marks the node; the edge never re-fires), so
    /// `#[must_use]` + this buffer make the discard unrepresentable:
    /// every `kube_only_observations` return is either drained into a
    /// healthy tick's Ack or merged here. Polarity: SUPPRESS — cleared
    /// on the lease-acquire edge (a stale buffered ICE-clear from a
    /// previous tenure could mask a genuinely ICE'd cell).
    pending_evidence: PendingSchedulerEvidence,
    /// bug_346: last acquisition epoch whose EDGE actions ran (the
    /// amplify-class `prev_idle` clear + the `pending_evidence`
    /// reset). Compared against `hooks.acquire_epoch` once per tick;
    /// fires exactly once per acquisition.
    edge_seen_epoch: u64,
    /// bug_346: last acquisition epoch whose PG reload SUCCEEDED
    /// (the suppress-class clears + sketch swap ride the Ok arm).
    /// `persist()` stays gated while this lags the epoch
    /// (latch-on-Ok-only, unchanged).
    reloaded_epoch: u64,
    /// `name → epoch-secs at which the node was first observed idle`
    /// (`requested.0 == 0`). r42 bug_020: the SOLE idle-duration source
    /// — Karpenter v1 does not write `Empty`, so the controller's own
    /// `requested.0` observation is the authority. Seeded with `now_secs`
    /// on first observation of idleness in
    /// [`consolidate::observe_idle_to_busy`]; pruned on idle→busy or
    /// when the node leaves `live`. Read by [`consolidate::reap_idle`]
    /// via `ReapInputs.prev_idle`. In-memory only — restart AND
    /// lease-acquire (both `Ok` and `Err` reload arms) re-seed every
    /// idle node from `now_secs`. Under-reap by one threshold cycle:
    /// SAFE direction, identical to a fresh fleet. The clear is
    /// unconditional on the lease-acquire edge — unlike the other
    /// in-memory sets, a stale `prev_idle` entry has AMPLIFY polarity
    /// (inflated idle → over-reap), so it cannot wait for `load_seeded`
    /// to succeed (r43 merged_bug_016). Pre-acquire idle→busy→idle
    /// cycles are unobservable; treating them as fresh-idle is the only
    /// safe assumption.
    prev_idle: HashMap<String, f64>,
    /// Cells written by [`Self::emit_live_gauges`] last tick that were
    /// NOT in `all_cells()` (i.e. carried by a live NodeClaim whose
    /// hwClass was removed from config mid-rollout). r41 bug_025: a
    /// `metrics::gauge!` series persists at its last `.set()` value
    /// — once the orphaned NodeClaim finishes draining and drops out of
    /// `by_state`, `terminating_age_max_seconds{cell}` would otherwise
    /// freeze at its last (possibly >300s) value and `StuckTerminating`
    /// would page forever. Tracking the prior tick's extras gives each
    /// vanished cell exactly one zero-write. In-memory only — restart
    /// drops one tick's worth of trailing zero-writes (same TTL-omitted
    /// rationale as `prev_idle`).
    prev_extra_cells: HashSet<Cell>,
    /// Cells that had FFD-unplaced demand (`by_cell.keys()`) but were
    /// not in `cfg.all_cells()` last tick; drives the trailing
    /// zero-write for `ffd_unplaced_cores`. Distinct from
    /// `prev_extra_cells` — that set is keyed on live NodeClaim cells,
    /// this on intent cells; an intent's cell may have no NodeClaims
    /// yet. NOT cleared on lease re-acquire (cleanup-pending polarity,
    /// same rationale as `prev_extra_cells`; r43 bug_026).
    prev_unplaced_extras: HashSet<Cell>,
    /// `name → cell` for NodeClaims `cover_deficit` created and that
    /// haven't yet been observed Registered, terminating, or absent.
    /// Tick-over-tick diff against `live`: a name in here but absent
    /// from `live` AND not reaped by us ⇒ Karpenter GC'd it
    /// (`Launched=False reason=LaunchFailed/InsufficientCapacity` →
    /// delete in ~1s, faster than the 10s tick). [`health::classify`]'s
    /// `Launched=False > timeout` never fires for those — the claim is
    /// gone before it's observed. ICE-masked via
    /// [`health::detect_vanished`].
    ///
    /// Mutators (any new writer must keep this list current — a fifth
    /// path that deletes/forgets a tracked claim without showing up
    /// here is exactly the bug_012/bug_020 shape):
    /// 1. `cover_deficit` `extend()`s the names it `create()`d.
    /// 2. `clear()` on the lease-acquire Ok arm (suppress polarity —
    ///    see the per-field table at the acquire match). There is NO
    ///    config-reload clear call site (corrected 2026-06-02; the
    ///    list previously named "the config-hash gate" — a design-era
    ///    label that never matched the code).
    /// 3. [`health::detect_vanished`] retains in-flight, drops
    ///    registered/terminating/vanished — runs in BOTH
    ///    `reconcile_once` and `consolidate_only`.
    /// 4. Reap-name removal: `reap_unhealthy` returns the names it
    ///    `delete()`d; both callers `remove()` them BEFORE
    ///    `detect_vanished` so the controller's own reaps aren't
    ///    misread as Karpenter GC.
    // r[impl ctrl.nodeclaim.inflight-conservation+2]
    inflight_created: HashMap<String, Cell>,
    /// Count of consecutive ticks where `GetSpawnIntents` returned ⊥
    /// (RPC error). Saturates at `u8::MAX`; reset on first success.
    consecutive_bot_ticks: u8,
    /// Monotonic tick counter for `cover_deficit`'s rotating-start
    /// round-robin.
    tick_counter: u64,
    /// OA2: per-node clustering of pull-mode attempt-deadline expiries
    /// over the open-attempt ledger view ([`wedge::WedgeTracker`]).
    /// Produces the Dead-node input `reap_unhealthy` consumes (the
    /// only such signal since the scheduler-side detector retired with
    /// the stream protocol). In-memory only and
    /// NOT cleared on lease edges: evidence is event-shaped (an expiry
    /// observed at time T never un-happens) and ages out of its window;
    /// a fresh process under-detects for at most one window — the same
    /// safe direction as the heartbeat detector it succeeds.
    wedge: wedge::WedgeTracker,
    /// Backing-node names reaped since the last wedge update — the
    /// tracker's REQUIRED eviction feed (consumed each tick). Reaps
    /// happen after the wedge verdict in the tick order, so feedback
    /// lands on the NEXT tick's update.
    pending_wedge_evictions: std::collections::BTreeSet<String>,
}

/// Cells `emit_live_gauges` must write this tick. r41 bug_025: a live
/// NodeClaim's `cell` is label-derived; `all_cells()` is config-derived
/// (GetHwClassConfig, ≤300s refresh). During a config rollout that
/// removes a hwClass while a NodeClaim is draining, the cell drops out
/// of `all_cells()` but stays in `by_state` — without the union, the
/// gauge loop stops writing `terminating_age_max_seconds{cell}` and
/// `RioNodeclaimPoolStuckTerminating` reads a frozen value forever
/// (`metrics::gauge!` series persist at their last `.set()`; no
/// deregister). `prev_extras` gives each vanished cell exactly one
/// trailing zero-write so a resolved stuck-drain doesn't page forever.
///
/// Returns `(to_write, new_extras)`: cells to emit gauges for, and the
/// next tick's extras set (`live_cells` outside config). Callers that
/// drive a trailing zero-write store the latter into a per-gauge
/// `prev_*` field (`prev_extra_cells`, `prev_unplaced_extras` — both
/// CLEANUP-polarity, see the lease-acquire table); the `reap_idle`
/// callers consume only `to_write` and discard the second half.
///
/// Free function (not a method): `NodeClaimPoolReconciler::new` is
/// `async` and needs a `kube::Client` + `PgPool`; the test module
/// can't construct one, so the testable invariant lives outside.
fn gauge_universe(
    configured: &[Cell],
    live_cells: impl Iterator<Item = Cell>,
    prev_extras: &HashSet<Cell>,
) -> (Vec<Cell>, HashSet<Cell>) {
    let configured_set: HashSet<&Cell> = configured.iter().collect();
    let extras: HashSet<Cell> = live_cells.filter(|c| !configured_set.contains(c)).collect();
    let trailing: Vec<Cell> = prev_extras
        .iter()
        .filter(|c| !configured_set.contains(c) && !extras.contains(*c))
        .cloned()
        .collect();
    let to_write: Vec<Cell> = configured
        .iter()
        .cloned()
        .chain(extras.iter().cloned())
        .chain(trailing)
        .collect();
    (to_write, extras)
}

/// Cells in `by_cell` not in `order` and the count of intents stranded
/// there. r41 bug_021: scheduler-stamped cells (`cells_of(i)`) the
/// controller's GetHwClassConfig doesn't know about — the cover loop
/// never visits them, the intent is silently dropped.
///
/// Free function for the same reason as [`gauge_universe`]:
/// `cover_deficit` is `async` with a `kube::Client` + `PgPool` the test
/// module can't construct; the partition is the testable invariant.
fn unknown_cell_intents<'m>(
    by_cell: &'m BTreeMap<Cell, Vec<&SpawnIntent>>,
    order: &[Cell],
) -> (u64, Vec<&'m Cell>) {
    let known: HashSet<&Cell> = order.iter().collect();
    let unknown: Vec<&Cell> = by_cell.keys().filter(|c| !known.contains(c)).collect();
    let n: u64 = unknown.iter().map(|c| by_cell[*c].len() as u64).sum();
    (n, unknown)
}

// The per-field stale-state polarity table
// (`#r("ctrl.nodeclaim.lease-edge-polarity")`) is asserted end-to-end
// by `lifecycle_tests`: real `tick()`/`reconcile_once`/
// `consolidate_only` bodies driven through lease-acquire/loss, standby,
// ⊥-streak, and consolidate-only edges via the existing seams
// (ApiServerVerifier scenario queues, MockAdmin/dead_channel, TestDb,
// the threaded `tick(now)` clock) — no trait extraction needed.
impl NodeClaimPoolReconciler {
    /// Construct + load persisted [`CellSketches`] from PG. Called once
    /// at startup AFTER PG connect; the loaded state survives controller
    /// restarts (rolling upgrade, OOM) so lead-time learning isn't reset.
    #[allow(clippy::too_many_arguments)]
    pub async fn new(
        kube: kube::Client,
        admin: AdminClient,
        pg: sqlx::PgPool,
        leader: LeaderState,
        hooks: ControllerLeaseHooks,
        cfg: NodeClaimPoolConfig,
        hw_config: HwClassConfig,
        placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    ) -> Self {
        // Load persisted sketches; fall back to empty on error (a fresh
        // table is the cold-start case anyway). `load_seeded` does
        // `load → maybe_rotate_all → seed` so `seed()` sees
        // post-rotation state (bug_017: a stale-epoch shadow that the
        // first tick would discard is discarded BEFORE seed runs).
        let halflife = Duration::from_secs(cfg.sketch_halflife_secs);
        let sketches = match CellSketches::load_seeded(
            &pg,
            &cfg.lead_time_seed,
            halflife,
            std::time::SystemTime::now(),
        )
        .await
        {
            Ok(s) => {
                info!(cells = s.len(), "loaded nodeclaim_cell_state from PG");
                s
            }
            Err(e) => {
                warn!(error = %e, "nodeclaim_cell_state load failed; starting empty");
                let mut d = CellSketches::default();
                d.seed(&cfg.lead_time_seed);
                d
            }
        };
        Self {
            nodeclaims: Api::all(kube.clone()),
            pools: Api::all(kube.clone()),
            pods: Api::all(kube),
            admin,
            pg,
            leader,
            cfg,
            hw_config,
            placeable_tx,
            hooks,
            sketches,
            recorded_boot: HashSet::new(),
            prev_idle: HashMap::new(),
            prev_extra_cells: HashSet::new(),
            prev_unplaced_extras: HashSet::new(),
            inflight_created: HashMap::new(),
            consecutive_bot_ticks: 0,
            pending_evidence: PendingSchedulerEvidence::default(),
            edge_seen_epoch: 0,
            reloaded_epoch: 0,
            tick_counter: 0,
            wedge: wedge::WedgeTracker::default(),
            pending_wedge_evictions: std::collections::BTreeSet::new(),
        }
    }

    /// Lease-acquire reload still pending (PG `load()` not yet
    /// succeeded since the last `on_acquire`). While true, the
    /// in-memory `self.sketches` may be stale (a long-running standby's
    /// startup snapshot, or `default()` if `new()` hit a PG outage) —
    /// `persist()` is gated off so it doesn't overwrite the previous
    /// leader's PG rows. Set false only on `CellSketches::load_seeded` Ok.
    fn reload_pending(&self) -> bool {
        self.reloaded_epoch
            != self
                .hooks
                .acquire_epoch
                .load(std::sync::atomic::Ordering::SeqCst)
    }

    /// Tick loop. Gated on [`LeaderState::is_leader`] — standby replicas
    /// (and the surge pod during a rolling upgrade) burn ticks as no-ops
    /// until they acquire. Stateful (`consecutive_bot_ticks`,
    /// `tick_counter`, `sketches`): not `spawn_periodic`. `biased;`
    /// inlined per `r[common.task.periodic-biased]`.
    pub async fn run(mut self, shutdown: rio_common::signal::Token) {
        info!(
            max_fleet_cores = self.cfg.max_fleet_cores,
            "nodeclaim_pool reconciler starting"
        );
        let mut interval = tokio::time::interval(TICK);
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);
        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => break,
                _ = interval.tick() => {}
            }
            self.tick(std::time::SystemTime::now()).await;
        }
        info!("nodeclaim_pool reconciler stopped");
    }

    /// One run-loop iteration: lease-edge work, standby skip,
    /// acquire-reload latch, then the reconcile dispatch. Extracted
    /// from [`Self::run`] so the lifecycle-invariants suite can drive
    /// real ticks with an injected `now` (the only wall-clock reads
    /// left in the tick path are the `Instant` duration histogram and
    /// tracing timestamps — both harmless). Behavior-identical to the
    /// pre-extraction loop body.
    async fn tick(&mut self, now: std::time::SystemTime) {
        // Lease-loss edge: unarm the gate so an ex-leader's stale
        // set doesn't drive `reap_excess_pending` against the new
        // leader's Jobs. Checked BEFORE `is_leader()` so it fires
        // on the same tick as the loss.
        // r[impl ctrl.nodeclaim.placeable-gate+5]
        if self
            .hooks
            .lose
            .swap(false, std::sync::atomic::Ordering::SeqCst)
        {
            self.placeable_tx.send_replace(None);
        }
        if !self.leader.is_leader() {
            debug!("standby; skipping nodeclaim_pool tick");
            return;
        }
        // Lease-acquire edge: reload sketches from PG and clear
        // edge-detector state so a long-running standby that wins
        // the lease doesn't `persist()` stale startup-time
        // sketches over the previous leader's accumulated samples,
        // and so `observe_registered`'s recency-gate sees an empty
        // `recorded_boot` (else days-old registrations mass-clear
        // the scheduler's IceBackoff).
        //
        // Latch-on-Ok-only: a transient PG error must NOT consume
        // the one-shot flag. On `Err`, warn and fall through —
        // `reconcile_once` runs degraded (in-memory sketches
        // suffice for FFD/reap), `persist()` is gated off via
        // `reload_pending()` so the stale state doesn't overwrite
        // the previous leader's PG rows. The flag stays set; next
        // tick retries the reload. Clears (recorded_boot etc.) go
        // in the Ok-arm only — atomic edge: full reload or full
        // retry.
        //
        // EXCEPTION: `prev_idle` clears unconditionally. Its
        // stale-state polarity is opposite the other two sets
        // (see the per-field table below): a stale `prev_idle`
        // entry inflates `now − since` and CAUSES a reap, not
        // suppresses one. Even if `load_seeded` errors, leaving a
        // pre-lapse timestamp over-reaps. Clearing here makes the
        // `Err` arm under-reap by one cycle (the documented SAFE
        // direction) instead of unboundedly over-reaping.
        // r[impl ctrl.nodeclaim.lease-edge-polarity+3]
        // r[impl ctrl.nodeclaim.acquire-edge-token]
        let epoch = self
            .hooks
            .acquire_epoch
            .load(std::sync::atomic::Ordering::SeqCst);
        if epoch != self.edge_seen_epoch {
            // EDGE actions: once per acquisition, by construction —
            // NOT once per Err retry (bug_346: the per-tick re-clear
            // disabled idle consolidation for the whole PG outage:
            // every idle clock restarted every tick, so reap_idle
            // never saw a spell older than one tick).
            self.prev_idle.clear();
            self.pending_evidence = PendingSchedulerEvidence::default();
            self.edge_seen_epoch = epoch;
        }
        if self.reloaded_epoch != epoch {
            let halflife = Duration::from_secs(self.cfg.sketch_halflife_secs);
            match CellSketches::load_seeded(&self.pg, &self.cfg.lead_time_seed, halflife, now).await
            {
                Ok(s) => {
                    self.sketches = s;
                    self.recorded_boot.clear();
                    self.inflight_created.clear();
                    // suppress polarity (see the table below):
                    // buffered evidence from a previous tenure could
                    // ship a stale ICE-clear.
                    self.pending_evidence = PendingSchedulerEvidence::default();
                    // `prev_extra_cells` is intentionally NOT cleared
                    // here (r41 bug_025). Per-field stale-state
                    // polarity on the lease-acquire edge:
                    //
                    //   - `recorded_boot`   suppress  → cleared in Ok arm
                    //     (stale entry skips a record; lost sample)
                    //   - `inflight_created` suppress → cleared in Ok arm
                    //     (stale entry → spurious ICE-mask)
                    //   - `prev_idle`       AMPLIFY   → cleared BEFORE the
                    //     match (stale entry inflates idle → over-reap;
                    //     r43 merged_bug_016)
                    //   - `prev_extra_cells` CLEANUP  → never cleared
                    //     (stale entry → one trailing zero-write — the
                    //     desired behavior; clearing would orphan a gauge
                    //     series at its last possibly-paging value)
                    //   - `prev_unplaced_extras` CLEANUP → never cleared
                    //     (stale entry → one trailing zero-write for
                    //     `ffd_unplaced_cores`; r43 bug_026)
                    //   - `pending_evidence` suppress → cleared above
                    //     (a stale buffered ICE-clear from a previous
                    //     tenure could mask a genuinely ICE'd cell;
                    //     merged_bug_007)
                    //
                    // When adding a field that holds in-memory edge
                    // state, classify its polarity here and put the
                    // clear (or not-clear) on the matching edge. See
                    // [`gauge_universe`].
                    self.reloaded_epoch = epoch;
                }
                Err(e) => {
                    warn!(
                        error = %e,
                        "CellSketches reload on leader-acquire failed; \
                         retrying next tick (persist gated)"
                    );
                }
            }
        }
        metrics::gauge!("rio_controller_sketches_reload_pending").set(if self.reload_pending() {
            1.0
        } else {
            0.0
        });
        self.tick_counter = self.tick_counter.wrapping_add(1);
        let started = std::time::Instant::now();
        if let Err(e) = self.reconcile_once(now).await {
            warn!(error = %e, "nodeclaim_pool tick failed");
        }
        metrics::histogram!("rio_controller_nodeclaim_tick_duration_seconds")
            .record(started.elapsed().as_secs_f64());
    }

    /// Kube-only sketch observations shared by [`Self::reconcile_once`]
    /// and [`Self::consolidate_only`]. Anything added here runs in BOTH
    /// paths — that is the point. If a new `observe_*` call is
    /// kube-only, it goes here; if it needs the scheduler, it stays in
    /// `reconcile_once`.
    ///
    /// Three rounds (r40 bug_012, r42 bug_023, r43 bug_023) hit the same
    /// shape: a kube-only observation added to `reconcile_once` but not
    /// `consolidate_only`, because the two paths each inlined their own
    /// copy. This helper is the §nth-strike STRUCTURAL close — adding a
    /// fourth inline copy re-arms the trap; adding here closes it.
    ///
    /// Caller must run AFTER `requested` is populated
    /// (`list_live_nodeclaims`) and BEFORE `reap_idle` records the
    /// censored half — `observe_idle_to_busy` needs uncensored
    /// idle→busy edges.
    ///
    /// Returns `(registered_cells, observed_types)` from
    /// [`CellSketches::observe_registered`]. `reconcile_once` consumes
    /// them; `consolidate_only` discards them (the scheduler is
    /// unreachable, so there is nothing to act on — same shape as the
    /// existing `ice_cells` discard).
    fn kube_only_observations(
        &mut self,
        live: &[ffd::LiveNode],
        now: f64,
        now_sys: std::time::SystemTime,
    ) -> PendingSchedulerEvidence {
        // Uncensored idle→busy edges (see caller-ordering note above).
        consolidate::observe_idle_to_busy(live, &mut self.prev_idle, &mut self.sketches, now);
        // r[ctrl.nodeclaim.lead-time-ddsketch]: record boot times on
        // Registered=True edges, then rotate any cells past halflife.
        //
        // r43 bug_023: `observe_registered` is a kube-only sketch-write.
        // A scheduler outage that spans a NodeClaim's Registered
        // transition would otherwise permanently lose the boot sample
        // via the 3×TICK recency-gate at `sketch.rs::observe_registered`
        // — the gate marks the node in `recorded_boot` (so it never
        // re-edges) without recording.
        let (registered, observed) =
            self.sketches
                .observe_registered(live, &mut self.recorded_boot, now);
        self.sketches
            .maybe_rotate_all(now_sys, Duration::from_secs(self.cfg.sketch_halflife_secs));
        PendingSchedulerEvidence {
            registered_cells: registered.into_iter().collect(),
            observed_types: observed,
            // The ICE-mark plane is produced by the reap path
            // (`record_reap`/`detect_vanished`), not by kube-only
            // observation — the reconcile bodies extend the buffer
            // directly at the production site.
            ice_cells: std::collections::BTreeSet::new(),
        }
    }

    /// One tick: poll → FFD sim → cover deficit → reap → persist.
    ///
    /// `anyhow::Result`: this isn't a kube `Controller::run` body so the
    /// crate's `Error` enum (built around `error_policy` requeue) doesn't
    /// apply. Any error is logged + retried next tick.
    // r[impl ctrl.nodeclaim.ffd-sim]
    #[instrument(skip(self, now_sys), fields(tick = self.tick_counter))]
    async fn reconcile_once(&mut self, now_sys: std::time::SystemTime) -> anyhow::Result<()> {
        // ⊥ on scheduler unreachable: warn + count, don't propagate.
        // `admin_call` bounds at ADMIN_RPC_TIMEOUT so a stalled
        // scheduler doesn't wedge the tick.
        //
        // §13e: NO `kind` filter. Pre-§13e this asked for Builder only —
        // FOD intents landed on the static helm `rio-fetcher` NodePool
        // and including them would over-reserve builder capacity in
        // FFD. Both premises die in §13e: there is no static fetcher
        // NodePool, and FOD intents carry distinct
        // `hw_class_names=[fetcher-*]` so `ffd::simulate` partitions
        // them into separate cells (Cell = (hw_class, capacity)) —
        // builder and fetcher deficits never collide.
        let intents: Option<GetSpawnIntentsResponse> = match admin_call(
            self.admin
                .clone()
                .get_spawn_intents(GetSpawnIntentsRequest {
                    ..Default::default()
                }),
        )
        .await
        {
            Ok(r) => Some(r.into_inner()),
            Err(e) => {
                warn!(error = %e, "GetSpawnIntents failed (⊥ tick)");
                None
            }
        };

        let Some(mut intents) = intents else {
            self.consecutive_bot_ticks = self.consecutive_bot_ticks.saturating_add(1);
            // r[impl ctrl.nodeclaim.consolidate-only-degraded+3]
            if self.consecutive_bot_ticks >= BOT_TICKS_BEFORE_CONSOLIDATE_ONLY {
                return self.consolidate_only(now_sys).await;
            }
            // Pre-threshold ⊥ tick (streak 1..4): run the SAME shared
            // kube-only observation block as `consolidate_only` and
            // `reconcile_once` — the r43 merged_bug_016/bug_023 close.
            // Without it, `prev_idle` stays un-pruned across unobserved
            // busy periods (a stale entry conflates two idle spells)
            // and Registered edges age past the 3×TICK recency gate
            // (boot sample + ICE-clear lost permanently). Returns are
            // discarded: `registered_cells`/`observed_types` need the
            // scheduler (same shape as `consolidate_only`'s discard).
            // NO reap/create/ack/publish before the threshold — growth
            // and destructive actions stay threshold-gated; cost is the
            // two consolidate-only LISTs on ≤4 ticks per outage, both
            // label-selected. A kube LIST failure propagates via `?`
            // (tick warned + retried by `run`, identical to
            // `consolidate_only`'s fail-closed posture); the streak
            // increment already happened above, so a kube failure
            // cannot stall the threshold.
            // r[impl ctrl.nodeclaim.consolidate-only-degraded+3]
            let pod_snapshot = self.list_pool_pods().await?;
            let live = self.list_live_nodeclaims(&pod_snapshot).await?;
            let ev = self.kube_only_observations(&live, epoch_secs(now_sys), now_sys);
            self.pending_evidence.merge(ev);
            return Ok(());
        };
        self.consecutive_bot_ticks = 0;

        // §13d Pool axis (r31 bug_019): drop intents no configured
        // Pool will Job-place, BEFORE `ffd::simulate` so the FFD set,
        // the deficit, and the cover are all consistent. The placer
        // (`pool/jobs::queued_for_pool`) only spawns a Job for intents
        // matched by some Pool's `(systems, effective_features)`;
        // provisioning for unplaceable intents mints a NodeClaim that
        // FFD-places (→ `reserved`) → `reap_idle` skips it forever —
        // a permanently-idle on-demand metal node. `retain_hosting_
        // cells` validates the *cell* axis; this is the *Pool* axis.
        //
        // §13e: covers Builder AND Fetcher Pools. The explicit
        // allowlist (instead of dropping the filter entirely) means a
        // future third `ExecutorKind` does NOT silently start minting
        // NodeClaims — the exclusion stays a conscious code change.
        // Coverage uses `effective_features(spec)` (the §13e chokepoint),
        // NOT `spec.features` — a Fetcher Pool's `spec.features` is
        // always `[]` (CEL-enforced) but its `effective_features` is
        // `[fetcher]`, and the FOD intent's `required_features=[fetcher]`
        // must match it through `pool_covers`'s `features_compatible`.
        //
        // `pools.list()` reads the apiserver, not a stale informer — a
        // Pool created mid-tick is seen next tick; the retained intent
        // re-appears next `GetSpawnIntents`.
        //
        // Fail-OPEN on transient apiserver error: skipping the filter
        // for one tick over-provisions (the legit intents still get
        // covered, plus possibly some uncoverable ones — same as
        // pre-fix behavior). Treating an error as empty coverage would
        // drop EVERY intent → ZERO Jobs (cluster stops building) — a
        // strictly worse failure than the bug this fixes.
        match self.pools.list(&Default::default()).await {
            Ok(l) => {
                let coverage: Vec<PoolCoverage> = l
                    .items
                    .iter()
                    .filter(|p| {
                        matches!(p.spec.kind, ExecutorKind::Builder | ExecutorKind::Fetcher)
                    })
                    .map(|p| {
                        (
                            pool::executor_kind_to_proto(p.spec.kind),
                            p.spec.systems.clone(),
                            pool::pod::effective_features(&p.spec),
                        )
                    })
                    .collect();
                let before = intents.intents.len();
                intents.intents.retain(|i| pool_covers(i, &coverage));
                let dropped = before - intents.intents.len();
                if dropped > 0 {
                    metrics::counter!(
                        "rio_controller_nodeclaim_intent_dropped_total",
                        "reason" => "no_pool_covers",
                    )
                    .increment(dropped as u64);
                    warn!(
                        dropped,
                        executor_pools = coverage.len(),
                        "SpawnIntents dropped at provisioner — no configured Pool \
                         (Builder or Fetcher) covers their (kind, system, effective_features); \
                         add a Pool or remove the hwClass advertising the feature"
                    );
                }
            }
            Err(e) => {
                warn!(error = %e, "Pool list failed — coverage filter skipped this tick");
            }
        }

        // §4(a)1: one label-selected Pod LIST per tick — the source of
        // `LiveNode.requested`, FFD's bound-intent short-circuit, the
        // OA2 wedge attribution fallback, and the scheduler-shipped
        // `bound_intents`. Failure propagates (tick aborted, retried
        // next tick): the same fail-closed posture as a failed
        // NodeClaim LIST below, replacing the deleted watch cache's
        // silent stale-data degradation.
        let pod_snapshot = self.list_pool_pods().await?;
        let live = self.list_live_nodeclaims(&pod_snapshot).await?;
        let now = epoch_secs(now_sys);
        // `registered_cells` feeds `report_unfulfillable`'s ICE-clear;
        // `observed_types` feeds the scheduler's `CostTable.cells`
        // (R24B7 instance-type autodiscovery).
        // Fresh evidence merges INTO the buffer; the buffer is shipped
        // FROM the buffer and cleared only when the Ack lands
        // (merged_bug_007 + merged_bug_045: ⊥-tick and consolidate-only
        // edges are consume-once and must not be lost — and neither may
        // an Ack-Err or a mid-tick abort lose them. No moved-out value
        // ever exists, which strictly subsumes re-merge-on-drop; the
        // merge also fixes the pre-existing ≥5-tick observed_types loss
        // across consolidate-only stretches).
        let fresh = self.kube_only_observations(&live, now, now_sys);
        self.pending_evidence.merge(fresh);

        let bound = pod_snapshot.bound_intents();
        // §13d STRIKE-7 (mb_012): the agnostic-fallback admit predicate
        // checks arch ∧ features. A `hw_class_names=[]` kvm intent must
        // NOT FFD-place onto a non-metal node (deficit appears covered →
        // no metal NodeClaim minted → kvm pod CrashLoopBackOff on ENXIO
        // `/dev/kvm`; pool-static nodeSelector deleted r33 bug_002); a
        // featureless intent must NOT land on a kvm-tainted metal node
        // (pod has no toleration → wasted on-demand metal).
        let (placeable, unplaced) = ffd::simulate(
            &intents.intents,
            &live,
            &self.sketches,
            bound,
            self.cfg.fuse_cache_bytes,
            |h, a, f| {
                self.hw_config.matches_arch(h, a)
                    && rio_common::k8s::features_compatible(f, &self.hw_config.provides_for(h))
            },
        );
        // Schmitt-adjust `lead_time_q` from the per-cell EWMA of
        // `on_reg/(on_reg+on_inf)` — the warm-hit proxy. A cell whose
        // placements land mostly in-flight (low ratio) is
        // under-provisioning → widen `q`; mostly registered → narrow.
        // Cells with zero placements this tick are skipped (EWMA holds;
        // Schmitt dead-zone absorbs the no-signal case).
        for (cell, (reg, inf)) in ffd::per_cell_hit_ratio(&placeable, &live) {
            let hit = reg as f64 / (reg + inf).max(1) as f64;
            let s = self.sketches.cell_mut(&cell);
            s.observe_hit_ratio(hit);
            s.schmitt_adjust(s.forecast_hit_ewma, 0.9, self.cfg.max_lead_time);
        }
        debug!(
            placeable = placeable.len(),
            unplaced = unplaced.len(),
            live = live.len(),
            "FFD simulation"
        );
        // r42 bug_023: snapshot `prev_extra_cells` BEFORE
        // `emit_tick_gauges` overwrites it — `reap_idle`'s Phase 0
        // gauge-reset needs the same `gauge_universe` set so a cell
        // removed from config gets one trailing zero-write for
        // `consolidate_threshold_seconds` after it drains.
        let prev_extras_for_reap = self.prev_extra_cells.clone();
        self.emit_tick_gauges(&live, &placeable, &unplaced, now);
        // r[impl ctrl.nodeclaim.placeable-gate+5]
        // Publish `intent_id`s FFD-placed on a `Registered=True` node
        // (`in_flight == false`). The `pool/jobs` reconciler retains
        // only these — Jobs are NOT created for intents placed on
        // in-flight claims (the pod would sit Pending until the claim
        // registers; `cover_deficit` already provisioned for them, so
        // the next tick after Registered picks them up). `send_replace`:
        // dropped receivers (controller shutdown) are not an error.
        let on_registered: HashSet<String> = placeable
            .iter()
            .filter(|(_, _, in_flight)| !in_flight)
            .map(|(i, _, _)| i.intent_id.clone())
            .collect();
        self.placeable_tx
            .send_replace(Some(Arc::new(on_registered)));

        // OA2: controller-side per-node wedge clustering — the only
        // hung-node signal (the scheduler's heartbeat-fed detector and
        // its `dead_nodes` field are gone with the stream protocol).
        // The controller derives the Dead-equivalent signal from the
        // open-attempt ledger view (deadline expiries clustered per
        // source node). RPC failure is fail-open for observation
        // only: `update(None, ..)` skips the tick's observation AND
        // verdict — previously accumulated evidence stays, and no node
        // is marked from stale data (the retired empty-view path ran
        // the verdict over an empty fleet, mass-marking every
        // retained-evidence node right after a systemic episode).
        // r[impl ctrl.nodeclaim.wedge-cluster+1]
        let open_attempts = match admin_call(
            self.admin
                .clone()
                .list_open_attempts(ListOpenAttemptsRequest {}),
        )
        .await
        {
            Ok(r) => Some(r.into_inner().attempts),
            Err(e) => {
                warn!(error = %e, "ListOpenAttempts failed; node-wedge clustering skips this tick's observation");
                None
            }
        };
        // r[impl ctrl.nodeclaim.wedge-two-axis+3]
        // Only per-node verdicts may feed the Dead arm; a systemic
        // pattern marks nothing (the warn + suppression counter fired
        // inside the sealed single-exit verdict). Last tick's reaps are
        // the REQUIRED eviction feed — a reaped node's evidence is dead.
        let evictions = std::mem::take(&mut self.pending_wedge_evictions);
        let dead_input = match self.wedge.update(open_attempts.as_deref(), &evictions, now) {
            wedge::WedgeVerdict::NodeWedged(nodes, _) => nodes,
            wedge::WedgeVerdict::Systemic { affected, of, .. } => {
                debug!(
                    affected,
                    of, "wedge verdict systemic; Dead arm receives no wedge input this tick"
                );
                Vec::new()
            }
            // bug_151: the unobserved tick is its own verdict — no
            // marking from stale data, no suppression accounting, and
            // structurally distinct from "zero nodes wedged".
            wedge::WedgeVerdict::Unobserved(_) => {
                debug!("wedge view unobserved this tick; Dead arm receives no wedge input");
                Vec::new()
            }
        };

        // Reap unhealthy/ICE BEFORE cover_deficit so cells that just
        // hit ICE this tick are masked in the same tick's cover (don't
        // immediately re-create what we just deleted). `reap_unhealthy`
        // catches `Launched=False reason=LaunchFailed` claims still IN
        // `live`; `detect_vanished` catches claims Karpenter already
        // GC'd between ticks (the ~1s GC < 10s tick race the live
        // Part-B finding hit).
        let health::ReapOutcome {
            mut ice_cells,
            reaped_claims: reaped,
            reaped_nodes,
        } = health::reap_unhealthy(
            &self.nodeclaims,
            &live,
            &dead_input,
            &self.sketches,
            &self.cfg,
            now,
        )
        .await?;
        // All reap reasons feed the wedge eviction stash (consumed by
        // the NEXT tick's update — reaps run after this tick's verdict).
        self.pending_wedge_evictions.extend(reaped_nodes);
        // r[impl ctrl.nodeclaim.inflight-conservation+2]
        // r40 bug_020: drop the controller's own reaps from inflight_created
        // BEFORE detect_vanished scans, so they're not misread as Karpenter
        // GC on the next tick. (reap_idle only reaps registered claims —
        // detect_vanished's `Some(n) if n.registered → false` arm already
        // drops those.)
        for name in &reaped {
            self.inflight_created.remove(name);
        }
        ice_cells.extend(health::detect_vanished(&mut self.inflight_created, &live));
        // bug_082: ICE marks enter the commit-on-Ack buffer AT the
        // production site — `report_unfulfillable` builds the request
        // from the buffer, so a failed Ack retains them exactly like
        // the sibling planes (the producers are consume-once; a
        // dropped mark can never be re-observed).
        self.pending_evidence.ice_cells.extend(ice_cells);
        // Mask = the scheduler's acked view ∪ every buffered-but-
        // unacked mark: a mark the scheduler has not (provably)
        // received must still keep cover_deficit out of the cell —
        // pre-fix only the local tick's cells masked, so the tick
        // after a failed Ack re-minted into a cell that just ICE'd.
        let mut masked: Vec<String> = intents.ice_masked_cells.clone();
        masked.extend(self.pending_evidence.ice_cells.iter().map(Cell::to_string));

        let cover = self.cover_deficit(&unplaced, &live, &masked).await?;
        debug!(created = cover.created.len(), "deficit cover");
        self.inflight_created.extend(cover.created.iter().cloned());
        // Kube-authoritative `intent_id → spec.nodeName` for the
        // scheduler's hung-node detector. Full set every tick (one
        // entry per bound builder pod) so the scheduler's
        // `authoritative_binding` map stays current without delta
        // tracking; cardinality is O(active builds).
        self.report_unfulfillable(pod_snapshot.bound_intent_protos())
            .await?;

        // r42 bug_023: same gauge_universe as `emit_live_gauges` —
        // configured ∪ live cells ∪ one trailing tick of cells
        // removed from config. Without the union, `reap_idle` Phase
        // 0 never zeroes `consolidate_threshold_seconds` for an
        // orphaned cell once it drains, contradicting the
        // `describe_gauge!` "0 when no idle nodes" promise.
        let configured = self.cfg.all_cells(&self.hw_config);
        let (all_cells, _) = gauge_universe(
            &configured,
            live.iter().filter_map(|n| n.cell.clone()),
            &prev_extras_for_reap,
        );
        consolidate::reap_idle(
            &self.nodeclaims,
            &live,
            &mut self.sketches,
            &consolidate::ReapInputs {
                placeable: &placeable,
                all_cells: &all_cells,
                prev_idle: &self.prev_idle,
                cfg: &self.cfg,
                hw_admits: |h, a, f| {
                    self.hw_config.matches_arch(h, a)
                        && rio_common::k8s::features_compatible(f, &self.hw_config.provides_for(h))
                },
                now_secs: now,
            },
        )
        .await?;

        if !self.reload_pending() {
            self.sketches.persist(&self.pg).await?;
        }
        Ok(())
    }

    /// Consolidate-only mode: scheduler has been unreachable for
    /// [`BOT_TICKS_BEFORE_CONSOLIDATE_ONLY`] ticks. Don't grow the
    /// fleet; DO keep reaping idle/unhealthy (kube-only reads).
    ///
    /// `placeable_tx` is intentionally NOT republished here (nor on the
    /// pre-threshold ⊥-tick early-return in `reconcile_once`): the FFD
    /// sim needs scheduler intents we don't have. Publishing `None`
    /// would unarm the gate for no benefit — `pool/jobs::reconcile`
    /// can't reach the scheduler either, so it has `intents=[]` and the
    /// stale set filters nothing. See [`PlaceableGate`] for the full
    /// staleness argument and the lease-loss contrast.
    async fn consolidate_only(&mut self, now_sys: std::time::SystemTime) -> anyhow::Result<()> {
        debug!(
            consecutive_bot = self.consecutive_bot_ticks,
            "consolidate-only (scheduler unreachable)"
        );
        // Same per-tick Pod LIST as `reconcile_once` (kube-only read):
        // `requested` feeds the idle/busy observation and `reap_idle`'s
        // busy guard. The bound-intent half goes unused here — every
        // consumer of it needs the scheduler.
        let pod_snapshot = self.list_pool_pods().await?;
        let live = self.list_live_nodeclaims(&pod_snapshot).await?;
        let now = epoch_secs(now_sys);
        // r43 bug_023: same kube-only block as `reconcile_once`. The
        // scheduler-bound outputs are BUFFERED (merged_bug_007): the
        // Registered edge is consume-once (`recorded_boot`), so an
        // ICE-clear or observed instance type produced here must
        // survive to the next healthy Ack — discarding it lost the
        // evidence permanently. `report_unfulfillable` and the
        // scheduler's `CostTable.cells` still need the scheduler; the
        // buffer drains on the next reconcile_once.
        let ev = self.kube_only_observations(&live, now, now_sys);
        self.pending_evidence.merge(ev);
        // r42 bug_023: `consolidate_only` calls `emit_live_gauges`
        // AFTER `reap_idle`, so `prev_extra_cells` is still the
        // previous tick's value here — use it directly.
        let configured = self.cfg.all_cells(&self.hw_config);
        let (all_cells, _) = gauge_universe(
            &configured,
            live.iter().filter_map(|n| n.cell.clone()),
            &self.prev_extra_cells,
        );
        consolidate::reap_idle(
            &self.nodeclaims,
            &live,
            &mut self.sketches,
            &consolidate::ReapInputs {
                placeable: &[],
                all_cells: &all_cells,
                prev_idle: &self.prev_idle,
                cfg: &self.cfg,
                hw_admits: |h, a, f| {
                    self.hw_config.matches_arch(h, a)
                        && rio_common::k8s::features_compatible(f, &self.hw_config.provides_for(h))
                },
                now_secs: now,
            },
        )
        .await?;
        // No wedge evidence without the scheduler (the open-attempt
        // view is unreadable); local
        // ICE-timeout detection still runs on `live`. bug_082 sibling:
        // the marks produced here are consume-once like everywhere
        // else — they enter the commit-on-Ack buffer and ship on the
        // next reconcile_once Ack (pre-fix they were dropped because
        // "report_unfulfillable needs the scheduler reachable", which
        // conflated DELIVERY being impossible this tick with the
        // EVIDENCE being disposable).
        let outcome =
            health::reap_unhealthy(&self.nodeclaims, &live, &[], &self.sketches, &self.cfg, now)
                .await?;
        let reaped = outcome.reaped_claims;
        self.pending_wedge_evictions.extend(outcome.reaped_nodes);
        self.pending_evidence.ice_cells.extend(outcome.ice_cells);
        // r[impl ctrl.nodeclaim.inflight-conservation+2]
        // r[impl ctrl.nodeclaim.consolidate-only-degraded+3]
        // r40 bug_012: prune inflight_created against this tick's `live`
        // so the controller's own reaps below aren't later misread by
        // reconcile_once's detect_vanished as Karpenter GC. The
        // doc-comment on `detect_vanished` ("the controller never deletes
        // its own in-flight claims") only holds if every code path that
        // deletes a tracked claim also removes it from `inflight_created`
        // — `reconcile_once` did, `consolidate_only` did not.
        for name in &reaped {
            self.inflight_created.remove(name);
        }
        let vanished = health::detect_vanished(&mut self.inflight_created, &live);
        self.pending_evidence.ice_cells.extend(vanished);
        // FFD-derived gauges (`ffd_unplaced_cores`, `ffd_placeable_intents`)
        // need scheduler intents; live-derived gauges read only `live` +
        // `now`, both available here. Without this call, a scheduler
        // outage freezes `nodeclaim_inflight_age_max_seconds` at its
        // pre-outage value and `RioNodeclaimPoolStuckPending` reads
        // stale data exactly when the operator needs it.
        self.emit_live_gauges(&live, now);
        if !self.reload_pending() {
            self.sketches.persist(&self.pg).await?;
        }
        Ok(())
    }

    /// Per-cell gauges derived from `live` + `now` only (no scheduler
    /// intents needed). Iterates `cfg.all_cells() ∪ by_state.keys() ∪
    /// prev_extra_cells` so every (h,cap) timeseries is emitted every
    /// tick — Prometheus gauge semantics: a cell that drained to 0 reads
    /// as 0, not stale-at-last-nonzero. Called from BOTH `reconcile_once`
    /// (via `emit_tick_gauges`) and `consolidate_only` so
    /// `RioNodeclaimPoolStuckPending` stays accurate during scheduler
    /// outages.
    fn emit_live_gauges(&mut self, live: &[ffd::LiveNode], now_secs: f64) {
        // `(registered, inflight, terminating, max_inflight_age, max_term_age)`.
        // The three counts partition `live` — every NodeClaim is exactly one
        // — so `state=registered` matches FFD's placement-candidate set
        // (terminating excluded). The investigation's smoking gun
        // (`live=1` while x2lm4 was draining → `deficit=0`) becomes
        // `live{state=registered}=0, live{state=terminating}=1` —
        // visible without log diving.
        let mut by_state: BTreeMap<Cell, (u64, u64, u64, f64, f64)> = BTreeMap::new();
        for n in live {
            let Some(c) = n.cell.clone() else { continue };
            let e = by_state.entry(c).or_default();
            if let Some(ts) = n.terminating_since {
                e.2 += 1;
                e.4 = e.4.max((now_secs - ts).max(0.0));
            } else if n.registered {
                e.0 += 1;
            } else {
                e.1 += 1;
                e.3 = e.3.max(n.age_secs(now_secs).unwrap_or(0.0));
            }
        }
        // r41 bug_025: union of config-derived and label-derived cells,
        // plus one trailing tick — see [`gauge_universe`].
        // `consolidate::reap_idle` Phase 0 takes the same
        // `gauge_universe` set via `ReapInputs.all_cells` (r42
        // bug_023) — both gauges drain trailing zeros from the same
        // `prev_extra_cells` snapshot.
        let configured = self.cfg.all_cells(&self.hw_config);
        let (to_write, new_extras) = gauge_universe(
            &configured,
            by_state.keys().cloned(),
            &self.prev_extra_cells,
        );
        for cell in &to_write {
            let label = cell.to_string();
            let (reg, inf, term, age, term_age) =
                by_state.get(cell).copied().unwrap_or((0, 0, 0, 0.0, 0.0));
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "registered")
            .set(reg as f64);
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "inflight")
            .set(inf as f64);
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "terminating")
            .set(term as f64);
            metrics::gauge!("rio_controller_nodeclaim_inflight_age_max_seconds",
                "cell" => label.clone())
            .set(age);
            metrics::gauge!("rio_controller_nodeclaim_terminating_age_max_seconds",
                "cell" => label.clone())
            .set(term_age);
            metrics::gauge!("rio_controller_nodeclaim_lead_time_seconds", "cell" => label.clone())
                .set(self.sketches.lead_time(cell));
            // §13b closed-loop SLIs (`forecast_hit_ewma` is the EWMA
            // `schmitt_adjust` reads at mod.rs:1202; `at_cap` is the
            // widen-gate ceiling). 0.9/false for unknown cells = the
            // `CellState::default()` mid-zone seed, i.e. a no-op.
            let s = self.sketches.get(cell);
            metrics::gauge!("rio_controller_nodeclaim_forecast_hit_ewma",
                "cell" => label.clone())
            .set(s.map_or(0.9, |s| s.forecast_hit_ewma));
            metrics::gauge!("rio_controller_nodeclaim_lead_time_q_at_cap",
                "cell" => label.clone())
            .set(s.is_some_and(|s| s.at_cap(self.cfg.max_lead_time)) as u64 as f64);
            // r41 bug_026: `RioNodeclaimPoolStuckPending` anchors on this
            // (the reaper's actual threshold) instead of `3×lead_time`
            // (a proxy that can fall below `2×seed` once the cell learns).
            // Same expression `health::classify` evaluates at health.rs:122.
            let ice_timeout = self.sketches.get(cell).map_or_else(
                || 2.0 * self.cfg.seed_for(cell),
                |s| s.ice_timeout(self.cfg.seed_for(cell)),
            );
            metrics::gauge!("rio_controller_nodeclaim_ice_timeout_seconds",
                "cell" => label)
            .set(ice_timeout);
        }
        self.prev_extra_cells = new_extras;
    }

    /// Per-tick `r[obs.metric.controller]` gauges: live-derived (via
    /// [`Self::emit_live_gauges`]) plus FFD-derived (`ffd_unplaced_cores`,
    /// `ffd_placeable_intents`) which need scheduler intents.
    fn emit_tick_gauges(
        &mut self,
        live: &[ffd::LiveNode],
        placeable: &[ffd::Placement],
        unplaced: &[SpawnIntent],
        now_secs: f64,
    ) {
        self.emit_live_gauges(live, now_secs);
        // Σ unplaced cores per cheapest-A_open cell — same assignment
        // cover_deficit uses, so the gauge equals cover's per-cell input.
        // No mask: the gauge shows raw demand; ICE-masking is a cover
        // policy, not a demand metric.
        let none = HashSet::new();
        let (by_cell, _) =
            cover::assign_to_cells(unplaced, &self.sketches, &none, cover::cell_rank, |i, m| {
                self.cfg.fallback_cell(i, &self.hw_config, m)
            });
        // r41 bug_021 sibling: same `all_cells()` blind spot as
        // `cover_deficit` — `by_cell` is keyed on scheduler-stamped
        // cells. r43 bug_026: extras cells need the same trailing
        // zero-write as `nodeclaim_live*` — once an extras cell drops
        // out of `by_cell`, it's in neither `configured` nor `extras`
        // and the gauge series freezes at its last value forever
        // (`metrics-exporter-prometheus` never deregisters). The r41
        // verifier's "not an alert anchor → no trailing zero" reasoning
        // was a premise failure: a stale gauge is wrong regardless of
        // whether an alert reads it.
        let configured = self.cfg.all_cells(&self.hw_config);
        let (to_write, new_unplaced_extras) = gauge_universe(
            &configured,
            by_cell.keys().cloned(),
            &self.prev_unplaced_extras,
        );
        for cell in &to_write {
            let label = cell.to_string();
            let unplaced_cores: u32 = by_cell
                .get(cell)
                .map(|v| v.iter().map(|i| i.cores).sum())
                .unwrap_or(0);
            metrics::gauge!("rio_controller_ffd_unplaced_cores", "cell" => label)
                .set(f64::from(unplaced_cores));
        }
        self.prev_unplaced_extras = new_unplaced_extras;
        // Placeable split: NOT per-cell (an intent may target multiple
        // cells; the placement node's cell would mislead). The single
        // `state=registered|inflight` split is the warm-hit proxy.
        let (on_reg, on_inf) =
            placeable.iter().fold(
                (0u64, 0u64),
                |(r, i), (_, _, inf)| {
                    if *inf { (r, i + 1) } else { (r + 1, i) }
                },
            );
        metrics::gauge!("rio_controller_ffd_placeable_intents", "state" => "registered")
            .set(on_reg as f64);
        metrics::gauge!("rio_controller_ffd_placeable_intents", "state" => "inflight")
            .set(on_inf as f64);
    }

    /// One label-selected (`rio.build/pool`) Pod LIST → this tick's
    /// `pods::PodSnapshot`.
    ///
    /// §4(a)1: replaces the watch-fed `PodRequestedCache`. The LIST is
    /// scoped to the same selector the deleted watcher used, so the
    /// object count is O(active builds), not the cluster's pod set.
    /// Errors propagate — the tick aborts and retries in `TICK`
    /// seconds, the same fail-closed posture as a failed NodeClaim
    /// LIST (and strictly safer than the watch cache's stale-data
    /// degradation, which silently mis-sized FFD).
    async fn list_pool_pods(&self) -> anyhow::Result<pods::PodSnapshot> {
        let list = self
            .pods
            .list(&ListParams::default().labels(pool::POOL_LABEL))
            .await?;
        Ok(pods::PodSnapshot::derive(&list.items))
    }

    /// List NodeClaims this reconciler owns (label-selected). Typed
    /// `Api<NodeClaim>` (B4) so `status.allocatable` / `conditions` are
    /// already decoded — no `serde_json::Value` paths.
    /// `LiveNode.requested` is post-filled from `pod_snapshot` so
    /// `free()` reflects what's already bound.
    async fn list_live_nodeclaims(
        &self,
        pod_snapshot: &pods::PodSnapshot,
    ) -> anyhow::Result<Vec<ffd::LiveNode>> {
        let list = self
            .nodeclaims
            .list(&ListParams::default().labels(OWNER_LABEL))
            .await?;
        Ok(list
            .items
            .into_iter()
            .map(|nc| {
                let mut n = ffd::LiveNode::from(nc);
                if let Some(node) = &n.node_name {
                    n.requested = pod_snapshot.requested_for(node);
                }
                n
            })
            .collect())
    }

    /// §13b deficit cover.
    ///
    /// 1. Group `unplaced` by cheapest cell in each intent's `A_open`
    ///    (`cover::assign_to_cells`). ICE-masked cells are filtered
    ///    from `A_open` so an intent fails over to its OD variant.
    ///    hw-agnostic intents (`hw_class_names=[]`, cold-start
    ///    `fit=None`) route to [`NodeClaimPoolConfig::fallback_cell`].
    /// 2. Round-robin `cfg.all_cells()` from `tick_counter` so no cell
    ///    starves under sustained pressure.
    /// 3. Per cell with deficit: [`cover::sizing`] returns the per-claim
    ///    `(c, m, d)` triples — `n = max(⌈Σ/max_node_*⌉)` across all
    ///    three axes, each claim sized to `max(Σ/n, sorted_desc[k])` so
    ///    the production FFD sim places every intent. Karpenter resolves
    ///    each against the hw-class's `requirements` to pick the
    ///    instance type.
    /// 4. `budget = max_fleet_cores − Σ live.allocatable.cpu −
    ///    created_this_tick`. The sum covers both Registered AND
    ///    in-flight claims so a slow-to-register burst doesn't
    ///    double-provision next tick.
    ///
    /// `Api::create` failures are warned + skipped (next tick retries);
    /// the method only propagates errors that would make the tick
    /// non-progressing.
    async fn cover_deficit(
        &self,
        unplaced: &[SpawnIntent],
        live: &[ffd::LiveNode],
        ice_masked: &[String],
    ) -> anyhow::Result<CoverResult> {
        if unplaced.is_empty() {
            return Ok(CoverResult::default());
        }
        // §13c-3: the controller is air-gapped — the global per-claim
        // cap comes ONLY from the scheduler's resolved global over
        // `GetHwClassConfig`. `None` (not yet loaded, or pre-§13c-3
        // scheduler whose proto field reads 0) → skip this tick,
        // fail-closed. Self-heals within ≤300s (next `hw_refresh`).
        // r[impl scheduler.sla.global.controller-mirror]
        let Some((global_cores, global_mem)) = self.hw_config.global_ceilings() else {
            warn!(
                unplaced = unplaced.len(),
                "§13c-3: GetHwClassConfig global ceilings not yet loaded; \
                 skipping cover this tick (self-heals on next 300s refresh)"
            );
            return Ok(CoverResult::default());
        };
        let ice: HashSet<Cell> = ice_masked.iter().filter_map(|s| Cell::parse(s)).collect();
        let live_cores: u32 = live.iter().map(|n| n.allocatable.0).sum();
        let mut created_cores = 0u32;

        let (by_cell, dropped) =
            cover::assign_to_cells(unplaced, &self.sketches, &ice, cover::cell_rank, |i, m| {
                self.cfg.fallback_cell(i, &self.hw_config, m)
            });
        // r41 merged_015: the runbook (sla-model.typ §NoHostingClass) and
        // pod.rs:769,772 both claim "the controller logs WARN once per
        // intent drop" — sibling reasons `no_pool_covers`,
        // `exceeds_cell_cap`, and `unknown_hw_class` already warn; this
        // was the asymmetric outlier. Per intent-tick (10s), so a
        // persistent gap is visible in logs without waiting for the 15m
        // alert window.
        //
        // Two `reason`s, two operator actions: collapsing them once
        // misdiagnosed an account-level AWS spot SLR gap as a
        // `[sla.hw_classes]` config gap (see `cover::DropTally`).
        if dropped.no_hosting_class > 0 {
            metrics::counter!(
                "rio_controller_nodeclaim_intent_dropped_total",
                "reason" => "no_hosting_class",
            )
            .increment(dropped.no_hosting_class);
            warn!(
                dropped = dropped.no_hosting_class,
                "SpawnIntents dropped — no configured hw-class can host them \
                 (wrong arch, footprint exceeds every class's max_cores/max_mem, \
                 or required_features unmatched); add or fix a [sla.hw_classes] \
                 entry. See sla-model.typ#rionodeclaimpool-nohostingclass"
            );
        }
        if dropped.all_cells_ice_masked > 0 {
            metrics::counter!(
                "rio_controller_nodeclaim_intent_dropped_total",
                "reason" => "all_cells_ice_masked",
            )
            .increment(dropped.all_cells_ice_masked);
            warn!(
                dropped = dropped.all_cells_ice_masked,
                masked_cells = ice.len(),
                "SpawnIntents dropped — every cell that could host them is \
                 ICE-masked (NodeClaim launches failing in the cloud, NOT a \
                 [sla.hw_classes] config gap); check the Karpenter controller \
                 log for capacity/quota/IAM errors and \
                 `rio_controller_nodeclaim_reaped_total{{reason=~\"ice|vanished\"}}`. \
                 See sla-model.typ#rionodeclaimpool-icemaskedhigh"
            );
        }
        let order =
            cover::cells_round_robin(self.cfg.all_cells(&self.hw_config), self.tick_counter);

        let mut created = Vec::new();
        // §13c T8: per-hwClass cores minted THIS TICK, keyed by `cell.0`
        // so spot+od share one cap (per-hwClass not per-Cell — D4).
        let mut class_created: HashMap<String, u32> = HashMap::new();
        for cell in &order {
            if ice.contains(cell) {
                continue;
            }
            let Some(u) = by_cell.get(cell) else {
                continue;
            };
            // Per-class ceilings (e.g. arm-only pool topping at 64c)
            // bound each claim so Karpenter's instance-type discovery
            // for THIS hw-class can fulfill it. Global caps still
            // apply (a misconfigured per-class > global is clamped).
            // `ceilings_for=None` (config not yet loaded, or pre-R26
            // scheduler) → global only.
            let (cls_c, cls_m) = self
                .hw_config
                .ceilings_for(&cell.0)
                .unwrap_or((global_cores, global_mem));
            let scfg = cover::SizingCfg {
                max_node_cores: cls_c.min(global_cores),
                max_node_mem: cls_m.min(global_mem),
                max_node_disk: self.cfg.max_node_disk,
                per_tick_cap: self.cfg.max_node_claims_per_cell_per_tick,
                budget: cover::class_budget(
                    self.cfg
                        .max_fleet_cores
                        .saturating_sub(live_cores)
                        .saturating_sub(created_cores),
                    self.hw_config.fleet_cap_for(&cell.0),
                    live,
                    &cell.0,
                    class_created.get(&cell.0).copied().unwrap_or(0),
                ),
                fuse_cache_bytes: self.cfg.fuse_cache_bytes,
            };
            let (claims, min_eta) = cover::sizing(cell, u, &scfg);
            if claims.is_empty() {
                debug!(%cell, budget = scfg.budget, "no claims (budget exhausted or empty)");
                continue;
            }
            let Some(hw_labels) = self.hw_config.labels_for(&cell.0) else {
                warn!(hw_class = %cell.0, "no hw-class labels (GetHwClassConfig not loaded?); skipping");
                continue;
            };
            let hw = cover::HwClassCtx {
                node_class: self
                    .hw_config
                    .node_class_for(&cell.0)
                    .filter(|nc| !nc.is_empty())
                    .unwrap_or_else(|| {
                        warn!(hw_class = %cell.0, "node_class empty (GetHwClassConfig stale?); using rio-default");
                        "rio-default".into()
                    }),
                labels: hw_labels,
                requirements: self.hw_config.requirements_for(&cell.0).unwrap_or_default(),
                taints: self
                    .hw_config
                    .taints_for(&cell.0)
                    .into_iter()
                    .map(|t| k8s_openapi::api::core::v1::Taint {
                        key: t.key,
                        value: (!t.value.is_empty()).then_some(t.value),
                        effect: t.effect,
                        ..Default::default()
                    })
                    .collect(),
                // §13e: cover::build_nodeclaim branches the role
                // taint+label on `provides_features ∋ fetcher` —
                // same map the scheduler routes against.
                provides_features: self.hw_config.provides_for(&cell.0),
            };
            let cover_cfg = cover::CoverCfg {
                metal_sizes: &self.cfg.metal_sizes,
            };
            for &(c, m, d) in &claims {
                let nc = cover::build_nodeclaim(cell, (c, m, d), min_eta, &hw, &cover_cfg);
                match self.nodeclaims.create(&PostParams::default(), &nc).await {
                    Ok(out) => {
                        let name = out.metadata.name.unwrap_or_default();
                        debug!(%cell, %name, cores = c, "NodeClaim created");
                        metrics::counter!(
                            "rio_controller_nodeclaim_created_total",
                            "cell" => cell.to_string(),
                        )
                        .increment(1);
                        created.push((name, cell.clone()));
                        // r[impl ctrl.nodeclaim.budget.per-class+2]
                        // r40 bug_015: budget counters track cores MINTED this
                        // tick (`class_budget` doc, cover.rs:339). A failed
                        // create is neither Registered nor in-flight; counting
                        // it phantom-consumes up to per_tick_cap × max_node_cores
                        // (8×192 = 1536c) of `global_remaining`, under-
                        // provisioning cells later in the round-robin. Increment
                        // only on Ok so `created_cores` ⟺ `created.len()`.
                        created_cores += c;
                        *class_created.entry(cell.0.clone()).or_default() += c;
                    }
                    Err(e) => {
                        warn!(%cell, error = %e, "NodeClaim create failed; skipping");
                    }
                }
            }
        }
        // r41 bug_021: `assign_to_cells` keys `by_cell` on scheduler-
        // stamped cells (`cells_of(i)` reads `hw_class_names`/
        // `node_affinity`, both written by the SCHEDULER at solve time).
        // The cover loop above only visits `order = all_cells()` —
        // derived from the CONTROLLER's `hw_config`, refreshed every
        // ≤300s via `GetHwClassConfig`. During a config rollout the
        // scheduler can stamp a hwClass the controller hasn't loaded
        // yet; those intents land in `by_cell`, the loop never visits
        // their cell, and they're silently dropped — no NodeClaim, no
        // metric, no warn. The runbook's
        // `RioNodeclaimPoolNoHostingClass` diagnose step explicitly
        // names this case ("the controller's `HwClassConfig` is stale")
        // and calls the alert "the ONLY signal" — but the alert reads
        // `intent_dropped_total{reason=no_hosting_class}` which fires
        // on `fallback_cell → None`, a different path. Surface the skew
        // with its own reason. Mirrors the sibling
        // `global_ceilings()`-absent / `labels_for()`-absent failure
        // modes in this function which already `warn!`.
        let (skewed, unknown) = unknown_cell_intents(&by_cell, &order);
        if skewed > 0 {
            metrics::counter!(
                "rio_controller_nodeclaim_intent_dropped_total",
                "reason" => "unknown_hw_class",
            )
            .increment(skewed);
            warn!(
                dropped = skewed,
                cells = ?unknown,
                "SpawnIntents target hwClasses not in the controller's \
                 all_cells() — scheduler/controller GetHwClassConfig skew \
                 (self-heals within ≤300s if the RPC is healthy) or hwClass \
                 removed from controller config; if this persists past 5min, \
                 check rio-controller GetHwClassConfig RPC errors and the \
                 scheduler/controller deployment ages"
            );
        }
        Ok(CoverResult { created })
    }

    /// Report the buffered ICE marks (`unfulfillable_cells`),
    /// `Registered=True` edges (`registered_cells`) and observed
    /// instance types to the scheduler via `AckSpawnedIntents`. The
    /// scheduler's ICE backoff ladder marks/clears each. `spawned` is
    /// empty: the `Pool` reconciler owns Job-creation acks (it creates
    /// the Jobs; this reconciler only gates which intents are eligible
    /// via [`PlaceableGate`]). bug_082: ALL evidence planes of this
    /// request are built FROM `pending_evidence` — the function takes
    /// no evidence parameters, so a plane that bypasses the
    /// commit-on-Ack buffer cannot exist at this RPC. An Ack failure
    /// retains every plane for the next tick (the marks' producers are
    /// consume-once; "next tick retries" is only true for evidence
    /// that lives in the buffer).
    async fn report_unfulfillable(
        &mut self,
        bound_intents: Vec<rio_proto::types::BoundIntent>,
    ) -> anyhow::Result<()> {
        // C2/285: NO empty-tick suppression — this reconciler ALWAYS
        // attaches the binding snapshot (even empty), because
        // present-and-empty is load-bearing: the scale-to-zero tick
        // has zero bound pods and must SAY so or the scheduler's
        // `authoritative_binding` keeps stale entries for the whole
        // idle window (and mis-attributes the next re-dispatch). One
        // Ack per tick is the cost; the old all-empty early-return
        // suppressed exactly the tick that mattered.
        // r[impl ctrl.nodeclaim.ice-mark-clear]
        // BTreeSet dedup (now inherent — the buffer IS a set):
        // `health::reap_unhealthy`/`detect_vanished` push one entry
        // per ICE'd CLAIM (up to 8/cell/tick); the scheduler loops
        // `mark()` per entry so duplicates would jump step 0→7 (TTL
        // 60s→600s) on a single transient dip. Same for
        // `registered_cells` (1 per registered CLAIM → multiple
        // `clear()` calls is harmless but wasteful).
        //
        // merged_bug_045 (commit-on-Ack) + bug_082 (all three planes):
        // the buffered evidence ships BY READ — the buffer is cleared
        // ONLY in the Ack-Ok arm below. An Ack-Err or a mid-tick
        // `?`-abort earlier in the tick leaves it intact for the next
        // tick. Duplicate delivery after a successful-but-unobserved
        // Ack: ICE-clears are no-ops and observed-type upserts dedup
        // server-side (idempotent); a re-delivered ICE MARK advances
        // the backoff ladder one extra step — bounded, and in the
        // conservative direction (away from re-minting into a cell
        // that provably ICE'd).
        let req = AckSpawnedIntentsRequest {
            // The explicit per-tick snapshot (always present from this
            // reconciler; empty = clear). R9: the legacy field 5 is
            // NEVER dual-written — old schedulers simply keep their
            // map until the fleet rolls (bounded skew, accepted).
            binding_snapshot: Some(rio_proto::types::BindingSnapshot {
                bound: bound_intents,
            }),
            spawned: vec![],
            unfulfillable_cells: self
                .pending_evidence
                .ice_cells
                .iter()
                .map(Cell::to_string)
                .collect(),
            registered_cells: self
                .pending_evidence
                .registered_cells
                .iter()
                .map(Cell::to_string)
                .collect(),
            observed_instance_types: self.pending_evidence.observed_types.clone(),
            bound_intents: vec![],
        };
        // r[impl ctrl.nodeclaim.evidence-ack-latch+1]
        match admin_call(self.admin.clone().ack_spawned_intents(req)).await {
            Ok(_) => {
                // The ONLY clear: the evidence provably reached the
                // scheduler.
                self.pending_evidence = PendingSchedulerEvidence::default();
            }
            Err(e) => {
                warn!(
                    error = %e,
                    "ack_spawned_intents failed; all buffered evidence planes \
                     (ICE marks, ICE clears, observed types) retained for the \
                     next tick"
                );
            }
        }
        Ok(())
    }
}

/// Probe whether the NodeClaim CRD is installed. k3s VM tests without
/// Karpenter have no `nodeclaims.karpenter.sh` resource —
/// `list_live_nodeclaims` would 404 every tick, `placeable_tx` would
/// never publish, and the gate's `None` arm would `intents.clear()`
/// every Builder pool reconcile (no Jobs spawn). Main.rs uses this to
/// set `Ctx.placeable = None` (gate pass-through) and skip spawning
/// the reconciler entirely. `false` ONLY on a 404; transient errors
/// return `true` so the reconciler retries normally.
pub async fn nodeclaim_crd_present(kube: &kube::Client) -> bool {
    let api: Api<NodeClaim> = Api::all(kube.clone());
    match api
        .list_metadata(&kube::api::ListParams::default().limit(1))
        .await
    {
        Err(kube::Error::Api(e)) if e.code == 404 => {
            warn!(
                "NodeClaim CRD absent (k3s without Karpenter?) — \
                 nodeclaim_pool disabled, PlaceableGate pass-through"
            );
            false
        }
        _ => true,
    }
}

/// Result of one [`NodeClaimPoolReconciler::cover_deficit`] tick.
#[derive(Debug, Default)]
pub(crate) struct CoverResult {
    /// `(name, cell)` for NodeClaims created this tick. Fed into
    /// `inflight_created` so next tick's [`health::detect_vanished`]
    /// can ICE-mask cells whose claims Karpenter GC'd before we
    /// observed them.
    pub created: Vec<(String, Cell)>,
}

/// Connect the reconciler's PG pool. Separate from the scheduler/store
/// `init_db_pool` because the controller does NOT run migrations —
/// store/scheduler own the migrator and run before this reconciler
/// reaches `CellSketches::load_seeded` (controller's `connect_forever` to the
/// scheduler in main.rs already orders that). Max 4 connections: persist
/// is one upsert per cell per 10s tick.
/// `connect_pg` + `NodeClaimPoolReconciler::{new, run}` as a single
/// `async fn`. Standalone (not an `async move` block in main.rs)
/// because `connect_forever`'s inner `async ||` closure plus a
/// borrowed param inside a nested `async move` block trips rustc's
/// HRTB Send check (rust-lang/rust issue 102211 family); a named
/// `async fn` desugars without the higher-ranked lifetime.
#[allow(clippy::too_many_arguments)]
pub async fn run_nodeclaim_pool(
    kube: kube::Client,
    admin: AdminClient,
    leader: LeaderState,
    hooks: ControllerLeaseHooks,
    cfg: NodeClaimPoolConfig,
    hw_config: HwClassConfig,
    placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    shutdown: rio_common::signal::Token,
) {
    let Some(pg) = connect_pg(&cfg.database_url, &shutdown).await else {
        return;
    };
    NodeClaimPoolReconciler::new(kube, admin, pg, leader, hooks, cfg, hw_config, placeable_tx)
        .await
        .run(shutdown)
        .await;
}

async fn connect_pg(
    database_url: &str,
    shutdown: &rio_common::signal::Token,
) -> Option<sqlx::PgPool> {
    // Hand-rolled retry (NOT `connect_forever`): the `async ||` form
    // trips rustc's HRTB Send check when the caller is itself spawned
    // via `spawn_monitored` (rust-lang/rust issue 102211 family).
    // Same 1→2→4→8→16s-steady backoff schedule.
    let mut delay = Duration::from_secs(1);
    loop {
        let try_connect = sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .min_connections(1)
            .idle_timeout(Duration::from_secs(60))
            .connect(database_url);
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => return None,
            r = try_connect => match r {
                Ok(pg) => return Some(pg),
                Err(e) => {
                    warn!(error = %e, "PG connect failed; retrying");
                    tokio::select! {
                        biased;
                        _ = shutdown.cancelled() => return None,
                        _ = tokio::time::sleep(delay) => {}
                    }
                    delay = (delay * 2).min(Duration::from_secs(16));
                }
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default() {
        let d = NodeClaimPoolConfig::default();
        assert!(d.database_url.is_empty());
        assert!(d.lease_name.is_none());
        assert!(d.reference_hw_class.is_empty());
        assert_eq!(d.max_fleet_cores, 10_000);
        assert_eq!(d.max_node_claims_per_cell_per_tick, 8);
        // r40 bug_018: matches the chart's `buildScheduler.enabled`
        // default — `true` so the second-scheduler routing is on by
        // default and a missing TOML key doesn't silently disable it.
        assert!(d.kube_build_scheduler_enabled);
        // bug_040: non-zero default so an unseeded cell's
        // health::classify timeout (2×seed) covers ~18s real boot.
        assert_eq!(d.default_lead_time_seed, 30.0);
        assert_eq!(d.seed_for(&Cell("nope".into(), CapacityType::Spot)), 30.0);
        // r35 bug_050: default fetcher floor restores the pre-§13e
        // Karpenter `consolidateAfter: 10m` policy via the prefix glob.
        assert_eq!(
            d.min_consolidation_time_for(&Cell("fetcher-x86".into(), CapacityType::Spot)),
            Some(600.0)
        );
        // Builder cells get a 300s floor: the NA-model floor
        // `boot_median/2` (~9s) is below the boot cost (~18s) — reaping
        // there is strictly dominated. 300s covers the sequential
        // inter-build gap (~60s) AND `>-<` DAG bottlenecks (mostly
        // <5min) where the §13b 1-layer forecast can't see the
        // wide-again layer. `fetcher-*` (prefix 8) overrides `*`
        // (prefix 0) per longest-prefix precedence.
        assert_eq!(
            d.min_consolidation_time_for(&Cell("hi-ebs-x86".into(), CapacityType::Spot)),
            Some(300.0),
            "builder cells get the 300s universal floor"
        );
        assert_eq!(
            d.min_consolidation_time_for(&Cell("metal-arm".into(), CapacityType::OnDemand)),
            Some(300.0),
        );
    }

    /// r35 bug_050: `min_consolidation_time_for` lookup precedence.
    /// Exact hw-class match wins over any glob; among globs, the
    /// longest prefix wins regardless of map iteration order (a
    /// shorter-glob-wins outcome would be a determinism bug). A key
    /// that does NOT end with `*` is an EXACT match only; an operator
    /// who writes `"fetcher": 600.0` expecting a glob gets a silent
    /// no-match — the doc names this, and this test pins it.
    #[test]
    fn min_consolidation_time_lookup_precedence() {
        let cfg = NodeClaimPoolConfig {
            min_consolidation_time: BTreeMap::from([
                ("fetcher-x86".into(), 100.0), // exact
                ("fetcher-*".into(), 200.0),   // glob, prefix len 8
                ("fetcher*".into(), 300.0),    // glob, prefix len 7
                ("metal".into(), 400.0),       // exact, no `*` — NOT a glob
            ]),
            ..Default::default()
        };
        let cell = |h: &str| Cell(h.into(), CapacityType::Spot);
        // Exact wins over both globs.
        assert_eq!(
            cfg.min_consolidation_time_for(&cell("fetcher-x86")),
            Some(100.0)
        );
        // No exact → longest matching glob prefix wins (8 > 7).
        assert_eq!(
            cfg.min_consolidation_time_for(&cell("fetcher-arm")),
            Some(200.0)
        );
        // Only the shorter glob matches (no `-` after `fetcher`).
        assert_eq!(
            cfg.min_consolidation_time_for(&cell("fetchernvme")),
            Some(300.0)
        );
        // `"metal"` without `*` is exact-only — does NOT match `metal-x86`.
        assert_eq!(cfg.min_consolidation_time_for(&cell("metal-x86")), None);
        assert_eq!(cfg.min_consolidation_time_for(&cell("metal")), Some(400.0));
        // Unmatched cell → None.
        assert_eq!(cfg.min_consolidation_time_for(&cell("hi-ebs-x86")), None);
    }

    /// r40 bug_024: `seed_for` accepts both `"h:od"` and `"h:on-demand"`
    /// key forms — matching `CellSketches::seed()`, the scheduler's
    /// `cell_key_serde`, and helm test 18 §5. Without this, an operator
    /// who writes the Karpenter `on-demand` vocabulary (one keystroke
    /// from `capacityTypes: ["on-demand"]`) passes every static gate,
    /// seeds the dashboard sketch, and STILL gets `default_lead_time_seed`
    /// from `health::classify` → boot-timeout reap loop.
    #[test]
    fn seed_for_accepts_on_demand_key_form() {
        let cell = Cell("metal-x86".into(), CapacityType::OnDemand);
        for key in ["metal-x86:od", "metal-x86:on-demand"] {
            let cfg = NodeClaimPoolConfig {
                lead_time_seed: [(key.to_string(), 600.0)].into(),
                default_lead_time_seed: 30.0,
                ..Default::default()
            };
            assert_eq!(cfg.seed_for(&cell), 600.0, "key form `{key}` should match");
        }
        // Both forms present → canonical `:od` wins (deterministic, not
        // HashMap iteration order). Pins the `.get()` -> `or_else(find_map)`
        // precedence; a regression to a bare `find_map` flakes here.
        let cfg = NodeClaimPoolConfig {
            lead_time_seed: [
                ("metal-x86:od".to_string(), 600.0),
                ("metal-x86:on-demand".to_string(), 700.0),
            ]
            .into(),
            default_lead_time_seed: 30.0,
            ..Default::default()
        };
        assert_eq!(
            cfg.seed_for(&cell),
            600.0,
            "`:od` precedence over `:on-demand`"
        );
        // Absent → default.
        let cfg = NodeClaimPoolConfig {
            lead_time_seed: HashMap::new(),
            default_lead_time_seed: 30.0,
            ..Default::default()
        };
        assert_eq!(cfg.seed_for(&cell), 30.0);
    }

    /// r41 bug_025: a live NodeClaim whose hwClass is removed from
    /// config mid-rollout still bills against `max_fleet_cores` but
    /// drops out of `all_cells()`. `emit_live_gauges` must keep writing
    /// `terminating_age_max_seconds{cell}` so `StuckTerminating` can
    /// fire — the §SCC sibling of `reap_idle`'s "derive on demand"
    /// fallback. After the NodeClaim finishes draining (drops out of
    /// `live` AND `all_cells()`), the gauge must get one trailing
    /// zero-write so a resolved stuck-drain doesn't page forever.
    #[test]
    fn gauge_universe_covers_extras_and_trailing() {
        let configured = vec![Cell("known-x86".into(), CapacityType::OnDemand)];
        let removed = Cell("removed-x86".into(), CapacityType::OnDemand);

        // Tick 1: hwClass removed from config; NodeClaim still draining.
        let prev = HashSet::new();
        let (to_write, extras) =
            gauge_universe(&configured, std::iter::once(removed.clone()), &prev);
        assert!(
            to_write.contains(&removed),
            "extra cell must be gauged while live"
        );
        assert!(extras.contains(&removed), "tracked for trailing zero-write");

        // Tick 2: NodeClaim finished draining; not in `live` or `all_cells()`.
        // One trailing zero-write so StuckTerminating clears.
        let (to_write, extras) = gauge_universe(&configured, std::iter::empty(), &extras);
        assert!(
            to_write.contains(&removed),
            "vanished extra gets one trailing zero-write"
        );
        assert!(
            extras.is_empty(),
            "no longer tracked after the trailing write"
        );

        // Tick 3: gone for good.
        let (to_write, _) = gauge_universe(&configured, std::iter::empty(), &extras);
        assert!(
            !to_write.contains(&removed),
            "no further writes after the trailing tick"
        );
        assert_eq!(to_write, configured, "back to configured-only");

        // Re-added to config: stops being an extra (configured wins).
        let (_, extras) = gauge_universe(
            std::slice::from_ref(&removed),
            std::iter::once(removed.clone()),
            &HashSet::new(),
        );
        assert!(extras.is_empty(), "configured cell is never an extra");
    }

    /// r43 bug_026: `ffd_unplaced_cores` extras get the same trailing
    /// zero-write as `nodeclaim_live*`. Without it, an extras cell that
    /// drops out of `by_cell` (demand clears) while still unconfigured
    /// freezes at its last non-zero value forever
    /// (`metrics-exporter-prometheus` never deregisters series).
    /// Reuses [`gauge_universe`]; the only thing this test pins beyond
    /// `gauge_universe_covers_extras_and_trailing` is the
    /// `ffd_unplaced_cores` callsite shape (`by_cell.keys()` →
    /// `live_cells`, `prev_unplaced_extras` → `prev_extras`).
    #[test]
    fn ffd_unplaced_extras_get_trailing_zero() {
        let configured = vec![Cell("a-x86".into(), CapacityType::Spot)];
        let unknown = Cell("z-x86".into(), CapacityType::Spot);

        // Tick N: unknown cell has unplaced demand (`by_cell` has it).
        let prev = HashSet::new();
        let (to_write, new_extras) =
            gauge_universe(&configured, std::iter::once(unknown.clone()), &prev);
        assert!(
            to_write.contains(&unknown),
            "extras cell gauged while demand persists"
        );
        assert!(
            new_extras.contains(&unknown),
            "tracked for trailing zero-write"
        );

        // Tick N+1: demand clears — `by_cell` no longer has `unknown`.
        let (to_write, new_extras) = gauge_universe(&configured, std::iter::empty(), &new_extras);
        assert!(
            to_write.contains(&unknown),
            "extras cell must get one trailing zero-write"
        );
        assert!(
            !new_extras.contains(&unknown),
            "trailing write is one-shot; not re-tracked"
        );

        // Tick N+2: gone for good.
        let (to_write, _) = gauge_universe(&configured, std::iter::empty(), &new_extras);
        assert!(
            !to_write.contains(&unknown),
            "no further writes after trailing tick"
        );
    }

    /// r42 bug_023: `reap_idle` Phase 0's gauge-reset of
    /// `consolidate_threshold_seconds` previously iterated only
    /// `cfg.all_cells()` — a cell removed from config that drains its
    /// last NodeClaim never got a final zero-write, contradicting the
    /// `describe_gauge!` "0 when no idle nodes" promise.
    /// `reconcile_once` and `consolidate_only` now pass the same
    /// `gauge_universe()` set through `ReapInputs.all_cells`. This test
    /// asserts the wiring shape: `gauge_universe(configured, live_cells,
    /// prev_extras_for_reap)` yields a trailing entry for an orphaned
    /// cell exactly one tick after it leaves `live`, then drops it.
    #[test]
    fn reap_idle_phase0_universe_includes_trailing_orphaned_cell() {
        let configured = vec![Cell("known-x86".into(), CapacityType::OnDemand)];
        let orphan = Cell("removed-x86".into(), CapacityType::OnDemand);

        // Tick N: orphaned cell still has a live NodeClaim. The reap
        // call site builds `live_cells` from `live.iter().filter_map(
        // |n| n.cell.clone())` — an orphan with a draining node is
        // included in `to_write` AND tracked in `extras`.
        let prev_extras_for_reap: HashSet<Cell> = HashSet::new();
        let live_cells = std::iter::once(orphan.clone());
        let (to_write, extras) = gauge_universe(&configured, live_cells, &prev_extras_for_reap);
        assert!(
            to_write.contains(&orphan),
            "orphan in ReapInputs.all_cells while a node is draining"
        );
        assert!(extras.contains(&orphan));

        // Tick N+1: NodeClaim drained, no live node carries the cell.
        // `prev_extras_for_reap` (snapshotted BEFORE `emit_tick_gauges`
        // overwrites `prev_extra_cells`) still has the orphan → one
        // trailing zero-write.
        let prev_extras_for_reap = extras;
        let (to_write, extras) =
            gauge_universe(&configured, std::iter::empty(), &prev_extras_for_reap);
        assert!(
            to_write.contains(&orphan),
            "orphan gets one trailing zero-write for consolidate_threshold_seconds"
        );
        assert!(extras.is_empty());

        // Tick N+2: gone for good.
        let (to_write, _) = gauge_universe(&configured, std::iter::empty(), &extras);
        assert!(
            !to_write.contains(&orphan),
            "no further writes after trailing tick"
        );
    }

    /// r41 bug_021: `assign_to_cells` keys `by_cell` on scheduler-
    /// stamped cells; the cover loop visits only `all_cells()`. The
    /// difference must be observable so config skew has a signal.
    #[test]
    fn unknown_cell_intents_partitions_by_order() {
        let intent = SpawnIntent::default();
        let known = Cell("x86-64".into(), CapacityType::OnDemand);
        let unknown_cell = Cell("unknown-x86".into(), CapacityType::OnDemand);
        let order = vec![known.clone()];

        // by_cell has an entry the cover loop will never visit.
        let by_cell: BTreeMap<Cell, Vec<&SpawnIntent>> = [
            (known.clone(), vec![&intent]),
            (unknown_cell.clone(), vec![&intent]),
        ]
        .into();
        let (n, unknown) = unknown_cell_intents(&by_cell, &order);
        assert_eq!(n, 1, "one intent stranded in the scheduler-only cell");
        assert_eq!(unknown, vec![&unknown_cell]);

        // by_cell ⊆ order → nothing stranded.
        let by_cell: BTreeMap<Cell, Vec<&SpawnIntent>> = [(known, vec![&intent])].into();
        let (n, unknown) = unknown_cell_intents(&by_cell, &order);
        assert_eq!(n, 0);
        assert!(unknown.is_empty());
    }

    /// `fallback_cell`: prefers `(reference_hw_class, Spot)` when its
    /// arch matches; else first (sorted) hw-class of matching arch;
    /// else `None`. Arch-agnostic hw-class (no `kubernetes.io/arch`
    /// label) matches any arch.
    #[test]
    fn fallback_cell_reference_then_first_by_arch() {
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "mid-ebs-x86".into(),
            ..Default::default()
        };
        let hw = HwClassConfig::from_literals(&[
            ("mid-ebs-x86", &[(ARCH_LABEL, "amd64")]),
            ("lo-ebs-arm", &[(ARCH_LABEL, "arm64")]),
            ("hi-ebs-arm", &[(ARCH_LABEL, "arm64")]),
        ]);
        let i = |sys: &str| SpawnIntent {
            system: sys.into(),
            ..Default::default()
        };
        let none = HashSet::new();
        // x86 → reference cell (arch matches).
        assert_eq!(
            cfg.fallback_cell(&i("x86_64-linux"), &hw, &none),
            Some(Cell("mid-ebs-x86".into(), CapacityType::Spot))
        );
        // aarch64 → reference is amd64; first sorted arm cell wins.
        assert_eq!(
            cfg.fallback_cell(&i("aarch64-linux"), &hw, &none),
            Some(Cell("hi-ebs-arm".into(), CapacityType::Spot))
        );
        // Unmappable system → None.
        assert_eq!(cfg.fallback_cell(&i("builtin"), &hw, &none), None);
        assert_eq!(cfg.fallback_cell(&i(""), &hw, &none), None);
        // No matching-arch hw-class loaded → None.
        let hw2 = HwClassConfig::from_literals(&[("mid-ebs-x86", &[(ARCH_LABEL, "amd64")])]);
        assert_eq!(cfg.fallback_cell(&i("aarch64-linux"), &hw2, &none), None);
        // mb_024(2): reference cell ICE-masked → next arch-matching
        // cell instead of stranding the cold-start probe on a cell
        // cover_deficit then `continue`s.
        let masked: HashSet<Cell> = [Cell("mid-ebs-x86".into(), CapacityType::Spot)].into();
        let hw_m = HwClassConfig::from_literals(&[
            ("mid-ebs-x86", &[(ARCH_LABEL, "amd64")]),
            ("lo-ebs-x86", &[(ARCH_LABEL, "amd64")]),
        ]);
        assert_eq!(
            cfg.fallback_cell(&i("x86_64-linux"), &hw_m, &masked),
            Some(Cell("lo-ebs-x86".into(), CapacityType::Spot)),
            "masked reference fails over to next arch-match"
        );
        // Arch-agnostic reference (no arch label) matches any system —
        // the kwok `vmtest` fixture case.
        let cfg3 = NodeClaimPoolConfig {
            reference_hw_class: "vmtest".into(),
            ..Default::default()
        };
        let hw3 =
            HwClassConfig::from_literals(&[("vmtest", &[("kubernetes.io/hostname", "agent")])]);
        assert_eq!(
            cfg3.fallback_cell(&i("x86_64-linux"), &hw3, &none),
            Some(Cell("vmtest".into(), CapacityType::Spot))
        );
    }

    /// r35 B1 (§13d placement⊇provisioning STRIKE-2): a `system="builtin"`
    /// FOD (`required_features=["fetcher"]`, `hw_class_names=[]`) MUST route
    /// to a `provides_features=["fetcher"]` class even though
    /// `system_to_arch("builtin")` returns `None`. Pre-fix the arch
    /// `?`-early-return dropped to `None` → `no_hosting_class` → no
    /// NodeClaim minted → fetcher pod permanently Pending with no alert
    /// (bug_003). The featureless arm (`fallback_cell_reference_then_
    /// first_by_arch`'s `i("builtin")` pin above) is preserved — a
    /// featureless arch-unmappable intent has NO constraint axis to
    /// route on; `None` is correct there.
    #[test]
    fn fallback_cell_builtin_fod_routes_to_fetcher() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch};
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "mid-ebs-x86".into(),
            ..Default::default()
        };
        let arch = |a: &str| NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: a.into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "mid-ebs-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 64,
                        max_mem: 256 << 30,
                        ..Default::default()
                    },
                ),
                (
                    "fetcher-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 8,
                        max_mem: 32 << 30,
                        provides_features: vec!["fetcher".into()],
                        capacity_types: vec!["od".into()],
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );
        let none = HashSet::new();
        // builtin FOD → fetcher-x86 (the only class providing
        // `fetcher`). Pre-B1 `system_to_arch("builtin")?` returned
        // `None` here.
        let fod = SpawnIntent {
            system: "builtin".into(),
            required_features: vec!["fetcher".into()],
            cores: 1,
            mem_bytes: 2 << 30,
            ..Default::default()
        };
        assert_eq!(
            cfg.fallback_cell(&fod, &hw, &none),
            Some(Cell("fetcher-x86".into(), CapacityType::OnDemand)),
            "builtin FOD must route to the fetcher class via features"
        );
    }

    /// r35 B1 negative: featureless arch-unmappable intent stays `None`.
    /// The B1 guard `(arch.is_none() && i.required_features.is_empty())`
    /// preserves the early-return for this arm — there is no constraint
    /// axis to route on, so `no_hosting_class` is the right answer.
    /// Sibling of `fallback_cell_reference_then_first_by_arch`'s
    /// `i("builtin")` and `i("")` pins.
    #[test]
    fn fallback_cell_unmappable_featureless_returns_none() {
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "mid-ebs-x86".into(),
            ..Default::default()
        };
        let hw = HwClassConfig::from_literals(&[("mid-ebs-x86", &[(ARCH_LABEL, "amd64")])]);
        let none = HashSet::new();
        let i = SpawnIntent {
            system: "darwin-pdp11".into(),
            required_features: vec![],
            ..Default::default()
        };
        assert_eq!(
            cfg.fallback_cell(&i, &hw, &none),
            None,
            "featureless arch-unmappable intent has no constraint axis to route on"
        );
    }

    /// r27 mb_006 producer-side: an hw-agnostic intent (override
    /// bypass-path) with `cores > class.max_cores` must NOT route to
    /// that cell — find a class that CAN host it, or `None` (caller's
    /// `no_hosting_class` metric). Without this, `cover::sizing`'s
    /// `exceeds_cell_cap` backstop drops it AFTER assignment; this
    /// filter delivers the invariant upstream.
    #[test]
    fn fallback_cell_filters_by_per_class_ceilings() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch};
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "lo-ebs-x86".into(),
            ..Default::default()
        };
        let arch = |a: &str| NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: a.into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "lo-ebs-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 32,
                        max_mem: 64 << 30,
                        ..Default::default()
                    },
                ),
                (
                    "hi-ebs-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 128,
                        max_mem: 256 << 30,
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );
        let mk = |cores: u32| SpawnIntent {
            system: "x86_64-linux".into(),
            cores,
            ..Default::default()
        };
        let none = HashSet::new();
        // 16c fits reference (32c cap).
        assert_eq!(
            cfg.fallback_cell(&mk(16), &hw, &none),
            Some(Cell("lo-ebs-x86".into(), CapacityType::Spot))
        );
        // 64c exceeds reference (32c) → fails over to hi (128c cap).
        assert_eq!(
            cfg.fallback_cell(&mk(64), &hw, &none),
            Some(Cell("hi-ebs-x86".into(), CapacityType::Spot)),
            "reference too small → next ceiling-fitting class"
        );
        // 256c exceeds ALL classes → None (no_hosting_class).
        assert_eq!(cfg.fallback_cell(&mk(256), &hw, &none), None);
    }

    /// §13d STRIKE-7 (r30 mb_012): `fallback_cell` filters by
    /// `provides_features`. The pre-r30 doc claimed kvm intents always
    /// get `hw_class_names=[metal-*]` — false for cold-start
    /// (`fit=None`). A cold-start kvm intent with `hw_class_names=[]`
    /// must NOT fall back to the non-metal reference cell (the kvm
    /// pod CrashLoopBackOffs on ENXIO `/dev/kvm`; pool-static
    /// nodeSelector deleted r33 bug_002).
    /// Inverse (∅-guard): a featureless intent must NOT route to a
    /// kvm-tainted metal cell (pod has no toleration → wasted on-demand
    /// metal Node).
    #[test]
    fn fallback_cell_filters_by_provides_features() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch};
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "mid-ebs-x86".into(),
            ..Default::default()
        };
        let arch = |a: &str| NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: a.into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "mid-ebs-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 64,
                        max_mem: 256 << 30,
                        ..Default::default()
                    },
                ),
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 64,
                        max_mem: 256 << 30,
                        provides_features: vec!["kvm".into()],
                        capacity_types: vec!["od".into()],
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );
        let mk = |features: &[&str]| SpawnIntent {
            system: "x86_64-linux".into(),
            cores: 4,
            mem_bytes: 8 << 30,
            required_features: features.iter().map(|s| (*s).to_string()).collect(),
            ..Default::default()
        };
        let none = HashSet::new();
        // kvm intent → metal cell, NOT the non-metal reference.
        assert_eq!(
            cfg.fallback_cell(&mk(&["kvm"]), &hw, &none),
            Some(Cell("metal-x86".into(), CapacityType::OnDemand)),
            "kvm intent must route to metal-x86, not the non-metal reference"
        );
        // featureless intent → reference cell, NOT metal (∅-guard).
        assert_eq!(
            cfg.fallback_cell(&mk(&[]), &hw, &none),
            Some(Cell("mid-ebs-x86".into(), CapacityType::Spot)),
            "featureless intent must NOT route to kvm-tainted metal"
        );
        // No class hosts the feature → None (no_hosting_class).
        assert_eq!(
            cfg.fallback_cell(&mk(&["nixos-test", "kvm"]), &hw, &none),
            None,
            "no class provides nixos-test+kvm → None → no_hosting_class metric"
        );
    }

    /// §13d STRIKE-7 inverse-guard (r30 mb_012): a featureless intent
    /// must NEVER fall back to a kvm-tainted metal cell, even when ALL
    /// non-metal cells are ICE-masked. Stranding (return `None`) is
    /// correct: the masked non-metal cells re-arm after backoff;
    /// minting a metal node a featureless pod can't tolerate just burns
    /// on-demand budget.
    #[test]
    fn fallback_cell_featureless_excludes_metal_even_when_others_masked() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch};
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "mid-ebs-x86".into(),
            ..Default::default()
        };
        let arch = |a: &str| NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: a.into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "mid-ebs-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 64,
                        max_mem: 256 << 30,
                        ..Default::default()
                    },
                ),
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        max_cores: 64,
                        max_mem: 256 << 30,
                        provides_features: vec!["kvm".into()],
                        capacity_types: vec!["od".into()],
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );
        // Mask every non-metal cell.
        let masked: HashSet<Cell> = [
            Cell("mid-ebs-x86".into(), CapacityType::Spot),
            Cell("mid-ebs-x86".into(), CapacityType::OnDemand),
        ]
        .into();
        let i = SpawnIntent {
            system: "x86_64-linux".into(),
            cores: 4,
            mem_bytes: 8 << 30,
            ..Default::default()
        };
        assert_eq!(
            cfg.fallback_cell(&i, &hw, &masked),
            None,
            "featureless intent must strand on masked non-metal cells, \
             not fall back to kvm-tainted metal"
        );
    }

    /// `ControllerLeaseHooks` epochs propagate via shared `Arc` — the
    /// run loop's `load()` sees the lease loop's clone's `fetch_add`.
    /// `Clone` (LeaseHooks bound) so it can be passed to both
    /// `run_lease_loop` and `NodeClaimPoolReconciler::new`.
    // r[verify ctrl.nodeclaim.acquire-edge-token]
    #[test]
    fn lease_hooks_flags_propagate_via_clone() {
        use std::sync::atomic::Ordering::SeqCst;
        let h = ControllerLeaseHooks::default();
        let h2 = h.clone();
        rio_lease::LeaseHooks::on_acquire(&h2);
        assert_eq!(h.acquire_epoch.load(SeqCst), 1, "epoch bumped via clone");
        assert!(!h.lose.load(SeqCst));
        rio_lease::LeaseHooks::on_lose(&h2);
        assert!(h.lose.swap(false, SeqCst), "lose set via clone");
    }

    /// bug_346: the epoch token gives latch-on-Ok-only AND
    /// edge-actions-fire-once by construction. The two cursors track
    /// the epoch independently: `reloaded_epoch` lags until a load
    /// succeeds (persist gated); a re-acquire mid-Err-loop is a NEW
    /// epoch (re-fires once). There is no boolean to wrongly consume
    /// on Err or wrongly re-read on every tick.
    // r[verify ctrl.nodeclaim.acquire-edge-token]
    #[test]
    fn acquire_epoch_token_semantics() {
        use std::sync::atomic::Ordering::SeqCst;
        let h = ControllerLeaseHooks::default();
        rio_lease::LeaseHooks::on_acquire(&h);
        let (mut edge_seen, mut reloaded) = (0u64, 0u64);

        // Tick 1: edge fires once; load() Err — reloaded lags.
        let ep = h.acquire_epoch.load(SeqCst);
        let edge_fired_t1 = ep != edge_seen;
        edge_seen = ep;
        assert!(edge_fired_t1, "tick1: edge action fires");
        assert_ne!(reloaded, ep, "tick1: persist gated on Err");

        // Tick 2: same epoch — edge does NOT re-fire; still gated.
        let ep2 = h.acquire_epoch.load(SeqCst);
        assert_eq!(ep2, edge_seen, "tick2: edge does not re-fire");
        assert_ne!(reloaded, ep2, "tick2: still gated");

        // Tick 3: load() Ok — reloaded catches up; persist ungated.
        reloaded = ep2;
        assert_eq!(reloaded, ep2, "tick3: latched on Ok");

        // Re-acquire mid-loop: NEW epoch — edge re-fires exactly once.
        rio_lease::LeaseHooks::on_acquire(&h);
        let ep3 = h.acquire_epoch.load(SeqCst);
        assert_ne!(ep3, edge_seen, "new tenure: edge re-fires once");
        assert_ne!(reloaded, ep3, "new tenure: persist re-gated");
    }

    #[test]
    fn cover_result_default_empty() {
        let r = CoverResult::default();
        assert!(r.created.is_empty());
    }

    /// `all_cells` = hw-class names × `capacity_types_for(h)`.
    #[test]
    fn all_cells_derives_from_hw_config() {
        let cfg = NodeClaimPoolConfig::default();
        let hw = HwClassConfig::from_literals(&[
            ("h1", &[(ARCH_LABEL, "amd64")]),
            ("h2", &[(ARCH_LABEL, "arm64")]),
        ]);
        let mut cells = cfg.all_cells(&hw);
        cells.sort();
        assert_eq!(
            cells,
            vec![
                Cell("h1".into(), CapacityType::Spot),
                Cell("h1".into(), CapacityType::OnDemand),
                Cell("h2".into(), CapacityType::Spot),
                Cell("h2".into(), CapacityType::OnDemand),
            ]
        );
        assert!(cfg.all_cells(&HwClassConfig::default()).is_empty());
    }

    /// §13c T9/T11: `all_cells` and `fallback_cell` honor per-hwClass
    /// `capacity_types`. An od-only class (metal) MUST NOT produce a
    /// `(h, Spot)` cell — the spot cell would carry a `cap-type In
    /// [spot]` requirement (cell-derived) and Karpenter would
    /// successfully provision a spot metal node, violating the cost
    /// model.
    #[test]
    fn all_cells_and_fallback_honor_capacity_types() {
        use rio_proto::types::{HwClassLabels, NodeLabelMatch};
        let arch = |a: &str| NodeLabelMatch {
            key: ARCH_LABEL.into(),
            value: a.into(),
        };
        let hw = HwClassConfig::default();
        hw.set(
            [
                (
                    "metal-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        capacity_types: vec!["on-demand".into()],
                        ..Default::default()
                    },
                ),
                (
                    "std-x86".into(),
                    HwClassLabels {
                        labels: vec![arch("amd64")],
                        // capacity_types empty (pre-§13c scheduler) → ALL.
                        ..Default::default()
                    },
                ),
            ]
            .into(),
            (192, 1536 << 30),
        );
        let cfg = NodeClaimPoolConfig {
            reference_hw_class: "metal-x86".into(),
            ..Default::default()
        };
        let mut cells = cfg.all_cells(&hw);
        cells.sort();
        assert_eq!(
            cells,
            vec![
                Cell("metal-x86".into(), CapacityType::OnDemand),
                Cell("std-x86".into(), CapacityType::Spot),
                Cell("std-x86".into(), CapacityType::OnDemand),
            ],
            "od-only class must not produce a (h, Spot) cell"
        );
        // fallback_cell picks the first listed cap for the reference
        // class — OnDemand for metal.
        let i = SpawnIntent {
            system: "x86_64-linux".into(),
            ..Default::default()
        };
        assert_eq!(
            cfg.fallback_cell(&i, &hw, &HashSet::new()),
            Some(Cell("metal-x86".into(), CapacityType::OnDemand)),
            "od-only reference → fallback picks OnDemand, not Spot"
        );
    }

    /// §13d Pool axis (r31 bug_019): the provisioner's coverage filter
    /// uses the SAME predicate as the placer — `kind` + `systems`
    /// membership + `features_compatible`. An intent the placer would
    /// never Job-place must not be provisioned (mints a
    /// permanently-idle metal node).
    ///
    /// r35 B2: every test intent carries an EXPLICIT `kind` so the
    /// kind clause is exercised, never vacuously passed via
    /// `Default::default()` (proto `ExecutorKind` defaults to
    /// `Builder=0`).
    #[test]
    fn pool_covers_mirrors_placer_predicate() {
        use rio_proto::types::ExecutorKind as PKind;
        let s = |v: &[&str]| v.iter().map(|s| s.to_string()).collect::<Vec<_>>();
        let intent = |kind: PKind, sys: &str, feats: &[&str]| SpawnIntent {
            kind: kind.into(),
            system: sys.into(),
            required_features: s(feats),
            ..Default::default()
        };

        // Default chart: only Builder Pool is `(Builder, systems=[x86_64-linux,
        // i686-linux], features=[])`. A kvm intent has nowhere to go.
        let default_chart: Vec<PoolCoverage> =
            vec![(PKind::Builder, s(&["x86_64-linux", "i686-linux"]), vec![])];
        assert!(
            !pool_covers(
                &intent(PKind::Builder, "x86_64-linux", &["kvm"]),
                &default_chart
            ),
            "kvm intent uncovered when no kvm Pool configured (default chart) — pre-fix \
             this minted a permanently-idle on-demand metal node"
        );
        assert!(
            pool_covers(&intent(PKind::Builder, "x86_64-linux", &[]), &default_chart),
            "featureless x86 intent covered by featureless x86 Pool"
        );
        // arch axis: aarch64 intent with no aarch64 Pool.
        assert!(
            !pool_covers(
                &intent(PKind::Builder, "aarch64-linux", &[]),
                &default_chart
            ),
            "aarch64 intent uncovered when only x86 Pools exist"
        );

        // EKS-shaped: kvm Pool added → kvm intent covered.
        let eks: Vec<PoolCoverage> = vec![
            (PKind::Builder, s(&["x86_64-linux", "i686-linux"]), vec![]),
            (
                PKind::Builder,
                s(&["x86_64-linux"]),
                s(&["kvm", "nixos-test"]),
            ),
            (PKind::Builder, s(&["aarch64-linux"]), vec![]),
        ];
        assert!(pool_covers(
            &intent(PKind::Builder, "x86_64-linux", &["kvm"]),
            &eks
        ));
        assert!(pool_covers(
            &intent(PKind::Builder, "aarch64-linux", &[]),
            &eks
        ));
        // Bidirectional ∅-guard: a featureless intent does NOT match
        // the kvm Pool — but it matches the featureless x86 Pool.
        assert!(pool_covers(
            &intent(PKind::Builder, "x86_64-linux", &[]),
            &eks
        ));

        // `systems=[]` ⇔ no system filter (passes_intent_filter
        // semantics: `req.systems.is_empty()` skips the check).
        let agnostic: Vec<PoolCoverage> = vec![(PKind::Builder, vec![], vec![])];
        assert!(
            pool_covers(&intent(PKind::Builder, "aarch64-linux", &[]), &agnostic),
            "systems=[] Pool admits any arch"
        );

        // Empty coverage: no Builder Pools configured. Drops everything
        // (fail-safe, not fail-open — the placer wouldn't spawn either).
        // Distinct from a Pool *list* error, which fail-OPENs by
        // skipping the retain entirely (see `reconcile_once`).
        assert!(!pool_covers(
            &intent(PKind::Builder, "x86_64-linux", &[]),
            &[]
        ));

        // §13e: a Fetcher Pool's coverage tuple uses
        // `effective_features(spec)` (= [fetcher]), NOT spec.features
        // (= []). FOD intents carry required_features=[fetcher].
        // The bidirectional ∅-guard makes [fetcher] ⊄ [] strict —
        // building coverage from spec.features would drop EVERY FOD
        // intent here (no fetcher NodeClaim ever minted).
        let fetcher_pool = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        let fetcher_coverage: Vec<PoolCoverage> = vec![(
            crate::reconcilers::pool::executor_kind_to_proto(fetcher_pool.spec.kind),
            fetcher_pool.spec.systems.clone(),
            crate::reconcilers::pool::pod::effective_features(&fetcher_pool.spec),
        )];
        let fod = SpawnIntent {
            kind: PKind::Fetcher.into(),
            system: fetcher_pool.spec.systems[0].clone(),
            required_features: vec![rio_common::k8s::FETCHER_FEATURE.into()],
            ..Default::default()
        };
        assert!(
            pool_covers(&fod, &fetcher_coverage),
            "FOD intent covered by Fetcher Pool via effective_features (§13e)"
        );
        // Cross-kind partition: FOD ⊄ builder pool, builder intent ⊄
        // fetcher pool (the bidirectional ∅-guard, AND the kind axis).
        assert!(
            !pool_covers(&fod, &eks),
            "FOD intent NOT covered by featureless builder Pool (∅-guard + kind)"
        );
        let bld = intent(PKind::Builder, &fetcher_pool.spec.systems[0], &[]);
        assert!(
            !pool_covers(&bld, &fetcher_coverage),
            "featureless builder intent NOT covered by Fetcher Pool (∅-guard + kind)"
        );
    }

    /// **r35 B0/B2 boundary tripwire** — `pool_covers` checks the
    /// `kind` axis. A `kind=Builder` intent carrying
    /// `required_features=["fetcher"]` (impossible post-B0: the
    /// scheduler-side `EffectiveFeatures::derive` strips `fetcher`
    /// from non-FOD declared sets so the wire intent never carries it)
    /// would otherwise be accepted by a Fetcher Pool's coverage tuple
    /// `(Fetcher, systems, [fetcher])` —
    /// `features_compatible([fetcher], [fetcher]) = true`.
    ///
    /// B0 makes this input unreachable; B2 hardens the predicate
    /// anyway (defense-in-depth: a future scheduler regression must
    /// not silently mint fetcher nodes for builder intents). This
    /// test documents the contract and is the red-first proof for
    /// B2.
    #[test]
    fn pool_covers_rejects_kind_mismatch() {
        let fetcher_pool = crate::fixtures::test_pool("f", ExecutorKind::Fetcher);
        let fetcher_coverage: Vec<PoolCoverage> = vec![(
            crate::reconcilers::pool::executor_kind_to_proto(fetcher_pool.spec.kind),
            fetcher_pool.spec.systems.clone(),
            crate::reconcilers::pool::pod::effective_features(&fetcher_pool.spec),
        )];
        // A Builder intent SHOULD never carry `[fetcher]` (B0 strips
        // it scheduler-side) — but `pool_covers` is the controller's
        // last line of defense against a scheduler regression. Build
        // the impossible input directly.
        let bld_with_fetcher = SpawnIntent {
            system: fetcher_pool.spec.systems[0].clone(),
            required_features: vec![rio_common::k8s::FETCHER_FEATURE.into()],
            kind: crate::reconcilers::pool::executor_kind_to_proto(ExecutorKind::Builder).into(),
            ..Default::default()
        };
        assert!(
            !pool_covers(&bld_with_fetcher, &fetcher_coverage),
            "Builder intent with [fetcher] must NOT be covered by a \
             Fetcher Pool — `pool_covers` must check `kind`, not just \
             features (B2: a scheduler regression that re-leaks \
             `fetcher` into a builder intent would otherwise mint a \
             permanently-idle fetcher node)"
        );
        // The inverse: a Fetcher intent must NOT be covered by a
        // Builder Pool advertising `[fetcher]` (impossible — Builder
        // Pools' `effective_features(spec)` strips `fetcher` — but
        // build the impossible input directly for the same
        // defense-in-depth proof).
        let bld_advertising_fetcher: Vec<PoolCoverage> = vec![(
            rio_proto::types::ExecutorKind::Builder,
            fetcher_pool.spec.systems.clone(),
            vec![rio_common::k8s::FETCHER_FEATURE.into()],
        )];
        let fod = SpawnIntent {
            system: fetcher_pool.spec.systems[0].clone(),
            required_features: vec![rio_common::k8s::FETCHER_FEATURE.into()],
            kind: rio_proto::types::ExecutorKind::Fetcher.into(),
            ..Default::default()
        };
        assert!(
            !pool_covers(&fod, &bld_advertising_fetcher),
            "Fetcher FOD intent must NOT be covered by a Builder Pool — \
             `pool_covers` checks `kind` regardless of features"
        );
    }
}
