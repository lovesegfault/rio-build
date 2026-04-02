ALTER TABLE builds
    ADD COLUMN total_drvs     INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN completed_drvs INTEGER NOT NULL DEFAULT 0,
    ADD COLUMN cached_drvs    INTEGER NOT NULL DEFAULT 0;

UPDATE builds b SET
    total_drvs = COALESCE(agg.total, 0),
    completed_drvs = COALESCE(agg.completed, 0),
    cached_drvs = COALESCE(agg.cached, 0)
FROM (
    SELECT
        bd.build_id,
        COUNT(bd.derivation_id) AS total,
        COUNT(*) FILTER (WHERE d.status = 'completed') AS completed,
        COUNT(*) FILTER (
            WHERE d.status = 'completed'
              AND NOT EXISTS (SELECT 1 FROM assignments a
                              WHERE a.derivation_id = d.derivation_id)
        ) AS cached
    FROM build_derivations bd
    JOIN derivations d ON bd.derivation_id = d.derivation_id
    GROUP BY bd.build_id
) agg
WHERE b.build_id = agg.build_id;
