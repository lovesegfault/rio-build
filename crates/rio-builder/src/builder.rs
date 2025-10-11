use anyhow::{Context, Result};
use rio_common::{
    BuilderId, Platform,
    proto::{
        BuilderCapacity, HeartbeatRequest, RegisterBuilderRequest,
        build_service_client::BuildServiceClient,
    },
};
use std::time::{Duration, SystemTime, UNIX_EPOCH};
use tonic::transport::Channel;
use tracing::{error, info, warn};

pub struct Builder {
    id: BuilderId,
    dispatcher_endpoint: String,
    platforms: Vec<Platform>,
    features: Vec<String>,
    grpc_client: Option<BuildServiceClient<Channel>>,
}

impl Builder {
    pub fn new(
        dispatcher_endpoint: String,
        platforms: Vec<Platform>,
        features: Vec<String>,
    ) -> Self {
        Self {
            id: BuilderId::new(),
            dispatcher_endpoint,
            platforms,
            features,
            grpc_client: None,
        }
    }

    /// Connect to the dispatcher
    pub async fn connect(&mut self) -> Result<()> {
        info!("Connecting to dispatcher at {}", self.dispatcher_endpoint);

        let client = BuildServiceClient::connect(self.dispatcher_endpoint.clone())
            .await
            .context("Failed to connect to dispatcher")?;

        self.grpc_client = Some(client);
        info!("Connected to dispatcher successfully");
        Ok(())
    }

    /// Register with the dispatcher
    pub async fn register(&mut self) -> Result<()> {
        let client = self
            .grpc_client
            .as_mut()
            .context("Not connected to dispatcher")?;

        let platform_strings: Vec<String> = self.platforms.iter().map(|p| p.to_string()).collect();

        let request = RegisterBuilderRequest {
            builder_id: self.id.to_string(),
            endpoint: format!("{}:{}", hostname::get()?.to_string_lossy(), 50052),
            capacity: Some(BuilderCapacity {
                cpu_cores: num_cpus::get() as i32,
                memory_mb: 8192, // TODO: Get actual memory
                disk_gb: 100,    // TODO: Get actual disk space
            }),
            platforms: platform_strings,
            features: self.features.clone(),
        };

        info!("Registering builder {} with dispatcher", self.id);

        let response = client
            .register_builder(request)
            .await
            .context("Failed to register with dispatcher")?
            .into_inner();

        if response.success {
            info!("Successfully registered: {}", response.message);
            Ok(())
        } else {
            error!("Registration failed: {}", response.message);
            Err(anyhow::anyhow!("Registration failed: {}", response.message))
        }
    }

    /// Start sending heartbeats to dispatcher
    pub async fn start_heartbeat_loop(&mut self) -> Result<()> {
        let client = self
            .grpc_client
            .as_mut()
            .context("Not connected to dispatcher")?;

        let builder_id = self.id.to_string();

        info!("Starting heartbeat loop");

        let (tx, rx) = tokio::sync::mpsc::channel(100);
        let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);

        // Spawn task to send periodic heartbeats
        let heartbeat_tx = tx.clone();
        let builder_id_clone = builder_id.clone();
        tokio::spawn(async move {
            let mut interval = tokio::time::interval(Duration::from_secs(30));

            loop {
                interval.tick().await;

                let timestamp = SystemTime::now()
                    .duration_since(UNIX_EPOCH)
                    .unwrap()
                    .as_secs() as i64;

                let heartbeat = HeartbeatRequest {
                    builder_id: builder_id_clone.clone(),
                    timestamp,
                    current_load: 0.0, // TODO: Calculate actual load
                    available_capacity: Some(BuilderCapacity {
                        cpu_cores: num_cpus::get() as i32,
                        memory_mb: 8192,
                        disk_gb: 100,
                    }),
                };

                if heartbeat_tx.send(heartbeat).await.is_err() {
                    warn!("Heartbeat channel closed, stopping heartbeat loop");
                    break;
                }
            }
        });

        // Start heartbeat stream
        let mut response_stream = client
            .heartbeat(outbound)
            .await
            .context("Failed to start heartbeat stream")?
            .into_inner();

        // Process responses from dispatcher
        tokio::spawn(async move {
            while let Ok(Some(response)) = response_stream.message().await {
                if !response.commands.is_empty() {
                    info!("Received commands from dispatcher: {:?}", response.commands);
                    // TODO: Process commands
                }
            }
            info!("Heartbeat response stream ended");
        });

        Ok(())
    }

    pub fn id(&self) -> &BuilderId {
        &self.id
    }
}
