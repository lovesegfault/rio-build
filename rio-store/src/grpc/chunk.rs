//! ChunkService gRPC implementation.
//!
//! Chunking is server-side only: PutPath drives `cas::put_chunked`
//! (FastCDC + dedup via the `refcount==1` RETURNING clause), and
//! GetPath streams whole NARs back. The only chunk-level RPC is
//! `GetChunk`; it has no production caller today and is exercised
//! only by tests, as the chunk-level retrieval surface for future
//! out-of-process reassembly.
//!
//! GetChunk is unscoped: knowing a BLAKE3 hash already proves you have
//! (or had) the bytes.

use std::sync::Arc;

use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::instrument;

use rio_proto::ChunkService;
use rio_proto::types::{GetChunkRequest, GetChunkResponse};

use crate::cas::{self, ChunkCache};

/// ChunkService implementation.
///
/// Shares `chunk_cache` with `StoreServiceImpl` — one gRPC server
/// process, two services, same state. `Arc` lets main.rs construct
/// both from the same backing pieces.
pub struct ChunkServiceImpl {
    /// Cache for GetChunk. Same cache as GetPath uses — a chunk fetched
    /// by either RPC warms the other. `None` = ChunkService effectively
    /// disabled (all RPCs return FAILED_PRECONDITION); main.rs only
    /// constructs this when a chunk backend is configured.
    chunk_cache: Option<Arc<ChunkCache>>,
}

impl ChunkServiceImpl {
    pub fn new(chunk_cache: Option<Arc<ChunkCache>>) -> Self {
        Self { chunk_cache }
    }

    /// Shared guard: all ChunkService RPCs need a cache. Without a
    /// backend, there's nothing to do at the chunk level.
    fn require_cache(&self) -> Result<&Arc<ChunkCache>, Status> {
        self.chunk_cache.as_ref().ok_or_else(|| {
            Status::failed_precondition(
                "ChunkService requires a chunk backend; this store is inline-only",
            )
        })
    }
}

#[tonic::async_trait]
impl ChunkService for ChunkServiceImpl {
    type GetChunkStream = ReceiverStream<Result<GetChunkResponse, Status>>;

    /// GetChunk: fetch a single chunk by BLAKE3 hash.
    ///
    /// Goes through the same `ChunkCache` as GetPath — a chunk warmed
    /// by GetPath is served from moka here, and vice versa. BLAKE3-
    /// verified (ChunkCache does that). Streams the chunk in one
    /// GetChunkResponse message (chunks are ≤256 KiB, no need to
    /// multi-message — GetPath's NAR_CHUNK_SIZE slicing is for the
    /// whole-NAR stream, not per-chunk).
    #[instrument(skip(self, request), fields(rpc = "GetChunk"))]
    async fn get_chunk(
        &self,
        request: Request<GetChunkRequest>,
    ) -> Result<Response<Self::GetChunkStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let cache = self.require_cache()?;
        let digest = request.into_inner().digest;

        let hash: [u8; 32] = digest.as_slice().try_into().map_err(|_| {
            Status::invalid_argument(format!(
                "digest must be 32 bytes (BLAKE3), got {}",
                digest.len()
            ))
        })?;

        // Synchronous: await here (not in a spawned task). Chunks are
        // small (≤256 KiB) and the cache is fast (moka hit = instant,
        // miss = one S3 GET). The GetPath streaming-task pattern is
        // for large NARs where the stream outlives the handler call;
        // GetChunk's "stream" is one message.
        let bytes = cache
            .get_verified(&hash)
            .await
            .map_err(|e| get_chunk_status(&hash, &e))?;

        // Single-message "stream". Channel buffer of 1 is enough; 2 is
        // belt-and-suspenders (one for the data, one in case tonic
        // wants to peek before forwarding).
        let (tx, rx) = tokio::sync::mpsc::channel(2);
        // The send can't fail here (fresh channel, rx not dropped).
        // If it somehow does, the client gets an empty stream — same
        // as a disconnect, which they handle.
        let _ = tx
            .send(Ok(GetChunkResponse {
                data: bytes.to_vec(),
            }))
            .await;

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

/// Map a chunk read failure to the client-facing Status. The auth arm
/// serves the SINGLE-SOURCED static remediation and logs the full
/// chain server-side: AWS error chains carry endpoint URLs, request
/// IDs, and key-shaped identifiers that untrusted callers must not
/// receive (round-15 scrub rule, 9075dcd8b precedent). The other arms
/// interpolate only ChunkError Display values that are themselves
/// hash-and-class shaped (NotFound/Corrupt name the hash; Backend
/// carries the transient class) — the no-leak pin below feeds a
/// poisoned auth chain through this mapping and asserts absence.
fn get_chunk_status(hash: &[u8; 32], e: &cas::ChunkError) -> Status {
    use cas::ChunkError;
    match e {
        ChunkError::NotFound(_) => {
            Status::not_found(format!("chunk {} not found", hex::encode(hash)))
        }
        ChunkError::Corrupt { .. } => Status::data_loss(format!(
            "chunk {} failed BLAKE3 verification: {e}",
            hex::encode(hash)
        )),
        ChunkError::AuthFailed { .. } => {
            tracing::error!(hash = %hex::encode(hash), error = %e,
                   "GetChunk: chunk backend auth failed");
            crate::grpc::backend_auth_status()
        }
        // Transient backend failure (round-16 bug_027): retriable,
        // says nothing about the chunk's existence.
        ChunkError::Backend { .. } => Status::unavailable(format!(
            "chunk {} backend fetch failed; retry: {e}",
            hex::encode(hash)
        )),
    }
}

/// No-leak pin (round-15 scrub rule, 9075dcd8b precedent): the
/// client-facing auth Status carries the static remediation ONLY —
/// nothing from the backend's error chain (endpoint URLs, request
/// IDs, key-shaped identifiers).
#[cfg(test)]
mod scrub_tests {
    use super::*;

    #[test]
    fn auth_status_never_echoes_the_backend_chain() {
        let poisoned = cas::ChunkError::AuthFailed {
            hash: [7u8; 32],
            // AWS's published documentation EXAMPLE key
            // (docs.aws.amazon.com) — deliberately key-shaped so this
            // test proves real credentials cannot reach client-facing
            // messages; not a secret.
            message: "AccessDenied for AKIAIOSFODNN7EXAMPLE at \
                      https://internal-bucket.s3.amazonaws.com req-id 0xDEADBEEF"
                .into(),
        };
        let status = get_chunk_status(&[7u8; 32], &poisoned);
        let msg = status.message();
        for needle in [
            "AKIAIOSFODNN7EXAMPLE",
            "internal-bucket",
            "amazonaws",
            "0xDEADBEEF",
        ] {
            assert!(
                !msg.contains(needle),
                "backend chain leaked into the client status: {needle} in {msg:?}"
            );
        }
        assert_eq!(
            msg,
            crate::grpc::backend_auth_status().message(),
            "single source: GetChunk serves the same static text as every reader"
        );
        assert_eq!(status.code(), tonic::Code::FailedPrecondition);
    }
}
