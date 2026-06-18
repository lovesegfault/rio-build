-- Commentary: see rio-migrations/src/migrations.rs M_115
CREATE INDEX IF NOT EXISTS idx_derivations_drv_path ON derivations (drv_path);
