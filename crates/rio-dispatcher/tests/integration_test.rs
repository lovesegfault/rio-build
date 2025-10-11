use rio_common::proto::{
    build_service_client::BuildServiceClient, BuilderCapacity, RegisterBuilderRequest,
};
use std::net::SocketAddr;
use tokio::time::{sleep, Duration};

/// Helper to start dispatcher in background
async fn start_test_dispatcher() -> (tokio::task::JoinHandle<()>, SocketAddr) {
    use rio_dispatcher::builder_pool::BuilderPool;
    use rio_dispatcher::grpc_server;

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap(); // Use random port
    let builder_pool = BuilderPool::new();

    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(
                rio_common::proto::build_service_server::BuildServiceServer::new(
                    grpc_server::BuildServiceImpl::new(builder_pool),
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
        response.message,
        "Builder registered successfully",
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

    let response1 = client1.register_builder(request1).await.unwrap().into_inner();
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

    let response2 = client2.register_builder(request2).await.unwrap().into_inner();
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
