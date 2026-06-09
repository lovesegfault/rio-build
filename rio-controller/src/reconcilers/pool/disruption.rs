//! DisruptionTarget watcher: pre-empt builds on evicting workers.
//!
//! K8s sets `status.conditions[type=DisruptionTarget, status=True]`
//! when eviction is imminent (node drain, spot interrupt, PDB-
//! mediated disruption). The watcher synthesizes the terminal
//! `ReportAttemptOutcome(preempted)` for the pod's attempt and
//! foreground-deletes the owning Job, so the pod's SIGTERM-abort
//! cgroup-kills the build now and the drv requeues at the report fold
//! in seconds instead of burning `activeDeadlineSeconds` (AD5/C6; the
//! stream-era `DrainExecutor{force:true}` hop retired with the stream
//! protocol at the 1d controller cleanup).
//!
//! # Why a Pod watcher, not a preStop hook
//!
//! `preStop` fires on EVERY termination, including graceful scale-
//! down. `DisruptionTarget` fires only on eviction-budget-mediated
//! disruption (node drain, spot interrupt, Karpenter consolidation)
//! where preemption IS the right call: the pod is dying regardless,
//! so closing its attempt and requeueing now is strictly better than
//! letting the build burn the grace period and then lose it anyway.
//!
//! # Idempotence
//!
//! `DisruptionTarget` stays True for the pod's remaining lifetime
//! (the condition is sticky until pod termination). Every watcher
//! event for that pod re-resolves the open attempt: the same attempt
//! re-synthesizes the same exec-pinned report (idempotent at the
//! scheduler's attempt-terminal gate); a closed/absent attempt
//! synthesizes NOTHING (no RPC at all — a sticky re-fire can never
//! close a newer attempt it did not observe). The Job delete is
//! idempotent either way. No client-side dedup needed.

// r[impl ctrl.drain.disruption-target+4]
// r[impl ctrl.pool.disruption+2]

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

        // r[impl ctrl.drain.disruption-target+4]
        // AD5/C6: preemption is synthesize the terminal report, then
        // foreground-delete the owning Job — the deletion's SIGTERM is
        // the abort (the builder cgroup-kills and makes its one
        // bounded report attempt inside the 45 s grace), and the
        // requeue happens at the report fold, never the establishment
        // sweep. There is no per-executor drain RPC: the stream-era
        // `DrainExecutor` hop retired with the stream protocol.
        let preempt = preemption_for_pod(&pod, executor_id);
        preempt_disrupted_pod(&client_for_jobs, &mut admin, &preempt).await;
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

/// What the AD5/C6 preemption needs for one disruption-targeted pod:
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

/// Pure projection of a disruption-targeted pod onto the preemption
/// decision: the owning Job, the intent annotation (the attempt
/// identity), the bound node, and the namespace. Every executor pod
/// takes this path — there is no per-mode gate left (the stream-era
/// force-drain hop is gone, and the builder aborts on SIGTERM
/// regardless of what its pod template renders).
pub(super) fn preemption_for_pod(pod: &Pod, pod_name: &str) -> PullPreemption {
    PullPreemption {
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
        pod_name: pod_name.to_owned(),
    }
}

/// Execute one preemption: synthesize the terminal
/// `ReportAttemptOutcome(preempted)` for the pod's attempt (best-effort
/// — the establishment sweep is the fallback classifier), then
/// foreground-delete the owning Job so the pod's SIGTERM-abort fires
/// now instead of at `activeDeadlineSeconds`.
pub(super) async fn preempt_disrupted_pod(
    client: &Client,
    admin: &mut AdminClient,
    p: &PullPreemption,
) {
    // merged_bug_135: resolve the attempt identity FIRST. A synthesized
    // verdict is exec-pinned — it exists only for an attempt the
    // controller actually observed open. No open attempt (pod never
    // pulled, or the attempt already closed) ⇒ NO report; the Job
    // delete below still proceeds and the establishment sweep remains
    // the classifier for anything that appears later. The view read is
    // best-effort: on error we skip the report (never invent one) and
    // still delete.
    let report = match admin_call(
        admin
            .clone()
            .list_open_attempts(rio_proto::types::ListOpenAttemptsRequest {}),
    )
    .await
    {
        Ok(resp) => super::job::synthesized_report_for_intent(
            Some(p.intent_id.as_str()).filter(|i| !i.is_empty()),
            p.job_name.clone().unwrap_or_else(|| p.pod_name.clone()),
            rio_proto::types::AttemptTerminalReason::Preempted,
            &resp.into_inner().attempts,
            // merged_bug_298: the watcher owns exactly the disrupted
            // pod (executor_id) on its known node — a same-intent
            // attempt elsewhere is not ours to close.
            super::job::AttemptOwner::Pod {
                pod: &p.pod_name,
                node: &p.node_name,
            },
        ),
        Err(e) => {
            warn!(
                pod = %p.pod_name, error = %e,
                "DisruptionTarget: ListOpenAttempts failed; skipping the synthesized \
                 report (the establishment sweep is the fallback classifier)"
            );
            None
        }
    };
    match report {
        None => {
            metrics::counter!(
                "rio_controller_disruption_drains_total",
                "result" => "preempted_pull_no_attempt"
            )
            .increment(1);
            debug!(
                pod = %p.pod_name,
                intent_id = %p.intent_id,
                "DisruptionTarget: no open attempt for the disrupted pod; nothing to report"
            );
        }
        Some(req) => match admin_call(admin.report_attempt_outcome(req)).await {
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
                    "DisruptionTarget: synthesized preempted report for the disrupted pod"
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
        },
    }
    let Some(job_name) = p.job_name.as_deref() else {
        debug!(pod = %p.pod_name, "disrupted pod has no owning Job; nothing to delete");
        return;
    };
    let jobs: Api<Job> = Api::namespaced(client.clone(), &p.namespace);
    match jobs.delete(job_name, &DeleteParams::foreground()).await {
        Ok(_) => {
            info!(job = %job_name, ns = %p.namespace, "DisruptionTarget: foreground-deleted the disrupted Job")
        }
        Err(kube::Error::Api(ae)) if ae.code == 404 => {
            debug!(job = %job_name, "disrupted Job already gone");
        }
        Err(e) => warn!(job = %job_name, error = %e,
                        "DisruptionTarget: Job delete failed (kubelet eviction will still abort the pod)"),
    }
}
