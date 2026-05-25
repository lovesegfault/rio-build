//! Disk-pressure LRU sweep over the mountd-owned shared caches.
//!
//! The backing cache (`cache/ab/<file_digest>`) and chunk cache
//! (`chunks/ab/<chunk_digest>`) are append-only on the request path:
//! `Promote`/`PromoteChunks` only ever add entries. Without an evictor
//! a busy node fills its SSD and every subsequent `Promote` fails with
//! `ENOSPC`, which surfaces as an infra-failed build. The sweep is the
//! only deleter of cache entries; it deletes least-recently-used first
//! so the entries most likely to be passed through again survive.
//!
//! The same tick retries the removal of staging trees whose `build_id`
//! is no longer live — connection teardown is the primary cleanup
//! path, but a `remove_dir_all` it lost to an unmount race (`EBUSY`)
//! would otherwise leak the tree until the next daemon restart.

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Mutex};
use std::time::{Duration, SystemTime};

use tracing::{info, warn};

use super::mountd::list_dir;
use crate::IgnorePoison;

/// How often the sweep samples `statvfs` and publishes
/// `rio_mountd_cache_free_bytes`. Disk pressure builds at fetch
/// throughput (~100 MiB/s per build), so a one-minute reaction window
/// costs at most a few GiB of overshoot — which is what the
/// low-watermark headroom is for.
const SWEEP_INTERVAL: Duration = Duration::from_secs(60);

/// Eviction triggers when any swept filesystem drops below this free
/// percentage.
const LOW_WATER_PCT: u64 = 10;

/// Eviction frees entries until the filesystem is back above this free
/// percentage. The gap above [`LOW_WATER_PCT`] is hysteresis: a single
/// threshold would evict one file, refill, and evict again every tick.
const HIGH_WATER_PCT: u64 = 20;

/// One cache entry that could be evicted, oldest-first.
#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    size: u64,
    /// `max(atime, mtime)`. Under `relatime` (the default) atime only
    /// advances when it is older than mtime or a day stale, so this is
    /// a day-granularity LRU — enough for a cache meant to survive
    /// across builds for days.
    used: SystemTime,
}

/// Periodic driver. Spawned once by [`super::mountd::run`]; never
/// returns. Each tick runs entirely on the blocking pool — a sweep
/// over a full cache is hundreds of thousands of `lstat`s.
pub(super) async fn run(
    cache_dir: PathBuf,
    chunks_dir: PathBuf,
    staging_dir: PathBuf,
    live_build_ids: Arc<Mutex<HashSet<String>>>,
) {
    let mut tick = tokio::time::interval(SWEEP_INTERVAL);
    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
    loop {
        tick.tick().await;
        let cache = cache_dir.clone();
        let chunks = chunks_dir.clone();
        let staging = staging_dir.clone();
        let live = Arc::clone(&live_build_ids);
        let res = tokio::task::spawn_blocking(move || {
            // Snapshot the live set once per tick. `Mount` inserts the
            // build_id BEFORE mkdir-ing its staging dir, so any
            // on-disk dir absent from this snapshot was already dead
            // when the snapshot was taken.
            let snapshot = live.lock().ignore_poison().clone();
            reap_dead_staging(&staging, &snapshot);
            sweep_once(&cache, &chunks, &staging);
        })
        .await;
        if let Err(e) = res {
            warn!(error = %e, "cache sweep task panicked");
        }
    }
}

/// One pressure check: publish the free-space gauge and evict if any
/// swept filesystem is below the low watermark.
// r[impl builder.fs.node-digest-cache]
fn sweep_once(cache_dir: &Path, chunks_dir: &Path, staging_dir: &Path) {
    // The gauge is specifically the cache dir's filesystem (the §14
    // table's definition); the eviction decision takes the worst of
    // the three, which may be three different filesystems.
    if let Some((free, _)) = fs_free(cache_dir) {
        metrics::gauge!("rio_mountd_cache_free_bytes").set(free as f64);
    }
    let Some(deficit) = max_deficit(&[cache_dir, chunks_dir, staging_dir]) else {
        return;
    };
    if deficit == 0 {
        return;
    }
    // Chunks first: they are regenerable intermediates (a re-fetch of
    // a missing chunk is one ranged GetChunks), while cache entries
    // are whole-file passthrough targets whose loss costs a full
    // re-fetch + re-promote on the next open.
    let mut total = 0u64;
    for (root, label) in [(chunks_dir, "chunks"), (cache_dir, "cache")] {
        if total >= deficit {
            break;
        }
        let freed = evict_from(collect_candidates(root), deficit - total);
        if freed > 0 {
            metrics::counter!("rio_mountd_cache_evicted_bytes_total", "dir" => label)
                .increment(freed);
        }
        total += freed;
    }
    if total < deficit {
        // Both caches are exhausted and a swept filesystem is still
        // under the watermark: the pressure is from something the
        // sweep does not own (live staging trees, the overlay upper, a
        // foreign tenant of the partition). The per-build staging
        // quota is the defense on that front.
        warn!(
            deficit,
            freed = total,
            "cache sweep exhausted both caches without reaching the high watermark"
        );
    } else {
        info!(freed = total, "cache sweep relieved disk pressure");
    }
}

