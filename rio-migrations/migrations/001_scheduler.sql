-- Scheduler tables for rio-build. Phase 3a baseline.
--
-- Squashed from the phase 2a-2c migration chain:
--   001_scheduler_tables.sql  base tables
--   003_add_dependency_failed.sql  'dependency_failed' in status CHECK
--   004_exclude_dependency_failed_from_index.sql  partial-index exclusion
--   005_build_logs.sql  build_logs table
--   006_phase2c.sql  build_history resource-tracking columns
--
-- Schema matches docs/src/components/scheduler.md as of phase 3a entry.

-- ----------------------------------------------------------------------------
-- Build requests
-- ----------------------------------------------------------------------------

CREATE TABLE builds (
    build_id        UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id       UUID,  -- nullable: multi-tenancy deferred to Phase 4
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

-- ----------------------------------------------------------------------------
-- Derivation nodes in the global DAG
--
-- 'dependency_failed' is terminal: when a derivation is poisoned, its parents
-- (transitively) can never complete. The scheduler cascades DependencyFailed
-- to those parents so keepGoing builds terminate instead of hanging forever.
-- Maps to Nix BuildStatus::DependencyFailed = 10.
-- ----------------------------------------------------------------------------

CREATE TABLE derivations (
    derivation_id       UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    tenant_id           UUID,  -- nullable: multi-tenancy deferred to Phase 4
    drv_hash            TEXT NOT NULL,         -- input-addressed: store path; CA: modular derivation hash
    drv_path            TEXT NOT NULL,         -- full store path of the .drv (for DAG reconstruction)
    pname               TEXT,
    system              TEXT NOT NULL,
    status              TEXT NOT NULL CHECK (status IN
        ('created', 'queued', 'ready', 'assigned', 'running',
         'completed', 'failed', 'poisoned', 'dependency_failed')),
    required_features   TEXT[] NOT NULL DEFAULT '{}',
    assigned_worker_id  TEXT,
    retry_count         INT NOT NULL DEFAULT 0,
    created_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    updated_at          TIMESTAMPTZ NOT NULL DEFAULT now(),
    CONSTRAINT derivations_drv_hash_uq UNIQUE (drv_hash)
);
-- Partial index over non-terminal states. dependency_failed is terminal
-- (like completed/poisoned) so indexing those rows would waste space.
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed');

-- ----------------------------------------------------------------------------
-- DAG edges between derivations
-- ----------------------------------------------------------------------------

CREATE TABLE derivation_edges (
    parent_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    child_id    UUID NOT NULL REFERENCES derivations (derivation_id),
    is_cutoff   BOOLEAN NOT NULL DEFAULT FALSE,  -- future CA early cutoff support
    PRIMARY KEY (parent_id, child_id)
);
CREATE INDEX derivation_edges_child_idx ON derivation_edges (child_id);

-- ----------------------------------------------------------------------------
-- Many-to-many: which builds are interested in which derivations
-- ----------------------------------------------------------------------------

CREATE TABLE build_derivations (
    build_id        UUID NOT NULL REFERENCES builds (build_id),
    derivation_id   UUID NOT NULL REFERENCES derivations (derivation_id),
    PRIMARY KEY (build_id, derivation_id)
);
CREATE INDEX build_derivations_deriv_idx ON build_derivations (derivation_id);

-- ----------------------------------------------------------------------------
-- Worker assignments with leader generation counter
-- ----------------------------------------------------------------------------

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

-- ----------------------------------------------------------------------------
-- Duration + resource estimation: running EMA per (pname, system)
--
-- Workers report resource fields in CompletionReport from per-build cgroup v2:
--   peak_memory_bytes   <- cgroup memory.peak (tree-wide, kernel-tracked)
--   peak_cpu_cores      <- cgroup cpu.stat usage_usec, polled 1Hz, delta/elapsed
--   output_size_bytes   <- sum of uploaded NAR sizes
-- Scheduler maintains EMA here alongside ema_duration_secs (same alpha=0.3).
--
-- Phase2c correction: ema_peak_memory_bytes rows written before I2 measured
-- the nix-daemon's RSS (~10MB regardless of builder memory -- daemon forks
-- the builder, waitpid()s, builder footprint never in daemon's /proc).
-- cgroup memory.peak fixes that. No migration: EMA alpha=0.3 washes bad
-- data out in ~10 completions (0.7^10 ~= 2.8% old value remains).
--
-- ema_peak_cpu_cores: wired as of I3. Not yet used for size-class routing
-- (classify() bumps on memory only) but the data accumulates now so future
-- cpu-bump logic is a pure-estimator change.
--
-- size_class: last class this (pname,system) was routed to. Informational
-- for dashboards; classify() reads ema_duration + ema_peak_memory, not this.
--
-- misclassification_count: incremented when a build exceeds 2x its class
-- cutoff. Phase 4 adaptive rebalancer uses this to detect
-- systematically-wrong cutoffs.
-- ----------------------------------------------------------------------------

CREATE TABLE build_history (
    pname                   TEXT NOT NULL,
    system                  TEXT NOT NULL,
    ema_duration_secs       DOUBLE PRECISION NOT NULL,
    sample_count            INT NOT NULL DEFAULT 0,
    last_updated            TIMESTAMPTZ NOT NULL DEFAULT now(),
    ema_peak_memory_bytes   DOUBLE PRECISION,
    ema_peak_cpu_cores      DOUBLE PRECISION,
    ema_output_size_bytes   DOUBLE PRECISION,
    size_class              TEXT,
    misclassification_count INTEGER NOT NULL DEFAULT 0,
    PRIMARY KEY (pname, system)
);

-- ----------------------------------------------------------------------------
-- Build log S3-blob metadata. One row per (build_id, drv_hash) pair.
--
-- A derivation builds exactly once even if N builds want it (DAG merging).
-- LogFlusher writes ONE S3 blob and N rows here (one per interested build,
-- all with the same s3_key). The dashboard's "logs for build X" query joins
-- on build_id; the dedup on s3_key is invisible to it.
--
-- is_complete: the 30s periodic flush writes rows with is_complete=false
-- (snapshot, derivation still running). The on-completion flush UPSERTs the
-- final row with is_complete=true. AdminService.GetBuildLogs checks the ring
-- buffer first (newer than any snapshot); only falls back to S3 for
-- is_complete=true rows (derivation has finished, ring buffer is gone).
-- ----------------------------------------------------------------------------

CREATE TABLE build_logs (
    id              UUID PRIMARY KEY DEFAULT gen_random_uuid(),
    build_id        UUID NOT NULL REFERENCES builds (build_id),
    drv_hash        TEXT NOT NULL,
    s3_key          TEXT NOT NULL,      -- logs/{build_id}/{drv_hash}.log.gz
    line_count      BIGINT NOT NULL,
    byte_size       BIGINT NOT NULL,    -- gzipped size (the S3 object size)
    is_complete     BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now(),
    -- UPSERT target: on-completion flush replaces any periodic snapshot.
    CONSTRAINT build_logs_build_drv_uq UNIQUE (build_id, drv_hash)
);

-- Dashboard query: "all logs for build X, ordered by derivation".
CREATE INDEX build_logs_build_idx ON build_logs (build_id);
