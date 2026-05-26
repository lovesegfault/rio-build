//! Disk-pressure sweep over the mountd-owned `/var/rio` trees (P0571).
//!
//! rio-mountd owns the node-shared backing cache (`cache/ab/<hex>`), the
//! chunk cache (`chunks/ab/<hex>`), and the per-build staging root.
//! Nothing else may delete from them (builders mount cache/chunks
//! read-only), so reclaiming space when the node SSD fills is the
//! daemon's job: a periodic `statvfs` over the three trees and, when
//! `min(free%)` drops below [`LOW_WATER_PCT`], an eviction pass that
//! removes entries until `min(free%)` clears [`HIGH_WATER_PCT`].
//!
//! Eviction order (cheapest-to-regenerate first):
//!
//! 1. **Orphaned staging dirs** — `staging/<build_id>` whose `build_id`
//!    has no live connection. Pure garbage from a crashed build: no
//!    future open can ever use it. Live builds' staging is never
//!    touched (it is bounded by the kernel project quota instead).
//! 2. **Chunk cache** — intermediate, regenerable from the store at
//!    chunk granularity; losing an entry costs one `GetChunks` refetch.
//! 3. **Backing cache** — the passthrough targets. Evicted last because
//!    a miss costs a whole-file refetch + re-promote on the next
//!    `open()` of that digest. Unlinking an entry that is currently
//!    passthrough-mapped is safe — the kernel holds its own reference
//!    to the inode, reads keep working, and the space is reclaimed when
//!    the last open closes.
//!
//! Within each cache tier, eviction is oldest-`mtime` first. The plan
//! called for atime ordering, but every production `/var/rio` mount is
//! `noatime` (`nix/nixos-node/eks-node.nix`), so atime never changes
//! after creation and would degrade to FIFO silently. mtime is used
//! instead, and the daemon bumps a backing entry's mtime on every
//! successful `BackingOpen` brokered for it (see
//! `mountd::touch_backing_entry`), making mtime a true last-used stamp
//! for the backing cache. Chunk-cache reads happen directly in the
//! builder (no daemon round-trip), so chunks age by promote time —
//! acceptable for the tier that is evicted first and regenerable.
//!
//! Placeholders owned by in-flight work (`*.promoting`, `*.tmp`,
//! `*.partial`) are never candidates; the startup orphan scan reclaims
//! stale ones.
// r[impl builder.fs.node-digest-cache]

use std::collections::HashSet;
use std::path::{Path, PathBuf};
use std::sync::Mutex;
use std::time::SystemTime;

use tracing::{info, warn};

use crate::IgnorePoison;

/// Trigger threshold: a sweep starts when the smallest free-space
/// fraction across the three trees drops below this percentage.
pub const LOW_WATER_PCT: f64 = 10.0;

/// Stop threshold: an in-progress sweep stops as soon as the smallest
/// free-space fraction clears this percentage.
pub const HIGH_WATER_PCT: f64 = 20.0;

/// Tombstone prefix used while deleting an orphaned staging dir. The
/// rename happens under the live-build-id lock; the (slow) recursive
/// delete happens after it is released. `validate_build_id` forbids
/// `.` in build ids, so a tombstone can never collide with a real one.
const TOMBSTONE_PREFIX: &str = ".sweep-";

/// The three mountd-owned roots the sweep watches. They may live on
/// one filesystem (the production XFS loopback) or on separate
/// partitions — `min_free_pct` takes the minimum either way.
#[derive(Debug, Clone)]
pub struct SweepDirs {
    pub cache: PathBuf,
    pub chunks: PathBuf,
    pub staging: PathBuf,
}

/// What one sweep pass did, for logging and tests. Metrics are emitted
/// at the individual removal sites.
#[derive(Debug, Default, PartialEq, Eq)]
pub struct SweepStats {
    /// The low-water trigger fired (free space was below
    /// [`LOW_WATER_PCT`] when the pass started).
    pub triggered: bool,
    /// Files and staging trees removed.
    pub removed: u64,
    /// Sum of the removed entries' sizes.
    pub freed_bytes: u64,
    /// The pass got free space back above the high-water mark. `false`
    /// with `triggered: true` means everything evictable is gone and
    /// the disk is still tight (live staging or foreign data).
    pub reached_high_water: bool,
}

