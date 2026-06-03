-- Persisted dispatch-resolve flag for derivations. NULL = legacy row
-- (pre-071). See M_071 in src/migrations.rs for rationale.
ALTER TABLE derivations
    ADD COLUMN needs_resolve BOOLEAN;
