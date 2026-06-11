-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_114
--
-- Index file_blobs(store_path_hash) so the manifests FK CASCADE
-- (GC sweep's narinfo DELETE) stops scanning the whole table per
-- deleted path. CONCURRENTLY: file_blobs is large and hot at deploy
-- time; precedent 022_builds_keyset_idx.sql. MUST be the only
-- statement in this file (implicit-transaction rule, see 022).
CREATE INDEX CONCURRENTLY IF NOT EXISTS file_blobs_path_idx
    ON file_blobs (store_path_hash);
