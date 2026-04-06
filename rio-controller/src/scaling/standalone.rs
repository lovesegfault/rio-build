//! Cluster-wide autoscaler loop: poll `ClusterStatus`, patch
//! StatefulSet replicas for STANDALONE WorkerPools.
//!
//! Runs separately from the reconciler (spawned in main.rs as its
//! own task). The reconciler ensures the StatefulSet EXISTS with
//! the right shape; the autoscaler adjusts `spec.replicas` within
// r[impl ctrl.autoscale.direct-patch]
// r[impl ctrl.autoscale.separate-field-manager]
// r[impl ctrl.autoscale.skip-deleting]
//! `[min, max]` based on queue depth.
//!
//! # Stabilization windows
//!
//! Scale-up: fast (30s). Queue is deep → builds waiting → get
//! workers online. The cost of over-scaling briefly is idle pods
//! (cheap); the cost of under-scaling is stalled builds (expensive
//! developer time).
//!
//! Scale-down: slow (10 min). The cost of scaling down too fast is
//! killing a worker right before the next burst (pod scheduling +
//! FUSE warm = minutes). The cost of over-scaling is idle pods —
//! acceptable for 10 minutes.
//!
//! Anti-flap: minimum 30s between any two patches. Prevents
//! oscillation when the desired value wobbles around a boundary.
//!
//! # Separate from the reconciler — why
//!
//! The reconciler is event-driven (CR change, StatefulSet change).
//! Autoscaling is POLL-driven (ClusterStatus every 30s). Wedging a
//! poll loop into the reconciler via requeue(30s) works but
//! conflates "spec changed, re-apply" with "time passed, re-check
//! queue." Separate task = separate concerns. The autoscaler
//! patches `spec.replicas` directly; the reconciler's `.owns()`
//! watches that StatefulSet and re-reconciles to update status.

use std::collections::HashMap;
use std::time::Instant;

use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::{Api, Patch, PatchParams};
use kube::{Client, Resource, ResourceExt};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use rio_proto::AdminServiceClient;
use rio_proto::types::{GetSizeClassStatusRequest, GetSizeClassStatusResponse};

use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::WorkerPoolSet;

use super::{
    Decision, Direction, STATUS_MANAGER, ScaleError, ScaleState, ScalingTiming,
    check_stabilization, compute_desired, is_wps_owned, pool_key, scaling_condition,
    sts_replicas_patch, wp_status_patch,
};

/// Autoscaler. Constructed once in main.rs, `run()` loops forever.
///
/// Fields `pub(super)` so the per-class submodule (`per_class.rs`)
/// can add its own `impl Autoscaler` block for `scale_wps_class`
/// — same split-impl pattern P0356 used for `SchedulerGrpc`.
pub struct Autoscaler {
    pub(super) client: Client,
    pub(super) scheduler: AdminServiceClient<Channel>,
    /// Per-pool state, keyed by `namespace/name`. Persists across
    /// iterations. Pruned when a pool disappears from the list.
    pub(super) states: HashMap<String, ScaleState>,
    /// Timing knobs. `Copy` — passed by value into
    /// check_stabilization (4 Durations = 32 bytes).
    pub(super) timing: ScalingTiming,
    /// K8s Events recorder. Emits ScaledUp/ScaledDown events on
    /// successful scale patches so operators see the autoscaler's
    /// decisions in `kubectl describe workerpool`.
    pub(super) recorder: kube::runtime::events::Recorder,
}

impl Autoscaler {
    pub fn new(
        client: Client,
        scheduler: AdminServiceClient<Channel>,
        timing: ScalingTiming,
        recorder: kube::runtime::events::Recorder,
    ) -> Self {
        Self {
            client,
            scheduler,
            states: HashMap::new(),
            timing,
            recorder,
        }
    }

