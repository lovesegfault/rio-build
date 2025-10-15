//! Agent state and management

use anyhow::{Context, Result};
use camino::Utf8PathBuf;
use openraft::Raft;
use parking_lot::Mutex;
use rio_common::nix_utils::NixConfig;
use rio_common::proto::BuildUpdate;
use rio_common::{AgentId, DerivationPath};
use std::collections::VecDeque;
use std::sync::Arc;
use std::time::Instant;
use tokio::sync::{Mutex as AsyncMutex, mpsc};
use tonic::Status;

use crate::storage::{StateMachineStore, TypeConfig};

/// Build agent
///
/// All agents use Raft for cluster coordination.
///
/// When an Agent is dropped, it will:
/// - Abort all background tasks (heartbeat, failure detector, coordinator, server)
/// - Propose AgentLeft to remove itself from the cluster (best-effort)
/// - Clean up resources
pub struct Agent {
    /// Agent unique identifier
    pub id: AgentId,

    /// Platforms this agent can build for
    pub platforms: Vec<String>,

    /// System features available
    pub features: Vec<String>,

    /// Currently executing build (None = available)
    pub current_build: Arc<AsyncMutex<Option<BuildJob>>>,

    /// Data directory for agent state and temporary builds
    pub data_dir: Utf8PathBuf,

    /// Raft instance
    pub raft: Arc<Raft<TypeConfig>>,

    /// State machine store for querying cluster state
    pub state_machine: StateMachineStore,

    /// Background task handles (None during initialization, set once tasks start)
    handles: Mutex<Option<TaskHandles>>,
}

struct TaskHandles {
    heartbeat: tokio::task::JoinHandle<()>,
    failure_detector: tokio::task::JoinHandle<()>,
    coordinator: tokio::task::JoinHandle<()>,
    server: tokio::task::JoinHandle<Result<()>>,
}

impl Drop for Agent {
    fn drop(&mut self) {
        tracing::info!(agent_id = %self.id, "Agent dropping - cleaning up background tasks");

        // Abort all background tasks if they exist
        if let Some(handles) = self.handles.lock().take() {
            handles.heartbeat.abort();
            handles.failure_detector.abort();
            handles.coordinator.abort();
            handles.server.abort();

            // Propose AgentLeft to remove from cluster (best-effort, may fail)
            // Note: We can't await in Drop, so we spawn a task
            let raft = self.raft.clone();
            let agent_id = self.id;
            tokio::spawn(async move {
                let cmd = crate::state_machine::RaftCommand::AgentLeft { id: agent_id };
                if let Err(e) = raft.client_write(cmd).await {
                    tracing::debug!(agent_id = %agent_id, error = %e, "Failed to propose AgentLeft on drop (expected in tests)");
                } else {
                    tracing::info!(agent_id = %agent_id, "Proposed AgentLeft on drop");
                }
            });
        }

        tracing::info!(agent_id = %self.id, "Agent cleanup complete");
    }
}

