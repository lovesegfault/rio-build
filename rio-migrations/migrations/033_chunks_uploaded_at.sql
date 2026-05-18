-- Commentary: see rio-store/src/migrations.rs M_033
ALTER TABLE chunks ADD COLUMN uploaded_at TIMESTAMPTZ;
UPDATE chunks SET uploaded_at = created_at;
