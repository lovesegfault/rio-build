# Phase 1b: Build Execution (Months 3-6)

**Goal:** End-to-end single-node build via ssh-ng.

## Tasks

- [ ] Derivation parser: `.drv` ATerm format, `BasicDerivation` wire serialization
- [ ] NAR format: streaming reader and writer
- [ ] Narinfo: parser and generator
- [ ] Build opcodes: `wopBuildDerivation` (36), `wopBuildPathsWithResults` (46), `wopBuildPaths` (9), `wopAddToStoreNar` (39), `wopAddMultipleToStore` (44), `wopNarFromPath` (38), `wopQueryMissing` (40), `wopQueryDerivationOutputMap` (41)
- [ ] `DerivedPath` string parser (opaque, built with explicit outputs, built with `!*`)
- [ ] `rio-store`: filesystem backend (NAR files on disk)
- [ ] Single-threaded local execution for `wopBuildDerivation` (invoke local nix)
- [ ] Integration test: `nix build --store ssh-ng://localhost .#hello` works end-to-end

## Milestones

- **Month 4:** ATerm derivation parser passes property-based tests against Nix-generated `.drv` files
- **Month 4:** NAR round-trip (read then write then read) is byte-identical
- **Month 5:** `wopBuildDerivation` for a trivial derivation (e.g., `writeText`) succeeds end-to-end
- **Month 6:** `nix build --store ssh-ng://localhost .#hello` completes --- full end-to-end single-node build

> **Scope note:** This is the densest phase in the plan. ATerm parsing and NAR streaming are each substantial. If the scope proves too large, consider extending by one month or splitting NAR/narinfo work from build opcodes.
