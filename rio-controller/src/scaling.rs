//! Autoscaling loop: poll `ClusterStatus`, patch StatefulSet replicas.
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
use std::time::{Duration, Instant};

use k8s_openapi::api::apps::v1::StatefulSet;
use kube::api::{Api, Patch, PatchParams};
use kube::{Client, Resource, ResourceExt};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use rio_proto::AdminServiceClient;
use rio_proto::types::{GetSizeClassStatusRequest, GetSizeClassStatusResponse};

use crate::crds::workerpool::WorkerPool;
use crate::crds::workerpoolset::WorkerPoolSet;

/// Stabilization window for scale-down. Long: avoid killing
/// workers right before the next burst. 10 min is the K8s HPA
/// default `--horizontal-pod-autoscaler-downscale-stabilization`;
/// we follow that convention as the DEFAULT. Configurable via
/// `RIO_AUTOSCALER_SCALE_DOWN_WINDOW_SECS` so VM tests can observe
/// a full up→down cycle without waiting 10 minutes — production
/// deploys leave this at the default.
#[derive(Debug, Clone, Copy)]
pub struct ScalingTiming {
    /// How often to poll ClusterStatus. Granularity for everything
    /// else — polling faster than the up-window doesn't help.
    pub poll_interval: Duration,
    /// Stabilization window for scale-up. Desired must be stable
    /// (same value) for this long before we patch. Queue depth
    /// is high-confidence → react relatively fast.
    pub scale_up_window: Duration,
    /// Stabilization window for scale-down. Default 10 min (K8s
    /// HPA convention). Anti-flap: killing workers right before
    /// the next burst is the #1 autoscaler failure mode.
    pub scale_down_window: Duration,
    /// Minimum interval between any two patches. Anti-flap: a
    /// desired that wobbles around a boundary shouldn't cause
    /// rapid patch churn.
    pub min_scale_interval: Duration,
}

impl Default for ScalingTiming {
    fn default() -> Self {
        Self {
            poll_interval: Duration::from_secs(30),
            scale_up_window: Duration::from_secs(30),
            scale_down_window: Duration::from_secs(600),
            min_scale_interval: Duration::from_secs(30),
        }
    }
}

/// Configuration error returned by `scale_one` for the caller
/// to surface via `WorkerPool.status.conditions`. Transient
/// errors (K8s API flake, STS not found) return `None` — they
/// log + retry next tick, no condition update.
#[derive(Debug, Clone)]
pub(crate) enum ScaleError {
    /// `spec.autoscaling.metric` is an unrecognized value.
    /// Carried value = the operator's metric string (for the
    /// condition message).
    UnknownMetric(String),
}

/// Per-pool stabilization state. Lives across poll iterations.
#[derive(Debug)]
struct ScaleState {
    /// What we computed LAST iteration. If this iteration's
    /// desired differs, reset `stable_since`.
    last_desired: i32,
    /// When `desired` last changed. Stabilization window starts here.
    stable_since: Instant,
    /// Last actual patch time. Anti-flap check.
    last_patch: Instant,
}

impl ScaleState {
    fn new(initial: i32, min_interval: Duration) -> Self {
        let now = Instant::now();
        Self {
            last_desired: initial,
            stable_since: now,
            // Initialize to "long ago" so the first patch isn't
            // anti-flap-blocked. checked_sub → None on underflow;
            // unwrap_or(now) is a no-op on the FIRST iteration
            // since stable_since hasn't elapsed anyway. Uses the
            // TUNABLE min_interval — with the 3s VM-test override,
            // a 60s-ago init would work but 2× the actual config
            // value is clearer (exactly "past the anti-flap gate").
            last_patch: now.checked_sub(min_interval * 2).unwrap_or(now),
        }
    }
}

/// Autoscaler. Constructed once in main.rs, `run()` loops forever.
pub struct Autoscaler {
    client: Client,
    scheduler: AdminServiceClient<Channel>,
    /// Per-pool state, keyed by `namespace/name`. Persists across
    /// iterations. Pruned when a pool disappears from the list.
    states: HashMap<String, ScaleState>,
    /// Timing knobs. `Copy` — passed by value into
    /// check_stabilization (4 Durations = 32 bytes).
    timing: ScalingTiming,
    /// K8s Events recorder. Emits ScaledUp/ScaledDown events on
    /// successful scale patches so operators see the autoscaler's
    /// decisions in `kubectl describe workerpool`.
    recorder: kube::runtime::events::Recorder,
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

