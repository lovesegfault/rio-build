//! rio-gateway binary entry point.
//!
//! Connects to the scheduler and store gRPC services, then starts an
//! SSH server that speaks the Nix worker protocol.

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::info;

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
//
// Two-struct split per rio-common/src/config.rs module docs:
//   - `Config`: merged result, all fields concrete, Default = compiled-in
//     defaults (must match the old clap `default_value=` values exactly —
//     see `tests::config_defaults_match_phase2a`).
//   - `CliArgs`: clap-parsed, all fields Option, no env= (figment's Env
//     provider handles that), no default_value (absence = None = don't
//     overlay).

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: std::net::SocketAddr,
    scheduler_addr: String,
    store_addr: String,
    host_key: std::path::PathBuf,
    authorized_keys: std::path::PathBuf,
    metrics_addr: std::net::SocketAddr,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:2222".parse().unwrap(),
            // scheduler_addr/store_addr have no sensible default — they're
            // deployment-specific. Empty string here + a post-load check in
            // main() gives a clear "required" error that names the field.
            scheduler_addr: String::new(),
            store_addr: String::new(),
            host_key: "/tmp/rio_host_key".into(),
            authorized_keys: "/tmp/rio_authorized_keys".into(),
            metrics_addr: "0.0.0.0:9090".parse().unwrap(),
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-gateway",
    about = "SSH gateway and Nix protocol frontend for rio-build"
)]
struct CliArgs {
    /// SSH listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<std::net::SocketAddr>,

    /// rio-scheduler gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

    /// SSH host key path
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    host_key: Option<std::path::PathBuf>,

    /// Authorized keys file path
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    authorized_keys: Option<std::path::PathBuf>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("gateway", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("gateway")?;

    // Required-field checks that `#[serde(default)]` can't express
    // (figment's "missing field" error for `String` defaulting to `""`
    // is a silent success, not an error).
    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or gateway.toml)"
    );

    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-gateway");

    rio_common::observability::init_metrics(cfg.metrics_addr)?;

    // Connect to gRPC services
    info!(addr = %cfg.store_addr, "connecting to store service");
    let store_client = rio_proto::client::connect_store(&cfg.store_addr).await?;

    info!(addr = %cfg.scheduler_addr, "connecting to scheduler service");
    let scheduler_client = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;

    // Load SSH keys
    let host_key = rio_gateway::load_or_generate_host_key(&cfg.host_key)?;
    let authorized_keys = rio_gateway::load_authorized_keys(&cfg.authorized_keys)?;

    // Start SSH server
    let server = rio_gateway::GatewayServer::new(store_client, scheduler_client, authorized_keys);

    info!(
        listen_addr = %cfg.listen_addr,
        scheduler_addr = %cfg.scheduler_addr,
        store_addr = %cfg.store_addr,
        "rio-gateway ready"
    );

    server.run(host_key, cfg.listen_addr).await?;

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: `Config::default()` must exactly match the old
    /// clap `#[arg(default_value = ...)]` values from phase 2a. A silent
    /// drift here would change behavior under VM tests (which set only the
    /// required fields via env and rely on defaults for the rest).
    #[test]
    fn config_defaults_match_phase2a() {
        let d = Config::default();
        assert_eq!(d.listen_addr.to_string(), "0.0.0.0:2222");
        assert_eq!(d.host_key, std::path::PathBuf::from("/tmp/rio_host_key"));
        assert_eq!(
            d.authorized_keys,
            std::path::PathBuf::from("/tmp/rio_authorized_keys")
        );
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9090");
        // scheduler_addr/store_addr had no default in phase2a (required).
        assert!(d.scheduler_addr.is_empty());
        assert!(d.store_addr.is_empty());
    }

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
