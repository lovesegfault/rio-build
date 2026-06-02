//! Thin trait facades over the rio gRPC surfaces the engine reads, so every
//! stage is unit-testable with in-memory fakes. The tonic-backed impls are
//! deliberately dumb adapters (no logic beyond chunking, rolling log-tail
//! truncation, and a per-attempt deadline + reconnect retry on connect
//! failures / UNAVAILABLE) — the RPC plumbing is exercised against a live
//! cluster during the first smoke campaign, while the retry, deadline, and
//! truncation logic has unit tests below (offline plus a scripted local
//! mock server).

use std::collections::HashMap;
use std::time::Duration;

use anyhow::{Context, Result};
use async_trait::async_trait;

/// Validity/nar-hash lookups against rio-store. Local-only semantics:
/// `BatchQueryPathInfo` reports what is already valid in rio-store and never
/// triggers substitution.
#[async_trait]
pub trait StoreApi: Send + Sync {
    /// For every requested path: Some((nar_hash_hex, nar_size)) when the path
    /// is valid in rio-store, None otherwise. Order-insensitive.
    async fn query_valid(&self, paths: &[String])
    -> Result<HashMap<String, Option<(String, u64)>>>;
}

/// Chunk size for BatchQueryPathInfo calls (store-side `= ANY(...)` query;
/// 500 keeps requests well under message-size limits).
pub const BATCH_QUERY_CHUNK: usize = 500;

/// [`StoreApi`] backed by the rio-store StoreService gRPC endpoint.
pub struct GrpcStoreApi {
    addr: String,
    timeout: Duration,
}

impl GrpcStoreApi {
    /// `timeout` is the per-chunk RPC deadline (each `BatchQueryPathInfo`
    /// call covers at most [`BATCH_QUERY_CHUNK`] paths).
    pub fn new(addr: impl Into<String>, timeout: Duration) -> Self {
        Self {
            addr: addr.into(),
            timeout,
        }
    }
}

#[async_trait]
impl StoreApi for GrpcStoreApi {
    async fn query_valid(
        &self,
        paths: &[String],
    ) -> Result<HashMap<String, Option<(String, u64)>>> {
        let channel = rio_proto::client::connect_channel(&self.addr)
            .await
            .with_context(|| format!("connect rio-store at {}", self.addr))?;
        let mut client = rio_proto::StoreServiceClient::new(channel)
            .max_decoding_message_size(rio_common::grpc::max_message_size())
            .max_encoding_message_size(rio_common::grpc::max_message_size());
        let mut out = HashMap::with_capacity(paths.len());
        for chunk in paths.chunks(BATCH_QUERY_CHUNK) {
            let entries = rio_proto::client::batch_query_path_info(
                &mut client,
                chunk.to_vec(),
                self.timeout,
                &[],
            )
            .await
            .map_err(|s| anyhow::anyhow!("BatchQueryPathInfo against {}: {s}", self.addr))?;
            for (path, info) in entries {
                out.insert(path, info.map(|i| (hex::encode(i.nar_hash), i.nar_size)));
            }
        }
        Ok(out)
    }
}

// ───────────────────────── Admin / Cluster facades ─────────────────────────

/// One node of a build's PG-backed DAG snapshot, as collect consumes it.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphNodeView {
    pub drv_path: String,
    /// Scheduler `derivations.status` string ("created"…"completed",
    /// "poisoned", "dependency_failed", "cancelled", "skipped").
    pub status: String,
    /// exec_id observed by THIS build for this drv; empty = no execution
    /// observed (cache hit / cascaded / never dispatched).
    pub exec_id: String,
    pub assigned_executor_id: String,
}

/// One build's DAG snapshot from `GetBuildGraph` (node status only — the
/// engine never needs the edges).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct GraphSnapshot {
    pub nodes: Vec<GraphNodeView>,
    pub truncated: bool,
    pub total_nodes: u32,
}

/// One poisoned derivation from `ListPoisoned`: which executors failed it
/// and how long ago it was poisoned (the evidence decays with the
/// scheduler's poison TTL).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct PoisonedView {
    pub drv_path: String,
    pub failed_executors: Vec<String>,
    pub poisoned_secs_ago: u64,
}

