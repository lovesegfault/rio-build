//! LRU cache for FUSE store paths backed by local SSD.
//!
//! Cached store paths are materialized as directory trees on disk (not stored
//! as NAR blobs). On cache miss the worker fetches the NAR via `GetPath`,
//! parses it, and extracts it to disk via [`rio_nix::nar::extract_to_path`].
// r[impl builder.fuse.cache-lru]
//!
//! A lightweight SQLite index tracks cached paths, sizes, and access
//! timestamps for LRU eviction decisions.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::{Arc, Condvar, Mutex, RwLock};
use std::time::Duration;

use rio_common::bloom::BloomFilter;
use sqlx::SqlitePool;
use sqlx::sqlite::{SqliteConnectOptions, SqliteJournalMode, SqlitePoolOptions, SqliteSynchronous};
use tokio::runtime::Handle;

/// Errors from cache operations.
#[derive(Debug, thiserror::Error)]
pub enum CacheError {
    #[error("sqlite error: {0}")]
    Sqlite(#[from] sqlx::Error),
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
}

/// Per-path coordination for in-flight fetches.
///
/// Waiters block on `cv` until `done` flips to true.
pub struct InflightEntry {
    done: Mutex<bool>,
    cv: Condvar,
}

impl InflightEntry {
    /// Block the current thread until the fetch completes or `timeout` elapses.
    ///
    /// Returns `true` if the fetch completed, `false` on timeout. A `false`
    /// return does NOT mean the fetcher is dead — the guard's `Drop` impl
    /// fires on success, error, and panic alike. `false` means only "still
    /// working after `timeout`." Callers that need to distinguish "slow"
    /// from "dead" should loop on `wait()` while [`Self::is_done`] stays
    /// `false`; the fetcher's own timeout (`GRPC_STREAM_TIMEOUT`) is the
    /// real deadline.
    pub fn wait(&self, timeout: Duration) -> bool {
        let done = self.done.lock().unwrap_or_else(|e| e.into_inner());
        let (done, wait_result) = self
            .cv
            .wait_timeout_while(done, timeout, |d| !*d)
            .unwrap_or_else(|e| e.into_inner());
        !wait_result.timed_out() && *done
    }

    /// Cheap check: has the fetcher finished (guard dropped)? No condvar wait.
    /// Use after a timed-out `wait()` to decide whether to wait again.
    pub fn is_done(&self) -> bool {
        *self.done.lock().unwrap_or_else(|e| e.into_inner())
    }
}

/// Outcome of [`Cache::try_start_fetch`].
pub enum FetchClaim<'a> {
    /// Caller owns the fetch. The guard notifies all waiters when dropped,
    /// regardless of fetch success or failure.
    Fetch(FetchGuard<'a>),
    /// Another thread is already fetching. Caller should wait on the entry.
    WaitFor(Arc<InflightEntry>),
}

/// RAII guard for an in-flight fetch claim.
///
/// On drop, removes the path from the inflight map and notifies all waiters.
/// This fires even if the fetcher panics, ensuring waiters are never stuck
/// indefinitely (they also have a belt-and-suspenders timeout).
pub struct FetchGuard<'a> {
    cache: &'a Cache,
    path: String,
}

impl Drop for FetchGuard<'_> {
    fn drop(&mut self) {
        let entry = {
            let mut inflight = self
                .cache
                .inflight
                .lock()
                .unwrap_or_else(|e| e.into_inner());
            inflight.remove(&self.path)
        };
        if let Some(entry) = entry {
            *entry.done.lock().unwrap_or_else(|e| e.into_inner()) = true;
            entry.cv.notify_all();
        }
    }
}

/// Blocking counting semaphore for FUSE-thread fetch concurrency.
///
/// `tokio::sync::Semaphore` is async; we're in a sync FUSE callback that
/// hasn't entered `block_on` yet. Building on `Mutex+Condvar` (same as
/// `InflightEntry`) avoids a dependency and a nested-block_on wart.
///
/// No `try_acquire` — a FUSE-initiated fetch MUST eventually happen (the
/// build depends on it). We always block; the semaphore just serializes
/// the rate so some FUSE threads stay free for the hot path.
pub(super) struct FetchSemaphore {
    permits: Mutex<usize>,
    cv: Condvar,
}

impl FetchSemaphore {
    pub(super) fn new(permits: usize) -> Self {
        Self {
            permits: Mutex::new(permits),
            cv: Condvar::new(),
        }
    }

    pub(super) fn acquire(&self) -> FetchPermit<'_> {
        let mut p = self.permits.lock().unwrap_or_else(|e| e.into_inner());
        while *p == 0 {
            p = self.cv.wait(p).unwrap_or_else(|e| e.into_inner());
        }
        *p -= 1;
        FetchPermit { sem: self }
    }
}

pub(super) struct FetchPermit<'a> {
    sem: &'a FetchSemaphore,
}

impl Drop for FetchPermit<'_> {
    fn drop(&mut self) {
        *self.sem.permits.lock().unwrap_or_else(|e| e.into_inner()) += 1;
        self.sem.cv.notify_one();
    }
}

/// Metadata for a cached store path (eviction candidate).
#[derive(Debug, Clone)]
pub struct CacheEntry {
    /// The store path basename (e.g. "abc...-hello-1.0").
    pub store_path: String,
    /// Size in bytes on disk.
    pub size_bytes: u64,
}

/// Grace period (seconds) protecting recently-touched entries from eviction.
///
/// `get_and_touch()` atomically stamps `last_access = now` BEFORE returning
/// a cached path to the caller. The eviction query filters out entries with
/// `last_access >= now - EVICT_GRACE_SECS`, so any entry returned by
/// `get_path()` has at least this long before it becomes an eviction
/// candidate — plenty of time for the caller's subsequent `open()` /
/// `symlink_metadata()` syscall. This closes the TOCTOU where eviction
/// raced the caller's open without adding a syscall to the hot path.
const EVICT_GRACE_SECS: i64 = 5;

