# Plan 0064: Newtype adoption follow-through + actor test split

## Design

P0062 introduced `DrvHash` and `WorkerId` newtypes in `state.rs` but deferred full adoption to keep that diff reviewable. This plan completes the sweep: ~60 sites across `dag`, `queue`, `state`, and all `actor/` submodules.

The change found a **real bug** in heartbeat reconciliation: the worker reports `drv_path` strings in its heartbeat, but the scheduler's `running_builds` set is keyed by `drv_hash`. Before newtypes, both were `String`, and the comparison `worker_reported.contains(&running_hash)` was comparing a path to a hash — it silently never matched, which meant heartbeat reconciliation was a no-op (stale assignments weren't being reaped). With typed keys, the compiler would have caught this. Fixed by resolving worker-reported paths through the DAG's reverse path→hash index before comparing.

Additional newtype ergonomics: `Borrow<String>`, `PartialEq<str>`, `PartialEq<&str>`, `PartialEq<String>` impls on both newtypes so `HashMap<DrvHash, _>` lookup with `&str` works without cloning, and test assertions like `assert_eq!(hash, "abc...")` compile.

Also splits `actor/tests.rs` (2375 lines, from P0062) into `actor/tests/{coverage,helpers,integration,wiring,mod}.rs` by existing `#[cfg]` section boundaries. And two small worker cleanups: removed `#[allow(dead_code)]` from `overlay.rs` (deleted the unused `lower` field + `lower_dir()`/`work_dir()` accessors — `work` *field* stays, Drop needs it), extracted `spawn_build_task` from a `main.rs` inline block.

## Files

```json files
[
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "HashMap<DrvHash,_> keys, MergeResult fields, return types (get_parents, find_newly_ready, build_derivations, find_leaves, find_roots, topological_order, compute_initial_states, hash_for_path)"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "VecDeque<DrvHash>, HashSet<DrvHash>"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "DerivationState.drv_hash, assigned_worker, failed_workers; BuildInfo.derivation_hashes; WorkerState.worker_id, running_builds; +Borrow<String>/PartialEq<str>"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "workers: HashMap<WorkerId,_>, ActorCommand worker_id fields; ProcessCompletion.drv_hash → drv_key (holds path OR hash)"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "newtype adoption"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "newtype adoption"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "newtype adoption"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "newtype adoption + BUG FIX: heartbeat path→hash resolution (was comparing path to hash, never matched)"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "newtype adoption"},
  {"path": "rio-scheduler/src/actor/tests.rs", "action": "DELETE", "note": "2375 lines → 5 submodules"},
  {"path": "rio-scheduler/src/actor/tests/mod.rs", "action": "NEW", "note": "test module root"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "NEW", "note": "per-branch coverage tests"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "NEW", "note": "shared test fixtures"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "NEW", "note": "multi-command actor flows"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "NEW", "note": "channel/oneshot wiring tests"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "delete unused lower field + accessors; remove #[allow(dead_code)]"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "extract spawn_build_task from inline block"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "spawn_build_task export"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. The heartbeat bug fix would retroactively land under `r[sched.heartbeat.reconcile]`.

## Entry

- Depends on **P0062** (actor split + newtypes introduced): completes what P0062 started.

## Exit

Merged as `d82d9d3..2a5d94c` (4 commits). `.#ci` green at merge. Heartbeat bug fix is semantics-changing but was previously a no-op, so no existing test broke; the fix makes the feature actually work.
