//! DisruptionTarget watcher: pre-empt builds on evicting workers.
//!
//! K8s sets `status.conditions[type=DisruptionTarget, status=True]`
//! when eviction is imminent (node drain, spot interrupt, PDB-
//! mediated disruption). We fire `DrainExecutor{force:true}` → the
//! scheduler reads `running_build`, sends `CancelSignal` → worker
//! `cgroup.kill()`s → the build reassigns in seconds
//! instead of burning the 2h `terminationGracePeriodSeconds`.
//!
//! This is what the four pre-existing comments at
//! `rio-scheduler/src/actor/worker.rs:220`, `actor/tests/worker.rs:345`,
//! `builders.rs:171`, and `builderpool/mod.rs:204` have been asserting
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
//! event for that pod fires another `DrainExecutor{force:true}`. The
//! scheduler's `handle_drain_executor` is idempotent: `force=true`
//! with `draining=true` re-preempts, which is a no-op on an
//! already-cleared `running_build`. No client-side dedup needed.

// r[impl ctrl.drain.disruption-target+2]
// r[impl ctrl.pool.disruption]

use futures_util::StreamExt;
use k8s_openapi::api::batch::v1::Job;
use k8s_openapi::api::core::v1::Pod;
use kube::api::DeleteParams;
use kube::runtime::{WatchStreamExt, watcher};
use kube::{Api, Client};
use tracing::{debug, info, warn};

use super::POOL_LABEL;
use crate::reconcilers::{AdminClient, admin_call};

/// Run the watcher. Returns on `shutdown.cancelled()` or if the
/// watch stream ends (never — `default_backoff()` retries
/// connection failures indefinitely, and `watcher()` re-lists on
/// desync).
///
/// `spawn_monitored("disruption-watcher", run(...))` from main.rs.
/// Panics are logged; the controller keeps reconciling (workers
/// just lose the fast-preemption path — SIGTERM self-drain is the
/// fallback).
pub async fn run(client: Client, mut admin: AdminClient, shutdown: rio_common::signal::Token) {
    // All-namespaces: Pool is namespaced, so pods can be in
    // any ns. Label selector filters to OUR pods at the apiserver
    // (not client-side) — cheap.
    let client_for_jobs = client.clone();
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
        let Some(executor_id) = is_disruption_target(&pod) else {
            continue;
        };

        // r[impl ctrl.drain.disruption-target+2]
        // AD5/C6 successor for pull-mode pods: `DrainExecutor` /
        // `CancelSignal` are structural no-ops for a pod with no
        // executor entry or stream, so preemption becomes synthesize
        // the terminal report, then foreground-delete the Job — the
        // deletion's SIGTERM is the abort (the builder cgroup-kills and
        // makes its one bounded report attempt inside the 45 s grace),
        // and the requeue happens at the report fold, never the
        // establishment sweep. Stream pods keep the force-drain hop
        // below, untouched.
        if let Some(preempt) = pull_mode_preemption(&pod) {
            preempt_pull_mode_pod(&client_for_jobs, &mut admin, &preempt).await;
            continue;
        }

        // Best-effort. `force=true` triggers the preemption block
        // at `rio-scheduler/src/actor/worker.rs:211-258`: take
        // running_build → CancelSignal → reassign.
        //
        // Failure modes:
        //   - Scheduler down → tonic ConnectError. Worker's own
        //     SIGTERM handler also calls DrainExecutor (force=false)
        //     via its direct channel, so no-drain is only as bad
        //     as "no preemption" (builds burn grace period).
        //   - Scheduler is standby → UNAVAILABLE. The `admin`
        //     channel is balanced (main.rs connect loop), routes
        //     to the leader. Standby reject is transient.
        //   - Unknown executor_id → accepted=false. Pod hasn't
        //     heartbeated yet, or already disconnected. No-op.
        match admin_call(
            admin.drain_executor(rio_proto::types::DrainExecutorRequest {
                executor_id: executor_id.to_string(),
                force: true,
            }),
        )
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
                    executor_id,
                    busy = r.busy,
                    accepted = r.accepted,
                    "DisruptionTarget: DrainExecutor force=true"
                );
            }
            Err(e) => {
                let result = if e.code() == tonic::Code::DeadlineExceeded {
                    "timeout"
                } else {
                    "rpc_error"
                };
                metrics::counter!(
                    "rio_controller_disruption_drains_total",
                    "result" => result
                )
                .increment(1);
                warn!(
                    executor_id,
                    error = %e,
                    "DisruptionTarget: DrainExecutor failed (SIGTERM fallback will drain)"
                );
            }
        }
    }
}

