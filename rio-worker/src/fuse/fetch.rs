//! Fetch and materialize store paths into the local cache.

use std::fs;
use std::io;
use std::path::{Path, PathBuf};

use fuser::Errno;
use tracing::instrument;

use super::NixStoreFs;
use super::cache::FetchClaim;

impl NixStoreFs {
    /// Ensure a store path is cached locally, fetching from remote if needed.
    ///
    /// Returns the local filesystem path to the materialized store path.
    /// If another thread is already fetching, blocks on a condition variable
    /// until that fetch completes (or a 30s timeout, then returns EAGAIN).
    pub(super) fn ensure_cached(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        match self.cache.get_path(store_basename) {
            Ok(Some(local_path)) => return Ok(local_path),
            Ok(None) => {} // not cached, fetch below
            Err(e) => {
                tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed");
                return Err(Errno::EIO);
            }
        }

        match self.cache.try_start_fetch(store_basename) {
            FetchClaim::Fetch(_guard) => {
                // We own the fetch. _guard notifies waiters on drop (success,
                // error, or panic) — no explicit cleanup needed.
                self.fetch_and_extract(store_basename)
            }
            FetchClaim::WaitFor(entry) => {
                // Another thread is fetching. Wait for it with a timeout as
                // belt-and-suspenders against a stuck fetcher (the guard's
                // Drop impl fires even on panic, so this timeout is defensive).
                const WAIT_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(30);
                if !entry.wait(WAIT_TIMEOUT) {
                    tracing::warn!(
                        store_path = store_basename,
                        timeout_secs = WAIT_TIMEOUT.as_secs(),
                        "concurrent fetch did not complete within timeout, returning EAGAIN"
                    );
                    return Err(Errno::EAGAIN);
                }
                // Fetch completed — check cache again. Fetcher failure =>
                // Ok(None) => ENOENT so the FUSE caller can retry.
                // Index error => EIO (loud failure, not silent re-fetch).
                match self.cache.get_path(store_basename) {
                    Ok(Some(p)) => Ok(p),
                    Ok(None) => Err(Errno::ENOENT),
                    Err(e) => {
                        tracing::error!(store_path = store_basename, error = %e, "FUSE cache index query failed after wait");
                        Err(Errno::EIO)
                    }
                }
            }
        }
    }

    /// Fetch a store path's NAR from remote store and extract to local cache.
    ///
    /// Debug-level span: this is the slow path (cache miss → gRPC + NAR
    /// extract), called at most once per store path per worker lifetime.
    /// Not on lookup/read (those are hot — kernel caches attr for 1h TTL
    /// so they fire once per path per process, but still high-volume).
    /// The `ensure_cached` caller is too broad (fast-path returns early).
    #[instrument(level = "debug", skip(self), fields(store_basename = %store_basename))]
    fn fetch_and_extract(&self, store_basename: &str) -> Result<PathBuf, Errno> {
        // Increment on miss (entry to this function), not on fetch success:
        // failed fetches (store outage, NAR parse error) are still cache
        // misses. The metric should spike during store outages so dashboards
        // surface the problem; incrementing only on success hides it.
        metrics::counter!("rio_worker_fuse_cache_misses_total").increment(1);
        let store_path = format!("/nix/store/{store_basename}");
        let local_path = self.cache.cache_dir().join(store_basename);

        tracing::debug!(store_path = %store_path, "fetching from remote store");

        // Fetch NAR data via gRPC (async bridged to sync). Timeout bounds the
        // entire fetch (initial call + stream drain) — a stalled store would
        // otherwise block this FUSE thread forever, and a few stalls exhaust
        // the FUSE thread pool and freeze the whole mount.
        let fetch_start = std::time::Instant::now();
        let nar_data = self.runtime.block_on(async {
            let mut client = self.store_client.clone();
            match rio_proto::client::get_path_nar(
                &mut client,
                &store_path,
                rio_common::grpc::GRPC_STREAM_TIMEOUT,
                rio_common::limits::MAX_NAR_SIZE,
            )
            .await
            {
                Ok(Some((_info, nar))) => Ok(nar),
                Ok(None) => {
                    // Path not in remote store. lookup() probes unknown names
                    // (.lock files, tmp paths); ENOENT is normal here.
                    Err(Errno::ENOENT)
                }
                Err(rio_proto::client::NarCollectError::SizeExceeded { got, limit }) => {
                    tracing::error!(
                        store_path = %store_path,
                        size = got,
                        limit,
                        "NAR exceeds MAX_NAR_SIZE"
                    );
                    Err(Errno::EFBIG)
                }
                Err(e) if e.is_not_found() => Err(Errno::ENOENT),
                Err(e) => {
                    tracing::warn!(store_path = %store_path, error = %e, "GetPath failed");
                    Err(Errno::EIO)
                }
            }
        })?;
        metrics::histogram!("rio_worker_fuse_fetch_duration_seconds")
            .record(fetch_start.elapsed().as_secs_f64());

        // Parse and extract NAR to local disk
        let node = rio_nix::nar::parse(&mut io::Cursor::new(&nar_data)).map_err(|e| {
            tracing::warn!(store_path = %store_path, error = %e, "NAR parse failed");
            Errno::EIO
        })?;

        // Extract to a temp sibling dir, then atomically rename into place.
        // If extraction fails mid-way (disk full, etc.), the partial tree stays
        // in the tmp dir and is cleaned up on next cache init, rather than
        // being served as a broken store path by subsequent lookups.
        let tmp_path = local_path.with_extension(format!("tmp-{:016x}", rand::random::<u64>()));
        rio_nix::nar::extract_to_path(&node, &tmp_path).map_err(|e| {
            tracing::warn!(
                store_path = %store_path,
                tmp_path = %tmp_path.display(),
                error = %e,
                "NAR extraction failed"
            );
            // Best-effort: remove the partial tmp tree.
            let _ = std::fs::remove_dir_all(&tmp_path);
            Errno::EIO
        })?;
        std::fs::rename(&tmp_path, &local_path).map_err(|e| {
            let _ = std::fs::remove_dir_all(&tmp_path);
            tracing::error!(
                store_path = %store_path,
                tmp_path = %tmp_path.display(),
                local_path = %local_path.display(),
                error = %e,
                "failed to rename extracted NAR into cache"
            );
            Errno::EIO
        })?;

        // Record in cache index. If this fails, the path is on disk but
        // invisible to contains() — every subsequent access would re-fetch
        // the NAR, creating an infinite re-fetch loop under DB failure.
        // Fail loudly (EIO) so the build surfaces the real problem instead
        // of silently amplifying network traffic.
        let size = dir_size(&local_path);
        if let Err(e) = self.cache.insert(store_basename, size) {
            tracing::error!(
                store_path = %store_basename,
                local_path = %local_path.display(),
                error = %e,
                "failed to record in cache index; path on disk but untracked"
            );
            return Err(Errno::EIO);
        }

        // Evict old entries if needed (best-effort)
        if let Err(e) = self.cache.evict_if_needed() {
            tracing::warn!(error = %e, "cache eviction failed");
        }

        Ok(local_path)
    }
}

