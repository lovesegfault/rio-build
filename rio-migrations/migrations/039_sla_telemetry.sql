ALTER TABLE build_samples
  ADD COLUMN cpu_limit_cores       DOUBLE PRECISION,
  ADD COLUMN peak_cpu_cores        DOUBLE PRECISION,
  ADD COLUMN cpu_seconds_total     DOUBLE PRECISION,
  ADD COLUMN peak_disk_bytes       BIGINT,
  ADD COLUMN peak_io_pressure_pct  DOUBLE PRECISION,
  ADD COLUMN version               TEXT,
  ADD COLUMN tenant                TEXT NOT NULL DEFAULT '',
  ADD COLUMN hw_class              TEXT,
  ADD COLUMN node_name             TEXT,
  ADD COLUMN enable_parallel_building BOOLEAN,
  ADD COLUMN prefer_local_build    BOOLEAN,
  ADD COLUMN outlier_excluded      BOOLEAN NOT NULL DEFAULT FALSE;

CREATE INDEX build_samples_key_idx ON build_samples (pname, system, tenant, completed_at DESC);
CREATE INDEX build_samples_incremental_idx ON build_samples (completed_at DESC);
