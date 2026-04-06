//! kind provider: docker-in-docker cluster, RustFS S3, bitnami PG.
//!
//! Faster than k3s for iteration: no sudo, ~30s cluster creation,
//! RustFS standalone (~10s) vs Rook (~2-5min). Multi-node by default
//! so worker-kill chaos can observe cross-node reassignment.

use std::collections::BTreeMap;
use std::time::Duration;

use anyhow::Result;
use async_trait::async_trait;
use tracing::info;

use crate::config::XtaskConfig;
use crate::k8s::provider::{BuiltImages, Provider, StepCounts};
use crate::k8s::{NS, ensure_namespaces, shared};
use crate::sh::{self, cmd, shell};
use crate::{helm, kube, ssh, ui};

mod push;

pub const CLUSTER: &str = "rio-dev";

pub struct Kind;

const PROVISION_STEPS: u64 = 3; // create + pids-limit + kubeconfig
const BUILD_STEPS: u64 = 1; // nix build (single arch)
const DEPLOY_STEPS: u64 = 9; // chart-deps + CRDs + ssh + pg-secret + jwt + helm + restart + rustfs-wait + bucket

#[async_trait(?Send)]
impl Provider for Kind {
    fn step_counts(&self) -> StepCounts {
        StepCounts {
            provision: PROVISION_STEPS,
            build: BUILD_STEPS,
            push: shared::IMAGE_COUNT,
            deploy: DEPLOY_STEPS,
            smoke: 0,
        }
    }

    fn context_matches(&self, ctx: &str) -> bool {
        ctx == format!("kind-{CLUSTER}")
    }

    async fn bootstrap(&self, _cfg: &XtaskConfig) -> Result<()> {
        info!("bootstrap: no-op for kind (no terraform state)");
        Ok(())
    }

    async fn provision(&self, _cfg: &XtaskConfig, _auto: bool, nodes: u8) -> Result<()> {
        let sh = shell()?;

        // Idempotent: skip create if the cluster already exists.
        let existing = sh::read(cmd!(sh, "kind get clusters"))?;
        if existing.lines().any(|l| l.trim() == CLUSTER) {
            ui::step_skip("kind create cluster", "already exists");
        } else {
            let cfg = kind_config(nodes);
            let cfg_file = tempfile::NamedTempFile::new()?;
            std::fs::write(cfg_file.path(), &cfg)?;
            let cfg_path = cfg_file.path().to_str().unwrap();

            ui::step(&format!("kind create cluster ({nodes} nodes)"), || {
                sh::run(cmd!(
                    sh,
                    "kind create cluster --name {CLUSTER} --config {cfg_path} --wait 120s"
                ))
            })
            .await?;
        }

        // Docker defaults kind node containers to PidsLimit ~2048. On
        // many-core hosts, tokio spawns worker_threads = CPU count per
        // binary; rustfs adds 16384 blocking threads. The NODE container's
        // pid cgroup caps ALL pods inside → "Resource temporarily
        // unavailable" on thread spawn. podPidsLimit in kubelet config
        // only controls POD cgroups, not the node container itself.
        // Idempotent: runs on both fresh and existing clusters.
        ui::step("raise node pids limit", || async {
            let containers = sh::read(cmd!(
                sh,
                "docker ps --filter label=io.x-k8s.kind.cluster={CLUSTER} -q"
            ))?;
            for id in containers.lines() {
                sh::run_sync(cmd!(sh, "docker update --pids-limit -1 {id}"))?;
            }
            Ok(())
        })
        .await?;

        ui::step("kubeconfig", || async {
            let dst = sh::kubeconfig_path();
            std::fs::create_dir_all(dst.parent().unwrap())?;
            let dst_s = dst.to_str().unwrap();
            sh::run_sync(cmd!(
                sh,
                "kind export kubeconfig --name {CLUSTER} --kubeconfig {dst_s}"
            ))
        })
        .await
    }

    async fn kubeconfig(&self, _cfg: &XtaskConfig) -> Result<()> {
        let sh = shell()?;
        let dst = sh::kubeconfig_path();
        std::fs::create_dir_all(dst.parent().unwrap())?;
        let dst_s = dst.to_str().unwrap();
        sh::run_sync(cmd!(
            sh,
            "kind export kubeconfig --name {CLUSTER} --kubeconfig {dst_s}"
        ))
    }

    async fn build(&self, cfg: &XtaskConfig) -> Result<BuiltImages> {
        push::build(cfg).await
    }

    async fn push(&self, images: &BuiltImages, cfg: &XtaskConfig) -> Result<()> {
        push::push(images, cfg).await
    }

