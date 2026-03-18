# Plan 0004: Design review remediation — DerivedPath, fuzz, JSON logs

## Context

After P0002 achieved end-to-end `nix path-info` and P0003 wired metrics, the implementation was audited against the design book (`docs/src/`, landed in `ffbbd8a`). Three review rounds found roughly twenty gaps between what was built and what the spec said should be built. This plan is the consolidated remediation: protocol correctness fixes, observability alignment, test coverage expansion, and the first net-new module since the scaffold (`DerivedPath`).

The phase 1a doc doesn't have a single task item for this — it's cross-cutting. "Make the metric names match the spec" isn't a feature, it's fixing drift. But several concrete deliverables are phase 1a task items that landed here: the Month 3 `nix store ls` milestone test, proptest coverage for strings/collections, and the fuzz targets.

## Commits

- `7986428` — fix: address Phase 1a design review gaps across protocol, observability, and testing
- `b1f7cf2` — fix: address Phase 1a design review gaps across protocol, observability, and testing
- `e180330` — docs: add protocol, observability, and design book guidance to CLAUDE.md
- `1e6f293` — fix: enforce type invariants, fix DerivedPath parsing, and harden protocol error handling
- `9d599e3` — docs: update phase1a.md to reflect completed implementation status

Five commits. `7986428` and `b1f7cf2` share a subject line but address different findings (14 gaps / 5-agent audit respectively). `e180330` is meta — it codifies the lessons into CLAUDE.md so future phases avoid the same categories of bugs.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/derived_path.rs", "action": "NEW", "note": "DerivedPath parser: path!* (all outputs) and path!out,dev (named outputs)"},
  {"path": "rio-nix/fuzz/fuzz_targets/wire_primitives.rs", "action": "NEW", "note": "cargo-fuzz target for wire read functions"},
  {"path": "rio-nix/fuzz/fuzz_targets/opcode_parsing.rs", "action": "NEW", "note": "cargo-fuzz target for opcode payload parsing"},
  {"path": "rio-nix/fuzz/Cargo.toml", "action": "NEW", "note": "fuzz workspace (excluded from main workspace, own lockfile)"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "NEW", "note": "structural golden tests (reworked entirely in P0005)"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "single canonical server_handshake_split (was two copies), InvalidMagic/VersionTooOld error types"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "STDERR_RESULT writer, StderrError::simple takes program name, #[instrument] on all writers"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "remove dead read_u32/write_u32, proptest for strings/string_pairs/full-UTF8"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "private fields + accessors, truncate_for_store_path returns Result (no panics)"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "private fields + accessors, FromStr impl"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "STDERR_ERROR on store failure (was silent EOF), wopQueryMissing respects store state, #[instrument] spans"},
  {"path": "rio-build/src/gateway/session.rs", "action": "MODIFY", "note": "connection gauge leak fix, abort task on channel close"},
  {"path": "rio-build/src/gateway/server.rs", "action": "MODIFY", "note": "auth_password explicit rejection, root span with component=gateway"},
  {"path": "rio-build/src/main.rs", "action": "MODIFY", "note": "default log format JSON (was pretty), FromStr for LogFormat CLI arg"},
  {"path": "rio-build/src/observability.rs", "action": "MODIFY", "note": "rio_gateway_ metric prefix (was gateway_)"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "stub-opcode tests, multi-opcode sequences, malformed wire input tests"},
  {"path": "rio-build/tests/protocol_conformance.rs", "action": "MODIFY", "note": "Month 3 nix-store-ls milestone test"},
  {"path": "docs/src/observability.md", "action": "MODIFY", "note": "sync emitted metrics with spec"}
]
```

## Design

The findings clustered into four categories:

**Protocol correctness.** `wopQueryMissing` was ignoring the store state entirely — returning the same result regardless of what paths existed. It also didn't parse `DerivedPath` format strings (`path!*` meaning "all outputs of this derivation", `path!out,dev` meaning "these specific outputs"). The new `derived_path.rs` module handles both opaque paths (bare store paths) and built paths (derivation + output selector). The `OutputSpec` enum distinguishes `All` from `Names(Vec<String>)`, with validation that `Names` is non-empty.

Store operation failures (`nar_from_path` returning `None` for a missing path) were silently closing the connection via `?` propagation instead of sending `STDERR_ERROR`. Every `?` in handler code that could bubble a store error was replaced with an explicit `match` that writes `STDERR_ERROR` first. This is the origin of the CLAUDE.md rule "Never use bare `?` to propagate errors from store operations."

The handshake had two implementations — one in `handshake.rs` for tests and one inlined in `session.rs` for the real server. They'd already drifted (the test one was missing the feature exchange). `1e6f293` consolidated to a single `server_handshake_split` that both call.

**Observability alignment.** `docs/src/observability.md` says all gateway metrics are prefixed `rio_gateway_`. P0003 used `gateway_`. Renamed. The root tracing span was supposed to carry `component="gateway"` per the structured-logging spec — it didn't. Added. Default log format was pretty-printed; spec says JSON in production. Flipped the default, kept `--log-format=pretty` as a CLI override.

**Type invariants.** `NixHash` and `StorePath` had public fields — callers could construct invalid values by mutation after parsing. Private fields, accessor methods, validation happens once at construction. `truncate_for_store_path` was using `assert_eq!` in library code — a length mismatch would panic the whole process. Changed to return `Result`.

**Test coverage.** Proptest roundtrips only covered `u64`/`bytes`/`bool`; extended to `string`/`strings`/`string_pairs` with full UTF-8 generators (not just ASCII). Twelve new byte-level tests: `STDERR_READ`/`STOP_ACTIVITY`/`RESULT` wire formats, `STDERR_ERROR` with position+traces, handshake error paths (`InvalidMagic`, `VersionTooOld`, non-zero CPU affinity). The Month 3 milestone test (`nix store ls --store ssh-ng://localhost`) verifies `wopNarFromPath` streams a valid NAR that the client can parse into a directory listing.

Two fuzz targets: `wire_primitives` feeds arbitrary bytes into `read_u64`/`read_bytes`/`read_string`/`read_strings` and asserts they either succeed or return an error — never panic, never allocate unbounded. `opcode_parsing` does the same for opcode payload readers. Both live in a `rio-nix/fuzz/` workspace excluded from the main workspace (own `Cargo.lock`, nightly-only).

The `golden_conformance.rs` file created here was a structural comparison — "does the response have the right number of frames in the right order" — not a byte-level comparison against a real daemon. P0005 replaced it entirely.

## Tracey

Predates tracey adoption. This is the plan where the CLAUDE.md guidance about tracey would have been written if tracey had existed; instead `e180330` codified the lessons into prose. Retro-tagged scope spans most of the gateway domain: `gw.opcode.*`, `gw.handshake.*`, `gw.wire.*`, `obs.*`.

## Outcome

104 tests total (up from 63). Month 3 milestone (`nix store ls`) green. `cargo fuzz run wire_primitives` runs without crashes. Metric names match `observability.md`. JSON logs by default. Every store-failure path sends `STDERR_ERROR` before closing. `DerivedPath` parsing handles both opaque and built-path formats. CLAUDE.md carries the distilled lessons so phases 2+ don't repeat the same gaps.