/// `(free_bytes, total_bytes)` for the filesystem holding `path`, or
/// `None` if it cannot be statted (the directory may legitimately not
/// exist yet on a fresh node).
fn fs_free(path: &Path) -> Option<(u64, u64)> {
    let vfs = nix::sys::statvfs::statvfs(path).ok()?;
    let frag = vfs.fragment_size();
    // f_bavail, not f_bfree: the sweep runs as root but the watermark
    // protects *builder* writes, which stop at the reserved blocks.
    Some((vfs.blocks_available() * frag, vfs.blocks() * frag))
}

/// The largest "bytes to free" requirement across the swept roots:
/// for each filesystem under [`LOW_WATER_PCT`], how many bytes short
/// of [`HIGH_WATER_PCT`] it is. `Some(0)` means every root is above
/// the low watermark. Returns the max rather than the sum because the
/// production layout puts all three roots on one bind-mounted XFS —
/// summing would triple-count the same deficit.
fn max_deficit(roots: &[&Path]) -> Option<u64> {
    let mut worst = 0u64;
    let mut any = false;
    for root in roots {
        let Some((free, total)) = fs_free(root) else {
            continue;
        };
        any = true;
        if total == 0 || free * 100 >= total * LOW_WATER_PCT {
            continue;
        }
        worst = worst.max((total / 100 * HIGH_WATER_PCT).saturating_sub(free));
    }
    any.then_some(worst)
}

/// Every evictable entry under `root/<shard>/`, oldest-first.
/// `.promoting`/`.tmp` placeholders are skipped — they are owned by a
/// live promote's copy loop, and unlinking one out from under it makes
/// the final `rename` fail and the build error. (Leaked placeholders
/// from a crashed promote are the startup orphan scan's job.)
fn collect_candidates(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    for shard in list_dir(root) {
        let dir = root.join(&shard);
        for name in list_dir(&dir) {
            if name.ends_with(".promoting") || name.ends_with(".tmp") {
                continue;
            }
            let path = dir.join(&name);
            let Ok(meta) = std::fs::symlink_metadata(&path) else {
                continue;
            };
            if !meta.is_file() {
                continue;
            }
            let used = match (meta.accessed(), meta.modified()) {
                (Ok(a), Ok(m)) => a.max(m),
                (Ok(t), Err(_)) | (Err(_), Ok(t)) => t,
                (Err(_), Err(_)) => SystemTime::UNIX_EPOCH,
            };
            out.push(Candidate {
                path,
                size: meta.len(),
                used,
            });
        }
    }
    out.sort_by_key(|c| c.used);
    out
}

/// Unlink candidates in order until `bytes_needed` have been freed or
/// the list is exhausted. Returns the bytes actually freed. Unlinking
/// an entry that a live build currently has passthrough-mapped is
/// safe: the kernel's backing-file registration holds the inode, so
/// in-flight reads keep working and only the *name* disappears — the
/// next build's `open()` of that digest is an ordinary cache miss.
fn evict_from(candidates: Vec<Candidate>, bytes_needed: u64) -> u64 {
    let mut freed = 0u64;
    for c in candidates {
        if freed >= bytes_needed {
            break;
        }
        match std::fs::remove_file(&c.path) {
            Ok(()) => freed += c.size,
            // Nothing else deletes cache entries, but a vanished
            // candidate freed its bytes either way — just don't count
            // them twice.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(path = %c.path.display(), error = %e, "evict failed"),
        }
    }
    freed
}