/// LRU cache manager backed by local SSD with SQLite metadata index.
///
/// Thread-safe: uses a connection pool for concurrent access. FUSE callbacks
/// are synchronous, so all DB ops are bridged via `Handle::block_on`.
pub struct Cache {
    /// Root directory where cached paths are materialized.
    cache_dir: PathBuf,
    /// Maximum cache size in bytes.
    max_size_bytes: u64,
    /// SQLite metadata index (async pool, bridged via block_on).
    pool: SqlitePool,
    /// Tokio runtime handle for block_on bridging.
    runtime: Handle,
    /// In-flight fetches with per-path condition variables for waiter notification.
    inflight: Mutex<HashMap<String, Arc<InflightEntry>>>,
    /// Bloom filter of cached store paths, for heartbeat locality scoring.
    ///
    /// Built once at startup from the SQLite inventory, then incrementally
    /// updated on each `insert()`. Never removed from (bloom filters don't
    /// support deletion) — evicted paths stay in the filter as stale
    /// positives. That's fine: the scheduler treats the filter as a HINT
    /// for scoring, not ground truth. A stale positive just means a
    /// worker scores slightly better than it should for one dispatch;
    /// the actual fetch still happens if the path isn't really cached.
    ///
    /// Rebuilt on process restart (filter isn't persisted). Restarting
    /// with a warm SSD cache but a fresh filter means the first few
    /// heartbeats under-report — harmless, corrects quickly as `new()`
    /// bulk-inserts from SQLite.
    ///
    /// `Arc<RwLock>` because: heartbeat loop reads (clone-and-serialize
    /// every 10s), `insert()` writes. Reads vastly outnumber writes.
    /// parking_lot would be marginally faster but std's RwLock is fine
    /// for this access pattern.
    bloom: Arc<RwLock<BloomFilter>>,
    /// I-110c: per-path manifest hints, keyed by store basename.
    /// Primed by the executor (one `BatchGetManifest` before daemon
    /// spawn); consumed by `fetch_extract_insert` (which removes on
    /// read so memory doesn't accumulate across builds). When a hint
    /// is present, `GetPath` carries it and the store skips its two
    /// PG lookups — the S3 chunk fetch still happens per-path, but
    /// PG sees ≤2 queries/builder for the whole input closure instead
    /// of ~1600.
    ///
    /// `Mutex<HashMap>` like `inflight` — short critical sections
    /// (insert/remove only), called from FUSE threads + the executor.
    manifest_hints: Mutex<HashMap<String, rio_proto::types::ManifestHint>>,
    /// JIT-fetch allowlist: store basenames the current build's input
    /// closure contains, with their NAR sizes (for size-aware fetch
    /// timeout). Populated by [`Self::register_inputs`] (executor,
    /// after `compute_input_closure`, before daemon spawn). FUSE
    /// `lookup()` consults it via [`Self::jit_classify`]: present →
    /// block-and-fetch; absent → fast ENOENT. NEVER returns ENOENT
    /// for a present entry — fetch failure → EIO so overlay doesn't
    /// negative-cache (the I-043 redesign).
    ///
    /// `RwLock<Option<_>>`:
    ///   `None`  → JIT not armed (tests, `RIO_BUILDER_JIT_FETCH=0`,
    ///             or `register_inputs` not yet called): lookup falls
    ///             back to legacy "gRPC anything store-path-shaped".
    ///   `Some`  → JIT armed: ONLY names in the map are fetched.
    ///
    /// Lives on `Cache` (not `NixStoreFs`) for the same reason as
    /// `manifest_hints` and `bloom`: `NixStoreFs` is consumed by
    /// `fuser::spawn_mount2`; the executor only holds `Arc<Cache>`.
    /// Not thread-local: FUSE callbacks run on `fuser`'s own thread
    /// pool, not the executor's tokio worker.
    known_inputs: RwLock<Option<HashMap<String, u64 /* nar_size */>>>,
}

/// Classification of a top-level FUSE `lookup` name against the
/// per-build JIT allowlist. See [`Cache::jit_classify`].
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JitClass {
    /// JIT not armed (`register_inputs` never called, or `clear_inputs`
    /// since). `lookup` falls back to legacy heuristic gRPC.
    NotArmed,
    /// JIT armed; name is NOT a registered input. Daemon probes
    /// (`.lock`, `.chroot`, output-path checks) land here. Fast ENOENT,
    /// no store contact.
    NotInput,
    /// JIT armed; name IS a registered input. `lookup` MUST block on
    /// fetch and on any failure return EIO (never ENOENT — overlay
    /// would negative-cache it).
    KnownInput { nar_size: u64 },
}

/// Default expected cache inventory size for bloom filter sizing. A
/// worker with a 100 GB SSD and typical 10 MB outputs holds ~10k paths.
/// We size for 50k at 1% FPR — headroom for workers with bigger caches,
/// cheap enough (~60 KB filter) that over-sizing doesn't matter.
///
/// If the actual inventory blows past this, FPR degrades gracefully
/// (more false positives, scheduler scoring gets noisier, but nothing
/// breaks). Operators with larger caches — or long-lived low-ordinal
/// StatefulSet workers that churn past 50k inserts via eviction —
/// override via `worker.toml bloom_expected_items`. Oversizing is
/// cheap: ~1.2 bytes/item at 1% FPR, so 500k items = ~600 KB filter.
const BLOOM_EXPECTED_ITEMS_DEFAULT: usize = 50_000;
const BLOOM_TARGET_FPR: f64 = 0.01;

