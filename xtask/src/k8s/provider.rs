//! Provider abstraction: same command surface, different backing cluster.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;

use super::shared::{ProcessGuard, SupervisedTunnel};
use crate::config::XtaskConfig;

/// Knobs for [`Provider::deploy`]. Bundled so adding the next deploy
/// option touches one struct + the impl that reads it, not the trait
/// + every impl + every mock + the call site.
#[derive(Clone, Default)]
pub struct DeployOpts {
    /// Sets `RUST_LOG` in all rio pods via `global.logLevel`.
    pub log_level: String,
    /// Overrides the `authorized_keys` comment (→ gateway's
    /// `tenant_name`); `None` falls through to `RIO_SSH_TENANT` then
    /// `ssh::DEFAULT_TENANT`.
    pub tenant: Option<String>,
    /// Bypass the pre-deploy cluster health check (EKS-only; k3s
    /// ignores it).
    pub skip_preflight: bool,
    /// Bypass the pg connection-budget preflight (EKS-only). The
    /// store ceiling then falls back to the tf-modeled
    /// `pg_max_connections` output with a loud warning instead of the
    /// live measurement.
    pub skip_pg_preflight: bool,
    /// Pass `--no-hooks` to helm — skips post-install/upgrade hooks
    /// (smoke tests etc.) for AMI bring-up where the hook itself needs
    /// the thing being brought up.
    pub no_hooks: bool,
    /// After deploy returns, block until Karpenter has replaced every
    /// Drifted NodeClaim. EKS-only (k3s has no Karpenter). An AMI
    /// change (`up --ami`) drifts every Karpenter node; without this,
    /// builds started immediately after `up` get evicted mid-run as
    /// the rollout proceeds.
    pub wait_drift: bool,
    /// Deploy with postgres.authMode=password instead of the iam
    /// default (EKS). Escape hatch for tfstate predating the IAM-auth
    /// infra (controller IRSA role / rds-db:connect): without the
    /// flag such a state is a HARD error, never a silent fallback —
    /// password mode re-opens the master-rotation outage class while
    /// reporting success.
    pub pg_password_mode: bool,
    /// Source CIDRs allowed to reach the gateway NLB. Non-empty flips
    /// the NLB to `internet-facing` and sets
    /// `spec.loadBalancerSourceRanges`; empty (default) keeps it
    /// `internal` (VPC-only via the SSM bastion). NLB scheme is
    /// immutable, so changing empty↔non-empty RECREATES the load
    /// balancer (new DNS name). EKS-only; k3s ignores it.
    pub public_cidrs: Vec<String>,
}

/// Output of the nix-build portion of push. Held separately so `up`
/// can run the build concurrently with provision (neither depends on
/// the other), then serialize on the upload portion.
pub struct BuiltImages {
    /// Contains `images-{arch}/` symlinks to nix store linkFarms.
    pub dir: TempDir,
    pub tag: String,
}

/// How [`Provider::gateway_endpoint`] reached the gateway's SSH port.
///
/// `rsb`/`cpt` previously routed unconditionally through `kubectl port-forward`
/// (the [`Provider::tunnel`] path); a port-forward death mid-build
/// drops the ssh-ng connection (sh-011, observed iter3 T+~1800s). The
/// NLB hostname (`.status.loadBalancer.ingress[].hostname`) is the
/// durable endpoint when the operator's source IP is in
/// `loadBalancerSourceRanges` — `Direct` carries it. `Tunnel` is the
/// fallback when the NLB is unreachable (internal scheme, source-CIDR
/// reject, still provisioning) or the provider has no NLB (k3s).
pub enum GatewayEndpoint {
    /// `ssh-ng://rio@{host}:{port}` — no local process; survives an
    /// xtask-side port-forward death.
    Direct { host: String, port: u16 },
    /// `ssh-ng://rio@localhost:{port}` via `kubectl port-forward`.
    /// Dropping `_guard` aborts the supervisor task → its inner
    /// `ProcessGuard` drops → killpg.
    Tunnel { port: u16, _guard: SupervisedTunnel },
}

