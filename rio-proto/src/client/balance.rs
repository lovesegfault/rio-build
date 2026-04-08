//! Health-aware load-balanced gRPC channel.
//!
//! The scheduler runs as a 2-replica Deployment. Both pods are Ready
//! (gRPC server up, process alive), but only the leader serves RPCs
//! --- the standby returns UNAVAILABLE on every handler
//! (`r[sched.grpc.leader-guard]`). tonic's `Channel::balance_channel`
//! does p2c load balancing over a dynamic endpoint set, but p2c only
//! ejects on *connection-level* failure (`poll_ready` error) --- the
//! standby's connection is perfectly healthy, RPCs just fail. p2c
//! keeps routing to it.
//!
//! This module adds an out-of-band health-check task: DNS-resolve
//! the headless Service, probe each pod IP via `grpc.health.v1/Check`
//! with the named service (e.g. `rio.scheduler.SchedulerService`),
//! feed `Change::Insert` for SERVING and `Change::Remove` for
//! NOT_SERVING. The balance channel only ever sees the leader.
//!
//! ## TLS domain override
//!
//! The balancer connects to *pod IPs* (`https://10.42.2.140:9001`),
//! but the cert's SAN is the service name (`rio-scheduler`), not the
//! IP. `ClientTlsConfig::domain_name()` decouples the verify domain
//! from the connect URI: we connect to the IP, tonic verifies against
//! the domain we supply. No cert-manager SAN change needed.
//!
//! ## First-tick blocking
//!
//! `BalancedChannel::new` awaits the first probe cycle before
//! returning. Without this, the p2c starts empty and the first RPC
//! fails "no ready endpoints" --- which in the gateway/builder is a
//! fail-fast `?` out of `main()`.

// r[impl sched.grpc.leader-guard]

use std::collections::HashSet;
use std::net::SocketAddr;
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::channel::Change;
use tonic::transport::{Channel, Endpoint};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
use tracing::{debug, warn};

/// Per-endpoint health probe timeout. A pod that takes >2s to
/// answer Health/Check is as good as down for routing purposes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Endpoint builder: wraps a pod IP in an `Endpoint` with the
/// TLS domain override applied. Factored out because both the
/// balance feed (Insert) and the health probe itself need to
/// connect to pod IPs with the same TLS config.
fn build_endpoint(addr: SocketAddr, tls_domain: &str) -> anyhow::Result<Endpoint> {
    let ep = match rio_common::grpc::client_tls() {
        Some(tls) => {
            // domain_name overrides the SNI + SAN-verify domain.
            // The connect URI's host (the pod IP) is still what
            // we dial, but rustls presents `tls_domain` as SNI
            // and checks the cert's SAN against it.
            //
            // IPv6: wrap in brackets for the URI authority.
            // SocketAddr's Display does NOT bracket the IP part
            // when you format the IP alone, so do it manually.
            let uri = match addr {
                SocketAddr::V4(_) => format!("https://{}:{}", addr.ip(), addr.port()),
                SocketAddr::V6(a) => format!("https://[{}]:{}", a.ip(), a.port()),
            };
            Endpoint::from_shared(uri)?
                .tls_config(tls.domain_name(tls_domain))?
                .connect_timeout(PROBE_TIMEOUT)
        }
        None => {
            let uri = match addr {
                SocketAddr::V4(_) => format!("http://{}:{}", addr.ip(), addr.port()),
                SocketAddr::V6(a) => format!("http://[{}]:{}", a.ip(), a.port()),
            };
            Endpoint::from_shared(uri)?.connect_timeout(PROBE_TIMEOUT)
        }
    };
    // h2 keepalive is NOT optional here. Probe rediscovery emits
    // Change::Remove --- that drops the endpoint from the p2c selection
    // pool, but does NOT close existing TCP connections. In-flight bidi
    // streams stay pinned to the dead peer until something at the
    // transport layer errors. Without h2 keepalive that "something" is
    // kernel TCP keepalive, ~2h on Linux defaults.
    //
    // I-048c: scheduler SIGKILL (no FIN) --- fetchers went 7 minutes
    // silent to the new leader. Both the bidi build stream AND the unary
    // heartbeat share this Channel, so neither pierced the dead h2
    // connection. ~40s detection (30s PING + 10s PONG timeout) bounds it.
    Ok(super::with_h2_keepalive(ep))
}

