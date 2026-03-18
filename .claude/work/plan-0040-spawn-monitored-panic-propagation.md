# Plan 0040: spawn_monitored — panic propagation + actor-liveness pattern

## Context

If the DAG actor task panics, the scheduler keeps accepting gRPC requests. Those requests send `ActorCommand`s to a closed channel, then block forever on oneshot reply channels that will never receive. No error, no log, no shutdown. Clients hang.

Same pattern everywhere: worker heartbeat loop (panic → scheduler times worker out → re-dispatches builds → duplicates, zero diagnostic), gateway channel pump (panic → SSH hangs, gauge leaks), scheduler broadcast bridges (panic → clean stream close → client thinks build completed).

This plan introduces `rio_common::task::spawn_monitored`: wraps futures in `catch_unwind`, `error!`-logs with task name + panic message, then resumes unwinding (so `JoinHandle` still reports it). Plus `ActorHandle::is_alive()` — checks `tx.is_closed()`; every gRPC handler entry-checks it and returns `UNAVAILABLE` if the actor is dead.

The pattern was introduced once and then rolled out across all spawn sites over five commits as the review pass found them.

## Commits

- `47d5ce4` — fix(rio-scheduler): wrap spawned tasks to log panics and propagate shutdown
- `25344be` — fix(rio-worker): wrap heartbeat loop and panic-catcher in spawn_monitored
- `9a18f75` — fix(rio-gateway): wrap channel tasks in spawn_monitored, decrement gauge via Drop
- `c1eaaa0` — fix(rio-scheduler): wrap gRPC bridge tasks in spawn_monitored
- `68820d3` — fix(rio-worker): fail hard when worker_id cannot be determined

(Introduction + 4 rollout commits. Discontinuous — commits 32, 47–50.)

## Files

```json files
[
  {"path": "rio-common/src/task.rs", "action": "NEW", "note": "spawn_monitored(name, fut): catch_unwind + error! log + resume_unwind"},
  {"path": "rio-common/src/lib.rs", "action": "MODIFY", "note": "pub mod task"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "is_alive() checks tx.is_closed(); DAG actor + worker-stream-reader + cleanup-timer use spawn_monitored; Tick loop breaks on channel close"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "all handlers check is_alive() → UNAVAILABLE; broadcast-bridge tasks (3 sites) spawn_monitored; test .ok()→.expect() on server startup"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "actor spawn uses spawn_monitored"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "heartbeat loop spawn_monitored + store JoinHandle + is_finished() check in main event loop (exit if dead); panic-catcher spawn_monitored; panic_tx.send failure logged; gethostname fail → process exit (was: 'unknown' → collisions); build-task watchdog sends InfrastructureFailure completion on panic"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "client_pump/proto_task/response_task spawn_monitored; channels_active gauge decrement via ChannelSession::Drop (fires on ALL paths)"}
]
```

## Design

**`spawn_monitored`:** `tokio::spawn(async move { match AssertUnwindSafe(fut).catch_unwind().await { Ok(r) => r, Err(panic) => { error!(task=name, panic=?downcast_panic_msg(panic)); resume_unwind(panic) } } })`. The `resume_unwind` means the `JoinHandle` still sees `Err(JoinError::Panic)` — callers that `.await` the handle can react. Callers that don't (most) at least get the log line.

**Actor liveness:** `is_alive()` = `!tx.is_closed()`. The actor receiver drops when the actor task exits (panic or clean return). Every gRPC handler: `if !handle.is_alive() { return Status::unavailable("actor dead") }`. Clean error instead of hang.

**Worker build-task watchdog (`47d5ce4`):** build tasks wrapped in `spawn_monitored`. A separate watchdog task `.await`s the `JoinHandle`; on `Err(panic)`, it sends `CompletionReport{status=InfrastructureFailure}` so the scheduler doesn't leave the derivation stuck in `Running`.

**Gateway gauge-via-Drop (`9a18f75`):** `rio_gateway_channels_active` was decremented at three explicit call sites (data(), channel_close(), ConnectionHandler::Drop). Abnormal paths bypassed them. Moved to `ChannelSession::Drop` — fires on every drop path. The ConnectionHandler's `HashMap::clear()` now drops each session, firing the per-session guard.

**Worker-id fail-hard (`68820d3`):** `gethostname()` fail + no `--worker-id` → previously `"unknown"`. Two "unknown" workers → scheduler keys by worker_id → builds stolen. Now: process exit.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[common.task.spawn-monitored]`, `r[sched.actor.liveness-check]`.

## Outcome

Merged as `47d5ce4`, `25344be`, `9a18f75`, `c1eaaa0`, `68820d3` (5 commits, discontinuous). `test_spawn_monitored_{normal,panic}`, `test_actor_is_alive_detection`. This pattern became project convention — every background task should use it.
