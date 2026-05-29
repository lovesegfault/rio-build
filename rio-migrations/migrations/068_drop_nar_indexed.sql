-- Commentary: see rio-migrations/src/migrations.rs M_068

DROP INDEX IF EXISTS manifests_nar_index_pending_idx;
ALTER TABLE manifests DROP COLUMN nar_indexed;
