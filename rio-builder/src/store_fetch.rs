//! Store-fetch primitives shared by the executor and the castore-FUSE.
//!
//! Anything that talks gRPC to rio-store and isn't FUSE-typed lives
//! here: the [`StoreClients`] bundle every fetch path threads around,
//! and the size-aware fetch-timeout policy the castore opener and
//! streaming fill use.

use std::time::Duration;

use tonic::transport::Channel;

use rio_proto::StoreServiceClient;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::store::directory_service_client::DirectoryServiceClient;

/// gRPC client bundle for store fetches.
///
/// Wraps `StoreServiceClient` and `ChunkServiceClient` over a single
/// (typically p2c-balanced) `tonic::transport::Channel`. Clone is
/// cheap — the channel is `Arc`-internal.
///
/// Kept as a struct (not a bare type alias) so future client additions
/// thread through every call site as one parameter. `chunk` is the
/// P0568 addition: the castore-FUSE fill task calls
/// `chunk.get_chunks(stream)` to pipeline local-cache misses to
/// rio-store's batched fan-out instead of paying a per-chunk
/// `GetChunk` RTT. `directory` is the P0559 addition: the mount-time
/// DAG prefetch (`GetDirectory`) and the `open()` whole-file fetch
/// (`ReadBlob`).
#[derive(Clone)]
pub struct StoreClients {
    pub store: StoreServiceClient<Channel>,
    pub chunk: ChunkServiceClient<Channel>,
    pub directory: DirectoryServiceClient<Channel>,
}

impl StoreClients {
    /// Wrap the store, chunk, and directory clients over a single
    /// `Channel` with the standard max-message-size headroom (matches
    /// `connect_single`'s convention). One channel: all three RPC
    /// services run on the same rio-store endpoint and share the p2c
    /// balancer.
    pub fn from_channel(ch: Channel) -> Self {
        let max = rio_common::grpc::max_message_size();
        Self {
            store: StoreServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            chunk: ChunkServiceClient::new(ch.clone())
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
            directory: DirectoryServiceClient::new(ch)
                .max_decoding_message_size(max)
                .max_encoding_message_size(max),
        }
    }
}

/// Minimum expected store→builder throughput for JIT fetch-timeout
/// sizing. I-178: 15 MiB/s is a conservative floor — half the ~30 MB/s
/// observed in cluster with the pre-ADR-022 FUSE fetch metrics (the
/// castore equivalents are `rio_builder_castore_fuse_fetch_bytes_total`
/// against the remote-hit `rio_builder_castore_fuse_open_seconds` sum).
/// A 1.9 GB NAR at this floor needs ≈127 s; the previous flat 60 s
/// timeout aborted the fetch mid-stream → daemon ENOENT →
/// PermanentFailure poison.
///
/// Tune DOWN if `rio_builder_input_materialization_failures_total` is
/// sustained nonzero (means real throughput is below this floor —
/// cross-AZ builders, S3 throttle).
pub const JIT_MIN_THROUGHPUT_BPS: u64 = 15 * 1024 * 1024;

/// Per-path JIT fetch timeout: `max(base, nar_size / MIN_THROUGHPUT)`.
///
/// `base` is `fuse_fetch_timeout` (60 s) so small paths are unchanged
/// from pre-I-178 behavior. Large paths get a size-proportional budget
/// — the I-178 1.9 GB input gets ≈127 s instead of the flat 60 s that
/// aborted it mid-stream.
///
/// Under JIT (I-043 redesign) the FUSE callback IS the fetch site —
/// the daemon's `lstat` blocks in `request_wait_answer` for this
/// duration on a cold input. The size-aware budget is therefore
/// load-bearing for correctness (a too-short timeout → EIO →
/// `InfrastructureFailure`), not just an optimization.
pub fn jit_fetch_timeout(base: Duration, nar_size: u64) -> Duration {
    base.max(Duration::from_secs(
        nar_size.div_ceil(JIT_MIN_THROUGHPUT_BPS),
    ))
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn jit_fetch_timeout_floors_at_base() {
        let base = Duration::from_secs(60);
        // Small path: timeout = base.
        assert_eq!(jit_fetch_timeout(base, 1024), base);
        // 1.9 GB at 15 MiB/s ≈ 127 s > base.
        let big = 1_900_000_000u64;
        let t = jit_fetch_timeout(base, big);
        assert!(t > base, "big NAR must extend the timeout, got {t:?}");
        assert_eq!(t.as_secs(), big.div_ceil(JIT_MIN_THROUGHPUT_BPS));
    }
}
