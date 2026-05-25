-- Commentary: see rio-migrations/src/migrations.rs M_066
CREATE INDEX narinfo_compat_pending_idx
    ON narinfo (registration_time)
    WHERE compat_file_hash IS NULL;
