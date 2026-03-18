# Plan 0086: Chunked CAS write — FastCDC + ChunkBackend + PutPath write-ahead flow

## Design

This is the phase 2c write path for large NARs: content-defined chunking, BLAKE3-addressed chunk storage, and a write-ahead flow that keeps refcounts consistent under concurrent uploads. Three commits building bottom-up: chunker primitives, storage backend, then the PutPath integration that ties them together.

### FastCDC chunker + manifest serialization

`chunker.rs`: FastCDC v2020 with 16/64/256 KiB min/avg/max. Content-defined boundaries mean a small edit perturbs only the chunk containing it — two NARs that are 70% identical share 70% of their chunks. BLAKE3 for chunk addressing (internal only, never Nix-facing; ~3× faster than SHA-256). The `Chunk` struct borrows `&[u8]` from the input — zero-copy; callers convert to owned `Bytes` at the S3-upload site.

`manifest.rs`: compact binary format `[version:u8][(blake3:32,size:u32_le)]*`. Fixed 36-byte stride makes it seekable. `u32` size (`CHUNK_MAX=256KiB` fits with 16k× headroom). Version byte for future evolution. `MAX_CHUNKS=200k` bound is checked BEFORE `Vec::with_capacity` — a 10GB NAR at 64KB chunks is ~5.6MB of manifest. Proptest covers roundtrip for arbitrary manifests (0..1000 entries), the `1+N*36` length invariant, and no-panic-on-garbage (deserialize either errors cleanly or produces a valid roundtrip). Test gotcha: untyped integer range literals default to `i32`; `i*7919` at `i=1M` overflows. Use `0u64..` for large test-data generators.

### ChunkBackend trait + S3/fs/memory impls

