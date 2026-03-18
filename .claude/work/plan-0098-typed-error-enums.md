# Plan 0098: Typed error enums across workspace

## Design

Four commits eliminated the last `anyhow::Error` erasure points in the hot path, replacing them with typed error enums that let callers discriminate retriable failures from permanent ones and map to precise gRPC status codes.

The anchor is `8d0df1c` (`MetadataError`), which replaced 16 `-> anyhow::Result<>` signatures in `rio-store`'s metadata layer with an 8-variant enum that classifies `sqlx::Error` by PostgreSQL error code at the `From` boundary. Before: every DB error became `Status::Internal` — a transient PG pool timeout looked identical to a corrupt manifest. After: `Connection` (PoolTimedOut/PoolClosed/Io/Tls) maps to `unavailable` (retriable), `Serialization` (PG 40001) and `PlaceholderMissing` (write-ahead race loser) map to `aborted` (retriable), `Conflict` (PG 23505/23503) maps to `already_exists`, `CorruptManifest` maps to `data_loss`. Nine `grpc.rs` call sites migrated from `internal_error` to `metadata_status`. Three integration tests exercise real PG 23505 unique, real PG 23503 FK violation, and real `PlaceholderMissing` (complete-without-insert).

`2b39543` removed the only type-erasing `#[from]` in the codebase: `ExecutorError::Overlay(#[from] anyhow::Error)` had meant any `?` on an anyhow result anywhere in `execute_build()` became "overlay setup failed" — even unrelated failures. New `OverlayError` enum (6 typed variants: `InvalidBuildId`, `DirCreate{path,source}`, `Stat{path,source}`, `SameFilesystem{upper,lower,st_dev}`, `Mount{mount_data,source}`, `Unmount{source}`). `synth_db::generate_db` went from `anyhow::Result` to `Result<_, sqlx::Error>` (all internal `?` were already sqlx). Also fixed: `JoinError` from `spawn_blocking` was being wrapped into anyhow into `Overlay` — doubly wrong; now its own `OverlayTaskPanic(JoinError)` variant.

`1205a5f` split `ActorError::Internal` (a string catch-all) into typed `Dag` + `MissingDbId` variants; `a47422d` mopped up the remaining `anyhow::bail!` sites across the workspace.

## Files

```json files
[
  {"path": "rio-store/src/metadata/mod.rs", "action": "MODIFY", "note": "MetadataError 8-variant enum; From<sqlx::Error> classifies by PG error code"},
  {"path": "rio-store/src/grpc/mod.rs", "action": "MODIFY", "note": "metadata_status() mapper; 9 call sites migrate internal_error→metadata_status"},
  {"path": "rio-worker/src/overlay.rs", "action": "MODIFY", "note": "OverlayError 6-variant enum; setup/teardown typed returns; mkdir_all helper"},
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "anyhow::Result → Result<_, sqlx::Error>"},
  {"path": "rio-worker/src/executor/mod.rs", "action": "MODIFY", "note": "ExecutorError: #[from] anyhow → #[from] OverlayError; SynthDb(#[from] sqlx::Error); OverlayTaskPanic(JoinError)"},
  {"path": "rio-worker/src/executor/error.rs", "action": "MODIFY", "note": "variant reorganization"},
  {"path": "rio-scheduler/src/actor/error.rs", "action": "MODIFY", "note": "ActorError::Internal → typed Dag + MissingDbId"},
  {"path": "rio-store/src/cas.rs", "action": "MODIFY", "note": "complete_manifest_chunked error via .into() (cascade boundary — stays anyhow)"},
  {"path": "rio-store/src/content_index.rs", "action": "MODIFY", "note": "lookup migrates (reuses NarinfoRow)"}
]
```

## Tracey

Markers added retroactively in f3957f7..813609f (P0126). The `MetadataError` → `tonic::Code` mapping is referenced by `r[store.grpc.error-mapping]` (added in the retroactive sweep). The `OverlayError` variants feed into `r[worker.overlay.*]` implementation annotations.

## Entry

- Depends on P0097: dead code removed first — `MetadataError` went into `metadata.rs` which P0102 later split; doing the error work on a smaller file was cleaner.

## Exit

Merged as `2b39543..8d0df1c` (4 commits). `.#ci` green at merge. 8 new tests: 4 unit (From<sqlx::Error> classification), 3 integration (real PG error codes), 1 unit (full sqlx → MetadataError → tonic::Code chain, 7 cases). `test_internal_error_hides_sqlx_details` renamed to `test_connection_error_is_unavailable_and_hides_sqlx_details` — PoolClosed now correctly maps to Unavailable. Overlay tests upgraded from `.is_err()` to `matches!()` on variant.
