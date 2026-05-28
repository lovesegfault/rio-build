-- Commentary: see rio-migrations/src/migrations.rs M_075
--
-- Retry-formal Phase-2 coda: drop the three frozen retry mirror columns.
-- Retry/poison budgets are folds over the drv_attempts ledger (068) and
-- survive failover through it; the per-counter mirror writers retired at
-- the Phase-1b cutover, and the transitional legacy seed that still read
-- these columns is removed together with this migration. poisoned_at is
-- NOT part of the drop (poison lifecycle, not a counter mirror).
-- Metadata-only.
ALTER TABLE derivations
    DROP COLUMN IF EXISTS retry_count,
    DROP COLUMN IF EXISTS failed_builders,
    DROP COLUMN IF EXISTS resubmit_cycles;
