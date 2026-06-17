//! Unified k8s deploy. One command surface, provider flag selects
//! k3s (local) vs eks (AWS).

use std::sync::Arc;

use anyhow::{Context, Result, anyhow, bail};
use clap::{Args, Subcommand, ValueEnum};

use crate::config::XtaskConfig;
use crate::{helm, sh, ui};

pub(crate) mod chaos;
pub mod client;
pub(crate) mod eks;
mod fsbench;
mod k3s;
mod phases;
mod probe_boot;
pub mod provider;
pub(crate) mod qa;
pub mod shared;
mod sla_gates;
pub(crate) mod ssm;
pub(crate) mod status;
mod stress;
mod wipe;

use client as kube;

pub use eks::ami::AmiArch;
pub use phases::Phase;
use phases::{PhaseParams, run_up_phases};
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
/// resources (store.yaml → rio-store, pool.yaml → rio-builders,
/// etc.) have somewhere to land.
pub async fn ensure_namespaces(client: &kube::Client) -> anyhow::Result<()> {
    for &(ns, privileged) in NAMESPACES {
        kube::ensure_namespace(client, ns, privileged).await?;
    }
    Ok(())
}

#[derive(Args, Default)]
pub struct UpOpts {
    /// tofu state bucket (eks). No-op on k3s.
    #[arg(long)]
    bootstrap: bool,
    /// tofu apply (eks) | rook install (k3s).
    #[arg(long)]
    provision: bool,
    /// aws eks update-kubeconfig | k3s.yaml copy.
    #[arg(long)]
    kubeconfig: bool,
    /// Build + register the NixOS node AMI (ADR-021). EKS-only;
    /// silently skipped on k3s in the all-phases path, hard error
    /// when explicitly requested.
    #[arg(long)]
    ami: bool,
    /// Build + push docker images (ECR | ctr import).
    #[arg(long)]
    pub(super) push: bool,
    /// helm upgrade rio chart.
    #[arg(long)]
    pub(super) deploy: bool,
    /// Wipe the data plane (S3 chunks, PG schema, tenants/builds, builder
    /// Jobs, gateway authorized_keys) BEFORE running the full `up`
    /// pipeline. Infra shape (RDS instance, bucket, AMI, NodePools,
    /// tofu-managed helm releases) is preserved. ~2min wipe vs
    /// `destroy`+`up`'s ~20min. Incompatible with phase flags — `--wipe`
    /// always runs the full pipeline so the redeployed cluster is
    /// consistent with current HEAD (push + deploy in particular).
    #[arg(long)]
    wipe: bool,

    // Namespaced per-phase opts. NO clap `requires` attribute —
    // validated at runtime by `validate_phase_opts()` so the
    // no-flags=all-phases path can still pass them.
    /// AMI architecture(s) to build. EKS-only.
    #[arg(long = "ami-arch", value_enum, default_value_t = AmiArch::All)]
    ami_arch: AmiArch,
    /// Skip the post-ami stale-AMI GC. By default, after a successful
    /// `up` that built a new AMI, prior `rio-nixos-node-*` AMIs older
    /// than 2d (and not tagged `ami-latest`) are deregistered + their
    /// snapshots deleted. EKS-only.
    #[arg(long = "skip-ami-gc")]
    skip_ami_gc: bool,
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
    /// Pass --no-hooks to helm upgrade — skips post-install/upgrade
    /// hooks (smoke tests). For AMI bring-up where the hook needs
    /// working nodes that don't exist yet.
    #[arg(long = "deploy-no-hooks")]
    deploy_no_hooks: bool,
    /// Deploy with postgres.authMode=password instead of the iam
    /// default. Escape hatch for environments whose tfstate predates
    /// the IAM-auth infra (no controller_iam_role_arn output) —
    /// without it that state is a hard error pointing at `tofu
    /// apply`. EKS-only.
    #[arg(long = "pg-password-mode")]
    pg_password_mode: bool,
    /// After deploy, block until Karpenter has replaced all Drifted
    /// NodeClaims (AMI rollout complete). Without this, builds started
    /// immediately after `up` may be disrupted by node drift eviction.
    /// EKS-only.
    #[arg(long)]
    wait_drift: bool,
    /// Allow this CIDR to reach the gateway NLB directly (internet-
    /// facing). Repeatable. When unset, the NLB stays `internal`
    /// (reachable only via the SSM bastion). Changing set↔unset
    /// recreates the NLB — new DNS name. EKS-only.
    #[arg(long = "public-cidr", value_name = "CIDR")]
    public_cidr: Vec<String>,

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
        // --wipe must run the full pipeline: a partial up after wipe
        // leaves the cluster half-reset (e.g. no ECR tag for HEAD if
        // push is skipped, no chart if deploy is skipped). Reject any
        // phase-flag combination rather than guess which subset is
        // safe.
        if self.wipe && explicit {
            let flags: Vec<_> = selected.iter().map(|p| format!("--{}", p.name())).collect();
            bail!(
                "--wipe requires the full pipeline; drop {} or drop --wipe",
                flags.join(" ")
            );
        }
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
        req!(self.deploy_no_hooks, "--deploy-no-hooks", Phase::Deploy);
        req!(self.wait_drift, "--wait-drift", Phase::Deploy);
        req!(!self.public_cidr.is_empty(), "--public-cidr", Phase::Deploy);
        req!(
            !matches!(self.ami_arch, AmiArch::All),
            "--ami-arch",
            Phase::Ami
        );
        Ok(())
    }
}

