//! DAG reconstruction and gRPC request building.
//!
//! Translates the per-session derivation cache into `SubmitBuildRequest`
//! messages for the scheduler, walking `inputDrvs` recursively to build
//! the full derivation graph.
// r[impl gw.dag.reconstruct]

use std::collections::{HashMap, HashSet, VecDeque};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::{BasicDerivation, Derivation, DerivationLike};
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tonic::transport::Channel;
use tracing::{debug, warn};

/// Per-node inline threshold. Most .drv files are 1-10 KB; 64 KB is
/// a generous cap. Anything larger is probably a generated derivation
/// with a huge env (flake inputs serialized) — not worth the bandwidth
/// savings, let the worker fetch it.
const MAX_INLINE_DRV_BYTES: usize = 64 * 1024;

/// Total budget across ALL inlined nodes in one SubmitBuild. Half the
/// gRPC message limit (32 MB). Cold cache with 10k drvs × 10 KB each
/// = 100 MB — WAY over. The budget means we inline the first ~1600
/// average-size drvs, then the rest fall back to worker-fetch. That's
/// still a huge win over inlining zero.
const INLINE_BUDGET_BYTES: usize = 16 * 1024 * 1024;

use crate::handler::{ClientOptions, resolve_derivation};

/// Maximum number of transitive input derivations to resolve (DoS prevention).
pub(crate) const MAX_TRANSITIVE_INPUTS: usize = 10_000;

/// Reconstruct the full derivation DAG starting from a root derivation.
///
/// Performs a BFS walk of `inputDrvs` to discover all transitive dependencies,
/// fetching missing derivations from the store via gRPC as needed.
///
/// Returns `(nodes, edges)` for `SubmitBuildRequest`.
pub async fn reconstruct_dag(
    root_path: &StorePath,
    root_drv: &Derivation,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<(Vec<types::DerivationNode>, Vec<types::DerivationEdge>)> {
    let mut nodes = Vec::new();
    let mut edges = Vec::new();
    let mut visited: HashSet<String> = HashSet::new();
    let mut queue: VecDeque<(StorePath, Derivation)> = VecDeque::new();

    visited.insert(root_path.to_string());
    queue.push_back((root_path.clone(), root_drv.clone()));

    let mut count = 0usize;

    while let Some((drv_path, drv)) = queue.pop_front() {
        count += 1;
        if count > MAX_TRANSITIVE_INPUTS {
            return Err(anyhow::anyhow!(
                "transitive input limit exceeded ({MAX_TRANSITIVE_INPUTS})"
            ));
        }

        let node = derivation_to_node(&drv_path, &drv);
        nodes.push(node);

        for child_path_str in drv.input_drvs().keys() {
            // Create edge: parent depends on child
            edges.push(types::DerivationEdge {
                parent_drv_path: drv_path.to_string(),
                child_drv_path: child_path_str.clone(),
            });

            if visited.insert(child_path_str.clone()) {
                // Resolve this child derivation.
                // An unparseable store path here means the parent .drv is
                // corrupt — fail hard rather than silently dropping the edge
                // (which would leave the DAG incomplete and cause a confusing
                // "edge references unknown node" error downstream).
                let child_sp = StorePath::parse(child_path_str).map_err(|e| {
                    anyhow::anyhow!(
                        "corrupted derivation '{drv_path}': invalid inputDrv path '{child_path_str}': {e}"
                    )
                })?;

                // If the child can't be resolved (store unreachable, .drv
                // missing from store), the build cannot proceed: a stub leaf
                // with system="" would never match any worker and hang forever.
                // Fail now with a clear error.
                let child_drv = resolve_derivation(&child_sp, store_client, drv_cache)
                    .await
                    .map_err(|e| {
                        anyhow::anyhow!(
                            "cannot resolve dependency '{child_path_str}' of '{drv_path}': {e} \
                             (store unreachable or .drv missing; build cannot proceed)"
                        )
                    })?;
                queue.push_back((child_sp, child_drv));
            }
        }
    }

    debug!(
        nodes = nodes.len(),
        edges = edges.len(),
        "DAG reconstruction complete"
    );

    // Populate input_srcs_nar_size for each node. Batched AFTER
    // BFS so we QueryPathInfo each unique src exactly once across
    // the whole DAG (many nodes share the same stdenv/bash srcs).
    // Best-effort: store error → log, leave 0 (estimator skips).
    populate_input_srcs_sizes(&mut nodes, drv_cache, store_client).await;

    // Populate ca_modular_hash for CA nodes. AFTER BFS so
    // hash_derivation_modulo has the full drv_cache to recurse
    // over (InputNotFound otherwise). Memoised via a single
    // shared hash_cache across all nodes — the recursive walk
    // hits every sub-hash once regardless of how many CA nodes
    // reference it. Best-effort: hash failure → log, leave empty
    // (scheduler's collect_ca_inputs skips; resolve degrades to
    // worker-fail-on-placeholder + retry).
    populate_ca_modular_hashes(&mut nodes, drv_cache);

    // Populate needs_resolve for the ia.deferred case: an IA (or
    // fixed-CA) derivation whose inputDrvs include a floating-CA
    // child has that child's placeholder path embedded in its
    // env/args — it needs resolve even though it's not floating-CA
    // itself. AFTER BFS so every child is in drv_cache.
    populate_needs_resolve(&mut nodes, drv_cache);

    Ok((nodes, edges))
}

/// Yield `(node_idx, &node, &Derivation)` for every node whose
/// `drv_path` is in `drv_cache`. Logs BFS-inconsistency at debug
/// for misses (the cache was populated BY the BFS — a miss means
/// the BFS and this walk disagree about what nodes exist; our bug,
/// not the operator's). Shared scaffold for every post-BFS
/// `populate_*` pass.
///
/// Index-based because callers need to write `nodes[idx].field`
/// AFTER the lookup. Yielding `&mut Node` would alias the iterator's
/// own immutable borrow of `nodes` (it reads `node.drv_path`). The
/// collect-then-apply pattern at each call-site sidesteps the split.
fn iter_cached_drvs<'a>(
    nodes: &'a [types::DerivationNode],
    drv_cache: &'a HashMap<StorePath, Derivation>,
    walker_name: &'static str,
) -> impl Iterator<Item = (usize, &'a types::DerivationNode, &'a Derivation)> + 'a {
    nodes.iter().enumerate().filter_map(move |(idx, node)| {
        let sp = StorePath::parse(&node.drv_path).ok()?;
        match drv_cache.get(&sp) {
            Some(drv) => Some((idx, node, drv)),
            None => {
                debug!(
                    drv_path = %node.drv_path,
                    walker = walker_name,
                    "drv not in cache (BFS inconsistency)"
                );
                None
            }
        }
    })
}

/// Compute [`hash_derivation_modulo`] via a `drv_cache`-backed
/// resolver. Pass the same `hash_cache` across calls to reuse
/// sub-hashes. Errors warn-and-return-`None` — both callers
/// (translate's populate + handler's builtOutputs) treat "no hash"
/// as log + degrade-gracefully.
///
/// [`hash_derivation_modulo`]: rio_nix::derivation::hash_derivation_modulo
pub(crate) fn compute_modular_hash_cached(
    drv: &Derivation,
    drv_path: &str,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> Option<[u8; 32]> {
    let resolve = |p: &str| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp));
    match rio_nix::derivation::hash_derivation_modulo(drv, drv_path, &resolve, hash_cache) {
        Ok(hash) => Some(hash),
        Err(e) => {
            warn!(
                drv_path = %drv_path,
                error = %e,
                "hash_derivation_modulo failed; caller will degrade"
            );
            None
        }
    }
}

