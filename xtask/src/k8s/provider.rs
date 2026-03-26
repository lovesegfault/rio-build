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

// ?Send: xshell::Shell uses RefCell (not Sync), and providers hold
// Shell across awaits. We never spawn providers to other threads —
// everything runs on the main tokio runtime — so Send isn't needed.
#[async_trait(?Send)]
pub trait Provider {
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
    async fn deploy(&self, cfg: &XtaskConfig) -> Result<()>;

    /// e2e build + worker-kill chaos. SSM tunnel (eks) | port-forward (k3s).
    async fn smoke(&self, cfg: &XtaskConfig) -> Result<()>;

    /// helm uninstall + tofu destroy (eks) | + rook teardown (k3s).
    async fn destroy(&self, cfg: &XtaskConfig) -> Result<()>;
}

pub fn get(kind: ProviderKind) -> Box<dyn Provider> {
    match kind {
        ProviderKind::K3s => Box::new(super::k3s::K3s),
        ProviderKind::Eks => Box::new(super::eks::Eks),
    }
}
