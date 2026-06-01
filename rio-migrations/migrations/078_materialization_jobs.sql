-- Commentary: see rio-migrations/src/migrations.rs M_078

-- Substitution-replacement Phase A (additive, dormant): the durable
-- per-(build, derivation) wanted relation, the materialization-job
-- table and derived interest view, the attempt-kind discriminator on
-- execution rows, and pin-kind discrimination on live pins. Nothing
-- here is written by any code path while the materialization flags
-- are off; every ALTER is DEFAULT-valued so existing writers are
-- untouched.

-- (1) The durable per-(build, derivation) wanted relation — the
--     PG-authoritative successor of the in-memory wanted_by_build
--     contributions (AW4). Written by the merge transaction when
--     materialization dispatch is enabled.
CREATE TABLE build_wanted_outputs (
    build_id            UUID        NOT NULL,
    derivation_id       UUID        NOT NULL,
    -- '{}' = all declared outputs wanted (the 062 convention, kept;
    -- saturation applied in exactly one place: the union query in
    -- rio-scheduler/src/db/wanted.rs).
    wanted_output_names TEXT[]      NOT NULL DEFAULT '{}',
    recorded_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    PRIMARY KEY (build_id, derivation_id)
);
CREATE INDEX build_wanted_outputs_by_drv ON build_wanted_outputs (derivation_id);

-- (2) Materialization jobs. State machine:
--     pending -> resolved_success | resolved_unobtainable |
--                resolved_from_source | obsolete | cancelled.
--     "Claimed" is not a job state: a claim is an open attempt
--     (assignments + drv_executions rows); the job row is untouched
--     until consumption.
CREATE TABLE materialization_jobs (
    job_id             UUID        PRIMARY KEY,
    derivation_id      UUID        NOT NULL,
    drv_hash           TEXT        NOT NULL,
    -- Upstream-selection context (creating build's tenant). Nullable:
    -- single-tenant/dev builds have none; the executor re-resolves at
    -- execution time and reports InfraFailure when no context exists.
    tenant_id          UUID,
    origin             TEXT        NOT NULL
        CHECK (origin IN ('pruned', 'cache_opportunity', 'stale_reset', 'reprobe')),
    state              TEXT        NOT NULL DEFAULT 'pending'
        CHECK (state IN ('pending', 'resolved_success', 'resolved_unobtainable',
                         'resolved_from_source', 'obsolete', 'cancelled')),
    -- Infra-budget exhaustion backoff (design §2.5). NULL = not parked.
    park_until         TIMESTAMPTZ,
    -- Serving generation at creation (fence audit trail).
    created_generation BIGINT      NOT NULL,
    -- The attempt that resolved it (exec_id identity, closes D7).
    resolution_exec_id UUID,
    created_at         TIMESTAMPTZ NOT NULL DEFAULT now(),
    resolved_at        TIMESTAMPTZ
);
-- The dedup the C3 fix protects: at most one unresolved job per
-- derivation, enforced by the database instead of in-memory checks.
CREATE UNIQUE INDEX materialization_jobs_unresolved
    ON materialization_jobs (derivation_id) WHERE state = 'pending';
-- The store's poll: pending jobs ordered by age (the open-attempt
-- anti-join happens at query time against assignments).
CREATE INDEX materialization_jobs_pending ON materialization_jobs (created_at)
    WHERE state = 'pending';

-- (3) Interest is DERIVED, never registered: a build is interested in
--     a job iff it is live and has a wanted-relation row for the
--     job's derivation (review finding AS-1).
CREATE VIEW materialization_interest AS
    SELECT j.job_id, w.build_id, w.wanted_output_names
      FROM materialization_jobs j
      JOIN build_wanted_outputs w USING (derivation_id)
      JOIN builds b ON b.build_id = w.build_id
     WHERE b.status IN ('pending', 'active');

-- (4) Attempt-kind discriminator: the work-class column (build vs
--     materialization). Successor of the transport discriminator
--     M_076 dropped; DEFAULT 'build' keeps the existing pull mint
--     untouched. The retry fold's kind partition, the establishment
--     sweep's branch, and the report intake's kind check all key on
--     this column and only this column (never on an ID prefix).
ALTER TABLE drv_executions ADD COLUMN attempt_kind TEXT NOT NULL DEFAULT 'build'
    CHECK (attempt_kind IN ('build', 'materialization'));

-- (5) Pin-kind discrimination (design §5.2): materialization pins are
--     released by the all-interest-terminal rule, never by the
--     build-input terminal-status sweeps. job_id set for
--     materialization pins only.
ALTER TABLE scheduler_live_pins ADD COLUMN pin_kind TEXT NOT NULL DEFAULT 'build_input'
    CHECK (pin_kind IN ('build_input', 'materialization'));
ALTER TABLE scheduler_live_pins ADD COLUMN job_id UUID;
