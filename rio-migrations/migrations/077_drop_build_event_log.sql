-- Commentary: see rio-migrations/src/migrations.rs M_077

-- WatchBuild resumability-layer deletion (C4): drop the persisted
-- build-event mirror. The persister, the since_sequence replay, and the
-- recovery sequence seeding — the table's only writer and only readers —
-- are deleted in the same change set this ships in; gateway reconnection
-- is snapshot-first and reads nothing from PG. The index
-- idx_build_event_log_created goes with the table.
DROP TABLE IF EXISTS build_event_log;
