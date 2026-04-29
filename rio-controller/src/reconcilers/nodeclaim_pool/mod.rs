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
//! 4. Reap idle Registered claims via Nelson-Aalen break-even.
//! 5. Reap unhealthy (scheduler-reported `dead_nodes`) and ICE-stuck
//!    claims.
//! 6. Persist `CellSketches` (DDSketch lead-time + idle-gap log) to PG.
//!
//! Lease-gated: only the leader replica runs `reconcile_once`. The
//! controller was historically single-replica, but the lease loop makes
//! rolling upgrades safe (the surge replica idles until the old one
//! releases) and allows `replicas: 2` for HA without double-provisioning.

mod consolidate;
mod cover;
mod ffd;
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

use crate::reconcilers::node_informer::HwClassConfig;
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
const TICK: Duration = Duration::from_secs(10);

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
pub struct PlaceableGate(Option<tokio::sync::watch::Receiver<PlaceableSet>>);

impl PlaceableGate {
    /// Disabled gate (`nodeclaim_pool.enabled = false`). [`Self::retain`]
    /// is a no-op; the legacy `ready` gate in `jobs.rs` applies instead.
    pub fn disabled() -> Self {
        Self(None)
    }

    /// `nodeclaim_pool.enabled`. Drives `schedulerName: kube-build-scheduler` +
    /// `priorityClassName` stamping in `build_job` AND switches `jobs.rs`
    /// from the §13a `ready` gate to the §13b placeable gate.
    pub fn enabled(&self) -> bool {
        self.0.is_some()
    }

    /// Retain only intents whose `intent_id` is in the last-published
    /// placeable set. Returns whether the gate is **armed** (a value has
    /// been published, OR the gate is disabled). `false` ⇔ enabled but
    /// no FFD tick has run yet — caller treats `queued` as unknown so
    /// `reap_excess_pending` stays fail-closed (a standby replica whose
    /// lease-gated reconciler never publishes would otherwise see
    /// `queued=0` and reap the leader's Pending Jobs).
    // r[impl ctrl.nodeclaim.placeable-gate]
    pub fn retain(&self, intents: &mut Vec<SpawnIntent>) -> bool {
        let Some(rx) = &self.0 else {
            return true;
        };
        match rx.borrow().clone() {
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

    /// Test-only: enabled gate seeded with `ids` (armed).
    #[cfg(test)]
    pub fn from_ids<I: IntoIterator<Item = &'static str>>(ids: I) -> Self {
        let set: HashSet<String> = ids.into_iter().map(str::to_owned).collect();
        let (_tx, rx) = tokio::sync::watch::channel(Some(Arc::new(set)));
        Self(Some(rx))
    }

    /// Test-only: enabled but unarmed (no publish yet).
    #[cfg(test)]
    pub fn unarmed() -> Self {
        let (_tx, rx) = tokio::sync::watch::channel(None);
        Self(Some(rx))
    }
}

/// Construct a placeable-gate channel pair. The sender is held by
/// [`NodeClaimPoolReconciler`]; the receiver wraps into [`PlaceableGate`]
/// in `Ctx`. Initial value `None` (unarmed) so the first Pool-reconcile
/// tick before the first FFD tick is fail-closed.
pub fn placeable_channel() -> (tokio::sync::watch::Sender<PlaceableSet>, PlaceableGate) {
    let (tx, rx) = tokio::sync::watch::channel(None);
    (tx, PlaceableGate(Some(rx)))
}

/// Figment-loaded config. Scalars via `RIO_NODECLAIM_POOL__*` env;
/// `instance_menu` / `lead_time_seed` via the `[nodeclaim_pool]` table
/// in `/etc/rio/controller.toml` (helm `rio-controller-config`
/// ConfigMap) — figment's Env provider yields bare strings, so nested
/// map/seq fields cannot load from env. `enabled = false` → reconciler
/// not spawned (legacy 12-NodePool mode; gate in main.rs).
#[derive(Debug, Clone, Serialize, Deserialize)]
#[serde(default)]
pub struct NodeClaimPoolConfig {
    /// Master gate. Mirrors helm `karpenter.nodeclaimPool.enabled`.
    pub enabled: bool,
    /// PostgreSQL URL for [`CellSketches`] persist/load. Same DB as
    /// store/scheduler (migration 059 lives there). Required when
    /// `enabled` — controller doesn't otherwise hold a PG handle.
    pub database_url: String,
    /// Lease object name for leader election. `None` → non-K8s mode
    /// (always-leader, see [`rio_lease::LeaseConfig::from_parts`]).
    pub lease_name: Option<String>,
    /// Lease namespace. `None` → in-cluster service-account mount.
    pub lease_namespace: Option<String>,
    /// `EC2NodeClass` name stamped on every created NodeClaim's
    /// `spec.nodeClassRef.name`. Helm: `sla.nodeClassRef`.
    pub node_class_ref: String,
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
    /// DDSketch active→shadow rotation interval (seconds). After
    /// `2×halflife` a sample has aged out entirely. Helm: not surfaced;
    /// 6h default per ADR §13b.
    pub sketch_halflife_secs: u64,
    /// Per-NodeClaim `resources.requests.cpu` ceiling. `cover_deficit`
    /// chunks a cell's `Σcores` deficit into ⌈Σ/this⌉ claims. Helm:
    /// `sla.maxCores` (the same ceiling the scheduler caps individual
    /// intents at, so a single intent always fits one claim).
    pub max_node_cores: u32,
    /// `karpenter.k8s.aws/instance-size NotIn` values appended to
    /// every NodeClaim's `spec.requirements` — the metal partition
    /// (I-205). Helm: `karpenter.metalSizes`. Empty (kwok/vmtest) → no
    /// instance-size requirement emitted.
    pub metal_sizes: Vec<String>,
}

impl NodeClaimPoolConfig {
    /// `lead_time_seed[cell]` (seconds). 0 for unconfigured cells —
    /// callers use this as a floor so 0 degenerates to "no floor".
    pub fn seed_for(&self, cell: &Cell) -> f64 {
        self.lead_time_seed
            .get(&cell.to_string())
            .copied()
            .unwrap_or(0.0)
    }

