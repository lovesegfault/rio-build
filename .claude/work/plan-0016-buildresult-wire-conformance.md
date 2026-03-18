# Plan 0016: BuildResult wire format conformance (u64 integers, cpuUser/cpuSystem, Realisation JSON)

## Context

With P0015 fixing the upload path, the integration test reached the build phase: `wopBuildDerivation` sent to gateway, gateway forwarded to local `nix-daemon --stdio`, daemon built, daemon sent `BuildResult` back. The gateway's `read_build_result` (P0011) then crashed with a wire error — the byte stream didn't match what it expected.

Three distinct format mismatches, fixed across three commits with some back-and-forth. The commit sequence is a minor saga: the first fix changed `BuildStatus` from u64 to u8 (wrong — Nix uses u64 for everything on the wire), the second reverted to u64, the third switched the enum to `repr(u64)` to eliminate a silent truncation on the write side. This is the distilled "Nix wire protocol is u64-everywhere" lesson.

## Commits

- `1128b71` — fix(rio-nix): correct BuildResult wire format for protocol 1.37+
- `b6b5a25` — fix(rio-nix): use u64 for all wire integers in BuildResult (Nix convention)
- `eaae0e5` — fix(rio-nix): use repr(u64) for BuildStatus and remove silent truncation

Three commits. `1128b71` found two of the three real bugs but introduced a new one; the next two commits converged on correct.

## Files

```json files
[
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "BuildStatus repr(u64); cpuUser/cpuSystem optional fields; builtOutputs as DrvOutput + Realisation JSON; new enum values (InputRejected, OutputRejected, ResolvesToAlreadyValid)"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "read_u8/write_u8 added (then deprecated with doc warnings — not used on Nix wire)"},
  {"path": "rio-nix/Cargo.toml", "action": "MODIFY", "note": "add serde_json for Realisation parsing"},
  {"path": "rio-build/src/gateway/handler.rs", "action": "MODIFY", "note": "KeyedBuildResult: include DerivedPath key in wopBuildPathsWithResults response; populate builtOutputs from derivation when AlreadyValid"},
  {"path": "rio-build/tests/integration_build.rs", "action": "MODIFY", "note": "iteration on assertion as format converged"}
]
```

## Design

**The three real bugs, in order of impact:**

**Missing `cpuUser`/`cpuSystem`.** Protocol ≥1.37 includes two optional fields between `stopTime` and `builtOutputs`: a `u8` tag (0 = absent, 1 = present) followed by an `i64` value if present. P0011's `BuildResult` skipped straight from `stopTime` to `builtOutputs`, so `read_build_result` read the `cpuUser` tag byte as the high byte of the `builtOutputs` count — stream desynchronized from there.

**Wrong `builtOutputs` encoding.** P0011 serialized as `(name, path, hashAlgo, hash)` tuples. The real format is `DrvOutput` string + `Realisation` JSON object. `DrvOutput` is `<drv-hash>!<output-name>`; `Realisation` is `{"id": "...", "outPath": "...", "signatures": [...], "dependentRealisations": {...}}`. The `outPath` field is a basename (no `/nix/store/` prefix), not a full path — another small-print detail that's invisible in docs and obvious in a packet capture.

**`BuildStatus` integer width.** `1128b71` changed `BuildStatus` from u64 to u8 after seeing a 1-byte `cpuUser` tag in the stream and misattributing it. `b6b5a25` reverted to u64 after discovering the Nix wire convention: all integers are u64, full stop. `eaae0e5` finished the job by making the enum `repr(u64)` — previously `as u8` on the write side silently truncated any variant >255 (hypothetical, but dangerous).

**Incidental fixes in `1128b71`.** Three `BuildStatus` enum values were missing (`InputRejected`, `OutputRejected`, `ResolvesToAlreadyValid` — added in Nix 2.31). `wopBuildPathsWithResults` was sending bare `BuildResult` entries; the real format is `KeyedBuildResult` — `DerivedPath` key + `BuildResult` value. And when a build returns `AlreadyValid` (output already in store), `builtOutputs` arrives empty from the daemon; the gateway now populates it from the derivation so the client gets the output paths.

## Tracey

Predates tracey adoption. No `r[impl ...]` at `phase-1b` tag. Retro-tagged scope: `tracey query rule gw.build.result.wire`, `gw.build.realisation`.

## Outcome

`read_build_result` parses real daemon output. Integration test's build phase now completes — the `BuildResult` from `nix-daemon` round-trips through the gateway to the client. Month 5 milestone ("`wopBuildDerivation` for a trivial derivation succeeds end-to-end") green. One more conformance bug remained in the upload path (P0018) — `AddToStoreNar` was using the wrong protocol mechanism entirely — but it was masked by `wopAddToStore` (P0015) handling uploads for the test derivation.
