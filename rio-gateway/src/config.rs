//! Gateway binary configuration.
//!
//! Two-struct split per `rio_common::config` module docs:
//!   - [`Config`]: merged result, all fields concrete, `Default` =
//!     compiled-in defaults (see `tests::config_defaults_are_stable`).
//!   - [`CliArgs`]: clap-parsed, all fields `Option`, no `env=`
//!     (figment's Env provider handles that), no `default_value`
//!     (absence = `None` = don't overlay).

use clap::Parser;
use serde::{Deserialize, Serialize};

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
pub struct Config {
    pub listen_addr: std::net::SocketAddr,
    /// rio-scheduler upstream. Env: `RIO_SCHEDULER__ADDR` /
    /// `RIO_SCHEDULER__BALANCE_HOST` / `RIO_SCHEDULER__BALANCE_PORT`.
    /// `balance_host = Some` (K8s, multi-replica): DNS-resolve, probe
    /// grpc.health.v1, route to SERVING (=leader). `None` (VM tests,
    /// single-replica): single-channel via `addr`. `addr` is still
    /// required even with balance — it's the ClusterIP Service, used
    /// as the TLS verify domain (the cert's SAN).
    pub scheduler: rio_common::config::UpstreamAddrs,
    /// rio-store upstream. Env: `RIO_STORE__ADDR`. Gateway connects
    /// single-channel only (no `balance_host` — store load is
    /// builder-driven, gateway's QueryPathInfo is light).
    pub store: rio_common::config::UpstreamAddrs,
    pub host_key: std::path::PathBuf,
    pub authorized_keys: std::path::PathBuf,
    /// Shared `tls` / `metrics_addr` / `drain_grace` fields. Flattened
    /// so the wire format (TOML keys, env var names) is unchanged from
    /// when they were direct fields.
    #[serde(flatten)]
    pub common: rio_common::config::CommonConfig,
    /// gRPC health check listen address. The gateway's main protocol is
    /// SSH (russh), not gRPC, so we can't piggyback health on an
    /// existing tonic server. This spawns a dedicated one with ONLY
    /// `grpc.health.v1.Health`. K8s readinessProbe hits this port.
    ///
    /// Separate from metrics_addr: metrics is HTTP (Prometheus scrape),
    /// health is gRPC. Could multiplex with tonic's accept_http1 +
    /// a route, but that's more complexity than a second listener.
    pub health_addr: std::net::SocketAddr,
    /// JWT issuance. `key_path` → K8s Secret mount at
    /// `/etc/rio/jwt/ed25519_seed` (see helm jwt-signing-secret.yaml).
    /// Unset = JWT disabled (dual-mode SSH-comment fallback path).
    /// Env: `RIO_JWT__KEY_PATH`, `RIO_JWT__REQUIRED`.
    pub jwt: rio_common::config::JwtConfig,
    /// ResolveTenant RPC timeout in milliseconds (env:
    /// `RIO_RESOLVE_TIMEOUT_MS`). The round-trip to the
    /// scheduler happens in the SSH auth hot path — every connect, once.
    /// Bounds the auth-time latency penalty when the scheduler is slow or
    /// unreachable. Default 500ms: long enough for a warm PG lookup + RPC
    /// overhead, short enough that a stuck scheduler doesn't make SSH auth
    /// hang noticeably. On timeout: `jwt.required=false` → degrade to
    /// tenant_name-only; `jwt.required=true` → reject.
    pub resolve_timeout_ms: u64,
    /// Seconds to wait after the SSH accept loop stops for active
    /// sessions to close on their own. The accept loop only stops
    /// ACCEPTING; already-established `nix build --store ssh-ng://`
    /// sessions continue until the client EOFs (build finished),
    /// errors, or this timeout expires (then process exit kills them).
    ///
    /// I-064: previously the gateway dropped `russh`'s accept future on
    /// `serve_shutdown`, which (via russh's broadcast channel)
    /// disconnected EVERY session — a rollout killed all in-flight
    /// clients with `Nix daemon disconnected unexpectedly`.
    ///
    /// k8s `terminationGracePeriodSeconds` must be ≥
    /// `drain_grace_secs + session_drain_secs + CANCEL_GRACE` + slack,
    /// or kubelet SIGKILLs mid-drain. Helm sets it. Default 60 — long enough for
    /// a typical `wopBuildPathsWithResults` round-trip on small
    /// closures; long builds outlive this and are dropped (client
    /// retries against the new replica via NLB). 0 = exit immediately
    /// after accept stops (pre-I-064 behavior, useful for tests).
    #[serde(rename = "session_drain_secs", with = "rio_common::config::secs")]
    pub session_drain: std::time::Duration,
    /// Per-tenant build-submit rate limiting. `None` (default) →
    /// disabled (unlimited). Set via `gateway.toml [rate_limit]`
    /// section or `RIO_RATE_LIMIT__PER_MINUTE` /
    /// `RIO_RATE_LIMIT__BURST` env vars. Both fields must be ≥1.
    /// See `r[gw.rate.per-tenant]`.
    pub rate_limit: Option<crate::RateLimitConfig>,
    /// HMAC key file for minting `x-rio-service-token` on store
    /// `PutPath`. SAME file as the store's `service_hmac_key_path`
    /// (separate secret from the assignment-token key). Unset =
    /// service-token disabled (store falls back to mTLS CN-allowlist
    /// or rejects). Set via `RIO_SERVICE_HMAC_KEY_PATH`.
    pub service_hmac_key_path: Option<std::path::PathBuf>,
    /// Global SSH connection cap (`r[gw.conn.cap]`). At cap, new
    /// connects are rejected at the first `auth_*` callback. Default
    /// [`crate::server::DEFAULT_MAX_CONNECTIONS`] (1000 —
    /// ≈2 GiB bounded at 4 channels × 2×256 KiB each).
    pub max_connections: usize,
    /// DAG-reconstruction cap on transitive input derivations per
    /// build (DoS guard). Default
    /// [`crate::drv_cache::DEFAULT_MAX_TRANSITIVE_INPUTS`]
    /// (100k — ~8 MB of path strings). Set via
    /// `RIO_MAX_TRANSITIVE_INPUTS`.
    pub max_transitive_inputs: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(2222),
            // scheduler/store addrs and host_key/authorized_keys have no
            // sensible default — they're deployment-specific. Empty here +
            // a post-load check in main() gives a clear "required" error
            // that names the field. The old /tmp/rio_* defaults were
            // footguns: silent key-generation in world-writable /tmp.
            scheduler: rio_common::config::UpstreamAddrs::with_port(9001),
            store: rio_common::config::UpstreamAddrs::with_port(9002),
            host_key: std::path::PathBuf::new(),
            authorized_keys: std::path::PathBuf::new(),
            common: rio_common::config::CommonConfig::new(9090),
            // 9190 = gateway's metrics port (9090) + 100. Scheduler
            // health piggybacks on its gRPC port (9001), store on 9002.
            // Gateway has no gRPC port so needs its own. The +100
            // pattern keeps it discoverable without a doc lookup.
            health_addr: rio_common::default_addr(9190),
            jwt: rio_common::config::JwtConfig::default(),
            resolve_timeout_ms: 500,
            session_drain: std::time::Duration::from_secs(60),
            rate_limit: None,
            service_hmac_key_path: None,
            max_connections: crate::server::DEFAULT_MAX_CONNECTIONS,
            max_transitive_inputs: crate::drv_cache::DEFAULT_MAX_TRANSITIVE_INPUTS,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-gateway",
    about = "SSH gateway and Nix protocol frontend for rio-build"
)]
pub struct CliArgs {
    /// SSH listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<std::net::SocketAddr>,

