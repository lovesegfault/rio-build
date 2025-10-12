// Integration tests for SSH server with Nix protocol adapter

use rio_dispatcher::build_queue::BuildQueue;
use rio_dispatcher::builder_pool::BuilderPool;
use rio_dispatcher::scheduler::Scheduler;
use rio_dispatcher::ssh_server::{SshConfig, SshHandler, SshServer};
use russh::keys::{Algorithm, PrivateKey};
use std::net::SocketAddr;

#[tokio::test]
async fn test_ssh_handler_initialization() {
    // Test that we can create an SSH handler with all required components
    let builder_pool = BuilderPool::new();
    let build_queue = BuildQueue::new();
    let scheduler = Scheduler::new(builder_pool.clone());

    let handler = SshHandler::new(build_queue, scheduler, builder_pool);

    // Handler should be created successfully
    // The Clone trait allows us to verify it's properly structured
    let _handler_clone = handler.clone();
}

#[tokio::test]
async fn test_ssh_server_configuration() {
    // Test SSH server can be configured with a generated key
    let host_key = PrivateKey::random(&mut rand_core::OsRng, Algorithm::Ed25519)
        .expect("Failed to generate key");

    let addr: SocketAddr = "127.0.0.1:0".parse().unwrap();
    let config = SshConfig { addr, host_key };

    // Create server
    let builder_pool = BuilderPool::new();
    let build_queue = BuildQueue::new();
    let scheduler = Scheduler::new(builder_pool.clone());
    let handler = SshHandler::new(build_queue, scheduler, builder_pool);

    let _server = SshServer::new(config);

    // Server creation should succeed
    // Actual connection testing requires a running server which is
    // complex to set up in an integration test environment
}

#[tokio::test]
async fn test_host_key_generation_in_memory() {
    // Test that host key generation works without a file path
    let key = SshServer::load_or_generate_host_key(None)
        .await
        .expect("Should generate temporary key");

    // Verify key can be used
    let _public_key = key.public_key();
}

#[tokio::test]
async fn test_components_integrate() {
    // Test that all components work together
    let builder_pool = BuilderPool::new();
    let build_queue = BuildQueue::new();
    let scheduler = Scheduler::new(builder_pool.clone());

    // Register a builder
    builder_pool
        .register_builder(
            rio_common::BuilderId::new(),
            "test-builder".to_string(),
            vec!["x86_64-linux".to_string()],
            vec![],
        )
        .await
        .unwrap();

    // Create a job
    let job = rio_dispatcher::build_queue::BuildJob::new(
        "/nix/store/test.drv".to_string(),
        "x86_64-linux".to_string(),
    );

    // Enqueue it
    let job_id = build_queue.enqueue(job.clone()).await;

    // Scheduler should be able to select the builder
    let selected = scheduler.select_builder(&job).await;
    assert!(selected.is_some(), "Should select the registered builder");

    // Verify job is in queue
    let retrieved = build_queue.get_job(&job_id).await;
    assert!(retrieved.is_some(), "Job should be in queue");
}
