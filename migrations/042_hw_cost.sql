CREATE TABLE hw_cost_factors (
  region TEXT NOT NULL,
  az TEXT NOT NULL,
  instance_type TEXT NOT NULL,
  capacity_type TEXT NOT NULL,
  price_per_vcpu_hr DOUBLE PRECISION NOT NULL,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (region, az, instance_type, capacity_type)
);
CREATE TABLE sla_ema_state (
  key TEXT PRIMARY KEY,
  value DOUBLE PRECISION NOT NULL,
  numerator DOUBLE PRECISION,
  denominator DOUBLE PRECISION,
  updated_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
ALTER TABLE builds ADD COLUMN attempted_candidates JSONB;
