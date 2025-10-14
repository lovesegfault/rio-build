//! Agent state and management

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use openraft::Raft;
use rio_common::nix_utils::NixConfig;
use rio_common::proto::BuildUpdate;
use rio_common::{AgentId, DerivationPath};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tonic::Status;

use crate::storage::{StateMachineStore, TypeConfig};

/// Build agent
///
/// Phase 1: Single agent with no Raft coordination.
/// Phase 2+: Includes Raft for cluster coordination.
pub struct Agent {
    /// Agent unique identifier
    pub id: AgentId,

    /// Platforms this agent can build for
    pub platforms: Vec<String>,

    /// System features available
    pub features: Vec<String>,

    /// Currently executing build (None = available)
    pub current_build: Arc<Mutex<Option<BuildJob>>>,

    /// Data directory for agent state and temporary builds
    pub data_dir: Utf8PathBuf,

    /// Raft instance (None for Phase 1, Some for Phase 2+)
    pub raft: Option<Arc<Raft<TypeConfig>>>,

    /// State machine store for querying cluster state (None for Phase 1)
    pub state_machine: Option<StateMachineStore>,
}

impl Agent {
    /// Create a new agent
    ///
    /// Queries Nix configuration and sets up data directory.
    pub async fn new(data_dir: Utf8PathBuf) -> Result<Self> {
        // Generate unique agent ID
        let id = uuid::Uuid::new_v4();

        // Query Nix configuration
        let nix_config = NixConfig::parse()
            .await
            .context("Failed to query Nix configuration")?;

        let platforms = nix_config.all_platforms();
        let features = nix_config.system_features;

        tracing::info!("Agent ID: {}", id);
        tracing::info!("Platforms: {:?}", platforms);
        tracing::info!("Features: {:?}", features);

        // Create data directory
        tokio::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("Failed to create data directory: {}", data_dir))?;

        Ok(Self {
            id,
            platforms,
            features,
            current_build: Arc::new(Mutex::new(None)),
            data_dir,
            raft: None,          // Phase 1: No Raft
            state_machine: None, // Phase 1: No state machine
        })
    }

    /// Create a new agent with Raft cluster (bootstrap single-node)
    ///
    /// Phase 2+: Creates agent and bootstraps a single-node Raft cluster.
    /// Returns (Agent, heartbeat_handle, failure_detector_handle).
    /// The handles run in background and will be cleaned up on process exit.
    ///
    /// # Arguments
    /// * `heartbeat_interval` - How often to send heartbeats (None = 10 seconds default)
    /// * `check_interval` - How often to check for failed agents (None = 15 seconds default)
    /// * `timeout` - Heartbeat timeout before marking agent Down (None = 30 seconds default)
    pub async fn bootstrap(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(
        Self,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    )> {
        // Generate unique agent ID
        let id = uuid::Uuid::new_v4();

        // Query Nix configuration
        let nix_config = NixConfig::parse()
            .await
            .context("Failed to query Nix configuration")?;

        let platforms = nix_config.all_platforms();
        let features = nix_config.system_features;

        tracing::info!("Agent ID: {}", id);
        tracing::info!("Platforms: {:?}", platforms);
        tracing::info!("Features: {:?}", features);

        // Create data directory
        tokio::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("Failed to create data directory: {}", data_dir))?;

        // Bootstrap Raft cluster (NodeId = AgentId = Uuid)
        let (raft, sm_store) =
            crate::raft_node::bootstrap_single_node(id, rpc_addr.clone(), &data_dir)
                .await
                .context("Failed to bootstrap Raft cluster")?;

        // Register this agent in the cluster
        crate::membership::register_agent(&raft, id, rpc_addr, platforms.clone(), features.clone())
            .await
            .context("Failed to register agent")?;

        // Start heartbeat tasks (Phase 2.5) with configurable intervals
        let heartbeat_interval = heartbeat_interval.unwrap_or(std::time::Duration::from_secs(10));
        let check_interval = check_interval.unwrap_or(std::time::Duration::from_secs(15));
        let timeout = timeout.unwrap_or(std::time::Duration::from_secs(30));

        let heartbeat_handle =
            crate::heartbeat::start_heartbeat_task(id, raft.clone(), heartbeat_interval);
        let failure_detector_handle = crate::heartbeat::start_failure_detector_task(
            raft.clone(),
            sm_store.clone(),
            check_interval,
            timeout,
        );

        tracing::info!("Heartbeat tasks started");

        Ok((
            Self {
                id,
                platforms,
                features,
                current_build: Arc::new(Mutex::new(None)),
                data_dir,
                raft: Some(raft),
                state_machine: Some(sm_store),
            },
            heartbeat_handle,
            failure_detector_handle,
        ))
    }
}

/// Active build job (metadata only, process managed by background task)
pub struct BuildJob {
    /// Derivation path (serves as identifier)
    pub drv_path: DerivationPath,

    /// Subscribers receiving build updates
    pub subscribers: Vec<mpsc::Sender<Result<BuildUpdate, Status>>>,
}
