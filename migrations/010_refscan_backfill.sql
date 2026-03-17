-- Migration 010: refs_backfilled tracking column.
--
-- Context (plan 02, PR 4/6):
--   Before the worker's NAR reference-scan fix (PR 2/6, commit 9165dc2),
--   uploaded paths had narinfo.references = '{}'. Paths uploaded AFTER the
--   fix have correct references. We need to distinguish the two so a
--   background job can re-scan the pre-fix paths without touching the
--   post-fix ones.
--
-- Column semantics:
--   NULL  -> uploaded post-fix (references are correct; never touched by backfill)
--   FALSE -> uploaded pre-fix (needs re-scan)
--   TRUE  -> re-scanned by the backfill job
--
-- The partial index for the backfill scan is in migration 011 (separate
-- file because CREATE INDEX CONCURRENTLY cannot share a batch with other
-- statements -- PostgreSQL treats a multi-statement simple-query string as
-- an implicit transaction block).

-- Track which paths need reference re-scan (uploaded before the scanner fix).
-- NULL = uploaded post-fix (correct refs). FALSE = needs backfill. TRUE = backfilled.
ALTER TABLE narinfo ADD COLUMN refs_backfilled BOOLEAN DEFAULT NULL;

-- Mark existing rows as needing backfill.
UPDATE narinfo SET refs_backfilled = FALSE WHERE refs_backfilled IS NULL;
