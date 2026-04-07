//! k3s smoke test: same chaos as EKS, port-forward instead of SSM.

use std::time::Duration;

use anyhow::{Context, Result};
use tokio::io::{AsyncBufReadExt, BufReader};

use crate::config::XtaskConfig;
use crate::k8s::eks::smoke as chaos;
use crate::k8s::shared::ProcessGuard;
use crate::k8s::{NS, NS_STORE};
use crate::{kube, ui};

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
        ui::step("bootstrap tenant", || chaos::step_tenant(&cli)).await?;
        ui::step("configure upstream cache", || chaos::step_upstream(&cli)).await?;
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
        ui::step("trivial build", || {
            chaos::smoke_build("fast", 5, 1, &store_url)
        })
        .await?;
        // 1 MiB NAR — over cas::INLINE_THRESHOLD (256 KiB) — forces
        // the chunked object-store path. On k3s the backend is
        // rook, not S3 — but a misconfigured bucket endpoint
        // or credential fails the same way. See I-006.
        ui::step("large-NAR build", || {
            chaos::smoke_build("large", 5, 1024, &store_url)
        })
        .await?;
        ui::step("rio-cli status", || chaos::step_status(&cli)).await?;
        ui::step("worker-kill chaos", || {
            chaos::step_worker_kill(&client, &store_url)
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

/// Spawn `kubectl port-forward <target> <local>:<remote>` in `ns` and
/// return `(bound_local_port, drop-guard)`. `target` is the full
/// kubectl resource ref (`svc/rio-gateway`, `pod/rio-scheduler-abc`).
///
/// Pass `local = 0` for an ephemeral port: kubectl binds `:0`, the OS
/// picks a free port, and we parse it from the `Forwarding from
/// 127.0.0.1:NNNNN -> REMOTE` stdout line. I-101: fixed local ports
/// made concurrent `xtask k8s cli` invocations race on bind — second
/// one's kubectl failed `address already in use`, surfacing later as a
/// bare `transport error` from rio-cli.
///
/// With `local != 0`, returns `(local, guard)` without waiting for the
/// bind line — callers layer their own readiness poll (SSH banner,
/// TCP-accept) as before.
pub(crate) async fn port_forward(
    ns: &str,
    target: &str,
    local: u16,
    remote: u16,
) -> Result<(u16, ProcessGuard)> {
    let mut cmd = tokio::process::Command::new("kubectl");
    cmd.args(["-n", ns, "port-forward", target])
        .arg(format!("{local}:{remote}"))
        .stderr(std::process::Stdio::null());
    if local != 0 {
        cmd.stdout(std::process::Stdio::null());
        return Ok((local, ProcessGuard::spawn(cmd)?));
    }
    cmd.stdout(std::process::Stdio::piped());
    let mut guard = ProcessGuard::spawn(cmd)?;
    let stdout = guard.child.stdout.take().expect("piped above");
    let mut lines = BufReader::new(stdout).lines();
    let bound = loop {
        let Some(line) = lines.next_line().await? else {
            anyhow::bail!("kubectl port-forward {target} exited before binding");
        };
        // First line: `Forwarding from 127.0.0.1:NNNNN -> REMOTE`.
        // (Second is `[::1]:NNNNN`; per-conn `Handling connection` follows.)
        if let Some(rest) = line.strip_prefix("Forwarding from 127.0.0.1:") {
            break rest
                .split_whitespace()
                .next()
                .and_then(|s| s.parse().ok())
                .with_context(|| format!("unparseable port-forward line: {line}"))?;
        }
    };
    // Drain the rest so kubectl never blocks on a full pipe.
    tokio::spawn(async move { while lines.next_line().await.ok().flatten().is_some() {} });
    Ok((bound, guard))
}

/// Look up the scheduler leader pod from the `rio-scheduler-leader`
/// Lease. Port-forwarding to `svc/rio-scheduler` load-balances across
/// all replicas; standbys reject writes with "not leader". Targeting
/// the leader pod directly makes admin ops deterministic.
///
/// Also used for the metrics forward (port 9091): the Service spec
/// only exposes 9001, so `kubectl port-forward svc/rio-scheduler
/// X:9091` errors with "does not have a service port 9091" — must
/// target the pod. See I-050.
pub(crate) async fn scheduler_leader_pod() -> Result<String> {
    let sh = crate::sh::shell()?;
    // xshell cmd! interpolates {var} — pass jsonpath as a var to avoid
    // brace-escaping gymnastics.
    let jp = "jsonpath={.spec.holderIdentity}";
    let holder = crate::sh::read(xshell::cmd!(
        sh,
        "kubectl -n {NS} get lease rio-scheduler-leader -o {jp}"
    ))?;
    anyhow::ensure!(
        !holder.is_empty(),
        "rio-scheduler-leader Lease has no holder"
    );
    Ok(format!("pod/{holder}"))
}

/// Port-forward scheduler:9001 + store:9002, wait for TCP accept on both.
/// Shared by all three providers — kubectl reaches the apiserver proxy
/// regardless of whether that's via k3s loopback or `aws eks
/// update-kubeconfig`. ADR-019: scheduler is in rio-system, store in
/// rio-store — per-service `-n`. Scheduler forward targets the leader
/// pod (from the Lease) because standbys reject admin writes.
///
/// Returns `((sched_port, guard), (store_port, guard))` — the bound
/// local ports may differ from the inputs when `0` (ephemeral) was
/// passed. Callers must use the RETURNED ports for the connection.
pub async fn tunnel_grpc(
    sched_port: u16,
    store_port: u16,
) -> Result<((u16, ProcessGuard), (u16, ProcessGuard))> {
    let leader = scheduler_leader_pod().await?;
    let sched = port_forward(NS, &leader, sched_port, 9001).await?;
    let store = port_forward(NS_STORE, "svc/rio-store", store_port, 9002).await?;
    let (sp, tp) = (sched.0, store.0);
    ui::poll(
        "scheduler+store TCP accept",
        Duration::from_secs(2),
        10,
        || async {
            // gRPC has no greeting — bare connect is the only signal.
            let s = tokio::net::TcpStream::connect(("127.0.0.1", sp)).await;
            let t = tokio::net::TcpStream::connect(("127.0.0.1", tp)).await;
            Ok((s.is_ok() && t.is_ok()).then_some(()))
        },
    )
    .await?;
    Ok((sched, store))
}
