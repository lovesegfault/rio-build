//! Full integration test - tests the complete gRPC server flow

use rio_common::proto::rio_agent_client::RioAgentClient;
use rio_common::proto::{QueueBuildRequest, SubscribeToBuildRequest, build_update};
use tokio::process::Command;
use uuid::Uuid;

/// Create a unique Nix derivation that won't be cached
async fn create_unique_derivation() -> anyhow::Result<(String, Vec<u8>)> {
    let unique_id = Uuid::new_v4();
    let nix_expr = format!(
        r#"
with import <nixpkgs> {{}};
runCommandNoCC "rio-test-{}" {{}} ''
  echo "Building unique test {}"
  echo "output-{}" > $out
''
"#,
        unique_id, unique_id, unique_id
    );

    // Write to temp file
    let temp_file = format!("/tmp/rio-test-{}.nix", unique_id);
    tokio::fs::write(&temp_file, nix_expr)
        .await
        .expect("Failed to write temp file");

    // Instantiate to get derivation path
    let output = Command::new("nix-instantiate")
        .arg(&temp_file)
        .output()
        .await
        .expect("Failed to run nix-instantiate");

    assert!(output.status.success(), "nix-instantiate failed");

    let drv_path = String::from_utf8(output.stdout)
        .expect("Invalid UTF-8")
        .trim()
        .to_string();

    // Export as NAR
    let export_output = Command::new("nix-store")
        .arg("--export")
        .arg(&drv_path)
        .output()
        .await
        .expect("Failed to export");

    assert!(export_output.status.success(), "nix-store --export failed");

    // Clean up temp file
    let _ = tokio::fs::remove_file(&temp_file).await;

    Ok((drv_path, export_output.stdout))
}

#[tokio::test]
async fn test_end_to_end_build_flow() {
    // Start the actual rio-agent server with Raft
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    // Use port 0 for dynamic allocation
    let listen_addr = "127.0.0.1:0".to_string();

    // Bootstrap agent with Raft (fast intervals for testing)
    // Note: bootstrap() now starts the gRPC server automatically
    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(std::time::Duration::from_secs(1)),
        Some(std::time::Duration::from_millis(500)),
        Some(std::time::Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    // Get actual bound address from cluster state
    let url = {
        let state = agent.state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent.id)
            .map(|a| a.address.to_string())
            .expect("Agent should be in cluster state")
    };

    println!("Agent bound to {}", url);

    // Wait for agent to become leader (server is already running)
    tokio::time::sleep(tokio::time::Duration::from_secs(2)).await;

    // Create unique derivation (guaranteed not cached)
    let (drv_path, drv_nar_bytes) = create_unique_derivation()
        .await
        .expect("Failed to create unique derivation");

    println!("Testing with derivation: {}", drv_path);

    // Connect client
    let mut client = RioAgentClient::connect(url.clone())
        .await
        .expect("Failed to connect");

    // Submit build (QueueBuild RPC)
    let queue_response = client
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar_bytes,
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: None,
        })
        .await
        .expect("Failed to queue build")
        .into_inner();

    // Verify we got BuildAssigned
    assert!(
        matches!(
            queue_response.result,
            Some(rio_common::proto::queue_build_response::Result::Assigned(_))
        ),
        "Should receive BuildAssigned"
    );

    // Wait for coordinator to notice assignment and start build (polls every 100ms)
    tokio::time::sleep(tokio::time::Duration::from_millis(300)).await;

    // Subscribe to build
    let mut stream = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("Failed to subscribe")
        .into_inner();

    // Collect build updates
    let mut got_log = false;
    let mut got_output_chunk = false;
    let mut got_completion = false;
    let mut update_count = 0;

    // Timeout the test if it hangs (would catch the deadlock)
    let timeout = tokio::time::sleep(tokio::time::Duration::from_secs(10));
    tokio::pin!(timeout);

    loop {
        tokio::select! {
            result = stream.message() => {
                match result {
                    Ok(Some(update)) => {
                        update_count += 1;
                        match update.update {
                            Some(build_update::Update::Log(log)) => {
                                println!("LOG: {}", log.line.trim());
                                got_log = true;
                            }
                            Some(build_update::Update::OutputChunk(_)) => {
                                println!("Got output chunk");
                                got_output_chunk = true;
                            }
                            Some(build_update::Update::Completed(completed)) => {
                                println!("Build completed: {:?}", completed.output_paths);
                                got_completion = true;
                                break;
                            }
                            Some(build_update::Update::Failed(failed)) => {
                                panic!("Build failed: {}", failed.error);
                            }
                            None => {}
                        }
                    }
                    Ok(None) => {
                        println!("Stream ended");
                        break;
                    }
                    Err(e) => {
                        panic!("Stream error: {}", e);
                    }
                }
            }
            _ = &mut timeout => {
                panic!("Test timed out after 10s - likely deadlock!");
            }
        }
    }

    assert!(got_log, "Should receive at least one log message");
    assert!(got_output_chunk, "Should receive output chunks");
    assert!(got_completion, "Should receive completion message");

    println!("Test passed! Received {} updates total", update_count);
}

