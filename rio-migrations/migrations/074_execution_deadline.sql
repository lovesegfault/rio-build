-- Commentary: see rio-migrations/src/migrations.rs M_074

-- The deadline (seconds) the pull-mode attempt was dispatched under,
-- written by the fenced pull mint from the same solve that sizes the
-- spawn intent / activeDeadlineSeconds. Nullable, no backfill, no
-- index: rows minted before this column exist only on dev/test
-- clusters and fall back to the sweep-time re-solve.
ALTER TABLE drv_executions ADD COLUMN deadline_secs DOUBLE PRECISION;
