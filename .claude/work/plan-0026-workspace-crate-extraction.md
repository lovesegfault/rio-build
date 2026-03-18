# Plan 0026: Workspace crate extraction — 6-crate distributed architecture scaffold

## Context

Phase 1a/1b built `rio-nix` (protocol) + `rio-build` (monolithic gateway). Phase 2a's goal — "multi-worker builds with a simple scheduler" — requires five independently deployable services. This plan is the surgical split: extract `rio-common`, `rio-gateway`, `rio-scheduler`, `rio-store`, `rio-worker` as new workspace members with scaffold binaries. It is pure structure — no new behavior. The next four plans (P0027–P0031) fill each crate in.

This is the first commit of phase 2a and the direct successor to P0025 (NAR streaming refactor), which landed the AsyncRead/HashingReader plumbing that the store service will ride on. `rio-store/src/validate.rs` is migrated here intact from `rio-build` (408 lines — the NAR validation core).

## Commits

- `10127fa` — refactor(workspace): extract crates for Phase 2a architecture

Single commit. 1,121 insertions, 27 files. All 353 pre-existing tests pass unchanged.

## Files

```json files
[
  {"path": "Cargo.toml", "action": "MODIFY", "note": "workspace members += 5 crates; workspace deps: tonic/prost 0.14, sqlx, aws-sdk-s3, fuser, clap"},
  {"path": "rio-common/Cargo.toml", "action": "NEW", "note": "shared crate: observability, config, error types"},
  {"path": "rio-common/src/observability.rs", "action": "NEW", "note": "migrated from rio-build: tracing init, Prometheus exporter, root_span helper"},
  {"path": "rio-common/src/config.rs", "action": "NEW", "note": "ServiceAddrs/CommonConfig — later deleted as unused (P0035)"},
  {"path": "rio-common/src/error.rs", "action": "NEW", "note": "ServiceError — later deleted as unused (P0035)"},
  {"path": "rio-gateway/Cargo.toml", "action": "NEW", "note": "SSH server + gRPC client deps"},
  {"path": "rio-gateway/src/main.rs", "action": "NEW", "note": "scaffold: CLI args, observability init, placeholder server start"},
  {"path": "rio-scheduler/Cargo.toml", "action": "NEW", "note": "sqlx-postgres + tonic server deps"},
  {"path": "rio-scheduler/src/main.rs", "action": "NEW", "note": "scaffold: CLI args, observability init"},
  {"path": "rio-store/Cargo.toml", "action": "NEW", "note": "aws-sdk-s3 + sqlx + tonic server deps"},
  {"path": "rio-store/src/backend/mod.rs", "action": "NEW", "note": "NarBackend trait: put/get/delete/exists"},
  {"path": "rio-store/src/backend/filesystem.rs", "action": "NEW", "note": "stub"},
  {"path": "rio-store/src/backend/memory.rs", "action": "NEW", "note": "stub (test-only)"},
  {"path": "rio-store/src/backend/s3.rs", "action": "NEW", "note": "stub"},
  {"path": "rio-store/src/validate.rs", "action": "NEW", "note": "MIGRATED: NAR validation (HashingReader etc.) from rio-build — 408 lines"},
  {"path": "rio-store/src/main.rs", "action": "NEW", "note": "scaffold: CLI args, observability init"},
  {"path": "rio-worker/Cargo.toml", "action": "NEW", "note": "fuser + nix syscalls + tonic client deps"},
  {"path": "rio-worker/src/main.rs", "action": "NEW", "note": "scaffold: CLI args, observability init, worker-id from hostname"}
]
```

## Design

The workspace layout mirrors the deployment topology from `docs/src/architecture.md`: gateway (SSH-facing, stateless), scheduler (DAG brain, single-instance in 2a), store (NAR blob + metadata), worker (FUSE+overlay+nix-daemon), common (shared observability). Each crate gets a `main.rs` with clap CLI (env-var mapped) and `rio_common::observability::init()` — the only shared boot code.

Workspace dependencies added: `tonic`/`prost` 0.14 for gRPC (matching `rio-proto`'s build.rs), `sqlx` with `postgres` feature (scheduler+store metadata), `aws-sdk-s3` (store backend), `fuser` 0.17 (worker — same version P0007's spike validated), `clap` with derive (all binaries). `nix` syscall crate already present from P0007.

`rio-store/src/validate.rs` is the only non-trivial migration — 408 lines of NAR validation moved verbatim from `rio-build`. It depends on `rio-nix`'s NAR types and `HashingReader` from P0025. Everything else is `main.rs` boilerplate + placeholder modules.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). No `r[impl]`/`r[verify]` markers in this commit. Architecture spec text in `docs/src/crate-structure.md` was later retro-tagged with `r[arch.crates.*]` markers when tracey was adopted.

## Outcome

Merged as `10127fa`. 353 tests green, clippy clean. Zero behavior change — pure structure. The next commit (P0027) fills `rio-proto` with the gRPC service contracts, and three commits after that the scaffolds become real services.
