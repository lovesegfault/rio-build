CREATE INDEX hw_perf_samples_recent_idx ON hw_perf_samples (hw_class, measured_at DESC);
DROP VIEW hw_perf_factors;
CREATE VIEW hw_perf_factors AS
  SELECT hw_class, percentile_cont(0.5) WITHIN GROUP (ORDER BY factor) AS factor
  FROM hw_perf_samples WHERE measured_at > now() - interval '7 days'
  GROUP BY hw_class HAVING count(DISTINCT pod_id) >= 3;
