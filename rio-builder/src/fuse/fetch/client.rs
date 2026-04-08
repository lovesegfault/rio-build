//! gRPC client bundle and transport selection for FUSE fetches.
//!
//! `StoreClients` bundles `StoreServiceClient` + `ChunkServiceClient` over
//! the same balanced channel; `FetchTransport` is the process-global switch
//! between `GetPath` and `GetChunk` (chunk fan-out, dataplane2).

use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::store::chunk_service_client::ChunkServiceClient;

/// Bundles `StoreServiceClient` + `ChunkServiceClient` over the SAME
/// (typically p2c-balanced) `tonic::transport::Channel`. Clone is cheap
/// — both wrap the channel, which is `Arc`-internal.
///
/// dataplane2: the chunk-fanout fetch path needs `ChunkServiceClient`
/// alongside the existing `StoreServiceClient`. Bundling them keeps the
/// invariant that both share one balanced channel (so `GetChunk` p2c-
/// fans across the same SERVING replicas as `GetPath`) and threads
/// through every `prefetch_path_blocking` call site as one parameter.
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    pub chunk: ChunkServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap both clients over a single `Channel`. Sets the same
    /// max-message-size on both (chunks are ≤1 MiB but the headroom
    /// matches `connect_store`'s convention).
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            chunk: ChunkServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}

/// How `fetch_extract_insert` pulls NAR bytes from rio-store.
///
/// `GetPath` (default): single server-stream RPC; the store reassembles
/// chunks. Pinned to one replica per fetch; throughput bounded by the
/// store's `.buffered(8)` × chunk-size / S3-RTT.
///
/// `GetChunk` (opt-in): builder drives reassembly via parallel
/// `ChunkService.GetChunk` over the manifest hint's chunk list. Each
/// RPC is independent so the p2c balancer fans across all SERVING
/// replicas. See `r[builder.fuse.fetch-chunk-fanout]` and
/// `.stress-test/PLAN-DATAPLANE2.md`.
#[derive(Debug, Clone, Copy, PartialEq, Eq, Default, serde::Serialize, serde::Deserialize)]
#[serde(rename_all = "lowercase")]
pub enum FetchTransport {
    #[default]
    GetPath,
    GetChunk,
}

impl FetchTransport {
    /// Process-global transport, set once from config at startup via
    /// [`Self::init`]. FUSE callbacks read this — they have no `Config`
    /// handle (run on `fuser`'s thread pool with only `Arc<Cache>`).
    /// Default `getpath` — chunk fan-out is opt-in until A/B'd live.
    pub fn current() -> Self {
        *CELL.get_or_init(Self::default)
    }

    /// Set the process-global transport. Call once from `main()` after
    /// config load, before mounting FUSE. Subsequent calls are no-ops.
    pub fn init(t: Self) {
        let _ = CELL.set(t);
        if t == Self::GetChunk {
            tracing::info!("FUSE fetch transport: getchunk (parallel chunk fan-out)");
        }
    }
}

static CELL: std::sync::OnceLock<FetchTransport> = std::sync::OnceLock::new();
