//! Approximate input closure shared by the pull mint and the GC
//! live-pin path.
//!
//! The stream-era worker-selection half (`best_executor()`, the
//! hard-filter/warm-gate two-pass and the per-clause rejection
//! diagnostic) was deleted with the placement layer, and
//! `statically_eligible` retired with the executors map: the pull
//! protocol has no scheduler-side placement decision and no in-memory
//! fleet to filter — the controller's spawn gate owns source
//! eligibility per AD2 and its `NoEligibleSource` report is the
//! fleet-exhaust path.

use crate::dag::DerivationDag;
use crate::db::SchedulerDb;
use crate::state::DrvHash;
use rio_proto::StoreServiceClient;
use std::borrow::Cow;
use tonic::transport::Channel;

/// Approximate input closure: the derivation's DAG children's
/// expected output paths PLUS its own `inputSrcs` (already-built
/// store paths declared in the ATerm, not represented as DAG nodes).
///
/// This is what the derivation NEEDS as inputs — its dependencies'
/// outputs and direct sources. Not perfect (misses transitive
/// closure of `inputSrcs`), but covers the bulk of what the
/// worker's FUSE will actually fetch. For a shallow DAG (leaf drv
/// with substituted/cached deps) `inputSrcs` is the ONLY signal —
/// without it the prefetch hint is empty and the worker
/// serial-fetches every input on first `lstat()`.
///
/// Used by the pull mint and the assignment-time GC live-pin path
/// (`scheduler_live_pins`) to approximate what the build will read.
///
/// Cheap: DAG iteration only, no store RPCs, no ATerm parse (the
/// parse happened once at merge time → `DerivationState.input_srcs`).
/// For a derivation with 20 dependencies each with 2 outputs +
/// 30 `inputSrcs`: ~70 string clones, ~1μs.
pub(crate) fn approx_input_closure(dag: &DerivationDag, drv_hash: &DrvHash) -> Vec<String> {
    let from_children = dag
        .get_children(drv_hash)
        .into_iter()
        .filter_map(|child| dag.node(&child))
        .flat_map(|child| {
            // Prefer REALIZED output_paths (populated at completion time
            // from the worker's BuildResult.built_outputs) over
            // expected_output_paths (populated at merge time from the
            // proto). For a floating-CA child, expected_output_paths is
            // `[""]` (the path is unknown pre-build) but output_paths
            // has the actual realized path once the child completes.
            // For IA children, expected_output_paths is correct and
            // output_paths is empty until completion — fall through.
            if child.output_paths.is_empty() {
                child.expected_output_paths.iter()
            } else {
                child.output_paths.iter()
            }
        });
    let from_srcs = dag
        .node(drv_hash)
        .map(|s| s.input_srcs.iter())
        .into_iter()
        .flatten();
    // inputSrcs first: they're declared in the ATerm (exact), while
    // dag-children outputs are an approximation (may over-include
    // unused multi-output siblings).
    from_srcs
        .chain(from_children)
        // Filter empties: a floating-CA child that hasn't completed yet
        // has expected_output_paths=[""] and output_paths=[]. The ""
        // would be a no-op PrefetchHint entry; cleaner to drop it here.
        .filter(|p| !p.is_empty())
        .cloned()
        .collect()
}

