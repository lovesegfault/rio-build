-- no-transaction
-- MUST be alone in this file: DROP INDEX CONCURRENTLY cannot run inside a
-- transaction block (same constraint as 011/022/108).
--
-- CONCURRENTLY: avoid the ACCESS EXCLUSIVE lock a plain DROP INDEX takes
-- on materialization_jobs (queues behind any in-flight merge tx and
-- head-of-line-blocks listing reads behind it). IF EXISTS for idempotency.
-- Ordered AFTER 108 so the listing query is never without index coverage.
DROP INDEX CONCURRENTLY IF EXISTS materialization_jobs_pending;