    /// Scale one WPS child pool using PER-CLASS queue depth.
    ///
    /// Looks up the class's `queued` from `GetSizeClassStatus`
    /// (not cluster-wide `ClusterStatus.queued_derivations`).
    /// Falls through to the child WorkerPool's own `autoscaling.
    /// target_value` / `replicas.{min,max}` — those were set by
    /// the WPS reconciler from `SizeClassSpec` (see
    /// `workerpoolset/builders.rs::build_child_workerpool`).
    ///
    /// Same stabilization mechanics as `scale_one` (shared
    /// `ScaleState` by pool key). Patches the child's StatefulSet
    /// `spec.replicas` with field manager `rio-controller-wps-
    /// autoscaler` — distinct from the standalone autoscaler's
    /// `rio-controller-autoscaler` so `kubectl get sts -o yaml |
    /// grep managedFields` shows which scaler owns the replica
    /// count.
    ///
    /// Skips children whose WorkerPool has `deletionTimestamp`
    /// (same finalizer-fight avoidance as the standalone loop).
    ///
    /// `pools`: the already-listed WorkerPools from `tick()`.
    /// We look up the child here rather than re-GETting — saves
    /// one apiserver call per class per tick.
    async fn scale_wps_class(
        &mut self,
        wps: &WorkerPoolSet,
        class: &crate::crds::workerpoolset::SizeClassSpec,
        sc_resp: &GetSizeClassStatusResponse,
        pools: &[WorkerPool],
    ) {
        let child_name = format!("{}-{}", wps.name_any(), class.name);
        let wps_ns = wps.namespace().unwrap_or_default();

        // r[impl ctrl.wps.autoscale]
        // Find the child WorkerPool in the already-listed set.
        // Two-key symmetry with `is_wps_owned`: the standalone-
        // pool loop skips pools WITH a WPS ownerRef; this loop
        // must skip pools WITHOUT. A name-match without ownerRef
        // means the pool was manually created (or created by
        // something else) with a colliding name — scaling it here
        // would fight the standalone loop. See P0374 for the flap
        // scenario this prevents.
        let child = match find_wps_child(wps, &class.name, pools) {
            ChildLookup::Found(c) => c,
            ChildLookup::NotCreated => {
                debug!(child = %child_name, "WPS child not yet created; skipping scale");
                return;
            }
            ChildLookup::NameCollision => {
                warn!(
                    child = %child_name,
                    "pool name matches {{wps}}-{{class}} but has no WPS ownerRef — \
                     not scaling per-class (would flap against standalone loop)"
                );
                return;
            }
        };

        // r[impl ctrl.autoscale.skip-deleting] — same for WPS children.
        if child.metadata.deletion_timestamp.is_some() {
            debug!(child = %child_name, "skipping: child is being deleted");
            return;
        }

        // Per-class queue depth. Missing class in the RPC response
        // = scheduler doesn't have size-class routing configured
        // for this name, or the response was empty (RPC failed,
        // default). Fall back to 0 → `compute_desired` returns
        // `min` → no spurious scale-up on a misconfigured class.
        let queued = sc_resp
            .classes
            .iter()
            .find(|c| c.name == class.name)
            .map(|c| c.queued)
            .unwrap_or(0);

        // Bounds come from the CHILD WorkerPool's spec (which the
        // WPS reconciler set from SizeClassSpec). Reading from
        // the child rather than re-deriving from `class.*` keeps
        // this in sync with what the reconciler actually applied
        // (operator may have manually edited the child).
        //
        // queued is u64; compute_desired takes u32. Saturate —
        // a queue > 4 billion derivations is pathological but
        // in u64 range; don't wrap to 0 (would scale DOWN under
        // extreme load).
        let queued_u32 = queued.min(u32::MAX as u64) as u32;
        let desired = compute_desired(
            queued_u32,
            child.spec.autoscaling.target_value,
            child.spec.replicas.min,
            child.spec.replicas.max,
        );

        let key = pool_key(child);
        let sts_name = format!("{child_name}-workers");
        let sts_api: Api<StatefulSet> = Api::namespaced(self.client.clone(), &wps_ns);

        // Current from STS (same pattern as scale_one — reconciler
        // status may lag).
        let current = match sts_api.get_opt(&sts_name).await {
            Ok(Some(sts)) => sts.spec.and_then(|s| s.replicas).unwrap_or(0),
            Ok(None) => {
                debug!(child = %child_name, "STS not yet created; skipping scale");
                return;
            }
            Err(e) => {
                warn!(child = %child_name, error = %e, "failed to read child STS");
                return;
            }
        };

        let timing = self.timing;
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| ScaleState::new(current, timing.min_scale_interval));

        let decision = check_stabilization(state, current, desired, timing);

