-- Commentary: see rio-migrations/src/migrations.rs M_086
CREATE VIEW live_wanted_interest AS
    SELECT bd.build_id,
           bd.derivation_id,
           b.tenant_id,
           COALESCE(w.wanted_output_names, '{}'::text[]) AS wanted_output_names,
           (w.build_id IS NULL) AS saturated_default
      FROM build_derivations bd
      JOIN builds b ON b.build_id = bd.build_id
      LEFT JOIN build_wanted_outputs w
        ON w.build_id = bd.build_id AND w.derivation_id = bd.derivation_id
     WHERE b.status IN ('pending', 'active');
DROP VIEW materialization_interest;
CREATE VIEW materialization_interest AS
    SELECT j.job_id, i.build_id, i.wanted_output_names
      FROM materialization_jobs j
      JOIN live_wanted_interest i USING (derivation_id);
