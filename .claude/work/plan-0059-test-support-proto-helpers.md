# Plan 0059: Test-support modularization + gRPC client helpers

## Design

Two tightly-coupled extractions that together eliminate ~600 LOC of cross-crate duplication before the monolith deletion (P0060).

**`rio-test-support` modularization** (`6b90a54`): the crate was a 425-line `lib.rs` (all PG bootstrap) plus duplicated helpers scattered across `rio-gateway/tests/`. Split into four modules: `pg.rs` (PG bootstrap, verbatim move with `TestDb` re-exported at crate root for backward compat), `wire.rs` (`do_handshake` extracted from 3 copies, `send_set_options` from 2, `drain_stderr_{until_last,expecting_error}`), `grpc.rs` (`MockStore` and `MockScheduler` unified — the `wire_opcodes.rs` and `integration_distributed.rs` copies had diverged in subtle ways), `fixtures.rs` (NAR builders, path generators). Second commit (`3e24103`) adopts these across all test files and deletes the originals.

**`rio-proto::client` helpers** (`8af5fbc`): five sites repeated `format!("http://{}") + Channel::from_shared + connect + max_message_size` — collapsed into `connect_{store,scheduler,worker}()`. Four sites repeated the `GetPath` stream-drain loop — collapsed into `collect_nar_stream()` (returns `(Option<PathInfo>, Vec<u8>)` with size limit). `chunk_nar_for_put()` provides a lazy `Arc<[u8]>`-backed `PutPath` chunk stream, addressing a `TODO(phase2b)` in `upload.rs` about eager-Vec 8 GiB peak memory (the real fix comes in P0080; this is the first step). `NAR_CHUNK_SIZE` unified at 256 KiB (was 64K in gateway, 256K in worker). Also adds `rio-common::with_timeout_status` — preserves `tonic::Status` for `NotFound` branching, which the existing `with_timeout` loses by going through `anyhow`.

Adoption removed ~40 LOC from `gateway/main.rs` + `worker/main.rs` (connect boilerplate), deleted `scheduler/main.rs`'s local `connect_store` fn, and replaced 4 inline NAR-drain loops. `worker/upload.rs` also gained a timeout where it was previously unbounded — a real bug fix bundled into the refactor.

## Files

```json files
[
  {"path": "rio-test-support/src/lib.rs", "action": "MODIFY", "note": "was 425-line PG monolith; now re-exports from submodules"},
  {"path": "rio-test-support/src/pg.rs", "action": "NEW", "note": "PG bootstrap (verbatim move, TestDb)"},
  {"path": "rio-test-support/src/wire.rs", "action": "NEW", "note": "do_handshake, send_set_options, drain_stderr_*"},
  {"path": "rio-test-support/src/grpc.rs", "action": "NEW", "note": "unified MockStore + MockScheduler"},
  {"path": "rio-test-support/src/fixtures.rs", "action": "NEW", "note": "NAR builders, path generators"},
  {"path": "rio-test-support/Cargo.toml", "action": "MODIFY", "note": "module deps"},
  {"path": "rio-proto/src/client.rs", "action": "NEW", "note": "connect_*, collect_nar_stream, chunk_nar_for_put, NAR_CHUNK_SIZE"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "mod client"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "client helper deps"},
  {"path": "rio-common/src/grpc.rs", "action": "MODIFY", "note": "with_timeout_status (preserves tonic::Status)"},
  {"path": "rio-common/Cargo.toml", "action": "MODIFY", "note": "grpc helper deps"},
  {"path": "rio-gateway/src/handler.rs", "action": "MODIFY", "note": "adopt collect_nar_stream (2x), chunk_nar_for_put, with_timeout*"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "adopt connect_* helpers (~40 LOC removed)"},
  {"path": "rio-gateway/tests/wire_opcodes.rs", "action": "MODIFY", "note": "adopt test-support helpers"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "adopt unified MockStore/MockScheduler"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "delete local connect_store fn"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "adopt test-support helpers"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "adopt connect_* helpers"},
  {"path": "rio-worker/src/executor.rs", "action": "MODIFY", "note": "adopt collect_nar_stream"},
  {"path": "rio-worker/src/fuse/mod.rs", "action": "MODIFY", "note": "adopt collect_nar_stream"},
  {"path": "rio-worker/src/upload.rs", "action": "MODIFY", "note": "adopt chunk_nar_for_put + with_timeout_status (was unbounded — timeout gap fixed)"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`.

## Entry

- Depends on **P0056** (phase-2a terminal): `MockStore`/`MockScheduler` and all adopted sites are 2a artifacts.

## Exit

Merged as `6b90a54..8af5fbc` (3 commits). `.#ci` green at merge.