/// Fill `input_srcs_nar_size` on each node via batched QueryPathInfo.
///
/// The estimator's closure-size-as-proxy fallback: a derivation with
/// 10GB of source tarballs probably takes longer than one with 100MB.
/// Used when there's no `build_history` entry yet (cold start on a
/// fresh `(pname, system)`).
///
/// Best-effort and ORTHOGONAL to the build actually working:
/// - Store error → `warn!` once, leave all nodes at 0, return.
/// - Path not in store → that src contributes 0.
///
/// Batching: union `input_srcs` across all nodes, dedup, query once
/// each. ~50 nodes × ~30 shared srcs → 30 RPCs not 1500.
async fn populate_input_srcs_sizes(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
    store_client: &mut StoreServiceClient<Channel>,
) {
    use rio_proto::client::query_path_info_opt;

    // HashSet not Vec: the dedup IS the point.
    let mut all_srcs: HashSet<String> = HashSet::new();
    for (_, _, drv) in iter_cached_drvs(nodes, drv_cache, "populate_input_srcs_sizes") {
        all_srcs.extend(drv.input_srcs().iter().cloned());
    }

    if all_srcs.is_empty() {
        // Pure inputDrv DAG. Nothing to do.
        return;
    }

    // Single store error → bail whole batch. Partial fill is
    // confusing for dashboards; prefer honest "no signal".
    let mut sizes: HashMap<String, u64> = HashMap::with_capacity(all_srcs.len());
    for src in &all_srcs {
        match query_path_info_opt(store_client, src, rio_common::grpc::DEFAULT_GRPC_TIMEOUT).await {
            Ok(Some(info)) => {
                sizes.insert(src.clone(), info.nar_size);
            }
            Ok(None) => {
                sizes.insert(src.clone(), 0);
            }
            Err(e) => {
                warn!(
                    error = %e,
                    queried = sizes.len(),
                    total = all_srcs.len(),
                    "populate_input_srcs: store error mid-batch; leaving all nodes at 0"
                );
                return;
            }
        }
    }

    // saturating_add: if someone has >u64::MAX bytes of srcs they
    // have bigger problems, but don't panic on it.
    let sums: Vec<(usize, u64)> = iter_cached_drvs(nodes, drv_cache, "populate_input_srcs_sizes")
        .map(|(idx, _, drv)| {
            let sum = drv
                .input_srcs()
                .iter()
                .map(|s| sizes.get(s).copied().unwrap_or(0))
                .fold(0u64, |acc, x| acc.saturating_add(x));
            (idx, sum)
        })
        .collect();
    for (idx, sum) in sums {
        nodes[idx].input_srcs_nar_size = sum;
    }
}

/// Fill `ca_modular_hash` on each CA node via `hash_derivation_modulo`.
///
/// The scheduler's CA-on-CA resolve queries `realisations` keyed on
/// `(modular_hash, output_name)`. The modular hash needs the full
/// transitive closure of parsed derivations (what BFS put in
/// `drv_cache`). Memoised via one shared `hash_cache` — for N CA
/// nodes sharing a common CA input, the common sub-hash is computed
/// once.
///
/// Best-effort: hash failure → warn, leave empty. Scheduler's
/// `collect_ca_inputs` skips empty; resolve degrades to worker-fail
/// + retry-with-backoff.
fn populate_ca_modular_hashes(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) {
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
    // IA nodes: ca_modular_hash stays empty — dead bytes on the
    // wire (scheduler's resolve gates on is_ca).
    let hashes: Vec<(usize, Vec<u8>)> =
        iter_cached_drvs(nodes, drv_cache, "populate_ca_modular_hashes")
            .filter(|(_, node, _)| node.is_content_addressed)
            .filter_map(|(idx, node, drv)| {
                compute_modular_hash_cached(drv, &node.drv_path, drv_cache, &mut hash_cache)
                    .map(|h| (idx, h.to_vec()))
            })
            .collect();
    for (idx, h) in hashes {
        nodes[idx].ca_modular_hash = h;
    }
}

/// Set `needs_resolve` for nodes with floating-CA inputs (`ia.deferred`).
///
/// ADR-018 Appendix B: Nix's `shouldResolve` returns true for IA
/// derivations when they're "deferred" — i.e., they have a floating-CA
/// input whose output path is a placeholder at eval time. The parent's
/// env/args reference that placeholder, so dispatch-time resolve must
/// rewrite it to the realized path.
///
/// `build_node` already set `needs_resolve = has_ca_floating_outputs()`
/// (self-floating always resolves). This pass ORs in the any-child-is-
/// floating-CA case. AFTER BFS so every child drv is in `drv_cache`.
///
/// Missing children (BFS inconsistency) → skip; the node keeps its
/// self-computed value. Same degrade as `populate_ca_modular_hashes`.
fn populate_needs_resolve(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) {
    let deferred: Vec<usize> = iter_cached_drvs(nodes, drv_cache, "populate_needs_resolve")
        .filter(|(_, node, _)| !node.needs_resolve)
        .filter(|(_, _, drv)| {
            drv.input_drvs().keys().any(|child_path| {
                StorePath::parse(child_path)
                    .ok()
                    .and_then(|sp| drv_cache.get(&sp))
                    .is_some_and(|child| child.has_ca_floating_outputs())
            })
        })
        .map(|(idx, _, _)| idx)
        .collect();
    for idx in deferred {
        nodes[idx].needs_resolve = true;
    }
}

