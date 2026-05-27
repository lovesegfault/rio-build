-- Commentary: see rio-migrations/src/migrations.rs M_071
--
-- Release B (refcount-formal): the chunk collector derives liveness
-- from the manifests, nothing reads chunks.refcount any more, and this
-- release deletes the counter writers. The CHECK has to go before the
-- writer-deletion code serves; the column itself is NOT dropped here
-- (that is migration 072, applied only after the Release B rollout
-- completes).
ALTER TABLE chunks DROP CONSTRAINT IF EXISTS chunks_refcount_nonneg;
DROP INDEX IF EXISTS idx_chunks_gc;
