//! End-to-end smoke test via port-forwarded gateway.
//!
//! EKS-unique surface only — VM tests (`nix/tests/`) cover the
//! trivial-build path and worker-kill chaos. This validates:
//!   1. bootstrap tenant via rio-cli (port-forwarded scheduler+store)
//!   2. install SSH key, restart gateway
//!   3. NLB target registration + health (aws-lbc reconcile)
//!   4. port-forward gateway:22 (NLB ingress: vm-ingress-v4v6-k3s)
//!   5. large-output build (NAR > 256 KiB → chunked S3 path → IRSA)

use std::io::Write;
use std::os::fd::AsRawFd;
use std::os::unix::fs::PermissionsExt;
use std::time::Duration;

use ::kube::api::{Api, ListParams};
use anyhow::{Context, Result, bail};
use k8s_openapi::api::core::v1::{Pod, Secret};
use nix::sys::memfd::{MFdFlags, memfd_create};
use tokio::io::AsyncReadExt;
use tracing::{debug, info};

use super::TF_DIR;
use crate::config::XtaskConfig;
use crate::k8s::client as kube;
use crate::k8s::shared::ProcessGuard;
use crate::k8s::{NS, NS_BUILDERS, NS_FETCHERS};
use crate::sh::{self, cmd, shell};
use crate::{ssh, tofu, ui};

pub const TENANT: &str = "smoke-test";

/// Default upstream binary cache for the smoke tenant. Without this,
/// substitution is dead (`cache_token=no`), and FODs whose live URL
/// drifted from the pinned hash (I-041 lzip) fail instead of resolving
/// from the cache.
pub const UPSTREAM_URL: &str = "https://cache.nixos.org";
pub const UPSTREAM_KEY: &str = "cache.nixos.org-1:6NCHdD59X431o0gWypbMrAURkbJ16ZPMQFGspcDShjY=";
pub const SSH_KEY: &str = "/tmp/rio-smoke-key";
const LOCAL_PORT: u16 = 2222;
const SCHED_PORT: u16 = 19001;
const STORE_PORT: u16 = 19002;
// Pool names from deploy.rs POOLS_JSON.
const BUILDER_POOL: &str = "x86-64";
const FETCHER_POOL: &str = "x86-64-fetcher";

/// Context for running rio-cli LOCALLY against a port-forwarded
/// scheduler+store. Holds the tunnel guards, the mTLS cert tempdir,
/// and the service-HMAC key memfd — dropping this tears everything
/// down. Fetched once at the top of the phase and threaded through to
/// each step that needs rio-cli (cheaper than re-opening tunnels per
/// step, and keeps `step_tenant`/ `step_status` provider-agnostic).
pub struct CliCtx {
    _guards: ((u16, ProcessGuard), (u16, ProcessGuard)),
    sched: u16,
    store: u16,
    /// Service-identity HMAC key, fetched from the live `rio-service-
    /// hmac` Secret. rio-cli reads `RIO_SERVICE_HMAC_KEY_PATH` and
    /// attaches `x-rio-service-token` to AdminService RPCs (G10 gates
    /// `CreateTenant`/`upstream` on it). Held in an anonymous memfd so
    /// the key never touches disk; child processes read it via
    /// `/dev/fd/N`. `None` when the Secret doesn't exist (dev cluster
    /// without HMAC) — rio-cli then runs tokenless and the server side
    /// admits when its verifier is also `None`.
    hmac_key_fd: Option<std::fs::File>,
}

