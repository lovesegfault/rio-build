//! Build coordinator - claims queued builds and starts execution
//!
//! Refactored to use claiming model: agents actively claim queued builds
//! based on affinity scores and suggested agent lists.

use openraft::Raft;
use rio_common::{AgentId, DerivationPath};
use std::collections::HashSet;
use std::sync::Arc;
use std::time::Duration;
use tokio::sync::Mutex;
use tokio::task::JoinHandle;

use crate::agent::BuildJob;
use crate::builder;
use crate::state_machine::{BuildStatus, RaftCommand, RaftResponse};
use crate::storage::{StateMachineStore, TypeConfig};

/// Start build coordinator task
///
/// Watches Raft state for queued builds and claims them based on affinity.
/// Runs indefinitely until agent shuts down.
#[tracing::instrument(skip(current_build, raft, sm_store), fields(agent_id = %agent_id))]
pub fn start_build_coordinator(
    agent_id: AgentId,
    current_build: Arc<Mutex<Option<BuildJob>>>,
    raft: Arc<Raft<TypeConfig>>,
    sm_store: StateMachineStore,
) -> JoinHandle<()> {
    tracing::info!("Starting build coordinator (claiming mode)");

    tokio::spawn(async move {
        // Track builds we've claimed/started to avoid duplicates
        let claimed_builds: Arc<Mutex<HashSet<DerivationPath>>> =
            Arc::new(Mutex::new(HashSet::new()));

        let mut check_interval = tokio::time::interval(Duration::from_millis(100));

        loop {
            check_interval.tick().await;

            // Check if we're currently busy
            let is_busy = current_build.lock().await.is_some();

            // If we're free, check for Claimed builds to start
            if !is_busy {
                let builds_to_start = {
                    let cluster_state = sm_store.data.read();

                    cluster_state
                        .cluster
                        .builds_in_progress
                        .iter()
                        .filter_map(|(drv_path, tracker)| {
                            // Find builds claimed by us that are ready to start
                            if tracker.status == BuildStatus::Claimed
                                && tracker.agent_id == Some(agent_id)
                            {
                                Some(drv_path.clone())
                            } else {
                                None
                            }
                        })
                        .collect::<Vec<_>>()
                };

                // Start one claimed build (if any)
                if let Some(drv_path) = builds_to_start.into_iter().next() {
                    tracing::info!("Starting claimed build {} (agent now free)", drv_path);

                    // Get derivation NAR from Raft storage
                    let drv_nar = {
                        let cluster_state = &sm_store.data.read().cluster;
                        cluster_state.pending_derivations.get(&drv_path).cloned()
                    };

                    if let Some(nar_bytes) = drv_nar {
                        // Start the build
                        if let Err(e) = builder::start_build(
                            &current_build,
                            raft.clone(),
                            sm_store.clone(),
                            drv_path.to_string(),
                            nar_bytes,
                        )
                        .await
                        {
                            tracing::error!("Failed to start claimed build {}: {}", drv_path, e);
                            // TODO: Propose BuildFailed to Raft
                        } else {
                            // Propose BuildStarted (with leader forwarding)
                            if let Err(e) =
                                forward_build_started(&raft, &sm_store, drv_path.clone()).await
                            {
                                tracing::warn!(
                                    "Failed to propose BuildStarted for {}: {}",
                                    drv_path,
                                    e
                                );
                            }
                        }
                    }
                }
            }

            // Find queued builds that we could claim (can claim even if busy for affinity)
            let claimable_builds = {
                let claimed = claimed_builds.lock().await;
                let cluster_state = sm_store.data.read();

                cluster_state
                    .cluster
                    .builds_in_progress
                    .iter()
                    .filter_map(|(drv_path, tracker)| {
                        // Only consider Queued builds we haven't tried to claim
                        if tracker.status != BuildStatus::Queued || claimed.contains(drv_path) {
                            return None;
                        }

                        // Calculate our affinity score
                        let our_score = calculate_affinity(
                            agent_id,
                            &tracker.parent_build,
                            &cluster_state.cluster,
                        );

                        // Check if we should try to claim this build
                        let elapsed = chrono::Utc::now().signed_duration_since(tracker.started_at);
                        let timeout_passed = elapsed.num_seconds() >= 2;

                        let should_claim = if tracker.suggested_agents.contains(&agent_id) {
                            // We're suggested - claim if we have affinity or after brief delay
                            our_score > 0 || elapsed.num_milliseconds() > 200
                        } else if timeout_passed {
                            // Timeout passed - any agent with affinity can claim
                            our_score > 0
                        } else {
                            // Not suggested and timeout not passed - skip
                            false
                        };

                        if should_claim {
                            Some((drv_path.clone(), our_score))
                        } else {
                            None
                        }
                    })
                    .collect::<Vec<_>>()
            };

            // Try to claim builds (highest affinity first)
            for (drv_path, affinity_score) in claimable_builds {
                tracing::info!(
                    "Attempting to claim build {} (affinity score: {})",
                    drv_path,
                    affinity_score
                );

                // Mark as claimed to avoid retrying
                claimed_builds.lock().await.insert(drv_path.clone());

                // Try to claim via Raft (with leader forwarding)
                match claim_build_via_leader(
                    &raft,
                    &sm_store,
                    drv_path.clone(),
                    agent_id,
                    affinity_score,
                )
                .await
                {
                    Ok(ClaimResult::Claimed) => {
                        tracing::info!("Successfully claimed build {}", drv_path);

                        // Get derivation NAR from Raft storage
                        let drv_nar = {
                            let cluster_state = &sm_store.data.read().cluster;
                            cluster_state.pending_derivations.get(&drv_path).cloned()
                        };

                        if let Some(nar_bytes) = drv_nar {
                            // Start the build
                            if let Err(e) = builder::start_build(
                                &current_build,
                                raft.clone(),
                                sm_store.clone(),
                                drv_path.to_string(),
                                nar_bytes,
                            )
                            .await
                            {
                                tracing::error!("Failed to start build {}: {}", drv_path, e);
                                // TODO: Propose BuildFailed to Raft
                                claimed_builds.lock().await.remove(&drv_path);
                            } else {
                                // Propose BuildStarted (with leader forwarding)
                                if let Err(e) =
                                    forward_build_started(&raft, &sm_store, drv_path.clone()).await
                                {
                                    tracing::warn!(
                                        "Failed to propose BuildStarted for {}: {}",
                                        drv_path,
                                        e
                                    );
                                }
                            }
                        } else {
                            tracing::error!(
                                "Build {} claimed but no derivation NAR in storage",
                                drv_path
                            );
                            claimed_builds.lock().await.remove(&drv_path);
                        }

                        // Only claim one build per iteration
                        break;
                    }
                    Ok(ClaimResult::Rejected(reason)) => {
                        tracing::debug!("Claim rejected for build {}: {}", drv_path, reason);
                        // Another agent claimed it or timeout not passed - that's fine
                    }
                    Err(e) => {
                        tracing::error!("Failed to claim build {}: {}", drv_path, e);
                        claimed_builds.lock().await.remove(&drv_path);
                    }
                }
            }
        }
    })
}

