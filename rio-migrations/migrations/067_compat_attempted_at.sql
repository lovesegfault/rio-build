-- Commentary: see rio-store/src/migrations.rs M_067
ALTER TABLE narinfo ADD COLUMN compat_attempted_at TIMESTAMPTZ;
