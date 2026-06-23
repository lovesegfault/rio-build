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
//! Nothing here may assume the scheduler's 2-replica shape: the SAME
//! client balances the rio-store fleet, where EVERY replica is
//! SERVING and KEDA scales the pod count well past any fixed buffer
//! size. See `DISCOVERY_BUFFER` and [`BalancedChannel::new`] for
//! the init-ordering consequence.

// r[impl sched.grpc.leader-guard]
// r[impl proto.client.balanced]

use std::collections::{HashMap, HashSet};
use std::net::SocketAddr;
use std::time::Duration;

use futures_util::future::BoxFuture;
use tokio::sync::{mpsc, watch};
use tonic::transport::channel::Change;
use tonic::transport::{Channel, Endpoint};
use tonic_health::pb::health_check_response::ServingStatus;
use tonic_health::pb::{HealthCheckRequest, health_client::HealthClient};
use tracing::{debug, warn};

/// Per-endpoint health probe timeout. A pod that takes >2s to
/// answer Health/Check is as good as down for routing purposes.
const PROBE_TIMEOUT: Duration = Duration::from_secs(2);

/// Capacity of the discovery buffer between the probe loop and tonic's
/// p2c balancer (`Channel::balance_channel`).
///
/// This is a flow-control window, NOT a fleet-size bound. tonic drains
/// the buffer only when the `Channel` is polled by an RPC; whenever a
/// tick's change set outruns the buffer, the probe loop parks mid-feed
/// until the next RPC drains it. The store fleet KEDA-scales to a
/// values-driven `maxReplicaCount` (three-digit today), so NO constant
/// here can cover the fleet --- and nothing may treat "buffer accepted
/// the change" as a readiness signal. [`BalancedChannel::new`] keys
/// readiness off the discovery-side watch instead; this buffer only
/// needs to swallow a typical steady-state tick (a handful of churn
/// events) without parking. Beyond that it degrades to backpressure,
/// never to deadlock.
const DISCOVERY_BUFFER: usize = 32;

/// Pluggable endpoint resolution for the probe loop: each call yields
/// the current fleet (one `SocketAddr` per pod). Production is
/// `tokio::net::lookup_host` over the headless Service name (see
/// [`BalancedChannel::new`]); tests inject fixed fleets of arbitrary
/// size without real DNS.
type Resolver = Box<dyn FnMut() -> BoxFuture<'static, std::io::Result<Vec<SocketAddr>>> + Send>;

