//! Observability setup: structured logging and Prometheus metrics.

use tracing_subscriber::EnvFilter;

/// Log output format.
#[derive(Debug, Clone, Copy, Default)]
pub enum LogFormat {
    /// JSON structured logs (production default).
    #[default]
    Json,
    /// Human-readable pretty-printed logs (development).
    Pretty,
}

/// Initialize the tracing subscriber with the given format and filter.
///
/// If `RUST_LOG` is set, it takes precedence over the `filter` parameter.
pub fn init_logging(format: LogFormat, filter: Option<&str>) {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| {
        filter
            .unwrap_or("info")
            .parse()
            .expect("invalid log filter")
    });

    match format {
        LogFormat::Json => {
            tracing_subscriber::fmt()
                .json()
                .with_env_filter(env_filter)
                .init();
        }
        LogFormat::Pretty => {
            tracing_subscriber::fmt()
                .pretty()
                .with_env_filter(env_filter)
                .init();
        }
    }
}

/// Initialize Prometheus metrics exporter.
///
/// This starts an HTTP server on the given address that serves `/metrics`.
pub fn init_metrics(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    let builder = metrics_exporter_prometheus::PrometheusBuilder::new();
    builder
        .with_http_listener(addr)
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus exporter: {e}"))?;

    tracing::info!(addr = %addr, "Prometheus metrics exporter started");
    Ok(())
}
