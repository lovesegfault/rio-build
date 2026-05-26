-- Commentary: see rio-migrations/src/migrations.rs M_067
ALTER TABLE narinfo ADD COLUMN compat_attempted_at TIMESTAMPTZ;
