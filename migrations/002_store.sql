-- Store tables for rio-build. Phase 3a baseline.
--
-- Squashed from the phase 2a-2c migration chain:
--   002_store_tables.sql  narinfo (nar_blobs created then dropped -- net zero)
--   006_phase2c.sql  chunked CAS + CA data model + narinfo extensions
--
-- Implements the target schema from docs/src/components/store.md.
-- Multi-tenancy deferred: tenant_id columns are nullable, not in PK (Phase 4).

-- ----------------------------------------------------------------------------
-- Store path metadata (narinfo equivalent)
--
-- registration_time / ultimate: wopQueryPathInfo and binary-cache narinfo
-- both expose these.
--
-- idx_narinfo_nar_hash: the binary-cache HTTP server's /nar/{narhash}.nar.zst
-- route looks up by nar_hash (standard-ish URL format matching cache.nixos.org,
-- not the store-path hash-part).
-- ----------------------------------------------------------------------------

CREATE TABLE narinfo (
    store_path_hash     BYTEA PRIMARY KEY,
    store_path          TEXT NOT NULL,
    deriver             TEXT,
    nar_hash            BYTEA NOT NULL,           -- SHA-256 digest
    nar_size            BIGINT NOT NULL,
    "references"        TEXT[] NOT NULL DEFAULT '{}',
    signatures          TEXT[] NOT NULL DEFAULT '{}',
    ca                  TEXT,                     -- content address (empty for input-addressed)
    tenant_id           UUID,                     -- nullable: multi-tenancy deferred to Phase 4
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    registration_time   BIGINT  NOT NULL DEFAULT 0,
    ultimate            BOOLEAN NOT NULL DEFAULT FALSE
);

CREATE INDEX idx_narinfo_nar_hash ON narinfo (nar_hash);

-- ----------------------------------------------------------------------------
-- Chunk manifests
--
-- Split into two tables per store.md TOAST note: chunk_list is large
-- (often TOASTed). Keeping it separate means flipping manifests.status
-- from 'uploading' -> 'complete' doesn't rewrite the TOAST pointer.
--
-- Inline invariant:
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
    -- at 64KB average chunk size is ~5.6MB of manifest -- well under the
    -- PG TOAST threshold for the typical case.
    chunk_list       BYTEA NOT NULL
);

-- ----------------------------------------------------------------------------
-- Chunk refcounting
--
-- refcount: incremented (in the same transaction as manifest insert) when
-- a manifest references the chunk; decremented when a manifest is deleted.
-- Both use SELECT FOR UPDATE to prevent concurrent modification.
--
-- deleted: soft-delete flag set by future GC sweep. The partial index
-- covers the "find GC candidates" query without bloating the main index
-- with every live chunk.
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
-- CA content index (ContentLookup)
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
-- CA realisations (wopRegisterDrvOutput / wopQueryRealisation)
--
-- Maps (modular_drv_hash, output_name) -> (output_path, output_hash).
-- Written by wopRegisterDrvOutput when Nix finishes a CA derivation build.
-- Read by wopQueryRealisation for CA cache hits.
--
-- drv_hash here is the MODULAR derivation hash (hashDerivationModulo), not
-- the store path -- it excludes output paths, depending only on fixed
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