impl Agent {
    /// Bootstrap a new single-node Raft cluster
    ///
    /// Creates a new agent and initializes it as a single-node Raft cluster leader.
    /// The gRPC server starts automatically and runs in the background.
    ///
    /// This is an atomic operation - the returned agent is fully initialized
    /// and ready to accept builds.
    ///
    /// # Arguments
    /// * `data_dir` - Directory for Raft storage and build data
    /// * `rpc_addr` - Address to bind gRPC server (e.g., "0.0.0.0:50051")
    /// * `heartbeat_interval` - How often to send heartbeats (None = 10 seconds default)
    /// * `check_interval` - How often to check for failed agents (None = 15 seconds default)
    /// * `timeout` - Heartbeat timeout before marking agent Down (None = 30 seconds default)
    ///
    /// # Returns
    /// Arc<Agent> with gRPC server and background tasks running
    pub async fn bootstrap(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Arc<Self>> {
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

        // Bind listener FIRST (reserves port before Raft needs it)
        let listener = tokio::net::TcpListener::bind(&rpc_addr)
            .await
            .with_context(|| format!("Failed to bind to {}", rpc_addr))?;

        let actual_addr = listener
            .local_addr()
            .context("Failed to get local address")?;
        let actual_addr_str = actual_addr.to_string();

        tracing::info!("Bound gRPC listener on {}", actual_addr_str);

        // Create uninitialized Raft instance
        let (raft, sm_store) =
            crate::raft_node::create_uninitialized_raft(id, actual_addr_str.clone(), &data_dir)
                .await
                .context("Failed to create Raft instance")?;

        let current_build = Arc::new(AsyncMutex::new(None));

        // Create agent without handles yet
        let agent = Arc::new(Self {
            id,
            platforms: platforms.clone(),
            features: features.clone(),
            current_build: current_build.clone(),
            data_dir: data_dir.clone(),
            raft: raft.clone(),
            state_machine: sm_store.clone(),
            handles: Mutex::new(None),
        });

        // Start gRPC server (must run BEFORE Raft initialization)
        let server_handle = tokio::spawn({
            let agent = agent.clone();
            async move { crate::grpc_server::serve_with_listener(listener, agent).await }
        });

        tokio::time::sleep(std::time::Duration::from_millis(500)).await;
        tracing::info!("gRPC server started");

        // Initialize Raft as single-node leader (server is now ready)
        crate::raft_node::initialize_single_node_leader(&raft, id, actual_addr_str.clone())
            .await
            .context("Failed to initialize Raft as leader")?;

        // Register this agent in the cluster
        crate::membership::register_agent(&raft, id, actual_addr_str, platforms, features)
            .await
            .context("Failed to register agent")?;

        // Start heartbeat tasks AFTER becoming leader and registering
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

        // Start build coordinator (Phase 3.2) - watches for builds assigned to this agent
        let coordinator_handle = crate::build_coordinator::start_build_coordinator(
            id,
            current_build.clone(),
            raft.clone(),
            sm_store.clone(),
        );

        tracing::info!("Build coordinator started");

        // Store all task handles (now that they're all started)
        *agent.handles.lock() = Some(TaskHandles {
            heartbeat: heartbeat_handle,
            failure_detector: failure_detector_handle,
            coordinator: coordinator_handle,
            server: server_handle,
        });

        Ok(agent)
    }

    /// Join an existing Raft cluster
    ///
    /// Connects to a seed agent and requests to join its cluster.
    /// The gRPC server starts automatically and runs in the background.
    ///
    /// This is an atomic operation - the returned agent is fully joined
    /// and synced with the cluster.
    ///
    /// # Arguments
    /// * `data_dir` - Directory for Raft storage and build data
    /// * `rpc_addr` - Address to bind gRPC server (e.g., "0.0.0.0:50051")
    /// * `seed_url` - URL of a seed agent to join (e.g., "http://node1:50051")
    /// * `heartbeat_interval` - How often to send heartbeats (None = 10 seconds default)
    /// * `check_interval` - How often to check for failed agents (None = 15 seconds default)
    /// * `timeout` - Heartbeat timeout before marking agent Down (None = 30 seconds default)
    ///
    /// # Returns
    /// Arc<Agent> with gRPC server and background tasks running
    pub async fn join(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        seed_url: String,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Arc<Self>> {
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

        // Bind listener FIRST
        let listener = tokio::net::TcpListener::bind(&rpc_addr)
            .await
            .with_context(|| format!("Failed to bind to {}", rpc_addr))?;

        let actual_addr = listener
            .local_addr()
            .context("Failed to get local address")?;
        let actual_addr_str = actual_addr.to_string();

        tracing::info!("Bound gRPC listener on {}", actual_addr_str);

        // Prepare our address for joining
        let our_address =
            if actual_addr_str.starts_with("http://") || actual_addr_str.starts_with("https://") {
                actual_addr_str.clone()
            } else {
                format!("http://{}", actual_addr_str)
            };

        // Initialize Raft FIRST (before JoinCluster RPC)
        // This allows us to receive replication from leader during join
        let (raft, sm_store) = crate::raft_node::join_cluster(id, our_address.clone(), &data_dir)
            .await
            .context("Failed to initialize Raft")?;

        let current_build = Arc::new(AsyncMutex::new(None));

        // Create agent without handles yet
        let agent = Arc::new(Self {
            id,
            platforms: platforms.clone(),
            features: features.clone(),
            current_build: current_build.clone(),
            data_dir: data_dir.clone(),
            raft: raft.clone(),
            state_machine: sm_store.clone(),
            handles: Mutex::new(None),
        });

        // Start gRPC server BEFORE calling JoinCluster RPC
        // This ensures we can receive Raft replication during the join process
        let server_handle = tokio::spawn({
            let agent = agent.clone();
            async move { crate::grpc_server::serve_with_listener(listener, agent).await }
        });

        // Give server time to start
        tokio::time::sleep(std::time::Duration::from_millis(500)).await;

        tracing::info!("gRPC server started on {}", actual_addr_str);

        // NOW call JoinCluster RPC (server is ready to handle Raft replication)
        let agent_info = ProtoAgentInfo {
            id: id.to_string(),
            address: our_address.clone(),
            platforms,
            features,
            status: 0, // Will be set to Available by leader
            capacity: None,
        };

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

        // NOW start heartbeat and failure detector (after successfully joining)
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

        // Start build coordinator
        let coordinator_handle = crate::build_coordinator::start_build_coordinator(
            id,
            agent.current_build.clone(),
            raft,
            sm_store,
        );

        tracing::info!("Background tasks started");

        // Store all task handles
        *agent.handles.lock() = Some(TaskHandles {
            heartbeat: heartbeat_handle,
            failure_detector: failure_detector_handle,
            coordinator: coordinator_handle,
            server: server_handle,
        });

        Ok(agent)
    }

    /// Auto-discovery: Try to join seeds, bootstrap if all fail
    ///
    /// Implements jitter-based race resolution for concurrent bootstraps.
    /// Returns Arc<Agent> with gRPC server running.
    pub async fn auto_join_or_bootstrap(
        data_dir: Utf8PathBuf,
        rpc_addr: String,
        seed_urls: Vec<String>,
        heartbeat_interval: Option<std::time::Duration>,
        check_interval: Option<std::time::Duration>,
        timeout: Option<std::time::Duration>,
    ) -> Result<Arc<Self>> {
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

    /// Historical logs for late joiners (capped at 10,000 entries)
    /// Only stores LogLine updates (not OutputChunk - too large)
    log_history: VecDeque<BuildUpdate>,

    /// When the build started (for duration tracking)
    started_at: Instant,
}

impl BuildJob {
    /// Create a new build job
    pub fn new(drv_path: DerivationPath) -> Self {
        Self {
            drv_path,
            subscribers: Vec::new(),
            log_history: VecDeque::with_capacity(100), // Pre-allocate some space
            started_at: Instant::now(),
        }
    }

    /// Add a log line to history and broadcast to all subscribers
    /// Caps history at 10,000 entries to prevent memory exhaustion
    pub async fn add_log(&mut self, update: BuildUpdate) {
        // Only store LogLine updates in history (skip OutputChunk - too large)
        if matches!(
            update.update,
            Some(rio_common::proto::build_update::Update::Log(_))
        ) {
            self.log_history.push_back(update.clone());

            // Cap at 10,000 entries
            if self.log_history.len() > 10_000 {
                self.log_history.pop_front();
            }
        }

        // Broadcast to all active subscribers
        // Remove failed subscribers (disconnected clients)
        self.subscribers
            .retain(|sub| sub.try_send(Ok(update.clone())).is_ok());
    }

    /// Get all historical logs for catch-up (returns a clone)
    pub fn get_catch_up_logs(&self) -> Vec<BuildUpdate> {
        self.log_history.iter().cloned().collect()
    }

    /// Get build duration since start
    pub fn duration(&self) -> std::time::Duration {
        self.started_at.elapsed()
    }
}
