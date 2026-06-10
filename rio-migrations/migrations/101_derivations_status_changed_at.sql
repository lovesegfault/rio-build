-- Commentary: see rio-migrations/src/migrations.rs M_101
ALTER TABLE derivations ADD COLUMN status_changed_at TIMESTAMPTZ NOT NULL DEFAULT now();
