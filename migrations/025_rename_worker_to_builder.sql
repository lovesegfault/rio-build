-- Commentary: see rio-store/src/migrations.rs M_025
ALTER TABLE derivations RENAME COLUMN assigned_worker_id TO assigned_builder_id;
ALTER TABLE assignments RENAME COLUMN worker_id TO builder_id;
ALTER INDEX assignments_worker_idx RENAME TO assignments_builder_idx;
ALTER TABLE derivations RENAME COLUMN failed_workers TO failed_builders;