impl GatewayEndpoint {
    /// `ssh-ng://` store URL for this endpoint, with `ssh-key=` set to
    /// the private half of `RIO_SSH_PUBKEY`.
    pub fn store_url(&self, key: &std::path::Path) -> String {
        let (host, port) = match self {
            Self::Direct { host, port } => (host.as_str(), *port),
            Self::Tunnel { port, .. } => ("localhost", *port),
        };
        format!(
            "ssh-ng://rio@{host}:{port}?compress=true&ssh-key={}",
            key.display()
        )
    }

    /// Label for the `info!` line in `with_remote_store`.
    pub fn kind(&self) -> &'static str {
        match self {
            Self::Direct { .. } => "direct (NLB)",
            Self::Tunnel { .. } => "tunnel (port-forward)",
        }
    }
}

#[derive(clap::ValueEnum, Clone, Copy, Debug)]
#[value(rename_all = "lower")]
pub enum ProviderKind {
    K3s,
    Eks,
}

impl std::fmt::Display for ProviderKind {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        // Lowercase to match the clap ValueEnum rendering (-p k3s).
        clap::ValueEnum::to_possible_value(self)
            .expect("no skipped variants")
            .get_name()
            .fmt(f)
    }
}

// Send + Sync: `run_up_phases` (I-198) spawns each phase on its own
// tokio task so a synchronously-blocking phase can't stall siblings.
// `tokio::spawn` needs `'static + Send`, which means `Arc<dyn Provider
// + Send + Sync>`. xshell::Shell is `!Sync` (RefCell internals) — impls
// scope `Shell` so its `&`-borrow never crosses an `.await` (the
// `sh::run`/`run_read` wrappers convert `Cmd<'_>` → owned `Command`
// synchronously and return a `Send + use<>` future).
#[async_trait]
pub trait Provider: Send + Sync {
    /// True if `ctx` (from `kubectl config current-context`) looks
    /// like it belongs to this provider. Used by `status` to guard
    /// against `-p k3s` reading an EKS kubeconfig.
    fn context_matches(&self, ctx: &str) -> bool;

    /// tofu state bucket (eks) | no-op (k3s).
    async fn bootstrap(&self, cfg: &XtaskConfig) -> Result<()>;

    /// tofu apply (eks) | rook install (k3s).
    async fn provision(&self, cfg: &XtaskConfig, auto: bool) -> Result<()>;

    /// aws eks update-kubeconfig | sudo cat k3s.yaml.
    async fn kubeconfig(&self, cfg: &XtaskConfig) -> Result<()>;

    /// nix build the dockerImages linkFarm(s). Multi-arch (eks) | host-arch (k3s).
    /// Independent of provision — `up` runs them concurrently.
    async fn build(&self, cfg: &XtaskConfig) -> Result<BuiltImages>;

    /// ECR skopeo (eks) | ctr import (k3s).
    async fn push(&self, images: &BuiltImages, cfg: &XtaskConfig) -> Result<()>;

    /// helm upgrade with provider-specific values/--set args. See
    /// [`DeployOpts`] for the per-field semantics.
    async fn deploy(&self, cfg: &XtaskConfig, opts: &DeployOpts) -> Result<()>;

    /// e2e build + worker-kill chaos. SSM tunnel (eks) | port-forward (k3s).
    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()>;

    /// Open a tunnel to the gateway's SSH port, waiting until the SSH
    /// banner reads through. SSM→NLB (eks) | kubectl port-forward (k3s).
    /// `local_port = 0` binds an ephemeral port; the returned port is
    /// the one actually bound — build store URLs from that. Drop the
    /// guard to tear down.
    async fn tunnel(&self, local_port: u16) -> Result<(u16, ProcessGuard)>;

