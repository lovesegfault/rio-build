//! Regenerate derived/checked-in files.

use anyhow::Result;
use clap::Subcommand;

use crate::ui;

mod cargo_json;
mod crds;
mod docs_data;
mod fuzz_lock;
mod hakari;
mod helm_obs;
pub(crate) mod seccomp;
mod sqlx;
mod tfvars;

#[derive(Subcommand)]
pub enum RegenCmd {
    /// Regenerate .sqlx/ offline query cache (ephemeral PG + cargo sqlx prepare).
    Sqlx,
    /// Regenerate infra/helm/crds/ from the crdgen binary.
    Crds,
    /// Regenerate Cargo.json via crate2nix.
    CargoJson,
    /// Regenerate the workspace-hack crate (feature unification).
    Hakari,
    /// Sync fuzz workspace lockfiles with the main workspace.
    FuzzLock,
    /// Regenerate docs/gen/*.json (metric/alert/error/config refs for typst).
    DocsData,
    /// Render describe_*! HELP into the chart (metric-help.json + dashboards).
    HelmObs,
    /// Regenerate infra/eks/generated.auto.tfvars.json from nix/pins.toml.
    Tfvars,
    /// Diff the worker seccomp profile against upstream moby (human review).
    Seccomp {
        /// moby git tag to fetch default.json from. Upstream renamed
        /// release tags v* → docker-v* at the 29.x line.
        #[arg(default_value = "docker-v29.6.0")]
        tag: String,
    },
}

pub async fn run(which: Option<RegenCmd>) -> Result<()> {
    match which {
        Some(RegenCmd::Sqlx) => sqlx::run().await,
        Some(RegenCmd::Crds) => crds::run().await,
        Some(RegenCmd::CargoJson) => cargo_json::run().await,
        Some(RegenCmd::Hakari) => hakari::run().await,
        Some(RegenCmd::FuzzLock) => fuzz_lock::run().await,
        Some(RegenCmd::DocsData) => docs_data::run().await,
        Some(RegenCmd::HelmObs) => helm_obs::run().await,
        Some(RegenCmd::Tfvars) => tfvars::run().await,
        Some(RegenCmd::Seccomp { tag }) => seccomp::run(&tag).await,
        None => {
            // Umbrella: run the idempotent regenerators (not seccomp —
            // that's a network-dependent diff, not a regen).
            ui::step("regen", || async {
                ui::step("hakari", hakari::run).await?;
                ui::step("sqlx", sqlx::run).await?;
                ui::step("crds", crds::run).await?;
                ui::step("docs-data", docs_data::run).await?;
                ui::step("helm-obs", helm_obs::run).await?;
                ui::step("tfvars", tfvars::run).await?;
                ui::step("fuzz-lock", fuzz_lock::run).await?;
                ui::step("cargo-json", cargo_json::run).await
            })
            .await
        }
    }
}
