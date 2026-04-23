DROP VIEW IF EXISTS hw_perf_factors;

ALTER TABLE hw_perf_samples
    ALTER COLUMN factor TYPE jsonb
    USING jsonb_build_object('alu', factor);

ALTER TABLE hw_perf_samples
    ADD COLUMN submitting_tenant TEXT;

CREATE INDEX hw_perf_samples_tenant_idx ON hw_perf_samples (hw_class, submitting_tenant);
