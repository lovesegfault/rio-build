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
    pub agent_id: AgentId,
    pub started_at: DateTime<Utc>,
    pub parent_build: Option<DerivationPath>, // None = top-level, Some = dependency
    pub status: BuildStatus,
}

/// Build status enumeration
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub enum BuildStatus {
    Building,                                             // Currently executing on agent
    QueuedDependency { blocked_on: Vec<DerivationPath> }, // Waiting for dependencies
    QueuedCapacity, // Waiting for agent capacity (agent is busy)
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
    BuildStatusChanged {
        derivation_path: DerivationPath,
        status: BuildStatus,
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

    /// Build queued and assigned to agent
    BuildAssigned {
        agent_id: AgentId,
        derivation_path: DerivationPath,
    },

    /// Build status changed
    StatusChanged,

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

                // Remove all builds where this agent was assigned
                self.builds_in_progress.retain(|drv_path, tracker| {
                    if tracker.agent_id == id {
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
                // Deterministic agent assignment algorithm (DESIGN.md Section 1)
                // Status-blind: Available and Busy agents compete equally

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

                // If no eligible agents, return error response
                // TODO: Add NoEligibleAgents response variant in Phase 3
                if eligible.is_empty() {
                    // For now, return with nil agent_id to signal error
                    return RaftResponse::BuildAssigned {
                        agent_id: AgentId::nil(),
                        derivation_path: top_level,
                    };
                }

                // Step 2: Score agents by affinity (count matching dependencies)
                let mut scores: HashMap<AgentId, usize> = HashMap::new();

                for dep_path in &dependencies {
                    // Count dependencies in builds_in_progress
                    if let Some(tracker) = self.builds_in_progress.get(dep_path) {
                        *scores.entry(tracker.agent_id).or_insert(0) += 1;
                    }
                    // Count dependencies in completed_builds
                    if let Some(completed) = self.completed_builds.get(dep_path) {
                        *scores.entry(completed.agent_id).or_insert(0) += 1;
                    }
                }

                // Step 3: Select highest affinity agent
                // Step 4: Tie-break by smallest agent_id (lexicographic)
                let selected_agent_id = scores
                    .iter()
                    .filter(|(id, _)| eligible.iter().any(|a| &a.id == *id))
                    .max_by(|(id_a, score_a), (id_b, score_b)| {
                        // First compare by score (higher is better)
                        match score_a.cmp(score_b) {
                            std::cmp::Ordering::Equal => {
                                // Tie-break: smaller agent_id wins (deterministic)
                                id_b.cmp(id_a)
                            }
                            other => other,
                        }
                    })
                    .map(|(id, _)| *id)
                    .or_else(|| {
                        // No agents with affinity - select smallest agent_id among eligible
                        eligible.iter().min_by_key(|a| &a.id).map(|a| a.id)
                    })
                    .expect("Should have at least one eligible agent");

                // Step 5: Update state
                let now = Utc::now();

                // Insert top-level build
                self.builds_in_progress.insert(
                    top_level.clone(),
                    BuildTracker {
                        agent_id: selected_agent_id,
                        started_at: now,
                        parent_build: None,
                        status: BuildStatus::Building,
                    },
                );

                // Insert all dependencies with parent_build pointer
                for dep in &dependencies {
                    self.builds_in_progress.insert(
                        dep.clone(),
                        BuildTracker {
                            agent_id: selected_agent_id,
                            started_at: now,
                            parent_build: Some(top_level.clone()),
                            status: BuildStatus::Building,
                        },
                    );
                }

                // Store derivation NAR (all agents have it now)
                self.pending_derivations
                    .insert(top_level.clone(), derivation_nar);

                // Mark agent as Busy
                if let Some(agent) = self.agents.get_mut(&selected_agent_id) {
                    agent.status = AgentStatus::Busy;
                }

                RaftResponse::BuildAssigned {
                    agent_id: selected_agent_id,
                    derivation_path: top_level,
                }
            }

            RaftCommand::BuildStatusChanged {
                derivation_path,
                status,
            } => {
                if let Some(tracker) = self.builds_in_progress.get_mut(&derivation_path) {
                    tracker.status = status;
                }
                RaftResponse::StatusChanged
            }

            RaftCommand::BuildCompleted {
                derivation_path,
                output_paths,
            } => {
                // Remove from in-progress
                if let Some(tracker) = self.builds_in_progress.remove(&derivation_path) {
                    // Move to completed builds
                    self.completed_builds.insert(
                        derivation_path.clone(),
                        CompletedBuild {
                            agent_id: tracker.agent_id,
                            output_paths,
                            completed_at: Utc::now(),
                        },
                    );

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
    use camino::Utf8PathBuf;

    #[test]
    fn test_agent_joined() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        let info = AgentInfo {
            id: agent_id,
            address: Url::parse("http://localhost:50051").unwrap(),
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
            state.agents.get(&agent_id).unwrap().address.as_str(),
            "http://localhost:50051/"
        );
    }

    #[test]
    fn test_build_lifecycle() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        // Add agent first
        let info = AgentInfo {
            id: agent_id,
            address: Url::parse("http://localhost:50051").unwrap(),
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

        assert!(matches!(response, RaftResponse::BuildAssigned { .. }));
        assert_eq!(state.builds_in_progress.len(), 1);

        // Complete the build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: drv_path.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/abc-foo")],
        });

        assert_eq!(state.builds_in_progress.len(), 0);
        assert_eq!(state.completed_builds.len(), 1);
    }

    #[test]
    fn test_dependency_cleanup_on_completion() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051").unwrap(),
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

        // Complete the build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: top.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/top")],
        });

        // Should remove top-level AND all dependencies
        assert_eq!(state.builds_in_progress.len(), 0);
        assert_eq!(state.completed_builds.len(), 1);
    }

    #[test]
    fn test_deterministic_assignment_platform_filter() {
        let mut state = ClusterState::default();

        // Add two agents with different platforms
        let agent_x86 = AgentId::new_v4();
        let agent_arm = AgentId::new_v4();

        state.agents.insert(
            agent_x86,
            AgentInfo {
                id: agent_x86,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                address: Url::parse("http://localhost:50052").unwrap(),
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

        // Should assign to x86 agent only
        match response {
            RaftResponse::BuildAssigned { agent_id, .. } => {
                assert_eq!(agent_id, agent_x86);
            }
            _ => panic!("Expected BuildAssigned response"),
        }
    }

    #[test]
    fn test_deterministic_assignment_feature_filter() {
        let mut state = ClusterState::default();

        let agent_with_kvm = AgentId::new_v4();
        let agent_without_kvm = AgentId::new_v4();

        state.agents.insert(
            agent_with_kvm,
            AgentInfo {
                id: agent_with_kvm,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                address: Url::parse("http://localhost:50052").unwrap(),
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

        // Should assign to agent with kvm only
        match response {
            RaftResponse::BuildAssigned { agent_id, .. } => {
                assert_eq!(agent_id, agent_with_kvm);
            }
            _ => panic!("Expected BuildAssigned response"),
        }
    }

    #[test]
    fn test_deterministic_assignment_affinity() {
        let mut state = ClusterState::default();

        let agent_a = AgentId::new_v4();
        let agent_b = AgentId::new_v4();

        // Add two eligible agents
        state.agents.insert(
            agent_a,
            AgentInfo {
                id: agent_a,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                address: Url::parse("http://localhost:50052").unwrap(),
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Available,
                last_heartbeat: Utc::now(),
            },
        );

        // Assign a dependency build to agent_a
        let dep = Utf8PathBuf::from("/nix/store/dep.drv");
        state.builds_in_progress.insert(
            dep.clone(),
            BuildTracker {
                agent_id: agent_a,
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

        // Should assign to agent_a due to affinity
        match response {
            RaftResponse::BuildAssigned { agent_id, .. } => {
                assert_eq!(agent_id, agent_a);
            }
            _ => panic!("Expected BuildAssigned response"),
        }
    }

    #[test]
    fn test_deterministic_assignment_tie_break() {
        let mut state = ClusterState::default();

        // Create two agents with deterministic IDs for tie-breaking test
        let agent_a = AgentId::from_bytes([0u8; 16]); // Smaller
        let agent_b = AgentId::from_bytes([1u8; 16]); // Larger

        state.agents.insert(
            agent_a,
            AgentInfo {
                id: agent_a,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                address: Url::parse("http://localhost:50052").unwrap(),
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
            RaftResponse::BuildAssigned { agent_id, .. } => {
                assert_eq!(agent_id, agent_a);
            }
            _ => panic!("Expected BuildAssigned response"),
        }
    }

    #[test]
    fn test_assignment_marks_agent_busy() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051").unwrap(),
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

        // Agent should now be Busy
        assert_eq!(
            state.agents.get(&agent_id).unwrap().status,
            AgentStatus::Busy
        );
    }

    #[test]
    fn test_status_blind_assignment_to_busy_agent() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        // Add one agent that is already Busy
        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051").unwrap(),
                platforms: vec!["x86_64-linux".to_string()],
                features: vec![],
                status: AgentStatus::Busy, // Already busy!
                last_heartbeat: Utc::now(),
            },
        );

        // Queue a build - should still assign to Busy agent (status-blind)
        let response = state.apply(RaftCommand::BuildQueued {
            top_level: Utf8PathBuf::from("/nix/store/bar.drv"),
            derivation_nar: vec![4, 5, 6],
            dependencies: vec![],
            platform: "x86_64-linux".to_string(),
            features: vec![],
        });

        // Should assign to the Busy agent (only eligible agent)
        match response {
            RaftResponse::BuildAssigned {
                agent_id: assigned_id,
                ..
            } => {
                assert_eq!(assigned_id, agent_id);
            }
            _ => panic!("Expected BuildAssigned response"),
        }

        // Build should be registered
        assert_eq!(state.builds_in_progress.len(), 1);
        // Derivation should be stored
        assert!(
            state
                .pending_derivations
                .contains_key(&Utf8PathBuf::from("/nix/store/bar.drv"))
        );
    }

    #[test]
    fn test_pending_derivations_cleanup_on_completion() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051").unwrap(),
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

        // Complete build
        state.apply(RaftCommand::BuildCompleted {
            derivation_path: drv_path.clone(),
            output_paths: vec![Utf8PathBuf::from("/nix/store/test")],
        });

        // Derivation should be removed
        assert!(!state.pending_derivations.contains_key(&drv_path));
    }

    #[test]
    fn test_agent_left_cleans_up_builds() {
        let mut state = ClusterState::default();

        let agent1 = AgentId::new_v4();
        let agent2 = AgentId::new_v4();

        // Add two agents
        state.agents.insert(
            agent1,
            AgentInfo {
                id: agent1,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                address: Url::parse("http://localhost:50052").unwrap(),
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
                agent_id: agent1,
                started_at: Utc::now(),
                parent_build: None,
                status: BuildStatus::Building,
            },
        );

        state.builds_in_progress.insert(
            drv2.clone(),
            BuildTracker {
                agent_id: agent2,
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
    }

    #[test]
    fn test_agent_left_cleans_up_multiple_builds() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        state.agents.insert(
            agent_id,
            AgentInfo {
                id: agent_id,
                address: Url::parse("http://localhost:50051").unwrap(),
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
                    agent_id,
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
    }
}
