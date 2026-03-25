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

/// Strip inherited `CARGO_*` env vars. Call once from main() before
/// the tokio runtime starts (remove_var is unsafe with threads).
///
/// When cargo runs the xtask binary it sets CARGO_MANIFEST_DIR,
/// CARGO_PKG_*, etc. If we shell out to a nested `cargo run`, those
/// leak into the child build's fingerprint — ring's build.rs tracks
/// CARGO_MANIFEST_DIR via rerun-if-env-changed, so the next top-level
/// `cargo build` (where the var is absent) triggers a full rebuild
/// from ring up. Stripping here makes nested cargo start clean.
///
/// # Safety
/// Must be called before any threads are spawned.
pub unsafe fn scrub_cargo_env() {
    for (k, _) in std::env::vars_os() {
        if let Some(k) = k.to_str()
            && k.starts_with("CARGO_")
            && k != "CARGO_HOME"
        {
            unsafe { std::env::remove_var(k) };
        }
    }
}
