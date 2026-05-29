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
use std::sync::atomic::{AtomicU64, Ordering};
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

/// Prefix of the quarantine name a doomed staging tree is renamed to
/// before deletion. It contains `.`, which the build-id character class
/// excludes, so no live or future `Mount` can ever claim or collide
/// with a quarantined name.
const REAP_PREFIX: &str = ".reap.";

/// Monotonic suffix for quarantine names, so a tree whose removal
/// failed (e.g. an `EBUSY` from an unmount race) never blocks
/// quarantining a later tree of the same `build_id`. Uniqueness only
/// matters within one daemon: the startup orphan scan empties
/// `staging/` — quarantined or not — before any connection exists.
static REAP_SEQ: AtomicU64 = AtomicU64::new(0);

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
            reap_dead_staging(&staging, &live);
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
    let deficit = max_deficit(&[cache_dir, chunks_dir, staging_dir]);
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
/// of [`HIGH_WATER_PCT`] it is. `0` means every root that could be
/// statted is above the low watermark. Returns the max rather than the
/// sum because the production layout puts all three roots on one
/// bind-mounted XFS — summing would triple-count the same deficit.
fn max_deficit(roots: &[&Path]) -> u64 {
    let mut worst = 0u64;
    for root in roots {
        let Some((free, total)) = fs_free(root) else {
            continue;
        };
        if total == 0 || free * 100 >= total * LOW_WATER_PCT {
            continue;
        }
        worst = worst.max((total / 100 * HIGH_WATER_PCT).saturating_sub(free));
    }
    worst
}

/// Every evictable entry under `root/<shard>/`, oldest-first.
/// `.promoting`/`.tmp` placeholders are skipped — both belong to a live
/// promote: the `.tmp` is its verify-copy destination (and the rename
/// source at publication, so unlinking it fails the promote), and the
/// `.promoting` is the flock-held claim that serializes concurrent
/// promotes of one digest (unlinking it costs a racer a duplicate
/// copy). Eviction has no business touching either; leaked placeholders
/// from a crashed daemon are the startup orphan scan's job.
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

/// Remove staging trees whose `build_id` is not (or no longer) live.
///
/// Connection teardown is the primary cleanup path; this is the
/// backstop for trees it leaked (e.g. a `remove_dir_all` lost to an
/// unmount-race `EBUSY`). Deleting a *live* build's staging tree fails
/// every fetch, stage and `Promote` that build has in flight, so each
/// deletion must be provably safe at the moment it happens — never
/// judged against a point-in-time snapshot a concurrent `Mount` may
/// have raced past:
///
/// 1. Liveness is checked per entry, under the same lock `Mount` uses
///    to register a `build_id` (and `Mount` registers BEFORE mkdir-ing
///    its staging dir), so while the check holds the lock no Mount can
///    sit between "registered" and "directory created".
/// 2. A dir that fails the check is renamed to a quarantine name while
///    the lock is still held. A later `Mount` reusing the same
///    `build_id` then mkdirs a fresh dir at the original name and is
///    untouched by the deletion.
/// 3. The (slow) recursive delete runs on the quarantine name outside
///    the lock. Quarantine names contain `.`, which
///    [`super::mountd::validate_build_id`] rejects, so nothing live can
///    ever appear under one.
fn reap_dead_staging(staging_dir: &Path, live: &Mutex<HashSet<String>>) {
    reap_dead_staging_inner(staging_dir, live, || {});
}

/// [`reap_dead_staging`] with a test seam: `after_listing` runs after
/// the staging root has been listed and before any liveness decision —
/// the point where a concurrent `Mount` is most adversarial.
fn reap_dead_staging_inner(
    staging_dir: &Path,
    live: &Mutex<HashSet<String>>,
    after_listing: impl FnOnce(),
) {
    let names = list_dir(staging_dir);
    after_listing();
    for name in names {
        let doomed = if name.starts_with(REAP_PREFIX) {
            // Quarantine leftover from a removal that failed on an
            // earlier tick; by construction it has no live owner.
            staging_dir.join(&name)
        } else {
            let guard = live.lock().ignore_poison();
            if guard.contains(&name) {
                continue;
            }
            let quarantine = staging_dir.join(format!(
                "{REAP_PREFIX}{}.{name}",
                REAP_SEQ.fetch_add(1, Ordering::Relaxed)
            ));
            // The rename happens while the lock is still held —
            // intentionally. It is a metadata-only syscall (no data is
            // copied), so it does not extend the critical section
            // meaningfully; moving it outside the lock would re-open the
            // window where a Mount registers this build_id between the
            // liveness check and the rename and then loses its tree.
            match std::fs::rename(staging_dir.join(&name), &quarantine) {
                Ok(()) => {}
                // Teardown won the race and already removed it.
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => continue,
                Err(e) => {
                    warn!(staging = %name, error = %e, "dead staging dir not quarantined");
                    continue;
                }
            }
            drop(guard);
            quarantine
        };
        match std::fs::remove_dir_all(&doomed) {
            Ok(()) => info!(staging = %doomed.display(), "reaped dead staging dir"),
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(staging = %doomed.display(), error = %e, "dead staging dir not removed")
            }
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

    /// `.promoting`/`.tmp` entries belong to a live promote — its claim
    /// and its verify-copy destination. Unlinking the `.tmp` fails that
    /// promote's publish rename; unlinking the `.promoting` hands the
    /// digest's claim to a racer mid-copy. The sweep must never treat
    /// either as evictable.
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

    /// A `Mount` that registers while a sweep tick is already in flight
    /// must not be reaped: its staging dir is on disk by the time the
    /// staging root is listed (e.g. leaked by a previous owner of the
    /// same `build_id`), so judging it against liveness data older than
    /// the listing deletes a live build's staging tree and fails every
    /// fetch/stage/Promote it has in flight. Liveness must be decided
    /// per entry, at deletion time.
    #[test]
    fn staging_reap_spares_build_registered_mid_tick() {
        let tmp = tempfile::tempdir().unwrap();
        fs::create_dir_all(tmp.path().join("late-build/chunks")).unwrap();
        let live = Mutex::new(HashSet::new());

        reap_dead_staging_inner(tmp.path(), &live, || {
            // The Mount lands after the listing: registered in the live
            // set; its mkdir finds the leaked dir already present.
            live.lock().ignore_poison().insert("late-build".to_string());
        });

        assert!(
            tmp.path().join("late-build").exists(),
            "sweep must not delete a staging tree owned by a build that registered mid-tick"
        );
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

        let live = Mutex::new(HashSet::from(["live-build".to_string()]));
        reap_dead_staging(tmp.path(), &live);

        assert!(tmp.path().join("live-build").exists());
        assert!(!tmp.path().join("dead-build").exists());
    }

    /// A quarantine left behind by a removal that failed (`EBUSY` from
    /// an unmount race) must be deleted on a later tick rather than
    /// accumulating until the next daemon restart.
    #[test]
    fn staging_reap_removes_quarantine_leftovers() {
        let tmp = tempfile::tempdir().unwrap();
        let leftover = format!("{REAP_PREFIX}0.gone-build");
        fs::create_dir_all(tmp.path().join(&leftover).join("chunks")).unwrap();

        reap_dead_staging(tmp.path(), &Mutex::new(HashSet::new()));

        assert!(!tmp.path().join(&leftover).exists());
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
        assert_eq!(max_deficit(&[tmp.path()]), 0);
    }
}
