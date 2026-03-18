# Plan 0013: Build opcodes via local daemon + E2E integration test

## Context

This is the phase 1b milestone feature. Everything prior — ATerm parser, NAR format, wire types, upload opcodes — was plumbing to get here. `wopBuildDerivation` (opcode 36) is the opcode that makes `nix build --store ssh-ng://localhost` actually build something.

Phase 1b's execution strategy is deliberately simple: the gateway doesn't build anything itself. It spawns `nix-daemon --stdio` as a subprocess, speaks client protocol to it (P0011), forwards the derivation, and relays the result. No sandboxing, no scheduling, no distribution — those are phase 2. This plan proves the protocol round-trip works with Nix doing the actual work.

## Commits

- `1eef751` — feat(rio-build): implement wopBuildDerivation, wopBuildPaths, and wopBuildPathsWithResults
- `e1eebf5` — test(rio-build): add end-to-end integration test for nix build via ssh-ng

Two commits. First adds the handlers; second adds the integration test that exercises the full phase 1b pipeline.

## Files

```json files
[
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "handle_build_derivation, handle_build_paths, handle_build_paths_with_results; build_via_local_daemon shared helper"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "retarget test_known_unimplemented_opcode (36 now implemented, use 42)"},
  {"path": "rio-build/tests/integration_build.rs", "action": "NEW", "note": "E2E: SSH handshake → upload → QueryDerivationOutputMap → BuildDerivation → NarFromPath"},
  {"path": "docs/src/phases/phase1b.md", "action": "MODIFY", "note": "mark build opcode tasks + integration test complete"}
]
```

## Design

**Three opcodes, one helper.** The three build opcodes differ in framing, not mechanics:

- `wopBuildDerivation` (36): single `BasicDerivation` + `BuildMode` → single `BuildResult`.
- `wopBuildPaths` (9): list of `DerivedPath` strings + `BuildMode` → void (success) or `STDERR_ERROR`.
- `wopBuildPathsWithResults` (46): list of `DerivedPath` strings + `BuildMode` → list of `KeyedBuildResult` (one per input).

All three call `build_via_local_daemon(drv, mode)`: spawn `nix-daemon --stdio`, `client_handshake`, `client_set_options`, `client_build_derivation`, read `BuildResult`, kill the daemon. The spawn uses `tokio::process::Command` with piped stdin/stdout; the subprocess's stderr is inherited to propagate daemon log messages.

`wopBuildPaths` and `wopBuildPathsWithResults` first resolve each `DerivedPath` string to a `BasicDerivation`: parse the `!`-separated path with P0004's `DerivedPath` parser, look up the `.drv` in the session cache (or store fallback), convert `Derivation` to `BasicDerivation` by dropping `inputDrvs`. Then call `build_via_local_daemon` once per entry. For `wopBuildPaths`, first failure aborts; for `wopBuildPathsWithResults`, failures become `BuildResult { status: MiscFailure, ... }` entries in the result list.

**Integration test.** `tests/integration_build.rs` uses a trivial shell derivation — `builtins.derivation { builder = "/bin/sh"; args = ["-c" "echo hello > $out"]; ... }`. The test starts the gateway on a port, runs `nix build --store ssh-ng://localhost:<port>`, and verifies the output exists. It exercises the full sequence: SSH handshake (P0002), `wopSetOptions` (P0001), `wopQueryValidPaths` (P0001), upload via `wopAddToStoreNar`/`wopAddMultipleToStore` (P0012), `wopQueryDerivationOutputMap` (P0012), `wopBuildDerivation` (this plan), `wopNarFromPath` (P0001) to download the result.

This version of the test only logged failures rather than asserting success — the build path wasn't yet working against real nix (P0015's opcode-number bug, P0016's `BuildResult` wire format, P0018's framed-stream bug were all still present). P0014 made the test assert.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.opcode.wopBuildDerivation`, `gw.opcode.wopBuildPaths`, `gw.opcode.wopBuildPathsWithResults`, `gw.daemon.spawn`.

## Outcome

Three build opcodes wired. `build_via_local_daemon` encapsulates the subprocess lifecycle. Integration test structure in place — not yet passing against real nix (multiple conformance bugs downstream of this plan) but passing against itself. Month 5 milestone ("`wopBuildDerivation` for a trivial derivation succeeds end-to-end") is structurally reached; Month 6 (`nix build .#hello`) needs P0015–P0018's conformance fixes before it greens.
