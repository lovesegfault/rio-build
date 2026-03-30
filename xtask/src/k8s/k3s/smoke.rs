//! k3s smoke test: same chaos as EKS, port-forward instead of SSM.

use std::time::Duration;

use anyhow::Result;

use crate::config::XtaskConfig;
use crate::k8s::eks::smoke as chaos;
use crate::k8s::shared::ProcessGuard;
use crate::k8s::{NS, NS_STORE};
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
        "fetcherpool reconcile"                            => chaos::step_fetcherpool_reconciled(&client);
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
    let guard = port_forward(NS, "rio-gateway", local_port, 22)?;
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

/// Spawn `kubectl port-forward svc/<svc> <local>:<remote>` in `ns` and
/// return a drop-guard. Does NOT wait for readiness — callers layer
/// their own poll (SSH banner for gateway, TCP-accept for gRPC).
fn port_forward(ns: &str, svc: &str, local: u16, remote: u16) -> Result<ProcessGuard> {
    let child = tokio::process::Command::new("kubectl")
        .args(["-n", ns, "port-forward", &format!("svc/{svc}")])
        .arg(format!("{local}:{remote}"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    Ok(ProcessGuard(child))
}

/// Port-forward scheduler:9001 + store:9002, wait for TCP accept on both.
/// Shared by all three providers — kubectl reaches the apiserver proxy
/// regardless of whether that's via kind/k3s loopback or `aws eks
/// update-kubeconfig`. ADR-019: scheduler is in rio-system, store in
/// rio-store — per-service `-n`.
pub async fn tunnel_grpc(sched_port: u16, store_port: u16) -> Result<(ProcessGuard, ProcessGuard)> {
    let sched = port_forward(NS, "rio-scheduler", sched_port, 9001)?;
    let store = port_forward(NS_STORE, "rio-store", store_port, 9002)?;
    ui::poll(
        "scheduler+store TCP accept",
        Duration::from_secs(2),
        10,
        || async {
            // gRPC has no greeting — bare connect is the only signal.
            let s = tokio::net::TcpStream::connect(("127.0.0.1", sched_port)).await;
            let t = tokio::net::TcpStream::connect(("127.0.0.1", store_port)).await;
            Ok((s.is_ok() && t.is_ok()).then_some(()))
        },
    )
    .await?;
    Ok((sched, store))
}
