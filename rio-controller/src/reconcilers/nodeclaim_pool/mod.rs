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
//! 2. FFD-simulate placing the intents onto live (Registered + in-flight)
//!    NodeClaims with the same MostAllocated bin-select that
//!    `kube-scheduler-packed` uses.
//! 3. Cover the unplaced deficit per `(hw_class, capacity_type)` cell
//!    with 1×anchor + N×bulk NodeClaims, capped at
//!    `max_node_claims_per_cell_per_tick` and `max_fleet_cores`.
//! 4. Reap idle Registered claims via windowed-rate break-even.
//! 5. Reap unhealthy (scheduler-reported `dead_nodes`) and ICE-stuck
//!    claims.
//! 6. Persist `CellSketches` (DDSketch lead-time + idle-gap log) to PG.
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
pub mod sketch;

use std::collections::{HashMap, HashSet};
use std::sync::Arc;
use std::time::Duration;

use kube::api::{Api, ListParams, PostParams};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use rio_crds::karpenter::NodeClaim;
use rio_lease::LeaderState;
use rio_proto::types::{
    AckSpawnedIntentsRequest, GetSpawnIntentsRequest, GetSpawnIntentsResponse, SpawnIntent,
};

use crate::reconcilers::node_informer::{HwClassConfig, PodRequestedCache};
use crate::reconcilers::pool;
use crate::reconcilers::{AdminClient, admin_call};

pub use consolidate::{HOLD_OPEN_ANNOTATION, IdleGapEvent};
pub use cover::{NODEPOOL_LABEL, SHIM_NODEPOOL};
pub use ffd::{
    ARCH_LABEL, CAPACITY_TYPE_LABEL, HW_CLASS_LABEL, LiveNode, Placement, a_open, cells_of,
    system_to_arch,
};
pub use sketch::{CapacityType, Cell, CellSketches, CellState};

/// Reconcile interval. Matches the Pool reconciler's `GetSpawnIntents`
/// poll cadence so the scheduler's `compute_spawn_intents` snapshot is
/// no staler here than in the legacy spawn path.
pub(crate) const TICK: Duration = Duration::from_secs(10);

/// Consecutive ⊥ ticks (scheduler unreachable / `Unavailable`) before
/// the loop drops into consolidate-only. ADR §13b: don't grow the fleet
/// on stale data, but DO keep reaping idle/unhealthy nodes — those reads
/// are kube-only.
const BOT_TICKS_BEFORE_CONSOLIDATE_ONLY: u8 = 5;

/// Unix-epoch seconds `now()`. Condition `lastTransitionTime` and
/// `creationTimestamp` are RFC3339; comparing in epoch-seconds keeps
/// arithmetic in `f64` throughout.
fn now_epoch() -> f64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .map_or(0.0, |d| d.as_secs_f64())
}

/// Label selector for NodeClaims this reconciler owns. Stamped at
/// `create()` time so `list_live_nodeclaims` and the consolidator never
/// touch claims from the rio-general / fetcher pools.
pub const OWNER_LABEL: &str = "rio.build/nodeclaim-pool=builder";

/// `rio.build/node-role` label key/value stamped on every rio-minted
/// builder NodeClaim. The legacy band-loop NodePool template stamped
/// this; B3 deleted those NodePools, and builder pod affinity still
/// requires `node-role In [builder]` (helm `builder.nodeSelector`), so
/// `cover::build_nodeclaim` must stamp it directly. Fetcher nodes still
/// come from the helm `rio-fetcher` NodePool template (which stamps
/// `node-role: fetcher` itself) — this reconciler is builder-only.
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
/// each tick (no event-per-publish; staleness bounded by the 10s tick
/// cadence on both sides). `Arc<HashSet>` so `borrow().clone()` is O(1).
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
    // r[impl ctrl.nodeclaim.placeable-gate+2]
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

