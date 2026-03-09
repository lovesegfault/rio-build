-- Round 4 validation: FK cascade + GIN index for GC safety.

-- Z1: cleanup_failed_merge delete_build was silently failing FK
-- violation. CASCADE so DELETE builds takes build_derivations.
ALTER TABLE build_derivations DROP CONSTRAINT build_derivations_build_id_fkey;
ALTER TABLE build_derivations
  ADD CONSTRAINT build_derivations_build_id_fkey
  FOREIGN KEY (build_id) REFERENCES builds(build_id) ON DELETE CASCADE;

-- Z2: sweep per-path reference re-check. Without this, each swept
-- path triggers a seqscan of narinfo. GIN on array column makes
-- `WHERE $path = ANY("references")` index-scannable.
CREATE INDEX idx_narinfo_references_gin ON narinfo USING GIN ("references");
