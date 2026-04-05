//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::time::Duration;

use anyhow::{Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::config::XtaskConfig;
use crate::{helm, kube, sh, ui};

mod chaos;
mod eks;
mod k3s;
mod kind;
mod metrics;
pub mod provider;
pub mod shared;
pub(crate) mod status;
mod stress;

pub use eks::ami::AmiArch;
use provider::{Provider, ProviderKind};
use tracing::{debug, info};

/// Control-plane namespace. Helm release anchors here; scheduler,
/// gateway, controller, and the SSH/postgres secrets live here. Code
/// that means "the rio namespace" uses this const — only multi-ns-aware
/// code iterates NAMESPACES.
pub const NS: &str = "rio-system";

/// Store namespace. ADR-019: store moved out of rio-system to run at
/// PSA baseline. Secrets the store reads (rio-postgres) need a copy
/// here.
pub const NS_STORE: &str = "rio-store";

/// Builder namespace. ADR-019: builders need CAP_SYS_ADMIN (FUSE),
/// which PSA baseline rejects. Builders are airgapped (NetworkPolicy).
pub const NS_BUILDERS: &str = "rio-builders";

/// Fetcher namespace. ADR-019: same FUSE story as builders, but with
/// open internet egress on 80/443 (FOD fetchurl/git).
pub const NS_FETCHERS: &str = "rio-fetchers";

/// All four rio namespaces and whether each needs the `privileged` PSA
/// label. Providers iterate this at deploy time so the namespaces exist
/// before `helm upgrade` (which renders cross-ns resources into them).
pub const NAMESPACES: &[(&str, bool)] = &[
    (NS, false),
    (NS_STORE, false),
    (NS_BUILDERS, true),
    (NS_FETCHERS, true),
];

/// Ensure all four rio namespaces exist with the right PSA label.
/// Called at the start of every provider's deploy() so cross-ns chart
/// resources (store.yaml → rio-store, builderpool.yaml → rio-builders,
/// etc.) have somewhere to land.
pub async fn ensure_namespaces(client: &kube::Client) -> anyhow::Result<()> {
    for &(ns, privileged) in NAMESPACES {
        kube::ensure_namespace(client, ns, privileged).await?;
    }
    Ok(())
}

/// Ordered bring-up phases for `k8s up`. `up` with no phase flags runs
/// the full sequence; flags select a subset (still in this order).
#[derive(Copy, Clone, Eq, PartialEq, Debug)]
pub enum Phase {
    Bootstrap,
    Provision,
    Kubeconfig,
    Ami,
    Push,
    Deploy,
    Envoy,
}

impl Phase {
    /// Canonical order. ami precedes push: deploy reads `.rio-ami-tag`,
    /// and if up is interrupted after push the operator's reflex is
    /// `up --deploy`, which then finds both tags present.
    pub const ALL: [Phase; 7] = [
        Phase::Bootstrap,
        Phase::Provision,
        Phase::Kubeconfig,
        Phase::Ami,
        Phase::Push,
        Phase::Deploy,
        Phase::Envoy,
    ];

    fn name(self) -> &'static str {
        match self {
            Phase::Bootstrap => "bootstrap",
            Phase::Provision => "provision",
            Phase::Kubeconfig => "kubeconfig",
            Phase::Ami => "ami",
            Phase::Push => "push",
            Phase::Deploy => "deploy",
            Phase::Envoy => "envoy",
        }
    }
}

#[derive(Args, Default)]
pub struct UpOpts {
    /// tofu state bucket (eks). No-op on k3s/kind.
    #[arg(long)]
    bootstrap: bool,
    /// tofu apply (eks) | rook install (k3s) | kind create cluster.
    #[arg(long)]
    provision: bool,
    /// aws eks update-kubeconfig | k3s.yaml copy | kind export kubeconfig.
    #[arg(long)]
    kubeconfig: bool,
    /// Build + register the NixOS node AMI (ADR-021). EKS-only;
    /// silently skipped on k3s/kind in the all-phases path, hard error
    /// when explicitly requested.
    #[arg(long)]
    ami: bool,
    /// Build + push docker images (ECR | ctr import | kind load).
    #[arg(long)]
    push: bool,
    /// helm upgrade rio chart.
    #[arg(long)]
    deploy: bool,
    /// Install Envoy Gateway operator (dashboard gRPC-Web).
    #[arg(long)]
    envoy: bool,

