-- Commentary: see rio-migrations/src/migrations.rs M_084
ALTER TABLE drv_attempts ADD COLUMN attempt_kind TEXT NOT NULL DEFAULT 'build' CHECK (attempt_kind IN ('build','materialization'));
UPDATE drv_attempts a SET attempt_kind='materialization' FROM drv_executions e WHERE a.exec_id=e.exec_id AND e.attempt_kind='materialization';
UPDATE drv_executions SET source_node=NULL WHERE attempt_kind='materialization';  -- scrub bug_075's live mis-stamps
ALTER TABLE drv_executions ADD CONSTRAINT drv_executions_build_only_source_node CHECK (attempt_kind='build' OR source_node IS NULL);
