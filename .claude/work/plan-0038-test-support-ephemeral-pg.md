# Plan 0038: rio-test-support — ephemeral PostgreSQL bootstrap, eliminate silent skips

## Context

Before this plan, PG-backed tests returned early with "skipping: DATABASE_URL not set" — green CI that tested nothing. 13 scheduler tests and 5 store tests were silently no-ops in `nix flake check`.

This is the infrastructure fix: a new `rio-test-support` crate lazily bootstraps a process-global PostgreSQL (`initdb` + direct `postgres` child process, Unix-socket-only) using binaries from the Nix dev shell. Each `TestDb::new()` creates an isolated database, runs migrations, drops on `Drop`. No external setup needed. Tests **panic** (not skip) if `postgres` binaries are unavailable — "silent skip" becomes "loud fail."

This is CLAUDE.md-documented infrastructure: "PostgreSQL integration tests bootstrap their own ephemeral postgres server (via `rio-test-support`) using `initdb`/`postgres` binaries from the dev shell. No manual setup needed."

## Commits

- `455cdf6` — feat(rio-test-support): bootstrap ephemeral postgres for tests, eliminate silent skips
- `5a778ad` — fix(rio-test-support): GC stale /tmp/rio-pg-* dirs on bootstrap

(Second commit landed much later — commit 129 of 141 — but it's a direct fix for a leak in this infrastructure. Discontinuous range.)

## Files

```json files
[
  {"path": "rio-test-support/Cargo.toml", "action": "NEW", "note": "dev-only crate: tempfile, nix (prctl), once_cell"},
  {"path": "rio-test-support/src/lib.rs", "action": "NEW", "note": "PgServer (OnceLock-global initdb+postgres subprocess), TestDb (per-test isolated DB + migrate + drop on Drop), PR_SET_PDEATHSIG for Linux cleanup, gc_stale_dirs PID-liveness protocol"},
  {"path": "flake.nix", "action": "MODIFY", "note": "postgresql_18 in dev shell + nativeCheckInputs; PG_BIN env var; migrations/ in Crane fileset"},
  {"path": "rio-scheduler/src/actor.rs", "action": "MODIFY", "note": "13 test call sites: TestDb skip-on-unset → rio-test-support panic-on-unavailable; delete local TestDb"},
  {"path": "rio-store/tests/grpc_integration.rs", "action": "MODIFY", "note": "5 call sites converted; delete local TestDb"},
  {"path": "rio-gateway/tests/integration_distributed.rs", "action": "MODIFY", "note": "FUSE stub uses TestDb, panics instead of skipping"},
  {"path": "Cargo.toml", "action": "MODIFY", "note": "nix crate += process,signal features (for prctl)"}
]
```

## Design

**Bootstrap:** `PgServer` is a `OnceLock<PgServer>` static. First `TestDb::new()` wins the race, runs: (1) `tempfile::Builder::new().prefix("rio-pg-")` for data dir, (2) `initdb -D {dir}`, (3) `postgres -D {dir} -k {dir} -h ''` (Unix socket only, no TCP), (4) poll socket until ready. On Linux, `PR_SET_PDEATHSIG(SIGKILL)` on the child — if the test process dies, the postgres dies too.

**Per-test isolation:** each `TestDb::new()` creates `rio_test_{uuid}` database on the shared server, runs `sqlx::migrate!()`, returns a pool. `Drop` opens a fresh admin connection (not from the pool — tests may have closed the pool for fault injection), runs `pg_terminate_backend` on connections to the test DB, then `DROP DATABASE`.

**Nextest:** test group `postgres` with 60s timeout (first test in the group pays the initdb cost, ~5s).

**Stale dir GC (`5a778ad`):** Rust never runs `Drop` on statics at process exit. Every test process leaked its `/tmp/rio-pg-*` dir (~56MB each). With nextest's process-per-test, a full suite run leaked ~50 dirs — 32GB `/tmp` tmpfs full after a few runs. Fix: PID-based liveness protocol. Bootstrap writes `owner.pid` immediately after `mkdtemp`. `gc_stale_dirs()` scans `/tmp/rio-pg-*` before creating a new dir, removes any whose `owner.pid` is dead (`kill(pid, 0)` → `ESRCH`). Concurrent scanners race benignly (`remove_dir_all` → `ENOENT`, ignored). Leak bounded to O(concurrent test processes).

**Override:** `DATABASE_URL` still takes precedence if set — useful for debugging against an external PG.

## Tracey

Phase 2a predates tracey adoption (landed phase 3a `f3957f7`). Test-support infrastructure doesn't get spec markers — it's not a product behavior. No retro-tagging.

## Outcome

Merged as `455cdf6` + `5a778ad` (discontinuous — main feat + later leak fix). `nix develop -c cargo nextest run` now runs all 18 previously-skipped PG tests with zero manual setup. The postgres binaries are a hard dependency of `.#ci` — which is correct: the tests matter.
