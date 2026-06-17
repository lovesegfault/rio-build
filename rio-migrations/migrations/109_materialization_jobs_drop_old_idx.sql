-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_109
DROP INDEX CONCURRENTLY IF EXISTS materialization_jobs_pending;