BLAKE3-addressed chunk storage. Key differences from the old `NarBackend` (deleted in P0082): `[u8;32]` keys not strings (no path-traversal concern — hex-encoding a fixed-width array can't contain `../`), `get()` returns `Bytes` not `AsyncRead` (chunks are max 256 KiB; buffering is fine and simplifies reassembly), and `exists_batch()` not `exists()` (PutPath checks ~100 chunks at once).

S3 key scheme: `chunks/{aa}/{blake3-hex}` where `{aa}` is the first two hex chars. Prefix-partitioning — S3 shards by key prefix, so spreading across 256 prefixes avoids hotspotting when 1000 workers hit the store simultaneously. Filesystem uses the same layout (keeps per-dir file counts reasonable, switching backends doesn't surprise operators). `FilesystemChunkBackend` precreates all 256 `{aa}/` subdirs on construction (~1ms, one-time) so `put()` never check-then-mkdirs on the hot path. Same atomic-write pattern as `FilesystemBackend`: temp + fsync + rename + dir-fsync. Skip any of these and a crash leaves the manifest claiming a chunk exists that's zero-length or absent.

S3 `exists_batch`: parallel `HeadObject` bounded to 16 concurrent via chunked `join_all` (simpler than pulling `futures-util` for `buffered()`). `join_all` preserves order — `result[i]` matches `hashes[i]`. `NoSuchKey` → `Ok(None)`; transient error → `Err`. Same distinction as the old S3 backend: "not there" vs "can't tell" are different. Conflating them makes every S3 hiccup look like data loss. Backends don't verify BLAKE3 on read — that's P0087's `ChunkCache` job (layered so the cache verifies exactly once regardless of source).

### Chunked PutPath write-ahead flow

The integration commit — ties chunker + manifest + `ChunkBackend` + refcounts together. Size gate at `INLINE_THRESHOLD` (256 KiB = `CHUNK_MAX`): small NARs stay inline (no dedup benefit from chunking 1-2 chunks; `.drv` files at <10KB all stay inline). Large NARs go through `cas::put_chunked`. Gate is on `nar_data.len()` not `info.nar_size` — they should match (validate checks) but `len()` is what's actually in hand.

**Write-ahead via upgrade-in-place:** `grpc.rs` step 3's `insert_manifest_uploading` runs BEFORE the NAR stream is consumed (it's the idempotency lock; size is unknown at that point). Only at step 6 do we know. So `put_chunked` UPGRADES the existing `uploading` placeholder (adds `manifest_data` + refcounts) rather than creating its own. A standalone insert would either need size upfront (can't) or delete+recreate (race window).

**Refcounts incremented at upgrade time (BEFORE upload), not at complete.** Protects chunks from GC sweep during the upload window. UPSERT via `INSERT ON CONFLICT DO UPDATE` — row-level atomic, no explicit `FOR UPDATE`. Two concurrent PutPaths with overlapping chunks both increment correctly. Dedup check: after UPSERT, chunks with `refcount==1` are ones WE just inserted (need upload); `refcount>1` existed before (skip). Subtle race acknowledged: two uploaders of the SAME chunk in the SAME ~10ms window could both see `count=2` and both skip. Vanishingly rare; P0087's `GetPath` BLAKE3-verify catches it as `NotFound` — clear error, not silent corruption.

**Rollback on any error in steps 3-5:** decrement refcounts + delete placeholders. Decrement FIRST (crash between delete and decrement would leak refcounts forever; decrement-first leaks a manifest pointing at `count=0` chunks, which the orphan scanner catches). No S3 delete on rollback — races with a concurrent uploader that just incremented. GC sweep's job. `put_chunked` handles its own rollback; caller doesn't `abort_upload` (that would delete a placeholder `put_chunked` already cleaned up).

`StoreServiceImpl::with_chunk_backend(pool, Arc<dyn ChunkBackend>)`. `None` (`::new`) forces all-inline — existing test harness unchanged. New `setup_store_chunked` test harness returns the `MemoryChunkBackend` for chunk-count assertions. `get_manifest` now reads `manifest_data` + deserializes in the `Chunked` arm. Invariant violation (NULL `inline_blob` but no `manifest_data`) → loud error, not silent `None`. `rio_store_chunk_dedup_ratio` gauge on each chunked PutPath.

## Files

```json files
[
  {"path": "rio-store/src/chunker.rs", "action": "NEW", "note": "FastCDC v2020 16/64/256 KiB, BLAKE3 addressing, zero-copy Chunk borrows"},
  {"path": "rio-store/src/manifest.rs", "action": "NEW", "note": "[version:u8][(blake3:32,size:u32_le)]* format, 36-byte stride, MAX_CHUNKS bound before alloc, 3 proptests"},
  {"path": "rio-store/src/backend/chunk.rs", "action": "NEW", "note": "ChunkBackend trait, S3/fs/memory impls, chunks/{aa}/ prefix partitioning, 256 subdirs precreated"},
  {"path": "rio-store/src/cas.rs", "action": "NEW", "note": "put_chunked: upgrade-in-place, refcount UPSERT before upload, rollback decrement-first"},
  {"path": "rio-store/src/backend/mod.rs", "action": "MODIFY", "note": "pub mod chunk"},
  {"path": "rio-store/src/lib.rs", "action": "MODIFY", "note": "pub mod chunker, manifest, cas"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "INLINE_THRESHOLD size gate, with_chunk_backend builder, get_manifest Chunked arm"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "insert_manifest_uploading upgrade, manifest_data column wire"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "5 chunked integration tests: small-stays-inline, large-chunks, dedup-across-uploads (refcount=2 + backend count unchanged), idempotent, hash-mismatch-no-leak"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

## Entry

- Depends on **P0082**: `manifests`, `manifest_data`, `chunks` tables from migration 006; `ManifestKind` enum with `Chunked` arm stubbed.

## Exit

Merged as `a94e68d..05cdc50` (3 commits). Tests: 659 → 697 (+38: 6 chunker, 8 manifest unit, 3 manifest proptest, 16 ChunkBackend, 5 chunked PutPath integration).

Library-complete but `rio-store/src/main.rs` ChunkBackend construction from config deferred to phase-3a — this was explicitly called out in `docs/src/phases/phase2c.md` deferrals. The chunked path existed and was fully tested, but the running store binary stayed inline-only until 3a wired `ChunkBackendKind` config → shared `Arc<ChunkCache>`.