#[derive(Args)]
// Allows `k8s up -p eks` (flag after subcommand) as well as
// `k8s -p eks up`. clap won't let a global arg be required, so the
// field is Option and validated in run().
#[command(args_conflicts_with_subcommands = false)]
pub struct K8sArgs {
    /// Target cluster provider. Reads RIO_K8S_PROVIDER if not given.
    #[arg(short, long, global = true, env = "RIO_K8S_PROVIDER")]
    provider: Option<ProviderKind>,

    #[command(subcommand)]
    cmd: K8sCmd,
}

#[derive(Subcommand)]
pub enum K8sCmd {
    /// Bring up the cluster: ami ∥ (bootstrap → provision → kubeconfig
    /// → push), then deploy. Phase flags select a subset.
    Up(UpOpts),
    /// Live-cluster QA: lint → health → scenarios → load → fault.
    /// Stage flags select a subset (mirrors `up`).
    Qa(qa::QaOpts),
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
        /// Delete NodeClaims stuck in Unknown >2min (Karpenter blocked
        /// on ResourceNotRegistered, InsufficientCapacity, etc.).
        /// Karpenter reprovisions healthy replacements.
        #[arg(long)]
        reap_stuck_nodes: bool,
        /// Append a one-shot Prometheus scrape of scheduler-leader +
        /// store replicas: mailbox depth, queued/running, workers,
        /// mean actor-cmd latency. The "is the actor wedged?" gauges.
        /// <5s; no Prometheus install required.
        #[arg(long, short = 'm')]
        metrics: bool,
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
    /// Run rio-cli LOCALLY against a port-forwarded scheduler+store
    /// (plaintext gRPC; encryption is at the Cilium overlay so the
    /// port-forward needs no client cert). Prefer this over `kubectl
    /// exec deploy/rio-scheduler -- rio-cli …`: in-pod exec forces the
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
    /// Run the native `rio build` client (ADR-024) LOCALLY against a
    /// port-forwarded scheduler+store. Resolves the tenant, mints a
    /// short-lived tenant JWT under the cluster's signing key, and
    /// hands it to `rio build` via /dev/fd — same plumbing CliTunnel
    /// uses for the service-HMAC key. For driving `rio` (or tools
    /// around it) yourself instead of a one-shot build, see `env`.
    #[command(visible_alias = "build")]
    BuildTunnel {
        /// Tenant the build runs as. Must already exist
        /// (`cargo xtask k8s cli -- create-tenant <name>`).
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Local port for scheduler:9001 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        sched_port: u16,
        /// Local port for store:9002 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        store_port: u16,
        /// Passed through to `rio build`.
        #[arg(trailing_var_arg = true, allow_hyphen_values = true, required = true)]
        args: Vec<String>,
    },
    /// Same tunnel + tenant-JWT setup as `build`, but instead of
    /// running `rio build` itself, run an arbitrary command — or your
    /// interactive shell when no command is given — with
    /// `RIO_SCHEDULER_ADDR` / `RIO_STORE_ADDR` /
    /// `RIO_TENANT_TOKEN_PATH` exported. Use `build` for a one-shot
    /// build; use `env` when you want to invoke `rio build` directly,
    /// repeatedly, or wrapped (`strace rio build …`). There is
    /// deliberately no "print eval-able exports and exit" mode: the
    /// port-forwards and the /dev/fd token die with this process, so
    /// pasted exports would point at dead endpoints.
    Env {
        /// Tenant the commands run as. Must already exist
        /// (`cargo xtask k8s cli -- create-tenant <name>`).
        #[arg(long, default_value = "default")]
        tenant: String,
        /// Local port for scheduler:9001 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        sched_port: u16,
        /// Local port for store:9002 forward. 0 = ephemeral.
        #[arg(long, default_value_t = 0)]
        store_port: u16,
        /// Command to run inside the environment, after a `--`
        /// separator (`env -- strace rio build .#foo`). Omit for an
        /// interactive shell ($SHELL, fallback /bin/sh).
        #[arg(last = true)]
        args: Vec<String>,
    },
    /// NixOS node AMI management (ADR-021). EKS-only — `up --ami`
    /// builds + registers; this is the maintenance side.
    #[command(subcommand)]
    Ami(AmiCmd),
    /// Grant build access to an SSH public key under its own tenant:
    /// creates the tenant, appends the key (with comment = tenant
    /// name) to the rio-gateway-ssh Secret, and rolls the gateway so
    /// the key takes effect. Idempotent — re-running for the same
    /// key/tenant pair is a no-op. Admin (rio-cli) access is NOT
    /// granted; the key only authenticates the ssh-ng build path.
    Grant {
        /// The user's OpenSSH public key — either inline
        /// (`'ssh-ed25519 AAAA... alice@host'`) or a path to a `.pub`
        /// file. Inline is tried first; if it doesn't parse as a key,
        /// the value is read as a file path.
        pubkey: String,
        /// Tenant name. Becomes the authorized_keys comment, which
        /// the gateway maps to `SubmitBuild.tenant_name`.
        #[arg(long)]
        tenant: String,
        /// Force a gateway rollout-restart so the key takes effect
        /// immediately. Without this, the gateway hot-reloads
        /// authorized_keys within ~70s (kubelet Secret refresh ~60s
        /// plus the gateway's 10s mtime poll) — no disruption to
        /// in-flight sessions.
        #[arg(long)]
        restart: bool,
    },
    /// castore-FUSE micro-benchmark (P0594): submit one bench build
    /// through the production mount path, sample co-tenancy, write
    /// .fsbench/{ts}/result.json. Operator tooling; NOT a CI test.
    Fsbench(fsbench::FsbenchArgs),
    /// ADR-023 §13a empirical gates (a–d). Operator-run on EKS; NOT a
    /// CI test. a/b assert; c/d report only.
    SlaGates {
        #[arg(long, value_enum)]
        gate: sla_gates::Gate,
    },
    /// ADR-023 §13b prerequisite: single-obs `leadTimeSeed` per
    /// `sla.hwClasses × {spot,od}` cell + 5 Karpenter-conformance
    /// assertions (naked NodeClaim launches; shim NodePool never
    /// provisions; Registered.lastTransitionTime populated;
    /// `karpenter.sh/nodepool` label survives to Node;
    /// `budgets:nodes:"0"` blocks drift). EKS-only, operator-run; NOT
    /// a CI test. Output: per-cell boot seconds + paste-ready
    /// `sla.leadTimeSeed:` YAML block.
    ProbeBoot,
    /// Rotate `rio-general` Karpenter nodes onto the current AMI.
    /// EKS-only. The rio-general NodePool has `budgets:0/Drifted`, so
    /// AMI changes leave its NodeClaims at `Drifted=True` indefinitely
    /// (control-plane pods are connection-stateful — auto-disruption
    /// would cut SSH sessions). This deletes those NodeClaims so
    /// Karpenter re-provisions on the new AMI, then waits for drift
    /// to settle. Run during a quiet window: gateway sessions on the
    /// drained nodes have up to `sessionDrainSecs` (1h) to finish.
    RotateGeneral,
    /// Run one SQL statement against the cluster's PostgreSQL via the
    /// in-cluster relay (`rio-qa-pg-relay` socat pod on EKS, direct
    /// port-forward on k3s). Operator surgery tool — there is no
    /// operator-side `psql` path: RDS lives in private VPC subnets and
    /// the credentials are in a k8s Secret, so the only sanctioned
    /// access is via `PgHandle` (credentials never reach a shell
    /// command line, env var, or temp file). One statement per
    /// invocation; for multi-statement surgery, run it twice.
    /// `SELECT/WITH/EXPLAIN/SHOW/TABLE` print rows; everything else
    /// prints rows-affected.
    Pg {
        /// The SQL statement. Quoted as one arg or trailing-var-arg —
        /// both work; spaces between args become spaces in the SQL.
        #[arg(trailing_var_arg = true, required = true)]
        sql: Vec<String>,
    },
}

