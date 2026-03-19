-- migration 017: tenant_keys FK — add ON DELETE CASCADE.
--
-- 014 declared `REFERENCES tenants(tenant_id)` with no ON DELETE clause
-- (PG default = NO ACTION). Every other tenant-FK is explicit: 009 SET
-- NULL (×2), 012 CASCADE, 015 RESTRICT (×2). 014 was the only silent
-- default (P0249 shipped it; P0332 caught it).
--
-- CASCADE matches the 012 path_tenants pattern: tenant_keys are OWNED
-- BY the tenant (an orphan signing key signs narinfo for a gone tenant
-- — meaningless). SET NULL would violate the NOT NULL at 014:14.
-- RESTRICT would make a tenant with only-revoked keys undeletable,
-- which is the bug.
--
-- WHY 017 NOT 014-EDIT: per 012's header — sqlx::migrate! is checksum-
-- checked; even a comment change to 014 breaks every persistent DB
-- with VersionMismatch. 014 stays byte-identical; archaeology lives
-- here only.
--
-- PG has no ALTER CONSTRAINT for FK referential actions. DROP + ADD.
-- PG auto-names CREATE TABLE inline REFERENCES as `<table>_<col>_fkey`.
-- No IF EXISTS (house style, per 014:28-30).

ALTER TABLE tenant_keys
    DROP CONSTRAINT tenant_keys_tenant_id_fkey;

ALTER TABLE tenant_keys
    ADD CONSTRAINT tenant_keys_tenant_id_fkey
    FOREIGN KEY (tenant_id) REFERENCES tenants(tenant_id)
    ON DELETE CASCADE;
