//! Layered configuration: compiled defaults → TOML file → `RIO_*` env → CLI flags.
//!
//! Precedence (highest wins): CLI > env > TOML > compiled defaults.
//! Env vars use `RIO_` prefix with `__` for nesting (`RIO_STORE__S3_BUCKET`
//! sets `store.s3_bucket`). Per `docs/src/configuration.md:3-5`.
//!
//! # How binaries wire this up
//!
//! Each binary's `main.rs` defines two structs:
//!
//! 1. `Config` — `#[derive(Serialize, Deserialize)]`, all fields are
//!    concrete `T` (not `Option<T>`), with `#[serde(default)]` or
//!    `#[serde(default = "fn")]` for every field. This is the merged
//!    result that the rest of `main()` consumes. `Default` must give
//!    the same values the old clap `#[arg(default_value = ...)]` did.
//!
//! 2. `CliArgs` — `#[derive(Parser, Serialize)]`, all fields are
//!    `Option<T>`, NO `env =` attribute (figment's `Env` provider
//!    replaces it), NO `default_value` (absence = `None`). Every field
//!    has `#[serde(skip_serializing_if = "Option::is_none")]` so that
//!    unset CLI flags don't overwrite lower layers with `null`.
//!
//! `main()` then does:
//!
//! ```ignore
//! let cli = CliArgs::parse();
//! let cfg: Config = rio_common::config::load("scheduler", cli)?;
//! ```
//!
//! # Why two structs instead of one
//!
//! Clap's `Option<T>` means "optional" but serde's `Option<T>` means
//! "nullable". If `CliArgs` and `Config` were the same struct with
//! `Option<T>` fields, we'd have to unwrap everywhere. If they were the
//! same struct with `T` fields + `default_value`, clap always sets a
//! value and CLI would ALWAYS win, defeating the layering.
//!
//! The two-struct split makes each side clean: clap sees Optional,
//! serde sees Required-With-Default, and figment bridges them.

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Serialize, de::DeserializeOwned};

/// Load configuration for `component` with the full precedence chain.
///
/// Search paths for TOML (first found wins; missing = skipped, not error):
/// 1. `/etc/rio/{component}.toml` (system-wide, typically from NixOS module)
/// 2. `./{component}.toml` (cwd, for local dev)
///
/// `cli_overlay` is the clap-parsed `CliArgs` struct. Its `None` fields
/// are skipped during serialization (see module docs), so only explicitly
/// passed CLI flags overlay the lower layers.
///
/// # Errors
///
/// - TOML parse error (malformed file). Missing file is NOT an error.
/// - Type mismatch at merge time (e.g., env var `RIO_FOO=notanumber`
///   for a numeric field).
/// - Required field (no `#[serde(default)]`) missing from every layer.
///
/// The error message includes which provider layer the failure came from.
pub fn load<C, O>(component: &str, cli_overlay: O) -> anyhow::Result<C>
where
    C: DeserializeOwned + Default + Serialize,
    O: Serialize,
{
    Figment::from(Serialized::defaults(C::default()))
        .merge(Toml::file(format!("/etc/rio/{component}.toml")))
        .merge(Toml::file(format!("{component}.toml")))
        // Env::split("__") turns RIO_STORE__S3_BUCKET into store.s3_bucket.
        // This matches configuration.md's spec. Note: figment lowercases the
        // env var key after stripping the prefix, so RIO_LISTEN_ADDR maps
        // to `listen_addr` in the Config struct.
        .merge(Env::prefixed("RIO_").split("__"))
        // CLI last = highest precedence. Serialized::defaults on a struct
        // whose None fields skip_serializing means only set flags land.
        .merge(Serialized::defaults(cli_overlay))
        .extract()
        .map_err(|e| anyhow::anyhow!("config load for {component:?} failed: {e}"))
}

/// Same as [`load`] but with an explicit TOML file path (for tests).
/// Skips the `/etc/rio/` and cwd search paths entirely.
#[cfg(test)]
fn load_from_path<C, O>(toml_path: &std::path::Path, cli_overlay: O) -> anyhow::Result<C>
where
    C: DeserializeOwned + Default + Serialize,
    O: Serialize,
{
    Figment::from(Serialized::defaults(C::default()))
        .merge(Toml::file(toml_path))
        .merge(Env::prefixed("RIO_").split("__"))
        .merge(Serialized::defaults(cli_overlay))
        .extract()
        .map_err(anyhow::Error::from)
}

