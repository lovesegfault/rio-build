-- Commentary: see rio-migrations/src/migrations.rs M_083
ALTER TABLE materialization_jobs
    ADD COLUMN park_began_at TIMESTAMPTZ;