    // Namespaced per-phase opts. NO clap `requires` attribute —
    // validated at runtime by `validate_phase_opts()` so the
    // no-flags=all-phases path can still pass them.
    /// AMI architecture(s) to build. EKS-only.
    #[arg(long = "ami-arch", value_enum, default_value_t = AmiArch::All)]
    ami_arch: AmiArch,
    /// Tenant name for the authorized_keys comment. Overrides
    /// RIO_SSH_TENANT. When neither is set, preserves the existing
    /// Secret's comment (I-100); falls back to "default" only on
    /// first deploy.
    #[arg(long = "deploy-tenant")]
    deploy_tenant: Option<String>,
    /// RUST_LOG directive for deployed pods (e.g. "info,rio_scheduler=trace").
    /// Defaults to RIO_LOG_LEVEL env / `config::RIO_DEBUG`.
    #[arg(long = "deploy-log-level", value_name = "DIRECTIVE")]
    deploy_log_level: Option<String>,
    /// Skip the pre-deploy cluster health check (eks).
    #[arg(long = "deploy-skip-preflight")]
    deploy_skip_preflight: bool,
    /// Cluster node count (kind only: 1 control + N-1 workers).
    #[arg(long = "provision-nodes", value_parser = clap::value_parser!(u8).range(1..))]
    provision_nodes: Option<u8>,

    /// Skip interactive confirmation prompts (tofu apply diff).
    #[arg(long)]
    yes: bool,
}

impl UpOpts {
    fn has(&self, p: Phase) -> bool {
        match p {
            Phase::Bootstrap => self.bootstrap,
            Phase::Provision => self.provision,
            Phase::Kubeconfig => self.kubeconfig,
            Phase::Ami => self.ami,
            Phase::Push => self.push,
            Phase::Deploy => self.deploy,
            Phase::Envoy => self.envoy,
        }
    }

    /// No phase flags → full canonical sequence. Any phase flag →
    /// only the flagged ones, still in canonical order.
    fn phases(&self) -> Vec<Phase> {
        let any = Phase::ALL.iter().any(|&p| self.has(p));
        if !any {
            return Phase::ALL.to_vec();
        }
        Phase::ALL.into_iter().filter(|&p| self.has(p)).collect()
    }

    /// Namespaced-opt validation: only enforce "X requires --phase"
    /// when at least one phase flag is set. `up --deploy-tenant foo`
    /// (no phase flags) is fine — all phases run, deploy uses the
    /// tenant. `up --push --deploy-tenant foo` errors: phase flags are
    /// explicit and `--deploy` isn't among them.
    fn validate_phase_opts(&self, selected: &[Phase]) -> Result<()> {
        let explicit = selected.len() != Phase::ALL.len();
        if !explicit {
            return Ok(());
        }
        macro_rules! req {
            ($cond:expr, $opt:literal, $phase:expr) => {
                if $cond && !selected.contains(&$phase) {
                    bail!(
                        "{} requires --{} (or omit phase flags to run all)",
                        $opt,
                        $phase.name()
                    );
                }
            };
        }
        req!(
            self.deploy_tenant.is_some(),
            "--deploy-tenant",
            Phase::Deploy
        );
        req!(
            self.deploy_log_level.is_some(),
            "--deploy-log-level",
            Phase::Deploy
        );
        req!(
            self.deploy_skip_preflight,
            "--deploy-skip-preflight",
            Phase::Deploy
        );
        req!(
            !matches!(self.ami_arch, AmiArch::All),
            "--ami-arch",
            Phase::Ami
        );
        req!(
            self.provision_nodes.is_some(),
            "--provision-nodes",
            Phase::Provision
        );
        Ok(())
    }
}

const RENAME_HINTS: &str = "\
RENAMED (use phase flags on `up`):
  bootstrap   → up --bootstrap
  provision   → up --provision
  kubeconfig  → up --kubeconfig
  ami push    → up --ami
  push        → up --push
  deploy      → up --deploy
  envoy       → up --envoy";

