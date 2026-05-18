-- Commentary: see rio-store/src/migrations.rs M_023
ALTER TABLE chunks ADD CONSTRAINT chunks_refcount_nonneg CHECK (refcount >= 0);
