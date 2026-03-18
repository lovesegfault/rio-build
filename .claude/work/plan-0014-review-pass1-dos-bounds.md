# Plan 0014: Review pass 1 — DoS bounds, STDERR_ERROR discipline, daemon timeout, proptests/fuzz, CA stubs

## Context

P0008–P0013 landed the entire phase 1b feature surface in a 34-minute burst (Feb 23 19:55–20:29). Code review started immediately after. This is the first of four review passes — same shape as P0004 in phase 1a, same categories of findings: unbounded allocations, silent error paths, missing test coverage. Eight commits over ~25 minutes (Feb 23 20:41–21:05).

## Commits

- `0b6b42f` — fix(rio-nix): enforce MAX_COLLECTION_COUNT on all wire collection loops
- `868eeb5` — fix(rio-build): cap nar_size allocation and guard against empty STDERR_READ chunks
- `1f954a6` — fix(rio-build): send STDERR_ERROR before closing connection on all error paths
- `ad19e31` — refactor(rio-build): consolidate daemon-spawn logic with timeout and fallible stdio
- `4992dca` — test(rio-nix): add proptests and fuzz targets for Phase 1b parsers
- `f05d0a8` — test(rio-build): add byte-level and error-path tests for Phase 1b opcodes
- `ac3517a` — test(rio-build): add golden conformance infrastructure for wopQueryDerivationOutputMap
- `08f56cb` — docs(rio-build): sync design docs with implementation and add CA opcode stubs

## Files

```json files
[
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "export MAX_COLLECTION_COUNT, MAX_STRING_LEN, MAX_FRAMED_TOTAL for sibling modules"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "bounds on read_basic_derivation output loop, read_build_result built-outputs loop; proptest roundtrip"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "bounds on STDERR trace/field loops"},
  {"path": "rio-nix/src/derivation.rs", "action": "MODIFY", "note": "proptest roundtrip for ATerm parse/serialize"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "cap nar_size at MAX_FRAMED_TOTAL; STDERR_ERROR on all error returns; build_via_local_daemon timeout + fallible stdio; CA opcode stubs (42, 43)"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "byte-level tests for opcodes 39/41/44/36/46, error-path tests"},
  {"path": "rio-build/tests/golden/mod.rs", "action": "MODIFY", "note": "builder/parser functions for opcode 41 conformance"},
  {"path": "rio-build/tests/integration_build.rs", "action": "MODIFY", "note": "assert success (was log-only)"},
  {"path": "rio-nix/fuzz/Cargo.toml", "action": "MODIFY", "note": "add derivation, nar, derived_path fuzz bins"},
  {"path": "rio-nix/fuzz/fuzz_targets/derivation_parsing.rs", "action": "NEW", "note": "fuzz ATerm parser"},
  {"path": "rio-nix/fuzz/fuzz_targets/nar_parsing.rs", "action": "NEW", "note": "fuzz NAR parser"},
  {"path": "rio-nix/fuzz/fuzz_targets/derived_path_parsing.rs", "action": "NEW", "note": "fuzz DerivedPath parser"},
  {"path": "rio-nix/fuzz/fuzz_targets/wire_primitives.rs", "action": "MODIFY", "note": "extend to framed stream"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "sync opcode table, clarify narHash hex encoding, CA opcode status"},
  {"path": "docs/src/crate-structure.md", "action": "MODIFY", "note": "actual module names (opcodes.rs, wire.rs, build.rs, client.rs)"}
]
```

## Design

**DoS bounds (`0b6b42f`, `868eeb5`).** P0000's `read_strings` and `read_string_pairs` enforced `MAX_COLLECTION_COUNT` before allocation. P0011's new loops in `build.rs` and `client.rs` — `read_basic_derivation` output loop, `read_build_result` built-outputs loop, STDERR trace readers — read a `u64` count from the wire and allocated a `Vec::with_capacity(count)` without checking. A malicious client could send `u64::MAX` to OOM the gateway. All collection-reading loops now check `count > MAX_COLLECTION_COUNT → WireError::CollectionTooLarge`. This is the origin of the CLAUDE.md rule "Every count-prefixed loop must enforce MAX_COLLECTION_COUNT before entering the loop."

`wopAddToStoreNar` read `nar_size` as u64 and did `Vec::with_capacity(nar_size as usize)`. Capped at `MAX_FRAMED_TOTAL` (1 GiB). Also: the STDERR_READ pull loop didn't handle zero-length chunks — a client responding with 0 bytes would loop forever with `remaining` never decreasing. Added a break on empty chunk.

**STDERR_ERROR discipline (`1f954a6`).** Every `?` in handler code that could propagate a store/NAR/ATerm error was replaced with explicit match + `STDERR_ERROR` write. Same lesson as P0004, applied to the new handlers. `wopBuildPathsWithResults` switched from abort-on-first-error to per-entry `BuildResult::failure` — batch opcodes should report per-entry failure, not abort.

**Daemon subprocess hardening (`ad19e31`).** `handle_build_derivation` duplicated `build_via_local_daemon`'s spawn+handshake+build sequence — now delegates. `daemon.stdin.take().unwrap()` → `.ok_or_else()` (can't panic the gateway on subprocess misbehavior). All daemon I/O wrapped in `tokio::time::timeout` (1h default, `RIO_DAEMON_TIMEOUT_SECS` env override) — a hung daemon can't hang the gateway forever. This is the origin of CLAUDE.md's "Local daemon subprocesses" section.

**Test expansion (`4992dca`, `f05d0a8`, `ac3517a`).** Proptest roundtrips for `Derivation` ATerm parse/serialize and `BuildResult`/`BasicDerivation` wire read/write. Three new fuzz targets covering the parsers added in P0008/P0009. Byte-level tests for all five new opcodes (construct raw wire bytes, feed to handler, assert result). Golden-conformance builder/parser functions for opcode 41 (opcode 39/44/36/46 need interactive STDERR_READ handling which the golden infra didn't support yet).

**CA opcode stubs (`08f56cb`).** `wopRegisterDrvOutput` (42) and `wopQueryRealisation` (43) are content-addressed-derivation opcodes. CA-aware Nix clients send them opportunistically after a build. Without stubs, the gateway returned STDERR_ERROR + closed connection — disconnecting the client mid-session. The stubs read the payload and return neutral results (void / empty set). This is the origin of CLAUDE.md's "Stub opcodes" section.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.safety.bounds`, `gw.error.stderr`, `gw.daemon.timeout`.

## Outcome

All collection-reading loops have bounds. All handler error paths send STDERR_ERROR. Daemon subprocess has a timeout. Proptests and fuzz targets cover phase 1b's parsers. CA opcodes don't disconnect the client. The integration test now asserts instead of logging — which meant it started failing, which is what drove P0015/P0016/P0018's conformance fixes.
