# Plan 0041: Scheduler review — backpressure sharing, dispatch skip, BuildOptions propagation

## Context

Eight miscellaneous scheduler/worker fixes from the third review pass. The two load-bearing ones:

**Dispatch skip:** `dispatch_ready` used `peek + break-on-first-ineligible`. An aarch64 derivation at queue head blocked all x86_64 work indefinitely. Now: drain the queue, dispatch eligible, defer ineligible, push-front the deferred back in order.

**Backpressure sharing:** actor computed hysteresis (80%/60%) but never told `ActorHandle`. Handle used simple 80% threshold → flapping near threshold. Now: shared `Arc<AtomicBool>`.

Plus: worker fetches .drv from store when scheduler sends empty `drv_content` (forward-compat with future inlining); heartbeat bypasses backpressure (false timeout prevention); cycle detection from new edges between existing nodes; per-build `BuildOptions` propagation; `requiredSystemFeatures` extraction from build-hook derivations.

## Commits

- `ef0fbe5` — fix(rio-worker): fetch .drv from store when drv_content is empty
- `e481ae8` — fix(rio-scheduler): heartbeat bypasses backpressure, worker stream errors logged
- `a2154f3` — fix(rio-scheduler): skip ineligible derivations in dispatch instead of blocking queue
- `ad6f3e3` — fix(rio-scheduler): share backpressure hysteresis state between actor and handle
- `5d33f2b` — fix(rio-scheduler): propagate BuildOptions, fix cycle detection, misc cleanup
- `cd64ada` — fix(workspace): worker/gateway fixes + remove unused rio-common deps
- `a67dd9c` — test(workspace): close remaining Step 8 coverage gaps
- `2fc3341` — docs: tag all TODOs with phase markers

## Files

```json files
[
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "fetch_drv_from_store: GetPath .drv NAR, extract_single_file ATerm, parse; compute_input_closure: BFS reference graph via QueryPathInfo from .drv+input_srcs; inherit nix-daemon stderr (piping deadlocks at >64KB)"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "Heartbeat via send_unchecked (not backpressure-gated send); stream.message() Err logged (was: silently dropped); BuildExecution end-to-end test with real DAG actor"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "dispatch_ready: drain/defer/push-front loop; Arc<AtomicBool> backpressure shared with handle; BuildOptions propagated via WorkAssignment (min of non-zero timeouts); workers_active gauge only decrements for fully-registered"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "cycle DFS starts from new_edges parents too (not just newly_inserted)"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "single_node_from_basic extracts requiredSystemFeatures from env; reconstruct_dag unit tests (transitive chain, missing-drv leaf fallback)"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "log SQLite errors (was: silently 0/None)"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "MockScheduler records submit_calls/cancel_calls; disconnect-without-build → no spurious cancel"}
]
```

## Design

**`fetch_drv_from_store`:** scheduler sends `WorkAssignment{drv_content: empty}` (phase 2c will inline it). Worker: `GetPath(drv_path)` → NAR bytes → `nar::extract_single_file` → ATerm bytes → `Derivation::parse`. Backward-compatible: when `drv_content` is non-empty, trust it.

**`compute_input_closure`:** when `input_paths` empty, BFS the reference graph. Seed with `.drv` + `input_srcs` + `input_drvs`. For each, `QueryPathInfo` → add `references` to frontier. Skip `NotFound` (pending dep outputs — FUSE lazy-fetches at build time). This populates the synth DB's `ValidPaths` + `Refs`.

**Dispatch defer:** `while let Some(hash) = ready_queue.pop_front() { if eligible { assign } else { deferred.push(hash) } } for d in deferred.rev() { push_front(d) }`. Repeat until full pass with zero dispatches. O(n²) in the deferred count — P0049 fixes with O(1) dedup.

**Cycle from existing-node edges (`5d33f2b`):** previous DFS only started from `newly_inserted`. A new edge between two *existing* nodes can create a cycle. DFS now also starts from parent endpoints of `new_edges`.

**BuildOptions:** `max_silent_time`, `build_timeout`, `build_cores`. Multiple builds share a derivation → use `min` of non-zero timeouts (most restrictive wins).

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.dispatch.defer-ineligible]`, `r[sched.backpressure.shared-flag]`, `r[worker.executor.fetch-drv]`.

## Outcome

Merged as `ef0fbe5..2fc3341` (8 commits, skipping `0cee559` docs + `c0b3af6` store-security → P0042). `test_dispatch_skips_ineligible_derivation`, `test_backpressure_hysteresis`, `test_cycle_via_new_edge_between_existing_nodes`, `test_build_options_propagated_to_worker`. CLAUDE.md gains the "Deferred work and TODOs" section — `TODO(phaseXY)` tag discipline.
