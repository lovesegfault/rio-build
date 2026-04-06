//! kube-rs helpers. Replaces the kubectl invocations in the deploy recipes.

use std::collections::BTreeMap;
use std::path::PathBuf;
use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::{DaemonSet, Deployment};
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Namespace, Pod, Secret};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::ResourceExt;
use kube::api::{Api, AttachParams, ListParams, Patch, PatchParams};
use serde::Serialize;
use serde_json::json;
use tempfile::TempDir;
use tokio::io::AsyncReadExt;

use crate::sh::repo_root;
use crate::ui;

// Re-export so callers that `use crate::kube` can still name the type.
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

/// Fetch `tls.crt`, `tls.key`, `ca.crt` from a cert-manager Secret
/// into a tempdir. Returns `(TempDir, cert, key, ca)` — hold the
/// [`TempDir`] for the lifetime of whatever consumes the paths
/// (dropping it removes the files).
///
/// cert-manager Secrets always populate all three keys once the
/// Certificate reaches Ready; any missing key means issuance hasn't
/// completed. Key file is written 0600 — tonic doesn't check, but it
/// lands on the operator's laptop.
pub async fn fetch_tls_to_tempdir(
    client: &Client,
    ns: &str,
    secret: &str,
) -> Result<(TempDir, PathBuf, PathBuf, PathBuf)> {
    use std::os::unix::fs::PermissionsExt;

    let api: Api<Secret> = Api::namespaced(client.clone(), ns);
    let data = api
        .get_opt(secret)
        .await?
        .and_then(|s| s.data)
        .with_context(|| {
            format!("Secret {ns}/{secret} not found — wait for Certificate {secret} Ready")
        })?;
    let get = |k: &str| -> Result<Vec<u8>> {
        data.get(k).map(|v| v.0.clone()).with_context(|| {
            format!(
                "Secret {ns}/{secret} missing key '{k}' — \
                 cert-manager hasn't issued yet (wait for Certificate {secret} Ready)"
            )
        })
    };

    let dir = TempDir::new()?;
    let cert = dir.path().join("tls.crt");
    let key = dir.path().join("tls.key");
    let ca = dir.path().join("ca.crt");
    std::fs::write(&cert, get("tls.crt")?)?;
    std::fs::write(&key, get("tls.key")?)?;
    std::fs::set_permissions(&key, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(&ca, get("ca.crt")?)?;
    Ok((dir, cert, key, ca))
}

/// Find the scheduler leader pod from the Lease.
pub async fn scheduler_leader(client: &Client, ns: &str) -> Result<String> {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    let lease = api.get("rio-scheduler-leader").await?;
    lease
        .spec
        .and_then(|s| s.holder_identity)
        .context("scheduler lease has no holder")
}

/// Run a command in the scheduler leader pod and return combined
/// stdout+stderr. Fails if no leader or pod attachment denied.
///
/// **For rio-cli, use [`crate::k8s::with_cli_tunnel`] or
/// `CliCtx` instead** — running rio-cli
/// in-pod forces the scheduler image to bundle it (and jq, column,
/// whatever pipes through), widening the control-plane attack surface.
/// This helper stays for non-rio-cli debugging (`ps`, `ls /proc`, …).
#[allow(dead_code)] // intentionally kept — see doc-comment
pub async fn run_in_scheduler(client: &Client, ns: &str, cmd: &[&str]) -> Result<String> {
    let leader = scheduler_leader(client, ns).await?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), ns);
    let params = AttachParams::default().stdout(true).stderr(true);
    let mut attached = Api::exec(&pods, &leader, cmd.iter().copied(), &params).await?;
    let mut stdout = attached.stdout().context("no stdout")?;
    let mut stderr = attached.stderr().context("no stderr")?;
    let mut out = String::new();
    let mut err = String::new();
    stdout.read_to_string(&mut out).await?;
    stderr.read_to_string(&mut err).await?;
    attached.join().await?;
    Ok(out + &err)
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

/// One DaemonSet's readiness snapshot.
#[derive(Serialize)]
pub struct DsStatus {
    pub name: String,
    pub ready: i32,
    pub desired: i32,
    pub scheduled: i32,
    pub ok: bool,
}

/// Readiness snapshot of every DaemonSet in `ns`, sorted by name.
pub async fn list_daemonset_status(client: &Client, ns: &str) -> Result<Vec<DsStatus>> {
    let api: Api<DaemonSet> = Api::namespaced(client.clone(), ns);
    let mut out: Vec<_> = api
        .list(&ListParams::default())
        .await?
        .iter()
        .map(|d| {
            let st = d.status.clone().unwrap_or_default();
            DsStatus {
                name: d.name_any(),
                ready: st.number_ready,
                desired: st.desired_number_scheduled,
                scheduled: st.current_number_scheduled,
                ok: st.number_ready == st.desired_number_scheduled
                    && st.number_unavailable.unwrap_or(0) == 0,
            }
        })
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
