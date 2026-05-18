CREATE TABLE hw_perf_samples (
  id BIGSERIAL PRIMARY KEY,
  hw_class TEXT NOT NULL,
  pod_id TEXT NOT NULL,
  factor DOUBLE PRECISION NOT NULL,
  measured_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE TABLE interrupt_samples (
  id BIGSERIAL PRIMARY KEY,
  hw_class TEXT NOT NULL,
  kind TEXT NOT NULL,
  value DOUBLE PRECISION NOT NULL,
  at TIMESTAMPTZ NOT NULL DEFAULT now()
);
CREATE VIEW hw_perf_factors AS
  SELECT hw_class, percentile_cont(0.5) WITHIN GROUP (ORDER BY factor) AS factor
  FROM hw_perf_samples GROUP BY hw_class HAVING count(DISTINCT pod_id) >= 3;
