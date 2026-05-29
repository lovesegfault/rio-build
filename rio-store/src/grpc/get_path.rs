//! GetPath: stream a store path's NAR data.
//!
//! Flow:
//! 1. Look up narinfo + manifest + castore DAG from PostgreSQL
//! 2. First response: PathInfo metadata
// r[impl store.nar.reassembly]
// r[impl store.integrity.verify-on-get]
//! 3. Stream NAR bytes: regenerate the framing from the Directory DAG
//!    ([`crate::castore_nar::nar_pieces`]) and splice each regular
//!    file's contents in from the blob stream (inline) or its chunk
//!    run (chunked)
//! 4. Verify whole-NAR SHA-256 (belt-and-suspenders over per-chunk BLAKE3)
//!
//! The NAR byte stream is not persisted anywhere (ADR-022 §6): the
//! store holds the Directory DAG plus the **blob stream** — regular-
//! file contents in canonical NAR walk order, either inline or as
//! per-file FastCDC chunks. The chunked path streams chunk-by-chunk
//! without materializing the full NAR in memory. K-parallel prefetch
//! (default 64, see [`DEFAULT_CHUNK_PREFETCH_K`]) via `buffered()`
//! (NOT `buffer_unordered` — chunk order matters for correct NAR
//! reconstruction, and the manifest's chunk order IS the walk order).
//!
//! [`DEFAULT_CHUNK_PREFETCH_K`]: super::DEFAULT_CHUNK_PREFETCH_K

use std::collections::HashMap;
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{error, warn};

use rio_proto::castore::{Directory, RootNode};
use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::types::{GetPathRequest, GetPathResponse, PathInfo, get_path_response};

use crate::castore_nar::{self, NarPiece};
use crate::metadata::{self, ManifestKind};

use super::sign;
use super::{StoreServiceImpl, drain_with_timeout, metadata_status, validate_store_path};

/// Walk-expansion cap for the framing regeneration, in tree nodes. The
/// DAG is bounded by `MAX_DIR_NODES` distinct bodies, but the walk
/// visits a shared subtree once per reference — this caps the expanded
/// tree so a pathological DAG cannot spin the stream task forever.
///
/// MUST be at least [`rio_nix::nar::MAX_NAR_ENTRIES`] (the node cap
/// every ingest entry point enforces), or a path the store accepted
/// and committed could never be served — the "complete ⇒ servable"
/// half of `r[store.ingest.tree-bounds]`. The 16× headroom is on the
/// node count only: the depth and index-byte caps `TreeWalk` applies
/// during this walk are the exact ingest-side constants
/// (`MAX_NAR_DEPTH`, `MAX_NAR_INDEX_BYTES`), with no slack — fine for
/// this greenfield deployment, where nothing was committed before the
/// ingest caps existed.
const MAX_WALK_NODES: usize = 1 << 24;

// r[impl store.ingest.tree-bounds+2]
const _: () = assert!(
    MAX_WALK_NODES >= rio_nix::nar::MAX_NAR_ENTRIES,
    "the GetPath walk cap must cover everything ingest accepts"
);

pub(super) type GetPathStream = ReceiverStream<Result<GetPathResponse, Status>>;

/// RAII guard for [`StoreServiceImpl::active_get_path_streams`] and the
/// `rio_store_get_path_active` gauge. Increment in `new()`, decrement
/// on `Drop` — moved into the spawned body-stream task so any exit
/// path (success, error send, timeout, panic, abort) decrements.
struct ActiveStreamGuard(Arc<AtomicUsize>);

impl ActiveStreamGuard {
    fn new(counter: Arc<AtomicUsize>) -> Self {
        counter.fetch_add(1, Ordering::Relaxed);
        // r[impl obs.metric.store]
        metrics::gauge!("rio_store_get_path_active").increment(1.0);
        Self(counter)
    }
}

impl Drop for ActiveStreamGuard {
    fn drop(&mut self) {
        self.0.fetch_sub(1, Ordering::Relaxed);
        metrics::gauge!("rio_store_get_path_active").decrement(1.0);
    }
}

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

