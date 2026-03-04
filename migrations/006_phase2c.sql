-- Phase 2c: chunked CAS, CA data model, intelligent scheduling.
--
-- Implements the target schema from docs/src/components/store.md §PostgreSQL
-- Schema and the build_history extension from docs/src/phases/phase2c.md.
--
-- BREAKING: DROP TABLE nar_blobs. Phase 2a stored whole NARs keyed by
-- SHA-256; phase 2c stores inline blobs (<256KB) in manifests.inline_blob
-- and larger NARs as FastCDC chunks in S3 + manifest_data.chunk_list.
-- User decision: no dual-read backward compat — VM tests start fresh,
-- any production instance must wipe and re-upload.

-- ----------------------------------------------------------------------------
-- 1. Drop phase2a whole-NAR storage
-- ----------------------------------------------------------------------------

DROP TABLE IF EXISTS nar_blobs;

-- ----------------------------------------------------------------------------
-- 2. narinfo: phase2c columns + nar_hash index
--
-- registration_time / ultimate: resolves TODO(phase2c) at metadata.rs:329.
-- wopQueryPathInfo and binary-cache narinfo both expose these; previously
-- they were silently dropped on PutPath and returned as 0/false.
--
-- idx_narinfo_nar_hash: the binary-cache HTTP server's /nar/{narhash}.nar.zst
-- route needs to look up by nar_hash (user decision: standard-ish URL format
-- matching cache.nixos.org, not the store-path hash-part).
-- ----------------------------------------------------------------------------

ALTER TABLE narinfo
  ADD COLUMN registration_time BIGINT  NOT NULL DEFAULT 0,
  ADD COLUMN ultimate          BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX idx_narinfo_nar_hash ON narinfo (nar_hash);

-- ----------------------------------------------------------------------------
-- 3. Chunk manifests
--
-- Split into two tables per store.md:214 TOAST note: chunk_list is large
-- (often TOASTed). Keeping it separate means flipping manifests.status
-- from 'uploading' -> 'complete' doesn't rewrite the TOAST pointer.
--
-- Inline invariant (store.md:222):
--   inline_blob IS NOT NULL  <=>  no manifest_data row exists
--   inline_blob IS NULL      <=>  manifest_data row MUST exist
-- Code checks inline_blob FIRST; only queries manifest_data if NULL.
--
-- ON DELETE CASCADE: deleting a narinfo row (future GC) takes its manifest
-- with it. manifest_data cascades from manifests for the same reason.
-- ----------------------------------------------------------------------------

CREATE TABLE manifests (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES narinfo (store_path_hash) ON DELETE CASCADE,
    status           TEXT NOT NULL DEFAULT 'uploading'
                     CHECK (status IN ('uploading', 'complete')),
    -- non-NULL => inline fast-path (NAR <256KB). Whole NAR stored here.
    -- NULL => chunked; manifest_data row holds the chunk list.
    inline_blob      BYTEA,
    updated_at       TIMESTAMPTZ NOT NULL DEFAULT now()
);

CREATE TABLE manifest_data (
    store_path_hash  BYTEA PRIMARY KEY
                     REFERENCES manifests (store_path_hash) ON DELETE CASCADE,
    -- Versioned binary format: [u8 version=1] followed by a packed array
    -- of (blake3_hash: [u8;32], chunk_size: u32_le) entries. A 10GB NAR
    -- at 64KB average chunk size is ~5.6MB of manifest — well under the
    -- PG TOAST threshold for the typical case.
    chunk_list       BYTEA NOT NULL
);

-- ----------------------------------------------------------------------------
-- 4. Chunk refcounting
--
-- refcount: incremented (in the same transaction as manifest insert) when
-- a manifest references the chunk; decremented when a manifest is deleted.
-- Both use SELECT FOR UPDATE to prevent concurrent modification.
--
-- deleted: soft-delete flag set by future GC sweep (not phase2c scope).
-- The partial index covers the "find GC candidates" query without bloating
-- the main index with every live chunk.
-- ----------------------------------------------------------------------------

