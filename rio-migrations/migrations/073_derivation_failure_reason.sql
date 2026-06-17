-- Commentary: see rio-migrations/src/migrations.rs M_073

ALTER TABLE derivations ADD COLUMN failure_msg TEXT;
ALTER TABLE derivations ADD COLUMN failure_exec_id UUID;