#[derive(Args)]
// Allows `k8s up -p eks` (flag after subcommand) as well as
// `k8s -p eks up`. clap won't let a global arg be required, so the
// field is Option and validated in run().
#[command(args_conflicts_with_subcommands = false, after_help = RENAME_HINTS)]
pub struct K8sArgs {
    /// Target cluster provider. Reads RIO_K8S_PROVIDER if not given.
    #[arg(short, long, global = true, env = "RIO_K8S_PROVIDER")]
    provider: Option<ProviderKind>,

    #[command(subcommand)]
    cmd: K8sCmd,
}

#[derive(Subcommand)]
pub enum K8sCmd {
    /// Bring up the cluster: bootstrap → provision → kubeconfig → ami
    /// → push → deploy → envoy. Phase flags select a subset.
    Up(UpOpts),
    /// End-to-end build + worker-kill chaos test.
    Smoke,
    /// helm rollback to REV (0 = previous).
    Rollback {
        #[arg(default_value_t = 0)]
        rev: u32,
    },
    /// helm release history.
    History,
    /// One-shot deployment health report.
    #[command(visible_alias = "st")]
    Status {
        /// Emit machine-readable JSON instead of the human report.
        #[arg(long)]
        json: bool,
        /// Cordon and delete NodeClaims for nodes where the seccomp-
        /// installer DaemonSet is stuck (I-020 cleanup). Also deletes
        /// stuck NodeClaims. Karpenter reprovisions healthy replacements.
        #[arg(long)]
        reap_stuck_nodes: bool,
    },
    /// One-shot Prometheus scrape of scheduler-leader + store
    /// replicas. Prints the gauges that answer "is the actor
    /// wedged?" — mailbox depth, queued/running, workers, mean
    /// actor-cmd latency. <5s; no Prometheus install required.
    #[command(visible_alias = "m")]
    Metrics {
        /// List what would be scraped instead of scraping.
        #[arg(long)]
        dry_run: bool,
    },
    /// Port-forward to Grafana (kube-prometheus-stack), print
    /// URL + credentials, hold until Ctrl-C. The 6 rio-* dashboards
    /// load via the Grafana sidecar (P0539b).
    #[command(visible_alias = "g")]
    Grafana {
        /// Local port to forward (0 = pick free).
        #[arg(long, default_value_t = 3000)]
        port: u16,
    },
    /// Tear down rio + backing infra.
    Destroy {
        /// Skip the interactive confirm. Destroy is irreversible —
        /// only use this from CI / scripted teardowns.
        #[arg(long)]
        yes: bool,
    },
    /// Open a tunnel to the gateway and run `nix build --store
    /// ssh-ng://rio@localhost:PORT <ARGS>`. Uses your SSH key (the
    /// pair of RIO_SSH_PUBKEY installed by `deploy`).
    #[command(visible_alias = "rsb")]
    RemoteStoreBuild {
        #[command(flatten)]
        remote: RemoteStoreArgs,
    },
    /// Open a tunnel to the gateway and run `nix copy --to
    /// ssh-ng://rio@localhost:PORT <ARGS>`. Uses your SSH key (the
    /// pair of RIO_SSH_PUBKEY installed by `deploy`).
    #[command(visible_alias = "cpt")]
    CopyTo {
        #[command(flatten)]
        remote: RemoteStoreArgs,
    },
    /// Run rio-cli LOCALLY against a port-forwarded scheduler+store.
    /// Fetches the mTLS client cert from the cluster (rio-scheduler-tls
    /// Secret — its `localhost` SAN makes a port-forwarded connection
    /// pass tonic's :authority check). Prefer this over `kubectl exec
    /// deploy/rio-scheduler -- rio-cli …`: in-pod exec forces the
    /// scheduler image to bundle rio-cli + jq + whatever pipes through.
    #[command(visible_alias = "cli")]
    CliTunnel {
        /// Local port for scheduler:9001 forward. 0 = ephemeral
        /// (I-101: fixed default raced concurrent invocations).
        #[arg(long, default_value_t = 0)]
        sched_port: u16,
        /// Local port for store:9002 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        store_port: u16,
        /// Passed through to rio-cli.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Detached stress-build harness. Fires N parallel builds through
    /// SSM tunnels, returns immediately, tracks PIDs on disk for
    /// SIGKILL-safe cleanup. Born from QA sessions where hand-rolled
    /// `setsid nohup` left zombie session-manager-plugin tunnels.
    #[command(subcommand)]
    Stress(stress::StressCmd),
}

#[derive(Args)]
pub struct RemoteStoreArgs {
    /// Local port for the tunnel.
    #[arg(long, default_value_t = 2222)]
    port: u16,
    /// Passed through to the nix command.
    #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
    args: Vec<String>,
}

pub async fn run(args: K8sArgs, cfg: &XtaskConfig) -> Result<()> {
    let kind = match args.provider {
        Some(k) => k,
        None => {
            ui::select("Provider?", ProviderKind::value_variants().to_vec())?.ok_or_else(|| {
                anyhow!(
                    "provider required: pass -p {{kind,k3s,eks}} or set RIO_K8S_PROVIDER in .env.local"
                )
            })?
        }
    };
    let p = provider::get(kind);
    match args.cmd {
        K8sCmd::Up(opts) => run_up(&*p, kind, cfg, opts).await,
        K8sCmd::Smoke => p.smoke(cfg).await,
        K8sCmd::Rollback { rev } => {
            let rev = if rev > 0 {
                rev
            } else {
                let revs = helm::history_json("rio", NS)?;
                ui::select("Rollback to?", revs)?
                    .map(|r| r.revision)
                    .ok_or_else(|| anyhow!("specify a revision: cargo xtask k8s rollback <REV>"))?
            };
            helm::rollback("rio", NS, rev)
        }
        K8sCmd::History => helm::history("rio", NS),
        K8sCmd::Status {
            json,
            reap_stuck_nodes,
        } => status::run(&*p, kind, cfg, json, reap_stuck_nodes).await,
        K8sCmd::Metrics { dry_run } => metrics::run(dry_run).await,
        K8sCmd::Grafana { port } => metrics::grafana(port).await,
        K8sCmd::Destroy { yes } => {
            let what = match kind {
                ProviderKind::Eks => crate::tofu::output(eks::TF_DIR, "cluster_name")
                    .map(|n| format!("EKS cluster '{n}' (RDS, S3, ECR, VPC, IAM)"))
                    .unwrap_or_else(|_| "the EKS cluster (RDS, S3, ECR, VPC, IAM)".into()),
                ProviderKind::K3s => "the local k3s rio deployment + rook".into(),
                ProviderKind::Kind => format!("kind cluster '{}'", kind::CLUSTER),
            };
            // confirm_destroy returns false on non-TTY stdin — `--yes`
            // is the only way to run this from a script.
            if !yes
                && !ui::confirm_destroy(&format!(
                    "This will DESTROY {what} and all data. Continue?"
                ))?
            {
                bail!("destroy cancelled (use --yes to bypass)");
            }
            p.destroy(cfg).await
        }
        K8sCmd::RemoteStoreBuild { remote } => {
            with_remote_store(&*p, cfg, remote.port, |sh, store| {
                let args = &remote.args;
                sh::run_interactive(sh::cmd!(
                    sh,
                    "nix build --store {store} --eval-store auto {args...}"
                ))
            })
            .await
        }
        K8sCmd::CopyTo { remote } => {
            with_remote_store(&*p, cfg, remote.port, |sh, store| {
                let args = &remote.args;
                sh::run_interactive(sh::cmd!(sh, "nix copy --to {store} {args...}"))
            })
            .await
        }
        K8sCmd::CliTunnel {
            sched_port,
            store_port,
            args,
        } => {
            with_cli_tunnel(&*p, sched_port, store_port, |sh| {
                // Prefer an installed rio-cli (nix run / cargo install);
                // fall back to cargo run for dev iteration. The PATH probe
                // uses `command -v` via xshell — it's available in any
                // POSIX sh and cheaper than pulling the `which` crate.
                let on_path = sh::read(sh::cmd!(sh, "command -v rio-cli")).is_ok();
                if on_path {
                    sh::run_interactive(sh::cmd!(sh, "rio-cli {args...}"))
                } else {
                    sh::run_interactive(sh::cmd!(sh, "cargo run -q -p rio-cli -- {args...}"))
                }
            })
            .await
        }
        K8sCmd::Stress(cmd) => stress::run(cmd, &*p, kind, cfg).await,
    }
}

/// Dispatch the selected `up` phases in canonical order.
///
/// `explicit` distinguishes `up --ami -p kind` (hard error: the user
/// asked for an EKS-only phase on the wrong provider) from `up -p kind`
/// (silent skip: ami is part of the canonical sequence but not
/// applicable here). Same distinction lets `validate_phase_opts`
/// reject `--push --deploy-tenant foo` while accepting
/// `--deploy-tenant foo` alone.
async fn run_up(p: &dyn Provider, kind: ProviderKind, cfg: &XtaskConfig, o: UpOpts) -> Result<()> {
    let selected = o.phases();
    let explicit = selected.len() != Phase::ALL.len();
    o.validate_phase_opts(&selected)?;

    let log_level = o
        .deploy_log_level
        .clone()
        .unwrap_or_else(|| cfg.log_level.clone());
    let tenant = o.deploy_tenant.as_deref();
    let nodes = o.provision_nodes.unwrap_or(3);

    ui::step("k8s up", || async {
        for &phase in &selected {
            match phase {
                Phase::Bootstrap => ui::step("bootstrap", || p.bootstrap(cfg)).await?,
                Phase::Provision => {
                    ui::step("provision", || p.provision(cfg, o.yes, nodes)).await?
                }
                Phase::Kubeconfig => ui::step("kubeconfig", || p.kubeconfig(cfg)).await?,
                Phase::Ami => match kind {
                    ProviderKind::Eks => {
                        ui::step("ami", || eks::ami::run_phase(o.ami_arch)).await?
                    }
                    _ if explicit => {
                        bail!("--ami is EKS-only (NixOS node AMI, ADR-021); pass -p eks")
                    }
                    _ => debug!("ami: provider={kind}, skipping"),
                },
                Phase::Push => {
                    let images = ui::step("build", || p.build(cfg)).await?;
                    ui::step("push", || p.push(&images, cfg)).await?
                }
                Phase::Deploy => {
                    ui::step("deploy", || {
                        p.deploy(cfg, &log_level, tenant, o.deploy_skip_preflight)
                    })
                    .await?
                }
                Phase::Envoy => ui::step("envoy", envoy_install).await?,
            }
        }
        if !explicit {
            info!("all phases done — run `cargo xtask k8s -p {kind} smoke` to verify");
        }
        Ok(())
    })
    .await
}

/// Open a tunnel to the gateway, resolve the ssh-ng:// store URL,
/// and run `f` with a prepared shell. The store URL's `ssh-key=`
/// points at the private half of RIO_SSH_PUBKEY (what `deploy` put
/// in authorized_keys). Gateway host key is ephemeral, so
/// NIX_SSHOPTS sets StrictHostKeyChecking=no. The tunnel tears down
/// when this function returns (ProcessGuard drop).
async fn with_remote_store<F>(p: &dyn Provider, cfg: &XtaskConfig, port: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell, &str) -> Result<()>,
{
    let key = crate::ssh::privkey_path(cfg)?;
    // P0539e: a prior `rsb`/`stress` may have left a session-manager-
    // plugin or kubectl port-forward bound to this port (ProcessGuard
    // only fires if xtask exited cleanly; SIGKILL/panic leaks it).
    // The new tunnel would then bind fine but route to the OLD NLB —
    // surfacing as the "type 80" SSH error 30s later. Reap first.
    shared::kill_port_listeners(port);
    let _guard = ui::step("establish tunnel", || p.tunnel(port)).await?;

    let store = format!(
        "ssh-ng://rio@localhost:{port}?compress=true&ssh-key={}",
        key.display()
    );
    info!("store: {store}");

    let shell = sh::shell()?;
    // I-149/I-161: ServerAliveInterval — see `shared::NIX_SSHOPTS_BASE`.
    let _env = shell.push_env("NIX_SSHOPTS", shared::NIX_SSHOPTS_BASE);
    f(&shell, &store)
}

// r[impl sec.image.control-plane-minimal]
/// Port-forward scheduler:9001 + store:9002, fetch the mTLS client
/// cert into a tempdir, set `RIO_SCHEDULER_ADDR`/`RIO_STORE_ADDR`/
/// `RIO_TLS__*` on a prepared shell, run `f`. Tunnels + tempdir tear
/// down on return.
///
/// The `rio-scheduler-tls` Secret already carries a `localhost` SAN
/// (cert-manager.yaml — originally for in-pod rio-cli). Port-forward
/// preserves that `:authority`, so the fetched cert validates cleanly
/// with no new Certificate resource. Enables running rio-cli LOCALLY
/// instead of via `kubectl exec deploy/rio-scheduler`, which in turn
/// lets the scheduler image drop rio-cli + its transitive deps (jq,
/// column, …) — every extra binary in a control-plane image is an
/// execution primitive in a compromised pod.
pub async fn with_cli_tunnel<F>(p: &dyn Provider, sched: u16, store: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell) -> Result<()>,
{
    let client = kube::client().await?;
    let ((sched, _g1), (store, _g2)) =
        ui::step("tunnel scheduler+store", || p.tunnel_grpc(sched, store)).await?;
    let (_dir, cert, key, ca) = ui::step("fetch mTLS cert", || {
        kube::fetch_tls_to_tempdir(&client, NS, "rio-scheduler-tls")
    })
    .await?;

    let sh = sh::shell()?;
    let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}"));
    let _e2 = sh.push_env("RIO_STORE_ADDR", format!("localhost:{store}"));
    let _e3 = sh.push_env("RIO_TLS__CERT_PATH", &cert);
    let _e4 = sh.push_env("RIO_TLS__KEY_PATH", &key);
    let _e5 = sh.push_env("RIO_TLS__CA_PATH", &ca);
    f(&sh)
}

