# Plan 0005: Live-daemon golden conformance tests

## Context

P0004's `golden_conformance.rs` compared response *structure* — "four STDERR frames then a u64" — but not response *bytes*. That catches gross format errors but misses encoding bugs: send `sha256:nixbase32` where the real daemon sends raw hex, and structural tests still pass. This plan is the evolution to true byte-level conformance: start a real `nix-daemon`, send it the same bytes we send `rio-build`, compare responses field-by-field.

The approach went through one iteration. First attempt (`71601eb`): record daemon responses into binary fixture files, check the fixtures into git, compare against them at test time. Immediately replaced (`deff12d`) because fixture files go stale silently — if the fixture isn't found, the old code returned early instead of failing, so a missing fixture meant a silent pass. The live-daemon approach is slower (each test starts its own daemon) but can't silently drift.

The phase 1a doc lists this as "Live-daemon golden conformance tests" under completed tasks — with the note that it "replaced stored binary fixtures to eliminate fixture staleness" and "fixed three conformance bugs found through golden testing." Those three bugs are the payoff of this plan.

## Commits

- `71601eb` — feat: add true golden conformance tests with recorded nix-daemon fixtures
- `1b34725` — docs: mark golden conformance tests as completed in phase1a
- `deff12d` — refactor: replace golden fixture files with live-daemon conformance tests
- `af318df` — docs: update verification, contributing, and CLAUDE.md for live-daemon golden tests
- `a241c72` — test: expand golden conformance coverage for NarFromPath, stub opcodes, and multi-chunk streaming
- `1d952aa` — refactor: harden golden conformance test infrastructure

Six commits, contiguous.

## Files

```json files
[
  {"path": "rio-build/tests/golden/daemon.rs", "action": "NEW", "note": "isolated nix-daemon lifecycle: temp socket, spawn, exchange, kill"},
  {"path": "rio-build/tests/golden/mod.rs", "action": "NEW", "note": "shared helpers: STDERR activity stripping, protocol-aware response reader"},
  {"path": "rio-build/tests/golden/record.rs", "action": "NEW", "note": "fixture recorder (obsoleted within this plan, kept for debugging)"},
  {"path": "rio-build/tests/golden_conformance.rs", "action": "MODIFY", "note": "8 scenarios: handshake, IsValidPath found/not-found, QueryPathInfo found/not-found, QueryValidPaths, AddTempRoot, QueryMissing"},
  {"path": "rio-build/tests/direct_protocol.rs", "action": "MODIFY", "note": "multi-chunk NarFromPath (>64KB), connection-stays-open-after-STDERR_ERROR"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "3 conformance fixes: SetOptions no-trailing-u64, QueryPathInfo raw-hex narHash, QueryMissing opaque→unknown"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "expose handshake bytes for golden comparison"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "document ~~intentional~~ [WRONG — bug #11] NarFromPath wire divergence"}
]
```

## Design

The test harness: each test calls `golden::daemon::spawn()` to get an isolated `nix-daemon` on a temp unix socket. The test then constructs a byte sequence (handshake + opcode + payload), sends it to *both* the daemon socket and a `rio-build` `DuplexStream` session, reads both responses, and asserts field-by-field equality after stripping STDERR activity frames.

The STDERR stripping is the subtle part. `nix-daemon` sends `STDERR_START_ACTIVITY` / `STDERR_STOP_ACTIVITY` frames that `rio-build` doesn't (they're progress-bar noise, not semantic content). The first stripping implementation was naive byte-scanning — look for the `STDERR_START_ACTIVITY` magic and skip forward. `1d952aa` replaced it with structured parsing: actually decode each STDERR frame, drop activity frames, keep the rest. This also validates that skipped frames are well-formed (a malformed activity frame is a test failure, not something to silently skip).

The three conformance bugs found:

**`wopSetOptions` spurious trailing value.** `rio-build` was sending `STDERR_LAST` then `u64(1)` as a "success" result. Real daemon sends only `STDERR_LAST` — SetOptions has no result value. The `u64(1)` was invented by the P0001 implementation and never caught because integration tests (`nix store info`) happen to not send SetOptions on a path that would expose the desync.

**`wopQueryPathInfo` narHash wire encoding.** The P0000 `PathInfo` serialization sent `narHash` as `sha256:nixbase32...` (the colon format used in `.narinfo` text files). Real daemon sends raw hex digest — 64 hex chars, no algorithm prefix. This is the origin of the CLAUDE.md "Wire encoding conventions" section: the wire format and the text format differ, and you can't tell which one a field uses without checking the real daemon.

**`wopQueryMissing` opaque-path categorization.** Given a nonexistent opaque path (bare store path, not `path!outputs`), `rio-build` was putting it in the `will_build` set. Real daemon puts it in `unknown` — only `Built` derived paths (derivations whose outputs are being requested) go in `will_build`. The distinction matters for `nix build --dry-run` output.

~~One intentional divergence documented: `wopNarFromPath` streaming. `nix-daemon` sends raw NAR bytes after `STDERR_LAST` (the client reads until EOF or known NAR length). `rio-build` sends `STDERR_WRITE` chunks instead. Both work — `nix store ls` parses either — but the bytes differ. `a241c72` discovered this, `1d952aa` documented it in `gateway.md` as deliberate (the `STDERR_WRITE` framing is more robust against partial reads).~~

> **[WRONG — bug #11, fixed 132de90b in phase 2a]:** Nix client's `processStderr()` passes no sink → `error: no sink`. STDERR_WRITE chunks do NOT work for NarFromPath; the real daemon's raw-bytes-after-STDERR_LAST is required. See `gateway.md:76` Historical note.

Eight golden scenarios landed: handshake, `IsValidPath` (found and not-found), `QueryPathInfo` (found and not-found), `QueryValidPaths`, `AddTempRoot`, `QueryMissing`. Plus live-daemon tests for `NarFromPath`, `QueryPathFromHashPart`, and `AddSignatures` (stub) in `a241c72`.

Test infrastructure hardening in `1d952aa`: shared daemon instance across parallel tests (was per-test spawn), protocol-aware response reading (was timeout-on-idle — flaky under load), leftover-byte validation (a response with trailing garbage is a test failure), and fixing an impure `nixpkgs` reference in `build_test_path` (was reading the host's `<nixpkgs>`, making tests non-reproducible).

## Tracey

Predates tracey adoption. Retro-tagged scope: `tracey query rule gw.opcode.wopSetOptions` (the trailing-value fix), `gw.opcode.wopQueryPathInfo` (the hex-encoding fix), `gw.opcode.wopQueryMissing` (the categorization fix). The golden test harness itself would fall under `gw.conformance.*` if such markers existed.

## Outcome

8 golden scenarios + 3 additional live-daemon tests. Three conformance bugs fixed — each one would have caused subtle client-side failures that integration tests alone wouldn't catch. The fixture-file approach is dead; every golden test starts a fresh daemon. `docs/src/components/gateway.md` documents the one ~~intentional~~ [WRONG — bug #11, fixed 132de90b] wire-format divergence.
