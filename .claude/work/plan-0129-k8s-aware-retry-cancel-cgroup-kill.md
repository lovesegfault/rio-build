# Plan 0129: K8s-aware retry + cancel via cgroup.kill

## Design

Phase 3a's scheduler had transient-failure retry but three gaps for Kubernetes reality: (1) no way to cancel a running build (worker just ran to completion even after `CancelBuild`), (2) no exclusion of workers that already failed a derivation (retried straight back to the same broken node), (3) no backstop for a build that hangs forever (stream alive, no progress, no timeout). This plan closed all three plus added the `Cancelled` terminal state distinct from `Failed`.

The cancel mechanism uses Linux cgroup v2's `cgroup.kill` file — writing `1` sends SIGKILL to every process in the cgroup atomically. Since phase 2b, each build already ran in its own `BuildCgroup` (for resource accounting); this plan added a `CancelRegistry` on the worker: `Arc<RwLock<HashMap<String, (PathBuf, Arc<AtomicBool>)>>>` mapping `drv_path → (cgroup_path, cancelled_flag)`. The executor inserts on `BuildCgroup::new`, removes via `scopeguard` at the end of `execute_build`. When `Msg::Cancel` arrives, `try_cancel_build` looks up the drv, writes `cgroup.kill`, and sets the flag. The daemon subprocess gets SIGKILL → `run_daemon_build` sees stdout EOF → returns `Err`. The executor's error arm checks the flag: if set → `BuildResultStatus::Cancelled`, else `InfrastructureFailure`.

On the scheduler side, `handle_cancel_build` now iterates derivations where this build is the SOLE interested build AND status==Running → sends `SchedulerMessage::Cancel(CancelSignal{drv_path, reason})` over the assigned worker's stream. Derivations with multiple interested builds stay running — another build still wants the result.

The `failed_workers` exclusion was a one-line filter in `best_worker`: `!drv.failed_workers.contains(&w.worker_id)`. Feeding it required `reassign_derivations` (called on worker disconnect) to insert the lost worker into `failed_workers` before resetting to Ready. This also feeds poison detection — three distinct failed workers → `Poisoned`.

Delayed re-queue replaced immediate `Ready` transition after transient failure: new `backoff_until: Option<Instant>` on `DerivationState`, set to `now + backoff` before transition, checked in `dispatch_ready`'s pop loop (same defer-and-repush pattern as size-class mismatch). `cfg(test)` shadow `RETRY_BACKOFF_TEST = 50ms`.

Backstop timeout: new `running_since: Option<Instant>` set when `transition(→Running)`. `handle_tick` iterates Running nodes; if `running_since.elapsed() > max(est_duration × 3, DEFAULT_DAEMON_TIMEOUT + 600)` → send `CancelSignal` to assigned worker (if stream alive), `reset_to_ready()`, `retry_count++`. Metric: `rio_scheduler_backstop_timeouts_total`.

`DrainWorker(force=true)` became the preemption hook: iterates `worker.running_builds`, sends `CancelSignal` for each, THEN resets to Ready. Controller will call this when it sees pod `DisruptionTarget` condition (phase 4 wiring) or on WorkerPool delete.

The `Cancelled` proto enum (`BUILD_RESULT_STATUS_CANCELLED = 11`) and scheduler `DerivationStatus::Cancelled` (terminal, parallel to `Poisoned`) were added. Valid transitions: `Running → Cancelled`, `Assigned → Cancelled`.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "BUILD_RESULT_STATUS_CANCELLED = 11"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "BuildCgroup::kill() writes cgroup.kill"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "CancelRegistry type alias + try_cancel_build"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "registry insert/scopeguard remove + cancelled flag check in Err arm"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Msg::Cancel handling"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "export CancelRegistry"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "Cancelled status + backoff_until + running_since fields"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "handle_cancel_build sends CancelSignal for sole-interest Running"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "CancelSignal variant"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "Cancelled terminal handling + backoff_until set"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "backoff_until defer check in dispatch_ready loop"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "reassign_derivations inserts failed_worker + backstop in handle_tick + DrainWorker force CancelSignal"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "cancel_build wrapper"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "actor dispatch"},
  {"path": "rio-scheduler/src/actor/tests/completion.rs", "action": "MODIFY", "note": "Cancelled transition tests"},
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "best_worker filter: !failed_workers.contains"},
  {"path": "rio-scheduler/src/dag/mod.rs", "action": "MODIFY", "note": "Cancelled in terminal set"}
]
```

## Tracey

No tracey markers landed in these commits. The `r[impl worker.cancel.cgroup-kill]` and `r[impl sched.backstop.timeout]` annotations were added retroactively in `dadc70c` (P0141) when the round-4 docs catchup introduced those spec rules.

## Entry

- Depends on P0127: phase 3a complete (`BuildCgroup`, `reassign_derivations`, `handle_tick` poison-TTL loop, `SchedulerMessage` stream all exist).

## Exit

Merged as `568c488..74690cf` (4 commits). `.#ci` green at merge. Tests: cgroup kill verifies process termination; scheduler cancel sends `CancelSignal` on mock worker stream; `failed_workers` exclusion dispatches to w2 when w1 is excluded; backstop fires after `running_since` in past.
