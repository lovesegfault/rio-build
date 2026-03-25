//! Regenerate derived/checked-in files.

use anyhow::Result;
use clap::Subcommand;

use crate::config::XtaskConfig;
use crate::ui;

mod cargo_json;
mod crds;
mod grafana;
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
    /// Diff the worker seccomp profile against upstream moby (human review).
    Seccomp {
        /// moby git tag to fetch default.json from.
        #[arg(default_value = "v27.5.1")]
        tag: String,
    },
}

pub async fn run(which: Option<RegenCmd>, _cfg: &XtaskConfig) -> Result<()> {
    match which {
        Some(RegenCmd::Sqlx) => sqlx::run(),
        Some(RegenCmd::Crds) => crds::run(),
        Some(RegenCmd::Grafana) => grafana::run(),
        Some(RegenCmd::CargoJson) => cargo_json::run(),
        Some(RegenCmd::Seccomp { tag }) => seccomp::run(&tag).await,
        None => {
            // Umbrella: run the idempotent regenerators (not seccomp —
            // that's a network-dependent diff, not a regen).
            type Step = (&'static str, fn() -> Result<()>);
            let steps: &[Step] = &[
                ("sqlx", sqlx::run),
                ("crds", crds::run),
                ("grafana", grafana::run),
                ("cargo-json", cargo_json::run),
            ];
            let bar = ui::bar(steps.len() as u64, "regen");
            for (name, f) in steps {
                bar.set_message(format!("regen {name}"));
                // Child processes (cargo, crate2nix) write directly to
                // stderr — suspend the bar so their output prints cleanly
                // above it, then redraw.
                ui::multi().suspend(f)?;
                bar.inc(1);
            }
            ui::finish_ok(&bar, "regen complete");
            Ok(())
        }
    }
}