/// Recursively compute the size of a directory tree.
///
/// Returns 0 and logs on I/O error. A silent 0 on error means the cache
/// index records size=0 for a large NAR — eviction never selects it (it
/// "takes no space"), and the cache can fill past its limit. The warn!
/// makes this visible.
pub(super) fn dir_size(path: &Path) -> u64 {
    match dir_size_inner(path) {
        Ok(s) => s,
        Err(e) => {
            tracing::warn!(
                path = %path.display(),
                error = %e,
                "dir_size failed; recording 0 (cache accounting will drift)"
            );
            0
        }
    }
}

fn dir_size_inner(path: &Path) -> io::Result<u64> {
    let meta = path.symlink_metadata()?;
    if meta.is_file() {
        return Ok(meta.len());
    }
    if !meta.is_dir() {
        // symlink, fifo, etc. — 0 contribution
        return Ok(0);
    }
    let mut total = 0u64;
    for entry in fs::read_dir(path)? {
        total += dir_size_inner(&entry?.path())?;
    }
    Ok(total)
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn test_dir_size() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("a.txt"), "hello")?;
        fs::write(dir.path().join("b.txt"), "world!")?;
        fs::create_dir(dir.path().join("sub"))?;
        fs::write(dir.path().join("sub/c.txt"), "nested")?;

        let size = dir_size(dir.path());
        // 5 + 6 + 6 = 17 bytes of file content
        assert_eq!(size, 17);
        Ok(())
    }

    /// Symlinks contribute 0 (they're not regular files, not dirs).
    /// Covers the `!meta.is_dir() → Ok(0)` branch.
    #[test]
    fn test_dir_size_symlink_contributes_zero() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        fs::write(dir.path().join("real.txt"), "hello")?; // 5 bytes
        std::os::unix::fs::symlink("real.txt", dir.path().join("link"))?;

        // 5 bytes from real.txt; link contributes 0.
        assert_eq!(dir_size(dir.path()), 5);
        Ok(())
    }

    /// Nonexistent path → 0 (logged at warn!, not panic). Covers the Err arm
    /// in the outer dir_size wrapper.
    #[test]
    fn test_dir_size_nonexistent_path_returns_zero() {
        let size = dir_size(Path::new("/nonexistent/rio-test-dir-size-xyz"));
        assert_eq!(size, 0);
    }
}
