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
use crate::k8s::shared::ProcessGuard;
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
    let region = tofu::output(TF_DIR, "region")?;
    let bastion = tofu::output(TF_DIR, "bastion_instance_id")?;
    let store_url = format!("ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={SSH_KEY}");

    ui::phase! { "smoke":
        "bootstrap tenant"                            => step_tenant(&client);
        "install ssh key"                             => step_install_key(&client);
        "restart gateway"   [+RESTART_GATEWAY_STEPS]  => step_restart_gateway(&client);
        // NLB target registration + health-check cycle is separate
        // from pod readiness (~30-90s). Wait before starting the SSM
        // tunnel so the bastion's agent doesn't hit "connection to
        // destination port failed" while targets are still `initial`.
        "NLB target health"                           => step_nlb_health(&client, &aws, &region);
        let nlb =
        "NLB DNS"                                     => step_nlb_dns(&client);
        let _tunnel =
        "SSM tunnel"        [+SSM_TUNNEL_STEPS]       => step_ssm_tunnel(&bastion, &region, &nlb);
        "workerpool reconcile"                        => step_workerpool_reconciled(&client);
        "trivial build (cold-start ~2-3min)"
                            [+SMOKE_BUILD_STEPS]      => smoke_build("fast", 5, &store_url);
        "rio-cli status"                              => step_status(&client);
        "worker-kill chaos" [+WORKER_KILL_STEPS]      => step_worker_kill(&client, &store_url);
    }
    .await?;
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