/// Probe one endpoint's health. Returns true iff the named service
/// reports SERVING. All errors (connect fail, timeout, NOT_SERVING,
/// UNKNOWN) collapse to false --- "not known good, don't route."
///
/// Uses an ephemeral Channel, NOT the balanced channel. Chicken and
/// egg: we're deciding whether to PUT this endpoint INTO the balance.
async fn probe(addr: SocketAddr, tls_domain: &str, service: &str) -> bool {
    let endpoint = match build_endpoint(addr, tls_domain) {
        Ok(e) => e,
        Err(e) => {
            // Should never happen (we control the URI format) but
            // don't panic in a background loop.
            warn!(%addr, error = %e, "probe: bad endpoint");
            return false;
        }
    };
    let fut = async {
        let ch = endpoint.connect().await.ok()?;
        let mut hc = HealthClient::new(ch);
        let resp = hc
            .check(HealthCheckRequest {
                service: service.to_string(),
            })
            .await
            .ok()?;
        Some(resp.into_inner().status())
    };
    match tokio::time::timeout(PROBE_TIMEOUT, fut).await {
        Ok(Some(ServingStatus::Serving)) => true,
        Ok(Some(status)) => {
            debug!(%addr, ?status, "probe: not serving");
            false
        }
        Ok(None) => {
            debug!(%addr, "probe: connect/check failed");
            false
        }
        Err(_) => {
            debug!(%addr, "probe: timeout");
            false
        }
    }
}

/// One probe cycle: resolve DNS, probe all endpoints, diff against
/// the live set, emit Change::Insert/Remove. Returns the new live
/// set for the next cycle's diff.
async fn tick(
    host: &str,
    port: u16,
    service: &str,
    tls_domain: &str,
    live: &HashSet<SocketAddr>,
    tx: &mpsc::Sender<Change<SocketAddr, Endpoint>>,
) -> HashSet<SocketAddr> {
    // DNS resolve. For a headless Service, CoreDNS returns ALL pod
    // IPs as A and/or AAAA records (TTL 5s by default; AAAA on
    // dual-stack — ipFamilyPolicy on the headless Service, P0542).
    // `lookup_host` resolves via the system resolver and returns
    // both families --- no extra deps. The port argument is required
    // by `ToSocketAddrs`; the returned SocketAddrs carry it.
    // build_endpoint() brackets v6 addrs for the URI authority.
    let resolved: HashSet<SocketAddr> = match tokio::net::lookup_host((host, port)).await {
        Ok(addrs) => addrs.collect(),
        Err(e) => {
            // DNS failure: keep the current live set unchanged.
            // Don't Remove everything --- a transient resolver
            // hiccup shouldn't eject the leader.
            warn!(%host, error = %e, "balance: DNS resolve failed; keeping current endpoints");
            return live.clone();
        }
    };

    // Probe all resolved addrs concurrently. Small set (2 pods
    // typically), overhead is negligible; do it in parallel so
    // a slow standby doesn't delay seeing the leader.
    let mut probes = Vec::with_capacity(resolved.len());
    for &addr in &resolved {
        let tls_domain = tls_domain.to_string();
        let service = service.to_string();
        probes.push(async move { (addr, probe(addr, &tls_domain, &service).await) });
    }
    let results: Vec<(SocketAddr, bool)> = futures_util::future::join_all(probes).await;
    let serving: HashSet<SocketAddr> = results
        .into_iter()
        .filter_map(|(a, ok)| ok.then_some(a))
        .collect();

    // Diff. Send Insert for new SERVING, Remove for no-longer-SERVING.
    // Order doesn't matter to p2c.
    for &addr in serving.difference(live) {
        match build_endpoint(addr, tls_domain) {
            Ok(ep) => {
                debug!(%addr, "balance: insert");
                if tx.send(Change::Insert(addr, ep)).await.is_err() {
                    // Balance channel dropped --- caller is shutting
                    // down. Stop feeding.
                    return serving;
                }
            }
            Err(e) => warn!(%addr, error = %e, "balance: build_endpoint failed on insert"),
        }
    }
    for &addr in live.difference(&serving) {
        debug!(%addr, "balance: remove");
        if tx.send(Change::Remove(addr)).await.is_err() {
            return serving;
        }
    }

    serving
}

/// A `Channel` that routes to SERVING endpoints only.
///
/// Cloning the inner channel is cheap (tonic Channels are
/// `Arc`-backed). Dropping the `BalancedChannel` aborts the
/// background probe task --- the channel stays usable with
/// its last-known endpoint set but won't rediscover.
///
/// Typical usage: construct once in `main()`, `.channel()` into
/// service clients, keep the guard alive for process lifetime.
pub struct BalancedChannel {
    channel: Channel,
    _task: tokio::task::JoinHandle<()>,
}

impl Drop for BalancedChannel {
    fn drop(&mut self) {
        self._task.abort();
    }
}

