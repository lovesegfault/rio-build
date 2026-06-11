-- Commentary: see rio-migrations/src/migrations.rs M_104

ALTER TABLE gc_holds
    DROP CONSTRAINT gc_holds_tenant_id_fkey;

ALTER TABLE gc_holds
    ADD CONSTRAINT gc_holds_tenant_id_fkey
    FOREIGN KEY (tenant_id) REFERENCES tenants (tenant_id)
    ON DELETE RESTRICT;
