//! Heartbeat and failure detection for Raft cluster
//!
//! Implements the heartbeat system from DESIGN.md Phase 2.5:
//! - Agents send heartbeats every 10 seconds
//! - Failure detection marks agents as Down after 30 seconds (3 missed heartbeats)
//! - Failed agents have their builds cleaned up automatically

use chrono::Utc;
use openraft::Raft;
use rio_common::AgentId;
use std::sync::Arc;
use std::time::Duration;
use tokio::task::JoinHandle;

use crate::state_machine::{AgentStatus, RaftCommand};
use crate::storage::{StateMachineStore, TypeConfig};

/// Start background task that sends heartbeats at the specified interval
///
/// The task runs indefinitely until the agent shuts down.
/// Heartbeat failures are logged but don't stop the task.
///
/// # Arguments
/// * `agent_id` - The agent's unique identifier
/// * `raft` - The Raft instance to send heartbeats to
/// * `interval` - How often to send heartbeats (default: 10 seconds in production)
#[tracing::instrument(skip(raft), fields(agent_id = %agent_id, interval_secs = interval.as_secs()))]
pub fn start_heartbeat_task(
    agent_id: AgentId,
    raft: Arc<Raft<TypeConfig>>,
    interval: Duration,
) -> JoinHandle<()> {
    tracing::info!("Starting heartbeat task");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(interval);

        loop {
            interval.tick().await;

            let cmd = RaftCommand::AgentHeartbeat {
                id: agent_id,
                timestamp: Utc::now(),
            };

            match raft.client_write(cmd).await {
                Ok(_) => {
                    tracing::trace!(agent_id = %agent_id, "Heartbeat sent");
                }
                Err(e) => {
                    tracing::debug!(
                        agent_id = %agent_id,
                        error = %e,
                        "Failed to send heartbeat (may be transient)"
                    );
                }
            }
        }
    })
}

/// Start background task that checks for failed agents at the specified interval
///
/// Marks agents as Down if their heartbeat is older than the timeout.
/// Default production values: check every 15s, timeout after 30s (3 missed heartbeats).
///
/// # Arguments
/// * `raft` - The Raft instance to propose AgentLeft commands
/// * `state_machine` - The state machine to check for stale agents
/// * `check_interval` - How often to check for failed agents (default: 15 seconds)
/// * `timeout` - How old a heartbeat can be before agent is marked Down (default: 30 seconds)
#[tracing::instrument(skip(raft, state_machine), fields(check_interval_secs = check_interval.as_secs(), timeout_secs = timeout.as_secs()))]
pub fn start_failure_detector_task(
    raft: Arc<Raft<TypeConfig>>,
    state_machine: StateMachineStore,
    check_interval: Duration,
    timeout: Duration,
) -> JoinHandle<()> {
    tracing::info!("Starting failure detector task");

    tokio::spawn(async move {
        let mut interval = tokio::time::interval(check_interval);

        loop {
            interval.tick().await;

            let failed_agents = check_failed_agents(&state_machine, timeout);

            for failed_id in failed_agents {
                tracing::warn!(
                    agent_id = %failed_id,
                    "Agent failed heartbeat check - marking as Down and removing from cluster"
                );

                // Use AgentLeft command to remove agent and clean up builds
                let cmd = RaftCommand::AgentLeft { id: failed_id };

                if let Err(e) = raft.client_write(cmd).await {
                    tracing::error!(
                        agent_id = %failed_id,
                        error = %e,
                        "Failed to propose AgentLeft for failed agent"
                    );
                }
            }
        }
    })
}