/// Pure filter: does this Pod have `DisruptionTarget=True`?
///
/// Returns `Some(pod_name)` if so. Pod name == executor_id (set via
/// `RIO_EXECUTOR_ID=$(POD_NAME)` downward API in `build_pod_spec`).
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

/// What the AD5/C6 preemption successor needs for one pull-mode pod:
/// the owning Job to foreground-delete, the attempt identity to
/// synthesize the `preempted` report with, and the namespace to do it
/// in.
#[derive(Debug, Clone, PartialEq, Eq)]
pub(super) struct PullPreemption {
    pub namespace: String,
    pub job_name: Option<String>,
    pub intent_id: String,
    pub node_name: String,
    pub pod_name: String,
}

/// Pure decision: a disruption-targeted pod belongs to a pull-mode
/// pool iff its executor container carries `RIO_DISPATCH_MODE=pull` —
/// the rendering of the Pool CR's `dispatchMode: Pull` (T-1b.6), so
/// gating on the env IS gating on the CR field without an extra
/// apiserver read from the watcher's hot path (the shared
/// `pod::pod_is_pull_mode` discriminator). Stream pods return `None`
/// and keep the force-drain hop.
pub(super) fn pull_mode_preemption(pod: &Pod) -> Option<PullPreemption> {
    if !super::pod::pod_is_pull_mode(pod) {
        return None;
    }
    let pod_name = pod.metadata.name.clone()?;
    Some(PullPreemption {
        namespace: pod.metadata.namespace.clone().unwrap_or_default(),
        job_name: pod
            .metadata
            .owner_references
            .as_ref()
            .and_then(|refs| refs.iter().find(|r| r.kind == "Job"))
            .map(|r| r.name.clone()),
        intent_id: pod
            .metadata
            .annotations
            .as_ref()
            .and_then(|a| a.get(super::jobs::INTENT_ID_ANNOTATION))
            .cloned()
            .unwrap_or_default(),
        node_name: pod
            .spec
            .as_ref()
            .and_then(|s| s.node_name.clone())
            .unwrap_or_default(),
        pod_name,
    })
}

/// Execute one pull-mode preemption: synthesize the terminal
/// `ReportAttemptOutcome(preempted)` for the pod's attempt (best-effort
/// — the establishment sweep is the fallback classifier), then
/// foreground-delete the owning Job so the pod's SIGTERM-abort fires
/// now instead of at `activeDeadlineSeconds`. No `DrainExecutor` is
/// ever sent for a pull-mode pod.
async fn preempt_pull_mode_pod(client: &Client, admin: &mut AdminClient, p: &PullPreemption) {
    match admin_call(
        admin.report_attempt_outcome(rio_proto::types::ReportAttemptOutcomeRequest {
            intent_id: p.intent_id.clone(),
            job_name: p.job_name.clone().unwrap_or_else(|| p.pod_name.clone()),
            exec_id: String::new(),
            reason: rio_proto::types::AttemptTerminalReason::Preempted.into(),
            node_name: p.node_name.clone(),
        }),
    )
    .await
    {
        Ok(_) => {
            metrics::counter!(
                "rio_controller_disruption_drains_total",
                "result" => "preempted_pull"
            )
            .increment(1);
            info!(
                pod = %p.pod_name,
                job = ?p.job_name,
                intent_id = %p.intent_id,
                "DisruptionTarget: synthesized preempted report for pull-mode pod"
            );
        }
        Err(e) => {
            metrics::counter!(
                "rio_controller_disruption_drains_total",
                "result" => "preempted_pull_report_failed"
            )
            .increment(1);
            warn!(
                pod = %p.pod_name, error = %e,
                "DisruptionTarget: preempted report failed; proceeding with the Job delete \
                 (the establishment sweep is the fallback classifier)"
            );
        }
    }
    let Some(job_name) = p.job_name.as_deref() else {
        debug!(pod = %p.pod_name, "pull-mode pod has no owning Job; nothing to delete");
        return;
    };
    let jobs: Api<Job> = Api::namespaced(client.clone(), &p.namespace);
    match jobs.delete(job_name, &DeleteParams::foreground()).await {
        Ok(_) => {
            info!(job = %job_name, ns = %p.namespace, "DisruptionTarget: foreground-deleted pull-mode Job")
        }
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            debug!(job = %job_name, "pull-mode Job already gone");
        }
        Err(e) => warn!(job = %job_name, error = %e,
                        "DisruptionTarget: pull-mode Job delete failed (kubelet eviction will still abort the pod)"),
    }
}
