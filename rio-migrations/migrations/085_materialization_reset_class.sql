-- Commentary: see rio-migrations/src/migrations.rs M_085

ALTER TABLE drv_attempts DROP CONSTRAINT drv_attempts_outcome_class_check;
ALTER TABLE drv_attempts ADD CONSTRAINT drv_attempts_outcome_class_check
    CHECK (outcome_class IN
        ('transient', 'infra', 'exempt_infra', 'timeout', 'permanent',
         'cascade', 'backstop', 'disconnected', 'executor_crash',
         'fleet_exhaust', 'resubmit_reset', 'cache_hit_clear',
         'poison_cleared',
         'materialization_unobtainable', 'materialization_infra',
         'materialization_reset'));
