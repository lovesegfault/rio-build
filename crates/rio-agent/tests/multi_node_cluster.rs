//! Multi-node cluster integration tests
//!
//! Tests Raft cluster formation, leader election, and distributed coordination.

use anyhow::Result;
use camino::Utf8PathBuf;
use rio_agent::agent::Agent;
use std::time::Duration;
use tokio::time::sleep;

/// Test: Explicit join - agent A bootstraps, agent B explicitly joins A
#[tokio::test]
async fn test_explicit_join_two_nodes() -> Result<()> {
    let _ = tracing_subscriber::fmt::try_init();

    // Create temp directories for agents
    let temp_dir1 = tempfile::tempdir()?;
    let temp_dir2 = tempfile::tempdir()?;
    let data_dir1 = Utf8PathBuf::from_path_buf(temp_dir1.path().to_path_buf()).unwrap();
    let data_dir2 = Utf8PathBuf::from_path_buf(temp_dir2.path().to_path_buf()).unwrap();

    // Use fixed ports for testing
    let listen_addr1 = "127.0.0.1:50771".to_string();
    let listen_addr2 = "127.0.0.1:50772".to_string();

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

    // Agent 2 joins via agent 1 (server starts automatically)
    eprintln!("TEST: Agent 2 joining cluster via {}", listen_addr1);
    let seed_url = format!("http://{}", listen_addr1);
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