/// Endpoint builder: wraps a pod IP in an `Endpoint`. Factored out
/// because both the balance feed (Insert) and the health probe itself
/// need to connect to pod IPs with the same config.
fn build_endpoint(addr: SocketAddr) -> anyhow::Result<Endpoint> {
    // IPv6: wrap in brackets for the URI authority. SocketAddr's
    // Display does NOT bracket the IP part when you format the IP
    // alone, so do it manually.
    let uri = match addr {
        SocketAddr::V4(_) => format!("http://{}:{}", addr.ip(), addr.port()),
        SocketAddr::V6(a) => format!("http://[{}]:{}", a.ip(), a.port()),
    };
    let ep = Endpoint::from_shared(uri)?.connect_timeout(PROBE_TIMEOUT);
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
/// Takes a (cached, lazy) `Channel` rather than connecting fresh —
/// the channel can't be the balanced one (chicken/egg: we're
/// deciding whether to PUT this addr INTO the balance), but it CAN
/// be reused across ticks. The pre-cache version did
/// `endpoint.connect()` here, i.e. one fresh TCP+TLS handshake per
/// endpoint per 3s tick: with N store replicas × M builder pods that
/// was the dominant builder→store connection churn observed in
/// hubble (Q-001 — RST-after-FIN every 3.000s). The lazy channel
/// connects once on first RPC and h2-multiplexes subsequent checks.
async fn probe(addr: SocketAddr, ch: Channel, service: &str) -> bool {
    let fut = async {
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

/// One probe cycle: resolve endpoints, probe them all, diff against
/// the live set, emit Change::Insert/Remove. Returns the new live
/// set for the next cycle's diff.
///
/// `probe_ch` caches one lazy `Channel` per resolved addr so the
/// Health/Check RPC reuses an existing h2 connection instead of
/// dialing fresh every tick. Addrs that disappear from resolution are
/// evicted; new addrs get a `connect_lazy()` channel (connects on
/// first RPC, auto-reconnects on failure).
///
/// The cycle's SERVING count is published to `serving_seen` BEFORE
/// any `tx.send`: the sends park when the discovery buffer is full
/// and nothing polls the balanced channel yet (tonic only drains the
/// buffer when an RPC polls the `Channel`), so anything waiting on
/// "first probe cycle done" must key off this discovery-side signal,
/// not off buffer acceptance. See [`BalancedChannel::new`].
async fn tick(
    host: &str,
    service: &str,
    live: &HashSet<SocketAddr>,
    probe_ch: &mut HashMap<SocketAddr, Channel>,
    resolve: &mut Resolver,
    serving_seen: &watch::Sender<Option<usize>>,
    tx: &mpsc::Sender<Change<SocketAddr, Endpoint>>,
) -> HashSet<SocketAddr> {
    // Resolve. For a headless Service, CoreDNS returns ALL pod IPs
    // as AAAA records (TTL 5s by default; rio Services are
    // SingleStack IPv6). The production resolver is `lookup_host`
    // via the system resolver --- no extra deps; the returned
    // SocketAddrs carry the configured port. build_endpoint()
    // brackets v6 addrs for the URI authority.
    let resolved: HashSet<SocketAddr> = match resolve().await {
        Ok(addrs) => addrs.into_iter().collect(),
        Err(e) => {
            // DNS failure: keep the current live set unchanged.
            // Don't Remove everything --- a transient resolver
            // hiccup shouldn't eject the leader. Still publish the
            // (unchanged) count: a first-cycle DNS failure must
            // surface to BalancedChannel::new as "cycle done, zero
            // serving" rather than leaving it waiting.
            warn!(%host, error = %e, "balance: DNS resolve failed; keeping current endpoints");
            serving_seen.send_replace(Some(live.len()));
            return live.clone();
        }
    };

    // Sync the probe-channel cache to the resolved set: drop addrs
    // DNS no longer returns (pod gone → close its probe TCP), insert
    // lazy channels for new addrs. `connect_lazy()` never blocks/fails;
    // the actual connect happens on the first Health/Check below,
    // bounded by PROBE_TIMEOUT.
    probe_ch.retain(|addr, _| resolved.contains(addr));
    for &addr in &resolved {
        if let std::collections::hash_map::Entry::Vacant(slot) = probe_ch.entry(addr) {
            match build_endpoint(addr) {
                Ok(ep) => {
                    slot.insert(ep.connect_lazy());
                }
                Err(e) => warn!(%addr, error = %e, "probe: bad endpoint"),
            }
        }
    }

    // Probe all resolved addrs concurrently. Bounded by the fleet
    // size (2 scheduler pods; up to the KEDA cap for the store);
    // do it in parallel so one slow pod doesn't delay the rest.
    let mut probes = Vec::with_capacity(resolved.len());
    for &addr in &resolved {
        let Some(ch) = probe_ch.get(&addr).cloned() else {
            continue;
        };
        let service = service.to_string();
        probes.push(async move { (addr, probe(addr, ch, &service).await) });
    }
    let results: Vec<(SocketAddr, bool)> = futures_util::future::join_all(probes).await;
    let serving: HashSet<SocketAddr> = results
        .into_iter()
        .filter_map(|(a, ok)| ok.then_some(a))
        .collect();

    // Discovery is done --- publish before feeding the balancer (the
    // sends below may park on a full buffer; see fn doc).
    serving_seen.send_replace(Some(serving.len()));

    // Diff. Send Insert for new SERVING, Remove for no-longer-SERVING.
    // Order doesn't matter to p2c.
    for &addr in serving.difference(live) {
        match build_endpoint(addr) {
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
///
/// `#[must_use]` is the bug_088 tripwire: dropping the guard aborts
/// the probe loop and the channel silently routes to stale endpoints
/// after the first upstream rollout. (Destructuring the
/// `Option<BalancedChannel>` half of `connect_raw`'s return into `_`
/// still bypasses the lint — main.rs binding `_store_balance_guard`
/// is the held convention.)
#[must_use = "dropping a BalancedChannel aborts the endpoint probe; \
              hold the guard for process lifetime (bug_088)"]
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
    /// - `probe_interval`: how often to re-resolve + re-probe.
    ///   Should be ≤ CoreDNS TTL (5s default) or we miss flips.
    ///
    /// Waits until the first probe cycle has *observed* the fleet,
    /// then returns. Errors only if that first cycle finds ZERO
    /// serving endpoints --- the balance channel would be empty,
    /// first RPC fails "no ready endpoints," caller errors out of
    /// main anyway (cold-start retry loops re-run construction).
    /// Better to fail here with a clear message.
    ///
    /// Readiness is signaled from the DISCOVERY side (the probe
    /// loop's watch), NOT from the discovery buffer draining: tonic
    /// only drains `Change`s when the `Channel` is polled, and
    /// nothing polls it until the caller issues its first RPC. A
    /// fleet larger than `DISCOVERY_BUFFER` parks the probe feed
    /// mid-tick until that first RPC drains it; construction still
    /// completes, and the buffered endpoints are enough to route.
    pub async fn new(
        host: String,
        port: u16,
        health_service: String,
        probe_interval: Duration,
    ) -> anyhow::Result<Self> {
        let resolver: Resolver = {
            let host = host.clone();
            Box::new(move || {
                let host = host.clone();
                Box::pin(async move {
                    let addrs = tokio::net::lookup_host((host.as_str(), port)).await?;
                    Ok(addrs.collect::<Vec<_>>())
                })
            })
        };
        Self::new_with_resolver(host, port, health_service, probe_interval, resolver).await
    }

    /// [`Self::new`] with resolution injected --- the seam that lets
    /// tests present arbitrary fleet sizes without real DNS.
    async fn new_with_resolver(
        host: String,
        port: u16,
        health_service: String,
        probe_interval: Duration,
        mut resolve: Resolver,
    ) -> anyhow::Result<Self> {
        let (channel, tx) = Channel::balance_channel::<SocketAddr>(DISCOVERY_BUFFER);
        // Discovery-side "cycle done" signal: `Some(n)` = the most
        // recent completed probe cycle observed n SERVING endpoints.
        let (serving_seen, mut first_cycle) = watch::channel(None::<usize>);

        // Background probe loop --- INCLUDING the first cycle. The
        // first cycle must NOT run inline here: with more SERVING
        // endpoints than DISCOVERY_BUFFER, the discovery feed parks
        // on a full buffer until an RPC polls the channel, and at
        // construction time no RPC exists --- an inline first cycle
        // deadlocks startup. (Seen live: a store fleet KEDA-scaled
        // past the buffer wedged every builder pod at "balance:
        // insert" #33, never reaching "balanced channel
        // initialized".)
        let task = tokio::spawn({
            let host = host.clone();
            let health_service = health_service.clone();
            async move {
                let mut live = HashSet::new();
                let mut probe_ch = HashMap::new();
                let mut interval = tokio::time::interval(probe_interval);
                // A cycle can park on the buffer feed for as long as
                // the caller goes without an RPC; don't burst-replay
                // the missed ticks afterwards.
                interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                loop {
                    // First iteration fires immediately (tokio
                    // interval semantics) --- that IS the first cycle.
                    interval.tick().await;
                    live = tick(
                        &host,
                        &health_service,
                        &live,
                        &mut probe_ch,
                        &mut resolve,
                        &serving_seen,
                        &tx,
                    )
                    .await;
                }
            }
        });
        // Construct the guard BEFORE the first await so every early
        // exit below (zero-endpoint bail, caller dropping this future
        // mid-wait) aborts the probe task via Drop --- no leaked
        // probe loop.
        let guard = Self {
            channel,
            _task: task,
        };

        // Wait for the first cycle's DISCOVERY result. This is the
        // readiness signal --- "first nonempty SERVING set observed"
        // --- and it deliberately does not require the balancer to
        // have accepted the corresponding Changes (tonic drains those
        // only once the caller's first RPC polls the channel).
        let first = match first_cycle.wait_for(Option::is_some).await {
            // `unwrap_or` instead of unwrap/expect: the predicate
            // guarantees Some, but degrading an impossible None to
            // the zero-endpoint error beats a panic path.
            Ok(seen) => (*seen).unwrap_or(0),
            Err(_) => anyhow::bail!(
                "balance: probe task exited before completing the first probe cycle \
                 for {host}:{port} (service={health_service})"
            ),
        };
        anyhow::ensure!(
            first > 0,
            "balance: no SERVING endpoints for {host}:{port} (service={health_service}); \
             either DNS returned nothing or all probes failed. \
             Check headless Service selector and that at least one pod is leader."
        );
        tracing::info!(
            %host, port, endpoints = first,
            "balanced channel initialized"
        );
        Ok(guard)
    }

    /// [`Self::new`] with `health_service` taken from a
    /// [`ProtoClient`](super::ProtoClient) impl and `probe_interval` =
    /// `DEFAULT_PROBE_INTERVAL`. The generic [`super::connect`]
    /// dispatches here when `balance_host` is set.
    pub async fn for_client<C: super::ProtoClient>(
        host: String,
        port: u16,
    ) -> anyhow::Result<Self> {
        Self::new(host, port, C::HEALTH_SERVICE.into(), DEFAULT_PROBE_INTERVAL).await
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

/// Default probe interval. CoreDNS headless-Service TTL is 5s
/// by default; 3s means we catch a leadership flip within one
/// missed heartbeat window.
pub(crate) const DEFAULT_PROBE_INTERVAL: Duration = Duration::from_secs(3);

/// Health service rio-store registers via `set_serving::<StoreService
/// Server<_>>`. Unlike the scheduler, all store replicas are
/// equivalent (PG is the shared state) — the named-service probe is
/// just "migrations done + listening", not leader detection.
pub(crate) const STORE_HEALTH_SERVICE: &str = "rio.store.StoreService";

/// Connect a `StoreAdminServiceClient` to a SPECIFIC pod IP (not the
/// balanced channel). The ComponentScaler reconciler fans out
/// `GetLoad` to every store pod — it needs each pod's individual
/// reading, so the p2c balanced channel (which would route all calls
/// to one or two pods) is the wrong tool. Takes an interceptor so the
/// caller can attach `x-rio-service-token`
/// (`r[store.admin.service-gate]`).
pub async fn connect_store_admin_at<I>(
    addr: SocketAddr,
    interceptor: I,
) -> anyhow::Result<
    crate::StoreAdminServiceClient<tonic::service::interceptor::InterceptedService<Channel, I>>,
>
where
    I: tonic::service::Interceptor,
{
    let ep = build_endpoint(addr)?;
    let ch = ep.connect().await?;
    Ok(crate::StoreAdminServiceClient::with_interceptor(
        ch,
        interceptor,
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    /// A [`Resolver`] that always yields the same fixed fleet.
    fn fixed_resolver(addrs: Vec<SocketAddr>) -> Resolver {
        Box::new(move || {
            let addrs = addrs.clone();
            Box::pin(async move { Ok(addrs) })
        })
    }

    /// Spawn `n` real tonic health servers on ephemeral loopback
    /// ports, all SERVING for `service`. Returns their addrs; the
    /// server tasks (and their reporters) live for the test duration.
    async fn spawn_serving_fleet(n: usize, service: &str) -> Vec<SocketAddr> {
        let mut addrs = Vec::with_capacity(n);
        for _ in 0..n {
            let (reporter, health_svc) = tonic_health::server::health_reporter();
            reporter
                .set_service_status(service, tonic_health::ServingStatus::Serving)
                .await;
            let listener = tokio::net::TcpListener::bind("127.0.0.1:0").await.unwrap();
            addrs.push(listener.local_addr().unwrap());
            let incoming = tokio_stream::wrappers::TcpListenerStream::new(listener);
            tokio::spawn(async move {
                // Keep the reporter alive for the server's lifetime so
                // the SERVING entry can't be torn down mid-test.
                let _reporter = reporter;
                let _ = tonic::transport::Server::builder()
                    .add_service(health_svc)
                    .serve_with_incoming(incoming)
                    .await;
            });
        }
        addrs
    }

    /// Regression: a fleet larger than [`DISCOVERY_BUFFER`] must not
    /// deadlock construction. tonic only drains the discovery buffer
    /// when the `Channel` is polled, and at construction time NO RPC
    /// exists to poll it --- so init must complete on the
    /// discovery-side signal alone. Pre-fix, `BalancedChannel::new`
    /// ran the first probe cycle inline and parked forever on Insert
    /// #(DISCOVERY_BUFFER+1); a store fleet scaled past the buffer
    /// wedged every consumer pod at startup, before "balanced channel
    /// initialized".
    #[tokio::test]
    async fn init_completes_when_fleet_exceeds_discovery_buffer() {
        let fleet = DISCOVERY_BUFFER + 1;
        let addrs = spawn_serving_fleet(fleet, "test.Service").await;

        let bc = tokio::time::timeout(
            Duration::from_secs(15),
            BalancedChannel::new_with_resolver(
                "test-fleet".into(),
                0,
                "test.Service".into(),
                Duration::from_secs(3),
                fixed_resolver(addrs),
            ),
        )
        .await
        .expect(
            "BalancedChannel construction deadlocked: init must not depend on \
             the discovery buffer draining (nothing polls the channel before \
             the first RPC)",
        )
        .expect("init failed against an all-SERVING fleet");

        // The channel must be USABLE: the first RPC polls the
        // Channel, which drains the parked discovery feed and routes
        // to a real backend.
        let mut hc = HealthClient::new(bc.channel());
        let resp = tokio::time::timeout(
            Duration::from_secs(15),
            hc.check(HealthCheckRequest {
                service: "test.Service".to_string(),
            }),
        )
        .await
        .expect("first RPC through the balanced channel hung")
        .expect("health check through balanced channel failed");
        assert_eq!(resp.into_inner().status(), ServingStatus::Serving);
    }

    /// First cycle with zero SERVING endpoints must ERROR (fast), not
    /// wait --- cold-start retry loops (`connect_forever`) re-run
    /// construction with backoff, and a clear error beats a hang.
    #[tokio::test]
    async fn init_errors_when_no_serving_endpoints() {
        let r = tokio::time::timeout(
            Duration::from_secs(15),
            BalancedChannel::new_with_resolver(
                "test-empty".into(),
                0,
                "test.Service".into(),
                Duration::from_secs(3),
                fixed_resolver(Vec::new()),
            ),
        )
        .await
        .expect("zero-endpoint init must fail fast, not hang");
        let err = match r {
            Ok(_) => panic!("zero SERVING endpoints must be a construction error"),
            Err(e) => e,
        };
        assert!(err.to_string().contains("no SERVING endpoints"), "{err}");
    }

    /// Smoke: build_endpoint formats IPv4/v6 URIs correctly.
    #[test]
    fn build_endpoint_formats_uri() {
        let v4: SocketAddr = "10.42.2.140:9001".parse().unwrap();
        let ep = build_endpoint(v4).unwrap();
        assert_eq!(ep.uri().to_string(), "http://10.42.2.140:9001/");

        let v6: SocketAddr = "[::1]:9001".parse().unwrap();
        let ep = build_endpoint(v6).unwrap();
        assert_eq!(ep.uri().to_string(), "http://[::1]:9001/");

        // Non-loopback v6 — the bracket-wrapping path BalancedChannel uses
        // for v6 pod IPs in a v6-only cluster.
        let v6_pod: SocketAddr = "[2001:db8::1]:9001".parse().unwrap();
        let ep = build_endpoint(v6_pod).unwrap();
        assert_eq!(ep.uri().to_string(), "http://[2001:db8::1]:9001/");
    }

    /// Smoke: probe against a dead address returns false fast
    /// (connect_timeout, not PROBE_TIMEOUT --- port 1 refuses).
    #[tokio::test]
    async fn probe_dead_addr_false() {
        let addr: SocketAddr = "127.0.0.1:1".parse().unwrap();
        let ch = build_endpoint(addr).unwrap().connect_lazy();
        let start = std::time::Instant::now();
        assert!(!probe(addr, ch, "x").await);
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
        let (seen, _seen_rx) = watch::channel(None);
        let mut probe_ch = HashMap::new();
        let mut resolve = fixed_resolver(vec![addr]);

        // Tick 1: NOT_SERVING → empty live set, no Insert.
        let live = tick(
            "127.0.0.1",
            "test.Service",
            &HashSet::new(),
            &mut probe_ch,
            &mut resolve,
            &seen,
            &tx,
        )
        .await;
        assert!(live.is_empty(), "NOT_SERVING should not be in live set");
        assert!(rx.try_recv().is_err(), "no Change should be emitted");
        assert_eq!(probe_ch.len(), 1, "probe channel cached after first tick");
        assert_eq!(*seen.borrow(), Some(0), "cycle published zero serving");

        // Flip to SERVING.
        reporter
            .set_service_status("test.Service", tonic_health::ServingStatus::Serving)
            .await;

        // Tick 2: SERVING → Insert.
        let live = tick(
            "127.0.0.1",
            "test.Service",
            &live,
            &mut probe_ch,
            &mut resolve,
            &seen,
            &tx,
        )
        .await;
        assert_eq!(live.len(), 1);
        match rx.try_recv().expect("Insert should be emitted") {
            Change::Insert(a, _) => assert_eq!(a, addr),
            Change::Remove(_) => panic!("expected Insert, got Remove"),
        }
        assert_eq!(*seen.borrow(), Some(1), "cycle published one serving");

        // Flip back to NOT_SERVING.
        reporter
            .set_service_status("test.Service", tonic_health::ServingStatus::NotServing)
            .await;

        // Tick 3: Remove.
        let live = tick(
            "127.0.0.1",
            "test.Service",
            &live,
            &mut probe_ch,
            &mut resolve,
            &seen,
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
