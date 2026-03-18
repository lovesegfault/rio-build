# Plan 0186: Rem-07 — STDERR_ERROR→STDERR_LAST desync (error() terminal, delete error+finish)

## Design

**HIGH (P1) — latent protocol corruption, currently masked by CppNix's discard-on-error pooling behavior.** Two DAG-validation failure paths in `rio-gateway/src/handler/build.rs` send `STDERR_ERROR` and then ALSO `STDERR_LAST` + a `BuildResult` payload. `STDERR_ERROR` is a terminal frame — confirmed against `libstore/worker-protocol-connection.cc:72`, real `nix-daemon`'s `stopWork()` sends `STDERR_ERROR` XOR `STDERR_LAST`, never both.

The client stops reading after `STDERR_ERROR` and throws, leaving `STDERR_LAST` + payload unconsumed in the TCP buffer. On the next opcode over a pooled connection, those stale bytes are read as the new opcode's response. **Self-incriminating:** the comment at `build.rs:160-164` already documented that `STDERR_ERROR` → `STDERR_LAST` is invalid. The code 350 lines down did the inverse.

Why latent: CppNix today discards the connection on any daemon error. That's one client-side optimization away from being false.

**Layer 2 (applied FIRST, gate-proved):** `StderrWriter` grows a separate `errored` flag. `error()` poisons the writer: `finish()`/`log()`/… return Err, `inner_mut()` panics. Two-flag design prevents `error()` → `inner_mut()` from silently handing out the raw writer for garbage bytes. Flag set after flush so a broken-pipe write doesn't poison.

**Layer 1:** delete the two `stderr.error()` calls at `build.rs:517`/`801`. The `BuildResult::failure` already carries the rejection reason; client sees identical text via `errorMsg` after a clean `STDERR_LAST`.

**Layer 3:** `r[gw.stderr.error-before-return]` made bidirectional (+2 bump via `tracey bump`).

Remediation doc: `docs/src/remediations/phase4a/07-stderr-error-desync.md` (611 lines).

## Files

```json files
[
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "delete stderr.error() at :517 and :801; BuildResult::failure already carries msg"},
  {"path": "rio-nix/src/protocol/stderr.rs", "action": "MODIFY", "note": "StderrWriter errored flag; error() poisons; finish()/log()/inner_mut() check"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "test_build_derivation_dag_reject_clean_stderr_last"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "verify tests"},
  {"path": "docs/src/components/gateway.md", "action": "MODIFY", "note": "r[gw.stderr.error-before-return] +2 (bidirectional)"}
]
```

## Tracey

- `r[impl gw.stderr.error-before-return+2]` ×2 — `0a4b3b4` (bumped rule)
- `r[verify gw.stderr.error-before-return+2]` ×6 — `0a4b3b4` (stderr.rs test + build.rs test + security.nix)

8 marker annotations (tracey-bumped rule `+2`).

## Entry

- Depends on P0148: phase 3b complete (extends 2c STDERR infrastructure)

## Exit

Merged as `dbdf5f4` (plan doc) + `0a4b3b4` (fix). `.#ci` green. `tracey bump` ran before commit; existing annotations at `handler/mod.rs:42`, `build.rs:673`, `stderr.rs:192` re-reviewed as compliant.
