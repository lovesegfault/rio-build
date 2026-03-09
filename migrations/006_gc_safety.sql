-- GC chunk TOCTOU fix: drain re-check via blake3_hash.
--
-- The race: sweep's FOR UPDATE OF manifests locks the MANIFEST row,
-- not the CHUNK rows. PutPath for a DIFFERENT path that happens to
-- share chunk X can run after sweep has already:
--   1. decremented X's refcount to 0
--   2. set deleted=true
--   3. enqueued X's S3 key to pending_s3_deletes
-- PutPath's ON CONFLICT then increments refcount (→ 1) but the
-- pending delete is already enqueued. Drain task later deletes the
-- S3 object → new path's GetPath fails with missing chunk.
--
-- Fix: store blake3_hash alongside s3_key in pending_s3_deletes.
-- Drain re-checks chunks.(deleted AND refcount=0) before calling
-- S3 DeleteObject; if the chunk was resurrected (PutPath cleared
-- deleted=false and/or bumped refcount), drain skips the S3 delete
-- and just removes the pending row.
--
-- Nullable for back-compat: pre-006 rows (enqueued before this
-- migration) have NULL blake3_hash → drain processes them
-- unconditionally (same behavior as before).

ALTER TABLE pending_s3_deletes ADD COLUMN blake3_hash BYTEA;

-- Partial index for the drain re-check join (chunks.blake3_hash
-- is already the PK, so this side is fine; the pending side needs
-- the index for the drain SELECT to stay fast when many rows are
-- eligible). Only live rows (attempts < 10) need it — permanently-
-- failed rows are excluded from drain anyway.
CREATE INDEX idx_pending_s3_deletes_hash
    ON pending_s3_deletes (blake3_hash)
    WHERE attempts < 10 AND blake3_hash IS NOT NULL;
