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
//     defaults (see `tests::config_defaults_are_stable`).
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
    /// gRPC health check listen address. The gateway's main protocol is
    /// SSH (russh), not gRPC, so we can't piggyback health on an
    /// existing tonic server. This spawns a dedicated one with ONLY
    /// `grpc.health.v1.Health`. K8s readinessProbe hits this port.
    ///
    /// Separate from metrics_addr: metrics is HTTP (Prometheus scrape),
    /// health is gRPC. Could multiplex with tonic's accept_http1 +
    /// a route, but that's more complexity than a second listener.
    health_addr: std::net::SocketAddr,
    /// mTLS: client cert + key + CA for outgoing gRPC connections
    /// (scheduler + store). Set via `RIO_TLS__CERT_PATH` etc. Unset
    /// = plaintext (dev mode). The gateway's INCOMING connections
    /// are SSH — TLS doesn't apply there.
    tls: rio_common::tls::TlsConfig,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:2222".parse().unwrap(),
            // scheduler_addr/store_addr/host_key/authorized_keys have no
            // sensible default — they're deployment-specific. Empty here +
            // a post-load check in main() gives a clear "required" error
            // that names the field. The old /tmp/rio_* defaults were
            // footguns: silent key-generation in world-writable /tmp.
            scheduler_addr: String::new(),
            store_addr: String::new(),
            host_key: std::path::PathBuf::new(),
            authorized_keys: std::path::PathBuf::new(),
            metrics_addr: "0.0.0.0:9090".parse().unwrap(),
            // 9190 = gateway's metrics port (9090) + 100. Scheduler
            // health piggybacks on its gRPC port (9001), store on 9002.
            // Gateway has no gRPC port so needs its own. The +100
            // pattern keeps it discoverable without a doc lookup.
            health_addr: "0.0.0.0:9190".parse().unwrap(),
            tls: rio_common::tls::TlsConfig::default(),
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

    /// gRPC health check listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    health_addr: Option<std::net::SocketAddr>,
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider MUST be installed before any TLS use.
    // tonic's tls-aws-lc feature enables aws-lc-rs; without
    // this install_default, rustls can't auto-select and panics on
    // first handshake. Gateway's outgoing gRPC (scheduler + store)
    // is the TLS user here — incoming is SSH.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("gateway", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("gateway")?;

    // Initialize the process-global client TLS config BEFORE any
    // connect_* call. None (TLS unconfigured) → plaintext. Some →
    // https + client cert. One config serves all outgoing
    // connections: server_name is scoped to the target DNS name,
    // not the client identity. cert-manager's per-component
    // Certificates all chain to the same CA, so the gateway's
    // client cert verifies against scheduler's and store's
    // client_ca_root identically.
    //
    // server_name here is a DEFAULT for the most common target. For
    // a gateway connecting to scheduler + store, the domain_name
    // in ClientTlsConfig needs to match EACH target's cert SAN.
    // We can't set per-target in a single ClientTlsConfig — but
    // tonic's domain_name is overridden by the `:authority` header
    // (which is the hostname from the endpoint URL). So as long as
    // we connect to `rio-scheduler:9001` (hostname), rustls verifies
    // against THAT SAN. The domain_name in ClientTlsConfig is a
    // fallback when `:authority` is absent (IP-literal endpoints).
    //
    // With K8s DNS addressing (`rio-scheduler`, `rio-store`), the
    // authority matches the SAN, so any domain_name works. We pick
    // "rio-scheduler" as a reasonable default (gateway→scheduler is
    // the most-used link).
    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );
    if cfg.tls.is_configured() {
        info!("client mTLS enabled for outgoing gRPC");
    }

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
    anyhow::ensure!(
        !cfg.host_key.as_os_str().is_empty(),
        "host_key is required (set --host-key, RIO_HOST_KEY, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.authorized_keys.as_os_str().is_empty(),
        "authorized_keys is required (set --authorized-keys, RIO_AUTHORIZED_KEYS, or gateway.toml)"
    );

    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-gateway");

    // Graceful shutdown: cancelled on SIGTERM/SIGINT. Lets main()
    // return normally so atexit handlers (LLVM coverage profraw flush,
    // tracing shutdown) fire. Health server gets serve_with_shutdown;
    // the SSH server.run() is wrapped in a select! below.
    let shutdown = rio_common::signal::shutdown_signal();

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_gateway::describe_metrics();

    info!(addr = %cfg.store_addr, "connecting to store service");
    let store_client = rio_proto::client::connect_store(&cfg.store_addr).await?;

    info!(addr = %cfg.scheduler_addr, "connecting to scheduler service");
    let scheduler_client = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;

    // gRPC health server. Dedicated tonic instance — the gateway's main
    // protocol is SSH (russh), no existing tonic server to attach to.
    //
    // SERVING gate: both gRPC connects above succeeded. That's the right
    // signal — a gateway that can't reach the scheduler would accept SSH
    // connections and then hang every wopBuild* opcode. Better to fail
    // readiness so K8s doesn't route to this pod until it's actually
    // usable. store/scheduler connect are `.await?` (fail-fast) so by
    // the time we're here, both are up.
    //
    // spawn_monitored: the SSH server's `.run().await` below blocks
    // forever. Health must run concurrently. If the health server dies
    // (port conflict, etc), spawn_monitored logs it — K8s readiness
    // probe starts failing, pod restarts. Self-healing.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();
    // Generic param: we don't have a "GatewayService" proto. Use the
    // tonic-health server's own type as a stand-in — the empty-string
    // "whole server" health check (which K8s sends) doesn't care about
    // the service name, it just wants ANY service marked SERVING.
    health_reporter
        .set_serving::<tonic_health::pb::health_server::HealthServer<
            tonic_health::server::HealthService,
        >>()
        .await;
    let health_addr = cfg.health_addr;
    let health_shutdown = shutdown.clone();
    rio_common::task::spawn_monitored("health-server", async move {
        info!(addr = %health_addr, "starting gRPC health server");
        if let Err(e) = tonic::transport::Server::builder()
            .add_service(health_service)
            .serve_with_shutdown(health_addr, health_shutdown.cancelled_owned())
            .await
        {
            tracing::error!(error = %e, "health server failed");
        }
    });

    let host_key = rio_gateway::load_or_generate_host_key(&cfg.host_key)?;
    let authorized_keys = rio_gateway::load_authorized_keys(&cfg.authorized_keys)?;

    let server = rio_gateway::GatewayServer::new(store_client, scheduler_client, authorized_keys);

    info!(
        listen_addr = %cfg.listen_addr,
        scheduler_addr = %cfg.scheduler_addr,
        store_addr = %cfg.store_addr,
        "rio-gateway ready"
    );

    // Race the SSH server against shutdown. Dropping the run() future
    // cancels the accept loop; per-session tasks die at process exit.
    // Clean enough for graceful shutdown + coverage flush.
    tokio::select! {
        r = server.run(host_key, cfg.listen_addr) => r?,
        _ = shutdown.cancelled() => {
            info!("gateway shut down cleanly");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: a silent drift in `Config::default()` would change
    /// behavior under VM tests (which set only the required fields via env
    /// and rely on defaults for the rest).
    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr.to_string(), "0.0.0.0:2222");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9090");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9190");
        // All four of these are deployment-specific: empty default + a
        // post-load ensure! in main(). Non-empty default here = silent
        // wrong-value at runtime instead of a clear startup error.
        assert!(d.scheduler_addr.is_empty());
        assert!(d.store_addr.is_empty());
        assert!(d.host_key.as_os_str().is_empty());
        assert!(d.authorized_keys.as_os_str().is_empty());
    }

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
