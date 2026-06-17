-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_108
CREATE INDEX CONCURRENTLY IF NOT EXISTS materialization_jobs_pending_priority
    ON materialization_jobs (priority DESC, created_at, job_id)
    WHERE state = 'pending';