#[derive(Subcommand)]
pub enum AmiCmd {
    /// Deregister stale rio AMIs + delete their snapshots. "Stale" =
    /// tagged `rio.build/ami`, NOT tagged `rio.build/ami-latest=true`,
    /// older than `--older-than-days`. Dry-run by default.
    Gc {
        /// Minimum age in days. AMIs newer than this are kept even if
        /// not latest (rollback window).
        #[arg(long, default_value_t = 7)]
        older_than_days: u64,
        /// Actually deregister + delete. Without this, prints the
        /// candidate set and exits.
        #[arg(long = "no-dry-run")]
        no_dry_run: bool,
    },
}

#[derive(Args)]
pub struct RemoteStoreArgs {
    /// Local port for the tunnel; 0 (default) picks an ephemeral port.
    #[arg(long, default_value_t = 0)]
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
                    "provider required: pass -p {{k3s,eks}} or set RIO_K8S_PROVIDER in .env.local"
                )
            })?
        }
    };
    let p = provider::get(kind);
    match args.cmd {
        K8sCmd::Up(opts) => run_up(p, kind, cfg, opts).await,
        K8sCmd::Qa(opts) => qa::run(opts, &*p, kind, cfg).await,
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
            metrics,
        } => status::run(&*p, kind, cfg, json, reap_stuck_nodes, metrics).await,
        K8sCmd::Grafana { port } => status::grafana(port).await,
        K8sCmd::Destroy { yes } => {
            let what = match kind {
                ProviderKind::Eks => crate::tofu::output(eks::TF_DIR, "cluster_name")
                    .map(|n| format!("EKS cluster '{n}' (RDS, S3, ECR, VPC, IAM)"))
                    .unwrap_or_else(|_| "the EKS cluster (RDS, S3, ECR, VPC, IAM)".into()),
                ProviderKind::K3s => "the local k3s rio deployment + rook".into(),
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
                // fall back to cargo run for dev iteration.
                if on_path(sh, "rio-cli") {
                    sh::run_interactive(sh::cmd!(sh, "rio-cli {args...}"))
                } else {
                    sh::run_interactive(sh::cmd!(sh, "cargo run -q -p rio-cli -- {args...}"))
                }
            })
            .await
        }
        K8sCmd::BuildTunnel {
            tenant,
            sched_port,
            store_port,
            args,
        } => with_build_tunnel(&*p, &tenant, sched_port, store_port, &args).await,
        K8sCmd::Env {
            tenant,
            sched_port,
            store_port,
            args,
        } => with_build_env(&*p, &tenant, sched_port, store_port, &args).await,
        K8sCmd::Grant {
            pubkey,
            tenant,
            restart,
        } => shared::grant(&pubkey, &tenant, restart).await,
        K8sCmd::Ami(cmd) => {
            if !matches!(kind, ProviderKind::Eks) {
                bail!("`ami` is EKS-only (NixOS node AMI, ADR-021); pass -p eks");
            }
            match cmd {
                AmiCmd::Gc {
                    older_than_days,
                    no_dry_run,
                } => eks::ami::gc(older_than_days, !no_dry_run).await,
            }
        }
        K8sCmd::Fsbench(a) => {
            let code = fsbench::run(a, &*p, kind, cfg).await?;
            // fsbench exit-code vocabulary: 2 = refusal (compare
            // identity mismatch OR refused --save-baseline),
            // 3 = regression — for operator scripting; anyhow's Err
            // path always exits 1, so nonzero verdicts exit directly.
            if code != 0 {
                std::process::exit(code);
            }
            Ok(())
        }
        K8sCmd::SlaGates { gate } => sla_gates::run(gate).await,
        K8sCmd::ProbeBoot => {
            if !matches!(kind, ProviderKind::Eks) {
                bail!(
                    "`probe-boot` is EKS-only (live Karpenter NodeClaim conformance, \
                     ADR-023 §13b); pass -p eks"
                );
            }
            probe_boot::run().await
        }
        K8sCmd::RotateGeneral => {
            if !matches!(kind, ProviderKind::Eks) {
                bail!("`rotate-general` is EKS-only (Karpenter NodeClaims); pass -p eks");
            }
            eks::deploy::rotate_general().await
        }
        K8sCmd::Pg { sql } => pg_exec(&sql.join(" ")).await,
    }
}

