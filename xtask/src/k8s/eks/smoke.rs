//! End-to-end smoke test via SSM-tunneled gateway.
//!
//! Replaces `infra/eks/smoke-test.sh`:
//!   1. bootstrap tenant via rio-cli in scheduler leader pod
//!   2. generate SSH key, install as authorized_keys Secret, restart gateway
//!   3. wait NLB target health, start SSM tunnel, read SSH banner
//!   4. build a trivial derivation through ssh-ng://
//!   5. worker-kill chaos: kill a pod mid-build, assert reassign + metric bump

use std::collections::BTreeMap;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ::kube::api::{Api, AttachParams, DeleteParams, ListParams};
use anyhow::{Context, Result, bail};
use k8s_openapi::api::core::v1::{Pod, Service};
use tokio::io::AsyncReadExt;
use tracing::info;

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::NS;
use crate::sh::{cmd, shell};
use crate::{kube, ssh, tofu, ui};

const TENANT: &str = "smoke-test";
pub const SSH_KEY: &str = "/tmp/rio-smoke-key";
const LOCAL_PORT: u16 = 2222;
const POOL: &str = "default";

/// Minimal self-contained derivation (busybox FOD + raw derivation).
/// `@TAG@` and `@SECS@` are replaced at use time.
const SMOKE_EXPR: &str = r#"
let
  busybox = builtins.derivation {
    name = "busybox";
    builder = "builtin:fetchurl";
    system = "builtin";
    url = "http://tarballs.nixos.org/stdenv/x86_64-unknown-linux-gnu/82b583ba2ba2e5706b35dbe23f31362e62be2a9d/busybox";
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
    unpack = false;
  };
in builtins.derivation {
  name = "rio-smoke-@TAG@-${toString builtins.currentTime}";
  system = "x86_64-linux";
  builder = "${busybox}";
  args = ["sh" "-c" "echo @TAG@; read -t @SECS@ x < /dev/zero || true; echo ok > $out"];
}"#;

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;
    let aws = aws_config::load_from_env().await;

    // 1. Bootstrap tenant
    step_tenant(&client).await?;

    // 2. SSH key + Secret + gateway restart
    step_ssh_key(&client).await?;

    // 3. NLB health + SSM tunnel
    let region = tofu::output(TF_DIR, "region")?;
    step_nlb_health(&client, &aws, &region).await?;
    let bastion = tofu::output(TF_DIR, "bastion_instance_id")?;
    let nlb = step_nlb_dns(&client).await?;
    let _tunnel = step_ssm_tunnel(&bastion, &region, &nlb).await?;

    // 4. WorkerPool reconcile
    step_workerpool_reconciled(&client).await?;

    // 5. Trivial build
    let store_url = format!("ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={SSH_KEY}");
    info!("building trivial derivation via {store_url} (cold-start ~2-3min)");
    smoke_build("fast", 5, &store_url)?;
    info!("trivial build OK");

    // 6. rio-cli status sanity
    step_status(&client).await?;

    // 7. Worker-kill chaos
    step_worker_kill(&client, &store_url).await?;

    info!("SMOKE TEST PASSED");
    Ok(())
}

pub async fn step_tenant(client: &kube::Client) -> Result<()> {
    info!("bootstrapping tenant '{TENANT}'");
    let out = sched_exec(client, &["rio-cli", "create-tenant", TENANT]).await?;
    if !out.contains("created") && !out.to_lowercase().contains("already exists") {
        bail!("create-tenant failed: {out}");
    }
    Ok(())
}

pub async fn step_ssh_key(client: &kube::Client) -> Result<()> {
    info!("generating SSH key with comment '{TENANT}'");
    let (priv_key, pub_key) = ssh::generate(TENANT)?;
    std::fs::write(SSH_KEY, &priv_key)?;
    std::fs::set_permissions(SSH_KEY, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(format!("{SSH_KEY}.pub"), &pub_key)?;

    info!("installing authorized_keys");
    kube::apply_secret(
        client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), pub_key)]),
    )
    .await?;

    // authorized_keys is loaded once at startup — no hot-reload.
    kube::rollout_restart(client, NS, "rio-gateway").await?;
    kube::wait_rollout(client, NS, "rio-gateway", Duration::from_secs(120)).await?;
    Ok(())
}

