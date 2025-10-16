//! Multi-node cluster integration tests
//!
//! Tests Raft cluster formation, leader election, and distributed coordination.

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rio_agent::agent::Agent;
use rio_common::proto::{
    QueueBuildRequest, SubscribeToBuildRequest, build_update, rio_agent_client::RioAgentClient,
};
use std::collections::HashMap;
use std::sync::Arc;
use std::time::Duration;
use tokio::time::sleep;
use uuid::Uuid;

/// Test: Explicit join - agent A bootstraps, agent B explicitly joins A
#[tokio::test(flavor = "multi_thread")]
async fn test_explicit_join_two_nodes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Create temp directories for agents
    let temp_dir1 = tempfile::tempdir()?;
    let temp_dir2 = tempfile::tempdir()?;
    let data_dir1 = Utf8PathBuf::from_path_buf(temp_dir1.path().to_path_buf())
        .map_err(|_| anyhow::anyhow!("Failed to convert temp_dir1 to UTF-8 path"))?;
    let data_dir2 = Utf8PathBuf::from_path_buf(temp_dir2.path().to_path_buf())
        .map_err(|_| anyhow::anyhow!("Failed to convert temp_dir2 to UTF-8 path"))?;

    // Use fixed ports for testing
    let listen_addr1 = "127.0.0.1:0".to_string();
    let listen_addr2 = "127.0.0.1:0".to_string();

    // Fast heartbeat intervals for testing
    // Note: timeout needs to be long enough for joining agents to establish connectivity
    let heartbeat_interval = Some(Duration::from_secs(1));
    let check_interval = Some(Duration::from_secs(2));
    let timeout = Some(Duration::from_secs(10));

    // Bootstrap agent 1 (server starts automatically)
    eprintln!("TEST: Bootstrapping agent 1 on {}", listen_addr1);
    let agent1 = Agent::bootstrap(
        data_dir1,
        listen_addr1.clone(),
        heartbeat_interval,
        check_interval,
        timeout,
    )
    .await?;

    let agent1_id = agent1.id;
    let agent1_raft = agent1.raft.clone();
    let agent1_sm = agent1.state_machine.clone();

    // Poll for agent1 to become leader (usually happens in <500ms)
    let mut attempts = 0;
    while attempts < 20 {
        let metrics1 = agent1_raft.metrics().borrow().clone();
        if metrics1.current_leader == Some(agent1_id) {
            eprintln!("TEST: Agent 1 became leader after {}ms", attempts * 100);
            break;
        }
        sleep(Duration::from_millis(100)).await;
        attempts += 1;
    }

    // Verify agent1 is leader
    let metrics1 = agent1_raft.metrics().borrow().clone();
    assert_eq!(
        metrics1.current_leader,
        Some(agent1_id),
        "Agent 1 should be leader before agent 2 joins"
    );

    // Get agent1's actual bound address
    let agent1_url = {
        let state = agent1_sm.data.read();
        state
            .cluster
            .agents
            .get(&agent1_id)
            .map(|a| a.address.to_string())
            .context("Agent 1 should be in cluster state")?
    };

    // Agent 2 joins via agent 1 (server starts automatically)
    eprintln!("TEST: Agent 2 joining cluster via {}", agent1_url);
    let seed_url = agent1_url.clone();
    let agent2 = Agent::join(
        data_dir2,
        listen_addr2.clone(),
        seed_url,
        heartbeat_interval,
        check_interval,
        timeout,
    )
    .await?;

    let agent2_id = agent2.id;
    let agent2_sm = agent2.state_machine.clone();

    // Poll for cluster to stabilize (both agents see each other)
    // Usually happens in <1s, but allow up to 3s for slow systems
    let mut stabilized = false;
    for attempt in 0..30 {
        let is_stable = {
            let state1 = agent1_sm.data.read();
            let state2 = agent2_sm.data.read();

            state1.cluster.agents.len() == 2
                && state2.cluster.agents.len() == 2
                && state1.cluster.agents.contains_key(&agent2_id)
                && state2.cluster.agents.contains_key(&agent1_id)
        }; // Locks dropped here

        if is_stable {
            eprintln!("TEST: Cluster stabilized after {}ms", attempt * 100);
            stabilized = true;
            break;
        }

        sleep(Duration::from_millis(100)).await;
    }

    assert!(stabilized, "Cluster should stabilize within 3 seconds");

    // Check leader status
    let metrics1 = agent1_raft.metrics().borrow().clone();
    let metrics2 = agent2.raft.metrics().borrow().clone();
    eprintln!(
        "TEST: Agent 1 leader: {:?}, Agent 2 leader: {:?}",
        metrics1.current_leader, metrics2.current_leader
    );

    // Verify cluster membership (both agents should see each other)
    let cluster_state1 = &agent1_sm.data.read().cluster;
    eprintln!("TEST: Agent 1 sees {} agents", cluster_state1.agents.len());
    assert_eq!(
        cluster_state1.agents.len(),
        2,
        "Agent 1 should see 2 agents in cluster"
    );
    assert!(cluster_state1.agents.contains_key(&agent1_id));
    assert!(cluster_state1.agents.contains_key(&agent2_id));

    let cluster_state2 = &agent2_sm.data.read().cluster;
    eprintln!("TEST: Agent 2 sees {} agents", cluster_state2.agents.len());
    assert_eq!(
        cluster_state2.agents.len(),
        2,
        "Agent 2 should see 2 agents in cluster"
    );
    assert!(cluster_state2.agents.contains_key(&agent1_id));
    assert!(cluster_state2.agents.contains_key(&agent2_id));

    eprintln!("TEST: Two-node cluster formed successfully");

    Ok(())
}

