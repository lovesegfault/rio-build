-- Commentary: see rio-store/src/migrations.rs M_027
CREATE TABLE cluster_key_history (
    pubkey      TEXT PRIMARY KEY,
    created_at  TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at  TIMESTAMPTZ NULL
);
