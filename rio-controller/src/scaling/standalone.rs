//! Cluster-wide autoscaler loop: poll `ClusterStatus`, patch
//! StatefulSet replicas for STANDALONE BuilderPools.
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

use crate::crds::builderpool::BuilderPool;
use crate::crds::builderpoolset::BuilderPoolSet;
use crate::crds::fetcherpool::FetcherPool;
use crate::reconcilers::common::sts::{ExecutorRole, sts_name};

use super::{
    Decision, Direction, STATUS_MANAGER, ScaleError, ScaleState, ScalingTiming, WaitReason,
    check_stabilization, compute_desired, find_condition, find_fp_condition, fp_pool_key,
    fp_status_patch, is_wps_owned, pool_key, scaling_condition, sts_replicas_patch,
    wp_status_patch,
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
    /// decisions in `kubectl describe builderpool`.
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
        // scale signal for standalone BuilderPools (those not owned
        // by a WPS).
        //
        // GetSizeClassStatus is NOT fetched here: WPS children need
        // a per-child feature-filtered request (I-176), so
        // `scale_wps_class` issues its own RPC per child.
        let status = self
            .scheduler
            .cluster_status(())
            .await
            .map(|r| r.into_inner())?;

        // ---- List all BuilderPools + BuilderPoolSets ----
        let pools_api: Api<BuilderPool> = Api::all(self.client.clone());
        let pools = pools_api.list(&Default::default()).await?.items;

        let wps_api: Api<BuilderPoolSet> = Api::all(self.client.clone());
        // Best-effort: WPS CRD may not be installed on older
        // clusters. An empty list is fine — no per-class scaling.
        let wps_list = wps_api
            .list(&Default::default())
            .await
            .map(|l| l.items)
            .unwrap_or_else(|e| {
                debug!(error = %e, "BuilderPoolSet list failed (CRD not installed?); per-class scaling disabled");
                Vec::new()
            });

        // ---- List FetcherPools ----
        // Same shape as BuilderPool listing. Best-effort: CRD may
        // not be installed (older clusters, or operator chose not
        // to deploy fetchers). Empty list → no fetcher scaling.
        let fp_api: Api<FetcherPool> = Api::all(self.client.clone());
        let fetcher_pools = fp_api
            .list(&Default::default())
            .await
            .map(|l| l.items)
            .unwrap_or_else(|e| {
                debug!(error = %e, "FetcherPool list failed (CRD not installed?); fetcher scaling disabled");
                Vec::new()
            });

        // ---- Prune state for deleted pools ----
        // Pools gone from the list → remove their ScaleState.
        // Without this, the map grows unboundedly over pool
        // create/delete cycles (small leak, but still).
        // Live set covers both BuilderPool and FetcherPool keys
        // (the latter prefixed `fp:` — see `fp_pool_key`).
        let live_keys: std::collections::HashSet<String> = pools
            .iter()
            .map(pool_key)
            .chain(fetcher_pools.iter().map(fp_pool_key))
            .collect();
        self.states.retain(|k, _| live_keys.contains(k));

        // ---- Compute + maybe patch each pool ----
        // scale_one() returns Some(err) for config errors (unknown
        // metric). Those surface via BuilderPool.status.conditions
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

        // ---- FetcherPool scaling ----
        // Same skip rules as BuilderPool (deletionTimestamp). No WPS
        // ownership — fetchers are simpler. Signal is
        // `queued_fod_derivations` (in-flight FOD demand: Ready +
        // Assigned + Running). Ephemeral pools are skipped here —
        // their reconciler spawns Jobs directly per the same signal.
        for fp in &fetcher_pools {
            if fp.metadata.deletion_timestamp.is_some() {
                debug!(pool = %fp_pool_key(fp), "skipping: FetcherPool is being deleted");
                continue;
            }
            if fp.spec.ephemeral {
                debug!(pool = %fp_pool_key(fp), "skipping: ephemeral pool, reconciler owns Job count");
                continue;
            }
            if let Some(err) = self.scale_fetcher_one(fp, &status).await {
                self.patch_fp_error_condition(fp, &err).await;
            }
        }

        // ---- Per-class scaling for BuilderPoolSet children ----
        // r[impl ctrl.wps.autoscale]
        // For each WPS, iterate its classes, look up per-class
        // queued from GetSizeClassStatus, compute desired, SSA-
        // patch the child's StatefulSet replicas with a DISTINCT
        // field manager (`rio-controller-wps-autoscaler`). SSA
        // merges field ownership — the BuilderPool reconciler owns
        // the STS template (containers, volumes, env); this owns
        // just spec.replicas.
        for wps in &wps_list {
            if wps.metadata.deletion_timestamp.is_some() {
                debug!(wps = %wps.name_any(), "skipping: WPS is being deleted");
                continue;
            }
            for class in &wps.spec.classes {
                self.scale_wps_class(wps, class, &pools).await;
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
    /// error that should surface in `BuilderPool.status.conditions`.
    /// The caller patches conditions — keeping the status patch
    /// out of this fn lets tick() batch it with the gauge updates.
    async fn scale_one(
        &mut self,
        pool: &BuilderPool,
        status: &rio_proto::types::ClusterStatusResponse,
    ) -> Option<ScaleError> {
        let key = pool_key(pool);
        let ns = pool.namespace().unwrap_or_default();
        let sts_name = sts_name(&pool.name_any(), ExecutorRole::Builder);

        // ---- Validate metric ----
        // Only "queueDepth" is supported. The CRD uses a free-form
        // string (not enum) so future metrics don't need a CRD
        // version bump — but an unrecognized value is an operator
        // error, not a "fall through to default." Skip this pool;
        // tick() surfaces the error via BuilderPool.status.conditions.
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
        // I-107: per-system breakdown so per-arch pools scale on their
        // own backlog. Falls back to the scalar on old schedulers.
        let queued = super::queued_for_systems(status, &pool.spec.systems);
        let desired = compute_desired(
            queued,
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
                            queued,
                            active = status.active_executors,
                            "scaled"
                        );
                        metrics::counter!("rio_controller_scaling_decisions_total",
                            "direction" => direction.as_str())
                        .increment(1);

                        // Emit K8s Event on scale. Operators see
                        // ScaledUp/ScaledDown in `kubectl describe
                        // builderpool <name>`. Best-effort (event
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
                                        "Scaled replicas {current}→{desired} (queued={queued}, active={})",
                                        status.active_executors
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
                        // BuilderPool.status. Fire-and-forget
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
                // Surface Stabilizing in status so tests can poll for it
                // without waiting out the full scale_down_window (600s
                // default). Only patch on reason TRANSITION — every tick
                // would be 20 patches/window at default, 200 at test poll.
                if matches!(reason, WaitReason::Stabilizing) {
                    let prev = find_condition(pool, "Scaling");
                    let already = prev
                        .as_ref()
                        .and_then(|p| p.get("reason").and_then(|r| r.as_str()))
                        == Some("Stabilizing");
                    if !already {
                        let msg = format!(
                            "desired={desired} (current={current}); waiting for stabilization window"
                        );
                        let cond = scaling_condition("True", "Stabilizing", &msg, prev.as_ref());
                        self.patch_status_partial(pool, Some(cond)).await;
                    }
                }
            }
        }

        None
    }

    /// Patch `BuilderPool.status.{lastScaleTime,conditions}` after
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
        pool: &BuilderPool,
        from: i32,
        to: i32,
        direction: Direction,
    ) {
        let reason = match direction {
            Direction::Up => "ScaledUp",
            Direction::Down => "ScaledDown",
        };
        let msg = format!("scaled from {from} to {to}");
        let prev = find_condition(pool, "Scaling");
        let cond = scaling_condition("True", reason, &msg, prev.as_ref());
        self.patch_status_partial(pool, Some(cond)).await;
    }

    /// Patch a config-error condition. The Autoscaling.metric doc
    /// promises "unknown metric is a RECONCILE error (surfaces in
    /// .status.conditions)" — this delivers it.
    async fn patch_error_condition(&self, pool: &BuilderPool, err: &ScaleError) {
        let (reason, msg) = match err {
            ScaleError::UnknownMetric(m) => (
                "UnknownMetric",
                format!("autoscaling.metric '{m}' is not supported (only 'queueDepth')"),
            ),
        };
        let prev = find_condition(pool, "Scaling");
        let cond = scaling_condition("False", reason, &msg, prev.as_ref());
        self.patch_status_partial(pool, Some(cond)).await;
    }

    /// Scale one FetcherPool. Same shape as [`scale_one`] but:
    /// metric is `"fodQueueDepth"`, signal is `queued_fod_derivations`,
    /// STS suffix is `-fetcher` (via `sts_name`).
    ///
    /// Shares `compute_desired` / `check_stabilization` /
    /// `sts_replicas_patch` with the BuilderPool path. The state
    /// key is prefixed `fp:` so a same-name BuilderPool and
    /// FetcherPool don't share a stabilization window.
    async fn scale_fetcher_one(
        &mut self,
        pool: &FetcherPool,
        status: &rio_proto::types::ClusterStatusResponse,
    ) -> Option<ScaleError> {
        let key = fp_pool_key(pool);
        let ns = pool.namespace().unwrap_or_default();
        let sts = sts_name(&pool.name_any(), ExecutorRole::Fetcher);

        if pool.spec.autoscaling.metric != "fodQueueDepth" {
            warn!(
                pool = %key,
                metric = %pool.spec.autoscaling.metric,
                "unknown FetcherPool autoscaling metric (only fodQueueDepth supported); skipping pool"
            );
            return Some(ScaleError::UnknownMetric(
                pool.spec.autoscaling.metric.clone(),
            ));
        }

        let desired = compute_desired(
            status.queued_fod_derivations,
            pool.spec.autoscaling.target_value,
            pool.spec.replicas.min,
            pool.spec.replicas.max,
        );

        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &ns);
        let current = match sts_api.get_opt(&sts).await {
            Ok(Some(s)) => s.spec.and_then(|s| s.replicas).unwrap_or(0),
            Ok(None) => {
                debug!(pool = %key, "StatefulSet not found; skipping scale");
                return None;
            }
            Err(e) => {
                warn!(pool = %key, error = %e, "failed to read StatefulSet");
                return None;
            }
        };

        let timing = self.timing;
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| ScaleState::new(current, timing.min_scale_interval));

        match check_stabilization(state, current, desired, timing) {
            Decision::Patch(direction) => {
                let patch = sts_replicas_patch(desired);
                match sts_api
                    .patch(
                        &sts,
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
                            queued_fod = status.queued_fod_derivations,
                            "scaled FetcherPool"
                        );
                        metrics::counter!("rio_controller_scaling_decisions_total",
                            "direction" => direction.as_str())
                        .increment(1);

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
                                        "Scaled replicas {current}→{desired} (queued_fod={})",
                                        status.queued_fod_derivations
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

                        let reason = match direction {
                            Direction::Up => "ScaledUp",
                            Direction::Down => "ScaledDown",
                        };
                        let msg = format!("scaled from {current} to {desired}");
                        let prev = find_fp_condition(pool, "Scaling");
                        let cond = scaling_condition("True", reason, &msg, prev.as_ref());
                        self.patch_fp_status(pool, Some(cond)).await;
                    }
                    Err(e) => {
                        warn!(pool = %key, error = %e, "FetcherPool scale patch failed");
                    }
                }
            }
            Decision::Wait(reason) => {
                debug!(pool = %key, current, desired, reason = reason.as_str(), "waiting");
                if matches!(reason, WaitReason::Stabilizing) {
                    let prev = find_fp_condition(pool, "Scaling");
                    let already = prev
                        .as_ref()
                        .and_then(|p| p.get("reason").and_then(|r| r.as_str()))
                        == Some("Stabilizing");
                    if !already {
                        let msg = format!(
                            "desired={desired} (current={current}); waiting for stabilization window"
                        );
                        let cond = scaling_condition("True", "Stabilizing", &msg, prev.as_ref());
                        self.patch_fp_status(pool, Some(cond)).await;
                    }
                }
            }
        }

        None
    }

    async fn patch_fp_error_condition(&self, pool: &FetcherPool, err: &ScaleError) {
        let (reason, msg) = match err {
            ScaleError::UnknownMetric(m) => (
                "UnknownMetric",
                format!("autoscaling.metric '{m}' is not supported (only 'fodQueueDepth')"),
            ),
        };
        let prev = find_fp_condition(pool, "Scaling");
        let cond = scaling_condition("False", reason, &msg, prev.as_ref());
        self.patch_fp_status(pool, Some(cond)).await;
    }

    /// FetcherPool variant of [`patch_status_partial`]. Same SSA
    /// field-manager (`STATUS_MANAGER`) — the autoscaler owns
    /// `lastScaleTime`/`conditions` for both pool kinds.
    async fn patch_fp_status(&self, pool: &FetcherPool, condition: Option<serde_json::Value>) {
        let ns = pool.namespace().unwrap_or_default();
        let name = pool.name_any();
        let conditions: Vec<serde_json::Value> = condition.into_iter().collect();
        let patch = fp_status_patch(&conditions);
        let api: Api<FetcherPool> = Api::namespaced(self.client.clone(), &ns);
        if let Err(e) = api
            .patch_status(
                &name,
                &PatchParams::apply(STATUS_MANAGER).force(),
                &Patch::Apply(&patch),
            )
            .await
        {
            warn!(
                pool = %fp_pool_key(pool),
                error = %e,
                "failed to patch FetcherPool.status (scale itself succeeded)"
            );
        }
    }

    /// Low-level: patch BuilderPool.status.{lastScaleTime,conditions}.
    /// Partial status (replicas/ready/desired are the reconciler's
    /// field-manager, not ours).
    ///
    /// `condition = None` → clear conditions (future use: when a
    /// previously-errored pool becomes valid again). Currently both
    /// callers pass Some.
    async fn patch_status_partial(&self, pool: &BuilderPool, condition: Option<serde_json::Value>) {
        let ns = pool.namespace().unwrap_or_default();
        let name = pool.name_any();

        // Conditions as a Vec (standard K8s Conditions pattern).
        // Single condition type ("Scaling") — we overwrite on
        // each call rather than appending (conditions are
        // identified by type; duplicates are a bug).
        let conditions: Vec<serde_json::Value> = condition.into_iter().collect();

        let patch = wp_status_patch(&conditions);

        let api: Api<BuilderPool> = Api::namespaced(self.client.clone(), &ns);
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
                "failed to patch BuilderPool.status (scale itself succeeded)"
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
        // manager (builderpoolset::MANAGER = "rio-controller-wps").
        // Exact-string check — a prefix accident would also hurt
        // (operators grepping managedFields by prefix).
        assert_ne!(
            WPS_AUTOSCALER_MANAGER, "rio-controller-wps",
            "WPS autoscaler manager must differ from WPS reconciler manager"
        );
    }
}
