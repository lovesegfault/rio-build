-- Commentary: see rio-store/src/migrations.rs M_048
UPDATE builds b SET
    completed_drvs = COALESCE(agg.completed, 0),
    cached_drvs    = COALESCE(agg.cached, 0)
FROM (
    SELECT
        bd.build_id,
        COUNT(*) FILTER (WHERE d.status IN ('completed', 'skipped')) AS completed,
        COUNT(*) FILTER (
            WHERE d.status IN ('completed', 'skipped')
              AND NOT EXISTS (SELECT 1 FROM assignments a
                              WHERE a.derivation_id = d.derivation_id)
        ) AS cached
    FROM build_derivations bd
    JOIN derivations d ON bd.derivation_id = d.derivation_id
    GROUP BY bd.build_id
) agg
WHERE b.build_id = agg.build_id
  AND b.status IN ('succeeded', 'failed', 'cancelled');
