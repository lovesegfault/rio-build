//! DAG reconstruction and gRPC request building.
//!
//! Translates the per-session derivation cache into `SubmitBuildRequest`
//! messages for the scheduler, walking `inputDrvs` recursively to build
//! the full derivation graph.

use std::collections::{HashMap, HashSet, VecDeque};

use rio_nix::derivation::{BasicDerivation, Derivation};
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
const MAX_TRANSITIVE_INPUTS: usize = 10_000;

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

    // Start with the root
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

        // Create a DerivationNode for this derivation
        let node = derivation_to_node(&drv_path, &drv);
        nodes.push(node);

        // Walk inputDrvs to create edges and discover children
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

    Ok((nodes, edges))
}

/// Fields of `DerivationNode` that are extracted identically from both
/// `BasicDerivation` and `Derivation`. Both types have `.outputs()`,
/// `.env()`, `.platform()`; the iterator chains are structurally the
/// same, just called on different receiver types.
struct NodeCommonFields {
    output_names: Vec<String>,
    expected_output_paths: Vec<String>,
    pname: String,
    system: String,
    required_features: Vec<String>,
}

/// Extract the fields that are computed identically for both derivation
/// kinds. The `outputs` iterator yields `(name, path)` pairs — callers
/// adapt their output type's accessors into that shape.
fn node_common_fields(
    outputs: impl Iterator<Item = (String, String)>,
    env: &std::collections::BTreeMap<String, String>,
    platform: &str,
) -> NodeCommonFields {
    let (output_names, expected_output_paths) = outputs.unzip();
    NodeCommonFields {
        output_names,
        expected_output_paths,
        pname: env.get("pname").cloned().unwrap_or_default(),
        system: platform.to_string(),
        required_features: env
            .get("requiredSystemFeatures")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
    }
}

/// Create a single-node DAG from a BasicDerivation (no inputDrvs).
/// Used as fallback when the full Derivation is not available.
pub fn single_node_from_basic(
    drv_path: &str,
    basic_drv: &BasicDerivation,
) -> Vec<types::DerivationNode> {
    let f = node_common_fields(
        basic_drv
            .outputs()
            .iter()
            .map(|o| (o.name().to_string(), o.path().to_string())),
        basic_drv.env(),
        basic_drv.platform(),
    );

    vec![types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Use drv_path as drv_hash fallback (input-addressed derivations
        // already use the store path as the hash; this is consistent)
        drv_hash: drv_path.to_string(),
        pname: f.pname,
        system: f.system,
        required_features: f.required_features,
        output_names: f.output_names,
        is_fixed_output: basic_drv.outputs().iter().any(|o| o.is_fixed_output()),
        expected_output_paths: f.expected_output_paths,
        // Single-node fallback: BasicDerivation has no inputDrvs. We
        // COULD serialize it, but this path is the "full drv not
        // available" fallback — if we don't have the full thing, the
        // worker will fetch it from store. Keep the fallback simple.
        drv_content: Vec::new(),
    }]
}

/// Convert a Derivation into a proto DerivationNode.
fn derivation_to_node(drv_path: &StorePath, drv: &Derivation) -> types::DerivationNode {
    let f = node_common_fields(
        drv.outputs()
            .iter()
            .map(|o| (o.name().to_string(), o.path().to_string())),
        drv.env(),
        drv.platform(),
    );

    types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Input-addressed derivations use the store path as the drv_hash.
        // This ensures every node has a unique, non-empty key in the DAG.
        drv_hash: drv_path.to_string(),
        pname: f.pname,
        system: f.system,
        required_features: f.required_features,
        output_names: f.output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths: f.expected_output_paths,
        // Empty here — filter_and_inline_drv() populates AFTER the
        // FindMissingPaths check. Inlining now would waste bytes on
        // cache-hit nodes that never dispatch.
        drv_content: Vec::new(),
    }
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
    // Collect all expected output paths across the DAG. One batched
    // FindMissingPaths call instead of N.
    let all_outputs: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.expected_output_paths.iter().cloned())
        .collect();

    if all_outputs.is_empty() {
        // No expected outputs (all BasicDerivation fallbacks, or
        // unusual derivations). Nothing to gate on; don't inline.
        return;
    }

    // Single FindMissingPaths. Timeout matches the other gateway
    // store calls. On any error: skip inlining (safe degrade).
    let missing: HashSet<String> = match tokio::time::timeout(
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
    };

    // Walk nodes; inline those with ANY missing output.
    let mut total_inlined: usize = 0;
    let mut inlined_count: usize = 0;
    let mut skipped_budget: usize = 0;

    for node in nodes.iter_mut() {
        // At least one output missing → this node will dispatch.
        // All outputs present → cache hit → never dispatches → skip.
        let will_dispatch = node
            .expected_output_paths
            .iter()
            .any(|p| missing.contains(p));
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

        // Budget gate. Once we hit 16 MB, stop inlining. Remaining
        // nodes fall back to worker-fetch. "Stop" not "skip" — there
        // could be hundreds more small ones, not worth looping to find
        // them when we're already over budget.
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
pub fn build_submit_request(
    nodes: Vec<types::DerivationNode>,
    edges: Vec<types::DerivationEdge>,
    options: Option<&ClientOptions>,
    priority_class: &str,
) -> types::SubmitBuildRequest {
    let (max_silent_time, build_timeout, build_cores, keep_going) = match options {
        Some(opts) => (
            opts.max_silent_time,
            opts.build_timeout(),
            opts.build_cores,
            opts.keep_going,
        ),
        None => (0, 0, 0, false),
    };

    types::SubmitBuildRequest {
        tenant_id: String::new(), // Populated from SSH key in production
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
    fn test_single_node_no_features() -> anyhow::Result<()> {
        let drv = make_basic_drv(BTreeMap::new())?;
        let nodes = single_node_from_basic("/nix/store/test.drv", &drv);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].required_features.is_empty());
        Ok(())
    }

    // -------------------------------------------------------------------
    // T7: reconstruct_dag unit tests (8.7)
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
        let outputs = format!(r#"[("out","{out_path}","","")]"#);
        let inputs: Vec<String> = input_drvs
            .iter()
            .map(|(path, outs)| {
                let outs_str: Vec<String> = outs.iter().map(|o| format!(r#""{o}""#)).collect();
                format!(r#"("{path}",[{}])"#, outs_str.join(","))
            })
            .collect();
        let input_drvs_str = format!("[{}]", inputs.join(","));
        let aterm = format!(
            r#"Derive({outputs},{input_drvs_str},[],"x86_64-linux","/bin/sh",[],[("out","{out_path}")])"#
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
        // Previously this produced a stub leaf with system:"" that never
        // matched any worker, causing a silent hang. Now it fails immediately.
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
        use rio_test_support::grpc::spawn_mock_store;

        let (store, addr, _handle) = spawn_mock_store().await?;
        let mut store_client = rio_proto::client::connect_store(&addr.to_string()).await?;

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

        // Early return on empty — doesn't even hit the (dead) store.
        assert!(nodes[0].drv_content.is_empty());
    }
}
