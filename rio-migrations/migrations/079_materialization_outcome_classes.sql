-- Commentary: see rio-migrations/src/migrations.rs M_079

-- Substitution-replacement Phase A: the two materialization outcome
-- classes. Expands the 068 CHECK alphabet (DROP+ADD, never an edit to
-- the frozen 068 file). The new classes are written only by
-- materialization-attempt consumption and establishment, which are
-- dormant until the materialization flags enable them.
ALTER TABLE drv_attempts DROP CONSTRAINT drv_attempts_outcome_class_check;
ALTER TABLE drv_attempts ADD CONSTRAINT drv_attempts_outcome_class_check
    CHECK (outcome_class IN
        ('transient', 'infra', 'exempt_infra', 'timeout', 'permanent',
         'cascade', 'backstop', 'disconnected', 'executor_crash',
         'fleet_exhaust', 'resubmit_reset', 'cache_hit_clear',
         'poison_cleared',
         'materialization_unobtainable', 'materialization_infra'));
