# Plan 0077: UUID v7 build_id for time-ordered correlation

## Design

Single production change: `grpc/mod.rs` `SubmitBuild` handler uses `Uuid::now_v7()` instead of `Uuid::new_v4()`. Test code stays v4 (~60 sites) — test IDs don't need ordering.

UUIDv7 (RFC 9562) encodes a Unix-ms timestamp in the high 48 bits. Three wins:

1. **S3 log key prefix-scanning.** `logs/{build_id}/...` sorts by time, so "last 24h of build logs" is a key-range scan, not a full listing.
2. **PG index locality.** Recent builds cluster at the end of the `builds.build_id` btree instead of scattering randomly (v4). Matters for the dashboard's "recent builds" query — the hot index pages stay in cache.
3. **Debugging.** `ls /var/lib/rio/logs/` is chronologically sorted.

No DB migration needed. `builds.build_id UUID PRIMARY KEY DEFAULT gen_random_uuid()` — the `DEFAULT` is a safety net, never hit (we always supply the id from Rust).

`test_build_ids_are_time_ordered_v7`: two submissions with a 2ms gap produce lexicographically-ordered IDs, both parse as version 7. Deliberately **not** testing intra-ms monotonicity — v7's counter field handles that per RFC, but testing it requires contriving >1 call per ms, which is flaky. We test the property we actually care about: chronological ordering at human timescales. 558 → 559 tests.

## Files

```json files
[
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "SubmitBuild: Uuid::new_v4() → Uuid::now_v7()"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "test_build_ids_are_time_ordered_v7 (2ms gap, lexicographic order, version=7)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[obs.correlation.build-id]` (per `observability.md:204`).

## Entry

- Depends on **P0066** (module splits): `grpc/mod.rs` and `grpc/tests.rs` were created as separate files in P0066.

## Exit

Merged as `22df87b` (1 commit). `.#ci` green at merge. `ci-local-fast` green.
