# Plan 0271: Cursor pagination at admin/builds.rs:22 (absorbs 4c A4)

[P0244](plan-0244-doc-sync-sweep-phase-x.md) retagged `admin/builds.rs:22` from `TODO(phase4c)` → `TODO(phase5)`. This plan closes it. Keyset pagination: `WHERE submitted_at < $cursor ORDER BY submitted_at DESC LIMIT $n`.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged
- [P0244](plan-0244-doc-sync-sweep-phase-x.md) merged (retag landed — verify: `grep 'TODO(phase5)' rio-scheduler/src/admin/builds.rs`)

## Tasks

### T1 — `feat(proto):` cursor field in ListBuilds

MODIFY [`rio-proto/proto/admin.proto`](../../rio-proto/proto/admin.proto):
```protobuf
message ListBuildsRequest {
  // ... existing ...
  optional string cursor = <next>;  // opaque — submitted_at timestamp as RFC3339
}
message ListBuildsResponse {
  // ... existing ...
  optional string next_cursor = <next>;  // null if no more pages
}
```

### T2 — `feat(scheduler):` keyset query

MODIFY [`rio-scheduler/src/admin/builds.rs`](../../rio-scheduler/src/admin/builds.rs) at `:22`:

```rust
let cursor_ts = request.cursor.as_deref()
    .map(|c| chrono::DateTime::parse_from_rfc3339(c))
    .transpose()?
    .unwrap_or_else(|| chrono::Utc::now());  // first page: now()
let builds = sqlx::query_as!(BuildRow,
    "SELECT * FROM builds WHERE submitted_at < $1 ORDER BY submitted_at DESC LIMIT $2",
    cursor_ts, limit as i64
).fetch_all(&pool).await?;
let next_cursor = builds.last().map(|b| b.submitted_at.to_rfc3339());
```

Delete the `TODO(phase5)` comment.

### T3 — `test(scheduler):` pagination walks full set

Insert 250 builds, page with limit=100, assert 3 pages with cursors chaining correctly, no duplicates, no gaps.

## Exit criteria

- `/nbr .#ci` green
- `rg 'TODO\(phase5\)' rio-scheduler/src/admin/builds.rs` → 0

## Tracey

none — pagination plumbing.

## Files

```json files
[
  {"path": "rio-proto/proto/admin.proto", "action": "MODIFY", "note": "T1: cursor field (parallel with P0270, both EOF-append)"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "T2: keyset query at :22, delete TODO"}
]
```

```
rio-proto/proto/
└── admin.proto                   # T1: cursor fields
rio-scheduler/src/admin/
└── builds.rs                     # T2: keyset at :22
```

## Dependencies

```json deps
{"deps": [245, 244], "soft_deps": [270], "note": "Absorbs 4c A4. admin.proto low — coordinate with P0270 (both EOF-append, merge clean)."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md). [P0244](plan-0244-doc-sync-sweep-phase-x.md) — retag landed.
**Conflicts with:** `admin.proto` parallel-safe with P0270. `admin/builds.rs` low.