    async fn deploy(&self, cfg: &XtaskConfig, log_level: &str, tenant: Option<&str>) -> Result<()> {
        let client = kube::client().await?;

        ui::step("chart deps", || async { shared::chart_deps() }).await?;
        ui::step("apply CRDs", || kube::apply_crds(&client)).await?;

        ui::step("namespaces + ssh secret", || async {
            ensure_namespaces(&client).await?;
            let authorized = ssh::authorized_keys(cfg, tenant)?;
            kube::apply_secret(
                &client,
                NS,
                "rio-gateway-ssh",
                BTreeMap::from([("authorized_keys".into(), authorized)]),
            )
            .await
        })
        .await?;

        ui::step("postgres secret", || shared::ensure_pg_secrets(&client)).await?;

        let jwt = ui::step("jwt keypair", || shared::ensure_jwt_keypair(&client)).await?;

        // nix/docker.nix hardcodes tag="dev" in the tarballs. kind load
        // image-archive imports with that baked-in tag — no retag step.
        // The git-SHA tag from BuiltImages.tag is for EKS where skopeo
        // retags on push. Same tag → Deployment spec unchanged → kube
        // won't re-pull on its own; forced restart below handles that.
        let was_installed = helm::release_status("rio", NS)?.is_some();
        ui::step("helm install rio", || async {
            helm::Helm::upgrade_install("rio", "infra/helm/rio-build")
                .namespace(NS)
                .values("infra/helm/rio-build/values/kind.yaml")
                .set("global.image.tag", "dev")
                .set("global.logLevel", log_level)
                .set("postgresql.auth.existingSecret", "rio-postgres-auth")
                .set("jwt.enabled", "true")
                .set("jwt.signingSeed", &jwt.seed)
                .set("jwt.publicKey", &jwt.pubkey)
                .wait(Duration::from_secs(300))
                .run()
        })
        .await?;

        if was_installed {
            ui::step("rollout restart (same-tag push)", || {
                shared::rollout_restart_rio(&client)
            })
            .await?;
        } else {
            ui::step_skip("rollout restart", "first install");
        }

        // RustFS subchart starts with helm install. Wait for it, then
        // create the bucket (RustFS doesn't auto-create like some S3s).
        kube::wait_rollout(&client, NS, "rio-rustfs", Duration::from_secs(120)).await?;
        ui::step("create rio-chunks bucket", || create_bucket(&client)).await
    }

    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()> {
        // Port-forward tunnel works identically to k3s.
        super::k3s::smoke::run(cfg).await
    }

    async fn tunnel(&self, local_port: u16) -> Result<shared::ProcessGuard> {
        super::k3s::smoke::tunnel(local_port).await
    }

    async fn destroy(&self, _cfg: &XtaskConfig) -> Result<()> {
        let sh = shell()?;
        ui::step("kind delete cluster", || {
            sh::run(cmd!(sh, "kind delete cluster --name {CLUSTER}"))
        })
        .await
    }
}

/// Generate a kind cluster config with 1 control-plane + (nodes-1)
/// workers. nodes=1 means control-plane only (kind removes the
/// NoSchedule taint so workloads still run).
fn kind_config(nodes: u8) -> String {
    // podPidsLimit: tokio reads available_parallelism() = host CPU
    // count. On many-core hosts (e.g. 192), each rio pod spawns that
    // many threads; rustfs adds 16384 blocking threads. kubelet's
    // default podPidsLimit (~4096 on most kind configs) isn't enough.
    // -1 disables the per-pod limit — fine for local dev; EKS handles
    // this differently per-node.
    let kubelet_patch = "  kubeadmConfigPatches:\n  \
         - |\n    \
           kind: KubeletConfiguration\n    \
           podPidsLimit: -1\n";
    let mut cfg = String::from(
        "kind: Cluster\n\
         apiVersion: kind.x-k8s.io/v1alpha4\n\
         nodes:\n\
         - role: control-plane\n",
    );
    cfg.push_str(kubelet_patch);
    // Label workers alternately builder/fetcher so BuilderPool/
    // FetcherPool reconciler nodeSelectors match. With 2 workers
    // (the default 3-node setup) you get one of each. More workers =
    // round-robin. Both roles also work as fallback for the other
    // since there's no taint — the selector just prefers placement.
    let roles = ["builder", "fetcher"];
    for i in 0..(nodes.saturating_sub(1)) {
        let role = roles[i as usize % roles.len()];
        cfg.push_str("- role: worker\n");
        cfg.push_str(&format!("  labels:\n    rio.build/node-role: {role}\n"));
        cfg.push_str(kubelet_patch);
    }
    cfg
}

/// Port-forward to RustFS and create the rio-chunks bucket.
/// Same pattern as k3s's rook::s3_bridge but against RustFS.
async fn create_bucket(_client: &kube::Client) -> Result<()> {
    let sh = shell()?;
    // RustFS chart Service template appends `-svc` to the fullname.
    let pf = std::process::Command::new("kubectl")
        .args(["-n", NS, "port-forward", "svc/rio-rustfs-svc", "19000:9000"])
        .stdout(std::process::Stdio::null())
        .stderr(std::process::Stdio::null())
        .spawn()?;
    let _guard = scopeguard::guard(pf, |mut c| {
        let _ = c.kill();
    });
    // Brief wait for port-forward to establish.
    tokio::time::sleep(Duration::from_secs(1)).await;

    let _e1 = sh.push_env("AWS_ACCESS_KEY_ID", "rustfsadmin");
    let _e2 = sh.push_env("AWS_SECRET_ACCESS_KEY", "rustfsadmin");
    // Idempotent: discard mb's error when the bucket already exists.
    let _ = sh::run_sync(cmd!(
        sh,
        "aws --endpoint-url http://localhost:19000 s3 mb s3://rio-chunks"
    ));
    Ok(())
}
