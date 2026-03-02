//! Observability setup: structured logging and Prometheus metrics.

use std::fmt;
use std::str::FromStr;

use tracing_subscriber::EnvFilter;

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub enum LogFormat {
    /// JSON structured logs (production default).
    #[default]
    Json,
    /// Human-readable pretty-printed logs (development).
    Pretty,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogFormat::Json => write!(f, "json"),
            LogFormat::Pretty => write!(f, "pretty"),
        }
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s {
            "json" => Ok(LogFormat::Json),
            "pretty" => Ok(LogFormat::Pretty),
            other => Err(format!(
                "unknown log format: {other:?} (expected \"json\" or \"pretty\")"
            )),
        }
    }
}

/// Initialize the tracing subscriber with the given format and filter.
///
/// If `RUST_LOG` is set, it takes precedence over the `filter` parameter.
pub fn init_logging(format: LogFormat, filter: Option<&str>) -> anyhow::Result<()> {
    let env_filter = match EnvFilter::try_from_default_env() {
        Ok(f) => f,
        Err(_) => filter.unwrap_or("info").parse().map_err(|e| {
            anyhow::anyhow!("invalid log filter {:?}: {e}", filter.unwrap_or("info"))
        })?,
    };

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
    Ok(())
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

/// Parse `RIO_LOG_FORMAT` environment variable, defaulting to JSON.
pub fn log_format_from_env() -> LogFormat {
    match std::env::var("RIO_LOG_FORMAT") {
        Ok(val) => match val.parse() {
            Ok(fmt) => fmt,
            Err(_) => {
                eprintln!(
                    "warning: invalid RIO_LOG_FORMAT={val:?}, valid options are 'json' or 'pretty'; defaulting to json"
                );
                LogFormat::default()
            }
        },
        Err(_) => LogFormat::default(),
    }
}

/// Initialize logging from `RIO_LOG_FORMAT` env var (default: JSON).
/// Convenience wrapper for `init_logging(log_format_from_env(), None)`.
pub fn init_from_env() -> anyhow::Result<()> {
    init_logging(log_format_from_env(), None)
}
