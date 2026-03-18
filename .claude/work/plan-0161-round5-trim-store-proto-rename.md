# Plan 0161: Round 5 — CreateTenant trim-store divergence + proto tenant_id→tenant_name rename

## Design

Fourth branch-review. Headline bug: **CreateTenant validated-trimmed-but-stored-untrimmed (`0b08b26`).** `tenant_name.trim().is_empty()` rejected whitespace-only but passed the raw `" team-a "` to PG. Read paths trim (gateway `comment().trim()`, cache auth `str::trim`), so `WHERE tenant_name = 'team-a'` never matched → invisible-whitespace "unknown tenant" bug. Same for `cache_token`. Fixed: trim once, use for both validation and storage.

**`reset_from_poison` didn't clear traceparent (`01faf80`):** live poison→ClearPoison→resubmit kept the original submitter's trace (dedup-upgrade only checked `is_empty()`). Round 4 claimed it fixed this but only covered the restart path.

**Proto rename (`3807d14`):** `SubmitBuildRequest.tenant_id` → `tenant_name` (reserved 1, new field 9). The field carried a NAME since phase-4a began but was NAMED after the UUID it resolves to; `BuildInfo.tenant_id`/`TenantInfo.tenant_id` are actual UUIDs — same name, two meanings. Breaking proto change (`!`) — reserved the old field.

**Layering cleanup (`f467d38`):** moved `resolve_tenant_name` out of `db.rs` (which had been pure-sqlx) into `grpc/mod.rs` alongside the other error-mapping helpers (`parse_build_id`, `actor_error_to_status`).

Polish: `auth.rs` logs DB errors in the 401 fallback path (`767d6ea`); `compute_fractions` split — callers never used both halves (`a1df320`); `extract_traceparent` demoted to private + fix stale doc (`5169d13`, `efe423b`); consistent `#[instrument]` style — drop redundant `tracing::` prefix (`92093d1`).

## Files

```json files
[
  {"path": "rio-scheduler/src/admin/mod.rs", "action": "MODIFY", "note": "CreateTenant: trim once, use trimmed for both validation and INSERT"},
  {"path": "rio-scheduler/src/state/derivation.rs", "action": "MODIFY", "note": "reset_from_poison clears traceparent"},
  {"path": "rio-proto/proto/scheduler.proto", "action": "MODIFY", "note": "SubmitBuildRequest: reserved 1, tenant_name=9"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "resolve_tenant_name moved here from db.rs"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "resolve_tenant_name removed (back to pure-sqlx)"},
  {"path": "rio-store/src/cache_server/auth.rs", "action": "MODIFY", "note": "log DB errors in misconfiguration-check fallback"},
  {"path": "rio-worker/src/cgroup.rs", "action": "MODIFY", "note": "split compute_fractions (callers never used both halves)"},
  {"path": "rio-proto/src/interceptor.rs", "action": "MODIFY", "note": "extract_traceparent private + intra-doc link fix"}
]
```

## Tracey

No new markers. This round is fixes + refactors.

## Entry

- Depends on P0160: round 4 (fixes round-4's missed reset_from_poison case)

## Exit

Merged as `92093d1..7497f2b` (10 commits). `.#ci` green. Phase doc round-5 summary: "92093d1..efe423b, +9 commits".
