-- Commentary: see rio-migrations/src/migrations.rs M_072
--
-- Phase 1c (refcount-formal): drop the chunks.refcount column itself.
-- Chunk liveness is derived from the manifests at collect time, the
-- counter writers were deleted at Release B (069 dropped the CHECK and
-- the GC index), and nothing reads or writes the column any more.
-- Metadata-only.
ALTER TABLE chunks DROP COLUMN IF EXISTS refcount;