/// Decode the castore DAG read alongside the manifest. A complete path
/// with no usable index is unservable — the NAR framing cannot be
/// reconstructed from blob-aligned content chunks — and because the
/// index was read in the same snapshot as the manifest
/// ([`metadata::get_manifest_with_dag`]), a missing DAG here is genuine
/// `DATA_LOSS`, never a concurrent-GC race (a collected path returns
/// `NOT_FOUND` from the combined read instead).
fn decode_dag_for(
    store_path: &rio_nix::store_path::StorePath,
    dag: Option<metadata::CastoreDagRows>,
) -> Result<(RootNode, HashMap<[u8; 32], Directory>), Status> {
    let Some((root, bodies)) = dag else {
        return Err(Status::data_loss(format!(
            "no castore index for {store_path}: the NAR framing cannot be regenerated"
        )));
    };
    castore_nar::decode_dag(&root, bodies)
        .map_err(|e| Status::data_loss(format!("corrupt castore index for {store_path}: {e}")))
}

impl StoreServiceImpl {
    pub(super) async fn get_path_impl(
        &self,
        request: Request<GetPathRequest>,
    ) -> Result<Response<GetPathStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant_id = self.request_tenant_id(&request)?;
        let req = request.into_inner();

        validate_store_path(&req.store_path)?;

        // Step 1: narinfo + manifest.
        // r[impl store.substitute.upstream]
        // On miss: try upstream substitution before NotFound. The
        // substituter ingests via the same write-ahead path as PutPath,
        // so the `get_manifest_with_dag` below picks up the
        // freshly-ingested NAR with no extra plumbing.
        let lookup_start = std::time::Instant::now();
        let local = metadata::query_path_info(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: query_path_info", e))?;
        let local_hit = local.is_some();
        let (info, vis) = match local {
            Some(i) => {
                // r[impl store.substitute.tenant-sig-visibility+2]
                // Same gate as QueryPathInfo: hide-as-NotFound on
                // failure, fall through to try_substitute_on_miss
                // (the requesting tenant's upstreams may also have
                // it, which would append a trusted sig).
                match self.sig_visibility_gate(tenant_id, &i).await? {
                    Some(vis) => (i, vis),
                    None => {
                        if let Some(sub) = self
                            .try_substitute_on_miss(tenant_id, &req.store_path)
                            .await?
                        {
                            (sub, sign::PathVisible::substituted_for_tenant())
                        } else {
                            return Err(Status::not_found(format!(
                                "path not found: {}",
                                req.store_path
                            )));
                        }
                    }
                }
            }
            None => {
                let sub = self
                    .try_substitute_on_miss(tenant_id, &req.store_path)
                    .await?
                    .ok_or_else(|| {
                        Status::not_found(format!("path not found: {}", req.store_path))
                    })?;
                (sub, sign::PathVisible::substituted_for_tenant())
            }
        };
        let lookup_elapsed = lookup_start.elapsed();
        if lookup_elapsed > std::time::Duration::from_secs(1) {
            warn!(
                store_path = %req.store_path,
                local_hit,
                elapsed = ?lookup_elapsed,
                "GetPath: slow narinfo lookup (>1s; substitute path if local_hit=false)"
            );
        }

        // Manifest + castore DAG in ONE statement (one MVCC snapshot).
        // `None` covers both defense-in-depth for a race where
        // query_path_info found the narinfo but the manifest read
        // doesn't (both filter on manifests.status='complete'), and a
        // GC sweep committing between the two — either way the answer
        // is NOT_FOUND. Reading the DAG in the same snapshot as the
        // manifest is what keeps a concurrently-collected path from
        // surfacing as DATA_LOSS ("no castore index") — that
        // classification is reserved for a complete manifest whose
        // index is genuinely missing (see `get_manifest_with_dag`).
        let (manifest, dag) = metadata::get_manifest_with_dag(&self.pool, &req.store_path)
            .await
            .map_err(|e| metadata_status("GetPath: get_manifest_with_dag", e))?
            .ok_or_else(|| {
                Status::not_found(format!("manifest not found for: {}", req.store_path))
            })?;

        let (root, dirs) = decode_dag_for(&info.store_path, dag)?;
        self.stream_path(vis, info, manifest, root, dirs).await
    }

