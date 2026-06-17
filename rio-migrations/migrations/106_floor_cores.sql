-- Commentary: see rio-migrations/src/migrations.rs M_106

ALTER TABLE derivations
  ADD COLUMN floor_cores integer NOT NULL DEFAULT 0;
