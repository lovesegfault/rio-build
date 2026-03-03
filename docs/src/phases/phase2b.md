# Phase 2b: Observability + Packaging (Months 7-9)

**Goal:** Production-quality observability, container images, and integration testing.

**Implements:** [Observability](../observability.md), [Configuration](../configuration.md)

## Tasks

- [ ] Build log streaming with batching and rate limiting (worker → scheduler → gateway)
  - Worker buffers up to 64 lines or 100ms per batch
  - Scheduler maintains per-derivation ring buffer for active log serving
  - Async flush to S3 on derivation completion
  - Log rate limiting per build (`log_rate_limit`, `log_size_limit`)
  - [x] Honor 100ms BATCH_TIMEOUT during silent build periods (spawn reader into owned task, select! on channel + interval; naive timeout-wrap is cancel-unsafe) — completed in pre-bulk cleanup (C4)
- [ ] Build correlation IDs (UUID v7): generated at `SubmitBuild`, propagated via gRPC metadata, included in all log spans
- [ ] `tracing-opentelemetry`: trace propagation across gRPC boundaries (export to stdout or local Jaeger in dev)
- [ ] Container images for each component (Nix-based via `dockerTools.buildLayeredImage`)
- [ ] Configuration management (TOML config + env var overlay with `RIO_` prefix)
- [ ] `cargo-deny` integration: license auditing (deny GPL-3.0), security advisory checking
- [ ] Integration test: multi-derivation build (A → B → C) across 3+ workers
- [ ] Integration test: cache hit path (second build is instant)
- [ ] gRPC contract tests for each service boundary
- [x] Add `rio-proto/src/validated.rs` with `ValidatedPathInfo { store_path: StorePath, nar_hash: [u8; 32], ... }` + `TryFrom<PathInfo>`. Migrate gRPC handlers and `NarinfoRow::into_path_info` (DB egress, currently no validation) to validated types — completed in pre-bulk cleanup (C5+C6)
- [ ] Lazy NAR upload streaming: replace eager `Vec<PutPathRequest>` (4GiB NAR → ~8GiB peak, ×4 parallel = 32GiB) with `stream::unfold` or `Arc<[u8]>`-based lazy chunk iterator
- [x] Track leaked overlay mounts across worker lifetime; escalate to infrastructure failure after N leaks (currently metric only) — completed in pre-bulk cleanup (C3)

## Post-Phase-2a Cleanup

- [x] Delete the `rio-build/` crate (phase-1 monolith, now dead code). Port unique tests first:
  - Golden conformance tests (`rio-build/tests/golden_conformance.rs`, 18 tests against real nix-daemon) → `rio-gateway/tests/`
  - Error-path tests from `direct_protocol.rs` not covered by `wire_opcodes.rs` (hash mismatch, unparseable paths, batch-continue-on-error)
- [x] Adopt `rio_common::grpc::with_timeout()` across 33 inline `tokio::time::timeout` sites (exists but unused)
- [x] Extract gRPC client connect helper to `rio-proto` (16 sites repeat `format!("http://{}") + connect + max_message_size`)
- [x] Extract test helpers to `rio-test-support`: `client_handshake()` (3×), `send_set_options()` (2×), mock gRPC services (~400 LOC duplicated in rio-gateway tests), `spawn_grpc_server()` (7×)
- [x] Parallelize worker input fetches (`fetch_input_metadata`, `compute_input_closure`, inputDrv resolution) via `buffer_unordered` — currently serial, adds 3-10s per build
- [x] Batch DB writes in scheduler `persist_merge_to_db` — currently 2N+E serial PG roundtrips, stalls actor ~3.5s for 1000-node DAG
- [x] Extract NAR-stream collect/chunk helpers (`collect_nar_stream`, `chunk_nar_for_put`) — duplicated 4× across gateway/worker/fuse
- [x] Split `rio-scheduler/src/actor.rs` (4760 lines) into `actor/{merge,completion,dispatch,worker,build,tests}.rs` submodules
- [x] Introduce `SessionContext` struct for `rio-gateway/src/handler.rs` (10-parameter `handle_opcode`; 5 `#[allow(too_many_arguments)]` in one file)
- [x] Migrate stringly-typed keys to newtypes: `DrvHash(String)`, `WorkerId(String)` in scheduler; `AssignmentStatus` enum in db.rs
- [x] Standardize metric outcome labels (worker uses `"success"`/`"failure"`, scheduler uses `"succeeded"`/`"failed"`)
- [x] FUSE hot-path wins: cache file handles in `read()` (currently opens per-chunk), stream `readdir` entries (currently buffers all), skip `ensure_cached` for children of materialized paths

## Milestone

Traces visible in Jaeger for a multi-worker build; container images build via `nix build .#dockerImages`.