/// Envoy Gateway operator (dashboard gRPC-Web → gRPC+mTLS translation).
/// Provider-agnostic — same helm chart, same namespace.
async fn envoy_install() -> Result<()> {
    let shell = sh::shell()?;
    let client = kube::client().await?;

    let eg = sh::read(sh::cmd!(
        shell,
        "nix build --no-link --print-out-paths .#helm-envoy-gateway"
    ))?;

    helm::Helm::upgrade_install("envoy-gateway", &eg)
        .namespace("envoy-gateway-system")
        .create_namespace()
        .run()?;
    kube::wait_rollout(
        &client,
        "envoy-gateway-system",
        "envoy-gateway",
        Duration::from_secs(120),
    )
    .await
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UpOpts {
        UpOpts::default()
    }

    #[test]
    fn no_flags_is_full_sequence() {
        let o = opts();
        assert_eq!(o.phases(), Phase::ALL.to_vec());
        // and namespaced opts are accepted in the all-phases path
        let mut o = opts();
        o.deploy_tenant = Some("t".into());
        o.ami_arch = AmiArch::X86_64;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
    }

    #[test]
    fn flags_select_subset_in_canonical_order() {
        // Flag order doesn't matter; canonical order does.
        let mut o = opts();
        o.deploy = true;
        o.push = true;
        assert_eq!(o.phases(), vec![Phase::Push, Phase::Deploy]);
    }

    #[test]
    fn namespaced_opt_requires_its_phase_when_explicit() {
        let mut o = opts();
        o.push = true;
        o.deploy_tenant = Some("t".into());
        let e = o.validate_phase_opts(&o.phases()).unwrap_err().to_string();
        assert!(e.contains("--deploy-tenant requires --deploy"), "{e}");

        // OK once --deploy is added.
        o.deploy = true;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
    }

    #[test]
    fn ami_arch_default_doesnt_trip_validation() {
        // --ami-arch defaults to All; validation should only fire on
        // a non-default value with explicit phases.
        let mut o = opts();
        o.push = true;
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
        o.ami_arch = AmiArch::Aarch64;
        assert!(o.validate_phase_opts(&o.phases()).is_err());
    }
}
