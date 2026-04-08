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
//!
//! # Standing-guard tests
//!
//! Each binary's `main.rs` (or `config.rs` for rio-builder) carries a
//! pair of `figment::Jail` tests via the `rio_test_support::jail_roundtrip!`
//! / `jail_defaults!` macros. See `rio-test-support/src/config.rs` for
//! the pattern rationale and the P0219 failure mode that motivated it.

use std::path::PathBuf;
use std::str::FromStr;

use figment::{
    Figment,
    providers::{Env, Format, Serialized, Toml},
};
use serde::{Deserialize, Serialize, de::DeserializeOwned};

/// Serde adapter: `Duration` ⇄ integer seconds. Lets a `Config` field
/// be a native [`std::time::Duration`] while keeping the wire format a
/// plain `u64` (TOML `foo_secs = 6`, env `RIO_FOO_SECS=6`).
///
/// ```ignore
/// #[serde(rename = "tick_interval_secs", with = "rio_common::config::secs")]
/// tick_interval: Duration,
/// ```
///
/// The `rename` keeps the on-disk key suffixed `_secs` so the unit is
/// self-documenting in TOML and the env var name stays stable across
/// the `u64 → Duration` field migration.
pub mod secs {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        d.as_secs().serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_secs)
    }
}

/// Serde adapter: `Duration` ⇄ integer milliseconds. Same shape as
/// [`secs`]; use for sub-second knobs (`*_ms` fields).
pub mod millis {
    use std::time::Duration;

    use serde::{Deserialize, Deserializer, Serialize, Serializer};

    pub fn serialize<S: Serializer>(d: &Duration, s: S) -> Result<S::Ok, S::Error> {
        u64::try_from(d.as_millis())
            .map_err(serde::ser::Error::custom)?
            .serialize(s)
    }
    pub fn deserialize<'de, D: Deserializer<'de>>(d: D) -> Result<Duration, D::Error> {
        u64::deserialize(d).map(Duration::from_millis)
    }
}

/// Read an env var with a typed fallback. For the handful of
/// bootstrap-time reads that run BEFORE [`load`] (tracing init,
/// observability) or that live in leaf crates with no `Config` struct.
///
/// Unset OR parse-failure → `default`. A bad value is logged to
/// stderr (tracing may not be initialized yet) so the operator sees
/// the fallback was taken.
///
/// Prefer a `Config` field over this for anything that can wait until
/// after figment load — this exists only for the chicken-and-egg
/// cases (`RIO_LOG_FORMAT`, `RIO_OTEL_*`) and test hooks.
pub fn env_or<T: FromStr>(name: &str, default: T) -> T {
    match std::env::var(name) {
        Ok(v) => v.parse().unwrap_or_else(|_| {
            eprintln!("warning: invalid {name}={v:?}; using default");
            default
        }),
        Err(_) => default,
    }
}

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

/// Validate a required string config field is non-empty (after trim).
///
/// Returns `Ok(())` if `value.trim()` is non-empty, `Err(anyhow)` with
/// a standardized message otherwise. The CLI flag and env var names
/// are derived from `field` by convention: `field_name` → flag
/// `--field-name`, env `RIO_FIELD_NAME`. All 10 call-sites across
/// the 5 binaries follow this convention today; if a field ever
/// diverges (unlikely — clap's `#[arg(long)]` and figment's
/// `Env::prefixed("RIO_")` both derive the same way), add a sibling
/// with explicit args rather than loosening this one.
///
/// DRYs the 10× identical `ensure!(!field.is_empty(), "X is required
/// (set --flag, RIO_ENV, or crate.toml)")` template spread across 5
/// crates' `validate_config()` — P0416 spread it 4× (pre-P0416, only
/// scheduler had 2); P0425 consolidates + adds trim.
///
/// The trim catches `RIO_FOO="  "` whitespace-typo — pre-helper, bare
/// `is_empty()` accepted it, startup failed with a cryptic tcp-connect
/// "invalid socket address syntax" buried in logs. Rejecting at
/// config-load puts the clear "X is required" message at the top of
/// the startup log instead.
///
/// The trim is validation-only: the HELPER does not return the
/// trimmed value, callers continue to use the original `value` (which
/// may still have leading/trailing whitespace). gRPC endpoint parsing
/// tolerates leading/trailing whitespace; if a consumer surfaces that
/// does not, switch the Config field's type to apply trim at
/// deserialize-time instead of layering it on here.
pub fn ensure_required(value: &str, field: &str, component: &str) -> anyhow::Result<()> {
    anyhow::ensure!(
        !value.trim().is_empty(),
        "{field} is required (set --{flag}, RIO_{env}, or {component}.toml)",
        flag = field.replace('_', "-"),
        env = field.to_uppercase(),
    );
    Ok(())
}

