//! Integration test for Phase 2: Single-node cluster with CLI discovery
//!
//! Tests the complete flow:
//! 1. Bootstrap single-node Raft cluster
//! 2. CLI discovers cluster and identifies leader
//! 3. CLI submits build to leader
//! 4. Build completes successfully

use anyhow::Result;
use camino::Utf8Path;
use rio_build::{cluster, evaluator};
use std::time::Duration;
use uuid::Uuid;

/// Create a unique Nix derivation that won't be cached
async fn create_unique_derivation() -> Result<(String, Vec<u8>)> {
    let unique_id = Uuid::new_v4();
    let nix_expr = format!(
        r#"
with import <nixpkgs> {{}};
runCommandNoCC "rio-phase2-test-{}" {{}} ''
  echo "Phase 2 integration test {}"
  echo "test-output-{}" > $out
''
"#,
        unique_id, unique_id, unique_id
    );

    // Write to temp file
    let temp_file = format!("/tmp/rio-phase2-test-{}.nix", unique_id);
    tokio::fs::write(&temp_file, nix_expr).await?;

    // Instantiate to get derivation path
    let output = tokio::process::Command::new("nix-instantiate")
        .arg(&temp_file)
        .output()
        .await?;

    anyhow::ensure!(output.status.success(), "nix-instantiate failed");

    let drv_path = String::from_utf8(output.stdout)?.trim().to_string();

    // Export as NAR
    let export_output = tokio::process::Command::new("nix-store")
        .arg("--export")
        .arg(&drv_path)
        .output()
        .await?;

    anyhow::ensure!(export_output.status.success(), "nix-store --export failed");

    // Clean up temp file
    let _ = tokio::fs::remove_file(&temp_file).await;

    Ok((drv_path, export_output.stdout))
}

#[tokio::test]
#[ignore = "Integration test - requires nix commands. Run with: cargo test --ignored"]
async fn test_phase2_single_node_cluster_end_to_end() {
    // 1. Setup: Create agent with Raft cluster
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path");

    // Use a fixed port for testing
    let listen_addr = "127.0.0.1:50997".to_string();
    let agent_url = format!("http://{}", listen_addr);

    println!("Starting agent at: {}", agent_url);

    // Bootstrap agent with fast heartbeat intervals for testing
    // Note: Agent::bootstrap() starts the gRPC server automatically
    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr.clone(),
        Some(Duration::from_secs(1)),     // Fast heartbeat
        Some(Duration::from_millis(500)), // Fast check
        Some(Duration::from_secs(3)),     // Fast timeout
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;
    println!("Agent ID: {}", agent_id);

    // Wait for agent to become leader (server is already running)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // 2. CLI discovers cluster
    println!("Discovering cluster from seed: {}", agent_url);
    let seed_url = url::Url::parse(&agent_url).expect("Invalid URL");
    let cluster_info = cluster::discover_cluster(&[seed_url])
        .await
        .expect("Failed to discover cluster");

    println!("Cluster discovered:");
    println!("  Leader ID: {}", cluster_info.leader_id);
    println!("  Leader address: {}", cluster_info.leader_address);
    println!("  Agents: {}", cluster_info.agents.len());

    // 3. Verify cluster info
    assert_eq!(
        cluster_info.leader_id,
        agent_id.to_string(),
        "Leader ID should match agent ID"
    );
    assert_eq!(cluster_info.agents.len(), 1, "Should have exactly 1 agent");
    // Note: Url type adds trailing slash, so we compare without it
    assert!(
        cluster_info.leader_address.starts_with(&agent_url),
        "Leader address {} should start with {}",
        cluster_info.leader_address,
        agent_url
    );

    // 4. Create unique derivation (not cached)
    println!("Creating unique derivation...");
    let (drv_path, drv_nar_bytes) = create_unique_derivation()
        .await
        .expect("Failed to create unique derivation");

    println!("Derivation: {}", drv_path);

    // Get platform and features for this derivation
    let build_info = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes,
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    // 5. Connect to leader and submit build
    println!("Connecting to leader at: {}", cluster_info.leader_address);
    let mut client = rio_build::client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect to leader");

    println!("Submitting build...");
    let stream = client
        .submit_build(build_info)
        .await
        .expect("Failed to submit build");

    // 6. Handle build stream and verify completion
    println!("Waiting for build to complete...");
    let result = rio_build::output_handler::handle_build_stream(stream).await;

    assert!(
        result.is_ok(),
        "Build should complete successfully: {:?}",
        result.err()
    );

    println!("✓ Phase 2 integration test passed!");
    println!("  - Agent bootstrapped and became leader");
    println!("  - CLI discovered cluster");
    println!("  - Build submitted and completed via cluster discovery");

    // Cleanup
    // Server runs in background and will be cleaned up when test ends
}