/// AdminService reads used by collect and the watchdog escalation.
#[async_trait]
pub trait AdminApi: Send + Sync {
    async fn get_build_graph(&self, build_id: &str) -> Result<GraphSnapshot>;
    async fn list_poisoned(&self) -> Result<Vec<PoisonedView>>;
    /// Last `max_bytes` of the drv's log (latest execution when exec_id is None).
    async fn log_tail(
        &self,
        drv_path: &str,
        exec_id: Option<&str>,
        max_bytes: usize,
    ) -> Result<Vec<u8>>;
    /// Recovery fallback when the `rio: build <uuid>` line was lost: list
    /// builds for the campaign tenant (most recent first). Returns
    /// `(build_id, submitted_at)` pairs; the timestamp is best-effort
    /// display text for correlating with batch start times.
    async fn list_builds(&self, tenant: &str, limit: u32) -> Result<Vec<(String, Option<String>)>>;
}

/// Cluster-wide queue/executor counts from `ClusterStatus`, polled by the
/// suspension predicate (cluster-idle and dispatch-gap detection).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct ClusterCounts {
    pub active_executors: u32,
    pub queued_derivations: u32,
    pub running_derivations: u32,
    pub substituting_derivations: u32,
}

/// Capacity-health snapshot from `GetSpawnIntents`: cells the scheduler has
/// masked as ICE-infeasible and nodes it considers dead.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct IceSnapshot {
    pub ice_masked_cells: Vec<String>,
    pub dead_nodes: Vec<String>,
}

/// Cluster health reads used by the suspension predicate.
#[async_trait]
pub trait ClusterApi: Send + Sync {
    async fn cluster_status(&self) -> Result<ClusterCounts>;
    async fn spawn_intents(&self) -> Result<IceSnapshot>;
}

/// Caller string the engine signs ServiceClaims with. Must stay listed in
/// the scheduler AdminService allowlists for the read RPCs the engine uses
/// (GetBuildGraph, ListPoisoned, GetSpawnIntents).
pub const ENGINE_CALLER: &str = "rio-replay";

/// Retry policy for leader-gated AdminService reads answered with
/// UNAVAILABLE by a standby or during a failover window. Failover usually
/// resolves within seconds, so the cap stays well under the collect poll
/// cadence. `Backoff` has no `Default` by design — per-site constants stay
/// local (`rio-common/src/backoff.rs`); proportional jitter keeps the
/// concurrent collect/poller tasks from reconnecting in lockstep.
const ADMIN_RETRY_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(500),
    mult: 2.0,
    cap: Duration::from_secs(15),
    jitter: rio_common::backoff::Jitter::Proportional(0.25),
};

/// Per-attempt deadline for unary admin RPCs. These are reads the leader
/// answers from memory or one PG query in milliseconds — the budget is
/// not a latency target, it bounds the pathological case: a leader that
/// dies ungracefully mid-RPC (node loss, netsplit — no FIN, no RST)
/// leaves the socket ESTABLISHED and the await otherwise pending until
/// kernel TCP keepalive (~2h on Linux). An elapsed deadline is retried
/// like UNAVAILABLE; the fresh connection re-resolves to the new leader.
const ADMIN_RPC_TIMEOUT: Duration = Duration::from_secs(60);

/// Per-attempt budget for `GetDerivationLogs`: covers the call PLUS full
/// stream consumption, since the whole op runs inside the retry loop.
/// The engine only fetches logs of finished executions, so the server
/// streams existing data and completes — five minutes absorbs a
/// hundreds-of-MB log on a congested link while still bounding an
/// ungraceful peer death mid-stream (which the channel's h2 keepalive
/// usually surfaces much sooner, in ~40s).
const ADMIN_STREAM_TIMEOUT: Duration = Duration::from_secs(300);

/// Failure of one attempt inside `GrpcAdminApi::with_retry`.
///
/// `Rpc` is the plain tonic outcome: UNAVAILABLE is retried (standby
/// replica, failover window), everything else is terminal. `Fatal` opts
/// a failure OUT of retry even when the underlying status is
/// UNAVAILABLE — `log_tail` uses it for a stream that breaks after data
/// already flowed, where a retry would restart the log from line 0 and
/// splice two reads together (see `collect_log_tail`).
#[derive(Debug)]
enum CallError {
    Rpc(tonic::Status),
    Fatal(anyhow::Error),
}

impl From<tonic::Status> for CallError {
    fn from(status: tonic::Status) -> Self {
        Self::Rpc(status)
    }
}

type AdminClient = rio_proto::AdminServiceClient<
    tonic::service::interceptor::InterceptedService<
        tonic::transport::Channel,
        rio_auth::hmac::ServiceTokenInterceptor,
    >,
>;

