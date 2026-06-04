-- Commentary: see rio-migrations/src/migrations.rs M_082
ALTER TABLE materialization_jobs
    ADD COLUMN carried_realized_paths TEXT[];
