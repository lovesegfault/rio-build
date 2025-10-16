//! Raft state machine for Rio cluster coordination
//!
//! Implements the cluster state and command processing logic from DESIGN.md Section 1.

use chrono::{DateTime, Utc};
use rio_common::types::{AgentId, DerivationPath};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;
use url::Url;

/// Node information for Raft (will be enhanced in Phase 2.4)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Node {
    pub rpc_addr: String,
}

/// Agent information stored in cluster state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub id: AgentId,
    pub address: Url, // gRPC endpoint (e.g., "http://agent1:50051")
    pub platforms: Vec<String>,
    pub features: Vec<String>,
    pub status: AgentStatus,
    pub last_heartbeat: DateTime<Utc>,
}

/// Agent status enumeration
#[derive(Debug, Clone, Copy, Serialize, Deserialize, PartialEq, Eq)]
pub enum AgentStatus {
    Available, // Idle, can accept builds
    Busy,      // Currently executing one build
    Down,      // Failed heartbeats
}

/// Build tracker - tracks which agent is building a derivation
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct BuildTracker {
    pub agent_id: Option<AgentId>, // None when queued, Some when claimed
    pub suggested_agents: Vec<AgentId>, // Ranked by affinity (best first)
    pub started_at: DateTime<Utc>,
    pub parent_build: Option<DerivationPath>, // None = top-level, Some = dependency
    pub status: BuildStatus,
}

/// Build status enumeration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BuildStatus {
    Queued,   // Waiting for agent to claim
    Claimed,  // Agent claimed, preparing to start
    Building, // Currently executing on agent
}

/// Completed build information
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct CompletedBuild {
    pub agent_id: AgentId,
    pub output_paths: Vec<DerivationPath>,
    pub completed_at: DateTime<Utc>,
}

/// Cluster state maintained by Raft consensus
#[derive(Debug, Clone, Serialize, Deserialize, Default)]
pub struct ClusterState {
    /// Cluster membership: agent_id → agent info
    pub agents: HashMap<AgentId, AgentInfo>,

    /// Active builds: derivation_path → which agent is building it
    pub builds_in_progress: HashMap<DerivationPath, BuildTracker>,

    /// Recently completed builds (will add LRU eviction in Phase 3)
    /// For now, just a HashMap (5 minute TTL will be enforced by periodic cleanup)
    pub completed_builds: HashMap<DerivationPath, CompletedBuild>,

    /// Pending derivations: NAR bytes cached while build is active
    /// Removed on BuildCompleted/BuildFailed, enables queueing and survives leader election
    pub pending_derivations: HashMap<DerivationPath, Vec<u8>>,
}

/// Raft commands for cluster coordination
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftCommand {
    // Membership commands
    AgentJoined {
        id: AgentId,
        info: AgentInfo,
    },
    AgentLeft {
        id: AgentId,
    },
    AgentHeartbeat {
        id: AgentId,
        timestamp: DateTime<Utc>,
    },

    // Build lifecycle commands
    BuildQueued {
        top_level: DerivationPath,
        derivation_nar: Vec<u8>, // NAR bytes, replicated to all agents
        dependencies: Vec<DerivationPath>,
        platform: String,
        features: Vec<String>,
    },
    BuildClaimed {
        derivation_path: DerivationPath,
        agent_id: AgentId,
        affinity_score: usize, // Agent's self-calculated affinity
    },
    BuildStarted {
        derivation_path: DerivationPath,
    },
    BuildCompleted {
        derivation_path: DerivationPath,
        output_paths: Vec<DerivationPath>,
    },
    BuildFailed {
        derivation_path: DerivationPath,
        error: String,
    },
}

/// Response from applying a Raft command
#[derive(Debug, Clone, Serialize, Deserialize)]
pub enum RaftResponse {
    /// Agent successfully joined
    AgentJoined { agent_id: AgentId },

    /// Agent successfully left
    AgentLeft { agent_id: AgentId },

    /// Heartbeat acknowledged
    HeartbeatAck,

    /// Build queued with suggested agents
    BuildQueued {
        derivation_path: DerivationPath,
        suggested_agents: Vec<AgentId>,
    },

    /// Build claimed by agent
    BuildClaimedAck {
        derivation_path: DerivationPath,
        agent_id: AgentId,
    },

    /// Build claim rejected
    BuildClaimRejected {
        derivation_path: DerivationPath,
        reason: String,
    },

