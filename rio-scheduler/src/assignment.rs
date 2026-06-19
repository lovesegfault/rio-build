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
use tonic::transport::Channel;

/// Per-arm `Ok(None)` accounting for [`attested_input_seeds`] (and the
/// dispatch hoist's `arm="drv_fetched"` success counter). Single-sourced
/// here so the in-fn `seeds_unknown!` macro and the dispatch hoist
/// can't drift on the literal. `lib.rs::register_metrics` keeps a
/// literal — the `xtask regen docs-data` / `helm-obs` source-scrapes
/// match `describe_*!("…")` literals only.
pub(crate) const METRIC_ATTESTED_SEEDS_UNKNOWN: &str = "rio_scheduler_attested_seeds_unknown_total";

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
/// CONSUMED outputs of every `inputDrvs` entry (the
/// `input_drvs()[child]` set — exactly what the build references;
/// unconsumed split outputs like `bash-{man,doc,debug}` are not
/// inputs and frequently lack a narinfo). The `.drv` paths are seeded so
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
/// sets `drv_content = Vec::new()`) is repopulated by the dispatch
/// callsite's hoisted `GetPath` fetch (written back to
/// `dag.node_mut().drv_content`) before this function is called, so
/// the parsed `inputDrvs` set is identical to what the gateway
/// originally inlined and the never-narrower invariant holds by
/// construction. If that fetch failed (store unconfigured, NotFound,
/// timeout) the node still has empty `drv_content` → `Ok(None)` with
/// `arm="drv_empty"`.
///
/// `None` → the dispatch site sends an empty closure/digest. Under
/// ADR-022 closure-scoped castore-FUSE this is NOT a safe degrade —
/// the builder's own drv-parsed BFS may EIO reading through the
/// empty-scoped mount, so the assignment infra-retries instead of
/// silently widening. `None` is therefore reserved for cases the
/// scheduler genuinely cannot resolve (an `inputDrvs` entry never
/// merged on this cluster, a floating-CA placeholder, or the dispatch
/// hoist's GetPath fetch failing). This keeps the invariant structural: state
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
) -> Result<Option<Vec<String>>, sqlx::Error> {
    /// Per-arm `Ok(None)` accounting. Macro so each arm is one line at
    /// the return site (and so the literal label is the metric label —
    /// no enum/match indirection to keep in sync).
    macro_rules! seeds_unknown {
        ($arm:literal) => {{
            metrics::counter!(METRIC_ATTESTED_SEEDS_UNKNOWN, "arm" => $arm).increment(1);
            return Ok(None);
        }};
    }

    let Some(node) = dag.node(drv_hash) else {
        seeds_unknown!("no_node");
    };
    // Recovered nodes have `drv_content = Vec::new()`
    // (`from_recovery_row` does not persist the ATerm). The dispatch
    // callsite's hoisted GetPath fetch repopulates the node BEFORE
    // calling this function (writing back to `dag.node_mut()` so
    // retries don't re-fetch); empty here means the hoist failed or
    // never ran (store unconfigured / Materialization kind).
    if node.drv_content.is_empty() {
        seeds_unknown!("drv_empty");
    }
    let Some(drv) = std::str::from_utf8(&node.drv_content)
        .ok()
        .and_then(|s| rio_nix::derivation::Derivation::parse(s).ok())
    else {
        seeds_unknown!("drv_unparseable");
    };

    // Own .drv path + inputSrcs first (declared in the ATerm; exact).
    let mut seeds: Vec<String> = vec![node.drv_path().to_string()];
    seeds.extend(drv.input_srcs().iter().cloned());

    // inputDrvs not in the in-memory DAG, batched into one
    // `derivations.(output_names, expected_output_paths)` lookup after
    // the loop. Each entry carries its consumed-output-name set so the
    // PG fallback applies the same name filter as the DAG arm.
    let mut dag_missed: Vec<(&String, &std::collections::BTreeSet<String>)> = Vec::new();
    for (input_drv_path, consumed) in drv.input_drvs() {
        // Seed the inputDrv's .drv path unconditionally (W03
        // forward-compat: nix-daemon reads it through FUSE).
        seeds.push(input_drv_path.clone());
        let Some(child) = dag.hash_for_path(input_drv_path).and_then(|h| dag.node(h)) else {
            dag_missed.push((input_drv_path, consumed));
            continue;
        };
        // Seed only the outputs the parent's `inputDrvs` declares it
        // consumes. The build references exactly these — unconsumed
        // sibling outputs (`bash-{man,doc,debug}`, …) are not inputs
        // and frequently have NO narinfo (never built / never
        // substituted), so seeding them degrades `compute_input_roots`
        // to `Ok(None)` for every drv that depends on `bash`/`curl`.
        // Never-narrower holds: a consumed name with no resolvable
        // path → no attestation.
        //
        // `output_names` ↔ `expected_output_paths` are positional
        // (both populated from the proto at merge time, and both
        // persisted by `batch_upsert_derivations`).
        match seed_consumed(&child.output_names, &child.expected_output_paths, consumed) {
            SeedConsumed::Resolved(paths) => seeds.extend(paths),
            // Floating-CA placeholder: degrade. The realized
            // `output_paths` list is NOT name-keyed (worker-reported
            // `buffer_unordered` order, may be shorter than
            // `output_names` per completion.rs's `built_outputs.len()
            // ≤ output_names.len()`) so seeding it wholesale cannot be
            // proven never-narrower — four review rounds found a new
            // edge case each time (filter-inverts; `[""]`-passes-len;
            // short-but-clean; iteration-order masking). The proper
            // fix is the name-keyed `realisations` table:
            //
            // TODO: a locally-built floating-CA child degrades here
            // even though `realisations` (002_store.sql:134) persists
            // the (modular_hash, output_name) → output_path mapping —
            // a `derivations.drv_path → modular_hash → realisations`
            // join would resolve the consumed name exactly. Same join
            // covers the PG-fallback arm below.
            SeedConsumed::Placeholder => seeds_unknown!("child_output_unknown"),
            // Consumed name not in `child.output_names` (malformed
            // graph — Nix's evaluator would reject, but the scheduler
            // path doesn't cross-validate `inputDrvs` against
            // `output_names`). Degrade per the never-narrower
            // contract.
            SeedConsumed::Unresolvable => seeds_unknown!("input_consumes_undeclared_output"),
        }
    }

    if !dag_missed.is_empty() {
        let missed_paths: Vec<String> = dag_missed.iter().map(|(p, _)| (*p).clone()).collect();
        let by_drv = db.expected_outputs_by_drv_path(&missed_paths).await?;
        for (drv_path, consumed) in &dag_missed {
            // The `derivations` row carries the same
            // `(output_names, expected_output_paths)` zip a DAG node
            // would — written from the gateway-parsed ATerm at merge
            // time and surviving reap. No name-blind reverse lookup,
            // no count heuristic: never-narrower by construction. A
            // floating-CA `""` for a consumed name degrades (same
            // `realisations`-join TODO as the DAG arm above); an
            // absent row means genuinely never merged.
            let arm = match by_drv
                .get(*drv_path)
                .map(|(names, paths)| seed_consumed(names, paths, consumed))
            {
                Some(SeedConsumed::Resolved(paths)) => {
                    seeds.extend(paths);
                    metrics::counter!(
                        "rio_scheduler_attested_seeds_pg_fallback_total",
                        "outcome" => "resolved"
                    )
                    .increment(1);
                    continue;
                }
                // Same arm labels as the DAG path so a triage doesn't
                // mis-bucket on which lookup hit.
                Some(SeedConsumed::Placeholder) => "child_output_unknown",
                Some(SeedConsumed::Unresolvable) => "input_consumes_undeclared_output",
                None => "input_drv_unresolved",
            };
            tracing::debug!(
                input_drv = %drv_path,
                arm,
                "inputDrv not in DAG and derivations-table fallback \
                 cannot establish its consumed outputs; degrading to unattested"
            );
            metrics::counter!(
                "rio_scheduler_attested_seeds_pg_fallback_total",
                "outcome" => "degraded_none"
            )
            .increment(1);
            metrics::counter!(METRIC_ATTESTED_SEEDS_UNKNOWN, "arm" => arm).increment(1);
            return Ok(None);
        }
    }

    Ok(Some(seeds))
}

