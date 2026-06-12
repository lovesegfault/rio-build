//! ADR-022 castore RPC surface (P0573 / P0577 / P0570).
//!
//! `GetDirectory` / `HasDirectories` / `HasBlobs` / `ReadBlob` /
//! `StatBlob`. All tenant-scoped: every query resolves a digest to the
//! store path(s) that contain it (`directory_paths` /
//! `file_blobs.store_path_hash`) and joins `path_tenants` on the
//! caller's `tenant_id`. When the junction probe misses, a digest may
//! still resolve through a SUBSTITUTION-ONLY containing path (zero
//! `path_tenants` rows) whose narinfo signature verifies against the
//! caller's trusted keys — the same per-caller predicate the validity
//! surface applies (`sig_visibility_gate` in sign.rs), so a path
//! reported valid is always readable. Anything else is invisible
//! (NotFound, or absent from the bitmap). Directory bodies leak child
//! names/digests — confidentiality, not just isolation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use bytes::Bytes;
use futures_util::StreamExt;
use prost::Message;
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{instrument, warn};

use rio_proto::DirectoryService;
use rio_proto::castore::Directory;
use rio_proto::types::{
    BlobChunk, ChunkMeta, GetDirectoryRequest, HasBitmap, HasBlobsRequest, HasDirectoriesRequest,
    ReadBlobRequest, StatBlobRequest, StatBlobResponse,
};

use super::{drain_with_timeout, sign};
use crate::cas::ChunkCache;
use crate::signing::TenantSigner;

/// BFS frontier batch cap. ~33 round trips for a chromium-scale
/// closure (8k dirs); matches PG's parameter limit headroom. This
/// only bounds the *size* side-query — the body fetch is further
/// split by [`GET_DIRECTORY_BATCH_BYTES`].
const GET_DIRECTORY_BATCH: usize = 256;

/// Per-`fetch_all` byte ceiling for `GetDirectory` body batches.
///
/// `Directory.body` rows have no write-side cap (only an entry-count
/// cap, `MAX_DIRECTORY_ENTRIES` ~46 MB encoded), so a count-only batch
/// can transiently materialize ~12 GB into one `Vec` before a single
/// row reaches the bounded channel. The size side-query (`length(body)`,
/// no TOAST detoast) lets the batch be split greedily under this
/// ceiling — same byte-budgeting `GetPath`/`PutPath` already apply.
const GET_DIRECTORY_BATCH_BYTES: u64 = 32 * 1024 * 1024;

/// Stream channel depth. Small on purpose — h2 flow control already
/// backpressures the wire, and a deep channel just buffers decoded
/// `Directory` bodies (potentially MBs each) for a slow client.
/// Decoupled from `GET_DIRECTORY_BATCH`: SQL batch ≠ in-flight bodies.
const GET_DIRECTORY_CHANNEL_DEPTH: usize = 8;

/// Hard ceiling on directories streamed by one recursive walk. A
/// chromium-scale closure is ~8k dirs; 256k means a pathological DAG
/// (or a bug). Past it the stream errors `RESOURCE_EXHAUSTED`.
///
/// TODO: this cap is not derived from the ingest-side bounds
/// (`r[store.ingest.tree-bounds]`): ingest accepts up to `MAX_DIR_NODES`
/// (1_048_576) distinct directory bodies per path, and the one production
/// consumer of `recursive=true` — the builder's FUSE mount-time prefetch
/// (`rio-builder/src/castore_fuse/tree.rs`) — walks a whole *input
/// closure* in a single call, so no per-path constant can make "ingest
/// accepts ⇒ the closure mounts" structural here. Real closures are ~8k
/// distinct dirs, leaving ~32× margin today. Closing the gap means paging
/// the recursive walk (continuation tokens, or one walk per root);
/// raising this cap alone only moves the cliff.
const GET_DIRECTORY_MAX_RESULTS: usize = 262_144;

/// Cap on the request digest list for `HasDirectories`/`HasBlobs`.
/// Both are single-RPC presence probes — a delta-sync caller batches
/// at the closure size (~25k files for chromium); larger means a
/// pathological caller or a bug.
pub(super) const HAS_BATCH_MAX: usize = 65_536;

/// `ReadBlob` chunk prefetch width. `buffered()`, not `_unordered`:
/// file bytes must arrive in offset order. Lower than `GetPath`'s 64
/// because a single-file read pulls fewer chunks and the FUSE caller
/// is rarely throughput-bound.
const READ_BLOB_PREFETCH_K: usize = 8;

/// `ReadBlob` response channel depth. Same rationale as
/// [`GET_DIRECTORY_CHANNEL_DEPTH`].
const READ_BLOB_CHANNEL_DEPTH: usize = 4;

pub struct DirectoryServiceImpl {
    pool: PgPool,
    /// HMAC verifier for assignment tokens. Same key the dispatch
    /// signer uses (NOT the service-token key). The builder presents
    /// `x-rio-assignment-token`; `claims.tenant` carries the tenant.
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// Shared with `StoreServiceImpl`/`ChunkServiceImpl`; `ReadBlob`
    /// fetches chunked-manifest files through it. `None` on an
    /// inline-only store: chunked files return `FAILED_PRECONDITION`,
    /// inline files still resolve.
    chunk_cache: Option<Arc<ChunkCache>>,
    /// Same `TenantSigner` the StoreService holds. DirectoryService
    /// never signs — this only feeds the caller's trusted-key set
    /// (cluster key + prior rotation history) into the sig-visibility
    /// fallback, so the read surface and `sig_visibility_gate` derive
    /// identical trust. `None` = no cluster key in the trusted set
    /// (upstream `trusted_keys` and `tenant_keys` still apply).
    signer: Option<Arc<TenantSigner>>,
}

impl DirectoryServiceImpl {
    pub fn new(
        pool: PgPool,
        hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
        chunk_cache: Option<Arc<ChunkCache>>,
        signer: Option<Arc<TenantSigner>>,
    ) -> Self {
        Self {
            pool,
            hmac_verifier,
            chunk_cache,
            signer,
        }
    }

    /// Resolve the caller's tenant via the shared
    /// [`resolve_castore_tenant`] ladder.
    // r[impl store.castore.tenant-scope+3]
    fn castore_tenant_id<T>(&self, request: &Request<T>) -> Result<uuid::Uuid, Status> {
        resolve_castore_tenant(
            request,
            self.hmac_verifier.as_ref(),
            "DirectoryService requires a tenant: send a JWT or an HMAC \
             assignment token",
        )
    }
}

