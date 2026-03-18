# Plan 0110: Store chunk backend config + streaming zstd

## Design

Phase 2c implemented chunk storage but left the backend hardcoded to PG-inline. Four commits made it configurable (inline/filesystem/S3) and replaced the buffered zstd compression in the NAR cache server with streaming — O(chunk) memory instead of O(nar_size).

`5bc7c6f` added `ChunkBackendKind` — a serde internally-tagged enum (`tag=kind`, not `type` — `type` is a Rust keyword, `r#type` everywhere would be noisy). `[chunk_backend] kind = "s3"` in TOML. S3 credentials come from aws-sdk default chain (env vars, instance profile), NOT this config — not putting secrets in TOML. Default = `Inline`: backward-compat with pre-phase3a configs that have no `[chunk_backend]` section. The commit also fixed a cache-sharing bug: `StoreServiceImpl::with_chunk_backend` created its OWN `ChunkCache` internally; the comment claimed sharing. Fix: `with_chunk_cache(Arc<ChunkCache>)` — `main.rs` constructs ONE cache and clones the Arc to `StoreServiceImpl` + `ChunkServiceImpl` + `CacheServerState`. A chunk warmed by `GetPath` is hot for `GetChunk`. `ChunkCache::backend()` accessor added so `PutPath` can write chunks directly to the backend without warming the cache (no point caching freshly-written chunks nothing asked for).

`d7af632` wired `ChunkBackend` in `main.rs` — previously a `todo!()`. `4ff6b9f` added NixOS module options: `extraConfig` TOML for `[chunk_backend]`.

`d2618b1` replaced buffered `zstd::encode_all` (4GB peak for 4GB NAR) with a streaming pipeline: chunk-stream → `StreamReader` → `ZstdEncoder` → `ReaderStream` → axum `Body`. Memory is O(chunk × K=8 + zstd window ~8MB) ≈ 10MB live regardless of NAR size. The `StreamReader`/`ReaderStream` round-trip through `AsyncRead` is because `async-compression`'s `ZstdEncoder` wants `AsyncBufRead` input and produces `AsyncRead` output, but axum `Body` wants `Stream<Bytes>` — `tokio-util` provides both adapters. `nar_chunk_stream` returns a boxed `Pin<Box<dyn Stream>>` unified across inline (1-element `stream::once`) and chunked (`buffered(8)` same as `GetPath`). No `Content-Length` header — compressed size unknown until stream ends; axum sends `Transfer-Encoding: chunked`. The `nar_streaming_no_content_length` test asserts this (the observable proof of streaming). Mid-stream chunk errors (S3 transient, BLAKE3 verify fail): stream yields `Err` → connection drops. Response already partially sent so can't change 200 to 500. Client sees truncated zstd, fails decompression. Not elegant but correct — the alternative (buffer everything to detect errors first) is the O(nar_size) being replaced.

## Files

```json files
[
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "ChunkBackend wiring; ONE Arc<ChunkCache> cloned to all consumers"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "ChunkBackendKind serde enum; with_chunk_cache constructor; ChunkCache::backend() accessor"},
  {"path": "rio-store/src/cache_server.rs", "action": "MODIFY", "note": "streaming zstd pipeline: chunk-stream→StreamReader→ZstdEncoder→ReaderStream→Body; nar_chunk_stream boxed Stream"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "with_chunk_cache replaces with_chunk_backend (cache sharing fix)"},
  {"path": "nix/modules/store.nix", "action": "MODIFY", "note": "extraConfig TOML for [chunk_backend]"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "async-compression, tokio-util deps; figment dev-dep (test feature) for Jail env isolation"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[store.chunk.backend-config]`, `r[store.cache.streaming-zstd]`, `r[store.cache.shared-arc]`. P0123 later removed `with_chunk_backend` entirely (`fbf9c17` — it was a footgun; only `with_chunk_cache` remains).

## Entry

- Depends on P0097: `NarBackend` deleted — `ChunkBackend` is now the only backend abstraction.
- Depends on P0102: `grpc/mod.rs` was split out by P0102; `with_chunk_cache` went into the post-split file.

## Exit

Merged as `5bc7c6f..d2618b1` (4 commits). `.#ci` green at merge. `nar_roundtrip_decompresses` still passes (streamed zstd output byte-identical to buffered at same level). `nar_streaming_no_content_length` added. figment TOML tests for `ChunkBackendKind` (what production runs via `rio_common::config::load`).