/// Test that we can export and import a derivation via NAR
#[tokio::test]
async fn test_derivation_nar_roundtrip() {
    let output = Command::new("nix-instantiate")
        .arg("../../tests/fixtures/trivial.nix")
        .output()
        .await
        .expect("Failed to run nix-instantiate");

    assert!(output.status.success());

    let drv_path = String::from_utf8(output.stdout)
        .expect("Invalid UTF-8")
        .trim()
        .to_string();

    // Export as NAR
    let export_output = Command::new("nix-store")
        .arg("--export")
        .arg(&drv_path)
        .output()
        .await
        .expect("Failed to export derivation");

    assert!(export_output.status.success());
    let nar_bytes = export_output.stdout;

    // Import back
    let mut import_child = Command::new("nix-store")
        .arg("--import")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .spawn()
        .expect("Failed to spawn nix-store --import");

    use tokio::io::AsyncWriteExt;
    let mut stdin = import_child.stdin.take().expect("No stdin");
    stdin.write_all(&nar_bytes).await.expect("Failed to write");
    stdin.shutdown().await.expect("Failed to close stdin");

    let import_output = import_child
        .wait_with_output()
        .await
        .expect("Failed to wait");
    assert!(import_output.status.success());

    let imported_path = String::from_utf8(import_output.stdout)
        .expect("Invalid UTF-8")
        .trim()
        .to_string();

    assert_eq!(drv_path, imported_path);
}

/// Test Phase 3.1: Queue build via Raft (assignment only, no execution)
#[tokio::test]
async fn test_queue_build_via_raft() {
    use std::time::Duration;

    // Bootstrap agent with Raft
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listen_addr = "127.0.0.1:0".to_string();

    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;

    // Get actual bound address
    let url = {
        let state = agent.state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent_id)
            .map(|a| a.address.to_string())
            .expect("Agent should be in cluster state")
    };

    // Wait for leader election (server already running)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Connect client
    let mut client = RioAgentClient::connect(url)
        .await
        .expect("Failed to connect");

    // Submit build
    let drv_path = "/nix/store/test-foo.drv";
    let response = client
        .queue_build(rio_common::proto::QueueBuildRequest {
            derivation_path: drv_path.to_string(),
            derivation: vec![1, 2, 3], // Dummy NAR
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: None,
        })
        .await
        .expect("Failed to queue build")
        .into_inner();

    // Verify BuildAssigned response
    match response.result {
        Some(rio_common::proto::queue_build_response::Result::Assigned(assigned)) => {
            assert_eq!(assigned.agent_id, agent_id.to_string());
            assert_eq!(assigned.derivation_path, drv_path);
            println!("✓ Build assigned to agent {} via Raft", assigned.agent_id);
        }
        other => panic!("Expected BuildAssigned, got: {:?}", other),
    }

    // Test deduplication: Submit same build again
    let response2 = client
        .queue_build(rio_common::proto::QueueBuildRequest {
            derivation_path: drv_path.to_string(),
            derivation: vec![1, 2, 3],
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: None,
        })
        .await
        .expect("Failed to queue build")
        .into_inner();

    // Should get AlreadyBuilding (build is in Raft state)
    match response2.result {
        Some(rio_common::proto::queue_build_response::Result::AlreadyBuilding(already)) => {
            assert_eq!(already.agent_id, agent_id.to_string());
            assert_eq!(already.derivation_path, drv_path);
            println!("✓ Deduplication works: AlreadyBuilding response");
        }
        other => panic!("Expected AlreadyBuilding, got: {:?}", other),
    }
}