/// `xtask k8s pg <SQL>` — see [`K8sCmd::Pg`].
///
/// Reads (`SELECT`/`WITH`/`EXPLAIN`/`SHOW`/`TABLE`) print one
/// pipe-separated line per row. Everything else (`DELETE`/`UPDATE`/
/// `INSERT`/DDL) executes and prints rows-affected. The SQL is run
/// verbatim — no injection guard, because the caller IS the operator
/// and the threat model for `xtask` is "trusted shell on the operator
/// box," not "untrusted user input." (`AssertSqlSafe` below is that
/// statement in type form — sqlx 0.9 makes the audit explicit.)
async fn pg_exec(sql: &str) -> Result<()> {
    use sqlx::{Column, Row, ValueRef};

    let kube_client = client::client().await?;
    let pg = qa::ctx::PgHandle::open(&kube_client).await?;
    let head = sql.split_whitespace().next().unwrap_or("");
    let is_read = matches!(
        head.to_ascii_uppercase().as_str(),
        "SELECT" | "WITH" | "EXPLAIN" | "SHOW" | "TABLE" | "VALUES"
    );
    if is_read {
        let rows = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .fetch_all(&pg.pool)
            .await?;
        if rows.is_empty() {
            println!("(0 rows)");
            return Ok(());
        }
        // Header from the first row's column metadata (PG returns the
        // same shape for every row of one statement).
        let cols: Vec<&str> = rows[0].columns().iter().map(|c| c.name()).collect();
        println!("{}", cols.join(" | "));
        for r in &rows {
            let vals: Vec<String> = (0..cols.len())
                .map(|i| {
                    // Render via `try_get_raw` → `as_str` so we don't have to
                    // pattern-match every PG type. NULL and non-text types
                    // (bytea, timestamptz) fall back to a placeholder; for
                    // anything you need rendered, cast in the SQL
                    // (`encode(.., 'hex')`, `..::text`).
                    r.try_get_raw(i)
                        .ok()
                        .and_then(|raw| {
                            if raw.is_null() {
                                Some("NULL".into())
                            } else {
                                raw.as_str().ok().map(str::to_owned)
                            }
                        })
                        .unwrap_or_else(|| "<bin>".into())
                })
                .collect();
            println!("{}", vals.join(" | "));
        }
        println!("({} rows)", rows.len());
    } else {
        let r = sqlx::query(sqlx::AssertSqlSafe(sql.to_owned()))
            .execute(&pg.pool)
            .await?;
        println!("{} rows affected", r.rows_affected());
    }
    Ok(())
}

/// Whether `run_up` should re-init the tofu backend (`tofu init
/// -reconfigure` against `rio-tfstate-{sts-account-id}`) before any
/// phase runs.
///
/// Every EKS phase that reads `tofu output` — `kubeconfig`, `ami`,
/// `push`, `deploy`, plus `--wipe` — needs `.terraform/` init'd
/// against the *current* account's bucket. `provision` and `destroy`
/// already self-heal via [`eks::init_backend`]; the rest historically
/// trusted whatever `.terraform/` was on disk and 403'd when it was
/// from a prior account (or NXDOMAIN'd on a kube call against the
/// stale `.kube/config` that `tofu output` would have corrected).
///
/// The one exception is fresh-account bootstrap: the state bucket
/// doesn't exist until `bootstrap` creates it, so `tofu init` against
/// it would fail; `provision` runs `init_backend` immediately after.
/// `--wipe` is excluded from that exception — it always init's — even
/// though it forces the full pipeline (so `Bootstrap` is in the
/// selected set), because wiping a fresh account is a no-op.
fn needs_upfront_backend_init(kind: ProviderKind, wipe: bool, selected: &[Phase]) -> bool {
    matches!(kind, ProviderKind::Eks) && (wipe || !selected.contains(&Phase::Bootstrap))
}