/// [`ensure_required`] for `PathBuf` fields. Same trim-then-empty
/// check on the path's string form (via `to_string_lossy` — non-UTF-8
/// paths are vanishingly unlikely for operator-set config, and would
/// fail downstream anyway). The gateway's `host_key` /
/// `authorized_keys` are the 2 `PathBuf` sites of the 10.
pub fn ensure_required_path(
    value: &std::path::Path,
    field: &str,
    component: &str,
) -> anyhow::Result<()> {
    // Lossy is fine here: this is a trim-then-empty check for operator-
    // facing error messages, not a parse path. A non-UTF-8 config path
    // would fail at file-open anyway — the empty-check is the point.
    #[allow(clippy::disallowed_methods)]
    ensure_required(&value.to_string_lossy(), field, component)
}

/// Startup-time bounds checks on operator-settable config fields.
///
/// Each binary implements this for its `Config` struct. The validation
/// body was previously a free `fn validate_config(cfg: &Config)` per
/// binary ("the P0409 pattern") — the trait unifies the contract and
/// moves the doc of the scrutiny recipe to one place (below), so
/// `validate_config` references across main.rs files no longer need to
/// cross-link to rio-scheduler.
///
/// Scrutiny recipe when wiring a new config field:
/// - grep for `interval(..<field>)` / `from_secs(<field>)` /
///   `random_range(..<field>)` in consumer code
/// - check what happens at 0, negative, very-large, NaN/inf
/// - add an `ensure!` here + a rejection test in the crate's `cfg(test)` mod
///
/// Pair with [`ensure_required`] / [`ensure_required_path`] for the
/// `#[serde(default)]` string fields that have no sensible compiled
/// default (deployment-specific addrs, paths).
pub trait ValidateConfig {
    /// Run all bounds / required-field checks. Called immediately after
    /// [`load`] in each binary's `main()`.
    fn validate(&self) -> anyhow::Result<()>;
}

/// JWT dual-mode configuration. Nested in each binary's `Config` as
/// `jwt: JwtConfig`. Env vars: `RIO_JWT__REQUIRED=true` etc.
///
/// Dual-mode is PERMANENT: both the JWT path and the SSH-comment
/// fallback stay maintained forever. `jwt_required` is the operator's
/// per-deployment switch — true = STRICT (reject on mint/verify
/// failure), false = PERMISSIVE (fall back to tenant_name on failure).
///
/// Gateway interpretation: `required=true` → if ResolveTenant fails or
/// times out during SSH auth, reject the connection (no JWT can be
/// minted → no tenant identity → unauthenticated). `required=false` →
/// degrade to tenant_name-only (jwt_token=None, downstream falls back).
///
/// Scheduler/store interpretation: handled at the HANDLER level (not
/// the interceptor — the interceptor stays permissive-on-absent-header
/// for workers/health probes regardless). A handler that needs strict
/// JWT reads `required` from its own config and checks for Claims
/// extension presence.
#[derive(Debug, Clone, PartialEq, Eq, Serialize, Deserialize)]
#[serde(default)]
pub struct JwtConfig {
    /// true = JWT mint/verify failure → reject. false (default) =
    /// fall back to SSH-comment tenant_name path.
    ///
    /// Default false: existing deployments (no JWT Secret mounted)
    /// keep working. Setting `RIO_JWT__REQUIRED=true` without also
    /// mounting the signing key → gateway rejects every SSH connect
    /// at auth time (clear failure, not silent degradation).
    pub required: bool,

