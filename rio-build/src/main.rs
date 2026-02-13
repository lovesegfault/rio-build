mod gateway;
mod observability;
mod store;

use std::net::SocketAddr;
use std::path::PathBuf;
use std::sync::Arc;

use anyhow::Context;
use tracing::info;

use gateway::GatewayServer;
use store::MemoryStore;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // Configuration from env vars (simple for Phase 1a; TOML config comes later)
    let listen_addr: SocketAddr = std::env::var("RIO_LISTEN_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:2222".to_string())
        .parse()
        .context("invalid RIO_LISTEN_ADDR")?;

    let metrics_addr: SocketAddr = std::env::var("RIO_METRICS_ADDR")
        .unwrap_or_else(|_| "0.0.0.0:9090".to_string())
        .parse()
        .context("invalid RIO_METRICS_ADDR")?;

    let host_key_path = PathBuf::from(
        std::env::var("RIO_HOST_KEY").unwrap_or_else(|_| "/tmp/rio_host_key".to_string()),
    );

    let authorized_keys_path = PathBuf::from(
        std::env::var("RIO_AUTHORIZED_KEYS")
            .unwrap_or_else(|_| "/tmp/rio_authorized_keys".to_string()),
    );

    let log_format = match std::env::var("RIO_LOG_FORMAT")
        .unwrap_or_else(|_| "json".to_string())
        .as_str()
    {
        "pretty" => observability::LogFormat::Pretty,
        _ => observability::LogFormat::Json,
    };

    // Initialize logging
    observability::init_logging(log_format, None);

    // Root span carries the `component` field required by the observability spec.
    // All child spans (and their log events) inherit it automatically.
    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();

    info!(
        version = env!("CARGO_PKG_VERSION"),
        protocol_version = "1.38",
        "starting rio-build"
    );

    // Initialize metrics
    observability::init_metrics(metrics_addr)?;

    // Load SSH host key
    let host_key = gateway::load_or_generate_host_key(&host_key_path)?;

    // Load authorized keys
    let authorized_keys = gateway::load_authorized_keys(&authorized_keys_path)?;

    // Create in-memory store
    let mem_store = MemoryStore::new();
    info!(paths = mem_store.len(), "in-memory store initialized");
    let store: Arc<dyn store::Store> = Arc::new(mem_store);

    // Start SSH server
    let server = GatewayServer::new(store, authorized_keys);
    server.run(host_key, listen_addr).await?;

    Ok(())
}
