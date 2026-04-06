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

use super::*;

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

impl StoreServiceImpl {
    pub(super) async fn get_path_impl(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Step 1: narinfo + manifest.
        let info = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: query_path_info", e))?
            .ok_or_else(|| Status::not_found(format!("path not found: {}", req.store_path)))?;

        // `None` here is defense-in-depth for a race where query_path_info
        // found the narinfo but get_manifest doesn't. Both filter on
        // manifests.status='complete', so in practice they agree.
        let manifest = metadata::get_manifest(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: get_manifest", e))?
            .ok_or_else(|| {
                Status::not_found(format!("manifest not found for: {}", req.store_path))
            })?;

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
                req.store_path, manifest_size, info.nar_size
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
        // r[impl obs.metric.transfer-volume]
        metrics::counter!("rio_store_get_path_bytes_total").increment(expected_size);
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