impl Cache {
    /// Create a new cache rooted at `cache_dir` with the given size limit.
    ///
    /// Creates the cache directory and SQLite index if they don't exist.
    /// Must be called from within a tokio runtime (captures the current `Handle`).
    ///
    /// `bloom_expected_items` — bloom filter capacity. `None` →
    /// `BLOOM_EXPECTED_ITEMS_DEFAULT` (50 000). Operators with
    /// long-lived StatefulSet workers override via `worker.toml`;
    /// the filter never shrinks (evicted paths stay as stale
    /// positives) so churn eventually saturates the default.
    pub async fn new(
        cache_dir: PathBuf,
        max_size_gb: u64,
        bloom_expected_items: Option<usize>,
        ephemeral: bool,
    ) -> Result<Self, CacheError> {
        std::fs::create_dir_all(&cache_dir)?;

        // Clean up stale .tmp-* directories left behind by interrupted NAR
        // extractions (fetch_and_extract extracts to a sibling tmp dir then
        // renames atomically; a crash mid-extraction leaves the tmp dir behind).
        Self::clean_stale_tmp_dirs(&cache_dir);

        let runtime = Handle::current();

        // r[impl builder.fuse.cache-ephemeral-memory]
        let pool = if ephemeral {
            // Ephemeral builders (RIO_EPHEMERAL=1, the P0537 default)
            // execute exactly one build then exit; the pod's emptyDir
            // filesystem is discarded. Persisting the cache index to
            // disk is pointless and on tiny-class node storage costs
            // >1s per write under load (I-141: ~10s wasted per build).
            //
            // :memory: with a pool: each pooled connection would be a
            // SEPARATE in-memory DB. Pin to one connection and disable
            // idle/lifetime reaping (closing it would drop the DB and
            // lose the index mid-build). Single-connection serialization
            // is fine — in-memory SQLite ops are µs-scale and FUSE
            // already serializes on the inflight map for the hot path.
            SqlitePoolOptions::new()
                .min_connections(1)
                .max_connections(1)
                .idle_timeout(None)
                .max_lifetime(None)
                .connect_with(SqliteConnectOptions::new().filename(":memory:"))
                .await?
        } else {
            // Long-lived (StatefulSet) builders persist across builds;
            // the on-disk index lets them remember what's already
            // fetched. WAL + synchronous=NORMAL via connect options so
            // EVERY pooled connection gets it — a post-connect
            // `PRAGMA synchronous` only hits the one connection that
            // ran it; other pool connections stay at FULL and fsync
            // every commit. Durability is not critical: this is a
            // local cache index, rio-store is the source of truth, and
            // a lost row just means one extra fetch.
            SqlitePoolOptions::new()
                .connect_with(
                    SqliteConnectOptions::new()
                        .filename(cache_dir.join("cache_index.sqlite"))
                        .create_if_missing(true)
                        .journal_mode(SqliteJournalMode::Wal)
                        .synchronous(SqliteSynchronous::Normal),
                )
                .await?
        };

        sqlx::query(
            r#"CREATE TABLE IF NOT EXISTS cached_paths (
                store_path TEXT PRIMARY KEY NOT NULL,
                size_bytes INTEGER NOT NULL,
                last_access INTEGER NOT NULL
            )"#,
        )
        .execute(&pool)
        .await?;
        sqlx::query("CREATE INDEX IF NOT EXISTS idx_last_access ON cached_paths(last_access)")
            .execute(&pool)
            .await?;

        // Build the bloom filter from the existing SQLite inventory.
        // Startup-only cost: one full-table scan + N inserts. For 10k
        // paths that's ~100ms — acceptable once at boot.
        //
        // We don't rebuild periodically — evicted paths stay in the
        // filter as stale positives (see the struct field doc for why
        // that's OK). Only restart clears it.
        let expected = bloom_expected_items.unwrap_or(BLOOM_EXPECTED_ITEMS_DEFAULT);
        let mut bloom = BloomFilter::new(expected, BLOOM_TARGET_FPR);
        let existing: Vec<(String,)> = sqlx::query_as("SELECT store_path FROM cached_paths")
            .fetch_all(&pool)
            .await?;
        for (path,) in &existing {
            bloom.insert(path);
        }
        tracing::info!(
            paths = existing.len(),
            bloom_expected_items = expected,
            bloom_bits = bloom.num_bits(),
            bloom_k = bloom.hash_count(),
            bloom_bytes = bloom.byte_len(),
            "FUSE cache bloom filter initialized"
        );

