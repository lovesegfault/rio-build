# Plan 0033: Distributed-path unbreak — worker registration + completion routing

## Context

**Without these fixes, the distributed build path was entirely non-functional.** Workers never became `is_registered` (no assignments), and even if they did, completions were silently dropped due to a key mismatch.

P0031 landed the worker and P0029 landed the scheduler. The in-process integration test (ba98d17) used mocks and passed. Nobody actually dispatched a build. The first TDD test that did — `test_completion_resolves_drv_path_to_hash` — hung: the build stuck in `Active` forever.

Two root causes, both invisible under mocks:

1. **Worker registration UUID mismatch.** The scheduler's `BuildExecution` stream handler generated a random UUID for the connecting worker. The worker's `Heartbeat` RPC sent its *own* worker_id. The actor saw two different IDs — stream-worker never got a heartbeat, heartbeat-worker never had a stream. `is_registered` (which required both) stayed `false` forever.

2. **Completion key mismatch.** Workers send `CompletionReport{drv_path}`. The DAG is keyed by `drv_hash`. The actor's `handle_completion` looked up `drv_path` as a key, found nothing, silently dropped the completion.

Plus three secondary fixes found in the same TDD sweep: state-machine invariant violations (direct `Running→Ready` assignment, skipping `Failed`), daemon process leaks on `?` between spawn and kill, and silent channel/DB failures.

## Commits

- `ae8f31e` — fix(rio-scheduler): fix worker registration and completion routing
- `4889163` — fix(rio-scheduler): enforce state machine invariants via encapsulation
- `fed2175` — fix(rio-worker): ensure daemon kill on all paths, wrap setup I/O in timeout
- `fed5fb0` — fix(rio-scheduler): eliminate silent failures on channels and DB
- `d2e84cf` — fix(rio-store): bound NAR accumulation to prevent OOM

## Files

```json files
[
  {"path": "rio-proto/proto/worker.proto", "action": "MODIFY", "note": "add WorkerRegister{worker_id} to WorkerMessage oneof — worker announces its own ID as first stream message"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "WorkerRegister message type"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "BuildExecution handler reads WorkerRegister before sending WorkerConnected (was: random UUID); WorkerConnected uses send_unchecked (was: try_send drop under backpressure)"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "drv_path_to_hash() helper (O(n) scan); handle_completion resolves path→hash before lookup; is_registered derived from stream_tx.is_some()&&system.is_some() (not stored bool); 13 `let _ = transition()` sites now warn!-on-fail"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "status field private; reset_to_ready() validated helper (Running→Failed→Ready, not direct Running→Ready); reset_from_poison(); set_status_for_test()"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "merge() returns Result<Vec<String>, DagError>; has_cycle_from 3-color DFS; rollback_merge removes newly-inserted on cycle"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "run_daemon_build() helper; execute_build ALWAYS kills daemon after helper returns (was: any `?` leaked process); DAEMON_SETUP_TIMEOUT=30s on handshake+setOptions; read_build_stderr_loop aborts with InfrastructureFailure if log channel closes"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "send WorkerRegister as first BuildExecution stream message"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "PutPath rejects chunks exceeding declared nar_size+4KB (was: unbounded accumulation, OOM)"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "grpc_get_path enforces MAX_NAR_SIZE on store-returned chunks"}
]
```

## Design

**Registration handshake (`ae8f31e`):** new proto message `WorkerRegister{worker_id}`. Worker sends it as the *first* message on the `BuildExecution` stream. Scheduler's stream handler reads it, extracts `worker_id`, *then* sends `ActorCommand::WorkerConnected{worker_id, stream_tx}`. Heartbeat arrives with the same `worker_id` → actor finds the existing `WorkerState`, fills in `system`/`max_builds`. Now `is_registered` becomes a derived method — `stream_tx.is_some() && system.is_some()` — eliminating the stored-bool sync bug.

**Completion routing:** `drv_path_to_hash` does a linear scan of DAG nodes matching `state.drv_path == path`. O(n) per completion — made O(1) with a reverse index in P0043. `handle_completion` calls it, then looks up the resolved hash in the DAG. `ProcessCompletion` also switched from `try_send` (drop under backpressure) to `send_unchecked` (blocking) — a dropped completion leaves a derivation stuck in `Running` forever.

**State machine encapsulation (`4889163`):** `DerivationState.status` becomes private. Writes go through validated helpers: `reset_to_ready()` handles the `Assigned|Running → Ready` path, but `Running` goes through `Failed` first (the state machine requires `Running → Failed → Ready`; direct `Running → Ready` was the pre-existing invariant violation in `handle_worker_disconnected`). This correctly counts the disconnect as a retry attempt. Cycle detection: 3-color (white/gray/black) DFS; on cycle, `rollback_merge` removes newly-inserted nodes/edges/build-interest so the DAG is pre-merge clean.

**Daemon kill-always (`fed2175`):** every `?` between `Command::spawn()` and `daemon.kill()` leaked the subprocess. Extracted `run_daemon_build()` containing all post-spawn I/O; `execute_build` wraps it and *always* kills after return. `DAEMON_SETUP_TIMEOUT=30s` on handshake+setOptions (was: only handshake had timeout; stuck setOptions would hang until `build_timeout`, potentially 2 hours).

**Silent failures (`fed5fb0`):** `Completion{result: None}` (empty proto oneof) was silently dropped — now synthesized as `InfrastructureFailure`. `WorkerDisconnected` used `try_send` — now `send_unchecked`. `MergeDag` reply-channel drop (client disconnected during merge) now cancels the orphaned build. Non-terminal DB writes (`Failed`/`Poisoned`/`Assigned` status) now `error!`-log instead of `let _ =`.

**NAR OOM bound (`d2e84cf`):** `PutPath` accumulated chunks with no check against `declared_nar_size`. Client declares `nar_size=1`, streams unbounded → server OOM. Now: `saturating_add` check, reject at `nar_size + 4KB` tolerance.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). The state-machine invariants enforced here were later spec'd as `r[sched.state.derivation.transition-guard]` and `r[sched.worker.register.handshake]`. No markers in these commits.

## Outcome

Merged as `ae8f31e..d2e84cf` (5 commits). `test_completion_resolves_drv_path_to_hash` now passes — build transitions to `Succeeded`. Closes review findings C1, C2, C7, C8, C9, C10, C11, C12, S1, S3, I15. The distributed dispatch path works for the first time — under mocks. The VM test (P0053) will find more.
