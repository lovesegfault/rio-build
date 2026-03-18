# Plan 0074: Test unwrap→Result sweep + llvm-cov nextest

## Design

528 tests, ~1100 `.unwrap()` calls. A test that panics on unwrap loses the error message ("called `Option::unwrap()` on a `None` value" tells you nothing). `anyhow::Result<()>` + `?` preserves the error chain. This sweep converts every test across all 8 crates. Mechanical but wide: one commit per crate to keep diffs reviewable.

Foundation (`e418019`, `24c2e74`): `anyhow` as dev-dep to `rio-nix` and `rio-test-support` (the only crates missing it). `rio-test-support` gets `TestResult` type alias + `Context` re-export. `spawn_mock_store`/`spawn_mock_scheduler` return `Result` instead of panicking on bind failure. `fixtures::make_nar` stays `expect` — `io::Write` for `Vec<u8>` is infallible, rippling `Result` through 32 call sites for an impossible error isn't worth it.

Per-crate conversion: tests that only use `.unwrap_err()` (intentional error assertion) or no unwrap at all stay returning `()`. Tests with assertions on `Result::Ok` convert. The `wire_bytes!`/`wire_send!` macros (P0070) get `?` support internally.

**`llvm-cov` nextest switch** (`c59c5e1`): a real flake root-caused. `cargo test` runs all tests in one process with `--test-threads=NCPU` (~56 on nixbuild.net). Scheduler tests share ONE ephemeral PG via `static PG: OnceLock`. Under 56-way parallelism + llvm-cov instrumentation overhead, that PG saturates on **query execution** (not connection count — `max_connections=500` was already tuned). `test_scheduler_cache_check_skips_build` does a gRPC `FindMissingPaths` call inside the actor's serial loop with `DEFAULT_GRPC_TIMEOUT=30s`. The in-process store's handler queries the saturated PG → stalls → 30s → timeout → `check_cached_outputs` returns empty → derivation stays `Created` → assertion fails. Different test each run, passes on retry. Nextest isolates each test in its own process — no shared `OnceLock`, no saturation. `craneLib.cargoLlvmCov` with `cargoExtraArgs = "nextest"`.

**TODO audit** (`f1b4d28`): the T4/T5 comments at `grpc/mod.rs:433,438` were `TODO(phase2b)` but are the headline phase-2b features (LogBatch/Progress forward) — they're *the work*, not deferred cleanup. Re-tagged as `PHASE 2B FEATURE ANCHOR` so `rg 'TODO\(phase2b\)'` comes up clean while still marking the integration point. Final audit: 0 `TODO(phase2b)`, 0 untagged `TODO`. `ci-fast` PASS (all three VM tests green).

## Files

```json files
[
  {"path": "rio-nix/Cargo.toml", "action": "MODIFY", "note": "anyhow dev-dep"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "anyhow prod-dep (publish=false anyway)"},
  {"path": "rio-test-support/src/lib.rs", "action": "MODIFY", "note": "TestResult alias + Context re-export"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "spawn_mock_* return Result"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "make_nar: unwrap→expect (Vec<u8> Write is infallible)"},
  {"path": "rio-test-support/src/wire.rs", "action": "MODIFY", "note": "macros use ?"},
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-common/src/task.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-proto/src/validated.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-store/src/metadata.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/grpc/tests.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/dag/tests.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "TODO(phase2b) → PHASE 2B FEATURE ANCHOR comments"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-worker/src/executor/daemon.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "tests return Result, macros use ?"},
  {"path": "rio-gateway/src/server.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/common/mod.rs", "action": "MODIFY", "note": "Result helpers"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/wire_opcodes/build.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/wire_opcodes/main.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/wire_opcodes/misc.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_read.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-gateway/tests/wire_opcodes/opcodes_write.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/derivation/aterm.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/derivation/hash.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/derivation/mod.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/nar.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "tests return Result"},
  {"path": "flake.nix", "action": "MODIFY", "note": "craneLib.cargoLlvmCov cargoExtraArgs = nextest (flake root-cause fix)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0071** (coverage drive): many of the tests being converted were just added.
- Soft dep on **P0069** (coverage-html): the llvm-cov infrastructure being fixed.

## Exit

Merged as `f1b4d28..74034f3` (12 commits). `.#ci` green at merge. `rg 'TODO\(phase2b\)' --type rust` → 0. `rg 'TODO[^(]' --type rust` → 0 (CLAUDE.md example only). `ci-fast` PASS (vm-phase1a, vm-phase1b, vm-phase2a green).
