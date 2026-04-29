//! kube-rs client helpers shared by all `k8s::*` modules. Thin wrappers
//! over `kube::Api` for the operations the deploy/status/chaos paths
//! need (SSA apply, rollout wait, secret read/write, problem-pod scan).
//!
//! Distinct from [`super::shared`]: this module is pure kube-rs (no
//! shell-outs, no provider awareness); `shared` layers provider-neutral
//! orchestration on top (port-forward, secrets bootstrap, NIX_SSHOPTS).

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result, bail};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret, Service};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::ResourceExt;
use kube::api::{Api, ListParams, Patch, PatchParams};
use serde::Serialize;
use serde_json::json;

use crate::sh::repo_root;
use crate::ui;

// Re-export so callers can write `client::Client` / `kube::Client`
// (most import this module `as kube`).
pub use kube::Client;

pub async fn client() -> Result<Client> {
    Client::try_default()
        .await
        .context("failed to build kube client (is kubeconfig set?)")
}

/// `kubectl config current-context` equivalent — reads straight from
/// the kubeconfig file at `sh::kubeconfig_path()`.
pub fn current_context() -> Result<String> {
    let path = crate::sh::kubeconfig_path();
    let body = std::fs::read_to_string(&path)
        .with_context(|| format!("read kubeconfig at {}", path.display()))?;
    let cfg: serde_yml::Value = serde_yml::from_str(&body)?;
    cfg.get("current-context")
        .and_then(|v| v.as_str())
        .map(String::from)
        .context("kubeconfig has no current-context")
}

/// Server-side apply all YAML files in `infra/helm/crds/`.
pub async fn apply_crds(client: &Client) -> Result<()> {
    let api: Api<CustomResourceDefinition> = Api::all(client.clone());
    let ssapply = PatchParams::apply("xtask").force();
    let dir = repo_root().join("infra/helm/crds");

    for e in std::fs::read_dir(&dir)? {
        let p = e?.path();
        if p.extension().is_some_and(|x| x == "yaml") {
            let crd: CustomResourceDefinition = serde_yml::from_str(&std::fs::read_to_string(&p)?)?;
            let name = crd.name_any();
            api.patch(&name, &ssapply, &Patch::Apply(&crd)).await?;
        }
    }
    Ok(())
}

/// `kubectl wait --for=condition=Established crd/NAME`.
pub async fn wait_crd_established(client: &Client, name: &str, timeout: Duration) -> Result<()> {
    let api: Api<CustomResourceDefinition> = Api::all(client.clone());
    ui::poll(
        &format!("CRD {name} Established"),
        Duration::from_secs(2),
        (timeout.as_secs() / 2) as u32,
        || async {
            let crd = api.get_opt(name).await?;
            Ok(crd.and_then(|c| {
                c.status?
                    .conditions?
                    .iter()
                    .find(|cond| cond.type_ == "Established" && cond.status == "True")
                    .map(|_| ())
            }))
        },
    )
    .await
}

/// Create namespace with PSA + part-of labels. Idempotent.
///
/// `app.kubernetes.io/part-of: rio-build` is set unconditionally — the
/// chart's NetworkPolicy namespaceSelector rules match by it. `pod-
/// security.kubernetes.io/enforce` is always set (baseline or
/// privileged) so the label stays SSA-owned by xtask and a later flip
/// doesn't hit field-manager conflicts.
pub async fn ensure_namespace(client: &Client, name: &str, privileged: bool) -> Result<()> {
    let api: Api<Namespace> = Api::all(client.clone());
    let mut ns = Namespace::default();
    ns.metadata.name = Some(name.into());
    let psa = if privileged { "privileged" } else { "baseline" };
    ns.metadata.labels = Some(BTreeMap::from([
        ("app.kubernetes.io/part-of".into(), "rio-build".into()),
        ("pod-security.kubernetes.io/enforce".into(), psa.into()),
    ]));
    let ssapply = PatchParams::apply("xtask").force();
    api.patch(name, &ssapply, &Patch::Apply(&ns)).await?;
    Ok(())
}

/// Create/update a generic Secret with string data. Idempotent.
pub async fn apply_secret(
    client: &Client,
    ns: &str,
    name: &str,
    data: BTreeMap<String, String>,
) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let mut secret = Secret::default();
    secret.metadata.name = Some(name.into());
    secret.metadata.namespace = Some(ns.into());
    secret.string_data = Some(data);
    let ssapply = PatchParams::apply("xtask").force();
    api.patch(name, &ssapply, &Patch::Apply(&secret)).await?;
    Ok(())
}

/// Delete a Secret. Idempotent.
pub async fn delete_secret(client: &Client, ns: &str, name: &str) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let _ = api.delete(name, &Default::default()).await;
    Ok(())
}

