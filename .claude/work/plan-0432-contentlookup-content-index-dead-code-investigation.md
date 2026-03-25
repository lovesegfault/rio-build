# Plan 0432: ContentLookup/content_index — dead-code investigation post CA-cutoff

sprint-1 cleanup finding. `ContentLookup` RPC at [`rio-store/src/grpc/mod.rs:587`](../../rio-store/src/grpc/mod.rs) has no production callers — scheduler CA cutoff switched to realisation-based lookup because ContentLookup's self-exclusion is broken for CA (same content → same path → excludes the only matching row; see [`actor/completion.rs`](../../rio-scheduler/src/actor/completion.rs)). Kept because 4 integration tests use it to verify `content_index` population after PutPath/PutPathBatch, and [`store.md`](../../docs/src/components/store.md) still documents it. Promoted from `WONTFIX(P0432)` at `mod.rs:592`.

**Investigation plan** — decide whether `content_index` itself is dead and can cascade-remove: RPC + proto definition + 4 tests + store.md section + PutPath `content_index` INSERT.

## Entry criteria

- CA cutoff landed (scheduler uses realisation-based lookup, not ContentLookup)

## Tasks

### T1 — `docs(adr):` trace content_index write-path and read-path

Grep for `content_index` INSERTs (PutPath, PutPathBatch) and SELECTs (ContentLookup handler, anything else). If the ONLY reader is the ContentLookup RPC and the ONLY callers of that RPC are the 4 integration tests, the table is test-only-observable — a candidate for removal.

Check if any future plan (P0434 manifest-mode, dedup metrics, etc.) plans to read `content_index`. Record findings in this plan doc.

### T2a — `refactor(store):` (route-a: remove) cascade-delete content_index

If T1 concludes dead:
- DELETE `content_index` INSERT from PutPath/PutPathBatch handlers
- DELETE ContentLookup RPC handler from `grpc/mod.rs`
- DELETE ContentLookup from proto (`rio-proto/proto/store.proto`)
- DELETE the 4 integration tests (or convert them to assert against narinfo directly)
- NEW migration: `DROP TABLE content_index` (remember: new migration, never edit shipped ones)
- DELETE store.md ContentLookup section

### T2b — `docs(store):` (route-b: keep) document the test-only-observable status

If T1 finds a planned reader or the INSERT cost is negligible and the table serves as a dedup-metrics source: replace `WONTFIX(P0432)` with a doc-comment explaining the table is write-only-for-now, pending plan P0NNN, and update store.md to mark ContentLookup as internal/test-only.

## Exit criteria

- `/nixbuild .#ci` green
- Route-a: `grep content_index rio-store/src/` → only migration history; `grep ContentLookup rio-proto/proto/` → 0 hits
- Route-b: `WONTFIX(P0432)` replaced with concrete rationale + future-plan reference

## Tracey

If route-a removes the RPC, also remove any `r[store.content-lookup.*]` markers from store.md (check `tracey query rule store.content-lookup` first).

## Files

```json files
[
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "T2a: delete ContentLookup handler OR T2b: update WONTFIX comment at :592"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T2a route-a: delete content_index INSERT. Re-grep exact location"},
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "T2a route-a: delete ContentLookup RPC definition"},
  {"path": "rio-store/migrations/", "action": "CREATE", "note": "T2a route-a: new migration DROP TABLE content_index. Pin checksum in rio-store/tests/migrations.rs"},
  {"path": "docs/src/components/store.md", "action": "MODIFY", "note": "T2a: delete ContentLookup section OR T2b: mark internal/test-only"}
]
```

## Dependencies

```json deps
{"deps": [], "soft_deps": [434], "note": "sprint-1 cleanup finding (discovered_from=rio-store cleanup worker). No hard deps — CA cutoff already landed. Soft-dep P0434 (manifest-mode — check if it reads content_index for dedup stats before removing). grpc/mod.rs count check at dispatch (moderate-traffic)."}
```

**Depends on:** nothing — CA cutoff is in.
**Conflicts with:** [`grpc/mod.rs`](../../rio-store/src/grpc/mod.rs) moderate-traffic; route-a deletes ~40L, route-b is comment-only. [`put_path.rs`](../../rio-store/src/grpc/put_path.rs) — several plans touch it; route-a's INSERT-delete is a small hunk.
