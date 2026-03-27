# Plan 0450: SQL migration — rename worker_id columns to builder_id

Standalone foundation for the [ADR-019](../../docs/src/decisions/019-builder-fetcher-split.md) builder/fetcher split. Migration files are checksum-frozen ([CLAUDE.md](../../CLAUDE.md) migration-freeze rule) so column renames go in a NEW migration, not by editing `001_scheduler.sql`/`004_recovery.sql`.

Three columns + one index across two tables:
- `derivations.assigned_worker_id` → `assigned_builder_id` (from `001_scheduler.sql:50`)
- `build_history.worker_id` → `builder_id` + `assignments_worker_idx` → `assignments_builder_idx` (from `001_scheduler.sql:91,100`)
- `poisoned_derivations.failed_workers` → `failed_builders` (from `004_recovery.sql:71`)

The 5 `.sqlx/*.json` cached queries that reference these columns regenerate via `cargo xtask regen sqlx`.

## Entry criteria

None — standalone, unblocks P0451.

## Tasks

### T1 — `chore(migrations):` new 025_rename_worker_to_builder.sql

```sql
-- ADR-019: worker → builder rename. ALTER because 001/004 are frozen.
ALTER TABLE derivations RENAME COLUMN assigned_worker_id TO assigned_builder_id;
ALTER TABLE build_history RENAME COLUMN worker_id TO builder_id;
ALTER INDEX assignments_worker_idx RENAME TO assignments_builder_idx;
ALTER TABLE poisoned_derivations RENAME COLUMN failed_workers TO failed_builders;
```

Pin checksum per the `rio-store/tests/migrations.rs` `PINNED` protocol: run the test, copy the `unpinned migration 025` panic hex into `PINNED`, commit both.

### T2 — `chore(store):` M_025 doc-const in rio-store/src/migrations.rs

Per CLAUDE.md, migration commentary goes in `migrations.rs`, not the `.sql`. Add `M_025` doc-const explaining this is the ADR-019 rename, pointing at the ADR.

### T3 — `chore(sqlx):` regenerate query cache

`cargo xtask regen sqlx` after the Rust-side query bindings update (P0451 owns the Rust-side; this plan just ensures the migration is in place so P0451's regen doesn't fail on column-not-found).

## Exit criteria

- `/nixbuild .#ci` green
- `cargo nextest run -p rio-store --test migrations` → `migration_checksums_frozen` passes with 025 pinned
- `psql -c '\d derivations'` on a fresh DB shows `assigned_builder_id` (not `assigned_worker_id`)

## Tracey

No new markers — schema rename, not behavior change.

## Files

```json files
[
  {"path": "migrations/025_rename_worker_to_builder.sql", "action": "CREATE", "note": "T1: ALTER TABLE RENAME COLUMN × 3 + ALTER INDEX"},
  {"path": "rio-store/src/migrations.rs", "action": "MODIFY", "note": "T2: M_025 doc-const"},
  {"path": "rio-store/tests/migrations.rs", "action": "MODIFY", "note": "T1: pin 025 checksum in PINNED"}
]
```

## Dependencies

None. Unblocks P0451 (the Rust query bindings rename needs the columns to exist under the new names).
