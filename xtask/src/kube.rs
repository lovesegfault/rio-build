//! kube-rs helpers. Replaces the kubectl invocations in the deploy recipes.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::Deployment;
use k8s_openapi::api::coordination::v1::Lease;
use k8s_openapi::api::core::v1::{Namespace, Secret};
use k8s_openapi::apiextensions_apiserver::pkg::apis::apiextensions::v1::CustomResourceDefinition;
use kube::ResourceExt;
use kube::api::{Api, Patch, PatchParams};
use serde_json::json;

use crate::sh::repo_root;
use crate::ui;

// Re-export so callers that `use crate::kube` can still name the type.
pub use kube::Client;

pub async fn client() -> Result<Client> {
    Client::try_default()
        .await
        .context("failed to build kube client (is kubeconfig set?)")
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

/// Create namespace with PSA label. Idempotent.
pub async fn ensure_namespace(client: &Client, name: &str, privileged: bool) -> Result<()> {
    let api: Api<Namespace> = Api::all(client.clone());
    let mut ns = Namespace::default();
    ns.metadata.name = Some(name.into());
    if privileged {
        ns.metadata.labels = Some(BTreeMap::from([(
            "pod-security.kubernetes.io/enforce".into(),
            "privileged".into(),
        )]));
    }
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

/// Find the scheduler leader pod from the Lease.
pub async fn scheduler_leader(client: &Client, ns: &str) -> Result<String> {
    let api: Api<Lease> = Api::namespaced(client.clone(), ns);
    let lease = api.get("rio-scheduler-leader").await?;
    lease
        .spec
        .and_then(|s| s.holder_identity)
        .context("scheduler lease has no holder")
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

/// Wait for a Deployment's rollout to complete (updatedReplicas == readyReplicas == spec.replicas).
pub async fn wait_rollout(client: &Client, ns: &str, name: &str, timeout: Duration) -> Result<()> {
    let api: Api<Deployment> = Api::namespaced(client.clone(), ns);
    ui::poll(
        &format!("rollout {name}"),
        Duration::from_secs(2),
        (timeout.as_secs() / 2) as u32,
        || async {
            let d = api.get(name).await?;
            let want = d.spec.as_ref().and_then(|s| s.replicas).unwrap_or(1);
            let st = d.status.unwrap_or_default();
            let ready = st.ready_replicas.unwrap_or(0);
            let updated = st.updated_replicas.unwrap_or(0);
            let total = st.replicas.unwrap_or(0);
            let obs = st.observed_generation.unwrap_or(0);
            let cur = d.metadata.generation.unwrap_or(0);
            // Same condition set as `kubectl rollout status`:
            // `total == updated` is the one that catches old pods
            // still hanging around during the surge window — without
            // it, this returns while an old pod has no deletion
            // timestamp yet, and downstream IP captures grab it.
            let done = obs >= cur && updated >= want && total == updated && ready >= want;
            Ok(done.then_some(()))
        },
    )
    .await
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
