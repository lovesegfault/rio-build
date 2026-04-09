//! rio-gateway binary entry point.
//!
//! Connects to the scheduler and store gRPC services, then starts an
//! SSH server that speaks the Nix worker protocol.

use clap::Parser;
use rio_gateway::config::{CliArgs, Config};
use tracing::info;

#[tokio::main]
async fn main() -> anyhow::Result<()> {
    let cli = CliArgs::parse();
    let rio_common::server::Bootstrap::<Config> {
        cfg,
        shutdown,
        serve_shutdown,
        otel_guard: _otel_guard,
        root_span: _root_span,
    } = rio_common::server::bootstrap(
        "gateway",
        cli,
        rio_gateway::describe_metrics,
        rio_gateway::HISTOGRAM_BUCKETS,
    )?;

    // Process-global DoS cap. Set ONCE before any SSH session can
    // call reconstruct_dag. OnceLock pattern
    // — the alternative (threading through ~18 call sites) is
    // invasive for a value that IS process-wide.
    rio_gateway::drv_cache::init_max_transitive_inputs(cfg.max_transitive_inputs);

    // Retry-until-connected via connect_forever (shutdown-aware,
    // exponential backoff). Cold-start race: all rio-* pods start in
    // parallel; this process can reach here before rio-store /
    // rio-scheduler Services have endpoints. Pod stays not-Ready
    // (health server below hasn't spawned yet) while retrying.
    //
    // r[impl gw.sched.balanced]
    // Both connects in ONE closure body: partial success (store OK,
    // scheduler refused) reconnects store on next attempt rather than
    // leaking a half-configured state.
    //
    // Scheduler has two modes:
    // - Balanced (K8s): DNS-resolve headless Service, health-probe
    //   each pod IP, route to the leader. The BalancedChannel guard
    //   must live for process lifetime — dropping it stops the probe
    //   loop. Held in _balance_guard.
    // - Single (non-K8s): plain connect. VM tests and local dev.
    //
    // `connect_forever` → `None` only on shutdown (clean exit).
    let Some((store_client, scheduler_client, _balance_guard)) =
        rio_proto::client::connect_forever(&shutdown, || async {
            let (store, _) = rio_proto::client::connect(&cfg.store).await?;
            let (sched, guard) = rio_proto::client::connect(&cfg.scheduler).await?;
            anyhow::Ok((store, sched, guard))
        })
        .await
    else {
        return Ok(());
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
    let reporter = health_reporter.clone();
    rio_common::server::spawn_drain_task(
        shutdown.clone(),
        serve_shutdown.clone(),
        cfg.common.drain_grace,
        move || async move {
            reporter
                .set_service_status("", tonic_health::ServingStatus::NotServing)
                .await;
        },
    );

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
    rio_common::server::spawn_health_server(
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
            per_minute = rl.per_minute.get(),
            burst = rl.burst.get(),
            "per-tenant rate limiting enabled"
        );
    }

    let service_signer = rio_auth::hmac::HmacSigner::load(cfg.service_hmac_key_path.as_deref())
        .map_err(|e| anyhow::anyhow!("service-HMAC key load: {e}"))?;
    if service_signer.is_some() {
        info!("x-rio-service-token minting enabled on store PutPath");
    }

    let server = rio_gateway::GatewayServer::new(store_client, scheduler_client, authorized_keys)
        .with_rate_limiter(limiter)
        .with_max_connections(cfg.max_connections)
        .with_resolve_timeout(std::time::Duration::from_millis(cfg.resolve_timeout_ms));
    let server = match service_signer {
        Some(s) => server.with_service_hmac_signer(s),
        None => server,
    };
    // I-109: hot-reload the key set when the Secret mount refreshes.
    // Tied to serve_shutdown so the watcher exits with the accept loop;
    // the JoinHandle is dropped (detached) — nothing to await on exit.
    rio_gateway::spawn_authorized_keys_watcher(
        server.authorized_keys_handle(),
        cfg.authorized_keys.clone(),
        rio_gateway::AUTHORIZED_KEYS_POLL_INTERVAL,
        serve_shutdown.clone(),
    );
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
        scheduler_addr = %cfg.scheduler.addr,
        store_addr = %cfg.store.addr,
        "rio-gateway ready"
    );

    // r[impl gw.drain.three-stage]
    // Three-stage shutdown (I-064):
    //
    //   SIGTERM ─► spawn_drain_task: NotServing ─► sleep(drain_grace) ─► serve_shutdown
    //                                                                         │
    //   run(): accept loop ◄──────────────────────────── stops accepting ◄────┘
    //   (sessions detached, continue)
    //                                                                         │
    //   wait_for_session_drain: poll active_conns → 0 OR session_drain_secs ──┘
    //                                                                         │
    //   return → process exit (any remaining sessions die here) ──────────────┘
    //
    // run() takes serve_shutdown directly: it returns when the token
    // fires, but spawned per-connection tasks continue (no russh
    // broadcast-disconnect coupling — see GatewayServer::run doc).
    let active_conns = server.active_conns_handle();
    let sessions_shutdown = server.sessions_shutdown_handle();
    server
        .run(host_key, cfg.listen_addr, serve_shutdown.clone())
        .await?;

    wait_for_session_drain(&active_conns, &sessions_shutdown, cfg.session_drain).await;

    info!("gateway shut down cleanly");
    Ok(())
}

