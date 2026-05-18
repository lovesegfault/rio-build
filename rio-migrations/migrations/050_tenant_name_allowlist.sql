-- Commentary: see rio-store/src/migrations.rs M_050.
ALTER TABLE tenants
    ADD CONSTRAINT tenant_name_allowlist
    CHECK (tenant_name ~ '^[a-zA-Z0-9._-]+$');