/// Validate a DAG before SubmitBuild. Returns `Err(reason)` if the
/// DAG should be rejected — caller sends STDERR_ERROR with the
/// reason. Returns `Ok(())` if valid.
///
/// Checks:
/// - `__noChroot=1` in any node's env → reject (sandbox escape)
/// - `nodes.len() > MAX_DAG_NODES` → reject (early, before gRPC)
///
/// The scheduler ALSO enforces MAX_DAG_NODES (grpc/mod.rs:298);
/// this is an early reject to save the gRPC round-trip for obvious
/// over-size submissions. The __noChroot check is ONLY here — the
/// scheduler doesn't have the env (DerivationNode doesn't carry it).
pub fn validate_dag(
    nodes: &[types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
) -> Result<(), String> {
    // MAX_DAG_NODES: early reject. Scheduler enforces too but
    // this saves a 100MB+ gRPC message for obvious over-size.
    if nodes.len() > rio_common::limits::MAX_DAG_NODES {
        return Err(format!(
            "DAG too large: {} nodes > {} max",
            nodes.len(),
            rio_common::limits::MAX_DAG_NODES
        ));
    }

    // r[impl gw.reject.nochroot]
    // __noChroot check: iterate nodes, look up each drv in the
    // cache (it was populated during BFS), check env. Nodes
    // without a cached drv (BasicDerivation fallback) are
    // skipped — we don't have the env. A __noChroot drv
    // arriving via BasicDerivation is a corner case (client
    // sent a pre-parsed BasicDerivation without inputDrvs);
    // the build would fail at the worker's sandbox anyway
    // (sandbox=true, sandbox-fallback=false), so the check
    // here is best-effort early rejection.
    //
    // Why reject: __noChroot=1 tells nix-daemon to skip the
    // sandbox. That's a sandbox escape — the build sees /etc,
    // $HOME, the host network, everything. Allowed in single-
    // user Nix for bootstrap derivations; NEVER allowed in a
    // multi-tenant build farm. A malicious .drv could use this
    // to exfiltrate secrets from the worker.
    for node in nodes {
        // drv_path is the StorePath key in drv_cache (we built
        // nodes from the cache during BFS).
        let Ok(sp) = StorePath::parse(&node.drv_path) else {
            continue; // malformed path — let scheduler reject
        };
        let Some(drv) = drv_cache.get(&sp) else {
            continue; // BasicDerivation fallback, no env
        };
        if drv
            .env()
            .get("__noChroot")
            .map(|v| v == "1")
            .unwrap_or(false)
        {
            return Err(format!(
                "derivation {} requests __noChroot (sandbox escape) — not permitted",
                node.drv_path
            ));
        }
    }

    Ok(())
}

/// Build the proto `DerivationNode` for any [`DerivationLike`].
///
/// Both [`Derivation`] (full BFS path) and [`BasicDerivation`]
/// (single-node fallback) route through here — the
/// [`DerivationLike`] trait (P0384) unifies the accessor surface so
/// the struct-literal is written once. Before the trait existed the
/// two paths were hand-rolled separately and drifted on every
/// `DerivationNode` field-add (the `is_fixed_output` divergence P0384
/// fixed; the dual `is_content_addressed` annotations P0250 added).
///
/// `drv_content`/`input_srcs_nar_size` are left zeroed —
/// [`filter_and_inline_drv`] and [`populate_input_srcs_sizes`] fill
/// them AFTER FindMissingPaths/BFS batching (see call-site comments
/// on the wrappers).
///
/// `is_fixed_output` is the strict [`DerivationLike::is_fixed_output`]
/// predicate (single `out` with both `hash_algo` AND `hash` set) —
/// matches the worker's strict recompute at executor/mod.rs:344.
// r[impl sched.ca.detect]
// Both CA kinds: floating (hash_algo set, hash empty) and
// fixed-output (hash also set). Cutoff applies to either — the
// output's nar_hash is what gets compared, not the input addressing.
fn build_node<D: DerivationLike>(drv_path: &str, drv: &D) -> types::DerivationNode {
    let (output_names, expected_output_paths): (Vec<_>, Vec<_>) = drv
        .outputs()
        .iter()
        .map(|o| (o.name().to_string(), o.path().to_string()))
        .unzip();
    let env = drv.env();
    types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Input-addressed derivations use the store path as the drv_hash.
        // This ensures every node has a unique, non-empty key in the DAG.
        drv_hash: drv_path.to_string(),
        // pname → name fallback: stdenv's mkDerivation sets both;
        // raw derivation{} calls typically only set name. Without
        // the fallback, raw derivations get pname="" → never match
        // build_history (keyed on pname,system) → 30s default →
        // wrong size-class routing. name includes version suffix so
        // it's a LESS stable key (hello-2.12 vs hello-2.13 are
        // different rows), but some history beats none.
        pname: env
            .get("pname")
            .or_else(|| env.get("name"))
            .cloned()
            .unwrap_or_default(),
        system: drv.platform().to_string(),
        required_features: env
            .get("requiredSystemFeatures")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
        output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths,
        drv_content: Vec::new(),
        input_srcs_nar_size: 0,
        is_content_addressed: drv.is_fixed_output() || drv.has_ca_floating_outputs(),
        // Empty here — populate_ca_modular_hashes() fills AFTER the
        // full BFS so hash_derivation_modulo has the complete
        // drv_cache to resolve transitive inputDrvs over. Doing it
        // inline would be a partial-closure recurse (InputNotFound
        // for inputs the BFS hasn't visited yet).
        ca_modular_hash: Vec::new(),
        // ADR-018 Appendix B: floating-CA self always resolves.
        // populate_needs_resolve() ORs in the ia.deferred case
        // (IA-with-floating-CA-input) AFTER BFS — needs the
        // drv_cache to look up children's addressing mode.
        needs_resolve: drv.has_ca_floating_outputs(),
    }
}

/// Create a single-node DAG from a [`BasicDerivation`] (no inputDrvs).
/// Used as fallback when the full [`Derivation`] is not available.
///
/// Wraps the generic `build_node`. `drv_content`/`input_srcs_nar_size`
/// stay zeroed — this is the "full drv not available" fallback; the
/// worker fetches from store.
pub fn single_node_from_basic(
    drv_path: &str,
    basic_drv: &BasicDerivation,
) -> Vec<types::DerivationNode> {
    vec![build_node(drv_path, basic_drv)]
}

/// Convert a full [`Derivation`] into a proto `DerivationNode`.
///
/// Wraps [`build_node`]. `drv_content`/`input_srcs_nar_size` are
/// populated AFTER by [`filter_and_inline_drv`] (batched
/// FindMissingPaths) and [`populate_input_srcs_sizes`] (batched
/// QueryPathInfo across the whole DAG).
fn derivation_to_node(drv_path: &StorePath, drv: &Derivation) -> types::DerivationNode {
    build_node(drv_path.as_str(), drv)
}

/// Inline .drv content into nodes whose outputs are missing from the
/// store — i.e., nodes that will actually dispatch. Saves one worker
/// → store round-trip per dispatched derivation (the `GetPath` fetch
/// in `fetch_drv_from_store`).
///
/// Gated by FindMissingPaths: cache-hit nodes stay empty (the scheduler
/// short-circuits them to Completed, they never dispatch). This is the
/// difference between "inline everything" (100 MB for a cold 10k-node
/// DAG) and "inline what's needed" (usually a handful of nodes).
///
/// Budget-capped at 16 MB total. First-come-first-serve — if we blow
/// the budget, remaining nodes fall back to worker-fetch. Not optimal
/// ordering (critical-path would be nice) but simple and correct.
///
/// On any error (FindMissingPaths timeout, store down, etc.): log and
/// skip inlining entirely. The worker-fetch path is the SAFE DEFAULT
/// — this is an optimization, not a correctness requirement.
pub async fn filter_and_inline_drv(
    nodes: &mut [types::DerivationNode],
    drv_cache: &HashMap<StorePath, Derivation>,
    store_client: &mut StoreServiceClient<Channel>,
) {
    // Collect all NON-EMPTY expected output paths across the DAG.
    // One batched FindMissingPaths call instead of N.
    //
    // Floating-CA outputs have path="" (computed post-build from NAR
    // hash — `DerivationOutput::path()` returns empty until built).
    // The store's `validate_store_path` rejects the WHOLE BATCH on
    // any empty path, so one CA node poisons inlining for the entire
    // DAG. Filter them here; CA nodes are handled in the
    // `will_dispatch` check below (empty path → always inline).
    let all_outputs: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.expected_output_paths.iter())
        .filter(|p| !p.is_empty())
        .cloned()
        .collect();

    // FindMissingPaths only if we have IA outputs to check. Pure-CA
    // DAGs (all floating) skip straight to the inline loop — every
    // floating-CA node dispatches (output path unknown → can't
    // cache-hit by path, so there's nothing for the store to gate).
    //
    // Timeout matches the other gateway store calls. On any error:
    // skip inlining (safe degrade — worker fetches from store).
    let missing: HashSet<String> = if all_outputs.is_empty() {
        HashSet::new()
    } else {
        match tokio::time::timeout(
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            store_client.find_missing_paths(types::FindMissingPathsRequest {
                store_paths: all_outputs,
            }),
        )
        .await
        {
            Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
            Ok(Err(e)) => {
                warn!(error = %e, "FindMissingPaths failed; skipping .drv inlining (worker will fetch)");
                return;
            }
            Err(_) => {
                warn!("FindMissingPaths timed out; skipping .drv inlining (worker will fetch)");
                return;
            }
        }
    };

    // Walk nodes; inline those with ANY missing output.
    let mut total_inlined: usize = 0;
    let mut inlined_count: usize = 0;
    let mut skipped_budget: usize = 0;

    for node in nodes.iter_mut() {
        // At least one output missing → this node will dispatch.
        // Empty path = floating-CA (unknown until built) → ALWAYS
        // dispatches: can't cache-hit by path, and the scheduler's
        // `maybe_resolve_ca` REQUIRES drv_content to rewrite
        // placeholder paths. The scheduler's store-fetch fallback
        // (`fetch_drv_content_from_store`) depends on its
        // startup-time store connection succeeding — a race we must
        // not rely on (layer-9 ca-cutoff failure: scheduler boots
        // before store ready → store_client=None → fallback dead).
        // All outputs present → cache hit → never dispatches → skip.
        let will_dispatch = node
            .expected_output_paths
            .iter()
            .any(|p| p.is_empty() || missing.contains(p));
        if !will_dispatch {
            continue;
        }

        // Look up the Derivation. drv_path is the key we used in
        // reconstruct_dag. If it's not in cache (shouldn't happen —
        // reconstruct_dag populates it) or won't parse, skip.
        let Ok(sp) = StorePath::parse(&node.drv_path) else {
            continue;
        };
        let Some(drv) = drv_cache.get(&sp) else {
            continue;
        };

        // Serialize. to_aterm() is deterministic (BTreeMap iteration)
        // so this is the same bytes the store has.
        let aterm = drv.to_aterm();
        let aterm_bytes = aterm.into_bytes();

        // Per-node size gate. Huge derivations (flake inputs dumped
        // into env) aren't worth it — worker fetches those.
        if aterm_bytes.len() > MAX_INLINE_DRV_BYTES {
            continue;
        }

        // Budget gate. Once we hit 16 MB, skip. Remaining nodes fall
        // back to worker-fetch. We still loop to count skipped_budget
        // for the metric, but no more inlining happens.
        if total_inlined + aterm_bytes.len() > INLINE_BUDGET_BYTES {
            skipped_budget += 1;
            continue;
        }

        total_inlined += aterm_bytes.len();
        inlined_count += 1;
        node.drv_content = aterm_bytes;
    }

    debug!(
        inlined = inlined_count,
        bytes = total_inlined,
        skipped_over_budget = skipped_budget,
        "inlined .drv content for will-dispatch nodes"
    );
}

