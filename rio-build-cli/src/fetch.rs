//! `--fetch` / `--out-link`: materialize build outputs through the
//! read path, narHash-verified on arrival (ADR-024 "Attach, detach,
//! results").
//!
//! Outputs stream as NARs via `StoreService::GetPath` (PathInfo first,
//! then nar_chunk frames). The client hashes the byte stream, compares
//! against the server's claimed `nar_hash`, and only then restores the
//! tree into the client CAS (`<cas_root>/fetched/<basename>`) — the
//! CAS is the local home for fetched content per ADR-024's "fetch
//! cache plus an index". `--out-link` symlinks into it.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use sha2::Digest as _;
use tracing::{info, instrument};

use crate::coordinator::clients::Clients;

/// Fetch one store path into the CAS. Returns the materialized
/// location. Idempotent: an already-materialized path is returned
/// as-is (content under a store-path basename is immutable).
// r[impl bc.fetch.narhash-verify]
#[instrument(skip(clients), fields(component = "build-client"))]
pub async fn materialize(
    clients: &mut Clients,
    cas_root: &Path,
    store_path: &str,
) -> anyhow::Result<PathBuf> {
    let basename = store_path
        .rsplit('/')
        .next()
        .filter(|b| !b.is_empty() && !b.contains(".."))
        .ok_or_else(|| anyhow::anyhow!("malformed store path {store_path:?}"))?;
    let fetched_dir = cas_root.join("fetched");
    let dest = fetched_dir.join(basename);
    if dest.exists() {
        return Ok(dest);
    }
    std::fs::create_dir_all(&fetched_dir)
        .with_context(|| format!("creating {}", fetched_dir.display()))?;

    let mut stream = clients
        .store
        .get_path(clients.req(rio_proto::types::GetPathRequest {
            store_path: store_path.to_string(),
        })?)
        .await
        .with_context(|| format!("GetPath {store_path}"))?
        .into_inner();

    // First frame: PathInfo (the claimed narHash); then NAR bytes.
    // TODO: stream-restore instead of buffering the whole NAR — needs
    // a verify-then-commit dance (hash isn't known good until EOF, and
    // a half-restored tree must never appear at `dest`). Buffering is
    // fine for typical outputs; large-output fetch is a P3 follow-up.
    let mut claimed_hash: Option<Vec<u8>> = None;
    let mut nar: Vec<u8> = Vec::new();
    while let Some(frame) = stream.message().await.context("GetPath stream")? {
        match frame.msg {
            Some(rio_proto::types::get_path_response::Msg::Info(info)) => {
                claimed_hash = Some(info.nar_hash);
            }
            Some(rio_proto::types::get_path_response::Msg::NarChunk(chunk)) => {
                nar.extend_from_slice(&chunk);
            }
            None => {}
        }
    }
    let claimed = claimed_hash
        .ok_or_else(|| anyhow::anyhow!("GetPath {store_path}: stream carried no PathInfo"))?;
    let got = sha2::Sha256::digest(&nar);
    if got.as_slice() != claimed.as_slice() {
        bail!(
            "narHash mismatch fetching {store_path}: server claims {}, stream hashes to {} — \
             refusing to materialize",
            hex::encode(&claimed),
            hex::encode(got)
        );
    }

    // Restore into a temp sibling, then rename — `dest` either doesn't
    // exist or is a complete verified tree, never a torn restore.
    let tmp = fetched_dir.join(format!(".tmp-{}-{basename}", std::process::id()));
    if tmp.exists() {
        std::fs::remove_dir_all(&tmp).ok();
    }
    rio_nix::nar::restore_path_streaming(&mut nar.as_slice(), &tmp)
        .with_context(|| format!("restoring NAR for {store_path}"))?;
    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("committing {} -> {}", tmp.display(), dest.display()))?;
    info!(store_path, dest = %dest.display(), "materialized output");
    Ok(dest)
}

/// Create (or replace) `link` pointing at `target` — nix's out-link
/// semantics, except the target is the CAS materialization (the
/// client has no /nix/store to link into).
pub fn out_link(link: &Path, target: &Path) -> anyhow::Result<()> {
    if let Some(parent) = link.parent()
        && !parent.as_os_str().is_empty()
    {
        std::fs::create_dir_all(parent)?;
    }
    match std::fs::symlink_metadata(link) {
        Ok(md) if md.file_type().is_symlink() => std::fs::remove_file(link)?,
        Ok(_) => bail!(
            "--out-link target {} exists and is not a symlink — refusing to replace",
            link.display()
        ),
        Err(_) => {}
    }
    std::os::unix::fs::symlink(target, link)
        .with_context(|| format!("symlinking {} -> {}", link.display(), target.display()))?;
    Ok(())
}
