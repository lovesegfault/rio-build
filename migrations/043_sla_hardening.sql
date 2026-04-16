CREATE INDEX hw_perf_samples_recent_idx ON hw_perf_samples (hw_class, measured_at DESC);
DROP VIEW hw_perf_factors;
CREATE VIEW hw_perf_factors AS
  SELECT hw_class, percentile_cont(0.5) WITHIN GROUP (ORDER BY factor) AS factor
  FROM hw_perf_samples WHERE measured_at > now() - interval '7 days'
  GROUP BY hw_class HAVING count(DISTINCT pod_id) >= 3;
ALTER TABLE sla_ema_state ADD COLUMN cluster TEXT NOT NULL DEFAULT '';
ALTER TABLE sla_ema_state DROP CONSTRAINT sla_ema_state_pkey;
ALTER TABLE sla_ema_state ADD PRIMARY KEY (cluster, key);
ALTER TABLE interrupt_samples ADD COLUMN cluster TEXT NOT NULL DEFAULT '';
CREATE INDEX interrupt_samples_cluster_at_idx ON interrupt_samples (cluster, at);
