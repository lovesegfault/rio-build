CREATE INDEX IF NOT EXISTS idx_narinfo_store_path_pattern
    ON narinfo (store_path text_pattern_ops);
