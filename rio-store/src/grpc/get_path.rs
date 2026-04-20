//! GetPath: stream a store path's NAR data.
//!
//! Flow:
//! 1. Look up narinfo + manifest from PostgreSQL
//! 2. First response: PathInfo metadata
// r[impl store.nar.reassembly]
// r[impl store.integrity.verify-on-get]
//! 3. Stream NAR bytes — branch on inline vs chunked
//! 4. Verify whole-NAR SHA-256 (belt-and-suspenders over per-chunk BLAKE3)
//!
//! The chunked path streams chunk-by-chunk without materializing the
//! full NAR in memory — that's the whole point. K=8 parallel prefetch
//! via `buffered()` (NOT `buffer_unordered` — chunk order matters for
//! correct NAR reconstruction).

use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{error, warn};

use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::types::{
    GetPathRequest, GetPathResponse, ManifestHint, PathInfo, get_path_response,
};

use rio_common::grpc::StatusExt;

use crate::metadata::{self, ManifestKind};

use super::{StoreServiceImpl, metadata_status, validate_store_path};

pub(super) type GetPathStream = ReceiverStream<Result<GetPathResponse, Status>>;

/// Stream a Bytes value to the GetPath channel in NAR_CHUNK_SIZE pieces.
///
/// Returns `false` if the client disconnected (send failed) — caller
/// should stop streaming. This is the one place the "slice into wire-
/// sized pieces + send" loop lives; both the inline and chunked paths
/// call it.
///
/// `.to_vec()` is one copy into the proto bytes field (protobuf needs
/// owned `Vec<u8>`, no way around it). The input `Bytes` stays valid
/// (Arc-refcounted), so for the chunked path this is the only copy
/// between S3 and the wire.
async fn stream_bytes(
    tx: &tokio::sync::mpsc::Sender<Result<GetPathResponse, Status>>,
    bytes: &Bytes,
) -> bool {
    for piece in bytes.chunks(NAR_CHUNK_SIZE) {
        let msg = GetPathResponse {
            msg: Some(get_path_response::Msg::NarChunk(piece.to_vec())),
        };
        if tx.send(Ok(msg)).await.is_err() {
            return false;
        }
    }
    true
}

/// Convert a client-supplied [`ManifestHint`] into the
/// `(ValidatedPathInfo, ManifestKind)` pair `stream_path` consumes.
///
/// Returns `Ok(None)` if `hint.info` is unset (caller falls through to
/// PG). Returns `Err(InvalidArgument)` if the hint is structurally
/// malformed (wrong path, bad hash length) — that's a client bug, not
/// a fall-through case.
fn hint_into_manifest(
    store_path: &str,
    hint: ManifestHint,
) -> Result<Option<(rio_proto::validated::ValidatedPathInfo, ManifestKind)>, Status> {
    let Some(raw_info) = hint.info else {
        return Ok(None);
    };
    // Hint must be FOR the requested path. A mismatched hint is a
    // client bug (hint cache keyed wrong), not a fall-through.
    if raw_info.store_path != store_path {
        return Err(Status::invalid_argument(format!(
            "manifest_hint.info.store_path {:?} != requested {:?}",
            raw_info.store_path, store_path
        )));
    }
    let info = rio_proto::validated::ValidatedPathInfo::try_from(raw_info)
        .status_invalid("manifest_hint.info malformed")?;

    // r[impl store.get.manifest-hint+2]
    // Bound the client-supplied hint at the trust boundary. The PG
    // path's `Manifest::deserialize` enforces MAX_CHUNKS; the hint
    // path bypasses that. nar_size on the hint path is also client-
    // controlled (the PG path's is server-written) — bound it so a
    // hostile worker can't allocate 200k Vec entries or claim a 28 PB
    // NAR. Same INVALID_ARGUMENT shape as the per-chunk hash-length
    // check below.
    if info.nar_size > rio_common::limits::MAX_NAR_SIZE {
        return Err(Status::invalid_argument(format!(
            "manifest_hint.info.nar_size {} exceeds MAX_NAR_SIZE {}",
            info.nar_size,
            rio_common::limits::MAX_NAR_SIZE
        )));
    }
    if hint.chunks.len() > crate::manifest::MAX_CHUNKS {
        return Err(Status::invalid_argument(format!(
            "manifest_hint has {} chunks, exceeds MAX_CHUNKS {}",
            hint.chunks.len(),
            crate::manifest::MAX_CHUNKS
        )));
    }

    let manifest = if hint.chunks.is_empty() {
        ManifestKind::Inline(Bytes::from(hint.inline_blob))
    } else {
        let entries: Vec<([u8; 32], u32)> = hint
            .chunks
            .into_iter()
            .map(|c| {
                let hash: [u8; 32] = c.hash.try_into().map_err(|v: Vec<u8>| {
                    Status::invalid_argument(format!(
                        "manifest_hint chunk hash must be 32 bytes, got {}",
                        v.len()
                    ))
                })?;
                Ok::<_, Status>((hash, c.size))
            })
            .collect::<Result<_, _>>()?;
        ManifestKind::Chunked(entries)
    };
    Ok(Some((info, manifest)))
}