/// Calculate affinity score for this agent
///
/// Counts how many dependencies are building/completed on this agent
fn calculate_affinity(
    agent_id: AgentId,
    parent_build: &Option<DerivationPath>,
    cluster: &crate::state_machine::ClusterState,
) -> usize {
    // For now, simple affinity based on parent build location
    // TODO: Full dependency analysis when dependencies are tracked

    let mut score = 0;

    // If this build has a parent, check if parent is on us
    if let Some(parent_path) = parent_build {
        if let Some(parent_tracker) = cluster.builds_in_progress.get(parent_path)
            && parent_tracker.agent_id == Some(agent_id)
        {
            score += 1;
        }
        if let Some(parent_completed) = cluster.completed_builds.get(parent_path)
            && parent_completed.agent_id == agent_id
        {
            score += 1;
        }
    }

    score
}

/// Result of a claim attempt
enum ClaimResult {
    Claimed,
    Rejected(String),
}

/// Forward BuildStarted to leader if necessary
async fn forward_build_started(
    raft: &Arc<Raft<TypeConfig>>,
    sm_store: &StateMachineStore,
    drv_path: DerivationPath,
) -> anyhow::Result<()> {
    use anyhow::Context;
    use rio_common::proto::rio_agent_client::RioAgentClient;
    use rio_common::proto::{BuildStartedReport, ReportBuildResultRequest};

    // Try direct Raft proposal first
    let cmd = RaftCommand::BuildStarted {
        derivation_path: drv_path.clone(),
    };

    match raft.client_write(cmd).await {
        Ok(_) => {
            tracing::debug!("BuildStarted committed to Raft (this agent is leader)");
            Ok(())
        }
        Err(e) => {
            let error_msg = e.to_string();
            if !error_msg.contains("has to forward request to") {
                anyhow::bail!("Raft proposal failed: {}", e);
            }

            // We're not the leader - forward via RPC
            tracing::debug!("Not leader, forwarding BuildStarted to leader");

            // Get leader info
            let leader_id = raft
                .metrics()
                .borrow()
                .current_leader
                .context("No current leader")?;

            let leader_addr = {
                let state = sm_store.data.read();
                state
                    .cluster
                    .agents
                    .get(&leader_id)
                    .map(|info| info.address.clone())
                    .context("Leader not found in cluster state")?
            };

            // Connect and forward
            let leader_url = leader_addr.to_string();
            let mut client = RioAgentClient::connect(leader_url)
                .await
                .context("Failed to connect to leader")?;

            client
                .report_build_result(ReportBuildResultRequest {
                    derivation_path: drv_path.to_string(),
                    result: Some(
                        rio_common::proto::report_build_result_request::Result::Started(
                            BuildStartedReport {},
                        ),
                    ),
                })
                .await
                .context("ReportBuildResult RPC failed")?;

            Ok(())
        }
    }
}

