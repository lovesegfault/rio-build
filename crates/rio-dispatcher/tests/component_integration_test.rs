// Component integration tests - verify Store + DispatcherLoop + Builder work together

use nix_daemon::{BuildMode, Progress, Store};
use rio_common::proto::{
    BuildCompleted, ExecuteBuildRequest, ExecuteBuildResponse, LogLine, OutputData,
    RegisterBuilderRequest,
    build_service_server::{BuildService, BuildServiceServer},
};
use rio_dispatcher::build_queue::{BuildQueue, JobStatus};
use rio_dispatcher::builder_pool::BuilderPool;
use rio_dispatcher::dispatcher_loop::DispatcherLoop;
use rio_dispatcher::nix_store::DispatcherStore;
use rio_dispatcher::scheduler::Scheduler;
use std::net::SocketAddr;
use std::time::Duration;
use tokio::sync::watch;
use tokio::time::{sleep, timeout};
use tonic::{Request, Response, Status};

/// Enhanced mock builder that simulates real build with output transfer
struct ComponentTestBuilder {}

#[tonic::async_trait]
impl BuildService for ComponentTestBuilder {
    async fn register_builder(
        &self,
        _request: Request<RegisterBuilderRequest>,
    ) -> Result<Response<rio_common::proto::RegisterBuilderResponse>, Status> {
        Err(Status::unimplemented("Not used in test"))
    }

    type HeartbeatStream = tokio_stream::wrappers::ReceiverStream<
        Result<rio_common::proto::HeartbeatResponse, Status>,
    >;

    async fn heartbeat(
        &self,
        _request: Request<tonic::Streaming<rio_common::proto::HeartbeatRequest>>,
    ) -> Result<Response<Self::HeartbeatStream>, Status> {
        Err(Status::unimplemented("Not used in test"))
    }

    type ExecuteBuildStream =
        tokio_stream::wrappers::ReceiverStream<Result<ExecuteBuildResponse, Status>>;

    async fn execute_build(
        &self,
        request: Request<ExecuteBuildRequest>,
    ) -> Result<Response<Self::ExecuteBuildStream>, Status> {
        let req = request.into_inner();
        let (tx, rx) = tokio::sync::mpsc::channel(100);

        tokio::spawn(async move {
            // Send initial log
            let _ = tx
                .send(Ok(ExecuteBuildResponse {
                    job_id: req.job_id.clone(),
                    update: Some(rio_common::proto::execute_build_response::Update::Log(
                        LogLine {
                            timestamp: 1234567890,
                            line: "Component test build started".to_string(),
                        },
                    )),
                }))
                .await;

            // Simulate build completion with outputs
            let output_path = "/nix/store/component-test-output".to_string();

            // Send completion message
            let _ = tx
                .send(Ok(ExecuteBuildResponse {
                    job_id: req.job_id.clone(),
                    update: Some(
                        rio_common::proto::execute_build_response::Update::Completed(
                            BuildCompleted {
                                output_paths: vec![output_path.clone()],
                                duration_ms: 100,
                            },
                        ),
                    ),
                }))
                .await;

            // Send mock NAR data chunks
            let mock_nar = b"MOCK NAR DATA FOR TESTING";
            const CHUNK_SIZE: usize = 10;

            for (index, chunk) in mock_nar.chunks(CHUNK_SIZE).enumerate() {
                let is_final = (index + 1) * CHUNK_SIZE >= mock_nar.len();

                let _ = tx
                    .send(Ok(ExecuteBuildResponse {
                        job_id: req.job_id.clone(),
                        update: Some(rio_common::proto::execute_build_response::Update::Output(
                            OutputData {
                                store_path: output_path.clone(),
                                nar_chunk: chunk.to_vec(),
                                chunk_index: index as u64,
                                is_final_chunk: is_final,
                            },
                        )),
                    }))
                    .await;

                if is_final {
                    break;
                }
            }
        });

        Ok(Response::new(tokio_stream::wrappers::ReceiverStream::new(
            rx,
        )))
    }

    async fn get_builder_status(
        &self,
        _request: Request<rio_common::proto::GetBuilderStatusRequest>,
    ) -> Result<Response<rio_common::proto::BuilderStatus>, Status> {
        Err(Status::unimplemented("Not used in test"))
    }
}

async fn start_component_test_builder() -> (tokio::task::JoinHandle<()>, SocketAddr) {
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let actual_addr = listener.local_addr().unwrap();

    let handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(BuildServiceServer::new(ComponentTestBuilder {}))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(100)).await;
    (handle, actual_addr)
}

