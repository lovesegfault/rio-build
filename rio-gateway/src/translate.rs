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
use tracing::{debug, warn};

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
                // Resolve this child derivation
                let child_sp = match StorePath::parse(child_path_str) {
                    Ok(sp) => sp,
                    Err(e) => {
                        warn!(
                            path = %child_path_str,
                            error = %e,
                            "invalid inputDrv store path, skipping"
                        );
                        continue;
                    }
                };

                match resolve_derivation(&child_sp, store_client, drv_cache).await {
                    Ok(child_drv) => {
                        queue.push_back((child_sp, child_drv));
                    }
                    Err(e) => {
                        tracing::error!(
                            path = %child_path_str,
                            error = %e,
                            "failed to resolve inputDrv, including as leaf node"
                        );
                        // Include as a leaf node without further traversal.
                        // Use drv_path as drv_hash fallback to ensure uniqueness
                        // (empty hash would collide if multiple children failed).
                        nodes.push(types::DerivationNode {
                            drv_path: child_path_str.clone(),
                            drv_hash: child_path_str.clone(),
                            pname: String::new(),
                            system: String::new(),
                            required_features: Vec::new(),
                            output_names: Vec::new(),
                            is_fixed_output: false,
                        });
                    }
                }
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

    let pname = basic_drv.env().get("pname").cloned().unwrap_or_default();

    vec![types::DerivationNode {
        drv_path: drv_path.to_string(),
        // Use drv_path as drv_hash fallback (input-addressed derivations
        // already use the store path as the hash; this is consistent)
        drv_hash: drv_path.to_string(),
        pname,
        system: basic_drv.platform().to_string(),
        required_features: Vec::new(),
        output_names,
        is_fixed_output: basic_drv.outputs().iter().any(|o| o.is_fixed_output()),
    }]
}

/// Convert a Derivation into a proto DerivationNode.
fn derivation_to_node(drv_path: &StorePath, drv: &Derivation) -> types::DerivationNode {
    let output_names: Vec<String> = drv.outputs().iter().map(|o| o.name().to_string()).collect();

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
