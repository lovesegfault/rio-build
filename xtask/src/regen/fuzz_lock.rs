//! Sync fuzz workspace lockfiles with the main workspace.
//!
//! Fuzz targets live in per-crate `fuzz/` workspaces excluded from the
//! main workspace (separate `Cargo.lock` each). When the fuzzed crate's
//! deps change, the fuzz lockfile drifts — `cargo update -p <crate>`
//! in each fuzz dir re-resolves against the crate's current deps.

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};
use crate::ui;

/// `(fuzz-dir, parent-crate-name)` — the crate name is what
/// `cargo update -p` targets to re-resolve transitive deps.
const FUZZ_WORKSPACES: &[(&str, &str)] =
    &[("rio-nix/fuzz", "rio-nix"), ("rio-store/fuzz", "rio-store")];

pub async fn run() -> Result<()> {
    for (dir, parent) in FUZZ_WORKSPACES {
        ui::step(&format!("cargo update -p {parent} ({dir})"), || async {
            let sh = shell()?;
            sh.change_dir(repo_root().join(dir));
            sh::run(cmd!(sh, "cargo update -p {parent}")).await
        })
        .await?;
    }
    Ok(())
}
