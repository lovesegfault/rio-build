# Plan 0065: Dead-code removal + helper-extraction pass

## Design

After the big module splits (P0062–P0064), a sweep through the codebase: remove what the splits made dead, extract what they made visibly duplicated. Fifteen commits, each tiny, collectively bringing the workspace to a steady state where the feature bulk (P0075+) lands on clean ground.

**Dead code** (4 commits): workspace-wide unused dependencies, stale `#[allow]` annotations that no longer suppress anything, dead parameters and functions that became unreferenced after the splits, a stale doc reference to `build_via_local_daemon` (renamed in phase-2a).

**gRPC helpers** (3 commits):
- `rio-common::check_bound` — the `if count > MAX { return Err(Status::invalid_argument(...)) }` pattern appeared at 7 gRPC ingress points.
- `rio-scheduler::grpc::{send_and_await, bridge_build_events, parse_build_id}` — `send_and_await` collapses 4 copies of `actor.send().await? + reply_rx.await??` (8 LOC each). `bridge_build_events` collapses 2 copies of the broadcast→mpsc bridge with `Lagged→DATA_LOSS` handling (25 LOC each — before extraction, `SubmitBuild` and `WatchBuild` had slightly different warning formats for the same condition). `grpc.rs`: 921→838 LOC.
- `rio-proto::client::{query_path_info_opt, get_path_nar}` — `QueryPathInfo` + timeout + `NotFound→None` in one call. Gateway's `grpc_query_path_info`: 20→5 LOC; `grpc_get_path`: 30→10 LOC. Also deleted `make_proto_path_info` (a 9-arg wrapper that added nothing — field names were identical to the struct literal). Worker `fetch_drv_from_store`: 47→34 LOC; `fetch_and_extract` fetch block: 62→37 LOC.

**FUSE lock-poison recovery** (`cfaaf73`): 11 inline `.unwrap_or_else(|e| e.into_inner())` patterns (some with 3-line `tracing::error!` bodies) replaced with `inodes_read`/`inodes_write`/`open_files_read`/`open_files_write`/`backing_state_write` helper methods on `NixStoreFs`. Poison recovery is safe for pure-data state (inode maps, file handles) — worst case is a stale inode or leaked fd until next `forget`/`release`. `cache.rs`'s `InflightEntry` locks left inline (different struct, only 4 sites, Condvar semantics).

**Struct extractions** (`4629db2`): `MergeDagRequest` (wraps 7 non-reply `ActorCommand::MergeDag` fields, removes an `#[allow(too_many_arguments)]`). `DerivationRow` (replaces a 6-tuple `(String, String, Option<String>, String, DerivationStatus, Vec<String>)` in `batch_upsert_derivations` — 4 positional Strings with opaque semantics, now named fields).

**Test harnesses** (2 commits): `GatewaySession` in `tests/common/mod.rs` (spawns mocks, connects, `DuplexStream`, runs `run_protocol` with EOF-as-clean-shutdown, `Drop` aborts task handles). `integration_distributed.rs`: 502→316 LOC. Scheduler test helpers: `merge_dag`/`query_status`/`complete_*`.

**Idioms** (3 commits): `static str` error fields where the value is a known constant (avoids `String` allocation in error-path hot loops). `write_strings` generalized to `impl IntoIterator<Item = impl AsRef<str>>`. `let-else` adoption. `&mut SessionContext` for build handlers instead of destructuring.

## Files