    /// Cold-start fallback cell for an hw-agnostic intent
    /// (`hw_class_names=[]`, i.e. `fit=None`): the
    /// `(referenceHwClass, Spot)` cell when its `kubernetes.io/arch`
    /// label matches `intent.system` (or is absent — arch-agnostic
    /// hw-class), else the first (sorted) hw-class whose arch matches.
    /// `None` ⇔ `system` unmappable OR no configured hw-class hosts
    /// that arch (caller emits
    /// `rio_controller_nodeclaim_intent_dropped_total
    /// {reason=no_menu_for_arch}`).
    ///
    /// Spot capacity-type only: cold-start probes are uniform
    /// `probe.cpu`-shaped and bounded by `max_node_claims_per_cell_per_
    /// tick`; on-demand fallback would defeat the §13b cost model.
    pub fn fallback_cell(&self, i: &SpawnIntent, hw: &HwClassConfig) -> Option<Cell> {
        let arch = ffd::system_to_arch(&i.system)?;
        if hw.matches_arch(&self.reference_hw_class, arch) {
            return Some(Cell(self.reference_hw_class.clone(), CapacityType::Spot));
        }
        hw.names()
            .into_iter()
            .find(|h| hw.matches_arch(h, arch))
            .map(|h| Cell(h, CapacityType::Spot))
    }

