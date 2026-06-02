-- Carrier for the floating-CA stale-reset lane (substitution-replacement
-- follow-up ledger row 1): the realized output paths the stale-Completed
-- verify destroys in memory (state.output_paths.clear()). Written ONLY by
-- the stale_reset origin at job creation -- a creation-time snapshot of
-- IMMUTABLE content-addressed store paths. The wanted NAME set stays live
-- (the design's "live wanted reads" rule applies to names; a recorded
-- realisation's path cannot change). NULL = no carrier (every other
-- origin; floating-CA slots then resolve through expected_output_paths
-- exactly as before this column existed).
ALTER TABLE materialization_jobs
    ADD COLUMN carried_realized_paths TEXT[];