impl CliCtx {
    /// Open scheduler+store tunnels and fetch the service-HMAC key
    /// from the cluster. Cheap: two `kubectl port-forward` children +
    /// one Secret GET.
    pub async fn open(client: &kube::Client, sched: u16, store: u16) -> Result<Self> {
        let guards = crate::k8s::shared::tunnel_grpc(sched, store).await?;
        // I-101: tunnel_grpc may bind ephemeral ports when 0 was
        // passed — use what kubectl actually bound, not the request.
        let (sched, store) = (guards.0.0, guards.1.0);

        // Raw 32-byte key (openssl rand 32) — NOT UTF-8, so fetch
        // bytes directly rather than via `kube::get_secret_key`.
        let secrets: Api<Secret> = Api::namespaced(client.clone(), NS);
        let hmac_key_fd = match secrets.get_opt("rio-service-hmac").await? {
            Some(s) => {
                let key = s
                    .data
                    .and_then(|d| d.get("service-hmac.key").map(|v| v.0.clone()))
                    .context("Secret rio-service-hmac missing key 'service-hmac.key'")?;
                // memfd_create(2): key only ever lives in anonymous
                // RAM. NO MFD_CLOEXEC — the rio-cli child must inherit
                // the fd to open /dev/fd/N. `HmacSigner::load` reads
                // via `std::fs::read`, which works on /dev/fd paths.
                let fd = memfd_create(c"rio-service-hmac", MFdFlags::empty())?;
                let mut f = std::fs::File::from(fd);
                f.write_all(&key)?;
                Some(f)
            }
            None => {
                debug!("Secret rio-service-hmac not found — rio-cli runs tokenless");
                None
            }
        };

        Ok(Self {
            _guards: guards,
            sched,
            store,
            hmac_key_fd,
        })
    }

    /// Run rio-cli locally with RIO_SCHEDULER_ADDR/RIO_STORE_ADDR/
    /// RIO_SERVICE_HMAC_KEY_PATH set. `rio-cli` on PATH; falls back to
    /// `cargo run -p rio-cli`.
    ///
    /// **Exit-code contract:** non-zero exit propagates as `Err` via
    /// [`sh::try_read`]. Callers that tolerate expected-failure exits
    /// (e.g., `create-tenant` on `AlreadyExists`) must match the `Err`
    /// arm and inspect the error text before propagating. See
    /// [`step_tenant`].
    pub fn run(&self, args: &[&str]) -> Result<String> {
        let sh = shell()?;
        let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{}", self.sched));
        let _e2 = sh.push_env("RIO_STORE_ADDR", format!("localhost:{}", self.store));
        let _e3 = self.hmac_key_fd.as_ref().map(|f| {
            sh.push_env(
                "RIO_SERVICE_HMAC_KEY_PATH",
                format!("/dev/fd/{}", f.as_raw_fd()),
            )
        });
        let on_path = sh::try_read(cmd!(sh, "command -v rio-cli")).is_ok();
        if on_path {
            sh::try_read(cmd!(sh, "rio-cli {args...}"))
        } else {
            sh::try_read(cmd!(sh, "cargo run -q -p rio-cli -- {args...}"))
        }
    }
}

/// Minimal self-contained derivation (busybox FOD + raw derivation).
/// `@TAG@`, `@SECS@`, and `@KB_ITER@` are replaced at use time.
///
/// `out_kb` controls output size — `@KB@` KiB of spaces via sh for+echo (word list + chunk from Rust)
/// (stdenv bootstrap busybox lacks dd, $((arith)), AND printf — CONFIG_SH_MATH=n).
/// `out_kb` ≥ 256 makes the resulting NAR exceed `cas::INLINE_THRESHOLD`
/// (rio-store/src/cas.rs) and take the chunked-S3 path, exercising the
/// store's IRSA credentials. The trivial-build step had a ~3-byte output
/// and silently rode the inline-in-postgres path — an IRSA namespace
/// drift went 3 days undetected (I-006) before this knob was added.
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
  args = ["sh" "-c" "echo @TAG@; read -t @SECS@ x < /dev/zero || true; for _ in @KB_ITER@; do echo @CHUNK@; done > $out"];
}"#;

