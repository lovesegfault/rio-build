-- Commentary: see rio-migrations/src/migrations.rs M_090
CREATE TABLE gc_collect_state (
  singleton BOOLEAN PRIMARY KEY DEFAULT TRUE CHECK (singleton),
  cycle_epoch BIGINT NOT NULL DEFAULT 0,
  last_live_cycle_at TIMESTAMPTZ, cursor BYTEA, backlog_estimate BIGINT,
  last_mark_set_size BIGINT, last_would_collect BIGINT,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now());
INSERT INTO gc_collect_state (singleton) VALUES (TRUE);
