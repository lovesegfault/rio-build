//! Agent state and management

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use rio_common::nix_utils::NixConfig;
use rio_common::proto::BuildUpdate;
use rio_common::{AgentId, DerivationPath};
use std::sync::Arc;
use tokio::sync::{Mutex, mpsc};
use tonic::Status;

/// Build agent
///
/// Phase 1: Single agent with no Raft coordination.
/// Executes one build at a time.
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
        })
    }
}

/// Active build job
pub struct BuildJob {
    /// Derivation path (serves as identifier)
    pub drv_path: DerivationPath,

    /// Build process handle
    pub process: tokio::process::Child,

    /// Subscribers receiving build updates
    pub subscribers: Vec<mpsc::Sender<Result<BuildUpdate, Status>>>,
}
