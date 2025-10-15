//! Tests for Phase 3.7: Build Deduplication
//!
//! Tests that CLI correctly handles:
//! - AlreadyBuilding responses (concurrent users)
//! - AlreadyCompleted responses (cache hits)
//! - Multiple subscribers to same build

use anyhow::Result;
use camino::Utf8Path;
use rio_build::{client, cluster, evaluator};
use rio_common::proto::build_update;
use std::time::Duration;
use uuid::Uuid;

/// Create a unique Nix derivation that won't be cached
async fn create_unique_derivation() -> Result<(String, Vec<u8>)> {
    let unique_id = Uuid::new_v4();
    let nix_expr = format!(
        r#"
with import <nixpkgs> {{}};
runCommandNoCC "rio-dedup-test-{}" {{}} ''
  echo "Deduplication test {}"
  echo "output-{}" > $out
''
"#,
        unique_id, unique_id, unique_id
    );

    let temp_file = format!("/tmp/rio-dedup-test-{}.nix", unique_id);
    tokio::fs::write(&temp_file, nix_expr).await?;

    let output = tokio::process::Command::new("nix-instantiate")
        .arg(&temp_file)
        .output()
        .await?;

    anyhow::ensure!(output.status.success(), "nix-instantiate failed");

    let drv_path = String::from_utf8(output.stdout)?.trim().to_string();

    let export_output = tokio::process::Command::new("nix-store")
        .arg("--export")
        .arg(&drv_path)
        .output()
        .await?;

    anyhow::ensure!(export_output.status.success(), "nix-store --export failed");

    let _ = tokio::fs::remove_file(&temp_file).await;

    Ok((drv_path, export_output.stdout))
}

/// Test that AlreadyBuilding response correctly subscribes to existing build
#[tokio::test]
async fn test_already_building_response() {
    // Start agent
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path");

    let listen_addr = "127.0.0.1:50895".to_string();
    let agent_url = format!("http://{}", listen_addr);

    let _agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr.clone(),
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Discover cluster
    let seed_url = url::Url::parse(&agent_url).expect("Invalid URL");
    let cluster_info = cluster::discover_cluster(&[seed_url])
        .await
        .expect("Failed to discover cluster");

    // Create unique derivation
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // First client: Submit build (will get BuildAssigned)
    let mut client1 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 1");

    let build_info1 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 1: Submitting build...");
    let mut stream1 = client1
        .submit_build(build_info1, &cluster_info)
        .await
        .expect("Client 1 failed to submit build");

    // Consume a few log messages to ensure build is running
    let mut log_count = 0;
    for _ in 0..3 {
        if let Some(Ok(update)) = stream1.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Log(_))) {
                log_count += 1;
            }
        } else {
            break;
        }
    }

    println!(
        "Client 1: Received {} log messages, build is running",
        log_count
    );

    // Second client: Submit same build (should get AlreadyBuilding)
    let mut client2 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 2");

    let build_info2 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 2: Submitting same build (should get AlreadyBuilding)...");
    let mut stream2 = client2
        .submit_build(build_info2, &cluster_info)
        .await
        .expect("Client 2 failed to submit build");

    // Client 2 should receive catch-up logs
    let mut client2_logs = 0;
    let mut client2_completed = false;

    while let Some(Ok(update)) = stream2.message().await.transpose() {
        match update.update {
            Some(build_update::Update::Log(_)) => {
                client2_logs += 1;
            }
            Some(build_update::Update::Completed(_)) => {
                client2_completed = true;
                break;
            }
            Some(build_update::Update::Failed(_)) => {
                panic!("Build failed unexpectedly");
            }
            _ => {}
        }
    }

    println!(
        "Client 2: Received {} logs and completed={}",
        client2_logs, client2_completed
    );

    // Verify client 2 got logs (including catch-up logs)
    assert!(
        client2_logs > 0,
        "Client 2 should receive logs (including catch-up)"
    );
    assert!(client2_completed, "Client 2 should receive completion");

    println!("✓ AlreadyBuilding test passed: Client 2 successfully joined in-progress build");
}