/// Free-space percentage (0–100) of the filesystem hosting `path`,
/// via `statvfs`. `f_bavail` (not `f_bfree`): the writers that matter
/// — builder staging writes and the daemon's own promotes — should not
/// be allowed to eat the root-reserved blocks.
fn free_pct(path: &Path) -> Option<f64> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    // fsblkcnt_t is typedef'd narrower than u64 on some targets; the
    // cast is a no-op on x86_64/aarch64.
    #[allow(clippy::unnecessary_cast)]
    let blocks = s.blocks() as u64;
    #[allow(clippy::unnecessary_cast)]
    let bavail = s.blocks_available() as u64;
    if blocks == 0 {
        return None;
    }
    Some(bavail as f64 * 100.0 / blocks as f64)
}

/// Free bytes (`f_bavail × f_frsize`) of the filesystem hosting `path`.
pub(crate) fn free_bytes(path: &Path) -> Option<u64> {
    let s = nix::sys::statvfs::statvfs(path).ok()?;
    #[allow(clippy::unnecessary_cast)]
    let frsize = s.fragment_size() as u64;
    #[allow(clippy::unnecessary_cast)]
    let bavail = s.blocks_available() as u64;
    Some(bavail.saturating_mul(frsize))
}

/// `min(free%)` across the three trees. `None` only when every
/// `statvfs` failed (paths missing — nothing to sweep anyway).
pub(crate) fn min_free_pct(dirs: &SweepDirs) -> Option<f64> {
    [&dirs.cache, &dirs.chunks, &dirs.staging]
        .into_iter()
        .filter_map(|p| free_pct(p))
        .reduce(f64::min)
}

/// One production sweep pass: emit the free-space gauge, check the
/// low-water trigger against the real filesystems, evict until the
/// high-water mark clears. Called from the daemon's periodic task on
/// the blocking pool.
pub(crate) fn sweep_pass(dirs: &SweepDirs, live_build_ids: &Mutex<HashSet<String>>) -> SweepStats {
    // Sampled every interval regardless of pressure — the design
    // overview's `rio_mountd_cache_free_bytes` gauge.
    if let Some(bytes) = free_bytes(&dirs.cache) {
        metrics::gauge!("rio_mountd_cache_free_bytes").set(bytes as f64);
    }
    sweep_pass_with(
        dirs,
        live_build_ids,
        LOW_WATER_PCT,
        HIGH_WATER_PCT,
        &mut || min_free_pct(dirs),
    )
}

