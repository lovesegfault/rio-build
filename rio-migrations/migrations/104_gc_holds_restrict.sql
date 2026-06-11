-- 104: gc_holds deletion-vector repair (RESTRICT).
-- The never-deleted doctrine becomes schema: gc_holds rows are audit
-- evidence and carry NO live deletion vector. 103's inline FK was
-- ON DELETE CASCADE; shipped migrations are frozen, so the repair is
-- this new migration (103 untouched). See M_104 for the rationale.

ALTER TABLE gc_holds
    DROP CONSTRAINT gc_holds_tenant_id_fkey;

ALTER TABLE gc_holds
    ADD CONSTRAINT gc_holds_tenant_id_fkey
    FOREIGN KEY (tenant_id) REFERENCES tenants (tenant_id)
    ON DELETE RESTRICT;
