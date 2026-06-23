//! Batch operations for `persist_merges` — build-derivation mapping +
//! `COPY`-streamed bulk inserts.

use std::collections::HashMap;
use std::fmt::Write as _;

use sqlx::PgConnection;
use uuid::Uuid;

use super::{DerivationRow, SchedulerDb};

/// Escape one column value for `COPY … FROM STDIN (FORMAT text)`.
///
/// PG's text COPY format is `\n`-terminated rows of `\t`-separated
/// columns; `\N` is NULL. The only escapes that matter are backslash,
/// tab, newline, and carriage return — everything else is literal.
///
/// Fast path: drv_hash / drv_path / system / pname / array-literals
/// essentially never contain `\\ \t \n \r`, so check first and
/// `push_str` (one memcpy) instead of per-char `push` (UTF-8 re-encode
/// per char). 14k rows × ~7 escaped columns × ~50 chars is the inner
/// loop of the sub-second persist path.
fn copy_escape_into(out: &mut String, s: &str) {
    if s.bytes()
        .all(|b| !matches!(b, b'\\' | b'\t' | b'\n' | b'\r'))
    {
        out.push_str(s);
        return;
    }
    for ch in s.chars() {
        match ch {
            '\\' => out.push_str("\\\\"),
            '\t' => out.push_str("\\t"),
            '\n' => out.push_str("\\n"),
            '\r' => out.push_str("\\r"),
            c => out.push(c),
        }
    }
}

/// Write a `text[]` column value for COPY directly into `out`: the
/// PG array literal `{"a","b"}` with combined COPY+array escaping.
///
/// Fuses [`super::encode_pg_text_array`] + [`copy_escape_into`] so the
/// per-row hot loop doesn't allocate a fresh `String` temporary per
/// array column and then re-walk it char-by-char. Array-literal layer:
/// backslash-escape `"` and `\` inside each element. COPY layer: every
/// `\` the array layer emits is itself escaped (`\\` → `\\\\`, `\"` →
/// `\\"`), and element-body `\t \n \r` get the COPY escape. PG
/// de-escapes COPY first, then parses the array literal.
fn copy_escape_pg_array_into(out: &mut String, items: &[String]) {
    out.push('{');
    for (i, item) in items.iter().enumerate() {
        if i > 0 {
            out.push(',');
        }
        out.push('"');
        if item
            .bytes()
            .all(|b| !matches!(b, b'\\' | b'"' | b'\t' | b'\n' | b'\r'))
        {
            out.push_str(item);
        } else {
            for ch in item.chars() {
                match ch {
                    // Array-layer `\"` → COPY layer escapes the `\`.
                    '"' => out.push_str("\\\\\""),
                    // Array-layer `\\` → COPY layer doubles each.
                    '\\' => out.push_str("\\\\\\\\"),
                    '\t' => out.push_str("\\t"),
                    '\n' => out.push_str("\\n"),
                    '\r' => out.push_str("\\r"),
                    c => out.push(c),
                }
            }
        }
        out.push('"');
    }
    out.push('}');
}