impl BalancedChannel {
    /// Build a health-aware balanced channel.
    ///
    /// - `host`: headless Service DNS name (e.g.
    ///   `rio-scheduler-headless.rio-system.svc.cluster.local`,
    ///   or just `rio-scheduler-headless` --- CoreDNS handles
    ///   search-path expansion)
    /// - `port`: gRPC port
    /// - `health_service`: the `grpc.health.v1` service name to
    ///   probe. For the scheduler this is
    ///   `rio.scheduler.SchedulerService` --- proto package +
    ///   service, NOT empty string (the scheduler only toggles
    ///   the named service, not `""`)
    /// - `tls_domain`: SAN to verify the server cert against.
    ///   Typically the ClusterIP Service name (`rio-scheduler`)
    ///   since that's what cert-manager puts in `dnsNames`.
    /// - `probe_interval`: how often to re-resolve + re-probe.
    ///   Should be ≤ CoreDNS TTL (5s default) or we miss flips.
    ///
    /// Blocks until the first probe cycle completes. Errors only
    /// if the first cycle finds ZERO serving endpoints --- the
    /// balance channel would be empty, first RPC fails "no ready
    /// endpoints," caller errors out of main anyway. Better to
    /// fail here with a clear message.
    pub async fn new(
        host: String,
        port: u16,
        health_service: String,
        tls_domain: String,
        probe_interval: Duration,
    ) -> anyhow::Result<Self> {
        // Channel::balance_channel takes a buffer capacity for the
        // discovery channel. We send at most 2×N changes per tick
        // (N = pod count, typically 2). 32 is ample.
        let (channel, tx) = Channel::balance_channel::<SocketAddr>(32);

        // First tick: blocking, so the channel has ≥1 endpoint
        // before the caller makes an RPC.
        let live = tick(
            &host,
            port,
            &health_service,
            &tls_domain,
            &HashSet::new(),
            &tx,
        )
        .await;
        anyhow::ensure!(
            !live.is_empty(),
            "balance: no SERVING endpoints for {host}:{port} (service={health_service}); \
             either DNS returned nothing or all probes failed. \
             Check headless Service selector and that at least one pod is leader."
        );
        tracing::info!(
            %host, port, endpoints = live.len(),
            "balanced channel initialized"
        );

        // Background probe loop. Runs until dropped.
        let task = tokio::spawn(async move {
            let mut live = live;
            let mut interval = tokio::time::interval(probe_interval);
            // First tick fires immediately; we already did one, skip.
            interval.tick().await;
            loop {
                interval.tick().await;
                live = tick(&host, port, &health_service, &tls_domain, &live, &tx).await;
            }
        });

        Ok(Self {
            channel,
            _task: task,
        })
    }

    /// [`Self::new`] with `health_service`/`tls_domain` taken from a
    /// [`ProtoClient`](super::ProtoClient) impl and `probe_interval` =
    /// `DEFAULT_PROBE_INTERVAL`. The generic [`super::connect`]
    /// dispatches here when `balance_host` is set.
    pub async fn for_client<C: super::ProtoClient>(
        host: String,
        port: u16,
    ) -> anyhow::Result<Self> {
        Self::new(
            host,
            port,
            C::HEALTH_SERVICE.into(),
            C::TLS_DOMAIN.into(),
            DEFAULT_PROBE_INTERVAL,
        )
        .await
    }

    /// Get a clone of the balanced channel. Cheap --- tonic
    /// Channels are Arc-backed. Wrap in service clients as usual.
    pub fn channel(&self) -> Channel {
        self.channel.clone()
    }
}

/// Named service string for the scheduler's health check.
///
/// This is the `proto package` + `service name`. The scheduler's
/// health-toggle loop calls `set_not_serving::<SchedulerServiceServer<_>>()`
/// which registers under this name. Empty string (`""`) is a
/// DIFFERENT health entry and always reports SERVING (tonic-health
/// default) --- probing that would make both pods look healthy.
// r[impl ctrl.probe.named-service]
pub(crate) const SCHEDULER_HEALTH_SERVICE: &str = "rio.scheduler.SchedulerService";

/// Default TLS domain for scheduler connections. Matches the
/// first SAN in `infra/helm/rio-build/templates/cert-manager.yaml`.
pub(crate) const SCHEDULER_TLS_DOMAIN: &str = "rio-scheduler";

/// Default probe interval. CoreDNS headless-Service TTL is 5s
/// by default; 3s means we catch a leadership flip within one
/// missed heartbeat window.
pub(crate) const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// Health service rio-store registers via `set_serving::<StoreService
/// Server<_>>`. Unlike the scheduler, all store replicas are
/// equivalent (PG is the shared state) — the named-service probe is
/// just "migrations done + listening", not leader detection.
pub(crate) const STORE_HEALTH_SERVICE: &str = "rio.store.StoreService";

