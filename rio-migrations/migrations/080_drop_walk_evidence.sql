-- ═══════════════════════════════════════════════════════════════════
-- Migration 080 — retire the walk-era evidence surface
--
-- Substitution-replacement Phase D′ (design §4/§8, owner GO
-- 2026-06-01). The scheduler walk, the Substituting status, and the
-- evidence breadcrumbs were deleted from the binaries before this
-- migration ships (D′.1); no deployable binary reads any of these.
-- Successors: topdown_pruned → materialization_jobs.origin='pruned';
-- closure_hole → durable-relation classification at consumption
-- (the strict three-part criterion); wanted_output_names (stored
-- union) → the build_wanted_outputs live join.
--
-- The 038 partial index (derivations_status_idx) predicates on the
-- terminal statuses only — 'substituting' is not in its WHERE list,
-- so no index DDL is needed here.
-- ═══════════════════════════════════════════════════════════════════

-- Leftover walk-era rows (the binaries' transitional decode arm
-- absorbed these as queued since D′.1; make it durable + impossible).
UPDATE derivations SET status = 'queued', updated_at = now()
 WHERE status = 'substituting';

ALTER TABLE derivations DROP CONSTRAINT derivations_status_check;
ALTER TABLE derivations ADD CONSTRAINT derivations_status_check
    CHECK (status IN ('created', 'queued', 'ready', 'assigned', 'running',
                      'completed', 'failed', 'poisoned',
                      'dependency_failed', 'cancelled', 'skipped'));

ALTER TABLE derivations DROP COLUMN topdown_pruned;
ALTER TABLE derivations DROP COLUMN closure_hole;
ALTER TABLE derivations DROP COLUMN wanted_output_names;