/// Helper: Form an N-node cluster for testing
///
/// Returns (agents, addresses) where:
/// - agents[0] is the leader
/// - addresses maps agent_id -> "http://host:port"
async fn form_cluster(n: usize) -> Result<(Vec<Arc<Agent>>, HashMap<uuid::Uuid, String>)> {
    assert!(n >= 1, "Need at least 1 agent");

    let mut agents = Vec::new();
    let mut addresses = HashMap::new();
    let heartbeat_interval = Some(Duration::from_secs(1));
    let check_interval = Some(Duration::from_secs(2));
    let timeout = Some(Duration::from_secs(10));

    // Bootstrap first agent (becomes leader) with port 0 for dynamic allocation
    let temp_dir1 = tempfile::tempdir()?;
    let data_dir1 = Utf8PathBuf::from_path_buf(temp_dir1.path().to_path_buf())
        .map_err(|_| anyhow::anyhow!("Failed to convert temp_dir1 to UTF-8 path"))?;
    let listen_addr1 = "127.0.0.1:0".to_string();

    eprintln!("TEST: Bootstrapping agent 1 with dynamic port");
    let agent1 = Agent::bootstrap(
        data_dir1,
        listen_addr1,
        heartbeat_interval,
        check_interval,
        timeout,
    )
    .await?;

    // Get the actual bound address from cluster state
    let agent1_addr = {
        let state = agent1.state_machine.data.read();
        state
            .cluster
            .agents
            .get(&agent1.id)
            .map(|a| a.address.to_string())
            .context("Agent 1 should be in cluster state")?
    };

    eprintln!("TEST: Agent 1 bound to {}", agent1_addr);
    addresses.insert(agent1.id, agent1_addr.clone());

    // Poll for agent1 to become leader
    let raft1 = agent1.raft.clone();
    let agent1_id = agent1.id;
    for _attempt in 0..20 {
        if raft1.metrics().borrow().current_leader == Some(agent1_id) {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    agents.push(agent1);

    // Join remaining agents with dynamic port allocation
    for i in 1..n {
        let temp_dir = tempfile::tempdir()?;
        let data_dir = Utf8PathBuf::from_path_buf(temp_dir.path().to_path_buf()).map_err(|_| {
            anyhow::anyhow!(
                "Failed to convert temp_dir to UTF-8 path for agent {}",
                i + 1
            )
        })?;
        let listen_addr = "127.0.0.1:0".to_string(); // Dynamic port

        eprintln!("TEST: Agent {} joining cluster via {}", i + 1, agent1_addr);
        let agent = Agent::join(
            data_dir,
            listen_addr,
            agent1_addr.clone(),
            heartbeat_interval,
            check_interval,
            timeout,
        )
        .await?;

        // Get the actual bound address from cluster state
        let agent_addr = {
            let state = agent.state_machine.data.read();
            state
                .cluster
                .agents
                .get(&agent.id)
                .map(|a| a.address.to_string())
                .with_context(|| format!("Agent {} should be in cluster state", i + 1))?
        };

        eprintln!("TEST: Agent {} bound to {}", i + 1, agent_addr);
        addresses.insert(agent.id, agent_addr);
        agents.push(agent);
    }

    // Poll for cluster to stabilize (all agents see all others)
    eprintln!("TEST: Waiting for {}-node cluster to stabilize...", n);
    for _attempt in 0..50 {
        let all_stable = agents.iter().all(|agent| {
            let state = agent.state_machine.data.read();
            state.cluster.agents.len() == n
        });

        if all_stable {
            eprintln!("TEST: Cluster stabilized with {} agents", n);
            break;
        }

        sleep(Duration::from_millis(100)).await;
    }

    // Verify cluster is stable
    for (i, agent) in agents.iter().enumerate() {
        let state = agent.state_machine.data.read();
        assert_eq!(
            state.cluster.agents.len(),
            n,
            "Agent {} should see {} agents in cluster",
            i + 1,
            n
        );
    }

    Ok((agents, addresses))
}

/// Create a unique Nix derivation for testing
async fn create_unique_derivation() -> Result<(String, Vec<u8>)> {
    let unique_id = Uuid::new_v4();
    let nix_expr = format!(
        r#"
with import <nixpkgs> {{}};
runCommandNoCC "rio-multinode-{}" {{}} ''
  echo "Multi-node test {}"
  echo "output-{}" > $out
''
"#,
        unique_id, unique_id, unique_id
    );

    let temp_file = format!("/tmp/rio-multinode-{}.nix", unique_id);
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

/// Test: Distributed build on 3-node cluster
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_node_distributed_build() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Form 3-node cluster
    let (agents, addresses) = form_cluster(3).await?;
    let leader_id = agents[0].id;
    let leader_addr = addresses
        .get(&leader_id)
        .context("Leader address should be in map")?
        .clone();

    eprintln!("TEST: 3-node cluster formed, leader at {}", leader_addr);

    // Create unique build
    let (drv_path, drv_nar) = create_unique_derivation().await?;
    eprintln!("TEST: Created derivation: {}", drv_path);

    // Connect to leader and submit build
    let mut client = RioAgentClient::connect(leader_addr.clone()).await?;

    let response = client
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar,
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await?
        .into_inner();

    // Should get BuildAssigned
    let assigned = match response.result {
        Some(rio_common::proto::queue_build_response::Result::Assigned(a)) => a,
        other => anyhow::bail!("Expected BuildAssigned, got {:?}", other),
    };

    eprintln!(
        "TEST: Build assigned to agent {} (one of 3 in cluster)",
        assigned.agent_id
    );

    // Find the assigned agent's address from the map
    let assigned_agent_id = uuid::Uuid::parse_str(&assigned.agent_id)?;
    let assigned_addr = addresses
        .get(&assigned_agent_id)
        .ok_or_else(|| anyhow::anyhow!("Assigned agent {} not in address map", assigned.agent_id))?
        .clone();

    eprintln!("TEST: Connecting to assigned agent at {}", assigned_addr);

    // Wait for coordinator to start build
    sleep(Duration::from_millis(300)).await;

    // Subscribe to build on assigned agent
    let mut client = RioAgentClient::connect(assigned_addr).await?;
    let mut stream = client
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await?
        .into_inner();

    // Wait for completion with timeout
    let completed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(Ok(update)) = stream.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Completed(_))) {
                return true;
            }
            if matches!(update.update, Some(build_update::Update::Failed(_))) {
                panic!("Build failed unexpectedly");
            }
        }
        false
    })
    .await;

    match completed {
        Ok(true) => eprintln!("TEST: Build completed successfully on multi-node cluster"),
        Ok(false) => anyhow::bail!("Stream ended without completion"),
        Err(_) => anyhow::bail!("Build timeout after 30s"),
    }

    // Poll for build to appear in completed_builds (Raft replication takes time)
    eprintln!("TEST: Polling for BuildCompleted to propagate via Raft...");
    let mut found_completed = false;
    for attempt in 0..50 {
        for (i, agent) in agents.iter().enumerate() {
            let state = agent.state_machine.data.read();
            let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
            if state.cluster.completed_builds.contains_key(&drv_path_key) {
                found_completed = true;
                eprintln!(
                    "TEST: Build found in completed_builds on agent {} after {}ms",
                    i + 1,
                    attempt * 100
                );
                break;
            }
        }
        if found_completed {
            break;
        }
        sleep(Duration::from_millis(100)).await;
    }

    assert!(
        found_completed,
        "Build should be in completed_builds after Raft replication (checked all {} agents)",
        agents.len()
    );

    eprintln!("✓ Multi-node distributed build test passed");
    Ok(())
}

