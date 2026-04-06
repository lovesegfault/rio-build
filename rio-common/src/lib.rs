//! Shared configuration, observability, and utility types.
//!
//! Leaf crate — no `rio-*` dependencies. Provides the `string_newtype!` macro
//! and [`DrvHash`](newtype::DrvHash) / [`ExecutorId`](newtype::ExecutorId) shared
//! across the workspace, plus [`limits`] constants, [`observability`] init,
//! and the self-describing [`BloomFilter`](bloom::BloomFilter).

pub mod bloom;
pub mod config;
pub mod grpc;
pub mod hmac;
pub mod jwt;
pub mod jwt_interceptor;
pub mod limits;
pub mod newtype;
pub mod observability;
pub mod server;
pub mod signal;
pub mod task;
pub mod tenant;
pub mod tls;

/// Default bind address for a service port. Used in config defaults.
///
/// Replaces the `"0.0.0.0:PORT".parse().unwrap()` idiom that was scattered
/// across every binary's `Config::default()` — typo-prone and opaque to
/// refactoring. Centralizing here also makes a future IPv6 dual-stack flip
/// (`[::]:PORT`) a one-line change.
pub fn default_addr(port: u16) -> std::net::SocketAddr {
    ([0, 0, 0, 0], port).into()
}
