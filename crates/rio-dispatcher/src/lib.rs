// Public API for testing and binary usage
pub mod build_queue;
pub mod builder_pool;
pub mod dispatcher_loop;
pub mod grpc_server;
pub mod scheduler;
pub mod ssh_server;

// Internal modules
pub(crate) mod async_progress;
pub(crate) mod channel_bridge;
mod dispatcher;
pub mod nix_store; // Public for integration tests
