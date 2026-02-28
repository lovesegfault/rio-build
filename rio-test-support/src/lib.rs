//! Test harness for rio-build integration tests.
//!
//! - [`pg`]: ephemeral PostgreSQL bootstrap
//! - [`wire`]: Nix wire protocol client helpers (handshake, setOptions, stderr drain)
//! - [`grpc`]: mock gRPC services and server spawn helpers
//! - [`fixtures`]: NAR and PathInfo builders

pub mod fixtures;
pub mod grpc;
pub mod pg;
pub mod wire;

// Re-export the most-used type at crate root for backwards compat
pub use pg::TestDb;