/// The sweep pass with the free-space probe and watermarks injected,
/// so the selection / ordering / stop-condition logic is testable
/// against a tempdir without filling a real filesystem.
///
/// TODO: a triggered pass runs to its stop condition without checking a
/// shutdown token between candidates, so a worst-case eviction (hundreds
/// of thousands of unlinks, minutes) makes SIGTERM wait for the pass —
/// or get killed by the unit's stop grace period mid-eviction (safe but
/// unaccounted). Thread the daemon's shutdown token through here and
/// check it alongside the free-space probe if that ever bites.
pub(crate) fn sweep_pass_with(
    dirs: &SweepDirs,
    live_build_ids: &Mutex<HashSet<String>>,
    low_pct: f64,
    high_pct: f64,
    free: &mut dyn FnMut() -> Option<f64>,
) -> SweepStats {
    let mut stats = SweepStats::default();
    let Some(start_free) = free() else {
        warn!(
            cache = %dirs.cache.display(),
            chunks = %dirs.chunks.display(),
            staging = %dirs.staging.display(),
            "sweep: free-space probe failed on every tree; skipping pass"
        );
        return stats;
    };
    if start_free >= low_pct {
        return stats;
    }
    stats.triggered = true;
    metrics::counter!("rio_mountd_sweep_low_space_total").increment(1);
    info!(
        free_pct = start_free,
        low_pct, high_pct, "sweep: low free space, starting eviction"
    );
    let started = std::time::Instant::now();

    // A failed mid-pass probe stops the WHOLE pass: later tiers must not
    // pay their readdir+sort only to hit the same dead probe, and the
    // closing "exhausted evictable entries" warning would misattribute
    // the early stop.
    let mut probe_failed = false;

    // ── Phase 1: orphaned staging trees.
    let mut orphans = staging_candidates(&dirs.staging, live_build_ids);
    orphans.sort_by_key(|(_, mtime)| *mtime);
    for (name, _) in orphans {
        match free() {
            Some(pct) if pct > high_pct => {
                stats.reached_high_water = true;
                break;
            }
            Some(_) => {}
            // Deleting blindly when we can no longer measure free space
            // is the one wrong answer — stop the pass.
            None => {
                warn!("sweep: free-space probe failed mid-pass; stopping");
                probe_failed = true;
                break;
            }
        }
        if let Some(bytes) = remove_staging_orphan(&dirs.staging, live_build_ids, &name) {
            stats.removed += 1;
            stats.freed_bytes += bytes;
            metrics::counter!("rio_mountd_sweep_removed_total", "tier" => "staging").increment(1);
            metrics::counter!("rio_mountd_sweep_bytes_freed_total", "tier" => "staging")
                .increment(bytes);
        }
    }

    // ── Phase 2 + 3: chunk cache, then backing cache, oldest first.
    if !stats.reached_high_water && !probe_failed {
        for (root, tier) in [(&dirs.chunks, "chunks"), (&dirs.cache, "cache")] {
            match evict_cache_tier(root, tier, high_pct, free, &mut stats) {
                TierOutcome::ClearedHighWater => {
                    stats.reached_high_water = true;
                    break;
                }
                TierOutcome::Exhausted => {}
                TierOutcome::ProbeFailed => {
                    probe_failed = true;
                    break;
                }
            }
        }
    }

    metrics::histogram!("rio_mountd_sweep_seconds").record(started.elapsed().as_secs_f64());
    if stats.reached_high_water {
        info!(
            removed = stats.removed,
            freed_bytes = stats.freed_bytes,
            "sweep: free space back above high-water mark"
        );
    } else if !probe_failed {
        // Probe failures already warned with the real cause; this
        // message is reserved for the genuinely-exhausted case.
        warn!(
            removed = stats.removed,
            freed_bytes = stats.freed_bytes,
            "sweep: exhausted evictable entries while still below the high-water mark — \
             remaining usage is live staging or data outside the mountd-owned trees"
        );
    }
    stats
}

/// Outcome of one cache tier's eviction loop.
enum TierOutcome {
    /// Free space cleared the high-water mark; the pass is done.
    ClearedHighWater,
    /// Every candidate in this tier was processed and the mark still
    /// has not cleared — continue with the next tier.
    Exhausted,
    /// The free-space probe failed; the caller stops the whole pass.
    ProbeFailed,
}

/// Evict `root`'s shard entries oldest-first until the high-water mark
/// clears.
fn evict_cache_tier(
    root: &Path,
    tier: &'static str,
    high_pct: f64,
    free: &mut dyn FnMut() -> Option<f64>,
    stats: &mut SweepStats,
) -> TierOutcome {
    let mut entries = cache_candidates(root);
    entries.sort_by_key(|e| e.mtime);
    for entry in entries {
        match free() {
            Some(pct) if pct > high_pct => return TierOutcome::ClearedHighWater,
            Some(_) => {}
            // Probe failure mid-pass: stop rather than evict blindly.
            None => {
                warn!("sweep: free-space probe failed mid-pass; stopping");
                return TierOutcome::ProbeFailed;
            }
        }
        match std::fs::remove_file(&entry.path) {
            Ok(()) => {
                stats.removed += 1;
                stats.freed_bytes += entry.size;
                metrics::counter!("rio_mountd_sweep_removed_total", "tier" => tier).increment(1);
                metrics::counter!("rio_mountd_sweep_bytes_freed_total", "tier" => tier)
                    .increment(entry.size);
            }
            // Already gone (e.g. a concurrent re-promote's rename-over):
            // not ours to count.
            Err(e) if e.kind() == std::io::ErrorKind::NotFound => {}
            Err(e) => {
                warn!(path = %entry.path.display(), error = %e, "sweep: unlink failed");
            }
        }
    }
    // Tier exhausted; one more probe decides whether the next tier is
    // needed at all (and a dead probe still short-circuits the pass).
    match free() {
        Some(pct) if pct > high_pct => TierOutcome::ClearedHighWater,
        Some(_) => TierOutcome::Exhausted,
        None => {
            warn!("sweep: free-space probe failed mid-pass; stopping");
            TierOutcome::ProbeFailed
        }
    }
}