async fn step_nlb_health(
    client: &kube::Client,
    aws: &aws_config::SdkConfig,
    region: &str,
) -> Result<()> {
    // New pod IPs (excluding Terminating — deletionTimestamp set).
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let want: Vec<String> = pods
        .list(&ListParams::default().labels("app.kubernetes.io/name=rio-gateway"))
        .await?
        .items
        .into_iter()
        .filter(|p| p.metadata.deletion_timestamp.is_none())
        .filter_map(|p| p.status?.pod_ip)
        .collect();

    let conf = aws_config::from_env()
        .region(aws_config::Region::new(region.to_string()))
        .load()
        .await;
    let elbv2 = aws_sdk_elasticloadbalancingv2::Client::new(&conf);
    let tgs = elbv2.describe_target_groups().send().await?;
    let tg_arn = tgs
        .target_groups()
        .iter()
        .find(|tg| tg.target_group_name().is_some_and(|n| n.contains("rio")))
        .and_then(|tg| tg.target_group_arn())
        .context("no rio target group found")?
        .to_string();

    let _ = aws; // only needed for region derivation if we didn't pass it

    ui::poll("NLB target health", Duration::from_secs(3), 30, || {
        let elbv2 = elbv2.clone();
        let tg_arn = tg_arn.clone();
        let want = want.clone();
        async move {
            let health = elbv2
                .describe_target_health()
                .target_group_arn(&tg_arn)
                .send()
                .await?;
            let healthy: Vec<&str> = health
                .target_health_descriptions()
                .iter()
                .filter(|d| {
                    d.target_health()
                        .and_then(|h| h.state())
                        .is_some_and(|s| s.as_str() == "healthy")
                })
                .filter_map(|d| d.target()?.id())
                .collect();
            let all = want.iter().all(|ip| healthy.contains(&ip.as_str()));
            Ok(all.then_some(()))
        }
    })
    .await
}

async fn step_nlb_dns(client: &kube::Client) -> Result<String> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), NS);
    ui::poll("NLB provisioning", Duration::from_secs(5), 30, || {
        let svcs = svcs.clone();
        async move {
            Ok(svcs
                .get("rio-gateway")
                .await?
                .status
                .and_then(|s| s.load_balancer?.ingress?.into_iter().next()?.hostname))
        }
    })
    .await
}

