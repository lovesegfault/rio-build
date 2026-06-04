//! Cold the builder node(s) between cold-reps by evicting every mountd
//! pod's chunk + backing cache.
//!
//! There is no remote eviction API and the mountd image ships no shell,
//! so we cannot `kubectl exec … rm`. Instead we exec the daemon binary
//! directly with the `evict-cache` subcommand: it deletes all chunk +
//! backing cache entries, prints the freed-byte map, and exits without
//! binding any ports.

use anyhow::{Context, Result};
use k8s_openapi::api::apps::v1::DaemonSet;
use k8s_openapi::api::core::v1::Node;
use kube::api::{ListParams, Patch, PatchParams};
use tracing::{info, warn};

use crate::k8s::NS_BUILDERS;
use crate::sh::{self, shell};
use xshell::cmd;

/// Cache dirs the mountd DaemonSet defaults to (see
/// `infra/.../mountd-ds.yaml`); the `evict-cache` subcommand reuses the
/// daemon's own `--chunks-dir` / `--cache-dir` flags.
const CHUNKS_DIR: &str = "/var/rio/chunks";
const CACHE_DIR: &str = "/var/rio/cache";

/// Evict the chunk + backing cache on every builder mountd pod. All or
/// nothing: a single pod left warm becomes a node a rep can land on and
/// pass the run while failing the honesty gate, so any exec error
/// propagates rather than skipping.
pub(super) async fn evict_builder_caches(client: &kube::Client) -> Result<()> {
    // The daemon binary path is deterministic from the DaemonSet spec;
    // don't guess it — a wrong path would silently leave nodes warm.
    let ds: kube::Api<DaemonSet> = kube::Api::namespaced(client.clone(), NS_BUILDERS);
    let ds = ds
        .get("rio-mountd")
        .await
        .context("get rio-mountd DaemonSet")?;
    let bin = ds
        .spec
        .and_then(|s| s.template.spec)
        .and_then(|p| p.containers.into_iter().next())
        .and_then(|c| c.command)
        .and_then(|cmd| cmd.into_iter().next())
        .context("rio-mountd DaemonSet has no containers[0].command[0]")?;

    let pods = super::cotenancy::all_mountd_pods(client).await?;
    anyhow::ensure!(
        !pods.is_empty(),
        "no rio-mountd pods found in {NS_BUILDERS} — cannot evict caches; a warm node \
         would fail the per-rep honesty gate"
    );

    // xshell `cmd!` `{ident}` interpolation requires plain locals.
    let sh = shell()?;
    let ns = NS_BUILDERS;
    let chunks_dir = CHUNKS_DIR;
    let cache_dir = CACHE_DIR;
    let bin = bin.as_str();
    for pod in &pods {
        let pod = pod.as_str();
        let out = sh::read(cmd!(
            sh,
            "kubectl -n {ns} exec {pod} -- {bin} --chunks-dir {chunks_dir} --cache-dir {cache_dir} evict-cache"
        ))
        .with_context(|| format!("evict-cache exec on pod {pod}"))?;
        info!("evicted mountd cache on {pod}: {}", out.trim());
    }
    Ok(())
}

/// Node label selecting builder nodes (Karpenter stamps it from the
/// builder NodePools; the mountd DaemonSet schedules on the same key).
const BUILDER_NODE_SELECTOR: &str = "rio.build/node-role=builder";

/// Cordon every builder node EXCEPT `target` so the scheduler can only
/// place the next rep on the run's pinned node. Spot churn provisions
/// replacement builder nodes mid-run; without this sweep reps bounce
/// between nodes and the node-pin assertion burns the redo budget on
/// placement coin-flips. Newly cordoned node names are appended to
/// `cordoned` so the caller can undo exactly what this run did — nodes
/// already unschedulable for other reasons are left alone.
pub(super) async fn cordon_other_builders(
    client: &kube::Client,
    target: &str,
    cordoned: &mut Vec<String>,
) -> Result<()> {
    let nodes: kube::Api<Node> = kube::Api::all(client.clone());
    let list = nodes
        .list(&ListParams::default().labels(BUILDER_NODE_SELECTOR))
        .await
        .context("list builder nodes for cordon sweep")?;
    for node in list {
        let Some(name) = node.metadata.name.clone() else {
            continue;
        };
        if name == target || cordoned.iter().any(|c| c == &name) {
            continue;
        }
        if node.spec.as_ref().and_then(|s| s.unschedulable) == Some(true) {
            continue;
        }
        nodes
            .patch(
                &name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({"spec": {"unschedulable": true}})),
            )
            .await
            .with_context(|| format!("cordon builder node {name}"))?;
        info!("cordoned non-target builder node {name} (target {target})");
        cordoned.push(name);
    }
    Ok(())
}

/// Annotate the run's pinned target node `karpenter.sh/do-not-disrupt`
/// so consolidation cannot reap it between reps — evict-cache makes the
/// node look idle exactly when the run needs it to survive. Removed by
/// [`remove_do_not_disrupt`] on every run exit path.
pub(super) async fn annotate_do_not_disrupt(client: &kube::Client, node: &str) -> Result<()> {
    let nodes: kube::Api<Node> = kube::Api::all(client.clone());
    nodes
        .patch(
            node,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": {"annotations": {"karpenter.sh/do-not-disrupt": "true"}}
            })),
        )
        .await
        .with_context(|| format!("annotate do-not-disrupt on node {node}"))?;
    info!("annotated karpenter.sh/do-not-disrupt on target node {node}");
    Ok(())
}

/// Best-effort removal of the do-not-disrupt annotation. Failures are
/// logged loudly but never propagated — same contract as
/// [`uncordon_nodes`].
pub(super) async fn remove_do_not_disrupt(client: &kube::Client, node: &str) {
    let nodes: kube::Api<Node> = kube::Api::all(client.clone());
    match nodes
        .patch(
            node,
            &PatchParams::default(),
            &Patch::Merge(serde_json::json!({
                "metadata": {"annotations": {"karpenter.sh/do-not-disrupt": serde_json::Value::Null}}
            })),
        )
        .await
    {
        Ok(_) => info!("removed do-not-disrupt annotation from node {node}"),
        Err(e) => warn!(
            "FAILED to remove do-not-disrupt annotation from node {node}: {e:#} — \
             fix by hand: kubectl annotate node {node} karpenter.sh/do-not-disrupt-"
        ),
    }
}

/// Best-effort uncordon of every node this run cordoned. Failures are
/// logged loudly (the operator must `kubectl uncordon` by hand) but
/// never propagated — cleanup must not mask the run's own result.
pub(super) async fn uncordon_nodes(client: &kube::Client, cordoned: &[String]) {
    if cordoned.is_empty() {
        return;
    }
    let nodes: kube::Api<Node> = kube::Api::all(client.clone());
    for name in cordoned {
        match nodes
            .patch(
                name,
                &PatchParams::default(),
                &Patch::Merge(serde_json::json!({"spec": {"unschedulable": false}})),
            )
            .await
        {
            Ok(_) => info!("uncordoned builder node {name}"),
            Err(e) => warn!(
                "FAILED to uncordon builder node {name}: {e:#} — \
                 fix by hand: kubectl uncordon {name}"
            ),
        }
    }
}
