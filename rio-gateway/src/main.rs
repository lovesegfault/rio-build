//! rio-gateway binary entry point.
//!
//! Connects to the scheduler and store gRPC services, then starts an
//! SSH server that speaks the Nix worker protocol.

use clap::Parser;
use serde::{Deserialize, Serialize};
use tracing::{info, warn};

// ---------------------------------------------------------------------------
// Configuration
// ---------------------------------------------------------------------------
//
// Two-struct split per rio-common/src/config.rs module docs:
//   - `Config`: merged result, all fields concrete, Default = compiled-in
//     defaults (see `tests::config_defaults_are_stable`).
//   - `CliArgs`: clap-parsed, all fields Option, no env= (figment's Env
//     provider handles that), no default_value (absence = None = don't
//     overlay).

#[derive(Debug, Serialize, Deserialize)]
#[serde(default)]
struct Config {
    listen_addr: std::net::SocketAddr,
    scheduler_addr: String,
    /// Headless Service host for health-aware balanced routing.
    /// `Some(host)` (K8s mode, multi-replica scheduler): DNS-
    /// resolve `host`, probe grpc.health.v1 on each pod IP,
    /// route to the SERVING (=leader) endpoint. `None` (env
    /// unset): single-channel via `scheduler_addr` (VM tests,
    /// non-K8s, single-replica scheduler).
    ///
    /// `scheduler_addr` is still required — it's the ClusterIP
    /// Service, used as the TLS verify domain (the cert's SAN).
    scheduler_balance_host: Option<String>,
    scheduler_balance_port: u16,
    store_addr: String,
    host_key: std::path::PathBuf,
    authorized_keys: std::path::PathBuf,
    metrics_addr: std::net::SocketAddr,
    /// gRPC health check listen address. The gateway's main protocol is
    /// SSH (russh), not gRPC, so we can't piggyback health on an
    /// existing tonic server. This spawns a dedicated one with ONLY
    /// `grpc.health.v1.Health`. K8s readinessProbe hits this port.
    ///
    /// Separate from metrics_addr: metrics is HTTP (Prometheus scrape),
    /// health is gRPC. Could multiplex with tonic's accept_http1 +
    /// a route, but that's more complexity than a second listener.
    health_addr: std::net::SocketAddr,
    /// mTLS: client cert + key + CA for outgoing gRPC connections
    /// (scheduler + store). Set via `RIO_TLS__CERT_PATH` etc. Unset
    /// = plaintext (dev mode). The gateway's INCOMING connections
    /// are SSH — TLS doesn't apply there.
    tls: rio_common::tls::TlsConfig,
    /// JWT issuance. `key_path` → K8s Secret mount at
    /// `/etc/rio/jwt/ed25519_seed` (see helm jwt-signing-secret.yaml).
    /// Unset = JWT disabled (dual-mode SSH-comment fallback path).
    /// Env: `RIO_JWT__KEY_PATH`, `RIO_JWT__REQUIRED`,
    /// `RIO_JWT__RESOLVE_TIMEOUT_MS`.
    jwt: rio_common::config::JwtConfig,
    /// Seconds to wait after SIGTERM between health=NOT_SERVING and
    /// stopping the SSH accept loop. Gives kubelet readinessProbe
    /// (periodSeconds: 5) + NLB target deregistration time. 0 = no
    /// drain. Default 6.
    drain_grace_secs: u64,
    /// Per-tenant build-submit rate limiting. `None` (default) →
    /// disabled (unlimited). Set via `gateway.toml [rate_limit]`
    /// section or `RIO_RATE_LIMIT__PER_MINUTE` /
    /// `RIO_RATE_LIMIT__BURST` env vars. Both fields must be ≥1.
    /// See `r[gw.rate.per-tenant]`.
    rate_limit: Option<rio_gateway::RateLimitConfig>,
    /// Global SSH connection cap (`r[gw.conn.cap]`). At cap, new
    /// connects are rejected at the first `auth_*` callback. Default
    /// [`rio_gateway::server::DEFAULT_MAX_CONNECTIONS`] (1000 —
    /// ≈2 GiB bounded at 4 channels × 2×256 KiB each).
    max_connections: usize,
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: rio_common::default_addr(2222),
            // scheduler_addr/store_addr/host_key/authorized_keys have no
            // sensible default — they're deployment-specific. Empty here +
            // a post-load check in main() gives a clear "required" error
            // that names the field. The old /tmp/rio_* defaults were
            // footguns: silent key-generation in world-writable /tmp.
            scheduler_addr: String::new(),
            scheduler_balance_host: None,
            scheduler_balance_port: 9001,
            store_addr: String::new(),
            host_key: std::path::PathBuf::new(),
            authorized_keys: std::path::PathBuf::new(),
            metrics_addr: rio_common::default_addr(9090),
            // 9190 = gateway's metrics port (9090) + 100. Scheduler
            // health piggybacks on its gRPC port (9001), store on 9002.
            // Gateway has no gRPC port so needs its own. The +100
            // pattern keeps it discoverable without a doc lookup.
            health_addr: rio_common::default_addr(9190),
            tls: rio_common::tls::TlsConfig::default(),
            jwt: rio_common::config::JwtConfig::default(),
            drain_grace_secs: 6,
            rate_limit: None,
            max_connections: rio_gateway::server::DEFAULT_MAX_CONNECTIONS,
        }
    }
}

