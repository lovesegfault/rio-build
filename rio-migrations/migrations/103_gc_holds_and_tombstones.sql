-- Commentary: see rio-migrations/src/migrations.rs M_103

CREATE TABLE path_tenant_tombstones (
    store_path_hash     BYTEA       NOT NULL,
    store_path          TEXT        NOT NULL,
    tenant_id           UUID        NOT NULL,
    first_referenced_at TIMESTAMPTZ NOT NULL,
    deriver             TEXT,
    swept_at            TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX path_tenant_tombstones_hash_idx
    ON path_tenant_tombstones (store_path_hash);

CREATE TABLE realisation_tombstones (
    drv_hash     BYTEA       NOT NULL,
    output_name  TEXT        NOT NULL,
    output_path  TEXT        NOT NULL,
    output_hash  BYTEA       NOT NULL,
    swept_at     TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE INDEX realisation_tombstones_path_idx
    ON realisation_tombstones (output_path);

CREATE TABLE gc_holds (
    hold_id     UUID        PRIMARY KEY DEFAULT gen_random_uuid(),
    scope       TEXT        NOT NULL CHECK (scope IN ('global', 'tenant')),
    tenant_id   UUID        REFERENCES tenants (tenant_id) ON DELETE CASCADE,
    reason      TEXT        NOT NULL,
    created_by  TEXT        NOT NULL,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    expires_at  TIMESTAMPTZ,
    released_at TIMESTAMPTZ,
    CONSTRAINT gc_holds_tenant_scope CHECK ((scope = 'tenant') = (tenant_id IS NOT NULL))
);
CREATE INDEX gc_holds_active_idx ON gc_holds (scope) WHERE released_at IS NULL;
