//! Test harness for rio workspace integration tests.
//!
//! - [`pg`]: ephemeral PostgreSQL bootstrap
//! - [`jail`]: process-global env/cwd test sandbox (Jail)
//! - [`wire`]: Nix wire protocol client helpers (handshake, setOptions, stderr drain)
//! - [`grpc`]: mock gRPC services and server spawn helpers
//! - [`fixtures`]: NAR and PathInfo builders
//! - [`kube_mock`]: scenario-driven mock kube::Client (tower-test)
//! - [`metrics`]: test-only `metrics::Recorder` impls (DescribedNames, CountingRecorder)
//! - [`config`]: figment::Jail standing-guard test macros (jail_roundtrip!, jail_defaults!)

// `pg` and `jail` are unconditional so xtask (default-features = false) can
// reuse them without pulling rio-nix/rio-proto/tonic/kube. Every other module
// is gated on `full` (the default).
pub mod jail;
pub mod pg;
pub use jail::Jail;

#[cfg(feature = "full")]
pub mod config;
#[cfg(feature = "full")]
pub mod fixtures;
#[cfg(feature = "full")]
pub mod grpc;
#[cfg(feature = "full")]
pub mod kube_mock;
#[cfg(feature = "full")]
pub mod metrics;
#[cfg(feature = "full")]
pub mod wire;

// Re-export at crate root — TestDb is the most-used type.
#[cfg(feature = "full")]
pub use pg::TestDb;

/// Standard return type for `#[test]` / `#[tokio::test]` bodies.
/// Lets tests use `?` instead of `.unwrap()`.
pub type TestResult = anyhow::Result<()>;

/// Idempotent tracing init for tests. `with_test_writer` routes spans
/// through libtest's capture so output only shows on failure;
/// `try_init` swallows the "already set" error so every test (or
/// fixture ctor) can call this without coordination.
///
/// `filter` is an `EnvFilter` directive string (e.g.,
/// `"rio_gateway=debug,rio_nix=debug"`). Without this, `tracing::debug!`
/// in error paths is void and failure logs are useless.
#[cfg(feature = "full")]
pub fn init_test_logging(filter: &str) {
    let _ = tracing_subscriber::fmt()
        .with_env_filter(filter)
        .with_test_writer()
        .try_init();
}
