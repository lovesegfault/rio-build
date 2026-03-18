# Plan 0103: Test infra consolidation — harnesses + manifest fuzz workspace

## Design

Seven commits consolidated test-support code and fixed the largest source of CI flakes: the 70-site `settle()` sleep pattern. Also: the first fuzz workspace for `rio-store`.

`ea8a8e4` deleted `settle()` entirely — a `tokio::time::sleep(50ms)` used at 70 call sites across `actor/tests/` to "let the actor process queued messages." All 70 were flaky under `cargo llvm-cov` (single-process test runner, shared PG via `OnceLock`, saturated on query execution → PG-stalled store → 30s+ timeouts → 50ms becomes a race). The insight: actor request-reply methods are already **true barriers**. The actor processes messages serially from one mpsc; a request-reply guarantees everything queued before it has been handled. Critically, `handle_merge_dag` calls `dispatch_ready()` **inline** before sending the reply — when `merge_single_node` returns, dispatch is done. 69 sites were delete-only (next line was already a barrier: `debug_query_*`, `query_status`, `merge_single_node`, `timeout(rx.recv())`); 1 site needed an explicit `barrier()` (a `logs_contain()` check with no actor-state read). Verification: 5× `cargo llvm-cov nextest -p rio-scheduler` → 5/5 pass, 181/181 tests, zero flakes.

`5953841` consolidated duplicate `DerivationNode` builders (5 copies across test files) and `spawn_grpc_server` helpers into `rio-test-support`. `aa9565c` unified the gateway's `TestHarness` into `GatewaySession`. `e707271` migrated hardcoded `/nix/store/...` literals across tests to a `test_store_path()` helper — 41 sites, each a potential divergence from the real store-path format. `4e8517f` added a `StoreSession` test harness with `Drop`-abort (the store server task is killed when the test scope ends, no zombie tasks). `884c285` added `recv_assignment` helper, `spawn_mock_store_with_client`, and per-crate `MIGRATOR` statics (tests in different crates no longer share a global migration lock).

`482e478` created `rio-store/fuzz/` as a new cargo workspace (own `Cargo.lock`, excluded from main workspace, mirrors `rio-nix/fuzz/`). First target: `manifest_deserialize` — `Manifest::deserialize` takes untrusted bytes from PostgreSQL BYTEA with bounds checks (`MAX_CHUNKS`, `ENTRY_SIZE` divisibility) that are exactly what fuzzing finds holes in. Verifies deserialize→serialize roundtrip for any input that parses, hunts panics for the rest. `flake.nix` refactored to support multiple fuzz workspaces via `mkFuzzWorkspace` — each builds independently with its own `cargoVendorDir`.

## Files

```json files
[
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "settle() DELETED; barrier() added with doc block; DerivationNode builders moved to rio-test-support"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "MODIFY", "note": "consolidated DerivationNode builders (5 copies merged); test_store_path() helper"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "generic spawn_grpc_server; spawn_mock_store_with_client; recv_assignment"},
  {"path": "rio-gateway/tests/common/mod.rs", "action": "MODIFY", "note": "TestHarness merged into GatewaySession"},
  {"path": "rio-store/tests/grpc/main.rs", "action": "MODIFY", "note": "StoreSession harness with Drop-abort"},
  {"path": "rio-store/fuzz/Cargo.toml", "action": "NEW", "note": "new fuzz workspace; own lockfile; excluded from main workspace"},
  {"path": "rio-store/fuzz/fuzz_targets/manifest_deserialize.rs", "action": "NEW", "note": "roundtrip + panic hunt"},
  {"path": "rio-store/fuzz/corpus/manifest_deserialize/seed-empty", "action": "NEW", "note": "0-byte seed"},
  {"path": "rio-store/fuzz/corpus/manifest_deserialize/seed-valid-small", "action": "NEW", "note": "1-entry seed matching serialize_format test"},
  {"path": "rio-store/src/manifest.rs", "action": "MODIFY", "note": "module made pub (was pub(crate)) for fuzz crate access"},
  {"path": "flake.nix", "action": "MODIFY", "note": "mkFuzzWorkspace: per-workspace cargoVendorDir; fuzzTargets list of {target, fuzzBuild, corpusRoot}"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). The fuzz target carries `r[verify store.manifest.deserialize]` via the retroactive sweep. The settle()→barrier migration has no spec marker — it's test-infrastructure hygiene.

## Entry

- Depends on P0102: `actor/tests/` was split into 7 topic modules by P0102; the settle() deletions happened across all 7 post-split files.

## Exit

Merged as `5953841..884c285` (7 commits). `.#ci` green at merge. Net −46 LOC from settle() deletion; 812 tests unchanged. 5× `cargo llvm-cov nextest -p rio-scheduler` → 5/5 pass (pre-fix: ~1/5 flaked). `rio-store/fuzz/` runs as `.#checks.x86_64-linux.fuzz-manifest_deserialize` (2min in `.#ci`).
