-- Exclude 'dependency_failed' from the partial index over non-terminal states.
--
-- Migration 003 added dependency_failed to the CHECK constraint, but the
-- partial index derivations_status_idx was created in 001 before that status
-- existed. DependencyFailed is terminal (like 'completed' and 'poisoned'), so
-- indexing those rows wastes space and bloats the index over time.

DROP INDEX derivations_status_idx;
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed');