        match decision {
            Decision::Patch(direction) => {
                // SSA body is identical to `sts_replicas_patch` —
                // the FIELD MANAGER is what differs. The patch
                // body must still carry apiVersion+kind (SSA
                // requirement — see lang-gotchas 3a bug).
                let patch = sts_replicas_patch(desired);
                match sts_api
                    .patch(
                        &sts_name,
                        &PatchParams::apply(WPS_AUTOSCALER_MANAGER).force(),
                        &Patch::Apply(&patch),
                    )
                    .await
                {
                    Ok(_) => {
                        state.last_patch = Instant::now();
                        info!(
                            wps = %wps.name_any(),
                            class = %class.name,
                            child = %child_name,
                            from = current,
                            to = desired,
                            direction = direction.as_str(),
                            queued,
                            "scaled (per-class)"
                        );
                        metrics::counter!("rio_controller_scaling_decisions_total",
                            "direction" => direction.as_str())
                        .increment(1);
                    }
                    Err(e) => {
                        warn!(child = %child_name, error = %e, "per-class scale patch failed");
                    }
                }
            }
            Decision::Wait(reason) => {
                debug!(
                    child = %child_name,
                    current,
                    desired,
                    queued,
                    reason = reason.as_str(),
                    "waiting (per-class)"
                );
            }
        }
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

/// SSA field-manager for autoscaler's WorkerPool.status patches.
/// DIFFERENT from the reconciler's "rio-controller" — SSA splits
/// field ownership so the two don't clobber each other.
const STATUS_MANAGER: &str = "rio-controller-autoscaler-status";

/// SSA field-manager for per-class (WPS child) StatefulSet
/// replica patches. Distinct from `rio-controller-autoscaler`
/// (standalone WorkerPool scaling) AND from `rio-controller-wps`
/// (the WPS reconciler's child-template sync). `kubectl get sts
/// -o yaml | grep managedFields` shows three managers on a WPS
/// child's STS, each owning its slice: reconciler owns the pod
/// template, this owns spec.replicas, and nothing else touches
/// either.
pub(crate) const WPS_AUTOSCALER_MANAGER: &str = "rio-controller-wps-autoscaler";

/// Build a K8s Condition for the "Scaling" type. Uses json!
/// instead of k8s_openapi's Condition struct because:
/// (1) Condition requires all fields including observedGeneration
/// which we don't track, (2) json! is easier to partial-patch.
///
/// `status`: "True" (scaled OK) / "False" (config error).
/// `reason`: CamelCase machine-readable (ScaledUp/UnknownMetric).
/// `message`: human-readable.
fn scaling_condition(status: &str, reason: &str, message: &str) -> serde_json::Value {
    // k8s_openapi re-exports jiff (kube 3.0's chrono replacement).
    // Timestamp::now() → Display is RFC3339 with offset (UTC Z).
    // K8s Condition.lastTransitionTime expects this format.
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    serde_json::json!({
        "type": "Scaling",
        "status": status,
        "reason": reason,
        "message": message,
        "lastTransitionTime": now,
    })
}

/// Build the SSA patch body for `WorkerPool.status.{lastScaleTime,
/// conditions}`. Partial status — replicas/ready/desired are the
/// reconciler's fields, not ours.
///
/// Same apiVersion+kind requirement as sts_replicas_patch (and
/// the reconciler's status_patch).
pub(crate) fn wp_status_patch(conditions: &[serde_json::Value]) -> serde_json::Value {
    use kube::CustomResourceExt;
    let ar = WorkerPool::api_resource();
    let now = k8s_openapi::jiff::Timestamp::now().to_string();
    serde_json::json!({
        "apiVersion": ar.api_version,
        "kind": ar.kind,
        "status": {
            "lastScaleTime": now,
            "conditions": conditions,
        },
    })
}

/// Build the SSA patch body for `StatefulSet.spec.replicas`.
///
/// apiVersion + kind are MANDATORY in SSA bodies: without them
/// the apiserver returns 400 "apiVersion must be set". Same
/// pattern as the reconcilers' status patches.
///
/// Extracted as a free fn so a unit test can assert the body
/// shape without spinning up a full mock-apiserver + mock
/// AdminService gRPC.
pub(crate) fn sts_replicas_patch(replicas: i32) -> serde_json::Value {
    serde_json::json!({
        "apiVersion": "apps/v1",
        "kind": "StatefulSet",
        "spec": { "replicas": replicas },
    })
}

/// Compute desired replicas from queue metrics.
///
/// The formula: `ceil(queued / target)` gives how many workers
/// we'd need if each handles `target` queued items. Clamped to
/// `[min, max]`.
///
/// We use queued/target, not queued/active. Why: if active=0 (all
/// workers crashed), we still want to scale up based on queue;
/// queued/active would divide by zero.
///
/// Edge: `target=0` would divide-by-zero. CRD doesn't enforce
/// `>0` (the CEL is on max_concurrent_builds, not target_value).
/// We clamp target to 1 here — target=0 is operator error ("scale
/// up on ANY queue") which clamping to 1 approximates.
///
/// Edge: `queued=0` → desired=0 → clamped to min. Correct: empty
/// queue means scale DOWN to min, not to zero.
pub(crate) fn compute_desired(queued: u32, target: i32, min: i32, max: i32) -> i32 {
    // Defensive min>max swap. The CRD's CEL validation enforces
    // min ≤ max, but a CRD installed before the CEL rule existed
    // (or edited with `--validate=false`) could have min>max.
    // i32::clamp PANICS if min>max — the autoscaler would die
    // silently. Swap with a warn so operator notices.
    let (min, max) = if min > max {
        tracing::warn!(
            min,
            max,
            "compute_desired: min > max (pre-CEL CRD?); swapping"
        );
        (max, min)
    } else {
        (min, max)
    };

    let target = target.max(1) as u32;
    // ceil division. clippy prefers the std method over the
    // (a + b - 1) / b idiom — same semantics, clearer intent.
    let raw = queued.div_ceil(target);
    // i32 clamp. min/max from the CRD are i32 (K8s replica counts
    // are). Bound raw within i32 range BEFORE casting — raw as i32
    // would wrap negative when raw > 2^31 (queued near u32::MAX),
    // and a negative value would then clamp to `min` → autoscaler
    // scales DOWN under extreme load. Pathological but in u32 range.
    let raw = raw.min(i32::MAX as u32) as i32;
    raw.clamp(min, max)
}

/// Scaling decision.
enum Decision {
    Patch(Direction),
    Wait(WaitReason),
}

#[derive(Clone, Copy)]
enum Direction {
    Up,
    Down,
}

impl Direction {
    fn as_str(self) -> &'static str {
        match self {
            Self::Up => "up",
            Self::Down => "down",
        }
    }
}

enum WaitReason {
    /// desired == current. Nothing to do.
    NoChange,
    /// desired changed this tick — reset window.
    DesiredChanged,
    /// Window hasn't elapsed yet.
    Stabilizing,
    /// Too soon since last patch.
    AntiFlap,
}

impl WaitReason {
    fn as_str(&self) -> &'static str {
        match self {
            Self::NoChange => "no_change",
            Self::DesiredChanged => "desired_changed",
            Self::Stabilizing => "stabilizing",
            Self::AntiFlap => "anti_flap",
        }
    }
}

/// Check stabilization windows. Updates `state` in place.
///
/// Separate fn for testability — all the timing logic is here,
/// no K8s or scheduler interaction.
fn check_stabilization(
    state: &mut ScaleState,
    current: i32,
    desired: i32,
    timing: ScalingTiming,
) -> Decision {
    let now = Instant::now();

    // Desired changed → reset window. Even if it changed BACK
    // to what it was 2 ticks ago, reset: we want "stable for
    // N seconds" which means no changes in that window.
    if desired != state.last_desired {
        state.last_desired = desired;
        state.stable_since = now;
        return Decision::Wait(WaitReason::DesiredChanged);
    }

    // At target. No-op. (After the change check: if we just
    // reached target THIS tick, the change check fires; if we
    // were ALREADY at target, this fires.)
    if desired == current {
        return Decision::Wait(WaitReason::NoChange);
    }

    // Direction + window. Both tunable: VM tests shorten both
    // to observe a full up→down cycle; production uses 30s up
    // / 600s down (K8s HPA convention for downscale anti-flap).
    let (direction, window) = if desired > current {
        (Direction::Up, timing.scale_up_window)
    } else {
        (Direction::Down, timing.scale_down_window)
    };

    if now.duration_since(state.stable_since) < window {
        return Decision::Wait(WaitReason::Stabilizing);
    }

    // Anti-flap. Prevents oscillation when desired wobbles
    // across a boundary (queue at exactly target*N).
    if now.duration_since(state.last_patch) < timing.min_scale_interval {
        return Decision::Wait(WaitReason::AntiFlap);
    }

    Decision::Patch(direction)
}

