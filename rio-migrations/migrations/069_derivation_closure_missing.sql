-- Commentary: see rio-migrations/src/migrations.rs M_069
CREATE TABLE derivation_closure_missing (
    drv_hash TEXT NOT NULL,
    missing_child TEXT NOT NULL,
    PRIMARY KEY (drv_hash, missing_child)
);
