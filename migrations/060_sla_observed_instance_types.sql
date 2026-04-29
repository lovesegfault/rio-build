CREATE TABLE sla_observed_instance_types (
  cluster       TEXT        NOT NULL DEFAULT '',
  hw_class      TEXT        NOT NULL,
  capacity_type TEXT        NOT NULL CHECK (capacity_type IN ('spot','od')),
  instance_type TEXT        NOT NULL,
  cores         INTEGER     NOT NULL,
  mem_bytes     BIGINT      NOT NULL,
  last_observed TIMESTAMPTZ NOT NULL DEFAULT now(),
  PRIMARY KEY (cluster, hw_class, capacity_type, instance_type)
);
