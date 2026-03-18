# Plan 0085: Scheduler cache-check circuit breaker

## Design

Resolved `TODO(phase2c)` at `rio-scheduler/src/actor/merge.rs:426`. Without this, a sustained store outage means every `SubmitBuild` treats every derivation as a cache miss — an avalanche of unnecessary rebuilds once the store comes back (or workers thrash trying to fetch inputs that aren't there).

`CacheCheckBreaker`: 5 consecutive failures trips the breaker open for 30 seconds. While open, the cache check STILL runs — this is the half-open probe. On success the breaker closes and the result is used; on failure `SubmitBuild` is rejected with `ActorError::StoreUnavailable` (maps to gRPC `UNAVAILABLE`). The half-open probe is better than skip-the-call-while-open: if we skipped entirely, the breaker could only close via timeout. A successful probe is a faster signal that the store is back. Same one-RPC-per-SubmitBuild cost as the closed state — no extra load.

Under-threshold failures (1-4 consecutive): proceed with empty cache-hit set. Wasteful (the build runs with 100% miss) but tolerable for a handful. The breaker catches SUSTAINED outages, not transient blips.

Breaker rejection rolls back the merge via the same `cleanup_failed_merge` path as DB persistence failure — otherwise the build would be half-committed (Active in DB but rejected to client). `saturating_add` on `consecutive_failures`: `u32` wrap to 0 would accidentally close the breaker. Not achievable in practice at any realistic failure rate, but a real bug with bare `+= 1`.

Test gotcha: each merge in the integration test needs a UNIQUE tag. `make_test_node` derives `drv_hash` from the tag; merging the same node twice gives empty `newly_inserted` → cache check skipped → no probe → no failure recorded. The test would silently pass for the wrong reason.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/mod.rs", "action": "MODIFY", "note": "CacheCheckBreaker struct, saturating_add"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "half-open probe, StoreUnavailable reject, cleanup_failed_merge rollback"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "ActorError::StoreUnavailable -> gRPC UNAVAILABLE"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "6 breaker unit tests + 1 integration (unique-tag gotcha)"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "UNAVAILABLE -> STDERR_ERROR mapping"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "minor adjustment for breaker test harness"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0062** (2b actor-split, `288479d`): `rio-scheduler/src/actor/merge.rs` submodule exists.

## Exit

Merged as `acd0cf8` (1 commit). Tests: 652 → 659 (+7: 1 integration, 6 breaker unit).

`rio_scheduler_cache_check_circuit_open_total` metric added. P0096's VM test discovered the breaker can't be triggered via `nix-build` when the store is fully down (gateway's `wopEnsurePath` fails BEFORE `SubmitBuild` reaches the scheduler) — unit tests are the validation surface.
