# Plan 0043: Perf + encapsulation — reverse index, private state, FUSE forget(), NAR atomic rename

## Context

Fourth review pass — performance and correctness improvements across scheduler, store, and worker FUSE:

**Scheduler perf:** `drv_path_to_hash` and `find_db_id_by_path` were O(n) linear scans per completion. `dag.merge()` rebuilt a path→hash map on every merge. New persistent `path_to_hash` index on `DerivationDag` — O(1) lookup.

**Scheduler encapsulation:** `BuildInfo.state` was `pub`; `handle_cancel_build` wrote directly, bypassing validation. Made private with `transition()` method (matching `DerivationState.status` from P0033).

**Store perf:** `PutPath` hash verification wrapped the 4GiB `nar_data` in `HashingReader` over `Cursor`, then `read_to_end` into a *second* Vec. Peak ~8GiB for a 4GiB NAR. `NarDigest::from_bytes` does SHA-256 over the slice — zero extra allocation.

**FUSE forget():** no `forget()` impl meant every lookup permanently added to inode maps. Long-running workers leaked memory proportional to unique paths accessed. FUSE protocol: kernel sends `forget(ino, n)` when it drops n references; at zero, filesystem may free the inode.

**FUSE atomic rename:** `fetch_and_extract` extracted directly to `local_path`. Disk-full mid-extraction → partial directory tree. Subsequent `lookup()` checks `symlink_metadata()` before cache index → serves partial extraction as valid. Extract to `.tmp-{rand}`, then atomic `rename`.

## Commits

- `664fead` — fix(rio-scheduler): prevent Timestamp overflow in EMA duration computation
- `cd0a76b` — fix(rio-scheduler): rollback_merge no longer clobbers pre-existing build interest
- `2a34b6a` — fix(rio-store): route all internal errors through internal_error() helper
- `68571ef` — perf(rio-store): eliminate double allocation in PutPath hash verification
- `0d8b7f2` — perf(rio-scheduler): add drv_path reverse index to DAG
- `4b07f55` — refactor(rio-scheduler): make BuildInfo.state private with validated transitions
- `5d6e344` — feat(rio-worker): implement FUSE forget() with nlookup refcounting
- `e2b436b` — fix(rio-worker): extract NAR to temp dir then atomically rename into cache

## Files

```json files
[
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "path_to_hash: HashMap<String,String>; maintained in merge/rollback/reap; hash_for_path() O(1); rollback only removes interest added THIS merge (track which nodes gained interest)"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "linear scans → hash_for_path(); saturating_sub for worker timestamps (overflow guard) + 30-day sanity bound on EMA duration"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "BuildInfo.state private; state() getter; transition() validated mutator; new_pending() constructor"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "4 remaining direct Status::internal sites → internal_error()"},
  {"path": "rio-store/src/validate.rs", "action": "MODIFY", "note": "NarDigest::from_bytes used directly (was: HashingReader+Cursor+read_to_end)"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "InodeMap.nlookup HashMap + increment_lookup/forget; get_or_create_inode_for_lookup (refcounting variant for lookup path); Filesystem::forget impl; extract to .tmp-rand then rename"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "Cache::new cleans stale .tmp-* on init"}
]
```

## Design

**Reverse index invariant:** `path_to_hash` mirrors `nodes` — inserted in `merge()`, removed in `rollback_merge()` for newly-inserted, removed in `remove_build_interest_and_reap()` for reaped. `test_path_to_hash_consistency` verifies across all three.

**Rollback fix (`cd0a76b`):** B1 merges with node A. B2 merges with {A, C}, cycle, rollback. Old: rollback removed B1 from A's interested_builds (iterated all pre-existing, removed unconditionally). New: track which pre-existing nodes *gained* interest this merge (insert returned true), only remove from those.

**FUSE nlookup protocol:** each successful `lookup()` reply increments kernel's refcount by 1. `forget(ino, n)` means kernel dropped n references. When our `nlookup[ino]` hits zero: remove from both `path_to_inode` and `inode_to_path`. `readdir` uses non-incrementing variant — kernel doesn't forget readdir inodes. ROOT inode never freed.

**Atomic rename:** `local_path.with_extension("tmp-{rand}")` for extraction, `fs::rename` into place. Rename is atomic on same filesystem. On failure: best-effort `remove_dir_all` on tmp. `Cache::new()` cleanup: glob `.tmp-*`, remove — remnants of crash mid-extraction.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.dag.reverse-index]`, `r[worker.fuse.forget]`, `r[worker.fuse.atomic-extract]`.

## Outcome

Merged as `664fead..e2b436b` (8 commits). `test_inode_map_forget_{removes_when_zero, keeps_when_nonzero, never_removes_root}`. `test_tmp_cleanup_on_init`. `test_path_to_hash_consistency`. `test_cycle_rollback_preserves_prior_interest`. `test_completion_with_extreme_timestamps` (actor survives i64::MIN/MAX timestamps).
