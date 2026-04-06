//! k3s smoke test: same chaos as EKS, port-forward instead of SSM.

use std::time::Duration;

use anyhow::Result;
use tokio::io::AsyncReadExt;

use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::k8s::eks::smoke as chaos;
use crate::k8s::shared::ProcessGuard;
use crate::{kube, ui};

const LOCAL_PORT: u16 = 2222;

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;

    chaos::step_tenant(&client).await?;
    chaos::step_ssh_key(&client).await?;

    // Port-forward to the gateway Service (instead of SSM→NLB).
    let _tunnel = tunnel().await?;

    chaos::step_workerpool_reconciled(&client).await?;

    let store_url = format!(
        "ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={}",
        chaos::SSH_KEY
    );
    ui::step("trivial build", || async {
        chaos::smoke_build("fast", 5, &store_url)
    })
    .await?;

    chaos::step_status(&client).await?;
    chaos::step_worker_kill(&client, &store_url).await?;

    tracing::info!("SMOKE TEST PASSED");
    Ok(())
}

async fn tunnel() -> Result<ProcessGuard> {
    let child = tokio::process::Command::new("kubectl")
        .args(["-n", NS, "port-forward", "svc/rio-gateway"])
        .arg(format!("{LOCAL_PORT}:22"))
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let guard = ProcessGuard(child);

    ui::poll(
        "port-forward (reading SSH banner)",
        Duration::from_secs(2),
        10,
        || async {
            let fut = async {
                let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", LOCAL_PORT))
                    .await
                    .ok()?;
                let mut buf = [0u8; 12];
                sock.read_exact(&mut buf).await.ok()?;
                buf.starts_with(b"SSH-2.0-").then_some(())
            };
            Ok(tokio::time::timeout(Duration::from_secs(3), fut)
                .await
                .ok()
                .flatten())
        },
    )
    .await?;
    Ok(guard)
}
