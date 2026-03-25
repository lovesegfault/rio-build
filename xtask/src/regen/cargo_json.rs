//! Regenerate Cargo.json via crate2nix after Cargo.lock changes.

use anyhow::Result;
use tracing::info;

use crate::sh::{cmd, repo_root, shell};

pub fn run() -> Result<()> {
    let sh = shell()?;
    info!("crate2nix generate");
    cmd!(sh, "crate2nix generate --format json -o Cargo.json").run()?;

    // crate2nix doesn't emit a trailing newline; end-of-file-fixer requires one.
    let path = repo_root().join("Cargo.json");
    let mut body = std::fs::read(&path)?;
    if body.last() != Some(&b'\n') {
        body.push(b'\n');
        std::fs::write(&path, body)?;
    }

    cmd!(sh, "git add Cargo.json").run()?;
    cmd!(sh, "git diff --cached --stat Cargo.json").run()?;
    Ok(())
}
