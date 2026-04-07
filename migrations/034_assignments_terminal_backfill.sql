UPDATE assignments a
SET status = CASE d.status
        WHEN 'completed' THEN 'completed'
        WHEN 'cancelled' THEN 'cancelled'
        ELSE 'failed'
    END,
    completed_at = COALESCE(a.completed_at, d.updated_at, now())
FROM derivations d
WHERE a.derivation_id = d.derivation_id
  AND a.status IN ('pending', 'acknowledged')
  AND d.status IN ('completed', 'poisoned', 'dependency_failed', 'cancelled', 'skipped');

ALTER TABLE assignments
    DROP CONSTRAINT assignments_derivation_id_fkey,
    ADD CONSTRAINT assignments_derivation_id_fkey
        FOREIGN KEY (derivation_id) REFERENCES derivations (derivation_id)
        ON DELETE CASCADE;