/// Read one string key from a Secret. `None` if the Secret or key
/// doesn't exist.
pub async fn get_secret_key(
    client: &Client,
    ns: &str,
    name: &str,
    key: &str,
) -> Result<Option<String>> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    Ok(api
        .get_opt(name)
        .await?
        .and_then(|s| s.data)
        .and_then(|d| d.get(key).cloned())
        .and_then(|v| String::from_utf8(v.0).ok()))
}

/// LoadBalancer ingress hostname for the gateway Service. `Err` if the
/// Service has no `.status.loadBalancer.ingress` yet (NLB still
/// provisioning) — caller decides whether to wait or print a
/// placeholder.
pub async fn gateway_lb_hostname(client: &Client, ns: &str) -> Result<String> {
    let api: Api<Service> = Api::namespaced(client.clone(), ns);
    api.get("rio-gateway")
        .await?
        .status
        .and_then(|s| s.load_balancer)
        .and_then(|lb| lb.ingress)
        .and_then(|mut i| i.pop())
        .and_then(|i| i.hostname.or(i.ip))
        .context("rio-gateway Service has no LoadBalancer ingress yet (NLB provisioning?)")
}

/// Count pods in `ns` matching `label_selector`. Used by
/// [`super::shared::tunnel_grpc`] to short-circuit the leader-wait
/// poll when no scheduler pods exist (post-`--wipe`, pre-deploy).
pub async fn count_pods(client: &Client, ns: &str, label_selector: &str) -> Result<usize> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    Ok(api
        .list(&ListParams::default().labels(label_selector))
        .await?
        .items
        .len())
}

/// Find the scheduler leader pod from the Lease, verifying the holder
/// is a live pod (Running, not Terminating). Bails with a descriptive
/// reason when the lease points at a stale pod; callers that need to
/// wait out a failover wrap this in a poll loop and retry on Err
/// ([`super::shared::tunnel_grpc`]).
///
/// During a rollout the Lease's `holderIdentity` keeps naming the OLD
/// pod for up to `leaseDurationSeconds` after it enters
/// terminationGracePeriod — release runs on the shutdown path, not in
/// preStop. Port-forwarding to that pod succeeds AND accepts TCP, then
/// dies mid-RPC: surfaced as a bare `transport error` from rio-cli the
/// first time `qa --health` ran straight after `helm upgrade --wait`.
pub async fn scheduler_leader(client: &Client, ns: &str) -> Result<String> {
    let leases: Api<Lease> = Api::namespaced(client.clone(), ns);
    let holder = leases
        .get("rio-scheduler-leader")
        .await?
        .spec
        .and_then(|s| s.holder_identity)
        .context("scheduler lease has no holder")?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    holder_live(&holder, pods.get_opt(&holder).await?)?;
    Ok(holder)
}

/// Pod-liveness gate for [`scheduler_leader`]. Split out so the
/// Terminating / not-Running / vanished branches are unit-testable
/// without a kube client. Err message is what the poll loop logs.
fn holder_live(holder: &str, pod: Option<Pod>) -> Result<()> {
    let Some(pod) = pod else {
        bail!("lease holder {holder} not found; waiting for new leader");
    };
    if pod.metadata.deletion_timestamp.is_some() {
        bail!("lease holder {holder} is Terminating; waiting for new leader");
    }
    let phase = pod.status.and_then(|s| s.phase).unwrap_or_default();
    if phase != "Running" {
        bail!("lease holder {holder} phase={phase}; waiting for Running");
    }
    Ok(())
}

/// Rollout-restart a Deployment (patch restartedAt annotation).
pub async fn rollout_restart(client: &Client, ns: &str, name: &str) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let patch = json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": chrono_now()
        }}}}
    });
    api.patch(name, &PatchParams::default(), &Patch::Merge(&patch))
        .await?;
    Ok(())
}

/// Rollout-restart every Deployment matching `label_selector` in `ns`.
/// Returns the restarted deployment names (for logging).
pub async fn rollout_restart_all(
    client: &Client,
    ns: &str,
    label_selector: &str,
) -> Result<Vec<String>> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let lp = ListParams::default().labels(label_selector);
    let now = chrono_now();
    let patch = json!({
        "spec": { "template": { "metadata": { "annotations": {
            "kubectl.kubernetes.io/restartedAt": now
        }}}}
    });
    let mut names = Vec::new();
    for d in api.list(&lp).await? {
        let name = d.name_any();
        api.patch(&name, &PatchParams::default(), &Patch::Merge(&patch))
            .await?;
        names.push(name);
    }
    Ok(names)
}