/// Dispatch the selected `up` phases.
///
/// `explicit` distinguishes `up --ami -p k3s` (hard error: the user
/// asked for an EKS-only phase on the wrong provider) from `up -p k3s`
/// (silent skip: ami is part of the canonical sequence but not
/// applicable here). Same distinction lets `validate_phase_opts`
/// reject `--push --deploy-tenant foo` while accepting
/// `--deploy-tenant foo` alone.
pub(super) async fn run_up(
    p: Arc<dyn Provider>,
    kind: ProviderKind,
    cfg: &XtaskConfig,
    o: UpOpts,
) -> Result<()> {
    let selected = o.phases();
    // Explicit = at least one phase flag set. Passing all 7 flags is
    // intentionally treated as implicit (≡ no flags) — semantically
    // "run everything"; the explicit/implicit distinction only governs
    // whether per-phase opt mismatches and provider-unsupported phases
    // are hard errors vs silent skips.
    let explicit = selected.len() != Phase::ALL.len();
    o.validate_phase_opts(&selected)?;
    // Provider-support validation BEFORE dispatch — same upfront-fail
    // discipline as validate_phase_opts. Without this,
    // `-p k3s up --bootstrap --provision --ami` would install rook
    // THEN error.
    if explicit && selected.contains(&Phase::Ami) && !matches!(kind, ProviderKind::Eks) {
        bail!("--ami is EKS-only (NixOS node AMI, ADR-021); pass -p eks");
    }

    let pp = PhaseParams {
        yes: o.yes,
        deploy: provider::DeployOpts {
            log_level: o
                .deploy_log_level
                .clone()
                .or_else(|| (!cfg.log_level.is_empty()).then(|| cfg.log_level.clone()))
                // Empty (e.g. `RIO_LOG_LEVEL=` in env, or
                // XtaskConfig::default()) would fall through to the
                // chart's `info` default — losing per-crate debug.
                .unwrap_or_else(|| crate::config::RIO_DEBUG.into()),
            tenant: o.deploy_tenant.clone(),
            skip_preflight: o.deploy_skip_preflight,
            no_hooks: o.deploy_no_hooks,
            pg_password_mode: o.pg_password_mode,
            wait_drift: o.wait_drift,
            // CLI > env: any --public-cidr flag wins; otherwise fall
            // back to RIO_PUBLIC_CIDRS so a bare `up --deploy` keeps
            // the allowlist instead of reverting the NLB to internal.
            public_cidrs: if o.public_cidr.is_empty() {
                cfg.public_cidrs.clone()
            } else {
                o.public_cidr.clone()
            },
        },
    };
    let cfg = Arc::new(cfg.clone());

    // ami_branch is built here (not inside run_up_phases) because the
    // EKS-only gate needs `kind`, which the provider-agnostic core
    // doesn't see. Tests inject their own ami future directly.
    let ami_selected = selected.contains(&Phase::Ami);
    let ami_arch = o.ami_arch;
    let skip_ami_gc = o.skip_ami_gc;
    let ami_branch = async move {
        if !ami_selected {
            return Ok(());
        }
        match kind {
            ProviderKind::Eks => ui::step("ami", || eks::ami::run_phase(ami_arch)).await,
            // explicit + non-EKS already rejected above; reaching here
            // means implicit full-sequence on k3s — skip.
            _ => {
                debug!("ami: provider={kind}, skipping");
                Ok(())
            }
        }
    };

    // Re-init `.terraform/` against the CURRENT account's tfstate
    // bucket before any phase reads `tofu output`. provision and
    // destroy already self-heal via init_backend (see its doc); the
    // standalone phases (--kubeconfig, --ami, --push, --deploy) and
    // --wipe were unguarded — a `.terraform/` cached from a prior
    // account 403s the first `tofu output`, and a stale `.kube/config`
    // NXDOMAINs the first kube call. Skipped only when bootstrap is
    // selected without --wipe: a fresh account's bucket doesn't exist
    // yet (bootstrap creates it; provision init's right after).
    if needs_upfront_backend_init(kind, o.wipe, &selected) {
        eks::init_backend(&cfg).await?;
    }

    if o.wipe {
        // wipe makes kube/kubectl/helm calls before the kubeconfig
        // phase ever runs — same staleness, one layer up. Refresh
        // first so wipe targets the cluster `tofu output` names, not
        // whatever `.kube/config` was last written against.
        ui::step("kubeconfig", || p.kubeconfig(&cfg)).await?;
        wipe::run(kind).await?;
    }

    ui::step("k8s up", || async move {
        run_up_phases(p, cfg, &selected, pp, ami_branch).await?;
        // Auto-GC stale AMIs once the new one is registered + deployed.
        // Gated on the ami phase having run (that's when accumulation
        // happens — `up --deploy` alone creates nothing to reap).
        // 2d keeps yesterday's AMI for rollback; `ami-latest` is never
        // collected regardless. dry_run=false: the standalone `ami gc`
        // subcommand defaults to dry-run, but auto-gc here is the
        // point — actually delete.
        if matches!(kind, ProviderKind::Eks) && ami_selected && !skip_ami_gc {
            ui::step("ami gc (>2d)", || eks::ami::gc(2, false))
                .await
                .context("auto ami-gc failed; re-run with --skip-ami-gc to bypass")?;
        }
        if !explicit {
            info!("all phases done — run `cargo xtask k8s -p {kind} qa --health` to verify");
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
    // P0539e: reap a SIGKILL'd run's stale listener — see
    // [`shared::kill_port_listeners`] for the "type 80" failure mode.
    if port != 0 {
        shared::kill_port_listeners(port);
    }
    let (port, _guard) = ui::step("establish tunnel", || p.tunnel(port)).await?;

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
/// Port-forward scheduler:9001 + store:9002, set `RIO_SCHEDULER_ADDR`
/// / `RIO_STORE_ADDR` on a prepared shell, run `f`. Tunnels tear down
/// on return.
///
/// gRPC is plaintext (encryption is at the Cilium overlay), so no
/// client cert is fetched and no `:authority` validation is in play.
/// Enables running rio-cli LOCALLY instead of via `kubectl exec
/// deploy/rio-scheduler`, which in turn lets the scheduler image drop
/// rio-cli + its transitive deps (jq, column, …) — every extra binary
/// in a control-plane image is an execution primitive in a compromised
/// pod.
pub async fn with_cli_tunnel<F>(p: &dyn Provider, sched: u16, store: u16, f: F) -> Result<()>
where
    F: FnOnce(&xshell::Shell) -> Result<()>,
{
    use std::os::fd::AsRawFd;
    let ((sched, _g1), (store, _g2)) =
        ui::step("tunnel scheduler+store", || p.tunnel_grpc(sched, store)).await?;

    let sh = sh::shell()?;
    let _e1 = sh.push_env("RIO_SCHEDULER_ADDR", format!("localhost:{sched}"));
    let _e2 = sh.push_env("RIO_STORE_ADDR", format!("localhost:{store}"));
    // Service-HMAC key so rio-cli can mint x-rio-service-token (Admin
    // RPCs are token-gated in production). None = dev cluster, run
    // unsigned. Secret data-key per _helpers.tpl:116 / smoke.rs.
    let _key_fd = match p
        .secret_bytes("rio-service-hmac", "service-hmac.key")
        .await?
    {
        Some(b) => {
            let fd = shared::bytes_to_memfd(&b)?;
            sh.set_var(
                "RIO_SERVICE_HMAC_KEY_PATH",
                format!("/dev/fd/{}", fd.as_raw_fd()),
            );
            Some(fd) // hold open for the closure's lifetime
        }
        None => {
            debug!("Secret rio-service-hmac not found — rio-cli runs tokenless");
            None
        }
    };
    f(&sh)
}

/// PATH probe for the tunnel commands' "installed binary vs nix run /
/// cargo run" fallback. xshell execs the program directly (no shell),
/// so a bare `cmd!(sh, "command -v {bin}")` looks up a `command`
/// EXECUTABLE — which doesn't exist; `command` is a shell builtin —
/// and the probe always falls through. Route through `sh -c` so the
/// builtin actually runs.
fn on_path(sh: &xshell::Shell, bin: &str) -> bool {
    let probe = format!("command -v {bin}");
    sh::read(sh::cmd!(sh, "sh -c {probe}")).is_ok()
}

// TODO: dev-cluster interim. Minting the tenant JWT here requires reading
// the gateway's signing Secret, i.e. cluster-admin access. The production
// path for native clients is a gateway token-issue endpoint plus external
// gRPC ingress; once that exists this command should obtain its token
// there and stop touching the Secret.
/// Tunnel + credential setup shared by `build` and `env`: port-forward
/// scheduler:9001 + store:9002, resolve `tenant` to its UUID over the
/// scheduler tunnel, sign a session-shaped JWT (same claims as the
/// gateway's `mint_session_jwt`) with the cluster's `rio-jwt-signing`
/// seed, and stash the token in an anonymous memfd
/// (`RIO_TENANT_TOKEN_PATH=/dev/fd/N`) so it never touches disk.
/// The port-forward guards and the memfd live as long as the returned
/// struct — keep it in scope until the child process has exited.
struct BuildEnv {
    sched: u16,
    store: u16,
    token_fd: std::fs::File,
    _guards: (shared::ProcessGuard, shared::ProcessGuard),
}

impl BuildEnv {
    async fn new(p: &dyn Provider, tenant: &str, sched: u16, store: u16) -> Result<Self> {
        let ((sched, g1), (store, g2)) =
            ui::step("tunnel scheduler+store", || p.tunnel_grpc(sched, store)).await?;

        // Signing key first: failing on a cluster without JWT support
        // before the resolve RPC keeps the error unambiguous.
        let client = kube::client().await?;
        let signing_key = shared::jwt_signing_key(&client).await?;

        // Resolve tenant name → UUID over the forwarded scheduler.
        // ResolveTenant is unauthenticated and not leader-gated, so a bare
        // channel is enough.
        let tenant_id = ui::step("resolve tenant", || async {
            let ch = rio_proto::client::connect_channel(&format!("localhost:{sched}")).await?;
            let resp = rio_proto::SchedulerServiceClient::new(ch)
                .resolve_tenant(rio_proto::scheduler::ResolveTenantRequest {
                    tenant_name: tenant.to_owned(),
                })
                .await
                .map_err(|status| {
                    anyhow!(
                        "ResolveTenant({tenant}): {} ({}); create it first with \
                         `cargo xtask k8s cli -- create-tenant {tenant}`",
                        status.message(),
                        status.code()
                    )
                })?;
            resp.into_inner()
                .tenant_id
                .parse::<uuid::Uuid>()
                .context("scheduler returned unparseable tenant_id UUID")
        })
        .await?;

        // Same claim shape as the gateway's mint_session_jwt: 1h + 5min
        // grace TTL, fresh jti per invocation.
        let now = std::time::SystemTime::now()
            .duration_since(std::time::UNIX_EPOCH)
            .expect("system clock before 1970")
            .as_secs() as i64;
        let claims = rio_auth::jwt::TenantClaims {
            sub: tenant_id,
            iat: now,
            exp: now + 3600 + 300,
            jti: uuid::Uuid::new_v4().to_string(),
        };
        let token = rio_auth::jwt::sign(&claims, &signing_key)?;

        let token_fd = shared::bytes_to_memfd(token.as_bytes())?;
        Ok(Self {
            sched,
            store,
            token_fd,
            _guards: (g1, g2),
        })
    }

    /// The three variables a native `rio build` client needs.
    /// RIO_CAS_ROOT stays unset: the client already defaults to
    /// $XDG_CACHE_HOME/rio/evalstore.
    fn vars(&self) -> [(&'static str, String); 3] {
        use std::os::fd::AsRawFd;
        [
            ("RIO_SCHEDULER_ADDR", format!("localhost:{}", self.sched)),
            ("RIO_STORE_ADDR", format!("localhost:{}", self.store)),
            // token_fd stays open until the child exits; bytes_to_memfd
            // leaves FD_CLOEXEC unset so /dev/fd/N survives the exec.
            (
                "RIO_TENANT_TOKEN_PATH",
                format!("/dev/fd/{}", self.token_fd.as_raw_fd()),
            ),
        ]
    }
}

/// `xtask k8s build` — see [`K8sCmd::BuildTunnel`]. [`BuildEnv`] setup,
/// then run `rio build <args>` as `tenant`.
async fn with_build_tunnel(
    p: &dyn Provider,
    tenant: &str,
    sched: u16,
    store: u16,
    args: &[String],
) -> Result<()> {
    let env = BuildEnv::new(p, tenant, sched, store).await?;

    // `rio build` needs the eval parent (RIO_EVAL_PARENT), which only
    // the nix-built pair wires up — so unlike CliTunnel there is no
    // `cargo run` fallback; use `nix run .#rio` instead.
    let sh = sh::shell()?;
    // Raw Command (sh.rs policy): xshell nulls the child's stdin, but
    // the rio build renderer only enables its TTY mode (colors, log
    // browser keys) when both stderr AND stdin are terminals — the
    // child must inherit the parent's real terminal.
    let mut cmd = if on_path(&sh, "rio") {
        let mut c = std::process::Command::new("rio");
        c.arg("build");
        c
    } else {
        let mut c = std::process::Command::new("nix");
        c.args(["run", ".#rio", "--", "build"]);
        c
    };
    cmd.args(args);
    cmd.current_dir(sh::repo_root());
    cmd.envs(env.vars());
    let argv = format_argv(&cmd);
    debug!("exec (interactive): {argv}");
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn: {argv}"))?;
    if !status.success() {
        bail!("{argv}: {status}");
    }
    Ok(())
}

/// `xtask k8s env` — see [`K8sCmd::Env`]. [`BuildEnv`] setup, then run
/// the given command (or an interactive shell) with the `RIO_*`
/// variables exported, so the user drives `rio build` — or tools
/// wrapping it — directly. The "no print-exports mode" rationale lives
/// on the [`K8sCmd::Env`] doc.
async fn with_build_env(
    p: &dyn Provider,
    tenant: &str,
    sched: u16,
    store: u16,
    args: &[String],
) -> Result<()> {
    let env = BuildEnv::new(p, tenant, sched, store).await?;

    // Raw Command (sh.rs policy): xshell nulls the child's stdin, but an
    // interactive shell — and anything the user wraps around `rio build`
    // — must inherit the parent's real terminal.
    let mut cmd = match args {
        [] => {
            let shell = std::env::var("SHELL").unwrap_or_else(|_| "/bin/sh".into());
            eprintln!("rio dev-cluster environment (tenant `{tenant}`):");
            for (k, v) in env.vars() {
                eprintln!("  {k}={v}");
            }
            eprintln!(
                "Tunnels and token live only as long as this shell (JWT expires after ~1h); \
                 exit to tear down."
            );
            std::process::Command::new(shell)
        }
        [program, rest @ ..] => {
            let mut c = std::process::Command::new(program);
            c.args(rest);
            c
        }
    };
    cmd.envs(env.vars());
    let argv = format_argv(&cmd);
    debug!("exec (interactive): {argv}");
    let status = cmd
        .status()
        .with_context(|| format!("failed to spawn: {argv}"))?;
    // Propagate the child's exit code so a wrapped failing command
    // looks like running it without the tunnel (signal deaths collapse
    // to 1 — code() is None for those). process::exit skips Drop, so
    // tear the port-forward guards down explicitly first.
    if !status.success() {
        drop(env);
        std::process::exit(status.code().unwrap_or(1));
    }
    Ok(())
}

/// Render a [`std::process::Command`]'s argv for log/error messages.
fn format_argv(cmd: &std::process::Command) -> String {
    std::iter::once(cmd.get_program())
        .chain(cmd.get_args())
        .map(|a| a.to_str().unwrap_or("?"))
        .collect::<Vec<_>>()
        .join(" ")
}

#[cfg(test)]
mod tests {
    use super::*;

    fn opts() -> UpOpts {
        UpOpts::default()
    }

    #[test]
    fn env_command_only_after_double_dash() {
        #[derive(clap::Parser)]
        struct T {
            #[command(subcommand)]
            cmd: K8sCmd,
        }
        // `last = true`: the wrapped command goes after `--`; flags
        // before it still belong to the subcommand.
        let t = <T as clap::Parser>::try_parse_from([
            "t", "env", "--tenant", "acme", "--", "rio", "build", ".#x",
        ])
        .unwrap();
        let K8sCmd::Env { tenant, args, .. } = t.cmd else {
            panic!("expected Env");
        };
        assert_eq!(tenant, "acme");
        assert_eq!(args, ["rio", "build", ".#x"]);
        // No command at all → empty args (interactive-shell path).
        let t = <T as clap::Parser>::try_parse_from(["t", "env"]).unwrap();
        let K8sCmd::Env { args, .. } = t.cmd else {
            panic!("expected Env");
        };
        assert!(args.is_empty());
        // Unlike `build`'s trailing_var_arg, a typo'd flag is an error
        // instead of silently becoming the command to run.
        assert!(<T as clap::Parser>::try_parse_from(["t", "env", "--bogus"]).is_err());
        // ...and so is a bare command without the `--` separator.
        assert!(<T as clap::Parser>::try_parse_from(["t", "env", "rio"]).is_err());
    }

    #[test]
    fn on_path_detects_via_shell_builtin() {
        // Regression: a bare `cmd!(sh, "command -v {bin}")` execs the
        // word `command` directly — ENOENT — and the probe always
        // reported absent. `sh` is on PATH in any POSIX environment.
        let sh = sh::shell().unwrap();
        assert!(on_path(&sh, "sh"));
        assert!(!on_path(&sh, "rio-definitely-not-a-binary"));
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
    fn wipe_rejects_phase_flags() {
        let mut o = opts();
        o.wipe = true;
        // bare --wipe → full pipeline, OK
        assert!(o.validate_phase_opts(&o.phases()).is_ok());
        // --wipe --push → error naming the offending flag
        o.push = true;
        let e = o.validate_phase_opts(&o.phases()).unwrap_err().to_string();
        assert!(
            e.contains("--wipe requires the full pipeline") && e.contains("--push"),
            "{e}"
        );
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

    /// `needs_upfront_backend_init` decides whether `run_up` re-inits
    /// `.terraform/` before any phase reads `tofu output`. The matrix
    /// below is the operational contract: every standalone phase that
    /// reads tofu state must self-heal a stale backend; the only
    /// exception is fresh-account bootstrap (no bucket exists yet —
    /// running `tofu init` against it would fail, and provision will
    /// init right after bootstrap creates it). `--wipe` always init's:
    /// it forces the full pipeline (so `Bootstrap` is in the selected
    /// set) but a wipe can never target a fresh account.
    #[test]
    fn upfront_backend_init_matrix() {
        use Phase::*;
        let eks = ProviderKind::Eks;
        let k3s = ProviderKind::K3s;

        // Standalone phases against an existing cluster — must init.
        // This is the bug class that motivated the guard: a stale
        // `.terraform/` 403'd the first `tofu output` for every one
        // of these, because none of them run `provision`.
        for sel in [
            &[Kubeconfig][..],
            &[Ami],
            &[Push],
            &[Deploy],
            &[Push, Deploy],
            &[Provision], // double-init with provision's own — idempotent
        ] {
            assert!(
                needs_upfront_backend_init(eks, false, sel),
                "{sel:?} should init"
            );
        }

        // --wipe forces the full pipeline (Bootstrap included), but
        // wipe targets an existing cluster so the bucket exists.
        assert!(needs_upfront_backend_init(eks, true, &Phase::ALL));

        // Fresh-account bootstrap: bucket may not exist; provision
        // init's after bootstrap creates it. Skipping here keeps a
        // first `up` from failing on a `tofu init` against nothing.
        assert!(!needs_upfront_backend_init(eks, false, &Phase::ALL));
        assert!(!needs_upfront_backend_init(eks, false, &[Bootstrap]));
        assert!(!needs_upfront_backend_init(
            eks,
            false,
            &[Bootstrap, Provision]
        ));

        // K3s has no tofu state — never init, regardless of phases.
        assert!(!needs_upfront_backend_init(k3s, false, &[Kubeconfig]));
        assert!(!needs_upfront_backend_init(k3s, true, &Phase::ALL));
    }
}