/// Test heartbeat lifecycle with bootstrapped Raft cluster
#[tokio::test]
async fn test_heartbeat_lifecycle() {
    use chrono::Utc;
    use std::time::Duration;

    // Bootstrap agent with Raft
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listen_addr = "127.0.0.1:0".to_string();
    // Use fast intervals for testing: 1s heartbeat, 0.5s check, 3s timeout
    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;
    let sm_store = agent.state_machine.clone();

    // Wait for initial heartbeat (should happen within 1-2 seconds with fast interval)
    tokio::time::sleep(Duration::from_millis(2500)).await;

    // Verify agent's heartbeat is recent
    let data = sm_store.data.read();
    let agent_info = data
        .cluster
        .agents
        .get(&agent_id)
        .expect("Agent not found in cluster");
    let age = Utc::now().signed_duration_since(agent_info.last_heartbeat);

    assert!(
        age.num_milliseconds() < 1500,
        "Heartbeat should be recent, but was {} milliseconds old",
        age.num_milliseconds()
    );

    println!(
        "✓ Heartbeat test passed: heartbeat age = {}ms",
        age.num_milliseconds()
    );
}

/// Test that late joiners receive catch-up logs from history
#[tokio::test]
async fn test_late_joiner_receives_catch_up_logs() {
    use std::time::Duration;

    // Start agent with Raft (bootstrap starts gRPC server automatically)
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listen_addr = "127.0.0.1:0".to_string();

    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    // Get actual bound address
    let url = {
        let state = agent.state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent.id)
            .map(|a| a.address.to_string())
            .expect("Agent should be in cluster state")
    };

    // Wait for agent to become leader (server is already running)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create client
    let mut client = RioAgentClient::connect(url.clone())
        .await
        .expect("Failed to connect");

    // Create unique build
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // Submit build
    let response = client
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar,
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await
        .expect("QueueBuild failed");

    let result = response.into_inner().result.expect("No result");
    assert!(
        matches!(
            result,
            rio_common::proto::queue_build_response::Result::Assigned(_)
        ),
        "Expected BuildAssigned"
    );

    // Wait for build coordinator to pick up the assignment (polls every 100ms)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Subscribe immediately (first subscriber)
    let mut stream1 = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("SubscribeToBuild failed")
        .into_inner();

    // Collect a few log messages from first subscriber
    let mut first_subscriber_logs = Vec::new();
    for _ in 0..3 {
        if let Some(Ok(update)) = stream1.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Log(_))) {
                first_subscriber_logs.push(update);
            }
        } else {
            break;
        }
    }

    println!(
        "First subscriber received {} log messages",
        first_subscriber_logs.len()
    );

    // Now subscribe as a late joiner (after some logs have been generated)
    let mut stream2 = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("SubscribeToBuild failed for late joiner")
        .into_inner();

    // Late joiner should receive catch-up logs first (with timeout)
    let late_joiner_catch_up_logs = tokio::time::timeout(Duration::from_secs(30), async {
        let mut logs = Vec::new();
        while let Some(Ok(update)) = stream2.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Log(_))) {
                logs.push(update.clone());
            }

            // Stop collecting after we get some logs or hit completion
            if logs.len() >= first_subscriber_logs.len()
                || matches!(update.update, Some(build_update::Update::Completed(_)))
                || matches!(update.update, Some(build_update::Update::Failed(_)))
            {
                break;
            }
        }
        logs
    })
    .await
    .expect("Test timeout: late joiner did not receive logs within 30 seconds");

    println!(
        "Late joiner received {} catch-up log messages",
        late_joiner_catch_up_logs.len()
    );

    // Verify late joiner got catch-up logs (should have at least some logs)
    assert!(
        !late_joiner_catch_up_logs.is_empty(),
        "Late joiner should receive catch-up logs from history"
    );

    println!("✓ Late joiner test passed: received catch-up logs");
}

