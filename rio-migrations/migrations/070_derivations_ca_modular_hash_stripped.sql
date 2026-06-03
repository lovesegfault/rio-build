-- Segregated preservation column for stripped (unverifiable) declared
-- CA modular hashes. See M_070 in src/migrations.rs for rationale.
ALTER TABLE derivations
    ADD COLUMN ca_modular_hash_stripped BYTEA;
