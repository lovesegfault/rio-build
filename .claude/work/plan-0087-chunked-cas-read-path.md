# Plan 0087: Chunked CAS read — ChunkCache + GetPath reassembly + ChunkService

## Design

The read-side mirror of P0086. Three layers: a chunk cache that deduplicates concurrent fetches and verifies integrity, `GetPath` reassembly that streams without materializing the full NAR, and the `ChunkService` RPCs for future chunk-level clients.

### ChunkCache: moka LRU + singleflight + BLAKE3 verify

`ChunkCache` wraps `ChunkBackend` with three layers:

1. **moka LRU (2 GiB default, weight-based):** hot chunks in memory. Tracks byte-size per entry so the cap is a real memory bound, not an entry count that might be 100 MiB or 2 GiB depending on chunk-size distribution. At 64 KiB avg, 2 GiB holds ~32k chunks — a whole stdenv closure stays hot.

2. **Singleflight (`DashMap<hash, Shared<BoxFuture>>`):** N concurrent `GetPath`s for the same chunk issue one backend GET, N-1 await the same future. The thundering-herd fix. Spawned (not just `Shared`) so first-caller cancellation doesn't abort the fetch for the N-1 others waiting on it.

3. **BLAKE3 verify:** EVERY returned chunk is hashed against the requested hash. Catches S3 bitrot, memory corruption, backend bugs. ~0.25ms for a 64 KiB chunk — trivial against S3's ~50ms GET. Verify lives HERE not in `ChunkBackend::get` because LRU hits would skip backend verify; putting it at this layer means verify-once regardless of source. Corrupt chunks are NOT cached (verify runs before moka insert). Next call retries backend — S3 bitrot is sometimes transient.

Type gymnastics: `Shared<JoinHandle>` doesn't work — `JoinHandle` output is `Result<T, JoinError>`, `JoinError` isn't `Clone`, `Shared` needs `Output: Clone`. Fix: `.map()` the `JoinHandle` through `.ok().flatten()` BEFORE sharing (panic → `None`, same as backend error → `None` — all three mean "couldn't get chunk", log distinguishes for operators). `.boxed()` erases the unnamable `Map<JoinHandle, closure>` type. Inflight cleanup: remove-after-await, not before. Before = duplicate fetches while awaiting. After = tiny window where a caller awaits an already-complete `Shared` (instant). Runs once per AWAITER (N removes, N-1 no-op) — `DashMap::remove` on missing is cheap. `rio_store_chunk_cache_{hits,misses}_total` counters (ratio gauge deferred — counter pairs aggregate; ratio gauges don't).

### GetPath chunked reassembly

Wires `ChunkCache` into `GetPath`'s `Chunked` arm. Streams chunk-by-chunk without materializing the full NAR — a 4 GiB NAR stays at ~8 chunks × 256 KiB = 2 MiB peak memory, not 4 GiB. `buffered(8)`, NOT `buffer_unordered`. Order matters: scrambled chunks = corrupt NAR. `buffered` preserves yield order even when fetches complete out of order (chunk i+1 might finish first but waits for i).

Incremental SHA-256: both paths feed the hasher as bytes stream past. No second pass, no full-NAR buffer. The chunked path already BLAKE3-verified each chunk (in `ChunkCache`), so whole-NAR SHA-256 is belt-and-suspenders: catches wrong-manifest (right chunks wrong order/missing), reassembly bugs, stale `narinfo.nar_hash`. For inline, SHA-256 IS the primary check. Pre-flight: chunked manifest + no cache configured = `FAILED_PRECONDITION` before spawning the task. Clear config error (inline-only store trying to read what a chunked store wrote) instead of cryptic mid-stream fail.

`StoreServiceImpl.chunk_cache`: `Arc<ChunkCache>` paired with `chunk_backend`. `Arc` because the spawned task needs an owned handle (outlives `&self`). Write path (PutPath) uses backend directly — no point caching freshly-written chunks nobody asked for. `stream_bytes()` helper: the one place slice-into-`NAR_CHUNK_SIZE` + `.to_vec()` + send lives. Both inline and chunked call it. The `.to_vec()` is the only copy between `ChunkCache` and wire.

### ChunkService RPCs

Replaces `ChunkServiceStub`. These RPCs are infrastructure — not used by the current PutPath flow (which chunks server-side and calls `ChunkBackend` directly). They exist so future clients (worker-side chunker, store-to-store replication) can interact at the chunk level without going through PutPath's NAR-shaped API.

`GetChunk`: fetches a single chunk by BLAKE3 hash. Goes through the SAME `ChunkCache` as `GetPath` — a chunk warmed by either RPC is hot for the other. BLAKE3-verified (`ChunkCache` does that). One-message stream (chunks are max 256 KiB, no need to multi-message). Synchronous await in the handler (not spawned task) — `GetPath`'s streaming-task pattern is for large NARs where the stream outlives the handler call; `GetChunk` is one message.

`FindMissingChunks`: batch PG check, returns digests NOT in `chunks` table. One roundtrip vs N S3 `HeadObject`. Validates each digest is 32 bytes; one bad digest fails the batch (client bug indicator, don't silently skip). Bounded at 100k digests (3.2 MiB request body). Piggybacks `rio_store_chunks_total` gauge via `count_chunks()` — infrequent RPC so the extra count query is cheap.

`PutChunk`: still `UNIMPLEMENTED`. Our PutPath is the only chunk writer; no client sends `PutChunk`. Implementing it needs a refcount policy for standalone chunks (chunk without manifest = immediately GC-eligible = useless). TODO(phase3a) if client-side chunking lands.

`main.rs`: `ChunkServiceStub` → `ChunkServiceImpl::new(pool, None)`. With `cache=None`, all RPCs return `FAILED_PRECONDITION` — right answer for an inline-only store. TODO(phase3a) shares the same `Arc<ChunkCache>` with `StoreService` once backend construction lands.

## Files

```json files
[
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "ChunkCache: moka weight-based LRU, singleflight DashMap<Shared<BoxFuture>>, BLAKE3 verify-before-insert"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "GetPath Chunked arm: buffered(8), incremental SHA-256, FAILED_PRECONDITION preflight; ChunkServiceImpl: GetChunk, FindMissingChunks"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "count_chunks() for gauge piggyback"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "ChunkServiceStub -> ChunkServiceImpl::new(pool, None)"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "chunked Put->Get roundtrip, missing-chunk DATA_LOSS, no-cache preflight; GetChunk after PutPath proves shared state, FindMissingChunks filters"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0086**: `ChunkBackend` trait, `cas.rs` module, `ManifestKind::Chunked` arm with deserialized manifest.

## Exit

Merged as `966e3fb..b6f2226` (3 commits). Tests: 697 → 713 (+16: 7 ChunkCache, 3 GetPath chunked, 6 ChunkService).

Completes the chunked CAS library. `main.rs` wiring deferred to phase-3a (see P0086 exit note).
