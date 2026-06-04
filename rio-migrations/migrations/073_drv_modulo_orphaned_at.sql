-- Commentary: see rio-migrations/src/migrations.rs M_073
ALTER TABLE drv_modulo_cache
    ADD COLUMN orphaned_at TIMESTAMPTZ;

CREATE INDEX drv_modulo_cache_orphaned_idx
    ON drv_modulo_cache (orphaned_at)
 WHERE orphaned_at IS NOT NULL;
