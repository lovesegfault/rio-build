ALTER TABLE derivation_edges
  DROP CONSTRAINT derivation_edges_parent_id_fkey,
  DROP CONSTRAINT derivation_edges_child_id_fkey;

ALTER TABLE build_derivations
  DROP CONSTRAINT build_derivations_derivation_id_fkey;
