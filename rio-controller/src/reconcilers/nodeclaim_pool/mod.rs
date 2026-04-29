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
pub use cover::{InstanceType, NODEPOOL_LABEL, SHIM_NODEPOOL};
pub use ffd::{CAPACITY_TYPE_LABEL, HW_CLASS_LABEL, LiveNode, Placement, a_open, cells_of};
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

/// Figment-loaded config (`RIO_NODECLAIM_POOL__*`). `enabled = false` →
/// reconciler not spawned (legacy 12-NodePool mode; gate in main.rs).
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
    /// `(hw_class:cap)` → instance-shape menu for §13b anchor+bulk
    /// selection. Helm: `sla.instanceMenu`. The controller does NOT
    /// run the scheduler's live spot-price poll; `price_per_vcpu_hr`
    /// is the static seed (relative ranking only). Validated when
    /// `enabled` (every cell's max menu entry covers
    /// `(maxCores,maxMem,maxDisk)` so the
    /// `placement_sim_mismatch_total{reason=menu_gap}` path is
    /// prod-unreachable).
    pub instance_menu: HashMap<String, Vec<InstanceType>>,
}

impl NodeClaimPoolConfig {
    /// Menu for `cell` (empty if unconfigured — caller emits the
    /// `menu_gap` metric).
    pub fn menu(&self, cell: &Cell) -> &[InstanceType] {
        self.instance_menu
            .get(&cell.to_string())
            .map(Vec::as_slice)
            .unwrap_or(&[])
    }

    /// Min `price_per_vcpu_hr` across `cell`'s menu — what
    /// `cover::assign_to_cells` ranks cheapest-open by. `f64::MAX`
    /// for an unconfigured cell so it's never picked over a configured
    /// one.
    pub fn cell_price(&self, cell: &Cell) -> f64 {
        self.menu(cell)
            .iter()
            .map(|t| t.price_per_vcpu_hr)
            .min_by(f64::total_cmp)
            .unwrap_or(f64::MAX)
    }

    /// `lead_time_seed[cell]` (seconds). 0 for unconfigured cells —
    /// callers use this as a floor so 0 degenerates to "no floor".
    pub fn seed_for(&self, cell: &Cell) -> f64 {
        self.lead_time_seed
            .get(&cell.to_string())
            .copied()
            .unwrap_or(0.0)
    }

