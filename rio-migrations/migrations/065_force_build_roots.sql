-- Commentary: see rio-migrations/src/migrations.rs M_065

-- Per-build force-build-roots flag (r[sched.merge.force-build-roots]).
-- Stamped from SubmitBuildRequest.force_build_roots at insert_build;
-- recovery re-reads it to rebuild BuildInfo after failover.
ALTER TABLE builds ADD COLUMN force_build_roots BOOLEAN NOT NULL DEFAULT FALSE;

-- Submission-root marker per (build, derivation). TRUE only for the
-- derivations that were roots of that build's submission (no parent edge
-- within the submission). Recovery joins this to re-derive the per-node
-- "do not substitute" sticky-OR for force_build_roots builds.
ALTER TABLE build_derivations ADD COLUMN is_root BOOLEAN NOT NULL DEFAULT FALSE;
