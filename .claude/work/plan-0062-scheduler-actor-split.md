# Plan 0062: Scheduler actor split + SessionContext + newtypes

## Design

The big architectural refactor of phase-2b's cleanup arc: `rio-scheduler/src/actor.rs` was 4760 lines. Every scheduler change for the rest of the project touches this file; every PR conflicts. Split into `actor/{merge,completion,dispatch,worker,build,mod,tests}.rs` by concern — the submodule structure mirrors the `ActorCommand` enum variants.

Simultaneously landed three type-level changes that touch the same files:

**`SessionContext` struct** (`rio-gateway/src/{handler,session}.rs`): `handle_opcode` took 10 parameters. The 7 per-session fields (clients, options, temp_roots, drv_cache, build tracking) became a struct; `handle_opcode` dropped to 4 params (opcode/reader/writer/ctx). One of five `#[allow(clippy::too_many_arguments)]` in the file removed. The three build handlers keep theirs — they destructure context fields and genuinely need everything.

**`AssignmentStatus` enum** (`rio-scheduler/src/db.rs`): was stringly-typed `&str`. Now an enum with `is_terminal()` method. Only `Pending`/`Completed` are set from Rust; `acknowledged`/`failed`/`cancelled` are reserved for phase-2c.

**`DrvHash` + `WorkerId` newtypes** (`rio-scheduler/src/state.rs`): `String` wrappers with `Borrow<str>` + `Deref` + `Display` + `From` impls. This commit *introduces* them; P0064 completes adoption across `dag`/`queue`/`state`/`actor` (deferred here to keep the diff reviewable — the introduction alone is +60 sites).

The test file `actor/tests.rs` (2375 lines) stayed monolithic here; P0064 splits it.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor.rs", "action": "DELETE", "note": "4760 lines, replaced by actor/ submodules"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "NEW", "note": "DagActor struct, ActorCommand enum, event loop, shared state"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "NEW", "note": "handle_merge_dag, persist_merge_to_db, rollback"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "NEW", "note": "handle_completion_{success,transient,permanent}, newly_ready propagation"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "NEW", "note": "pick_worker, dispatch_next, maxBuilds accounting"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "NEW", "note": "connect/disconnect/heartbeat, poison-TTL tick"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "NEW", "note": "SubmitBuild, WatchBuild, QueryBuildStatus handlers"},
  {"path": "rio-scheduler/src/actor/tests.rs", "action": "NEW", "note": "2375-line test module (split in P0064)"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "DrvHash + WorkerId newtypes (Borrow<str>+Deref+Display+From)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "AssignmentStatus enum, is_terminal()"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "handle_opcode: 10 params → 4 (opcode/reader/writer/ctx)"},
  {"path": "rio-gateway/src/session.rs", "action": "MODIFY", "note": "SessionContext struct (clients, options, temp_roots, drv_cache, build tracking)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. The actor submodule structure retroactively carries `r[sched.actor.*]` domain markers (one per submodule concern).

## Entry

- Depends on **P0061** (perf): `actor.rs` was modified in P0061 (batched DB); splitting it here means P0061's changes must land first.

## Exit

Merged as `288479d..18977de` (2 commits; second is the phase-doc checklist update marking 6 post-2a items complete). `.#ci` green at merge.
