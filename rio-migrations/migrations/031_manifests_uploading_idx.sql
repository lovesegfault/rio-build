-- Commentary: see rio-store/src/migrations.rs M_031
CREATE INDEX idx_manifests_uploading_updated_at
    ON manifests (updated_at)
    WHERE status = 'uploading';
