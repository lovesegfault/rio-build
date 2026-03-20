-- retro P0027: derivation_edges.is_cutoff was a "future CA early cutoff"
-- placeholder. P0252 (CA cutoff) uses DerivationStatus::Skipped node-status
-- instead — edge flag is dead. Zero .rs references (db.rs INSERT omits it,
-- relies on DEFAULT; SELECT omits it). P0276 already dropped it from
-- GraphEdge proto (field 3 RESERVED).
--
-- Non-blocking on PG 12+: column is at the end of the row, no rewrite.
ALTER TABLE derivation_edges DROP COLUMN is_cutoff;
