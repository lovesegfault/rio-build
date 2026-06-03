-- M_074: 32-byte width domain for persisted modular hashes.
-- Rationale and genealogy: rio-migrations/src/migrations.rs (M_074).
ALTER TABLE derivations
    ADD CONSTRAINT derivations_ca_modular_hash_width CHECK (
        ca_modular_hash IS NULL OR octet_length(ca_modular_hash) = 32
    ),
    ADD CONSTRAINT derivations_ca_modular_hash_stripped_width CHECK (
        ca_modular_hash_stripped IS NULL OR octet_length(ca_modular_hash_stripped) = 32
    );