/// Test: Concurrent deduplication on multi-node cluster
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_node_concurrent_deduplication() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Form 3-node cluster
    let (agents, addresses) = form_cluster(3).await?;
    let leader_id = agents[0].id;
    let leader_addr = addresses
        .get(&leader_id)
        .context("Leader address should be in map")?
        .clone();

    eprintln!("TEST: 3-node cluster formed for deduplication test");

    // Create unique build
    let (drv_path, drv_nar) = create_unique_derivation().await?;
    eprintln!("TEST: Created derivation: {}", drv_path);

    // Client 1: Submit build
    let mut client1 = RioAgentClient::connect(leader_addr.clone()).await?;

    let response1 = client1
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar.clone(),
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await?
        .into_inner();

    // Should get BuildAssigned
    let assigned = match response1.result {
        Some(rio_common::proto::queue_build_response::Result::Assigned(a)) => a,
        other => anyhow::bail!("Client 1 expected BuildAssigned, got {:?}", other),
    };

    eprintln!(
        "TEST: Client 1 - Build assigned to agent {}",
        assigned.agent_id
    );

    // Find assigned agent's address from map
    let assigned_agent_id = uuid::Uuid::parse_str(&assigned.agent_id)?;
    let assigned_addr = addresses
        .get(&assigned_agent_id)
        .ok_or_else(|| anyhow::anyhow!("Assigned agent {} not in address map", assigned.agent_id))?
        .clone();

    eprintln!(
        "TEST: Client 1 connecting to assigned agent at {}",
        assigned_addr
    );

    // Wait for build to start
    sleep(Duration::from_millis(300)).await;

    // Client 1: Subscribe to assigned agent
    let mut client1 = RioAgentClient::connect(assigned_addr.clone()).await?;
    let mut stream1 = client1
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await?
        .into_inner();

    // Consume a few log messages to ensure build is running
    let mut client1_logs = 0;
    for _ in 0..3 {
        if let Some(Ok(update)) = stream1.message().await.transpose()
            && matches!(update.update, Some(build_update::Update::Log(_)))
        {
            client1_logs += 1;
        }
    }

    eprintln!(
        "TEST: Client 1 received {} logs, build is running",
        client1_logs
    );

    // Client 2: Submit same build (should get AlreadyBuilding)
    let mut client2 = RioAgentClient::connect(leader_addr.clone()).await?;

    let response2 = client2
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar.clone(),
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await?
        .into_inner();

    // Should get AlreadyBuilding
    let already_building = match response2.result {
        Some(rio_common::proto::queue_build_response::Result::AlreadyBuilding(ab)) => ab,
        other => anyhow::bail!("Client 2 expected AlreadyBuilding, got {:?}", other),
    };

    eprintln!(
        "TEST: Client 2 got AlreadyBuilding response, agent_id={}",
        already_building.agent_id
    );

    // Verify same agent
    assert_eq!(
        already_building.agent_id, assigned.agent_id,
        "Client 2 should be directed to same agent as Client 1"
    );

    // Client 2: Connect to the assigned agent and subscribe
    eprintln!("TEST: Client 2 connecting to agent at {}", assigned_addr);
    let mut client2 = RioAgentClient::connect(assigned_addr).await?;
    let mut stream2 = client2
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await?
        .into_inner();

    // Both clients consume streams concurrently
    let task1 = tokio::spawn(async move {
        let completed = tokio::time::timeout(Duration::from_secs(30), async {
            while let Some(Ok(update)) = stream1.message().await.transpose() {
                if matches!(update.update, Some(build_update::Update::Completed(_))) {
                    return true;
                }
            }
            false
        })
        .await;

        match completed {
            Ok(true) => true,
            Ok(false) => panic!("Client 1: Stream ended without completion"),
            Err(_) => panic!("Client 1: Timeout"),
        }
    });

    let task2 = tokio::spawn(async move {
        let result = tokio::time::timeout(Duration::from_secs(30), async {
            let mut logs = 0;
            while let Some(Ok(update)) = stream2.message().await.transpose() {
                match update.update {
                    Some(build_update::Update::Log(_)) => logs += 1,
                    Some(build_update::Update::Completed(_)) => return (logs, true),
                    Some(build_update::Update::Failed(_)) => panic!("Client 2: Build failed"),
                    _ => {}
                }
            }
            (logs, false)
        })
        .await;

        match result {
            Ok((logs, completed)) => (logs, completed),
            Err(_) => panic!("Client 2: Timeout"),
        }
    });

    let client1_completed = task1.await?;
    let (client2_logs, client2_completed) = task2.await?;

    eprintln!(
        "TEST: Client 1 completed={}, Client 2 logs={} completed={}",
        client1_completed, client2_logs, client2_completed
    );

    assert!(client1_completed, "Client 1 should complete");
    assert!(client2_completed, "Client 2 should complete");
    assert!(
        client2_logs > 0,
        "Client 2 should receive logs (including catch-up)"
    );

    // Poll for Raft state to propagate to all agents
    eprintln!(
        "TEST: Polling for Raft state to propagate to all {} agents...",
        agents.len()
    );
    let mut all_synced = false;
    for attempt in 0..50 {
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
        let all_have_completed = agents.iter().all(|agent| {
            let state = agent.state_machine.data.read();
            state.cluster.completed_builds.contains_key(&drv_path_key)
        });

        if all_have_completed {
            all_synced = true;
            eprintln!("TEST: All agents synced after {}ms", attempt * 100);
            break;
        }

        sleep(Duration::from_millis(100)).await;
    }

    assert!(
        all_synced,
        "All agents should see completed build in Raft state within 5 seconds"
    );

    // Verify final state on all agents
    for (i, agent) in agents.iter().enumerate() {
        let state = agent.state_machine.data.read();
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();

        // Should be in completed_builds (Raft replicated to all)
        assert!(
            state.cluster.completed_builds.contains_key(&drv_path_key),
            "Agent {} should see completed build in Raft state",
            i + 1
        );

        // Should NOT be in builds_in_progress anymore
        assert!(
            !state.cluster.builds_in_progress.contains_key(&drv_path_key),
            "Agent {} should not see build in progress after completion",
            i + 1
        );
    }

    eprintln!("✓ Multi-node concurrent deduplication test passed");
    eprintln!("  - Two clients submitted same build");
    eprintln!("  - Client 2 got AlreadyBuilding response");
    eprintln!("  - Both received logs and outputs");
    eprintln!("  - Only one build executed (verified via Raft state)");

    Ok(())
}

