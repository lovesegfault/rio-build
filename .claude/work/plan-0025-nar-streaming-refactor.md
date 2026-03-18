# Plan 0025: NAR streaming refactor — AsyncRead read+write paths, FramedStreamReader, HashingReader

## Context

Phase 1b's `MemoryStore` held NAR data in `Vec<u8>`. Every upload allocated a buffer sized to the full NAR; every download cloned that buffer. For a trivial test derivation that's kilobytes. For `nixpkgs#hello`'s closure it's tens of megabytes per path. For phase 2's real filesystem store the buffer is pure waste — data should flow from wire to disk (or disk to wire) without ever materializing fully in memory.

This plan converts both directions to streaming. Read side first (`nar_from_path` returns `AsyncRead` instead of `Vec<u8>`), a small proof of concept. Then write side (`add_path` takes `AsyncRead`, `FramedStreamReader` decodes the wire format as a stream, `HashingReader` computes SHA-256 incrementally as bytes flow through). The two halves landed a day apart (Feb 24 20:12, Feb 25 15:46–16:24), separated by P0021–P0024's feature work, but they're one architectural change.

## Commits

- `93c3b7d` — refactor(rio-build): stream nar_from_path via AsyncRead instead of Vec<u8>
- `d6806a8` — docs(rio-build): add TODO comments for deferred streaming improvements
- `161747e` — feat(rio-build): stream NAR data via FramedStreamReader and move validation to store
- `c657349` — refactor(rio-build): use buffered NAR data for .drv caching in add-path handlers
- `2ffc6f6` — feat(rio-build): add streaming NAR validation via HashingReader
- `27c72dd` — refactor(rio-build): use bytes::Bytes for NAR storage in MemoryStore

Six commits. `93c3b7d`+`d6806a8` are the read-side; `161747e..27c72dd` are the write-side. Non-contiguous in git history — `d6806a8`'s TODOs literally document the gap between the two halves.

## Files

```json files
[
  {"path": "rio-build/src/store/traits.rs", "action": "MODIFY", "note": "nar_from_path → Option<NarReader> (Box<dyn AsyncRead + Send + Unpin>); add_path takes impl AsyncRead + drain contract"},
  {"path": "rio-build/src/store/memory.rs", "action": "MODIFY", "note": "Cursor<Bytes> for nar_from_path; add_path drains reader into Bytes; bytes::Bytes storage (Arc-refcounted, O(1) clone)"},
  {"path": "rio-build/src/store/mod.rs", "action": "MODIFY", "note": "pub type NarReader; pub mod validate"},
  {"path": "rio-build/src/store/validate.rs", "action": "NEW", "note": "HashingReader<R: AsyncRead> — wraps reader, computes SHA-256 + byte count incrementally; validate_nar_digest(info, digest, size)"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "FramedStreamReader<R: AsyncRead> — decodes framed format as a stream (impl AsyncRead for it)"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_nar_from_path streams 64KB chunks; opcode 39 streams via FramedStreamReader (no buffer); opcode 44 streams inner NARs; opcodes 7/8/44 keep buffer for .drv caching (Cursor::new(buf.as_slice()))"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "pass reader through to store"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "streaming-compatible test framing"},
  {"path": "rio-build/Cargo.toml", "action": "MODIFY", "note": "add bytes dep"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "bytes workspace dep"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "document streaming architecture + .drv buffering rationale"}
]
```

## Design

**Read side (`93c3b7d`).** `Store::nar_from_path` signature changed from `→ Option<Vec<u8>>` to `→ Option<NarReader>` where `NarReader = Box<dyn AsyncRead + Send + Unpin>`. `MemoryStore` wraps its stored bytes in `std::io::Cursor` (which implements tokio `AsyncRead` via `tokio::io::AsyncReadExt`). `handle_nar_from_path` now loops reading 64 KiB chunks from the reader and writing them to the wire — no intermediate `Vec<u8>` holding the full NAR.

**Write side: `FramedStreamReader` (`161747e`).** Previously `read_framed_stream` collected all frames into a `Vec<u8>` before returning. `FramedStreamReader<R>` implements `AsyncRead` by decoding frames on demand: `poll_read` reads the next frame length, then serves bytes from that frame until exhausted, then reads the next length. `u64(0)` frame → EOF. The upload handler wraps the wire reader in `FramedStreamReader` and passes it directly to `Store::add_path` — NAR bytes flow from wire to store without a handler-level buffer.

**`Store::add_path` drain contract.** The trait now takes `impl AsyncRead` instead of `Vec<u8>`. The contract: implementations must fully drain the reader (read to EOF) even on error, because the wire reader's position must advance past the NAR for the next opcode to parse correctly. `MemoryStore` drains into an internal `BytesMut`, freezes to `Bytes`, stores.

**`HashingReader` (`2ffc6f6`).** The final composable piece. Wraps any `AsyncRead`; as bytes pass through `poll_read`, they're also fed to a `Sha256` hasher and a byte counter. After draining, `into_digest()` returns `(hash, size)`. The store backend wraps the incoming `FramedStreamReader` in `HashingReader`, drains it, then calls `validate_nar_digest(info, digest, size)` which compares against `PathInfo.narHash` / `PathInfo.narSize`. No second pass over the buffer — hashing happens during the stream.

**`.drv` caching exception (`c657349`).** Opcodes 7, 8, and 44's per-entry NARs are buffered at the handler level — `Cursor::new(buf.as_slice())` — so `try_cache_drv` can run on the bytes after `add_path` consumes the cursor. This is intentional: `.drv` files are small (hundreds of bytes + ~112 bytes NAR wrapper) and must be fully in memory for the ATerm parser anyway. Opcode 39 streams unbuffered and skips `.drv` caching — if a `.drv` arrives via 39, `QueryDerivationOutputMap` falls back to the store fetch path. Documented in `gateway.md` so it doesn't look like an oversight.

**`bytes::Bytes` storage (`27c72dd`).** `MemoryStore`'s NAR map switched from `Vec<u8>` to `bytes::Bytes`. `Bytes` is Arc-refcounted — cloning is `O(1)` (pointer copy + refcount bump) instead of `O(n)` (allocate + memcpy). `nar_from_path` clones out of the RwLock-guarded map; with `Vec<u8>` that held the write lock for the duration of the memcpy. With `Bytes` it's constant time.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule store.stream.read`, `store.stream.write`, `store.validate.hash`.

## Outcome

NAR data never fully materializes at the handler layer (except for `.drv` caching, which is bounded by `.drv` size). `FramedStreamReader` and `HashingReader` are reusable primitives — phase 2's filesystem store uses them verbatim. `MemoryStore` clones are `O(1)`. Phase 1b ends here — `phase-1b` tag on `27c72dd`. Architecture is ready for phase 2a's real store backend and distributed workers.
