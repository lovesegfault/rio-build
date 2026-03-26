//! Regenerate derived/checked-in files.

use anyhow::Result;
use clap::Subcommand;

use crate::config::XtaskConfig;
use crate::ui;

mod cargo_json;
mod crds;
mod fuzz_lock;
mod grafana;
mod hakari;
mod seccomp;
mod sqlx;

#[derive(Subcommand)]
pub enum RegenCmd {
    /// Regenerate .sqlx/ offline query cache (ephemeral PG + cargo sqlx prepare).
    Sqlx,
    /// Regenerate infra/helm/crds/ from the crdgen binary.
    Crds,
    /// Regenerate infra/helm/grafana/configmap.yaml from dashboard JSONs.
    Grafana,
    /// Regenerate Cargo.json via crate2nix.
    CargoJson,
    /// Regenerate the workspace-hack crate (feature unification).
    Hakari,
    /// Sync fuzz workspace lockfiles with the main workspace.
    FuzzLock,
    /// Diff the worker seccomp profile against upstream moby (human review).
    Seccomp {
        /// moby git tag to fetch default.json from.
        #[arg(default_value = "v27.5.1")]
        tag: String,
    },
}

pub async fn run(which: Option<RegenCmd>, _cfg: &XtaskConfig) -> Result<()> {
    match which {
        Some(RegenCmd::Sqlx) => sqlx::run().await,
        Some(RegenCmd::Crds) => crds::run().await,
        Some(RegenCmd::Grafana) => grafana::run(),
        Some(RegenCmd::CargoJson) => cargo_json::run().await,
        Some(RegenCmd::Hakari) => hakari::run().await,
        Some(RegenCmd::FuzzLock) => fuzz_lock::run().await,
        Some(RegenCmd::Seccomp { tag }) => seccomp::run(&tag).await,
        None => {
            // Umbrella: run the idempotent regenerators (not seccomp —
            // that's a network-dependent diff, not a regen).
            ui::phase("regen", 6, || async {
                ui::step("hakari", hakari::run).await?;
                ui::inc();
                ui::step("sqlx", sqlx::run).await?;
                ui::inc();
                ui::step("crds", crds::run).await?;
                ui::inc();
                ui::step("grafana", || async { grafana::run() }).await?;
                ui::inc();
                ui::step("fuzz-lock", fuzz_lock::run).await?;
                ui::inc();
                ui::step("cargo-json", cargo_json::run).await?;
                ui::inc();
                Ok(())
            })
            .await
        }
    }
}
