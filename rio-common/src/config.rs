//! Service address configuration for rio-build components.

use std::net::SocketAddr;

/// Common configuration shared by all rio-build binaries.
#[derive(Debug, Clone)]
pub struct CommonConfig {
    /// Address for Prometheus metrics endpoint.
    pub metrics_addr: SocketAddr,
    /// Log format (json or pretty).
    pub log_format: crate::observability::LogFormat,
}

impl Default for CommonConfig {
    fn default() -> Self {
        Self {
            metrics_addr: "0.0.0.0:9090".parse().unwrap(),
            log_format: crate::observability::LogFormat::default(),
        }
    }
}

/// gRPC service addresses for connecting to rio-build services.
#[derive(Debug, Clone)]
pub struct ServiceAddrs {
    /// Address of the scheduler gRPC server.
    pub scheduler_addr: String,
    /// Address of the store gRPC server.
    pub store_addr: String,
}
