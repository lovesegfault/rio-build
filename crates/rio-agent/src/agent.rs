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

use crate::storage::TypeConfig;

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
            raft: None, // Phase 1: No Raft
        })
    }

    /// Create a new agent with Raft cluster (bootstrap single-node)
    ///
    /// Phase 2+: Creates agent and bootstraps a single-node Raft cluster.
    pub async fn bootstrap(data_dir: Utf8PathBuf, rpc_addr: String) -> Result<Self> {
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

        // Bootstrap Raft cluster
        let node_id = id.as_u128() as u64; // Convert UUID to u64 for Raft NodeId
        let raft = crate::raft_node::bootstrap_single_node(node_id, rpc_addr, &data_dir)
            .await
            .context("Failed to bootstrap Raft cluster")?;

        Ok(Self {
            id,
            platforms,
            features,
            current_build: Arc::new(Mutex::new(None)),
            data_dir,
            raft: Some(raft),
        })
    }
}

/// Active build job (metadata only, process managed by background task)
pub struct BuildJob {
    /// Derivation path (serves as identifier)
    pub drv_path: DerivationPath,

    /// Subscribers receiving build updates
    pub subscribers: Vec<mpsc::Sender<Result<BuildUpdate, Status>>>,
}