    /// SSH host key path
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    host_key: Option<std::path::PathBuf>,

    /// Authorized keys file path
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    authorized_keys: Option<std::path::PathBuf>,

    /// Prometheus metrics listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    metrics_addr: Option<std::net::SocketAddr>,

    /// gRPC health check listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    health_addr: Option<std::net::SocketAddr>,

    /// Session drain timeout in seconds (0 = exit immediately after accept stops)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    session_drain_secs: Option<u64>,
}

impl rio_common::config::ValidateConfig for Config {
    /// Bounds checks on operator-settable fields. Extracted from
    /// `main()` so the checks are unit-testable without spinning up
    /// the full gateway (gRPC connect, SSH listener).
    fn validate(&self) -> anyhow::Result<()> {
        // Required-field checks that `#[serde(default)]` can't express
        // (figment's "missing field" error for `String` defaulting to `""`
        // is a silent success, not an error).
        use rio_common::config::ensure_required;
        self.scheduler
            .ensure_required("scheduler.addr", "gateway")?;
        self.store.ensure_required("store.addr", "gateway")?;
        // Per the field doc: gateway connects to store single-channel
        // only. main.rs's `let (store, _) = connect(...)` drops the
        // BalancedChannel guard — silently accepting `balance_host`
        // here would abort the probe loop and route to a stale IP on
        // pod churn. Reject at startup instead.
        anyhow::ensure!(
            self.store.balance_host.is_none(),
            "store.balance_host is not supported for the gateway (store load is \
             builder-driven; gateway uses single-channel via store.addr) — \
             unset RIO_STORE__BALANCE_HOST"
        );
        // Lossy is fine: trim-then-empty check for operator-facing
        // error messages, not a parse. A non-UTF-8 config path would
        // fail at file-open anyway.
        #[allow(clippy::disallowed_methods)]
        {
            ensure_required(&self.host_key.to_string_lossy(), "host_key", "gateway")?;
            ensure_required(
                &self.authorized_keys.to_string_lossy(),
                "authorized_keys",
                "gateway",
            )?;
        }
        // jwt.required=true + key_path=None is a misconfiguration: can't
        // mint, can't degrade → every SSH connect would be rejected. Fail
        // loud at startup (clear error) instead of rejecting every
        // connection at runtime (operator wonders why SSH is broken).
        anyhow::ensure!(
            !(self.jwt.required && self.jwt.key_path.is_none()),
            "jwt.required=true but jwt.key_path is unset — cannot mint JWTs, \
             would reject every SSH connection (set RIO_JWT__KEY_PATH or \
             unset RIO_JWT__REQUIRED)"
        );
        Ok(())
    }
}