/// Spawn SSM tunnel; returns a drop-guard that kills it.
async fn step_ssm_tunnel(
    bastion: &str,
    region: &str,
    nlb: &str,
) -> Result<scopeguard::ScopeGuard<tokio::process::Child, impl FnOnce(tokio::process::Child)>> {
    info!("starting SSM tunnel {bastion} → {nlb}:22 → localhost:{LOCAL_PORT}");
    let child = tokio::process::Command::new("aws")
        .args([
            "ssm",
            "start-session",
            "--region",
            region,
            "--target",
            bastion,
        ])
        .args([
            "--document-name",
            "AWS-StartPortForwardingSessionToRemoteHost",
        ])
        .args([
            "--parameters",
            &format!("host={nlb},portNumber=22,localPortNumber={LOCAL_PORT}"),
        ])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let guard = scopeguard::guard(child, |mut c| {
        let _ = c.start_kill();
    });

    // Read SSH banner to prove the full path works (not just that
    // session-manager-plugin bound the local socket).
    ui::poll(
        "SSM tunnel (reading SSH banner)",
        Duration::from_secs(3),
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

pub async fn step_workerpool_reconciled(client: &kube::Client) -> Result<()> {
    use rio_crds::workerpool::WorkerPool;
    let api: Api<WorkerPool> = Api::namespaced(client.clone(), NS);
    ui::poll(
        &format!("WorkerPool/{POOL} reconcile"),
        Duration::from_secs(5),
        12,
        || {
            let api = api.clone();
            async move {
                Ok(api
                    .get_opt(POOL)
                    .await?
                    .and_then(|wp| wp.status)
                    .map(|_| ()))
            }
        },
    )
    .await
}

pub async fn step_status(client: &kube::Client) -> Result<()> {
    info!("checking cluster status");
    let out = sched_exec(client, &["rio-cli", "status"]).await?;
    // suspend bars before dumping multi-line output — raw println!
    // would freeze a copy of the active bars in scrollback.
    #[allow(clippy::print_stdout)]
    crate::ui::suspend(|| println!("{out}"));
    if !out.contains("worker ") {
        bail!("no workers in status output");
    }
    if !out.contains("build ") {
        bail!("no builds in status output");
    }
    Ok(())
}

pub async fn step_worker_kill(client: &kube::Client, store_url: &str) -> Result<()> {
    info!("capturing disconnect baseline");
    let before = sched_metric(client, "rio_scheduler_worker_disconnects_total").await?;
    info!("baseline: {before}");

    info!("starting background build + worker kill");
    let store_url = store_url.to_string();
    let build = tokio::task::spawn_blocking(move || smoke_build("slow", 180, &store_url));

    use rio_crds::workerpool::WorkerPool;
    let wp: Api<WorkerPool> = Api::namespaced(client.clone(), NS);
    ui::poll(">=2 ready workers", Duration::from_secs(10), 18, || {
        let wp = wp.clone();
        async move {
            let ready = wp
                .get(POOL)
                .await?
                .status
                .map(|s| s.ready_replicas)
                .unwrap_or(0);
            Ok((ready >= 2).then_some(()))
        }
    })
    .await?;

    info!("killing one worker pod");
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let victim = pods
        .list(&ListParams::default().labels(&format!("rio.build/pool={POOL}")))
        .await?
        .items
        .into_iter()
        .next()
        .context("no worker pods found")?
        .metadata
        .name
        .unwrap();
    pods.delete(&victim, &DeleteParams::default()).await?;

    build.await??;
    info!("build survived worker kill");

    let after = sched_metric(client, "rio_scheduler_worker_disconnects_total").await?;
    info!("after: {after}");
    if after <= before {
        bail!("disconnect counter didn't increase (scheduler missed the kill?)");
    }
    Ok(())
}

/// Exec a command in the scheduler leader pod.
async fn sched_exec(client: &kube::Client, cmd: &[&str]) -> Result<String> {
    let leader = kube::scheduler_leader(client, NS).await?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let mut attached = pods
        .exec(
            &leader,
            cmd.iter().copied(),
            &AttachParams::default().stdout(true).stderr(true),
        )
        .await?;
    let mut stdout = attached.stdout().context("no stdout")?;
    let mut stderr = attached.stderr().context("no stderr")?;
    let mut out = String::new();
    let mut err = String::new();
    stdout.read_to_string(&mut out).await?;
    stderr.read_to_string(&mut err).await?;
    attached.join().await?;
    Ok(out + &err)
}

/// Scrape a metric from the scheduler leader via port-forward.
async fn sched_metric(client: &kube::Client, name: &str) -> Result<f64> {
    let leader = kube::scheduler_leader(client, NS).await?;
    let pods: Api<Pod> = Api::namespaced(client.clone(), NS);
    let mut pf = pods.portforward(&leader, &[9091]).await?;
    let stream = pf.take_stream(9091).context("no portforward stream")?;

    // kube's portforward returns a duplex stream; hand-roll a minimal
    // HTTP GET over it (pulling hyper just for one metrics scrape is
    // heavier than 10 lines of HTTP/1.0).
    use tokio::io::AsyncWriteExt;
    let mut stream = stream;
    stream
        .write_all(b"GET /metrics HTTP/1.0\r\nHost: localhost\r\n\r\n")
        .await?;
    let mut body = String::new();
    stream.read_to_string(&mut body).await?;

    for line in body.lines() {
        if let Some(rest) = line.strip_prefix(name)
            && let Some(v) = rest.split_whitespace().last()
        {
            return Ok(v.parse().unwrap_or(0.0));
        }
    }
    Ok(0.0)
}

pub fn smoke_build(tag: &str, secs: u32, store_url: &str) -> Result<()> {
    let expr = SMOKE_EXPR
        .replace("@TAG@", tag)
        .replace("@SECS@", &secs.to_string());
    let sh = shell()?;
    let _env = sh.push_env(
        "NIX_SSHOPTS",
        "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null",
    );
    let drv = cmd!(sh, "nix-instantiate --expr {expr}").read()?;
    let drv = drv.trim();
    info!(
        "  copying closure for {}",
        drv.rsplit('/').next().unwrap_or(drv)
    );
    cmd!(sh, "nix copy --to {store_url} --derivation {drv}").run()?;
    info!("  building");
    let drv_out = format!("{drv}^*");
    cmd!(
        sh,
        "nix build --store {store_url} --no-link --print-out-paths {drv_out}"
    )
    .run()?;
    Ok(())
}
