-- no-transaction
-- MUST be alone in this file. CREATE INDEX CONCURRENTLY cannot run inside
-- a transaction block, and PostgreSQL treats a multi-statement simple-query
-- string as an implicit transaction block even when sqlx's `-- no-transaction`
-- directive (line 1) suppresses the explicit BEGIN/COMMIT wrapper. One
-- statement = no implicit block. Precedent: 011_refscan_backfill_idx.sql,
-- 022_builds_keyset_idx.sql.
--
-- CONCURRENTLY: avoid the SHARE lock a plain CREATE INDEX takes on
-- materialization_jobs for the full table scan; during a rolling deploy
-- the old leader's create/resolve/park writes would block behind it.
-- IF NOT EXISTS for idempotency across re-runs; if CONCURRENTLY fails
-- mid-build it may leave an INVALID index behind --- recovery is DROP
-- INDEX materialization_jobs_pending_priority then re-run this migration.
CREATE INDEX CONCURRENTLY IF NOT EXISTS materialization_jobs_pending_priority
    ON materialization_jobs (priority DESC, created_at, job_id)
    WHERE state = 'pending';
