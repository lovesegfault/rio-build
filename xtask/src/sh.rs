//! Shell-out helpers. Thin wrapper over xshell that pins cwd to the
//! repo root so every `cmd!()` resolves paths relative to it.

use std::path::{Path, PathBuf};
use std::sync::OnceLock;

use anyhow::{Context, Result};
use xshell::Shell;

pub use xshell::cmd;

static REPO_ROOT: OnceLock<PathBuf> = OnceLock::new();

/// Absolute path to the workspace root (the dir containing Cargo.toml
/// with [workspace]). Computed from CARGO_MANIFEST_DIR at build time.
pub fn repo_root() -> &'static Path {
    REPO_ROOT.get_or_init(|| {
        // xtask/Cargo.toml → parent = repo root
        PathBuf::from(env!("CARGO_MANIFEST_DIR"))
            .parent()
            .expect("xtask has a parent dir")
            .to_path_buf()
    })
}

/// Shell rooted at the repo root.
pub fn shell() -> Result<Shell> {
    let sh = Shell::new().context("failed to create xshell")?;
    sh.change_dir(repo_root());
    Ok(sh)
}
