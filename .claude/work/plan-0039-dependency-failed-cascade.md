# Plan 0039: DependencyFailed cascade + terminal cleanup + heartbeat merge

## Context

Five scheduler correctness bugs found in the third review pass, all variations on "state drifts from reality and the build hangs forever":

1. **Cached double-count:** `summary.completed` already included cached derivations (they transition to Completed). Adding `build.cached_count` again → 2× reported.
2. **DB errors silently discarded:** `complete_build`/`transition_build_to_failed` errors via `let _ =`. In-memory advances, PG doesn't, no log.
3. **Heartbeat clobber:** `handle_heartbeat` replaced `running_builds` wholesale. Race: scheduler assigns X, worker sends stale heartbeat (pre-assignment), scheduler clobbers its own tracking.
4. **Poisoned-dep hang:** with `keepGoing=true`, when a derivation is poisoned, its parents stay `Queued` forever (`all_deps_completed` always false). Build never terminates.
5. **Unbounded memory:** `builds`/`build_events`/`build_sequences` maps and `dag.nodes` never removed terminal entries. Long-running scheduler → OOM.

The poisoned-dep fix introduces `DerivationStatus::DependencyFailed` (maps to Nix's `BuildStatus::DependencyFailed=10`). The cleanup fix introduces a 60s-delayed `CleanupTerminalBuild` actor command.

## Commits

- `b56fead` — fix(rio-scheduler): remove double-counting of cached derivations in completed_count
- `22c9197` — fix(rio-scheduler): log build lifecycle transition DB errors instead of discarding
- `46920fb` — fix(rio-scheduler): merge heartbeat running_builds instead of replacing (TOCTOU)
- `af0eb62` — fix(rio-scheduler): cascade DependencyFailed to parents of poisoned derivations
- `d1051dc` — fix(rio-scheduler): cleanup terminal build state to prevent unbounded memory growth
- `a0d8125` — fix(migrations): exclude dependency_failed from derivations_status_idx partial index
- `ebf0b73` — test(rio-scheduler): add DependencyFailed to db.rs status roundtrip test
- `e07473d` — fix(rio-scheduler): detect pre-existing failed deps in compute_initial_states

(Last three landed later — commits 66, 67, 70 — as follow-ups to the DependencyFailed work. Discontinuous range.)

## Files

```json files
[
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "new DerivationStatus::DependencyFailed; valid transitions Queued/Ready/Created→DependencyFailed; counted as terminal+failed"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "cascade_dependency_failure walks parents transitively; handle_derivation_failure syncs counts from DAG ground truth; heartbeat reconcile (keep scheduler-tracked, warn+accept worker-reported unknowns); CleanupTerminalBuild command scheduled 60s post-terminal via WeakSender"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "remove_build_interest_and_reap: delete orphaned+terminal nodes + their edges; any_dep_terminally_failed() checks Poisoned/DependencyFailed children"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "DependencyFailed in status roundtrip test"},
  {"path": "migrations/003_add_dependency_failed.sql", "action": "NEW", "note": "add dependency_failed to status CHECK constraint"},
  {"path": "migrations/004_exclude_dependency_failed_from_index.sql", "action": "NEW", "note": "partial index should exclude terminal states; 003 forgot this"}
]
```

## Design

**Heartbeat merge:** scheduler is authoritative for what it assigned. Reconcile: (a) keep scheduler-tracked builds still `Assigned/Running` in DAG — heartbeat may predate the assignment; (b) `warn!` + accept heartbeat-reported builds scheduler didn't assign — split-brain indicator; (c) drop scheduler-tracked builds only if DAG shows them no longer in-flight — completion already processed.

**DependencyFailed cascade:** when `poison_derivation` fires, walk `dag.get_parents()` transitively. Any ancestor in `Queued/Ready/Created` → `DependencyFailed`. Counted in `BuildSummary.failed`. `handle_derivation_failure` now re-syncs counts from DAG ground truth (picks up the cascaded ones).

**Pre-existing failed deps (`e07473d`):** the cascade only runs on *transition to* Poisoned. If build B2 merges a node depending on an *already-poisoned* node from B1 (within `TERMINAL_CLEANUP_DELAY`), `compute_initial_states` just put it in `Queued`. New `any_dep_terminally_failed()` check → `DependencyFailed` at merge time.

**Terminal cleanup:** `TERMINAL_CLEANUP_DELAY = 60s` after a build reaches `Succeeded/Failed/Cancelled`. Delay allows late `WatchBuild` subscribers. `CleanupTerminalBuild` removes from all three maps + `remove_build_interest_and_reap` deletes DAG nodes with empty `interested_builds` and terminal status. `WeakSender` so the actor can shut down when all handles drop (delayed-cleanup tasks don't hold it open).

**Migration 004:** migration 001's partial index excluded `completed`/`poisoned`. Migration 003 added `dependency_failed` to the CHECK but not the index exclusion — terminal rows indexed = bloat.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.state.dependency-failed]`, `r[sched.heartbeat.reconcile]`, `r[sched.cleanup.terminal]`.

## Outcome

Merged as `b56fead..d1051dc` + `a0d8125`, `ebf0b73`, `e07473d` (8 commits, discontinuous). `test_keepgoing_poisoned_dependency_cascades_failure`: 3-node chain A→B→C, poison C → A and B become DependencyFailed, build terminates Failed. `test_terminal_build_cleanup_after_delay` verifies maps drain. `test_merge_with_prepoisoned_dep_marks_dependency_failed` covers the late-add.
