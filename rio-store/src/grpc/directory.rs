//! ADR-022 castore RPC surface (P0573 / P0577 / P0570 / P0591).
//!
//! `GetDirectory` / `HasDirectories` / `HasBlobs` / `ReadBlob` /
//! `StatBlob` / `PresentClosure`. All tenant-scoped: every query
//! resolves a digest to the store path(s) that contain it
//! (`directory_paths` / `file_blobs.store_path_hash`) and joins
//! `path_tenants` on the caller's `tenant_id`, so a digest the tenant
//! didn't produce is invisible (NotFound, or absent from the bitmap).
//! Directory bodies leak child names/digests — confidentiality, not
//! just isolation.
//!
//! Assignment-token callers are additionally **closure-scoped**
//! (P0591, [`crate::grpc::scope`]): the membership predicate
//! ([`SCOPE_PREDICATE`]) is ANDed with the tenant join, so a build's
//! token reads exactly the input closure the scheduler signed for it —
//! JWT (gateway/user) callers keep tenant-wide reads, and the
//! digest-keyed chunk RPCs are untouched.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

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
    PresentClosureRequest, PresentClosureResponse, ReadBlobRequest, StatBlobRequest,
    StatBlobResponse,
};

use crate::cas::ChunkCache;
use crate::grpc::scope::{CastoreScope, ReadScope, ScopeSet};

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
const GET_DIRECTORY_MAX_RESULTS: usize = 262_144;

/// Cap on the request digest list for `HasDirectories`/`HasBlobs`.
/// Both are single-RPC presence probes — a delta-sync caller batches
/// at the closure size (~25k files for chromium); larger means a
/// pathological caller or a bug.
const HAS_BATCH_MAX: usize = 65_536;

/// `ReadBlob` chunk prefetch width. `buffered()`, not `_unordered`:
/// file bytes must arrive in offset order. Lower than `GetPath`'s 64
/// because a single-file read pulls fewer chunks and the FUSE caller
/// is rarely throughput-bound.
const READ_BLOB_PREFETCH_K: usize = 8;

/// `ReadBlob` response channel depth. Same rationale as
/// [`GET_DIRECTORY_CHANNEL_DEPTH`].
const READ_BLOB_CHANNEL_DEPTH: usize = 4;

/// Shared closure-scope membership predicate, appended (with the
/// existing `WHERE … tenant_id = $2` intact) to every castore read
/// query. `$3` is the scope bind: `NULL` for unscoped callers (JWT,
/// `mode = off|log`) — the predicate then passes — or the sorted
/// `store_path_hash` array of the caller's attested closure under
/// enforce. One fragment, one bind position, all eight query sites.
// r[impl store.castore.closure-scope]
const SCOPE_PREDICATE: &str = " AND ($3::bytea[] IS NULL OR pt.store_path_hash = ANY($3::bytea[]))";

/// Authorization result for one castore read: the caller's tenant plus
/// the closure-scope decision the query sites must consume. Returned by
/// [`DirectoryServiceImpl::castore_authz`] so a new query site cannot
/// compile without deciding what to do with the scope.
struct CastoreAuthz {
    tenant: uuid::Uuid,
    scope: ReadScope,
}

pub struct DirectoryServiceImpl {
    pool: PgPool,
    /// HMAC verifier for assignment tokens. Same key the dispatch
    /// signer uses (NOT the service-token key). The builder presents
    /// `x-rio-assignment-token`; `claims.tenant` carries the tenant.
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
    /// Shared with `StoreServiceImpl`/`ChunkServiceImpl`; `ReadBlob`
    /// fetches file bytes through it. Always present: a chunk backend
    /// is a required part of store config.
    chunk_cache: Arc<ChunkCache>,
    /// Terminal-build revocation probe for assignment-token callers
    /// (`r[store.castore.terminal-revocation]`). `None` = revocation
    /// disabled (`assignment_revocation.enabled = false`, or a test
    /// fixture that doesn't wire it). JWT callers are never probed.
    revocation: Option<crate::revocation::BuildTerminalProbe>,
    /// Closure read scope state (`[castore_read_scope]`,
    /// `r[store.castore.closure-scope]`). Constructed `mode = off` by
    /// default so fixtures keep tenant-only behavior; main.rs replaces
    /// it from config via [`Self::with_castore_scope`].
    scope: Arc<CastoreScope>,
}