/// Append one streamed log line (plus the newline the chunking strips) to
/// `buf`, then trim `buf` from the FRONT so it never holds more than
/// `max_bytes`. The rolling trim keeps memory bounded while streaming
/// arbitrarily large logs — only the requested tail ever stays buffered.
fn push_log_tail_line(buf: &mut Vec<u8>, line: &[u8], max_bytes: usize) {
    buf.extend_from_slice(line);
    buf.push(b'\n');
    if buf.len() > max_bytes {
        let excess = buf.len() - max_bytes;
        buf.drain(..excess);
    }
}

/// Tonic-backed AdminService facade with reconnect-on-not-leader retry
/// and a per-attempt deadline owned by the retry loop:
///
/// - The leader-gated reads (ClusterStatus, GetSpawnIntents,
///   GetBuildGraph) answer UNAVAILABLE from a standby replica, so every
///   call runs against a fresh connection and retries UNAVAILABLE with
///   backoff. GetDerivationLogs reports not-leader as the FIRST
///   in-stream item instead (grpc-web compatibility), so its stream
///   consumption runs inside the retry loop too — see `collect_log_tail`.
/// - Every attempt is bounded (`with_retry` owns the deadline) and the
///   channel carries h2 keepalive, so an ungracefully dead leader (node
///   loss, netsplit — no FIN) becomes a retried attempt instead of an
///   indefinite hang of the poller/collect task that issued the RPC.
pub struct GrpcAdminApi {
    addr: String,
    signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    max_attempts: u32,
    /// Per-attempt deadline for unary RPCs ([`ADMIN_RPC_TIMEOUT`] in
    /// production; tests shrink it to keep wall time low).
    rpc_timeout: Duration,
    /// Per-attempt budget for the log stream, call plus consumption
    /// ([`ADMIN_STREAM_TIMEOUT`] in production).
    stream_timeout: Duration,
}

impl GrpcAdminApi {
    /// `addr` is the scheduler AdminService endpoint; `hmac_key_path` is the
    /// shared service HMAC key used to mint `x-rio-service-token` (None =
    /// dev mode, no token).
    pub fn new(addr: impl Into<String>, hmac_key_path: Option<&std::path::Path>) -> Result<Self> {
        let signer = rio_auth::hmac::HmacSigner::load(hmac_key_path)
            .map_err(|e| anyhow::anyhow!("service HMAC key load: {e}"))?
            .map(std::sync::Arc::new);
        Ok(Self {
            addr: addr.into(),
            signer,
            max_attempts: 5,
            rpc_timeout: ADMIN_RPC_TIMEOUT,
            stream_timeout: ADMIN_STREAM_TIMEOUT,
        })
    }

    async fn client(&self) -> Result<AdminClient> {
        // Keepalive, not just throughput tuning: the log stream can sit
        // on one connection across several PING intervals, and keepalive
        // turns an ungracefully dead peer into a stream error in ~40s —
        // the per-attempt deadline in `with_retry` is the backstop, not
        // the primary detector.
        let channel = rio_proto::client::connect_channel_keepalive(&self.addr)
            .await
            .with_context(|| format!("connect scheduler at {}", self.addr))?;
        Ok(rio_proto::AdminServiceClient::with_interceptor(
            channel,
            rio_auth::hmac::ServiceTokenInterceptor::new(self.signer.clone(), ENGINE_CALLER),
        )
        .max_decoding_message_size(rio_common::grpc::max_message_size())
        .max_encoding_message_size(rio_common::grpc::max_message_size()))
    }