/// Max time to wait for `cancel_active_builds` RPCs to land after
/// firing `sessions_shutdown` on drain timeout. Each cancel is a unary
/// `CancelBuild` with `DEFAULT_GRPC_TIMEOUT` (30s) wrapper, but the
/// scheduler is local and warm — 5s covers `N_builds × ~ms` plus the
/// 1s poll tick. `terminationGracePeriodSeconds` (helm: 90s) bounds
/// `drain_grace_secs + session_drain_secs + this`; SIGKILL is the
/// backstop if the scheduler is unreachable.
const CANCEL_GRACE: std::time::Duration = std::time::Duration::from_secs(5);

/// Poll `active_conns` until 0 or `timeout`. 1s tick — coarse is fine
/// (drain is bounded by `terminationGracePeriodSeconds` anyway). Logs
/// every tick so the operator's `kubectl logs -f` shows live progress
/// during a rollout.
///
/// I-081: on timeout, fire `sessions_shutdown` so every proto_task runs
/// `cancel_active_builds`, then poll a brief [`CANCEL_GRACE`] more.
/// Without this the process exits with the proto_tasks Drop'd —
/// `CancelBuild` never sent, scheduler leaks builds Active until 24h
/// TTL, workers keep building cancelled work. SIGKILL still leaks
/// (can't be helped — scheduler-side timeout is the recovery there).
///
/// `timeout.is_zero()` returns immediately without cancelling (pre-I-064
/// behavior — tests and the standalone-fixture VM scenarios that don't
/// open SSH).
async fn wait_for_session_drain(
    active_conns: &std::sync::Arc<std::sync::atomic::AtomicUsize>,
    sessions_shutdown: &rio_common::signal::Token,
    timeout: std::time::Duration,
) {
    use std::sync::atomic::Ordering;
    if timeout.is_zero() {
        return;
    }
    let mut deadline = tokio::time::Instant::now() + timeout;
    let mut tick = tokio::time::interval(std::time::Duration::from_secs(1));
    loop {
        let n = active_conns.load(Ordering::Relaxed);
        if n == 0 {
            info!("session drain: all SSH sessions closed");
            return;
        }
        if tokio::time::Instant::now() >= deadline {
            if sessions_shutdown.is_cancelled() {
                tracing::warn!(
                    remaining = n,
                    "session drain: cancel grace exhausted — exiting \
                     (CancelBuild may not have reached scheduler for {n} session(s))"
                );
                return;
            }
            tracing::warn!(
                remaining = n,
                timeout_secs = timeout.as_secs(),
                "session drain: timeout — cancelling {n} session(s) so \
                 scheduler hears CancelBuild before exit"
            );
            sessions_shutdown.cancel();
            deadline = tokio::time::Instant::now() + CANCEL_GRACE;
            // fall through to next tick — proto_tasks need a poll to
            // observe the token, then their cancel loop runs.
        }
        info!(remaining = n, "session drain: waiting for SSH sessions");
        tick.tick().await;
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::signal::Token;

    /// timeout=0 returns immediately without polling or cancelling —
    /// pre-I-064 behavior, used by VM-test fixtures that send SIGTERM
    /// with no SSH sessions open.
    #[tokio::test(start_paused = true)]
    async fn session_drain_zero_timeout_immediate() {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(5));
        let tok = Token::new();
        let start = tokio::time::Instant::now();
        wait_for_session_drain(&n, &tok, std::time::Duration::ZERO).await;
        assert_eq!(start.elapsed(), std::time::Duration::ZERO);
        assert!(!tok.is_cancelled(), "zero timeout must not cancel");
    }

    /// Count → 0 mid-wait returns at the next 1s tick, not at timeout.
    /// Precondition assert at t=0 proves we entered the wait (not
    /// short-circuited on a zero count). Natural drain → no cancel.
    #[tokio::test(start_paused = true)]
    async fn session_drain_exits_when_count_zeroes() {
        use std::sync::atomic::Ordering;
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(2));
        assert_eq!(n.load(Ordering::Relaxed), 2, "precondition");
        let n2 = n.clone();
        tokio::spawn(async move {
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            n2.store(0, Ordering::Relaxed);
        });
        let tok = Token::new();
        let start = tokio::time::Instant::now();
        wait_for_session_drain(&n, &tok, std::time::Duration::from_secs(60)).await;
        // Polled at 1s ticks: 0ms (n=2), 1000ms (n=2), 2000ms (n=0 → exit).
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(2));
        assert!(!tok.is_cancelled(), "natural drain must not cancel");
    }

    /// I-081: timeout fires with sessions still open → cancels
    /// `sessions_shutdown`, then waits CANCEL_GRACE. The token cancel
    /// is what reaches every proto_task's `cancel_active_builds`.
    // r[verify gw.conn.session-drain]
    #[tokio::test(start_paused = true)]
    async fn session_drain_timeout_cancels_then_graces() {
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let tok = Token::new();
        let start = tokio::time::Instant::now();
        wait_for_session_drain(&n, &tok, std::time::Duration::from_secs(3)).await;
        // 3s drain timeout + 5s CANCEL_GRACE (n never reached 0).
        assert_eq!(
            start.elapsed(),
            std::time::Duration::from_secs(3) + CANCEL_GRACE
        );
        assert_eq!(n.load(std::sync::atomic::Ordering::Relaxed), 1);
        assert!(tok.is_cancelled(), "timeout must cancel sessions_shutdown");
    }

    /// I-081: count reaching 0 during the cancel-grace phase exits
    /// early — proto_tasks' cancel loops returned, response_tasks sent
    /// EOF+close, clients dropped the SSH connection.
    #[tokio::test(start_paused = true)]
    async fn session_drain_timeout_then_zero_during_grace() {
        use std::sync::atomic::Ordering;
        let n = std::sync::Arc::new(std::sync::atomic::AtomicUsize::new(1));
        let tok = Token::new();
        let child = tok.child_token();
        let n2 = n.clone();
        // Model a session that observes the cancel and drops the
        // connection ~1.5s later (cancel_active_builds + EOF roundtrip).
        tokio::spawn(async move {
            child.cancelled().await;
            tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
            n2.store(0, Ordering::Relaxed);
        });
        let start = tokio::time::Instant::now();
        wait_for_session_drain(&n, &tok, std::time::Duration::from_secs(3)).await;
        // 3s timeout → cancel → child fires, sleeps 1.5s → n=0 observed
        // at the next 1s tick (t=5s). Early exit, didn't burn full grace.
        assert_eq!(start.elapsed(), std::time::Duration::from_secs(5));
        assert!(tok.is_cancelled());
    }
}