/// Default TLS domain for store connections. First SAN in
/// `infra/helm/rio-build/templates/cert-manager.yaml`.
pub const STORE_TLS_DOMAIN: &str = "rio-store";

/// Connect a `StoreAdminServiceClient` to a SPECIFIC pod IP (not the
/// balanced channel). The ComponentScaler reconciler fans out
/// `GetLoad` to every store pod — it needs each pod's individual
/// reading, so the p2c balanced channel (which would route all calls
/// to one or two pods) is the wrong tool.
///
/// Uses `build_endpoint` so the TLS-domain override applies (cert
/// SAN is `rio-store`, not the pod IP). Without this, mTLS deployments
/// fail the SAN check on every per-pod connect.
pub async fn connect_store_admin_at(
    addr: SocketAddr,
) -> anyhow::Result<crate::StoreAdminServiceClient<Channel>> {
    let ep = build_endpoint(addr, STORE_TLS_DOMAIN)?;
    let ch = ep.connect().await?;
    Ok(super::ProtoClient::wrap(ch))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Smoke: build_endpoint formats IPv4/v6 URIs correctly.
    /// (No TLS in tests --- `rio_common::grpc::CLIENT_TLS` is unset.)
    #[test]
    fn build_endpoint_formats_uri() {
        let v4: SocketAddr = "10.42.2.140:9001".parse().unwrap();
        let ep = build_endpoint(v4, "rio-scheduler").unwrap();
        assert_eq!(ep.uri().to_string(), "http://10.42.2.140:9001/");

        let v6: SocketAddr = "[::1]:9001".parse().unwrap();
        let ep = build_endpoint(v6, "rio-scheduler").unwrap();
        assert_eq!(ep.uri().to_string(), "http://[::1]:9001/");
    }

    /// Smoke: probe against a dead address returns false fast
    /// (connect_timeout, not PROBE_TIMEOUT --- port 1 refuses).
    #[tokio::test]
    async fn probe_dead_addr_false() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let start = std::time::Instant::now();
        assert!(!probe(addr, "x", "x").await);
        // Should fail fast (connection refused), not hit the 2s
        // timeout. Give it 1s of slack for CI.
        assert!(start.elapsed() < Duration::from_secs(1));
    }

    /// Integration: spin up a real tonic health server, flip
    /// between SERVING and NOT_SERVING, verify tick() sees it.
    /// Uses 127.0.0.1 as the "DNS result" --- lookup_host on
    /// 127.0.0.1 returns itself.
    #[tokio::test]
    async fn tick_follows_health_flip() {
        // Health server on an ephemeral port.
        let (reporter, health_svc) = tonic_health::server::health_reporter();
        // Start NOT_SERVING for our test service name.
        reporter
            .set_service_status("test.Service", tonic_health::ServingStatus::NotServing)
            .await;

        let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
        let addr = listener.local_addr().unwrap();
        let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
        tokio::spawn(
            tonic::transport::Server::builder()
                .add_service(health_svc)
                .serve_with_incoming(incoming),
        );

        // Balance discovery channel sink.
        let (tx, mut rx) = mpsc::channel(8);

        // Tick 1: NOT_SERVING → empty live set, no Insert.
        let live = tick(
            "127.0.0.1",
            addr.port(),
            "test.Service",
            "unused",
            &HashSet::new(),
            &tx,
        )
        .await;
        assert!(live.is_empty(), "NOT_SERVING should not be in live set");
        assert!(rx.try_recv().is_err(), "no Change should be emitted");

        // Flip to SERVING.
        reporter
            .set_service_status("test.Service", tonic_health::ServingStatus::Serving)
            .await;

        // Tick 2: SERVING → Insert.
        let live = tick(
            "127.0.0.1",
            addr.port(),
            "test.Service",
            "unused",
            &live,
            &tx,
        )
        .await;
        assert_eq!(live.len(), 1);
        match rx.try_recv().expect("Insert should be emitted") {
            Change::Insert(a, _) => assert_eq!(a, addr),
            Change::Remove(_) => panic!("expected Insert, got Remove"),
        }

        // Flip back to NOT_SERVING.
        reporter
            .set_service_status("test.Service", tonic_health::ServingStatus::NotServing)
            .await;

        // Tick 3: Remove.
        let live = tick(
            "127.0.0.1",
            addr.port(),
            "test.Service",
            "unused",
            &live,
            &tx,
        )
        .await;
        assert!(live.is_empty());
        match rx.try_recv().expect("Remove should be emitted") {
            Change::Remove(a) => assert_eq!(a, addr),
            Change::Insert(_, _) => panic!("expected Remove, got Insert"),
        }
    }
}
