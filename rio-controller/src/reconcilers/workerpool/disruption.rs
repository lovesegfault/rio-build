//! DisruptionTarget watcher: pre-empt builds on evicting workers.
//!
//! K8s sets `status.conditions[type=DisruptionTarget, status=True]`
//! when eviction is imminent (node drain, spot interrupt, PDB-
//! mediated disruption). We fire `DrainWorker{force:true}` → the
//! scheduler iterates `running_builds`, sends `CancelSignal` per
//! build → worker `cgroup.kill()`s → builds reassign in seconds
//! instead of burning the 2h `terminationGracePeriodSeconds`.
//!
//! This is what the four pre-existing comments at
//! `rio-scheduler/src/actor/worker.rs:220`, `actor/tests/worker.rs:345`,
//! `builders.rs:171`, and `workerpool/mod.rs:204` have been asserting
//! — without a production caller until now.
//!
//! # Why a Pod watcher, not a preStop hook
//!
//! `preStop` fires on EVERY termination, including graceful scale-
//! down where `force=false` is correct (let in-flight builds finish).
//! `DisruptionTarget` fires only on eviction-budget-mediated
//! disruption (node drain, spot interrupt, Karpenter consolidation)
//! where preemption IS the right call: the pod is dying regardless,
//! so reassigning in-flight builds to healthy workers is strictly
//! better than letting them burn 2h of wall-clock and then SIGKILL.
//!
//! # Idempotence
//!
//! `DisruptionTarget` stays True for the pod's remaining lifetime
//! (the condition is sticky until pod termination). Every watcher
//! event for that pod fires another `DrainWorker{force:true}`. The
//! scheduler's `handle_drain_worker` is idempotent: `force=true`
//! with `draining=true` re-preempts, which is a no-op on an
//! already-empty `running_builds`. No client-side dedup needed.

// r[impl ctrl.drain.disruption-target]

use futures_util::StreamExt;
use k8s_openapi::api::core::v1::Pod;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use rio_proto::AdminServiceClient;
use tonic::transport::Channel;
use tracing::{debug, info, warn};

use super::POOL_LABEL;

/// Run the watcher. Returns on `shutdown.cancelled()` or if the
/// watch stream ends (never — `default_backoff()` retries
/// connection failures indefinitely, and `watcher()` re-lists on
/// desync).
///
/// `spawn_monitored("disruption-watcher", run(...))` from main.rs.
/// Panics are logged; the controller keeps reconciling (workers
/// just lose the fast-preemption path — SIGTERM self-drain is the
/// fallback).
pub async fn run(
    client: Client,
    mut admin: AdminServiceClient<Channel>,
    shutdown: rio_common::signal::Token,
) {
    // All-namespaces: WorkerPool is namespaced, so pods can be in
    // any ns. Label selector filters to OUR pods at the apiserver
    // (not client-side) — cheap.
    let pods: Api<Pod> = Api::all(client);
    let cfg = watcher::Config::default().labels(POOL_LABEL);

    // applied_objects(): emits the Pod on Add/Modify. Delete
    // events are dropped — we don't care, the pod is already gone.
    // default_backoff(): exponential retry on watch connection
    // loss (apiserver restart, network blip). Without it, a single
    // watch error would terminate the stream and this task.
    let mut stream = watcher(pods, cfg)
        .default_backoff()
        .applied_objects()
        .boxed();

    info!("DisruptionTarget watcher started (label={POOL_LABEL})");

    loop {
        let pod = tokio::select! {
            biased;
            _ = shutdown.cancelled() => {
                debug!("DisruptionTarget watcher: shutdown");
                return;
            }
            next = stream.next() => match next {
                Some(Ok(pod)) => pod,
                Some(Err(e)) => {
                    // default_backoff handles retries, but errors
                    // still surface (e.g. watch init 403 on RBAC
                    // misconfig). Log + continue — backoff will
                    // retry and next iteration sees the same err
                    // until fixed.
                    warn!(error = %e, "DisruptionTarget watcher: stream error");
                    continue;
                }
                None => {
                    // Stream ended. Shouldn't happen with
                    // default_backoff (it retries indefinitely).
                    // If it does (kube-runtime bug?), log + exit
                    // the task — spawn_monitored notices, and the
                    // SIGTERM fallback covers eviction.
                    warn!("DisruptionTarget watcher: stream ended (unexpected)");
                    return;
                }
            },
        };

        // Filter: DisruptionTarget=True? Pure function for
        // unit-testability — see tests.rs::disruption_filter_*.
        let Some(worker_id) = is_disruption_target(&pod) else {
            continue;
        };

        // Best-effort. `force=true` triggers the preemption block
        // at `rio-scheduler/src/actor/worker.rs:211-258`: drain
        // running_builds → CancelSignal each → reassign.
        //
        // Failure modes:
        //   - Scheduler down → tonic ConnectError. Worker's own
        //     SIGTERM handler also calls DrainWorker (force=false)
        //     via its direct channel, so no-drain is only as bad
        //     as "no preemption" (builds burn grace period).
        //   - Scheduler is standby → UNAVAILABLE. The `admin`
        //     channel is balanced (main.rs connect loop), routes
        //     to the leader. Standby reject is transient.
        //   - Unknown worker_id → accepted=false. Pod hasn't
        //     heartbeated yet, or already disconnected. No-op.
        match admin
            .drain_worker(rio_proto::types::DrainWorkerRequest {
                worker_id: worker_id.to_string(),
                force: true,
            })
            .await
        {
            Ok(resp) => {
                let r = resp.into_inner();
                metrics::counter!(
                    "rio_controller_disruption_drains_total",
                    "result" => "sent"
                )
                .increment(1);
                info!(
                    worker_id,
                    running = r.running_builds,
                    accepted = r.accepted,
                    "DisruptionTarget: DrainWorker force=true"
                );
            }
            Err(e) => {
                metrics::counter!(
                    "rio_controller_disruption_drains_total",
                    "result" => "rpc_error"
                )
                .increment(1);
                warn!(
                    worker_id,
                    error = %e,
                    "DisruptionTarget: DrainWorker failed (SIGTERM fallback will drain)"
                );
            }
        }
    }
}

/// Pure filter: does this Pod have `DisruptionTarget=True`?
///
/// Returns `Some(pod_name)` if so. Pod name == worker_id (set via
/// `RIO_WORKER_ID=$(POD_NAME)` downward API in `build_pod_spec`).
///
/// Returns `None` for:
///   - No `DisruptionTarget` condition (never evicted — normal)
///   - `DisruptionTarget` present but `status != "True"` (K8s may
///     set False/Unknown transiently during eviction probe)
///   - Pod has no name (impossible via apiserver, but `ObjectMeta.
///     name` is `Option<String>` so belt-and-suspenders)
pub(super) fn is_disruption_target(pod: &Pod) -> Option<&str> {
    let disrupted = pod
        .status
        .as_ref()?
        .conditions
        .as_ref()?
        .iter()
        .any(|c| c.type_ == "DisruptionTarget" && c.status == "True");
    if !disrupted {
        return None;
    }
    pod.metadata.name.as_deref()
}