    /// Run `op` against a fresh client with a per-attempt deadline of
    /// `budget`, retrying connect failures (scheduler restart, DNS
    /// blip), UNAVAILABLE responses (not-leader, reconnect window), and
    /// elapsed deadlines (ungracefully dead peer) with the same backoff
    /// and attempt budget. Other statuses are returned to the caller;
    /// exhausted retries name the scheduler address and the attempt
    /// count. [`CallError::Fatal`] failures return immediately, never
    /// retried.
    ///
    /// The deadline lives HERE rather than at call sites so an admin RPC
    /// cannot be added without a bound: a leader that dies without FIN
    /// mid-RPC otherwise hangs the await until kernel TCP keepalive
    /// (~2h), freezing the sequential poller loop — including the
    /// watchdog ticks that would have reported the stall. The eager
    /// connect in `client()` is bounded separately by rio-proto's
    /// connect timeout.
    async fn with_retry<T, E, F, Fut>(&self, what: &str, budget: Duration, mut op: F) -> Result<T>
    where
        F: FnMut(AdminClient) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, E>>,
        E: Into<CallError>,
    {
        let mut attempt = 0u32;
        loop {
            let client = match self.client().await {
                Ok(client) => client,
                Err(e) if attempt + 1 < self.max_attempts => {
                    tracing::warn!(
                        rpc = what,
                        attempt,
                        error = %format!("{e:#}"),
                        "scheduler connect failed; retrying"
                    );
                    tokio::time::sleep(ADMIN_RETRY_BACKOFF.duration(attempt)).await;
                    attempt += 1;
                    continue;
                }
                Err(e) => {
                    return Err(e.context(format!(
                        "{what}: scheduler at {} unreachable after {} attempts",
                        self.addr, self.max_attempts
                    )));
                }
            };
            match tokio::time::timeout(budget, op(client)).await {
                Ok(Ok(v)) => return Ok(v),
                Err(_elapsed) if attempt + 1 < self.max_attempts => {
                    tracing::warn!(
                        rpc = what,
                        attempt,
                        budget_secs = budget.as_secs(),
                        "admin RPC hit its per-attempt deadline; reconnecting and retrying"
                    );
                    tokio::time::sleep(ADMIN_RETRY_BACKOFF.duration(attempt)).await;
                    attempt += 1;
                }
                Err(_elapsed) => {
                    return Err(anyhow::anyhow!(
                        "{what} against scheduler at {}: no response within {:?} in any of {} \
                         attempts",
                        self.addr,
                        budget,
                        self.max_attempts
                    ));
                }
                Ok(Err(e)) => match e.into() {
                    CallError::Rpc(s)
                        if s.code() == tonic::Code::Unavailable
                            && attempt + 1 < self.max_attempts =>
                    {
                        tracing::warn!(
                            rpc = what,
                            attempt,
                            msg = %s.message(),
                            "admin RPC unavailable; retrying"
                        );
                        tokio::time::sleep(ADMIN_RETRY_BACKOFF.duration(attempt)).await;
                        attempt += 1;
                    }
                    CallError::Rpc(s) if s.code() == tonic::Code::Unavailable => {
                        return Err(anyhow::anyhow!(
                            "{what} against scheduler at {}: still UNAVAILABLE after {} \
                             attempts: {}",
                            self.addr,
                            self.max_attempts,
                            s.message()
                        ));
                    }
                    CallError::Rpc(s) => {
                        return Err(anyhow::anyhow!(
                            "{what} against scheduler at {}: {} ({:?})",
                            self.addr,
                            s.message(),
                            s.code()
                        ));
                    }
                    CallError::Fatal(e) => return Err(e),
                },
            }
        }
    }
}

/// Drain a `GetDerivationLogs` stream into a rolling `max_bytes` tail.
///
/// The scheduler delivers errors IN-STREAM on an otherwise-successful
/// RPC (grpc-web clients can't read trailers, so leader-gated stream
/// handlers return `Ok(stream)` whose first item is the status — see the
/// scheduler's `logs::err_stream`). A status arriving BEFORE any chunk —
/// not-leader from a standby, NOT_FOUND for a missing log — is therefore
/// really the RPC-level outcome: it maps to [`CallError::Rpc`], which
/// restores the facade's UNAVAILABLE retry for this RPC. A stream that
/// breaks AFTER chunks flowed maps to [`CallError::Fatal`]: a retry
/// would restart from line 0 (and may resolve a different replica),
/// splicing two reads of the log into one tail — callers treat the tail
/// as failure evidence, where a hard error beats silently incomplete
/// output.
async fn collect_log_tail<S>(
    mut stream: S,
    drv_path: &str,
    max_bytes: usize,
) -> std::result::Result<Vec<u8>, CallError>
where
    S: futures_util::Stream<
            Item = std::result::Result<rio_proto::types::DerivationLogChunk, tonic::Status>,
        > + Unpin,
{
    use futures_util::StreamExt;

    let mut buf: Vec<u8> = Vec::new();
    let mut saw_chunk = false;
    while let Some(item) = stream.next().await {
        match item {
            Ok(chunk) => {
                saw_chunk = true;
                // Trim rolling, per line, so the buffer never grows past
                // the requested tail even when the full log is hundreds
                // of MB.
                for line in chunk.lines {
                    push_log_tail_line(&mut buf, &line, max_bytes);
                }
                if chunk.is_complete {
                    break;
                }
            }
            Err(status) if !saw_chunk => return Err(CallError::Rpc(status)),
            Err(status) => {
                return Err(CallError::Fatal(anyhow::anyhow!(
                    "GetDerivationLogs stream for {drv_path} broke after data started flowing \
                     (not retried — a fresh attempt would restart the log from line 0): {status}"
                )));
            }
        }
    }
    Ok(buf)
}