/// Remove staging trees whose `build_id` is not in the live set.
fn reap_dead_staging(staging_dir: &Path, live: &HashSet<String>) {
    for name in list_dir(staging_dir) {
        if live.contains(&name) {
            continue;
        }
        let path = staging_dir.join(&name);
        match std::fs::remove_dir_all(&path) {
            Ok(()) => info!(staging = %path.display(), "reaped dead staging dir"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => warn!(staging = %path.display(), error = %e, "dead staging dir not removed"),
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    /// Write `name` under `root/<first two chars>/` with `size` bytes
    /// and an mtime+atime `age` seconds in the past. The shard layout
    /// mirrors what `Promote` produces.
    fn put(root: &Path, name: &str, size: usize, age: u64) -> PathBuf {
        let dir = root.join(&name[..2]);
        fs::create_dir_all(&dir).unwrap();
        let path = dir.join(name);
        fs::write(&path, vec![0u8; size]).unwrap();
        let t = nix::sys::time::TimeSpec::from_duration(
            (SystemTime::now() - Duration::from_secs(age))
                .duration_since(SystemTime::UNIX_EPOCH)
                .unwrap(),
        );
        nix::sys::stat::utimensat(
            nix::fcntl::AT_FDCWD,
            &path,
            &t,
            &t,
            nix::sys::stat::UtimensatFlags::NoFollowSymlink,
        )
        .unwrap();
        path
    }

    /// Evicting the newest entries while cold ones remain would defeat
    /// the cache: the working set a build is actively passing through
    /// would vanish between opens. Oldest-first is the load-bearing
    /// property.
    // r[verify builder.fs.node-digest-cache]
    #[test]
    fn evicts_oldest_first_and_stops_at_target() {
        let tmp = tempfile::tempdir().unwrap();
        let old = put(tmp.path(), "aa01", 100, 3000);
        let mid = put(tmp.path(), "bb02", 100, 2000);
        let new = put(tmp.path(), "cc03", 100, 1000);

        // Need 150 bytes -> the two oldest go, the newest survives.
        let freed = evict_from(collect_candidates(tmp.path()), 150);
        assert_eq!(freed, 200);
        assert!(!old.exists(), "oldest entry must be evicted first");
        assert!(
            !mid.exists(),
            "second-oldest must be evicted to reach the target"
        );
        assert!(new.exists(), "eviction must stop once the target is met");
    }

    /// A `.promoting` placeholder is a live promote's destination;
    /// unlinking it makes that promote's final `rename` fail and the
    /// build error out. The sweep must never treat one as evictable.
    #[test]
    fn placeholders_and_subdirs_are_not_candidates() {
        let tmp = tempfile::tempdir().unwrap();
        put(tmp.path(), "aa01.promoting", 100, 5000);
        put(tmp.path(), "aa02.tmp", 100, 5000);
        fs::create_dir_all(tmp.path().join("aa").join("not-a-file")).unwrap();
        let real = put(tmp.path(), "aa03", 100, 10);

        let cands = collect_candidates(tmp.path());
        assert_eq!(cands.len(), 1, "only the final cache entry is evictable");
        assert_eq!(cands[0].path, real);
    }

    /// Deleting a live build's staging tree fails every fetch that
    /// build has in flight. The live-set check is the only thing
    /// standing between the sweep and that outcome.
    #[test]
    fn staging_reap_spares_live_builds() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("live-build/chunks")).unwrap();
        fs::create_dir_all(tmp.path().join("dead-build/chunks")).unwrap();
        fs::write(tmp.path().join("dead-build/chunks/x"), b"x").unwrap();

        let live = HashSet::from(["live-build".to_string()]);
        reap_dead_staging(tmp.path(), &live);

        assert!(tmp.path().join("live-build").exists());
        assert!(!tmp.path().join("dead-build").exists());
    }

    /// Above the low watermark the sweep must be a no-op: a tick that
    /// evicts even one entry on a healthy disk turns the cache into a
    /// 60-second TTL.
    #[test]
    fn no_deficit_above_low_watermark() {
        let tmp = tempfile::tempdir().unwrap();
        // A fresh tempdir's filesystem is assumed to have >10% free.
        // If the build host is itself above 90% full this test cannot
        // distinguish "correctly idle" from "correctly evicting", so
        // skip rather than assert the wrong thing.
        let (free, total) = fs_free(tmp.path()).unwrap();
        if free * 100 < total * LOW_WATER_PCT {
            eprintln!("skipping: test host filesystem is under the low watermark");
            return;
        }
        assert_eq!(max_deficit(&[tmp.path()]), Some(0));
    }
}
