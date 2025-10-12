use rio_common::proto::{
    BuildCompleted, BuilderCapacity, ExecuteBuildRequest, ExecuteBuildResponse, LogLine,
    RegisterBuilderRequest,
    build_service_client::BuildServiceClient,
    build_service_server::{BuildService, BuildServiceServer},
};
use std::net::SocketAddr;
use tokio::time::{Duration, sleep};
use tonic::{Request, Response, Status};

/// Helper to start dispatcher in background
async fn start_test_dispatcher() -> (tokio::task::JoinHandle<()>, SocketAddr) {
    use rio_dispatcher::build_queue::BuildQueue;
    use rio_dispatcher::builder_pool::BuilderPool;
    use rio_dispatcher::grpc_server;
    use rio_dispatcher::scheduler::Scheduler;

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap(); // Use random port
    let builder_pool = BuilderPool::new();
    let build_queue = BuildQueue::new();
    let scheduler = Scheduler::new(builder_pool.clone());

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                rio_common::proto::build_service_server::BuildServiceServer::new(
                    grpc_server::BuildServiceImpl::new(builder_pool, build_queue, scheduler),
                ),
            )
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Give server time to start
    sleep(Duration::from_millis(100)).await;

    (handle, actual_addr)
}

#[tokio::test]
async fn test_builder_registration() {
    let (_handle, addr) = start_test_dispatcher().await;

    // Connect as a builder
    let mut client = BuildServiceClient::connect(format!("http://{}", addr))
        .await
        .expect("Failed to connect to dispatcher");

    // Register builder
    let request = RegisterBuilderRequest {
        builder_id: "test-builder-001".to_string(),
        endpoint: "test-endpoint-001".to_string(), // Placeholder - not used yet
        capacity: Some(BuilderCapacity {
            cpu_cores: 4,
            memory_mb: 8192,
            disk_gb: 100,
        }),
        platforms: vec!["x86_64-linux".to_string()],
        features: vec![],
    };

    let response = client
        .register_builder(request)
        .await
        .expect("Registration failed")
        .into_inner();

    assert!(response.success, "Registration should succeed");
    assert_eq!(
        response.message, "Builder registered successfully",
        "Expected success message"
    );
}

#[tokio::test]
async fn test_multiple_builder_registration() {
    let (_handle, addr) = start_test_dispatcher().await;

    // Register first builder
    let mut client1 = BuildServiceClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    let request1 = RegisterBuilderRequest {
        builder_id: "test-builder-001".to_string(),
        endpoint: "test-endpoint-001".to_string(), // Placeholder - not used yet
        capacity: Some(BuilderCapacity {
            cpu_cores: 4,
            memory_mb: 8192,
            disk_gb: 100,
        }),
        platforms: vec!["x86_64-linux".to_string()],
        features: vec![],
    };

    let response1 = client1
        .register_builder(request1)
        .await
        .unwrap()
        .into_inner();
    assert!(response1.success);

    // Register second builder
    let mut client2 = BuildServiceClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    let request2 = RegisterBuilderRequest {
        builder_id: "test-builder-002".to_string(),
        endpoint: "test-endpoint-002".to_string(), // Placeholder - not used yet
        capacity: Some(BuilderCapacity {
            cpu_cores: 8,
            memory_mb: 16384,
            disk_gb: 200,
        }),
        platforms: vec!["aarch64-linux".to_string()],
        features: vec!["kvm".to_string()],
    };

    let response2 = client2
        .register_builder(request2)
        .await
        .unwrap()
        .into_inner();
    assert!(response2.success);
}

#[tokio::test]
async fn test_builder_status_query() {
    let (_handle, addr) = start_test_dispatcher().await;

    let mut client = BuildServiceClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    // Register a builder first
    let builder_id = "test-builder-status-001";
    let register_request = RegisterBuilderRequest {
        builder_id: builder_id.to_string(),
        endpoint: "test-endpoint-status-001".to_string(), // Placeholder - not used yet
        capacity: Some(BuilderCapacity {
            cpu_cores: 4,
            memory_mb: 8192,
            disk_gb: 100,
        }),
        platforms: vec!["x86_64-linux".to_string()],
        features: vec![],
    };

    client
        .register_builder(register_request)
        .await
        .unwrap()
        .into_inner();

    // Query builder status
    let status_request = rio_common::proto::GetBuilderStatusRequest {
        builder_id: builder_id.to_string(),
    };

    let status_response = client
        .get_builder_status(status_request)
        .await
        .expect("Should get builder status")
        .into_inner();

    assert_eq!(status_response.builder_id, builder_id);
}

#[tokio::test]
async fn test_heartbeat_stream() {
    let (_handle, addr) = start_test_dispatcher().await;

    let mut client = BuildServiceClient::connect(format!("http://{}", addr))
        .await
        .unwrap();

    // Register builder first
    let builder_id = "test-builder-heartbeat-001";
    let register_request = RegisterBuilderRequest {
        builder_id: builder_id.to_string(),
        endpoint: "test-endpoint-heartbeat-001".to_string(), // Placeholder - not used yet
        capacity: Some(BuilderCapacity {
            cpu_cores: 4,
            memory_mb: 8192,
            disk_gb: 100,
        }),
        platforms: vec!["x86_64-linux".to_string()],
        features: vec![],
    };

    client.register_builder(register_request).await.unwrap();

    // Create heartbeat stream
    let (tx, rx) = tokio::sync::mpsc::channel(10);
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);

    let mut response_stream = client.heartbeat(outbound).await.unwrap().into_inner();

    // Send a heartbeat
    let heartbeat = rio_common::proto::HeartbeatRequest {
        builder_id: builder_id.to_string(),
        timestamp: 1234567890,
        current_load: 0.5,
        available_capacity: Some(BuilderCapacity {
            cpu_cores: 2,
            memory_mb: 4096,
            disk_gb: 50,
        }),
    };

    tx.send(heartbeat).await.unwrap();

    // Receive response
    let response = tokio::time::timeout(Duration::from_secs(1), response_stream.message())
        .await
        .expect("Timeout waiting for heartbeat response")
        .expect("Failed to receive heartbeat response")
        .expect("Empty response");

    assert!(
        response.commands.is_empty(),
        "No commands expected for this test"
    );
}

