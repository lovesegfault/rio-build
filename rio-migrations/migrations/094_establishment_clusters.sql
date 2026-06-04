-- Commentary: see rio-migrations/src/migrations.rs M_094
CREATE FUNCTION establishment_clusters(window_span interval DEFAULT '30 minutes')
RETURNS TABLE(
    source_node    text,
    distinct_drvs  bigint,
    establishments bigint,
    first_seen     timestamptz,
    last_seen      timestamptz
)
LANGUAGE sql STABLE
AS $$
    SELECT a.source_node,
           count(DISTINCT a.derivation_id) AS distinct_drvs,
           count(*)                        AS establishments,
           min(a.recorded_at)              AS first_seen,
           max(a.recorded_at)              AS last_seen
    FROM drv_attempts a
    WHERE a.outcome_class = 'executor_crash'
      AND a.termination_reason = 'unreported'
      AND a.recorded_at > now() - window_span
    GROUP BY a.source_node
    ORDER BY 2 DESC
$$;