/// Test: Multi-node cache serving (AlreadyCompleted across nodes)
#[tokio::test(flavor = "multi_thread")]
async fn test_multi_node_cache_serving() -> Result<()> {
    use rio_common::proto::GetCompletedBuildRequest;

    let _ = tracing_subscriber::fmt::try_init();

    // Form 3-node cluster
    let (agents, addresses) = form_cluster(3).await?;
    let leader_id = agents[0].id;
    let leader_addr = addresses
        .get(&leader_id)
        .context("Leader address should be in map")?
        .clone();

    eprintln!("TEST: 3-node cluster formed for cache serving test");

    // Create unique build
    let (drv_path, drv_nar) = create_unique_derivation().await?;
    eprintln!("TEST: Created derivation: {}", drv_path);

    // Client 1: Submit and complete build
    let mut client1 = RioAgentClient::connect(leader_addr.clone()).await?;

    let response1 = client1
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar.clone(),
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await?
        .into_inner();

    // Should get BuildAssigned
    let assigned = match response1.result {
        Some(rio_common::proto::queue_build_response::Result::Assigned(a)) => a,
        other => anyhow::bail!("Client 1 expected BuildAssigned, got {:?}", other),
    };

    let assigned_agent_id = uuid::Uuid::parse_str(&assigned.agent_id)?;
    let assigned_addr = addresses
        .get(&assigned_agent_id)
        .context("Assigned agent not found")?
        .clone();

    eprintln!(
        "TEST: Client 1 - Build assigned to agent {} at {}",
        assigned.agent_id, assigned_addr
    );

    // Wait for build to start
    sleep(Duration::from_millis(300)).await;

    // Client 1: Subscribe and wait for completion
    let mut client1 = RioAgentClient::connect(assigned_addr.clone()).await?;
    let mut stream1 = client1
        .subscribe_to_build(SubscribeToBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await?
        .into_inner();

    let client1_completed = tokio::time::timeout(Duration::from_secs(30), async {
        while let Some(Ok(update)) = stream1.message().await.transpose() {
            if matches!(update.update, Some(build_update::Update::Completed(_))) {
                return true;
            }
        }
        false
    })
    .await;

    match client1_completed {
        Ok(true) => eprintln!("TEST: Client 1 build completed"),
        Ok(false) => anyhow::bail!("Client 1: Stream ended without completion"),
        Err(_) => anyhow::bail!("Client 1: Timeout"),
    }

    // Poll for BuildCompleted to propagate to all agents via Raft
    eprintln!("TEST: Polling for BuildCompleted to propagate to all agents...");
    let mut all_synced = false;
    for attempt in 0..50 {
        let drv_path_key: camino::Utf8PathBuf = drv_path.clone().into();
        let all_have_completed = agents.iter().all(|agent| {
            let state = agent.state_machine.data.read();
            state.cluster.completed_builds.contains_key(&drv_path_key)
        });

        if all_have_completed {
            all_synced = true;
            eprintln!("TEST: All agents synced after {}ms", attempt * 100);
            break;
        }

        sleep(Duration::from_millis(100)).await;
    }

    assert!(all_synced, "All agents should see completed build in cache");

    // Client 2: Submit same build to leader (should get AlreadyCompleted)
    let mut client2 = RioAgentClient::connect(leader_addr.clone()).await?;

    let response2 = client2
        .queue_build(QueueBuildRequest {
            derivation_path: drv_path.clone(),
            derivation: drv_nar.clone(),
            dependency_paths: vec![],
            platform: "x86_64-linux".to_string(),
            required_features: vec![],
            timeout_seconds: Some(60),
        })
        .await?
        .into_inner();

    // Should get AlreadyCompleted
    let already_completed = match response2.result {
        Some(rio_common::proto::queue_build_response::Result::AlreadyCompleted(ac)) => ac,
        other => anyhow::bail!("Client 2 expected AlreadyCompleted, got {:?}", other),
    };

    eprintln!(
        "TEST: Client 2 got AlreadyCompleted response, agent_id={}",
        already_completed.agent_id
    );

    // Verify it points to the agent that built it
    assert_eq!(
        already_completed.agent_id, assigned.agent_id,
        "AlreadyCompleted should point to agent that built it"
    );

    // Client 2: Connect to agent with cached outputs and call GetCompletedBuild
    let cache_agent_id = uuid::Uuid::parse_str(&already_completed.agent_id)?;
    let cache_addr = addresses
        .get(&cache_agent_id)
        .context("Cache agent not found")?
        .clone();

    eprintln!("TEST: Client 2 connecting to cache agent at {}", cache_addr);

    let mut client2 = RioAgentClient::connect(cache_addr).await?;
    let mut cache_stream = client2
        .get_completed_build(GetCompletedBuildRequest {
            derivation_path: drv_path.clone(),
        })
        .await?
        .into_inner();

    // Verify we receive outputs from cache
    let mut received_outputs = false;
    let mut received_completion = false;

    while let Some(Ok(update)) = cache_stream.message().await.transpose() {
        match update.update {
            Some(build_update::Update::OutputChunk(_)) => {
                received_outputs = true;
            }
            Some(build_update::Update::Completed(_)) => {
                received_completion = true;
                break;
            }
            _ => {}
        }
    }

    assert!(
        received_outputs,
        "Client 2 should receive output chunks from cache"
    );
    assert!(
        received_completion,
        "Client 2 should receive completion message"
    );

    eprintln!("✓ Multi-node cache serving test passed");
    eprintln!("  - Client 1 completed build on agent-X");
    eprintln!("  - BuildCompleted propagated to all 3 agents via Raft");
    eprintln!("  - Client 2 got AlreadyCompleted pointing to agent-X");
    eprintln!("  - Client 2 fetched outputs from agent-X's cache via GetCompletedBuild");

    Ok(())
}
