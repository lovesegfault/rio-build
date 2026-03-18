# Plan 0091: Worker locality scoring (bloom + load weighted)

## Design

Replaces `dispatch.rs` first-eligible-worker FIFO with scored selection. `score = transfer_cost * 0.7 + load_fraction * 0.3`. Lowest wins.

`transfer_cost`: normalized count of input paths the worker's bloom filter says are MISSING. Closure approximation = children's `expected_output_paths` (the inputs this derivation needs — persisted on `DerivationState` by P0090). Normalized by max across candidates — relative, not absolute (everyone missing 5 = no locality advantage for anyone). `load_fraction`: `running/max`. Both terms in `[0,1]`.

`W_locality > W_load`: fetching a GB takes MINUTES, queue wait costs a slot. Fetch usually dominates. Test `locality_can_override_load` proves a busier worker with inputs cached beats an idle one without.

No bloom = pessimistic (assume everything missing). A worker that doesn't report its cache gets no locality bonus. Incentivizes workers to send the filter. `WorkerState` gains `bloom` + `size_class` fields. Heartbeat handler stores bloom unconditionally-overwrites — `None` clears stale filter; worker stopped sending = FUSE unmounted, don't use stale snapshot.

gRPC heartbeat parses `local_paths` via `from_wire`. Bounded at 1MiB (8M bits ≈ 800k items at 1% FPR, WAY more than realistic). Rejects whole heartbeat on validation failure — don't silently drop and score as "no locality"; that masks the worker bug.

`BloomFilter` custom `Debug`: don't dump 60KB of bits. Summary `(num_bits, hash_count, byte_len)`.

Single-candidate short-circuit: skip scoring when only one passes the filter. Common in small deployments. `size_class` filter: `None` = no filter (backward compat); worker `size_class=None` = wildcard (pre-P0093 workers).

## Files

```json files
[
  {"path": "rio-scheduler/src/assignment.rs", "action": "NEW", "note": "best_worker: score = transfer_cost*0.7 + load*0.3, normalized, single-candidate short-circuit, size_class filter"},
  {"path": "rio-scheduler/src/actor/dispatch.rs", "action": "MODIFY", "note": "first-eligible FIFO -> best_worker() call"},
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "best_worker wire-in"},
  {"path": "rio-scheduler/src/actor/worker.rs", "action": "MODIFY", "note": "heartbeat bloom overwrite-unconditionally"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "heartbeat local_paths parse via from_wire, 1MiB bound, reject on validation fail"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "WorkerState gains bloom + size_class"},
  {"path": "rio-scheduler/src/lib.rs", "action": "MODIFY", "note": "pub mod assignment"},
  {"path": "rio-common/src/bloom.rs", "action": "MODIFY", "note": "custom Debug (summary, not 60KB bit dump)"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "8 scoring tests: no-candidates-none, single-short-circuits, prefers-lower-load, prefers-inputs-cached, locality-overrides-load, size-class-filter, unclassified-wildcard, no-bloom-pessimistic"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "worker fixtures with bloom"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "dispatch integration with scoring"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0089**: `BloomFilter` in `rio-common`, `HeartbeatRequest.local_paths` proto field populated by workers.
- Depends on **P0090**: `expected_output_paths` persisted on `DerivationState` (closure approximation).

## Exit

Merged as `48e8906` (1 commit). Tests: 780 → 788 (+8).

Created `rio-scheduler/src/assignment.rs` — the module that P0093 (size-class routing) extends with `classify()`.
