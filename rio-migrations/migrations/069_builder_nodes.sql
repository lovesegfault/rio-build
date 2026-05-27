-- Commentary: see rio-migrations/src/migrations.rs M_069
ALTER TABLE assignments
    ADD COLUMN node_name TEXT;

CREATE TABLE builder_nodes (
    node_name  TEXT PRIMARY KEY,
    first_seen TIMESTAMPTZ NOT NULL DEFAULT now(),
    last_seen  TIMESTAMPTZ NOT NULL DEFAULT now(),
    retired_at TIMESTAMPTZ
);