impl SchedulerDb {
    /// Link a build to a derivation. Test-only singular form; production
    /// path is [`Self::batch_insert_build_derivations`].
    #[cfg(test)]
    pub(crate) async fn insert_build_derivation(
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

    // r[impl sched.db.batch-unnest+2]
    // r[impl sched.db.merge-batch-shape]
    /// Batch-upsert derivations. Returns a map
    /// `drv_hash -> (derivation_id, resource_floor)`.
    ///
    /// Shape (sh-036): stream rows via `COPY (FORMAT text)` into an
    /// `ON COMMIT DROP` temp table, then one `INSERT … SELECT FROM
    /// _merge_derivations ON CONFLICT … RETURNING`. The temp-table DDL
    /// runs here (self-contained) so direct callers — production's
    /// `persist_merges` and the eight test sites — need no per-call
    /// setup; the table is session-scoped to the caller's transaction
    /// connection and dropped at commit.
    ///
    /// `RETURNING drv_hash` because PG doesn't guarantee `RETURNING`
    /// order matches insert order; the result map keys by `drv_hash`.
    ///
    /// `floor_*` columns are returned so merge can hydrate them onto
    /// newly-inserted in-memory state (I-208) — `try_from_node` sets
    /// `floor=zeros`, but the DB row may pre-exist (ON CONFLICT) with a
    /// floor promoted by a prior run's failures. Without this the next
    /// SpawnIntent re-uses probe defaults and re-OOMs every run.
    pub(crate) async fn batch_upsert_derivations(
        tx: &mut PgConnection,
        rows: &[DerivationRow],
    ) -> Result<HashMap<String, (Uuid, crate::state::ResourceFloor)>, sqlx::Error> {
        if rows.is_empty() {
            return Ok(HashMap::new());
        }

        // Nested-array columns (required_features, expected_output_paths,
        // output_names) are declared `text[]` (not `text`) so COPY's
        // text-format parser reads the `{"a","b"}` literal as an array
        // value directly — no `::text[]` cast needed in the step-3
        // SELECT. PG multidim arrays are rectangular, so a `text[][]`
        // bind was never an option; the COPY-literal route is the same
        // workaround the prior UNNEST form used, just one parse layer
        // earlier.
        sqlx::query(
            r#"
            CREATE TEMP TABLE _merge_derivations (
                drv_hash               text    NOT NULL,
                drv_path               text    NOT NULL,
                pname                  text,
                system                 text    NOT NULL,
                status                 text    NOT NULL,
                required_features      text[]  NOT NULL,
                expected_output_paths  text[]  NOT NULL,
                output_names           text[]  NOT NULL,
                is_fixed_output        boolean NOT NULL,
                is_ca                  boolean NOT NULL
            ) ON COMMIT DROP
            "#,
        )
        .execute(&mut *tx)
        .await?;

        // One in-memory buffer, one .send(). The text[] columns get
        // COPY-escaped on top of the array-literal escaping —
        // PG de-escapes the COPY layer first, then parses the array
        // literal, so an element backslash round-trips as `\\\\`.
        let mut buf = String::with_capacity(rows.len() * 256);
        for r in rows {
            copy_escape_into(&mut buf, &r.drv_hash);
            buf.push('\t');
            copy_escape_into(&mut buf, &r.drv_path);
            buf.push('\t');
            match &r.pname {
                Some(p) => copy_escape_into(&mut buf, p),
                None => buf.push_str("\\N"),
            }
            buf.push('\t');
            copy_escape_into(&mut buf, &r.system);
            buf.push('\t');
            buf.push_str(r.status.as_str());
            buf.push('\t');
            copy_escape_pg_array_into(&mut buf, &r.required_features);
            buf.push('\t');
            copy_escape_pg_array_into(&mut buf, &r.expected_output_paths);
            buf.push('\t');
            copy_escape_pg_array_into(&mut buf, &r.output_names);
            buf.push('\t');
            buf.push(if r.is_fixed_output { 't' } else { 'f' });
            buf.push('\t');
            buf.push(if r.is_ca { 't' } else { 'f' });
            buf.push('\n');
        }
        let mut copy = tx
            .copy_in_raw(
                "COPY _merge_derivations \
                 (drv_hash, drv_path, pname, system, status, \
                  required_features, expected_output_paths, output_names, \
                  is_fixed_output, is_ca) FROM STDIN (FORMAT text)",
            )
            .await?;
        copy.send(buf.as_bytes()).await?;
        let copied = copy.finish().await?;
        debug_assert_eq!(copied, rows.len() as u64);

        // ON CONFLICT: update the recovery columns too. For
        // expected_output_paths / output_names / is_* a second build
        // requesting the same derivation carries identical values
        // (same drv_hash → same .drv content → same declared outputs),
        // so overwriting with EXCLUDED is idempotent and just keeps
        // the row in sync with in-mem. status/retry etc stay as-is —
        // those reflect LIVE state, not merge-time snapshot.
        //
        // Per-build wanted-output interest is NOT a derivations column:
        // it lives in the `build_wanted_outputs` relation
        // (`record_wanted_in_tx`, written by the same merge
        // transaction), keyed by (build, derivation) — a per-consumer
        // fact never belongs on the per-drv row.
        //
        // Duplicate `drv_hash` within one batch would raise
        // "ON CONFLICT DO UPDATE … cannot affect row a second time" —
        // identically to the prior `FROM UNNEST` form; `dedup_dag`
        // upstream guarantees uniqueness.
        let result: Vec<(String, Uuid, i64, i64, i64, i64)> = sqlx::query_as(
            r#"
            INSERT INTO derivations
                (drv_hash, drv_path, pname, system, status, required_features,
                 expected_output_paths, output_names, is_fixed_output, is_ca)
            SELECT
                drv_hash, drv_path, pname, system, status,
                required_features, expected_output_paths, output_names,
                is_fixed_output, is_ca
            FROM _merge_derivations
            -- is_ca UPDATE is idempotent-by-construction: drv_hash is
            -- deterministic (input-addressed=store path; CA=modular hash
            -- per rio-nix hashDerivationModulo). Same drv_hash → same
            -- .drv content → same outputs[] → same is_ca. The EXCLUDED
            -- value always equals the existing row's value. Kept in the
            -- SET-list for insert-columns parity.
            ON CONFLICT (drv_hash) DO UPDATE SET
                updated_at = now(),
                expected_output_paths = EXCLUDED.expected_output_paths,
                output_names = EXCLUDED.output_names,
                is_fixed_output = EXCLUDED.is_fixed_output,
                is_ca = EXCLUDED.is_ca
            RETURNING drv_hash, derivation_id,
                      floor_mem_bytes, floor_disk_bytes, floor_deadline_secs,
                      floor_cores::bigint
            "#,
        )
        .fetch_all(&mut *tx)
        .await?;
        Ok(result
            .into_iter()
            .map(|(h, id, mem, disk, deadline, cores)| {
                (
                    h,
                    (
                        id,
                        crate::state::ResourceFloor {
                            mem_bytes: mem.max(0) as u64,
                            disk_bytes: disk.max(0) as u64,
                            deadline_secs: deadline.clamp(0, u32::MAX as i64) as u32,
                            cores: i64::clamp(cores, 0, u32::MAX as i64) as u32,
                        },
                    ),
                )
            })
            .collect())
    }

    /// Single-build convenience over
    /// [`Self::batch_insert_build_derivations_multi`] for test sites
    /// that hold one `build_id` and a derivation_id slice.
    #[cfg(test)]
    pub(crate) async fn batch_insert_build_derivations(
        tx: &mut PgConnection,
        build_id: Uuid,
        derivation_ids: &[Uuid],
    ) -> Result<(), sqlx::Error> {
        let pairs: Vec<(Uuid, Uuid)> = derivation_ids.iter().map(|&d| (build_id, d)).collect();
        Self::batch_insert_build_derivations_multi(tx, &pairs).await
    }

    /// Batch-insert build_derivations links — `(build_id,
    /// derivation_id)` pairs from N merges in one round-trip (P2
    /// phase-5 coalesce). Two parallel-array binds; same `ON CONFLICT
    /// DO NOTHING` so a shared derivation linked by two builds in one
    /// batch is benign.
    pub(crate) async fn batch_insert_build_derivations_multi(
        tx: &mut PgConnection,
        pairs: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if pairs.is_empty() {
            return Ok(());
        }
        let builds: Vec<Uuid> = pairs.iter().map(|(b, _)| *b).collect();
        let drvs: Vec<Uuid> = pairs.iter().map(|(_, d)| *d).collect();
        sqlx::query(
            r#"
            INSERT INTO build_derivations (build_id, derivation_id)
            SELECT b, d FROM UNNEST($1::uuid[], $2::uuid[]) AS t(b, d)
            ON CONFLICT DO NOTHING
            "#,
        )
        .bind(&builds)
        .bind(&drvs)
        .execute(&mut *tx)
        .await?;
        Ok(())
    }

    // r[impl sched.db.merge-batch-shape]
    /// Batch-insert edges. Same `COPY → ON COMMIT DROP temp →
    /// INSERT … SELECT … ON CONFLICT DO NOTHING` shape as
    /// [`Self::batch_upsert_derivations`]; no `RETURNING`.
    pub(crate) async fn batch_insert_edges(
        tx: &mut PgConnection,
        edges: &[(Uuid, Uuid)],
    ) -> Result<(), sqlx::Error> {
        if edges.is_empty() {
            return Ok(());
        }
        sqlx::query(
            "CREATE TEMP TABLE _merge_edges \
             (parent_id uuid NOT NULL, child_id uuid NOT NULL) \
             ON COMMIT DROP",
        )
        .execute(&mut *tx)
        .await?;

        // Uuid::Display is the canonical hyphenated form PG accepts;
        // contains no `\t` `\n` `\\` so no escaping needed.
        let mut buf = String::with_capacity(edges.len() * 74);
        for (parent, child) in edges {
            let _ = writeln!(buf, "{parent}\t{child}");
        }
        let mut copy = tx
            .copy_in_raw("COPY _merge_edges (parent_id, child_id) FROM STDIN (FORMAT text)")
            .await?;
        copy.send(buf.as_bytes()).await?;
        copy.finish().await?;

        sqlx::query(
            r#"
            INSERT INTO derivation_edges (parent_id, child_id)
            SELECT parent_id, child_id FROM _merge_edges
            ON CONFLICT DO NOTHING
            "#,
        )
        .execute(&mut *tx)
        .await?;
        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// `copy_escape_pg_array_into` MUST equal
    /// `copy_escape_into(encode_pg_text_array(items))` for every input
    /// — it's a fused fast-path for the same encoding, not a new one.
    #[test]
    fn fused_array_escape_matches_composed() {
        use proptest::prelude::*;
        // Pin a few hand-picked corners first (the round-trip
        // PG-integration test in db/tests/batch.rs covers the encoding
        // itself; this pin is the fused≡composed identity).
        for items in [
            &[][..],
            &["a".into()],
            &["a".into(), "b".into()],
            &[r#"has"quote"#.into()],
            &[r"has\backslash".into()],
            &["tab\there".into(), "nl\nhere".into(), "cr\rhere".into()],
        ] {
            let mut fused = String::new();
            copy_escape_pg_array_into(&mut fused, items);
            let mut composed = String::new();
            copy_escape_into(&mut composed, &super::super::encode_pg_text_array(items));
            assert_eq!(fused, composed, "items={items:?}");
        }
        proptest!(|(items in proptest::collection::vec(".*", 0..5))| {
            let mut fused = String::new();
            copy_escape_pg_array_into(&mut fused, &items);
            let mut composed = String::new();
            copy_escape_into(&mut composed, &super::super::encode_pg_text_array(&items));
            prop_assert_eq!(fused, composed);
        });
    }
}
