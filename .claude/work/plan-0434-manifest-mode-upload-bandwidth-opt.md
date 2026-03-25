# Plan 0434: Manifest-mode upload — bandwidth opt for untrusted-worker path

sprint-1 cleanup finding, deferred remainder of [P0263](plan-0263-worker-upload-findmissing-precheck.md). Worker upload at [`rio-worker/src/upload.rs:547`](../../rio-worker/src/upload.rs) currently streams the full NAR even when the store already has most chunks. Manifest-mode would send manifest-only, letting the store fetch missing chunks from ChunkCache. P0263 scoped down to the zero-proto-change path (FindMissingPaths pre-check); this is the proto-change remainder.

**Gated on measuring `rio_store_chunk_cache_hits_total` ratio in production.** Worker is NOT trusted → store must reconstruct NAR to verify, so the "win" is net positive only if ChunkCache hit rate is high (>80%). Below that, the reconstruction overhead dominates the bandwidth savings. Promoted from the orphan `TODO(P0263-followup)` comment block.

## Entry criteria

- Production metrics confirm `rio_store_chunk_cache_hits_total` / `rio_store_chunk_cache_requests_total` ≥ 0.80 sustained
- [P0263](plan-0263-worker-upload-findmissing-precheck.md) merged (DONE)

## Tasks

### T1 — `feat(proto):` PutPath manifest-mode variant

MODIFY [`rio-proto/proto/store.proto`](../../rio-proto/proto/store.proto). Add a `PutPathManifest` RPC (or a mode flag on PutPath) that accepts a chunk manifest instead of raw NAR bytes. Response indicates which chunks the store couldn't find in ChunkCache (so worker can fall back to streaming those).

### T2 — `feat(store):` PutPathManifest handler — fetch missing chunks from ChunkCache

NEW handler in [`rio-store/src/grpc/`](../../rio-store/src/grpc/). Receives manifest, queries ChunkCache for each chunk hash, reconstructs NAR server-side, verifies nar_hash matches PathInfo. If any chunk is missing, return `NOT_FOUND` with the missing chunk list so worker can retry with full stream.

### T3 — `feat(worker):` upload — try manifest-mode first, fall back to stream

MODIFY [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs) around `:547`. Compute manifest locally (chunk the NAR), send via PutPathManifest. On NOT_FOUND, fall back to the existing full-stream path. Delete the `TODO(P0434)` comment block.

### T4 — `test(store):` manifest-mode hit/miss paths

Integration test: (a) all chunks in cache → PutPathManifest succeeds, zero NAR bytes transferred. (b) one chunk missing → NOT_FOUND with that chunk's hash.

### T5 — `test(worker):` manifest-mode fallback on miss

Worker upload test: mock store returns NOT_FOUND on PutPathManifest → worker retries with full stream → succeeds.

## Exit criteria

- `/nixbuild .#ci` green
- Bandwidth metric: new `rio_worker_upload_bytes_saved` counter shows non-zero in VM test with warm ChunkCache
- Fallback path: manifest-mode failure doesn't break upload (T5 proves it)

## Tracey

May need new `r[store.put.manifest-mode]` and `r[worker.upload.manifest-mode]` markers. Check [`store.md`](../../docs/src/components/store.md) and [`worker.md`](../../docs/src/components/worker.md) for existing chunk-dedup spec text.

## Files

```json files
[
  {"path": "rio-proto/proto/store.proto", "action": "MODIFY", "note": "T1: +PutPathManifest RPC or mode flag"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "T2: +manifest-mode handler (or new put_path_manifest.rs)"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T3: manifest-first try + fallback at :547"},
  {"path": "docs/src/components/worker.md", "action": "MODIFY", "note": "Document manifest-mode + hit-rate gating threshold"}
]
```

## Dependencies

```json deps
{"deps": [263], "soft_deps": [430, 433], "note": "sprint-1 cleanup finding (discovered_from=rio-worker cleanup worker, orphan TODO P0263-followup). Hard-dep P0263 (DONE — the zero-proto-change pre-check). Soft-dep P0430 (ChunkServiceClient decision — manifest-mode may BE the first production caller, affecting P0430's route-a/route-b). Soft-dep P0433 (trailer-refs — both touch upload.rs streaming; coordinate). GATED on production chunk-cache hit-rate measurement — do not dispatch until metrics confirm >80%."}
```

**Depends on:** [P0263](plan-0263-worker-upload-findmissing-precheck.md) — DONE.
**Conflicts with:** [`upload.rs`](../../rio-worker/src/upload.rs) — [P0433](plan-0433-trailer-refs-protocol-extension.md) touches `:177`; T3 here touches `:547`. Same file, non-overlapping sections. [`store.proto`](../../rio-proto/proto/store.proto) — P0433-T1 also adds a field; both additive.
