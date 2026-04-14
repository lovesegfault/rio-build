-- ═══════════════════════════════════════════════════════════════════
-- Migration 038 — add 'substituting' to derivations.status
--
-- Detached upstream-substitute fetch: a background task is doing
-- QueryPathInfo (which triggers store-side try_substitute) for the
-- derivation's outputs. NOT terminal — recovery resets to Ready/Queued
-- (the spawned task is gone after restart).
--
-- Mirrors 021_derivation_status_skipped.sql: DROP+ADD the CHECK,
-- DROP+CREATE the partial index.
--
-- r[sched.substitute.detached] — scheduler side.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE derivations DROP CONSTRAINT derivations_status_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_status_check
    CHECK (status IN ('created', 'queued', 'ready', 'assigned', 'running',
                      'substituting', 'completed', 'failed', 'poisoned',
                      'dependency_failed', 'cancelled', 'skipped'));

-- Partial index unchanged (substituting is non-terminal so it stays
-- in the index — recovery loads it). Kept here as the CHECK and the
-- index are the canonical pair the migration_checksums_frozen test
-- pins together.
DROP INDEX derivations_status_idx;
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed',
                         'cancelled', 'skipped');
