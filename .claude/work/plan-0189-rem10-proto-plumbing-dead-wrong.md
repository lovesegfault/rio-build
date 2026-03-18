# Plan 0189: Rem-10 — Proto plumbing: real ResourceUsage sampler + generation fence + BuildOptions wiring

## Design

**P1 (HIGH).** Three dead/wrong proto plumbing sites. All have correct proto definitions; the code just never populated/read them.

**`ResourceUsage::default()` zero-producer:** `rio-worker/src/runtime.rs:94` sent `resources: Some(ResourceUsage::default())`. Scheduler side (`ListWorkers`) correctly plumbed — served zeros. Autoscaler blind. Fix: `utilization_reporter_loop` writes a shared `ResourceSnapshot` (cpu.stat delta, memory.current/max, statvfs on overlay_base_dir) every 10s; heartbeat loop reads it. Single sampling site → Prometheus gauges and `ListWorkers` always agree. Poll 15s→10s to match `HEARTBEAT_INTERVAL`. Uses cgroup v2, NOT `/proc` — machinery already exists (P0152) and `/proc` would report host-wide not worker-tree.

**Generation fence one-sided:** `HeartbeatResponse.generation` was unread. Worker-side `r[sched.lease.generation-fence]` was unimplemented — split-brain bound only enforced scheduler-side. Fix: worker stores `HeartbeatResponse.generation` via `fetch_max` (not `store` — stale-leader heartbeats interleave during the 15s Lease TTL window; `store` would regress the fence). Reject assignments with `generation < latest-observed`; no ACK (deposed leader actor state is going away). New metric `rio_worker_stale_assignments_rejected_total`.

**`BuildOptions` subfields ignored:** `max_silent_time` + `build_cores` hardcoded 0 in `client_set_options`. Scheduler already computed per-derivation min-nonzero/max across intersecting builds; worker was dropping it. Fix: plumb through `run_daemon_build` from `assignment.build_options`.

Cleanup: reserve `WorkerMessage` field 4 (`ProgressUpdate` — never sent, TODO(phase5)); reserve `BuildOptions.keep_going` (wrong layer — per-BUILD semantics in `SubmitBuildRequest.keep_going`). Delete scheduler Progress drop-arm.

Remediation doc: `docs/src/remediations/phase4a/10-proto-plumbing-dead-wrong.md` (763 lines).

## Files

```json files
[
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "utilization_reporter_loop writes shared ResourceSnapshot; statvfs for disk"},
  {"path": "rio-worker/src/runtime.rs", "action": "MODIFY", "note": "heartbeat reads ResourceSnapshot (not ::default()); fetch_max generation; reject stale assignments"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "shared ResourceSnapshot Arc"},
  {"path": "rio-worker/src/executor/daemon/spawn.rs", "action": "MODIFY", "note": "plumb max_silent_time + build_cores from assignment.build_options"},
  {"path": "rio-worker/src/executor/daemon/stderr_loop.rs", "action": "MODIFY", "note": "BuildOptions plumbing"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "BuildOptions param"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "client_set_options: max_silent_time + build_cores from params not hardcoded 0"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "reserve WorkerMessage field 4; reserve BuildOptions.keep_going"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "delete Progress drop-arm"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "delete no-op Progress tests"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "generation fence verify test"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "rio_worker_stale_assignments_rejected_total"}
]
```

## Tracey

- `r[impl sched.lease.generation-fence]` ×3 — `190a506` (worker-side, 3 sites)
- `r[verify sched.lease.generation-fence]` — `190a506`

4 marker annotations.

## Entry

- Depends on P0152: worker utilization gauges (this shares the cgroup sampling infrastructure)
- Depends on P0166: balanced channel (generation fence defends against the split-brain BalancedChannel routes around)

## Exit

Merged as `3f29e5c` (plan doc) + `190a506` (fix). `.#ci` green.