/// A published cache/chunk entry that may be evicted.
#[derive(Debug)]
struct Candidate {
    path: PathBuf,
    mtime: SystemTime,
    size: u64,
}

/// Published entries under `root/<shard>/<file>`. In-flight
/// placeholders (`.promoting`, `.tmp`, `.partial`) are skipped — they
/// belong to a live promote or fill, and the startup orphan scan owns
/// the stale ones.
fn cache_candidates(root: &Path) -> Vec<Candidate> {
    let mut out = Vec::new();
    for shard in read_dir_root(root) {
        if !shard.file_type().is_ok_and(|t| t.is_dir()) {
            continue;
        }
        for entry in read_dir_or_empty(&shard.path()) {
            let name = entry.file_name();
            // Non-UTF-8 names are foreign junk (every name the daemon
            // publishes is ASCII hex) — same hands-off policy as the
            // startup orphan scan.
            let Some(name) = name.to_str() else { continue };
            if name.ends_with(".promoting") || name.ends_with(".tmp") || name.ends_with(".partial")
            {
                continue;
            }
            let Ok(meta) = entry.metadata() else { continue };
            if !meta.is_file() {
                continue;
            }
            out.push(Candidate {
                path: entry.path(),
                mtime: meta.modified().unwrap_or(SystemTime::UNIX_EPOCH),
                size: meta.len(),
            });
        }
    }
    out
}

/// Staging children that are sweep candidates: tombstones from an
/// interrupted earlier sweep, plus any directory whose name is not a
/// live `build_id`. The liveness check here is only a pre-filter — the
/// authoritative re-check happens under the lock in
/// [`remove_staging_orphan`].
fn staging_candidates(
    staging: &Path,
    live_build_ids: &Mutex<HashSet<String>>,
) -> Vec<(String, SystemTime)> {
    let live = live_build_ids.lock().ignore_poison().clone();
    read_dir_root(staging)
        .into_iter()
        .filter_map(|entry| {
            let name = entry.file_name().into_string().ok()?;
            let meta = entry.metadata().ok()?;
            if !meta.is_dir() {
                return None;
            }
            if !name.starts_with(TOMBSTONE_PREFIX) && live.contains(&name) {
                return None;
            }
            Some((name, meta.modified().unwrap_or(SystemTime::UNIX_EPOCH)))
        })
        .collect()
}

/// Remove one orphaned staging dir. Returns the bytes it held, or
/// `None` if it was skipped (became live, or already gone).
///
/// Race safety against a concurrent `Mount{build_id}` re-using the same
/// id: `handle_mount` claims the id in `live_build_ids` *before* it
/// touches the filesystem, and this function re-checks the claim and
/// renames the dir to a tombstone *while holding that same lock*. So
/// either the Mount claimed first (we skip here), or we renamed first
/// and the Mount's `mkdirat` creates a fresh, empty staging dir — it
/// can never inherit a half-deleted one, and we never delete a live
/// build's files. Only the rename happens under the lock; the
/// recursive delete runs after it is released.
fn remove_staging_orphan(
    staging: &Path,
    live_build_ids: &Mutex<HashSet<String>>,
    name: &str,
) -> Option<u64> {
    let doomed = if name.starts_with(TOMBSTONE_PREFIX) {
        // Already tombstoned by an earlier (interrupted) sweep. A
        // tombstone is outside the build-id namespace (build ids
        // cannot contain `.`), so no liveness check applies.
        staging.join(name)
    } else {
        let tomb = staging.join(format!("{TOMBSTONE_PREFIX}{name}"));
        {
            let live = live_build_ids.lock().ignore_poison();
            if live.contains(name) {
                return None;
            }
            std::fs::rename(staging.join(name), &tomb).ok()?;
        }
        tomb
    };
    let bytes = dir_size(&doomed);
    match std::fs::remove_dir_all(&doomed) {
        Ok(()) => {
            info!(orphan = %doomed.display(), bytes, "sweep: reaped orphan staging dir");
            Some(bytes)
        }
        Err(e) => {
            warn!(orphan = %doomed.display(), error = %e, "sweep: could not remove orphan staging dir");
            // Partially removed at best; report what we measured so the
            // caller's accounting stays an upper bound.
            Some(bytes)
        }
    }
}