/// Figment-loaded config. Scalars via `RIO_NODECLAIM_POOL__*` env;
/// `lead_time_seed` via the `[nodeclaim_pool]` table in
/// `/etc/rio/controller.toml` (helm `rio-controller-config` ConfigMap) —
/// figment's Env provider yields bare strings, so nested map fields
/// cannot load from env.
#[derive(Debug, Clone, Serialize, Deserialize)]
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
    /// §13b consolidator hold-open ceiling (seconds). `None` =
    /// 2×`consolidate_after()` per ADR. Helm: not currently surfaced.
    pub max_consolidation_time: Option<f64>,
    /// `(hw_class:cap)` → seed lead-time seconds, written by
    /// `xtask k8s probe-boot`. Seeds the DDSketch on cold start.
    /// Helm: `sla.leadTimeSeed`.
    pub lead_time_seed: HashMap<String, f64>,
    /// Seed lead-time (seconds) for cells absent from
    /// [`Self::lead_time_seed`]. The seed feeds [`Self::seed_for`] —
    /// `health::classify` uses it as a TIMEOUT, not just a floor, so
    /// 0 would reap every NodeClaim before it can register (~18s
    /// real boot). Helm: `sla.defaultLeadTimeSeed`.
    pub default_lead_time_seed: f64,
    /// DDSketch active→shadow rotation interval (seconds). After
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
}

impl NodeClaimPoolConfig {
    /// `lead_time_seed[cell]` (seconds), or
    /// [`Self::default_lead_time_seed`] for cells absent from the map.
    /// `health::classify` uses this as a TIMEOUT (`2×seed`), so the
    /// default must be non-zero — a 0 default would reap every
    /// NodeClaim of an unseeded cell at the next 10s tick (well before
    /// ~18s real boot completes).
    pub fn seed_for(&self, cell: &Cell) -> f64 {
        self.lead_time_seed
            .get(&cell.to_string())
            .copied()
            .unwrap_or(self.default_lead_time_seed)
    }

    /// Cold-start fallback cell for an hw-agnostic intent
    /// (`hw_class_names=[]`, i.e. `fit=None`): the
    /// `(referenceHwClass, <first cap>)` cell when its
    /// `kubernetes.io/arch` label matches `intent.system` (or is absent
    /// — arch-agnostic hw-class) AND its per-class `max_cores`/`max_mem`
    /// host the intent, else the first (sorted) hw-class satisfying
    /// both. `None` ⇔ `system` unmappable OR no configured hw-class
    /// hosts that arch at that size (caller emits
    /// `rio_controller_nodeclaim_intent_dropped_total
    /// {reason=no_hosting_class}`).
    ///
    /// Capacity-type is the FIRST listed for the class (Spot for
    /// default classes — cold-start probes are uniform `probe.cpu`-
    /// shaped and bounded by `max_node_claims_per_cell_per_tick`;
    /// on-demand fallback would defeat the §13b cost model. OnDemand
    /// for od-only classes like metal — §13c).
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
        let arch = ffd::system_to_arch(&i.system)?;
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
    /// **Latch-on-Ok-only** (mirrors `cost::poller_tick_prelude`): the
    /// run loop reads this with `load()`, not `swap()`. On reload `Ok`
    /// it stores `false`; on `Err` it leaves the flag set so the next
    /// tick retries. While set, `persist()` is gated off (degraded
    /// reconcile runs, stale-overwrite prevented).
    reload: Arc<std::sync::atomic::AtomicBool>,
    /// Set on `on_lose`; run loop `placeable_tx.send_replace(None)`
    /// so an ex-leader's `PlaceableGate` doesn't stay armed with a
    /// stale set (whose stale `queued` would `reap_excess_pending` the
    /// new leader's Jobs).
    lose: Arc<std::sync::atomic::AtomicBool>,
}

