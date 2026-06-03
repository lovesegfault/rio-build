-- Commentary: see rio-migrations/src/migrations.rs M_093
DELETE FROM scheduler_live_pins WHERE pin_kind = 'materialization' AND job_id IS NULL;  -- defensive; expected 0 rows
ALTER TABLE scheduler_live_pins DROP CONSTRAINT scheduler_live_pins_pkey;
ALTER TABLE scheduler_live_pins ADD PRIMARY KEY (store_path_hash, drv_hash, pin_kind);
ALTER TABLE scheduler_live_pins ADD CONSTRAINT scheduler_live_pins_materialization_job CHECK (pin_kind <> 'materialization' OR job_id IS NOT NULL);