    /// Main loop. main.rs spawns this via `spawn_monitored`.
    /// Returns on cancellation (SIGTERM/SIGINT) or panic (logged
    /// by spawn_monitored; controller keeps reconciling without
    /// autoscale).
    ///
    /// Stateful: `self.states: HashMap` is cross-tick mutable, so
    /// not spawn_periodic (FnMut can't lend &mut self across
    /// .await). biased; inlined per r[common.task.periodic-biased].
    pub async fn run(mut self, shutdown: rio_common::signal::Token) {
        let mut interval = tokio::time::interval(self.timing.poll_interval);
        // MissedTickBehavior::Skip: if one iteration takes >30s
        // (slow apiserver), don't fire twice immediately after.
        // Catch up on the NEXT normal tick.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            tokio::select! {
                biased;
                _ = shutdown.cancelled() => return,
                _ = interval.tick() => {}
            }
            if let Err(e) = self.tick().await {
                // Error on one tick doesn't kill the loop. The
                // scheduler might be restarting, apiserver busy,
                // etc. Next tick tries again.
                warn!(error = %e, "autoscale tick failed; will retry");
            }
        }
    }

    /// One iteration: poll, compute, maybe patch.
    async fn tick(&mut self) -> anyhow::Result<()> {
        // ---- Read queue depth from scheduler ----
        // ClusterStatus: cluster-wide `queued_derivations` — the
        // scale signal for standalone WorkerPools (those not owned
        // by a WPS).
        //
        // GetSizeClassStatus: per-class `queued` — the scale
        // signal for WPS child pools. Best-effort: if the RPC
        // fails, WPS children fall back to cluster-wide depth
        // (worse signal but not wrong — still scales up under
        // load). The RPC can fail independently of ClusterStatus
        // if size-class routing is unconfigured.
        let status = self
            .scheduler
            .cluster_status(())
            .await
            .map(|r| r.into_inner())?;

        let sc_resp = self
            .scheduler
            .get_size_class_status(GetSizeClassStatusRequest {})
            .await
            .map(|r| r.into_inner())
            .unwrap_or_else(|e| {
                debug!(error = %e, "GetSizeClassStatus unavailable; WPS children use cluster-wide queue depth");
                GetSizeClassStatusResponse::default()
            });

        // ---- List all WorkerPools + WorkerPoolSets ----
        let pools_api: Api<WorkerPool> = Api::all(self.client.clone());
        let pools = pools_api.list(&Default::default()).await?.items;

        let wps_api: Api<WorkerPoolSet> = Api::all(self.client.clone());
        // Best-effort: WPS CRD may not be installed on older
        // clusters. An empty list is fine — no per-class scaling.
        let wps_list = wps_api
            .list(&Default::default())
            .await
            .map(|l| l.items)
            .unwrap_or_else(|e| {
                debug!(error = %e, "WorkerPoolSet list failed (CRD not installed?); per-class scaling disabled");
                Vec::new()
            });

        // ---- Prune state for deleted pools ----
        // Pools gone from the list → remove their ScaleState.
        // Without this, the map grows unboundedly over pool
        // create/delete cycles (small leak, but still).
        let live_keys: std::collections::HashSet<String> = pools.iter().map(pool_key).collect();
        self.states.retain(|k, _| live_keys.contains(k));

        // ---- Compute + maybe patch each pool ----
        // scale_one() returns Some(err) for config errors (unknown
        // metric). Those surface via WorkerPool.status.conditions
        // — best-effort (log + continue on patch failure).
        //
        // Skip pools with deletionTimestamp: the finalizer's
        // cleanup() scales STS to 0 as part of drain; the
        // autoscaler rescaling to min would fight that (STS
        // ping-pongs 0↔min every 3s poll → pods never terminate
        // → finalizer never removes itself → kubectl delete hangs
        // forever). With a fast poll interval this is infinite;
        // with the default 30s it's a ~30s stall (finalizer usually
        // wins the race between polls), but still wrong.
        //
        // Skip WPS-owned pools: per-class scaling for those
        // happens in the WPS loop below (with per-class queue
        // depth from GetSizeClassStatus). Scaling them here with
        // cluster-wide depth would fight the per-class scaler
        // (two SSA field managers patching the same STS replicas
        // on alternating polls → flap).
        for pool in &pools {
            if pool.metadata.deletion_timestamp.is_some() {
                debug!(pool = %pool_key(pool), "skipping: pool is being deleted");
                continue;
            }
            // Ephemeral pools have no STS to patch. Job spawning is
            // driven by the reconciler (ephemeral.rs), not this
            // autoscaler. Without this skip, scale_one would log
            // a spurious "STS not found" every poll.
            if pool.spec.ephemeral {
                debug!(pool = %pool_key(pool), "skipping: ephemeral pool (Job mode, no STS)");
                continue;
            }
            if is_wps_owned(pool) {
                debug!(pool = %pool_key(pool), "skipping: WPS-owned (scaled per-class below)");
                continue;
            }
            if let Some(err) = self.scale_one(pool, &status).await {
                self.patch_error_condition(pool, &err).await;
            }
        }

        // ---- Per-class scaling for WorkerPoolSet children ----
        // r[impl ctrl.wps.autoscale]
        // For each WPS, iterate its classes, look up per-class
        // queued from GetSizeClassStatus, compute desired, SSA-
        // patch the child's StatefulSet replicas with a DISTINCT
        // field manager (`rio-controller-wps-autoscaler`). SSA
        // merges field ownership — the WorkerPool reconciler owns
        // the STS template (containers, volumes, env); this owns
        // just spec.replicas.
        for wps in &wps_list {
            if wps.metadata.deletion_timestamp.is_some() {
                debug!(wps = %wps.name_any(), "skipping: WPS is being deleted");
                continue;
            }
            for class in &wps.spec.classes {
                self.scale_wps_class(wps, class, &sc_resp, &pools).await;
            }
        }

        // Emit gauges AFTER scaling decisions. One `set` per pool.
        // Two gauges (actual vs desired) so dashboards can plot
        // the gap.
        for pool in &pools {
            let key = pool_key(pool);
            let actual = pool.status.as_ref().map(|s| s.replicas).unwrap_or(0);
            let desired = self
                .states
                .get(&key)
                .map(|s| s.last_desired)
                .unwrap_or(pool.spec.replicas.min);
            metrics::gauge!("rio_controller_workerpool_replicas",
                "pool" => key.clone(), "kind" => "actual")
            .set(actual as f64);
            metrics::gauge!("rio_controller_workerpool_replicas",
                "pool" => key.clone(), "kind" => "desired")
            .set(desired as f64);
        }

        Ok(())
    }

    /// Scale one pool. Computes desired, checks stabilization,
    /// maybe patches the StatefulSet.
    ///
    /// Returns `None` on success / valid skip (STS not found,
    /// read error, stabilizing); `Some(reason)` on configuration
    /// error that should surface in `WorkerPool.status.conditions`.
    /// The caller patches conditions — keeping the status patch
    /// out of this fn lets tick() batch it with the gauge updates.
    async fn scale_one(
        &mut self,
        pool: &WorkerPool,
        status: &rio_proto::types::ClusterStatusResponse,
    ) -> Option<ScaleError> {
        let key = pool_key(pool);
        let ns = pool.namespace().unwrap_or_default();
        let sts_name = format!("{}-workers", pool.name_any());

        // ---- Validate metric ----
        // Only "queueDepth" is supported. The CRD uses a free-form
        // string (not enum) so future metrics don't need a CRD
        // version bump — but an unrecognized value is an operator
        // error, not a "fall through to default." Skip this pool;
        // tick() surfaces the error via WorkerPool.status.conditions.
        if pool.spec.autoscaling.metric != "queueDepth" {
            warn!(
                pool = %key,
                metric = %pool.spec.autoscaling.metric,
                "unknown autoscaling metric (only queueDepth supported); skipping pool"
            );
            return Some(ScaleError::UnknownMetric(
                pool.spec.autoscaling.metric.clone(),
            ));
        }

        // ---- Compute desired ----
        let desired = compute_desired(
            status.queued_derivations,
            pool.spec.autoscaling.target_value,
            pool.spec.replicas.min,
            pool.spec.replicas.max,
        );

        // ---- Current (from StatefulSet, not pool.status — the
        // reconciler's status may lag). ----
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &ns);
        let current = match sts_api.get_opt(&sts_name).await {
            Ok(Some(sts)) => sts.spec.and_then(|s| s.replicas).unwrap_or(0),
            Ok(None) => {
                // StatefulSet doesn't exist yet (reconciler hasn't
                // run, or was just deleted). Skip — reconciler
                // creates it, next tick scales.
                debug!(pool = %key, "StatefulSet not found; skipping scale");
                return None;
            }
            Err(e) => {
                warn!(pool = %key, error = %e, "failed to read StatefulSet");
                return None;
            }
        };

        // ---- Stabilization check ----
        let timing = self.timing;
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| ScaleState::new(current, timing.min_scale_interval));

        let decision = check_stabilization(state, current, desired, timing);

        match decision {
            Decision::Patch(direction) => {
                // Patch `spec.replicas` directly. Server-side
                // apply with a DIFFERENT field manager than the
                // reconciler — we own `spec.replicas`, reconciler
                // owns everything else. Two managers, no conflict.
                //
                let patch = sts_replicas_patch(desired);
                match sts_api
                    .patch(
                        &sts_name,
                        &PatchParams::apply("rio-controller-autoscaler").force(),
                        &Patch::Apply(&patch),
                    )
                    .await
                {
                    Ok(_) => {
                        state.last_patch = Instant::now();
                        info!(
                            pool = %key,
                            from = current,
                            to = desired,
                            direction = direction.as_str(),
                            queued = status.queued_derivations,
                            active = status.active_workers,
                            "scaled"
                        );
                        metrics::counter!("rio_controller_scaling_decisions_total",
                            "direction" => direction.as_str())
                        .increment(1);

                        // Emit K8s Event on scale. Operators see
                        // ScaledUp/ScaledDown in `kubectl describe
                        // workerpool <name>`. Best-effort (event
                        // publish fail logged).
                        let event_reason = match direction {
                            Direction::Up => "ScaledUp",
                            Direction::Down => "ScaledDown",
                        };
                        if let Err(e) = self
                            .recorder
                            .publish(
                                &kube::runtime::events::Event {
                                    type_: kube::runtime::events::EventType::Normal,
                                    reason: event_reason.into(),
                                    note: Some(format!(
                                        "Scaled replicas {current}→{desired} (queued={}, active={})",
                                        status.queued_derivations,
                                        status.active_workers
                                    )),
                                    action: "Scale".into(),
                                    secondary: None,
                                },
                                &pool.object_ref(&()),
                            )
                            .await
                        {
                            warn!(pool = %key, error = %e, "K8s scale event publish failed");
                        }

                        // Best-effort: record the scale in
                        // WorkerPool.status. Fire-and-forget
                        // (log on fail) — the STS patch is the
                        // source of truth; status is observability.
                        self.patch_scaled_status(pool, current, desired, direction)
                            .await;
                    }
                    Err(e) => {
                        warn!(pool = %key, error = %e, "scale patch failed");
                    }
                }
            }
            Decision::Wait(reason) => {
                debug!(
                    pool = %key,
                    current,
                    desired,
                    reason = reason.as_str(),
                    "waiting"
                );
            }
        }

        None
    }

    /// Patch `WorkerPool.status.{lastScaleTime,conditions}` after
    /// a successful scale. Best-effort: log warn on failure, don't
    /// propagate. The STS patch already landed — that's the source
    /// of truth. This is observability.
    ///
    /// Uses a SEPARATE SSA field-manager from the reconciler
    /// ("rio-controller-autoscaler-status" vs "rio-controller").
    /// SSA splits field ownership: reconciler owns replicas/ready/
    /// desired (patched every reconcile), we own lastScaleTime/
    /// conditions (patched only on scale). Neither clobbers the
    /// other.
    async fn patch_scaled_status(
        &self,
        pool: &WorkerPool,
        from: i32,
        to: i32,
        direction: Direction,
    ) {
        let reason = match direction {
            Direction::Up => "ScaledUp",
            Direction::Down => "ScaledDown",
        };
        let msg = format!("scaled from {from} to {to}");
        let cond = scaling_condition("True", reason, &msg);
        self.patch_status_partial(pool, Some(cond)).await;
    }

    /// Patch a config-error condition. The Autoscaling.metric doc
    /// promises "unknown metric is a RECONCILE error (surfaces in
    /// .status.conditions)" — this delivers it.
    async fn patch_error_condition(&self, pool: &WorkerPool, err: &ScaleError) {
        let (reason, msg) = match err {
            ScaleError::UnknownMetric(m) => (
                "UnknownMetric",
                format!("autoscaling.metric '{m}' is not supported (only 'queueDepth')"),
            ),
        };
        let cond = scaling_condition("False", reason, &msg);
        self.patch_status_partial(pool, Some(cond)).await;
    }

    /// Low-level: patch WorkerPool.status.{lastScaleTime,conditions}.
    /// Partial status (replicas/ready/desired are the reconciler's
    /// field-manager, not ours).
    ///
    /// `condition = None` → clear conditions (future use: when a
    /// previously-errored pool becomes valid again). Currently both
    /// callers pass Some.
    async fn patch_status_partial(&self, pool: &WorkerPool, condition: Option<serde_json::Value>) {
        let ns = pool.namespace().unwrap_or_default();
        let name = pool.name_any();

        // Conditions as a Vec (standard K8s Conditions pattern).
        // Single condition type ("Scaling") — we overwrite on
        // each call rather than appending (conditions are
        // identified by type; duplicates are a bug).
        let conditions: Vec<serde_json::Value> = condition.into_iter().collect();

        let patch = wp_status_patch(&conditions);

        let api: Api<WorkerPool> = Api::namespaced(self.client.clone(), &ns);
        if let Err(e) = api
            .patch_status(
                &name,
                &PatchParams::apply(STATUS_MANAGER).force(),
                &Patch::Apply(&patch),
            )
            .await
        {
            // Warn not error: status patch is observability, not
            // correctness. The STS replica patch already landed.
            warn!(
                pool = %pool_key(pool),
                error = %e,
                "failed to patch WorkerPool.status (scale itself succeeded)"
            );
        }
    }
}