/// Resolve the caller's tenant: JWT extension (gateway path), else
/// HMAC assignment-token `claims.tenant` (builder path), via the
/// shared [`super::resolve_tenant_id`] mapping (the write side uses
/// the same one, so content committed by a builder is readable by
/// that builder's tenant). No tenant → `UNAUTHENTICATED`; never fall
/// back to anonymous. Shared by `DirectoryService`, `ChunkService`
/// (`HasChunks`), and `DrvBlobService` so the tenant-scoped surfaces
/// cannot drift on what counts as a tenant.
// r[impl store.castore.tenant-scope+3]
pub(super) fn resolve_castore_tenant<T>(
    request: &Request<T>,
    verifier: Option<&Arc<rio_auth::hmac::HmacVerifier>>,
    missing: &'static str,
) -> Result<uuid::Uuid, Status> {
    match caller_identity(request, verifier, missing)? {
        CallerIdentity::Jwt(sub) => Ok(sub),
        CallerIdentity::Hmac(claims) => {
            super::resolve_tenant_id(None, Some(&claims)).ok_or_else(|| {
                Status::unauthenticated(
                    "assignment token has no usable tenant claim (missing or not a UUID)",
                )
            })
        }
    }
}

/// A verified caller identity: the gateway's JWT extension or a
/// builder's HMAC assignment token. Shared with `ChunkService`
/// (chunk.rs) so the two auth ladders can't drift on what counts as
/// "authenticated"; each caller decides what to do with the identity
/// (the tenant-scoped surfaces resolve a tenant via
/// [`resolve_castore_tenant`]; the chunk retrieval RPCs only need
/// proof).
pub(super) enum CallerIdentity {
    /// `TenantClaims.sub` from the gateway's JWT interceptor.
    Jwt(uuid::Uuid),
    /// Verified builder assignment-token claims.
    Hmac(rio_auth::hmac::AssignmentClaims),
}

/// JWT request extension (gateway path), else `ASSIGNMENT_TOKEN_HEADER`
/// verified against `verifier` (builder path). `missing` is the
/// `UNAUTHENTICATED` message when neither identity is present — callers
/// name themselves so the error stays actionable.
pub(super) fn caller_identity<T>(
    request: &Request<T>,
    verifier: Option<&Arc<rio_auth::hmac::HmacVerifier>>,
    missing: &'static str,
) -> Result<CallerIdentity, Status> {
    if let Some(jwt) = request
        .extensions()
        .get::<rio_auth::jwt::TenantClaims>()
        .map(|c| c.sub)
    {
        return Ok(CallerIdentity::Jwt(jwt));
    }
    let tok = request
        .metadata()
        .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
        .and_then(|v| v.to_str().ok());
    if let (Some(verifier), Some(tok)) = (verifier, tok) {
        return match verifier.verify::<rio_auth::hmac::AssignmentClaims>(tok) {
            Ok(claims) => Ok(CallerIdentity::Hmac(claims)),
            // Don't echo why the token failed — sig-vs-expiry is an
            // oracle. The legitimate caller's fix is the same.
            Err(_) => Err(Status::unauthenticated("assignment token rejected")),
        };
    }
    Err(Status::unauthenticated(missing))
}

pub(super) fn parse_digest(d: &[u8]) -> Result<[u8; 32], Status> {
    d.try_into()
        .map_err(|_| Status::invalid_argument(format!("digest must be 32 bytes, got {}", d.len())))
}

pub(super) fn parse_digests(ds: &[Vec<u8>]) -> Result<Vec<[u8; 32]>, Status> {
    if ds.len() > HAS_BATCH_MAX {
        return Err(Status::invalid_argument(format!(
            "digest batch too large: {} > {HAS_BATCH_MAX}",
            ds.len()
        )));
    }
    ds.iter().map(|d| parse_digest(d)).collect()
}

/// Bit `i` set ⇔ `requested[i]` ∈ `present`. LSB-first within each
/// byte; trailing bits zeroed. Shared with `HasChunks` (chunk.rs) so
/// the three presence probes can't drift on bit order.
pub(super) fn build_bitmap(requested: &[[u8; 32]], present: &HashSet<[u8; 32]>) -> Vec<u8> {
    let mut bitmap = vec![0u8; requested.len().div_ceil(8)];
    for (i, d) in requested.iter().enumerate() {
        if present.contains(d) {
            bitmap[i / 8] |= 1 << (i % 8);
        }
    }
    bitmap
}

#[tonic::async_trait]
// r[impl store.castore.directory-rpc]
impl DirectoryService for DirectoryServiceImpl {
    type GetDirectoryStream = ReceiverStream<Result<Directory, Status>>;
    type ReadBlobStream = ReceiverStream<Result<BlobChunk, Status>>;

    /// Server-side BFS over the Directory DAG.
    ///
    /// `recursive=false`: exactly the requested body, NotFound if
    /// missing or not tenant-visible. `recursive=true`: BFS from the
    /// frontier `{by_what.digest} ∪ digests`, deduped on digest, until
    /// the frontier drains. Children absent from the table (GC'd or
    /// not visible) are skipped — the client detects the gap by
    /// absence, not error.
    #[instrument(skip(self, request), fields(rpc = "GetDirectory"))]
    async fn get_directory(
        &self,
        request: Request<GetDirectoryRequest>,
    ) -> Result<Response<Self::GetDirectoryStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        let req = request.into_inner();

        let mut frontier: Vec<[u8; 32]> = Vec::new();
        if let Some(rio_proto::types::get_directory_request::ByWhat::Digest(d)) = &req.by_what {
            frontier.push(parse_digest(d)?);
        }
        for d in &req.digests {
            frontier.push(parse_digest(d)?);
        }
        if frontier.is_empty() {
            return Err(Status::invalid_argument("at least one digest required"));
        }
        if frontier.len() > HAS_BATCH_MAX {
            return Err(Status::invalid_argument(format!(
                "digest batch too large: {} > {HAS_BATCH_MAX}",
                frontier.len()
            )));
        }

