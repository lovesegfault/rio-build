# Plan 0090: Critical-path scheduling — estimator + priority + BinaryHeap queue

## Design

Replaces FIFO dispatch with critical-path-aware priority scheduling. Three commits building the chain: duration estimation feeds priority computation, priority feeds a `BinaryHeap` ready queue. Without this, FIFO means a 5s derivation submitted after a 2h one waits 2h even if the 5s one blocks 50 downstream. With critical-path, the 5s one has priority `5 + sum(downstream)` = huge.

### Estimator: build_history read + fallback chain

Duration estimation for critical-path priority and size-class routing (P0093). A wrong estimate means suboptimal scheduling (short waits behind long), not incorrectness — so "always return something" over "fail if uncertain".

Fallback chain: (1) exact `(pname, system)` → EMA from `build_history`; (2) `pname` on ANY system → cross-system mean (ARM and x86 builds of the same package take similar time; algorithm dominates target); (3) default 30s (cold-start; rough "typical small build"). `peak_memory` does NOT cross-system fallback: memory is more arch-dependent than duration (word sizes, optimization passes). Rather say "unknown" than OOM-kill on a small worker. TODO(phase3a): closure-size-as-proxy fallback. Needs per-path `nar_size` which the scheduler doesn't track. 30s default is acceptable meanwhile.

Snapshot, not live: refreshed every ~6 ticks (60s at 10s interval) via `db.read_build_history()` full-table scan. Full replace, not diff — 10k rows = 800KB, simpler and fast enough. PG fail → keep old estimator (stale better than none), log, next refresh catches up. Actor owns `Estimator` (single-threaded, no `Arc`/lock). `tick_count` for periodic tasks less frequent than every tick. `wrapping_add` — drifts one tick per 5.8 billion years. clippy: `%` → `is_multiple_of`.

### Critical-path priority: bottom-up + incremental

`priority = est_duration + max(children's priority)`. Leaves have `priority = est_duration`; roots have sum-along-longest-path. Dispatch highest-priority-first: work on derivations whose completion unblocks the most remaining work.

Three entry points:
- `compute_initial`: full bottom-up sweep for newly-merged nodes. Called once per `SubmitBuild` in `merge.rs` AFTER cache-hit transitions (so completed deps don't block). Topo-sort via Kahn's (in-degree = children-in-new-set). Propagates to existing nodes if the new subgraph raises their priority.
- `update_ancestors`: incremental walk UP from a completed node. Called in `completion.rs`. Terminal children excluded (done work doesn't block). Dirty-flag stop: priority unchanged → don't walk further. O(affected ancestors), typically small.
- `full_sweep`: all non-terminal nodes, every ~60s on Tick. Belt-and-suspenders over incremental (catches float drift, missed edges).

`DerivationState` gains `priority`/`est_duration`/`expected_output_paths`. `expected_output_paths` was proto-only before (read during merge then discarded); now persisted on the state for P0091's closure approximation. `dag::get_children` added as mirror of `get_parents`. `rio_scheduler_critical_path_accuracy` histogram on completion: actual/estimated. Helps tune the estimator.

### Priority BinaryHeap ReadyQueue

Replaces FIFO `VecDeque`. Derivations pop in critical-path priority order (highest first).

`BinaryHeap` + lazy invalidation: `BinaryHeap` can't `remove(x)`. Keep a `removed` HashSet; `pop()` skips marked entries. Same amortized complexity, simpler than an indexed heap. `compact()` rebuilds without garbage when >50% of heap is stale — called on Tick, bounded memory.

`Entry = (OrderedFloat<f64>, Reverse<u64>, DrvHash)`: `OrderedFloat` because `f64` doesn't impl `Ord` (NaN). `Reverse<u64>` gives FIFO tiebreak (`BinaryHeap` is max-heap; `Reverse` makes lower sequence = older sort higher). `DrvHash` now derives `Ord` (`string_newtype` macro updated).

Interactive boost: old `push_front` replaced by `priority += 1e9`. Large enough to dominate any realistic critical-path sum (100k nodes × 3600s = 3.6e8). Not `f64::MAX` — that would tie all interactive builds, losing relative ordering within interactive.

Re-push updates priority: if hash already in `members`, push again. Two heap entries with same hash; `pop` returns higher-priority one, removes from `members`; lower-priority stale one skipped later (not in `members`). No explicit `removed`-set entry needed for this case — `members` gate suffices.

`push_ready()` helper on actor: computes priority from node + interactive boost. All push sites migrated (merge, completion, worker-disconnect, dispatch-defer). Old `push_front`/`push_back` split is now a number. `test_interactive_priority_push_front` → `test_interactive_priority_boost`: same observable behavior, different mechanism.

## Files

```json files
[
  {"path": "rio-scheduler/src/estimator.rs", "action": "NEW", "note": "fallback chain (exact -> cross-system -> 30s default), peak_memory no cross-system, snapshot refresh every 6 ticks"},
  {"path": "rio-scheduler/src/critical_path.rs", "action": "NEW", "note": "compute_initial (Kahn's bottom-up), update_ancestors (dirty-flag stop), full_sweep"},
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "VecDeque -> BinaryHeap, (OrderedFloat, Reverse<u64>, DrvHash) entry, lazy invalidation + compact"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "Estimator owned by actor, tick_count, push_ready() helper"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "compute_initial call after cache-hit transitions"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "update_ancestors call, critical_path_accuracy histogram"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "push_ready migration"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "push_ready migration (worker-disconnect path)"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "DerivationState gains priority/est_duration/expected_output_paths"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "get_children mirror of get_parents"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "read_build_history full-table scan"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "pub mod estimator, critical_path"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "10 estimator + 7 critical-path + 10 queue tests"},
  {"path": "rio-common/src/newtype.rs", "action": "MODIFY", "note": "string_newtype macro derives Ord"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `build_history` resource columns from migration 006 (estimator reads `ema_build_duration_seconds`).
- Depends on **P0062** (2b actor-split, `288479d`): `actor/{merge,completion,dispatch}.rs` submodules exist.
- Depends on **P0029** (2a FIFO scheduler DAG, `f58f948`): `dag/mod.rs` + `get_parents`; this plan adds `get_children` mirror.

## Exit

Merged as `2c7610a..a84f017` (3 commits). Tests: 759 → 780 (+21: 10 estimator, 7 critical-path, 10 queue replacing old FIFO tests, net +4 queue).

Phase 2c's "multi-build DAG merging" task (`interest_added` tracking for priority-boost) was already done in Phase 2a's `dag/mod.rs merge()` — this plan consumed it, didn't implement it.