#[derive(Parser, Serialize, Default)]
#[command(
    name = "rio-gateway",
    about = "SSH gateway and Nix protocol frontend for rio-build"
)]
struct CliArgs {
    /// SSH listen address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    listen_addr: Option<std::net::SocketAddr>,

    /// rio-scheduler gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    scheduler_addr: Option<String>,

    /// rio-store gRPC address
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    store_addr: Option<String>,

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

    /// Drain grace period in seconds (0 = disabled)
    #[arg(long)]
    #[serde(skip_serializing_if = "Option::is_none")]
    drain_grace_secs: Option<u64>,
}

/// Config validation — bounds checks on operator-settable fields.
///
/// Extracted from `main()` so the checks are unit-testable without
/// spinning up the full gateway (gRPC connect, SSH listener). See
/// rio-scheduler/src/main.rs validate_config for the scrutiny recipe.
fn validate_config(cfg: &Config) -> anyhow::Result<()> {
    // Required-field checks that `#[serde(default)]` can't express
    // (figment's "missing field" error for `String` defaulting to `""`
    // is a silent success, not an error).
    use rio_common::config::{ensure_required as required, ensure_required_path as required_path};
    required(&cfg.scheduler_addr, "scheduler_addr", "gateway")?;
    required(&cfg.store_addr, "store_addr", "gateway")?;
    required_path(&cfg.host_key, "host_key", "gateway")?;
    required_path(&cfg.authorized_keys, "authorized_keys", "gateway")?;
    // jwt.required=true + key_path=None is a misconfiguration: can't
    // mint, can't degrade → every SSH connect would be rejected. Fail
    // loud at startup (clear error) instead of rejecting every
    // connection at runtime (operator wonders why SSH is broken).
    anyhow::ensure!(
        !(cfg.jwt.required && cfg.jwt.key_path.is_none()),
        "jwt.required=true but jwt.key_path is unset — cannot mint JWTs, \
         would reject every SSH connection (set RIO_JWT__KEY_PATH or \
         unset RIO_JWT__REQUIRED)"
    );
    Ok(())
}

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    // rustls CryptoProvider MUST be installed before any TLS use.
    // tonic's tls-aws-lc feature enables aws-lc-rs; without
    // this install_default, rustls can't auto-select and panics on
    // first handshake. Gateway's outgoing gRPC (scheduler + store)
    // is the TLS user here — incoming is SSH.
    let _ = rustls::crypto::aws_lc_rs::default_provider().install_default();

    let cli = CliArgs::parse();
    let cfg: Config = rio_common::config::load("gateway", cli)?;
    let _otel_guard = rio_common::observability::init_tracing("gateway")?;

    // Initialize the process-global client TLS config BEFORE any
    // connect_* call. None (TLS unconfigured) → plaintext. Some →
    // https + client cert. One config serves all outgoing
    // connections: server_name is scoped to the target DNS name,
    // not the client identity. cert-manager's per-component
    // Certificates all chain to the same CA, so the gateway's
    // client cert verifies against scheduler's and store's
    // client_ca_root identically.
    //
    // server_name here is a DEFAULT for the most common target. For
    // a gateway connecting to scheduler + store, the domain_name
    // in ClientTlsConfig needs to match EACH target's cert SAN.
    // We can't set per-target in a single ClientTlsConfig — but
    // tonic's domain_name is overridden by the `:authority` header
    // (which is the hostname from the endpoint URL). So as long as
    // we connect to `rio-scheduler:9001` (hostname), rustls verifies
    // against THAT SAN. The domain_name in ClientTlsConfig is a
    // fallback when `:authority` is absent (IP-literal endpoints).
    //
    // With K8s DNS addressing (`rio-scheduler`, `rio-store`), the
    // authority matches the SAN, so any domain_name works. We pick
    // "rio-scheduler" as a reasonable default (gateway→scheduler is
    // the most-used link).
    rio_proto::client::init_client_tls(
        rio_common::tls::load_client_tls(&cfg.tls)
            .map_err(|e| anyhow::anyhow!("TLS config: {e}"))?,
    );
    if cfg.tls.is_configured() {
        info!("client mTLS enabled for outgoing gRPC");
    }

    validate_config(&cfg)?;

    let _root_guard = tracing::info_span!("gateway", component = "gateway").entered();
    info!(version = env!("CARGO_PKG_VERSION"), "starting rio-gateway");

    // Graceful shutdown: cancelled on SIGTERM/SIGINT. Lets main()
    // return normally so atexit handlers (LLVM coverage profraw flush,
    // tracing shutdown) fire. Health server gets serve_with_shutdown;
    // the SSH server.run() is wrapped in a select! below.
    let shutdown = rio_common::signal::shutdown_signal();

    rio_common::observability::init_metrics(cfg.metrics_addr)?;
    rio_gateway::describe_metrics();

    // Retry-until-connected. Cold-start race: all rio-* pods start
    // in parallel (helm install, node drain+reschedule); this process
    // can reach here before rio-store / rio-scheduler Services have
    // endpoints. connect_* uses eager .connect().await → refused →
    // Err. Bare `?` meant process-exit → CrashLoopBackOff → kubelet's
    // 10s/20s/40s backoff delays recovery past dep readiness.
    // Retry internally: pod stays not-Ready (health server below
    // hasn't spawned yet). Same pattern as rio-controller/src/main.rs:192.
    //
    // Both connects in ONE loop body: partial success (store OK,
    // scheduler refused) reconnects store on next iteration rather
    // than leaking a half-configured state.
    //
    // Scheduler has two modes:
    // - Balanced (K8s): DNS-resolve headless Service, health-probe
    //   each pod IP, route to the leader. The BalancedChannel guard
    //   must live for process lifetime — dropping it stops the probe
    //   loop. Held in _balance_guard.
    // - Single (non-K8s): plain connect. VM tests and local dev.
    let (store_client, scheduler_client, _balance_guard) = loop {
        let result: anyhow::Result<_> = async {
            info!(addr = %cfg.store_addr, "connecting to store service");
            let store = rio_proto::client::connect_store(&cfg.store_addr).await?;

            let (sched, guard) = match &cfg.scheduler_balance_host {
                None => {
                    info!(addr = %cfg.scheduler_addr, "connecting to scheduler (single-channel)");
                    let c = rio_proto::client::connect_scheduler(&cfg.scheduler_addr).await?;
                    (c, None)
                }
                Some(host) => {
                    info!(
                        %host, port = cfg.scheduler_balance_port,
                        "connecting to scheduler (health-aware balanced)"
                    );
                    let (c, bc) = rio_proto::client::balance::connect_scheduler_balanced(
                        host.clone(),
                        cfg.scheduler_balance_port,
                    )
                    .await?;
                    (c, Some(bc))
                }
            };
            Ok((store, sched, guard))
        }
        .await;
        match result {
            Ok(triple) => break triple,
            Err(e) => {
                warn!(error = %e, "upstream connect failed; retrying in 2s (pod stays not-Ready)");
                tokio::time::sleep(std::time::Duration::from_secs(2)).await;
            }
        }
    };

    // gRPC health server. Dedicated tonic instance — the gateway's main
    // protocol is SSH (russh), no existing tonic server to attach to.
    //
    // SERVING gate: retry loop above exited ⇒ both store + scheduler
    // are reachable. A gateway that can't reach the scheduler would
    // accept SSH and then hang every wopBuild* opcode — fail readiness
    // instead so K8s doesn't route here.
    //
    // spawn_monitored: the SSH server's `.run().await` below blocks
    // forever. Health must run concurrently. If the health server dies
    // (port conflict, etc), spawn_monitored logs it — K8s readiness
    // probe starts failing, pod restarts. Self-healing.
    let (health_reporter, health_service) = tonic_health::server::health_reporter();

    // Two-stage shutdown — see rio_common::server::spawn_drain_task
    // for the INDEPENDENT-token rationale. Both the health server AND
    // the SSH accept loop wait for serve_shutdown — new SSH
    // connections that were already NLB-routed before endpoint
    // propagation land on a live listener.
    //
    // Gateway's probe is empty-string (no `service:` field in helm
    // gateway.yaml). set_not_serving::<S>() only flips the NAMED
    // service — must use set_service_status("") directly. See
    // scheduler/main.rs health_toggle_not_serving test for the proof
    // that named-only is tonic-health's behavior.
    let serve_shutdown = rio_common::signal::Token::new();
    {
        let reporter = health_reporter.clone();
        rio_common::server::spawn_drain_task(
            shutdown.clone(),
            serve_shutdown.clone(),
            std::time::Duration::from_secs(cfg.drain_grace_secs),
            move || async move {
                reporter
                    .set_service_status("", tonic_health::ServingStatus::NotServing)
                    .await;
            },
        );
    }

    // Generic param: we don't have a "GatewayService" proto. Use the
    // tonic-health server's own type as a stand-in — the empty-string
    // "whole server" health check (which K8s sends) doesn't care about
    // the service name, it just wants ANY service marked SERVING.
    health_reporter
        .set_serving::<tonic_health::pb::health_server::HealthServer<
            tonic_health::server::HealthService,
        >>()
        .await;
    // Gateway has no tonic main-server (SSH accept loop instead). Health
    // is always on a separate plaintext port. Same shared-reporter
    // pattern applies: the SIGTERM drain loop above flips NOT_SERVING
    // via this reporter.
    rio_common::server::spawn_health_plaintext(
        health_service,
        cfg.health_addr,
        serve_shutdown.clone(),
    );

    let host_key = rio_gateway::load_or_generate_host_key(&cfg.host_key)?;
    let authorized_keys = rio_gateway::load_authorized_keys(&cfg.authorized_keys)?;

    // JWT signing key — K8s Secret mount. File format: 32-byte ed25519
    // seed, base64'd (the operator's `openssl rand -base64 32` output,
    // Secret-mounted). NOT PKCS#8 DER — SigningKey::from_bytes takes
    // raw seed. See helm templates/jwt-signing-secret.yaml. The
    // required=true↔key_path consistency check lives in validate_config
    // above — it fires BEFORE gRPC connects (fail-fast), not here.

    // Rate limiter: constructed from Option — None → disabled no-op.
    // No compiled-in quota default (see r[gw.rate.per-tenant]).
    let limiter = rio_gateway::TenantLimiter::new(cfg.rate_limit);
    if let Some(rl) = &cfg.rate_limit {
        info!(
            per_minute = rl.per_minute,
            burst = rl.burst,
            "per-tenant rate limiting enabled"
        );
    }

    let server = rio_gateway::GatewayServer::new(store_client, scheduler_client, authorized_keys)
        .with_rate_limiter(limiter)
        .with_max_connections(cfg.max_connections);
    let server = match &cfg.jwt.key_path {
        None => {
            info!("JWT issuance disabled (no key_path); dual-mode SSH-comment fallback");
            server
        }
        Some(path) => {
            // Same parse shape as load_jwt_pubkey but for the SIGNING
            // side: trim_ascii for ConfigMap/Secret mount trailing \n,
            // base64-decode, 32-byte check, SigningKey::from_bytes.
            // Not extracted to rio-common because the gateway is the
            // ONLY process that holds the signing key (asymmetric:
            // scheduler/store only get the pubkey). A "shared" helper
            // for a single call site is abstraction overhead.
            use base64::Engine;
            let raw = std::fs::read(path).map_err(|e| {
                anyhow::anyhow!("read JWT signing seed from {}: {e}", path.display())
            })?;
            let decoded = base64::engine::general_purpose::STANDARD
                .decode(raw.trim_ascii())
                .map_err(|e| anyhow::anyhow!("JWT signing seed base64 decode: {e}"))?;
            let arr: [u8; 32] = decoded.try_into().map_err(|v: Vec<u8>| {
                anyhow::anyhow!(
                    "JWT signing seed must be exactly 32 bytes after base64 decode, got {} \
                     (expected raw ed25519 seed, not PKCS#8 DER)",
                    v.len()
                )
            })?;
            let key = ed25519_dalek::SigningKey::from_bytes(&arr);
            info!(path = %path.display(), required = cfg.jwt.required, "JWT issuance enabled");
            server.with_jwt_signing_key(key, cfg.jwt.clone())
        }
    };

    info!(
        listen_addr = %cfg.listen_addr,
        scheduler_addr = %cfg.scheduler_addr,
        store_addr = %cfg.store_addr,
        "rio-gateway ready"
    );

    // Race the SSH server against serve_shutdown (child), not the
    // parent: the SSH accept loop stays live during the drain window
    // so late-routed connections (NLB propagation lag) still land on
    // a listener. Dropping the run() future cancels the accept loop;
    // per-session tasks die at process exit.
    tokio::select! {
        r = server.run(host_key, cfg.listen_addr) => r?,
        _ = serve_shutdown.cancelled() => {
            info!("gateway shut down cleanly");
        }
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Regression guard: a silent drift in `Config::default()` would change
    /// behavior under VM tests (which set only the required fields via env
    /// and rely on defaults for the rest).
    #[test]
    fn config_defaults_are_stable() {
        let d = Config::default();
        assert_eq!(d.listen_addr.to_string(), "0.0.0.0:2222");
        assert_eq!(d.metrics_addr.to_string(), "0.0.0.0:9090");
        assert_eq!(d.health_addr.to_string(), "0.0.0.0:9190");
        // All four of these are deployment-specific: empty default + a
        // post-load ensure! in main(). Non-empty default here = silent
        // wrong-value at runtime instead of a clear startup error.
        assert!(d.scheduler_addr.is_empty());
        assert!(d.store_addr.is_empty());
        assert!(d.host_key.as_os_str().is_empty());
        assert!(d.authorized_keys.as_os_str().is_empty());
        assert_eq!(d.drain_grace_secs, 6);
        // JWT: disabled by default. Existing deployments (no Secret
        // mounted) keep working via the SSH-comment fallback path.
        assert!(!d.jwt.required);
        assert!(d.jwt.key_path.is_none());
        assert_eq!(d.jwt.resolve_timeout_ms, 500);
        // Rate limiting disabled by default — no compiled-in quota
        // (the right value is workload-dependent).
        assert!(d.rate_limit.is_none());
        assert_eq!(
            d.max_connections,
            rio_gateway::server::DEFAULT_MAX_CONNECTIONS
        );
    }

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }

    // -----------------------------------------------------------------------
    // figment::Jail standing-guard tests — catch the NEXT orphan.
    //
    // `config_defaults_are_stable` above only checks fields that ARE
    // on Config; if a builder `with_foo()` ships without a `Config.foo`
    // field, that test doesn't know to miss it. This pair proves
    // STRUCTURE: every sub-config table wired + empty-toml defaults
    // hold. See rio-scheduler/src/main.rs all_subconfigs_roundtrip_toml + all_subconfigs_default_when_absent for the pattern
    // rationale + the P0219 failure mode that motivated it.
    // -----------------------------------------------------------------------

    /// Standing guard: TOML → Config roundtrip for EVERY sub-config
    /// table via the REAL `rio_common::config::load` path (not raw
    /// figment). Jail changes cwd to a temp dir; `./gateway.toml`
    /// there is picked up by load()'s `{component}.toml` layer.
    ///
    /// When you add `Config.newfield`: ADD IT HERE or this test's
    /// doc-comment is a lie. The companion `all_subconfigs_default_
    /// when_absent` catches "new required field breaks existing
    /// deployments" (figment missing-field error).
    ///
    /// `#[allow(result_large_err)]` — figment::Error is 208B, API-fixed.
    #[test]
    #[allow(clippy::result_large_err)]
    fn all_subconfigs_roundtrip_toml() {
        figment::Jail::expect_with(|jail| {
            // Every sub-config table with at least one NON-default
            // value. Proves: (a) the table name is wired; (b) the
            // Deserialize derive works; (c) the value reaches Config.
            jail.create_file(
                "gateway.toml",
                r#"
                max_connections = 555

                [tls]
                cert_path = "/etc/tls/cert.pem"

                [jwt]
                required = true
                key_path = "/etc/rio/jwt/ed25519_seed"

                [rate_limit]
                per_minute = 42
                burst = 7
                "#,
            )?;
            let cfg: Config = rio_common::config::load("gateway", CliArgs::default()).unwrap();
            assert_eq!(cfg.max_connections, 555);
            assert_eq!(
                cfg.tls.cert_path.as_deref(),
                Some(std::path::Path::new("/etc/tls/cert.pem")),
                "[tls] table must thread through figment into TlsConfig"
            );
            assert!(
                cfg.jwt.required,
                "[jwt] table must thread through figment into JwtConfig"
            );
            assert_eq!(
                cfg.jwt.key_path.as_deref(),
                Some(std::path::Path::new("/etc/rio/jwt/ed25519_seed"))
            );
            // Unspecified sub-field defaults via #[serde(default)]
            // on the sub-struct (partial table must work).
            assert_eq!(cfg.jwt.resolve_timeout_ms, 500);
            let rl = cfg
                .rate_limit
                .expect("[rate_limit] table must deserialize to Some");
            assert_eq!(rl.per_minute, 42);
            assert_eq!(rl.burst, 7);
            Ok(())
        });
    }

    /// Near-empty gateway.toml → every sub-config gets its Default
    /// impl. If `Config.foo` is added WITHOUT `#[serde(default)]` AND
    /// the sub-struct lacks `impl Default`, this fails with a figment
    /// missing-field error — catches "new required field breaks
    /// existing deployments".
    ///
    /// `drain_grace_secs` is set to prove the TOML IS loaded (a
    /// truly empty file would be indistinguishable from a missing one
    /// in terms of sub-config defaults).
    #[test]
    #[allow(clippy::result_large_err)]
    fn all_subconfigs_default_when_absent() {
        figment::Jail::expect_with(|jail| {
            jail.create_file("gateway.toml", "drain_grace_secs = 6")?;
            let cfg: Config = rio_common::config::load("gateway", CliArgs::default()).unwrap();
            // Every sub-config / optional field at its default. When
            // you add Config.newfield: ADD IT HERE.
            assert!(!cfg.tls.is_configured());
            assert_eq!(cfg.jwt, rio_common::config::JwtConfig::default());
            assert!(cfg.rate_limit.is_none());
            assert!(cfg.scheduler_balance_host.is_none());
            assert_eq!(
                cfg.max_connections,
                rio_gateway::server::DEFAULT_MAX_CONNECTIONS
            );
            Ok(())
        });
    }

    // -----------------------------------------------------------------------
    // validate_config rejection tests — spreads the P0409 pattern
    // (rio-scheduler/src/main.rs) to the gateway.
    // -----------------------------------------------------------------------

    /// All four required fields filled with placeholders. The
    /// returned config passes validate_config as-is; each rejection
    /// test mutates ONE field to prove that specific check fires.
    fn test_valid_config() -> Config {
        Config {
            scheduler_addr: "http://localhost:9000".into(),
            store_addr: "http://localhost:9001".into(),
            host_key: "/tmp/host_key".into(),
            authorized_keys: "/tmp/authorized_keys".into(),
            ..Config::default()
        }
    }

    /// Whitespace-only scheduler_addr must be rejected as empty.
    /// Regression guard for `ensure_required`'s trim — pre-helper,
    /// bare `is_empty()` accepted `"   "`, startup failed later at
    /// gRPC connect with "invalid socket address syntax: '  '".
    #[test]
    fn config_rejects_whitespace_scheduler_addr() {
        let mut cfg = test_valid_config();
        cfg.scheduler_addr = "   ".into();
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(
            err.contains("scheduler_addr is required"),
            "whitespace-only scheduler_addr must be rejected as empty, got: {err}"
        );
    }

    /// Each required field is independently checked — clearing any
    /// one should reject, naming THAT field in the error (so the
    /// operator knows which env var to set).
    #[test]
    fn config_rejects_empty_required_addrs() {
        type Patch = fn(&mut Config);
        let cases: &[(&str, Patch)] = &[
            ("scheduler_addr", |c| c.scheduler_addr = String::new()),
            ("store_addr", |c| c.store_addr = String::new()),
            ("host_key", |c| c.host_key = std::path::PathBuf::new()),
            ("authorized_keys", |c| {
                c.authorized_keys = std::path::PathBuf::new();
            }),
        ];
        for (field, patch) in cases {
            let mut cfg = test_valid_config();
            patch(&mut cfg);
            let err = validate_config(&cfg)
                .expect_err("cleared required field must be rejected")
                .to_string();
            assert!(
                err.contains(field),
                "error for cleared {field} must name it: {err}"
            );
        }
    }

    /// jwt.required=true without key_path is a misconfiguration —
    /// can't mint, can't degrade. validate_config catches BEFORE the
    /// SSH listener spawns (not at first-connect).
    #[test]
    fn config_rejects_jwt_required_without_key() {
        let mut cfg = test_valid_config();
        cfg.jwt.required = true;
        cfg.jwt.key_path = None;
        let err = validate_config(&cfg).unwrap_err().to_string();
        assert!(err.contains("jwt.required"), "{err}");
    }

    /// Baseline: `test_valid_config()` itself passes — proves the
    /// rejection tests above are testing ONLY their mutation. Also
    /// covers the jwt non-required path (required=false by default →
    /// key_path=None is fine).
    #[test]
    fn config_accepts_valid() {
        validate_config(&test_valid_config()).expect("valid config should pass");
        // And required=true WITH key_path is fine.
        let mut cfg = test_valid_config();
        cfg.jwt.required = true;
        cfg.jwt.key_path = Some("/etc/rio/jwt/ed25519_seed".into());
        validate_config(&cfg).expect("required+key_path should pass");
    }
}
