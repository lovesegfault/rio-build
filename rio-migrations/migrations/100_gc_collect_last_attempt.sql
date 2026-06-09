-- Commentary: see rio-migrations/src/migrations.rs M_100
ALTER TABLE gc_collect_state ADD COLUMN last_attempt_at TIMESTAMPTZ;