    /// Path to the ed25519 signing seed (gateway) or pubkey
    /// (scheduler/store). K8s: Secret mount at
    /// `/etc/rio/jwt/ed25519_seed` or ConfigMap mount at
    /// `/etc/rio/jwt/ed25519_pubkey`. `None` → JWT disabled for
    /// this process (matches the `Option<SigningKey>` /
    /// `Option<Arc<RwLock<VerifyingKey>>>` pattern in jwt.rs /
    /// jwt_interceptor.rs).
    pub key_path: Option<PathBuf>,

    /// ResolveTenant RPC timeout (gateway only). The round-trip to
    /// the scheduler happens in the SSH auth hot path — every
    /// connect, once. If the scheduler is slow (PG under load) or
    /// unreachable, this bounds the auth-time latency penalty.
    /// Default 500ms: long enough for a warm PG lookup + RPC
    /// overhead, short enough that a stuck scheduler doesn't make
    /// SSH auth hang noticeably. On timeout: `required=false` →
    /// degrade; `required=true` → reject.
    #[serde(default = "default_resolve_timeout_ms")]
    pub resolve_timeout_ms: u64,
}

fn default_resolve_timeout_ms() -> u64 {
    500
}

impl Default for JwtConfig {
    /// Must match the `#[serde(default = ...)]` attrs above. figment's
    /// `load()` starts from `Serialized::defaults(C::default())` — if
    /// this impl and serde's field-defaults diverge, the BASE layer
    /// (used when no TOML/env/CLI) disagrees with the partial-override
    /// layer (used when some fields are set). The `jwt_config_defaults`
    /// test pins both paths at once.
    fn default() -> Self {
        Self {
            required: false,
            key_path: None,
            resolve_timeout_ms: default_resolve_timeout_ms(),
        }
    }
}

/// Deserialize `Vec<String>` from EITHER a comma-separated string
/// (env var layer) OR a sequence (TOML layer). Figment's `Env`
/// provider gives strings; `Toml` gives sequences. A bare
/// `Vec<String>` field fails on the env layer ("invalid type:
/// string, expected a sequence"). This visitor bridges both.
///
/// Usage on a Config field:
///
/// ```ignore
/// #[serde(default, deserialize_with = "rio_common::config::comma_vec")]
/// systems: Vec<String>,
/// ```
///
/// Empty string → empty vec (not `[""]`). Leading/trailing
/// whitespace in each element is trimmed. Empty elements (e.g.,
/// `"a,,b"` → `["a","b"]`) are dropped — an accidental trailing
/// comma shouldn't produce a spurious empty feature/system name.
pub fn comma_vec<'de, D>(d: D) -> Result<Vec<String>, D::Error>
where
    D: serde::Deserializer<'de>,
{
    struct CommaVecVisitor;

    impl<'de> serde::de::Visitor<'de> for CommaVecVisitor {
        type Value = Vec<String>;

        fn expecting(&self, f: &mut std::fmt::Formatter) -> std::fmt::Result {
            f.write_str("a comma-separated string or a sequence of strings")
        }

        // Env provider path: "x86_64-linux,aarch64-linux" → vec!
        fn visit_str<E: serde::de::Error>(self, s: &str) -> Result<Self::Value, E> {
            Ok(s.split(',')
                .map(str::trim)
                .filter(|s| !s.is_empty())
                .map(String::from)
                .collect())
        }

        // TOML provider path: ["x86_64-linux", "aarch64-linux"]
        fn visit_seq<A: serde::de::SeqAccess<'de>>(
            self,
            mut seq: A,
        ) -> Result<Self::Value, A::Error> {
            let mut out = Vec::with_capacity(seq.size_hint().unwrap_or(0));
            while let Some(s) = seq.next_element::<String>()? {
                out.push(s);
            }
            Ok(out)
        }
    }

    d.deserialize_any(CommaVecVisitor)
}

