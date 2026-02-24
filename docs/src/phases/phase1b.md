# Phase 1b: Build Execution (Months 3-6)

**Goal:** End-to-end single-node build via ssh-ng.

## Tasks

- [x] Derivation parser: `.drv` ATerm format (`rio-nix/src/derivation.rs`). `BasicDerivation` wire serialization deferred to Step 6.
- [x] NAR format: streaming reader and writer (`rio-nix/src/nar.rs`). Synchronous `Read`/`Write`-based. Golden-tested against `nix-store --dump`.
- [x] Narinfo: parser and generator (`rio-nix/src/narinfo.rs`)
- [x] Build opcodes:
  - [x] `wopBuildDerivation` (36): spawns local `nix-daemon --stdio`, forwards build via client protocol
  - [x] `wopBuildPathsWithResults` (46): per-path BuildResult via local daemon
  - [x] `wopBuildPaths` (9): delegates to local daemon per DerivedPath
  - [x] `wopAddMultipleToStore` (44): framed stream parsing with per-entry NAR hash validation
  - [x] `wopAddToStoreNar` (39): framed stream parsing with NAR hash/size validation and .drv cache
  - [x] `wopAddToStore` (7): legacy CA import with framed stream parsing
  - [x] `wopAddTextToStore` (8): legacy text file import
  - [x] `wopEnsurePath` (10): store path validity check (no-op success)
  - [x] `wopNarFromPath` (38): implemented in Phase 1a
  - [x] `wopQueryMissing` (40): implemented in Phase 1a
  - [x] `wopQueryDerivationOutputMap` (41): session .drv cache lookup with store fallback
- [x] `DerivedPath` string parser (opaque, built with explicit outputs, built with `!*`) — completed in Phase 1a (`rio-nix/src/protocol/derived_path.rs`)
- [ ] ~~`rio-store`: filesystem backend (NAR files on disk)~~ — deferred; MemoryStore sufficient for Phase 1b single-node
- [x] Single-threaded local execution for `wopBuildDerivation` (gateway speaks client protocol to local `nix-daemon --stdio`)
- [x] Integration test: `nix build --store ssh-ng://localhost` exercises full protocol path (`rio-build/tests/integration_build.rs`)

## Milestones

- **Month 4:** ATerm derivation parser passes property-based tests against Nix-generated `.drv` files
- **Month 4:** NAR round-trip (read then write then read) is byte-identical
- **Month 5:** `wopBuildDerivation` for a trivial derivation (e.g., `writeText`) succeeds end-to-end
- **Month 6:** `nix build --store ssh-ng://localhost .#hello` completes --- full end-to-end single-node build

> **Scope note:** This is the densest phase in the plan. ATerm parsing and NAR streaming are each substantial. If the scope proves too large, consider extending by one month or splitting NAR/narinfo work from build opcodes.
