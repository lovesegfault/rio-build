//! helm upgrade from the working tree.
//!
//! Reads infra values from tofu outputs, image tag from .rio-image-tag
//! (written by `eks push`). No git roundtrip — chart changes on a dirty
//! tree deploy directly.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::{Context, Result};
use serde_json::json;
use tracing::info;

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::sh::repo_root;
use crate::{helm, kube, ssh, tofu};

pub async fn run(cfg: &XtaskConfig) -> Result<()> {
    let tag = std::fs::read_to_string(repo_root().join(".rio-image-tag"))
        .context("no .rio-image-tag — run `cargo xtask k8s push -p eks` first")?;
    let tag = tag.trim();

    let ecr = tofu::output(TF_DIR, "ecr_registry")?;
    let bucket = tofu::output(TF_DIR, "chunk_bucket_name")?;
    let store_arn = tofu::output(TF_DIR, "store_iam_role_arn")?;
    let scheduler_arn = tofu::output(TF_DIR, "scheduler_iam_role_arn")?;
    let bootstrap_arn = tofu::output(TF_DIR, "bootstrap_iam_role_arn")?;
    let db_arn = tofu::output(TF_DIR, "db_secret_arn")?;
    let db_host = tofu::output(TF_DIR, "db_endpoint")?;
    let region = tofu::output(TF_DIR, "region")?;
    let cluster = tofu::output(TF_DIR, "cluster_name")?;
    let node_role = tofu::output(TF_DIR, "karpenter_node_role_name")?;

    info!("deploy tag={tag} registry={ecr} cluster={cluster}");

    let client = kube::client().await?;

    // CRDs first, server-side apply.
    info!("applying CRDs");
    kube::apply_crds(&client).await?;

    // NodeOverlay CRD comes from the Karpenter chart (terraform-managed).
    // The rio chart renders a NodeOverlay CR — helm install fails with
    // "no matches for kind" if the CRD hasn't established yet.
    kube::wait_crd_established(
        &client,
        "nodeoverlays.karpenter.sh",
        Duration::from_secs(120),
    )
    .await?;

    // Namespace + SSH secret. Namespace is created here (not by the
    // chart — namespace.create=false below) because: (a) the SSH Secret
    // must exist before helm runs; (b) Helm refuses to adopt a namespace
    // it didn't create. PSA label (privileged) is load-bearing: workers
    // need SYS_ADMIN for FUSE mounts.
    let authorized = ssh::authorized_keys(cfg)?;
    kube::ensure_namespace(&client, NS, true).await?;
    kube::apply_secret(
        &client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), authorized)]),
    )
    .await?;

    // Subchart symlink (same requirement as dev apply).
    crate::k8s::shared::chart_deps()?;

    // NLB annotations (previously a --set-json one-liner in bash).
    let nlb_ann = json!({
        "service.beta.kubernetes.io/aws-load-balancer-type": "external",
        "service.beta.kubernetes.io/aws-load-balancer-nlb-target-type": "ip",
        "service.beta.kubernetes.io/aws-load-balancer-scheme": "internal",
        "service.beta.kubernetes.io/aws-load-balancer-attributes": "load_balancing.cross_zone.enabled=true",
        "service.beta.kubernetes.io/aws-load-balancer-listener-attributes.TCP-22": "tcp.idle_timeout.seconds=3600",
    });

    info!("helm upgrade");
    helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
        .namespace(NS)
        .set("namespace.create", "false")
        .set("global.image.registry", &ecr)
        .set("global.image.tag", tag)
        .set("global.region", &region)
        .set("global.logLevel", &cfg.log_level)
        .set("store.chunkBackend.bucket", &bucket)
        .set("scheduler.logS3Bucket", &bucket)
        .set(
            r"store.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
            &store_arn,
        )
        .set(
            r"scheduler.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
            &scheduler_arn,
        )
        .set("externalSecrets.enabled", "true")
        .set("externalSecrets.auroraSecretArn", &db_arn)
        .set("externalSecrets.auroraEndpoint", &db_host)
        .set("bootstrap.enabled", "true")
        .set(
            r"bootstrap.serviceAccount.annotations.eks\.amazonaws\.com/role-arn",
            &bootstrap_arn,
        )
        .set_json("gateway.service.annotations", nlb_ann.to_string())
        .set("karpenter.enabled", "true")
        .set("karpenter.clusterName", &cluster)
        .set("karpenter.nodeRoleName", &node_role)
        .set("workerPool.enabled", "true")
        .wait(Duration::from_secs(600))
        .run()?;
    Ok(())
}