```json files
[
  {"path": "Cargo.toml", "action": "MODIFY", "note": "remove unused workspace deps"},
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "check_bound helper"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "dep hygiene"},
  {"path": "rio-proto/src/client.rs", "action": "MODIFY", "note": "query_path_info_opt, get_path_nar, NarCollectError::is_not_found()"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "dep hygiene"},
  {"path": "rio-scheduler/src/grpc.rs", "action": "MODIFY", "note": "send_and_await, bridge_build_events, parse_build_id; MergeDagRequest adoption; 921→838 LOC"},
  {"path": "rio-scheduler/src/actor/build.rs", "action": "MODIFY", "note": "adopt send_and_await"},
  {"path": "rio-scheduler/src/actor/merge.rs", "action": "MODIFY", "note": "MergeDagRequest; .get_or_insert_with()"},
  {"path": "rio-scheduler/src/db.rs", "action": "MODIFY", "note": "DerivationRow named struct (was 6-tuple)"},
  {"path": "rio-gateway/src/handler/mod.rs", "action": "MODIFY", "note": "&mut SessionContext to build handlers"},
  {"path": "rio-gateway/src/handler/grpc.rs", "action": "MODIFY", "note": "grpc_query_path_info 20→5; grpc_get_path 30→10; delete make_proto_path_info"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "&mut SessionContext; let-else"},
  {"path": "rio-gateway/src/handler/opcodes_read.rs", "action": "MODIFY", "note": "let-else"},
  {"path": "rio-gateway/src/handler/opcodes_write.rs", "action": "MODIFY", "note": "remove make_proto_path_info callers"},
  {"path": "rio-gateway/src/translate.rs", "action": "MODIFY", "note": "stale allow removal"},
  {"path": "rio-gateway/tests/common/mod.rs", "action": "NEW", "note": "GatewaySession harness (spawn+connect+DuplexStream+Drop)"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "adopt GatewaySession; 502→316 LOC"},
  {"path": "rio-gateway/tests/golden_conformance.rs", "action": "MODIFY", "note": "adopt helpers"},
  {"path": "rio-gateway/tests/golden/daemon.rs", "action": "MODIFY", "note": "stale allow removal"},
  {"path": "rio-gateway/tests/golden/mod.rs", "action": "MODIFY", "note": "stale allow removal"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "MODIFY", "note": "adopt helpers"},
  {"path": "rio-gateway/Cargo.toml", "action": "MODIFY", "note": "dep hygiene"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "inodes_*/open_files_*/backing_state_* lock-poison helpers (11 sites)"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "fetch_drv_from_store 47→34; adopt query_path_info_opt"},
  {"path": "rio-nix/src/hash.rs", "action": "MODIFY", "note": "static str error fields"},
  {"path": "rio-nix/src/narinfo.rs", "action": "MODIFY", "note": "static str error fields"},
  {"path": "rio-nix/src/protocol/build.rs", "action": "MODIFY", "note": "static str error fields"},
  {"path": "rio-nix/src/protocol/client.rs", "action": "MODIFY", "note": "dead code removal"},
  {"path": "rio-nix/src/protocol/handshake.rs", "action": "MODIFY", "note": "stale allow removal"},
  {"path": "rio-nix/src/protocol/wire.rs", "action": "MODIFY", "note": "write_strings → impl IntoIterator<Item=impl AsRef<str>>"},
  {"path": "rio-scheduler/src/actor/tests/helpers.rs", "action": "MODIFY", "note": "merge_dag/query_status/complete_* helpers"},
  {"path": "rio-scheduler/src/actor/tests/coverage.rs", "action": "MODIFY", "note": "adopt helpers"},
  {"path": "rio-scheduler/src/actor/tests/integration.rs", "action": "MODIFY", "note": "adopt helpers"},
  {"path": "rio-scheduler/src/actor/tests/wiring.rs", "action": "MODIFY", "note": "adopt helpers"},
  {"path": "rio-test-support/src/grpc.rs", "action": "MODIFY", "note": "type StoredPath alias (removes #[allow(type_complexity)])"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0063** (gateway handler split): extracts helpers from `handler/grpc.rs`, `handler/build.rs`.
- Depends on **P0064** (newtype follow-through): `actor/tests/` submodules were created in P0064.

## Exit

Merged as `177a0cd..f96b303` (15 commits). `.#ci` green at merge. Net: workspace -~400 LOC, 4 `#[allow(clippy::*)]` annotations removed, zero new tests (pure refactor).
