-- Phase 3b: scheduler state recovery columns.
--
-- Previously all state was in-memory only; on scheduler restart the
-- new leader started with an EMPTY DAG and builds were lost (clients
-- saw "unknown build" on WatchBuild). These columns let the new
-- leader reconstruct the full DAG + build state from PG.
--
-- Design: recover_from_pg() is called on LeaderAcquired transition
-- (non-blocking -- lease loop keeps renewing while recovery runs).
-- Clears in-mem state, loads all non-terminal builds + derivations +
-- edges + build_derivations + assignments, rebuilds DerivationDag,
-- recomputes critical-path priorities, repopulates ready queue.
--
-- Lossy fields that CAN'T be persisted: Instant timestamps
-- (ready_at, running_since, poisoned_at, backoff_until), drv_content
-- (ATerm bytes -- workers re-fetch from store if empty). These reset
-- to conservative defaults on recovery (Instant::now() for time
-- fields; empty for drv_content which triggers the pre-D8 worker
-- fetch-from-store path).
--
-- Migration 004 runs BEFORE C track's 005_gc.sql (execution order:
-- T-A-S-B-E-D-C-F-G-H). sqlx rejects out-of-order migrations.

-- ----------------------------------------------------------------------------
-- builds: recovery needs keep_going + options to rebuild BuildInfo
-- ----------------------------------------------------------------------------

-- keep_going: previously only in-memory (BuildInfo.keep_going).
-- Controls whether a single derivation failure aborts the build or
-- lets independent branches continue. Critical for recovery -- a
-- recovered build that lost this flag would default to wrong
-- semantics on the first failure post-recovery.
ALTER TABLE builds ADD COLUMN keep_going BOOLEAN NOT NULL DEFAULT true;

-- BuildOptions serialized as JSONB. Contains max_silent_time,
-- build_timeout, build_cores. Small (3 fields, ~50 bytes); JSONB
-- gives us schema flexibility without ALTER on option additions.
--
-- NULL for rows written before this migration (recovery code treats
-- as BuildOptions::default() -- all zeroes = unlimited).
ALTER TABLE builds ADD COLUMN options_json JSONB;

-- ----------------------------------------------------------------------------
-- derivations: recovery needs output paths + names + fod flag + failed workers
-- ----------------------------------------------------------------------------

-- Expected output paths (from proto DerivationNode.expected_output_
-- paths at merge time). Used by: cache-check (merge.rs), transfer-
-- cost scoring (D6), gc_roots (C4). Recovery without these would
-- break best_worker's bloom scoring AND leave live-build outputs
-- unprotected from GC.
ALTER TABLE derivations ADD COLUMN expected_output_paths TEXT[] NOT NULL DEFAULT '{}';

-- Output names (e.g., ["out", "dev"]). Forwarded into WorkAssignment.
-- Without this, a recovered derivation would dispatch with empty
-- output_names and the worker wouldn't know what to upload.
ALTER TABLE derivations ADD COLUMN output_names TEXT[] NOT NULL DEFAULT '{}';

-- is_fixed_output: controls worker's FOD handling (network access +
-- output hash verification). Recovery without this would dispatch
-- FODs as regular builds -- sandbox would block the fetch.
ALTER TABLE derivations ADD COLUMN is_fixed_output BOOLEAN NOT NULL DEFAULT false;

-- failed_workers: feeds best_worker exclusion + poison detection.
-- Recovery without this would reset the history -- a derivation that
-- failed on workers A and B pre-restart would retry on A or B post-
-- restart (wasteful; might oscillate again).
--
-- TEXT[] of worker_id strings. Set is unordered; PG array preserves
-- order but recovery code rebuilds the HashSet from it.
ALTER TABLE derivations ADD COLUMN failed_workers TEXT[] NOT NULL DEFAULT '{}';

-- 'cancelled' status: Phase 3b adds DerivationStatus::Cancelled.
-- The CHECK constraint lists allowed values; add the new one.
-- DROP + ADD is the standard PG idiom (can't ALTER a CHECK body).
ALTER TABLE derivations DROP CONSTRAINT derivations_status_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_status_check
    CHECK (status IN ('created', 'queued', 'ready', 'assigned', 'running',
                      'completed', 'failed', 'poisoned', 'dependency_failed',
                      'cancelled'));

-- Also exclude 'cancelled' from the partial index (it's terminal).
DROP INDEX derivations_status_idx;
CREATE INDEX derivations_status_idx ON derivations (status)
    WHERE status NOT IN ('completed', 'poisoned', 'dependency_failed', 'cancelled');
