//! Integration tests for rio-build client

mod mock_agent;

use anyhow::Context;
use camino::Utf8PathBuf;
use rio_build::client::RioClient;
use rio_build::cluster::ClusterInfo;
use rio_build::evaluator::BuildInfo;
use rio_common::proto::{AgentInfo, BuildCompleted, BuildUpdate, LogLine, build_update};
use std::collections::HashMap;

#[tokio::test(flavor = "multi_thread")]
async fn test_client_submit_and_subscribe() -> anyhow::Result<()> {
    // Create mock agent with test response
    let drv_path = "/nix/store/test123-hello.drv";
    let mock = mock_agent::MockRioAgent::new_queued(drv_path, vec!["test-agent".to_string()])
        .with_updates(vec![
            BuildUpdate {
                derivation_path: drv_path.to_string(),
                update: Some(build_update::Update::Log(LogLine {
                    timestamp: 1234567890,
                    line: "Building...\n".to_string(),
                })),
            },
            BuildUpdate {
                derivation_path: drv_path.to_string(),
                update: Some(build_update::Update::Completed(BuildCompleted {
                    output_paths: vec!["/nix/store/test123-hello".to_string()],
                    duration_ms: 1000,
                })),
            },
        ]);

    // Start mock server
    let (_server, url) = mock_agent::start_mock_server(mock).await?;

    // Connect client
    let mut client = RioClient::connect(&url).await?;

    // Create test build info
    let build_info = BuildInfo {
        drv_path: Utf8PathBuf::from(drv_path),
        drv_nar_bytes: vec![1, 2, 3], // Dummy NAR bytes for test
        platform: "x86_64-linux".to_string(),
        required_features: vec![],
        dependency_paths: vec![],
    };

    // Create mock cluster info
    let mut agents = HashMap::new();
    agents.insert(
        "test-agent".to_string(),
        AgentInfo {
            id: "test-agent".to_string(),
            address: url.clone(),
            platforms: vec!["x86_64-linux".to_string()],
            features: vec![],
            status: 0, // Available
            capacity: None,
        },
    );

    let cluster_info = ClusterInfo {
        leader_id: "test-agent".to_string(),
        leader_address: url.clone(),
        agents,
        discovered_at: std::time::Instant::now(),
    };

    // Submit build
    let mut stream = client.submit_build(build_info, &cluster_info).await?;

    // Verify we get updates
    let mut got_log = false;
    let mut got_completion = false;

    while let Some(update) = stream.message().await.context("stream should not error")? {
        match update.update {
            Some(build_update::Update::Log(_)) => {
                got_log = true;
            }
            Some(build_update::Update::Completed(_)) => {
                got_completion = true;
            }
            _ => {}
        }
    }

    assert!(got_log, "Should receive log message");
    assert!(got_completion, "Should receive completion message");
    Ok(())
}
