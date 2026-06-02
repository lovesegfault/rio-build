-- Commentary: see rio-migrations/src/migrations.rs M_065

ALTER TABLE builds ADD COLUMN force_build_roots BOOLEAN NOT NULL DEFAULT FALSE;

ALTER TABLE build_derivations ADD COLUMN is_root BOOLEAN NOT NULL DEFAULT FALSE;
