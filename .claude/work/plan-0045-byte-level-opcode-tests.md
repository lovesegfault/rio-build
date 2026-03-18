# Plan 0045: Byte-level opcode tests — wire_opcodes.rs TestHarness + Mock gRPC

## Context

P0005 (phase 1a) established live-daemon golden conformance — the gold standard. But golden tests are slow and coarse. This plan adds the *fast* byte-level layer: construct wire bytes directly, feed through the handler, assert the response bytes. No SSH, no real daemon, no real gRPC — in-process duplex stream + mock store/scheduler.

The `TestHarness` spawns `run_protocol` on a tokio duplex, performs handshake + `setOptions`, then the test writes opcode + payload and reads STDERR frames + result. `MockStore` pre-seedable with paths+NAR, records `PutPath` calls. `MockScheduler` has configurable outcomes per build.

18 tests in the first commit, expanded to 30+ over four follow-ups. Covers happy + error paths for store/build/stub opcodes.

**The crucial limitation** (learned the hard way in P0054): these tests verify the handler matches *our* understanding of the wire format. They don't verify *our understanding* matches Nix's. `test_add_multiple_to_store_batch` passed while the handler was reading the format wrong — because the test was written from the same wrong spec.

## Commits

- `86aad2a` — test(rio-gateway): add byte-level opcode tests for store and stub opcodes
- `8a8a7b7` — test(rio-gateway): add byte-level tests for build and misc opcodes
- `7f77415` — test(rio-gateway): add byte-level and error-path tests for opcodes 1/19/31/39/46
- `b61c43a` — test(rio-gateway): add MockScheduler close_stream_early, fix weak assertion
- `1a18340` — test(rio-gateway): add byte-level tests for wopAddToStore and parse_cam_str unit tests

(Discontinuous — commits 63, 64, 95, 126, 127.)

## Files

```json files
[
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "NEW", "note": "TestHarness (duplex + handshake + mock gRPC injection); MockStore (seedable, records PutPath); MockScheduler (configurable outcome, close_stream_early); drain_stderr_until_last / drain_stderr_expecting_error; 30+ opcode tests"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "parse_cam_str unit tests: text:sha256, fixed:r:sha256, fixed:git:sha1, fixed:sha256, rejects unknown method/algo"}
]
```

## Design

**Opcode coverage:** `IsValidPath` (1) exists+missing, `EnsurePath` (10) stub, `AddTempRoot` (11) stub, `SetOptions` (19) standalone, `QueryPathInfo` (26) exists+missing full wire format, `QueryPathFromHashPart` (29) found+not-found, `QueryValidPaths` (31) filters missing, `AddSignatures` (37) stub, `NarFromPath` (38) streams chunks + missing error, `AddToStoreNar` (39) valid + hex-narHash passthrough + invalid-path error + oversized error, `AddMultipleToStore` (44) batch upload, `BuildPathsWithResults` (46) invalid-DerivedPath per-entry-failure, `RegisterDrvOutput` (42) stub, `QueryRealisation` (43) stub, `wopAddToStore` (7) text-method + fixed-flat + invalid-cam-str, unknown opcode (99) STDERR_ERROR.

**Error-path convention documented:** block comment explaining which opcodes gracefully handle invalid input (return false/success — `IsValidPath`, `EnsurePath`, `AddTempRoot`, `QueryPathInfo`) vs. which error (`AddToStoreNar`, `NarFromPath`). Nix-compatible.

**Regression guard (`b61c43a`):** `MockSchedulerOutcome::close_stream_early` drops the stream sender after `BuildStarted` — simulates scheduler disconnect. Verifies opcode 9 produces exactly ONE `STDERR_ERROR` (from caller `handle_build_paths`), not two (would happen if P0050's fix for `process_build_events` double-send regressed).

**parse_cam_str:** parses `text:sha256` / `fixed:r:sha256` / `fixed:git:sha1` / `fixed:sha256` content-address method strings. Opcode 7 is the most complex handler (157 LOC, 6 error paths). These unit tests cover the parse branches; byte-level tests cover the full wire path.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Byte-level tests later gained `// r[verify gw.opcode.*]` markers when tracey was adopted — this file became a major source of `verify` coverage.

## Outcome

Merged as `86aad2a`, `8a8a7b7`, `7f77415`, `b61c43a`, `1a18340` (5 commits, discontinuous across 3 bursts). 30+ tests, all passing. Two of them were passing *wrongly* — P0054 corrects after the VM test exposes the wire-format bugs.
