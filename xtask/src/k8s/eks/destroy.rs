//! Tear down the EKS deployment.
//!
//! WorkerPool finalizers hold pods → NLB → tofu destroy blocks.
//! Karpenter NodeClaims must also be deleted BEFORE tofu destroys
//! helm_release.karpenter — otherwise EC2 instances are orphaned.

use anyhow::Result;
use tracing::info;

use super::TF_DIR;
use crate::k8s::NS;
use crate::sh::{cmd, shell};
use crate::tofu;

pub async fn run() -> Result<()> {
    let sh = shell()?;
    info!("deleting WorkerPools (releases finalizers)");
    cmd!(
        sh,
        "kubectl -n {NS} delete workerpool --all --wait=true --ignore-not-found"
    )
    .run()?;

    info!("deleting NodeClaims (releases EC2 instances)");
    let _ = cmd!(
        sh,
        "kubectl delete nodeclaim --all --wait=true --ignore-not-found"
    )
    .quiet()
    .run();

    tofu::destroy(TF_DIR)
}
