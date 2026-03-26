//! Sync fuzz workspace lockfiles with the main workspace.
//!
//! Fuzz targets live in per-crate `fuzz/` workspaces excluded from the
//! main workspace (separate `Cargo.lock` each). When the fuzzed crate's
//! deps change, the fuzz lockfile drifts — `cargo update -p <crate>`
//! in each fuzz dir re-resolves against the crate's current deps.
//!
//! Fuzz workspaces are auto-discovered from `[workspace] exclude` in
//! the root Cargo.toml — any `*/fuzz` entry. Adding a new fuzz crate
//! means adding it to exclude (required anyway) and nothing else.

use anyhow::Result;
use serde::Deserialize;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

#[derive(Deserialize)]
struct Manifest {
    workspace: Workspace,
}

#[derive(Deserialize)]
struct Workspace {
    exclude: Vec<String>,
}

pub async fn run() -> Result<()> {
    let manifest: Manifest =
        toml::from_str(&std::fs::read_to_string(repo_root().join("Cargo.toml"))?)?;

    for dir in &manifest.workspace.exclude {
        // `<crate>/fuzz` → parent crate is `<crate>`.
        let Some(parent) = dir.strip_suffix("/fuzz") else {
            continue;
        };
        ui::step(&format!("cargo update -p {parent} ({dir})"), || async {
            let sh = shell()?;
            sh.change_dir(repo_root().join(dir));
            sh::run(cmd!(sh, "cargo update -p {parent}")).await
        })
        .await?;
    }
    Ok(())
}
