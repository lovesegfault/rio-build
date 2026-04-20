//! Heartbeat construction and the background heartbeat loop.
//!
//! `build_heartbeat_request` assembles capability/resource data for the
//! scheduler; `spawn_heartbeat` drives the 10s tick. The generation
//! fence (`r[sched.lease.generation-fence]`) is updated from the
//! response.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64};
use std::time::Duration;

use rio_proto::types::{HeartbeatRequest, HeartbeatResponse};

use super::setup::WorkerClient;
use super::slot::BuildSlot;
use crate::IgnorePoison;
use crate::cgroup::ResourceSnapshotHandle;

/// Heartbeat interval. Shared source of truth with the scheduler's timeout
/// check (`rio_common::limits::HEARTBEAT_TIMEOUT_SECS` derives from this).
pub(super) const HEARTBEAT_INTERVAL: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS);

/// Per-RPC timeout, strictly < `HEARTBEAT_INTERVAL` so one slow RPC can
/// never consume more than one missed-heartbeat budget. 2s slack leaves
/// room for the request-build + apply between tick and RPC. bug_044:
/// the loop is sequential (`tick → await RPC → apply`); without this,
/// one RPC stalled >30s (scheduler actor mpsc backed up at
/// `send_unchecked`, or asymmetric network delay on a live connection)
/// blocked the whole loop past `HEARTBEAT_TIMEOUT_SECS=30` →
/// `tick_check_heartbeats` reaped a healthy worker. h2 keepalive (~40s
/// detection) doesn't help — it detects dead transports, not slow
/// application handlers.
// r[impl builder.heartbeat.rpc-timeout]
pub(super) const HEARTBEAT_RPC_TIMEOUT: Duration =
    Duration::from_secs(rio_common::limits::HEARTBEAT_INTERVAL_SECS - 2);

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
    pub(super) executor_token: String,
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
        executor_token,
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
        // bug_044: after an 8s RPC, default `Burst` would fire the
        // missed tick immediately (two heartbeats back-to-back).
        // `Delay` resumes the regular cadence — the scheduler's reap
        // check is wall-clock, so bursting buys nothing.
        interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
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

            // r[impl sec.executor.identity-token]
            // Attach the scheduler-signed token so the heartbeat's
            // body `intent_id` is bound to the HMAC-attested one.
            // Empty in dev mode → header omitted → scheduler
            // permissive (no key configured either).
            let mut request = tonic::Request::new(request);
            if !executor_token.is_empty() {
                let _ = rio_common::grpc::inject_metadata(
                    request.metadata_mut(),
                    &[(rio_proto::EXECUTOR_TOKEN_HEADER, &executor_token)],
                );
            }

            apply_heartbeat_response(
                rio_common::grpc::with_timeout_status(
                    "Heartbeat",
                    HEARTBEAT_RPC_TIMEOUT,
                    client.heartbeat(request),
                )
                .await
                .map(|r| r.into_inner()),
                &ready,
                &generation,
            );
        }
    })
}

/// Apply a heartbeat RPC result to the shared `ready` / `generation`
/// atomics. Extracted from `spawn_heartbeat`'s loop body so the
/// generation-fence WRITE side (`fetch_max`) is unit-testable without
/// a mock gRPC client — bug_417: the previous test
/// (`heartbeat_gen_monotone_under_interleaving`) called `fetch_max` on
/// a local `AtomicU64` and never reached this code; mutating
/// `fetch_max → store` here passed `nextest -p rio-builder`.
pub(super) fn apply_heartbeat_response(
    resp: Result<HeartbeatResponse, tonic::Status>,
    ready: &AtomicBool,
    generation: &AtomicU64,
) {
    match resp {
        Ok(resp) => {
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

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering;

    /// bug_044: one slow RPC must not consume >1 missed-heartbeat
    /// budget. The loop is `tick → await RPC → apply`; with the RPC
    /// bounded under the interval, a stalled scheduler handler costs
    /// at most one tick on the scheduler's `now - last_heartbeat`
    /// clock instead of all `MAX_MISSED_HEARTBEATS`.
    // r[verify builder.heartbeat.rpc-timeout]
    #[test]
    fn heartbeat_rpc_timeout_under_interval() {
        assert!(
            HEARTBEAT_RPC_TIMEOUT < HEARTBEAT_INTERVAL,
            "RPC timeout {HEARTBEAT_RPC_TIMEOUT:?} must be < interval \
             {HEARTBEAT_INTERVAL:?} so one slow RPC ≤ one missed budget"
        );
        assert!(
            HEARTBEAT_RPC_TIMEOUT >= Duration::from_secs(1),
            "RPC timeout {HEARTBEAT_RPC_TIMEOUT:?} must allow real RTT"
        );
    }

    /// `with_timeout_status` returns `Status::deadline_exceeded` on
    /// elapse; this is the path the loop takes when bug_044's bound
    /// fires. Pin that the existing `Err` arm handles it (sets
    /// `ready=false`, doesn't touch `generation`).
    #[test]
    fn apply_heartbeat_response_timeout_sets_unready() {
        let ready = AtomicBool::new(true);
        let generation = AtomicU64::new(7);
        apply_heartbeat_response(
            Err(tonic::Status::deadline_exceeded(
                "'Heartbeat' timed out after 8s",
            )),
            &ready,
            &generation,
        );
        assert!(!ready.load(Ordering::Relaxed));
        assert_eq!(generation.load(Ordering::Relaxed), 7);
    }
}