fn pool_key(pool: &WorkerPool) -> String {
    format!(
        "{}/{}",
        pool.namespace().unwrap_or_default(),
        pool.name_any()
    )
}

/// Is this WorkerPool a WPS child? Checks `ownerReferences` for
/// `kind=WorkerPoolSet` with `controller=true`. The WPS reconciler
/// sets this via `controller_owner_ref(&())` (see
/// `workerpoolset/builders.rs::build_child_workerpool`).
///
/// Used by `tick()` to skip WPS children in the standalone-pool
/// loop (those get per-class scaling via `scale_wps_class`
/// instead — two autoscalers on the same STS with different
/// signals would flap).
pub(crate) fn is_wps_owned(pool: &WorkerPool) -> bool {
    pool.metadata
        .owner_references
        .as_deref()
        .unwrap_or_default()
        .iter()
        .any(|or| or.kind == "WorkerPoolSet" && or.controller == Some(true))
}

/// Result of looking up the WPS child pool for per-class scaling.
/// Captures the three distinct outcomes so `scale_wps_class` can
/// log at the right level (debug for not-yet-created, warn for a
/// name collision — operators should know about the latter).
#[derive(Debug)]
pub(crate) enum ChildLookup<'a> {
    /// Child found by name AND has a WPS controller ownerRef.
    /// Safe to scale per-class.
    Found(&'a WorkerPool),
    /// No pool matches `{wps}-{class}` in the WPS namespace.
    /// Reconciler hasn't created it yet — skip this tick.
    NotCreated,
    /// A pool matches the name but is NOT WPS-owned. This is a
    /// name collision: likely a manually-created standalone pool
    /// named `{wps}-{class}` before the WPS was stood up. Scaling
    /// it per-class would flap against the standalone loop (both
    /// autoscalers patching the same STS with different signals).
    NameCollision,
}

/// Find the WPS child pool to scale for a class. Enforces the
/// two-key symmetry (name-match AND `is_wps_owned`) that prevents
/// the asymmetric-key flap: the standalone loop skips by ownerRef,
/// so the per-class loop must require ownerRef after name-match.
///
/// Pure fn so the gate is unit-testable without constructing an
/// Autoscaler + mock apiserver. The test at
/// `scale_wps_class_skips_name_collision_without_ownerref` is the
/// flap-regression check — with only name-match, that test fails.
pub(crate) fn find_wps_child<'a>(
    wps: &WorkerPoolSet,
    class_name: &str,
    pools: &'a [WorkerPool],
) -> ChildLookup<'a> {
    let child_name = format!("{}-{}", wps.name_any(), class_name);
    let wps_ns = wps.namespace().unwrap_or_default();
    match pools
        .iter()
        .find(|p| p.name_any() == child_name && p.namespace().as_deref() == Some(&wps_ns))
    {
        None => ChildLookup::NotCreated,
        // r[impl ctrl.wps.autoscale] — ownerRef gate after name-match.
        Some(child) if is_wps_owned(child) => ChildLookup::Found(child),
        Some(_) => ChildLookup::NameCollision,
    }
}

// r[verify ctrl.autoscale.direct-patch]
// r[verify ctrl.autoscale.separate-field-manager]
#[cfg(test)]
mod tests {
    use super::*;

    // ---- sts_replicas_patch: SSA body shape ----

    /// SSA patches MUST carry apiVersion + kind, or the apiserver
    /// returns 400 "apiVersion must be set". A body like
    /// `{"spec":{"replicas":N}}` silently fails every scale. This
    /// test is a tripwire: if someone strips the GVK fields "for
    /// brevity," this breaks.
    /// WorkerPool status patch needs apiVersion+kind (same SSA
    /// requirement as the STS replicas patch). Also verify it's a
    /// PARTIAL status: replicas/ready/desired absent (reconciler's
    /// fields), lastScaleTime + conditions present (ours).
    #[test]
    fn wp_status_patch_has_gvk_and_partial_status() {
        let cond = scaling_condition("True", "ScaledUp", "from 1 to 3");
        let patch = wp_status_patch(std::slice::from_ref(&cond));

        // GVK: SSA rejects without these.
        assert!(
            patch.get("apiVersion").and_then(|v| v.as_str()).is_some(),
            "SSA body without apiVersion → 400"
        );
        assert_eq!(
            patch.get("kind").and_then(|v| v.as_str()),
            Some("WorkerPool")
        );

        let status = patch.get("status").expect("status key");
        // Autoscaler's fields present.
        assert!(
            status.get("lastScaleTime").is_some(),
            "autoscaler owns this"
        );
        assert_eq!(
            status
                .get("conditions")
                .and_then(|c| c.as_array())
                .map(|a| a.len()),
            Some(1),
            "one Scaling condition"
        );
        // Reconciler's fields ABSENT — SSA field ownership split.
        // If we included these, our patch would fight the
        // reconciler's on every scale.
        assert!(
            status.get("replicas").is_none(),
            "reconciler owns replicas; our patch must not touch it"
        );
        assert!(status.get("desiredReplicas").is_none());
    }

