//! Heartbeat construction and the background heartbeat loop.
//!
//! `build_heartbeat_request` assembles capability/resource data for the
//! scheduler; `spawn_heartbeat` drives the 10s tick. The generation
//! fence (`r[sched.lease.generation-fence]`) is updated from the
//! response.

use std::sync::Arc;
use std::time::Duration;

use rio_proto::types::HeartbeatRequest;

use super::setup::WorkerClient;
use super::slot::BuildSlot;
use crate::IgnorePoison;
use crate::cgroup::ResourceSnapshotHandle;

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (`rio_common::limits::HEARTBEAT_TIMEOUT_SECS` derives from this).
pub(super) const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

/// Build a heartbeat request, populating `running_build` from the shared
/// tracker.
///
/// Extracted for testability — the heartbeat loop in main.rs calls this.
///
/// `systems` + `features`: slice refs to the worker's static config.
/// Both are `.to_vec()`'d into the proto — a heartbeat every 10s
/// means ~100 allocs/min for typically 1-3 elements; not worth the
/// lifetime-threading to avoid.
#[allow(clippy::too_many_arguments)]
pub async fn build_heartbeat_request(
    executor_id: &str,
    executor_kind: rio_proto::types::ExecutorKind,
    systems: &[String],
    features: &[String],
    intent_id: &str,
    slot: &BuildSlot,
    resources: &ResourceSnapshotHandle,
    store_degraded: bool,
    draining: bool,
) -> HeartbeatRequest {
    let current = slot.running();

    // Read lock held for one struct clone. First heartbeat (before
    // first 10s poll) sends zeros — same as ResourceUsage::default(),
    // converges after one tick.
    let resources = *resources.read().ignore_poison();

    HeartbeatRequest {
        executor_id: executor_id.to_string(),
        running_build: current,
        resources: Some(resources),
        systems: systems.to_vec(),
        supported_features: features.to_vec(),
        // r[impl builder.heartbeat.store-degraded]
        // Reflects CircuitBreaker::is_open() — main.rs reads the
        // breaker each heartbeat tick and passes it here. Scheduler
        // treats this like `draining`: has_capacity() returns false
        // while the FUSE fetch circuit is open (store unreachable
        // or degraded). Half-open counts as NOT degraded (the probe
        // is in flight, let it decide).
        store_degraded,
        // I-063: the worker is the authority on its own drain state.
        // main.rs flips this on first SIGTERM and keeps the stream
        // alive for completion reports; scheduler trusts this over
        // DrainExecutor RPC or reconnect inference.
        draining,
        // Builder or Fetcher — from `RIO_EXECUTOR_KIND` config.
        // Scheduler routes FODs to fetchers only per
        // spec sched.dispatch.fod-to-fetcher.
        kind: executor_kind as i32,
        // ADR-023: SpawnIntent match key from `RIO_INTENT_ID`
        // (downward-API pod annotation). Empty = Static-sized pod;
        // scheduler maps to None and falls through to pick-from-queue.
        intent_id: intent_id.to_string(),
    }
}

/// Inputs to the heartbeat loop. Grouped so main() doesn't grow 12
/// `let heartbeat_* = ...` prelude lines before the spawn.
pub(super) struct HeartbeatCtx {
    pub(super) executor_id: String,
    pub(super) executor_kind: rio_proto::types::ExecutorKind,
    pub(super) systems: Vec<String>,
    pub(super) features: Vec<String>,
    pub(super) intent_id: String,
    pub(super) slot: Arc<BuildSlot>,
    pub(super) ready: Arc<std::sync::atomic::AtomicBool>,
    pub(super) resources: crate::cgroup::ResourceSnapshotHandle,
    pub(super) circuit: Arc<crate::fuse::circuit::CircuitBreaker>,
    pub(super) draining: Arc<std::sync::atomic::AtomicBool>,
    pub(super) generation: Arc<std::sync::atomic::AtomicU64>,
    pub(super) client: WorkerClient,
}

/// Spawn the heartbeat loop. A panicking heartbeat loop leaves the
/// worker silently alive but unreachable from the scheduler's
/// perspective — the scheduler times it out and re-dispatches its
/// builds to another worker, leading to duplicate builds. Wrap in
/// spawn_monitored so panics are logged; main() checks
/// `handle.is_finished()` each select-loop iteration.
pub(super) fn spawn_heartbeat(ctx: HeartbeatCtx) -> tokio::task::JoinHandle<()> {
    let HeartbeatCtx {
        executor_id,
        executor_kind,
        systems,
        features,
        intent_id,
        slot,
        ready,
        resources,
        circuit,
        draining,
        generation,
        mut client,
    } = ctx;
    rio_common::task::spawn_monitored("heartbeat-loop", async move {
        let mut interval = tokio::time::interval(HEARTBEAT_INTERVAL);
        loop {
            interval.tick().await;

            let request = build_heartbeat_request(
                &executor_id,
                executor_kind,
                &systems,
                &features,
                &intent_id,
                &slot,
                &resources,
                circuit.is_open(),
                draining.load(std::sync::atomic::Ordering::Relaxed),
            )
            .await;

            match client.heartbeat(request).await {
                Ok(response) => {
                    let resp = response.into_inner();
                    if resp.accepted {
                        // READY. Set unconditionally — it's idempotent
                        // (already-true → true is a no-op at the atomic
                        // level) and cheaper than a load-then-store.
                        ready.store(true, std::sync::atomic::Ordering::Relaxed);
                        // r[impl sched.lease.generation-fence]
                        // fetch_max, not store: during the 15s Lease TTL
                        // split-brain window (r[sched.lease.k8s-lease]),
                        // both old and new leader answer with accepted=true.
                        // If responses interleave new-then-old, `store`
                        // would REGRESS the fence and let through exactly
                        // the stale assignment we're blocking. fetch_max
                        // is monotone regardless of response ordering.
                        generation.fetch_max(resp.generation, std::sync::atomic::Ordering::Relaxed);
                    } else {
                        tracing::warn!("heartbeat rejected by scheduler");
                        // NOT READY: scheduler is reachable but rejecting
                        // us. Could mean the scheduler doesn't recognize
                        // our executor_id (stale registration, scheduler
                        // restarted and lost in-memory state). The
                        // BuildExecution stream reconnect logic handles
                        // the actual recovery; readiness flag just
                        // reflects the current state.
                        ready.store(false, std::sync::atomic::Ordering::Relaxed);
                    }
                }
                Err(e) => {
                    tracing::warn!(error = %e, "heartbeat failed");
                    // NOT READY: gRPC error. Scheduler unreachable or
                    // overloaded. Don't flip liveness (restarting won't
                    // fix the network) but do flip readiness. The next
                    // successful heartbeat flips back — this tracks
                    // the scheduler's availability from our perspective.
                    ready.store(false, std::sync::atomic::Ordering::Relaxed);
                }
            }
        }
    })
}
