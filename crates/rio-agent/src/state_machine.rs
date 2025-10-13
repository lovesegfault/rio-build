//! Raft state machine for Rio cluster coordination
//!
//! Implements the cluster state and command processing logic from DESIGN.md Section 1.

use chrono::{DateTime, Utc};
use rio_common::types::{AgentId, DerivationPath};
use serde::{Deserialize, Serialize};
use std::collections::HashMap;

/// Node information for Raft (will be enhanced in Phase 2.4)
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq, Default)]
pub struct Node {
    pub rpc_addr: String,
}

/// Agent information stored in cluster state
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq, Eq)]
pub struct AgentInfo {
    pub id: AgentId,
    pub address: String,
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
    Queued { blocked_on: Vec<DerivationPath> }, // Waiting for dependencies
    Building,                                   // Currently executing
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
}

impl ClusterState {
    /// Apply a Raft command to the cluster state
    pub fn apply(&mut self, command: RaftCommand) -> RaftResponse {
        match command {
            RaftCommand::AgentJoined { id, info } => {
                self.agents.insert(id, info);
                RaftResponse::AgentJoined { agent_id: id }
            }

            RaftCommand::AgentLeft { id } => {
                self.agents.remove(&id);
                // TODO: Clean up builds assigned to this agent in Phase 2.5
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
                dependencies,
                platform: _,
                features: _,
            } => {
                // TODO: Implement deterministic assignment in Phase 2.3
                // For now, just return a placeholder
                let agent_id = self
                    .agents
                    .keys()
                    .next()
                    .cloned()
                    .unwrap_or_else(|| AgentId::nil());

                self.builds_in_progress.insert(
                    top_level.clone(),
                    BuildTracker {
                        agent_id,
                        started_at: Utc::now(),
                        parent_build: None,
                        status: BuildStatus::Building,
                    },
                );

                // Register dependencies (placeholder - will enhance in Phase 2.3)
                for dep in dependencies {
                    self.builds_in_progress.insert(
                        dep.clone(),
                        BuildTracker {
                            agent_id,
                            started_at: Utc::now(),
                            parent_build: Some(top_level.clone()),
                            status: BuildStatus::Building,
                        },
                    );
                }

                RaftResponse::BuildAssigned {
                    agent_id,
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
            address: "localhost:50051".to_string(),
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
            state.agents.get(&agent_id).unwrap().address,
            "localhost:50051"
        );
    }

    #[test]
    fn test_build_lifecycle() {
        let mut state = ClusterState::default();
        let agent_id = AgentId::new_v4();

        // Add agent first
        let info = AgentInfo {
            id: agent_id,
            address: "localhost:50051".to_string(),
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
                address: "localhost:50051".to_string(),
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
}