/// Claim a build, forwarding to leader if necessary
///
/// Try to propose BuildClaimed directly. If we're not the leader, forward to leader via RPC.
async fn claim_build_via_leader(
    raft: &Arc<Raft<TypeConfig>>,
    sm_store: &StateMachineStore,
    drv_path: DerivationPath,
    agent_id: AgentId,
    affinity_score: usize,
) -> anyhow::Result<ClaimResult> {
    use anyhow::Context;
    use rio_common::proto::ClaimBuildRequest;
    use rio_common::proto::rio_agent_client::RioAgentClient;

    // Try direct Raft proposal first (works if we're the leader)
    let claim_cmd = RaftCommand::BuildClaimed {
        derivation_path: drv_path.clone(),
        agent_id,
        affinity_score,
    };

    match raft.client_write(claim_cmd).await {
        Ok(response) => match response.data {
            RaftResponse::BuildClaimedAck { .. } => Ok(ClaimResult::Claimed),
            RaftResponse::BuildClaimRejected { reason, .. } => Ok(ClaimResult::Rejected(reason)),
            _ => anyhow::bail!("Unexpected response to BuildClaimed"),
        },
        Err(e) => {
            let error_msg = e.to_string();
            if !error_msg.contains("has to forward request to") {
                // Some other error
                anyhow::bail!("Raft proposal failed: {}", e);
            }

            // We're not the leader - find leader and forward via RPC
            tracing::debug!("Not leader, forwarding claim to leader");

            // Get leader ID from Raft metrics
            let leader_id = raft
                .metrics()
                .borrow()
                .current_leader
                .context("No current leader")?;

            // Get leader address from cluster state
            let leader_addr = {
                let state = sm_store.data.read();
                state
                    .cluster
                    .agents
                    .get(&leader_id)
                    .map(|info| info.address.clone())
                    .context("Leader not found in cluster state")?
            };

            // Connect to leader (Url already has scheme)
            let leader_url = leader_addr.to_string();

            let mut client = RioAgentClient::connect(leader_url)
                .await
                .context("Failed to connect to leader")?;

            // Call ClaimBuild RPC
            let response = client
                .claim_build(ClaimBuildRequest {
                    derivation_path: drv_path.to_string(),
                    agent_id: agent_id.to_string(),
                    affinity_score: affinity_score as u32,
                })
                .await
                .context("ClaimBuild RPC failed")?
                .into_inner();

            if response.success {
                Ok(ClaimResult::Claimed)
            } else {
                Ok(ClaimResult::Rejected(response.message))
            }
        }
    }
}