/// Check for agents with stale heartbeats
///
/// Returns list of agent IDs that should be marked as Down.
///
/// # Arguments
/// * `state_machine` - The state machine to check
/// * `timeout` - How old a heartbeat can be before agent is considered failed
fn check_failed_agents(state_machine: &StateMachineStore, timeout: Duration) -> Vec<AgentId> {
    let now = Utc::now();

    let data = state_machine.data.read();
    let cluster_state = &data.cluster;

    cluster_state
        .agents
        .values()
        .filter(|agent| {
            // Only check agents that are not already Down
            if agent.status == AgentStatus::Down {
                return false;
            }

            // Check if heartbeat is stale
            let age = now
                .signed_duration_since(agent.last_heartbeat)
                .to_std()
                .unwrap_or_default();

            age > timeout
        })
        .map(|agent| agent.id)
        .collect()
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use camino::Utf8Path;

    #[tokio::test(flavor = "multi_thread")]
    async fn test_heartbeat_task_sends_periodic_heartbeats() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let node_id = AgentId::new_v4();
        let agent_id = AgentId::new_v4();
        let rpc_addr = "localhost:50051".to_string();
        let (raft, sm_store) =
            crate::raft_node::create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
                .await?;

        crate::raft_node::initialize_single_node_leader(&raft, node_id, rpc_addr).await?;

        // Wait for node to become leader
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Register agent
        crate::membership::register_agent(
            &raft,
            agent_id,
            "localhost:50051".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        // Start heartbeat task with 1 second interval (much faster for tests)
        let handle = start_heartbeat_task(agent_id, raft.clone(), Duration::from_secs(1));

        // Wait for 2 heartbeats (2+ seconds)
        tokio::time::sleep(Duration::from_millis(2500)).await;

        // Check that heartbeat was updated
        let data = sm_store.data.read();
        let agent = data
            .cluster
            .agents
            .get(&agent_id)
            .context("agent should exist")?;

        // Heartbeat should be recent (< 1.5 seconds old with 1s interval)
        let age = Utc::now().signed_duration_since(agent.last_heartbeat);
        assert!(
            age.num_milliseconds() < 1500,
            "Heartbeat is stale: {}ms",
            age.num_milliseconds()
        );

        handle.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_failure_detector_marks_stale_agents_as_down() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let node_id = AgentId::new_v4();
        let rpc_addr = "localhost:50051".to_string();
        let (raft, sm_store) =
            crate::raft_node::create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
                .await?;

        crate::raft_node::initialize_single_node_leader(&raft, node_id, rpc_addr).await?;

        // Wait for node to become leader
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Register two agents
        let agent1 = AgentId::new_v4();
        let agent2 = AgentId::new_v4();

        crate::membership::register_agent(
            &raft,
            agent1,
            "localhost:50051".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        crate::membership::register_agent(
            &raft,
            agent2,
            "localhost:50052".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        // Start heartbeat for agent1 only (agent2 will go stale)
        // Use 1 second interval for faster test
        let _handle1 = start_heartbeat_task(agent1, raft.clone(), Duration::from_secs(1));

        // Start failure detector with fast settings: check every 0.5s, timeout after 3s
        let detector_handle = start_failure_detector_task(
            raft.clone(),
            sm_store.clone(),
            Duration::from_millis(500),
            Duration::from_secs(3),
        );

        // Wait for agent2 to be detected as failed (3s timeout + 0.5s check interval)
        tokio::time::sleep(Duration::from_secs(4)).await;

        // Agent2 should be removed (AgentLeft was called)
        let data = sm_store.data.read();
        assert!(
            !data.cluster.agents.contains_key(&agent2),
            "Agent2 should be removed"
        );
        assert!(
            data.cluster.agents.contains_key(&agent1),
            "Agent1 should still exist"
        );

        detector_handle.abort();
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_check_failed_agents_empty_on_fresh_cluster() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let (_log_store, sm_store) = crate::storage::new_storage(temp_path).await?;

        let failed = check_failed_agents(&sm_store, Duration::from_secs(30));
        assert!(failed.is_empty(), "Fresh cluster should have no failures");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_raft_write_works_after_bootstrap() -> anyhow::Result<()> {
        // Minimal test to verify Raft can accept writes after bootstrap
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let node_id = AgentId::new_v4();
        let rpc_addr = "localhost:50051".to_string();
        let (raft, _sm_store) =
            crate::raft_node::create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
                .await?;

        crate::raft_node::initialize_single_node_leader(&raft, node_id, rpc_addr).await?;

        // Wait for leader to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify node is leader
        let metrics = raft.metrics().borrow().clone();
        assert_eq!(metrics.current_leader, Some(node_id));

        // Try a simple write
        let test_agent = AgentId::new_v4();
        let cmd = crate::state_machine::RaftCommand::AgentHeartbeat {
            id: test_agent,
            timestamp: Utc::now(),
        };

        let _result = raft.client_write(cmd).await?;
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_check_failed_agents_detects_stale() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let node_id = AgentId::new_v4();
        let rpc_addr = "localhost:50051".to_string();
        let (raft, sm_store) =
            crate::raft_node::create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
                .await?;

        crate::raft_node::initialize_single_node_leader(&raft, node_id, rpc_addr).await?;

        // Wait for node to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        // Verify node is leader
        let metrics = raft.metrics().borrow().clone();
        assert_eq!(
            metrics.current_leader,
            Some(node_id),
            "Node should be leader after initialization"
        );

        let agent1 = AgentId::new_v4();
        let agent2 = AgentId::new_v4();

        // Register agent1 with recent heartbeat
        crate::membership::register_agent(
            &raft,
            agent1,
            "localhost:50051".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        // Register agent2 and manually set stale heartbeat
        crate::membership::register_agent(
            &raft,
            agent2,
            "localhost:50052".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        // Manually update agent2's heartbeat to be stale (40 seconds ago)
        let stale_heartbeat_cmd = crate::state_machine::RaftCommand::AgentHeartbeat {
            id: agent2,
            timestamp: Utc::now() - chrono::Duration::seconds(40),
        };
        raft.client_write(stale_heartbeat_cmd).await?;

        let failed = check_failed_agents(&sm_store, Duration::from_secs(30));
        assert_eq!(failed.len(), 1, "Should detect one failed agent");
        assert_eq!(failed[0], agent2, "Should detect agent2 as failed");
        Ok(())
    }

    #[tokio::test(flavor = "multi_thread")]
    async fn test_check_failed_agents_ignores_already_down() -> anyhow::Result<()> {
        let temp_dir = tempfile::tempdir()?;
        let temp_path =
            Utf8Path::from_path(temp_dir.path()).context("temp dir path should be valid UTF-8")?;

        let node_id = AgentId::new_v4();
        let rpc_addr = "localhost:50051".to_string();
        let (raft, sm_store) =
            crate::raft_node::create_uninitialized_raft(node_id, rpc_addr.clone(), temp_path)
                .await?;

        crate::raft_node::initialize_single_node_leader(&raft, node_id, rpc_addr).await?;

        // Wait for node to be ready
        tokio::time::sleep(Duration::from_millis(100)).await;

        let agent1 = AgentId::new_v4();

        // Register agent1
        crate::membership::register_agent(
            &raft,
            agent1,
            "localhost:50051".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await?;

        // Set stale heartbeat
        let stale_heartbeat_cmd = crate::state_machine::RaftCommand::AgentHeartbeat {
            id: agent1,
            timestamp: Utc::now() - chrono::Duration::seconds(100),
        };
        raft.client_write(stale_heartbeat_cmd).await?;

        // Mark agent as Down (AgentLeft removes it entirely)
        let agent_left_cmd = crate::state_machine::RaftCommand::AgentLeft { id: agent1 };
        raft.client_write(agent_left_cmd).await?;

        // After AgentLeft, the agent is removed entirely
        // check_failed_agents should return empty since the agent is gone
        let failed = check_failed_agents(&sm_store, Duration::from_secs(30));
        assert!(
            failed.is_empty(),
            "Should not detect removed agents as failed"
        );
        Ok(())
    }
}
