-- Commentary: see rio-migrations/src/migrations.rs M_096
ALTER TABLE assignments ADD COLUMN claim_nonce UUID NULL;
