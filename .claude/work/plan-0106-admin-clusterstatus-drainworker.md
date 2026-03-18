# Plan 0106: AdminService.ClusterStatus + DrainWorker + GenerationReader

## Design

First feat cluster of phase 3a proper. Four commits implemented the scheduler admin surface that the rest of the phase builds on: `ClusterStatus` (the autoscaler's input signal), `DrainWorker` (the graceful-shutdown hook), and `GenerationReader` (leader-election prep).

`8e18b9f` implemented `AdminService.ClusterStatus` — a point-in-time snapshot of scheduler state (active/total/draining workers, queued/running derivations, builds-in-progress, uptime). The controller autoscaler polls this every 30s to decide scale-up/down. `ActorCommand::ClusterSnapshot` is O(workers + builds + dag_nodes) per call, fine for 30s poll cadence. Uses `send_unchecked` to bypass backpressure: the autoscaler needs a reading **especially** when saturated; dropping the query under load would blind it exactly when scale-up is needed. `active_workers` = `is_registered()` (stream AND system both set) — stream-only workers (BuildExecution opened, no heartbeat yet) count in total but not active; they're not dispatchable capacity. `running_derivations` = `Assigned | Running` — both mean a worker slot is reserved. `uptime_since` computed as `SystemTime::now() - started_at.elapsed()` rather than capturing SystemTime at construction: `Instant` is monotonic, so this gives "start time in current wall-clock terms" even across NTP jumps.

`b015b30` implemented `DrainWorker`: set `WorkerState.draining=true`, dispatch skips via `has_capacity()` checking `!draining` first (short-circuit the arithmetic for the common scale-down case). One-way — no un-drain. A draining worker is on its way out; recovery is a fresh pod with a new `worker_id`. Eliminates drain/undrain races with dispatch. `force=true` reassigns in-flight via the same `reset_to_ready` path as `WorkerDisconnected` (extracted into `reassign_derivations`) — the worker's nix-daemon keeps running those builds, but scheduler stops caring and redispatches. Deterministic builds → same output either way. Unknown `worker_id` → `accepted=false, running=0`, **not** a gRPC error: the worker's preStop races with stream close on SIGTERM (`select!` break → stream drop → `WorkerDisconnected` removes entry → drain arrives at empty slot). Caller proceeds as if drain succeeded. `send_unchecked` again: drain MUST land under backpressure — a shutting-down worker accepting new assignments when capacity is shrinking is a feedback loop into more load.

`8b952b3` refactored `generation` from `u64` to `Arc<AtomicU64>` with a `GenerationReader` accessor — prep for P0114 where the lease task increments it on acquire.

`531add9` added phase3a workspace deps (`kube`, `kube-runtime`, `k8s-openapi`, `tonic-health`, `schemars`) in one chore commit.

## Files

```json files
[
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "ClusterStatus handler; check_actor_alive guards dead actor"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "ClusterSnapshot computation; WorkerState.draining flag"},
  {"path": "rio-scheduler/src/actor/handle.rs", "action": "MODIFY", "note": "send_unchecked for ClusterSnapshot + DrainWorker; GenerationReader accessor"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "has_capacity checks !draining first; reassign_derivations extracted"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "dispatch_ready early-returns for draining workers"},
  {"path": "rio-scheduler/src/state/worker.rs", "action": "MODIFY", "note": "draining: bool field; is_registered helper"},
  {"path": "rio-scheduler/src/actor/command.rs", "action": "MODIFY", "note": "ActorCommand::ClusterSnapshot + DrainWorker variants"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "phase3a workspace deps: kube, kube-runtime, k8s-openapi, tonic-health, schemars"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[sched.admin.cluster-status]`, `r[sched.admin.drain-worker]`, `r[sched.worker.draining]` — all placed on `admin/mod.rs` and `state/worker.rs` module headers in the retroactive sweep. `r[verify]` annotations on `admin/tests.rs`.

## Entry

- Depends on P0096: phase 2c complete — `WorkerState` and the actor mailbox pattern existed.
- Depends on P0102: `state/worker.rs` was created by the split in P0102; `draining` was added to the post-split file.

## Exit

Merged as `531add9..8b952b3` (4 commits). `.#ci` green at merge. 8 new tests: `cluster_status_empty`, `counts_registered_workers` (stream-only vs fully-registered distinction), `counts_queued_and_running` (max_builds=1 + 2 drvs → 1 queued, 1 Assigned), `actor_dead_returns_unavailable`, `drain_empty_id_invalid`, `drain_unknown_not_error` (preStop race), `drain_stops_dispatch` (ClusterStatus shows draining=1/active=0), `force_reassigns` (other worker gets reassigned drv).
