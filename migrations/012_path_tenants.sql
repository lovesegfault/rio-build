-- migration 012: path_tenants join table for per-tenant GC retention.
--
-- WHY 012 NOT 009-APPEND: The 009 header said "Part C appended later",
-- but migrations 010+011 landed after 009. sqlx::migrate! is checksum-
-- checked -- even a comment change to 009 breaks every persistent DB
-- with VersionMismatch. 012 is the only checksum-safe path.
--
-- WHY NO FK to narinfo: follows migrations/007_live_pins.sql precedent --
-- path_tenants references paths that MAY not be in narinfo yet (upsert
-- fires at completion, narinfo PUT may lag).
--
-- narinfo.tenant_id drop: column added at 002:30, zero Rust readers
-- (grep empty). Replaced by this N:M join table.

CREATE TABLE path_tenants (
    store_path_hash     BYTEA       NOT NULL,
    tenant_id           UUID        NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    first_referenced_at TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (store_path_hash, tenant_id)
);

CREATE INDEX path_tenants_retention_idx ON path_tenants (tenant_id, first_referenced_at);

ALTER TABLE narinfo DROP COLUMN tenant_id;
