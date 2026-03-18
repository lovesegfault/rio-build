# Plan 0102: Large-file module splits — grpc/metadata/state/daemon/tests

## Design

Seven commits split five files that had grown past 900 LOC into submodule directories. Each split was a pure move — no logic change, same `pub` surface via `mod.rs` re-exports — but the split points were chosen for cohesion, not line count.

The splits:
- `rio-store/src/grpc.rs` (1266 LOC) → `grpc/{mod,put_path,get_path,chunk}.rs`. The `mod.rs` holds the `StoreServiceImpl` struct and `metadata_status()` error mapper; each RPC handler gets its own file with its tests.
- `rio-store/src/metadata.rs` (1149 LOC) → `metadata/{mod,inline,chunked,queries}.rs`. `mod.rs` holds `MetadataError` (from P0098) and the `Metadata` struct; `inline.rs` handles sub-threshold blobs, `chunked.rs` handles manifest-backed storage, `queries.rs` holds the raw SQL.
- `rio-scheduler/src/state/mod.rs` (923 LOC) → `state/{mod,derivation,build,worker}.rs`. One file per entity: `DerivationState`, `BuildState`, `WorkerState`. The `mod.rs` re-exports and holds the shared `State` container.
- `rio-worker/src/executor/daemon.rs` → `daemon/{mod,spawn,stderr_loop}.rs`. `spawn.rs` holds `spawn_daemon_in_namespace` (unshare + bind-mounts); `stderr_loop.rs` holds the `STDERR_*` message pump.
- `rio-scheduler/src/actor/tests/coverage.rs` (1482 LOC) → 7 topic modules: `{build,completion,dispatch,fault,keep_going,merge,worker}.rs`. The `CacheCheckBreaker` unit tests moved to `actor/breaker.rs` adjacent to the code they test.

`17e2a69` also extracted `actor/{breaker,handle}.rs` from `actor/mod.rs` (897 → 693 LOC): `CacheCheckBreaker` + its 6 unit tests, `ActorHandle` + the `cfg(test)` `DebugWorkerInfo`/`DebugDerivationInfo`. Visibility adjustment: `CacheCheckBreaker` went `pub(super)` → `pub(crate)` (can't re-export at wider visibility than the item's own); `ActorHandle.tx` + `.backpressure` went `pub(super)` so tests can construct directly and set backpressure for fault injection.

These splits were prerequisite for the phase3a feats: P0106 adds `admin/mod.rs`, P0109 adds `cgroup.rs`, P0114 adds `lease.rs` — all of which would have fought single-file layouts. **File-path continuity note:** P0097-P0101 touch pre-split paths (`grpc.rs`, `metadata.rs`, `state/mod.rs`, `daemon.rs`); their `## Files` sections reference final post-split paths.

## Files

```json files
[
  {"path": "rio-store/src/grpc/mod.rs", "action": "NEW", "note": "split from grpc.rs: StoreServiceImpl + metadata_status mapper"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "NEW", "note": "split: PutPath handler + tests"},
  {"path": "rio-store/src/grpc/get_path.rs", "action": "NEW", "note": "split: GetPath handler + tests"},
  {"path": "rio-store/src/grpc/chunk.rs", "action": "NEW", "note": "split: GetChunk/PutChunk handlers"},
  {"path": "rio-store/src/metadata/mod.rs", "action": "NEW", "note": "split from metadata.rs: MetadataError + Metadata struct"},
  {"path": "rio-store/src/metadata/inline.rs", "action": "NEW", "note": "split: sub-threshold inline_blob path"},
  {"path": "rio-store/src/metadata/chunked.rs", "action": "NEW", "note": "split: manifest-backed storage"},
  {"path": "rio-store/src/metadata/queries.rs", "action": "NEW", "note": "split: raw SQL"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "NEW", "note": "split: DerivationState"},
  {"path": "rio-scheduler/src/state/build.rs", "action": "NEW", "note": "split: BuildState"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "NEW", "note": "split: WorkerState"},
  {"path": "rio-worker/src/executor/daemon/mod.rs", "action": "NEW", "note": "split from daemon.rs"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "NEW", "note": "split: spawn_daemon_in_namespace"},
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "NEW", "note": "split: STDERR_* message pump"},
  {"path": "rio-scheduler/src/actor/breaker.rs", "action": "NEW", "note": "extracted: CacheCheckBreaker + 6 unit tests"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "NEW", "note": "extracted: ActorHandle + cfg(test) Debug* structs"},
  {"path": "rio-scheduler/src/actor/tests/build.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/dispatch.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/fault.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/keep_going.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/merge.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-scheduler/src/actor/tests/worker.rs", "action": "NEW", "note": "split from coverage.rs"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "NEW", "note": "split from tests/grpc_integration.rs"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). The retroactive sweep placed module-header `r[impl]` annotations on the post-split files: `r[store.put.*]` on `put_path.rs`, `r[sched.state.*]` on `state/derivation.rs`, etc. This plan created the files those annotations landed on.

## Entry

- Depends on P0100: `put_path.rs` was created from `grpc.rs` after P0100 made the trailer-only change — the split carries the simplified code.
- Depends on P0098: `metadata/mod.rs` carries `MetadataError` from P0098.

## Exit

Merged as `1c6ae70..34b556c` (7 commits, non-contiguous). `.#ci` green at merge. Zero behavior change — all splits are pure moves with `mod.rs` re-exports preserving public surface. `git diff --stat` shows −4800/+4800 line churn; semantic diff is zero.
