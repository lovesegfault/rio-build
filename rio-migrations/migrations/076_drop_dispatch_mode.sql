-- Commentary: see rio-migrations/src/migrations.rs M_076

-- Executor-lifecycle knob retirement: drop the pull/stream coexistence
-- discriminator. The stream dispatch path is deleted, every execution
-- row is minted by the pull transaction, and the open-attempt view no
-- longer filters on the column. Metadata-only.
ALTER TABLE drv_executions DROP COLUMN IF EXISTS dispatch_mode;
