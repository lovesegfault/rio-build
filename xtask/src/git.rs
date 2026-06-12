//! Git helpers via gix. Used for the ECR image tag: short-SHA plus a
//! `-dirty-${hash}` suffix if the tree has uncommitted changes.

use std::sync::OnceLock;

use anyhow::{Context, Result};
use sha2::{Digest, Sha256};

use crate::sh::{self, cmd, shell};

static TAG_CACHE: OnceLock<String> = OnceLock::new();

pub fn open() -> Result<gix::Repository> {
    gix::open(sh::repo_root()).context("failed to open git repo")
}

/// `git rev-parse --short=12 HEAD`
pub fn short_sha(repo: &gix::Repository) -> Result<String> {
    let head = repo.head_commit().context("no HEAD commit")?;
    Ok(head.id().to_hex_with_len(12).to_string())
}

/// Image tag: short-SHA, plus `-dirty-${hash}` if the worktree differs
/// from HEAD.
///
/// The hash is `sha256(git diff HEAD)` — content-addressed, so
/// identical dirty state → identical tag (recomputable across
/// invocations / worktrees), and editing a file's CONTENT changes the
/// tag even when the set of changed paths stays the same. The previous
/// path-list hash gave the same tag for two different edits to the
/// same file → second `--push` was a no-op against immutable ECR tags
/// and silently kept stale image content.
///
/// Scope matches what `nix build` sees from the git fileset:
/// `git diff HEAD` covers staged + unstaged changes to tracked files
/// (including staged new files); untracked-unstaged files are excluded
/// here AND invisible to the nix build, so they correctly don't affect
/// the tag. Binary files are covered by the `index <old>..<new>` blob-
/// SHA line in the diff header even without `--binary`.
///
/// Process-cached: `up` runs Push and Ami concurrently, then Deploy.
/// Recomputing in Deploy after Ami (or a parallel editor) touched the
/// tree would yield a different tag than Push used → `assert_in_ecr`
/// rejects the tag Push just uploaded. First call wins; all phases in
/// one `xtask` invocation see the same value.
pub fn image_tag(repo: &gix::Repository) -> Result<String> {
    if let Some(t) = TAG_CACHE.get() {
        return Ok(t.clone());
    }
    let sha = short_sha(repo)?;
    let sh = shell()?;
    let diff = sh::read(cmd!(sh, "git diff HEAD --no-ext-diff"))?;
    let tag = if diff.is_empty() {
        sha
    } else {
        format!(
            "{sha}-dirty-{}",
            hex::encode(&Sha256::digest(diff.as_bytes())[..4])
        )
    };
    Ok(TAG_CACHE.get_or_init(|| tag).clone())
}

/// True iff `sha` resolves in this repo and is an ancestor of (or
/// equal to) HEAD.
///
/// `sha` comes from an EC2 tag (untrusted input): require it to be a
/// plain hex object name before it goes anywhere near a git argv — a
/// validated hex string cannot start with `-`, so no flag smuggling.
/// Anything else is by definition not a commit in our history →
/// `false`.
///
/// `git merge-base --is-ancestor` exits 0 for an ancestor, 1 for a
/// known-but-unrelated commit, and 128 for an object this repo has
/// never seen. The last two both mean "not our history" — a foreign
/// or rewritten-away commit is exactly what the caller wants to
/// reject — so any non-zero status maps to `false`.
pub async fn is_ancestor_of_head(sha: &str) -> Result<bool> {
    let is_hex_oid = (7..=64).contains(&sha.len()) && sha.bytes().all(|b| b.is_ascii_hexdigit());
    if !is_hex_oid {
        return Ok(false);
    }
    // Shell scoped tight so it isn't held across the await (xshell::
    // Shell is !Sync) — same shape as ami::ami_tag.
    let fut = {
        let sh = shell()?;
        sh::run_capture(cmd!(sh, "git merge-base --is-ancestor {sha} HEAD"))
    };
    let (status, _output) = fut.await?;
    Ok(status.success())
}
