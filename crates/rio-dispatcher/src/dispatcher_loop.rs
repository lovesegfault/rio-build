// Background task that processes the build queue

use crate::build_queue::{BuildJob, BuildQueue, JobStatus};
use crate::builder_pool::BuilderPool;
use crate::scheduler::Scheduler;
use rio_common::proto::{ExecuteBuildRequest, build_service_client::BuildServiceClient};
use std::time::Duration;
use tokio::sync::watch;
use tracing::{error, info, warn};

/// Background loop that processes jobs from the build queue
pub struct DispatcherLoop {
    build_queue: BuildQueue,
    scheduler: Scheduler,
    builder_pool: BuilderPool,
}

impl DispatcherLoop {
    pub fn new(build_queue: BuildQueue, scheduler: Scheduler, builder_pool: BuilderPool) -> Self {
        Self {
            build_queue,
            scheduler,
            builder_pool,
        }
    }

    /// Run the dispatcher loop until shutdown signal is received
    pub async fn run(self, mut shutdown: watch::Receiver<bool>) {
        info!("Dispatcher loop started");

        loop {
            tokio::select! {
                _ = shutdown.changed() => {
                    info!("Shutdown signal received, stopping dispatcher loop");
                    break;
                }

                result = self.process_next_job() => {
                    if let Err(e) = result {
                        error!("Error processing job: {}", e);
                    }
                }
            }
        }

        info!("Dispatcher loop stopped");
    }

    /// Try to dequeue and process the next job
    async fn process_next_job(&self) -> anyhow::Result<()> {
        // Try to dequeue a job (non-blocking)
        let job = match self.build_queue.dequeue().await {
            Some(job) => job,
            None => {
                // Queue empty, sleep briefly to avoid busy-waiting
                tokio::time::sleep(Duration::from_millis(100)).await;
                return Ok(());
            }
        };

        info!(
            "Processing job {} for platform {} (derivation: {})",
            job.job_id, job.platform, job.derivation_path
        );

        // Update status to Dispatched
        self.build_queue
            .update_status(&job.job_id, JobStatus::Dispatched)
            .await;

        // Spawn task to handle this job asynchronously so we can process more jobs
        let queue = self.build_queue.clone();
        let scheduler = self.scheduler.clone();
        let pool = self.builder_pool.clone();

        tokio::spawn(async move {
            if let Err(e) = Self::dispatch_job(job.clone(), queue, scheduler, pool).await {
                error!("Failed to dispatch job {}: {}", job.job_id, e);
            }
        });

        Ok(())
    }

    /// Dispatch a job to a builder and wait for completion
    async fn dispatch_job(
        job: BuildJob,
        queue: BuildQueue,
        scheduler: Scheduler,
        pool: BuilderPool,
    ) -> anyhow::Result<()> {
        // Select a builder for this job
        let builder_id = match scheduler.select_builder(&job).await {
            Some(id) => id,
            None => {
                warn!(
                    "No builder available for job {} (platform: {}), re-queuing",
                    job.job_id, job.platform
                );
                // Re-queue the job to try again later
                queue.enqueue(job).await;
                return Ok(());
            }
        };

        info!("Selected builder {} for job {}", builder_id, job.job_id);

        // Get builder info
        let builder_info = pool
            .get_builder(&builder_id)
            .await
            .ok_or_else(|| anyhow::anyhow!("Builder {} disappeared from pool", builder_id))?;

        // Update status to Building
        queue.update_status(&job.job_id, JobStatus::Building).await;

        // Connect to builder's gRPC server
        let endpoint = builder_info.endpoint.clone();
        info!("Connecting to builder at {}", endpoint);

        let mut client = BuildServiceClient::connect(endpoint.clone()).await?;

        // Read the derivation file
        let derivation = tokio::fs::read(&job.derivation_path)
            .await
            .unwrap_or_else(|_| {
                warn!(
                    "Could not read derivation file {}, sending empty",
                    job.derivation_path
                );
                Vec::new()
            });

        // Create build request
        let request = ExecuteBuildRequest {
            job_id: job.job_id.to_string(),
            derivation,
            required_systems: vec![job.platform.clone()],
            env: Default::default(),
            timeout_seconds: Some(3600), // 1 hour default timeout
        };

        info!(
            "Sending ExecuteBuild request to builder for job {}",
            job.job_id
        );

        // Execute build and stream responses
        let mut stream = client.execute_build(request).await?.into_inner();

        let mut completed = false;
        let mut failed = false;

        // Process response stream
        while let Some(response) = stream.message().await? {
            use rio_common::proto::execute_build_response::Update;

            match response.update {
                Some(Update::Log(log)) => {
                    info!("[Job {}] {}", job.job_id, log.line);
                }
                Some(Update::Completed(result)) => {
                    info!(
                        "Job {} completed successfully: {:?}",
                        job.job_id, result.output_paths
                    );
                    completed = true;
                }
                Some(Update::Failed(failure)) => {
                    error!("Job {} failed: {}", job.job_id, failure.error);
                    if let Some(stderr) = failure.stderr {
                        error!("Job {} stderr: {}", job.job_id, stderr);
                    }
                    failed = true;
                }
                None => {}
            }
        }

        // Update final status
        let final_status = if completed {
            JobStatus::Completed
        } else if failed {
            JobStatus::Failed
        } else {
            // Stream ended without completion or failure message
            warn!(
                "Job {} stream ended without result, marking as failed",
                job.job_id
            );
            JobStatus::Failed
        };

        queue.update_status(&job.job_id, final_status).await;

        info!("Job {} processing complete: {:?}", job.job_id, final_status);

        Ok(())
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::build_queue::BuildJob;
    use rio_common::proto::{
        BuildCompleted, ExecuteBuildResponse, LogLine, RegisterBuilderRequest,
        build_service_server::{BuildService, BuildServiceServer},
    };
    use std::net::SocketAddr;
    use tokio::time::{sleep, timeout};
    use tonic::{Request, Response, Status};

    /// Mock builder service for testing
    struct MockBuilderService {}

    #[tonic::async_trait]
    impl BuildService for MockBuilderService {
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
            let (tx, rx) = tokio::sync::mpsc::channel(10);

            tokio::spawn(async move {
                // Send log
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
            Err(Status::unimplemented("Not used in test"))
        }
    }

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

        sleep(Duration::from_millis(100)).await;
        (handle, actual_addr)
    }

    #[tokio::test]
    async fn test_dispatcher_loop_processes_job() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());