#[tokio::test]
async fn test_store_to_builder_integration() {
    // Setup all components
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    // Start mock builder
    let (_builder_handle, builder_addr) = start_component_test_builder().await;

    // Register builder
    pool.register_builder(
        rio_common::BuilderId::new(),
        format!("http://{}", builder_addr),
        vec!["x86_64-linux".to_string()],
        vec![],
    )
    .await
    .unwrap();

    // Start dispatcher loop
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_instance = DispatcherLoop::new(queue.clone(), scheduler.clone(), pool.clone());

    let loop_handle = tokio::spawn(async move {
        loop_instance.run(shutdown_rx).await;
    });

    // Create Store instance (what SSH handler uses)
    let mut store = DispatcherStore::new(queue.clone(), scheduler, pool);

    // Simulate what happens when client calls build_paths
    let drv_paths = vec!["/nix/store/test-component.drv"];
    let build_result = store
        .build_paths(drv_paths, BuildMode::Normal)
        .result()
        .await;

    assert!(build_result.is_ok(), "build_paths should succeed");

    // Wait for background processing
    sleep(Duration::from_secs(2)).await;

    // Verify job was created and processed
    let all_jobs = queue.get_all_jobs().await;
    assert!(!all_jobs.is_empty(), "Should have at least one job");

    let job = &all_jobs[0];
    assert!(
        matches!(
            job.status,
            JobStatus::Completed | JobStatus::Building | JobStatus::Dispatched
        ),
        "Job should be processed, got {:?}",
        job.status
    );

    // Shutdown
    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(2), loop_handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_full_build_cycle_with_outputs() {
    // Setup components
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    // Start mock builder with output streaming
    let (_builder_handle, builder_addr) = start_component_test_builder().await;

    pool.register_builder(
        rio_common::BuilderId::new(),
        format!("http://{}", builder_addr),
        vec!["x86_64-linux".to_string()],
        vec![],
    )
    .await
    .unwrap();

    // Start dispatcher loop
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_instance = DispatcherLoop::new(queue.clone(), scheduler.clone(), pool.clone());

    let loop_handle = tokio::spawn(async move {
        loop_instance.run(shutdown_rx).await;
    });

    // Create store
    let mut store = DispatcherStore::new(queue.clone(), scheduler, pool);

    // Build a derivation
    let drv_paths = vec!["/nix/store/test-outputs.drv"];
    store
        .build_paths(drv_paths, BuildMode::Normal)
        .result()
        .await
        .unwrap();

    // Wait for processing including output transfer
    sleep(Duration::from_secs(3)).await;

    // Verify job completed
    let all_jobs = queue.get_all_jobs().await;
    assert!(!all_jobs.is_empty());

    let completed_jobs: Vec<_> = all_jobs
        .iter()
        .filter(|j| j.status == JobStatus::Completed)
        .collect();

    assert!(
        !completed_jobs.is_empty(),
        "At least one job should complete, statuses: {:?}",
        all_jobs.iter().map(|j| &j.status).collect::<Vec<_>>()
    );

    // Shutdown
    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(2), loop_handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_query_operations_after_build() {
    // Setup components
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    // Create store
    let mut store = DispatcherStore::new(queue, scheduler.clone(), pool.clone());

    // Test is_valid_path for nonexistent
    let valid = store
        .is_valid_path("/nix/store/nonexistent-xyz")
        .result()
        .await
        .unwrap();
    assert!(!valid, "Nonexistent path should not be valid");

    // Test query_valid_paths with mix
    let paths = vec![
        "/nix/store/nonexistent-1",
        "/nix/store/nonexistent-2",
        "/nix/store/nonexistent-3",
    ];

    let valid_paths = store
        .query_valid_paths(paths, false)
        .result()
        .await
        .unwrap();

    assert_eq!(valid_paths.len(), 0, "No nonexistent paths should be valid");
}

#[tokio::test]
async fn test_concurrent_builds() {
    // Setup components
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    // Start mock builder
    let (_builder_handle, builder_addr) = start_component_test_builder().await;

    pool.register_builder(
        rio_common::BuilderId::new(),
        format!("http://{}", builder_addr),
        vec!["x86_64-linux".to_string()],
        vec![],
    )
    .await
    .unwrap();

    // Start dispatcher loop
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_instance = DispatcherLoop::new(queue.clone(), scheduler.clone(), pool.clone());

    let loop_handle = tokio::spawn(async move {
        loop_instance.run(shutdown_rx).await;
    });

    // Create store
    let mut store = DispatcherStore::new(queue.clone(), scheduler, pool);

    // Submit multiple builds concurrently
    let paths = vec![
        "/nix/store/concurrent-1.drv",
        "/nix/store/concurrent-2.drv",
        "/nix/store/concurrent-3.drv",
    ];

    store
        .build_paths(paths, BuildMode::Normal)
        .result()
        .await
        .unwrap();

    // Wait for processing
    sleep(Duration::from_secs(3)).await;

    // Verify multiple jobs were processed
    let all_jobs = queue.get_all_jobs().await;
    assert_eq!(all_jobs.len(), 3, "Should have 3 jobs");

    let processed = all_jobs
        .iter()
        .filter(|j| {
            matches!(
                j.status,
                JobStatus::Completed | JobStatus::Building | JobStatus::Dispatched
            )
        })
        .count();

    assert!(processed >= 2, "At least 2 jobs should be processed");

    // Shutdown
    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(2), loop_handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_build_failure_propagation() {
    use rio_common::proto::BuildFailed;

    /// Mock builder that fails builds
    struct FailingBuilder {}

    #[tonic::async_trait]
    impl BuildService for FailingBuilder {
        async fn register_builder(
            &self,
            _request: Request<RegisterBuilderRequest>,
        ) -> Result<Response<rio_common::proto::RegisterBuilderResponse>, Status> {
            Err(Status::unimplemented("Not used"))
        }

        type HeartbeatStream = tokio_stream::wrappers::ReceiverStream<
            Result<rio_common::proto::HeartbeatResponse, Status>,
        >;

        async fn heartbeat(
            &self,
            _request: Request<tonic::Streaming<rio_common::proto::HeartbeatRequest>>,
        ) -> Result<Response<Self::HeartbeatStream>, Status> {
            Err(Status::unimplemented("Not used"))
        }

        type ExecuteBuildStream =
            tokio_stream::wrappers::ReceiverStream<Result<ExecuteBuildResponse, Status>>;

        async fn execute_build(
            &self,
            request: Request<ExecuteBuildRequest>,
        ) -> Result<Response<Self::ExecuteBuildStream>, Status> {
            let req = request.into_inner();
            let (tx, rx) = tokio::sync::mpsc::channel(10);

            tokio::spawn(async move {
                // Send failure
                let _ = tx
                    .send(Ok(ExecuteBuildResponse {
                        job_id: req.job_id.clone(),
                        update: Some(rio_common::proto::execute_build_response::Update::Failed(
                            BuildFailed {
                                error: "Simulated build failure".to_string(),
                                stderr: Some("Mock stderr output".to_string()),
                                exit_code: 1,
                            },
                        )),
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
            Err(Status::unimplemented("Not used"))
        }
    }

    // Start failing builder
    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let listener = tokio::net::TcpListener::bind(addr).await.unwrap();
    let builder_addr = listener.local_addr().unwrap();

    let _builder_handle = tokio::spawn(async move {
        tonic::transport::Server::builder()
            .add_service(BuildServiceServer::new(FailingBuilder {}))
            .serve_with_incoming(tokio_stream::wrappers::TcpListenerStream::new(listener))
            .await
            .unwrap();
    });

    sleep(Duration::from_millis(100)).await;

    // Setup dispatcher
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    pool.register_builder(
        rio_common::BuilderId::new(),
        format!("http://{}", builder_addr),
        vec!["x86_64-linux".to_string()],
        vec![],
    )
    .await
    .unwrap();

    // Start dispatcher loop
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_instance = DispatcherLoop::new(queue.clone(), scheduler.clone(), pool.clone());

    let loop_handle = tokio::spawn(async move {
        loop_instance.run(shutdown_rx).await;
    });

    // Create store and submit build
    let mut store = DispatcherStore::new(queue.clone(), scheduler, pool);

    store
        .build_paths(vec!["/nix/store/fail-test.drv"], BuildMode::Normal)
        .result()
        .await
        .unwrap();

    // Wait for processing
    sleep(Duration::from_secs(1)).await;

    // Verify job failed
    let all_jobs = queue.get_all_jobs().await;
    assert!(!all_jobs.is_empty());

    let failed_jobs: Vec<_> = all_jobs
        .iter()
        .filter(|j| j.status == JobStatus::Failed)
        .collect();

    assert!(
        !failed_jobs.is_empty(),
        "Should have at least one failed job"
    );

    // Shutdown
    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(1), loop_handle)
        .await
        .unwrap()
        .unwrap();
}

#[tokio::test]
async fn test_no_builders_available() {
    // Setup with NO builders registered
    let pool = BuilderPool::new();
    let queue = BuildQueue::new();
    let scheduler = Scheduler::new(pool.clone());

    // Start dispatcher loop
    let (shutdown_tx, shutdown_rx) = watch::channel(false);
    let loop_instance = DispatcherLoop::new(queue.clone(), scheduler.clone(), pool.clone());

    let loop_handle = tokio::spawn(async move {
        loop_instance.run(shutdown_rx).await;
    });

    // Create store
    let mut store = DispatcherStore::new(queue.clone(), scheduler, pool);

    // Submit build
    store
        .build_paths(vec!["/nix/store/no-builder.drv"], BuildMode::Normal)
        .result()
        .await
        .unwrap();

    // Wait a bit
    sleep(Duration::from_millis(500)).await;

    // Job should be queued but not completed (no builders)
    let all_jobs = queue.get_all_jobs().await;
    assert!(!all_jobs.is_empty());

    // Should still be in queue (re-queued)
    let queue_size = queue.size().await;
    assert!(queue_size >= 1, "Job should be re-queued");

    // Shutdown
    shutdown_tx.send(true).unwrap();
    timeout(Duration::from_secs(1), loop_handle)
        .await
        .unwrap()
        .unwrap();
}
