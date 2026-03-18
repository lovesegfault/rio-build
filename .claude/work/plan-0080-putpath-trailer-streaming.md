# Plan 0080: PutPathTrailer + single-pass streaming NAR tee upload

## Design

Before this plan, uploading a 4 GiB build output meant: read the entire NAR into memory (4 GiB), compute SHA-256, allocate a `Vec<PutPathRequest>` of chunks (~4 GiB again), stream it. Peak ~8 GiB per upload; ×4 parallel builds = ~32 GiB. This plan brings it to ~256 KiB per in-flight chunk, ×4 = ~4 MiB total.

**`PutPathTrailer` proto** (`ea91927`): new `oneof` case on `PutPathRequest`: `PutPathTrailer{nar_hash, nar_size}`. Two upload modes, dispatched by whether `metadata.info.nar_hash` is empty:

- **HASH-UPFRONT** (phase-2a, unchanged): metadata has real hash, server validates computed SHA-256 against it, trailer ignored. Used by gateway (`wopAddToStoreNar` — Nix client sends hash before bytes).
- **HASH-TRAILER** (new): metadata hash empty. Client streams NAR while computing SHA-256, sends trailer with finalized hash as the LAST message. Server validates against trailer.

Security invariant unchanged: server ALWAYS computes SHA-256 of received bytes and validates against SOMETHING (either upfront metadata or trailer). The client can never skip validation by sending neither.

**`dump_path_streaming`** (`45d50bd`, `rio-nix`): `(path, &mut impl Write) → byte_count`. Walks the FS, writes NAR framing directly, reads file contents in 256 KiB chunks. **Byte-identical** output to `dump_path` (same wire format, same sort order, same padding) — only memory profile differs. `CountingWriter` wrapper returns total bytes for the trailer's `nar_size`. Short-read detection: file shrinks between `symlink_metadata` (gives len) and the read loop → fail loud. The NAR would already be corrupt (wrote len as prefix, can't provide that many bytes). Overlay upper is frozen post-build so this only catches FS/overlay bugs.

**`HashingChannelWriter` tee** (`rio-worker`): `upload_output` rewritten. `HashingChannelWriter` implements `io::Write`: each `write()` call updates a SHA-256 hasher AND sends a `PutPathRequest::Chunk` message on an mpsc channel. The gRPC upload stream reads from the channel's receiver. `dump_path_streaming` runs in `spawn_blocking` (FS I/O is blocking), writing to the `HashingChannelWriter`; the gRPC stream drains the channel concurrently. One file read, hash + stream simultaneously. Finalize: hasher → trailer, send trailer, close channel.

## Files

```json files
[
  {"path": "rio-proto/proto/types.proto", "action": "MODIFY", "note": "PutPathRequest oneof: +PutPathTrailer{nar_hash, nar_size}"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "dispatch hash-upfront vs hash-trailer by metadata.info.nar_hash.is_empty(); server always validates"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "trailer roundtrip; mode dispatch; security: neither-sent rejected"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockStore trailer support"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "dump_path_streaming(path, &mut impl Write) → byte_count; CountingWriter; short-read detection; BYTE-IDENTICAL to dump_path"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "HashingChannelWriter tee (Write: hasher.update + mpsc.send); spawn_blocking; trailer finalize. 8 GiB peak → ~256 KiB/chunk"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[store.put.trailer]`, `r[wk.upload.stream]`, `r[nix.nar.dump-streaming]`.

## Entry

- Depends on **P0009** (NAR from 1b): `dump_path_streaming` is a second writer for the NAR format P0009 established.
- Depends on **P0025** (NAR-streaming from 1b): commit body says "BYTE-IDENTICAL output to dump_path" — `dump_path` is from P0025's streaming work.
- Depends on **P0073** (ValidatedPathInfo): `rio-store/src/grpc.rs` ingress validation modified in both.

## Exit

Merged as `ea91927..45d50bd` (2 commits). `.#ci` green at merge.