/// Redact the password component of a database URL for safe logging.
///
/// `postgres://user:SECRET@host:5432/db` → `postgres://user:***@host:5432/db`.
///
/// Falls back to `"<redacted>"` if the URL doesn't parse in the
/// expected `scheme://[user[:pass]@]host[...]` shape — better to
/// over-redact than leak a password from an unusual format.
///
/// Without redaction, logging `cfg.database_url` at INFO exposes
/// PG credentials to anyone who can read pod logs (`kubectl logs`,
/// log aggregators).
pub fn redact_db_url(url: &str) -> String {
    // Find scheme://. If absent, not a URL we recognize.
    let Some(scheme_end) = url.find("://") else {
        return "<redacted>".to_string();
    };
    let after_scheme = &url[scheme_end + 3..];

    // Find the userinfo@host boundary. RFC 3986: the userinfo delimiter
    // is the *last* '@' before the host — passwords may contain literal
    // '@' (percent-encoding is "should", not "must"). Using find('@')
    // here would truncate at the first '@' and leak the password tail.
    // If no '@', there's no userinfo component → no password → safe as-is.
    let Some(at_idx) = after_scheme.rfind('@') else {
        return url.to_string();
    };

    // Userinfo is everything before '@'. If it contains ':', the
    // part after ':' is the password.
    let userinfo = &after_scheme[..at_idx];
    let Some(colon_idx) = userinfo.find(':') else {
        // user@host (no password) → safe as-is.
        return url.to_string();
    };

    // Rebuild: scheme:// + user + :*** + @host...
    let mut out = String::with_capacity(url.len());
    out.push_str(&url[..scheme_end + 3]); // scheme://
    out.push_str(&userinfo[..colon_idx]); // user
    out.push_str(":***");
    out.push_str(&after_scheme[at_idx..]); // @host:port/db?...
    out
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

    // ---- comma_vec: env-string OR toml-seq → Vec<String> ----

    #[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
    struct VecConfig {
        #[serde(default, deserialize_with = "super::comma_vec")]
        systems: Vec<String>,
        #[serde(default, deserialize_with = "super::comma_vec")]
        features: Vec<String>,
    }

    // Figment's Serialized::defaults needs a map-serializing overlay.
    // `()` serializes as unit → "invalid type: found unit, expected
    // map". Empty struct serializes as an empty map.
    #[derive(Serialize, Default)]
    struct NoCli {}

    #[test]
    fn comma_vec_from_env_string() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_SYSTEMS", "x86_64-linux,aarch64-linux");
            let cfg: VecConfig = load("rio-test-vec", NoCli::default()).unwrap();
            assert_eq!(
                cfg.systems,
                vec!["x86_64-linux", "aarch64-linux"],
                "comma-sep env string → vec"
            );
            Ok(())
        });
    }

    #[test]
    fn comma_vec_from_toml_array() {
        figment::Jail::expect_with(|_jail| {
            let f = write_toml(r#"systems = ["x86_64-linux", "aarch64-linux"]"#);
            let cfg: VecConfig = load_from_path(f.path(), NoCli::default()).unwrap();
            assert_eq!(
                cfg.systems,
                vec!["x86_64-linux", "aarch64-linux"],
                "TOML array → vec"
            );
            Ok(())
        });
    }

    #[test]
    fn comma_vec_empty_and_whitespace_filtered() {
        figment::Jail::expect_with(|jail| {
            // Trailing comma + internal empty + whitespace — should all
            // be filtered/trimmed. Operator fat-fingering the env var
            // is easy; spurious empty feature names would break
            // requiredSystemFeatures matching silently.
            jail.set_env("RIO_FEATURES", "kvm, big-parallel ,,");
            let cfg: VecConfig = load("rio-test-vec", NoCli::default()).unwrap();
            assert_eq!(
                cfg.features,
                vec!["kvm", "big-parallel"],
                "empty segments dropped, whitespace trimmed"
            );
            Ok(())
        });
    }

    #[test]
    fn comma_vec_empty_string_is_empty_vec() {
        // RIO_FEATURES="" → [], not [""]. Matches the "unset" case.
        // An empty feature name would never match any required_features
        // check but would pollute debug output.
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_FEATURES", "");
            let cfg: VecConfig = load("rio-test-vec", NoCli::default()).unwrap();
            assert!(cfg.features.is_empty(), "empty env → empty vec");
            Ok(())
        });
    }

    #[test]
    fn comma_vec_default_is_empty_when_unset() {
        // Neither TOML nor env → serde's #[serde(default)] gives [].
        figment::Jail::expect_with(|_jail| {
            let cfg: VecConfig = load("rio-test-vec", NoCli::default()).unwrap();
            assert!(cfg.systems.is_empty());
            assert!(cfg.features.is_empty());
            Ok(())
        });
    }

    // --- JwtConfig tests ---

    #[derive(Serialize, Deserialize, Debug, PartialEq, Default)]
    struct JwtHost {
        #[serde(default)]
        jwt: super::JwtConfig,
    }

    /// Default: required=false, key_path=None, resolve_timeout_ms=500.
    /// These defaults are load-bearing: false→ existing deployments
    /// keep working without JWT config; None→ JWT disabled until
    /// operator mounts the Secret; 500ms→ the plan doc's recommended
    /// timeout for graceful degradation.
    #[test]
    fn jwt_config_defaults() {
        figment::Jail::expect_with(|_jail| {
            let cfg: JwtHost = load("rio-test-jwt", NoCli::default()).unwrap();
            assert!(!cfg.jwt.required, "default: not required (permissive)");
            assert!(cfg.jwt.key_path.is_none(), "default: no key → JWT off");
            assert_eq!(cfg.jwt.resolve_timeout_ms, 500);
            Ok(())
        });
    }

    #[test]
    fn jwt_config_env_nesting() {
        // RIO_JWT__REQUIRED=true → jwt.required. Double-underscore
        // nesting per the figment Env::split("__") convention.
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_JWT__REQUIRED", "true");
            jail.set_env("RIO_JWT__KEY_PATH", "/etc/rio/jwt/seed");
            jail.set_env("RIO_JWT__RESOLVE_TIMEOUT_MS", "1000");
            let cfg: JwtHost = load("rio-test-jwt", NoCli::default()).unwrap();
            assert!(cfg.jwt.required);
            assert_eq!(
                cfg.jwt.key_path.as_deref(),
                Some(std::path::Path::new("/etc/rio/jwt/seed"))
            );
            assert_eq!(cfg.jwt.resolve_timeout_ms, 1000);
            Ok(())
        });
    }

    // --- redact_db_url tests ---

    #[test]
    fn redact_db_url_basic() {
        assert_eq!(
            redact_db_url("postgres://user:secretpw@host:5432/db"),
            "postgres://user:***@host:5432/db"
        );
    }

    #[test]
    fn redact_db_url_no_password() {
        // No password → returned as-is (nothing to redact).
        assert_eq!(
            redact_db_url("postgres://user@host/db"),
            "postgres://user@host/db"
        );
    }

    #[test]
    fn redact_db_url_no_userinfo() {
        // No userinfo → returned as-is.
        assert_eq!(redact_db_url("postgres://host/db"), "postgres://host/db");
    }

    #[test]
    fn redact_db_url_malformed() {
        // Doesn't look like a URL → fully redacted (safe default).
        assert_eq!(redact_db_url("not a url"), "<redacted>");
        assert_eq!(redact_db_url(""), "<redacted>");
    }

    #[test]
    fn redact_db_url_preserves_query() {
        // Query params after the path should be preserved.
        assert_eq!(
            redact_db_url("postgres://u:pw@h:5432/d?sslmode=require&connect_timeout=30"),
            "postgres://u:***@h:5432/d?sslmode=require&connect_timeout=30"
        );
    }

    #[test]
    fn redact_db_url_password_with_at_sign() {
        // RFC 3986: userinfo delimiter is the *last* '@'. Passwords
        // containing '@' must not leak the tail past the first '@'.
        assert_eq!(
            redact_db_url("postgres://user:has@sign@host/db"),
            "postgres://user:***@host/db"
        );
        // Multiple '@' in password.
        assert_eq!(
            redact_db_url("postgres://admin:a@b@c@db.example.com:5432/rio"),
            "postgres://admin:***@db.example.com:5432/rio"
        );
    }

    // --- Duration adapter tests ---

    #[derive(Serialize, Deserialize, Debug, PartialEq)]
    struct DurCfg {
        #[serde(rename = "tick_secs", with = "secs")]
        tick: std::time::Duration,
        #[serde(rename = "timeout_ms", with = "millis")]
        timeout: std::time::Duration,
    }

    /// `secs`/`millis` round-trip via figment's `Serialized` provider —
    /// the same path `load()` uses for the base layer. Proves the
    /// adapters compose with figment, not just raw serde.
    #[test]
    fn duration_adapters_roundtrip_via_figment() {
        let original = DurCfg {
            tick: std::time::Duration::from_secs(7),
            timeout: std::time::Duration::from_millis(250),
        };
        let extracted: DurCfg = Figment::from(Serialized::defaults(&original))
            .extract()
            .unwrap();
        assert_eq!(extracted, original);
        // Wire format: rename keeps the unit-suffixed key, value is a
        // plain integer (so env `RIO_TICK_SECS=7` and TOML `tick_secs
        // = 7` both work via figment's existing layers).
        let json = serde_json::to_value(&original).unwrap();
        assert_eq!(json["tick_secs"], 7);
        assert_eq!(json["timeout_ms"], 250);
    }

    #[test]
    fn env_or_parses_and_falls_back() {
        // Unset → default.
        assert_eq!(env_or::<u32>("RIO_TEST_DEFINITELY_UNSET", 42), 42);
        // figment::Jail for thread-safe env mutation.
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_TEST_ENV_OR_OK", "7");
            assert_eq!(env_or::<u32>("RIO_TEST_ENV_OR_OK", 0), 7);
            jail.set_env("RIO_TEST_ENV_OR_BAD", "notanumber");
            assert_eq!(env_or::<u32>("RIO_TEST_ENV_OR_BAD", 99), 99);
            Ok(())
        });
    }

    // --- ensure_required tests ---

    /// Whitespace-only must be rejected as empty. This is the
    /// correctness fix motivating the helper: pre-trim, `"   "`.
    /// is_empty() == false → validation silently passes → cryptic
    /// "invalid socket address syntax" at gRPC connect.
    #[test]
    fn ensure_required_rejects_whitespace_only() {
        let err = ensure_required("   ", "scheduler_addr", "gateway")
            .unwrap_err()
            .to_string();
        assert!(
            err.contains("scheduler_addr is required"),
            "whitespace-only must be rejected as empty, got: {err}"
        );
    }

    /// Padded-but-nonempty passes. The trim is validation-only —
    /// the helper does NOT return the trimmed value; caller's
    /// `cfg.field` still has the padding. gRPC endpoint parsing
    /// tolerates it; if a consumer surfaces that doesn't, move
    /// the trim to deserialize-time.
    #[test]
    fn ensure_required_accepts_padded_nonempty() {
        ensure_required("  http://foo  ", "addr", "gateway")
            .expect("padded-but-nonempty should pass");
    }

    /// Flag/env/toml filename derived from field + component by
    /// convention. All 10 production call-sites follow this
    /// convention (clap's `#[arg(long)]` and figment's
    /// `Env::prefixed("RIO_")` both derive identically).
    #[test]
    fn ensure_required_derives_flag_and_env() {
        let err = ensure_required("", "database_url", "store")
            .unwrap_err()
            .to_string();
        assert!(err.contains("--database-url"), "flag derived: {err}");
        assert!(err.contains("RIO_DATABASE_URL"), "env derived: {err}");
        assert!(err.contains("store.toml"), "toml from component: {err}");
    }

    /// PathBuf variant — delegates through the string helper after
    /// `to_string_lossy()`. Covers gateway's `host_key` /
    /// `authorized_keys` PathBuf sites.
    #[test]
    fn ensure_required_path_rejects_empty_and_whitespace() {
        // Empty PathBuf.
        let err = ensure_required_path(std::path::Path::new(""), "host_key", "gateway")
            .unwrap_err()
            .to_string();
        assert!(err.contains("host_key is required"), "{err}");
        assert!(err.contains("--host-key"), "{err}");
        assert!(err.contains("RIO_HOST_KEY"), "{err}");
        // Whitespace-only PathBuf — same trim-check.
        ensure_required_path(std::path::Path::new("  "), "host_key", "gateway")
            .expect_err("whitespace-only path must be rejected");
        // Nonempty passes.
        ensure_required_path(
            std::path::Path::new("/etc/rio/host_key"),
            "host_key",
            "gateway",
        )
        .expect("real path should pass");
    }
}
