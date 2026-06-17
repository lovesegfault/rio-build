-- Commentary: see rio-migrations/src/migrations.rs M_107
ALTER TABLE materialization_jobs
    ADD COLUMN priority DOUBLE PRECISION NOT NULL DEFAULT 0;
