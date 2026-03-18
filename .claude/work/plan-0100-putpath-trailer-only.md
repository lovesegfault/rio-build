# Plan 0100: PutPath trailer-only protocol mode (delete hash-upfront)

## Design

`PutPath` had two upload modes since phase 2b: **hash-upfront** (client sends `metadata.nar_hash` before the NAR body, store validates at end) and **trailer** (client sends NAR body, hash in `PutPathTrailer` at stream end — hash-on-arrival). The worker used trailer mode (it computes the hash while streaming from FUSE); the gateway used hash-upfront (it already had the hash from the wire protocol). Two modes, two code paths in `rio-store/src/grpc.rs`, two test matrices.

This single-commit plan collapsed both into trailer-only. The gateway's `chunk_nar_for_put` now `mem::take`s the `nar_hash`/`nar_size` out of metadata (zeroing them) and chains a `PutPathTrailer` at stream end. The store rejects non-empty `metadata.nar_hash` as a protocol violation — a loud failure for un-updated clients instead of silent confusion about which mode was active.

Store side: −37 LOC. The `is_trailer_mode` conditional deleted, the `max_allowed` branching deleted, `Vec::with_capacity(nar_size)` deleted (chunks are already bounded by `MAX_NAR_SIZE`; `trailer.nar_size` checked separately). `gateway/grpc_put_path` unchanged — the chaining happens in `chunk_nar_for_put`.

This is in the spirit of the NAR-streaming work (P0025) that introduced `PutPathTrailer`: the trailer mode was the correct design; hash-upfront was legacy scaffolding. The 2c→3a deployment wipe made the protocol break safe.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "PutPathTrailer promoted to only mode; metadata.nar_hash documented as must-be-empty"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "chunk_nar_for_put mem::takes hash/size, chains trailer at stream end"},
  {"path": "rio-store/src/grpc/put_path.rs", "action": "MODIFY", "note": "-37 LOC: is_trailer_mode conditional deleted; non-empty metadata.nar_hash → protocol violation"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "MODIFY", "note": "put_path_raw extracts hash/size for trailer; test_trailer_ignored_when_metadata_has_hash DELETED; test_metadata_with_hash_rejected ADDED (inverted assertion)"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore trailer-apply now unconditional"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "test callers migrated to trailer mode"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). `r[store.put.trailer]` — PutPathTrailer writes hash after NAR body; `r[store.put.hash-verify]` — trailer hash verified against computed. Both annotations were placed on the post-split `put_path.rs` module header in the retroactive sweep.

## Entry

- Depends on P0096: phase 2c complete — the deployment wipe at the 2c→3a boundary made the protocol break safe.
- Depends on P0025: NAR-streaming introduced `PutPathTrailer`; this plan removes the pre-trailer legacy path.

## Exit

Merged as `1428578` (1 commit). `.#ci` green at merge. 813 → 812 tests (one test deleted for removed mode, one test renamed with inverted assertion). `test_put_path_rejects_oversized_nar` now asserts "size mismatch" (trailer vs computed digest) instead of "exceed" (chunk vs declared).
