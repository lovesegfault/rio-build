-- Commentary: see rio-migrations/src/migrations.rs M_067

-- Greenfield: drop without backfill (pre-prod). The build-log data plane
-- moved to drv_executions + drv_log_chunks (064); the scheduler code that
-- read and wrote this table is deleted in the same commit this ships in.
DROP TABLE IF EXISTS drv_logs;
