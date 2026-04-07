//! Provider abstraction: same command surface, different backing cluster.

use std::sync::Arc;

use anyhow::Result;
use async_trait::async_trait;
use tempfile::TempDir;

use crate::config::XtaskConfig;

/// Output of the nix-build portion of push. Held separately so `up`
/// can run the build concurrently with provision (neither depends on
/// the other), then serialize on the upload portion.
pub struct BuiltImages {
    /// Contains `images-{arch}/` symlinks to nix store linkFarms.
    pub dir: TempDir,
    pub tag: String,
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

    /// helm upgrade with provider-specific values/--set args.
    /// `log_level` sets RUST_LOG in all rio pods via `global.logLevel`.
    /// `tenant` overrides the authorized_keys comment (→ gateway's
    /// `tenant_name`); `None` falls through to RIO_SSH_TENANT then
    /// `ssh::DEFAULT_TENANT`. `skip_preflight` bypasses the pre-deploy
    /// cluster health check (EKS-only; k3s ignores it). `no_hooks`
    /// passes `--no-hooks` to helm — skips post-install/upgrade hooks
    /// (smoke tests etc.) for AMI bring-up where the hook itself needs
    /// the thing being brought up.
    async fn deploy(
        &self,
        cfg: &XtaskConfig,
        log_level: &str,
        tenant: Option<&str>,
        skip_preflight: bool,
        no_hooks: bool,
    ) -> Result<()>;

    /// e2e build + worker-kill chaos. SSM tunnel (eks) | port-forward (k3s).
    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()>;

    /// Open a tunnel to the gateway's SSH port, waiting until the SSH
    /// banner reads through. SSM→NLB (eks) | kubectl port-forward (k3s).
    /// Drop the guard to tear down.
    async fn tunnel(&self, local_port: u16) -> Result<super::shared::ProcessGuard>;

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

    /// helm uninstall + tofu destroy (eks) | rook teardown (k3s).
    async fn destroy(&self, cfg: &XtaskConfig) -> Result<()>;
}

pub fn get(kind: ProviderKind) -> Arc<dyn Provider> {
    match kind {
        ProviderKind::K3s => Arc::new(super::k3s::K3s),
        ProviderKind::Eks => Arc::new(super::eks::Eks),
    }
}