/// One Deployment's readiness snapshot. `ok` is the same predicate
/// `kubectl rollout status` checks — see `wait_rollout`.
#[derive(Serialize)]
pub struct DeployStatus {
    pub name: String,
    pub ready: i32,
    pub want: i32,
    pub updated: i32,
    pub ok: bool,
}

fn deploy_status(d: &Deployment) -> DeployStatus {
    let want = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
    let st = d.status.clone().unwrap_or_default();
    let ready = st.ready_replicas.unwrap_or(0);
    let updated = st.updated_replicas.unwrap_or(0);
    let total = st.replicas.unwrap_or(0);
    let obs = st.observed_generation.unwrap_or(0);
    let cur = d.metadata.generation.unwrap_or(0);
    // `total == updated` catches old pods still hanging around during
    // the surge window — without it, this returns while an old pod
    // has no deletion timestamp yet.
    let ok = obs >= cur && updated >= want && total == updated && ready >= want;
    DeployStatus {
        name: d.name_any(),
        ready,
        want,
        updated,
        ok,
    }
}

/// Wait for a Deployment's rollout to complete (updatedReplicas == readyReplicas == spec.replicas).
pub async fn wait_rollout(client: &Client, ns: &str, name: &str, timeout: Duration) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    ui::poll(
        &format!("rollout {name}"),
        Duration::from_secs(2),
        (timeout.as_secs() / 2) as u32,
        || async { Ok(deploy_status(&api.get(name).await?).ok.then_some(())) },
    )
    .await
}

/// Readiness snapshot of every Deployment in `ns`, sorted by name.
pub async fn list_deployment_status(client: &Client, ns: &str) -> Result<Vec<DeployStatus>> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .iter()
        .map(deploy_status)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// One DaemonSet's readiness snapshot. `ok` mirrors [`deploy_status`]:
/// observed-generation current AND every pod updated AND ready, so a
/// mid-surge rollout (old pods still ready, `updated < desired`) reads
/// as not-ok rather than green.
#[derive(Serialize)]
pub struct DsStatus {
    pub name: String,
    pub ready: i32,
    pub desired: i32,
    pub scheduled: i32,
    pub updated: i32,
    pub ok: bool,
}

fn ds_status(d: &DaemonSet) -> DsStatus {
    let st = d.status.clone().unwrap_or_default();
    let desired = st.desired_number_scheduled;
    let updated = st.updated_number_scheduled.unwrap_or(0);
    let obs = st.observed_generation.unwrap_or(0);
    let cur = d.metadata.generation.unwrap_or(0);
    // Same surge-window guard as `deploy_status`: `updated == desired`
    // catches old pods still running during a maxSurge rollout —
    // without it, `xtask k8s status` shows cilium ok while old pods run.
    let ok = obs >= cur
        && updated == desired
        && st.number_ready == desired
        && st.number_unavailable.unwrap_or(0) == 0;
    DsStatus {
        name: d.name_any(),
        ready: st.number_ready,
        desired,
        scheduled: st.current_number_scheduled,
        updated,
        ok,
    }
}