    /// Scaling condition has the standard K8s Condition shape
    /// (type/status/reason/message/lastTransitionTime). kubectl
    /// describe reads these fields by convention.
    #[test]
    fn scaling_condition_has_standard_fields() {
        let c = scaling_condition("False", "UnknownMetric", "metric 'foo' unsupported");
        assert_eq!(c.get("type").and_then(|v| v.as_str()), Some("Scaling"));
        assert_eq!(c.get("status").and_then(|v| v.as_str()), Some("False"));
        assert_eq!(
            c.get("reason").and_then(|v| v.as_str()),
            Some("UnknownMetric")
        );
        assert!(c.get("message").is_some());
        // lastTransitionTime is an RFC3339 timestamp. Cheap check:
        // contains T and ends with Z (UTC). Full parse in k8s
        // apiserver; this just catches the obvious "we passed
        // unix-epoch-secs as a number" class of mistakes.
        let ts = c
            .get("lastTransitionTime")
            .and_then(|v| v.as_str())
            .expect("timestamp string");
        assert!(
            ts.contains('T') && ts.ends_with('Z'),
            "RFC3339 UTC format; got {ts}"
        );
    }

    #[test]
    fn sts_replicas_patch_has_gvk() {
        let patch = sts_replicas_patch(5);
        assert_eq!(
            patch.get("apiVersion").and_then(|v| v.as_str()),
            Some("apps/v1"),
            "SSA body without apiVersion → apiserver 400"
        );
        assert_eq!(
            patch.get("kind").and_then(|v| v.as_str()),
            Some("StatefulSet"),
            "SSA body without kind → apiserver 400"
        );
        assert_eq!(
            patch
                .get("spec")
                .and_then(|s| s.get("replicas"))
                .and_then(|r| r.as_i64()),
            Some(5),
            "the actual payload"
        );
    }

    // ---- compute_desired: pure arithmetic ----

    #[test]
    fn compute_desired_basic() {
        // 15 queued, target 5 per worker → need 3 workers.
        assert_eq!(compute_desired(15, 5, 1, 10), 3);
        // 16 queued, target 5 → ceil(16/5) = 4.
        assert_eq!(compute_desired(16, 5, 1, 10), 4);
    }

    #[test]
    fn compute_desired_clamps() {
        // 100 queued, target 5 → 20, but max=10.
        assert_eq!(compute_desired(100, 5, 1, 10), 10);
        // 0 queued → 0, but min=2.
        assert_eq!(compute_desired(0, 5, 2, 10), 2, "empty queue → min, not 0");
    }

    #[test]
    fn compute_desired_target_zero_clamped() {
        // target=0 would div-by-zero. Clamped to 1. 5 queued → 5.
        assert_eq!(compute_desired(5, 0, 1, 10), 5);
        // Negative target (shouldn't happen via CRD, but be safe).
        assert_eq!(compute_desired(5, -3, 1, 10), 5);
    }

    #[test]
    fn compute_desired_no_wrap_at_high_queue() {
        // queued > i32::MAX — pathological, but in u32 range.
        // A naive `raw as i32` wraps negative → .clamp(min,max)
        // returns min → autoscaler scales DOWN under extreme load.
        // Bounding to i32::MAX first makes it clamp to max.
        let queued = u32::MAX; // > 4 billion
        let got = compute_desired(queued, 1, 2, 100);
        assert_eq!(
            got, 100,
            "high queue must clamp to max, not wrap negative → min"
        );
    }

    /// min > max would PANIC in i32::clamp. Defensive swap prevents
    /// the autoscaler loop from dying silently on a pre-CEL CRD (or
    /// a --validate=false edit).
    #[test]
    fn compute_desired_swaps_min_greater_than_max() {
        // min=10, max=2 → swap to min=2, max=10. With queued=50,
        // target=5 → desired=10 (at max).
        assert_eq!(
            compute_desired(50, 5, 10, 2),
            10,
            "min>max swapped; clamped to original min (now max)"
        );
        // queued=0 → desired=0 → clamped to swapped min (original max=2).
        assert_eq!(
            compute_desired(0, 5, 10, 2),
            2,
            "queued=0 → swapped min (original max)"
        );
    }

    // ---- check_stabilization: timing logic ----
    //
    // We manually construct Instants (subtract durations from
    // now()) to avoid real sleeps. Tests use the DEFAULT timing
    // (30s up-window, 30s min-interval); production may use
    // different values via config but the LOGIC is the same.

    const T: ScalingTiming = ScalingTiming {
        poll_interval: Duration::from_secs(30),
        scale_up_window: Duration::from_secs(30),
        scale_down_window: Duration::from_secs(600),
        min_scale_interval: Duration::from_secs(30),
    };

    fn mk_state(initial: i32) -> ScaleState {
        ScaleState::new(initial, T.min_scale_interval)
    }

    /// Fresh state with a desired that differs from current →
    /// first call returns DesiredChanged (new desired seen, window
    /// reset). Second call with same desired and enough time
    /// elapsed → Patch.
    #[test]
    fn stabilization_window_before_patch() {
        let mut state = mk_state(2);
        // Manually age stable_since past the window. We do this
        // instead of sleeping because real sleeps in tests are
        // flaky (CI noisy neighbors).
        state.stable_since = Instant::now()
            .checked_sub(T.scale_up_window + Duration::from_secs(1))
            .unwrap();

        // First call: desired=5 is new (state has last_desired=2
        // from init). Reset.
        let d1 = check_stabilization(&mut state, 2, 5, T);
        assert!(
            matches!(d1, Decision::Wait(WaitReason::DesiredChanged)),
            "new desired → reset window"
        );
        // stable_since was just reset to now.

        // Second call, same desired, but window hasn't elapsed.
        let d2 = check_stabilization(&mut state, 2, 5, T);
        assert!(
            matches!(d2, Decision::Wait(WaitReason::Stabilizing)),
            "window not elapsed"
        );

        // Age it.
        state.stable_since = Instant::now()
            .checked_sub(T.scale_up_window + Duration::from_secs(1))
            .unwrap();
        let d3 = check_stabilization(&mut state, 2, 5, T);
        assert!(
            matches!(d3, Decision::Patch(Direction::Up)),
            "window elapsed, same desired → patch"
        );
    }

