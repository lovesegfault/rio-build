-- Phase 3b GC: pending S3 deletes + explicit GC roots.
--
-- Two-phase GC (mark + sweep):
--   1. Mark: recursive CTE over narinfo."references" from root
--      seeds (gc_roots + uploading manifests + recently-created
--      + scheduler live-build paths passed as extra_roots).
--   2. Sweep: for each unreachable complete manifest: DELETE
--      narinfo (CASCADE to manifests/manifest_data/content_index/
--      realisations), decrement chunk refcounts, mark refcount=0
--      chunks deleted, enqueue S3 keys to pending_s3_deletes.
--   3. Drain task (background, rio-store): SELECT pending rows,
--      S3 DeleteObject, DELETE row on success / increment
--      attempts+last_error on fail.
--
-- Migration 005: runs AFTER S track's 004_recovery.sql (execution
-- order T-A-S-B-E-D-C). sqlx rejects out-of-order.

-- ----------------------------------------------------------------------------
-- pending_s3_deletes: S3 keys to delete, drained by background task.
--
-- Two-phase commit for S3 cleanup: the sweep tx can DELETE narinfo
-- rows atomically, but S3 DeleteObject isn't transactional. Instead:
-- enqueue S3 keys in the SAME tx as the narinfo DELETE, then a
-- separate drain task does the actual S3 delete. If the drain
-- fails (S3 down, rate-limited), the row stays; drain retries.
-- Worst case: S3 object leaks (storage cost) but the PG-side
-- refcount is correct. Better than the reverse (S3 deleted, PG
-- tx rolled back, dangling chunk reference → GetPath fails).
-- ----------------------------------------------------------------------------

CREATE TABLE pending_s3_deletes (
    id          BIGSERIAL PRIMARY KEY,
    s3_key      TEXT NOT NULL,
    enqueued_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    attempts    INT NOT NULL DEFAULT 0,
    last_error  TEXT
);

-- Drain query: SELECT ... WHERE attempts < N ORDER BY enqueued_at
-- LIMIT batch. Partial index on eligible rows (attempts < 10 is
-- the cutoff before we give up and alert). The index excludes
-- permanently-failed rows so drain queries stay fast even if
-- those accumulate (operator should investigate and manually
-- resolve or delete).
CREATE INDEX idx_pending_s3_deletes_drain
    ON pending_s3_deletes (enqueued_at) WHERE attempts < 10;

-- ----------------------------------------------------------------------------
-- gc_roots: explicit pins.
--
-- NOT for temp roots (uploading/recent paths -- those use the
-- grace period in mark.rs). NOT for scheduler live-build roots
-- (those pass as extra_roots param to mark -- they're in-flight
-- build outputs that may NOT exist in narinfo yet, so the FK
-- would reject them).
--
-- This is for "pin this path forever" (or until explicit unpin):
-- a build result you want to keep regardless of reachability.
-- ----------------------------------------------------------------------------

CREATE TABLE gc_roots (
    -- FK with CASCADE: if narinfo is deleted anyway (manual
    -- intervention, schema reset), the pin goes too -- a pin on
    -- a path that doesn't exist is meaningless.
    store_path_hash BYTEA PRIMARY KEY
        REFERENCES narinfo (store_path_hash) ON DELETE CASCADE,
    -- Where this pin came from (operator name, API call source
    -- etc). Free-form; for audit/debugging.
    source          TEXT NOT NULL,
    pinned_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);