#[async_trait]
impl AdminApi for GrpcAdminApi {
    async fn get_build_graph(&self, build_id: &str) -> Result<GraphSnapshot> {
        let resp = self
            .with_retry("GetBuildGraph", self.rpc_timeout, |mut c| {
                let req = rio_proto::types::GetBuildGraphRequest {
                    build_id: build_id.to_string(),
                };
                async move { c.get_build_graph(req).await.map(|r| r.into_inner()) }
            })
            .await?;
        Ok(GraphSnapshot {
            nodes: resp
                .nodes
                .into_iter()
                .map(|n| GraphNodeView {
                    drv_path: n.drv_path,
                    status: n.status,
                    exec_id: n.exec_id,
                    assigned_executor_id: n.assigned_executor_id,
                })
                .collect(),
            truncated: resp.truncated,
            total_nodes: resp.total_nodes,
        })
    }

    async fn list_poisoned(&self) -> Result<Vec<PoisonedView>> {
        let resp = self
            .with_retry("ListPoisoned", self.rpc_timeout, |mut c| async move {
                c.list_poisoned(()).await.map(|r| r.into_inner())
            })
            .await?;
        Ok(resp
            .derivations
            .into_iter()
            .map(|p| PoisonedView {
                drv_path: p.drv_path,
                failed_executors: p.failed_executors,
                poisoned_secs_ago: p.poisoned_secs_ago,
            })
            .collect())
    }

    async fn log_tail(
        &self,
        drv_path: &str,
        exec_id: Option<&str>,
        max_bytes: usize,
    ) -> Result<Vec<u8>> {
        let drv = drv_path.to_string();
        let exec = exec_id.unwrap_or("").to_string();
        // The WHOLE stream consumption runs inside the retry loop, not
        // just the initial call: the scheduler reports not-leader as the
        // first in-stream item (never as an RPC-level error), so the
        // retry must see stream errors to honor the facade's not-leader
        // contract. Each attempt starts a fresh buffer — partial output
        // from a retried attempt is never mixed into the next one.
        self.with_retry("GetDerivationLogs", self.stream_timeout, |mut c| {
            let drv = drv.clone();
            let exec = exec.clone();
            async move {
                let req = rio_proto::types::GetDerivationLogsRequest {
                    derivation_path: drv.clone(),
                    exec_id: exec,
                    since_line: 0,
                };
                let stream = c.get_derivation_logs(req).await?.into_inner();
                collect_log_tail(stream, &drv, max_bytes).await
            }
        })
        .await
    }

    async fn list_builds(&self, tenant: &str, limit: u32) -> Result<Vec<(String, Option<String>)>> {
        let tenant = tenant.to_string();
        let resp = self
            .with_retry("ListBuilds", self.rpc_timeout, |mut c| {
                let req = rio_proto::types::ListBuildsRequest {
                    status_filter: String::new(),
                    limit,
                    offset: 0,
                    tenant_filter: tenant.clone(),
                    cursor: None,
                };
                async move { c.list_builds(req).await.map(|r| r.into_inner()) }
            })
            .await?;
        Ok(resp
            .builds
            .into_iter()
            .map(|b| {
                // Render as RFC3339 for human correlation with batch start
                // times; an out-of-range timestamp falls back to the raw
                // seconds.nanos pair rather than dropping the row.
                let submitted_at = b.submitted_at.map(|t| {
                    jiff::Timestamp::new(t.seconds, t.nanos)
                        .map(|ts| ts.to_string())
                        .unwrap_or_else(|_| format!("{}.{}", t.seconds, t.nanos))
                });
                (b.build_id, submitted_at)
            })
            .collect())
    }
}

#[async_trait]
impl ClusterApi for GrpcAdminApi {
    async fn cluster_status(&self) -> Result<ClusterCounts> {
        let resp = self
            .with_retry("ClusterStatus", self.rpc_timeout, |mut c| async move {
                c.cluster_status(()).await.map(|r| r.into_inner())
            })
            .await?;
        Ok(ClusterCounts {
            active_executors: resp.active_executors,
            queued_derivations: resp.queued_derivations,
            running_derivations: resp.running_derivations,
            substituting_derivations: resp.substituting_derivations,
        })
    }

