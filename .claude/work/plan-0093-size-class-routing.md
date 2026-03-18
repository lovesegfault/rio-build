# Plan 0093: Size-class routing (static cutoffs + memory-bump + overflow)

## Design

Scheduler routes derivations to right-sized worker pools based on estimated duration. Operator-configured cutoffs in `scheduler.toml` (`[[size_classes]]` array). Empty config = backward-compat no-filter. This was the phase 2c answer to "a 2-hour LLVM build shouldn't block a slot on a worker sized for 30-second fetchurl builds." See ADR-015 for the full design rationale.

`classify()`: picks the smallest class where `est_dur <= cutoff`, then bumps to next larger if `ema_peak_memory > class.mem_limit`. Overflows to largest for very-slow builds (they have to go somewhere). Sorts internally so TOML order doesn't matter.

**Overflow chain in dispatch:** try target class first, then walk UP (small → large only). A slow build on a small worker dominates that slot for hours; a fast build on a large worker is just slightly wasteful. Asymmetric costs, asymmetric fallback. `assigned_size_class` recorded on `DerivationState` so completion knows which cutoff to check against.

**Misclassification:** `actual > 2× assigned-class cutoff` → penalty-write EMA (overwrite not blend — one bad route is enough to correct; next normal completion blends back down if it was a fluke). `misclassification_count++` for dashboards. The adaptive `CutoffRebalancer` that would adjust cutoffs from `misclassification_count` was deferred to Phase 4; this plan uses static operator-configured cutoffs.

**Worker side:** `Config.size_class` (default empty = wildcard) → heartbeat → `WorkerState.size_class` (overwrite-unconditionally, same as bloom in P0091). Proto: `HeartbeatRequest.size_class` string (empty → `None` at gRPC boundary).

Metrics: `rio_scheduler_size_class_assignments_total{class}`, `rio_scheduler_misclassifications_total`, `rio_scheduler_cutoff_seconds{class}`, `rio_scheduler_class_queue_depth{class}`. `handle_heartbeat` bloom+size_class tupled to stay under clippy 7-arg limit.

## Files

```json files
[
  {"path": "rio-scheduler/src/assignment.rs", "action": "MODIFY", "note": "classify(): smallest-fitting cutoff, memory-bump, overflow to largest; sorts TOML internally"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "overflow chain: target class first, walk UP only"},
  {"path": "rio-scheduler/src/actor/completion.rs", "action": "MODIFY", "note": "misclassification check (actual > 2x cutoff), penalty-write overwrite-not-blend"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "size_classes config wiring"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "heartbeat size_class overwrite"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "DerivationState.assigned_size_class"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "heartbeat size_class parse (empty -> None); bloom+size_class tupled (clippy 7-arg)"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "heartbeat size_class passthrough"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "penalty-write EMA, misclassification_count++"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "[[size_classes]] TOML config read"},
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "HeartbeatRequest.size_class string"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "size_class in heartbeat"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "Config.size_class (default empty = wildcard)"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "7 classify/cutoff tests"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "size_class worker fixtures"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "e2e routing: bigthing with 120s EMA goes to w-large not w-small"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0090**: `Estimator` provides `est_dur` for `classify()`.
- Depends on **P0091**: `assignment.rs` module created, `size_class` filter hook in `best_worker`, `WorkerState.size_class` field.
- Depends on **P0092**: `ema_peak_memory_bytes` populated (memory-bump decision reads it).

## Exit

Merged as `727938d` (1 commit). Tests: +10 (7 classify/cutoff, 1 heartbeat size_class passthrough, 1 misclassified overwrite-not-blend, 1 e2e routing).

`WorkerPoolSet` CRD + adaptive `CutoffRebalancer` deferred to Phase 4. Cutoffs remained static TOML config through phase 3a/3b.
