# Plan 0051: Worker FUSE/cache error propagation + synth-DB indexes

## Context

Eleven worker hardening fixes. The theme: **stop silently degrading on SQLite/I/O errors.** Multiple sites caught errors and returned "not cached" / "0 bytes" / "skip" — which made the worker slow and wrong instead of loud and fixable.

Representative: `Cache::contains()` caught SQLite errors → returned `0` (="not cached"). During a SQLite hiccup (disk full, WAL overflow), every FUSE lookup re-fetched the NAR. Multi-GB NARs re-downloaded on every file access → network amplification, saturated store bandwidth, slow builds — root cause buried in `warn!` logs.

Plus: synth-DB missing `IndexReferrer`/`IndexReference` on `Refs` — nix-daemon's sandbox closure walk was O(n²). And `Realisations.outputPath` was `TEXT` instead of `INTEGER FK` — dormant (CA deferred) but would silently return nothing on Nix's FK-join queries.

## Commits

Eleven commits, discontinuous:
- `f95e5c4` — fix: propagate FUSE cache DB errors instead of treating as not-cached
- `a3e17d4` — fix: fail on cache insert failure after NAR fetch
- `2fa9686` — feat: add overlay_teardown_failures metric, log leaked mount path
- `9313220` — fix: correct error variant for nix.conf, warn on empty proto msgs
- `d857ac7` — fix: ephemeral readdir inodes to prevent InodeMap growth
- `6279025` — perf: consolidate FUSE cache hot-path block_on roundtrips
- `053423d` — fix: remove NotFound paths from closure set before continuing BFS
- `982a957` — fix: centralize overlay_teardown_failures metric in Drop
- `5064298` — fix: propagate FUSE cache eviction DB errors instead of unbounded growth
- `61b157b` — fix: add missing Refs indexes and fix Realisations.outputPath type in synth DB
- `c7c8680` — fix: cache miss metric timing, symlink_metadata errors, dir_size silent 0

## Files

```json files
[
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "contains()→Result<bool,CacheError>; get_path()→Result<Option<PathBuf>,CacheError>; get_and_touch() combines contains+touch in one UPDATE..RETURNING; total_size()→Result; update_size_gauge() helper; dir_size_inner→io::Result"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "ensure_cached maps CacheError→EIO; cache insert fail→Err(EIO) not Ok (stop infinite re-fetch); ephemeral readdir inodes; cache_misses_total moved to function ENTRY (count on detect, not success); lookup distinguishes NotFound (fall through) from other errno (reply EIO)"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "compute_input_closure: closure.remove(&path) before continue on NotFound; ExecutorError::NixConf variant; GetPathResponse{msg:None} logged"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "overlay_teardown_failures_total in Drop (fires on all paths); teardown_overlay only sets mounted=false on SUCCESS (so Drop can retry+increment on fail)"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "IndexReferrer+IndexReference on Refs; Realisations.outputPath INTEGER FK→ValidPaths(id)"}
]
```

## Design

**Error→EIO mapping:** `CacheError::Sqlite` → `Errno::EIO`. The build fails with a clear I/O error pointing at the cache. Operator looks at logs, sees SQLite, fixes disk. Versus: silent degradation, everything slow, nobody knows why.

**Insert-fail → fail (`a3e17d4`):** NAR fetched, extracted, renamed — successful. `cache.insert()` fails. Path on disk but invisible to `contains()`. Old: return `Ok` anyway. Every subsequent access → re-fetch, compete for rename, insert fails again. Infinite loop. New: `Err(EIO)`.

**Metric-in-Drop (`982a957`):** `overlay_teardown_failures_total` was only incremented on *explicit* teardown. Multiple `?` early-returns between `setup_overlay` and explicit teardown rely on `OverlayMount::Drop`. Drop didn't increment. Alerting blind to the most common failure mode. Moved to Drop. Also: `teardown_overlay` only sets `mounted=false` on success — previously cleared unconditionally, so Drop saw `mounted=false` and skipped retry + metric.

**Closure NotFound remove (`053423d`):** `compute_input_closure` inserts paths into the set for dedup BEFORE the `QueryPathInfo` call. On `NotFound` (output of not-yet-built dep), `continue` — but path stays in set. Downstream `fetch_input_metadata` has no NotFound skip → fails.

**Refs indexes:** when `sandbox=true`, nix-daemon walks `Refs` to compute closure to bind-mount. Without `IndexReferrer`, 1000+ path closure walk is O(n²). The design doc's "<50ms for 1000+ paths" was only for INSERT.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[worker.fuse.cache-err-propagate]`, `r[worker.overlay.teardown-metric-drop]`, `r[worker.synthdb.refs-index]`.

## Outcome

Merged as 11 discontinuous commits. `test_cache_db_error_propagates` closes SQLite pool, asserts `contains`/`get_path`/`total_size`/`evict_if_needed` all `Err`. `test_generate_db_creates_valid_schema` asserts all 4 required indexes exist.
