-- Sticky first-failure derivation hash, paired with error_summary.
-- NULL = no failure recorded (or pre-072 row). See M_072 in
-- src/migrations.rs for rationale.
ALTER TABLE builds
    ADD COLUMN failed_derivation TEXT;