    #[test]
    fn stabilization_down_window_longer() {
        let mut state = mk_state(10);
        state.last_desired = 3; // as if we've seen 3 before

        // Age past T.scale_up_window but NOT T.scale_down_window.
        state.stable_since = Instant::now()
            .checked_sub(T.scale_up_window + Duration::from_secs(60))
            .unwrap();

        // desired=3 < current=10 → scale DOWN. up-window elapsed
        // but down-window (10 min) hasn't.
        let d = check_stabilization(&mut state, 10, 3, T);
        assert!(
            matches!(d, Decision::Wait(WaitReason::Stabilizing)),
            "scale-down needs the 10min window, not the 30s one"
        );

        // Age past down window.
        state.stable_since = Instant::now()
            .checked_sub(T.scale_down_window + Duration::from_secs(1))
            .unwrap();
        let d2 = check_stabilization(&mut state, 10, 3, T);
        assert!(matches!(d2, Decision::Patch(Direction::Down)));
    }

    #[test]
    fn stabilization_no_change() {
        let mut state = mk_state(5);
        state.last_desired = 5;
        let d = check_stabilization(&mut state, 5, 5, T);
        assert!(matches!(d, Decision::Wait(WaitReason::NoChange)));
    }

    #[test]
    fn stabilization_anti_flap() {
        let mut state = mk_state(2);
        state.last_desired = 5;
        // Window elapsed.
        state.stable_since = Instant::now()
            .checked_sub(T.scale_up_window + Duration::from_secs(1))
            .unwrap();
        // But last_patch was just now.
        state.last_patch = Instant::now();

        let d = check_stabilization(&mut state, 2, 5, T);
        assert!(
            matches!(d, Decision::Wait(WaitReason::AntiFlap)),
            "too soon since last patch, even though window elapsed"
        );
    }

    #[test]
    fn stabilization_desired_wobble_resets() {
        // desired changes 5 → 6 → 5. Each change resets. The
        // 5 → 6 → 5 round trip doesn't get to "5 was stable for
        // 30s" because the window reset when it hit 6.
        let mut state = mk_state(2);
        state.last_desired = 5;
        state.stable_since = Instant::now()
            .checked_sub(T.scale_up_window + Duration::from_secs(1))
            .unwrap();

        // Wobble to 6.
        let d = check_stabilization(&mut state, 2, 6, T);
        assert!(matches!(d, Decision::Wait(WaitReason::DesiredChanged)));
        assert_eq!(state.last_desired, 6);

        // Wobble back to 5. Resets AGAIN.
        let d = check_stabilization(&mut state, 2, 5, T);
        assert!(
            matches!(d, Decision::Wait(WaitReason::DesiredChanged)),
            "5 → 6 → 5 resets twice; 'stable' means no changes in window"
        );
    }

    // ---- WPS per-class autoscaler: SSA field manager + ownership ----

    // r[verify ctrl.wps.autoscale]
    /// `is_wps_owned` must return true for pools with a
    /// `controller=true` WorkerPoolSet owner reference, false
    /// otherwise. The distinction is LOAD-BEARING: a false
    /// positive makes standalone pools silently un-scaled; a
    /// false negative makes WPS children double-scaled (the
    /// standalone loop's cluster-wide depth fights the per-class
    /// loop's class-specific depth → flap).
    #[test]
    fn is_wps_owned_detects_controller_ownerref() {
        use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPoolSpec};
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;

        // Full WorkerPoolSpec — no Default derive (all required-in-
        // CEL fields must be explicit). Same shape as
        // workerpoolset/builders.rs build_child_workerpool output.
        let spec = WorkerPoolSpec {
            replicas: Replicas { min: 1, max: 10 },
            autoscaling: Autoscaling {
                metric: "queueDepth".into(),
                target_value: 5,
            },
            max_concurrent_builds: 4,
            fuse_cache_size: "50Gi".into(),
            systems: vec!["x86_64-linux".into()],
            size_class: "small".into(),
            image: "rio-worker:test".into(),
            features: vec![],
            ephemeral: false,
            resources: None,
            image_pull_policy: None,
            node_selector: None,
            tolerations: None,
            fuse_threads: None,
            bloom_expected_items: None,
            fuse_passthrough: None,
            daemon_timeout_secs: None,
            termination_grace_period_seconds: None,
            privileged: None,
            seccomp_profile: None,
            host_network: None,
            tls_secret_name: None,
            topology_spread: None,
            fod_proxy_url: None,
        };

        // Standalone pool: no owner reference at all.
        let standalone = WorkerPool::new("standalone-pool", spec.clone());
        assert!(
            !is_wps_owned(&standalone),
            "no ownerRef → NOT WPS-owned (standalone pools must be scaled by the cluster-wide loop)"
        );

        // WPS child: ownerRef kind=WorkerPoolSet, controller=true.
        // Mirrors what builders::build_child_workerpool sets via
        // controller_owner_ref(&()).
        let mut wps_child = WorkerPool::new("test-wps-small", spec.clone());
        wps_child.metadata.owner_references = Some(vec![OwnerReference {
            api_version: "rio.build/v1alpha1".into(),
            kind: "WorkerPoolSet".into(),
            name: "test-wps".into(),
            uid: "wps-uid-456".into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }]);
        assert!(
            is_wps_owned(&wps_child),
            "WorkerPoolSet ownerRef with controller=true → WPS-owned"
        );

        // Owned by something ELSE (e.g., a Helm chart adopting a
        // WorkerPool). controller=true but wrong kind.
        let mut other_owned = WorkerPool::new("other-owned", spec.clone());
        other_owned.metadata.owner_references = Some(vec![OwnerReference {
            api_version: "apps/v1".into(),
            kind: "Deployment".into(),
            name: "helm-release".into(),
            uid: "helm-uid".into(),
            controller: Some(true),
            block_owner_deletion: None,
        }]);
        assert!(
            !is_wps_owned(&other_owned),
            "non-WorkerPoolSet ownerRef → NOT WPS-owned (kind check is load-bearing)"
        );

