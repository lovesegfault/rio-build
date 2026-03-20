//! Batch operations for `persist_merge_to_db` — build-derivation mapping +
//! UNNEST-based bulk inserts.

use std::collections::HashMap;

use sqlx::PgConnection;
use uuid::Uuid;

use super::{DerivationRow, SchedulerDb, encode_pg_text_array};

impl SchedulerDb {
    /// Link a build to a derivation.
    pub async fn insert_build_derivation(
        &self,
        build_id: Uuid,
        derivation_id: Uuid,
    ) -> Result<(), sqlx::Error> {
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            VALUES ($1, $2)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_id)
        .execute(&self.pool)
        .await?;

        Ok(())
    }

    // r[impl sched.db.batch-unnest]
    /// Batch-upsert derivations. Returns a map `drv_hash -> derivation_id`.
    ///
    /// Array parameters via `UNNEST`: 10 bind params total regardless of
    /// row count (vs `push_values`' 10×N, which hits PG's 65535-param
    /// limit at ~6553 rows). `RETURNING drv_hash` because PG doesn't
    /// guarantee `RETURNING` order matches `UNNEST` input order either.
    pub async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
    ) -> Result<HashMap<String, Uuid>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        // Decompose struct-of-rows into row-of-arrays. Ten parallel
        // Vecs, one per column. This IS a transpose — lives for the
        // duration of one INSERT, cheaper than N roundtrips.
        //
        // Nested-array columns (required_features, expected_output_paths,
        // output_names) can't unnest as text[][] — PG's multidim arrays
        // are rectangular, but per-row feature lists have variable
        // length. Encode as pg text[] literals ("{a,b,c}") and cast
        // back in the SELECT. sqlx doesn't expose a Vec<Vec<String>>
        // → text[][] Encode anyway.
        let mut drv_hash = Vec::with_capacity(rows.len());
        let mut drv_path = Vec::with_capacity(rows.len());
        let mut pname = Vec::with_capacity(rows.len());
        let mut system = Vec::with_capacity(rows.len());
        let mut status = Vec::with_capacity(rows.len());
        let mut required_features = Vec::with_capacity(rows.len());
        let mut expected_output_paths = Vec::with_capacity(rows.len());
        let mut output_names = Vec::with_capacity(rows.len());
        let mut is_fixed_output = Vec::with_capacity(rows.len());
        let mut is_ca = Vec::with_capacity(rows.len());
        for r in rows {
            drv_hash.push(r.drv_hash.as_str());
            drv_path.push(r.drv_path.as_str());
            pname.push(r.pname.as_deref());
            system.push(r.system.as_str());
            status.push(r.status.as_str());
            required_features.push(encode_pg_text_array(&r.required_features));
            expected_output_paths.push(encode_pg_text_array(&r.expected_output_paths));
            output_names.push(encode_pg_text_array(&r.output_names));
            is_fixed_output.push(r.is_fixed_output);
            is_ca.push(r.is_ca);
        }

        // ON CONFLICT: update the recovery columns too. A second
        // build requesting the same derivation may have fresher
        // expected_output_paths (same drv_hash → same outputs, so
        // this is idempotent in practice, but keeps the row in sync
        // with in-mem). status/retry etc stay as-is — those reflect
        // LIVE state, not merge-time snapshot.
        let result: Vec<(String, Uuid)> = sqlx::query_as(
            r#"
            INSERT INTO derivations
                (drv_hash, drv_path, pname, system, status, required_features,
                 expected_output_paths, output_names, is_fixed_output, is_ca)
            SELECT
                drv_hash, drv_path, pname, system, status,
                required_features::text[],
                expected_output_paths::text[],
                output_names::text[],
                is_fixed_output, is_ca
            FROM UNNEST(
                $1::text[], $2::text[], $3::text[], $4::text[], $5::text[],
                $6::text[], $7::text[], $8::text[], $9::bool[], $10::bool[]
            ) AS t(drv_hash, drv_path, pname, system, status,
                   required_features, expected_output_paths, output_names,
                   is_fixed_output, is_ca)
            ON CONFLICT (drv_hash) DO UPDATE SET
                updated_at = now(),
                expected_output_paths = EXCLUDED.expected_output_paths,
                output_names = EXCLUDED.output_names,
                is_fixed_output = EXCLUDED.is_fixed_output,
                is_ca = EXCLUDED.is_ca
            RETURNING drv_hash, derivation_id
            "#,
        )
        .bind(&drv_hash)
        .bind(&drv_path)
        .bind(&pname)
        .bind(&system)
        .bind(&status)
        .bind(&required_features)
        .bind(&expected_output_paths)
        .bind(&output_names)
        .bind(&is_fixed_output)
        .bind(&is_ca)
        .fetch_all(&mut *tx)
        .await?;
        Ok(result.into_iter().collect())
    }

    /// Batch-insert build_derivations links.
    pub async fn batch_insert_build_derivations(
        tx: &mut PgConnection,
        build_id: Uuid,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        if derivation_ids.is_empty() {
            return Ok(());
        }
        // build_id is constant across rows — bind once as scalar $1,
        // cross-join UNNEST of the derivation_id array. Two binds total.
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            SELECT $1, derivation_id FROM UNNEST($2::uuid[]) AS t(derivation_id)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(build_id)
        .bind(derivation_ids)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    /// Batch-insert edges.
    pub async fn batch_insert_edges(
        tx: &mut PgConnection,
        edges: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if edges.is_empty() {
            return Ok(());
        }
        let (parents, children): (Vec<Uuid>, Vec<Uuid>) = edges.iter().copied().unzip();
        sqlx::query(
            r#"
            INSERT INTO derivation_edges (parent_id, child_id)
            SELECT * FROM UNNEST($1::uuid[], $2::uuid[])
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&parents)
        .bind(&children)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }
}
