-- Add 'dependency_failed' to the derivation status CHECK constraint.
--
-- When a derivation is poisoned, its parents (transitively) can never
-- complete. The scheduler cascades DependencyFailed to those parents so
-- keepGoing builds terminate instead of hanging forever with parents
-- stuck in Queued.
--
-- Maps to Nix BuildStatus::DependencyFailed = 10.

ALTER TABLE derivations DROP CONSTRAINT derivations_status_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_status_check
    CHECK (status IN
        ('created', 'queued', 'ready', 'assigned', 'running',
         'completed', 'failed', 'poisoned', 'dependency_failed'));
