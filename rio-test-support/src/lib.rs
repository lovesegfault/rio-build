//! Test harness for rio workspace integration tests.
//!
//! - [`pg`]: ephemeral PostgreSQL bootstrap
//! - [`wire`]: Nix wire protocol client helpers (handshake, setOptions, stderr drain)
//! - [`grpc`]: mock gRPC services and server spawn helpers
//! - [`fixtures`]: NAR and PathInfo builders
//! - [`kube_mock`]: scenario-driven mock kube::Client (tower-test)
//! - [`metrics`]: test-only `metrics::Recorder` impls (DescribedNames, CountingRecorder)

pub mod fixtures;
pub mod grpc;
pub mod kube_mock;
pub mod metrics;
pub mod pg;
pub mod wire;

// metrics_grep.rs is include!()-ed by crate build.rs files, not a
// public module of this crate. Compile it at test time ONLY so the
// grep_spec_names self-test runs via `cargo test -p rio-test-support`.
// The #[allow(dead_code)] attributes inside handle the unused-fn
// warnings (emit_metrics_grep isn't called from tests).
#[cfg(test)]
mod metrics_grep;

// Re-export at crate root — TestDb is the most-used type.
pub use pg::TestDb;
pub use pg::{TenantSeed, seed_tenant};

/// Standard return type for `#[test]` / `#[tokio::test]` bodies.
/// Lets tests use `?` instead of `.unwrap()`.
pub type TestResult = anyhow::Result<()>;
pub use anyhow::Context;
