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
}

impl Default for Config {
    fn default() -> Self {
        Self {
            listen_addr: "0.0.0.0:2222".parse().unwrap(),
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
            metrics_addr: "0.0.0.0:9090".parse().unwrap(),
            // 9190 = gateway's metrics port (9090) + 100. Scheduler
            // health piggybacks on its gRPC port (9001), store on 9002.
            // Gateway has no gRPC port so needs its own. The +100
            // pattern keeps it discoverable without a doc lookup.
            health_addr: "0.0.0.0:9190".parse().unwrap(),
            tls: rio_common::tls::TlsConfig::default(),
            jwt: rio_common::config::JwtConfig::default(),
            drain_grace_secs: 6,
            rate_limit: None,
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

    // Required-field checks that `#[serde(default)]` can't express
    // (figment's "missing field" error for `String` defaulting to `""`
    // is a silent success, not an error).
    anyhow::ensure!(
        !cfg.scheduler_addr.is_empty(),
        "scheduler_addr is required (set --scheduler-addr, RIO_SCHEDULER_ADDR, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.store_addr.is_empty(),
        "store_addr is required (set --store-addr, RIO_STORE_ADDR, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.host_key.as_os_str().is_empty(),
        "host_key is required (set --host-key, RIO_HOST_KEY, or gateway.toml)"
    );
    anyhow::ensure!(
        !cfg.authorized_keys.as_os_str().is_empty(),
        "authorized_keys is required (set --authorized-keys, RIO_AUTHORIZED_KEYS, or gateway.toml)"
    );

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

    // Two-stage shutdown. `shutdown` (parent) fires on SIGTERM.
    // `serve_shutdown` is an INDEPENDENT token — NOT child_token(),
    // which would cascade and cancel the instant SIGTERM fires (zero
    // drain window). It fires only via the drain task below, AFTER
    // set_not_serving + sleep. Both the health server AND the SSH
    // accept loop wait for serve_shutdown — new SSH connections that
    // were already NLB-routed before endpoint propagation land on a
    // live listener.
    let serve_shutdown = rio_common::signal::Token::new();
    {
        let reporter = health_reporter.clone();
        let parent = shutdown.clone();
        let child = serve_shutdown.clone();
        let grace = std::time::Duration::from_secs(cfg.drain_grace_secs);
        rio_common::task::spawn_monitored("drain-on-sigterm", async move {
            parent.cancelled().await;
            // r[impl common.drain.not-serving-before-exit]
            // Gateway's probe is empty-string (no `service:` field in
            // helm gateway.yaml). set_not_serving::<S>() only flips
            // the NAMED service — must use set_service_status("")
            // directly. See scheduler/main.rs health_toggle_not_serving
            // test for the proof that named-only is tonic-health's
            // behavior.
            reporter
                .set_service_status("", tonic_health::ServingStatus::NotServing)
                .await;
            tracing::info!(
                grace_secs = grace.as_secs(),
                "SIGTERM: health=NOT_SERVING, draining"
            );
            if !grace.is_zero() {
                tokio::time::sleep(grace).await;
            }
            child.cancel();
        });
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
    // raw seed. See helm templates/jwt-signing-secret.yaml.
    //
    // required=true + key_path=None is a misconfiguration: can't mint,
    // can't degrade → every SSH connect would be rejected. Fail loud at
    // startup (clear error) instead of rejecting every connection at
    // runtime (operator wonders why SSH is broken).
    anyhow::ensure!(
        !(cfg.jwt.required && cfg.jwt.key_path.is_none()),
        "jwt.required=true but jwt.key_path is unset — cannot mint JWTs, \
         would reject every SSH connection (set RIO_JWT__KEY_PATH or \
         unset RIO_JWT__REQUIRED)"
    );

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
        .with_rate_limiter(limiter);
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
    }

    /// clap --help must still work (no panics in derive expansion).
    #[test]
    fn cli_args_parse_help() {
        use clap::CommandFactory;
        CliArgs::command().debug_assert();
    }
}
