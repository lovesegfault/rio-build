CREATE TABLE sla_config_epoch (
    cluster TEXT PRIMARY KEY,
    reference_hw_class TEXT,
    epoch BIGINT NOT NULL DEFAULT 0,
    set_at TIMESTAMPTZ NOT NULL DEFAULT now()
);