#[cfg(test)]
// figment::Jail::expect_with's closure returns Result<(), figment::Error>
// where figment::Error is 208 bytes. That's figment's API — the closure
// signature is fixed by the library, we can't box the error type. The lint
// is about return-value copy cost on the Err path; in a test module that
// only runs in CI, the ~200-byte copy is irrelevant.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;
    use serde::{Deserialize, Serialize};
    use std::io::Write;

    /// Test config shape mirroring a real binary Config.
    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct TestConfig {
        #[serde(default = "default_listen")]
        listen_addr: String,
        #[serde(default = "default_port")]
        port: u16,
        #[serde(default)]
        debug: bool,
        #[serde(default)]
        nested: NestedConfig,
    }

    #[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
    struct NestedConfig {
        #[serde(default)]
        s3_bucket: String,
    }

    fn default_listen() -> String {
        "0.0.0.0:9000".into()
    }
    fn default_port() -> u16 {
        8080
    }

    impl Default for TestConfig {
        fn default() -> Self {
            Self {
                listen_addr: default_listen(),
                port: default_port(),
                debug: false,
                nested: NestedConfig::default(),
            }
        }
    }

    /// CLI overlay shape: all Option, skip_serializing_if = "Option::is_none".
    #[derive(Serialize, Default)]
    struct TestCli {
        #[serde(skip_serializing_if = "Option::is_none")]
        listen_addr: Option<String>,
        #[serde(skip_serializing_if = "Option::is_none")]
        port: Option<u16>,
        #[serde(skip_serializing_if = "Option::is_none")]
        debug: Option<bool>,
    }

    /// Write TOML to a tempfile and return the handle (file lives until dropped).
    fn write_toml(content: &str) -> tempfile::NamedTempFile {
        let mut f = tempfile::NamedTempFile::new().unwrap();
        f.write_all(content.as_bytes()).unwrap();
        f.flush().unwrap();
        f
    }

    /// SAFETY NOTE: These tests use `figment::Jail` which manipulates the
    /// process-global environment and cwd. Jail serializes itself via a
    /// global mutex AND restores env+cwd on drop, so under cargo-test's
    /// single-process concurrent threads this is sound — but only because
    /// all env-manipulating tests in this crate go through Jail. If any
    /// other test in rio-common directly calls std::env::set_var, it would
    /// race with these. nextest (per-test process) sidesteps the issue
    /// entirely for CI.

    #[test]
    fn defaults_only_no_toml_no_env_no_cli() {
        figment::Jail::expect_with(|_jail| {
            // Nonexistent TOML path → falls through to Default.
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), TestCli::default()).unwrap();
            assert_eq!(cfg, TestConfig::default());
            Ok(())
        });
    }

    #[test]
    fn toml_overrides_defaults() {
        figment::Jail::expect_with(|_jail| {
            let f = write_toml(
                r#"
                listen_addr = "1.2.3.4:5000"
                port = 9999
                "#,
            );
            let cfg: TestConfig = load_from_path(f.path(), TestCli::default()).unwrap();
            assert_eq!(cfg.listen_addr, "1.2.3.4:5000");
            assert_eq!(cfg.port, 9999);
            assert!(!cfg.debug, "debug not in TOML → falls through to default");
            Ok(())
        });
    }

    #[test]
    fn env_overrides_toml() {
        figment::Jail::expect_with(|jail| {
            let f = write_toml(r#"port = 1111"#);
            jail.set_env("RIO_PORT", "2222");
            let cfg: TestConfig = load_from_path(f.path(), TestCli::default()).unwrap();
            assert_eq!(cfg.port, 2222, "env must override TOML");
            Ok(())
        });
    }

    #[test]
    fn cli_overrides_env() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_PORT", "3333");
            let cli = TestCli {
                port: Some(4444),
                ..Default::default()
            };
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), cli).unwrap();
            assert_eq!(cfg.port, 4444, "CLI must override env");
            Ok(())
        });
    }

    #[test]
    fn cli_none_does_not_override_lower_layers() {
        // Core regression guard: an unset CLI flag must NOT clobber a
        // value set by env/TOML. This is WHY skip_serializing_if matters.
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_PORT", "5555");
            let cli = TestCli {
                port: None, // ← NOT passed on CLI
                ..Default::default()
            };
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), cli).unwrap();
            assert_eq!(
                cfg.port, 5555,
                "unset CLI flag must not override env; got {}",
                cfg.port
            );
            Ok(())
        });
    }

    #[test]
    fn env_double_underscore_nesting() {
        // RIO_NESTED__S3_BUCKET → nested.s3_bucket per configuration.md:3.
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_NESTED__S3_BUCKET", "my-bucket");
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), TestCli::default()).unwrap();
            assert_eq!(cfg.nested.s3_bucket, "my-bucket");
            Ok(())
        });
    }

    #[test]
    fn full_precedence_chain() {
        // Defaults < TOML < env < CLI, each layer visible where the
        // higher layers don't set a value.
        figment::Jail::expect_with(|jail| {
            let f = write_toml(
                r#"
                listen_addr = "toml-addr"
                port = 1000
                debug = true
                "#,
            );
            jail.set_env("RIO_PORT", "2000"); // overrides TOML's 1000
            let cli = TestCli {
                debug: Some(false), // overrides TOML's true
                ..Default::default()
            };
            let cfg: TestConfig = load_from_path(f.path(), cli).unwrap();
            assert_eq!(cfg.listen_addr, "toml-addr", "TOML, nothing above it");
            assert_eq!(cfg.port, 2000, "env overrode TOML");
            assert!(!cfg.debug, "CLI overrode TOML");
            Ok(())
        });
    }

    #[test]
    fn malformed_toml_errors_with_layer_attribution() {
        figment::Jail::expect_with(|_jail| {
            let f = write_toml("this is not = = valid toml [[");
            let err = load_from_path::<TestConfig, _>(f.path(), TestCli::default()).unwrap_err();
            let msg = err.to_string();
            // Figment puts the source (file path or provider name) in the error.
            assert!(
                msg.contains("TOML") || msg.to_lowercase().contains("parse"),
                "error should mention TOML/parse failure: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    fn env_type_mismatch_errors() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_PORT", "not-a-number");
            let err = load_from_path::<TestConfig, _>(
                std::path::Path::new("/nonexistent"),
                TestCli::default(),
            )
            .unwrap_err();
            let msg = err.to_string();
            assert!(
                msg.contains("port") || msg.contains("PORT"),
                "error should mention the failing field: {msg}"
            );
            Ok(())
        });
    }

    #[test]
    fn missing_toml_file_is_not_an_error() {
        // This is the common case — no config file deployed, all config
        // via env vars (e.g., NixOS modules set env vars). Must not fail.
        figment::Jail::expect_with(|_jail| {
            let cfg: TestConfig = load_from_path(
                std::path::Path::new("/definitely/not/there"),
                TestCli::default(),
            )
            .unwrap();
            assert_eq!(cfg, TestConfig::default());
            Ok(())
        });
    }

    #[test]
    fn bool_env_var_true_and_false() {
        // Figment's Env provider parses bool-ish strings. Guard: make sure
        // "true"/"false" actually work (important for RIO_FUSE_PASSTHROUGH).
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_DEBUG", "true");
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), TestCli::default()).unwrap();
            assert!(cfg.debug);
            Ok(())
        });
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_DEBUG", "false");
            let cfg: TestConfig =
                load_from_path(std::path::Path::new("/nonexistent"), TestCli::default()).unwrap();
            assert!(!cfg.debug);
            Ok(())
        });
    }

    /// The real `load()` (not `load_from_path`) searches /etc/rio and cwd.
    /// In CI sandboxes neither exists — must still succeed with defaults.
    #[test]
    fn prod_load_with_no_toml_anywhere() {
        figment::Jail::expect_with(|_jail| {
            // "rio-test-nonexistent-component" → neither /etc/rio/... nor
            // ./rio-test-...toml exists. Should fall through to defaults.
            let cfg: TestConfig =
                load("rio-test-nonexistent-component", TestCli::default()).unwrap();
            assert_eq!(cfg, TestConfig::default());
            Ok(())
        });
    }
}
