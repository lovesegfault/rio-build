//! Provider abstraction: same command surface, different backing cluster.

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

/// Per-method inner `ui::step` counts, so `k8s up` can compute its
/// phase total from the provider. Each impl constructs this from
/// consts defined next to its method bodies — same co-location
/// discipline as the regen/smoke helpers.
pub struct StepCounts {
    pub provision: u64,
    pub build: u64,
    pub push: u64,
    pub deploy: u64,
    pub smoke: u64,
}

// ?Send: xshell::Shell uses RefCell (not Sync), and providers hold
// Shell across awaits. We never spawn providers to other threads —
// everything runs on the main tokio runtime — so Send isn't needed.
#[async_trait(?Send)]
pub trait Provider {
    /// Step counts for each method. `push` depends on the number of
    /// docker images (a const per provider — changes when
    /// nix/docker.nix does).
    fn step_counts(&self) -> StepCounts;

    /// tofu state bucket (eks) | no-op (k3s).
    async fn bootstrap(&self, cfg: &XtaskConfig) -> Result<()>;

    /// tofu apply + kubeconfig (eks) | rook install + s3-bridge (k3s).
    async fn provision(&self, cfg: &XtaskConfig, auto: bool) -> Result<()>;

    /// aws eks update-kubeconfig (eks) | no-op (k3s — already configured).
    async fn kubeconfig(&self, cfg: &XtaskConfig) -> Result<()>;

    /// nix build the dockerImages linkFarm(s). Multi-arch (eks) | host-arch (k3s).
    /// Independent of provision — `up` runs them concurrently.
    async fn build(&self, cfg: &XtaskConfig) -> Result<BuiltImages>;

    /// ECR + skopeo + manifest-tool (eks) | ctr import (k3s).
    async fn push(&self, images: &BuiltImages, cfg: &XtaskConfig) -> Result<()>;

    /// helm upgrade with provider-specific values/--set args.
    /// `log_level` sets RUST_LOG in all rio pods via `global.logLevel`.
    async fn deploy(&self, cfg: &XtaskConfig, log_level: &str) -> Result<()>;

    /// e2e build + worker-kill chaos. SSM tunnel (eks) | port-forward (k3s).
    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()>;

    /// Open a tunnel to the gateway's SSH port, waiting until the SSH
    /// banner reads through. SSM→NLB (eks) | kubectl port-forward (k3s).
    /// Drop the guard to tear down.
    async fn tunnel(&self, local_port: u16) -> Result<super::shared::ProcessGuard>;

    /// helm uninstall + tofu destroy (eks) | + rook teardown (k3s).
    async fn destroy(&self, cfg: &XtaskConfig) -> Result<()>;
}

pub fn get(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::K3s => Box::new(super::k3s::K3s),
        ProviderKind::Eks => Box::new(super::eks::Eks),
    }
}