/// [`seed_consumed`] outcome — 3-state so callers emit distinct
/// `seeds_unknown` arm labels (`child_output_unknown` vs
/// `input_consumes_undeclared_output`) for the two degrade paths.
/// Both degrade; only `Resolved` seeds.
enum SeedConsumed {
    /// Every consumed name resolved to a non-empty path.
    Resolved(Vec<String>),
    /// At least one consumed name's path is the floating-CA `""`
    /// placeholder. Degrade — the `realisations`-table join (TODO at
    /// the DAG-arm callsite) is the only never-narrower-safe escape
    /// hatch.
    Placeholder,
    /// At least one consumed name is not in `names`, or `names`/`paths`
    /// length skew. The seed set CANNOT cover this input — degrade.
    /// Dominates `Placeholder` when both co-occur.
    Unresolvable,
}

/// Resolve the path of every `consumed` output name via the positional
/// `names` ↔ `paths` zip. The never-narrower gate: a consumed output
/// the seed set cannot cover means no attestation. Returns owned paths
/// on `Resolved` (no partial mutation of caller state otherwise).
///
/// Kin of [`rio_common::wanted_outputs::verifiable_wanted_paths`] (the
/// demand-driven completeness predicate's zip-filter): same
/// `names ↔ paths` positional invariant, same degrade-on-unverifiable
/// contract; differs in that `consumed` is an explicit name set (no
/// empty-means-all sentinel — `inputDrvs` always names outputs) and
/// `Placeholder` is distinguished from `Unresolvable` for the metric
/// arm labels (both degrade). The merged_bug_026
/// length-skew guard is a release-mode check (matching
/// `verifiable_wanted_paths`); the producer invariant (`translate.rs`
/// `unzip`, `batch_upsert_derivations` writing both columns in one
/// statement) keeps it unreachable.
fn seed_consumed(
    names: &[String],
    paths: &[String],
    consumed: &std::collections::BTreeSet<String>,
) -> SeedConsumed {
    if names.len() != paths.len() {
        debug_assert_eq!(
            names.len(),
            paths.len(),
            "output_names ↔ expected_output_paths positional invariant (merged_bug_026)"
        );
        return SeedConsumed::Unresolvable;
    }
    // Scan every consumed name: `Unresolvable` dominates `Placeholder`
    // (an undeclared name means the seed set CANNOT cover this input
    // regardless of what the other names resolve to), so a first-hit
    // early-return would let BTreeSet iteration order decide which
    // wins — the lexicographically-smallest name's outcome would mask
    // the rest. The metric arm distinction (`child_output_unknown` vs
    // `input_consumes_undeclared_output`) is the load-bearing reason
    // to keep the 3-state split.
    let mut resolved = Vec::with_capacity(consumed.len());
    let mut saw_placeholder = false;
    for name in consumed {
        let Some(i) = names.iter().position(|n| n == name) else {
            return SeedConsumed::Unresolvable;
        };
        // Safe: len-checked above.
        if paths[i].is_empty() {
            saw_placeholder = true;
        } else {
            resolved.push(paths[i].clone());
        }
    }
    if saw_placeholder {
        SeedConsumed::Placeholder
    } else {
        SeedConsumed::Resolved(resolved)
    }
}

