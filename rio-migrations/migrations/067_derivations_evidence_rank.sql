-- Commentary: see rio-migrations/src/migrations.rs M_067
ALTER TABLE derivations
    ADD COLUMN evidence_rank TEXT NOT NULL DEFAULT 'unverified_claim'
    CONSTRAINT derivations_evidence_rank_valid CHECK (
        evidence_rank IN (
            'unverified_claim', 'content_bound_claim',
            'path_bound_bytes', 'verified_built'
        )
    );