impl DirectoryServiceImpl {
    pub fn new(
        pool: PgPool,
        hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
        chunk_cache: Arc<ChunkCache>,
    ) -> Self {
        Self {
            pool,
            hmac_verifier,
            chunk_cache,
            revocation: None,
            scope: Arc::new(CastoreScope::disabled()),
        }
    }

    /// Enable terminal-build revocation for assignment-token callers
    /// (main.rs wires this from `[assignment_revocation]` config).
    pub fn with_revocation(mut self, probe: crate::revocation::BuildTerminalProbe) -> Self {
        self.revocation = Some(probe);
        self
    }

    /// Set the closure read-scope state (main.rs wires this from
    /// `[castore_read_scope]` config; tests construct the mode they
    /// exercise).
    pub fn with_castore_scope(mut self, scope: CastoreScope) -> Self {
        self.scope = Arc::new(scope);
        self
    }

    /// Verify the `x-rio-assignment-token` header, if present and a
    /// verifier is configured. `Ok(None)` means "no assignment token in
    /// play" (JWT callers, dev mode); errors are deliberately opaque
    /// (sig-vs-expiry is an oracle; the legitimate caller's fix is the
    /// same).
    fn assignment_claims<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_auth::hmac::AssignmentClaims>, Status> {
        let tok = request
            .metadata()
            .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        match (&self.hmac_verifier, tok) {
            (Some(verifier), Some(tok)) => verifier
                .verify::<rio_auth::hmac::AssignmentClaims>(tok)
                .map(Some)
                .map_err(|_| Status::unauthenticated("assignment token rejected")),
            _ => Ok(None),
        }
    }

    /// Resolve the caller's authorization: JWT extension (gateway path,
    /// tenant-wide by design), else HMAC assignment-token
    /// `claims.tenant` (builder path, additionally closure-scoped). No
    /// tenant → `UNAUTHENTICATED`; never fall back to anonymous.
    ///
    /// Assignment-token callers keep the existing check order: HMAC
    /// signature/expiry → terminal-build revocation probe → closure
    /// scope resolution. A verified token whose build already reached a
    /// terminal state is rejected with `PERMISSION_DENIED` before any
    /// data-plane query runs; scope resolution never widens what the
    /// tenant join alone would have allowed.
    // r[impl store.castore.tenant-scope+2]
    // r[impl store.castore.closure-scope]
    async fn castore_authz<T>(&self, request: &Request<T>) -> Result<CastoreAuthz, Status> {
        if let Some(jwt) = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub)
        {
            // JWT (user/gateway) callers keep tenant-wide reads.
            return Ok(CastoreAuthz {
                tenant: jwt,
                scope: ReadScope::Unscoped,
            });
        }
        if let Some(claims) = self.assignment_claims(request)? {
            // r[impl store.castore.terminal-revocation]
            // Signature + expiry already verified; now check the
            // build is still live. Terminal ⇒ the token's only
            // legitimate holder is gone — reject like any other
            // authorization failure, without saying why (the
            // legitimate caller no longer exists; an attacker
            // gets no oracle beyond "rejected").
            if let Some(probe) = &self.revocation
                && probe.is_terminal(&self.pool, &claims.drv_hash).await
            {
                metrics::counter!("rio_store_castore_terminal_rejected_total").increment(1);
                warn!(
                    drv_hash = %claims.drv_hash,
                    executor_id = %claims.executor_id,
                    "castore read with an assignment token for a terminal build"
                );
                return Err(Status::permission_denied("assignment token rejected"));
            }
            let tenant = match claims.tenant.as_deref() {
                None => {
                    return Err(Status::unauthenticated(
                        "assignment token has no tenant claim",
                    ));
                }
                Some(t) => t.parse().map_err(|_| {
                    Status::unauthenticated("assignment token tenant is not a UUID")
                })?,
            };
            let scope = self.scope.resolve(&claims, &self.pool).await?;
            return Ok(CastoreAuthz { tenant, scope });
        }
        Err(Status::unauthenticated(
            "DirectoryService requires a tenant: send a JWT or an HMAC \
             assignment token",
        ))
    }

    /// Post-query closure-scope accounting for the single-digest read
    /// RPCs. Never changes the response — the wire status for an
    /// out-of-scope digest is the same `NOT_FOUND` an absent digest
    /// gets; this is where the deny metric/log learn the real reason.
    ///
    /// - `Audit` (log mode) + row served: probe membership and count a
    ///   `would_deny` if every containing path is outside the scope.
    /// - `Enforce` + no row: probe WITHOUT the scope to distinguish
    ///   "absent / not tenant-visible" from "denied by closure scope".
    ///   (The probe ignores `manifests.status`, so an in-flight upload
    ///   can over-count a deny — metrics-only, accepted.)
    async fn scope_audit_single(
        &self,
        rpc: &'static str,
        table: HasTable,
        digest: [u8; 32],
        tenant: uuid::Uuid,
        scope: &ReadScope,
        found: bool,
    ) -> Result<(), Status> {
        match scope {
            ReadScope::Audit(set, ctx) if found => {
                let in_scope = self
                    .has_in(table, &[digest], tenant, Some(set.as_ref()))
                    .await?;
                if !in_scope.contains(&digest) {
                    self.scope.record_out_of_scope(false, rpc, ctx, &digest);
                }
            }
            ReadScope::Enforce(_, ctx) if !found => {
                let visible = self.has_in(table, &[digest], tenant, None).await?;
                if visible.contains(&digest) {
                    self.scope.record_out_of_scope(true, rpc, ctx, &digest);
                }
            }
            _ => {}
        }
        Ok(())
    }
}