impl StoreServiceImpl {
    pub(super) async fn get_path_impl(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = self.request_tenant_id(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // r[impl store.get.manifest-hint+2]
        // I-110c: client-supplied (PathInfo, manifest) — skip both PG
        // lookups. The whole-NAR SHA-256 verify (step 4) checks the
        // reassembled bytes against `hint.info.nar_hash`, so a
        // stale/forged hint surfaces as DATA_LOSS exactly like a
        // corrupt manifest_data row would. Chunks are content-
        // addressed (BLAKE3-verified in `get_verified`), so a hint
        // can't read chunks the client doesn't already know the hash
        // of. Falls through to PG on `info=None` or a malformed hint.
        //
        // NOT sig-visibility-gated: BLAKE3 chunk hashes are capability
        // tokens — possessing them means the caller already had access.
        // The chokepoint that protects this is `BatchGetManifest`'s
        // `reject_end_user_tenant`: end-user tenants can't obtain
        // chunk hashes for paths the gate would hide.
        if let Some(hint) = req.manifest_hint
            && let Some((info, manifest)) = hint_into_manifest(&req.store_path, hint)?
        {
            return self.stream_path(info, manifest).await;
        }

        // Step 1: narinfo + manifest.
        // r[impl store.substitute.upstream]
        // On miss: try upstream substitution before NotFound. The
        // substituter ingests via the same write-ahead path as PutPath,
        // so the `get_manifest` below picks up the freshly-ingested
        // NAR with no extra plumbing.
        let local = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: query_path_info", e))?;
        let info = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility+2]
                // Same gate as QueryPathInfo: hide-as-NotFound on
                // failure, fall through to try_substitute_on_miss
                // (the requesting tenant's upstreams may also have
                // it, which would append a trusted sig).
                if self.sig_visibility_gate(tenant_id, &i).await? {
                    i
                } else if let Some(sub) = self
                    .try_substitute_on_miss(tenant_id, &req.store_path)
                    .await?
                {
                    sub
                } else {
                    return Err(Status::not_found(format!(
                        "path not found: {}",
                        req.store_path
                    )));
                }
            }
            None => self
                .try_substitute_on_miss(tenant_id, &req.store_path)
                .await?
                .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?,
        };

        // `None` here is defense-in-depth for a race where query_path_info
        // found the narinfo but get_manifest doesn't. Both filter on
        // manifests.status='complete', so in practice they agree.
        let manifest = metadata::get_manifest(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: get_manifest", e))?
            .ok_or_else(|| {
                Status::not_found(format!("manifest not found for: {}", req.store_path))
            })?;

        self.stream_path(info, manifest).await
    }

    /// Steps 2-4 of GetPath: pre-flight size/backend check, spawn the
    /// streaming task, hash-verify on the way out. Split out so the
    /// I-110c manifest-hint fast path and the PG-lookup path share it.
    async fn stream_path(
        &self,
        info: rio_proto::validated::ValidatedPathInfo,
        manifest: ManifestKind,
    ) -> Result<Response<GetPathStream>, Status> {
        // r[impl store.get.size-sanity-check]
        // Pre-flight: manifest's summed size must match narinfo.nar_size.
        // Drift means PutPath wrote inconsistent state (bug, or manual DB
        // surgery). Fail fast with DATA_LOSS — better than streaming a
        // NAR the client will reject on its own size check, or worse,
        // silently accept. The post-stream check at step 4 also catches
        // this, but only after wasting the client's bandwidth.
        let manifest_size = manifest.total_size();
        if manifest_size != info.nar_size {
            return Err(Status::data_loss(format!(
                "manifest/narinfo size mismatch for {}: manifest sums to {} bytes, narinfo says {} bytes",
                info.store_path, manifest_size, info.nar_size
            )));
        }

        // Pre-flight: chunked manifest but no cache configured = we can't
        // serve this path. Inline-only stores (tests, or a misconfigured
        // deployment) hitting this means a PREVIOUS store instance wrote
        // chunked data and this one can't read it. Fail clearly rather
        // than the spawned task erroring with no context.
        if matches!(manifest, ManifestKind::Chunked(_)) && self.chunk_cache.is_none() {
            return Err(Status::failed_precondition(
                "path is stored chunked but this store instance has no chunk backend configured",
            ));
        }

        let expected_hash = info.nar_hash;
        let expected_size = info.nar_size;
        let start = std::time::Instant::now();
        // Clone for the spawned task. Arc-clone is cheap; the cache
        // itself (moka + DashMap) is shared.
        let cache = self.chunk_cache.clone();

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let info_raw: PathInfo = info.into();

        rio_common::task::spawn_monitored("get-path-stream", async move {
            // Bound the entire streaming task. A slow client otherwise
            // keeps this alive forever.
            let stream_fut = async {
                // Step 2: First message is PathInfo.
                if tx
                    .send(Ok(GetPathResponse {
                        msg: Some(get_path_response::Msg::Info(info_raw)),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }

                // Step 3+4: stream + verify. Both branches feed the hasher
                // incrementally and check at the end. The chunked path
                // streams chunk-by-chunk; the inline path is one blob.
                let mut hasher = Sha256::new();
                let mut total_bytes = 0u64;

                match manifest {
                    ManifestKind::Inline(bytes) => {
                        hasher.update(&bytes);
                        total_bytes = bytes.len() as u64;
                        if !stream_bytes(&tx, &bytes).await {
                            return; // client disconnected
                        }
                    }
                    ManifestKind::Chunked(entries) => {
                        // Pre-flight checked cache is Some.
                        let cache = cache.expect("pre-flight checked chunk_cache is Some");

                        // K=8 parallel prefetch. `buffered()` preserves
                        // order — chunk i arrives before chunk i+1 even if
                        // i+1's fetch finishes first. `buffer_unordered`
                        // would scramble the NAR.
                        //
                        // Each future is a cache.get_verified() call.
                        // BLAKE3 verify happens inside that; any corrupt
                        // chunk surfaces as ChunkError here.
                        use futures_util::stream::{self, StreamExt};
                        const PREFETCH_K: usize = 8;

                        let mut chunk_stream = stream::iter(entries)
                            .map(|(hash, _size)| {
                                let cache = Arc::clone(&cache);
                                async move { cache.get_verified(&hash).await }
                            })
                            .buffered(PREFETCH_K);

                        while let Some(result) = chunk_stream.next().await {
                            let chunk_bytes = match result {
                                Ok(b) => b,
                                Err(e) => {
                                    error!(error = %e, "GetPath: chunk fetch/verify failed");
                                    // DATA_LOSS: the manifest says this
                                    // chunk exists, but we can't get
                                    // good bytes for it. S3 lost it,
                                    // or it's corrupt.
                                    let _ = tx
                                        .send(Err(Status::data_loss(format!(
                                            "chunk reassembly failed: {e}"
                                        ))))
                                        .await;
                                    return;
                                }
                            };
                            hasher.update(&chunk_bytes);
                            total_bytes += chunk_bytes.len() as u64;
                            if !stream_bytes(&tx, &chunk_bytes).await {
                                return; // client disconnected
                            }
                        }
                    }
                }

                // Step 4: whole-NAR SHA-256 verify. The chunked path
                // already BLAKE3-verified each chunk, so this is belt-
                // and-suspenders: catches (a) the manifest being WRONG
                // (right chunks, wrong order / missing one), (b) a bug
                // in our reassembly, (c) narinfo.nar_hash being stale.
                //
                // For inline, this is the PRIMARY check (no per-piece
                // verify for inline blobs).
                let actual: [u8; 32] = hasher.finalize().into();
                if actual != expected_hash || total_bytes != expected_size {
                    error!(
                        expected_hash = %hex::encode(expected_hash),
                        actual_hash = %hex::encode(actual),
                        expected_size,
                        total_bytes,
                        "GetPath: whole-NAR integrity check failed"
                    );
                    metrics::counter!("rio_store_integrity_failures_total").increment(1);
                    let _ = tx
                        .send(Err(Status::data_loss(
                            "whole-NAR integrity check failed (SHA-256 or size mismatch)",
                        )))
                        .await;
                } else {
                    // r[impl obs.metric.transfer-volume]
                    // Incremented post-stream (not pre-stream) so a
                    // bogus-hash hint or DATA_LOSS mid-stream doesn't
                    // inflate the counter by claimed nar_size before
                    // any byte was actually transferred.
                    metrics::counter!("rio_store_get_path_bytes_total").increment(total_bytes);
                    metrics::counter!("rio_store_get_path_total").increment(1);
                    metrics::histogram!("rio_store_get_path_duration_seconds")
                        .record(start.elapsed().as_secs_f64());
                }
            };

            if tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, stream_fut)
                .await
                .is_err()
            {
                warn!(
                    timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                    "GetPath streaming task timed out"
                );
                let _ = tx
                    .send(Err(Status::deadline_exceeded(
                        "GetPath streaming timed out",
                    )))
                    .await;
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_proto::types::ChunkRef;
    use rio_test_support::fixtures::{make_nar, make_path_info, test_store_path};

    fn hint_with_chunks(path: &str, n_chunks: usize, nar_size: u64) -> ManifestHint {
        let (nar, nar_hash) = make_nar(b"hint");
        let mut info: PathInfo = make_path_info(path, &nar, nar_hash).into();
        info.nar_size = nar_size;
        ManifestHint {
            info: Some(info),
            chunks: vec![
                ChunkRef {
                    hash: vec![0u8; 32],
                    size: 1,
                };
                n_chunks
            ],
            inline_blob: vec![],
        }
    }

    // r[verify store.get.manifest-hint+2]
    /// Client-supplied hints are bounded by MAX_CHUNKS — same bound the
    /// PG path's `Manifest::deserialize` enforces. Pre-fix: hint path
    /// bypassed it (200_001-entry Vec allocated unconditionally).
    #[test]
    fn hint_into_manifest_rejects_over_max_chunks() {
        let path = test_store_path("hint-chunks");
        let hint = hint_with_chunks(&path, crate::manifest::MAX_CHUNKS + 1, 100);
        let err = hint_into_manifest(&path, hint).expect_err("over MAX_CHUNKS → InvalidArgument");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("MAX_CHUNKS"),
            "msg: {}",
            err.message()
        );

        // Boundary: exactly MAX_CHUNKS is accepted (`>` not `>=`).
        let ok = hint_with_chunks(&path, crate::manifest::MAX_CHUNKS, 100);
        assert!(hint_into_manifest(&path, ok).is_ok());
    }

    // r[verify store.get.manifest-hint+2]
    /// nar_size on the hint path is client-controlled — bound it.
    #[test]
    fn hint_into_manifest_rejects_oversize_nar() {
        let path = test_store_path("hint-narsize");
        let hint = hint_with_chunks(&path, 1, rio_common::limits::MAX_NAR_SIZE + 1);
        let err = hint_into_manifest(&path, hint).expect_err("over MAX_NAR_SIZE → InvalidArgument");
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
        assert!(
            err.message().contains("MAX_NAR_SIZE"),
            "msg: {}",
            err.message()
        );
    }
}
