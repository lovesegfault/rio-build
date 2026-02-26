# Phase 2a: Core Distribution (Months 5-7)

**Goal:** Multi-worker builds with a simple scheduler.

**Implements:** [rio-proto](../components/proto.md), [rio-scheduler](../components/scheduler.md) (FIFO), [rio-worker](../components/worker.md), [rio-store](../components/store.md) (filesystem backend)

## Tasks

- [x] `rio-proto`: protobuf definitions for SchedulerService, WorkerService, StoreService, AdminService
- [x] Simple FIFO scheduler with actor-based concurrency model (no critical path, no locality scoring yet)
- [x] PostgreSQL for build state (builds, derivations, derivation_edges, assignments tables) with `tenant_id` columns (nullable, unused in this phase)
- [x] Database migration framework (sqlx migrations with offline query metadata via `sqlx prepare`)
- [x] Worker bidirectional BuildExecution stream
- [x] Worker FUSE store integration (rio-fuse for local `/nix/store` access)
- [x] Worker local nix invocation for builds (via `nix-daemon --stdio`)
- [x] Store: simple filesystem or S3 backend (full NARs, no chunking) with NAR hash verification on PutPath
- [x] Multi-process deployment (gateway, scheduler, store, workers as separate processes)
- [x] IFD handling: `wopBuildDerivation` during evaluation treated as normal build request, prioritized in scheduler
- [x] Basic TransientFailure retry (re-queue to another worker, 2 attempts; exponential backoff computed but immediate re-queue — delayed re-queue deferred to Phase 3)
- [x] Build hook protocol: `--builders` mode path where Nix delegates individual derivations to rio-build

> **Shared derivation priority simplification:** Phase 2a uses a binary interactive/scheduled FIFO (interactive builds push_front the ready queue). Full max(priority) across interested builds is deferred to Phase 2c with critical-path scheduling.

> **FUSE fallback impact:** If the Phase 1a FUSE+overlay spike resulted in the bind-mount fallback, the "Worker FUSE store integration" task above changes significantly: instead of `rio-fuse`, workers use `nix-store --realise` to pre-materialize all input store paths on local disk before each build. This eliminates lazy loading and prefetch hints, but simplifies the worker architecture. The scheduler's bloom filter locality scoring still applies (workers cache materialized paths), but the cache management is simpler (explicit directory management instead of FUSE).

## Milestone

`nix build --store ssh-ng://rio nixpkgs#hello` completes across 2+ workers.