        if !req.recursive {
            // Multi-root non-recursive is ambiguous: which body? Reject.
            if frontier.len() != 1 {
                return Err(Status::invalid_argument(
                    "non-recursive GetDirectory takes exactly one digest",
                ));
            }
            let digest = frontier[0];
            let mut body: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT d.body FROM directories d \
                  JOIN directory_paths dp ON dp.digest = d.digest \
                  JOIN path_tenants pt ON pt.store_path_hash = dp.store_path_hash \
                 WHERE d.digest = $1 AND pt.tenant_id = $2 \
                 LIMIT 1",
            )
            .bind(digest.as_slice())
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
            if body.is_none()
                && sig_fallback_digests(
                    &self.pool,
                    self.signer.as_deref(),
                    SUBST_ONLY_DIRECTORY_CANDIDATES_SQL,
                    &[digest],
                    tenant,
                )
                .await?
                .contains(&digest)
            {
                // Junction miss but the digest is reachable through a
                // sig-visible substitution-only path — content lookup
                // is by digest (directories PK), authorization just
                // happened above.
                body = sqlx::query_as("SELECT body FROM directories WHERE digest = $1")
                    .bind(digest.as_slice())
                    .fetch_optional(&self.pool)
                    .await
                    .map_err(internal)?;
            }
            let Some((body,)) = body else {
                return Err(Status::not_found("directory not found"));
            };
            let dir = Directory::decode(body.as_slice()).map_err(corrupt)?;
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let _ = tx.send(Ok(dir)).await;
            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        // Recursive BFS, spawned so the stream can outlive this call.
        // No drain guard (unlike get_path): a SIGTERM RST-closing this
        // mid-stream is fine — the FUSE prefetch retries from where it
        // left off via the deduped `seen` set on its side.
        let pool = self.pool.clone();
        let signer = self.signer.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(GET_DIRECTORY_CHANNEL_DEPTH);
        rio_common::task::spawn_monitored("get-directory-bfs", async move {
            let started = Instant::now();
            // `seen` dedups both seeds and discovered children. Rebuild
            // the frontier from it so a duplicate seed split across two
            // SQL chunks isn't streamed twice.
            let mut seen: HashSet<[u8; 32]> = frontier.iter().copied().collect();
            let mut frontier: Vec<[u8; 32]> = seen.iter().copied().collect();
            let mut sent = 0usize;
            // A stalled client would pin the buffered Directory bodies;
            // `drain_with_timeout` is the backstop.
            let walk = async {
                while !frontier.is_empty() {
                    let mut next: Vec<[u8; 32]> = Vec::new();
                    for batch in frontier.chunks(GET_DIRECTORY_BATCH) {
                        let digests: Vec<&[u8]> = batch.iter().map(|d| d.as_slice()).collect();
                        // Size side-query: `length(body)` reads the
                        // bytea header, never detoasts. Splits the
                        // body fetch under GET_DIRECTORY_BATCH_BYTES
                        // so one `fetch_all` can't materialize GBs.
                        // DISTINCT: a digest shared by N tenant-visible
                        // paths must count once (and stream once).
                        type SizeRow = (Vec<u8>, i64);
                        let mut sizes: Vec<SizeRow> = match sqlx::query_as(
                            "SELECT DISTINCT d.digest, length(d.body)::bigint FROM directories d \
                              JOIN directory_paths dp ON dp.digest = d.digest \
                              JOIN path_tenants pt ON pt.store_path_hash = dp.store_path_hash \
                             WHERE d.digest = ANY($1::bytea[]) AND pt.tenant_id = $2",
                        )
                        .bind(&digests)
                        .bind(tenant)
                        .fetch_all(&pool)
                        .await
                        {
                            Ok(r) => r,
                            Err(e) => {
                                let _ = tx.send(Err(internal(e))).await;
                                return;
                            }
                        };
                        // Junction misses → sig-visibility fallback.
                        // This stage is where tenant authorization
                        // happens for the whole batch (junction OR
                        // sig-visible substitution-only path); the
                        // body fetch below is a pure content lookup.
                        let got: HashSet<[u8; 32]> = sizes
                            .iter()
                            .filter_map(|(d, _)| d.as_slice().try_into().ok())
                            .collect();
                        let missing: Vec<[u8; 32]> = batch
                            .iter()
                            .filter(|d| !got.contains(*d))
                            .copied()
                            .collect();
                        if !missing.is_empty() {
                            let extra = match sig_fallback_digests(
                                &pool,
                                signer.as_deref(),
                                SUBST_ONLY_DIRECTORY_CANDIDATES_SQL,
                                &missing,
                                tenant,
                            )
                            .await
                            {
                                Ok(e) => e,
                                Err(e) => {
                                    let _ = tx.send(Err(e)).await;
                                    return;
                                }
                            };
                            if !extra.is_empty() {
                                let extra_slices: Vec<&[u8]> =
                                    extra.iter().map(|d| d.as_slice()).collect();
                                let extra_sizes: Vec<SizeRow> = match sqlx::query_as(
                                    "SELECT digest, length(body)::bigint FROM directories \
                                     WHERE digest = ANY($1::bytea[])",
                                )
                                .bind(&extra_slices)
                                .fetch_all(&pool)
                                .await
                                {
                                    Ok(r) => r,
                                    Err(e) => {
                                        let _ = tx.send(Err(internal(e))).await;
                                        return;
                                    }
                                };
                                sizes.extend(extra_sizes);
                            }
                        }
                        for sub in greedy_split_by_bytes(&sizes, GET_DIRECTORY_BATCH_BYTES) {
                            let sub_digests: Vec<&[u8]> =
                                sub.iter().map(|(d, _)| d.as_slice()).collect();
                            // No tenancy join here: every digest in
                            // `sub` was authorized by the size stage
                            // above (junction or sig-fallback), and
                            // `directories.digest` is the PK — re-
                            // joining `path_tenants` would drop the
                            // sig-visible digests again.
                            let rows: Vec<(Vec<u8>,)> = match sqlx::query_as(
                                "SELECT body FROM directories \
                                 WHERE digest = ANY($1::bytea[]) \
                                 ORDER BY digest",
                            )
                            .bind(&sub_digests)
                            .fetch_all(&pool)
                            .await
                            {
                                Ok(r) => r,
                                Err(e) => {
                                    let _ = tx.send(Err(internal(e))).await;
                                    return;
                                }
                            };
                            for (body,) in rows {
                                let dir = match Directory::decode(body.as_slice()) {
                                    Ok(d) => d,
                                    Err(e) => {
                                        // A corrupt persisted body is a write-side
                                        // bug, not a transient — fail loud.
                                        let _ = tx.send(Err(corrupt(e))).await;
                                        return;
                                    }
                                };
                                for child in &dir.directories {
                                    match parse_digest(&child.digest) {
                                        Ok(d) => {
                                            if seen.insert(d) {
                                                next.push(d);
                                            }
                                        }
                                        // Same write-side corruption class as an
                                        // undecodable body, but recoverable: skip
                                        // the child and keep streaming.
                                        Err(_) => warn!(
                                            len = child.digest.len(),
                                            "corrupt child digest in directory body; skipping"
                                        ),
                                    }
                                }
                                sent += 1;
                                if sent > GET_DIRECTORY_MAX_RESULTS {
                                    let _ = tx
                                    .send(Err(Status::resource_exhausted(format!(
                                        "directory walk exceeded {GET_DIRECTORY_MAX_RESULTS} results"
                                    ))))
                                    .await;
                                    return;
                                }
                                if tx.send(Ok(dir)).await.is_err() {
                                    return; // client hung up
                                }
                            }
                        }
                    }
                    frontier = next;
                }
                metrics::histogram!("rio_store_directory_get_seconds")
                    .record(started.elapsed().as_secs_f64());
            };
            if drain_with_timeout("GetDirectory", &tx, walk)
                .await
                .is_none()
            {
                warn!(sent, "GetDirectory stream timed out");
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    #[instrument(skip(self, request), fields(rpc = "HasDirectories"))]
    async fn has_directories(
        &self,
        request: Request<HasDirectoriesRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        self.has_response(
            &request.into_inner().digests,
            HAS_DIRECTORIES_SQL,
            SUBST_ONLY_DIRECTORY_CANDIDATES_SQL,
            "HasDirectories",
            tenant,
        )
        .await
    }

    /// Presence bitmap over `file_blobs` × `path_tenants`. The join is
    /// per-path (`store_path_hash`), so a digest is "present" iff at
    /// least one tenant-readable NAR contains it — GC of one referrer
    /// can't dangle the answer.
    #[instrument(skip(self, request), fields(rpc = "HasBlobs"))]
    async fn has_blobs(
        &self,
        request: Request<HasBlobsRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        self.has_response(
            &request.into_inner().digests,
            HAS_BLOBS_SQL,
            SUBST_ONLY_BLOB_CANDIDATES_SQL,
            "HasBlobs",
            tenant,
        )
        .await
    }

    /// Stream a regular file's bytes by `file_digest`.
    ///
    /// `file_digest → (nar_offset, size)` via `file_blobs`, then to a
    /// chunk window via the manifest cumsum. The caller never sees
    /// rio's chunk layout.
    // r[impl store.castore.blob-read]
    #[instrument(skip(self, request), fields(rpc = "ReadBlob"))]
    async fn read_blob(
        &self,
        request: Request<ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        let digest = parse_digest(&request.into_inner().file_digest)?;
        let started = Instant::now();

        // `manifests.status` filter excludes 'uploading' placeholders.
        // LIMIT 1: any tenant-visible referrer's NAR works; the bytes
        // are content-addressed. The `path_tenants` join is on the same
        // `store_path_hash` as `file_blobs`, so the chosen NAR is one
        // this tenant may read — a digest-only join could pick another
        // tenant's NAR for a content-shared file. The ORDER BY makes
        // the winner deterministic (lowest tenant-visible referrer,
        // same ordering as the upload-side window proof's canonical
        // row), so a row racing in cannot steer resolution by
        // insertion order.
        // `file_blobs.size` (M_066) is denormalized so this never
        // decodes `nar_index.entries` (O(files-in-NAR)) on the FUSE
        // `open()` fast path.
        type BlobRow = (i64, i64, Option<Vec<u8>>, Option<Vec<u8>>);
        let mut row: Option<BlobRow> = sqlx::query_as(
            "SELECT f.nar_offset, f.size, m.inline_blob, md.chunk_list \
               FROM file_blobs f \
               JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
               JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                    AND m.status = 'complete' \
               LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
              WHERE f.digest = $1 AND pt.tenant_id = $2 \
              ORDER BY f.store_path_hash, f.nar_offset \
              LIMIT 1",
        )
        .bind(digest.as_slice())
        .bind(tenant)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        if row.is_none() {
            // Junction miss → sig-visible substitution-only path. Same
            // row shape, pinned to the path the fallback authorized.
            if let Some(path_hash) =
                blob_sig_fallback_hash(&self.pool, self.signer.as_deref(), &digest, tenant).await?
            {
                row = sqlx::query_as(
                    "SELECT f.nar_offset, f.size, m.inline_blob, md.chunk_list \
                       FROM file_blobs f \
                       JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                            AND m.status = 'complete' \
                       LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
                      WHERE f.digest = $1 AND f.store_path_hash = $2",
                )
                .bind(digest.as_slice())
                .bind(&path_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(internal)?;
            }
        }
        let Some((nar_offset, file_size, inline_blob, chunk_list)) = row else {
            return Err(Status::not_found("file blob not found"));
        };
        let (nar_offset, end) = nar_window(nar_offset, file_size)?;

        let plan = match (inline_blob, chunk_list) {
            (Some(blob), _) => BlobPlan::Inline(slice_inline(blob, nar_offset, end)?),
            (None, Some(chunk_list)) => {
                let cache = self.chunk_cache.clone().ok_or_else(|| {
                    Status::failed_precondition(
                        "ReadBlob requires a chunk backend for chunked manifests",
                    )
                })?;
                BlobPlan::Chunked(cache, build_chunk_plan(&chunk_list, nar_offset, end)?)
            }
            // Same invariant `metadata::get_manifest` enforces: a
            // complete manifest has exactly one of inline_blob /
            // manifest_data. Surface as DATA_LOSS, not NotFound, so
            // corruption isn't mistaken for a cache miss.
            (None, None) => {
                return Err(Status::data_loss(
                    "manifest has neither inline_blob nor manifest_data",
                ));
            }
        };

        let (tx, rx) = tokio::sync::mpsc::channel(READ_BLOB_CHANNEL_DEPTH);
        rio_common::task::spawn_monitored("read-blob-stream", async move {
            let stream_fut = stream_blob(&tx, plan, digest);
            match drain_with_timeout("ReadBlob", &tx, stream_fut).await {
                None => {
                    warn!(file_size = end - nar_offset, "ReadBlob stream timed out");
                }
                // Record only on a clean stream so DATA_LOSS and
                // disconnect timings don't skew the histogram. Same
                // policy as get_directory and get_path.
                Some(true) => {
                    metrics::histogram!("rio_store_directory_read_seconds")
                        .record(started.elapsed().as_secs_f64());
                }
                Some(false) => {}
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// Resolve `file_digest` to a chunk window. Same `file_blobs` →
    /// cumsum resolution as `read_blob`, but returns the chunk list so
    /// the FUSE caller can fetch chunks itself (`/var/rio/chunks/`,
    /// then `GetChunks`) and slice the boundary chunks.
    // r[impl store.castore.blob-stat]
    #[instrument(skip(self, request), fields(rpc = "StatBlob"))]
    async fn stat_blob(
        &self,
        request: Request<StatBlobRequest>,
    ) -> Result<Response<StatBlobResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        let req = request.into_inner();
        let digest = parse_digest(&req.file_digest)?;

        // Probe before the chunk-list query: `md.chunk_list` is TOASTed
        // (megabytes for a large NAR) and the probe never reads it.
        // `has_in()` would also do, but it skips `m.status = 'complete'`.
        if !req.send_chunks {
            let exists: Option<(i32,)> = sqlx::query_as(
                "SELECT 1 FROM file_blobs f \
                   JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
                   JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                        AND m.status = 'complete' \
                  WHERE f.digest = $1 AND pt.tenant_id = $2 LIMIT 1",
            )
            .bind(digest.as_slice())
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
            // Junction miss → sig-visibility fallback, same as ReadBlob.
            let visible = exists.is_some()
                || blob_sig_fallback_hash(&self.pool, self.signer.as_deref(), &digest, tenant)
                    .await?
                    .is_some();
            return visible
                .then(|| Response::new(StatBlobResponse::default()))
                .ok_or_else(|| Status::not_found("file blob not found"));
        }

        // `IS NOT NULL`, not the bytes: only the classification
        // matters. ORDER BY: same deterministic-winner rule as
        // `read_blob` — `StatBlob` hands the chunk window straight to
        // the FUSE caller, so which referrer resolves must not depend
        // on insertion order.
        type StatRow = (i64, i64, bool, Option<Vec<u8>>);
        let mut row: Option<StatRow> = sqlx::query_as(
            "SELECT f.nar_offset, f.size, m.inline_blob IS NOT NULL, md.chunk_list \
               FROM file_blobs f \
               JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
               JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                    AND m.status = 'complete' \
               LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
              WHERE f.digest = $1 AND pt.tenant_id = $2 \
              ORDER BY f.store_path_hash, f.nar_offset \
              LIMIT 1",
        )
        .bind(digest.as_slice())
        .bind(tenant)
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        if row.is_none() {
            // Junction miss → sig-visible substitution-only path, same
            // as ReadBlob.
            if let Some(path_hash) =
                blob_sig_fallback_hash(&self.pool, self.signer.as_deref(), &digest, tenant).await?
            {
                row = sqlx::query_as(
                    "SELECT f.nar_offset, f.size, m.inline_blob IS NOT NULL, md.chunk_list \
                       FROM file_blobs f \
                       JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                            AND m.status = 'complete' \
                       LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
                      WHERE f.digest = $1 AND f.store_path_hash = $2",
                )
                .bind(digest.as_slice())
                .bind(&path_hash)
                .fetch_optional(&self.pool)
                .await
                .map_err(internal)?;
            }
        }
        let Some((nar_offset, file_size, is_inline, chunk_list)) = row else {
            return Err(Status::not_found("file blob not found"));
        };
        if is_inline {
            // No chunk list to return; a synthetic ChunkMeta wouldn't be
            // in the chunk store, and a file in an inline NAR is small
            // enough for ReadBlob anyway.
            return Err(Status::failed_precondition(
                "file is in an inline manifest; use ReadBlob",
            ));
        }
        let Some(chunk_list) = chunk_list else {
            return Err(Status::data_loss(
                "manifest has neither inline_blob nor manifest_data",
            ));
        };
        let (nar_offset, end) = nar_window(nar_offset, file_size)?;
        let plan = build_chunk_plan(&chunk_list, nar_offset, end)?;
        // Bounded by entry.size: u32, but a future build_chunk_plan
        // refactor must not silently wrap a slice offset.
        let off =
            |v: usize| u32::try_from(v).map_err(|_| Status::internal("chunk slice offset > u32"));
        Ok(Response::new(StatBlobResponse {
            first_chunk_skip: plan.first().map_or(Ok(0), |s| off(s.start))?,
            last_chunk_take: plan.last().map_or(Ok(0), |s| off(s.end))?,
            chunks: plan
                .into_iter()
                .map(|s| ChunkMeta {
                    digest: s.hash.to_vec(),
                    size: u64::from(s.size),
                })
                .collect(),
        }))
    }
}

/// `(nar_offset, file_size)` from PG to a `[nar_offset, end)` byte
/// window, rejecting negative or overflowing values as `DATA_LOSS`
/// (write-side bugs, not client errors).
fn nar_window(nar_offset: i64, file_size: i64) -> Result<(u64, u64), Status> {
    let nar_offset = u64::try_from(nar_offset)
        .map_err(|_| Status::data_loss("file_blobs.nar_offset is negative"))?;
    let file_size =
        u64::try_from(file_size).map_err(|_| Status::data_loss("file_blobs.size is negative"))?;
    let end = nar_offset
        .checked_add(file_size)
        .ok_or_else(|| Status::data_loss("nar_offset + file_size overflows"))?;
    Ok((nar_offset, end))
}

/// Resolved fetch plan for one `ReadBlob`.
enum BlobPlan {
    /// File bytes already sliced from `manifests.inline_blob`.
    Inline(Bytes),
    /// Chunk hashes in NAR order with the byte range to emit from each.
    /// Pre-slicing every chunk (not just first/last) keeps the stream
    /// loop branch-free. Carrying the cache here proves a chunked plan
    /// always has a backend.
    Chunked(Arc<ChunkCache>, Vec<ChunkSlice>),
}

struct ChunkSlice {
    hash: [u8; 32],
    /// Total chunk size from the manifest. `ReadBlob`'s stream loop
    /// only needs `start..end`; `StatBlob` returns this so the caller
    /// can size its fetch buffer and validate `GetChunks` responses.
    size: u32,
    /// Byte range within the chunk to emit.
    start: usize,
    end: usize,
}

/// Slice `[nar_offset, end)` from an inline NAR.
fn slice_inline(blob: Vec<u8>, nar_offset: u64, end: u64) -> Result<Bytes, Status> {
    let blob = Bytes::from(blob);
    if end > blob.len() as u64 {
        return Err(Status::data_loss(format!(
            "inline NAR is {} bytes but file ends at {end}",
            blob.len()
        )));
    }
    // Safe: end ≤ blob.len() ≤ usize::MAX (checked above).
    Ok(blob.slice(nar_offset as usize..end as usize))
}

/// Compute the chunk slices covering NAR bytes `[nar_offset, end)`.
/// `cumsum[i]` is where chunk `i` starts; `partition_point` finds the
/// first/last chunks straddling the range.
fn build_chunk_plan(
    chunk_list: &[u8],
    nar_offset: u64,
    end: u64,
) -> Result<Vec<ChunkSlice>, Status> {
    let manifest = crate::manifest::Manifest::deserialize(chunk_list)
        .map_err(|e| Status::data_loss(format!("corrupt manifest_data.chunk_list: {e}")))?;
    let mut cumsum: Vec<u64> = Vec::with_capacity(manifest.entries.len());
    let mut acc = 0u64;
    for e in &manifest.entries {
        cumsum.push(acc);
        acc += u64::from(e.size);
    }
    if end > acc {
        return Err(Status::data_loss(format!(
            "chunked NAR is {acc} bytes but file ends at {end}"
        )));
    }
    if nar_offset == end {
        return Ok(Vec::new()); // zero-byte file
    }
    // Last chunk starting at or before `nar_offset`.
    let first = cumsum
        .partition_point(|&c| c <= nar_offset)
        .saturating_sub(1);
    // First chunk starting at or after `end` (exclusive bound).
    let last_excl = cumsum.partition_point(|&c| c < end);
    let plan = manifest.entries[first..last_excl]
        .iter()
        .zip(&cumsum[first..last_excl])
        .map(|(entry, &chunk_start)| {
            let chunk_end = chunk_start + u64::from(entry.size);
            let take_start = nar_offset.max(chunk_start) - chunk_start;
            let take_end = end.min(chunk_end) - chunk_start;
            ChunkSlice {
                hash: entry.hash,
                size: entry.size,
                // ManifestEntry.size is u32, so the per-chunk range fits usize.
                start: take_start as usize,
                end: take_end as usize,
            }
        })
        .collect();
    Ok(plan)
}

/// Drive the `ReadBlob` body stream. Inline = one sliced buffer;
/// chunked = K-parallel ordered prefetch via `cache.get_verified()`.
///
/// Returns `true` only on a clean stream with the body matching
/// `file_digest`, so the caller can gate the latency histogram.
async fn stream_blob(tx: &FrameTx, plan: BlobPlan, file_digest: [u8; 32]) -> bool {
    let mut hasher = blake3::Hasher::new();
    match plan {
        BlobPlan::Inline(bytes) => {
            for piece in bytes.chunks(rio_proto::client::NAR_CHUNK_SIZE) {
                if !send_piece(tx, &mut hasher, piece).await {
                    return false;
                }
            }
        }
        BlobPlan::Chunked(cache, slices) => {
            let mut chunk_stream = futures_util::stream::iter(slices)
                .map(|s| {
                    let cache = Arc::clone(&cache);
                    async move { cache.get_verified(&s.hash).await.map(|b| (s, b)) }
                })
                .buffered(READ_BLOB_PREFETCH_K);
            while let Some(result) = chunk_stream.next().await {
                let (slice, bytes) = match result {
                    Ok(v) => v,
                    Err(e) => {
                        tracing::error!(error = %e, "ReadBlob: chunk fetch/verify failed");
                        let _ = tx
                            .send(Err(Status::data_loss(format!(
                                "chunk reassembly failed: {e}"
                            ))))
                            .await;
                        return false;
                    }
                };
                let Some(piece) = bytes.get(slice.start..slice.end) else {
                    let _ = tx
                        .send(Err(Status::data_loss(
                            "chunk shorter than manifest declared",
                        )))
                        .await;
                    return false;
                };
                if !send_piece(tx, &mut hasher, piece).await {
                    return false;
                }
            }
        }
    }
    // Whole-file BLAKE3 verify, like GetPath's whole-NAR SHA-256.
    // Per-chunk BLAKE3 already ran in `get_verified()`, so a mismatch
    // here means cumsum/index drift (wrong offset or size), not S3
    // corruption. The trailer attributes the failure to the server
    // instead of leaving the client to guess from a bad hash.
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != file_digest {
        tracing::error!(
            expected = %hex::encode(file_digest),
            actual = %hex::encode(actual),
            "ReadBlob: whole-file integrity check failed"
        );
        metrics::counter!("rio_store_integrity_failures_total", "site" => "read_blob").increment(1);
        let _ = tx
            .send(Err(Status::data_loss(
                "whole-file integrity check failed (BLAKE3 mismatch)",
            )))
            .await;
        return false;
    }
    true
}

type FrameTx = tokio::sync::mpsc::Sender<Result<BlobChunk, Status>>;

/// Hash and emit one body frame. `false` on client disconnect.
async fn send_piece(tx: &FrameTx, hasher: &mut blake3::Hasher, piece: &[u8]) -> bool {
    hasher.update(piece);
    tx.send(Ok(BlobChunk {
        data: piece.to_vec(),
    }))
    .await
    .is_ok()
}

/// `HasDirectories` presence query: a directory digest is present iff
/// at least one tenant-readable store path contains it.
const HAS_DIRECTORIES_SQL: &str = "SELECT DISTINCT d.digest FROM directories d \
     JOIN directory_paths dp ON dp.digest = d.digest \
     JOIN path_tenants t ON t.store_path_hash = dp.store_path_hash \
     WHERE d.digest = ANY($1::bytea[]) AND t.tenant_id = $2";

/// `HasBlobs` presence query: same shape over `file_blobs`, whose
/// junction to `path_tenants` is direct (no `directory_paths` hop).
const HAS_BLOBS_SQL: &str = "SELECT DISTINCT d.digest FROM file_blobs d \
     JOIN path_tenants t ON t.store_path_hash = d.store_path_hash \
     WHERE d.digest = ANY($1::bytea[]) AND t.tenant_id = $2";

/// Sig-visibility fallback candidates over `directory_paths`:
/// `(digest, store_path_hash)` pairs reachable from the given digests
/// through SUBSTITUTION-ONLY paths. The `NOT EXISTS` keeps the
/// fallback per-path: a path with a `path_tenants` row for ANY tenant
/// is junction-gated only — a trusted signature must not bypass the
/// I-217 isolation policy (built-by-another ⇒ hidden), matching
/// `sig_visibility_gate`'s precedence.
const SUBST_ONLY_DIRECTORY_CANDIDATES_SQL: &str = "SELECT dp.digest, dp.store_path_hash \
       FROM directory_paths dp \
      WHERE dp.digest = ANY($1::bytea[]) \
        AND NOT EXISTS (SELECT 1 FROM path_tenants pt \
                         WHERE pt.store_path_hash = dp.store_path_hash)";

/// Sig-visibility fallback candidates over `file_blobs` (the direct
/// junction — no `directory_paths` hop). Same per-path
/// substitution-only filter as the directory variant.
const SUBST_ONLY_BLOB_CANDIDATES_SQL: &str = "SELECT f.digest, f.store_path_hash \
       FROM file_blobs f \
      WHERE f.digest = ANY($1::bytea[]) \
        AND NOT EXISTS (SELECT 1 FROM path_tenants pt \
                         WHERE pt.store_path_hash = f.store_path_hash)";

/// Sig-visibility fallback for digest-level presence: of `missing`
/// (digests the strict junction probe did NOT grant), return those
/// reachable through a substitution-only path whose narinfo signature
/// verifies against `tenant`'s trusted set — the shared predicate
/// from sign.rs, so the read surface can't drift from the validity
/// gate. Free function (not a method) so the spawned `GetDirectory`
/// BFS task can call it without `&self`.
///
/// Cost shape: only runs when the junction probe missed; candidate
/// lookup is a `(digest, store_path_hash)` PK prefix scan, narinfo
/// verification is a PK `= ANY` fetch inside
/// [`sign::sig_visible_path_hashes`].
// r[impl store.castore.tenant-scope+3]
async fn sig_fallback_digests(
    pool: &PgPool,
    signer: Option<&TenantSigner>,
    sql: &'static str,
    missing: &[[u8; 32]],
    tenant: uuid::Uuid,
) -> Result<HashSet<[u8; 32]>, Status> {
    if missing.is_empty() {
        return Ok(HashSet::new());
    }
    let slices: Vec<&[u8]> = missing.iter().map(|d| d.as_slice()).collect();
    let candidates: Vec<(Vec<u8>, Vec<u8>)> = sqlx::query_as(sql)
        .bind(&slices)
        .fetch_all(pool)
        .await
        .map_err(internal)?;
    if candidates.is_empty() {
        return Ok(HashSet::new());
    }
    let mut hashes: Vec<Vec<u8>> = candidates.iter().map(|(_, h)| h.clone()).collect();
    hashes.sort_unstable();
    hashes.dedup();
    let visible = sign::sig_visible_path_hashes(pool, signer, tenant, &hashes).await?;
    Ok(candidates
        .into_iter()
        .filter(|(_, h)| visible.contains(h))
        .filter_map(|(d, _)| d.try_into().ok())
        .collect())
}

/// `ReadBlob`/`StatBlob` arm of the sig-visibility fallback: pick the
/// substitution-only path (complete manifest) containing `digest`
/// that is sig-visible to `tenant`. Lowest `store_path_hash` wins for
/// determinism. `None` ⇒ the caller's NotFound stands.
// r[impl store.castore.tenant-scope+3]
async fn blob_sig_fallback_hash(
    pool: &PgPool,
    signer: Option<&TenantSigner>,
    digest: &[u8; 32],
    tenant: uuid::Uuid,
) -> Result<Option<Vec<u8>>, Status> {
    let candidates: Vec<(Vec<u8>,)> = sqlx::query_as(
        "SELECT f.store_path_hash FROM file_blobs f \
           JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                AND m.status = 'complete' \
          WHERE f.digest = $1 \
            AND NOT EXISTS (SELECT 1 FROM path_tenants pt \
                             WHERE pt.store_path_hash = f.store_path_hash)",
    )
    .bind(digest.as_slice())
    .fetch_all(pool)
    .await
    .map_err(internal)?;
    if candidates.is_empty() {
        return Ok(None);
    }
    let hashes: Vec<Vec<u8>> = candidates.into_iter().map(|(h,)| h).collect();
    let visible = sign::sig_visible_path_hashes(pool, signer, tenant, &hashes).await?;
    Ok(hashes.into_iter().filter(|h| visible.contains(h)).min())
}

impl DirectoryServiceImpl {
    /// Shared `HasDirectories`/`HasBlobs` body: parse digests → record
    /// the batch-size histogram → presence query (junction first, then
    /// the sig-visibility fallback for the misses) → bitmap. The two
    /// RPCs differ only in the SQL pair and the `rpc` metric label.
    async fn has_response(
        &self,
        raw: &[Vec<u8>],
        sql: &'static str,
        fallback_sql: &'static str,
        rpc: &'static str,
        tenant: uuid::Uuid,
    ) -> Result<Response<HasBitmap>, Status> {
        let digests = parse_digests(raw)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => rpc)
            .record(digests.len() as f64);
        let mut present = self.has_in(sql, &digests, tenant).await?;
        // Fallback arm only engages for digests the junction probe
        // missed — the owned-path fast path stays one query.
        let missing: Vec<[u8; 32]> = {
            let mut m: Vec<[u8; 32]> = digests
                .iter()
                .filter(|d| !present.contains(*d))
                .copied()
                .collect();
            m.sort_unstable();
            m.dedup();
            m
        };
        if !missing.is_empty() {
            present.extend(
                sig_fallback_digests(
                    &self.pool,
                    self.signer.as_deref(),
                    fallback_sql,
                    &missing,
                    tenant,
                )
                .await?,
            );
        }
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
    }

    /// `SELECT DISTINCT d.digest FROM <table×junction> WHERE digest =
    /// ANY($1) AND tenant_id = $2` — `sql` is one of
    /// [`HAS_DIRECTORIES_SQL`] / [`HAS_BLOBS_SQL`].
    async fn has_in(
        &self,
        sql: &'static str,
        digests: &[[u8; 32]],
        tenant: uuid::Uuid,
    ) -> Result<HashSet<[u8; 32]>, Status> {
        if digests.is_empty() {
            return Ok(HashSet::new());
        }
        let slices: Vec<&[u8]> = digests.iter().map(|d| d.as_slice()).collect();
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(sql)
            .bind(&slices)
            .bind(tenant)
            .fetch_all(&self.pool)
            .await
            .map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|(d,)| d.try_into().ok())
            .collect())
    }
}

/// Greedily split `(digest, size)` rows into runs whose summed `size`
/// stays under `budget`, except a single row over budget is its own
/// run (it would never fit; emit it alone rather than loop forever).
/// Used to bound the per-`fetch_all` byte sum for `GetDirectory`.
fn greedy_split_by_bytes(rows: &[(Vec<u8>, i64)], budget: u64) -> Vec<&[(Vec<u8>, i64)]> {
    let mut out: Vec<&[(Vec<u8>, i64)]> = Vec::new();
    let mut start = 0usize;
    let mut acc = 0u64;
    for (i, (_, sz)) in rows.iter().enumerate() {
        let sz = u64::try_from(*sz).unwrap_or(0);
        if i > start && acc.saturating_add(sz) > budget {
            out.push(&rows[start..i]);
            start = i;
            acc = 0;
        }
        acc = acc.saturating_add(sz);
    }
    if start < rows.len() {
        out.push(&rows[start..]);
    }
    out
}

fn internal(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "DirectoryService PG error");
    Status::internal("directory query failed")
}

fn corrupt(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "DirectoryService: corrupt directory body");
    Status::data_loss("corrupt directory body")
}

#[cfg(test)]
mod tests {
    use super::*;

    #[test]
    fn parse_digests_batch_boundary() {
        let at = vec![vec![0u8; 32]; HAS_BATCH_MAX];
        assert!(parse_digests(&at).is_ok());
        let over = vec![vec![0u8; 32]; HAS_BATCH_MAX + 1];
        let err = parse_digests(&over).unwrap_err();
        assert_eq!(err.code(), tonic::Code::InvalidArgument);
    }

    /// Per-batch byte sum is bounded; an over-budget
    /// single row is its own run rather than an infinite loop.
    #[test]
    fn greedy_split_bounds_byte_sum() {
        let row = |n: u8, sz: i64| (vec![n; 32], sz);
        // Empty.
        assert!(greedy_split_by_bytes(&[], 100).is_empty());
        // Fits in one run.
        let small = vec![row(1, 10), row(2, 10), row(3, 10)];
        let runs = greedy_split_by_bytes(&small, 100);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len(), 3);
        // Splits when exceeding budget.
        let mixed = vec![row(1, 60), row(2, 60), row(3, 60)];
        let runs = greedy_split_by_bytes(&mixed, 100);
        assert_eq!(runs.len(), 3, "each 60-byte row alone fits under 100");
        // Single over-budget row is its own run, not an infinite loop.
        let big = vec![row(1, 1000)];
        let runs = greedy_split_by_bytes(&big, 100);
        assert_eq!(runs.len(), 1);
        assert_eq!(runs[0].len(), 1);
    }

    #[test]
    fn slice_inline_end_boundary() {
        // end == len: full slice, not data_loss.
        assert_eq!(slice_inline(vec![1, 2, 3], 1, 3).unwrap().as_ref(), &[2, 3]);
        // end > len: data_loss.
        assert_eq!(
            slice_inline(vec![1, 2, 3], 0, 4).unwrap_err().code(),
            tonic::Code::DataLoss
        );
    }

    /// Dev-only sanity: the sig-visibility fallback queries must stay
    /// on index scans — the candidate probe via the `file_blobs`
    /// digest index + `path_tenants` PK anti-join, and the narinfo
    /// verification fetch via the `narinfo` PK. A seq scan on any of
    /// these would put O(table) work on the castore-FUSE open() path
    /// whenever the junction probe misses.
    ///
    /// `#[ignore]` because EXPLAIN output depends on PG's cost model
    /// (same caveat as `scan_query_uses_uploading_partial_idx` in
    /// gc/orphan.rs). Run locally with
    /// `cargo test -p rio-store -- --ignored sig_fallback_queries`.
    #[ignore = "EXPLAIN plan depends on PG cost model; dev-only sanity"]
    #[tokio::test]
    async fn sig_fallback_queries_use_indexes() {
        use rio_test_support::TestDb;
        let db = TestDb::new(&crate::MIGRATOR).await;

        // Enough rows that the cost model would reject index scans if
        // the predicates didn't line up with the indexes.
        sqlx::query(
            "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size)
             SELECT sha256(i::text::bytea),
                    '/nix/store/' || lpad(to_hex(i), 32, '0') || '-seed',
                    sha256(i::text::bytea), 0
             FROM generate_series(1, 2000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status)
             SELECT sha256(i::text::bytea), 'complete'
             FROM generate_series(1, 2000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query(
            "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size)
             SELECT sha256(('blob' || i)::text::bytea), sha256(i::text::bytea), 0, 0
             FROM generate_series(1, 2000) AS i",
        )
        .execute(&db.pool)
        .await
        .unwrap();
        // Populate path_tenants too — against an empty table PG
        // (correctly) seq-scans 0 rows, which would vacuously pass the
        // anti-join assertion below.
        let tid = crate::test_helpers::seed_tenant(&db.pool, "explain-sanity").await;
        sqlx::query(
            "INSERT INTO path_tenants (store_path_hash, tenant_id)
             SELECT sha256(i::text::bytea), $1 FROM generate_series(1, 2000) AS i",
        )
        .bind(tid)
        .execute(&db.pool)
        .await
        .unwrap();
        sqlx::query("ANALYZE narinfo, manifests, file_blobs, path_tenants")
            .execute(&db.pool)
            .await
            .unwrap();

        let explain = |sql: String| {
            let pool = db.pool.clone();
            async move {
                let digests: Vec<&[u8]> = vec![&[0u8; 32]];
                // AssertSqlSafe: const SQL + an "EXPLAIN" prefix, no
                // runtime data in the string.
                let plan: Vec<(String,)> = sqlx::query_as(sqlx::AssertSqlSafe(sql))
                    .bind(&digests)
                    .fetch_all(&pool)
                    .await
                    .unwrap();
                plan.into_iter()
                    .map(|(l,)| l)
                    .collect::<Vec<_>>()
                    .join("\n")
            }
        };

        for (label, sql) in [
            (
                "blob candidates",
                format!("EXPLAIN (FORMAT TEXT) {SUBST_ONLY_BLOB_CANDIDATES_SQL}"),
            ),
            (
                "directory candidates",
                format!("EXPLAIN (FORMAT TEXT) {SUBST_ONLY_DIRECTORY_CANDIDATES_SQL}"),
            ),
            (
                "narinfo verification",
                "EXPLAIN (FORMAT TEXT) SELECT store_path_hash FROM narinfo \
                 WHERE store_path_hash = ANY($1::bytea[])"
                    .to_string(),
            ),
        ] {
            let joined = explain(sql).await;
            eprintln!("{label} plan:\n{joined}\n");
            for table in ["narinfo", "file_blobs", "directory_paths", "path_tenants"] {
                assert!(
                    !joined.contains(&format!("Seq Scan on {table}")),
                    "{label}: plan should NOT seq-scan {table}; got:\n{joined}"
                );
            }
        }
    }
}
