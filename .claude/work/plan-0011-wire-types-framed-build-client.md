# Plan 0011: Phase 1b wire types — framed stream, BasicDerivation/BuildResult, client protocol

## Context

P0000 scaffolded the wire-format vocabulary for phase 1a's read-only opcodes: u64 primitives, length-prefixed strings, `PathInfo`. Phase 1b's build opcodes need a larger vocabulary. `wopBuildDerivation` takes a `BasicDerivation` on input and returns a `BuildResult`. `wopAddMultipleToStore` wraps its entries in a "framed" byte stream — a different encoding from the padded-string format. And because phase 1b's build execution strategy is "gateway spawns a local `nix-daemon --stdio` and talks client protocol to it," rio needs to speak the client side of the handshake too.

These three additions — framed stream primitive, build wire types, client protocol — landed in three consecutive commits to `rio-nix/src/protocol/`. They're the plumbing layer between P0008's parsers and P0012/P0013's opcode handlers.

## Commits

- `ef4f611` — feat(rio-nix): add framed byte stream read/write for wopAddMultipleToStore
- `23d8c80` — feat(rio-nix): add BasicDerivation and BuildResult wire format types
- `d1d056d` — feat(rio-nix): add client-side Nix worker protocol for local daemon communication

Three commits. One extends `wire.rs`; two add new modules.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "read_framed_stream / write_framed_stream: u64(chunk_len) + unpadded bytes, u64(0) terminator"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "NEW", "note": "BuildMode, BuildStatus, BuildResult, BuiltOutput; read/write_basic_derivation, read/write_build_result"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "NEW", "note": "client_handshake, StderrMessage reader (all 8 types), client_set_options, client_build_derivation"},
  {"path": "rio-nix/src/protocol/mod.rs", "action": "MODIFY", "note": "pub mod build; pub mod client"}
]
```

## Design

**Framed stream.** The standard wire-format string encoding is `u64(len) + data + padding-to-8-bytes`. The framed encoding is different: `u64(chunk_len) + data` (no padding), repeated, terminated by `u64(0)`. `wopAddMultipleToStore` uses framed for the outer stream because individual NAR entries inside can be large and the client wants to send them incrementally without knowing the total size upfront. The implementation enforces per-frame (`MAX_FRAMED_FRAME` = 64 MiB) and aggregate (`MAX_FRAMED_TOTAL` = 1 GiB) bounds before allocation. Proptest roundtrip covers arbitrary chunk boundaries.

**Build types.** `BuildMode` is a three-variant enum (`Normal`/`Repair`/`Check`). `BuildStatus` is thirteen variants including `Built`, `Substituted`, `AlreadyValid`, `TimedOut`, `MiscFailure`. `BuildResult` bundles status, error message, timing fields (`startTime`/`stopTime`), and a list of `BuiltOutput` entries (output name → store path + CA info). `read_basic_derivation` and `write_basic_derivation` serialize P0008's `BasicDerivation` to the wire — `wopBuildDerivation` payload is literally `write_basic_derivation(drv)` followed by `write_u64(build_mode)`.

This version of `BuildResult` had three wire-format bugs that P0016 fixed: wrong integer widths, missing `cpuUser`/`cpuSystem` optional fields, and wrong `builtOutputs` encoding. The bugs were invisible in unit tests (roundtrip passes — wrong format round-trips itself fine) and only surfaced against a real daemon.

**Client protocol.** `client_handshake` is the mirror of P0000's server handshake: send magic, read magic back, negotiate version, exchange features. Tested bidirectionally — client handshake against our own server handshake, so a bug in either breaks the test. `drain_stderr` reads the daemon's STDERR frames until `STDERR_LAST` (daemon acknowledges the operation). `client_build_derivation` sends `wopBuildDerivation` + serialized derivation, then drains STDERR, then reads the `BuildResult`. This is the code that phase 2's workers reuse.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.wire.framed`, `gw.build.result`, `gw.build.mode`, `gw.client.handshake`.

## Outcome

`protocol/build.rs` provides the types P0013's build-opcode handlers serialize. `protocol/client.rs` provides the code P0013's `build_via_local_daemon` calls. `wire.rs` framed-stream primitive provides the decoder P0012's `wopAddMultipleToStore` needs. All three have unit-test roundtrip coverage; all three had conformance bugs that later real-nix integration found.
