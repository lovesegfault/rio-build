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
#[ignore = "Requires Phase 3.2 (agent watching Raft commits). Run with: cargo test --ignored"]
async fn test_end_to_end_build_flow() {
    // Start the actual rio-agent server with Raft
    let temp_dir = tempfile::tempdir().expect("Failed to create temp dir");
    let temp_path =
        camino::Utf8Path::from_path(temp_dir.path()).expect("Invalid UTF-8 path for temp dir");

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().expect("No local addr");
    let listen_addr = format!("127.0.0.1:{}", addr.port());
    let url = format!("http://{}", listen_addr);

    // Bootstrap agent with Raft (fast intervals for testing)
    let (agent, _h1, _h2) = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(std::time::Duration::from_secs(1)),
        Some(std::time::Duration::from_millis(500)),
        Some(std::time::Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let server_task = tokio::spawn(async move {
        use rio_common::proto::rio_agent_server::RioAgentServer;
        use tonic::transport::Server;

        Server::builder()
            .add_service(RioAgentServer::new(
                rio_agent::grpc_server::RioAgentService::new(agent),
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("Server failed");
    });

    // Wait for server to start and agent to become leader
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

    // Subscribe to build - THIS IS THE CRITICAL PART that triggers the deadlock!
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

    // Clean up
    server_task.abort();
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

    let listener = tokio::net::TcpListener::bind("127.0.0.1:0")
        .await
        .expect("Failed to bind");
    let addr = listener.local_addr().expect("No local addr");
    let listen_addr = format!("127.0.0.1:{}", addr.port());
    let url = format!("http://{}", listen_addr);

    let (agent, _h1, _h2) = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;

    // Start gRPC server
    let server_task = tokio::spawn(async move {
        use rio_common::proto::rio_agent_server::RioAgentServer;
        use tonic::transport::Server;

        Server::builder()
            .add_service(RioAgentServer::new(
                rio_agent::grpc_server::RioAgentService::new(agent),
            ))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .expect("Server failed");
    });

    // Wait for leader election
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

    server_task.abort();
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

    let listen_addr = "127.0.0.1:50999".to_string();
    // Use fast intervals for testing: 1s heartbeat, 0.5s check, 3s timeout
    let (agent, _h1, _h2) = rio_agent::agent::Agent::bootstrap(
        temp_path.to_path_buf(),
        listen_addr,
        Some(Duration::from_secs(1)),
        Some(Duration::from_millis(500)),
        Some(Duration::from_secs(3)),
    )
    .await
    .expect("Failed to bootstrap agent");

    let agent_id = agent.id;
    let sm_store = agent
        .state_machine
        .as_ref()
        .expect("No state machine")
        .clone();

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
