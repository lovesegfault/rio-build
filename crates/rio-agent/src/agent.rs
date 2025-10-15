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
/// All agents use Raft for cluster coordination.
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

    /// Raft instance
    pub raft: Arc<Raft<TypeConfig>>,

    /// State machine store for querying cluster state
    pub state_machine: StateMachineStore,
}

impl Agent {
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

        // Create agent instance
        let agent = Self {
            id,
            platforms,
            features,
            current_build: Arc::new(Mutex::new(None)),
            data_dir,
            raft: raft.clone(),
            state_machine: sm_store.clone(),
        };

        // Start build coordinator (Phase 3.2) - watches for builds assigned to this agent
        let coordinator_handle = crate::build_coordinator::start_build_coordinator(
            id,
            agent.current_build.clone(),
            raft,
            sm_store,
        );

        tracing::info!("Build coordinator started");

        Ok((
            agent,
            heartbeat_handle,
            failure_detector_handle,
            coordinator_handle,
        ))
    }

    /// Join an existing Raft cluster
    ///
    /// Connects to a seed agent and requests to join its cluster.
    /// Returns agent with background tasks started.
    pub async fn join(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        seed_url: String,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(
        Self,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    )> {
        use rio_common::proto::rio_agent_client::RioAgentClient;
        use rio_common::proto::{AgentInfo as ProtoAgentInfo, JoinClusterRequest};

        // Generate unique agent ID
        let id = uuid::Uuid::new_v4();

        // Query Nix configuration
        let nix_config = NixConfig::parse()
            .await
            .context("Failed to query Nix configuration")?;

        let platforms = nix_config.all_platforms();
        let features = nix_config.system_features;

        tracing::info!("Agent ID: {}", id);
        tracing::info!("Joining cluster via seed: {}", seed_url);
        tracing::info!("Platforms: {:?}", platforms);
        tracing::info!("Features: {:?}", features);

        // Create data directory
        tokio::fs::create_dir_all(&data_dir)
            .await
            .with_context(|| format!("Failed to create data directory: {}", data_dir))?;

        // Prepare our agent info for joining
        let our_address = if rpc_addr.starts_with("http://") || rpc_addr.starts_with("https://") {
            rpc_addr.clone()
        } else {
            format!("http://{}", rpc_addr)
        };

        let agent_info = ProtoAgentInfo {
            id: id.to_string(),
            address: our_address.clone(),
            platforms: platforms.clone(),
            features: features.clone(),
            status: 0, // Will be set to Available by leader
            capacity: None,
        };

        // Connect to seed agent and request to join
        tracing::info!("Connecting to seed agent at: {}", seed_url);
        let mut client = RioAgentClient::connect(seed_url.clone())
            .await
            .with_context(|| format!("Failed to connect to seed agent: {}", seed_url))?;

        let join_response = client
            .join_cluster(JoinClusterRequest {
                agent_info: Some(agent_info),
            })
            .await
            .context("JoinCluster RPC failed")?
            .into_inner();

        if !join_response.success {
            anyhow::bail!("Failed to join cluster: {}", join_response.message);
        }

        tracing::info!("Successfully joined cluster: {}", join_response.message);

        // Initialize Raft as a learner/member (not bootstrap)
        // The leader has already added us to the cluster
        let (raft, sm_store) = crate::raft_node::join_cluster(id, our_address.clone(), &data_dir)
            .await
            .context("Failed to initialize Raft after joining")?;

        // Start heartbeat, failure detector, and coordinator tasks
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

        // Create agent instance
        let agent = Self {
            id,
            platforms,
            features,
            current_build: Arc::new(Mutex::new(None)),
            data_dir,
            raft: raft.clone(),
            state_machine: sm_store.clone(),
        };

        // Start build coordinator
        let coordinator_handle = crate::build_coordinator::start_build_coordinator(
            id,
            agent.current_build.clone(),
            raft,
            sm_store,
        );

        tracing::info!("Background tasks started");

        Ok((
            agent,
            heartbeat_handle,
            failure_detector_handle,
            coordinator_handle,
        ))
    }

    /// Auto-discovery: Try to join seeds, bootstrap if all fail
    ///
    /// Implements jitter-based race resolution for concurrent bootstraps.
    pub async fn auto_join_or_bootstrap(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        seed_urls: Vec<String>,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<(
        Self,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
        tokio::task::JoinHandle<()>,
    )> {
        tracing::info!("Attempting to join cluster from {} seeds", seed_urls.len());

        // Try each seed
        for seed_url in &seed_urls {
            tracing::debug!("Trying to join via seed: {}", seed_url);

            match Self::join(
                data_dir.clone(),
                rpc_addr.clone(),
                seed_url.clone(),
                heartbeat_interval,
                check_interval,
                timeout,
            )
            .await
            {
                Ok(result) => {
                    tracing::info!("Successfully joined cluster via {}", seed_url);
                    return Ok(result);
                }
                Err(e) => {
                    tracing::warn!("Failed to join via {}: {}", seed_url, e);
                }
            }
        }

        // All seeds failed - bootstrap new cluster with jitter
        tracing::info!("Could not join any seed, will bootstrap new cluster");

        // Add random jitter (0-1000ms) to avoid simultaneous bootstrap
        let jitter_ms = rand::random::<u64>() % 1000;
        tracing::debug!("Waiting {}ms jitter before bootstrap", jitter_ms);
        tokio::time::sleep(std::time::Duration::from_millis(jitter_ms)).await;

        // Try join one more time (maybe someone else bootstrapped during jitter)
        for seed_url in &seed_urls {
            if let Ok(result) = Self::join(
                data_dir.clone(),
                rpc_addr.clone(),
                seed_url.clone(),
                heartbeat_interval,
                check_interval,
                timeout,
            )
            .await
            {
                tracing::info!("Joined cluster via {} after jitter wait", seed_url);
                return Ok(result);
            }
        }

        // Still no cluster - bootstrap
        tracing::info!("Bootstrapping new single-node cluster");
        Self::bootstrap(
            data_dir,
            rpc_addr,
            heartbeat_interval,
            check_interval,
            timeout,
        )
        .await
    }
}

/// Active build job (metadata only, process managed by background task)
pub struct BuildJob {
    /// Derivation path (serves as identifier)
    pub drv_path: DerivationPath,

    /// Subscribers receiving build updates
    pub subscribers: Vec<mpsc::Sender<Result<BuildUpdate, Status>>>,
}
