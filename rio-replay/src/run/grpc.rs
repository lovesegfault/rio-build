//! Thin trait facades over the rio gRPC surfaces the engine reads, so every
//! stage is unit-testable with in-memory fakes. The tonic-backed impls are
//! deliberately dumb adapters (no logic beyond chunking, rolling log-tail
//! truncation, and a reconnect retry on connect failures / UNAVAILABLE) —
//! the RPC plumbing is exercised against a live cluster during the first
//! smoke campaign, while the retry and truncation logic has offline unit
//! tests below.

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
pub const ENGINE_CALLER: &str = "rio-parity";

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

/// Tonic-backed AdminService facade with reconnect-on-not-leader retry:
/// the leader-gated reads (ClusterStatus, GetSpawnIntents, GetBuildGraph)
/// answer UNAVAILABLE from a standby replica, so every call runs against a
/// fresh connection and retries UNAVAILABLE with backoff.
pub struct GrpcAdminApi {
    addr: String,
    signer: Option<std::sync::Arc<rio_auth::hmac::HmacSigner>>,
    max_attempts: u32,
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
        })
    }

    async fn client(&self) -> Result<AdminClient> {
        let channel = rio_proto::client::connect_channel(&self.addr)
            .await
            .with_context(|| format!("connect scheduler at {}", self.addr))?;
        Ok(rio_proto::AdminServiceClient::with_interceptor(
            channel,
            rio_auth::hmac::ServiceTokenInterceptor::new(self.signer.clone(), ENGINE_CALLER),
        )
        .max_decoding_message_size(rio_common::grpc::max_message_size())
        .max_encoding_message_size(rio_common::grpc::max_message_size()))
    }

    /// Run `op` against a fresh client, retrying connect failures
    /// (scheduler restart, DNS blip) and UNAVAILABLE responses (not-leader,
    /// reconnect window) with the same backoff and attempt budget. Other
    /// statuses are returned to the caller; exhausted retries name the
    /// scheduler address and the attempt count.
    async fn with_retry<T, F, Fut>(&self, what: &str, mut op: F) -> Result<T>
    where
        F: FnMut(AdminClient) -> Fut,
        Fut: std::future::Future<Output = std::result::Result<T, tonic::Status>>,
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
            match op(client).await {
                Ok(v) => return Ok(v),
                Err(s)
                    if s.code() == tonic::Code::Unavailable && attempt + 1 < self.max_attempts =>
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
                Err(s) if s.code() == tonic::Code::Unavailable => {
                    return Err(anyhow::anyhow!(
                        "{what} against scheduler at {}: still UNAVAILABLE after {} attempts: {}",
                        self.addr,
                        self.max_attempts,
                        s.message()
                    ));
                }
                Err(s) => {
                    return Err(anyhow::anyhow!(
                        "{what} against scheduler at {}: {} ({:?})",
                        self.addr,
                        s.message(),
                        s.code()
                    ));
                }
            }
        }
    }
}

#[async_trait]
impl AdminApi for GrpcAdminApi {
    async fn get_build_graph(&self, build_id: &str) -> Result<GraphSnapshot> {
        let resp = self
            .with_retry("GetBuildGraph", |mut c| {
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
            .with_retry("ListPoisoned", |mut c| async move {
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
        let mut stream = self
            .with_retry("GetDerivationLogs", |mut c| {
                let req = rio_proto::types::GetDerivationLogsRequest {
                    derivation_path: drv.clone(),
                    exec_id: exec.clone(),
                    since_line: 0,
                };
                async move { c.get_derivation_logs(req).await.map(|r| r.into_inner()) }
            })
            .await?;
        let mut buf: Vec<u8> = Vec::new();
        while let Some(chunk) = stream
            .message()
            .await
            .map_err(|s| anyhow::anyhow!("GetDerivationLogs stream for {drv_path}: {s}"))?
        {
            // Trim rolling, per line, so the buffer never grows past the
            // requested tail even when the full log is hundreds of MB.
            for line in chunk.lines {
                push_log_tail_line(&mut buf, &line, max_bytes);
            }
            if chunk.is_complete {
                break;
            }
        }
        Ok(buf)
    }

    async fn list_builds(&self, tenant: &str, limit: u32) -> Result<Vec<(String, Option<String>)>> {
        let tenant = tenant.to_string();
        let resp = self
            .with_retry("ListBuilds", |mut c| {
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
            .with_retry("ClusterStatus", |mut c| async move {
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
            .with_retry("GetSpawnIntents", |mut c| {
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
        let api = GrpcAdminApi {
            addr: "127.0.0.1:1".to_string(),
            signer: None,
            max_attempts: 2,
        };
        let err = api.list_poisoned().await.unwrap_err();
        let msg = format!("{err:#}");
        assert!(msg.contains("127.0.0.1:1"), "{msg}");
        assert!(msg.contains("2 attempts"), "{msg}");
    }
}
