-- Scheduler tables for rio-build Phase 2a.
--
-- Tables: builds, derivations, derivation_edges, build_derivations,
--         assignments, build_history.
--
-- Schema matches docs/src/components/scheduler.md § Schema (pseudo-DDL) as of Phase 2a.

-- Build requests
CREATE TABLE builds (
    build_id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID,  -- nullable: unused in Phase 2a
    requestor       TEXT NOT NULL DEFAULT '',
    status          TEXT NOT NULL CHECK (status IN ('pending', 'active', 'succeeded', 'failed', 'cancelled')),
    priority_class  TEXT NOT NULL DEFAULT 'scheduled'
                    CHECK (priority_class IN ('ci', 'interactive', 'scheduled')),
    submitted_at    TIMESTAMPTZ NOT NULL DEFAULT now(),
    started_at      TIMESTAMPTZ,
    finished_at     TIMESTAMPTZ,
    error_summary   TEXT
);
CREATE INDEX builds_status_idx ON builds (status) WHERE status IN ('pending', 'active');

-- Derivation nodes in the global DAG
CREATE TABLE derivations (
    derivation_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID,  -- nullable: unused in Phase 2a
    drv_hash            TEXT NOT NULL,         -- input-addressed: store path; CA: modular derivation hash
    drv_path            TEXT NOT NULL,         -- full store path of the .drv (plan addition for DAG reconstruction)
    pname               TEXT,
    system              TEXT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN
        ('created', 'queued', 'ready', 'assigned', 'running', 'completed', 'failed', 'poisoned')),
    required_features   TEXT[] NOT NULL DEFAULT '{}',
    assigned_worker_id  TEXT,
    retry_count         INT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT derivations_drv_hash_uq UNIQUE (drv_hash)
);
CREATE INDEX derivations_status_idx ON derivations (status) WHERE status NOT IN ('completed', 'poisoned');

-- DAG edges between derivations
CREATE TABLE derivation_edges (
    parent_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    child_id    UUID NOT NULL REFERENCES derivations (derivation_id),
    is_cutoff   BOOLEAN NOT NULL DEFAULT FALSE,  -- future CA early cutoff support
    PRIMARY KEY (parent_id, child_id)
);
CREATE INDEX derivation_edges_child_idx ON derivation_edges (child_id);

-- Many-to-many mapping: which builds are interested in which derivations
CREATE TABLE build_derivations (
    build_id        UUID NOT NULL REFERENCES builds (build_id),
    derivation_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    PRIMARY KEY (build_id, derivation_id)
);
CREATE INDEX build_derivations_deriv_idx ON build_derivations (derivation_id);

-- Worker assignments with leader generation counter
CREATE TABLE assignments (
    assignment_id   UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    derivation_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    worker_id       TEXT NOT NULL,
    generation      BIGINT NOT NULL,           -- leader generation counter
    status          TEXT NOT NULL CHECK (status IN ('pending', 'acknowledged', 'completed', 'failed', 'cancelled')),
    assigned_at     TIMESTAMPTZ NOT NULL DEFAULT now(),
    completed_at    TIMESTAMPTZ
);
-- At most one active (pending or acknowledged) assignment per derivation
CREATE UNIQUE INDEX assignments_active_uq ON assignments (derivation_id)
    WHERE status IN ('pending', 'acknowledged');
CREATE INDEX assignments_worker_idx ON assignments (worker_id, status);

-- Duration estimation: running EMA per (pname, system)
CREATE TABLE build_history (
    pname               TEXT NOT NULL,
    system              TEXT NOT NULL,
    ema_duration_secs   DOUBLE PRECISION NOT NULL,
    sample_count        INT NOT NULL DEFAULT 0,
    last_updated        TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (pname, system)
);
