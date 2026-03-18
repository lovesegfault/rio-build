# Plan 0095: ContentLookup + CA roundtrip integration test

## Design

Completes the CA cache-hit path started by P0084. P0084 stored realisations (`(drv_hash, output_name) → output_path`); this plan added the content-addressed reverse index (`nar_hash → store_path`) and the end-to-end integration test proving the whole chain works.

### ContentLookup impl

`content_index` answers the CA cache-hit question "have we ever seen these bytes?". `nar_hash` IS the content identity for CA — same bytes = same SHA-256 regardless of store path — so `content_hash = nar_hash` is correct, not a hack.

`insert`: `ON CONFLICT DO NOTHING` on `(content_hash, store_path_hash)` PK. Multiple paths can have the same `content_hash` — both rows kept, lookup picks one (same bytes, doesn't matter which). `lookup`: joins `content_index` + `narinfo` + `manifests.status='complete'`. Filters stuck-uploading manifests rather than trusting insert-order. `LIMIT 1` — arbitrary which matching path; they're all the same bytes.

`PutPath` calls `insert` AFTER `complete_manifest` commits (so the join always sees `complete`). Best-effort: failure logs but doesn't fail the upload (path is still addressable by `store_path`; CA hit just missed).

`ContentLookup` RPC: validates 32-byte hash (SHA-256), queries, returns empty-string `store_path` for not-found (proto convention). `NarinfoRow` + `try_into_validated` → `pub(crate)` for the narinfo join.

### CA roundtrip integration test

End-to-end: `wopRegisterDrvOutput` → `wopQueryRealisation` → `ContentLookup`. The Phase 2c CA cache-hit path before Phase 5 early cutoff.

Test flow:
1. Upload a path (the CA build output) with known `nar_hash`
2. `wopRegisterDrvOutput` with Realisation JSON → gateway → store gRPC
3. `wopQueryRealisation` with same `DrvOutput` id → `outPath` roundtrips (THIS is the cache-hit payload)
4. `ContentLookup` via gRPC with `nar_hash` → finds the same path (content identity — `content_index` in action)

Uses `MockStore` (in-memory, PG-free). Wire protocol is real (gateway session via `DuplexStream`). `MockStore.content_lookup` upgraded from stub to `nar_hash` scan (O(n) fine for mock; real has PG index).

## Files

```json files
[
  {"path": "rio-store/src/content_index.rs", "action": "NEW", "note": "insert (ON CONFLICT DO NOTHING), lookup (joins narinfo + manifests.status='complete', LIMIT 1)"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod content_index"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "ContentLookup RPC (32-byte validation, empty-string not-found); PutPath calls insert after complete_manifest"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "NarinfoRow + try_into_validated -> pub(crate)"},
  {"path": "rio-gateway/tests/ca_roundtrip.rs", "action": "NEW", "note": "wopRegisterDrvOutput -> wopQueryRealisation -> ContentLookup end-to-end; MockStore, DuplexStream wire"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore.content_lookup O(n) nar_hash scan"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `content_index` table from migration 006.
- Depends on **P0084**: `RegisterRealisation`/`QueryRealisation` RPCs + `wopRegisterDrvOutput`/`wopQueryRealisation` gateway opcodes — the roundtrip test exercises all of them.

## Exit

Merged as `3fd688f..a8ee9d9` (2 commits). Tests: +5 (3 content_index: insert+lookup roundtrip with narinfo join, missing=None, idempotent duplicate insert; 2 ca_roundtrip: full roundtrip, ContentLookup miss returns empty).

Enables CA cache hits before Phase 5's early cutoff. The `content_index` population at `PutPath` time means every upload is also a CA index entry — no separate indexing pass.
