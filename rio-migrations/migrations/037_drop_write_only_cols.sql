ALTER TABLE build_history
    DROP COLUMN IF EXISTS ema_output_size_bytes,
    DROP COLUMN IF EXISTS size_class,
    DROP COLUMN IF EXISTS misclassification_count;

ALTER TABLE builds DROP COLUMN IF EXISTS requestor;

ALTER TABLE build_logs DROP COLUMN IF EXISTS byte_size;
