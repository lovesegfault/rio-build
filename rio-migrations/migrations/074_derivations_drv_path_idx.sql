-- no-transaction
-- Commentary: see rio-migrations/src/migrations.rs M_074
--
-- Index derivations(drv_path) so GetDerivationLog's drv-path-keyed
-- execution lookup (no pinned build) is an index probe instead of a
-- seq scan. CONCURRENTLY: derivations is written on every merge;
-- precedent 022/071. MUST be the only statement in this file
-- (implicit-transaction rule, see 022).
CREATE INDEX CONCURRENTLY IF NOT EXISTS derivations_drv_path_idx
    ON derivations (drv_path);
