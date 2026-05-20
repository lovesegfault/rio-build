-- Commentary: see rio-migrations/src/migrations.rs M_065
CREATE TABLE leader_generation_claims (
    generation  BIGINT PRIMARY KEY,
    holder_id   TEXT NOT NULL DEFAULT '',
    claimed_at  TIMESTAMPTZ NOT NULL DEFAULT now()
);
