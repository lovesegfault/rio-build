-- Commentary: see rio-migrations/src/migrations.rs M_065

-- Authoritative inline derivation bytes for content-bound hook-fallback
-- nodes (DerivationNode.drv_content_authoritative): the .drv exists in
-- no store, so these bytes are the only copy and must survive scheduler
-- failover. NULL for every other derivation. No size CHECK: the bound
-- is enforced at SubmitBuild ingress from the shared Rust constant
-- (rio-common MAX_DRV_CONTENT_BYTES); a hard-coded SQL bound would just
-- recreate the producer/consumer drift this column's feature fixed.
ALTER TABLE derivations ADD COLUMN drv_content BYTEA;
