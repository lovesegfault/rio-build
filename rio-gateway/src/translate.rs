//! DAG reconstruction and gRPC request building.
//!
//! Translates the per-session derivation cache into `SubmitBuildRequest`
//! messages for the scheduler, walking `inputDrvs` recursively to build
//! the full derivation graph.

use std::collections::{HashMap, HashSet, VecDeque};

use rio_nix::derivation::{BasicDerivation, Derivation};
use rio_nix::store_path::StorePath;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types;
use tonic::transport::Channel;
use tracing::debug;

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

/// Create a single-node DAG from a BasicDerivation (no inputDrvs).
/// Used as fallback when the full Derivation is not available.
pub fn single_node_from_basic(
    drv_path: &str,
    basic_drv: &BasicDerivation,
) -> Vec<types::DerivationNode> {
    let output_names: Vec<String> = basic_drv
        .outputs()
        .iter()
        .map(|o| o.name().to_string())
        .collect();
    let expected_output_paths: Vec<String> = basic_drv
        .outputs()
        .iter()
        .map(|o| o.path().to_string())
        .collect();

    let pname = basic_drv.env().get("pname").cloned().unwrap_or_default();

    vec![types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Use drv_path as drv_hash fallback (input-addressed derivations
        // already use the store path as the hash; this is consistent)
        drv_hash: drv_path.to_string(),
        pname,
        system: basic_drv.platform().to_string(),
        required_features: basic_drv
            .env()
            .get("requiredSystemFeatures")
            .map(|s| s.split_whitespace().map(String::from).collect())
            .unwrap_or_default(),
        output_names,
        is_fixed_output: basic_drv.outputs().iter().any(|o| o.is_fixed_output()),
        expected_output_paths,
    }]
}

/// Convert a Derivation into a proto DerivationNode.
fn derivation_to_node(drv_path: &StorePath, drv: &Derivation) -> types::DerivationNode {
    let output_names: Vec<String> = drv.outputs().iter().map(|o| o.name().to_string()).collect();
    let expected_output_paths: Vec<String> =
        drv.outputs().iter().map(|o| o.path().to_string()).collect();

    let pname = drv.env().get("pname").cloned().unwrap_or_default();

    let required_features: Vec<String> = drv
        .env()
        .get("requiredSystemFeatures")
        .map(|s| s.split_whitespace().map(String::from).collect())
        .unwrap_or_default();

    types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Input-addressed derivations use the store path as the drv_hash.
        // This ensures every node has a unique, non-empty key in the DAG.
        drv_hash: drv_path.to_string(),
        pname,
        system: drv.platform().to_string(),
        required_features,
        output_names,
        is_fixed_output: drv.is_fixed_output(),
        expected_output_paths,
    }
}

/// Build a `SubmitBuildRequest` from nodes, edges, and client options.
pub fn build_submit_request(
    nodes: Vec<types::DerivationNode>,
    edges: Vec<types::DerivationEdge>,
    options: &Option<ClientOptions>,
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

    fn make_basic_drv(env: BTreeMap<String, String>) -> BasicDerivation {
        let output = DerivationOutput::new("out", "/nix/store/test-out", "", "").unwrap();
        BasicDerivation::new(
            vec![output],
            BTreeSet::new(),
            "x86_64-linux".into(),
            "/bin/sh".into(),
            vec![],
            env,
        )
        .unwrap()
    }

    #[test]
    fn test_single_node_extracts_features() {
        let mut env = BTreeMap::new();
        env.insert("requiredSystemFeatures".into(), "kvm big-parallel".into());
        let drv = make_basic_drv(env);

        let nodes = single_node_from_basic("/nix/store/test.drv", &drv);
        assert_eq!(nodes.len(), 1);
        assert_eq!(
            nodes[0].required_features,
            vec!["kvm".to_string(), "big-parallel".to_string()],
            "requiredSystemFeatures should be extracted from BasicDerivation env"
        );
    }

    #[test]
    fn test_single_node_no_features() {
        let drv = make_basic_drv(BTreeMap::new());
        let nodes = single_node_from_basic("/nix/store/test.drv", &drv);
        assert_eq!(nodes.len(), 1);
        assert!(nodes[0].required_features.is_empty());
    }

    // -------------------------------------------------------------------
    // T7: reconstruct_dag unit tests (8.7)
    // -------------------------------------------------------------------
    //
    // reconstruct_dag calls resolve_derivation which checks drv_cache FIRST
    // before hitting the store. By pre-populating drv_cache with all needed
    // derivations, we can test reconstruct_dag without a live store.

    use rio_proto::store::store_service_client::StoreServiceClient;

    /// Spin up a mock store that fails all RPCs (lazy connect to dead port).
    /// Used to verify reconstruct_dag fails hard on unresolvable inputDrvs.
    async fn unreachable_store() -> StoreServiceClient<tonic::transport::Channel> {
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
        let root_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv");
        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[]);

        let mut store = unreachable_store().await;
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
        let root_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv");
        let child_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-child.drv");

        let root_drv = make_test_derivation(
            "/nix/store/aaa-root-out",
            &[(&child_path.to_string(), &["out"])],
        );
        let child_drv = make_test_derivation("/nix/store/bbb-child-out", &[]);

        let mut store = unreachable_store().await;
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
        let root_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv");
        let missing_child = "/nix/store/cccccccccccccccccccccccccccccccc-missing.drv";

        let root_drv =
            make_test_derivation("/nix/store/aaa-root-out", &[(missing_child, &["out"])]);

        let mut store = unreachable_store().await;
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
        let root_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-root.drv");
        let bogus_child = "/not/a/store/path";

        let root_drv = make_test_derivation("/nix/store/aaa-root-out", &[(bogus_child, &["out"])]);

        let mut store = unreachable_store().await;
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
        let a_path = sp("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv");
        let b_path = sp("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv");
        let c_path = sp("/nix/store/cccccccccccccccccccccccccccccccc-c.drv");

        let a_drv = make_test_derivation("/nix/store/aaa-out", &[(&b_path.to_string(), &["out"])]);
        let b_drv = make_test_derivation("/nix/store/bbb-out", &[(&c_path.to_string(), &["out"])]);
        let c_drv = make_test_derivation("/nix/store/ccc-out", &[]);

        let mut store = unreachable_store().await;
        let mut cache = HashMap::new();
        cache.insert(b_path.clone(), b_drv);
        cache.insert(c_path.clone(), c_drv);

        let (nodes, edges) = reconstruct_dag(&a_path, &a_drv, &mut store, &mut cache)
            .await
            .expect("reconstruct should succeed");

        assert_eq!(nodes.len(), 3, "A->B->C chain -> 3 nodes");
        assert_eq!(edges.len(), 2, "A->B and B->C -> 2 edges");
    }
}