/// Test that build completion updates Raft state correctly
#[tokio::test]
async fn test_build_completion_updates_raft_state() {
    use std::time::Duration;

    // Start agent with Raft (bootstrap starts gRPC server automatically)
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listen_addr = "127.0.0.1:0".to_string();

    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;
    let state_machine = agent.state_machine.clone();

    // Get actual bound address
    let url = {
        let state = state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent_id)
            .map(|a| a.address.to_string())
            .expect("Agent should be in cluster state")
    };

    // Wait for agent to become leader (server is already running)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create client
    let mut client = RioAgentClient::connect(url.clone())
        .await
        .expect("Failed to connect");

    // Create unique build
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // Verify build is NOT in state before submission
    {
        let state = state_machine.data.read();
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
        assert!(
            !state.cluster.builds_in_progress.contains_key(&drv_path_key),
            "Build should not be in progress before submission"
        );
        assert!(
            !state.cluster.completed_builds.contains_key(&drv_path_key),
            "Build should not be in completed before submission"
        );
    }

    // Submit build
    let response = client
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar,
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await
        .expect("QueueBuild failed");

    let result = response.into_inner().result.expect("No result");
    assert!(
        matches!(
            result,
            rio_common::proto::queue_build_response::Result::Assigned(_)
        ),
        "Expected BuildAssigned"
    );

    // Verify build is in builds_in_progress after submission
    // Wait for build coordinator to pick up assignment (polls every 100ms)
    tokio::time::sleep(Duration::from_millis(300)).await;
    {
        let state = state_machine.data.read();
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
        let tracker = state.cluster.builds_in_progress.get(&drv_path_key);
        assert!(
            tracker.is_some(),
            "Build should be in progress after submission"
        );
        assert_eq!(
            tracker.unwrap().agent_id,
            agent_id,
            "Build should be assigned to this agent"
        );
    }

    // Subscribe and wait for completion
    let mut stream = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("SubscribeToBuild failed")
        .into_inner();

    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(result) = stream.message().await.transpose() {
            match result {
                Ok(update) => {
                    if matches!(update.update, Some(build_update::Update::Completed(_))) {
                        return true;
                    }
                    if matches!(update.update, Some(build_update::Update::Failed(_))) {
                        panic!("Build failed unexpectedly");
                    }
                }
                Err(e) => {
                    panic!("Stream error: {}", e);
                }
            }
        }
        // Stream ended without completion message
        false
    })
    .await;

    match completed {
        Ok(true) => {
            // Build completed successfully
        }
        Ok(false) => {
            panic!("Stream ended without receiving completion message");
        }
        Err(_) => {
            panic!("Test timeout: build did not complete within 30 seconds");
        }
    }

    // Poll for Raft state to update (with timeout)
    let mut raft_updated = false;
    for _attempt in 0..50 {
        let has_completed = {
            let state = state_machine.data.read();
            let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
            state.cluster.completed_builds.contains_key(&drv_path_key)
        }; // Lock dropped here

        if has_completed {
            raft_updated = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        raft_updated,
        "Raft should process BuildCompleted within 5 seconds"
    );

    // Verify build moved from in_progress to completed_builds
    {
        let state = state_machine.data.read();
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();

        assert!(
            !state.cluster.builds_in_progress.contains_key(&drv_path_key),
            "Build should be removed from in_progress after completion"
        );

        let completed = state.cluster.completed_builds.get(&drv_path_key);
        assert!(
            completed.is_some(),
            "Build should be in completed_builds after completion"
        );

        let completed_build = completed.unwrap();
        assert_eq!(
            completed_build.agent_id, agent_id,
            "Completed build should track which agent built it"
        );
        assert!(
            !completed_build.output_paths.is_empty(),
            "Completed build should have output paths"
        );

        println!(
            "✓ Completed build in cache with {} outputs",
            completed_build.output_paths.len()
        );
    }

    println!("✓ Build completion Raft state test passed");
}