    /// All configured cells (parsed from `instance_menu` keys), for
    /// round-robin iteration. Malformed keys are skipped + warned.
    pub fn all_cells(&self) -> Vec<Cell> {
        self.instance_menu
            .keys()
            .filter_map(|k| {
                let c = Cell::parse(k);
                if c.is_none() {
                    warn!(key = %k, "instance_menu key not parseable as h:cap; skipping");
                }
                c
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
            // Matches helm `sla.maxFleetCores` / `maxNodeClaimsPerCellPerTick`
            // / `maxLeadTime` defaults. Validated in main.rs only when
            // `enabled` so unit tests don't need to populate these.
            max_fleet_cores: 10_000,
            max_node_claims_per_cell_per_tick: 8,
            max_lead_time: 600.0,
            max_consolidation_time: None,
            lead_time_seed: HashMap::new(),
            sketch_halflife_secs: 6 * 3600,
            instance_menu: HashMap::new(),
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
    sketches: CellSketches,
    /// NodeClaim names whose `Registered=True` boot time has already
    /// been recorded into `sketches`. Edge-detector state for
    /// [`CellSketches::observe_registered`]; pruned to live names each
    /// tick. In-memory only — a restart re-records the live set's boot
    /// times once (a few duplicate samples in a DDSketch is harmless).
    recorded_boot: HashSet<String>,
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
            sketches,
            recorded_boot: HashSet::new(),
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
            if let Err(e) = self.reconcile_once().await {
                warn!(error = %e, "nodeclaim_pool tick failed");
            }
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
        // TODO(B12): per-cell `schmitt_adjust` once forecast hit-ratio
        // is observable (needs rio-packed pod placement to compare
        // against FFD's prediction).
        let registered_cells = self
            .sketches
            .observe_registered(&live, &mut self.recorded_boot);
        self.sketches.maybe_rotate_all(
            std::time::SystemTime::now(),
            Duration::from_secs(self.cfg.sketch_halflife_secs),
        );

        let (placeable, unplaced) = ffd::simulate(&intents.intents, &live, &self.sketches);
        // TODO(B12): pod.rs `schedulerName=rio-packed` + priorityClassName
        // wiring; until then `placeable` is observability only — the legacy
        // Pool reconciler still spawns the actual Jobs.
        debug!(
            placeable = placeable.len(),
            unplaced = unplaced.len(),
            live = live.len(),
            "FFD simulation"
        );

        let now = now_epoch();
        // Reap unhealthy/ICE BEFORE cover_deficit so cells that just
        // hit ICE this tick are masked in the same tick's cover (don't
        // immediately re-create what we just deleted).
        let ice_cells = health::reap_unhealthy(
            &self.nodeclaims,
            &live,
            &intents.dead_nodes,
            &self.sketches,
            &self.cfg,
            now,
        )
        .await?;
        let mut masked: Vec<String> = intents.ice_masked_cells.clone();
        masked.extend(ice_cells.iter().map(Cell::to_string));

        let cover = self.cover_deficit(&unplaced, &live, &masked).await?;
        debug!(created = cover.created.len(), "deficit cover");
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

    /// §13b anchor+bulk deficit cover.
    ///
    /// 1. Group `unplaced` by cheapest cell in each intent's `A_open`
    ///    (`cover::assign_to_cells`). hw-agnostic intents (empty
    ///    `A_open`) drop out — the legacy `Pool` reconciler covers
    ///    those.
    /// 2. Round-robin `cfg.all_cells()` from `tick_counter` so no cell
    ///    starves under sustained pressure.
    /// 3. Per cell with deficit (∉ `ice_masked`): pick anchor =
    ///    smallest fitting `max_U(c*,M,D)`, bulk = cheapest meeting
    ///    `median_U(M/c*)`; mint `N = min(need, per_tick_cap,
    ///    budget_fit)` NodeClaims (1×anchor + (N−1)×bulk).
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

        let by_cell = cover::assign_to_cells(unplaced, &self.sketches, |c| self.cfg.cell_price(c));
        let order = cover::cells_round_robin(self.cfg.all_cells(), self.tick_counter);

        let mut created = Vec::new();
        for cell in &order {
            if ice.contains(cell) {
                continue;
            }
            let Some(u) = by_cell.get(cell) else {
                continue;
            };
            let menu = self.cfg.menu(cell);
            let (max_c, max_m, max_d) = u.iter().fold((0, 0, 0), |(c, m, d), i| {
                (c.max(i.cores), m.max(i.mem_bytes), d.max(i.disk_bytes))
            });
            let Some(anchor) = cover::pick_anchor(menu, max_c, max_m, max_d) else {
                metrics::counter!(
                    "rio_controller_placement_sim_mismatch_total",
                    "reason" => "menu_gap",
                    "cell" => cell.to_string(),
                )
                .increment(1);
                continue;
            };
            let med_ratio = cover::median(
                u.iter()
                    .filter(|i| i.cores > 0)
                    .map(|i| i.mem_bytes as f64 / f64::from(i.cores))
                    .collect(),
            );
            let bulk = cover::pick_bulk(menu, med_ratio, anchor);
            let sum_c: u32 = u.iter().map(|i| i.cores).sum();
            let budget = self
                .cfg
                .max_fleet_cores
                .saturating_sub(live_cores)
                .saturating_sub(created_cores);
            let n = cover::claim_count(
                sum_c,
                anchor.cores,
                bulk.cores,
                self.cfg.max_node_claims_per_cell_per_tick,
                budget,
            );
            if n == 0 {
                debug!(%cell, budget, anchor = anchor.cores, "fleet-core budget exhausted");
                continue;
            }
            let Some(hw_labels) = self.hw_config.labels_for(&cell.0) else {
                warn!(hw_class = %cell.0, "no hw-class labels (GetHwClassConfig not loaded?); skipping");
                continue;
            };
            for k in 0..n {
                let shape = if k == 0 { anchor } else { bulk };
                let nc = cover::build_nodeclaim(cell, shape, &hw_labels, &self.cfg.node_class_ref);
                match self.nodeclaims.create(&PostParams::default(), &nc).await {
                    Ok(out) => {
                        let name = out.metadata.name.unwrap_or_default();
                        debug!(%cell, %name, cores = shape.cores, "NodeClaim created");
                        created.push(name);
                    }
                    Err(e) => {
                        warn!(%cell, error = %e, "NodeClaim create failed; skipping");
                    }
                }
            }
            created_cores += anchor.cores + (n - 1) * bulk.cores;
        }
        Ok(CoverResult { created })
    }

    /// Report this tick's ICE-hit cells (`unfulfillable_cells`) and
    /// `Registered=True` edges (`registered_cells`) to the scheduler
    /// via `AckSpawnedIntents`. The scheduler's ICE backoff ladder
    /// marks/clears each. `spawned` is empty: the legacy `Pool`
    /// reconciler still owns Job-creation acks until B12 routes pods
    /// here. RPC failure is warned + dropped (next tick retries; the
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
    /// NodeClaim names created this tick.
    pub created: Vec<String>,
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
        assert_eq!(d.max_fleet_cores, 10_000);
        assert_eq!(d.max_node_claims_per_cell_per_tick, 8);
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

    type MenuLit = (u32, u64, u64, f64);
    fn cfg_with_menu(menus: &[(&str, &[MenuLit])]) -> NodeClaimPoolConfig {
        let instance_menu = menus
            .iter()
            .map(|(k, m)| {
                let v = m
                    .iter()
                    .map(|&(cores, mem, disk, p)| InstanceType {
                        cores,
                        mem_bytes: mem,
                        disk_bytes: disk,
                        price_per_vcpu_hr: p,
                    })
                    .collect();
                ((*k).to_string(), v)
            })
            .collect();
        NodeClaimPoolConfig {
            instance_menu,
            ..Default::default()
        }
    }

    #[test]
    fn config_menu_and_cell_price() {
        const GI: u64 = 1 << 30;
        let cfg = cfg_with_menu(&[
            (
                "h1:spot",
                &[(4, 16 * GI, 100 * GI, 0.04), (8, 32 * GI, 100 * GI, 0.03)],
            ),
            ("h2:od", &[(16, 64 * GI, 200 * GI, 0.10)]),
        ]);
        let h1 = Cell("h1".into(), CapacityType::Spot);
        let h2 = Cell("h2".into(), CapacityType::OnDemand);
        assert_eq!(cfg.menu(&h1).len(), 2);
        assert_eq!(cfg.menu(&h2).len(), 1);
        assert!(
            cfg.menu(&Cell("nope".into(), CapacityType::Spot))
                .is_empty()
        );
        // cell_price = min over menu.
        assert!((cfg.cell_price(&h1) - 0.03).abs() < 1e-12);
        assert!((cfg.cell_price(&h2) - 0.10).abs() < 1e-12);
        // Unconfigured cell → MAX so it's never cheapest.
        assert_eq!(
            cfg.cell_price(&Cell("x".into(), CapacityType::Spot)),
            f64::MAX
        );
        // all_cells parses keys; sort for stable assert.
        let mut cells = cfg.all_cells();
        cells.sort();
        assert_eq!(cells, vec![h1, h2]);
    }

    #[test]
    fn config_all_cells_skips_malformed() {
        let cfg = cfg_with_menu(&[("good:spot", &[(4, 1, 1, 0.04)]), ("bad-key", &[])]);
        assert_eq!(cfg.all_cells().len(), 1);
    }
}
