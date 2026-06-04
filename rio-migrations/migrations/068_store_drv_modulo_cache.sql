-- Commentary: see rio-migrations/src/migrations.rs M_068
CREATE TABLE drv_modulo_cache (
    drv_path_hash   BYTEA PRIMARY KEY,
    drv_path        TEXT NOT NULL,
    modulo_hash     BYTEA NOT NULL
        CONSTRAINT drv_modulo_cache_hash_len CHECK (octet_length(modulo_hash) = 32),
    ia_output_paths JSONB NOT NULL DEFAULT '{}'::jsonb,
    deferred        BOOLEAN NOT NULL DEFAULT FALSE,
    created_at      TIMESTAMPTZ NOT NULL DEFAULT now()
);