/// Test that AlreadyCompleted response correctly fetches from cache
#[tokio::test]
async fn test_already_completed_response() {
    // Start agent
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path");

    let listen_addr = "127.0.0.1:50896".to_string();
    let agent_url = format!("http://{}", listen_addr);

    let _agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr.clone(),
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Discover cluster
    let seed_url = url::Url::parse(&agent_url).expect("Invalid URL");
    let cluster_info = cluster::discover_cluster(&[seed_url])
        .await
        .expect("Failed to discover cluster");

    // Create unique derivation
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // First client: Submit and complete build
    let mut client1 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 1");

    let build_info1 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 1: Submitting build...");
    let mut stream1 = client1
        .submit_build(build_info1, &cluster_info)
        .await
        .expect("Client 1 failed to submit build");

    // Wait for build to complete
    let mut client1_completed = false;
    while let Some(Ok(update)) = stream1.message().await.transpose() {
        if matches!(update.update, Some(build_update::Update::Completed(_))) {
            client1_completed = true;
            break;
        }
    }

    assert!(client1_completed, "Client 1 build should complete");
    println!("Client 1: Build completed");

    // Wait for Raft to process BuildCompleted
    tokio::time::sleep(Duration::from_millis(500)).await;

    // Second client: Submit same build (should get AlreadyCompleted)
    let mut client2 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 2");

    let build_info2 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 2: Submitting same build (should get AlreadyCompleted)...");
    let mut stream2 = client2
        .submit_build(build_info2, &cluster_info)
        .await
        .expect("Client 2 failed to get cached build");

    // Client 2 should receive outputs from cache
    let mut client2_outputs = false;
    let mut client2_completed = false;

    while let Some(Ok(update)) = stream2.message().await.transpose() {
        match update.update {
            Some(build_update::Update::OutputChunk(_)) => {
                client2_outputs = true;
            }
            Some(build_update::Update::Completed(_)) => {
                client2_completed = true;
                break;
            }
            Some(build_update::Update::Failed(_)) => {
                panic!("Cached build should not fail");
            }
            _ => {}
        }
    }

    println!(
        "Client 2: Received outputs={}, completed={}",
        client2_outputs, client2_completed
    );

    assert!(
        client2_outputs,
        "Client 2 should receive output chunks from cache"
    );
    assert!(client2_completed, "Client 2 should receive completion");

    println!("✓ AlreadyCompleted test passed: Client 2 successfully fetched from cache");
}

/// Test concurrent subscribers to the same build (both get all updates)
#[tokio::test]
async fn test_concurrent_subscribers_same_build() {
    // Start agent
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path = Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path");

    let listen_addr = "127.0.0.1:50897".to_string();
    let agent_url = format!("http://{}", listen_addr);

    let _agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr.clone(),
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    tokio::time::sleep(Duration::from_secs(2)).await;

    // Discover cluster
    let seed_url = url::Url::parse(&agent_url).expect("Invalid URL");
    let cluster_info = cluster::discover_cluster(&[seed_url])
        .await
        .expect("Failed to discover cluster");

    // Create unique derivation
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // Client 1: Submit build
    let mut client1 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 1");

    let build_info1 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 1: Submitting build...");
    let stream1 = client1
        .submit_build(build_info1, &cluster_info)
        .await
        .expect("Client 1 failed to submit");

    // Client 2: Submit same build concurrently (should get AlreadyBuilding)
    let mut client2 = client::RioClient::connect(&cluster_info.leader_address)
        .await
        .expect("Failed to connect client 2");

    let build_info2 = evaluator::BuildInfo {
        drv_path: drv_path.clone().into(),
        drv_nar_bytes: drv_nar.clone(),
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    println!("Client 2: Submitting same build (should get AlreadyBuilding)...");
    let stream2 = client2
        .submit_build(build_info2, &cluster_info)
        .await
        .expect("Client 2 failed to subscribe to existing build");

    // Spawn tasks to consume both streams concurrently
    let task1 = tokio::spawn(async move {
        let mut logs = 0;
        let mut outputs = false;
        let mut completed = false;

        let mut stream = stream1;
        while let Some(Ok(update)) = stream.message().await.transpose() {
            match update.update {
                Some(build_update::Update::Log(_)) => logs += 1,
                Some(build_update::Update::OutputChunk(_)) => outputs = true,
                Some(build_update::Update::Completed(_)) => {
                    completed = true;
                    break;
                }
                Some(build_update::Update::Failed(_)) => {
                    panic!("Build failed");
                }
                _ => {}
            }
        }

        (logs, outputs, completed)
    });

    let task2 = tokio::spawn(async move {
        let mut logs = 0;
        let mut outputs = false;
        let mut completed = false;

        let mut stream = stream2;
        while let Some(Ok(update)) = stream.message().await.transpose() {
            match update.update {
                Some(build_update::Update::Log(_)) => logs += 1,
                Some(build_update::Update::OutputChunk(_)) => outputs = true,
                Some(build_update::Update::Completed(_)) => {
                    completed = true;
                    break;
                }
                Some(build_update::Update::Failed(_)) => {
                    panic!("Build failed");
                }
                _ => {}
            }
        }

        (logs, outputs, completed)
    });

    // Wait for both to complete
    let (client1_logs, client1_outputs, client1_completed) =
        task1.await.expect("Client 1 task panicked");
    let (client2_logs, client2_outputs, client2_completed) =
        task2.await.expect("Client 2 task panicked");

    println!(
        "Client 1: logs={}, outputs={}, completed={}",
        client1_logs, client1_outputs, client1_completed
    );
    println!(
        "Client 2: logs={}, outputs={}, completed={}",
        client2_logs, client2_outputs, client2_completed
    );

    // Both clients should complete successfully
    assert!(client1_completed, "Client 1 should complete");
    assert!(client2_completed, "Client 2 should complete");

    // Both should receive outputs
    assert!(client1_outputs, "Client 1 should receive outputs");
    assert!(client2_outputs, "Client 2 should receive outputs");

    // Both should receive logs
    assert!(client1_logs > 0, "Client 1 should receive logs");
    assert!(
        client2_logs > 0,
        "Client 2 should receive logs (including catch-up)"
    );

    println!("✓ Concurrent subscribers test passed: Both clients received complete build");
}
