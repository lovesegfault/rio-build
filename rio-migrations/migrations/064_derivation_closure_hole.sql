-- Closure-hole breadcrumb (failover safety). True while a node kept
-- by the top-down prune has had an un-produced child reaped out from
-- under it, so its persisted children no longer represent its pruned
-- input closure; cleared when a later full merge re-inserts edges
-- where the node is the parent, when the node's topdown_pruned mark
-- is cleared, and when the fail-fast consumes the mark. OR-combined
-- on conflict — see db/batch.rs.
ALTER TABLE derivations ADD COLUMN closure_hole BOOLEAN NOT NULL DEFAULT false;