/// Mock builder service for testing
struct MockBuilderService {}

#[tonic::async_trait]
impl BuildService for MockBuilderService {
    async fn register_builder(
        &self,
        _request: Request<RegisterBuilderRequest>,
    ) -> Result<Response<rio_common::proto::RegisterBuilderResponse>, Status> {
        Err(Status::unimplemented("Not used in builder"))
    }

    type HeartbeatStream = tokio_stream::wrappers::ReceiverStream<
        Result<rio_common::proto::HeartbeatResponse, Status>,
    >;

    async fn heartbeat(
        &self,
        _request: Request<tonic::Streaming<rio_common::proto::HeartbeatRequest>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        Err(Status::unimplemented("Not used in builder"))
    }

    type ExecuteBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<ExecuteBuildResponse, Status>>;

    async fn execute_build(
        &self,
        request: Request<ExecuteBuildRequest>,
    ) -> Result<Response<Self::ExecuteBuildStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        // Simulate build execution
        tokio::spawn(async move {
            // Send log message
            let _ = tx
                .send(Ok(ExecuteBuildResponse {
                    job_id: req.job_id.clone(),
                    update: Some(rio_common::proto::execute_build_response::Update::Log(
                        LogLine {
                            timestamp: 1234567890,
                            line: "Mock build started".to_string(),
                        },
                    )),
                }))
                .await;

            // Send completion
            let _ = tx
                .send(Ok(ExecuteBuildResponse {
                    job_id: req.job_id.clone(),
                    update: Some(
                        rio_common::proto::execute_build_response::Update::Completed(
                            BuildCompleted {
                                output_paths: vec!["/nix/store/mock-output".to_string()],
                                duration_ms: 100,
                            },
                        ),
                    ),
                }))
                .await;
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn get_builder_status(
        &self,
        _request: Request<rio_common::proto::GetBuilderStatusRequest>,
    ) -> Result<Response<rio_common::proto::BuilderStatus>, Status> {
        Err(Status::unimplemented("Not used in builder"))
    }
}

/// Helper to start a mock builder gRPC server
async fn start_mock_builder() -> (tokio::task::JoinHandle<()>, SocketAddr) {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(BuildServiceServer::new(MockBuilderService {}))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    // Give server time to start
    sleep(Duration::from_millis(100)).await;

    (handle, actual_addr)
}

#[tokio::test]
async fn test_end_to_end_build_dispatching() {
    // Start dispatcher
    let (_dispatcher_handle, dispatcher_addr) = start_test_dispatcher().await;

    // Start mock builder
    let (_builder_handle, builder_addr) = start_mock_builder().await;

    // Connect to dispatcher as a builder to register
    let mut dispatcher_client = BuildServiceClient::connect(format!("http://{}", dispatcher_addr))
        .await
        .expect("Failed to connect to dispatcher");

    // Register builder with dispatcher, providing builder's gRPC endpoint
    let builder_id = "test-builder-e2e-001";
    let register_request = RegisterBuilderRequest {
        builder_id: builder_id.to_string(),
        endpoint: format!("http://{}", builder_addr),
        capacity: Some(BuilderCapacity {
            cpu_cores: 4,
            memory_mb: 8192,
            disk_gb: 100,
        }),
        platforms: vec!["x86_64-linux".to_string()],
        features: vec![],
    };

    let register_response = dispatcher_client
        .register_builder(register_request)
        .await
        .expect("Failed to register builder")
        .into_inner();

    assert!(
        register_response.success,
        "Builder registration should succeed"
    );

    // Now send a build request to the dispatcher
    let build_request = ExecuteBuildRequest {
        job_id: "test-job-001".to_string(),
        derivation: vec![0x00, 0x01, 0x02], // Mock derivation bytes
        required_systems: vec!["x86_64-linux".to_string()],
        env: Default::default(),
        timeout_seconds: Some(300),
    };

    let mut build_response_stream = dispatcher_client
        .execute_build(build_request)
        .await
        .expect("Failed to execute build")
        .into_inner();

    // Collect all responses
    let mut responses = Vec::new();
    loop {
        match tokio::time::timeout(Duration::from_secs(2), build_response_stream.message()).await {
            Ok(Ok(Some(response))) => {
                responses.push(response);
            }
            Ok(Ok(None)) => {
                // Stream ended
                break;
            }
            Ok(Err(e)) => {
                panic!("Error receiving response: {:?}", e);
            }
            Err(_) => {
                // Timeout - no more messages
                break;
            }
        }
    }

    // Verify we got responses
    assert!(!responses.is_empty(), "Should receive build responses");

    // Verify log message
    let has_log = responses.iter().any(|r| {
        matches!(
            r.update,
            Some(rio_common::proto::execute_build_response::Update::Log(_))
        )
    });
    assert!(has_log, "Should receive at least one log message");

    // Verify completion
    let has_completion = responses.iter().any(|r| {
        matches!(
            r.update,
            Some(rio_common::proto::execute_build_response::Update::Completed(_))
        )
    });
    assert!(has_completion, "Should receive build completion");
}
