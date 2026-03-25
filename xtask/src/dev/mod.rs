//! Local k8s (k3s/kind) deploy. Replaces `infra/dev.just`.
//!
//! Rook-Ceph provides dev S3 (real S3 semantics, ~3GB memory floor,
//! multi-minute bootstrap). PG is a bitnami subchart inside the rio chart.

use anyhow::Result;
use clap::Subcommand;

use crate::config::XtaskConfig;

pub mod apply;
mod delete;
mod envoy;
mod rook;

#[derive(Subcommand)]
pub enum DevCmd {
    /// Install Rook-Ceph operator + cluster (dev S3 backend).
    Rook,
    /// Install Envoy Gateway operator (dashboard gRPC-Web translation).
    EnvoyGateway,
    /// Full bring-up: CRDs + rook + S3-bridge + helm install rio.
    Apply,
    /// helm uninstall rio + secrets cleanup.
    Delete,
    /// Full teardown including Rook.
    Destroy,
}

pub async fn run(cmd: DevCmd, cfg: &XtaskConfig) -> Result<()> {
    match cmd {
        DevCmd::Rook => rook::install().await,
        DevCmd::EnvoyGateway => envoy::install().await,
        DevCmd::Apply => apply::run(cfg).await,
        DevCmd::Delete => delete::delete().await,
        DevCmd::Destroy => delete::destroy().await,
    }
}
