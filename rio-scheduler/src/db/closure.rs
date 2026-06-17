//! Transitive input-closure walk for `WorkAssignment.input_roots`.
//!
//! At dispatch time the scheduler resolves the transitive runtime
//! closure of a build's inputs (BFS over `narinfo.references`) and
//! attaches each closure path's castore `root_node` to the assignment.
//! The same sorted closure feeds `AssignmentClaims.input_closure_digest`
//! for the §6.3 server-side refscan attestation.
//!
//! Distinct from `approx_input_closure`, the shallow prefetch hint:
//! this one BFSes `narinfo.references` so it covers the full runtime
//! closure regardless of DAG shape.

use tracing::warn;

use super::SchedulerDb;

/// How many missing store paths the degrade warning names before
/// truncating to a count (a large closure could otherwise log
/// thousands of paths).
const MISSING_LOG_CAP: usize = 8;

/// One closure path with its castore root, if indexed.
#[derive(Debug)]
pub struct InputRootRow {
    /// Store path (e.g. `/nix/store/aaaa-foo`).
    pub store_path: String,
    /// Encoded `rio.castore.RootNode`, or `None` if the path's
    /// `nar_index` row hasn't been populated yet (P0552 indexer is
    /// async). The builder falls back to `GetNarIndex` for these.
    pub root_node: Option<Vec<u8>>,
}

impl SchedulerDb {
    /// Compute the transitive runtime closure of `seeds` (BFS over
    /// `narinfo.references`) and join each path against
    /// `nar_index.root_node`.
    ///
    /// `seeds` are the dispatch-time direct inputs from
    /// `attested_input_seeds` (the parsed drv's `inputSrcs` ∪ its
    /// `inputDrvs`' outputs).
    ///
    /// Returns `None` when any closure member (seed or transitive) has
    /// no `narinfo` row: such a member's own references are unknown,
    /// so the walk cannot prove the set complete and the resulting
    /// closure may be NARROWER than the build's true input closure.
    /// Signing a digest over a truncated set would let the builder's
    /// refscan silently drop references to the unseen tail — GC could
    /// then collect still-referenced paths. Mirrors
    /// `attested_input_seeds`: state the scheduler cannot prove
    /// complete degrades to "no attestation" (`None`), never to a
    /// silently narrower attestation.
    ///
    /// A member that has a `narinfo` row but no `nar_index` row keeps
    /// `root_node = None` (indexer is async) — that only degrades the
    /// castore prefetch, not the attestation.
    ///
    /// Sorted by `store_path` so `input_closure_digest` is stable.
    // r[impl sched.dispatch.input-roots+2]
    pub async fn compute_input_roots(
        &self,
        seeds: &[String],
    ) -> Result<Option<Vec<InputRootRow>>, sqlx::Error> {
        if seeds.is_empty() {
            return Ok(Some(Vec::new()));
        }
        // UNION (set semantics) terminates on cycles. Members missing
        // from `narinfo` survive via the seed/reference arm + LEFT
        // JOIN and are flagged by the `missing` column.
        let rows: Vec<(String, Option<Vec<u8>>, bool)> = sqlx::query_as(
            r#"
            WITH RECURSIVE closure(store_path) AS (
                SELECT unnest($1::text[])
                UNION
                SELECT unnest(n."references")
                  FROM narinfo n
                  JOIN closure c ON n.store_path = c.store_path
            )
            SELECT c.store_path, ni.root_node, (n.store_path IS NULL) AS missing
              FROM closure c
              LEFT JOIN narinfo n ON n.store_path = c.store_path
              LEFT JOIN nar_index ni ON ni.store_path_hash = n.store_path_hash
             ORDER BY c.store_path
            "#,
        )
        .bind(seeds)
        .fetch_all(&self.pool)
        .await?;

        let missing: Vec<&str> = rows
            .iter()
            .filter(|(_, _, missing)| *missing)
            .map(|(p, _, _)| p.as_str())
            .collect();
        if !missing.is_empty() {
            warn!(
                missing_count = missing.len(),
                missing = ?&missing[..missing.len().min(MISSING_LOG_CAP)],
                "input closure has members with no narinfo row; their \
                 references are unknown so the closure cannot be proven \
                 complete — degrading to unattested"
            );
            return Ok(None);
        }

        Ok(Some(
            rows.into_iter()
                .map(|(store_path, root_node, _)| InputRootRow {
                    store_path,
                    root_node,
                })
                .collect(),
        ))
    }
}
