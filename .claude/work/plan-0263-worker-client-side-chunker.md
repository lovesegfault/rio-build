# Plan 0263: Worker client-side chunker (scoped down — audit Batch A #1)

**Scope change 2026-03-18:** Worker is NOT fully trusted (threat model decision). `r[store.integrity.verify-on-put]` requires store to independently SHA-256 the NAR stream — with manifest-only mode, store would have to reconstruct the NAR from ChunkCache/S3 to verify. Bandwidth "win" is net positive only if ChunkCache hit rate is high.

**This plan is now the ZERO-PROTO-CHANGE path.** Exit criterion ("second identical NAR → zero chunks") is already met by the idempotency fast-path (`r[store.put.idempotent]`). Worker calls existing `FindMissingPaths` → if path already complete, skip. Else stream full NAR via existing `PutPath`. Server-side `refcount==1` dedup (cas.rs:81) already skips S3 for existing chunks.

The manifest-mode bandwidth optimization → `TODO(phase6)`: measure `rio_store_chunk_cache_hits_total` ratio first. If high (>80%), revisit. If low, the reconstruction cost negates the upload savings.

## Entry criteria

- [P0262](plan-0262-putchunk-impl-grace-ttl.md) merged (`PutChunk` server accepts — used by a future phase6 if manifest-mode ships; not by this plan)

## Tasks

### T1 — `feat(worker):` FindMissingPaths pre-check

MODIFY [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs):

```rust
// Idempotency pre-check: is this path already in the store?
// r[store.put.idempotent] — zero bytes transferred if already complete.
let already_valid = store_client
    .find_missing_paths(FindMissingPathsRequest {
        paths: vec![store_path.to_string()],
    })
    .await?
    .into_inner()
    .missing
    .is_empty();

if already_valid {
    metrics::counter!("rio_worker_upload_skipped_idempotent_total").increment(1);
    return Ok(());
}

// Stream full NAR via existing PutPath. Server-side refcount==1 dedup
// (cas.rs:81) skips S3 for chunks it already has.
// TODO(phase6): manifest-mode bandwidth opt — measure chunk_cache hit
// rate first (worker NOT trusted → store must reconstruct NAR to verify).
self.stream_put_path(store_path, nar_bytes).await?;
```

### T2 — `feat(worker):` metrics

Register `rio_worker_upload_skipped_idempotent_total` in worker's metrics init.

### T3 — `test(vm):` same NAR twice → zero chunks second time

VM scenario (or extend existing upload test):
```nix
# r[verify store.chunk.put-standalone]  (col-0 header)
# Build same derivation twice. Scrape rio_worker_chunks_skipped_total
# before/after second build — expect skipped > 0, uploaded == 0 for identical NAR.
```

## Exit criteria

- `/nbr .#ci` green
- VM: second identical-NAR upload shows `chunks_skipped_total > 0`

## Tracey

References existing markers:
- `r[store.chunk.put-standalone]` — T3 VM-verifies (cross-plan verify from [P0262](plan-0262-putchunk-impl-grace-ttl.md)'s unit test)

## Files

```json files
[
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "T1: chunk-before-upload flow"},
  {"path": "rio-worker/src/lib.rs", "action": "MODIFY", "note": "T2: metric registration"}
]
```

```
rio-worker/src/
├── upload.rs                     # T1: chunk → FindMissing → PutChunk → PutPath-manifest
└── lib.rs                        # T2: metrics
```

## Dependencies

```json deps
{"deps": [262], "soft_deps": [], "note": "upload.rs last major touch was 4a NAR scanner, no 4b/4c — low collision. FastCDC params shared with rio-store/src/chunker.rs (via rio-common or copy)."}
```

**Depends on:** [P0262](plan-0262-putchunk-impl-grace-ttl.md) — `PutChunk` server works.
**Conflicts with:** `upload.rs` — last major touch was 4a NAR scanner, no 4b/4c.