/// Build a `SubmitBuildRequest` from nodes, edges, and client options.
///
/// `tenant_name` is `Option<&NormalizedName>` — the proto boundary
/// convention is empty-string-as-absent, so `None` (single-tenant
/// mode) serializes to `""`, `Some(n)` serializes to the normalized
/// inner. The type guarantees no leading/trailing/interior whitespace
/// ever reaches the scheduler.
///
/// **`options: Some(_)` is dead code for ssh-ng** (P0310 T0 source-verified):
/// `SSHStore::setOptions()` is an empty override (ssh-store.cc:81-88,
/// 088ef8175) so `wopSetOptions` never reaches `handle_set_options`, and
/// `ctx.options` stays `None` for the entire session. The `Some(opts) =>`
/// arm below is kept as future-proofing: if rio-gateway ever advertises the
/// `set-options-map-only` protocol feature AND the flake's nix input bumps
/// past 32827b9fb, ssh-ng clients start sending a filtered `wopSetOptions`
/// and this path goes live — WONTFIX(P0310) tracks that.
pub fn build_submit_request(
    nodes: Vec<types::DerivationNode>,
    edges: Vec<types::DerivationEdge>,
    options: Option<&ClientOptions>,
    priority_class: &str,
    tenant_name: Option<&NormalizedName>,
) -> types::SubmitBuildRequest {
    let (max_silent_time, build_timeout, build_cores, keep_going) = match options {
        Some(opts) => (
            opts.max_silent_time(),
            opts.build_timeout(),
            opts.build_cores,
            opts.keep_going,
        ),
        None => (0, 0, 0, false),
    };

    types::SubmitBuildRequest {
        // Proto convention: empty string = absent/single-tenant.
        tenant_name: tenant_name.map(|n| n.to_string()).unwrap_or_default(),
        priority_class: priority_class.to_string(),
        nodes,
        edges,
        max_silent_time,
        build_timeout,
        build_cores,
        keep_going,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::collections::{BTreeMap, BTreeSet};

    use rio_nix::derivation::DerivationOutput;

    fn make_basic_drv(env: BTreeMap<String, String>) -> anyhow::Result<BasicDerivation> {
        let output = DerivationOutput::new("out", "/nix/store/test-out", "", "")?;
        Ok(BasicDerivation::new(
            vec![output],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            env,
        )?)
    }

    /// Same as make_basic_drv but with a configurable single output.
    fn make_basic_drv_with_output(hash_algo: &str, hash: &str) -> anyhow::Result<BasicDerivation> {
        let output = DerivationOutput::new("out", "/nix/store/test-out", hash_algo, hash)?;
        Ok(BasicDerivation::new(
            vec![output],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            BTreeMap::new(),
        )?)
    }

    // r[verify sched.ca.detect]
    // Both Derivation and BasicDerivation route through build_node<D> —
    // the three tests below prove the trait dispatch is correct for
    // each DerivationLike impl (not that the builder logic differs; it
    // doesn't). Regression guard for the pre-P0388 divergence shape:
    // same proto field, two hand-rolled struct literals, drift on
    // every DerivationNode field-add.
    /// is_content_addressed on the BasicDerivation path (single_node_from_basic).
    /// Three cases: input-addressed (both empty), floating-CA (algo set,
    /// hash empty), fixed-output (both set).
    #[test]
    fn test_single_node_is_content_addressed() -> anyhow::Result<()> {
        // Input-addressed: hash_algo="", hash="" → false.
        let ia = make_basic_drv_with_output("", "")?;
        let nodes = single_node_from_basic("/nix/store/test.drv", &ia);
        assert!(
            !nodes[0].is_content_addressed,
            "input-addressed (hash_algo empty) → false"
        );

        // Floating-CA: hash_algo set, hash empty → true via has_ca_floating_outputs().
        let floating = make_basic_drv_with_output("sha256", "")?;
        let nodes = single_node_from_basic("/nix/store/test.drv", &floating);
        assert!(
            nodes[0].is_content_addressed,
            "floating-CA (hash_algo set, hash empty) → true"
        );

        // Fixed-output: both set → true via is_fixed_output().
        let fod = make_basic_drv_with_output("sha256", "deadbeef")?;
        let nodes = single_node_from_basic("/nix/store/test.drv", &fod);
        assert!(
            nodes[0].is_content_addressed,
            "fixed-output (hash_algo + hash both set) → true"
        );
        Ok(())
    }

    // r[verify sched.ca.detect]
    /// Floating-CA via single-node fallback: is_fixed_output MUST be
    /// false (strict predicate — hash_algo set but hash empty doesn't
    /// qualify), is_content_addressed MUST be true (either-kind disjunct).
    ///
    /// Pre-fix the loose per-output predicate made is_fixed_output=true
    /// here, diverging from the full-DAG path (derivation_to_node) which
    /// already uses the strict form. Worker's strict recompute at
    /// executor/mod.rs:344 saw false → warn! at :346 fired spuriously.
    #[test]
    fn single_node_floating_ca_strict_fod_false() -> anyhow::Result<()> {
        // Floating-CA: hash_algo set, hash empty.
        let floating = make_basic_drv_with_output("sha256", "")?;
        let nodes = single_node_from_basic("/nix/store/abc-floating.drv", &floating);
        assert_eq!(nodes.len(), 1);
        assert!(
            !nodes[0].is_fixed_output,
            "floating-CA via fallback: strict FOD predicate → false (hash is empty)"
        );
        assert!(
            nodes[0].is_content_addressed,
            "floating-CA via fallback: is_ca true via has_ca_floating_outputs()"
        );

        // True FOD: both set → strict predicate true, consistency with
        // derivation_to_node on the same drv shape.
        let fod = make_basic_drv_with_output("sha256", "deadbeef")?;
        let nodes = single_node_from_basic("/nix/store/abc-fod.drv", &fod);
        assert!(nodes[0].is_fixed_output, "true FOD → true on both paths");

        // Input-addressed: both empty → false (sanity).
        let ia = make_basic_drv_with_output("", "")?;
        let nodes = single_node_from_basic("/nix/store/abc-ia.drv", &ia);
        assert!(!nodes[0].is_fixed_output, "input-addressed → false");
        Ok(())
    }

    // r[verify sched.ca.detect]
    /// is_content_addressed on the full-Derivation path (derivation_to_node).
    /// ATerm parse covers both input-addressed and floating-CA.
    #[test]
    fn test_derivation_to_node_is_content_addressed() -> anyhow::Result<()> {
        let drv_path = sp(&test_drv_path("ca-test"));

        // Input-addressed: all-empty output tuple.
        let ia_aterm =
            r#"Derive([("out","/nix/store/aaa-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let ia = Derivation::parse(ia_aterm)?;
        let node = derivation_to_node(&drv_path, &ia);
        assert!(!node.is_content_addressed, "IA drv → false");

        // Floating-CA: hash_algo="r:sha256", hash="" (what __contentAddressed=true emits).
        let ca_aterm = r#"Derive([("out","/nix/store/bbb-out","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let ca = Derivation::parse(ca_aterm)?;
        let node = derivation_to_node(&drv_path, &ca);
        assert!(
            node.is_content_addressed,
            "floating-CA drv → true via has_ca_floating_outputs()"
        );

        // Fixed-output: both set, single "out".
        let fod_aterm = r#"Derive([("out","/nix/store/ccc-out","sha256","deadbeef")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let fod = Derivation::parse(fod_aterm)?;
        let node = derivation_to_node(&drv_path, &fod);
        assert!(
            node.is_content_addressed,
            "FOD drv → true via is_fixed_output()"
        );
        Ok(())
    }

    #[test]
    fn test_build_submit_request_carries_tenant_name() {
        let name = NormalizedName::new("team-foo").unwrap();
        let req = build_submit_request(vec![], vec![], None, "ci", Some(&name));
        assert_eq!(req.tenant_name, "team-foo");
        assert_eq!(req.priority_class, "ci");

        // None → empty string on the wire (proto's empty-as-absent
        // convention for single-tenant mode).
        let req_empty = build_submit_request(vec![], vec![], None, "ci", None);
        assert_eq!(
            req_empty.tenant_name, "",
            "None tenant_name → empty string (single-tenant mode)"
        );
    }

    #[test]
    fn test_single_node_extracts_features() -> anyhow::Result<()> {
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), "kvm big-parallel".into());
        let drv = make_basic_drv(env)?;

        let nodes = single_node_from_basic("/nix/store/test.drv", &drv);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].required_features,
            vec!["kvm".to_string(), "big-parallel".to_string()],
            "requiredSystemFeatures should be extracted from BasicDerivation env"
        );
        Ok(())
    }

    #[test]
    fn validate_dag_rejects_oversized() {
        // MAX_DAG_NODES is 100k; build 100k+1 nodes to trigger.
        // No drv_cache needed — the size check fires first.
        let oversized: Vec<types::DerivationNode> = (0..=rio_common::limits::MAX_DAG_NODES)
            .map(|i| types::DerivationNode {
                drv_path: format!("/nix/store/node{i}.drv"),
                drv_hash: format!("node{i}"),
                ..Default::default()
            })
            .collect();
        let empty_cache = HashMap::new();
        let result = validate_dag(&oversized, &empty_cache);
        assert!(
            result.is_err(),
            "{} nodes > {} max should reject",
            oversized.len(),
            rio_common::limits::MAX_DAG_NODES
        );
        assert!(result.unwrap_err().contains("DAG too large"));
    }

    #[test]
    fn validate_dag_accepts_normal_size_no_nochroot() {
        // A few nodes, empty cache (BasicDerivation fallback path),
        // no __noChroot → Ok.
        let nodes = vec![
            types::DerivationNode {
                drv_path: "/nix/store/aaa-test.drv".into(),
                drv_hash: "aaa".into(),
                ..Default::default()
            },
            types::DerivationNode {
                drv_path: "/nix/store/bbb-test.drv".into(),
                drv_hash: "bbb".into(),
                ..Default::default()
            },
        ];
        let empty_cache = HashMap::new();
        assert!(validate_dag(&nodes, &empty_cache).is_ok());
    }

    // __noChroot rejection is hard to unit-test here because it
    // needs a Derivation in drv_cache with __noChroot=1 in env,
    // and constructing a full Derivation (not BasicDerivation)
    // requires ATerm parsing or a complex builder. Coverage comes
    // from the golden tests at tests/wire_opcodes/build.rs (seed
    // NOCHROOT_DRV_ATERM into the mock store so resolve_derivation
    // populates drv_cache, then drive opcodes 36 + 46 and assert
    // the failure BuildResult carries the "sandbox escape" message).

    #[test]
    fn test_single_node_no_features() -> anyhow::Result<()> {
        let drv = make_basic_drv(BTreeMap::new())?;
        let nodes = single_node_from_basic("/nix/store/test.drv", &drv);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].required_features.is_empty());
        Ok(())
    }

    /// pname falls back to name for raw derivation{} calls that only
    /// set name. Without this, pname="" → no build_history match →
    /// 30s default estimate → wrong size-class.
    #[test]
    fn test_pname_fallback_to_name() -> anyhow::Result<()> {
        // pname wins when both set (stdenv mkDerivation case).
        let mut env = BTreeMap::new();
        env.insert("pname".into(), "hello".into());
        env.insert("name".into(), "hello-2.12".into());
        let drv = make_basic_drv(env)?;
        let nodes = single_node_from_basic("/nix/store/x.drv", &drv);
        assert_eq!(nodes[0].pname, "hello", "pname preferred over name");

        // name fallback when pname absent (raw derivation{} case).
        let mut env = BTreeMap::new();
        env.insert("name".into(), "rawbuild-1.0".into());
        let drv = make_basic_drv(env)?;
        let nodes = single_node_from_basic("/nix/store/x.drv", &drv);
        assert_eq!(
            nodes[0].pname, "rawbuild-1.0",
            "name fallback — less stable (includes version) but beats empty"
        );

        // neither → empty (no build_history key possible).
        let drv = make_basic_drv(BTreeMap::new())?;
        let nodes = single_node_from_basic("/nix/store/x.drv", &drv);
        assert_eq!(nodes[0].pname, "");

        Ok(())
    }

    // -------------------------------------------------------------------
    // reconstruct_dag unit tests
    // -------------------------------------------------------------------
    //
    // reconstruct_dag calls resolve_derivation which checks drv_cache FIRST
    // before hitting the store. By pre-populating drv_cache with all needed
    // derivations, we can test reconstruct_dag without a live store.

    use rio_proto::StoreServiceClient;
    use rio_test_support::fixtures::test_drv_path;

    /// Spin up a mock store that fails all RPCs (lazy connect to dead port).
    /// Used to verify reconstruct_dag fails hard on unresolvable inputDrvs.
    fn unreachable_store() -> StoreServiceClient<tonic::transport::Channel> {
        // Lazy channel to a dead port — any RPC will fail.
        let channel = tonic::transport::Channel::from_static("http://127.0.0.1:1").connect_lazy();
        StoreServiceClient::new(channel)
    }

    /// Parse a minimal ATerm derivation with the given inputDrvs.
    /// Format: Derive([outputs],[inputDrvs],[inputSrcs],system,builder,args,env)
    fn make_test_derivation(out_path: &str, input_drvs: &[(&str, &[&str])]) -> Derivation {
        make_test_derivation_with_srcs(out_path, input_drvs, &[])
    }

    /// Same as make_test_derivation but with explicit inputSrcs
    /// (for populate_input_srcs_sizes tests).
    fn make_test_derivation_with_srcs(
        out_path: &str,
        input_drvs: &[(&str, &[&str])],
        input_srcs: &[&str],
    ) -> Derivation {
        let outputs = format!(r#"[("out","{out_path}","","")]"#);
        let inputs: Vec<String> = input_drvs
            .iter()
            .map(|(path, outs)| {
                let outs_str: Vec<String> = outs.iter().map(|o| format!(r#""{o}""#)).collect();
                format!(r#"("{path}",[{}])"#, outs_str.join(","))
            })
            .collect();
        let input_drvs_str = format!("[{}]", inputs.join(","));
        let srcs_str: Vec<String> = input_srcs.iter().map(|s| format!(r#""{s}""#)).collect();
        let input_srcs_str = format!("[{}]", srcs_str.join(","));
        let aterm = format!(
            r#"Derive({outputs},{input_drvs_str},{input_srcs_str},"x86_64-linux","/bin/sh",[],[("out","{out_path}")])"#
        );
        Derivation::parse(&aterm).expect("test ATerm should parse")
    }

    fn sp(s: &str) -> StorePath {
        StorePath::parse(s).expect("valid test store path")
    }

    #[tokio::test]
    async fn test_reconstruct_dag_single_node_no_inputs() {
        let root_path = sp(&test_drv_path("root"));
        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        let (nodes, edges) = reconstruct_dag(&root_path, &root_drv, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 1, "single derivation -> 1 node");
        assert_eq!(nodes[0].drv_path, root_path.to_string());
        assert_eq!(nodes[0].system, "x86_64-linux");
        assert!(edges.is_empty(), "no inputDrvs -> 0 edges");
    }

    #[tokio::test]
    async fn test_reconstruct_dag_one_input_drv() {
        let root_path = sp(&test_drv_path("root"));
        let child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-child.drv");

        let root_drv = make_test_derivation(
            "/nix/store/aaa-root-out",
            &[(child_path.as_str(), &["out"])],
        );
        let child_drv = make_test_derivation("/nix/store/bbb-child-out", &[]);

        let mut store = unreachable_store();
        // Pre-populate cache so resolve_derivation finds the child without gRPC.
        let mut cache = HashMap::new();
        cache.insert(child_path.clone(), child_drv);

        let (nodes, edges) = reconstruct_dag(&root_path, &root_drv, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 2, "root + 1 inputDrv -> 2 nodes");
        assert_eq!(edges.len(), 1, "1 inputDrv relationship -> 1 edge");
        assert_eq!(edges[0].parent_drv_path, root_path.to_string());
        assert_eq!(edges[0].child_drv_path, child_path.to_string());

        // Both nodes should have correct drv_path set.
        let paths: std::collections::HashSet<String> =
            nodes.iter().map(|n| n.drv_path.clone()).collect();
        assert!(paths.contains(&root_path.to_string()));
        assert!(paths.contains(&child_path.to_string()));
    }

    #[tokio::test]
    async fn test_reconstruct_dag_unresolvable_inputdrv_fails() {
        // inputDrv not in cache AND store unreachable -> hard failure.
        // Regression: unresolvable inputDrv must fail, not produce a
        // stub leaf that silently hangs.
        let root_path = sp(&test_drv_path("root"));
        let missing_child = "/nix/store/cccccccccccccccccccccccccccccccc-missing.drv";

        let root_drv =
            make_test_derivation("/nix/store/aaa-root-out", &[(missing_child, &["out"])]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new(); // child NOT in cache

        let result = reconstruct_dag(&root_path, &root_drv, &mut store, &mut cache).await;

        let err = result.expect_err("unresolvable inputDrv must fail reconstruct_dag");
        let msg = err.to_string();
        assert!(
            msg.contains("cannot resolve dependency"),
            "error should mention unresolvable dependency, got: {msg}"
        );
        assert!(
            msg.contains(missing_child),
            "error should include the missing child path, got: {msg}"
        );
        assert!(
            msg.contains(&root_path.to_string()),
            "error should include the parent drv path, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_reconstruct_dag_invalid_inputdrv_path_fails() {
        // inputDrv is not a valid store path -> hard failure (corrupt .drv).
        let root_path = sp(&test_drv_path("root"));
        let bogus_child = "/not/a/store/path";

        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[(bogus_child, &["out"])]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        let result = reconstruct_dag(&root_path, &root_drv, &mut store, &mut cache).await;

        let err = result.expect_err("invalid inputDrv path must fail reconstruct_dag");
        let msg = err.to_string();
        assert!(
            msg.contains("corrupted derivation"),
            "error should mention corruption, got: {msg}"
        );
        assert!(
            msg.contains("invalid inputDrv path"),
            "error should mention invalid path, got: {msg}"
        );
    }

    #[tokio::test]
    async fn test_reconstruct_dag_transitive_chain() {
        // A -> B -> C chain. All in cache.
        let a_path = sp(&test_drv_path("a"));
        let b_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let c_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-c.drv");

        let a_drv = make_test_derivation("/nix/store/aaa-out", &[(b_path.as_str(), &["out"])]);
        let b_drv = make_test_derivation("/nix/store/bbb-out", &[(c_path.as_str(), &["out"])]);
        let c_drv = make_test_derivation("/nix/store/ccc-out", &[]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();
        cache.insert(b_path.clone(), b_drv);
        cache.insert(c_path.clone(), c_drv);

        let (nodes, edges) = reconstruct_dag(&a_path, &a_drv, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 3, "A->B->C chain -> 3 nodes");
        assert_eq!(edges.len(), 2, "A->B and B->C -> 2 edges");
    }

    // -------------------------------------------------------------------
    // filter_and_inline_drv
    // -------------------------------------------------------------------

    /// Core behavior: only nodes with MISSING outputs get inlined.
    /// Cache-hit nodes stay empty → SubmitBuild doesn't bloat for
    /// derivations that never dispatch.
    #[tokio::test]
    async fn test_filter_and_inline_drv_gates_on_missing() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, mut store_client, _handle) = spawn_mock_store_with_client().await?;

        // Two derivations: "cached" (output in store), "missing" (not).
        let cached_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-cached.drv");
        let cached_out = "/nix/store/cccccccccccccccccccccccccccccccc-cached-out";
        let missing_path = sp("/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing.drv");
        let missing_out = "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing-out";

        let cached_drv = make_test_derivation(cached_out, &[]);
        let missing_drv = make_test_derivation(missing_out, &[]);

        // Seed the "cached" output into MockStore so FindMissingPaths
        // reports it as present. Content doesn't matter — just the key.
        store.seed(
            rio_proto::validated::ValidatedPathInfo {
                store_path: rio_nix::store_path::StorePath::parse(cached_out)?,
                nar_hash: [0u8; 32],
                nar_size: 1,
                store_path_hash: vec![],
                deriver: None,
                references: vec![],
                signatures: vec![],
                content_address: None,
                registration_time: 0,
                ultimate: false,
            },
            vec![0u8; 1],
        );

        let mut cache = HashMap::new();
        cache.insert(cached_path.clone(), cached_drv.clone());
        cache.insert(missing_path.clone(), missing_drv.clone());

        let mut nodes = vec![
            derivation_to_node(&cached_path, &cached_drv),
            derivation_to_node(&missing_path, &missing_drv),
        ];

        // Pre: both empty.
        assert!(nodes[0].drv_content.is_empty());
        assert!(nodes[1].drv_content.is_empty());

        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;

        // Post: cached stays empty (won't dispatch), missing is inlined.
        assert!(
            nodes[0].drv_content.is_empty(),
            "cache-hit node should NOT be inlined (won't dispatch)"
        );
        assert!(
            !nodes[1].drv_content.is_empty(),
            "missing-output node SHOULD be inlined (will dispatch)"
        );

        // The inlined content is the ATerm — roundtrip-parse to prove
        // it's real, not garbage.
        let inlined = std::str::from_utf8(&nodes[1].drv_content)?;
        let reparsed = Derivation::parse(inlined)?;
        assert_eq!(reparsed.platform(), "x86_64-linux");
        assert_eq!(
            inlined,
            missing_drv.to_aterm(),
            "inlined bytes = exactly what to_aterm() produces"
        );

        Ok(())
    }

    /// Store unreachable → skip inlining entirely. Safe degrade:
    /// worker will fetch. This is an OPTIMIZATION, not correctness.
    #[tokio::test]
    async fn test_filter_and_inline_drv_store_error_skips_safely() {
        let drv_path = sp(&test_drv_path("x"));
        let drv = make_test_derivation("/nix/store/aaa-out", &[]);

        let mut cache = HashMap::new();
        cache.insert(drv_path.clone(), drv.clone());

        let mut nodes = vec![derivation_to_node(&drv_path, &drv)];

        // Dead store — FindMissingPaths will fail.
        let mut dead_store = unreachable_store();

        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        // On error: nothing inlined, no panic, function just returns.
        // Worker-fetch path handles this.
        assert!(
            nodes[0].drv_content.is_empty(),
            "store error → skip inlining (safe degrade)"
        );
    }

    /// Empty expected_output_paths → nothing to gate on → skip.
    /// (single_node_from_basic fallback has no expected outputs.)
    #[tokio::test]
    async fn test_filter_and_inline_drv_no_expected_outputs_skips() {
        let mut dead_store = unreachable_store();
        let cache = HashMap::new();

        // Node with no expected_output_paths (like single_node_from_basic).
        let mut nodes = vec![types::DerivationNode {
            drv_path: test_drv_path("x"),
            drv_hash: "x".into(),
            expected_output_paths: vec![], // KEY: empty
            ..Default::default()
        }];

        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        // Empty Vec → all_outputs empty → skips FindMissingPaths
        // (doesn't hit the dead store) → will_dispatch=false (no
        // elements to .any() over) → not inlined.
        assert!(nodes[0].drv_content.is_empty());
    }

    /// Floating-CA nodes (expected_output_paths = [""]) must ALWAYS
    /// inline. Their output paths are unknown until built (computed
    /// post-build from NAR hash), so they can't cache-hit by path
    /// and the scheduler's maybe_resolve_ca REQUIRES drv_content to
    /// rewrite placeholders.
    ///
    /// Regression (layer-9 ca-cutoff): previously, the empty string
    /// was sent to FindMissingPaths, which rejected the whole batch
    /// ("invalid store path"), causing the gateway to skip inlining
    /// entirely. The scheduler's store-fetch fallback then depended
    /// on a startup race (scheduler connects to store before store
    /// ready → store_client=None → fallback dead → dispatch
    /// unresolved → worker fails on placeholder).
    #[tokio::test]
    async fn test_filter_and_inline_drv_floating_ca_always_inlined() {
        let ca_path = sp("/nix/store/cacacacacacacacacacacacacacacaca-ca.drv");
        // Floating-CA ATerm: output path empty, hashAlgo set, hash
        // empty. Mirrors what nix produces for __contentAddressed.
        let ca_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#;
        let ca_drv = Derivation::parse(ca_aterm).expect("CA ATerm should parse");

        let mut cache = HashMap::new();
        cache.insert(ca_path.clone(), ca_drv.clone());

        // derivation_to_node produces expected_output_paths=[""] for
        // the floating-CA output (DerivationOutput::path() = "").
        let mut nodes = vec![derivation_to_node(&ca_path, &ca_drv)];
        assert_eq!(
            nodes[0].expected_output_paths,
            vec![String::new()],
            "floating-CA output path should be empty string"
        );

        // Dead store — must NOT matter. Empty paths are filtered
        // before FindMissingPaths, so a pure-CA DAG never hits it.
        let mut dead_store = unreachable_store();
        filter_and_inline_drv(&mut nodes, &cache, &mut dead_store).await;

        assert!(
            !nodes[0].drv_content.is_empty(),
            "floating-CA node must be inlined (empty path → always dispatches)"
        );
        // Inlined bytes = the ATerm we parsed.
        assert_eq!(
            std::str::from_utf8(&nodes[0].drv_content).unwrap(),
            ca_drv.to_aterm(),
        );
    }

    /// Mixed DAG (IA + CA): CA empty strings must not poison
    /// FindMissingPaths for IA nodes. Pre-fix, one CA node made the
    /// store reject the whole batch → no inlining for anyone.
    #[tokio::test]
    async fn test_filter_and_inline_drv_ca_does_not_poison_ia() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (_store, mut store_client, _h) = spawn_mock_store_with_client().await?;

        // IA node: output missing from (empty) mock store → will inline.
        let ia_path = sp("/nix/store/iaiaiaiaiaiaiaiaiaiaiaiaiaiaiaia-ia.drv");
        let ia_out = "/nix/store/iaiaiaiaiaiaiaiaiaiaiaiaiaiaiaia-ia-out";
        let ia_drv = make_test_derivation(ia_out, &[]);

        // CA node: empty output path.
        let ca_path = sp("/nix/store/cacacacacacacacacacacacacacacaca-ca.drv");
        let ca_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[("out","")])"#;
        let ca_drv = Derivation::parse(ca_aterm)?;

        let mut cache = HashMap::new();
        cache.insert(ia_path.clone(), ia_drv.clone());
        cache.insert(ca_path.clone(), ca_drv.clone());

        let mut nodes = vec![
            derivation_to_node(&ia_path, &ia_drv),
            derivation_to_node(&ca_path, &ca_drv),
        ];

        filter_and_inline_drv(&mut nodes, &cache, &mut store_client).await;

        // Both inlined: IA because missing, CA because empty-path.
        // Pre-fix: CA's "" poisoned the batch → store rejected →
        // gateway bailed → BOTH stayed empty.
        assert!(
            !nodes[0].drv_content.is_empty(),
            "IA node should inline (output missing); CA must not poison the batch"
        );
        assert!(
            !nodes[1].drv_content.is_empty(),
            "CA node should inline (empty path → always dispatches)"
        );
        Ok(())
    }

    // -------------------------------------------------------------------
    // populate_input_srcs_sizes
    // -------------------------------------------------------------------

    /// Two input_srcs seeded in store → node gets their sum.
    /// Exercises the batch-query-then-sum-per-node flow.
    #[tokio::test]
    async fn test_input_srcs_sizes_sums_from_store() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, mut client, _h) = spawn_mock_store_with_client().await?;

        // Seed two srcs with known sizes. nar_size is what matters.
        let src_a = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src-a";
        let src_b = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-src-b";
        for (path, size) in [(src_a, 1000u64), (src_b, 500u64)] {
            store.seed(
                rio_proto::validated::ValidatedPathInfo {
                    store_path: rio_nix::store_path::StorePath::parse(path)?,
                    nar_hash: [0u8; 32],
                    nar_size: size,
                    store_path_hash: vec![],
                    deriver: None,
                    references: vec![],
                    signatures: vec![],
                    content_address: None,
                    registration_time: 0,
                    ultimate: false,
                },
                vec![0u8; 1],
            );
        }

        // Derivation referencing both srcs.
        let drv_path = sp(&test_drv_path("with-srcs"));
        let drv = make_test_derivation_with_srcs(
            "/nix/store/oooooooooooooooooooooooooooooooo-out",
            &[],
            &[src_a, src_b],
        );
        let mut cache = HashMap::new();
        cache.insert(drv_path.clone(), drv.clone());

        // reconstruct_dag calls populate_input_srcs_sizes internally.
        let (nodes, _edges) = reconstruct_dag(&drv_path, &drv, &mut client, &mut cache).await?;

        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].input_srcs_nar_size, 1500,
            "sum of 1000 + 500 from the two seeded srcs"
        );
        Ok(())
    }

    /// Store error mid-batch → ALL nodes stay 0. Partial fill would
    /// be confusing (some nodes have data, some don't — dashboards
    /// comparing sizes see inconsistency).
    #[tokio::test]
    async fn test_input_srcs_sizes_store_error_leaves_zero() -> anyhow::Result<()> {
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, mut client, _h) = spawn_mock_store_with_client().await?;

        // Inject query_path_info failure.
        store
            .fail_query_path_info
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let src = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-src";
        let drv_path = sp(&test_drv_path("store-flaky"));
        let drv = make_test_derivation_with_srcs(
            "/nix/store/oooooooooooooooooooooooooooooooo-out",
            &[],
            &[src],
        );
        let mut cache = HashMap::new();
        cache.insert(drv_path.clone(), drv.clone());

        let (nodes, _edges) = reconstruct_dag(&drv_path, &drv, &mut client, &mut cache).await?;

        assert_eq!(
            nodes[0].input_srcs_nar_size, 0,
            "store error → 0 (no-signal), build still proceeds — this is estimation metadata"
        );
        Ok(())
    }

    /// No input_srcs (pure inputDrv chain) → 0, no store RPCs.
    /// Uses unreachable_store to prove no RPC fired.
    #[tokio::test]
    async fn test_input_srcs_sizes_empty_no_rpc() -> anyhow::Result<()> {
        let drv_path = sp(&test_drv_path("no-srcs"));
        let drv = make_test_derivation("/nix/store/oooooooooooooooooooooooooooooooo-out", &[]);

        let mut store = unreachable_store();
        let mut cache = HashMap::new();

        // If populate_input_srcs_sizes made an RPC, unreachable_store
        // would fail it. Success proves the early-return on empty.
        let (nodes, _edges) = reconstruct_dag(&drv_path, &drv, &mut store, &mut cache).await?;

        assert_eq!(
            nodes[0].input_srcs_nar_size, 0,
            "no srcs → 0, no RPC (unreachable store didn't error)"
        );
        Ok(())
    }

    // -------------------------------------------------------------------
    // iter_cached_drvs + compute_modular_hash_cached (P0413 walker dedup)
    // -------------------------------------------------------------------

    /// 3 nodes, 2 in drv_cache, 1 miss. Helper yields exactly the
    /// 2 cached indices; the miss is debug-logged and skipped.
    /// Mutation-anchor: returning `None` unconditionally → `hits` is
    /// empty → assert fires.
    #[test]
    fn cached_drv_walker_skips_cache_miss() {
        let a = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv");
        let b = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let c = sp("/nix/store/cccccccccccccccccccccccccccccccc-c.drv");
        let mk_node = |p: &StorePath| types::DerivationNode {
            drv_path: p.to_string(),
            ..Default::default()
        };
        let nodes = vec![mk_node(&a), mk_node(&b), mk_node(&c)];

        // Only a + c in the cache; b is the miss (BFS-inconsistency).
        let mut drv_cache = HashMap::new();
        drv_cache.insert(a, make_test_derivation("/nix/store/aaa-out", &[]));
        drv_cache.insert(c, make_test_derivation("/nix/store/ccc-out", &[]));

        let hits: Vec<usize> = iter_cached_drvs(&nodes, &drv_cache, "test")
            .map(|(i, _, _)| i)
            .collect();
        assert_eq!(hits, vec![0, 2], "indices 0+2 cached; 1 skipped");
    }

    /// inputDrv not in cache → `hash_derivation_modulo` returns
    /// `InputNotFound` → wrapper returns `None` (no panic, no garbage).
    #[test]
    fn modular_hash_wrapper_none_on_resolver_miss() {
        let drv = make_test_derivation(
            "/nix/store/oooooooooooooooooooooooooooooooo-out",
            &[(
                "/nix/store/mmmmmmmmmmmmmmmmmmmmmmmmmmmmmmmm-missing.drv",
                &["out"],
            )],
        );
        let drv_cache = HashMap::new(); // empty — guaranteed miss
        let mut hash_cache = HashMap::new();
        assert!(
            compute_modular_hash_cached(
                &drv,
                "/nix/store/pppppppppppppppppppppppppppppppp-parent.drv",
                &drv_cache,
                &mut hash_cache
            )
            .is_none(),
            "resolver miss → InputNotFound → None (warn-and-degrade)"
        );
    }

    // r[verify sched.ca.detect]
    /// `populate_needs_resolve`: IA parent depending on a floating-CA
    /// child gets `needs_resolve = true` (the ia.deferred case from
    /// ADR-018 Appendix B). IA-on-IA stays false. Floating-CA self
    /// stays true (set earlier by `build_node`, unchanged here).
    #[test]
    fn populate_needs_resolve_ia_deferred() {
        let ca_child_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-ca.drv");
        let ia_child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-ia.drv");
        let ia_parent_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-parent.drv");
        let ia_pure_path = sp("/nix/store/dddddddddddddddddddddddddddddddd-pure.drv");

        // Floating-CA child: hash_algo set, hash empty.
        let ca_child_aterm =
            r#"Derive([("out","","r:sha256","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let ca_child = Derivation::parse(ca_child_aterm).unwrap();

        // IA child: all-empty output tuple.
        let ia_child = make_test_derivation("/nix/store/ia-out", &[]);

        // IA parent depending on the floating-CA child.
        let ia_parent = make_test_derivation(
            "/nix/store/parent-out",
            &[(ca_child_path.as_str(), &["out"])],
        );

        // IA parent depending only on the IA child (pure IA-on-IA).
        let ia_pure =
            make_test_derivation("/nix/store/pure-out", &[(ia_child_path.as_str(), &["out"])]);

        let mut drv_cache = HashMap::new();
        drv_cache.insert(ca_child_path.clone(), ca_child.clone());
        drv_cache.insert(ia_child_path.clone(), ia_child.clone());
        drv_cache.insert(ia_parent_path.clone(), ia_parent.clone());
        drv_cache.insert(ia_pure_path.clone(), ia_pure.clone());

        let mut nodes = vec![
            derivation_to_node(&ca_child_path, &ca_child),
            derivation_to_node(&ia_child_path, &ia_child),
            derivation_to_node(&ia_parent_path, &ia_parent),
            derivation_to_node(&ia_pure_path, &ia_pure),
        ];

        // build_node already set needs_resolve from self.
        assert!(nodes[0].needs_resolve, "floating-CA self → true pre-pass");
        assert!(!nodes[2].needs_resolve, "IA parent → false pre-pass");

        populate_needs_resolve(&mut nodes, &drv_cache);

        assert!(nodes[0].needs_resolve, "floating-CA unchanged");
        assert!(!nodes[1].needs_resolve, "IA leaf → still false");
        assert!(
            nodes[2].needs_resolve,
            "ia.deferred: IA with floating-CA input → needs_resolve=true"
        );
        assert!(
            !nodes[3].needs_resolve,
            "IA with only-IA inputs → still false"
        );
    }
}