fn parse_digest(d: &[u8]) -> Result<[u8; 32], Status> {
    d.try_into()
        .map_err(|_| Status::invalid_argument(format!("digest must be 32 bytes, got {}", d.len())))
}

fn parse_digests(ds: &[Vec<u8>]) -> Result<Vec<[u8; 32]>, Status> {
    if ds.len() > HAS_BATCH_MAX {
        return Err(Status::invalid_argument(format!(
            "digest batch too large: {} > {HAS_BATCH_MAX}",
            ds.len()
        )));
    }
    ds.iter().map(|d| parse_digest(d)).collect()
}

/// Bit `i` set ⇔ `requested[i]` ∈ `present`. LSB-first within each
/// byte; trailing bits zeroed.
fn build_bitmap(requested: &[[u8; 32]], present: &HashSet<[u8; 32]>) -> Vec<u8> {
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
        let authz = self.castore_authz(&request).await?;
        let tenant = authz.tenant;
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
            // AssertSqlSafe: the only formatted-in piece is the
            // compile-time SCOPE_PREDICATE fragment; digest, tenant and
            // scope are bind parameters.
            let body: Option<(Vec<u8>,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT d.body FROM directories d \
                  JOIN directory_paths dp ON dp.digest = d.digest \
                  JOIN path_tenants pt ON pt.store_path_hash = dp.store_path_hash \
                 WHERE d.digest = $1 AND pt.tenant_id = $2{SCOPE_PREDICATE} \
                 LIMIT 1",
            )))
            .bind(digest.as_slice())
            .bind(tenant)
            .bind(authz.scope.sql_bind())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
            self.scope_audit_single(
                "GetDirectory",
                HasTable::Directories,
                digest,
                tenant,
                &authz.scope,
                body.is_some(),
            )
            .await?;
            let Some((body,)) = body else {
                return Err(Status::not_found("directory not found"));
            };
            let dir = Directory::decode(body.as_slice()).map_err(corrupt)?;
            let (tx, rx) = tokio::sync::mpsc::channel(2);
            let _ = tx.send(Ok(dir)).await;
            return Ok(Response::new(ReceiverStream::new(rx)));
        }

        // Closure scope for the recursive walk applies to the seed
        // frontier only: children discovered during the descent belong
        // to the same containing store path as their authorized parent
        // (junction rows are written for every interior digest at
        // commit), so they inherit scope by containment and the BFS
        // below keeps today's per-batch tenant-only join.
        match &authz.scope {
            ReadScope::Unscoped => {}
            ReadScope::Enforce(set, ctx) => {
                let in_scope = self
                    .has_in(HasTable::Directories, &frontier, tenant, Some(set.as_ref()))
                    .await?;
                let visible = self
                    .has_in(HasTable::Directories, &frontier, tenant, None)
                    .await?;
                let denied = visible.iter().filter(|d| !in_scope.contains(*d)).count();
                if denied > 0 {
                    self.scope
                        .record_out_of_scope_batch(true, "GetDirectory", ctx, denied);
                }
                // Out-of-scope seeds are simply absent from the stream —
                // the same silent skip a non-tenant-visible seed gets.
                frontier.retain(|d| in_scope.contains(d));
            }
            ReadScope::Audit(set, ctx) => {
                let in_scope = self
                    .has_in(HasTable::Directories, &frontier, tenant, Some(set.as_ref()))
                    .await?;
                let visible = self
                    .has_in(HasTable::Directories, &frontier, tenant, None)
                    .await?;
                let denied = visible.iter().filter(|d| !in_scope.contains(*d)).count();
                if denied > 0 {
                    self.scope
                        .record_out_of_scope_batch(false, "GetDirectory", ctx, denied);
                }
            }
        }

        // Recursive BFS, spawned so the stream can outlive this call.
        // No drain guard (unlike get_path): a SIGTERM RST-closing this
        // mid-stream is fine — the FUSE prefetch retries from where it
        // left off via the deduped `seen` set on its side.
        let pool = self.pool.clone();
        let (tx, rx) = tokio::sync::mpsc::channel(GET_DIRECTORY_CHANNEL_DEPTH);
        rio_common::task::spawn_monitored("get-directory-bfs", async move {
            let started = Instant::now();
            // `seen` dedups both seeds and discovered children. Rebuild
            // the frontier from it so a duplicate seed split across two
            // SQL chunks isn't streamed twice.
            let mut seen: HashSet<[u8; 32]> = frontier.iter().copied().collect();
            let mut frontier: Vec<[u8; 32]> = seen.iter().copied().collect();
            let mut sent = 0usize;
            // Like get_path: a stalled client otherwise pins this task
            // (and the buffered Directory bodies) forever.
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
                        let sizes: Vec<SizeRow> = match sqlx::query_as(
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
                        for sub in greedy_split_by_bytes(&sizes, GET_DIRECTORY_BATCH_BYTES) {
                            let sub_digests: Vec<&[u8]> =
                                sub.iter().map(|(d, _)| d.as_slice()).collect();
                            let rows: Vec<(Vec<u8>,)> = match sqlx::query_as(
                                "SELECT DISTINCT ON (d.digest) d.body FROM directories d \
                              JOIN directory_paths dp ON dp.digest = d.digest \
                              JOIN path_tenants pt ON pt.store_path_hash = dp.store_path_hash \
                             WHERE d.digest = ANY($1::bytea[]) AND pt.tenant_id = $2 \
                             ORDER BY d.digest",
                            )
                            .bind(&sub_digests)
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
            if tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, walk)
                .await
                .is_err()
            {
                warn!(
                    timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                    sent, "GetDirectory stream timed out"
                );
                let _ = tx
                    .send(Err(Status::deadline_exceeded("stream timeout")))
                    .await;
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    /// A bit is set iff the digest is present AND tenant-visible AND —
    /// for enforce-mode assignment-token callers — contained in a path
    /// of the attested closure (`r[store.castore.closure-scope]`,
    /// uniform with the other read RPCs so presence can't be used as a
    /// scope oracle). The gateway's delta-sync calls this with a JWT
    /// and is unaffected by the mode.
    #[instrument(skip(self, request), fields(rpc = "HasDirectories"))]
    async fn has_directories(
        &self,
        request: Request<HasDirectoriesRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let authz = self.castore_authz(&request).await?;
        let digests = parse_digests(&request.into_inner().digests)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasDirectories")
            .record(digests.len() as f64);
        let present = self
            .has_scoped(HasTable::Directories, "HasDirectories", &digests, &authz)
            .await?;
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
    }

    /// Presence bitmap over `file_blobs` × `path_tenants`. The join is
    /// per-path (`store_path_hash`), so a digest is "present" iff at
    /// least one tenant-readable NAR contains it — GC of one referrer
    /// can't dangle the answer. Closure-scoped for enforce-mode
    /// assignment-token callers, same as `HasDirectories`.
    #[instrument(skip(self, request), fields(rpc = "HasBlobs"))]
    async fn has_blobs(
        &self,
        request: Request<HasBlobsRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let authz = self.castore_authz(&request).await?;
        let digests = parse_digests(&request.into_inner().digests)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasBlobs")
            .record(digests.len() as f64);
        let present = self
            .has_scoped(HasTable::FileBlobs, "HasBlobs", &digests, &authz)
            .await?;
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
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
        let authz = self.castore_authz(&request).await?;
        let tenant = authz.tenant;
        let digest = parse_digest(&request.into_inner().file_digest)?;
        let started = Instant::now();

        // `manifests.status` filter excludes 'uploading' placeholders.
        // LIMIT 1: any tenant-visible (and, under enforce, in-closure)
        // referrer's NAR works; the bytes are content-addressed. The
        // `path_tenants` join is on the same `store_path_hash` as
        // `file_blobs`, so the chosen NAR is one this tenant may read —
        // a digest-only join could pick another tenant's NAR for a
        // content-shared file; the scope predicate rides the same join,
        // so a digest shared with an out-of-closure path still resolves
        // through the in-closure referrer. `file_blobs.size` (M_063) is
        // denormalized so this never decodes `nar_index.entries`
        // (O(files-in-NAR)) on the FUSE `open()` fast path.
        type BlobRow = (i64, i64, Option<Vec<u8>>);
        let row: Option<BlobRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT f.nar_offset, f.size, md.chunk_list \
               FROM file_blobs f \
               JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
               JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                    AND m.status = 'complete' \
               LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
              WHERE f.digest = $1 AND pt.tenant_id = $2{SCOPE_PREDICATE} \
              LIMIT 1",
        )))
        .bind(digest.as_slice())
        .bind(tenant)
        .bind(authz.scope.sql_bind())
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        self.scope_audit_single(
            "ReadBlob",
            HasTable::FileBlobs,
            digest,
            tenant,
            &authz.scope,
            row.is_some(),
        )
        .await?;
        let Some((nar_offset, file_size, chunk_list)) = row else {
            return Err(Status::not_found("file blob not found"));
        };
        let (nar_offset, end) = nar_window(nar_offset, file_size)?;

        // Same invariant `metadata::get_manifest` enforces: a complete
        // manifest has a manifest_data row. Surface its absence as
        // DATA_LOSS, not NotFound, so corruption (or a pre-065 inline
        // path) isn't mistaken for a cache miss.
        let Some(chunk_list) = chunk_list else {
            return Err(Status::data_loss(
                "manifest has no manifest_data row (content unreadable)",
            ));
        };
        let plan = build_chunk_plan(&chunk_list, nar_offset, end)?;
        let cache = Arc::clone(&self.chunk_cache);

        let (tx, rx) = tokio::sync::mpsc::channel(READ_BLOB_CHANNEL_DEPTH);
        rio_common::task::spawn_monitored("read-blob-stream", async move {
            let stream_fut = stream_blob(&tx, cache, plan, digest);
            match tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, stream_fut).await {
                Err(_) => {
                    warn!(
                        timeout = ?rio_common::grpc::GRPC_STREAM_TIMEOUT,
                        file_size = end - nar_offset,
                        "ReadBlob stream timed out"
                    );
                    let _ = tx
                        .send(Err(Status::deadline_exceeded("stream timeout")))
                        .await;
                }
                // Record only on a clean stream so DATA_LOSS and
                // disconnect timings don't skew the histogram. Same
                // policy as get_directory and get_path.
                Ok(true) => {
                    metrics::histogram!("rio_store_directory_read_seconds")
                        .record(started.elapsed().as_secs_f64());
                }
                Ok(false) => {}
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
        let authz = self.castore_authz(&request).await?;
        let tenant = authz.tenant;
        let req = request.into_inner();
        let digest = parse_digest(&req.file_digest)?;

        // Probe before the chunk-list query: `md.chunk_list` is TOASTed
        // (megabytes for a large NAR) and the probe never reads it.
        // `has_in()` would also do, but it skips `m.status = 'complete'`.
        if !req.send_chunks {
            let exists: Option<(i32,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
                "SELECT 1 FROM file_blobs f \
                   JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
                   JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                        AND m.status = 'complete' \
                  WHERE f.digest = $1 AND pt.tenant_id = $2{SCOPE_PREDICATE} LIMIT 1",
            )))
            .bind(digest.as_slice())
            .bind(tenant)
            .bind(authz.scope.sql_bind())
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
            self.scope_audit_single(
                "StatBlob",
                HasTable::FileBlobs,
                digest,
                tenant,
                &authz.scope,
                exists.is_some(),
            )
            .await?;
            return exists
                .map(|_| Response::new(StatBlobResponse::default()))
                .ok_or_else(|| Status::not_found("file blob not found"));
        }

        type StatRow = (i64, i64, Option<Vec<u8>>);
        let row: Option<StatRow> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT f.nar_offset, f.size, md.chunk_list \
               FROM file_blobs f \
               JOIN path_tenants pt ON pt.store_path_hash = f.store_path_hash \
               JOIN manifests m ON m.store_path_hash = f.store_path_hash \
                    AND m.status = 'complete' \
               LEFT JOIN manifest_data md ON md.store_path_hash = f.store_path_hash \
              WHERE f.digest = $1 AND pt.tenant_id = $2{SCOPE_PREDICATE} LIMIT 1",
        )))
        .bind(digest.as_slice())
        .bind(tenant)
        .bind(authz.scope.sql_bind())
        .fetch_optional(&self.pool)
        .await
        .map_err(internal)?;
        self.scope_audit_single(
            "StatBlob",
            HasTable::FileBlobs,
            digest,
            tenant,
            &authz.scope,
            row.is_some(),
        )
        .await?;
        let Some((nar_offset, file_size, chunk_list)) = row else {
            return Err(Status::not_found("file blob not found"));
        };
        // TODO: store.proto documents FailedPrecondition for a manifest
        // with no chunk list ("inline manifest — use ReadBlob"), but this
        // returns DATA_LOSS like read_blob does — post-P0583 every
        // manifest is chunked, so a missing manifest_data row here is
        // corruption, not a legacy shape. The builder's StatBlob→ReadBlob
        // fallback keys on FailedPrecondition and therefore stays dormant
        // until a store-side decision either changes this status or
        // updates the proto doc (and retires the dormant fallback).
        let Some(chunk_list) = chunk_list else {
            return Err(Status::data_loss(
                "manifest has no manifest_data row (content unreadable)",
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

    /// Establish the closure read scope for the presenting assignment
    /// token (ADR-022 P0591): verify the presented list against the
    /// token's signed `input_closure_digest` and cache the resulting
    /// ScopeSet on this replica. Idempotent, no Postgres; works in
    /// every `[castore_read_scope]` mode so a presenting builder never
    /// has to care what the store enforces.
    ///
    /// JWT callers have nothing to present (their reads are tenant-wide
    /// by design) — an assignment token is required.
    // r[impl store.castore.scope-establish]
    #[instrument(skip(self, request), fields(rpc = "PresentClosure"))]
    async fn present_closure(
        &self,
        request: Request<PresentClosureRequest>,
    ) -> Result<Response<PresentClosureResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let Some(claims) = self.assignment_claims(&request)? else {
            return Err(Status::unauthenticated(
                "PresentClosure requires an HMAC assignment token",
            ));
        };
        let closure = request.into_inner().closure;
        self.scope.establish(&claims, &closure).await?;
        Ok(Response::new(PresentClosureResponse {}))
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

/// One chunk of a `ReadBlob`/`StatBlob` fetch plan: chunk hashes in NAR
/// order with the byte range to emit from each. Pre-slicing every chunk
/// (not just first/last) keeps the stream loop branch-free.
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

/// Drive the `ReadBlob` body stream: K-parallel ordered prefetch via
/// `cache.get_verified()`.
///
/// Returns `true` only on a clean stream with the body matching
/// `file_digest`, so the caller can gate the latency histogram.
async fn stream_blob(
    tx: &FrameTx,
    cache: Arc<ChunkCache>,
    slices: Vec<ChunkSlice>,
    file_digest: [u8; 32],
) -> bool {
    let mut hasher = blake3::Hasher::new();
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

/// Closed enum of `has_in` join targets. Keeps the table+junction
/// fragment off the request path so the `format!` inside `has_in` can
/// never become an injection sink under refactoring.
#[derive(Clone, Copy)]
enum HasTable {
    Directories,
    FileBlobs,
}

impl HasTable {
    const fn join_clause(self) -> &'static str {
        match self {
            Self::Directories => {
                "directories d \
                 JOIN directory_paths dp ON dp.digest = d.digest \
                 JOIN path_tenants pt ON pt.store_path_hash = dp.store_path_hash"
            }
            Self::FileBlobs => {
                "file_blobs d JOIN path_tenants pt ON pt.store_path_hash = d.store_path_hash"
            }
        }
    }
}

impl DirectoryServiceImpl {
    /// `SELECT DISTINCT d.digest FROM <table×junction> WHERE digest =
    /// ANY($1) AND tenant_id = $2 [AND in-scope]`.
    ///
    /// `scope = Some(set)` additionally requires a containing path in
    /// the closure scope (the same [`SCOPE_PREDICATE`] the read RPCs
    /// append); `None` binds SQL NULL and the predicate passes.
    async fn has_in(
        &self,
        table: HasTable,
        digests: &[[u8; 32]],
        tenant: uuid::Uuid,
        scope: Option<&ScopeSet>,
    ) -> Result<HashSet<[u8; 32]>, Status> {
        if digests.is_empty() {
            return Ok(HashSet::new());
        }
        let slices: Vec<&[u8]> = digests.iter().map(|d| d.as_slice()).collect();
        let scope_bind: Option<Vec<&[u8]>> =
            scope.map(|s| s.hashes().iter().map(|h| h.as_slice()).collect());
        // AssertSqlSafe: `from` is a `&'static str` from the closed
        // `HasTable` enum above and the scope fragment is the
        // compile-time SCOPE_PREDICATE const — no request-derived data
        // reaches the format string; the digests, tenant id, and scope
        // hashes are bind parameters.
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(sqlx::AssertSqlSafe(format!(
            "SELECT DISTINCT d.digest FROM {from} \
             WHERE d.digest = ANY($1::bytea[]) AND pt.tenant_id = $2{SCOPE_PREDICATE}",
            from = table.join_clause(),
        )))
        .bind(&slices)
        .bind(tenant)
        .bind(scope_bind)
        .fetch_all(&self.pool)
        .await
        .map_err(internal)?;
        Ok(rows
            .into_iter()
            .filter_map(|(d,)| d.try_into().ok())
            .collect())
    }

    /// `HasDirectories`/`HasBlobs` presence set under the caller's
    /// scope decision: enforce filters the bitmap (a presence bit means
    /// present AND tenant-visible AND in-closure); log mode serves
    /// today's tenant-wide bitmap while counting how many bits enforce
    /// would have cleared; JWT/off callers are exactly today's query.
    async fn has_scoped(
        &self,
        table: HasTable,
        rpc: &'static str,
        digests: &[[u8; 32]],
        authz: &CastoreAuthz,
    ) -> Result<HashSet<[u8; 32]>, Status> {
        match &authz.scope {
            ReadScope::Unscoped => self.has_in(table, digests, authz.tenant, None).await,
            ReadScope::Enforce(set, _) => {
                self.has_in(table, digests, authz.tenant, Some(set.as_ref()))
                    .await
            }
            ReadScope::Audit(set, ctx) => {
                let all = self.has_in(table, digests, authz.tenant, None).await?;
                let scoped = self
                    .has_in(table, digests, authz.tenant, Some(set.as_ref()))
                    .await?;
                let denied = all.iter().filter(|d| !scoped.contains(*d)).count();
                if denied > 0 {
                    self.scope
                        .record_out_of_scope_batch(false, rpc, ctx, denied);
                }
                Ok(all)
            }
        }
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
}