        // Start mock builder
        let (_builder_handle, builder_addr) = start_mock_builder().await;

        // Register builder
        pool.register_builder(
            rio_common::BuilderId::new(),
            format!("http://{}", builder_addr),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        // Enqueue a job
        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job_id = queue.enqueue(job).await;

        // Start dispatcher loop
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_instance = DispatcherLoop::new(queue.clone(), scheduler, pool);

        let loop_handle = tokio::spawn(async move {
            loop_instance.run(shutdown_rx).await;
        });

        // Wait for job to be processed
        sleep(Duration::from_secs(1)).await;

        // Check job status
        let job = queue.get_job(&job_id).await.unwrap();
        assert!(
            job.status == JobStatus::Completed || job.status == JobStatus::Building,
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
    async fn test_dispatcher_loop_requeues_when_no_builders() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());

        // No builders registered!

        // Enqueue a job
        let job = BuildJob::new(
            "/nix/store/test.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        queue.enqueue(job).await;

        let initial_size = queue.size().await;
        assert_eq!(initial_size, 1);

        // Start dispatcher loop
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_instance = DispatcherLoop::new(queue.clone(), scheduler, pool);

        let loop_handle = tokio::spawn(async move {
            loop_instance.run(shutdown_rx).await;
        });

        // Wait a bit for processing attempt
        sleep(Duration::from_millis(300)).await;

        // Job should be re-queued (size should still be 1 or more)
        let size = queue.size().await;
        assert!(
            size >= 1,
            "Job should be re-queued when no builders available"
        );

        // Shutdown
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), loop_handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_dispatcher_loop_shutdown() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_instance = DispatcherLoop::new(queue, scheduler, pool);

        let loop_handle = tokio::spawn(async move {
            loop_instance.run(shutdown_rx).await;
        });

        // Send shutdown immediately
        shutdown_tx.send(true).unwrap();

        // Loop should exit quickly
        let result = timeout(Duration::from_secs(1), loop_handle).await;
        assert!(result.is_ok(), "Loop should shutdown gracefully");
    }

    #[tokio::test]
    async fn test_dispatcher_loop_handles_empty_queue() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());

        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_instance = DispatcherLoop::new(queue, scheduler, pool);

        let loop_handle = tokio::spawn(async move {
            loop_instance.run(shutdown_rx).await;
        });

        // Let it run with empty queue
        sleep(Duration::from_millis(300)).await;

        // Should still be running (not crashed)
        assert!(!loop_handle.is_finished());

        // Shutdown
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(1), loop_handle)
            .await
            .unwrap()
            .unwrap();
    }

    #[tokio::test]
    async fn test_dispatcher_loop_concurrent_jobs() {
        let pool = BuilderPool::new();
        let queue = BuildQueue::new();
        let scheduler = Scheduler::new(pool.clone());

        // Start mock builder
        let (_builder_handle, builder_addr) = start_mock_builder().await;

        // Register builder
        pool.register_builder(
            rio_common::BuilderId::new(),
            format!("http://{}", builder_addr),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

        // Enqueue multiple jobs
        let job1 = BuildJob::new(
            "/nix/store/job1.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job2 = BuildJob::new(
            "/nix/store/job2.drv".to_string(),
            "x86_64-linux".to_string(),
        );
        let job3 = BuildJob::new(
            "/nix/store/job3.drv".to_string(),
            "x86_64-linux".to_string(),
        );

        let id1 = queue.enqueue(job1).await;
        let id2 = queue.enqueue(job2).await;
        let id3 = queue.enqueue(job3).await;

        // Start dispatcher loop
        let (shutdown_tx, shutdown_rx) = tokio::sync::watch::channel(false);
        let loop_instance = DispatcherLoop::new(queue.clone(), scheduler, pool);

        let loop_handle = tokio::spawn(async move {
            loop_instance.run(shutdown_rx).await;
        });

        // Wait for jobs to be processed
        sleep(Duration::from_secs(2)).await;

        // Check all jobs were processed
        let mut jobs_completed = 0;
        for job_id in &[id1, id2, id3] {
            if let Some(job) = queue.get_job(job_id).await {
                if matches!(
                    job.status,
                    JobStatus::Completed | JobStatus::Building | JobStatus::Dispatched
                ) {
                    jobs_completed += 1;
                }
            }
        }

        assert!(
            jobs_completed >= 2,
            "At least 2 jobs should be processed, got {}",
            jobs_completed
        );

        // Shutdown
        shutdown_tx.send(true).unwrap();
        timeout(Duration::from_secs(2), loop_handle)
            .await
            .unwrap()
            .unwrap();
    }
}