/// Outcome of [`fetch_drv_aterm`]: distinguishes a definitive miss
/// (store says NotFound / InvalidArgument, or it returned bytes that
/// don't unwrap to a `.drv`, or any non-transient gRPC code) from a
/// transient failure ([`NarCollectError::is_transient`](rio_proto::client::NarCollectError::is_transient): Unavailable /
/// Unknown / ResourceExhausted / Aborted; plus `DeadlineExceeded` —
/// the dispatch hoist retries with backoff so the FUSE-thread
/// "compounds the wait" exclusion doesn't apply). The dispatch hoist
/// negative-caches the former (re-RPCing the same `drv_path` won't
/// change the answer) but NOT the latter (a blip self-heals on the
/// next dispatch).
#[derive(Debug)]
pub(crate) enum DrvFetch {
    /// ATerm bytes successfully fetched and NAR-unwrapped.
    Found(Vec<u8>),
    /// Store reachable, definitively says the `.drv` is absent (or
    /// present-but-unusable: NAR-unwrap failed). Negative-cache.
    Missing,
    /// [`NarCollectError::is_transient`](rio_proto::client::NarCollectError::is_transient) (Unavailable / Unknown /
    /// ResourceExhausted / Aborted), `DeadlineExceeded` /
    /// `Cancelled`, or the dispatch callsite's outer `grpc_timeout`.
    /// NOT negative-cached — retry on next dispatch.
    Transient,
}

