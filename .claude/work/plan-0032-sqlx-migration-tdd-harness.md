# Plan 0032: rusqlite→sqlx-sqlite migration + TDD actor-test harness

## Context

Three SQLite users in the codebase (`rio-worker` FUSE cache, `rio-worker` synth DB, `rio-spike`) used `rusqlite`. The scheduler/store used `sqlx` for PostgreSQL. Having both `rusqlite` and `sqlx`'s `sqlite` feature enabled causes a dual `libsqlite3-sys` linking conflict — which is why `sqlx::migrate!()` (needed for `#[sqlx::test]`) couldn't be turned on.

This plan migrates all SQLite access to `sqlx-sqlite`, unlocking the migrate feature. Bonus: `sqlx-sqlite` is async, so the FUSE cache's SQLite pool can be shared with the tokio runtime (via `Handle::block_on` in sync FUSE callbacks).

The same commit also lands the TDD infrastructure for the upcoming review passes: `#[cfg(test)]` debug-query commands on the actor (`DebugQueryWorkers`, `DebugQueryDerivation`) and an in-process tonic server harness for store gRPC tests. The review passes (P0033 onward) are TDD — failing test first, then fix — and needed these hooks.

First real migration run discovered: `references` is a PostgreSQL reserved word. Migration 002 now quotes it.

## Commits

- `8132ce5` — refactor(workspace): migrate rusqlite to sqlx-sqlite, add TDD test infrastructure
- `2646886` — test(workspace): make PG integration tests skip gracefully without DATABASE_URL

## Files

```json files
[
  {"path": "rio-worker/src/synth_db.rs", "action": "MODIFY", "note": "rusqlite::Connection → sqlx::SqliteConnection, all queries now async"},
  {"path": "rio-worker/src/fuse/cache.rs", "action": "MODIFY", "note": "rusqlite → SqlitePool; Handle::block_on bridges sync FUSE callbacks to async pool"},
  {"path": "rio-spike/src/main.rs", "action": "MODIFY", "note": "#[tokio::main] (now async)"},
  {"path": "rio-spike/src/synthetic_db.rs", "action": "MODIFY", "note": "async queries"},
  {"path": "rio-spike/src/validate.rs", "action": "MODIFY", "note": "async consistency checks"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "drop rusqlite; sqlx features += sqlite+migrate"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "#[cfg(test)] DebugQueryWorkers + DebugQueryDerivation commands; ActorHandle::debug_query_* methods; setup_actor/make_test_node/connect_worker/merge_single_node helpers; send_unchecked() for lifecycle commands"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "NEW", "note": "in-process tonic server harness: setup_store, make_nar, make_path_info, put_path helpers"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "sqlx::migrate!() now runs on startup"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "sqlx::migrate!() now runs on startup"},
  {"path": "migrations/002_store_tables.sql", "action": "MODIFY", "note": "FIX: quote \"references\" (PG reserved word — discovered by first migrate!() run)"}
]
```

## Design

**rusqlite → sqlx-sqlite:** mostly mechanical. `rusqlite::Connection::open` → `SqlitePool::connect`. `conn.execute(sql, params)` → `sqlx::query(sql).bind(p).execute(&pool).await`. The FUSE cache is the subtle one: FUSE callbacks (`lookup`, `read`, `getattr`) are synchronous, but the `SqlitePool` is async. `tokio::runtime::Handle::current().block_on(async { ... })` bridges. This works because the FUSE daemon runs on its own threads (fuser spawns `n_threads`), not tokio worker threads — so `block_on` doesn't deadlock the reactor. (It nearly did anyway — P0055's second deadlock.)

**TestDb harness (commit `2646886`):** before P0038's ephemeral bootstrap, PG tests needed `DATABASE_URL`. The harness: if unset, early-return (skip — silently, which P0038 later makes loud). If set, create `rio_test_{uuid}` database, run migrations, return pool. `Drop` runs `pg_terminate_backend` + `DROP DATABASE`. Isolated per-test.

**Debug commands:** `ActorCommand::DebugQueryWorkers` returns the worker-registration state; `DebugQueryDerivation` returns a derivation's status. Both `#[cfg(test)]`-gated. These let tests assert on internal actor state without exposing internals in the prod API. `send_unchecked()` (bypass backpressure) is also introduced here — it's what P0033's lifecycle-command fixes use.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). No markers in this commit.

## Outcome

Merged as `8132ce5..2646886`. 425 tests (up from 423). `nix develop -c cargo nextest run` passes out-of-the-box without PostgreSQL (tests skip gracefully). The `"references"` fix is the first evidence that migrations had never actually been run before this point.
