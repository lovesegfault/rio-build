-- Commentary: see rio-migrations/src/migrations.rs M_092
ALTER TABLE manifests ADD COLUMN claim_phase TEXT CHECK (claim_phase IN ('downloading','budget_parked','persisting'));