/// Exact direct-input seed set for the attested input closure
/// (`WorkAssignment.input_closure` /
/// `AssignmentClaims.input_closure_digest`, the P0589 §6.3 server-side
/// refscan attestation).
///
/// Unlike [`approx_input_closure`] — a best-effort prefetch hint that
/// silently degrades (recovered nodes lose `drv_content`/`input_srcs`,
/// and recovery drops DAG edges to children that completed before the
/// restart) — the attested closure must NEVER be narrower than the
/// build's true input closure: the builder uses it as the
/// reference-scan candidate set and cannot widen it (the store checks
/// the digest), so an omitted path means references to it are silently
/// missing from the uploaded narinfo and GC can collect
/// still-referenced paths.
///
/// The seeds are therefore derived from the node's parsed derivation —
/// the ground truth for direct inputs: the derivation's own `.drv`
/// path ∪ `inputSrcs` ∪ every `inputDrvs` entry's `.drv` path ∪ the
/// outputs of every `inputDrvs` entry. The `.drv` paths are seeded so
/// the closure covers what nix-daemon reads through FUSE under W03
/// closure-scope enforcement (the builder's own seed set already
/// includes them; the scheduler's must not be narrower). Each
/// `inputDrvs` entry's outputs are resolved first through the
/// in-memory DAG, then — for entries with no DAG node (substituted-
/// then-reaped, or completed before a restart so recovery skipped it;
/// the FOD shape where curl/bash/stdenv come from the binary cache) —
/// through the persisted `derivations.expected_output_paths` row,
/// which is the same authority a fresh-merge DAG node would have
/// carried. Returns `None` whenever the exact set cannot be
/// established:
///
///   - no / unparseable `drv_content` (recovery-loaded node, or the
///     gateway didn't inline the `.drv`),
///   - an `inputDrvs` entry has no DAG node AND no `derivations` row
///     (genuinely never merged on this cluster),
///   - the resolved output paths contain an empty entry (floating-CA
///     placeholder; the consumed output is unknowable pre-build).
///
/// A recovered node ([`crate::state::DerivationState::from_recovery_row`]
/// sets `drv_content = Vec::new()`) reaches the inputDrvs loop via a
/// store-side `GetPath` of its own `.drv` — the same ATerm bytes the
/// gateway originally inlined, so the parsed `inputDrvs` set is
/// identical and the never-narrower invariant holds by construction.
/// The fetch is bounded by the dispatch callsite's `grpc_timeout`
/// wrapper (and `fetch_drv_aterm`'s own per-chunk idle bound). On
/// fetch failure (`store=None`, GetPath error/NotFound/timeout, NAR
/// unwrap failure) → `Ok(None)` with the per-arm metric, same as the
/// other unresolvable arms.
///
/// `None` → the dispatch site sends an empty closure/digest. Under
/// ADR-022 closure-scoped castore-FUSE this is NOT a safe degrade —
/// the builder's own drv-parsed BFS may EIO reading through the
/// empty-scoped mount, so the assignment infra-retries instead of
/// silently widening. `None` is therefore reserved for cases the
/// scheduler genuinely cannot resolve (an `inputDrvs` entry never
/// merged on this cluster, a floating-CA placeholder, or the GetPath
/// fetch itself failing). This keeps the invariant structural: state
/// the scheduler cannot prove complete degrades to "no attestation",
/// never to a silently narrower attestation — no recovery-path
/// bookkeeping to keep in sync.
///
/// Every `Ok(None)` arm increments
/// `rio_scheduler_attested_seeds_unknown_total{arm=…}` so a non-zero
/// `input_closure_unattested_total{reason=seeds_unknown}` is
/// diagnosable without a debugger (the recovered-target arm went
/// undetected for a deploy cycle when this was a single bucket).
// r[impl sched.dispatch.input-roots+3]
// r[impl sched.dispatch.never-narrower]
pub(crate) async fn attested_input_seeds(
    dag: &DerivationDag,
    drv_hash: &DrvHash,
    db: &SchedulerDb,
    store: Option<&StoreServiceClient<Channel>>,
) -> Result<Option<Vec<String>>, sqlx::Error> {
    /// Per-arm `Ok(None)` accounting. Macro so each arm is one line at
    /// the return site (and so the literal label is the metric label —
    /// no enum/match indirection to keep in sync).
    macro_rules! seeds_unknown {
        ($arm:literal) => {{
            metrics::counter!("rio_scheduler_attested_seeds_unknown_total", "arm" => $arm)
                .increment(1);
            return Ok(None);
        }};
    }

    let Some(node) = dag.node(drv_hash) else {
        seeds_unknown!("no_node");
    };
    // Recovered nodes have `drv_content = Vec::new()`
    // (`from_recovery_row` does not persist the ATerm). Fetch from the
    // store — the same path a worker takes when
    // `WorkAssignment.drv_content` is empty — so the inputDrvs loop
    // below (and its `derivations`-table fallback) runs. Without this,
    // every recovered target degraded to no-attestation BEFORE reaching
    // the per-inputDrv resolver, regardless of how complete the
    // persisted state was.
    //
    // The fetched bytes are NOT written back to `dag.node_mut()` (dag
    // is `&` here); a follow-up may hoist the fetch to the dispatch
    // callsite where `&mut self.dag` is in scope so retries and
    // `maybe_resolve_ca` share one fetch. ~10-50ms once per recovered
    // dispatch is acceptable meanwhile.
    let drv_content: Cow<'_, [u8]> = if node.drv_content.is_empty() {
        let Some(client) = store else {
            seeds_unknown!("drv_empty_no_store");
        };
        match fetch_drv_aterm(&mut client.clone(), node.drv_path().as_ref()).await {
            Some(bytes) => {
                metrics::counter!(
                    "rio_scheduler_attested_seeds_unknown_total",
                    "arm" => "drv_fetched"
                )
                .increment(1);
                Cow::Owned(bytes)
            }
            None => seeds_unknown!("drv_fetch_failed"),
        }
    } else {
        Cow::Borrowed(node.drv_content.as_slice())
    };
    let Some(drv) = std::str::from_utf8(&drv_content)
        .ok()
        .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok())
    else {
        seeds_unknown!("drv_unparseable");
    };

    // Own .drv path + inputSrcs first (declared in the ATerm; exact).
    let mut seeds: Vec<String> = vec![node.drv_path().to_string()];
    seeds.extend(drv.input_srcs().iter().cloned());

    // inputDrvs not in the in-memory DAG, batched into one
    // `derivations.expected_output_paths` lookup after the loop.
    let mut dag_missed: Vec<String> = Vec::new();
    for input_drv_path in drv.input_drvs().keys() {
        // Seed the inputDrv's .drv path unconditionally (W03
        // forward-compat: nix-daemon reads it through FUSE).
        seeds.push(input_drv_path.clone());
        let Some(child) = dag.hash_for_path(input_drv_path).and_then(|h| dag.node(h)) else {
            dag_missed.push(input_drv_path.clone());
            continue;
        };
        // Prefer realized output paths (covers floating-CA, whose
        // expected paths are "" pre-build); fall back to the
        // merge-time expected paths (IA / fixed-CA). Either list may
        // over-include sibling outputs the parent doesn't consume —
        // harmless for a refscan candidate set (a path only becomes a
        // recorded reference if its hash actually appears in the
        // output bytes). What is NOT allowed is an unknown output
        // path: any empty entry means the seed set might not cover a
        // consumed output → no attestation.
        let paths = if child.output_paths.is_empty() {
            &child.expected_output_paths
        } else {
            &child.output_paths
        };
        if paths.is_empty() || paths.iter().any(String::is_empty) {
            seeds_unknown!("child_output_unknown");
        }
        seeds.extend(paths.iter().cloned());
    }

    if !dag_missed.is_empty() {
        let by_drv = db.expected_outputs_by_drv_path(&dag_missed).await?;
        for drv_path in &dag_missed {
            // The `derivations` row carries the same
            // `expected_output_paths` a DAG node would — written from
            // the gateway-parsed ATerm at merge time and surviving
            // reap. No name-blind reverse lookup, no count heuristic:
            // never-narrower by construction. A floating-CA `[""]`
            // entry is the same unknowable-output gate as the DAG arm
            // above; an absent row means genuinely never merged.
            match by_drv.get(drv_path) {
                Some(paths) if !paths.is_empty() && !paths.iter().any(String::is_empty) => {
                    seeds.extend(paths.iter().cloned());
                }
                found => {
                    tracing::debug!(
                        input_drv = %drv_path,
                        row_present = found.is_some(),
                        "inputDrv not in DAG and derivations-table fallback \
                         cannot establish its outputs; degrading to unattested"
                    );
                    metrics::counter!(
                        "rio_scheduler_attested_seeds_pg_fallback_total",
                        "outcome" => "degraded_none"
                    )
                    .increment(1);
                    seeds_unknown!("input_drv_unresolved");
                }
            }
        }
        metrics::counter!(
            "rio_scheduler_attested_seeds_pg_fallback_total",
            "outcome" => "resolved"
        )
        .increment(dag_missed.len() as u64);
    }

    Ok(Some(seeds))
}