    /// Steps 2-4 of GetPath: pre-flight backend check, spawn the
    /// streaming task, hash-verify on the way out.
    async fn stream_path(
        &self,
        _vis: sign::PathVisible,
        info: rio_proto::validated::ValidatedPathInfo,
        manifest: ManifestKind,
        root: RootNode,
        dirs: HashMap<[u8; 32], Directory>,
    ) -> Result<Response<GetPathStream>, Status> {
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
        let store_path = info.store_path.to_string();
        let start = std::time::Instant::now();
        // Clone for the spawned task. Arc-clone is cheap; the cache
        // itself (moka + DashMap) is shared.
        let cache = self.chunk_cache.clone();
        let prefetch_k = self.chunk_prefetch_k;

        let (tx, rx) = tokio::sync::mpsc::channel(16);
        let info_raw: PathInfo = info.into();

        // r[impl store.shutdown.drain-getpath]
        // Incremented HERE (synchronously, before returning Response)
        // so a SIGTERM that races the spawn sees active≥1. Guard moves
        // into the task; drops on any exit path.
        let guard = ActiveStreamGuard::new(Arc::clone(&self.active_get_path_streams));

        rio_common::task::spawn_monitored("get-path-stream", async move {
            let _guard = guard;
            // The whole streaming task is bounded by `drain_with_timeout`
            // below.
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

                // Step 3+4: regenerate the NAR framing from the
                // Directory DAG and splice file contents in between.
                // Everything sent — framing and contents — feeds the
                // SHA-256 incrementally; the check at the end is over
                // the exact byte stream the client received.
                let mut hasher = Sha256::new();
                let mut total_bytes = 0u64;

                // Content source state. Inline: a cursor over the blob
                // stream (file contents in walk order — the SAME order
                // nar_pieces yields Contents placeholders, so a running
                // cursor is the lookup). Chunked: an order-preserving
                // K-parallel prefetch over the manifest's chunk list
                // (also in walk order; file boundaries are chunk
                // boundaries so each Contents placeholder consumes a
                // whole number of chunks).
                use futures_util::stream::{self, StreamExt};
                let (inline_blob, mut chunk_stream) = match manifest {
                    ManifestKind::Inline(bytes) => (Some(bytes), None),
                    ManifestKind::Chunked(entries) => {
                        // Pre-flight checked cache is Some.
                        let cache = cache.expect("pre-flight checked chunk_cache is Some");
                        // r[impl store.get.chunk-prefetch]
                        // `buffered()` preserves order — chunk i arrives
                        // before chunk i+1 even if i+1's fetch finishes
                        // first. `buffer_unordered` would scramble the
                        // file contents. BLAKE3 verify happens inside
                        // get_verified.
                        let s = stream::iter(entries)
                            .map(move |(hash, _size)| {
                                let cache = Arc::clone(&cache);
                                async move { cache.get_verified(&hash).await }
                            })
                            .buffered(prefetch_k);
                        (None, Some(Box::pin(s)))
                    }
                };
                let mut inline_cursor: usize = 0;

                for piece in castore_nar::nar_pieces(&root, &dirs, MAX_WALK_NODES) {
                    let piece = match piece {
                        Ok(p) => p,
                        Err(e) => {
                            // A directory body referenced by the walk is
                            // missing/corrupt. The index was written
                            // atomically with the manifest, so this is
                            // storage corruption, not a race.
                            error!(error = %e, "GetPath: castore walk failed");
                            let _ = tx
                                .send(Err(Status::data_loss(format!("castore walk failed: {e}"))))
                                .await;
                            return;
                        }
                    };
                    match piece {
                        NarPiece::Framing(bytes) => {
                            hasher.update(&bytes);
                            total_bytes += bytes.len() as u64;
                            if !stream_bytes(&tx, &Bytes::from(bytes)).await {
                                return; // client disconnected
                            }
                        }
                        NarPiece::Contents { size, .. } => {
                            if let Some(blob) = &inline_blob {
                                // Checked slice: a truncated blob is
                                // DATA_LOSS, not a panic.
                                let end = inline_cursor.checked_add(size as usize);
                                let slice = end
                                    .filter(|e| *e <= blob.len())
                                    .map(|e| blob.slice(inline_cursor..e));
                                let Some(slice) = slice else {
                                    let _ = tx
                                        .send(Err(Status::data_loss(
                                            "inline blob stream shorter than the Directory DAG implies",
                                        )))
                                        .await;
                                    return;
                                };
                                inline_cursor += size as usize;
                                hasher.update(&slice);
                                total_bytes += slice.len() as u64;
                                if !stream_bytes(&tx, &slice).await {
                                    return;
                                }
                            } else {
                                // Consume whole chunks until this file's
                                // content is fully streamed. File
                                // boundaries are chunk boundaries, so
                                // the run sums to exactly `size` for a
                                // consistent manifest; anything else is
                                // DATA_LOSS.
                                let stream = chunk_stream
                                    .as_mut()
                                    .expect("chunked manifest has a chunk stream");
                                let mut got = 0u64;
                                while got < size {
                                    let chunk_bytes = match stream.next().await {
                                        Some(Ok(b)) => b,
                                        Some(Err(e)) => {
                                            error!(error = %e, "GetPath: chunk fetch/verify failed");
                                            // DATA_LOSS: the manifest says
                                            // this chunk exists, but we
                                            // can't get good bytes for it.
                                            let _ = tx
                                                .send(Err(Status::data_loss(format!(
                                                    "chunk reassembly failed: {e}"
                                                ))))
                                                .await;
                                            return;
                                        }
                                        None => {
                                            let _ = tx
                                                .send(Err(Status::data_loss(
                                                    "chunk list exhausted before the Directory DAG's files were served",
                                                )))
                                                .await;
                                            return;
                                        }
                                    };
                                    got += chunk_bytes.len() as u64;
                                    hasher.update(&chunk_bytes);
                                    total_bytes += chunk_bytes.len() as u64;
                                    if !stream_bytes(&tx, &chunk_bytes).await {
                                        return; // client disconnected
                                    }
                                }
                                if got != size {
                                    let _ = tx
                                        .send(Err(Status::data_loss(format!(
                                            "chunk run for a {size}-byte file yielded {got} bytes \
                                             (file boundaries must be chunk boundaries)"
                                        ))))
                                        .await;
                                    return;
                                }
                            }
                        }
                    }
                }

                // Step 4: whole-NAR SHA-256 verify over the regenerated
                // framing + spliced contents. The chunked path already
                // BLAKE3-verified each chunk, so this is belt-and-
                // suspenders: catches (a) the manifest or DAG being
                // WRONG (right chunks, wrong order / missing one /
                // wrong tree), (b) a bug in our reassembly, (c)
                // narinfo.nar_hash being stale.
                //
                // For inline, this is the PRIMARY check (no per-piece
                // verify for inline blobs).
                //
                // r[impl store.get.size-sanity-check]
                // The size half of the check (total_bytes vs nar_size)
                // subsumes the old pre-flight manifest-sum comparison:
                // the manifest now sums to the blob-stream length, not
                // nar_size, so the only place the full NAR length is
                // known is after the framing has been regenerated.
                let actual: [u8; 32] = hasher.finalize().into();
                if actual != expected_hash || total_bytes != expected_size {
                    error!(
                        expected_hash = %hex::encode(expected_hash),
                        actual_hash = %hex::encode(actual),
                        expected_size,
                        total_bytes,
                        "GetPath: whole-NAR integrity check failed"
                    );
                    metrics::counter!("rio_store_integrity_failures_total", "site" => "get_path")
                        .increment(1);
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

            if drain_with_timeout("GetPath", &tx, stream_fut)
                .await
                .is_none()
            {
                warn!(
                    store_path = %store_path,
                    nar_size = expected_size,
                    elapsed = ?start.elapsed(),
                    "GetPath streaming task timed out"
                );
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Guard increments synchronously on `new()` and decrements on
    /// drop — including when the holding future is aborted (the
    /// `spawn_monitored` task may be cancelled if the runtime shuts
    /// down before the stream completes).
    // r[verify store.shutdown.drain-getpath]
    #[tokio::test]
    async fn active_stream_guard_decrements_on_abort() {
        let counter = Arc::new(AtomicUsize::new(0));
        assert_eq!(counter.load(Ordering::Relaxed), 0);

        let guard = ActiveStreamGuard::new(Arc::clone(&counter));
        assert_eq!(counter.load(Ordering::Relaxed), 1, "increments on new()");

        let handle = tokio::spawn(async move {
            let _guard = guard;
            std::future::pending::<()>().await;
        });
        tokio::task::yield_now().await;
        assert_eq!(counter.load(Ordering::Relaxed), 1, "still held by task");

        handle.abort();
        let _ = handle.await;
        assert_eq!(
            counter.load(Ordering::Relaxed),
            0,
            "decrements on abort via Drop"
        );
    }
}