pub async fn run(_cfg: &XtaskConfig) -> Result<()> {
    let client = kube::client().await?;
    let region = tofu::output(TF_DIR, "region")?;
    let store_url = format!("ssh-ng://rio@localhost:{LOCAL_PORT}?ssh-key={SSH_KEY}");

    ui::step("smoke", || async {
        let cli = ui::step("open cli tunnel", || {
            CliCtx::open(&client, SCHED_PORT, STORE_PORT)
        })
        .await?;
        ui::step("bootstrap tenant", || step_tenant(&cli, TENANT)).await?;
        ui::step("configure upstream cache", || step_upstream(&cli, TENANT)).await?;
        ui::step("install ssh key", || step_install_key(&client)).await?;
        ui::step("restart gateway", || step_restart_gateway(&client)).await?;
        // NLB target registration + health-check cycle is separate
        // from pod readiness (~30-90s). Kept as an EKS-unique aws-lbc
        // assertion even though the build below now goes via port-
        // forward — the actual NLB ingress path is what
        // vm-ingress-v4v6-k3s covers.
        ui::step("NLB target health", || step_nlb_health(&client, &region)).await?;
        let _tunnel = ui::step("port-forward gateway", || gateway_port_forward(LOCAL_PORT)).await?;
        ui::step("builder pool reconcile", || {
            step_pool_reconciled(&client, NS_BUILDERS, BUILDER_POOL)
        })
        .await?;
        // SMOKE_EXPR has a builtin:fetchurl FOD + raw consumer.
        // P0452 hard-split routes the FOD to kind=Fetcher only —
        // without a reconciled fetcher the FOD queues forever.
        ui::step("fetcher pool reconcile", || {
            step_pool_reconciled(&client, NS_FETCHERS, FETCHER_POOL)
        })
        .await?;
        // 1 MiB NAR — well over cas::INLINE_THRESHOLD (256 KiB) —
        // forces PutPath down the chunked-S3 path. Catches store-side
        // S3-credential faults (IRSA drift, bucket policy, endpoint
        // misconfig) that the inline path silently skips. Trivial-
        // build (inline-PG path) and worker-kill chaos are covered by
        // the VM test suite; here we exercise only EKS-unique infra.
        ui::step("large-NAR build (S3 chunked path)", || {
            smoke_build("large", 5, 1024, &store_url)
        })
        .await
    })
    .await?;
    info!("SMOKE TEST PASSED");
    Ok(())
}

pub async fn step_tenant(cli: &CliCtx, tenant: &str) -> Result<()> {
    info!("bootstrapping tenant '{tenant}'");
    // rio-cli exits non-zero on AlreadyExists → CliCtx::run returns Err.
    // Check the error text for idempotent re-run before propagating.
    let out = match cli.run(&["create-tenant", tenant]) {
        Ok(s) => s,
        Err(e) => {
            let msg = format!("{e:#}");
            if msg.to_lowercase().contains("already exists") {
                info!("tenant '{tenant}' already exists (idempotent re-run)");
                return Ok(());
            }
            debug!("create-tenant error text (no already-exists match): {msg}");
            return Err(e);
        }
    };
    // print_tenant format: "tenant <name> (<uuid>)  gc_retention=..."
    if !out.contains(&format!("tenant {tenant}")) {
        bail!("create-tenant failed: {out}");
    }
    Ok(())
}

/// I-092: configure cache.nixos.org as the tenant's upstream
/// substitution cache. Idempotent — checks `upstream list` first.
/// Relies on I-093 (`--tenant` accepts a name).
pub async fn step_upstream(cli: &CliCtx, tenant: &str) -> Result<()> {
    let listed = cli.run(&["upstream", "list", "--tenant", tenant, "--json"])?;
    if listed.contains(UPSTREAM_URL) {
        info!("upstream {UPSTREAM_URL} already configured (idempotent re-run)");
        return Ok(());
    }
    cli.run(&[
        "upstream",
        "add",
        "--tenant",
        tenant,
        "--url",
        UPSTREAM_URL,
        "--trusted-key",
        UPSTREAM_KEY,
    ])?;
    info!("added upstream {UPSTREAM_URL} for tenant '{tenant}'");
    Ok(())
}

pub async fn step_install_key(client: &kube::Client) -> Result<()> {
    let (priv_key, pub_key) = ssh::generate(TENANT)?;
    std::fs::write(SSH_KEY, &priv_key)?;
    std::fs::set_permissions(SSH_KEY, std::fs::Permissions::from_mode(0o600))?;
    std::fs::write(format!("{SSH_KEY}.pub"), &pub_key)?;
    // Merge (not overwrite) so the operator's key (installed by
    // `deploy`) survives — `rsb` keeps working after smoke runs.
    crate::k8s::shared::merge_authorized_key(client, &pub_key).await
}