pub async fn step_install_key(client: &kube::Client) -> Result<()> {
    let (priv_key, pub_key) = ssh::generate(TENANT)?;
    std::fs::write(SSH_KEY, &priv_key)?;
    std::fs::set_permissions(SSH_KEY, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(format!("{SSH_KEY}.pub"), &pub_key)?;
    kube::apply_secret(
        client,
        NS,
        "rio-gateway-ssh",
        BTreeMap::from([("authorized_keys".into(), pub_key)]),
    )
    .await
}

/// authorized_keys is loaded once at startup — no hot-reload.
pub const RESTART_GATEWAY_STEPS: u64 = ui::POLL_STEPS; // wait_rollout
pub async fn step_restart_gateway(client: &kube::Client) -> Result<()> {
    kube::rollout_restart(client, NS, "rio-gateway").await?;
    kube::wait_rollout(client, NS, "rio-gateway", Duration::from_secs(120)).await
}

async fn step_nlb_health(
    client: &kube::Client,
    _aws: &aws_config::SdkConfig,
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
    anyhow::ensure!(
        !want.is_empty(),
        "no rio-gateway pod IPs found — label selector mismatch?"
    );
    info!("want healthy: {want:?}");

    let conf = aws_config::from_env()
        .region(aws_config::Region::new(region.to_string()))
        .load()
        .await;
    let elbv2 = aws_sdk_elasticloadbalancingv2::Client::new(&conf);

    // Find the target group by its aws-lbc-applied tag rather than
    // substring-matching the auto-generated name (`k8s-riosyste-
    // riogatew-<hash>`). Name match picks the FIRST "rio"-containing
    // TG, which can be a stale group from a prior cluster.
    let tg_arn = find_gateway_tg(&elbv2).await?;
    info!("target group: {tg_arn}");

    let last_seen = std::sync::Arc::new(std::sync::Mutex::new(String::new()));
    let (ls, w) = (last_seen.clone(), want.clone());
    ui::poll_in(Duration::from_secs(3), 30, move || {
        let elbv2 = elbv2.clone();
        let tg_arn = tg_arn.clone();
        let want = w.clone();
        let ls = ls.clone();
        async move {
            let health = elbv2
                .describe_target_health()
                .target_group_arn(&tg_arn)
                .send()
                .await?;
            // Capture targets+states so a timeout shows what the poll
            // actually saw (feedback_timeout-not-timing: check the
            // actual state, don't assume timing).
            let seen: Vec<(String, String)> = health
                .target_health_descriptions()
                .iter()
                .filter_map(|d| {
                    Some((
                        d.target()?.id()?.to_string(),
                        d.target_health()?.state()?.as_str().to_string(),
                    ))
                })
                .collect();
            *ls.lock().unwrap() = seen
                .iter()
                .map(|(ip, st)| format!("{ip}={st}"))
                .collect::<Vec<_>>()
                .join(" ");

            let healthy: Vec<&str> = seen
                .iter()
                .filter(|(_, st)| st == "healthy")
                .map(|(ip, _)| ip.as_str())
                .collect();
            let all = want.iter().all(|ip| healthy.contains(&ip.as_str()));
            Ok(all.then_some(()))
        }
    })
    .await
    .with_context(|| format!("want={want:?} last_seen=[{}]", last_seen.lock().unwrap()))
}

async fn find_gateway_tg(elbv2: &aws_sdk_elasticloadbalancingv2::Client) -> Result<String> {
    // aws-load-balancer-controller tags each TG it creates with
    // `service.k8s.aws/stack = <ns>/<svc>`. Filter by that instead
    // of substring-matching the auto-generated name.
    let tgs: Vec<_> = elbv2
        .describe_target_groups()
        .into_paginator()
        .items()
        .send()
        .try_collect()
        .await?;
    let arns: Vec<String> = tgs
        .iter()
        .filter_map(|tg| tg.target_group_arn().map(String::from))
        .collect();

    for chunk in arns.chunks(20) {
        let tags = elbv2
            .describe_tags()
            .set_resource_arns(Some(chunk.to_vec()))
            .send()
            .await?;
        for desc in tags.tag_descriptions() {
            let is_gateway = desc.tags().iter().any(|t| {
                t.key() == Some("service.k8s.aws/stack")
                    && t.value() == Some(&format!("{NS}/rio-gateway"))
            });
            if is_gateway && let Some(arn) = desc.resource_arn() {
                return Ok(arn.to_string());
            }
        }
    }
    bail!(
        "no target group tagged service.k8s.aws/stack={NS}/rio-gateway \
         — is aws-load-balancer-controller running?"
    )
}

async fn step_nlb_dns(client: &kube::Client) -> Result<String> {
    let svcs: Api<Service> = Api::namespaced(client.clone(), NS);
    ui::poll_in(Duration::from_secs(5), 30, || {
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
const SSM_TUNNEL_STEPS: u64 = ui::POLL_STEPS; // banner poll
async fn step_ssm_tunnel(bastion: &str, region: &str, nlb: &str) -> Result<ProcessGuard> {
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
    let guard = ProcessGuard(child);

    // Read SSH banner to prove the full path works (not just that
    // session-manager-plugin bound the local socket).
    ui::poll(
        "SSM tunnel (reading SSH banner)",
        Duration::from_secs(3),
        10,
        || async {
            Ok(
                tokio::time::timeout(Duration::from_secs(3), ssh_banner(LOCAL_PORT))
                    .await
                    .ok()
                    .flatten(),
            )
        },
    )
    .await?;
    Ok(guard)
}

/// Connect to `127.0.0.1:port` and read the server's SSH version
/// string. russh waits for the CLIENT's banner before sending its own
/// (RFC 4253 §4.2 doesn't mandate order), so write ours first.
pub async fn ssh_banner(port: u16) -> Option<()> {
    use tokio::io::AsyncWriteExt;
    let mut sock = tokio::net::TcpStream::connect(("127.0.0.1", port))
        .await
        .ok()?;
    sock.write_all(b"SSH-2.0-xtask-probe\r\n").await.ok()?;
    let mut buf = [0u8; 12];
    sock.read_exact(&mut buf).await.ok()?;
    buf.starts_with(b"SSH-2.0-").then_some(())
}

pub async fn step_workerpool_reconciled(client: &kube::Client) -> Result<()> {
    use rio_crds::workerpool::WorkerPool;
    let api: Api<WorkerPool> = Api::namespaced(client.clone(), NS);
    ui::poll_in(Duration::from_secs(5), 12, || {
        let api = api.clone();
        async move {
            Ok(api
                .get_opt(POOL)
                .await?
                .and_then(|wp| wp.status)
                .map(|_| ()))
        }
    })
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

/// baseline + bg-build(+its inner) + >=2-poll + kill + await + verify
pub const WORKER_KILL_STEPS: u64 = 5 + ui::POLL_STEPS + SMOKE_BUILD_STEPS;
pub async fn step_worker_kill(client: &kube::Client, store_url: &str) -> Result<()> {
    let before = ui::step("capture disconnect baseline", || async {
        let b = sched_metric(client, "rio_scheduler_worker_disconnects_total").await?;
        info!("baseline: {b}");
        Ok::<_, anyhow::Error>(b)
    })
    .await?;

    info!("starting background build (180s)");
    let store_url = store_url.to_string();
    // step_owned (not step) so the span is created synchronously here
    // — inside the phase — before spawn moves the future to a worker
    // with no span context.
    let build = tokio::spawn(ui::step_owned("background build".into(), async move {
        smoke_build("slow", 180, &store_url).await
    }));

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

    ui::step("kill worker pod", || async {
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
        info!("victim: {victim}");
        pods.delete(&victim, &DeleteParams::default()).await?;
        Ok::<_, anyhow::Error>(())
    })
    .await?;

    ui::step("await build (should survive reassign)", || async {
        build.await??;
        Ok::<_, anyhow::Error>(())
    })
    .await?;

    ui::step("verify disconnect counter increased", || async {
        let after = sched_metric(client, "rio_scheduler_worker_disconnects_total").await?;
        info!("before={before} after={after}");
        if after <= before {
            bail!("disconnect counter didn't increase (scheduler missed the kill?)");
        }
        Ok(())
    })
    .await
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

/// nix-instantiate + nix copy + nix build
pub const SMOKE_BUILD_STEPS: u64 = 3;
pub async fn smoke_build(tag: &str, secs: u32, store_url: &str) -> Result<()> {
    let expr = SMOKE_EXPR
        .replace("@TAG@", tag)
        .replace("@SECS@", &secs.to_string());
    const SSHOPTS: &str = "-o StrictHostKeyChecking=no -o UserKnownHostsFile=/dev/null";

    // Scope each Shell to end before .await so the future stays Send
    // (Shell is !Sync — RefCell internals). sh::run converts Cmd<'_>
    // to owned Command synchronously, so the borrow on `sh` is
    // released before the returned future is polled.
    let drv = ui::step("nix-instantiate", || {
        let sh = shell().unwrap();
        crate::sh::run_read(cmd!(sh, "nix-instantiate --expr {expr}"))
    })
    .await?;

    ui::step(
        &format!("nix copy {}", drv.rsplit('/').next().unwrap_or(&drv)),
        || {
            let sh = shell().unwrap();
            crate::sh::run(
                cmd!(sh, "nix copy --to {store_url} --derivation {drv}")
                    .env("NIX_SSHOPTS", SSHOPTS),
            )
        },
    )
    .await?;

    let drv_out = format!("{drv}^*");
    ui::step("nix build", || {
        let sh = shell().unwrap();
        crate::sh::run(
            cmd!(
                sh,
                "nix build --store {store_url} --no-link --print-out-paths {drv_out}"
            )
            .env("NIX_SSHOPTS", SSHOPTS),
        )
    })
    .await
}