/// Fetch a `.drv`'s ATerm bytes from the store via `GetPath`.
///
/// The store returns NAR-framed bytes; a `.drv` is a single regular
/// file, so [`rio_nix::nar::extract_single_file`] unwraps it to the
/// raw ATerm. Same path the worker takes when
/// `WorkAssignment.drv_content` is empty
/// (`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`).
///
/// `None` on any failure: `GetPath` error/timeout/NotFound, NAR-unwrap
/// failure, or NAR > 1 MiB (a `.drv` is ~1-50 KB ASCII; 1 MiB is ~20×
/// any real `.drv` — bail rather than pull a multi-GB closure if the
/// path was mis-resolved). The 2s per-chunk idle bound covers a slow
/// store without blocking dispatch (the call is also under the
/// dispatch site's `grpc_timeout` wrapper).
///
/// Shared by [`attested_input_seeds`] (recovered dispatch target) and
/// `dispatch.rs::fetch_drv_content_from_store` (recovered CA-resolve
/// target).
pub(crate) async fn fetch_drv_aterm(
    client: &mut StoreServiceClient<Channel>,
    drv_path: &str,
) -> Option<Vec<u8>> {
    const MAX_DRV_NAR_SIZE: u64 = 1024 * 1024;
    const FETCH_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(2);

    let nar = match rio_proto::client::get_path_nar(
        client,
        drv_path,
        FETCH_TIMEOUT,
        MAX_DRV_NAR_SIZE,
        &[],
    )
    .await
    {
        Ok(Some((_info, nar))) => nar,
        Ok(None) => {
            tracing::debug!(%drv_path, "drv_content fetch: .drv not found in store");
            return None;
        }
        Err(e) => {
            tracing::debug!(%drv_path, error = %e, "drv_content fetch: GetPath failed");
            return None;
        }
    };
    match rio_nix::nar::extract_single_file(&nar) {
        Ok(bytes) => Some(bytes),
        Err(e) => {
            tracing::debug!(
                %drv_path, error = %e,
                "drv_content fetch: NAR unwrap failed (not a single regular file)"
            );
            None
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::db::SchedulerDb;
    use crate::state::DerivationState;
    use rio_test_support::TestDb;
    use rio_test_support::fixtures::make_derivation_node;
    use sha2::Digest as _;

    /// Per-test PG: `attested_input_seeds` falls through to a
    /// `narinfo.deriver` lookup for DAG-missed `inputDrvs`, so even
    /// the pure-DAG cases need a (possibly empty) database to call
    /// against.
    async fn test_db() -> (TestDb, SchedulerDb) {
        let test_db = TestDb::new(&crate::MIGRATOR).await;
        let db = SchedulerDb::new(test_db.pool.clone());
        (test_db, db)
    }

    /// Insert a `narinfo` row for `out_path` with the given `deriver`
    /// — the shape a substituted-from-cache output has. Used by the
    /// never-narrower regression test to model the narinfo state that
    /// the (rejected) name-blind `narinfo.deriver` reverse-lookup
    /// would have read.
    async fn put_narinfo_with_deriver(pool: &sqlx::PgPool, out_path: &str, deriver: Option<&str>) {
        let h = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
        sqlx::query(
            "INSERT INTO narinfo \
               (store_path_hash, store_path, deriver, nar_hash, nar_size) \
             VALUES ($1, $2, $3, $1, 0)",
        )
        .bind(&h)
        .bind(out_path)
        .bind(deriver)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Insert a `derivations` row for `drv_path` with the given
    /// `expected_output_paths` — the persisted shape a
    /// substituted-then-reaped (or completed-pre-restart) inputDrv
    /// has: written at merge by `batch_upsert_derivations`, surviving
    /// in PG after the in-memory DAG node is gone.
    async fn put_derivation_row(pool: &sqlx::PgPool, drv_path: &str, expected_outputs: &[&str]) {
        let outs: Vec<String> = expected_outputs.iter().map(|s| s.to_string()).collect();
        // concat! keeps `INSERT INTO` and the table name on separate
        // source lines so the production-write fence
        // (`derivations_sql_confined_to_embedded_sources`, a per-line
        // scan that cannot see #[cfg(test)]) does not false-positive
        // on this test fixture.
        sqlx::query(concat!(
            "INSERT INTO ",
            "derivations ",
            "(drv_hash, drv_path, system, status, expected_output_paths) ",
            "VALUES ($1, $1, 'x86_64-linux', 'completed', $2)",
        ))
        .bind(drv_path)
        .bind(&outs)
        .execute(pool)
        .await
        .unwrap();
    }

    /// Shallow DAG: leaf node (no DAG children) with `inputSrcs` —
    /// `approx_input_closure` must return the inputSrcs, not empty.
    /// This is the `nix-bench#hello-shallow` shape: deps substituted/
    /// cached so they're not DAG nodes, only listed in the ATerm.
    #[test]
    fn approx_input_closure_includes_input_srcs_for_leaf() {
        let mut dag = DerivationDag::new();
        let mut leaf =
            DerivationState::try_from_node(&make_derivation_node("leaf", "x86_64-linux").into())
                .unwrap();
        let src_a = rio_test_support::fixtures::test_store_path("gcc-13.2.0");
        let src_b = rio_test_support::fixtures::test_store_path("glibc-2.39");
        leaf.input_srcs = vec![src_a.clone(), src_b.clone()];
        dag.insert_recovered_node(leaf);

        let got = approx_input_closure(&dag, &"leaf".into());
        assert_eq!(got.len(), 2, "leaf with 2 inputSrcs → 2 prefetch paths");
        assert!(got.contains(&src_a));
        assert!(got.contains(&src_b));
    }

    /// Node with BOTH a DAG child and inputSrcs → union of child's
    /// outputs and own inputSrcs. Order is srcs-then-children:
    /// declared inputs (exact) come before dag-children outputs
    /// (approximation, may over-include).
    #[test]
    fn approx_input_closure_unions_children_and_srcs() {
        let mut dag = DerivationDag::new();
        let child_out = rio_test_support::fixtures::test_store_path("child-out");
        let mut child =
            DerivationState::try_from_node(&make_derivation_node("child", "x86_64-linux").into())
                .unwrap();
        child.expected_output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("source-tarball");
        let mut parent =
            DerivationState::try_from_node(&make_derivation_node("parent", "x86_64-linux").into())
                .unwrap();
        parent.input_srcs = vec![src.clone()];
        dag.insert_recovered_node(parent);
        dag.insert_recovered_edge("parent".into(), "child".into());

        let got = approx_input_closure(&dag, &"parent".into());
        assert_eq!(got, vec![src, child_out]);
    }

    /// Parent node whose `drv_content` is a real ATerm with one
    /// inputDrv (`child_drv_path`, output "out") and one inputSrc.
    fn make_attest_parent(child_drv_path: &str, src: &str) -> DerivationState {
        let parent_out = rio_test_support::fixtures::test_store_path("attest-parent-out");
        let aterm = format!(
            r#"Derive([("out","{parent_out}","","")],[("{child_drv_path}",["out"])],["{src}"],"x86_64-linux","/bin/sh",[],[("out","{parent_out}")])"#
        );
        let mut node = make_derivation_node("attest-parent", "x86_64-linux");
        node.drv_content = aterm.into_bytes();
        DerivationState::try_from_node(&node.into()).unwrap()
    }

    /// Happy path: parsed drv with a resolvable inputDrv child →
    /// seeds = inputSrcs ∪ the child's realized outputs.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_resolve_parsed_drv_inputs() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv_path = rio_test_support::fixtures::test_drv_path("attest-child");
        let child_out = rio_test_support::fixtures::test_store_path("attest-child-out");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
            .await
            .unwrap()
            .expect("parsed drv with resolvable inputs is attestable");
        assert!(got.contains(&src), "inputSrcs entry missing: {got:?}");
        assert!(
            got.contains(&child_out),
            "inputDrv child's realized output missing: {got:?}"
        );
    }

    /// Recovery shape with no store client: `from_recovery_row` clears
    /// `drv_content`, the GetPath fetch is unavailable, so the exact
    /// direct-input set cannot be established → no attestation, even
    /// though a DAG child with a known output exists (the approximation
    /// would have produced a non-empty — and possibly narrower-than-
    /// true — seed set).
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_drv_empty_and_no_store() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_paths = vec![rio_test_support::fixtures::test_store_path(
            "attest-child-out",
        )];
        dag.insert_recovered_node(child);

        // No drv_content (recovered / not inlined). No store client.
        let parent = DerivationState::try_from_node(
            &make_derivation_node("attest-parent", "x86_64-linux").into(),
        )
        .unwrap();
        dag.insert_recovered_node(parent);
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
                .await
                .unwrap()
                .is_none(),
            "no parsed .drv and no store to fetch from → must not attest"
        );
    }

    /// Recovered target: `drv_content = Vec::new()` (the
    /// `from_recovery_row` shape), but the store has the `.drv` ATerm
    /// at `drv_path` and the inputDrv has a persisted `derivations`
    /// row. The GetPath fetch supplies the bytes the gateway originally
    /// inlined → `inputDrvs` parses → the `derivations`-table fallback
    /// resolves the inputDrv's outputs → `Some(seeds)`.
    ///
    /// Regression test for the live-cluster shape where 28% of
    /// dispatches degraded to `seeds_unknown` at the parse step BEFORE
    /// reaching the per-inputDrv resolver, simply because the dispatch
    /// target was recovery-loaded.
    // r[verify sched.dispatch.input-roots+3]
    // r[verify sched.dispatch.never-narrower]
    #[tokio::test]
    async fn attested_seeds_recovered_target_fetches_drv_content() {
        let (t, db) = test_db().await;
        let (store, store_client) = rio_test_support::grpc::spawn_mock_store_inproc()
            .await
            .unwrap();
        let mut dag = DerivationDag::new();

        let sub_drv = rio_test_support::fixtures::test_drv_path("attest-substituted");
        let sub_out = rio_test_support::fixtures::test_store_path("attest-substituted-out");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        let parent_drv = rio_test_support::fixtures::test_drv_path("attest-parent");

        // The inputDrv is NOT a DAG node, but its persisted
        // `derivations` row carries expected_output_paths — the same
        // C1-hardened shape as `_fall_back_to_pg_for_substituted_…`.
        put_derivation_row(&t.pool, &sub_drv, &[&sub_out]).await;

        // Parent's ATerm lives only in the store (recovered node:
        // `drv_content` empty in-memory). Same ATerm
        // `make_attest_parent` would have inlined; the fetched bytes
        // are byte-identical to what the gateway sent → never-narrower
        // by construction.
        let parent_out = rio_test_support::fixtures::test_store_path("attest-parent-out");
        let aterm = format!(
            r#"Derive([("out","{parent_out}","","")],[("{sub_drv}",["out"])],["{src}"],"x86_64-linux","/bin/sh",[],[("out","{parent_out}")])"#
        );
        store.seed_with_content(&parent_drv, aterm.as_bytes());

        let parent = DerivationState::try_from_node(
            &make_derivation_node("attest-parent", "x86_64-linux").into(),
        )
        .unwrap();
        assert!(parent.drv_content.is_empty(), "fixture sanity");
        dag.insert_recovered_node(parent);

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db, Some(&store_client))
            .await
            .unwrap()
            .expect(
                "recovered target with store-side .drv + persisted inputDrv row \
                 attests via GetPath → derivations-table fallback",
            );
        assert!(got.contains(&src), "inputSrcs entry missing: {got:?}");
        assert!(
            got.contains(&sub_out),
            "inputDrv output (from derivations.expected_output_paths via \
             store-fetched ATerm) missing: {got:?}"
        );
        assert!(
            got.contains(&parent_drv),
            "own .drv path must be seeded (W03): {got:?}"
        );
        assert!(
            got.contains(&sub_drv),
            "inputDrv .drv path must be seeded (W03): {got:?}"
        );
    }

    /// Recovered target whose `.drv` is not in the store either
    /// (GetPath → NotFound) → no attestation. Covers the
    /// `drv_fetch_failed` arm.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_drv_fetch_fails() {
        let (_t, db) = test_db().await;
        let (_store, store_client) = rio_test_support::grpc::spawn_mock_store_inproc()
            .await
            .unwrap();
        let mut dag = DerivationDag::new();

        // Parent has empty drv_content; store has nothing seeded at
        // its drv_path → GetPath returns NotFound.
        let parent = DerivationState::try_from_node(
            &make_derivation_node("attest-parent", "x86_64-linux").into(),
        )
        .unwrap();
        dag.insert_recovered_node(parent);

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db, Some(&store_client))
                .await
                .unwrap()
                .is_none(),
            "drv_content empty + GetPath NotFound → must not attest"
        );
    }

    /// An inputDrv not in the in-memory DAG (substituted-then-reaped,
    /// or completed pre-restart — the FOD shape: curl/bash/stdenv) but
    /// with a persisted `derivations` row → attests with the row's
    /// `expected_output_paths` as seeds, plus the `.drv` paths
    /// themselves (W03 forward-compat).
    ///
    /// Under ADR-022 closure-scoped castore-FUSE an empty
    /// `input_roots` makes the builder's own drv-parsed fallback EIO,
    /// so a DAG miss must NOT degrade to no-attestation when the
    /// persisted authority can supply the outputs. Regression test for
    /// the 1331-stuck-FOD shape.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_fall_back_to_pg_for_substituted_input_drv() {
        let (t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let sub_drv = rio_test_support::fixtures::test_drv_path("attest-substituted");
        let sub_out = rio_test_support::fixtures::test_store_path("attest-substituted-out");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        let parent_drv = rio_test_support::fixtures::test_drv_path("attest-parent");

        // The inputDrv is NOT a DAG node, but its persisted
        // `derivations` row carries expected_output_paths.
        put_derivation_row(&t.pool, &sub_drv, &[&sub_out]).await;
        dag.insert_recovered_node(make_attest_parent(&sub_drv, &src));

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
            .await
            .unwrap()
            .expect("DAG-missed inputDrv with a derivations row attests via PG fallback");
        assert!(got.contains(&src), "inputSrcs entry missing: {got:?}");
        assert!(
            got.contains(&sub_out),
            "substituted inputDrv output (from derivations.expected_output_paths) \
             missing: {got:?}"
        );
        assert!(
            got.contains(&parent_drv),
            "own .drv path must be seeded (W03): {got:?}"
        );
        assert!(
            got.contains(&sub_drv),
            "inputDrv .drv path must be seeded (W03): {got:?}"
        );
    }

    /// Never-narrower: a 3-output inputDrv where the parent consumes
    /// only `["out"]`, narinfo has `dev`+`man` rows with `deriver` set
    /// but `out` has `deriver=NULL`. A name-blind `narinfo.deriver`
    /// reverse-lookup with a `len() >= consumed` count-check would
    /// pass (2 ≥ 1) and seed `[dev, man]` — silently dropping `out`,
    /// the one output the build actually references. The
    /// `derivations`-table resolver returns the full
    /// `expected_output_paths` instead, so `out` is seeded regardless
    /// of narinfo's deriver state.
    // r[verify sched.dispatch.input-roots+3]
    // r[verify sched.dispatch.never-narrower]
    #[tokio::test]
    async fn attested_seeds_never_narrower_multi_output() {
        let (t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let multi_drv = rio_test_support::fixtures::test_drv_path("attest-multi");
        let out = rio_test_support::fixtures::test_store_path("attest-multi-out");
        let dev = rio_test_support::fixtures::test_store_path("attest-multi-dev");
        let man = rio_test_support::fixtures::test_store_path("attest-multi-man");
        let src = rio_test_support::fixtures::test_store_path("attest-src");

        // narinfo state that would fool a deriver-count heuristic:
        // dev+man have deriver set, out has deriver NULL.
        put_narinfo_with_deriver(&t.pool, &dev, Some(&multi_drv)).await;
        put_narinfo_with_deriver(&t.pool, &man, Some(&multi_drv)).await;
        put_narinfo_with_deriver(&t.pool, &out, None).await;
        // The persisted authority: full expected_output_paths.
        put_derivation_row(&t.pool, &multi_drv, &[&out, &dev, &man]).await;

        // Parent declares it consumes ["out"] only (make_attest_parent
        // builds inputDrvs=[(child,["out"])]).
        dag.insert_recovered_node(make_attest_parent(&multi_drv, &src));

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
            .await
            .unwrap()
            .expect("derivations-table resolver attests the full output set");
        assert!(
            got.contains(&out),
            "the consumed output `out` MUST be seeded even though its \
             narinfo.deriver is NULL — never-narrower: {got:?}"
        );
    }

    /// An inputDrv not in the DAG AND with no `derivations` row
    /// (genuinely never merged on this cluster) → no attestation.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_input_drv_unresolvable() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let missing_child = rio_test_support::fixtures::test_drv_path("attest-gone-child");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&missing_child, &src));

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
                .await
                .unwrap()
                .is_none(),
            "inputDrv missing from DAG AND derivations table → must not attest"
        );
    }

    /// An inputDrv not in the DAG whose `derivations` row has a
    /// floating-CA placeholder `[""]` → no attestation (the consumed
    /// output is unknowable pre-build).
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_pg_expected_paths_are_placeholders() {
        let (t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let ca_drv = rio_test_support::fixtures::test_drv_path("attest-floating-ca");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        put_derivation_row(&t.pool, &ca_drv, &[""]).await;
        dag.insert_recovered_node(make_attest_parent(&ca_drv, &src));

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
                .await
                .unwrap()
                .is_none(),
            "floating-CA placeholder in derivations row → must not attest"
        );
    }

    /// An inputDrv child whose output paths aren't known yet (floating-
    /// CA placeholder "" and no realized paths) → no attestation.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_child_outputs_unknown() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv_path = rio_test_support::fixtures::test_drv_path("attest-child");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-child", "x86_64-linux").into(),
        )
        .unwrap();
        child.expected_output_paths = vec![String::new()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db, None)
                .await
                .unwrap()
                .is_none(),
            "unknown child output path → must not attest"
        );
    }
}
