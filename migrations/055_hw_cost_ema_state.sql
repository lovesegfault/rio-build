ALTER TABLE hw_cost_factors
    ADD COLUMN price_ema_state jsonb,
    ADD COLUMN lambda_num_ema double precision NOT NULL DEFAULT 0,
    ADD COLUMN lambda_den_ema double precision NOT NULL DEFAULT 0,
    ADD COLUMN node_count_ema double precision NOT NULL DEFAULT 0;
