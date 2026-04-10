//! k3s smoke test: same chaos as EKS, port-forward instead of SSM.

use std::time::Duration;

use anyhow::Result;

use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::client as kube;
use crate::k8s::eks::smoke as chaos;
use crate::k8s::shared::ProcessGuard;
use crate::ui;

use crate::k8s::shared::port_forward;

const LOCAL_PORT: u16 = 2222;
const SCHED_PORT: u16 = 19001;
const STORE_PORT: u16 = 19002;

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;
    let store_url = format!(
        "ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={}",
        chaos::SSH_KEY
    );

    ui::step("smoke", || async {
        let cli = ui::step("open cli tunnel", || {
            chaos::CliCtx::open(&client, SCHED_PORT, STORE_PORT)
        })
        .await?;
        ui::step("bootstrap tenant", || chaos::step_tenant(&cli, chaos::TENANT)).await?;
        ui::step("configure upstream cache", || chaos::step_upstream(&cli, chaos::TENANT)).await?;
        ui::step("install ssh key", || chaos::step_install_key(&client)).await?;
        ui::step("restart gateway", || chaos::step_restart_gateway(&client)).await?;
        // Port-forward to the gateway Service (instead of SSM→NLB).
        let _tunnel = ui::step("establish tunnel", || tunnel(LOCAL_PORT)).await?;
        ui::step("builderpool reconcile", || {
            chaos::step_builderpoolset_reconciled(&client)
        })
        .await?;
        ui::step("fetcherpool reconcile", || {
            chaos::step_fetcherpool_reconciled(&client)
        })
        .await?;
        // 1 MiB NAR — over cas::INLINE_THRESHOLD (256 KiB) — forces
        // the chunked object-store path. On k3s the backend is rook,
        // not S3 — but a misconfigured bucket endpoint or credential
        // fails the same way (I-006). Trivial-build + worker-kill
        // chaos are covered by the VM test suite.
        ui::step("large-NAR build", || {
            chaos::smoke_build("large", 5, 1024, &store_url)
        })
        .await
    })
    .await?;
    tracing::info!("SMOKE TEST PASSED");
    Ok(())
}

pub async fn tunnel(local_port: u16) -> Result<ProcessGuard> {
    let (_, guard) = port_forward(NS, "svc/rio-gateway", local_port, 22).await?;
    ui::poll("reading SSH banner", Duration::from_secs(2), 10, || async {
        Ok(
            tokio::time::timeout(Duration::from_secs(3), chaos::ssh_banner(local_port))
                .await
                .ok()
                .flatten(),
        )
    })
    .await?;
    Ok(guard)
}
