//! Autoscaling loop: poll `ClusterStatus`, patch StatefulSet replicas.
//!
//! Runs separately from the reconciler (spawned in main.rs as its
//! own task). The reconciler ensures the StatefulSet EXISTS with
//! the right shape; the autoscaler adjusts `spec.replicas` within
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
use kube::{Client, ResourceExt};
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use rio_proto::AdminServiceClient;

use crate::crds::workerpool::WorkerPool;

/// Stabilization window for scale-up. Desired must be stable (same
/// value) for this long before we patch. Short: queue depth is a
/// high-confidence signal, react fast.
const SCALE_UP_WINDOW: Duration = Duration::from_secs(30);

/// Stabilization window for scale-down. Long: avoid killing
/// workers right before the next burst. 10 min is the K8s HPA
/// default `--horizontal-pod-autoscaler-downscale-stabilization`;
/// we follow that convention.
const SCALE_DOWN_WINDOW: Duration = Duration::from_secs(600);

/// Minimum interval between patches. Anti-flap.
const MIN_SCALE_INTERVAL: Duration = Duration::from_secs(30);

/// Poll ClusterStatus this often. Matches the stabilization
/// granularity — polling faster wouldn't help (decisions need 30s
/// windows anyway).
const POLL_INTERVAL: Duration = Duration::from_secs(30);

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
    fn new(initial: i32) -> Self {
        let now = Instant::now();
        Self {
            last_desired: initial,
            stable_since: now,
            // Initialize to "long ago" so the first patch isn't
            // anti-flap-blocked. checked_sub → None on underflow;
            // unwrap_or(now) is a no-op on the FIRST iteration
            // since stable_since hasn't elapsed anyway.
            last_patch: now.checked_sub(MIN_SCALE_INTERVAL * 2).unwrap_or(now),
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
}

impl Autoscaler {
    pub fn new(client: Client, scheduler: AdminServiceClient<Channel>) -> Self {
        Self {
            client,
            scheduler,
            states: HashMap::new(),
        }
    }

    /// Main loop. Never returns (barring panic). main.rs spawns
    /// this via `spawn_monitored` — if it dies, logged, controller
    /// keeps reconciling (just without autoscale).
    pub async fn run(mut self) {
        let mut interval = tokio::time::interval(POLL_INTERVAL);
        // MissedTickBehavior::Skip: if one iteration takes >30s
        // (slow apiserver), don't fire twice immediately after.
        // Catch up on the NEXT normal tick.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

        loop {
            interval.tick().await;
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
        // One ClusterStatus call for ALL pools. The scheduler
        // returns cluster-wide `queued_derivations`; we divide by
        // active_workers for the scale signal. Per-pool queue
        // depth would be more precise (small-class queue vs
        // large-class) but that's
        // `rio_scheduler_class_queue_depth` metrics, not a gRPC
        // field — phase4's WorkerPoolSet wires that.
        let status = self
            .scheduler
            .cluster_status(())
            .await
            .map(|r| r.into_inner())?;

        // ---- List all WorkerPools ----
        let pools_api: Api<WorkerPool> = Api::all(self.client.clone());
        let pools = pools_api.list(&Default::default()).await?.items;

        // ---- Prune state for deleted pools ----
        // Pools gone from the list → remove their ScaleState.
        // Without this, the map grows unboundedly over pool
        // create/delete cycles (small leak, but still).
        let live_keys: std::collections::HashSet<String> = pools.iter().map(pool_key).collect();
        self.states.retain(|k, _| live_keys.contains(k));

        // ---- Compute + maybe patch each pool ----
        for pool in &pools {
            self.scale_one(pool, &status).await;
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
    async fn scale_one(
        &mut self,
        pool: &WorkerPool,
        status: &rio_proto::types::ClusterStatusResponse,
    ) {
        let key = pool_key(pool);
        let ns = pool.namespace().unwrap_or_default();
        let sts_name = format!("{}-workers", pool.name_any());

        // ---- Compute desired ----
        let desired = compute_desired(
            status.queued_derivations,
            status.active_workers,
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
                return;
            }
            Err(e) => {
                warn!(pool = %key, error = %e, "failed to read StatefulSet");
                return;
            }
        };

        // ---- Stabilization check ----
        let state = self
            .states
            .entry(key.clone())
            .or_insert_with(|| ScaleState::new(current));

        let decision = check_stabilization(state, current, desired);

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
    }
}

/// Build the SSA patch body for `StatefulSet.spec.replicas`.
///
/// apiVersion + kind are MANDATORY in SSA bodies: without them
/// the apiserver returns 400 "apiVersion must be set". Same
/// pattern as workerpool.rs's status patch and build.rs's
/// patch_status — but previously forgotten here. vm-phase3a
/// used `kubectl scale` directly, bypassing this path, so the
/// missing fields were never caught against a real apiserver.
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
/// `active_workers` is NOT directly in the formula — we use
/// queued/target, not queued/active. Why: if active=0 (all
/// workers crashed), we still want to scale up based on queue.
/// queued/active would divide by zero. active_workers is logged
/// for observability.
///
/// Edge: `target=0` would divide-by-zero. CRD doesn't enforce
/// `>0` (the CEL is on max_concurrent_builds, not target_value).
/// We clamp target to 1 here — target=0 is operator error ("scale
/// up on ANY queue") which clamping to 1 approximates.
///
/// Edge: `queued=0` → desired=0 → clamped to min. Correct: empty
/// queue means scale DOWN to min, not to zero.
pub(crate) fn compute_desired(queued: u32, _active: u32, target: i32, min: i32, max: i32) -> i32 {
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
fn check_stabilization(state: &mut ScaleState, current: i32, desired: i32) -> Decision {
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

    // Direction + window.
    let (direction, window) = if desired > current {
        (Direction::Up, SCALE_UP_WINDOW)
    } else {
        (Direction::Down, SCALE_DOWN_WINDOW)
    };

    if now.duration_since(state.stable_since) < window {
        return Decision::Wait(WaitReason::Stabilizing);
    }

    // Anti-flap. Prevents oscillation when desired wobbles
    // across a boundary (queue at exactly target*N).
    if now.duration_since(state.last_patch) < MIN_SCALE_INTERVAL {
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

#[cfg(test)]
mod tests {
    use super::*;

    // ---- sts_replicas_patch: SSA body shape ----

    /// SSA patches MUST carry apiVersion + kind, or the apiserver
    /// returns 400 "apiVersion must be set". Previously the
    /// autoscaler sent `{"spec":{"replicas":N}}` and silently
    /// failed every scale (vm-phase3a used `kubectl scale` direct,
    /// bypassing this path). This test is a tripwire: if someone
    /// strips the GVK fields "for brevity," this breaks.
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
        assert_eq!(compute_desired(15, 0, 5, 1, 10), 3);
        // 16 queued, target 5 → ceil(16/5) = 4.
        assert_eq!(compute_desired(16, 0, 5, 1, 10), 4);
    }

    #[test]
    fn compute_desired_clamps() {
        // 100 queued, target 5 → 20, but max=10.
        assert_eq!(compute_desired(100, 0, 5, 1, 10), 10);
        // 0 queued → 0, but min=2.
        assert_eq!(
            compute_desired(0, 0, 5, 2, 10),
            2,
            "empty queue → min, not 0"
        );
    }

    #[test]
    fn compute_desired_target_zero_clamped() {
        // target=0 would div-by-zero. Clamped to 1. 5 queued → 5.
        assert_eq!(compute_desired(5, 0, 0, 1, 10), 5);
        // Negative target (shouldn't happen via CRD, but be safe).
        assert_eq!(compute_desired(5, 0, -3, 1, 10), 5);
    }

    #[test]
    fn compute_desired_no_wrap_at_high_queue() {
        // queued > i32::MAX — pathological, but in u32 range.
        // Previously: raw as i32 wrapped negative → .clamp(min,max)
        // returned min → autoscaler scaled DOWN under extreme load.
        // Now: bounded to i32::MAX first, clamps to max.
        let queued = u32::MAX; // > 4 billion
        let got = compute_desired(queued, 0, 1, 2, 100);
        assert_eq!(
            got, 100,
            "high queue must clamp to max, not wrap negative → min"
        );
    }

    #[test]
    fn compute_desired_ignores_active() {
        // active=0 (all crashed) doesn't break the formula. We
        // scale on queued/target, not queued/active.
        assert_eq!(compute_desired(15, 0, 5, 1, 10), 3);
        assert_eq!(
            compute_desired(15, 100, 5, 1, 10),
            3,
            "active is logged-only"
        );
    }

    // ---- check_stabilization: timing logic ----
    //
    // These use tokio::time::pause + advance so SCALE_UP_WINDOW
    // (30s) doesn't make the test take 30s. Instant::now() in the
    // prod code uses the real clock — but wait, that's std::time,
    // not tokio::time. tokio::time::pause doesn't affect
    // std::time::Instant.
    //
    // Actually: we can test the logic with MANUALLY constructed
    // Instants. `Instant` has no public constructor but we can
    // get one via `Instant::now()` and subtract/add durations.

    /// Fresh state with a desired that differs from current →
    /// first call returns DesiredChanged (new desired seen, window
    /// reset). Second call with same desired and enough time
    /// elapsed → Patch.
    #[test]
    fn stabilization_window_before_patch() {
        let mut state = ScaleState::new(2);
        // Manually age stable_since past the window. We do this
        // instead of sleeping because real sleeps in tests are
        // flaky (CI noisy neighbors).
        state.stable_since = Instant::now()
            .checked_sub(SCALE_UP_WINDOW + Duration::from_secs(1))
            .unwrap();

        // First call: desired=5 is new (state has last_desired=2
        // from init). Reset.
        let d1 = check_stabilization(&mut state, 2, 5);
        assert!(
            matches!(d1, Decision::Wait(WaitReason::DesiredChanged)),
            "new desired → reset window"
        );
        // stable_since was just reset to now.

        // Second call, same desired, but window hasn't elapsed.
        let d2 = check_stabilization(&mut state, 2, 5);
        assert!(
            matches!(d2, Decision::Wait(WaitReason::Stabilizing)),
            "window not elapsed"
        );

        // Age it.
        state.stable_since = Instant::now()
            .checked_sub(SCALE_UP_WINDOW + Duration::from_secs(1))
            .unwrap();
        let d3 = check_stabilization(&mut state, 2, 5);
        assert!(
            matches!(d3, Decision::Patch(Direction::Up)),
            "window elapsed, same desired → patch"
        );
    }

    #[test]
    fn stabilization_down_window_longer() {
        let mut state = ScaleState::new(10);
        state.last_desired = 3; // as if we've seen 3 before

        // Age past SCALE_UP_WINDOW but NOT SCALE_DOWN_WINDOW.
        state.stable_since = Instant::now()
            .checked_sub(SCALE_UP_WINDOW + Duration::from_secs(60))
            .unwrap();

        // desired=3 < current=10 → scale DOWN. up-window elapsed
        // but down-window (10 min) hasn't.
        let d = check_stabilization(&mut state, 10, 3);
        assert!(
            matches!(d, Decision::Wait(WaitReason::Stabilizing)),
            "scale-down needs the 10min window, not the 30s one"
        );

        // Age past down window.
        state.stable_since = Instant::now()
            .checked_sub(SCALE_DOWN_WINDOW + Duration::from_secs(1))
            .unwrap();
        let d2 = check_stabilization(&mut state, 10, 3);
        assert!(matches!(d2, Decision::Patch(Direction::Down)));
    }

    #[test]
    fn stabilization_no_change() {
        let mut state = ScaleState::new(5);
        state.last_desired = 5;
        let d = check_stabilization(&mut state, 5, 5);
        assert!(matches!(d, Decision::Wait(WaitReason::NoChange)));
    }

    #[test]
    fn stabilization_anti_flap() {
        let mut state = ScaleState::new(2);
        state.last_desired = 5;
        // Window elapsed.
        state.stable_since = Instant::now()
            .checked_sub(SCALE_UP_WINDOW + Duration::from_secs(1))
            .unwrap();
        // But last_patch was just now.
        state.last_patch = Instant::now();

        let d = check_stabilization(&mut state, 2, 5);
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
        let mut state = ScaleState::new(2);
        state.last_desired = 5;
        state.stable_since = Instant::now()
            .checked_sub(SCALE_UP_WINDOW + Duration::from_secs(1))
            .unwrap();

        // Wobble to 6.
        let d = check_stabilization(&mut state, 2, 6);
        assert!(matches!(d, Decision::Wait(WaitReason::DesiredChanged)));
        assert_eq!(state.last_desired, 6);

        // Wobble back to 5. Resets AGAIN.
        let d = check_stabilization(&mut state, 2, 5);
        assert!(
            matches!(d, Decision::Wait(WaitReason::DesiredChanged)),
            "5 → 6 → 5 resets twice; 'stable' means no changes in window"
        );
    }
}
