-- Roots-only-prune marker (failover safety). True while a demanded
-- node kept by the top-down prune has no dependency closure in the
-- DAG; cleared when a later full merge inserts edges where the node
-- is the parent. OR-combined on conflict — see db/batch.rs.
ALTER TABLE derivations ADD COLUMN topdown_pruned BOOLEAN NOT NULL DEFAULT false;