    /// All configured cells (`hw_classes × {spot, od}`), for
    /// round-robin iteration and per-cell gauges. Derived from the
    /// loaded [`HwClassConfig`] (not from `lead_time_seed` keys —
    /// those may be a subset).
    pub fn all_cells(&self, hw: &HwClassConfig) -> Vec<Cell> {
        hw.names()
            .into_iter()
            .flat_map(|h| {
                [CapacityType::Spot, CapacityType::OnDemand].map(move |c| Cell(h.clone(), c))
            })
            .collect()
    }
}

impl Default for NodeClaimPoolConfig {
    fn default() -> Self {
        Self {
            enabled: false,
            database_url: String::new(),
            lease_name: None,
            lease_namespace: None,
            // Matches helm `sla.nodeClassRef` default.
            node_class_ref: "rio-default".into(),
            reference_hw_class: String::new(),
            // Matches helm `sla.maxFleetCores` / `maxNodeClaimsPerCellPerTick`
            // / `maxLeadTime` defaults. Validated in main.rs only when
            // `enabled` so unit tests don't need to populate these.
            max_fleet_cores: 10_000,
            max_node_claims_per_cell_per_tick: 8,
            max_lead_time: 600.0,
            max_consolidation_time: None,
            lead_time_seed: HashMap::new(),
            sketch_halflife_secs: 6 * 3600,
            // Matches helm `sla.maxCores` default (64).
            max_node_cores: 64,
            metal_sizes: Vec::new(),
        }
    }
}

/// Per-component lease hooks. The reconciler has no actor channel — the
/// tick loop polls [`LeaderState::is_leader`] directly — so the hooks
/// only emit `rio_controller_lease_*_total` per the
/// `rio_{component}_` naming rule (see [`rio_lease::LeaseHooks`] doc).
#[derive(Clone)]
pub struct ControllerLeaseHooks;

impl rio_lease::LeaseHooks for ControllerLeaseHooks {
    fn on_acquire(&self) {
        metrics::counter!("rio_controller_lease_acquired_total").increment(1);
    }
    fn on_lose(&self) {
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
    /// Publish side of [`PlaceableGate`]. Written once per successful
    /// FFD tick with the `intent_id`s placed on `Registered=True`
    /// nodes; the `pool/jobs` reconciler reads it via `Ctx.placeable`.
    placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    sketches: CellSketches,
    /// NodeClaim names whose `Registered=True` boot time has already
    /// been recorded into `sketches`. Edge-detector state for
    /// [`CellSketches::observe_registered`]; pruned to live names each
    /// tick. In-memory only — a restart re-records the live set's boot
    /// times once (a few duplicate samples in a DDSketch is harmless).
    recorded_boot: HashSet<String>,
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
    pub async fn new(
        kube: kube::Client,
        admin: AdminClient,
        pg: sqlx::PgPool,
        leader: LeaderState,
        cfg: NodeClaimPoolConfig,
        hw_config: HwClassConfig,
        placeable_tx: tokio::sync::watch::Sender<PlaceableSet>,
    ) -> Self {
        // Load persisted sketches; fall back to empty on error (a fresh
        // table is the cold-start case anyway). `seed()` overlays
        // `cfg.lead_time_seed` on top of any cells the load didn't
        // populate.
        let mut sketches = match CellSketches::load(&pg).await {
            Ok(s) => {
                info!(cells = s.len(), "loaded nodeclaim_cell_state from PG");
                s
            }
            Err(e) => {
                warn!(error = %e, "nodeclaim_cell_state load failed; starting empty");
                CellSketches::default()
            }
        };
        sketches.seed(&cfg.lead_time_seed);
        Self {
            nodeclaims: Api::all(kube),
            admin,
            pg,
            leader,
            cfg,
            hw_config,
            placeable_tx,
            sketches,
            recorded_boot: HashSet::new(),
            inflight_created: HashMap::new(),
            consecutive_bot_ticks: 0,
            tick_counter: 0,
        }
    }

    /// Tick loop. Gated on [`LeaderState::is_leader`] — standby replicas
    /// (and the surge pod during a rolling upgrade) burn ticks as no-ops
    /// until they acquire. Stateful (`consecutive_bot_ticks`,
    /// `tick_counter`, `sketches`): not `spawn_periodic`. `biased;`
    /// inlined per `r[common.task.periodic-biased]`.
    pub async fn run(mut self, shutdown: rio_common::signal::Token) {
        info!(
            node_class_ref = %self.cfg.node_class_ref,
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
            if !self.leader.is_leader() {
                debug!("standby; skipping nodeclaim_pool tick");
                continue;
            }
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
        let intents: Option<GetSpawnIntentsResponse> = match admin_call(
            self.admin
                .clone()
                .get_spawn_intents(GetSpawnIntentsRequest::default()),
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

        // r[ctrl.nodeclaim.lead-time-ddsketch]: record boot times on
        // Registered=True edges, then rotate any cells past halflife.
        // `registered_cells` feeds `report_unfulfillable`'s ICE-clear.
        // TODO: wire `CellState::schmitt_adjust` once a true forecast
        // hit-ratio is computable — needs cross-tick tracking of
        // "forecast intent placed on in-flight claim → did claim
        // register before intent's deps completed". The
        // `ffd_placeable_intents{state}` gauge below exposes the
        // single-tick proxy (registered/(registered+inflight)) so the
        // operator can observe the input now.
        let registered_cells = self
            .sketches
            .observe_registered(&live, &mut self.recorded_boot);
        self.sketches.maybe_rotate_all(
            std::time::SystemTime::now(),
            Duration::from_secs(self.cfg.sketch_halflife_secs),
        );

        let (placeable, unplaced) =
            ffd::simulate(&intents.intents, &live, &self.sketches, |h, a| {
                self.hw_config.matches_arch(h, a)
            });
        debug!(
            placeable = placeable.len(),
            unplaced = unplaced.len(),
            live = live.len(),
            "FFD simulation"
        );
        self.emit_tick_gauges(&live, &placeable, &unplaced);
        // r[impl ctrl.nodeclaim.placeable-gate]
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

        let now = now_epoch();
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
        self.report_unfulfillable(&ice_cells, &registered_cells)
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

        self.sketches.persist(&self.pg).await?;
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
        self.sketches.persist(&self.pg).await?;
        Ok(())
    }

    /// Per-tick `r[obs.metric.controller]` gauges. Iterates
    /// `cfg.all_cells()` (NOT just cells observed in `live`/`unplaced`)
    /// so every (h,cap) timeseries is emitted every tick — Prometheus
    /// gauge semantics: a cell that drained to 0 reads as 0, not
    /// stale-at-last-nonzero.
    fn emit_tick_gauges(
        &self,
        live: &[ffd::LiveNode],
        placeable: &[ffd::Placement],
        unplaced: &[SpawnIntent],
    ) {
        use std::collections::BTreeMap;
        // (registered, inflight) per cell.
        let mut by_state: BTreeMap<Cell, (u64, u64)> = BTreeMap::new();
        for n in live {
            let Some(c) = n.cell.clone() else { continue };
            let e = by_state.entry(c).or_default();
            if n.registered {
                e.0 += 1;
            } else {
                e.1 += 1;
            }
        }
        // Σ unplaced cores per cheapest-A_open cell — same assignment
        // cover_deficit uses, so the gauge equals cover's per-cell input.
        // No mask: the gauge shows raw demand; ICE-masking is a cover
        // policy, not a demand metric.
        let (by_cell, _) = cover::assign_to_cells(
            unplaced,
            &self.sketches,
            &HashSet::new(),
            cover::cell_rank,
            |i| self.cfg.fallback_cell(i, &self.hw_config),
        );
        for cell in self.cfg.all_cells(&self.hw_config) {
            let label = cell.to_string();
            let (reg, inf) = by_state.get(&cell).copied().unwrap_or((0, 0));
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "registered")
            .set(reg as f64);
            metrics::gauge!("rio_controller_nodeclaim_live",
                "cell" => label.clone(), "state" => "inflight")
            .set(inf as f64);
            let unplaced_cores: u32 = by_cell
                .get(&cell)
                .map(|v| v.iter().map(|i| i.cores).sum())
                .unwrap_or(0);
            metrics::gauge!("rio_controller_ffd_unplaced_cores", "cell" => label.clone())
                .set(f64::from(unplaced_cores));
            metrics::gauge!("rio_controller_nodeclaim_lead_time_seconds", "cell" => label)
                .set(self.sketches.lead_time(&cell));
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
        Ok(list.items.into_iter().map(ffd::LiveNode::from).collect())
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
    /// 3. Per cell with deficit: sum `(Σc, Σm, max d)` over the cell's
    ///    intents; mint `N = min(⌈Σc/max_node_cores⌉, per_tick_cap,
    ///    budget_fit)` NodeClaims, each requesting `(chunk, Σm/N,
    ///    max d)`. Karpenter resolves each against the hw-class's
    ///    `requirements` to pick the instance type.
    /// 4. `budget = max_fleet_cores − Σ live.allocatable.cpu −
    ///    created_this_tick`. The sum covers both Registered AND
    ///    in-flight claims so a slow-to-register burst doesn't
    ///    double-provision next tick.
    ///
    /// `Api::create` failures are warned + skipped (next tick retries);
    /// the method only propagates errors that would make the tick
    /// non-progressing.
    // r[impl ctrl.nodeclaim.anchor-bulk]
    async fn cover_deficit(
        &self,
        unplaced: &[SpawnIntent],
        live: &[ffd::LiveNode],
        ice_masked: &[String],
    ) -> anyhow::Result<CoverResult> {
        if unplaced.is_empty() {
            return Ok(CoverResult::default());
        }
        let ice: HashSet<Cell> = ice_masked.iter().filter_map(|s| Cell::parse(s)).collect();
        let live_cores: u32 = live.iter().map(|n| n.allocatable.0).sum();
        let mut created_cores = 0u32;

        let (by_cell, dropped) =
            cover::assign_to_cells(unplaced, &self.sketches, &ice, cover::cell_rank, |i| {
                self.cfg.fallback_cell(i, &self.hw_config)
            });
        if dropped > 0 {
            metrics::counter!(
                "rio_controller_nodeclaim_intent_dropped_total",
                "reason" => "no_menu_for_arch",
            )
            .increment(dropped);
        }
        let order =
            cover::cells_round_robin(self.cfg.all_cells(&self.hw_config), self.tick_counter);

        let mut created = Vec::new();
        for cell in &order {
            if ice.contains(cell) {
                continue;
            }
            let Some(u) = by_cell.get(cell) else {
                continue;
            };
            let (sum_c, sum_m, max_d) = cover::sum_deficit(u);
            let budget = self
                .cfg
                .max_fleet_cores
                .saturating_sub(live_cores)
                .saturating_sub(created_cores);
            let (n, chunk) = cover::claim_count(
                sum_c,
                self.cfg.max_node_cores,
                self.cfg.max_node_claims_per_cell_per_tick,
                budget,
            );
            if n == 0 {
                debug!(%cell, budget, sum_c, "fleet-core budget exhausted");
                continue;
            }
            let Some(hw_labels) = self.hw_config.labels_for(&cell.0) else {
                warn!(hw_class = %cell.0, "no hw-class labels (GetHwClassConfig not loaded?); skipping");
                continue;
            };
            let hw_reqs = self.hw_config.requirements_for(&cell.0).unwrap_or_default();
            // Per-claim mem: Σm spread evenly. max_d as-is (each pod's
            // disk allocates independently, so the claim need only fit
            // the largest).
            let mem_per = sum_m / u64::from(n);
            for _ in 0..n {
                let nc = cover::build_nodeclaim(
                    cell,
                    (chunk, mem_per, max_d),
                    &hw_labels,
                    &hw_reqs,
                    &self.cfg.metal_sizes,
                    &self.cfg.node_class_ref,
                );
                match self.nodeclaims.create(&PostParams::default(), &nc).await {
                    Ok(out) => {
                        let name = out.metadata.name.unwrap_or_default();
                        debug!(%cell, %name, cores = chunk, "NodeClaim created");
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
            }
            created_cores += n * chunk;
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
    ) -> anyhow::Result<()> {
        if ice_cells.is_empty() && registered_cells.is_empty() {
            return Ok(());
        }
        let req = AckSpawnedIntentsRequest {
            spawned: vec![],
            unfulfillable_cells: ice_cells.iter().map(Cell::to_string).collect(),
            registered_cells: registered_cells.iter().map(Cell::to_string).collect(),
        };
        if let Err(e) = admin_call(self.admin.clone().ack_spawned_intents(req)).await {
            warn!(error = %e, "ack_spawned_intents (unfulfillable/registered) failed");
        }
        Ok(())
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
/// reaches `CellSketches::load` (controller's `connect_forever` to the
/// scheduler in main.rs already orders that). Max 4 connections: persist
/// is one upsert per cell per 10s tick.
pub async fn connect_pg(
    database_url: &str,
    shutdown: &rio_common::signal::Token,
) -> Option<sqlx::PgPool> {
    rio_proto::client::connect_forever(shutdown, async || {
        sqlx::postgres::PgPoolOptions::new()
            .max_connections(4)
            .min_connections(1)
            .idle_timeout(Duration::from_secs(60))
            .connect(database_url)
            .await
    })
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn config_default_disabled() {
        let d = NodeClaimPoolConfig::default();
        assert!(!d.enabled, "enabled=false → reconciler not spawned");
        assert!(d.database_url.is_empty());
        assert!(d.lease_name.is_none());
        assert_eq!(d.node_class_ref, "rio-default");
        assert!(d.reference_hw_class.is_empty());
        assert_eq!(d.max_fleet_cores, 10_000);
        assert_eq!(d.max_node_claims_per_cell_per_tick, 8);
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
        // x86 → reference cell (arch matches).
        assert_eq!(
            cfg.fallback_cell(&i("x86_64-linux"), &hw),
            Some(Cell("mid-ebs-x86".into(), CapacityType::Spot))
        );
        // aarch64 → reference is amd64; first sorted arm cell wins.
        assert_eq!(
            cfg.fallback_cell(&i("aarch64-linux"), &hw),
            Some(Cell("hi-ebs-arm".into(), CapacityType::Spot))
        );
        // Unmappable system → None.
        assert_eq!(cfg.fallback_cell(&i("builtin"), &hw), None);
        assert_eq!(cfg.fallback_cell(&i(""), &hw), None);
        // No matching-arch hw-class loaded → None.
        let hw2 = HwClassConfig::from_literals(&[("mid-ebs-x86", &[(ARCH_LABEL, "amd64")])]);
        assert_eq!(cfg.fallback_cell(&i("aarch64-linux"), &hw2), None);
        // Arch-agnostic reference (no arch label) matches any system —
        // the kwok `vmtest` fixture case.
        let cfg3 = NodeClaimPoolConfig {
            reference_hw_class: "vmtest".into(),
            ..Default::default()
        };
        let hw3 =
            HwClassConfig::from_literals(&[("vmtest", &[("kubernetes.io/hostname", "agent")])]);
        assert_eq!(
            cfg3.fallback_cell(&i("x86_64-linux"), &hw3),
            Some(Cell("vmtest".into(), CapacityType::Spot))
        );
    }

    /// `ControllerLeaseHooks` is the no-state metrics-only impl. Prove
    /// it's `Clone` (LeaseHooks bound) and the calls don't panic — the
    /// metrics emission is asserted in the VM lease test, not here.
    #[test]
    fn lease_hooks_clone_and_fire() {
        let h = ControllerLeaseHooks;
        let h2 = h.clone();
        rio_lease::LeaseHooks::on_acquire(&h2);
        rio_lease::LeaseHooks::on_lose(&h2);
    }

    #[test]
    fn cover_result_default_empty() {
        let r = CoverResult::default();
        assert!(r.created.is_empty());
    }

    /// `all_cells` = hw-class names × {spot, od}.
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
}
