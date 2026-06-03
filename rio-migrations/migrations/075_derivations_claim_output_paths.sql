-- M_075: dispatch-resolved claim paths survive failover (round-17
-- merged_bug_099). Commentary in rio-migrations/src/migrations.rs::M_075.
ALTER TABLE derivations
    ADD COLUMN claim_output_paths TEXT[];
