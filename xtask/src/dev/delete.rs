//! Teardown: helm uninstall + ns cleanup.

use anyhow::Result;
use tracing::info;

use crate::sh::{cmd, shell};
use crate::{helm, kube};

pub async fn delete() -> Result<()> {
    helm::uninstall("rio", "rio-system")?;
    let client = kube::client().await?;
    kube::delete_secret(&client, "rio-system", "rio-gateway-ssh").await?;
    kube::delete_secret(&client, "rio-system", "rio-s3-creds").await?;
    Ok(())
}

/// Full teardown including Rook. Delete in reverse order so cleanup
/// finalizers don't hang.
pub async fn destroy() -> Result<()> {
    delete().await?;
    info!("tearing down rook cluster");
    helm::uninstall("rook-ceph-cluster", "rook-ceph")?;

    // Cluster finalizer waits for data deletion. If it hangs, the Rook
    // docs say to patch the finalizers away; we just best-effort wait.
    let sh = shell()?;
    let _ = cmd!(
        sh,
        "kubectl -n rook-ceph wait --for=delete cephcluster/rook-ceph --timeout=300s"
    )
    .run();

    helm::uninstall("rook-ceph", "rook-ceph")?;
    cmd!(sh, "kubectl delete ns rook-ceph --ignore-not-found").run()?;
    Ok(())
}