    /// Build started
    BuildStartedAck,

    /// Build completed
    BuildCompletedAck,

    /// Build failed
    BuildFailedAck,

    /// Internal Raft operation (Blank entry, Membership change)
    /// These operations don't correspond to client commands
    InternalOp,
}

impl ClusterState {
    /// Apply a Raft command to the cluster state
    pub fn apply(&mut self, command: RaftCommand) -> RaftResponse {
        match command {
            RaftCommand::AgentJoined { id, info } => {
                self.agents.insert(id, info);
                tracing::debug!(agent_id = %id, total_agents = self.agents.len(), "Agent joined cluster");
                RaftResponse::AgentJoined { agent_id: id }
            }

            RaftCommand::AgentLeft { id } => {
                self.agents.remove(&id);

                // Clean up all builds assigned to this agent
                // When an agent fails, all its builds are lost (process died)
                // CLIs will detect stream disconnect and retry elsewhere
                let mut removed_builds = Vec::new();

                // Remove all builds where this agent claimed/was building
                self.builds_in_progress.retain(|drv_path, tracker| {
                    if tracker.agent_id == Some(id) {
                        removed_builds.push(drv_path.clone());
                        false // Remove this build
                    } else {
                        true // Keep this build
                    }
                });

                // Remove pending derivations for removed builds
                for drv_path in &removed_builds {
                    self.pending_derivations.remove(drv_path);
                }

                tracing::info!(
                    agent_id = %id,
                    builds_removed = removed_builds.len(),
                    "Agent removed from cluster, builds cleaned up"
                );

                RaftResponse::AgentLeft { agent_id: id }
            }

            RaftCommand::AgentHeartbeat { id, timestamp } => {
                if let Some(agent) = self.agents.get_mut(&id) {
                    agent.last_heartbeat = timestamp;
                }
                RaftResponse::HeartbeatAck
            }

            RaftCommand::BuildQueued {
                top_level,
                derivation_nar,
                dependencies,
                platform,
                features,
            } => {
                // Create ranked list of suggested agents based on affinity
                // Agents will self-select and claim builds based on this ranking

                // Step 1: Filter eligible agents (platform + features, not Down)
                let eligible: Vec<&AgentInfo> = self
                    .agents
                    .values()
                    .filter(|agent| {
                        agent.platforms.contains(&platform)
                            && features.iter().all(|f| agent.features.contains(f))
                            && agent.status != AgentStatus::Down
                    })
                    .collect();

                // Step 2: Score ALL eligible agents by affinity (count matching dependencies)
                let mut agent_scores: Vec<(AgentId, usize)> = eligible
                    .iter()
                    .map(|agent| {
                        let mut score = 0;
                        for dep_path in &dependencies {
                            // Count dependencies in builds_in_progress
                            if let Some(tracker) = self.builds_in_progress.get(dep_path)
                                && tracker.agent_id == Some(agent.id)
                            {
                                score += 1;
                            }
                            // Count dependencies in completed_builds
                            if let Some(completed) = self.completed_builds.get(dep_path)
                                && completed.agent_id == agent.id
                            {
                                score += 1;
                            }
                        }
                        (agent.id, score)
                    })
                    .collect();

                // Step 3: Sort by score (descending), then by agent_id (ascending) for determinism
                agent_scores.sort_by(|(id_a, score_a), (id_b, score_b)| {
                    match score_b.cmp(score_a) {
                        // Higher score is better
                        std::cmp::Ordering::Equal => id_a.cmp(id_b), // Smaller ID wins tie
                        other => other,
                    }
                });

                // Extract ranked list of agent IDs
                let suggested_agents: Vec<AgentId> =
                    agent_scores.iter().map(|(id, _)| *id).collect();

                // Step 4: Create build tracker in Queued state
                let now = Utc::now();

                self.builds_in_progress.insert(
                    top_level.clone(),
                    BuildTracker {
                        agent_id: None, // No agent claimed yet
                        suggested_agents: suggested_agents.clone(),
                        started_at: now,
                        parent_build: None,
                        status: BuildStatus::Queued,
                    },
                );

                // Insert all dependencies as queued too
                for dep in &dependencies {
                    self.builds_in_progress.insert(
                        dep.clone(),
                        BuildTracker {
                            agent_id: None,
                            suggested_agents: suggested_agents.clone(), // Same agents suggested
                            started_at: now,
                            parent_build: Some(top_level.clone()),
                            status: BuildStatus::Queued,
                        },
                    );
                }

                // Store derivation NAR (all agents have it now)
                self.pending_derivations
                    .insert(top_level.clone(), derivation_nar);

                // Do NOT mark any agent as Busy yet (they haven't claimed)

                tracing::info!(
                    "Build {} queued, {} eligible agents, suggested: {:?}",
                    top_level,
                    suggested_agents.len(),
                    &suggested_agents[..suggested_agents.len().min(3)]
                );

                RaftResponse::BuildQueued {
                    derivation_path: top_level,
                    suggested_agents,
                }
            }

            RaftCommand::BuildClaimed {
                derivation_path,
                agent_id,
                affinity_score: _,
            } => {
                // Verify build exists and is claimable
                if let Some(tracker) = self.builds_in_progress.get_mut(&derivation_path) {
                    // Check if build is in Queued state
                    if tracker.status != BuildStatus::Queued {
                        return RaftResponse::BuildClaimRejected {
                            derivation_path,
                            reason: format!(
                                "Build already claimed/started (status: {:?})",
                                tracker.status
                            ),
                        };
                    }

                    // Check if agent is in suggested list OR timeout has passed
                    let elapsed = Utc::now().signed_duration_since(tracker.started_at);
                    let timeout_passed = elapsed.num_seconds() >= 2;

                    if !tracker.suggested_agents.contains(&agent_id) && !timeout_passed {
                        return RaftResponse::BuildClaimRejected {
                            derivation_path,
                            reason: "Wait for suggested agents (timeout not passed)".to_string(),
                        };
                    }

                    // Accept the claim
                    tracker.agent_id = Some(agent_id);
                    tracker.status = BuildStatus::Claimed;

                    // Mark agent as Busy
                    if let Some(agent) = self.agents.get_mut(&agent_id) {
                        agent.status = AgentStatus::Busy;
                    }

                    tracing::info!("Build {} claimed by agent {}", derivation_path, agent_id);

                    RaftResponse::BuildClaimedAck {
                        derivation_path,
                        agent_id,
                    }
                } else {
                    RaftResponse::BuildClaimRejected {
                        derivation_path,
                        reason: "Build not found".to_string(),
                    }
                }
            }

            RaftCommand::BuildStarted { derivation_path } => {
                if let Some(tracker) = self.builds_in_progress.get_mut(&derivation_path) {
                    // Verify build is in Claimed state
                    if tracker.status != BuildStatus::Claimed {
                        tracing::warn!(
                            "BuildStarted for {} but status is {:?}, ignoring",
                            derivation_path,
                            tracker.status
                        );
                        return RaftResponse::BuildStartedAck;
                    }

                    tracker.status = BuildStatus::Building;
                    tracing::info!("Build {} started", derivation_path);
                }
                RaftResponse::BuildStartedAck
            }

            RaftCommand::BuildCompleted {
                derivation_path,
                output_paths,
            } => {
                // Remove from in-progress
                if let Some(tracker) = self.builds_in_progress.remove(&derivation_path) {
                    // Move to completed builds (only if we have an agent_id)
                    if let Some(agent_id) = tracker.agent_id {
                        self.completed_builds.insert(
                            derivation_path.clone(),
                            CompletedBuild {
                                agent_id,
                                output_paths,
                                completed_at: Utc::now(),
                            },
                        );
                    } else {
                        tracing::warn!(
                            "Build {} completed but no agent_id set (never claimed?)",
                            derivation_path
                        );
                    }

                    // Remove dependencies (where parent_build = this derivation_path)
                    self.builds_in_progress
                        .retain(|_, t| t.parent_build.as_ref() != Some(&derivation_path));

                    // Remove derivation NAR (no longer needed)
                    self.pending_derivations.remove(&derivation_path);
                }
                RaftResponse::BuildCompletedAck
            }

            RaftCommand::BuildFailed {
                derivation_path,
                error: _,
            } => {
                // Remove from in-progress (allow immediate retry)
                if let Some(_tracker) = self.builds_in_progress.remove(&derivation_path) {
                    // Remove dependencies
                    self.builds_in_progress
                        .retain(|_, t| t.parent_build.as_ref() != Some(&derivation_path));

                    // Remove derivation NAR (no longer needed)
                    self.pending_derivations.remove(&derivation_path);
                }
                RaftResponse::BuildFailedAck
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use anyhow::Context;
    use camino::Utf8PathBuf;

    #[test]
    fn test_agent_joined() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        let info = AgentInfo {
            id: agent_id,
            address: Url::parse("http://localhost:50051")?,
            platforms: vec!["x86_64-linux".to_string()],
            features: vec!["kvm".to_string()],
            status: AgentStatus::Available,
            last_heartbeat: Utc::now(),
        };

        let response = state.apply(RaftCommand::AgentJoined {
            id: agent_id,
            info: info.clone(),
        });

        assert!(matches!(response, RaftResponse::AgentJoined { .. }));
        assert_eq!(state.agents.len(), 1);
        assert_eq!(
            state
                .agents
                .get(&agent_id)
                .context("agent should exist")?
                .address
                .as_str(),
            "http://localhost:50051/"
        );
        Ok(())
    }

    #[test]
    fn test_build_lifecycle() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        // Add agent first
        let info = AgentInfo {
            id: agent_id,
            address: Url::parse("http://localhost:50051")?,
            platforms: vec!["x86_64-linux".to_string()],
            features: vec![],
            status: AgentStatus::Available,
            last_heartbeat: Utc::now(),
        };

        state.apply(RaftCommand::AgentJoined { id: agent_id, info });

        // Queue a build
        let drv_path = Utf8PathBuf::from("/nix/store/abc-foo.drv");
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: drv_path.clone(),
            derivation_nar: vec![1, 2, 3], // Placeholder NAR
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        assert!(matches!(response, RaftResponse::BuildQueued { .. }));
        assert_eq!(state.builds_in_progress.len(), 1);

        // Claim the build
        state.apply(RaftCommand::BuildClaimed {
            derivation_path: drv_path.clone(),
            agent_id,
            affinity_score: 0,
        });

        // Start the build
        state.apply(RaftCommand::BuildStarted {
            derivation_path: drv_path.clone(),
        });

        // Complete the build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: drv_path.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/abc-foo")],
        });