/// Recursive size of a directory tree (sum of file sizes). Best-effort:
/// unreadable entries count as zero.
fn dir_size(path: &Path) -> u64 {
    let mut total = 0;
    for entry in read_dir_or_empty(path) {
        let Ok(meta) = entry.metadata() else { continue };
        if meta.is_dir() {
            total += dir_size(&entry.path());
        } else {
            total += meta.len();
        }
    }
    total
}

/// `read_dir` that swallows errors (missing dir, permission) — the
/// sweep treats anything it cannot list as not evictable. Used for
/// shard subdirs and size accounting, where a vanished entry mid-walk
/// is routine.
fn read_dir_or_empty(path: &Path) -> Vec<std::fs::DirEntry> {
    std::fs::read_dir(path)
        .map(|rd| rd.filter_map(Result::ok).collect())
        .unwrap_or_default()
}

/// `read_dir` over a tier ROOT (cache/, chunks/, staging/). Same
/// fallback as [`read_dir_or_empty`], but warns: the daemon creates the
/// roots at startup, so an unlistable root means the sweep is silently
/// blind to a whole tier — worth a log line, unlike the per-shard case.
fn read_dir_root(root: &Path) -> Vec<std::fs::DirEntry> {
    match std::fs::read_dir(root) {
        Ok(rd) => rd.filter_map(Result::ok).collect(),
        Err(e) => {
            warn!(root = %root.display(), error = %e, "sweep: cannot list tier root");
            Vec::new()
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::os::fd::AsRawFd;

    /// A `SweepDirs` triple inside one tempdir plus helpers to stage
    /// content with controlled mtimes and to fake the free-space probe.
    struct Fx {
        _tmp: tempfile::TempDir,
        dirs: SweepDirs,
        live: Mutex<HashSet<String>>,
    }

    impl Fx {
        fn new() -> Self {
            let tmp = tempfile::tempdir().unwrap();
            let dirs = SweepDirs {
                cache: tmp.path().join("cache"),
                chunks: tmp.path().join("chunks"),
                staging: tmp.path().join("staging"),
            };
            for d in [&dirs.cache, &dirs.chunks, &dirs.staging] {
                std::fs::create_dir_all(d).unwrap();
            }
            Self {
                _tmp: tmp,
                dirs,
                live: Mutex::new(HashSet::new()),
            }
        }

        /// Write `size` bytes under `root/ab/<name>` with the given
        /// mtime (seconds since epoch).
        fn put(&self, root: &Path, name: &str, size: usize, mtime_secs: i64) -> PathBuf {
            let dir = root.join("ab");
            std::fs::create_dir_all(&dir).unwrap();
            let path = dir.join(name);
            std::fs::write(&path, vec![0u8; size]).unwrap();
            set_mtime(&path, mtime_secs);
            path
        }

        /// Create `staging/<build_id>/` with one file of `size` bytes.
        fn stage_build(&self, build_id: &str, size: usize, mtime_secs: i64) -> PathBuf {
            let dir = self.dirs.staging.join(build_id);
            std::fs::create_dir_all(&dir).unwrap();
            std::fs::write(dir.join("blob"), vec![0u8; size]).unwrap();
            set_mtime(&dir, mtime_secs);
            dir
        }

        /// Bytes currently held across all three trees.
        fn used(&self) -> u64 {
            dir_size(&self.dirs.cache) + dir_size(&self.dirs.chunks) + dir_size(&self.dirs.staging)
        }

        /// Run one pass against a pretend disk of `capacity` bytes
        /// whose only contents are this fixture's trees — the probe
        /// reports free space as a function of what is still on disk,
        /// so the stop condition reacts to the sweep's own deletions.
        fn sweep(&self, capacity: u64) -> SweepStats {
            let mut probe =
                || Some((capacity.saturating_sub(self.used())) as f64 * 100.0 / capacity as f64);
            sweep_pass_with(
                &self.dirs,
                &self.live,
                LOW_WATER_PCT,
                HIGH_WATER_PCT,
                &mut probe,
            )
        }
    }

    fn set_mtime(path: &Path, secs: i64) {
        let f = std::fs::File::open(path).unwrap();
        let ts = nix::libc::timespec {
            tv_sec: secs,
            tv_nsec: 0,
        };
        // SAFETY: `f` is a live fd for the duration of the call and the
        // two-element timespec array outlives it.
        let r = unsafe { nix::libc::futimens(f.as_raw_fd(), [ts, ts].as_ptr()) };
        assert_eq!(r, 0, "futimens failed");
    }

    /// Plenty of free space → the pass is a no-op even with old entries
    /// present.
    #[test]
    fn no_eviction_above_low_water() {
        let fx = Fx::new();
        let kept = fx.put(&fx.dirs.cache, "old", 1024, 1_000);
        // 1 KiB used on a 1 MiB "disk" → ~99.9% free.
        let stats = fx.sweep(1 << 20);
        assert_eq!(stats, SweepStats::default());
        assert!(kept.exists());
    }

    /// Below the low-water mark the sweep removes chunks before cache,
    /// oldest first within each tier, and stops as soon as the
    /// high-water mark clears — newer cache entries survive.
    // r[verify builder.fs.node-digest-cache]
    #[test]
    fn evicts_chunks_first_oldest_first_and_stops_at_high_water() {
        let fx = Fx::new();
        const KIB: usize = 1024;
        // 10 KiB "disk". Usage: 9.5 KiB → ~5% free (< 10% trigger).
        // High water (20%) needs ≥ 2 KiB free → must remove ~1.5 KiB.
        let chunk_old = fx.put(&fx.dirs.chunks, "chunk-old", KIB, 1_000);
        let chunk_new = fx.put(&fx.dirs.chunks, "chunk-new", KIB, 2_000);
        let cache_old = fx.put(&fx.dirs.cache, "cache-old", KIB, 500);
        let cache_new = fx.put(&fx.dirs.cache, "cache-new", 6 * KIB + 512, 3_000);

        let stats = fx.sweep(10 * KIB as u64);

        assert!(stats.triggered);
        assert!(stats.reached_high_water, "stats: {stats:?}");
        // Chunks go before the (older!) cache entry; oldest chunk first.
        assert!(!chunk_old.exists(), "oldest chunk evicted first");
        assert!(
            !chunk_new.exists(),
            "second chunk needed to clear high water"
        );
        assert!(
            cache_old.exists(),
            "cache tier must not be touched once high water cleared"
        );
        assert!(cache_new.exists());
        assert_eq!(stats.removed, 2);
        assert_eq!(stats.freed_bytes, 2 * KIB as u64);
    }

    /// When the chunk tier alone is not enough, the cache tier is
    /// evicted too — oldest first.
    #[test]
    fn falls_through_to_cache_tier_when_chunks_insufficient() {
        let fx = Fx::new();
        const KIB: usize = 1024;
        let chunk = fx.put(&fx.dirs.chunks, "chunk", KIB, 2_000);
        let cache_old = fx.put(&fx.dirs.cache, "cache-old", 3 * KIB, 500);
        let cache_new = fx.put(&fx.dirs.cache, "cache-new", 5 * KIB + 512, 3_000);

        let stats = fx.sweep(10 * KIB as u64);

        assert!(stats.reached_high_water, "stats: {stats:?}");
        assert!(!chunk.exists());
        assert!(!cache_old.exists(), "older cache entry evicted");
        assert!(cache_new.exists(), "newest cache entry survives");
    }

    /// Exhausting every candidate without clearing the high-water mark
    /// is reported (not an infinite loop, not a panic).
    #[test]
    fn reports_unrelieved_pressure_when_nothing_left_to_evict() {
        let fx = Fx::new();
        // The only usage is a LIVE build's staging — untouchable.
        // 9.5 KiB on a 10 KiB disk → 5% free, below the trigger.
        fx.live.lock().unwrap().insert("live-build".into());
        let staged = fx.stage_build("live-build", 9 * 1024 + 512, 1_000);

        let stats = fx.sweep(10 * 1024);

        assert!(stats.triggered);
        assert!(!stats.reached_high_water);
        assert_eq!(stats.removed, 0);
        assert!(staged.exists(), "live build staging must never be swept");
    }

    /// Orphaned staging dirs (no live connection) are removed; live
    /// ones and in-flight placeholders are not.
    // r[verify builder.fs.node-digest-cache]
    #[test]
    fn staging_orphan_filter_spares_live_builds_and_placeholders() {
        let fx = Fx::new();
        const KIB: usize = 1024;
        fx.live.lock().unwrap().insert("live-build".into());
        let live_dir = fx.stage_build("live-build", KIB, 1_000);
        let dead_dir = fx.stage_build("dead-build", 6 * KIB, 900);
        // Tombstone left by an interrupted earlier sweep: always fair game.
        let tomb_dir = fx.stage_build(".sweep-older-crash", KIB, 800);
        // In-flight promote placeholder in the cache: never a candidate.
        let promoting = fx.put(&fx.dirs.cache, "aaaa.promoting", KIB, 100);
        let partial = fx.put(&fx.dirs.cache, "bbbb.partial", KIB, 100);

        // 10 KiB used on a 10 KiB disk → 0% free → triggered; reaping
        // the two orphans (dead-build 6 KiB + tombstone 1 KiB) clears it.
        let stats = fx.sweep(10 * KIB as u64);

        assert!(stats.triggered);
        assert!(stats.reached_high_water, "stats: {stats:?}");
        assert!(live_dir.exists(), "live build staging spared");
        assert!(!dead_dir.exists(), "orphan staging reaped");
        assert!(!tomb_dir.exists(), "old tombstone reaped");
        assert!(promoting.exists(), "in-flight .promoting spared");
        assert!(partial.exists(), ".partial spared");
    }

    /// The cache-tier candidate filter, exercised by a pass that
    /// genuinely reaches the cache tier (the probe never clears the
    /// high-water mark, so pressure stays on past the staging and chunk
    /// phases). In-flight placeholders (`.promoting`, `.tmp`,
    /// `.partial`) and non-file entries (a stray subdirectory) must
    /// survive while published entries around them are evicted —
    /// proving the spare is the filter at work, not the pass stopping
    /// early.
    // r[verify builder.fs.node-digest-cache]
    #[test]
    fn cache_tier_eviction_skips_placeholders_and_subdirs() {
        let fx = Fx::new();
        const KIB: usize = 1024;
        let evictable_old = fx.put(&fx.dirs.cache, "aaaa", KIB, 1_000);
        let evictable_new = fx.put(&fx.dirs.cache, "bbbb", KIB, 2_000);
        let promoting = fx.put(&fx.dirs.cache, "cccc.promoting", KIB, 100);
        let tmp = fx.put(&fx.dirs.cache, "dddd.tmp", KIB, 100);
        let partial = fx.put(&fx.dirs.cache, "eeee.partial", KIB, 100);
        // A stray subdirectory inside a shard (foreign junk): not a
        // regular file, never a candidate.
        let subdir = fx.dirs.cache.join("ab").join("not-an-entry");
        std::fs::create_dir_all(&subdir).unwrap();
        std::fs::write(subdir.join("inner"), vec![0u8; KIB]).unwrap();

        // The probe reports pressure on every call: the pass runs every
        // tier to exhaustion instead of clearing high water in phase 1.
        let stats = sweep_pass_with(
            &fx.dirs,
            &fx.live,
            LOW_WATER_PCT,
            HIGH_WATER_PCT,
            &mut || Some(5.0),
        );

        assert!(stats.triggered);
        assert!(
            !stats.reached_high_water,
            "the probe never clears, so the pass must run out of candidates: {stats:?}"
        );
        assert!(
            !evictable_old.exists() && !evictable_new.exists(),
            "published cache entries ARE evicted — the tier was genuinely processed"
        );
        assert_eq!(stats.removed, 2, "only the two published entries count");
        assert!(promoting.exists(), "in-flight .promoting spared");
        assert!(tmp.exists(), "in-flight .tmp spared");
        assert!(partial.exists(), "in-flight .partial spared");
        assert!(
            subdir.join("inner").exists(),
            "a subdirectory inside a shard is not a candidate"
        );
    }

    /// The orphan delete is atomic against a Mount that claims the same
    /// build_id between the candidate listing and the removal: once the
    /// id is in `live_build_ids`, the per-dir re-check skips it.
    // r[verify builder.fs.node-digest-cache]
    #[test]
    fn staging_orphan_recheck_skips_concurrently_mounted_build() {
        let fx = Fx::new();
        let dir = fx.stage_build("reused-id", 1024, 1_000);

        // Candidate listing sees it as an orphan…
        let candidates = staging_candidates(&fx.dirs.staging, &fx.live);
        assert_eq!(candidates.len(), 1);

        // …then a Mount{reused-id} claims it (handle_mount inserts into
        // live_build_ids before touching the filesystem)…
        fx.live.lock().unwrap().insert("reused-id".into());

        // …so the removal re-check must skip it.
        let removed = remove_staging_orphan(&fx.dirs.staging, &fx.live, "reused-id");
        assert_eq!(removed, None);
        assert!(dir.exists());
    }

    /// A probe that dies mid-pass stops the sweep instead of letting it
    /// evict blindly with no stop condition — and stops the WHOLE pass:
    /// later tiers are not listed only to hit the same dead probe.
    #[test]
    fn probe_failure_mid_pass_stops_eviction() {
        let fx = Fx::new();
        let kept_a = fx.put(&fx.dirs.chunks, "chunk", 1024, 1_000);
        let kept_b = fx.put(&fx.dirs.cache, "entry", 1024, 2_000);

        // First call (the trigger check) reports pressure; every later
        // call fails.
        let mut calls = 0u32;
        let mut probe = || {
            calls += 1;
            (calls == 1).then_some(5.0)
        };
        let stats = sweep_pass_with(
            &fx.dirs,
            &fx.live,
            LOW_WATER_PCT,
            HIGH_WATER_PCT,
            &mut probe,
        );

        assert!(stats.triggered);
        assert!(!stats.reached_high_water);
        assert_eq!(stats.removed, 0, "nothing may be evicted blind");
        assert!(kept_a.exists());
        assert!(kept_b.exists());
        assert_eq!(
            calls, 2,
            "the pass must stop at the first failed mid-pass probe (trigger + one \
             chunk-tier check), not fall through to the cache tier"
        );
    }

    /// Missing trees (fresh node, dirs not created yet) are handled
    /// without error.
    #[test]
    fn missing_dirs_do_not_error() {
        let tmp = tempfile::tempdir().unwrap();
        let dirs = SweepDirs {
            cache: tmp.path().join("nope/cache"),
            chunks: tmp.path().join("nope/chunks"),
            staging: tmp.path().join("nope/staging"),
        };
        let live = Mutex::new(HashSet::new());
        let stats = sweep_pass_with(&dirs, &live, LOW_WATER_PCT, HIGH_WATER_PCT, &mut || {
            Some(5.0)
        });
        assert!(stats.triggered);
        assert_eq!(stats.removed, 0);
    }

    /// `min_free_pct` over real filesystems returns a plausible value
    /// (the production probe path, minus the pressure).
    #[test]
    fn min_free_pct_probes_real_filesystems() {
        let fx = Fx::new();
        let pct = min_free_pct(&fx.dirs).expect("tempdir statvfs");
        assert!((0.0..=100.0).contains(&pct), "got {pct}");
        assert!(free_bytes(&fx.dirs.cache).is_some());
    }
}