    async fn spawn_intents(&self) -> Result<IceSnapshot> {
        let resp = self
            .with_retry("GetSpawnIntents", self.rpc_timeout, |mut c| {
                let req = rio_proto::types::GetSpawnIntentsRequest {
                    kind: None,
                    systems: vec![],
                    features: vec![],
                    filter_features: false,
                };
                async move { c.get_spawn_intents(req).await.map(|r| r.into_inner()) }
            })
            .await?;
        Ok(IceSnapshot {
            ice_masked_cells: resp.ice_masked_cells,
            dead_nodes: resp.dead_nodes,
        })
    }
}

#[cfg(test)]
pub(crate) mod test_support {
    use super::*;
    use std::sync::Mutex;

    /// In-memory StoreApi: paths present in the map are valid. Setting
    /// `error` makes every `query_valid` call fail with that message
    /// (collect's store-failure paths).
    #[derive(Default)]
    pub struct FakeStoreApi {
        pub valid: HashMap<String, (String, u64)>,
        pub calls: Mutex<usize>,
        /// When set, `query_valid` fails with this message.
        pub error: Mutex<Option<String>>,
    }

    impl FakeStoreApi {
        /// Make every subsequent `query_valid` call fail with `message`.
        pub fn fail_with(&self, message: &str) {
            *self.error.lock().unwrap() = Some(message.to_string());
        }
    }

    #[async_trait]
    impl StoreApi for FakeStoreApi {
        async fn query_valid(
            &self,
            paths: &[String],
        ) -> Result<HashMap<String, Option<(String, u64)>>> {
            *self.calls.lock().unwrap() += 1;
            if let Some(message) = self.error.lock().unwrap().clone() {
                anyhow::bail!("{message}");
            }
            Ok(paths
                .iter()
                .map(|p| (p.clone(), self.valid.get(p).cloned()))
                .collect())
        }
    }
}

#[cfg(test)]
mod tests {
    use std::sync::atomic::Ordering;

    use rio_test_support::grpc::{LogScript, spawn_mock_admin};

    use super::*;

    /// The rolling trim caps the log-tail buffer at `max_bytes` after every
    /// appended line, so streaming a huge log never buffers more than the
    /// requested tail.
    #[test]
    fn log_tail_buffer_is_trimmed_rolling_to_max_bytes() {
        let mut buf = Vec::new();
        for i in 0..1000 {
            push_log_tail_line(&mut buf, format!("line {i:04}").as_bytes(), 100);
            assert!(buf.len() <= 100, "buffer exceeded the cap after line {i}");
        }
        let text = String::from_utf8(buf).unwrap();
        assert!(text.ends_with("line 0999\n"), "{text}");
        assert!(!text.contains("line 0000"), "{text}");
        // A single line longer than the cap keeps only its last max_bytes.
        let mut big = Vec::new();
        push_log_tail_line(&mut big, &[b'x'; 300], 100);
        assert_eq!(big.len(), 100);
        // A zero cap keeps nothing.
        let mut none = Vec::new();
        push_log_tail_line(&mut none, b"anything", 0);
        assert!(none.is_empty());
    }

