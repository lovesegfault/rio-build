-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_075
--
-- Index build_derivations(exec_id) so GetDerivationLog's execution→tenant
-- attribution probe is an index lookup instead of a seq scan. Partial:
-- most rows never record an execution (cache hits, never-dispatched
-- terminals) and are never probed by exec. CONCURRENTLY: precedent
-- 022/071. MUST be the only statement in this file
-- (implicit-transaction rule, see 022).
CREATE INDEX CONCURRENTLY IF NOT EXISTS build_derivations_exec_idx
    ON build_derivations (exec_id)
    WHERE exec_id IS NOT NULL;