        Ok(Self {
            cache_dir,
            max_size_bytes: max_size_gb * 1024 * 1024 * 1024,
            pool,
            runtime,
            inflight: Mutex::new(HashMap::new()),
            bloom: Arc::new(RwLock::new(bloom)),
            manifest_hints: Mutex::new(HashMap::new()),
            known_inputs: RwLock::new(None),
        })
    }

    /// Arm JIT fetch for the upcoming build. `inputs` is the
    /// `(basename, nar_size)` projection of `compute_input_closure`'s
    /// result (already computed as `input_sized` at the executor).
    ///
    /// EXTENDS the existing map (no implicit clear): store paths are
    /// immutable, so a basename registered by build A is still valid
    /// for build N on a multi-build STS pod. Memory: ~60 B/entry ×
    /// ~1k paths/build; ephemeral pods (the default) exit after one
    /// build so it never accumulates. STS callers MAY call
    /// [`Self::clear_inputs`] at build end if growth becomes a concern.
    // r[impl builder.fuse.jit-register]
    pub fn register_inputs(&self, inputs: impl IntoIterator<Item = (String, u64)>) {
        let mut g = self.known_inputs.write().unwrap_or_else(|e| e.into_inner());
        g.get_or_insert_with(HashMap::new).extend(inputs);
    }

    /// Disarm JIT: drop the allowlist back to `None`. Subsequent
    /// `lookup`s fall back to [`JitClass::NotArmed`] (legacy heuristic
    /// gRPC) until the next `register_inputs`.
    pub fn clear_inputs(&self) {
        *self.known_inputs.write().unwrap_or_else(|e| e.into_inner()) = None;
    }

    /// Classify `basename` against the JIT allowlist. See [`JitClass`].
    /// `None` → not armed (legacy lookup). Armed + present → block-
    /// and-fetch with size-aware timeout. Armed + absent → fast ENOENT.
    pub fn jit_classify(&self, basename: &str) -> JitClass {
        match &*self.known_inputs.read().unwrap_or_else(|e| e.into_inner()) {
            None => JitClass::NotArmed,
            Some(m) => match m.get(basename) {
                Some(&nar_size) => JitClass::KnownInput { nar_size },
                None => JitClass::NotInput,
            },
        }
    }

    /// Current size of the JIT allowlist (`None` → 0). For the
    /// `rio_builder_jit_inputs_registered` gauge.
    pub fn known_inputs_len(&self) -> usize {
        self.known_inputs
            .read()
            .unwrap_or_else(|e| e.into_inner())
            .as_ref()
            .map_or(0, HashMap::len)
    }

    /// Snapshot the bloom filter for heartbeat serialization.
    ///
    /// Clones the filter under a read lock — cheap (`Vec<u8>` clone, ~60 KB
    /// for the default sizing). The heartbeat loop calls this every 10s;
    /// the clone avoids holding the read lock across the gRPC send (which
    /// would block `insert()` for the duration of a network roundtrip).
    ///
    /// Returns the clone, not a guard — caller serializes at leisure.
    pub fn bloom_snapshot(&self) -> BloomFilter {
        self.bloom.read().unwrap_or_else(|e| e.into_inner()).clone()
    }

    /// Get a shareable handle to the bloom filter.
    ///
    /// main.rs needs this because the `Cache` itself is MOVED into
    /// `mount_fuse_background` (the FUSE session owns it). But the
    /// heartbeat loop (a separate spawned task) also needs to read
    /// the bloom every 10s. Giving it an Arc-clone of the inner
    /// `Arc<RwLock<BloomFilter>>` lets both coexist: FUSE owns the
    /// Cache, heartbeat owns a read-handle to just the bloom.
    ///
    /// The returned handle reads from the SAME RwLock that `insert()`
    /// writes to — inserts show up in subsequent heartbeat snapshots.
    /// I-110c: prime the manifest-hint map. Called once per build by
    /// the executor (after `BatchGetManifest`), before the FUSE-warm
    /// stat loop. Keys are store BASENAMES (`abc..-hello`, not
    /// `/nix/store/abc..-hello`) — that's what `fetch_extract_insert`
    /// has in hand.
    pub fn prime_manifest_hints(
        &self,
        hints: impl IntoIterator<Item = (String, rio_proto::types::ManifestHint)>,
    ) {
        let mut map = self
            .manifest_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner());
        map.extend(hints);
    }

    /// I-110c: take (remove) the hint for `store_basename`, if any.
    /// Removed on read — once the path is fetched the hint is dead
    /// weight, and concurrent builds churn the closure set so leaving
    /// entries would grow unbounded.
    pub fn take_manifest_hint(
        &self,
        store_basename: &str,
    ) -> Option<rio_proto::types::ManifestHint> {
        self.manifest_hints
            .lock()
            .unwrap_or_else(|e| e.into_inner())
            .remove(store_basename)
    }

    pub fn bloom_handle(&self) -> Arc<RwLock<BloomFilter>> {
        Arc::clone(&self.bloom)
    }

    /// Remove stale `*.tmp-*` extraction trees and `*.nar-*` spool files
    /// from the cache root. These are remnants of interrupted NAR fetches.
    fn clean_stale_tmp_dirs(cache_dir: &Path) {
        let Ok(entries) = std::fs::read_dir(cache_dir) else {
            return;
        };
        // Match foo.<tag>-<16 hex chars> EXACTLY for tag in {tmp, nar}.
        // Require the suffix to be exactly 16 hex chars (rand::<u64>
        // formatted {:016x}) — a loose substring match would delete
        // legitimate store paths like "rust-1.75.0-tmp-build".
        let has_hex16_suffix = |s: &str, tag: &str| {
            s.rsplit_once(tag).is_some_and(|(_, sfx)| {
                sfx.len() == 16 && sfx.chars().all(|c| c.is_ascii_hexdigit())
            })
        };
        for entry in entries.flatten() {
            let name = entry.file_name();
            let Some(name_str) = name.to_str() else {
                continue;
            };
            let path = entry.path();
            if has_hex16_suffix(name_str, ".tmp-") {
                // Partial extracted tree (dir OR single-file path —
                // restore_path_streaming creates whichever the NAR
                // root is). remove_dir_all handles dirs; fall back to
                // remove_file for the single-regular-file case.
                let res = std::fs::remove_dir_all(&path).or_else(|_| std::fs::remove_file(&path));
                match res {
                    Ok(()) => {
                        tracing::info!(path = %path.display(), "removed stale tmp extraction")
                    }
                    Err(e) => tracing::warn!(
                        path = %path.display(), error = %e,
                        "failed to remove stale tmp extraction"
                    ),
                }
            } else if has_hex16_suffix(name_str, ".nar-") {
                // I-180 NAR spool file (always a regular file).
                match std::fs::remove_file(&path) {
                    Ok(()) => tracing::info!(path = %path.display(), "removed stale NAR spool"),
                    Err(e) => tracing::warn!(
                        path = %path.display(), error = %e,
                        "failed to remove stale NAR spool"
                    ),
                }
            }
        }
    }

    /// Root directory where cached paths are materialized.
    pub fn cache_dir(&self) -> &Path {
        &self.cache_dir
    }

    /// Check if a store path is cached.
    ///
    /// Propagates SQLite errors so callers can distinguish "not cached"
    /// from "index query failed". Treating DB errors as not-cached would
    /// trigger re-fetches for every FUSE op during a SQLite hiccup,
    /// saturating store bandwidth and masking the root cause.
    #[cfg(test)]
    pub fn contains(&self, store_path: &str) -> Result<bool, CacheError> {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let count: i64 =
                sqlx::query_scalar("SELECT COUNT(*) FROM cached_paths WHERE store_path = ?1")
                    .bind(store_path)
                    .fetch_one(pool)
                    .await?;
            Ok(count > 0)
        })
    }

    /// Check if cached AND update last_access atomically in one DB roundtrip.
    ///
    /// Replaces the contains() + touch() pair in get_path's hot path. Each
    /// block_on involves cross-thread sync with the tokio runtime; doing
    /// both in a single call halves the overhead on every cache hit.
    fn get_and_touch(&self, store_path: &str) -> Result<bool, CacheError> {
        let now = unix_now();
        let pool = &self.pool;
        self.runtime.block_on(async {
            // UPDATE...RETURNING: affects 0 rows if path not cached.
            let result = sqlx::query(
                "UPDATE cached_paths SET last_access = ?1 WHERE store_path = ?2 RETURNING store_path",
            )
            .bind(now)
            .bind(store_path)
            .fetch_optional(pool)
            .await?;
            Ok(result.is_some())
        })
    }

    /// Get the full filesystem path for a cached store path.
    ///
    /// Returns `Ok(None)` if the path is not cached, `Err` on index failure.
    ///
    /// An entry returned by this function is protected from eviction for
    /// `EVICT_GRACE_SECS` (5s): `get_and_touch()` stamps `last_access = now`
    /// atomically in the same DB roundtrip, and `evict_if_needed()` filters
    /// `last_access < now - grace`. The caller's subsequent open/stat has the
    /// full grace window before eviction can select this entry.
    pub fn get_path(&self, store_path: &str) -> Result<Option<PathBuf>, CacheError> {
        if self.get_and_touch(store_path)? {
            Ok(Some(self.cache_dir.join(store_path)))
        } else {
            Ok(None)
        }
    }

    /// Remove a stale index row. Called when the index says "present" but
    /// the file is gone from disk (external rm, interrupted eviction).
    /// Best-effort: logs on failure but doesn't propagate — if the DELETE
    /// fails, the next fetch will `INSERT OR REPLACE` over it anyway.
    ///
    /// Does NOT touch the bloom filter (bloom doesn't support deletion;
    /// `insert()` already documents stale-positive tolerance).
    pub fn remove_stale(&self, store_path: &str) {
        let pool = &self.pool;
        if let Err(e) = self.runtime.block_on(async {
            sqlx::query("DELETE FROM cached_paths WHERE store_path = ?1")
                .bind(store_path)
                .execute(pool)
                .await
        }) {
            tracing::warn!(
                store_path,
                error = %e,
                "failed to remove stale index row (will be overwritten on re-fetch)"
            );
        }
    }

    /// Record a store path as cached after extraction.
    pub fn insert(&self, store_path: &str, size_bytes: u64) -> Result<(), CacheError> {
        let now = unix_now();
        let pool = &self.pool;
        self.runtime.block_on(async {
            sqlx::query(
                "INSERT OR REPLACE INTO cached_paths (store_path, size_bytes, last_access)
                 VALUES (?1, ?2, ?3)",
            )
            .bind(store_path)
            .bind(size_bytes as i64)
            .bind(now)
            .execute(pool)
            .await?;
            Ok::<_, sqlx::Error>(())
        })?;

        // Bloom insert AFTER the SQLite write succeeds. If we did it
        // before and the SQLite write failed, we'd have a bloom positive
        // for a path we don't actually have — a confusing stale positive
        // that doesn't correct on restart (restart rebuilds from SQLite,
        // which doesn't have the path). Ordering after means bloom and
        // SQLite stay consistent modulo evictions (which we intentionally
        // don't remove from bloom).
        //
        // Write lock is brief: one blake3 hash + k bit-sets (~1μs).
        // Heartbeat readers won't notice.
        self.bloom
            .write()
            .unwrap_or_else(|e| e.into_inner())
            .insert(store_path);

        // Update cache size metric (ground-truth from DB)
        self.update_size_gauge();
        Ok(())
    }

    /// Total size of all cached paths in bytes.
    ///
    /// Propagates SQLite errors rather than silently returning 0: a silent
    /// 0 on error makes the evict loop think the cache is under limit when
    /// it may not be, causing unbounded growth until disk fills.
    pub fn total_size(&self) -> Result<u64, CacheError> {
        let pool = &self.pool;
        self.runtime.block_on(async {
            let size: i64 =
                sqlx::query_scalar("SELECT COALESCE(SUM(size_bytes), 0) FROM cached_paths")
                    .fetch_one(pool)
                    .await?;
            Ok(size as u64)
        })
    }

    /// Update the cache size gauge (best-effort; logs on DB error).
    fn update_size_gauge(&self) {
        match self.total_size() {
            Ok(bytes) => {
                metrics::gauge!("rio_builder_fuse_cache_size_bytes").set(bytes as f64);
            }
            Err(e) => {
                tracing::warn!(error = %e, "failed to read cache total_size for metric");
            }
        }
    }

    /// Evict least-recently-used entries until total size is under the limit.
    ///
    /// Entries with `last_access >= now - EVICT_GRACE_SECS` are skipped —
    /// they're either in active use or just returned by `get_path()`. If all
    /// entries are within grace and total is still over limit, breaks and lets
    /// the next insert retry (this is transient, not drift).
    pub fn evict_if_needed(&self) -> Result<u64, CacheError> {
        let grace_cutoff = unix_now() - EVICT_GRACE_SECS;
        let mut freed = 0u64;

        loop {
            let total = self.total_size()?;
            if total <= self.max_size_bytes {
                break;
            }

            let pool = &self.pool;
            let entry: Option<CacheEntry> = self.runtime.block_on(async {
                let row: Option<(String, i64)> = sqlx::query_as(
                    "SELECT store_path, size_bytes FROM cached_paths
                     WHERE last_access < ?1
                     ORDER BY last_access ASC LIMIT 1",
                )
                .bind(grace_cutoff)
                .fetch_optional(pool)
                .await
                .map_err(|e| {
                    // Propagate (not swallow): if this fails while total > max,
                    // breaking silently would let the cache grow unbounded.
                    tracing::error!(error = %e, "FUSE cache eviction query failed; cache may grow unbounded");
                    CacheError::Sqlite(e)
                })?;
                Ok::<_, CacheError>(row.map(|(path, size)| CacheEntry {
                    store_path: path,
                    size_bytes: size as u64,
                }))
            })?;

            let Some(entry) = entry else {
                // total > max but nothing evictable. Two cases:
                //   (a) All entries are within the grace window (transient —
                //       just inserted or just touched; retry on next insert).
                //   (b) Index has rows but none match the filter AND none are
                //       hot (accounting drift; self-corrects on next insert).
                // Distinguish by counting hot entries.
                let hot: i64 = self.runtime.block_on(async {
                    sqlx::query_scalar("SELECT COUNT(*) FROM cached_paths WHERE last_access >= ?1")
                        .bind(grace_cutoff)
                        .fetch_one(pool)
                        .await
                        .unwrap_or(0)
                });
                if hot > 0 {
                    tracing::debug!(
                        total,
                        max = self.max_size_bytes,
                        hot_entries = hot,
                        grace_secs = EVICT_GRACE_SECS,
                        "cache over limit but all remaining entries within grace; will retry on next insert"
                    );
                } else {
                    tracing::warn!(
                        total,
                        max = self.max_size_bytes,
                        "cache total exceeds limit but no entries to evict; accounting drift"
                    );
                }
                break;
            };

            // Remove from disk
            let path = self.cache_dir.join(&entry.store_path);
            if path.exists()
                && let Err(e) = std::fs::remove_dir_all(&path)
            {
                tracing::warn!(
                    path = %path.display(),
                    error = %e,
                    "failed to remove evicted cache entry from disk"
                );
            }

            // Remove from index
            self.runtime.block_on(async {
                sqlx::query("DELETE FROM cached_paths WHERE store_path = ?1")
                    .bind(&entry.store_path)
                    .execute(pool)
                    .await
            })?;

            freed += entry.size_bytes;
            tracing::debug!(
                store_path = %entry.store_path,
                size = entry.size_bytes,
                "evicted cache entry"
            );
        }

        // Update cache size metric after eviction
        if freed > 0 {
            self.update_size_gauge();
        }
        Ok(freed)
    }

    /// Try to claim responsibility for fetching a path.
    ///
    /// Returns [`FetchClaim::Fetch`] if the path was not already in-flight;
    /// the caller must perform the fetch. The returned guard notifies all
    /// waiters on drop (including on panic).
    ///
    /// Returns [`FetchClaim::WaitFor`] if another thread is already fetching;
    /// the caller should block on [`InflightEntry::wait`].
    pub fn try_start_fetch(&self, store_path: &str) -> FetchClaim<'_> {
        use std::collections::hash_map::Entry;
        let mut inflight = self.inflight.lock().unwrap_or_else(|e| {
            tracing::error!("inflight lock poisoned, recovering");
            e.into_inner()
        });
        match inflight.entry(store_path.to_string()) {
            Entry::Occupied(e) => FetchClaim::WaitFor(Arc::clone(e.get())),
            Entry::Vacant(e) => {
                e.insert(Arc::new(InflightEntry {
                    done: Mutex::new(false),
                    cv: Condvar::new(),
                }));
                FetchClaim::Fetch(FetchGuard {
                    cache: self,
                    path: store_path.to_string(),
                })
            }
        }
    }
}

