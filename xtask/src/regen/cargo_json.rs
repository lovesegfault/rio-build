//! Regenerate Cargo.json via crate2nix after Cargo.lock changes.
//!
//! One Cargo.json per workspace: the root workspace plus each fuzz
//! workspace (each is its own workspace with its own Cargo.lock;
//! nix/fuzz.nix consumes them for per-crate sancov-instrumented
//! caching). Fuzz workspaces are discovered from the root Cargo.toml's
//! `[workspace] exclude` — the same source `regen fuzz-lock` uses — so
//! adding a fuzz workspace there is the only edit needed.

use anyhow::Result;

use crate::fuzz::discover_dirs;
use crate::sh::{self, cmd, repo_root, shell};

pub async fn run() -> Result<()> {
    let sh = shell()?;
    let root = repo_root();

    for dir in std::iter::once(".".to_string()).chain(discover_dirs()?) {
        let _push = sh.push_dir(root.join(&dir));
        sh::run(cmd!(sh, "crate2nix generate --format json -o Cargo.json")).await?;
        // crate2nix doesn't emit a trailing newline; end-of-file-fixer
        // requires one.
        let path = root.join(&dir).join("Cargo.json");
        let mut body = std::fs::read(&path)?;
        if body.last() != Some(&b'\n') {
            body.push(b'\n');
            std::fs::write(&path, body)?;
        }
        sh::run(cmd!(sh, "git add Cargo.json")).await?;
    }
    Ok(())
}
