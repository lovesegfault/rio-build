# Plan 0263: Worker client-side chunker

Worker-side dedup: before streaming a full NAR, chunk locally with FastCDC (same parameters as [`rio-store/src/chunker.rs`](../../rio-store/src/chunker.rs)), call `FindMissingChunks`, `PutChunk` only the missing ones, then `PutPath` with manifest-only mode. Identical NARs upload zero bytes on the second attempt.

## Entry criteria

- [P0262](plan-0262-putchunk-impl-grace-ttl.md) merged (`PutChunk` server accepts chunks)

## Tasks

### T1 — `feat(worker):` chunker integration

MODIFY [`rio-worker/src/upload.rs`](../../rio-worker/src/upload.rs) (or NEW `rio-worker/src/chunker.rs`):

```rust
// 1. Chunk the NAR locally — same FastCDC params as store's chunker.
//    Share constants via rio-common OR copy (small enough to copy).
let chunks = fastcdc::chunk(&nar_bytes, MIN_SIZE, AVG_SIZE, MAX_SIZE);
let chunk_hashes: Vec<_> = chunks.iter().map(|c| blake3::hash(c).into()).collect();

// 2. Ask store which chunks it's missing.
let missing = store_client.find_missing_chunks(FindMissingChunksRequest {
    chunk_hashes: chunk_hashes.clone(),
}).await?.into_inner().missing;

// 3. Upload only missing chunks.
for (hash, data) in chunk_hashes.iter().zip(chunks.iter()) {
    if missing.contains(hash) {
        store_client.put_chunk(stream_chunk(hash, data)).await?;
        metrics::counter!("rio_worker_chunks_uploaded_total").increment(1);
    } else {
        metrics::counter!("rio_worker_chunks_skipped_total").increment(1);
    }
}

// 4. PutPath with manifest-only mode (chunk list, not raw NAR).
store_client.put_path_manifest(PutPathManifestRequest {
    store_path, chunk_hashes, nar_hash, ...
}).await?;
```

### T2 — `feat(worker):` metrics

Register `rio_worker_chunks_skipped_total` + `rio_worker_chunks_uploaded_total` in worker's metrics init.

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
