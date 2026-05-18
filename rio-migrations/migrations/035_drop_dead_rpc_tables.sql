DROP TABLE IF EXISTS chunk_tenants;

DROP TABLE IF EXISTS content_index;

ALTER TABLE narinfo DROP COLUMN IF EXISTS refs_backfilled;
DROP INDEX IF EXISTS narinfo_refs_backfill_pending_idx;
