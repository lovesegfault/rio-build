# Plan 0049: Scheduler boundary validation + error propagation hardening

## Context

The final scheduler hardening pass before the VM test. Fourteen commits, mostly: (a) validate untrusted gRPC input at the boundary, (b) make state-transition rejections loud, (c) make internal types compile-time-safe.

The two most load-bearing fixes:

**ReadyQueue O(n²):** `push_back`/`push_front` used `VecDeque::contains` for dedup — O(n). The dispatch-defer loop (P0041) re-queues deferred items via `push_front`, making each dispatch pass O(n²). 10k deferred ineligibles → 10⁸ ops inside the single-threaded actor.

**`running_builds` leak on assign failure:** `assign_to_worker` added `drv_hash` to `worker.running_builds`, then on `try_send` failure reset to `Ready` — but never removed from `running_builds`. With `max_builds>=2`: infinite spin (worker still `has_capacity()`, channel still full, `push_front` → immediate re-dispatch). With `max_builds=1`: phantom capacity leak, worker appears permanently full.

## Commits

Fourteen commits, discontinuous:
- `1c71eac` — fix: clean up running_builds on assign try_send failure
- `9a30bb6` — fix: send data_loss on broadcast Lagged, prevent silent client hang
- `80ec82c` — refactor: make DerivationState.drv_path read-only
- `7607fa1` — feat: validate drv_hash/drv_path non-empty at gRPC boundary
- `0ca8397` — fix: replay terminal state in WatchBuild, spawn_monitored for cleanup timer
- `b1c4933` — test: add tests for CancelBuild, WatchBuild, max_retries exhaustion
- `5cb5d71` — fix: log state transition rejections in actor event loop
- `f1c47fa` — feat: validate system field non-empty at gRPC boundary
- `54bd4ae` — refactor: replace priority_class String with PriorityClass enum
- `8402b58` — feat: validate tenant_id, edges count, heartbeat payload at gRPC boundary
- `61a39e5` — fix: skip DB write when in-memory build transition rejected
- `f4ddfa9` — perf: add companion HashSet to ReadyQueue for O(1) dedup
- `2c935e0` — fix: distinguish TrySendError Full vs Closed in ActorHandle::try_send
- `343be3b` — refactor: wrap ActorHandle backpressure flag in BackpressureReader newtype

## Files

```json files
[
  {"path": "rio-scheduler/src/queue.rs", "action": "MODIFY", "note": "parallel HashSet<String> membership index; push_back/pop_front O(1); debug_assert queue.len()==members.len()"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "assign_to_worker returns bool, cleans running_builds on fail; transition rejections error!/warn! + transition_rejected_total metric; WatchBuild replays terminal state; transition()→rejection→early-return no DB write; TrySendError Full→Backpressure, Closed→ChannelSend"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "submit_build validates: drv_hash/drv_path/system non-empty, nodes<MAX_DAG_NODES, edges<MAX_DAG_EDGES, tenant_id parses UUID; heartbeat validates: features<64, running_builds<1000; PriorityClass::from_str at boundary; broadcast Lagged→data_loss status"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "DerivationState.drv_path private + drv_path() accessor (reverse-index corruption guard); PriorityClass enum {Ci,Interactive,Scheduled} with FromStr+is_interactive()"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "insert_build takes PriorityClass, binds .as_str()"},
  {"path": "rio-common/src/limits.rs", "action": "MODIFY", "note": "MAX_DAG_EDGES=500k (nixpkgs full ≈200k/60k nodes), MAX_HEARTBEAT_FEATURES=64, MAX_HEARTBEAT_RUNNING_BUILDS=1000"}
]
```

## Design

**ReadyQueue companion set:** `HashSet::insert` returns `bool` → free dedup. `push_back`: if insert returns false, no-op. `pop_front`: also removes from set. `remove`: O(1) set-check short-circuit, O(n) only when present. Invariant `queue.len() == members.len()` debug-asserted.

**`drv_path` private:** the DAG's `path_to_hash` reverse index keys on `drv_path`. Since `node_mut()` returns `&mut DerivationState`, a caller could write `drv_path` and corrupt the index. Private field + `drv_path() -> &str` accessor = compile-time guarantee.

**`PriorityClass` enum:** closed vocabulary (ci/interactive/scheduled) was String. Invalid value like `"urgent"` passed gRPC (only empty-checked) → PostgreSQL CHECK constraint violation leaked in `Status::internal`. Enum + `FromStr` at boundary → `invalid_argument`.

**WatchBuild replay (`0ca8397`):** late `WatchBuild` within `TERMINAL_CLEANUP_DELAY` but after `BuildCompleted` already sent (possibly to zero receivers) → hang. Fix: after subscribing, check if already terminal → reconstruct + re-send the terminal event via the broadcast Sender. New subscriber gets it immediately.

**Transition-rejection logging:** `handle_success_completion` Running→Completed rejection = build result LOST, downstream stuck. `assign_to_worker` Ready→Assigned rejection = TOCTOU. `reset_to_ready` rejection = derivation orphaned in Assigned. All should-never-fire in single-threaded actor, but loudly visible if they do.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.grpc.boundary-validate]`, `r[sched.queue.dedup-o1]`.

## Outcome

Merged as 14 discontinuous commits. 15+ new tests covering boundary rejection, `PriorityClass` roundtrip, assign-failure cleanup, WatchBuild replay, CancelBuild idempotency, `max_retries` exhaustion. `BackpressureReader` newtype: `Arc<AtomicBool>` was structurally writable by both actor and handle; now handle gets read-only view.
