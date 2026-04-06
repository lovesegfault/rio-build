//! Tear down the EKS deployment.
//!
//! WorkerPool finalizers hold pods → NLB → tofu destroy blocks.
//! Karpenter NodeClaims must also be deleted BEFORE tofu destroys
//! helm_release.karpenter — otherwise EC2 instances are orphaned.

use anyhow::Result;

use super::TF_DIR;
use crate::k8s::NS;
use crate::sh::{cmd, shell};
use crate::{tofu, ui};

pub async fn run() -> Result<()> {
    let sh = shell()?;
    ui::step("delete WorkerPools", || {
        crate::sh::run(cmd!(
            sh,
            "kubectl -n {NS} delete workerpool --all --wait=true --ignore-not-found"
        ))
    })
    .await?;

    ui::step("delete NodeClaims", || async {
        let sh = shell()?;
        let _ = crate::sh::run(cmd!(
            sh,
            "kubectl delete nodeclaim --all --wait=true --ignore-not-found"
        ))
        .await;
        Ok(())
    })
    .await?;

    ui::step("tofu destroy", || async { tofu::destroy(TF_DIR) }).await
}
