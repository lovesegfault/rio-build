# Plan 0071: Coverage-drive test suite expansion (mock-based)

## Design

With `coverage-html` live (P0069), the per-file lcov breakdown showed specific blind spots: `rio-worker/src/upload.rs` at 27% (only `scan_new_outputs` tested — the entire gRPC upload path had zero coverage), `rio-gateway/src/handler/build.rs::process_build_events` at 51% (only the three coarse outcomes tested, not the per-event match arms), `rio-store/src/backend/s3.rs` untested (no mock-S3 harness). Eleven commits, each targeting one red region.

**MockScheduler/MockStore extensions** (`8df9b21`): `MockSchedulerOutcome` gains `scripted_events: Option<Vec<BuildEvent>>`. When set, `submit_build` sends the events verbatim (auto-filling `build_id` and sequence) then closes the stream. This unlocks gateway tests for every `process_build_events` match arm (Log, Derivation lifecycle, Progress, Cancelled) — previously only three coarse bool-flag outcomes were possible. `MockStore` gains `fail_next_puts: AtomicU32` (decremented on each `put_path`, returns `Unavailable` while >0 — upload retry tests) and `fail_find_missing: AtomicBool` (scheduler cache-check error-path — cleaner than aborting the mock server and racing the RPC).

**Worker upload + input-fetch** (`ab8716c`): `upload_output`, `do_upload`, `upload_all_outputs` — the entire gRPC upload path — now tested via `MockStore`. Input-fetch via `MockStore` too.

**Gateway `process_build_events`** (`d1c19ae`): 7 new tests using `scripted_events`: Log lines → `STDERR_NEXT` frames (1:1), Derivation Started/Completed → `START/STOP_ACTIVITY` with matching ID, Derivation Failed stops activity AND emits failure log line, Cancelled returns `MiscFailure` with reason in error_msg, Progress/Cached/Queued emit zero stderr frames (silent-arm coverage), first-event-is-Completed short-circuit, first-event-is-Failed short-circuit. 41→48 tests.

**Poison-TTL expiry + DB fault injection** (`d56597d`): `POISON_TTL` `cfg(test)`-shadowed to 100ms (was 24h). `std::time::Instant` can't be faked; the only way to observe expiry is to actually wait. `test_tick_expires_poisoned_derivation`: poison, sleep 150ms, Tick, assert status reset to Created. DB fault-injection: `pool.close()` pattern (from `integration.rs`) to test that in-memory state transitions succeed even when DB writes fail — completion success with pool closed, transient failure with pool closed, 2-node chain newly-ready with pool closed. 11 new tests.

**Mock-S3 error distinction** (`f0dd9ce`): S3 is the primary production NAR backend. `s3.rs::get()` and `exists()` distinguish `NoSuchKey`/`NotFound` (→ `Ok(None)`/`Ok(false)`) from transient server errors (→ `Err`). A regression conflating them turns every store outage into **silent cache misses** instead of loud EIO. 5 tests via `aws-smithy-mocks`: NoSuchKey→None, 5xx→Err (NOT None), NotFound→false, 5xx→Err (NOT false), key-prefix handling. Removes `TODO(phase2b)` at `s3.rs:6`.

Remaining: worker `read_build_stderr_loop` via Cursor, rio-nix protocol client STDERR variants + drain error paths, gateway SSH key-loading helpers, scheduler `db.rs` direct-query coverage, store gRPC error-branch, rio-nix trait-impl + `dir_size` edge coverage.

## Files

```json files
[
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "MockSchedulerOutcome.scripted_events; MockStore.fail_next_puts, fail_find_missing"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "TESTED: upload_output, do_upload, upload_all_outputs via MockStore (was 27%)"},
  {"path": "rio-worker/src/executor/inputs.rs", "action": "MODIFY", "note": "TESTED: input-fetch via MockStore"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "TESTED: read_build_stderr_loop via Cursor"},
  {"path": "rio-worker/src/fuse/fetch.rs", "action": "MODIFY", "note": "TESTED: fetch error paths"},
  {"path": "rio-worker/Cargo.toml", "action": "MODIFY", "note": "test dev-deps"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "7 new scripted-events tests for process_build_events match arms"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "TESTED: SSH key-loading helpers"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "poison-TTL (cfg(test) 100ms shadow) + DB fault-injection suite (pool.close)"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "TESTED: direct-query coverage via TestDb"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "test hooks"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "error-branch coverage"},
  {"path": "rio-scheduler/src/state/mod.rs", "action": "MODIFY", "note": "POISON_TTL cfg(test) shadow"},
  {"path": "rio-store/src/backend/s3.rs", "action": "MODIFY", "note": "TESTED: NoSuchKey vs 5xx distinction via aws-smithy-mocks; removes TODO(phase2b)"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "error-branch coverage"},
  {"path": "rio-store/Cargo.toml", "action": "MODIFY", "note": "aws-smithy-mocks dev-dep"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "TESTED: STDERR variants + drain error paths"},
  {"path": "rio-nix/src/store_path.rs", "action": "MODIFY", "note": "TESTED: trait-impl edge coverage"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. These tests would retroactively carry `r[verify *]` markers for the behaviors they cover.

## Entry

- Depends on **P0070** (wire_bytes macros): tests use the macros.
- Soft dep on **P0059** (test-support): `MockStore`/`MockScheduler` being extended.

## Exit

Merged as `8df9b21..f0dd9ce` (11 commits). `.#ci` green at merge. Coverage (unit-test lcov) for the targeted files: `upload.rs` 27%→~85%, `handler/build.rs` 51%→~90%, `s3.rs` 0%→~75%.
