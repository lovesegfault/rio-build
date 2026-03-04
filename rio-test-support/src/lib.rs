//! Test harness for rio workspace integration tests.
//!
//! - [`pg`]: ephemeral PostgreSQL bootstrap
//! - [`wire`]: Nix wire protocol client helpers (handshake, setOptions, stderr drain)
//! - [`grpc`]: mock gRPC services and server spawn helpers
//! - [`fixtures`]: NAR and PathInfo builders

pub mod fixtures;
pub mod grpc;
pub mod pg;
pub mod wire;

// Re-export at crate root — TestDb is the most-used type.
pub use pg::TestDb;

/// Standard return type for `#[test]` / `#[tokio::test]` bodies.
/// Lets tests use `?` instead of `.unwrap()`.
pub type TestResult = anyhow::Result<()>;
pub use anyhow::Context;
