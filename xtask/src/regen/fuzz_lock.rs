//! Sync fuzz workspace lockfiles with the main workspace.
//!
//! Fuzz targets live in per-crate `fuzz/` workspaces excluded from the
//! main workspace (separate `Cargo.lock` each). When the fuzzed crate's
//! deps change, the fuzz lockfile drifts — `cargo update -p <crate>`
//! in each fuzz dir re-resolves against the crate's current deps.

use anyhow::Result;

use crate::fuzz::discover_dirs;
use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

pub async fn run() -> Result<()> {
    for dir in discover_dirs()? {
        // `<crate>/fuzz` → parent crate is `<crate>`.
        let parent = dir.strip_suffix("/fuzz").expect("discover_dirs filters");
        ui::step(&format!("cargo update -p {parent} ({dir})"), || async {
            let sh = shell()?;
            sh.change_dir(repo_root().join(&dir));
            sh::run(cmd!(sh, "cargo update -p {parent}")).await
        })
        .await?;
    }
    Ok(())
}