    /// Connect failures are retried with the same attempt budget as
    /// UNAVAILABLE, and the exhausted-retry error names the scheduler
    /// address and the attempt count. Port 1 on loopback has nothing
    /// listening, so the eager connect fails immediately without touching
    /// any real network.
    #[tokio::test(start_paused = true)]
    async fn with_retry_exhausts_connect_failures_naming_addr_and_attempts() {
        let api = api_at("127.0.0.1:1", 2);
        let err = api.list_poisoned().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("127.0.0.1:1"), "{msg}");
        assert!(msg.contains("2 attempts"), "{msg}");
    }

    /// Test facade with production deadlines but a configurable address
    /// and attempt budget. Tests that exercise the deadline itself
    /// override `rpc_timeout`/`stream_timeout` with sub-second values —
    /// the suite runs in real time (see the note on the first mock-server
    /// test), so production budgets would mean minutes-long tests.
    fn api_at(addr: impl Into<String>, max_attempts: u32) -> GrpcAdminApi {
        GrpcAdminApi {
            addr: addr.into(),
            signer: None,
            max_attempts,
            rpc_timeout: ADMIN_RPC_TIMEOUT,
            stream_timeout: ADMIN_STREAM_TIMEOUT,
        }
    }

    /// The scheduler delivers not-leader as the FIRST in-stream item on an
    /// otherwise-successful GetDerivationLogs RPC (grpc-web compatibility:
    /// leader-gated stream handlers return `Ok(err_stream(status))`, never
    /// `Err(Status)`). The facade's not-leader retry MUST treat that
    /// pre-data in-stream UNAVAILABLE exactly like an RPC-level
    /// UNAVAILABLE: reconnect (re-resolve) and retry. Without it, every
    /// log fetch that lands on a standby or a failover window errors
    /// immediately and collect silently loses its log evidence.
    ///
    /// Real time, not start_paused: live TCP + paused clock makes tokio
    /// auto-advance fire connect/RPC deadlines while the kernel does real
    /// work (see rio-proto's connect tests).
    #[tokio::test]
    async fn log_tail_retries_in_stream_unavailable_before_data() {
        let (admin, addr, server) = spawn_mock_admin().await.unwrap();
        admin.push_log_script(LogScript::ErrFirst(tonic::Status::unavailable(
            "not leader (standby replica)",
        )));
        admin.push_log_script(LogScript::Complete {
            lines: vec![b"error: builder for x failed".to_vec()],
        });
        let api = api_at(addr.to_string(), 5);
        let tail = api
            .log_tail(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
                None,
                4096,
            )
            .await
            .unwrap();
        assert_eq!(tail, b"error: builder for x failed\n");
        assert_eq!(
            admin.log_calls.load(Ordering::SeqCst),
            2,
            "expected one not-leader attempt plus one successful retry"
        );
        server.abort();
    }

    /// A log stream that breaks AFTER chunks already flowed is NOT
    /// retried, even when the status is UNAVAILABLE: a fresh attempt
    /// restarts from line 0 (and may resolve to a different replica), so
    /// the rolling tail would splice two reads of the log together.
    /// Callers use the tail as failure evidence — a hard error beats
    /// silently incomplete output.
    #[tokio::test]
    async fn log_tail_does_not_retry_stream_break_after_data() {
        let (admin, addr, server) = spawn_mock_admin().await.unwrap();
        admin.push_log_script(LogScript::BreakAfter {
            lines: vec![b"phase: building".to_vec()],
            status: tonic::Status::unavailable("leader failover mid-stream"),
        });
        let api = api_at(addr.to_string(), 5);
        let err = api
            .log_tail(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
                None,
                4096,
            )
            .await
            .unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("leader failover mid-stream"), "{msg}");
        assert_eq!(
            admin.log_calls.load(Ordering::SeqCst),
            1,
            "a mid-transfer stream break must not be retried"
        );
        server.abort();
    }

    /// A server that accepts the RPC and then never responds (ungraceful
    /// peer death: no FIN, no RST, nothing on the wire) trips the
    /// per-attempt deadline owned by `with_retry`, and the elapsed
    /// deadline is retried like UNAVAILABLE — against a FRESH connection,
    /// which is what rescues the call after a leader failover. Without
    /// the deadline this await would hang until kernel TCP keepalive.
    #[tokio::test]
    async fn hung_log_rpc_trips_deadline_and_retries() {
        let (admin, addr, server) = spawn_mock_admin().await.unwrap();
        admin.push_log_script(LogScript::Hang);
        admin.push_log_script(LogScript::Complete {
            lines: vec![b"recovered after failover".to_vec()],
        });
        let mut api = api_at(addr.to_string(), 5);
        // 1s, not something tighter: the budget also binds the SECOND
        // (successful) attempt, and a loopback RPC under full nextest
        // parallelism on a loaded builder can take hundreds of ms. The
        // hung attempt is the only one that burns the whole budget, so
        // the test stays ~2s wall.
        api.stream_timeout = Duration::from_secs(1);
        let tail = api
            .log_tail(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv",
                None,
                4096,
            )
            .await
            .unwrap();
        assert_eq!(tail, b"recovered after failover\n");
        assert_eq!(
            admin.log_calls.load(Ordering::SeqCst),
            2,
            "expected one hung attempt (deadline) plus one successful retry"
        );
        server.abort();
    }

    /// A hung UNARY RPC — the exact shape that used to freeze the poller
    /// loop, whose first await each tick is a unary admin read — trips
    /// the deadline wired through `rpc_timeout` and is retried against a
    /// fresh connection. The server parks the first call forever, so
    /// only the client-side deadline can unblock the attempt; the second
    /// call answers and the wrapper returns normally.
    #[tokio::test]
    async fn hung_unary_rpc_trips_deadline_and_retries() {
        let (admin, addr, server) = spawn_mock_admin().await.unwrap();
        admin.poisoned_hangs.store(1, Ordering::SeqCst);
        let mut api = api_at(addr.to_string(), 5);
        // Same headroom reasoning as the hung-stream test: the budget
        // also binds the successful second attempt.
        api.rpc_timeout = Duration::from_secs(1);
        let poisoned = api.list_poisoned().await.unwrap();
        assert!(poisoned.is_empty(), "{poisoned:?}");
        assert_eq!(
            admin.poisoned_calls.load(Ordering::SeqCst),
            2,
            "expected one hung attempt (deadline) plus one successful retry"
        );
        server.abort();
    }

    /// When every attempt hangs (a leader that never comes back), the
    /// elapsed deadlines consume the retry budget and the final error
    /// names the budget and the attempt count. The server parks ALL
    /// calls, so a tight per-attempt budget cannot race a reply.
    #[tokio::test]
    async fn hung_unary_rpc_exhausts_attempts_with_descriptive_error() {
        let (admin, addr, server) = spawn_mock_admin().await.unwrap();
        admin.poisoned_hangs.store(usize::MAX, Ordering::SeqCst);
        let mut api = api_at(addr.to_string(), 2);
        api.rpc_timeout = Duration::from_millis(250);
        let err = api.list_poisoned().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("no response within"), "{msg}");
        assert!(msg.contains("2 attempts"), "{msg}");
        assert_eq!(
            admin.poisoned_calls.load(Ordering::SeqCst),
            2,
            "both attempts must have reached the server before exhaustion"
        );
        server.abort();
    }

    /// Offline classification contract of `collect_log_tail`: an error
    /// before any chunk is the RPC-level outcome (`Rpc` — retried by
    /// `with_retry` when UNAVAILABLE, exactly how the scheduler delivers
    /// not-leader in-stream); an error after data is `Fatal` (never
    /// retried); `is_complete` stops consumption; EOF without
    /// `is_complete` returns what arrived.
    #[tokio::test]
    async fn collect_log_tail_classifies_stream_errors() {
        let chunk = |lines: Vec<&[u8]>, is_complete: bool| rio_proto::types::DerivationLogChunk {
            derivation_path: "/nix/store/x.drv".into(),
            exec_id: String::new(),
            lines: lines.into_iter().map(<[u8]>::to_vec).collect(),
            first_line_number: 0,
            is_complete,
        };

        // Error before any chunk → Rpc (the in-stream not-leader shape).
        let s = futures_util::stream::iter(vec![Err(tonic::Status::unavailable("not leader"))]);
        match collect_log_tail(s, "/nix/store/x.drv", 100).await {
            Err(CallError::Rpc(status)) => {
                assert_eq!(status.code(), tonic::Code::Unavailable);
            }
            Err(CallError::Fatal(e)) => panic!("pre-data error must be Rpc, got Fatal: {e}"),
            Ok(buf) => panic!("pre-data error must fail, got {buf:?}"),
        }

        // Error after a chunk → Fatal, even though the code is UNAVAILABLE.
        let s = futures_util::stream::iter(vec![
            Ok(chunk(vec![b"line 1"], false)),
            Err(tonic::Status::unavailable("died mid-stream")),
        ]);
        match collect_log_tail(s, "/nix/store/x.drv", 100).await {
            Err(CallError::Fatal(e)) => {
                let msg = format!("{e:#}");
                assert!(msg.contains("died mid-stream"), "{msg}");
            }
            Err(CallError::Rpc(s)) => panic!("post-data error must be Fatal, got Rpc: {s}"),
            Ok(buf) => panic!("post-data error must fail, got {buf:?}"),
        }

        // is_complete=true ends consumption even with items still queued.
        let s = futures_util::stream::iter(vec![
            Ok(chunk(vec![b"done"], true)),
            Err(tonic::Status::internal("must never be polled")),
        ]);
        let buf = collect_log_tail(s, "/nix/store/x.drv", 100).await.unwrap();
        assert_eq!(buf, b"done\n");

        // EOF without is_complete returns what arrived (matches the
        // pre-existing drain semantics for truncated server streams).
        let s = futures_util::stream::iter(vec![Ok(chunk(vec![b"partial"], false))]);
        let buf = collect_log_tail(s, "/nix/store/x.drv", 100).await.unwrap();
        assert_eq!(buf, b"partial\n");
    }
}
