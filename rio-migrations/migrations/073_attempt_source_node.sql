-- Commentary: see rio-migrations/src/migrations.rs M_073

-- Durable source-node attribution for attempts/executions (nullable,
-- no backfill, no index), plus the pull/stream coexistence
-- discriminator on the execution row. The stream dispatch path never
-- sets dispatch_mode and relies on the default.
ALTER TABLE drv_attempts ADD COLUMN source_node TEXT;
ALTER TABLE drv_executions ADD COLUMN source_node TEXT;
ALTER TABLE drv_executions ADD COLUMN dispatch_mode TEXT NOT NULL DEFAULT 'stream'
    CHECK (dispatch_mode IN ('stream', 'pull'));