/// I-109: authorized_keys IS hot-reloaded (~70s: kubelet Secret-
/// refresh ~60s + gateway 10s mtime poll). The smoke test still
/// restarts because it can't afford a 70s wait per key install; for
/// operator key rotation, just `kubectl apply` the Secret and wait.
pub async fn step_restart_gateway(client: &kube::Client) -> Result<()> {
    kube::rollout_restart(client, NS, "rio-gateway").await?;
    kube::wait_rollout(client, NS, "rio-gateway", Duration::from_secs(120)).await
}

async fn step_nlb_health(client: &kube::Client, region: &str) -> Result<()> {
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

    let conf = crate::aws::config(Some(region)).await;
    let elbv2 = aws_sdk_elasticloadbalancingv2::Client::new(conf);

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

            // target-type: instance → target IDs are instance IDs, not
            // pod IPs. Compare counts rather than IDs so this works for
            // both ip and instance modes. With externalTrafficPolicy:
            // Local + instance, only nodes hosting a gateway pod pass
            // the health check, so healthy count == replica count.
            let healthy = seen.iter().filter(|(_, st)| st == "healthy").count();
            Ok((healthy >= want.len()).then_some(()))
        }
    })
    .await
    .with_context(|| format!("want={want:?} last_seen=[{}]", last_seen.lock().unwrap()))
}

