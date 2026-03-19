# Phase 2b: Observability + Packaging (Months 7-9)

**Goal:** Production-quality observability, container images, and integration testing.

**Implements:** [Observability](../observability.md), [Configuration](../configuration.md)

## Tasks

- [x] Build log streaming with batching and rate limiting (worker → scheduler → gateway)
  - Worker buffers up to 64 lines or 100ms per batch
  - Scheduler maintains per-derivation ring buffer for active log serving
  - Async flush to S3 on derivation completion
  - Log rate limiting per build (`log_rate_limit`, `log_size_limit`)
  - [x] Honor 100ms BATCH_TIMEOUT during silent build periods (spawn reader into owned task, select! on channel + interval; naive timeout-wrap is cancel-unsafe) — completed in pre-bulk cleanup (C4)
  - Completed in C6 (ring buffer + forward), C7 (rate/size limits), C8 (S3 flush + periodic), C9 (AdminService.GetBuildLogs), C10 (e2e wire test). End-to-end validated in `vm-phase2b` assertion 2 (PHASE2B-LOG-MARKER grep).
- [x] Build correlation IDs (UUID v7): generated at `SubmitBuild`, propagated via gRPC metadata, included in all log spans — C5. v7 time-ordered for S3 log key prefix-scanning + PG index locality. `build_id` span field per `observability.md:204` added in C13.
- [x] `tracing-opentelemetry`: trace propagation across gRPC boundaries (export to stdout or local Jaeger in dev) — C11 (OTLP layer, RIO_OTEL_ENDPOINT-gated), C12 (W3C traceparent inject/extract), C13 (wired into 11 handlers + gateway→scheduler client inject). Full OTLP to Tempo, not stdout — milestone says "visible in Jaeger", that needs a real exporter. Validated in `vm-phase2b` assertion 4.
- [x] Container images for each component (Nix-based via `dockerTools.buildLayeredImage`) — C17 (`nix/docker.nix`). Worker includes nix + fuse3 + util-linux + passwd stubs for nixbld user. `.#dockerImages` aggregate.
- [x] Configuration management (TOML config + env var overlay with `RIO_` prefix) — C3 (figment loader), C4 (all 4 main.rs migrated). Two-struct split: `Config` (concrete T, serde defaults) + `CliArgs` (Option<T>, skip_serializing_if). Precedence: CLI > env > TOML > defaults.
- [x] `cargo-deny` integration: license auditing (deny GPL-3.0), security advisory checking — C2 (`deny.toml` + `craneLib.cargoDeny`). One accepted advisory: RUSTSEC-2023-0071 (russh→rsa Marvin) — we use ed25519 only, no RSA privkey held.
- [x] Integration test: multi-derivation build (A → B → C) across 3+ workers — `vm-phase2b` (5 VMs, 3 workers, sequential chain, one derivation per worker via `maxBuilds=1`).
- [x] Integration test: cache hit path (second build is instant) — `vm-phase2b` assertion 3 (< 10s + `cache_hits_total` > 0).
- [x] gRPC contract tests for each service boundary — C16 (`rio-proto/tests/contract.rs`). Trailer contract tests landed with C14 in `rio-store/tests/`. Compile-time limit tripwires via `const { assert! }`.
- [x] Add `rio-proto/src/validated.rs` with `ValidatedPathInfo { store_path: StorePath, nar_hash: [u8; 32], ... }` + `TryFrom<PathInfo>`. Migrate gRPC handlers and `NarinfoRow::into_path_info` (DB egress, currently no validation) to validated types — completed in pre-bulk cleanup (C5+C6)
- [x] Lazy NAR upload streaming: replace eager `Vec<PutPathRequest>` (4GiB NAR → ~8GiB peak, ×4 parallel = 32GiB) with `stream::unfold` or `Arc<[u8]>`-based lazy chunk iterator — C14 (PutPathTrailer proto), C15 (single-pass tee upload). `dump_path_streaming` + `HashingChannelWriter` in `spawn_blocking`. Peak: 8GiB → ~1MiB per in-flight, ×4 = ~4MiB total.
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

## Automated Validation

The milestone is validated by a NixOS VM test:

```bash
nix build .#checks.x86_64-linux.vm-phase2b
```

Five VMs (control + 3 workers + client) exercise the phase2b feature set:

1. **Chain build**: A→B→C sequential derivations, forcing one-per-worker
   dispatch (`maxBuilds=1` × 3 workers × 3 steps).
2. **Log pipeline**: Each step echoes `PHASE2B-LOG-MARKER: building {name}`
   to stderr. Test greps the client's `nix-build` output — presence proves
   the full chain: worker LogBatcher → scheduler ring buffer → ForwardLogBatch
   → BuildEvent::Log → gateway STDERR_NEXT → SSH → client.
3. **Cache hit**: Second build of the same chain completes fast and increments
   `rio_scheduler_cache_hits_total`.
4. **OTLP export**: Tempo (Jaeger-equivalent, OTLP-compatible) `/api/search`
   returns traces for `service.name=scheduler`. Proves `init_tracing` OTLP
   layer + `RIO_OTEL_ENDPOINT` wiring + SubmitBuild span export.

Container images:

```bash
nix build .#dockerImages
ls result/  # gateway.tar.gz scheduler.tar.gz store.tar.gz worker.tar.gz
```

> **Tempo vs Jaeger:** The milestone says "Jaeger". Tempo validates the same
> contract (OTLP/gRPC ingest + queryable trace API) and is what's packaged
> in nixpkgs. In production, either works — point `RIO_OTEL_ENDPOINT` at
> whichever OTLP collector you run.