        // WPS ownerRef but controller=None (garbage-collection
        // owner, not controller). The reconciler always sets
        // controller=true; this case shouldn't happen but
        // defensive: controller=None means NOT controller-owned.
        let mut gc_only = WorkerPool::new("gc-only", spec);
        gc_only.metadata.owner_references = Some(vec![OwnerReference {
            api_version: "rio.build/v1alpha1".into(),
            kind: "WorkerPoolSet".into(),
            name: "test-wps".into(),
            uid: "wps-uid-789".into(),
            controller: None,
            block_owner_deletion: None,
        }]);
        assert!(
            !is_wps_owned(&gc_only),
            "controller=None → NOT WPS-owned (GC-only owner doesn't skip cluster-wide scaling)"
        );
    }

    // ---- find_wps_child: name-match + ownerRef gate ----
    //
    // Test helpers for the find_wps_child cases. These mirror the
    // builders::tests::test_wps_with_classes pattern but live here
    // because that helper is in a private #[cfg(test)] mod inside
    // builders.rs (unreachable cross-module). Keeping the fixture
    // local avoids either (a) hoisting to rio-test-support (heavy
    // for a 3-test fixture) or (b) making builders' helper
    // pub(crate) (leaks test surface).

    fn test_wp_spec() -> crate::crds::workerpool::WorkerPoolSpec {
        use crate::crds::workerpool::{Autoscaling, Replicas, WorkerPoolSpec};
        // Full WorkerPoolSpec — no Default derive (all required-in-
        // CEL fields must be explicit). Same shape as
        // is_wps_owned_detects_controller_ownerref above — if that
        // fixture changes (new WorkerPoolSpec field), update both.
        WorkerPoolSpec {
            replicas: Replicas { min: 1, max: 10 },
            autoscaling: Autoscaling {
                metric: "queueDepth".into(),
                target_value: 5,
            },
            max_concurrent_builds: 4,
            fuse_cache_size: "50Gi".into(),
            systems: vec!["x86_64-linux".into()],
            size_class: "small".into(),
            image: "rio-worker:test".into(),
            features: vec![],
            ephemeral: false,
            resources: None,
            image_pull_policy: None,
            node_selector: None,
            tolerations: None,
            fuse_threads: None,
            fuse_passthrough: None,
            daemon_timeout_secs: None,
            termination_grace_period_seconds: None,
            privileged: None,
            seccomp_profile: None,
            host_network: None,
            tls_secret_name: None,
            topology_spread: None,
            fod_proxy_url: None,
        }
    }

    fn test_wps(name: &str, ns: &str, class_names: &[&str]) -> WorkerPoolSet {
        use crate::crds::workerpoolset::{PoolTemplate, SizeClassSpec, WorkerPoolSetSpec};
        use k8s_openapi::api::core::v1::ResourceRequirements;
        let classes = class_names
            .iter()
            .map(|n| SizeClassSpec {
                name: (*n).to_string(),
                cutoff_secs: 60.0,
                min_replicas: Some(1),
                max_replicas: Some(10),
                target_queue_per_replica: Some(5),
                resources: ResourceRequirements::default(),
            })
            .collect();
        let spec = WorkerPoolSetSpec {
            classes,
            pool_template: PoolTemplate {
                image: "rio-worker:test".into(),
                systems: vec!["x86_64-linux".into()],
                ..Default::default()
            },
            cutoff_learning: None,
        };
        let mut wps = WorkerPoolSet::new(name, spec);
        wps.metadata.uid = Some(format!("{name}-uid"));
        wps.metadata.namespace = Some(ns.into());
        wps
    }

    fn test_wp_in_ns(name: &str, ns: &str) -> WorkerPool {
        let mut wp = WorkerPool::new(name, test_wp_spec());
        wp.metadata.namespace = Some(ns.into());
        wp
    }

    fn with_wps_owner(mut wp: WorkerPool, wps_name: &str, wps_uid: &str) -> WorkerPool {
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::OwnerReference;
        wp.metadata.owner_references = Some(vec![OwnerReference {
            api_version: "rio.build/v1alpha1".into(),
            kind: "WorkerPoolSet".into(),
            name: wps_name.into(),
            uid: wps_uid.into(),
            controller: Some(true),
            block_owner_deletion: Some(true),
        }]);
        wp
    }

    /// A pool named `{wps}-{class}` but WITHOUT a WPS ownerRef must
    /// NOT be returned by `find_wps_child` — it's a name collision,
    /// not a WPS child. Without the is_wps_owned gate, both the
    /// standalone loop and the per-class loop would scale it → flap.
    ///
    /// This is the flap-prevention half of the asymmetric-keys bug.
    /// Load-bearing: `scale_wps_class` early-returns on
    /// `ChildLookup::NameCollision` (warn! + return), so no replica
    /// patch is issued. With only name-match (pre-P0374), this
    /// pool would be passed through to the STS-patch code → the
    /// per-class scaler fights the standalone scaler on the same
    /// `spec.replicas`.
    // r[verify ctrl.wps.autoscale]
    #[test]
    fn scale_wps_class_skips_name_collision_without_ownerref() {
        let wps = test_wps("prod", "rio", &["small"]);
        // Name matches `{wps}-{class}` shape, but NO owner_references
        // — a manually-created standalone pool that happens to
        // collide. `is_wps_owned` returns false → the standalone
        // loop scales it; the per-class loop must NOT.
        let colliding = test_wp_in_ns("prod-small", "rio");
        assert!(colliding.metadata.owner_references.is_none());

        match find_wps_child(&wps, "small", std::slice::from_ref(&colliding)) {
            ChildLookup::NameCollision => {} // expected — warn!, don't scale
            ChildLookup::Found(_) => panic!(
                "name-match pool without WPS ownerRef was returned as Found — \
                 would flap against standalone loop. is_wps_owned gate missing?"
            ),
            ChildLookup::NotCreated => panic!(
                "pool with matching name was classified NotCreated — \
                 name+ns match should be checked before ownerRef"
            ),
        }
    }

    /// Positive control for `find_wps_child`: a properly-owned
    /// child (name matches AND has WPS controller ownerRef) is
    /// returned as `Found`. This plus the NameCollision test above
    /// prove the two-key symmetry: name-match alone is insufficient,
    /// ownerRef alone is checked by `is_wps_owned` (standalone-loop
    /// skip), and BOTH together gate per-class scaling.
    #[test]
    fn find_wps_child_returns_found_for_owned_child() {
        let wps = test_wps("prod", "rio", &["small", "large"]);
        let small = with_wps_owner(test_wp_in_ns("prod-small", "rio"), "prod", "prod-uid");
        let large = with_wps_owner(test_wp_in_ns("prod-large", "rio"), "prod", "prod-uid");
        // A standalone pool in the same namespace — must not
        // interfere with the lookup (name doesn't match).
        let standalone = test_wp_in_ns("unrelated-pool", "rio");
        let pools = [small, large, standalone];

        // "small" → Found(prod-small).
        match find_wps_child(&wps, "small", &pools) {
            ChildLookup::Found(c) => assert_eq!(c.name_any(), "prod-small"),
            other => panic!("expected Found for owned child, got {other:?}"),
        }

        // Nonexistent class → NotCreated.
        match find_wps_child(&wps, "xlarge", &pools) {
            ChildLookup::NotCreated => {}
            other => panic!("expected NotCreated for missing child, got {other:?}"),
        }
    }

    /// Namespace scoping: a pool matching the name but in a
    /// DIFFERENT namespace is `NotCreated`, not `NameCollision`
    /// or `Found`. Pool names are namespace-scoped; a `prod-small`
    /// in `rio-dev` is not the `prod` WPS's child in `rio-prod`.
    #[test]
    fn find_wps_child_respects_namespace() {
        let wps = test_wps("prod", "rio-prod", &["small"]);
        // Same name, WRONG namespace. Even with a matching ownerRef
        // this shouldn't be found — namespaces are boundaries.
        let wrong_ns = with_wps_owner(test_wp_in_ns("prod-small", "rio-dev"), "prod", "prod-uid");

        match find_wps_child(&wps, "small", std::slice::from_ref(&wrong_ns)) {
            ChildLookup::NotCreated => {} // expected — no name+ns match
            other => panic!(
                "cross-namespace pool should be NotCreated (namespaces are boundaries), got {other:?}"
            ),
        }
    }

    // r[verify ctrl.wps.autoscale]
    /// The WPS autoscaler SSA-patches STS replicas with field
    /// manager `rio-controller-wps-autoscaler` — distinct from
    /// `rio-controller-autoscaler` (standalone pools) and
    /// `rio-controller` (WorkerPool reconciler). SSA tracks
    /// `managedFields` per manager; the apiserver uses this to
    /// merge ownership. The unit-test-level proof that SSA is
    /// engaged: `fieldManager=...` appears in the PATCH query
    /// string (merge-patch doesn't use that param). The
    /// end-to-end proof (actual `.metadata.managedFields` entry
    /// on the apiserver) is P0239's VM lifecycle test — a mock
    /// apiserver can't track managedFields.
    ///
    /// The patch body is the same `sts_replicas_patch` as the
    /// standalone autoscaler; the GVK assertion in
    /// `sts_replicas_patch_has_gvk` covers body shape. THIS
    /// test proves the DISTINCT field manager + the query-string
    /// that engages SSA.
    #[tokio::test]
    async fn wps_autoscaler_writes_via_ssa_field_manager() {
        use crate::fixtures::{ApiServerVerifier, Scenario};

        let (client, verifier) = ApiServerVerifier::new();

        // Expect a single PATCH to the child STS with the WPS
        // autoscaler's field manager in the query string. kube-rs
        // emits `force=true&fieldManager=...` (order stable since
        // PatchParams serde is struct-field order). The substring
        // match proves both: SSA engaged (fieldManager param —
        // merge-patch doesn't use it) AND force (last-write-wins
        // instead of conflict-abort on manager overlap).
        let guard = verifier.run(vec![Scenario::ok(
            http::Method::PATCH,
            "force=true&fieldManager=rio-controller-wps-autoscaler",
            serde_json::json!({
                "apiVersion": "apps/v1",
                "kind": "StatefulSet",
                "metadata": { "name": "test-wps-small-workers", "namespace": "rio" },
                "spec": { "replicas": 4 },
            })
            .to_string(),
        )]);

        let sts_api: Api<StatefulSet> = Api::namespaced(client, "rio");

        // Reuse the pure patch builder. The field manager is in
        // PatchParams, NOT the body — both halves must be right.
        let patch = sts_replicas_patch(4);
        sts_api
            .patch(
                "test-wps-small-workers",
                &PatchParams::apply(WPS_AUTOSCALER_MANAGER).force(),
                &Patch::Apply(&patch),
            )
            .await
            .expect("patch succeeds");

        // Proves the PATCH had the expected query string. The
        // verifier panics on mismatch (method/path), or the
        // outer 5s timeout fires if the call was never made.
        guard.verified().await;

        // Body-shape proof: apiVersion + kind MANDATORY for SSA.
        // Without these, the apiserver returns 400 and no
        // managedFields entry would ever be written.
        assert_eq!(
            patch.get("apiVersion").and_then(|v| v.as_str()),
            Some("apps/v1"),
            "SSA body without apiVersion → 400, no managedFields entry"
        );
        assert_eq!(
            patch.get("kind").and_then(|v| v.as_str()),
            Some("StatefulSet"),
            "SSA body without kind → 400"
        );
    }

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
