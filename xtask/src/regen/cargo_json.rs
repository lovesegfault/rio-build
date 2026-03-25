//! Regenerate Cargo.json via crate2nix after Cargo.lock changes.

use anyhow::Result;

use crate::sh::{self, cmd, repo_root, shell};

pub async fn run() -> Result<()> {
    let sh = shell()?;
    sh::run(cmd!(sh, "crate2nix generate --format json -o Cargo.json")).await?;

    // crate2nix doesn't emit a trailing newline; end-of-file-fixer requires one.
    let path = repo_root().join("Cargo.json");
    let mut body = std::fs::read(&path)?;
    if body.last() != Some(&b'\n') {
        body.push(b'\n');
        std::fs::write(&path, body)?;
    }

    sh::run(cmd!(sh, "git add Cargo.json")).await?;
    Ok(())
}
