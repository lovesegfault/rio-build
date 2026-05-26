-- Commentary: see rio-migrations/src/migrations.rs M_068

-- The scheduler-owned durable attempt ledger: one row per attempt or
-- reset event, keyed by the DAG key (derivations.derivation_id).
-- Writer: rio-scheduler only. Retention: scheduler-owned (deliberately
-- NOT under rio-store's log TTL sweep) — hence no FK to drv_executions.
CREATE TABLE drv_attempts (
    attempt_id         UUID        PRIMARY KEY,
    derivation_id      UUID        NOT NULL,
    exec_id            UUID,
    executor_id        TEXT,
    event_kind         TEXT        NOT NULL
        CHECK (event_kind IN ('attempt', 'reset')),
    outcome_class      TEXT        NOT NULL
        CHECK (outcome_class IN
            ('transient', 'infra', 'exempt_infra', 'timeout', 'permanent',
             'cascade', 'backstop', 'disconnected', 'executor_crash',
             'fleet_exhaust', 'resubmit_reset', 'cache_hit_clear',
             'poison_cleared')),
    termination_reason TEXT,
    reporting_party    TEXT        NOT NULL,
    exempt             BOOLEAN     NOT NULL DEFAULT FALSE,
    floor_promoted     BOOLEAN     NOT NULL DEFAULT FALSE,
    floor_at_cap       BOOLEAN     NOT NULL DEFAULT FALSE,
    error_msg          TEXT,
    final_line_count   BIGINT,
    resubmit_cycle     INT         NOT NULL DEFAULT 0,
    occurred_at        TIMESTAMPTZ NOT NULL,
    recorded_at        TIMESTAMPTZ NOT NULL DEFAULT now()
);

-- Suffix load: WHERE derivation_id = ANY($1) ORDER BY (recorded_at, attempt_id).
CREATE INDEX drv_attempts_derivation_recorded
    ON drv_attempts (derivation_id, recorded_at);

-- Reset-row lookup serving the suffix query's cut point (rows at-or-after
-- the most recent event_kind='reset' row per derivation).
CREATE INDEX drv_attempts_reset
    ON drv_attempts (derivation_id, recorded_at)
    WHERE event_kind = 'reset';

-- One execution = at most one attempt row: the schema half of
-- "NoDoubleCount stays a schema property". Rows without an exec_id
-- (cascade victims, fleet-exhaust markers, resets, never-dispatched
-- attempts) are outside the partial index and unconstrained.
CREATE UNIQUE INDEX drv_attempts_exec_id_uniq
    ON drv_attempts (exec_id)
    WHERE exec_id IS NOT NULL;