/// Readiness snapshot of every DaemonSet in `ns`, sorted by name.
pub async fn list_daemonset_status(client: &Client, ns: &str) -> Result<Vec<DsStatus>> {
    let api: Api<DaemonSet> = Api::namespaced(client.clone(), ns);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .iter()
        .map(ds_status)
        .collect();
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// A pod that's not healthy — phase ≠ Running or a container not ready.
#[derive(Serialize)]
pub struct PodProblem {
    pub name: String,
    pub phase: String,
    pub reason: Option<String>,
    pub restarts: i32,
}

/// Pods in `ns` whose phase isn't Running or that have a not-ready
/// container. Skips Succeeded (bootstrap-job completes and stays).
pub async fn problem_pods(client: &Client, ns: &str) -> Result<Vec<PodProblem>> {
    let api: Api<Pod> = Api::namespaced(client.clone(), ns);
    let mut out = Vec::new();
    for p in api.list(&ListParams::default()).await? {
        let Some(st) = &p.status else { continue };
        let phase = st.phase.clone().unwrap_or_default();
        if phase == "Succeeded" {
            continue;
        }
        let cs = st.container_statuses.clone().unwrap_or_default();
        let all_ready = cs.iter().all(|c| c.ready);
        if phase == "Running" && all_ready {
            continue;
        }
        // Surface the most useful reason: container waiting reason
        // (ImagePullBackOff, CrashLoopBackOff) beats pod-level reason.
        let reason = cs
            .iter()
            .find_map(|c| c.state.as_ref()?.waiting.as_ref()?.reason.clone())
            .or_else(|| st.reason.clone());
        let restarts = cs.iter().map(|c| c.restart_count).max().unwrap_or(0);
        out.push(PodProblem {
            name: p.name_any(),
            phase,
            reason,
            restarts,
        });
    }
    out.sort_by(|a, b| a.name.cmp(&b.name));
    Ok(out)
}

/// Wait for a Secret key to exist and be non-empty.
pub async fn wait_secret_key(
    client: &Client,
    ns: &str,
    name: &str,
    key: &str,
    timeout: Duration,
) -> Result<()> {
    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    ui::poll(
        &format!("Secret {ns}/{name} key {key}"),
        Duration::from_secs(5),
        (timeout.as_secs() / 5) as u32,
        || async {
            Ok(api
                .get_opt(name)
                .await?
                .and_then(|s| s.data)
                .and_then(|d| d.get(key).map(|v| !v.0.is_empty()).filter(|&b| b))
                .map(|_| ()))
        },
    )
    .await
}

fn chrono_now() -> String {
    // RFC3339 "now" without pulling chrono. k8s doesn't validate the
    // format strictly, it just compares for change.
    use std::time::{SystemTime, UNIX_EPOCH};
    let secs = SystemTime::now()
        .duration_since(UNIX_EPOCH)
        .unwrap()
        .as_secs();
    format!("{secs}")
}

#[cfg(test)]
mod tests {
    use super::*;
    use k8s_openapi::api::apps::v1::DaemonSetStatus;

    fn ds(generation: i64, st: DaemonSetStatus) -> DaemonSet {
        let mut d = DaemonSet::default();
        d.metadata.generation = Some(generation);
        d.status = Some(st);
        d
    }

    #[test]
    fn ds_status_false_during_surge() {
        // Mid-rollout: old pods still ready, new pods not all out yet.
        // numberReady==desired and unavailable==0, but updated < desired.
        // Previously reported ok=true (green) here.
        let d = ds(
            6,
            DaemonSetStatus {
                observed_generation: Some(6),
                desired_number_scheduled: 3,
                number_ready: 3,
                number_unavailable: Some(0),
                updated_number_scheduled: Some(1),
                current_number_scheduled: 3,
                ..Default::default()
            },
        );
        assert!(!ds_status(&d).ok);
    }

    #[test]
    fn ds_status_false_on_stale_generation() {
        // Spec edited (gen=6), controller hasn't observed it yet (obs=5).
        let d = ds(
            6,
            DaemonSetStatus {
                observed_generation: Some(5),
                desired_number_scheduled: 3,
                number_ready: 3,
                number_unavailable: Some(0),
                updated_number_scheduled: Some(3),
                current_number_scheduled: 3,
                ..Default::default()
            },
        );
        assert!(!ds_status(&d).ok);
    }

    fn pod(phase: &str, terminating: bool) -> Pod {
        use k8s_openapi::api::core::v1::PodStatus;
        use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
        let mut p = Pod::default();
        if terminating {
            // Any value — holder_live only checks is_some().
            p.metadata.deletion_timestamp = Some(Time(jiff::Timestamp::now()));
        }
        p.status = Some(PodStatus {
            phase: Some(phase.into()),
            ..Default::default()
        });
        p
    }

    /// `qa --health` straight after `helm upgrade --wait`: the Lease
    /// still names the old pod (terminationGracePeriod). Port-forward
    /// to it succeeds, TCP-accept passes, then it dies mid-RPC.
    /// `scheduler_leader` must reject it so the poll loop waits for
    /// the new leader instead of tunneling into a dying pod.
    #[test]
    fn holder_live_rejects_terminating() {
        let e = holder_live("old-sched-0", Some(pod("Running", true))).unwrap_err();
        assert!(e.to_string().contains("Terminating"), "{e}");
        assert!(e.to_string().contains("old-sched-0"), "{e}");
    }

    #[test]
    fn holder_live_rejects_missing_and_not_running() {
        let e = holder_live("gone", None).unwrap_err();
        assert!(e.to_string().contains("not found"), "{e}");
        let e = holder_live("p", Some(pod("Pending", false))).unwrap_err();
        assert!(e.to_string().contains("phase=Pending"), "{e}");
    }

    #[test]
    fn holder_live_accepts_running() {
        holder_live("sched-0", Some(pod("Running", false))).unwrap();
    }

    #[test]
    fn ds_status_true_at_steady_state() {
        let d = ds(
            6,
            DaemonSetStatus {
                observed_generation: Some(6),
                desired_number_scheduled: 3,
                number_ready: 3,
                number_unavailable: Some(0),
                updated_number_scheduled: Some(3),
                current_number_scheduled: 3,
                ..Default::default()
            },
        );
        assert!(ds_status(&d).ok);
    }
}