rio_common::impl_has_common_config!(Config);

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::config::ValidateConfig as _;

    /// Regression guard: a silent drift in `Config::default()` would change
    /// behavior under VM tests (which set only the required fields via env
    /// and rely on defaults for the rest).
    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr.to_string(), "[::]:2222");
        assert_eq!(d.common.metrics_addr.to_string(), "[::]:9090");
        assert_eq!(d.health_addr.to_string(), "[::]:9190");
        // All four of these are deployment-specific: empty default + a
        // post-load ensure! in main(). Non-empty default here = silent
        // wrong-value at runtime instead of a clear startup error.
        assert!(d.scheduler.addr.is_empty());
        assert!(d.store.addr.is_empty());
        assert!(d.host_key.as_os_str().is_empty());
        assert!(d.authorized_keys.as_os_str().is_empty());
        assert_eq!(d.common.drain_grace, std::time::Duration::from_secs(6));
        assert_eq!(d.session_drain, std::time::Duration::from_secs(60));
        // JWT: disabled by default. Existing deployments (no Secret
        // mounted) keep working via the SSH-comment fallback path.
        assert!(!d.jwt.required);
        assert!(d.jwt.key_path.is_none());
        assert_eq!(d.resolve_timeout_ms, 500);
        // Rate limiting disabled by default — no compiled-in quota
        // (the right value is workload-dependent).
        assert!(d.rate_limit.is_none());
        assert_eq!(d.max_connections, crate::server::DEFAULT_MAX_CONNECTIONS);
    }

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // figment::Jail standing-guard tests — see rio-test-support/src/config.rs.
    // When you add Config.newfield: ADD IT to both assert blocks below.

    rio_test_support::jail_roundtrip!(
        "gateway",
        r#"
        max_connections = 555
        max_transitive_inputs = 250000


        [jwt]
        required = true
        key_path = "/etc/rio/jwt/ed25519_seed"

        [rate_limit]
        per_minute = 42
        burst = 7
        "#,
        |cfg: Config| {
            assert_eq!(cfg.max_connections, 555);
            assert_eq!(cfg.max_transitive_inputs, 250_000);
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through figment into JwtConfig"
            );
            assert_eq!(
                cfg.jwt.key_path.as_deref(),
                Some(std::path::Path::new("/etc/rio/jwt/ed25519_seed"))
            );
            // Top-level resolve_timeout_ms defaults via #[serde(default)]
            // when absent from TOML.
            assert_eq!(cfg.resolve_timeout_ms, 500);
            let rl = cfg
                .rate_limit
                .expect("[rate_limit] table must deserialize to Some");
            assert_eq!(rl.per_minute.get(), 42);
            assert_eq!(rl.burst.get(), 7);
        }
    );

    rio_test_support::jail_defaults!("gateway", "drain_grace_secs = 6", |cfg: Config| {
        assert_eq!(cfg.session_drain, std::time::Duration::from_secs(60));
        assert_eq!(cfg.jwt, rio_common::config::JwtConfig::default());
        assert!(cfg.rate_limit.is_none());
        assert!(cfg.scheduler.balance_host.is_none());
        assert_eq!(cfg.max_connections, crate::server::DEFAULT_MAX_CONNECTIONS);
        assert_eq!(
            cfg.max_transitive_inputs,
            crate::drv_cache::DEFAULT_MAX_TRANSITIVE_INPUTS
        );
    });

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the gateway.
    // -----------------------------------------------------------------------

    /// All four required fields filled with placeholders. The
    /// returned config passes validate_config as-is; each rejection
    /// test mutates ONE field to prove that specific check fires.
    fn test_valid_config() -> Config {
        let mut cfg = Config {
            host_key: "/tmp/host_key".into(),
            authorized_keys: "/tmp/authorized_keys".into(),
            ..Config::default()
        };
        cfg.scheduler.addr = "http://localhost:9000".into();
        cfg.store.addr = "http://localhost:9001".into();
        cfg
    }

    /// Each required field is independently checked — clearing any
    /// one should reject, naming THAT field in the error (so the
    /// operator knows which env var to set).
    #[test]
    fn config_rejects_empty_required_addrs() {
        type Patch = fn(&mut Config);
        let cases: &[(&str, Patch)] = &[
            ("scheduler.addr", |c| c.scheduler.addr = String::new()),
            ("store.addr", |c| c.store.addr = String::new()),
            ("host_key", |c| c.host_key = std::path::PathBuf::new()),
            ("authorized_keys", |c| {
                c.authorized_keys = std::path::PathBuf::new();
            }),
        ];
        for (field, patch) in cases {
            let mut cfg = test_valid_config();
            patch(&mut cfg);
            let err = cfg
                .validate()
                .expect_err("cleared required field must be rejected")
                .to_string();
            assert!(
                err.contains(field),
                "error for cleared {field} must name it: {err}"
            );
        }
    }

    /// bug_088: gateway connects to store single-channel only;
    /// `store.balance_host` is silently dropped in main.rs (`let
    /// (store, _) = connect(...)`). Reject at config-validate so the
    /// operator sees a startup error instead of stale-IP routing on
    /// store-pod churn.
    #[test]
    fn config_rejects_store_balance_host() {
        let mut cfg = test_valid_config();
        cfg.store.balance_host = Some("rio-store-headless".into());
        let err = cfg.validate().unwrap_err().to_string();
        assert!(
            err.contains("RIO_STORE__BALANCE_HOST"),
            "error must name the env var to unset: {err}"
        );
    }

    /// Scheduler balanced mode IS supported (the guard is held in
    /// main.rs's `_balance_guard`). Positive case so the rejection
    /// above stays scoped to `store`.
    #[test]
    fn config_accepts_scheduler_balance_host() {
        let mut cfg = test_valid_config();
        cfg.scheduler.balance_host = Some("rio-scheduler-headless".into());
        cfg.validate()
            .expect("scheduler.balance_host must remain supported");
    }

    /// jwt.required=true without key_path is a misconfiguration —
    /// can't mint, can't degrade. validate_config catches BEFORE the
    /// SSH listener spawns (not at first-connect).
    #[test]
    fn config_rejects_jwt_required_without_key() {
        let mut cfg = test_valid_config();
        cfg.jwt.required = true;
        cfg.jwt.key_path = None;
        let err = cfg.validate().unwrap_err().to_string();
        assert!(err.contains("jwt.required"), "{err}");
    }

    /// Baseline: `test_valid_config()` itself passes — proves the
    /// rejection tests above are testing ONLY their mutation. Also
    /// covers the jwt non-required path (required=false by default →
    /// key_path=None is fine).
    #[test]
    fn config_accepts_valid() {
        test_valid_config()
            .validate()
            .expect("valid config should pass");
        // And required=true WITH key_path is fine.
        let mut cfg = test_valid_config();
        cfg.jwt.required = true;
        cfg.jwt.key_path = Some("/etc/rio/jwt/ed25519_seed".into());
        cfg.validate().expect("required+key_path should pass");
    }
}