#[cfg(test)]
mod tests {
    use super::super::{STATUS_MANAGER, WPS_AUTOSCALER_MANAGER};

    /// The WPS autoscaler manager name differs from the standalone
    /// autoscaler's. If they're accidentally the same, SSA can't
    /// distinguish the two scalers and ownership collapses (both
    /// look like one manager to the apiserver → no conflict
    /// detection → last-write-wins without the operator knowing).
    #[test]
    fn wps_manager_distinct_from_standalone() {
        // "rio-controller-autoscaler" patches standalone pool
        // STS replicas (see scale_one → sts_api.patch). The WPS
        // manager MUST be a different string.
        assert_ne!(
            WPS_AUTOSCALER_MANAGER, "rio-controller-autoscaler",
            "WPS autoscaler manager must differ from standalone autoscaler's"
        );
        assert_ne!(
            WPS_AUTOSCALER_MANAGER, STATUS_MANAGER,
            "WPS autoscaler manager must differ from autoscaler-status manager"
        );
        // Also distinct from the WPS reconciler's child-template
        // manager (workerpoolset::MANAGER = "rio-controller-wps").
        // Exact-string check — a prefix accident would also hurt
        // (operators grepping managedFields by prefix).
        assert_ne!(
            WPS_AUTOSCALER_MANAGER, "rio-controller-wps",
            "WPS autoscaler manager must differ from WPS reconciler manager"
        );
    }
}
