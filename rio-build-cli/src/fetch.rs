//! Client-CAS materialization of build outputs through the read path,
//! narHash-verified on arrival (ADR-024 "Attach, detach, results").
//!
//! This is the fallback home for fetched outputs when no local nix
//! daemon is reachable (the default path imports into `/nix/store`, see
//! [`crate::import`]) and the materialization step for IFD outputs,
//! which are consumed by the eval store rather than a human.
//!
//! Outputs stream as NARs via `StoreService::GetPath` (PathInfo first,
//! then nar_chunk frames). The NAR is restored *streaming* into a temp
//! sibling while the byte stream is SHA-256-hashed; only once the hash
//! and size match the server's claim is the tree renamed into place
//! (`<cas_root>/fetched/<basename>`) — a mismatch deletes the temp tree,
//! so a torn or corrupt restore never appears at the destination.

use std::path::{Path, PathBuf};

use anyhow::{Context, bail};
use tracing::{info, instrument};

use crate::coordinator::clients::Clients;
use crate::import::VerifiedNarReader;

/// Fetch one store path into the CAS. Returns the materialized
/// location. Idempotent: an already-materialized path is returned
/// as-is (content under a store-path basename is immutable).
// r[impl bc.fetch.narhash-verify+2]
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
    let frame = stream
        .message()
        .await
        .with_context(|| format!("GetPath {store_path} stream"))?
        .ok_or_else(|| anyhow::anyhow!("GetPath {store_path}: empty stream"))?;
    let Some(rio_proto::types::get_path_response::Msg::Info(info)) = frame.msg else {
        bail!("GetPath {store_path}: stream did not start with PathInfo");
    };

    // Restore into a temp sibling while hashing the stream, then rename —
    // `dest` either doesn't exist or is a complete verified tree, never a
    // torn or corrupt restore. The verifying reader refuses to report EOF
    // on a hash/size mismatch, so the restore task fails before the
    // rename.
    let tmp = fetched_dir.join(format!(".tmp-{}-{basename}", std::process::id()));
    remove_path_all(&tmp);
    let reader =
        VerifiedNarReader::new(stream, info.nar_hash, info.nar_size, store_path.to_string());
    let bridge = tokio_util::io::SyncIoBridge::new(reader);
    let restore_dest = tmp.clone();
    let restore_path = store_path.to_string();
    let restored = tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
        let mut r = bridge;
        rio_nix::nar::restore_path_streaming(&mut r, &restore_dest)
            .with_context(|| format!("restoring NAR for {restore_path}"))?;
        // The restore stops exactly at the end of the NAR structure; read
        // to EOF so the verifying reader's hash/size check runs (and any
        // trailing garbage is treated as corruption, not ignored).
        std::io::copy(&mut r, &mut std::io::sink())
            .with_context(|| format!("verifying NAR stream for {restore_path}"))?;
        Ok(())
    })
    .await
    .context("NAR restore task panicked")?;
    if let Err(e) = restored {
        remove_path_all(&tmp);
        return Err(e);
    }

    std::fs::rename(&tmp, &dest)
        .with_context(|| format!("committing {} -> {}", tmp.display(), dest.display()))?;
    info!(store_path, dest = %dest.display(), "materialized output");
    Ok(dest)
}

/// Best-effort removal of a leftover temp restore (file, symlink or
/// directory — the NAR root can be any of the three).
fn remove_path_all(path: &Path) {
    match std::fs::symlink_metadata(path) {
        Ok(md) if md.file_type().is_dir() => {
            let _ = std::fs::remove_dir_all(path);
        }
        Ok(_) => {
            let _ = std::fs::remove_file(path);
        }
        Err(_) => {}
    }
}

/// The name of the `idx`-th out-link, following nix-build's numbering:
/// `result`, `result-2`, `result-3`, …
pub fn numbered_link(link: &Path, idx: usize) -> PathBuf {
    if idx == 0 {
        return link.to_path_buf();
    }
    let mut name = link.file_name().unwrap_or_default().to_os_string();
    name.push(format!("-{}", idx + 1));
    link.with_file_name(name)
}

/// Create (or replace) `link` pointing at `target` — nix's out-link
/// semantics; the target is either the imported `/nix/store` path or the
/// CAS materialization (daemonless fallback).
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

#[cfg(test)]
mod tests {
    use super::*;

    /// nix-build numbering: the first link keeps the given name, further
    /// outputs get `-2`, `-3`, … (not `-1`).
    // r[verify bc.outlink.nix-parity]
    #[test]
    fn numbered_link_follows_nix_build_numbering() {
        let link = Path::new("/tmp/result");
        assert_eq!(numbered_link(link, 0), PathBuf::from("/tmp/result"));
        assert_eq!(numbered_link(link, 1), PathBuf::from("/tmp/result-2"));
        assert_eq!(numbered_link(link, 2), PathBuf::from("/tmp/result-3"));
    }
}
