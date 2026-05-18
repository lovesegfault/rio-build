-- Commentary: see rio-store/src/migrations.rs M_026
CREATE TABLE tenant_upstreams (
    id             SERIAL PRIMARY KEY,
    tenant_id      UUID NOT NULL REFERENCES tenants(tenant_id) ON DELETE CASCADE,
    url            TEXT NOT NULL,
    priority       INT  NOT NULL DEFAULT 50,
    trusted_keys   TEXT[] NOT NULL DEFAULT '{}',
    sig_mode       TEXT NOT NULL DEFAULT 'keep'
                   CHECK (sig_mode IN ('keep','add','replace')),
    created_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    UNIQUE (tenant_id, url)
);
CREATE INDEX tenant_upstreams_tenant_idx ON tenant_upstreams(tenant_id, priority);
