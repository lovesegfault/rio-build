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

use super::SchedulerDb;

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
    /// `inputDrvs`' outputs). Paths not in `narinfo` still appear
    /// (with `root_node = None`) but contribute no further references.
    ///
    /// Sorted by `store_path` so `input_closure_digest` is stable.
    // r[impl sched.dispatch.input-roots+2]
    pub async fn compute_input_roots(
        &self,
        seeds: &[String],
    ) -> Result<Vec<InputRootRow>, sqlx::Error> {
        if seeds.is_empty() {
            return Ok(Vec::new());
        }
        // UNION (set semantics) terminates on cycles. Seeds missing
        // from `narinfo` survive via the seed arm + LEFT JOIN.
        let rows: Vec<(String, Option<Vec<u8>>)> = sqlx::query_as(
            r#"
            WITH RECURSIVE closure(store_path) AS (
                SELECT unnest($1::text[])
                UNION
                SELECT unnest(n."references")
                  FROM narinfo n
                  JOIN closure c ON n.store_path = c.store_path
            )
            SELECT c.store_path, ni.root_node
              FROM closure c
              LEFT JOIN narinfo n ON n.store_path = c.store_path
              LEFT JOIN nar_index ni ON ni.store_path_hash = n.store_path_hash
             ORDER BY c.store_path
            "#,
        )
        .bind(seeds)
        .fetch_all(&self.pool)
        .await?;

        Ok(rows
            .into_iter()
            .map(|(store_path, root_node)| InputRootRow {
                store_path,
                root_node,
            })
            .collect())
    }
}
