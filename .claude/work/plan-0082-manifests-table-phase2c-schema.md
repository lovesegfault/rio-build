# Plan 0082: Manifests-table storage + phase2c schema (drops nar_blobs)

## Design

Phase 2a's store persisted NAR content in a separate `nar_blobs` table with a two-step write: `backend.put` then `complete_upload`. Two steps means two failure modes — an orphaned blob if the second write crashes, or metadata pointing at nothing if they run in the wrong order. This plan collapsed it to a single transaction: NAR content now lives in `manifests.inline_blob` (PG BYTEA), and either the NAR and its metadata both commit or neither does. No orphaned-blob cleanup needed. `NarBackend` is gone entirely.

Migration `006_phase2c.sql` is the phase foundation — it was written in full here even though most tables stay empty until later plans wire them. The `manifests` table (inline BYTEA storage + `manifest_data` for the chunked path) replaces `nar_blobs`. `chunks` (BLAKE3-keyed refcounts), `content_index` (nar_hash → store_path reverse index for CA cache hits), and `realisations` (CA derivation outputs) are all created here but populated by P0084/P0086/P0095. `build_history` gains the resource-tracking columns (`ema_peak_memory_bytes`, `ema_output_size_bytes`, `ema_peak_cpu_cores`, `size_class`, `misclassification_count`) for P0092/P0093. `narinfo` gains `registration_time`, `ultimate`, and `idx_narinfo_nar_hash` (the index P0088's cache server uses for `/nar/{hash}.nar.zst` lookups).

The one place the inline/chunked branch lives is `metadata::ManifestKind` — an enum with `Inline` and `Chunked` arms. `GetPath` matches on it; the `Chunked` arm stays a stub here and P0087 fills it. Implementation gotcha: `check_manifest_complete` uses `SELECT EXISTS` not `SELECT 1` — the latter returns `int4`, and decoding that as `i64` is a `ColumnDecode` footgun that produces wrong results silently.

One test was deleted, not ported: `test_get_path_backend_blob_missing` covered the race where metadata is present but the backend lost the blob. That race no longer exists — `inline_blob` lives in the same transaction that flips `status=complete`. The chunked path reintroduces a similar race (manifest claims a chunk that S3 lost), and a test for it landed with P0086. `main.rs` still reads `backend/s3_*` config fields (logs them, unused) so P0086's `ChunkBackend` wiring is purely additive rather than reworking config parsing.

Workspace deps (`fastcdc`, `blake3`, `moka`, `zstd`, `axum`, `tower`, `tower-http`, `ed25519-dalek`, `async-stream`, `ordered-float`) were declared at the workspace root in the same pass — declaration-only, `Cargo.lock` grows as each crate adds `.workspace = true`. `blake3` is shared by chunking (chunk addressing) and bloom filtering (Kirsch-Mitzenmacher double-hash indices from a split 256-bit output). Portable across architectures, unlike AES-NI-based hashes — matters because bloom filters are a wire protocol between worker and scheduler.

## Files

```json files
[
  {"path": "migrations/006_phase2c.sql", "action": "NEW", "note": "manifests, manifest_data, chunks, content_index, realisations tables; narinfo.{registration_time,ultimate,idx_nar_hash}; build_history resource columns; DROPs nar_blobs"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "ManifestKind enum (Inline|Chunked), check_manifest_complete via SELECT EXISTS, registration_time/ultimate persist"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "PutPath single-tx write, GetPath ManifestKind match (Chunked arm stubbed)"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "NarBackend deleted; backend/s3_* config still read (logged, unused)"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "test_get_path_backend_blob_missing deleted (race no longer exists)"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "build_history fixture updated for new columns"}
]
```

## Tracey

Predates tracey adoption (adopted phase-3a, `f3957f7`). No `r[impl`/`r[verify` markers added in this cluster's commits. `tracey_covered=0`.

Spec sites retroactively markered in later phases (for archaeology): store schema migration flow, `ManifestKind` enum as the single inline/chunked branch point, `SELECT EXISTS` for manifest completion check.

## Entry

- Depends on **P0081** (2b terminal): phase 2b complete — `rio-store/src/metadata.rs` and `grpc.rs` at their post-2b state; `nar_blobs` table exists to be dropped.

## Exit

Merged as `03071eb..c1f2153` (2 commits). Tests: 631 → 630 (net -1; `test_get_path_backend_blob_missing` deleted, race eliminated).

BREAKING: migration 006 DROPs `nar_blobs`. Not backward-compatible with 2a/2b store binaries.