        assert_eq!(state.builds_in_progress.len(), 0);
        assert_eq!(state.completed_builds.len(), 1);
        Ok(())
    }

    #[test]
    fn test_dependency_cleanup_on_completion() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue build with dependencies
        let top = Utf8PathBuf::from("/nix/store/top.drv");
        let dep1 = Utf8PathBuf::from("/nix/store/dep1.drv");
        let dep2 = Utf8PathBuf::from("/nix/store/dep2.drv");

        state.apply(RaftCommand::BuildQueued {
            top_level: top.clone(),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![dep1.clone(), dep2.clone()],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should have 3 entries (top + 2 deps)
        assert_eq!(state.builds_in_progress.len(), 3);

        // Claim and start the build
        state.apply(RaftCommand::BuildClaimed {
            derivation_path: top.clone(),
            agent_id,
            affinity_score: 0,
        });

        state.apply(RaftCommand::BuildStarted {
            derivation_path: top.clone(),
        });

        // Complete the build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: top.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/top")],
        });

        // Should remove top-level AND all dependencies
        assert_eq!(state.builds_in_progress.len(), 0);
        assert_eq!(state.completed_builds.len(), 1);
        Ok(())
    }

    #[test]
    fn test_deterministic_assignment_platform_filter() -> anyhow::Result<()> {
        let mut state = ClusterState::default();

        // Add two agents with different platforms
        let agent_x86 = AgentId::new_v4();
        let agent_arm = AgentId::new_v4();

        state.agents.insert(
            agent_x86,
            AgentInfo {
                id: agent_x86,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent_arm,
            AgentInfo {
                id: agent_arm,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["aarch64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue build requiring x86_64-linux
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/foo.drv"),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should suggest x86 agent only
        match response {
            RaftResponse::BuildQueued {
                suggested_agents, ..
            } => {
                assert_eq!(suggested_agents.len(), 1);
                assert_eq!(suggested_agents[0], agent_x86);
            }
            _ => anyhow::bail!("Expected BuildQueued response"),
        }
        Ok(())
    }

    #[test]
    fn test_deterministic_assignment_feature_filter() -> anyhow::Result<()> {
        let mut state = ClusterState::default();

        let agent_with_kvm = AgentId::new_v4();
        let agent_without_kvm = AgentId::new_v4();

        state.agents.insert(
            agent_with_kvm,
            AgentInfo {
                id: agent_with_kvm,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec!["kvm".to_string()],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent_without_kvm,
            AgentInfo {
                id: agent_without_kvm,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue build requiring kvm feature
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/vm-test.drv"),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec!["kvm".to_string()],
        });

        // Should suggest agent with kvm only
        match response {
            RaftResponse::BuildQueued {
                suggested_agents, ..
            } => {
                assert_eq!(suggested_agents.len(), 1);
                assert_eq!(suggested_agents[0], agent_with_kvm);
            }
            _ => anyhow::bail!("Expected BuildQueued response"),
        }
        Ok(())
    }

    #[test]
    fn test_deterministic_assignment_affinity() -> anyhow::Result<()> {
        let mut state = ClusterState::default();

        let agent_a = AgentId::new_v4();
        let agent_b = AgentId::new_v4();

        // Add two eligible agents
        state.agents.insert(
            agent_a,
            AgentInfo {
                id: agent_a,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent_b,
            AgentInfo {
                id: agent_b,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Claim a dependency build on agent_a
        let dep = Utf8PathBuf::from("/nix/store/dep.drv");
        state.builds_in_progress.insert(
            dep.clone(),
            BuildTracker {
                agent_id: Some(agent_a),
                suggested_agents: vec![],
                started_at: Utc::now(),
                parent_build: None,
                status: BuildStatus::Building,
            },
        );

        // Queue build that depends on the dep
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/app.drv"),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![dep],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should suggest agent_a first due to affinity
        match response {
            RaftResponse::BuildQueued {
                suggested_agents, ..
            } => {
                assert_eq!(suggested_agents.len(), 2);
                assert_eq!(suggested_agents[0], agent_a);
            }
            _ => anyhow::bail!("Expected BuildQueued response"),
        }
        Ok(())
    }

    #[test]
    fn test_deterministic_assignment_tie_break() -> anyhow::Result<()> {
        let mut state = ClusterState::default();

        // Create two agents with deterministic IDs for tie-breaking test
        let agent_a = AgentId::from_bytes([0u8; 16]); // Smaller
        let agent_b = AgentId::from_bytes([1u8; 16]); // Larger

        state.agents.insert(
            agent_a,
            AgentInfo {
                id: agent_a,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent_b,
            AgentInfo {
                id: agent_b,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue build with no dependencies (both agents have affinity score 0)
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/foo.drv"),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should tie-break to smallest agent_id
        match response {
            RaftResponse::BuildQueued {
                suggested_agents, ..
            } => {
                assert_eq!(suggested_agents.len(), 2);
                assert_eq!(suggested_agents[0], agent_a);
            }
            _ => anyhow::bail!("Expected BuildQueued response"),
        }
        Ok(())
    }

    #[test]
    fn test_claim_marks_agent_busy() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue a build
        state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/foo.drv"),
            derivation_nar: vec![1, 2, 3],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Agent should still be Available after queuing
        assert_eq!(
            state
                .agents
                .get(&agent_id)
                .context("agent should exist")?
                .status,
            AgentStatus::Available
        );

        // Claim the build
        state.apply(RaftCommand::BuildClaimed {
            derivation_path: Utf8PathBuf::from("/nix/store/foo.drv"),
            agent_id,
            affinity_score: 0,
        });

        // Agent should now be Busy after claiming
        assert_eq!(
            state
                .agents
                .get(&agent_id)
                .context("agent should exist")?
                .status,
            AgentStatus::Busy
        );
        Ok(())
    }

    #[test]
    fn test_queued_build_excludes_down_agents() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_available = AgentId::new_v4();
        let agent_down = AgentId::new_v4();

        // Add one Available agent and one Down agent
        state.agents.insert(
            agent_available,
            AgentInfo {
                id: agent_available,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent_down,
            AgentInfo {
                id: agent_down,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Down,
                last_heartbeat: Utc::now(),
            },
        );

        // Queue a build
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/bar.drv"),
            derivation_nar: vec![4, 5, 6],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should only suggest the Available agent (Down agent excluded)
        match response {
            RaftResponse::BuildQueued {
                suggested_agents, ..
            } => {
                assert_eq!(suggested_agents.len(), 1);
                assert_eq!(suggested_agents[0], agent_available);
            }
            _ => anyhow::bail!("Expected BuildQueued response"),
        }

        // Build should be registered
        assert_eq!(state.builds_in_progress.len(), 1);
        // Derivation should be stored
        assert!(
            state
                .pending_derivations
                .contains_key(&Utf8PathBuf::from("/nix/store/bar.drv"))
        );
        Ok(())
    }

    #[test]
    fn test_pending_derivations_cleanup_on_completion() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        let drv_path = Utf8PathBuf::from("/nix/store/test.drv");

        // Queue build
        state.apply(RaftCommand::BuildQueued {
            top_level: drv_path.clone(),
            derivation_nar: vec![7, 8, 9],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Derivation should be stored
        assert!(state.pending_derivations.contains_key(&drv_path));

        // Claim and start the build
        state.apply(RaftCommand::BuildClaimed {
            derivation_path: drv_path.clone(),
            agent_id,
            affinity_score: 0,
        });

        state.apply(RaftCommand::BuildStarted {
            derivation_path: drv_path.clone(),
        });

        // Complete build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: drv_path.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/test")],
        });

        // Derivation should be removed
        assert!(!state.pending_derivations.contains_key(&drv_path));
        Ok(())
    }

    #[test]
    fn test_agent_left_cleans_up_builds() -> anyhow::Result<()> {
        let mut state = ClusterState::default();

        let agent1 = AgentId::new_v4();
        let agent2 = AgentId::new_v4();

        // Add two agents
        state.agents.insert(
            agent1,
            AgentInfo {
                id: agent1,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        state.agents.insert(
            agent2,
            AgentInfo {
                id: agent2,
                address: Url::parse("http://localhost:50052")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Add builds to both agents
        let drv1 = Utf8PathBuf::from("/nix/store/foo.drv");
        let drv2 = Utf8PathBuf::from("/nix/store/bar.drv");

        state.builds_in_progress.insert(
            drv1.clone(),
            BuildTracker {
                agent_id: Some(agent1),
                suggested_agents: vec![],
                started_at: Utc::now(),
                parent_build: None,
                status: BuildStatus::Building,
            },
        );

        state.builds_in_progress.insert(
            drv2.clone(),
            BuildTracker {
                agent_id: Some(agent2),
                suggested_agents: vec![],
                started_at: Utc::now(),
                parent_build: None,
                status: BuildStatus::Building,
            },
        );

        state
            .pending_derivations
            .insert(drv1.clone(), vec![1, 2, 3]);
        state
            .pending_derivations
            .insert(drv2.clone(), vec![4, 5, 6]);

        // Agent1 fails
        state.apply(RaftCommand::AgentLeft { id: agent1 });

        // Agent1's builds should be removed
        assert!(!state.builds_in_progress.contains_key(&drv1));
        assert!(!state.pending_derivations.contains_key(&drv1));

        // Agent2's builds should remain
        assert!(state.builds_in_progress.contains_key(&drv2));
        assert!(state.pending_derivations.contains_key(&drv2));

        // Agent1 should be removed
        assert!(!state.agents.contains_key(&agent1));
        assert!(state.agents.contains_key(&agent2));
        Ok(())
    }

    #[test]
    fn test_agent_left_cleans_up_multiple_builds() -> anyhow::Result<()> {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051")?,
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Add 3 builds to the agent
        let builds = vec![
            Utf8PathBuf::from("/nix/store/foo.drv"),
            Utf8PathBuf::from("/nix/store/bar.drv"),
            Utf8PathBuf::from("/nix/store/baz.drv"),
        ];

        for drv in &builds {
            state.builds_in_progress.insert(
                drv.clone(),
                BuildTracker {
                    agent_id: Some(agent_id),
                    suggested_agents: vec![],
                    started_at: Utc::now(),
                    parent_build: None,
                    status: BuildStatus::Building,
                },
            );
            state.pending_derivations.insert(drv.clone(), vec![1, 2, 3]);
        }

        assert_eq!(state.builds_in_progress.len(), 3);
        assert_eq!(state.pending_derivations.len(), 3);

        // Agent fails
        state.apply(RaftCommand::AgentLeft { id: agent_id });

        // All builds should be cleaned up
        assert_eq!(state.builds_in_progress.len(), 0);
        assert_eq!(state.pending_derivations.len(), 0);
        assert!(!state.agents.contains_key(&agent_id));
        Ok(())
    }
}