/// Test GetCompletedBuild RPC serves cached outputs
#[tokio::test]
async fn test_get_completed_build_serves_cache() {
    use rio_common::proto::GetCompletedBuildRequest;
    use std::time::Duration;

    // Start agent with Raft (bootstrap starts gRPC server automatically)
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listen_addr = "127.0.0.1:0".to_string();

    let agent = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let state_machine = agent.state_machine.clone();

    // Get actual bound address
    let url = {
        let state = state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent.id)
            .map(|a| a.address.to_string())
            .expect("Agent should be in cluster state")
    };

    // Wait for agent to become leader (server is already running)
    tokio::time::sleep(Duration::from_secs(2)).await;

    // Create client
    let mut client = RioAgentClient::connect(url.clone())
        .await
        .expect("Failed to connect");

    // Create and complete a build first
    let (drv_path, drv_nar) = create_unique_derivation()
        .await
        .expect("Failed to create derivation");

    // Submit build
    client
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar,
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await
        .expect("QueueBuild failed");

    // Wait for build coordinator to pick up assignment (polls every 100ms)
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Subscribe and wait for completion
    let mut stream = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("SubscribeToBuild failed")
        .into_inner();

    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(Ok(update)) = stream.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Completed(_))) {
                return true;
            }
        }
        false
    })
    .await;

    match completed {
        Ok(true) => {
            // Build completed successfully
        }
        Ok(false) => {
            panic!("Stream ended without receiving completion message");
        }
        Err(_) => {
            panic!("Test timeout: build did not complete within 30 seconds");
        }
    }

    // Poll for Raft state to update (instead of fixed sleep)
    let mut raft_updated = false;
    for _attempt in 0..50 {
        let has_completed = {
            let state = state_machine.data.read();
            let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
            state.cluster.completed_builds.contains_key(&drv_path_key)
        }; // Lock dropped here

        if has_completed {
            raft_updated = true;
            break;
        }

        tokio::time::sleep(Duration::from_millis(100)).await;
    }

    assert!(
        raft_updated,
        "Raft should process BuildCompleted within 5 seconds"
    );

    println!("Build completed, now testing GetCompletedBuild...");

    // Now try to get the completed build from cache
    let mut cache_stream = client
        .get_completed_build(GetCompletedBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await
        .expect("GetCompletedBuild should succeed for cached build")
        .into_inner();

    // Verify we receive output chunks and completion message
    let mut received_outputs = false;
    let mut received_completion = false;

    while let Some(Ok(update)) = cache_stream.message().await.transpose() {
        match update.update {
            Some(build_update::Update::OutputChunk(_)) => {
                received_outputs = true;
                println!("Received output chunk from cache");
            }
            Some(build_update::Update::Completed(_)) => {
                received_completion = true;
                println!("Received completion message from cache");
                break;
            }
            _ => {}
        }
    }

    assert!(received_outputs, "Should receive output chunks from cache");
    assert!(
        received_completion,
        "Should receive completion message from cache"
    );

    println!("✓ GetCompletedBuild test passed: served outputs from cache");

    // Test that requesting non-existent build returns error
    let non_existent_result = client
        .get_completed_build(GetCompletedBuildRequest {
            derivation_path: "/nix/store/nonexistent-foo.drv".to_string(),
        })
        .await;

    assert!(
        non_existent_result.is_err(),
        "GetCompletedBuild should fail for non-existent build"
    );

    println!("✓ GetCompletedBuild correctly rejects non-existent build");
}
