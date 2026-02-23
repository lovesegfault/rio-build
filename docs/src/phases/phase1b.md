# Phase 1b: Build Execution (Months 3-6)

**Goal:** End-to-end single-node build via ssh-ng.

## Tasks

- [x] Derivation parser: `.drv` ATerm format (`rio-nix/src/derivation.rs`). `BasicDerivation` wire serialization deferred to Step 6.
- [x] NAR format: streaming reader and writer (`rio-nix/src/nar.rs`). Synchronous `Read`/`Write`-based. Golden-tested against `nix-store --dump`.
- [x] Narinfo: parser and generator (`rio-nix/src/narinfo.rs`)
- [ ] Build opcodes: `wopBuildDerivation` (36), `wopBuildPathsWithResults` (46), `wopBuildPaths` (9)
  - [x] `wopAddMultipleToStore` (44): framed stream parsing with per-entry NAR hash validation
  - [x] `wopAddToStoreNar` (39): STDERR_READ pull loop with NAR hash validation and .drv cache
  - [x] `wopNarFromPath` (38): implemented in Phase 1a
  - [x] `wopQueryMissing` (40): implemented in Phase 1a
  - [x] `wopQueryDerivationOutputMap` (41): session .drv cache lookup with store fallback
- [x] `DerivedPath` string parser (opaque, built with explicit outputs, built with `!*`) — completed in Phase 1a (`rio-nix/src/protocol/derived_path.rs`)
- [ ] `rio-store`: filesystem backend (NAR files on disk)
- [ ] Single-threaded local execution for `wopBuildDerivation` (invoke local nix)
- [ ] Integration test: `nix build --store ssh-ng://localhost .#hello` works end-to-end

## Milestones

- **Month 4:** ATerm derivation parser passes property-based tests against Nix-generated `.drv` files
- **Month 4:** NAR round-trip (read then write then read) is byte-identical
- **Month 5:** `wopBuildDerivation` for a trivial derivation (e.g., `writeText`) succeeds end-to-end
- **Month 6:** `nix build --store ssh-ng://localhost .#hello` completes --- full end-to-end single-node build

> **Scope note:** This is the densest phase in the plan. ATerm parsing and NAR streaming are each substantial. If the scope proves too large, consider extending by one month or splitting NAR/narinfo work from build opcodes.