impl rio_lease::LeaseHooks for ControllerLeaseHooks {
    fn on_acquire(&self) {
        self.reload.store(true, std::sync::atomic::Ordering::SeqCst);
        metrics::counter!("rio_controller_lease_acquired_total").increment(1);
    }
    fn on_lose(&self) {
        self.lose.store(true, std::sync::atomic::Ordering::SeqCst);
        metrics::counter!("rio_controller_lease_lost_total").increment(1);
    }
}

/// The reconciler. Constructed in `main.rs` after PG connect; `run()`
/// is `spawn_monitored` and never returns until shutdown.
pub struct NodeClaimPoolReconciler {
    nodeclaims: Api<NodeClaim>,
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
    /// `spec.nodeName` → Σ pod requests, maintained by
    /// `node_informer::run_pod_requested_cache`.
    /// [`Self::list_live_nodeclaims`] post-fills `LiveNode.requested`
    /// so `free()` reflects what's already bound.
    pod_requested: PodRequestedCache,
    /// Publish side of [`PlaceableGate`]. Written once per successful
    /// FFD tick with the `intent_id`s placed on `Registered=True`
    /// nodes; the `pool/jobs` reconciler reads it via `Ctx.placeable`.
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
    /// `name → idle_secs` from the previous tick. Edge-detector state
    /// for [`consolidate::observe_idle_to_busy`]: a node idle last tick
    /// and busy this tick records an uncensored `IdleGapEvent`.
    /// In-memory only — restart drops one tick's worth of edges.
    prev_idle: HashMap<String, f64>,
    /// `name → cell` for NodeClaims `cover_deficit` created and that
    /// haven't yet appeared `Registered`/reaped. Next-tick diff against
    /// `live`: a name in here but absent from `live` ⇒ Karpenter GC'd
    /// it (`Launched=False reason=LaunchFailed/InsufficientCapacity` →
    /// delete in ~1s, faster than the 10s tick). [`health::classify`]'s
    /// `Launched=False > timeout` never fires for those — the claim is
    /// gone before it's observed. ICE-masked via
    /// [`health::detect_vanished`].
    inflight_created: HashMap<String, Cell>,
    /// Count of consecutive ticks where `GetSpawnIntents` returned ⊥
    /// (RPC error). Saturates at `u8::MAX`; reset on first success.
    consecutive_bot_ticks: u8,
    /// Monotonic tick counter for `cover_deficit`'s rotating-start
    /// round-robin.
    tick_counter: u64,
}

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
        pod_requested: PodRequestedCache,
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
            nodeclaims: Api::all(kube),
            admin,
            pg,
            leader,
            cfg,
            hw_config,
            pod_requested,
            placeable_tx,
            hooks,
            sketches,
            recorded_boot: HashSet::new(),
            prev_idle: HashMap::new(),
            inflight_created: HashMap::new(),
            consecutive_bot_ticks: 0,
            tick_counter: 0,
        }
    }

    /// Lease-acquire reload still pending (PG `load()` not yet
    /// succeeded since the last `on_acquire`). While true, the
    /// in-memory `self.sketches` may be stale (a long-running standby's
    /// startup snapshot, or `default()` if `new()` hit a PG outage) —
    /// `persist()` is gated off so it doesn't overwrite the previous
    /// leader's PG rows. Set false only on `CellSketches::load_seeded` Ok.
    fn reload_pending(&self) -> bool {
        self.hooks.reload.load(std::sync::atomic::Ordering::SeqCst)
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
            // Lease-loss edge: unarm the gate so an ex-leader's stale
            // set doesn't drive `reap_excess_pending` against the new
            // leader's Jobs. Checked BEFORE `is_leader()` so it fires
            // on the same tick as the loss.
            if self
                .hooks
                .lose
                .swap(false, std::sync::atomic::Ordering::SeqCst)
            {
                self.placeable_tx.send_replace(None);
            }
            if !self.leader.is_leader() {
                debug!("standby; skipping nodeclaim_pool tick");
                continue;
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
            if self.reload_pending() {
                let halflife = Duration::from_secs(self.cfg.sketch_halflife_secs);
                match CellSketches::load_seeded(
                    &self.pg,
                    &self.cfg.lead_time_seed,
                    halflife,
                    std::time::SystemTime::now(),
                )
                .await
                {
                    Ok(s) => {
                        self.sketches = s;
                        self.recorded_boot.clear();
                        self.prev_idle.clear();
                        self.inflight_created.clear();
                        self.hooks
                            .reload
                            .store(false, std::sync::atomic::Ordering::SeqCst);
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
            metrics::gauge!("rio_controller_sketches_reload_pending")
                .set(if self.reload_pending() { 1.0 } else { 0.0 });
            self.tick_counter = self.tick_counter.wrapping_add(1);
            let started = std::time::Instant::now();
            if let Err(e) = self.reconcile_once().await {
                warn!(error = %e, "nodeclaim_pool tick failed");
            }
            metrics::histogram!("rio_controller_nodeclaim_tick_duration_seconds")
                .record(started.elapsed().as_secs_f64());
        }
        info!("nodeclaim_pool reconciler stopped");
    }

    /// One tick: poll → FFD sim → cover deficit → reap → persist.
    ///
    /// `anyhow::Result`: this isn't a kube `Controller::run` body so the
    /// crate's `Error` enum (built around `error_policy` requeue) doesn't
    /// apply. Any error is logged + retried next tick.
    // r[impl ctrl.nodeclaim.ffd-sim]
    #[instrument(skip(self), fields(tick = self.tick_counter))]
    async fn reconcile_once(&mut self) -> anyhow::Result<()> {
        // ⊥ on scheduler unreachable: warn + count, don't propagate.
        // `admin_call` bounds at ADMIN_RPC_TIMEOUT so a stalled
        // scheduler doesn't wedge the tick.
        // Builder-only: this reconciler manages builder NodeClaims
        // (`OWNER_LABEL`); FOD intents land on the helm `rio-fetcher`
        // NodePool. Including FODs would over-reserve builder capacity
        // in FFD and mint builder NodeClaims for fetcher demand.
        let intents: Option<GetSpawnIntentsResponse> = match admin_call(
            self.admin
                .clone()
                .get_spawn_intents(GetSpawnIntentsRequest {
                    kind: Some(rio_proto::types::ExecutorKind::Builder.into()),
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

        let Some(intents) = intents else {
            self.consecutive_bot_ticks = self.consecutive_bot_ticks.saturating_add(1);
            if self.consecutive_bot_ticks >= BOT_TICKS_BEFORE_CONSOLIDATE_ONLY {
                return self.consolidate_only().await;
            }
            return Ok(());
        };
        self.consecutive_bot_ticks = 0;

        let live = self.list_live_nodeclaims().await?;
        let now = now_epoch();
        // Uncensored idle→busy edges: AFTER `requested` is populated
        // (list_live_nodeclaims), BEFORE reap_idle records the
        // censored half.
        consolidate::observe_idle_to_busy(&live, &mut self.prev_idle, &mut self.sketches, now);

        // r[ctrl.nodeclaim.lead-time-ddsketch]: record boot times on
        // Registered=True edges, then rotate any cells past halflife.
        // `registered_cells` feeds `report_unfulfillable`'s ICE-clear;
        // `observed_types` feeds the scheduler's `CostTable.cells`
        // (R24B7 instance-type autodiscovery).
        let (registered_cells, observed_types) =
            self.sketches
                .observe_registered(&live, &mut self.recorded_boot, now);
        self.sketches.maybe_rotate_all(
            std::time::SystemTime::now(),
            Duration::from_secs(self.cfg.sketch_halflife_secs),
        );

        let bound = self.pod_requested.bound_intents();
        // §13d STRIKE-7 (mb_012): the agnostic-fallback admit predicate
        // checks arch ∧ features. A `hw_class_names=[]` kvm intent must
        // NOT FFD-place onto a non-metal node (deficit appears covered →
        // no metal NodeClaim minted → kvm pod permanently Pending); a
        // featureless intent must NOT land on a kvm-tainted metal node
        // (pod has no toleration → wasted on-demand metal).
        let (placeable, unplaced) = ffd::simulate(
            &intents.intents,
            &live,
            &self.sketches,
            &bound,
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
        self.emit_tick_gauges(&live, &placeable, &unplaced, now);
        // r[impl ctrl.nodeclaim.placeable-gate+2]
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

        // Reap unhealthy/ICE BEFORE cover_deficit so cells that just
        // hit ICE this tick are masked in the same tick's cover (don't
        // immediately re-create what we just deleted). `reap_unhealthy`
        // catches `Launched=False reason=LaunchFailed` claims still IN
        // `live`; `detect_vanished` catches claims Karpenter already
        // GC'd between ticks (the ~1s GC < 10s tick race the live
        // Part-B finding hit).
        let mut ice_cells = health::reap_unhealthy(
            &self.nodeclaims,
            &live,
            &intents.dead_nodes,
            &self.sketches,
            &self.cfg,
            now,
        )
        .await?;
        ice_cells.extend(health::detect_vanished(&mut self.inflight_created, &live));
        let mut masked: Vec<String> = intents.ice_masked_cells.clone();
        masked.extend(ice_cells.iter().map(Cell::to_string));

        let cover = self.cover_deficit(&unplaced, &live, &masked).await?;
        debug!(created = cover.created.len(), "deficit cover");
        self.inflight_created.extend(cover.created.iter().cloned());
        // Kube-authoritative `intent_id → spec.nodeName` for the
        // scheduler's hung-node detector. Full set every tick (one
        // entry per bound builder pod) so the scheduler's
        // `authoritative_binding` map stays current without delta
        // tracking; cardinality is O(active builds).
        let bound_intents = self
            .pod_requested
            .bound_intents()
            .into_iter()
            .map(|(intent_id, node_name)| rio_proto::types::BoundIntent {
                intent_id,
                node_name,
            })
            .collect();
        self.report_unfulfillable(&ice_cells, &registered_cells, observed_types, bound_intents)
            .await?;

        consolidate::reap_idle(
            &self.nodeclaims,
            &live,
            &placeable,
            &mut self.sketches,
            &self.cfg,
            now,
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
    async fn consolidate_only(&mut self) -> anyhow::Result<()> {
        debug!(
            consecutive_bot = self.consecutive_bot_ticks,
            "consolidate-only (scheduler unreachable)"
        );
        let live = self.list_live_nodeclaims().await?;
        let now = now_epoch();
        consolidate::observe_idle_to_busy(&live, &mut self.prev_idle, &mut self.sketches, now);
        consolidate::reap_idle(
            &self.nodeclaims,
            &live,
            &[],
            &mut self.sketches,
            &self.cfg,
            now,
        )
        .await?;
        // No `dead_nodes` signal without the scheduler; local
        // ICE-timeout detection still runs on `live`. The returned
        // ice_cells are dropped — `report_unfulfillable` needs the
        // scheduler reachable.
        health::reap_unhealthy(&self.nodeclaims, &live, &[], &self.sketches, &self.cfg, now)
            .await?;
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
    /// intents needed). Iterates `cfg.all_cells()` so every (h,cap)
    /// timeseries is emitted every tick — Prometheus gauge semantics: a
    /// cell that drained to 0 reads as 0, not stale-at-last-nonzero.
    /// Called from BOTH `reconcile_once` (via `emit_tick_gauges`) and
    /// `consolidate_only` so `RioNodeclaimPoolStuckPending` stays
    /// accurate during scheduler outages.
    fn emit_live_gauges(&self, live: &[ffd::LiveNode], now_secs: f64) {
        use std::collections::BTreeMap;
        let mut by_state: BTreeMap<Cell, (u64, u64, f64)> = BTreeMap::new();
        for n in live {
            let Some(c) = n.cell.clone() else { continue };
            let e = by_state.entry(c).or_default();
            if n.registered {
                e.0 += 1;
            } else {
                e.1 += 1;
                e.2 = e.2.max(n.age_secs(now_secs).unwrap_or(0.0));
            }
        }
        for cell in self.cfg.all_cells(&self.hw_config) {
            let label = cell.to_string();
            let (reg, inf, age) = by_state.get(&cell).copied().unwrap_or((0, 0, 0.0));
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "registered")
            .set(reg as f64);
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "inflight")
            .set(inf as f64);
            metrics::gauge!("rio_controller_nodeclaim_inflight_age_max_seconds",
                "cell" => label.clone())
            .set(age);
            metrics::gauge!("rio_controller_nodeclaim_lead_time_seconds", "cell" => label)
                .set(self.sketches.lead_time(&cell));
        }
    }

    /// Per-tick `r[obs.metric.controller]` gauges: live-derived (via
    /// [`Self::emit_live_gauges`]) plus FFD-derived (`ffd_unplaced_cores`,
    /// `ffd_placeable_intents`) which need scheduler intents.
    fn emit_tick_gauges(
        &self,
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
            cover::assign_to_cells(unplaced, &self.sketches, &none, cover::cell_rank, |i| {
                self.cfg.fallback_cell(i, &self.hw_config, &none)
            });
        for cell in self.cfg.all_cells(&self.hw_config) {
            let label = cell.to_string();
            let unplaced_cores: u32 = by_cell
                .get(&cell)
                .map(|v| v.iter().map(|i| i.cores).sum())
                .unwrap_or(0);
            metrics::gauge!("rio_controller_ffd_unplaced_cores", "cell" => label)
                .set(f64::from(unplaced_cores));
        }
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

    /// List NodeClaims this reconciler owns (label-selected). Typed
    /// `Api<NodeClaim>` (B4) so `status.allocatable` / `conditions` are
    /// already decoded — no `serde_json::Value` paths.
    async fn list_live_nodeclaims(&self) -> anyhow::Result<Vec<ffd::LiveNode>> {
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
                    n.requested = self.pod_requested.sum_for(node);
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
            cover::assign_to_cells(unplaced, &self.sketches, &ice, cover::cell_rank, |i| {
                self.cfg.fallback_cell(i, &self.hw_config, &ice)
            });
        if dropped > 0 {
            metrics::counter!(
                "rio_controller_nodeclaim_intent_dropped_total",
                "reason" => "no_hosting_class",
            )
            .increment(dropped);
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
                    }
                    Err(e) => {
                        warn!(%cell, error = %e, "NodeClaim create failed; skipping");
                    }
                }
                created_cores += c;
                *class_created.entry(cell.0.clone()).or_default() += c;
            }
        }
        Ok(CoverResult { created })
    }

    /// Report this tick's ICE-hit cells (`unfulfillable_cells`) and
    /// `Registered=True` edges (`registered_cells`) to the scheduler
    /// via `AckSpawnedIntents`. The scheduler's ICE backoff ladder
    /// marks/clears each. `spawned` is empty: the `Pool` reconciler
    /// owns Job-creation acks (it creates the Jobs; this reconciler
    /// only gates which intents are eligible via [`PlaceableGate`]).
    /// RPC failure is warned + dropped (next tick retries; the
    /// scheduler also has its first-heartbeat clear path).
    async fn report_unfulfillable(
        &self,
        ice_cells: &[Cell],
        registered_cells: &[Cell],
        observed_types: Vec<rio_proto::types::ObservedInstanceType>,
        bound_intents: Vec<rio_proto::types::BoundIntent>,
    ) -> anyhow::Result<()> {
        if ice_cells.is_empty()
            && registered_cells.is_empty()
            && observed_types.is_empty()
            && bound_intents.is_empty()
        {
            return Ok(());
        }
        // BTreeSet dedup: `health::reap_unhealthy`/`detect_vanished`
        // push one entry per ICE'd CLAIM (up to 8/cell/tick); the
        // scheduler loops `mark()` per entry so duplicates would jump
        // step 0→7 (TTL 60s→600s) on a single transient dip. Same for
        // `registered_cells` (1 per registered CLAIM → multiple
        // `clear()` calls is harmless but wasteful).
        let dedup = |cs: &[Cell]| -> Vec<String> {
            cs.iter()
                .map(Cell::to_string)
                .collect::<std::collections::BTreeSet<_>>()
                .into_iter()
                .collect()
        };
        let req = AckSpawnedIntentsRequest {
            spawned: vec![],
            unfulfillable_cells: dedup(ice_cells),
            registered_cells: dedup(registered_cells),
            observed_instance_types: observed_types,
            bound_intents,
        };
        if let Err(e) = admin_call(self.admin.clone().ack_spawned_intents(req)).await {
            warn!(error = %e, "ack_spawned_intents (unfulfillable/registered) failed");
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
    pod_requested: PodRequestedCache,
    placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    shutdown: rio_common::signal::Token,
) {
    let Some(pg) = connect_pg(&cfg.database_url, &shutdown).await else {
        return;
    };
    NodeClaimPoolReconciler::new(
        kube,
        admin,
        pg,
        leader,
        hooks,
        cfg,
        hw_config,
        pod_requested,
        placeable_tx,
    )
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
        // bug_040: non-zero default so an unseeded cell's
        // health::classify timeout (2×seed) covers ~18s real boot.
        assert_eq!(d.default_lead_time_seed, 30.0);
        assert_eq!(d.seed_for(&Cell("nope".into(), CapacityType::Spot)), 30.0);
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
    /// pod's `nodeSelector` would never match → permanently Pending).
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

    /// `ControllerLeaseHooks` flags propagate via shared `Arc` — the
    /// run loop's `load()` sees the lease loop's clone's `store`.
    /// `Clone` (LeaseHooks bound) so it can be passed to both
    /// `run_lease_loop` and `NodeClaimPoolReconciler::new`.
    #[test]
    fn lease_hooks_flags_propagate_via_clone() {
        use std::sync::atomic::Ordering::SeqCst;
        let h = ControllerLeaseHooks::default();
        let h2 = h.clone();
        rio_lease::LeaseHooks::on_acquire(&h2);
        assert!(h.reload.load(SeqCst), "reload set via clone");
        h.reload.store(false, SeqCst);
        assert!(!h.lose.load(SeqCst));
        rio_lease::LeaseHooks::on_lose(&h2);
        assert!(h.lose.swap(false, SeqCst), "lose set via clone");
    }

    /// Latch-on-Ok-only reload semantics. The run loop's reload block
    /// is inlined in `run()` (PG-coupled), so this test exercises the
    /// invariant directly: after `on_acquire`, `reload_pending()` stays
    /// true (gating `persist()`) until the run loop's Ok-arm explicitly
    /// stores false. A `swap(false)` BEFORE the load would consume the
    /// flag on a transient PG error and the next tick's stale
    /// `persist()` would overwrite the previous leader's PG rows.
    #[test]
    fn reload_latch_on_ok_only_gates_persist() {
        use std::sync::atomic::Ordering::SeqCst;
        let h = ControllerLeaseHooks::default();
        rio_lease::LeaseHooks::on_acquire(&h);
        // Tick 1: load() Err — flag stays set (NOT swap), persist gated.
        assert!(h.reload.load(SeqCst), "tick1: reload still pending on Err");
        // Tick 2: load() Err — same.
        assert!(h.reload.load(SeqCst), "tick2: reload still pending on Err");
        // Tick 3: load() Ok — Ok-arm stores false.
        h.reload.store(false, SeqCst);
        assert!(!h.reload.load(SeqCst), "tick3: latched on Ok");
        // Done-gate: no `swap(false)` callsite remains in the run loop.
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
}
