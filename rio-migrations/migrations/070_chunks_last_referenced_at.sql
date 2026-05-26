-- Commentary: see rio-migrations/src/migrations.rs M_070
--
-- NULL means "never re-referenced since insert"; the collector's
-- grace predicate reads GREATEST(created_at, last_referenced_at),
-- which ignores NULL, so no backfill is needed.
ALTER TABLE chunks ADD COLUMN last_referenced_at TIMESTAMPTZ;
