# Plan 0029: FIFO scheduler — DAG actor + state machines + backpressure

## Context

The scheduler is the brain. Every build request lands here as a DAG of derivations; the scheduler tracks state transitions, dispatches ready work to workers, handles completions, manages retries. Phase 2a keeps it simple: FIFO ready queue (no critical-path scoring, no locality — deferred to 2c), single instance (no leader election — deferred to 3a), PostgreSQL for persistence.

The concurrency model is the load-bearing decision: **single-owner actor with bounded mpsc(10_000) channel.** No shared mutable state, no locks. Every gRPC handler converts to an `ActorCommand`, sends, awaits a oneshot reply. The actor processes commands serially. This makes state-machine invariants trivial to reason about — there's one writer. It also makes the actor event loop a single point of stall: any blocking operation inside it freezes the world. P0046 (gRPC timeouts) and P0033 (worker registration uses `send_unchecked`, not backpressure-gated `send`) both stem from this.

**Shared commit with P0028:** `f58f948` landed store AND scheduler together. This plan covers the `rio-scheduler/` half.

## Commits

- `f58f948` — feat(rio-build): implement store service and FIFO scheduler (scheduler half)

## Files

```json files
[
  {"path": "rio-scheduler/src/actor.rs", "action": "NEW", "note": "DagActor: single-owner event loop, ActorCommand enum, ActorHandle with backpressure send"},
  {"path": "rio-scheduler/src/dag.rs", "action": "NEW", "note": "DerivationDag: nodes/children/parents HashMaps, merge() with cycle detection (3-color DFS), compute_initial_states"},
  {"path": "rio-scheduler/src/state.rs", "action": "NEW", "note": "DerivationState machine (Created→Queued→Ready→Assigned→Running→Completed/Failed/Poisoned), BuildInfo machine (Pending→Active→Succeeded/Failed/Cancelled), transition guards"},
  {"path": "rio-scheduler/src/queue.rs", "action": "NEW", "note": "ReadyQueue: VecDeque with contains() dedup (O(n) — fixed P0049)"},
  {"path": "rio-scheduler/src/db.rs", "action": "NEW", "note": "sqlx unchecked queries: insert_build, upsert_derivation, record_assignment, update_status"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "NEW", "note": "SchedulerServiceServer + WorkerServiceServer impls: proto → ActorCommand bridges"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "wire: PG pool, spawn actor, serve both gRPC services"}
]
```

## Design

**State machines:** `DerivationStatus` has 8 variants (9 after P0039 adds `DependencyFailed`). Valid transitions are an explicit allowlist in `validate_transition()`. `Created→Queued→Ready→Assigned→Running` is the happy path; `Running→Failed→Ready` is the retry loop (retry_count++); `Failed→Poisoned` at `POISON_THRESHOLD=3` distinct failing workers. `BuildState` is simpler: `Pending→Active→{Succeeded|Failed|Cancelled}`, terminal states reject all transitions. Both machines are idempotent on self-transitions to the same state (no-op, not error).

**DAG merge:** `SubmitBuild` carries nodes + edges. The scheduler merges them into the existing DAG — nodes already present (shared derivations across builds) gain the new build in their `interested_builds` set. After merge, 3-color DFS cycle detection; on cycle, `rollback_merge` removes newly-inserted nodes and edges. `compute_initial_states` then walks the merged nodes: if all children are `Completed`, the node is `Ready`; else `Queued`.

**Dispatch:** `dispatch_ready` runs on every actor Tick. For each ready derivation, find a registered worker whose `system` matches and `running_builds.len() < max_builds`. `try_send` the `WorkAssignment` down the worker's stream. On send, transition `Ready→Assigned`. FIFO with one exception: `priority_class="interactive"` uses `push_front` (IFD priority — P0034 implements).

**Backpressure:** hysteresis at 80% channel capacity (activate) / 60% (deactivate). `ActorHandle::send()` checks the flag and returns `RESOURCE_EXHAUSTED` when active. `send_unchecked()` bypasses — used for lifecycle commands (worker connect/disconnect, completion) that must never be dropped.

**Persistence:** every state transition writes PostgreSQL. Unchecked `sqlx::query()` strings (not compile-time checked — no build-time DB dependency). DB writes are best-effort in the middle tier (log on failure, don't block in-memory state) but build-submit is transactional (insert_build before DAG merge).

**keepGoing semantics:** Nix's `--keep-going` maps to `BuildOptions.keep_going`. When true, a poisoned derivation doesn't fail the build — siblings continue, build fails when all derivations reach terminal state. When false, first permanent failure fails the build immediately.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Scheduler spec text in `docs/src/components/scheduler.md` was later retro-tagged with `r[sched.actor.single-owner]`, `r[sched.state.derivation]`, `r[sched.state.build]`, `r[sched.dispatch.fifo]`, `r[sched.backpressure.hysteresis]` markers.

## Outcome

Merged as `f58f948` (shared with P0028). 382 tests total (29 new). State machine transition tests cover all valid/invalid pairs. The actor's `drv_path_to_hash` lookup is O(n) (linear scan — fixed P0043). Worker registration has a UUID-mismatch bug that makes the distributed path non-functional — found and fixed in P0033.