CREATE TABLE chunks (
    blake3_hash  BYTEA PRIMARY KEY,
    refcount     INTEGER NOT NULL DEFAULT 0,
    size         BIGINT NOT NULL,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    deleted      BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX idx_chunks_gc ON chunks (blake3_hash)
    WHERE refcount = 0 AND deleted = FALSE;

-- ----------------------------------------------------------------------------
-- 5. CA content index (ContentLookup)
--
-- Maps output NAR content hash -> store path. Populated at PutPath-complete
-- time (content_hash = nar_hash). ContentLookup RPC reads this for CA cache
-- hits: "does a path with this content already exist?"
--
-- Composite PK allows the same content to appear under multiple store paths
-- (happens with FODs: two derivations fetching the same tarball).
-- tenant_id is nullable (multi-tenancy deferred to Phase 4).
-- ----------------------------------------------------------------------------

CREATE TABLE content_index (
    content_hash     BYTEA NOT NULL,   -- SHA-256 of output NAR
    store_path_hash  BYTEA NOT NULL
                     REFERENCES narinfo (store_path_hash) ON DELETE CASCADE,
    tenant_id        UUID,
    PRIMARY KEY (content_hash, store_path_hash)
);

-- ----------------------------------------------------------------------------
-- 6. CA realisations (wopRegisterDrvOutput / wopQueryRealisation)
--
-- Maps (modular_drv_hash, output_name) -> (output_path, output_hash).
-- Written by wopRegisterDrvOutput when Nix finishes a CA derivation build.
-- Read by wopQueryRealisation for CA cache hits before Phase 5's full
-- early-cutoff activation.
--
-- drv_hash here is the MODULAR derivation hash (hashDerivationModulo), not
-- the store path — it excludes output paths, depending only on fixed
-- attributes. Two CA derivations with the same inputs hash the same even
-- if their output store paths differ.
--
-- output_hash: SHA-256 of the output NAR. Redundant with content_index
-- but stored here for signature verification (Phase 5 signs the tuple).
-- ----------------------------------------------------------------------------

CREATE TABLE realisations (
    drv_hash     BYTEA NOT NULL,
    output_name  TEXT NOT NULL,
    output_path  TEXT NOT NULL,
    output_hash  BYTEA NOT NULL,
    signatures   TEXT[] NOT NULL DEFAULT '{}',
    tenant_id    UUID,
    created_at   TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (drv_hash, output_name)
);

CREATE INDEX realisations_output_idx ON realisations (output_path);

-- ----------------------------------------------------------------------------
-- 7. build_history: resource tracking for size-class routing
--
-- Workers report peak_memory_bytes (from /proc/{pid}/status VmHWM) and
-- output_size_bytes in CompletionReport. Scheduler maintains EMA here
-- alongside ema_duration_secs (same alpha=0.3).
--
-- size_class: last class this (pname,system) was routed to. Informational
-- for dashboards; classify() reads ema_duration + ema_peak_memory, not this.
--
-- misclassification_count: incremented when a build exceeds 2x its class
-- cutoff. Phase 3a's adaptive rebalancer (deferred) uses this to detect
-- systematically-wrong cutoffs. Phase 2c just records it.
--
-- ema_peak_cpu_cores: column present per spec but TODO(phase3a) — CPU%
-- needs polling during the build, VmHWM is a one-shot read at the end.
-- ----------------------------------------------------------------------------

ALTER TABLE build_history
  ADD COLUMN ema_peak_memory_bytes   DOUBLE PRECISION,
  ADD COLUMN ema_peak_cpu_cores      DOUBLE PRECISION,
  ADD COLUMN ema_output_size_bytes   DOUBLE PRECISION,
  ADD COLUMN size_class              TEXT,
  ADD COLUMN misclassification_count INTEGER NOT NULL DEFAULT 0;
