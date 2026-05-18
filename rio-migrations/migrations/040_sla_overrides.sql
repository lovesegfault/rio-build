CREATE TABLE sla_overrides (
  id BIGSERIAL PRIMARY KEY,
  pname TEXT NOT NULL,
  system TEXT,
  tenant TEXT,
  cluster TEXT,
  tier TEXT,
  p50_secs DOUBLE PRECISION,
  p90_secs DOUBLE PRECISION,
  p99_secs DOUBLE PRECISION,
  cores DOUBLE PRECISION,
  mem_bytes BIGINT,
  capacity_type TEXT,
  expires_at TIMESTAMPTZ,
  created_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  created_by TEXT
);
CREATE INDEX sla_overrides_lookup_idx ON sla_overrides (pname, system, tenant);
