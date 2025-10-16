//! Mock RioAgent server for testing rio-build client

use rio_common::proto::rio_agent_server::{RioAgent, RioAgentServer};
use rio_common::proto::*;
use std::sync::Arc;
use tokio::sync::Mutex;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

/// Mock agent server for testing
pub struct MockRioAgent {
    /// Received build requests (for verification)
    pub received_requests: Arc<Mutex<Vec<QueueBuildRequest>>>,

    /// Response to return from queue_build
    pub queue_response: QueueBuildResponse,

    /// Messages to send in subscribe stream
    pub build_updates: Vec<BuildUpdate>,
}

impl MockRioAgent {
    /// Create a mock agent that returns BuildAssigned
    pub fn new_assigned(agent_id: &str, drv_path: &str) -> Self {
        Self {
            received_requests: Arc::new(Mutex::new(Vec::new())),
            queue_response: QueueBuildResponse {
                result: Some(queue_build_response::Result::Assigned(BuildAssigned {
                    agent_id: agent_id.to_string(),
                    derivation_path: drv_path.to_string(),
                })),
            },
            build_updates: Vec::new(),
        }
    }

    /// Add build updates to send to subscribers
    pub fn with_updates(mut self, updates: Vec<BuildUpdate>) -> Self {
        self.build_updates = updates;
        self
    }
}

#[tonic::async_trait]
impl RioAgent for MockRioAgent {
    async fn queue_build(
        &self,
        request: Request<QueueBuildRequest>,
    ) -> Result<Response<QueueBuildResponse>, Status> {
        // Store the request for verification
        let req = request.into_inner();
        self.received_requests.lock().await.push(req);

        // Return the configured response
        Ok(Response::new(self.queue_response.clone()))
    }

    type SubscribeToBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<BuildUpdate, Status>>;

    async fn subscribe_to_build(
        &self,
        _request: Request<SubscribeToBuildRequest>,
    ) -> Result<Response<Self::SubscribeToBuildStream>, Status> {
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Send all configured build updates
        let updates = self.build_updates.clone();
        tokio::spawn(async move {
            for update in updates {
                let _ = tx.send(Ok(update)).await;
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn get_cluster_members(
        &self,
        _request: Request<GetClusterMembersRequest>,
    ) -> Result<Response<ClusterMembers>, Status> {
        Err(Status::unimplemented("Mock agent"))
    }

    type GetCompletedBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<BuildUpdate, Status>>;

    async fn get_completed_build(
        &self,
        _request: Request<GetCompletedBuildRequest>,
    ) -> Result<Response<Self::GetCompletedBuildStream>, Status> {
        Err(Status::unimplemented("Mock agent"))
    }

    async fn get_build_status(
        &self,
        _request: Request<GetBuildStatusRequest>,
    ) -> Result<Response<BuildStatusResponse>, Status> {
        Err(Status::unimplemented("Mock agent"))
    }

    async fn join_cluster(
        &self,
        _request: Request<JoinClusterRequest>,
    ) -> Result<Response<JoinClusterResponse>, Status> {
        Err(Status::unimplemented("Mock agent"))
    }

    async fn report_build_result(
        &self,
        _request: Request<ReportBuildResultRequest>,
    ) -> Result<Response<ReportBuildResultResponse>, Status> {
        Err(Status::unimplemented("Mock agent"))
    }
}

/// Start a mock agent server on a random port
///
/// Returns the server task handle and the URL to connect to
pub async fn start_mock_server(
    mock: MockRioAgent,
) -> Result<
    (
        tokio::task::JoinHandle<Result<(), tonic::transport::Error>>,
        String,
    ),
    Box<dyn std::error::Error>,
> {
    // Use port 0 to get a random available port
    let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await?;
    let addr = listener.local_addr()?;
    let url = format!("http://{}", addr);

    let server_task = tokio::spawn(async move {
        Server::builder()
            .add_service(RioAgentServer::new(mock))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
    });

    // Give server a moment to start
    tokio::time::sleep(tokio::time::Duration::from_millis(100)).await;

    Ok((server_task, url))
}
