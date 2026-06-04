-- Commentary: see rio-migrations/src/migrations.rs M_066

-- Ingress-validated CA modular hash for content-addressed derivations:
-- the content-bound identity evidence the merge gate compares
-- (sched.merge.authoritative-conflict / authoritative-claim-no-redefine)
-- and the realisation key for floating-CA outputs. Persisting it keeps a
-- store-backed CA node's evidence across leader failover (its .drv bytes
-- are never persisted, so the hash cannot be recomputed the way an
-- authoritative row's is). NULL for non-CA derivations and for rows
-- whose creating submission carried no hash. Evidence only — never a
-- substitute for ingress validation of authoritative content.
ALTER TABLE derivations ADD COLUMN ca_modular_hash BYTEA;