/// Poll the gateway TargetGroup until ≥1 target is `healthy`. Lighter
/// than [`step_nlb_health`] (which waits for SPECIFIC pod IPs after a
/// gateway restart) — used by `deploy` so a follow-up `rsb` doesn't
/// race the NLB's ~30-90s registration cycle and hit the "type 80"
/// SSH error against a target group with zero healthy backends.
///
/// 3min timeout: aws-lbc reconcile + initial health-check round is
/// typically ~45s; leaves headroom for a slow node-group.
pub async fn wait_any_target_healthy(region: &str) -> Result<()> {
    let conf = crate::aws::config(Some(region)).await;
    let elbv2 = aws_sdk_elasticloadbalancingv2::Client::new(conf);

    // The TG itself may not exist until aws-lbc reconciles the Service
    // (first deploy). Poll for both existence and health in one loop.
    let last_seen = std::sync::Arc::new(std::sync::Mutex::new(String::from("(no TG yet)")));
    let ls = last_seen.clone();
    ui::poll_in(Duration::from_secs(5), 36, move || {
        let elbv2 = elbv2.clone();
        let ls = ls.clone();
        async move {
            let tg_arn = match find_gateway_tg(&elbv2).await {
                Ok(arn) => arn,
                Err(e) => {
                    *ls.lock().unwrap() = format!("(tg lookup: {e:#})");
                    return Ok(None);
                }
            };
            let health = elbv2
                .describe_target_health()
                .target_group_arn(&tg_arn)
                .send()
                .await?;
            let states: Vec<String> = health
                .target_health_descriptions()
                .iter()
                .filter_map(|d| Some(d.target_health()?.state()?.as_str().to_string()))
                .collect();
            *ls.lock().unwrap() = if states.is_empty() {
                "(no targets registered)".into()
            } else {
                states.join(",")
            };
            Ok(states.iter().any(|s| s == "healthy").then_some(()))
        }
    })
    .await
    .with_context(|| format!("NLB target health: last_seen={}", last_seen.lock().unwrap()))
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

/// `kubectl port-forward svc/rio-gateway local:22` + SSH-banner
/// readiness. Replaces the old bastion→NLB:22 SSM path:
/// `loadBalancerSourceRanges` is set to public CIDRs only, so the
/// bastion's intra-VPC source isn't in that allowlist. Port-forward
/// reaches the pod via the apiserver proxy regardless of NLB ingress
/// rules; the NLB ingress path itself is what vm-ingress-v4v6-k3s
/// covers.
pub async fn gateway_port_forward(local_port: u16) -> Result<ProcessGuard> {
    crate::k8s::shared::kill_port_listeners(local_port);
    let (_, guard) =
        crate::k8s::shared::port_forward(NS, "svc/rio-gateway", local_port, 22).await?;
    ui::poll("reading SSH banner", Duration::from_secs(3), 25, || async {
        Ok(
            tokio::time::timeout(Duration::from_secs(3), ssh_banner(local_port))
                .await
                .ok()
                .flatten(),
        )
    })
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

/// Poll until a namespaced CR exists and its `.status` subresource is set.
async fn wait_cr_status<K>(client: &kube::Client, ns: &str, name: &'static str) -> Result<()>
where
    K: ::kube::Resource<Scope = ::kube::core::NamespaceResourceScope>
        + ::kube::core::object::HasStatus
        + Clone
        + serde::de::DeserializeOwned
        + std::fmt::Debug,
    K::DynamicType: Default,
{
    let api: Api<K> = Api::namespaced(client.clone(), ns);
    ui::poll_in(Duration::from_secs(5), 12, || {
        let api = api.clone();
        async move {
            Ok(api
                .get_opt(name)
                .await?
                .filter(|r| r.status().is_some())
                .map(|_| ()))
        }
    })
    .await
}

pub async fn step_pool_reconciled(
    client: &kube::Client,
    ns: &'static str,
    name: &'static str,
) -> Result<()> {
    use rio_crds::pool::Pool;
    // deploy.rs POOLS_JSON renders one Pool per (arch, kind); poll
    // until the controller writes .status (replicas + conditions).
    // SMOKE_EXPR's builtin:fetchurl FOD queues forever without a
    // reconciled fetcher (P0452 hard-split: FODs never go to builders).
    wait_cr_status::<Pool>(client, ns, name).await
}

/// The pinned-busybox FOD `let`-binding from [`SMOKE_EXPR`], exposed
/// so qa scenarios can compose their own derivations on top of it
/// (e.g. `requiredSystemFeatures`, multi-output, custom name) without
/// duplicating the tarball pin.
pub const BUSYBOX_LET: &str = r#"let busybox = builtins.derivation {
    name = "busybox";
    builder = "builtin:fetchurl";
    system = "builtin";
    url = "http://tarballs.nixos.org/stdenv/x86_64-unknown-linux-gnu/82b583ba2ba2e5706b35dbe23f31362e62be2a9d/busybox";
    outputHashMode = "recursive";
    outputHashAlgo = "sha256";
    outputHash = "sha256-QrTEnQTBM1Y/qV9odq8irZkQSD9uOMbs2Q5NgCvKCNQ=";
    executable = true;
    unpack = false;
  }; in"#;

/// Render the busybox SMOKE_EXPR template. `out_kb >= 256` pushes the
/// NAR over `cas::INLINE_THRESHOLD` and exercises the chunked-S3
/// PutPath — see the doc on [`SMOKE_EXPR`] for why that matters.
pub fn smoke_expr(tag: &str, secs: u32, out_kb: u32) -> String {
    SMOKE_EXPR
        .replace("@TAG@", tag)
        .replace("@SECS@", &secs.to_string())
        .replace("@KB_ITER@", &"x ".repeat(out_kb as usize))
        // 1023 chars + echo's newline = 1024 bytes/iter. No quoting
        // needed (alphanumeric). This busybox lacks printf too.
        .replace("@CHUNK@", &"x".repeat(1023))
}

/// nix-instantiate + nix copy + nix build for an arbitrary expression.
/// The expr must evaluate to a single derivation. Factored out of the
/// busybox-only path so qa scenarios can submit derivations with
/// `requiredSystemFeatures`, multi-output, custom names, etc.
pub async fn build_expr(expr: &str, store_url: &str) -> Result<()> {
    // IdentitiesOnly=yes (see `shared::NIX_SSHOPTS_BASE`): the user's
    // deploy key (comment "default") is in authorized_keys too. Without
    // this, ssh-agent offers that key first → gateway routes the build
    // to tenant "default" instead of the intended one.
    const SSHOPTS: &str = crate::k8s::shared::NIX_SSHOPTS_BASE;

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

/// Busybox build = `build_expr(smoke_expr(...))`. Kept as a convenience
/// for the health-check path where most callers don't care about the
/// expression body.
pub async fn smoke_build(tag: &str, secs: u32, out_kb: u32, store_url: &str) -> Result<()> {
    build_expr(&smoke_expr(tag, secs, out_kb), store_url).await
}
