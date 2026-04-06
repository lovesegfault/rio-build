//! k3s smoke test: same chaos as EKS, port-forward instead of SSM.

use std::time::Duration;

use anyhow::Result;

use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::eks::smoke as chaos;
use crate::k8s::shared::ProcessGuard;
use crate::{kube, ui};

const LOCAL_PORT: u16 = 2222;

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;
    let store_url = format!(
        "ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={}",
        chaos::SSH_KEY
    );

    ui::phase! { "smoke":
        "bootstrap tenant"                                => chaos::step_tenant(&client);
        "install ssh key"                                 => chaos::step_install_key(&client);
        "restart gateway"  [+chaos::RESTART_GATEWAY_STEPS] => chaos::step_restart_gateway(&client);
        // Port-forward to the gateway Service (instead of SSM→NLB).
        let _tunnel =
        "establish tunnel" [+TUNNEL_STEPS]                => tunnel(LOCAL_PORT);
        "builderpool reconcile"                            => chaos::step_workerpool_reconciled(&client);
        "trivial build"    [+chaos::SMOKE_BUILD_STEPS]    => chaos::smoke_build("fast", 5, &store_url);
        "rio-cli status"                                  => chaos::step_status(&client);
        "worker-kill chaos" [+chaos::WORKER_KILL_STEPS]   => chaos::step_worker_kill(&client, &store_url);
    }
    .await?;
    tracing::info!("SMOKE TEST PASSED");
    Ok(())
}

pub const TUNNEL_STEPS: u64 = ui::POLL_STEPS; // banner poll
pub async fn tunnel(local_port: u16) -> Result<ProcessGuard> {
    let child = tokio::process::Command::new("kubectl")
        .args(["-n", NS, "port-forward", "svc/rio-gateway"])
        .arg(format!("{local_port}:22"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let guard = ProcessGuard(child);

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
