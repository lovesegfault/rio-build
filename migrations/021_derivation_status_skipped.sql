-- ═══════════════════════════════════════════════════════════════════
-- Migration 021 — add 'skipped' to derivations.status
--
-- CA early-cutoff: when a CA derivation completes with a content-
-- index match (output byte-identical to a prior build), downstream
-- Queued derivations can be Skipped without running. Skipped is
-- terminal — leader failover MUST NOT re-queue.
--
-- Mirrors 004_recovery.sql's 'cancelled' addition: DROP+ADD the
-- CHECK, DROP+CREATE the partial index.
--
-- r[sched.ca.cutoff-propagate] — scheduler side.
-- ═══════════════════════════════════════════════════════════════════

ALTER TABLE derivations DROP CONSTRAINT derivations_status_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_status_check
    CHECK (status IN ('created', 'queued', 'ready', 'assigned', 'running',
                      'completed', 'failed', 'poisoned', 'dependency_failed',
                      'cancelled', 'skipped'));

-- Partial index: exclude 'skipped' (terminal — recovery doesn't re-queue).
-- Must match TERMINAL_STATUS_SQL in rio-scheduler/src/db.rs:41 and the
-- drift tests test_terminal_statuses_match_is_terminal +
-- test_partial_index_predicate_matches_const.
DROP INDEX derivations_status_idx;
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed',
                         'cancelled', 'skipped');
