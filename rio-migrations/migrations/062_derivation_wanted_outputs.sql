-- Demand-driven completeness (wanted-outputs cache-hit criterion).
-- The subset of output_names any consumer references. '{}' = all
-- declared outputs wanted (pre-migration rows recover with the old
-- conservative criterion). Unioned on conflict — see db/batch.rs.
ALTER TABLE derivations ADD COLUMN wanted_output_names TEXT[] NOT NULL DEFAULT '{}';
