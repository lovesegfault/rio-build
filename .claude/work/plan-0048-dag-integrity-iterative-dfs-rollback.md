# Plan 0048: DAG integrity — iterative DFS + MergeResult rollback on post-merge failure

## Context

Two scheduler DAG correctness bugs with high blast radius:

**Stack overflow in cycle detection.** `has_cycle_from` recursed depth-proportional. `MAX_DAG_NODES=100k` (from P0042). A linear 100k chain → 100k stack frames × ~150B (including String clones) ≈ 15MB. Default tokio task stack: 2MB. Actor panics.

**Memory leak on post-merge DB failure.** `handle_merge_dag` inserted `build_events`/`build_sequences`/`builds` maps *before* `dag.merge()`. A cycle or post-merge DB error left those maps permanently populated — no terminal cleanup ever scheduled. Worse: an `Option<db_id>` returning `None` (edge references un-upserted derivation — a flow bug) was silently skipped, leaving in-memory DAG with the edge but PostgreSQL without it. After restart, recovered DAG missing dependency edges → builds fail with "input not found" that *looks* like a store bug.

## Commits

- `092fe05` — fix(rio-scheduler): convert recursive cycle detection to iterative DFS
- `ae45a9f` — fix(rio-scheduler): guard in-memory state against handle_merge_dag failure with MergeResult rollback

## Files

```json files
[
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "has_cycle_from: explicit-stack iterative DFS, same 3-color semantics, frames hold (node, children snapshot, next-child index); merge() returns MergeResult{newly_inserted, new_edges, interest_added}; rollback_merge pub(crate)"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "handle_merge_dag reordered: insert_build(DB)→dag.merge()→map inserts→persist_merge_to_db with rollback on error; cleanup_failed_merge helper rolls back DAG+maps+best-effort delete_build; edge db_id None → ActorError::Internal (was: silent skip)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "delete_build() for cleanup"}
]
```

## Design

**Iterative DFS:** each frame: `(node, Vec<String> children_snapshot, next_child_idx)`. Pop frame, if `next_child_idx < children.len()` → visit child: if gray → cycle, if white → mark gray, push current frame with `idx+1`, push child frame. If `idx == len` → mark black, don't push back. Snapshotting children avoids borrow-checker conflict with `color.insert()`.

**MergeResult surfacing:** `dag.merge()` already tracked `newly_inserted`, `new_edges`, `interest_added` internally for cycle-rollback. Returning them lets `handle_merge_dag` invoke `rollback_merge()` if *its own* post-merge persistence fails.

**Reordered `handle_merge_dag`:**
1. `insert_build` (DB) — fails → nothing in memory yet
2. `dag.merge()` — fails → nothing in maps, best-effort `delete_build`
3. Map inserts (`build_events`, `build_sequences`, `builds`)
4. `persist_merge_to_db` + `transition_build(Active)` — fails → `cleanup_failed_merge` rolls back DAG + maps + best-effort `delete_build`
5. Post-Active processing — DB failures now log-and-continue (build is Active and valid; DB catches up)

**`db_id` None escalation:** previously `if let Some(id) = db_id { persist edge } // else: skip silently`. Now: `db_id.ok_or(ActorError::Internal("edge references un-upserted derivation"))`. The silent-skip produced state that looked like a store bug on restart.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.dag.cycle-iterative]`, `r[sched.merge.rollback-on-fail]`.

## Outcome

Merged as `092fe05..ae45a9f` (2 commits). Stress tests: `test_cycle_detection_deep_linear_chain_no_overflow` (10k acyclic chain), `test_cycle_detection_deep_chain_with_back_edge` (5k chain + back-edge → detected + rolled back). `test_cyclic_merge_does_not_leak_in_memory_state`: cyclic submission → `QueryBuildStatus` returns `BuildNotFound`, DAG nodes absent.