    /// Resolve the durable gateway SSH endpoint for `rsb`/`cpt`.
    ///
    /// Additive sibling to [`Provider::tunnel`] (which stays for
    /// callers that WANT a local port — `stress.rs:94`'s multi-port
    /// loop; `qa/ctx.rs` open-codes `shared::port_forward` directly
    /// and is a separate sibling-sweep). Default impl is `Tunnel` via
    /// `self.tunnel(local_port)` — k3s and the `phases.rs` test mock
    /// inherit it. EKS overrides: probe `gateway_lb_hostname` with an
    /// SSH-banner read (stronger than bare `TcpStream::connect` — an
    /// NLB that accepts but has no healthy backend would pass connect
    /// and hang the banner) → `Direct` on success, `Tunnel` fallback
    /// on refusal/timeout (`loadBalancerSourceRanges` rejecting the
    /// operator's source IP is the expected refusal case).
    ///
    /// `local_port` is the bind hint for the `Tunnel` fallback; `0`
    /// binds ephemerally. Ignored for `Direct`.
    async fn gateway_endpoint(&self, local_port: u16) -> Result<GatewayEndpoint> {
        let (port, guard) = self.tunnel(local_port).await?;
        Ok(GatewayEndpoint::Tunnel {
            port,
            _guard: SupervisedTunnel::hold(guard),
        })
    }

    /// Open port-forwards to scheduler:9001 and store:9002, waiting
    /// until both accept TCP. Drop the guards to tear down. Unlike
    /// [`Provider::tunnel`], readiness is bare TCP accept — gRPC has
    /// no greeting banner. Always `kubectl port-forward` (eks too: the
    /// scheduler/store aren't behind the NLB; kubectl reaches them via
    /// the apiserver proxy, which `aws eks update-kubeconfig` already
    /// set up). Pass `0` for either port to bind an ephemeral local
    /// port (I-101); the RETURNED port is what to connect to.
    async fn tunnel_grpc(
        &self,
        sched_port: u16,
        store_port: u16,
    ) -> Result<(
        (u16, super::shared::ProcessGuard),
        (u16, super::shared::ProcessGuard),
    )>;

    /// Fetch a Secret's data key from the rio-system namespace.
    /// `None` if the Secret is absent (dev cluster without HMAC); a
    /// Secret that exists but lacks `key` hard-errors. Default impl
    /// creates a fresh kube client; the phases.rs test mock returns
    /// `Ok(None)`.
    async fn secret_bytes(&self, name: &str, key: &str) -> Result<Option<Vec<u8>>> {
        super::shared::secret_bytes(name, key).await
    }

    /// helm uninstall + tofu destroy (eks) | rook teardown (k3s).
    async fn destroy(&self, cfg: &XtaskConfig) -> Result<()>;
}

pub fn get(kind: ProviderKind) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::K3s => Arc::new(super::k3s::K3s),
        ProviderKind::Eks => Arc::new(super::eks::Eks),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::time::Duration;

    /// sh-011 red-first: `Direct` formats the NLB hostname into the
    /// store URL (not `localhost`). RED at base: `GatewayEndpoint` does
    /// not exist.
    #[test]
    fn gateway_endpoint_store_url_direct() {
        let ep = GatewayEndpoint::Direct {
            host: "abc.elb.us-west-2.amazonaws.com".into(),
            port: 22,
        };
        assert_eq!(
            ep.store_url(std::path::Path::new("/k")),
            "ssh-ng://rio@abc.elb.us-west-2.amazonaws.com:22?compress=true&ssh-key=/k"
        );
        assert_eq!(ep.kind(), "direct (NLB)");
    }

    /// sh-011 red-first: an unreachable host:port (refused) MUST yield
    /// `None` from the banner probe so `Eks::gateway_endpoint` takes
    /// the port-forward fallback arm. RED at base: `ssh_banner` is
    /// `127.0.0.1`-only (single `port` arg).
    #[tokio::test]
    async fn gateway_endpoint_probe_unreachable_falls_back() {
        let probed = tokio::time::timeout(
            Duration::from_secs(3),
            crate::k8s::eks::smoke::ssh_banner("127.0.0.1", 1),
        )
        .await
        .ok()
        .flatten();
        assert!(
            probed.is_none(),
            "refused port must not satisfy banner probe"
        );
    }
}
