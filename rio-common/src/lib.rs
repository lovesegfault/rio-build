//! Shared configuration, observability, and utility types.
//!
//! Leaf crate — no `rio-*` dependencies. Provides [`limits`] constants,
//! [`observability`] init, and gRPC/TLS/JWT plumbing shared across the
//! workspace.

pub mod backoff;
pub mod config;
pub mod grpc;
pub mod k8s;
pub mod limits;
#[cfg(feature = "postgres")]
pub mod migrate;
pub mod newtype;
pub mod observability;
#[cfg(feature = "aws")]
pub mod s3;
#[cfg(feature = "postgres")]
pub mod schema;
pub mod server;
pub mod signal;
pub mod task;
pub mod tenant;

/// Default bind address for a service port. Used in config defaults.
///
/// `[::]` (v6 unspecified) binds dual-stack on Linux's default
/// `net.ipv6.bindv6only=0` — accepts native v6 AND v4-mapped (`::ffff:a.b.c.d`).
/// One socket, both families. P0542: builders may run on v6-only pod
/// CIDR (I-073/I-079 IPv4 subnet exhaustion); the in-cluster services
/// they dial bind here and answer on whichever family the Service routes.
// r[impl common.helpers]
pub fn default_addr(port: u16) -> std::net::SocketAddr {
    (std::net::Ipv6Addr::UNSPECIFIED, port).into()
}
