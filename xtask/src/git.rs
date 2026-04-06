//! Git helpers via gix. Used for the ECR image tag: short-SHA plus a
//! `-dirty-${hash}` suffix if the tree has uncommitted changes.

use anyhow::{Context, Result};
use gix::bstr::ByteSlice;
use sha2::{Digest, Sha256};

use crate::sh;

pub fn open() -> Result<gix::Repository> {
    gix::open(sh::repo_root()).context("failed to open git repo")
}

/// `git rev-parse --short=12 HEAD`
pub fn short_sha(repo: &gix::Repository) -> Result<String> {
    let head = repo.head_commit().context("no HEAD commit")?;
    Ok(head.id().to_hex_with_len(12).to_string())
}

/// Image tag: short-SHA, plus `-dirty-${hash}` if the worktree has
/// changes (tracked or untracked) vs HEAD.
///
/// The hash is SHA-256 over the sorted list of changed paths — same
/// determinism property as the bash's `git diff HEAD | sha256sum`:
/// identical dirty state → identical tag → re-push is a no-op.
pub fn image_tag(repo: &gix::Repository) -> Result<String> {
    let sha = short_sha(repo)?;
    let mut changed: Vec<Vec<u8>> = Vec::new();

    let status = repo
        .status(gix::progress::Discard)?
        .into_iter(None)
        .context("git status iterator")?;
    for item in status {
        let item = item?;
        changed.push(item.location().to_owned().into());
    }

    if changed.is_empty() {
        return Ok(sha);
    }

    changed.sort();
    let mut h = Sha256::new();
    for p in &changed {
        h.update(p.as_bstr());
        h.update(b"\n");
    }
    let suffix = hex::encode(&h.finalize()[..4]);
    Ok(format!("{sha}-dirty-{suffix}"))
}
