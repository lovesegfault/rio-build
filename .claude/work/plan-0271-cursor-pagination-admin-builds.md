# Plan 0271: Cursor pagination at admin/builds.rs:22 (absorbs 4c A4)

[P0244](plan-0244-doc-sync-sweep-phase-x.md) retagged `admin/builds.rs:22` from `TODO(phase4c)` → `TODO(phase5)`. This plan closes it. Keyset pagination: `WHERE submitted_at < $cursor ORDER BY submitted_at DESC LIMIT $n`.

## Entry criteria

- [P0245](plan-0245-prologue-phase5-markers-gt-verify.md) merged
- [P0244](plan-0244-doc-sync-sweep-phase-x.md) merged (retag landed — verify: `grep 'TODO(phase5)' rio-scheduler/src/admin/builds.rs`)

## Tasks

### T1 — `feat(proto):` cursor field in ListBuilds (audit Batch A #3)

**Audit finding:** RFC3339 string is NOT opaque (leaks single-column impl), chrono is not a dep (db.rs:146 uses `EXTRACT(EPOCH)::bigint` deliberately), and `ListBuildsRequest` is at **types.proto:693** not admin.proto.

MODIFY [`rio-proto/proto/types.proto`](../../rio-proto/proto/types.proto) at `ListBuildsRequest` (`:693`):
```protobuf
message ListBuildsRequest {
  // ... existing ...
  // Opaque cursor. base64(version_byte || submitted_at_micros_i64_be || build_id).
  // Version prefix = change encoding later without wire break.
  optional string cursor = <next>;
}
message ListBuildsResponse {
  // ... existing ...
  optional string next_cursor = <next>;  // null if no more pages
}
```

### T2 — `feat(scheduler):` keyset query + cursor codec

MODIFY [`rio-scheduler/src/admin/builds.rs`](../../rio-scheduler/src/admin/builds.rs) at `:22`:

```rust
// Cursor codec: base64(0x01 || submitted_at_micros_i64_be || build_id_bytes)
// Version byte lets us change this later. No chrono — PG gives micros
// via EXTRACT(EPOCH FROM submitted_at)*1e6, matching db.rs:146 pattern.
const CURSOR_V1: u8 = 0x01;

fn encode_cursor(submitted_at_micros: i64, build_id: &BuildId) -> String {
    let mut buf = vec![CURSOR_V1];
    buf.extend_from_slice(&submitted_at_micros.to_be_bytes());
    buf.extend_from_slice(build_id.as_bytes());
    base64::engine::general_purpose::URL_SAFE_NO_PAD.encode(buf)
}

fn decode_cursor(s: &str) -> Result<(i64, BuildId), Status> {
    let buf = base64::engine::general_purpose::URL_SAFE_NO_PAD
        .decode(s).map_err(|_| Status::invalid_argument("bad cursor"))?;
    if buf.first() != Some(&CURSOR_V1) || buf.len() < 9 {
        return Err(Status::invalid_argument("bad cursor version"));
    }
    let micros = i64::from_be_bytes(buf[1..9].try_into().unwrap());
    let build_id = BuildId::from_bytes(&buf[9..])?;
    Ok((micros, build_id))
}

// Keyset: (submitted_at, build_id) compound — build_id is the tiebreaker
// for same-timestamp rows. No chrono, no RFC3339.
let (cursor_micros, cursor_id) = match request.cursor.as_deref() {
    Some(c) => decode_cursor(c)?,
    None => (i64::MAX, BuildId::max()),  // first page: everything
};
let builds = sqlx::query_as!(BuildRow,
    r#"SELECT *, (EXTRACT(EPOCH FROM submitted_at)*1e6)::bigint AS micros
       FROM builds
       WHERE (EXTRACT(EPOCH FROM submitted_at)*1e6)::bigint < $1
          OR ((EXTRACT(EPOCH FROM submitted_at)*1e6)::bigint = $1 AND build_id < $2)
       ORDER BY submitted_at DESC, build_id DESC LIMIT $3"#,
    cursor_micros, cursor_id.as_bytes(), limit as i64
).fetch_all(&pool).await?;
let next_cursor = builds.last().map(|b| encode_cursor(b.micros, &b.build_id));
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
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "T1: cursor field at ListBuildsRequest :693 (audit: was wrongly admin.proto)"},
  {"path": "rio-scheduler/src/admin/builds.rs", "action": "MODIFY", "note": "T2: keyset query at :22, delete TODO"}
]
```

```
rio-proto/proto/
└── types.proto                   # T1: cursor fields at :693
rio-scheduler/src/admin/
└── builds.rs                     # T2: keyset at :22
```

## Dependencies

```json deps
{"deps": [245, 244], "soft_deps": [270], "note": "Absorbs 4c A4. types.proto is HOT (count=29) — coordinate with P0270+P0248+P0276 (all EOF-append, merge clean but serialize)."}
```

**Depends on:** [P0245](plan-0245-prologue-phase5-markers-gt-verify.md). [P0244](plan-0244-doc-sync-sweep-phase-x.md) — retag landed.
**Conflicts with:** `admin.proto` parallel-safe with P0270. `admin/builds.rs` low.