fn unix_now() -> i64 {
    std::time::SystemTime::now()
        .duration_since(std::time::UNIX_EPOCH)
        .unwrap_or_default()
        .as_secs() as i64
}

// r[verify builder.fuse.cache-lru]
#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::Arc;

    /// Cache sync methods use `block_on`, so tests create the cache in async
    /// context then exercise the sync methods via `spawn_blocking`.
    async fn make_cache(cache_dir: PathBuf, max_size_gb: u64) -> anyhow::Result<Arc<Cache>> {
        Ok(Arc::new(
            Cache::new(cache_dir, max_size_gb, None, false).await?,
        ))
    }

    #[tokio::test]
    async fn test_cache_new_creates_dir_and_db() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");

        let cache = make_cache(cache_dir.clone(), 1).await?;
        assert!(cache_dir.exists());
        assert!(cache_dir.join("cache_index.sqlite").exists());

        let size = tokio::task::spawn_blocking(move || cache.total_size()).await??;
        assert_eq!(size, 0);
        Ok(())
    }

    /// JIT allowlist round-trip: unarmed → NotArmed; register →
    /// KnownInput / NotInput; clear → NotArmed again. Also: register
    /// EXTENDS, not replaces.
    // r[verify builder.fuse.jit-register]
    #[tokio::test]
    async fn test_jit_classify_roundtrip() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        let hello = "aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello";
        let world = "bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-world";
        let probe = "cccccccccccccccccccccccccccccccc-out.drv.lock";

        // Unarmed: everything is NotArmed (legacy fallback).
        assert_eq!(cache.jit_classify(hello), JitClass::NotArmed);
        assert_eq!(cache.jit_classify(probe), JitClass::NotArmed);
        assert_eq!(cache.known_inputs_len(), 0);

        // Arm with one input.
        cache.register_inputs([(hello.to_owned(), 1024)]);
        assert_eq!(
            cache.jit_classify(hello),
            JitClass::KnownInput { nar_size: 1024 }
        );
        assert_eq!(cache.jit_classify(world), JitClass::NotInput);
        assert_eq!(cache.jit_classify(probe), JitClass::NotInput);
        assert_eq!(cache.known_inputs_len(), 1);

        // Second register EXTENDS (hello stays, world added).
        cache.register_inputs([(world.to_owned(), 2_000_000_000)]);
        assert_eq!(
            cache.jit_classify(hello),
            JitClass::KnownInput { nar_size: 1024 }
        );
        assert_eq!(
            cache.jit_classify(world),
            JitClass::KnownInput {
                nar_size: 2_000_000_000
            }
        );
        assert_eq!(cache.known_inputs_len(), 2);

        // Clear disarms back to NotArmed (NOT NotInput).
        cache.clear_inputs();
        assert_eq!(cache.jit_classify(hello), JitClass::NotArmed);
        assert_eq!(cache.jit_classify(probe), JitClass::NotArmed);
        assert_eq!(cache.known_inputs_len(), 0);
        Ok(())
    }

    // r[verify builder.fuse.cache-ephemeral-memory]
    /// Ephemeral mode keeps the index in `:memory:` — no on-disk SQLite
    /// file is created, but the cache_dir (for materialized NAR trees)
    /// still is. Insert/contains must round-trip across the single
    /// pinned pool connection.
    #[tokio::test]
    async fn cache_ephemeral_uses_memory() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");

        let cache = Arc::new(Cache::new(cache_dir.clone(), 1, None, true).await?);
        // cache_dir is still created (NAR trees land here); only the
        // SQLite index is in-memory.
        assert!(cache_dir.exists());
        assert!(
            !cache_dir.join("cache_index.sqlite").exists(),
            "ephemeral mode must not write cache_index.sqlite to disk"
        );

        // Index round-trips across the pinned single connection. Guards
        // against a future refactor that lets the pool open a second
        // :memory: connection (which would be an empty DB).
        let c = Arc::clone(&cache);
        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            c.insert("abc-hello-1.0", 1024)?;
            assert!(c.contains("abc-hello-1.0")?);
            assert_eq!(c.total_size()?, 1024);
            Ok(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_cache_insert_and_contains() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            assert!(!cache.contains("abc-hello-1.0")?);

            cache.insert("abc-hello-1.0", 1024)?;
            assert!(cache.contains("abc-hello-1.0")?);
            assert_eq!(cache.total_size()?, 1024);
            Ok(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_cache_get_path() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().join("cache");
        let cache = make_cache(cache_dir.clone(), 1).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            assert!(cache.get_path("abc-hello-1.0")?.is_none());

            cache.insert("abc-hello-1.0", 512)?;
            let path = cache.get_path("abc-hello-1.0")?.expect("just inserted");
            assert_eq!(path, cache_dir.join("abc-hello-1.0"));
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// get_and_touch must update last_access in a single DB roundtrip.
    #[tokio::test]
    async fn test_get_and_touch_updates_last_access() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;
        let pool = cache.pool.clone();

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            cache.insert("touch-path", 100)?;
            // Wait 1s so last_access changes detectably.
            std::thread::sleep(std::time::Duration::from_millis(1100));

            let found = cache.get_and_touch("touch-path")?;
            assert!(found, "inserted path should be found");

            // Verify last_access was updated (should be >= now - 1s).
            let last_access: i64 = cache.runtime.block_on(async {
                sqlx::query_scalar("SELECT last_access FROM cached_paths WHERE store_path = ?1")
                    .bind("touch-path")
                    .fetch_one(&pool)
                    .await
            })?;
            let now = super::unix_now();
            assert!(
                (now - last_access).abs() <= 1,
                "last_access should be ~now after get_and_touch; got diff = {}",
                now - last_access
            );

            // Nonexistent path: returns false, no error.
            assert!(!cache.get_and_touch("no-such-path")?);
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// DB errors from contains()/get_path() must propagate, not be treated
    /// as "not cached". Otherwise a SQLite hiccup would trigger re-fetches
    /// for every FUSE op, saturating store bandwidth and masking root cause.
    #[tokio::test]
    async fn test_cache_db_error_propagates() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        // Close the pool to force DB errors on all subsequent queries.
        let pool = cache.pool.clone();
        pool.close().await;

        tokio::task::spawn_blocking(move || {
            // contains() must return Err, not false.
            let result = cache.contains("some-path");
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "contains() on closed pool should return Err, got {result:?}"
            );

            // get_path() must return Err, not None.
            let result = cache.get_path("some-path");
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "get_path() on closed pool should return Err, got {result:?}"
            );

            // total_size() must return Err, not 0 (0 would make the evict
            // loop think the cache is under limit when it may not be).
            let result = cache.total_size();
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "total_size() on closed pool should return Err, got {result:?}"
            );

            // evict_if_needed() must return Err, not Ok(0) (Ok(0) would let
            // the cache grow unbounded on DB error).
            let result = cache.evict_if_needed();
            assert!(
                matches!(result, Err(CacheError::Sqlite(_))),
                "evict_if_needed() on closed pool should return Err, got {result:?}"
            );
        })
        .await?;
        Ok(())
    }

    /// Insert a row with an explicit last_access (bypasses insert()'s `now`).
    /// Tests use this to create "old" entries that are eviction-eligible.
    fn raw_insert(
        cache: &Cache,
        store_path: &str,
        size_bytes: i64,
        last_access: i64,
    ) -> anyhow::Result<()> {
        let pool = cache.pool.clone();
        cache.runtime.block_on(async {
            sqlx::query(
                "INSERT OR REPLACE INTO cached_paths (store_path, size_bytes, last_access)
                 VALUES (?1, ?2, ?3)",
            )
            .bind(store_path)
            .bind(size_bytes)
            .bind(last_access)
            .execute(&pool)
            .await?;
            anyhow::Ok(())
        })
    }

    #[tokio::test]
    async fn test_cache_eviction() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // 0 GB max to force eviction
        let cache = make_cache(dir.path().join("cache"), 0).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            // last_access = 1 (ancient) — well outside the grace window.
            raw_insert(&cache, "old-path", 100, 1)?;
            raw_insert(&cache, "new-path", 200, 2)?;

            let freed = cache.evict_if_needed()?;
            assert!(freed > 0);
            Ok(())
        })
        .await??;
        Ok(())
    }

    /// TOCTOU protection: an entry touched within the grace window must NOT be
    /// evicted, even when the cache is over limit. An entry outside the grace
    /// window IS evicted. This closes the race where `get_path()` returned a
    /// path that `evict_if_needed()` (running concurrently on another FUSE
    /// thread) then deleted from disk before the caller could open it.
    #[tokio::test]
    async fn test_evict_skips_recently_touched() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // 0 GB max to force eviction of anything eligible.
        let cache = make_cache(dir.path().join("cache"), 0).await?;

        tokio::task::spawn_blocking(move || -> anyhow::Result<()> {
            // cold-path: ancient last_access, eviction-eligible.
            raw_insert(&cache, "cold-path", 100, 1)?;
            // hot-path: insert via the public API (stamps last_access = now),
            // then touch it (stamps again) — both well within grace.
            cache.insert("hot-path", 200)?;
            let touched = cache.get_and_touch("hot-path")?;
            assert!(touched, "hot-path should be found");

            // Pre-check: both in the index.
            assert!(cache.contains("cold-path")?);
            assert!(cache.contains("hot-path")?);
            assert_eq!(cache.total_size()?, 300);

            // Evict. Only cold-path is outside grace; hot-path is protected.
            let freed = cache.evict_if_needed()?;
            assert_eq!(freed, 100, "should free exactly cold-path (100 bytes)");

            // cold-path gone, hot-path stays.
            assert!(!cache.contains("cold-path")?, "cold-path should be evicted");
            assert!(
                cache.contains("hot-path")?,
                "hot-path should survive eviction (within grace window)"
            );

            // Total is now 200. Still over the 0-byte limit, but the next
            // evict_if_needed() should be a no-op — hot-path is STILL in grace.
            // This proves the loop doesn't spin infinitely when all remaining
            // entries are protected.
            let freed2 = cache.evict_if_needed()?;
            assert_eq!(
                freed2, 0,
                "second eviction should free nothing (hot-path still in grace)"
            );
            assert!(cache.contains("hot-path")?);
            Ok(())
        })
        .await??;
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_claim_and_notify() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        // First claim should succeed.
        let FetchClaim::Fetch(guard) = cache.try_start_fetch("abc-path") else {
            panic!("first claim should return Fetch");
        };
        // Second claim should get WaitFor.
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("abc-path") else {
            panic!("second claim should return WaitFor");
        };

        // Drop guard → should notify. Waiter should see completion immediately.
        drop(guard);
        assert!(entry.wait(Duration::from_millis(100)));

        // After guard drop, path is no longer inflight — can claim again.
        let FetchClaim::Fetch(_) = cache.try_start_fetch("abc-path") else {
            panic!("should be able to re-claim after guard drop");
        };
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_concurrent_wait() -> anyhow::Result<()> {
        use std::sync::atomic::{AtomicU32, Ordering};
        use std::time::Instant;

        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;
        let fetch_count = Arc::new(AtomicU32::new(0));

        // Two threads race to fetch the same path. Exactly one should get
        // Fetch; the other should WaitFor and be woken promptly by the
        // condvar notify, not time out.
        let (c1, f1) = (Arc::clone(&cache), Arc::clone(&fetch_count));
        let (c2, f2) = (cache, Arc::clone(&fetch_count));

        let t1 = std::thread::spawn(move || do_claim(&c1, &f1));
        let t2 = std::thread::spawn(move || do_claim(&c2, &f2));

        let (d1, d2) = (t1.join().expect("thread"), t2.join().expect("thread"));

        // Exactly one fetch happened.
        assert_eq!(fetch_count.load(Ordering::SeqCst), 1);
        // Both threads finished in roughly the fetcher's sleep time (200ms),
        // not the old backoff total (1.4s). Generous bound for CI.
        assert!(d1 < Duration::from_millis(800), "t1 took {d1:?}");
        assert!(d2 < Duration::from_millis(800), "t2 took {d2:?}");

        fn do_claim(cache: &Cache, fetch_count: &AtomicU32) -> Duration {
            let start = Instant::now();
            match cache.try_start_fetch("race-path") {
                FetchClaim::Fetch(_guard) => {
                    fetch_count.fetch_add(1, Ordering::SeqCst);
                    // Simulate a fetch taking 200ms.
                    std::thread::sleep(Duration::from_millis(200));
                    // _guard drops here, notifying the waiter.
                }
                FetchClaim::WaitFor(entry) => {
                    assert!(
                        entry.wait(Duration::from_secs(5)),
                        "waiter should be notified"
                    );
                }
            }
            start.elapsed()
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_inflight_wait_timeout() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        // Claim the fetch but never drop the guard.
        let FetchClaim::Fetch(guard) = cache.try_start_fetch("stuck-path") else {
            panic!("first claim should return Fetch");
        };
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("stuck-path") else {
            panic!("second claim should return WaitFor");
        };

        // Wait should time out since the guard is never dropped.
        let start = std::time::Instant::now();
        assert!(!entry.wait(Duration::from_millis(100)));
        assert!(start.elapsed() >= Duration::from_millis(100));

        drop(guard);
        Ok(())
    }

    /// `is_done()` reflects guard-drop state without waiting on the condvar.
    /// Before guard drop: `false`. After guard drop: `true`.
    #[tokio::test]
    async fn test_inflight_is_done_tracks_guard_drop() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache = make_cache(dir.path().join("cache"), 1).await?;

        let FetchClaim::Fetch(guard) = cache.try_start_fetch("done-path") else {
            panic!("first claim should return Fetch");
        };
        let FetchClaim::WaitFor(entry) = cache.try_start_fetch("done-path") else {
            panic!("second claim should return WaitFor");
        };

        // Guard still held: not done.
        assert!(!entry.is_done(), "is_done should be false while guard held");

        // wait() times out → false. is_done() is STILL false (fetcher working).
        assert!(!entry.wait(Duration::from_millis(50)));
        assert!(!entry.is_done(), "is_done still false after timed-out wait");

        // Drop the guard: now done.
        drop(guard);
        assert!(entry.is_done(), "is_done should be true after guard drop");

        // And wait() now returns immediately true.
        assert!(entry.wait(Duration::from_secs(1)));
        Ok(())
    }

    /// FetchSemaphore: permits are consumed by acquire, restored on permit drop.
    /// Second acquire blocks until first permit drops.
    #[test]
    fn test_fetch_semaphore_permit_raii() {
        let sem = Arc::new(FetchSemaphore::new(1));

        // First acquire succeeds immediately.
        let p1 = sem.acquire();

        // Second acquire on another thread blocks until p1 drops.
        let sem2 = Arc::clone(&sem);
        let started = std::time::Instant::now();
        let t = std::thread::spawn(move || {
            let _p2 = sem2.acquire();
            started.elapsed()
        });

        // Hold p1 for 100ms, then drop.
        std::thread::sleep(Duration::from_millis(100));
        drop(p1);

        // t should have waited ~100ms for the permit.
        let waited = t.join().expect("thread");
        assert!(
            waited >= Duration::from_millis(90),
            "second acquire should have blocked, waited only {waited:?}"
        );

        // Both permits released: a fresh acquire succeeds immediately.
        let start = std::time::Instant::now();
        let _p3 = sem.acquire();
        assert!(
            start.elapsed() < Duration::from_millis(50),
            "acquire with available permit should be immediate"
        );
    }

    #[tokio::test]
    async fn test_tmp_cleanup_on_init() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let cache_dir = dir.path().to_path_buf();

        // Create a stale tmp dir that mimics an interrupted extraction.
        let stale_tmp = cache_dir.join("abc-hello.tmp-deadbeef12345678");
        std::fs::create_dir_all(&stale_tmp)?;
        std::fs::write(stale_tmp.join("partial_file"), b"incomplete")?;
        assert!(stale_tmp.exists());

        // I-180: stale NAR spool file (regular file, not dir) from a
        // process kill mid-spool.
        let stale_spool = cache_dir.join("abc-hello.nar-0123456789abcdef");
        std::fs::write(&stale_spool, b"partial nar bytes")?;
        assert!(stale_spool.exists());

        // Also create a legitimate (non-tmp) entry to verify it's NOT removed.
        let real_entry = cache_dir.join("def-world");
        std::fs::create_dir_all(&real_entry)?;
        // And a store path with `.nar-` in the NAME (not the suffix) —
        // must NOT be swept. Only an exact 16-hex trailing suffix matches.
        let nar_named = cache_dir.join("ghi-some.nar-fixture");
        std::fs::create_dir_all(&nar_named)?;

        // Cache::new should clean the stale tmp dir but leave the real entry.
        let _cache = Cache::new(cache_dir, 10, None, false).await?;
        assert!(
            !stale_tmp.exists(),
            "stale tmp dir should be removed on init"
        );
        assert!(
            !stale_spool.exists(),
            "stale .nar- spool file should be removed on init"
        );
        assert!(
            real_entry.exists(),
            "real cache entries must not be removed"
        );
        assert!(
            nar_named.exists(),
            "store paths with .nar- in the name (not 16-hex suffix) must not be swept"
        );
        Ok(())
    }
}