/// Fetch a `.drv`'s ATerm bytes from the store via `GetPath`.
///
/// The store returns NAR-framed bytes; a `.drv` is a single regular
/// file, so [`rio_nix::nar::extract_single_file`] unwraps it to the
/// raw ATerm. Same path the worker takes when
/// `WorkAssignment.drv_content` is empty
/// (`rio-builder/src/executor/inputs.rs::fetch_drv_from_store`).
///
/// `Transient` only on [`NarCollectError::is_transient`](rio_proto::client::NarCollectError::is_transient) (the
/// `rio_common::grpc::is_transient` allowlist). Everything else —
/// NotFound, InvalidArgument, NAR-unwrap failure, NAR > 1 MiB (a
/// `.drv` is ~1-50 KB ASCII; 1 MiB is ~20× any real `.drv`),
/// Validation, Io, non-allowlist gRPC codes — is `Missing`
/// (definitive: re-fetching the same `drv_path` won't change the
/// answer). The 2s per-chunk idle bound covers a slow store without
/// blocking dispatch (the dispatch callsite also wraps the whole call
/// in `grpc_timeout`).
///
/// Sole caller: `dispatch.rs::build_assignment_proto`'s hoisted
/// recovered-target fetch, which writes a `Found` result back to
/// `dag.node_mut()` via `rehydrate_from_aterm` and stamps
/// `drv_fetch_attempted` on `Found`/`Missing` (not `Transient`) so
/// retries (and `maybe_resolve_ca`) read from memory.
pub(crate) async fn fetch_drv_aterm(
    client: &mut StoreServiceClient<Channel>,
    drv_path: &str,
) -> DrvFetch {
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
            return DrvFetch::Missing;
        }
        Err(e) if e.is_not_found() => {
            tracing::debug!(%drv_path, error = %e, "drv_content fetch: GetPath NotFound");
            return DrvFetch::Missing;
        }
        Err(e) if e.is_transient() => {
            tracing::debug!(%drv_path, error = %e, "drv_content fetch: GetPath transient error");
            return DrvFetch::Transient;
        }
        // refusal-census: allow(per-callsite override of is_transient's
        // DeadlineExceeded/Cancelled exclusion — backoff-retried, not
        // tight-loop; centralizing as is_transient_with_backoff is the
        // R7 follow-up named in the iter-5 review)
        Err(e)
            if matches!(
                e.grpc_code(),
                Some(tonic::Code::DeadlineExceeded | tonic::Code::Cancelled)
            ) =>
        {
            // `is_transient` excludes DeadlineExceeded by design (its
            // rationale: "retrying with the same idle bound won't help
            // on a FUSE-thread caller"). That doesn't apply here — the
            // dispatch hoist retries on the next build attempt with
            // backoff, not immediately. The 2s per-chunk idle bound
            // (`collect_nar_stream` synthesizes this status on a >2s
            // gap) and store-side `deadline_exceeded` on backend stall
            // are blips, not a verdict on `drv_path`. `Cancelled` is
            // the same class: a rolling-restart RST_STREAM /
            // SIGTERM-on-the-store maps to Cancelled depending on
            // tonic/h2/envoy mapping (`rio-proto`'s
            // `RefusalKind::Undecided` groups it with the transport
            // codes, not per-request verdicts) — also a blip.
            tracing::debug!(%drv_path, error = %e, "drv_content fetch: GetPath deadline/cancelled (transient)");
            return DrvFetch::Transient;
        }
        Err(e) => {
            // Definitive: InvalidArgument (store-path didn't parse —
            // `NarCollectError::is_invalid_argument`'s "treat as
            // ENOENT, NOT retry-worthy"), SizeExceeded / Validation
            // (the bytes won't change on retry), Io (local disk), and
            // any non-allowlist gRPC code (PermissionDenied / Internal
            // / FailedPrecondition — re-RPCing the same path won't
            // resolve auth/config). Catch-all is `Missing` (latch) so
            // future `rio_common::grpc::is_transient` allowlist
            // changes propagate without a matching edit here; the
            // iter-2 `Err(_) => Transient` re-fired a doomed in-actor
            // RPC every retry (up to ~1 MiB for SizeExceeded).
            tracing::debug!(
                %drv_path, error = %e,
                "drv_content fetch: definitive non-NotFound error (latching)"
            );
            return DrvFetch::Missing;
        }
    };
    match rio_nix::nar::extract_single_file(&nar) {
        Ok(bytes) => DrvFetch::Found(bytes),
        Err(e) => {
            tracing::debug!(
                %drv_path, error = %e,
                "drv_content fetch: NAR unwrap failed (not a single regular file)"
            );
            // The store has SOMETHING at this path that isn't a
            // single-file `.drv`. Re-fetching won't change that.
            DrvFetch::Missing
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
    /// `(output_names, expected_output_paths)` — the persisted shape a
    /// substituted-then-reaped (or completed-pre-restart) inputDrv
    /// has: written at merge by `batch_upsert_derivations`, surviving
    /// in PG after the in-memory DAG node is gone.
    async fn put_derivation_row(
        pool: &sqlx::PgPool,
        drv_path: &str,
        output_names: &[&str],
        expected_outputs: &[&str],
    ) {
        let names: Vec<String> = output_names.iter().map(|s| s.to_string()).collect();
        let outs: Vec<String> = expected_outputs.iter().map(|s| s.to_string()).collect();
        // concat! keeps `INSERT INTO` and the table name on separate
        // source lines so the production-write fence
        // (`derivations_sql_confined_to_embedded_sources`, a per-line
        // scan that cannot see #[cfg(test)]) does not false-positive
        // on this test fixture.
        sqlx::query(concat!(
            "INSERT INTO ",
            "derivations ",
            "(drv_hash, drv_path, system, status, output_names, expected_output_paths) ",
            "VALUES ($1, $1, 'x86_64-linux', 'completed', $2, $3)",
        ))
        .bind(drv_path)
        .bind(&names)
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
        // IA happy-path shape: `output_names ↔ expected_output_paths`
        // positional (the merged_bug_026 producer invariant
        // `seed_consumed` debug-asserts).
        child.expected_output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db)
            .await
            .unwrap()
            .expect("parsed drv with resolvable inputs is attestable");
        assert!(got.contains(&src), "inputSrcs entry missing: {got:?}");
        assert!(
            got.contains(&child_out),
            "inputDrv child's consumed output missing: {got:?}"
        );
    }

    /// `drv_content = Vec::new()` (the `from_recovery_row` shape, or
    /// the gateway didn't inline) → the exact direct-input set cannot
    /// be established → no attestation, even though a DAG child with a
    /// known output exists (the approximation would have produced a
    /// non-empty — and possibly narrower-than-true — seed set).
    ///
    /// The dispatch callsite's hoisted GetPath fetch repopulates
    /// `drv_content` before this function is called; this test pins
    /// the `drv_empty` arm for when that hoist failed (store
    /// unconfigured / NotFound / timeout). The hoist+writeback itself
    /// is pinned by `dispatch_caches_recovered_drv_content`.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_none_when_drv_content_empty() {
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

        let parent = DerivationState::try_from_node(
            &make_derivation_node("attest-parent", "x86_64-linux").into(),
        )
        .unwrap();
        dag.insert_recovered_node(parent);
        dag.insert_recovered_edge("attest-parent".into(), "attest-child".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
                .await
                .unwrap()
                .is_none(),
            "no parsed .drv → must not attest"
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
        put_derivation_row(&t.pool, &sub_drv, &["out"], &[&sub_out]).await;
        dag.insert_recovered_node(make_attest_parent(&sub_drv, &src));

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db)
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

    /// Round-3 dag-actor stall: an inputDrv with split outputs
    /// (`[out, man, doc, debug]`) where the parent consumes only
    /// `["out"]` must seed only the `out` path. Seeding all four
    /// `expected_output_paths` is over-broad for the refscan candidate
    /// set (harmless there) but fatal for `compute_input_roots`: the
    /// unconsumed split outputs (`-man`, `-doc`, `-debug`) are never
    /// built or substituted (nothing wants them), so they have no
    /// narinfo row → the closure walk degrades to unattested for every
    /// derivation that depends on `bash`/`curl`/etc.
    ///
    /// The consumed-output set is exactly `drv.input_drvs()[child]` —
    /// the build only references what `inputDrvs` declares — so
    /// seeding consumed-only is never-narrower by construction.
    // r[verify sched.dispatch.input-roots+3]
    // r[verify sched.dispatch.never-narrower]
    #[tokio::test]
    async fn attested_seeds_only_consumed_outputs() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv = rio_test_support::fixtures::test_drv_path("attest-split");
        let p_out = rio_test_support::fixtures::test_store_path("attest-split-out");
        let p_man = rio_test_support::fixtures::test_store_path("attest-split-man");
        let p_doc = rio_test_support::fixtures::test_store_path("attest-split-doc");
        let p_debug = rio_test_support::fixtures::test_store_path("attest-split-debug");
        let src = rio_test_support::fixtures::test_store_path("attest-src");

        // Child declares 4 outputs; expected_output_paths is positional
        // with output_names (both come from the proto at merge time).
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-split", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_names = vec!["out".into(), "man".into(), "doc".into(), "debug".into()];
        child.expected_output_paths =
            vec![p_out.clone(), p_man.clone(), p_doc.clone(), p_debug.clone()];
        dag.insert_recovered_node(child);

        // Parent's inputDrvs declares it consumes ["out"] only
        // (make_attest_parent builds inputDrvs=[(child,["out"])]).
        dag.insert_recovered_node(make_attest_parent(&child_drv, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-split".into());

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db)
            .await
            .unwrap()
            .expect("split-output inputDrv with the consumed output known is attestable");
        assert!(
            got.contains(&p_out),
            "the consumed output `out` must be seeded: {got:?}"
        );
        assert!(
            !got.contains(&p_man) && !got.contains(&p_doc) && !got.contains(&p_debug),
            "unconsumed split outputs must NOT be seeded — they have no \
             narinfo and would degrade compute_input_roots to None: {got:?}"
        );
    }

    /// Same as [`attested_seeds_only_consumed_outputs`] but through the
    /// PG fallback (inputDrv not in the in-memory DAG). The persisted
    /// `derivations` row carries `(output_names, expected_output_paths)`
    /// and the consumed-name filter applies there too.
    // r[verify sched.dispatch.input-roots+3]
    #[tokio::test]
    async fn attested_seeds_only_consumed_outputs_pg_fallback() {
        let (t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv = rio_test_support::fixtures::test_drv_path("attest-split-pg");
        let p_out = rio_test_support::fixtures::test_store_path("attest-split-pg-out");
        let p_man = rio_test_support::fixtures::test_store_path("attest-split-pg-man");
        let src = rio_test_support::fixtures::test_store_path("attest-src");

        // No DAG node — only the persisted row.
        put_derivation_row(&t.pool, &child_drv, &["out", "man"], &[&p_out, &p_man]).await;
        dag.insert_recovered_node(make_attest_parent(&child_drv, &src));

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db)
            .await
            .unwrap()
            .expect("PG fallback resolves the consumed output");
        assert!(got.contains(&p_out), "consumed `out` seeded: {got:?}");
        assert!(
            !got.contains(&p_man),
            "unconsumed `man` must NOT be seeded via PG fallback: {got:?}"
        );
    }

    /// Never-narrower: a 3-output inputDrv where the parent consumes
    /// only `["out"]`, narinfo has `dev`+`man` rows with `deriver` set
    /// but `out` has `deriver=NULL`. A name-blind `narinfo.deriver`
    /// reverse-lookup with a `len() >= consumed` count-check would
    /// pass (2 ≥ 1) and seed `[dev, man]` — silently dropping `out`,
    /// the one output the build actually references. The
    /// `derivations`-table resolver name-keys the consumed output
    /// instead, so `out` is seeded (and `dev`/`man` are NOT) regardless
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
        // dev+man have deriver set, out has deriver NULL. Unread by
        // `seed_consumed` itself — that's the point: this fixture
        // catches a regression to a deriver-based resolver.
        put_narinfo_with_deriver(&t.pool, &dev, Some(&multi_drv)).await;
        put_narinfo_with_deriver(&t.pool, &man, Some(&multi_drv)).await;
        put_narinfo_with_deriver(&t.pool, &out, None).await;
        // The persisted authority: full (output_names, expected_output_paths).
        put_derivation_row(
            &t.pool,
            &multi_drv,
            &["out", "dev", "man"],
            &[&out, &dev, &man],
        )
        .await;

        // Parent declares it consumes ["out"] only (make_attest_parent
        // builds inputDrvs=[(child,["out"])]).
        dag.insert_recovered_node(make_attest_parent(&multi_drv, &src));

        let got = attested_input_seeds(&dag, &"attest-parent".into(), &db)
            .await
            .unwrap()
            .expect("derivations-table resolver name-keys the consumed output");
        assert!(
            got.contains(&out),
            "the consumed output `out` MUST be seeded even though its \
             narinfo.deriver is NULL — never-narrower: {got:?}"
        );
        assert!(
            !got.contains(&dev) && !got.contains(&man),
            "unconsumed `dev`/`man` must NOT be seeded even though their \
             narinfo.deriver IS set: {got:?}"
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
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
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
        put_derivation_row(&t.pool, &ca_drv, &["out"], &[""]).await;
        dag.insert_recovered_node(make_attest_parent(&ca_drv, &src));

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
                .await
                .unwrap()
                .is_none(),
            "floating-CA placeholder in derivations row → must not attest"
        );
    }

    /// Never-narrower: an inputDrv child with
    /// `expected_output_paths = [""]` (floating-CA placeholder) AND
    /// `output_paths = [""]` (`complete_ready_from_store_batch` cloned
    /// expected into realized for an IA that turned out floating-CA).
    /// `Placeholder` degrades unconditionally — the wholesale
    /// `output_paths` fallback was DROPPED (the realized list is not
    /// name-keyed and `built_outputs.len() ≤ output_names.len()`, so
    /// it cannot be proven never-narrower; see the `realisations`-join
    /// TODO at the DAG-arm callsite). This test pins that a non-empty
    /// realized list does NOT resurrect the fallback.
    // r[verify sched.dispatch.never-narrower]
    #[tokio::test]
    async fn attested_seeds_never_narrower_placeholder_in_realized() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv_path = rio_test_support::fixtures::test_drv_path("attest-ca-clone");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-ca-clone", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_names = vec!["out".into()];
        child.expected_output_paths = vec![String::new()];
        // The bug shape: realized list non-empty (passes the len guard)
        // but every entry the placeholder `""` (filter drops them all).
        child.output_paths = vec![String::new()];
        dag.insert_recovered_node(child);

        let src = rio_test_support::fixtures::test_store_path("attest-src");
        dag.insert_recovered_node(make_attest_parent(&child_drv_path, &src));
        dag.insert_recovered_edge("attest-parent".into(), "attest-ca-clone".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
                .await
                .unwrap()
                .is_none(),
            "Placeholder degrades unconditionally; a non-empty realized \
             list must NOT resurrect the dropped wholesale fallback"
        );
    }

    /// `seed_consumed`: `Unresolvable` dominates `Placeholder` when
    /// both co-occur, regardless of BTreeSet iteration order. Iter-3's
    /// first-hit early-return let the lexicographically-smallest
    /// consumed name's outcome mask the rest — `{"lib","zzz"}` with
    /// `lib→""` and `zzz` undeclared returned `Placeholder` (lib sorts
    /// first), but `{"aaa","lib"}` returned `Unresolvable`.
    // r[verify sched.dispatch.never-narrower]
    #[test]
    fn seed_consumed_unresolvable_dominates_placeholder() {
        use std::collections::BTreeSet;
        let names = vec!["lib".to_string(), "out".to_string()];
        let paths = vec![String::new(), "/nix/store/x-out".to_string()];
        // Placeholder ("lib"→"") sorts before undeclared ("zzz").
        let consumed: BTreeSet<String> = ["lib".into(), "zzz".into()].into();
        assert!(
            matches!(
                seed_consumed(&names, &paths, &consumed),
                SeedConsumed::Unresolvable
            ),
            "undeclared name must dominate placeholder regardless of iteration order"
        );
        // Reverse: undeclared sorts first — same outcome.
        let consumed: BTreeSet<String> = ["aaa".into(), "lib".into()].into();
        assert!(matches!(
            seed_consumed(&names, &paths, &consumed),
            SeedConsumed::Unresolvable
        ));
    }

    /// Never-narrower: parent's `inputDrvs` declares `["dev"]` but the
    /// child only declares `["out"]` — malformed graph (Nix's evaluator
    /// would reject it), but nothing in the scheduler path
    /// cross-validates `inputDrvs` against `child.output_names`. The
    /// 3-state `SeedConsumed` keeps undeclared-name distinct from the
    /// floating-CA placeholder for the metric arm label; both degrade.
    // r[verify sched.dispatch.never-narrower]
    #[tokio::test]
    async fn attested_seeds_degrade_on_undeclared_consumed_name() {
        let (_t, db) = test_db().await;
        let mut dag = DerivationDag::new();
        let child_drv = rio_test_support::fixtures::test_drv_path("attest-undecl");
        let child_out = rio_test_support::fixtures::test_store_path("attest-undecl-out");
        let mut child = DerivationState::try_from_node(
            &make_derivation_node("attest-undecl", "x86_64-linux").into(),
        )
        .unwrap();
        child.output_names = vec!["out".into()];
        child.expected_output_paths = vec![child_out.clone()];
        // Non-empty realized list — pins that the dropped wholesale
        // `output_paths` fallback stays dropped (it would have seeded
        // `[out]` for a parent that consumes `dev`).
        child.output_paths = vec![child_out.clone()];
        dag.insert_recovered_node(child);

        // Parent inputDrvs=[(child, ["dev"])] — `dev` is NOT in the
        // child's output_names.
        let parent_out = rio_test_support::fixtures::test_store_path("attest-undecl-parent-out");
        let src = rio_test_support::fixtures::test_store_path("attest-src");
        let aterm = format!(
            r#"Derive([("out","{parent_out}","","")],[("{child_drv}",["dev"])],["{src}"],"x86_64-linux","/bin/sh",[],[("out","{parent_out}")])"#
        );
        let mut parent_node = make_derivation_node("attest-parent", "x86_64-linux");
        parent_node.drv_content = aterm.into_bytes();
        dag.insert_recovered_node(DerivationState::try_from_node(&parent_node.into()).unwrap());
        dag.insert_recovered_edge("attest-parent".into(), "attest-undecl".into());

        assert!(
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
                .await
                .unwrap()
                .is_none(),
            "consumed name `dev` not in child.output_names → the seed set \
             CANNOT cover this input; must degrade to None (never-narrower)"
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
            attested_input_seeds(&dag, &"attest-parent".into(), &db)
                .await
                .unwrap()
                .is_none(),
            "unknown child output path → must not attest"
        );
    }
}
