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
mod ffd;
mod health;
pub mod sketch;

use std::collections::HashMap;
use std::time::Duration;

use kube::api::{Api, ListParams};
use serde::{Deserialize, Serialize};
use tracing::{debug, info, instrument, warn};

use rio_crds::karpenter::NodeClaim;
use rio_lease::LeaderState;
use rio_proto::types::{GetSpawnIntentsRequest, GetSpawnIntentsResponse};

use crate::reconcilers::{AdminClient, admin_call};

pub use consolidate::IdleGapEvent;
pub use ffd::{LiveNode, Placement};
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
    sketches: CellSketches,
    /// Count of consecutive ticks where `GetSpawnIntents` returned ⊥
    /// (RPC error). Saturates at `u8::MAX`; reset on first success.
    consecutive_bot_ticks: u8,
    /// Monotonic tick counter for `cover_deficit`'s rotating-start
    /// round-robin (B8).
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
    ) -> Self {
        // Load persisted sketches; fall back to empty on error (a fresh
        // table is the cold-start case anyway). B9's `seed()` will
        // overlay `cfg.lead_time_seed` on top of any cells the load
        // didn't populate.
        let sketches = match CellSketches::load(&pg).await {
            Ok(s) => {
                info!(cells = s.len(), "loaded nodeclaim_cell_state from PG");
                s
            }
            Err(e) => {
                warn!(error = %e, "nodeclaim_cell_state load failed; starting empty");
                CellSketches::default()
            }
        };
        Self {
            nodeclaims: Api::all(kube),
            admin,
            pg,
            leader,
            cfg,
            sketches,
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

        let cover = self
            .cover_deficit(&unplaced, &live, &intents.ice_masked_cells)
            .await?;
        debug!(created = cover.created.len(), "deficit cover");
        self.report_unfulfillable(&cover.ice_cells).await?;

        consolidate::reap_idle(
            &self.nodeclaims,
            &live,
            &placeable,
            &self.sketches,
            &self.cfg,
        )
        .await?;
        health::reap_unhealthy(
            &self.nodeclaims,
            &live,
            &intents.dead_nodes,
            &mut self.sketches,
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
        consolidate::reap_idle(&self.nodeclaims, &live, &[], &self.sketches, &self.cfg).await?;
        // No `dead_nodes` signal without the scheduler; B11's local
        // ICE-timeout detection still runs on `live`.
        health::reap_unhealthy(&self.nodeclaims, &live, &[], &mut self.sketches).await?;
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
    // TODO(B8): full implementation. Skeleton returns empty so the
    // persist/consolidate path exercises end-to-end without creating
    // NodeClaims until B8+B12 land together.
    async fn cover_deficit(
        &self,
        unplaced: &[rio_proto::types::SpawnIntent],
        live: &[ffd::LiveNode],
        ice_masked: &[String],
    ) -> anyhow::Result<CoverResult> {
        let _ = (unplaced, live, ice_masked, &self.cfg);
        Ok(CoverResult::default())
    }

    /// Report cells where NodeClaim creation hit ICE (Launched=False or
    /// Registered timeout) back to the scheduler via
    /// `AckSpawnedIntents.unfulfillable_cells`.
    // TODO(B11): wire `AckSpawnedIntentsRequest`. No-op until B8
    // populates `ice_cells`.
    async fn report_unfulfillable(&self, ice_cells: &[Cell]) -> anyhow::Result<()> {
        let _ = ice_cells;
        Ok(())
    }
}

/// Result of one [`NodeClaimPoolReconciler::cover_deficit`] tick.
#[derive(Debug, Default)]
pub(crate) struct CoverResult {
    /// NodeClaim names created this tick.
    pub created: Vec<String>,
    /// Cells where create hit ICE this tick (fed to
    /// `report_unfulfillable`).
    pub ice_cells: Vec<Cell>,
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
        assert!(r.ice_cells.is_empty());
    }
}
