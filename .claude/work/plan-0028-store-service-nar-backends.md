# Plan 0028: Store service — NAR backends + PostgreSQL metadata layer

## Context

The store service is the distributed NAR cache: every worker reads inputs from it, every build uploads outputs to it, the gateway queries it for `wopQueryPathInfo`/`wopIsValidPath`. It's the simplest of the four services architecturally — stateless-ish CRUD on blobs + metadata — but it's load-bearing for everything else.

This plan implements the full `StoreService` gRPC surface plus the backend abstraction (filesystem, S3, in-memory). Critically: the write-ahead pattern. `PutPath` inserts a placeholder row (`status='uploading'`), writes the blob, verifies SHA-256, then flips `status='complete'`. Idempotent on re-upload. `GetPath` streams with a `HashingReader` wrapper that verifies integrity on the way out — bitrot defense.

**Shared commit with P0029:** `f58f948` is a 4,529-insertion mega-commit that landed store AND scheduler together. This plan covers the `rio-store/` half. See P0029 for the scheduler half.

## Commits

- `f58f948` — feat(rio-build): implement store service and FIFO scheduler (store half)

## Files

```json files
[
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "StoreServiceServer impl: PutPath (streaming + SHA-256 verify), GetPath (streaming + integrity check), QueryPathInfo, FindMissingPaths"},
  {"path": "rio-store/src/backend/mod.rs", "action": "MODIFY", "note": "NarBackend trait: async put(key, bytes) -> key, get(key) -> bytes, delete, exists"},
  {"path": "rio-store/src/backend/filesystem.rs", "action": "MODIFY", "note": "local disk: tokio::fs::write + rename (NO fsync yet — fixed P0047)"},
  {"path": "rio-store/src/backend/s3.rs", "action": "MODIFY", "note": "aws-sdk-s3 put_object/get_object/delete_object/head_object"},
  {"path": "rio-store/src/backend/memory.rs", "action": "MODIFY", "note": "HashMap<String, Vec<u8>> — test-only"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "PG layer: insert_uploading (ON CONFLICT DO NOTHING placeholder), complete_upload (flip status), query_path_info (filter status='complete')"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "wire gRPC server + backend selection via CLI (--backend=fs|s3|memory)"}
]
```

## Design

**PutPath flow:** client streams chunks (first chunk carries `PathInfo` metadata, rest are NAR bytes). Server: (1) validates `nar_size` declared, (2) `insert_uploading` — placeholder row with `nar_size=0` marker, (3) accumulates chunks, (4) `NarDigest::from_bytes` verifies SHA-256 matches the declared `nar_hash`, (5) `backend.put(sha256_hex + ".nar", bytes)`, (6) `complete_upload` flips `nar_blobs.status='complete'` and updates `narinfo.nar_size`. On hash mismatch: `INVALID_ARGUMENT`, placeholder remains (BUG — fixed in P0035/P0042).

Idempotency: if `check_complete` returns an existing entry before step 2, short-circuit with `created=false`. This matters when two workers upload the same output concurrently.

**GetPath flow:** `query_path_info` for the blob key, `backend.get(key)`, stream chunks wrapped in `HashingReader`. After the last chunk, compare computed hash to stored `nar_hash`. On mismatch: `DATA_LOSS` status. The client knows not to trust the data.

**Backend trait:** deliberately minimal — `put` returns the key (allowing backends to shard/rewrite), `get` returns `Option<Bytes>`, `delete`, `exists`. S3 backend uses the SDK's default retry config (3 attempts — made tunable in P0036).

P0009 (NAR reader/writer) is the serialization layer these bytes are in. P0010 (narinfo format) defines the metadata shape. Both from phase 1b.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Store spec text in `docs/src/components/store.md` was later retro-tagged with `r[store.putpath.*]`, `r[store.getpath.integrity]`, `r[store.backend.*]` markers.

## Outcome

Merged as `f58f948` (shared with P0029). 29 new tests across store+scheduler; store portion covers PutPath roundtrip, hash verification, idempotency. Filesystem backend has NO fsync — this is a durability bug that surfaces in P0047.
