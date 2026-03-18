# Plan 0037: Scheduler-side store cache check — close submission TOCTOU

## Context

The gateway's `reconstruct_dag` calls `FindMissingPaths` before submitting. But between that check and the scheduler's DAG merge, another build might complete a shared derivation. The scheduler would queue it anyway → unnecessary rebuild.

This plan closes the TOCTOU: the scheduler re-queries the store *at merge time*. If a newly-inserted derivation's expected outputs all exist in the store, skip straight to `Completed`. This needs a new proto field (`DerivationNode.expected_output_paths`) and a store client inside the scheduler.

The store query runs *inside the actor event loop* — which is single-threaded. A slow store blocks everything. P0046 wraps this in a timeout.

## Commits

- `5dc6cc4` — fix(rio-scheduler): check store for cached outputs before queueing (closes TOCTOU)
- `8bc0a31` — test(rio-scheduler): verify DB failure handling via pool close and log assertions

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "DerivationNode.expected_output_paths (field 8, additive)"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "populate expected_output_paths from drv.outputs() / basic_drv.outputs()"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "DagActor takes Option<StoreServiceClient>; check_cached_outputs() calls FindMissingPaths before compute_initial_states; emits DerivationCached event + cache_hits_total{source=scheduler}"},
  {"path": "rio-scheduler/src/dag.rs", "action": "MODIFY", "note": "cache-hit derivations transition to Completed before readiness computation"},
  {"path": "rio-scheduler/src/state.rs", "action": "MODIFY", "note": "Completed transition from Created (cache-hit path)"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "connect to store (non-fatal if unavailable — fallback to build-everything)"}
]
```

## Design

**Flow:** `handle_merge_dag` → `dag.merge()` → `check_cached_outputs(newly_inserted)` → for each with non-empty `expected_output_paths`, `store.FindMissingPaths(paths)` → if `missing.is_empty()`, transition `Created→Completed`, emit `DerivationCached` event. Then `compute_initial_states` runs as before — but cached derivations are already `Completed`, so their parents see `all_deps_completed()` true immediately.

**Metric:** `rio_scheduler_cache_hits_total{source="scheduler"}` — distinct from `{source="existing"}` (derivation already Completed in DAG from a prior build, no store query needed).

**Failure mode:** `store_client` is `Option`. If None (connect failed at startup), or if the gRPC call errors, log warn and return empty set = treat as 100% cache miss. Builds proceed, just potentially wastefully. P0044 adds a failure-count metric for alerting.

**DB-failure policy lock-in (`8bc0a31`):** closes the PG pool mid-completion, asserts in-memory state transitions to Completed despite DB write failure AND the error is logged (via `tracing-test`'s `logs_contain`). This locks in the P0033 policy: DB writes are best-effort, in-memory is authoritative.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Later retro-tagged: `r[sched.merge.cache-check]`, `r[sched.merge.cache-hit-transition]`.

## Outcome

Merged as `5dc6cc4..8bc0a31` (2 commits). In-process store with `MemoryBackend` for testing. `tracing-test` 0.2 dev-dep added. The store query has no timeout — P0046 fixes. Double-counting bug introduced here (cached derivations counted in both `completed` and `cached_count`) — P0039 fixes.
