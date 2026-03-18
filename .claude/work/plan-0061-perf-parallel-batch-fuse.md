# Plan 0061: Perf — parallel fetches, batched DB writes, FUSE hot-path wins

## Design

Three orthogonal perf wins, all flagged during phase-2a review as "works but slow", landed in a single commit because they share the same pattern: serial I/O inside latency-sensitive paths.

**Worker input fetching** (`rio-worker/src/executor.rs`): three serial loops converted to `buffer_unordered`.
- `inputDrv` resolution: serial → `buffer_unordered(16)`. For 200 inputs at ~5ms RTT, saves ~1s per build.
- `fetch_input_metadata`: same pattern, but `buffered` (order-preserving) since the result slice is indexed.
- `compute_input_closure`: was serial BFS; now layer-parallel BFS. Typical closure depth is 5–15, so 100–500 paths × 5ms drops from 0.5–2.5s to ~50–75ms. Also adopts `with_timeout_status` (from P0059) for clean `NotFound` branching without unwrapping `anyhow`.

**Scheduler DB batching** (`rio-scheduler/src/{db,actor}.rs`): `persist_merge_to_db` did 2N+E serial PostgreSQL roundtrips (upsert each derivation, insert each build-derivation link, insert each edge). For a 1000-node DAG that's ~3.5s stalling the single-threaded actor — all heartbeats, completions, and dispatches block. Replaced with three `QueryBuilder::push_values` batched inserts in one transaction: ~50ms total. Also added `sqlx` `uuid` feature so `Uuid` binds directly without `::text`/`::uuid` casts. `cleanup_failed_merge` left unchanged — it handles in-memory rollback, transaction scope handles DB rollback. This was the follow-up to the `TODO(phase2b)` that P0058's `df593a3` planted.

**FUSE hot-path** (`rio-worker/src/fuse/{mod,read}.rs`): three wins from the phase-doc checklist. Cache file handles in `read()` (was opening the backing file on every chunk read — syscall per read callback). Stream `readdir` entries instead of buffering all (some store paths have thousands of children). Skip `ensure_cached` for children of already-materialized paths (if the parent's NAR is on disk, the children are too).

## Files

```json files
[
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "inputDrv/fetch_input_metadata/compute_input_closure: serial→buffer_unordered; with_timeout_status"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "skip ensure_cached for materialized-parent children"},
  {"path": "rio-worker/src/fuse/read.rs", "action": "MODIFY", "note": "cache file handles, stream readdir"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "batch_{upsert_derivations,insert_build_derivations,insert_edges} via QueryBuilder::push_values"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "persist_merge_to_db calls batched functions; single transaction"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "sqlx uuid feature"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive markers would land under `r[wk.inputs.fetch]` (worker closure computation) and `r[sched.merge.persist]` (DB batching).

## Entry

- Depends on **P0056** (phase-2a terminal): `executor.rs`, `fuse/mod.rs`, `actor.rs`, `db.rs` are all 2a artifacts.
- Soft dep on **P0059** (proto helpers): adopts `with_timeout_status`.

## Exit

Merged as `595a7df` (1 commit). `.#ci` green at merge. No new tests — all three optimizations preserve semantics; existing tests cover behavior, timing validated via `vm-phase2a` passing within budget.
