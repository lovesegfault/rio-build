//! ADR-022 castore RPC surface (P0573).
//!
//! `GetDirectory` / `HasDirectories` / `HasBlobs`. All tenant-scoped:
//! every query joins `directory_tenants` / `file_blob_tenants` on the
//! caller's `tenant_id` so a digest the tenant didn't produce is
//! invisible (NotFound, or absent from the bitmap). Directory bodies
//! leak child names/digests — confidentiality, not just isolation.

use std::collections::HashSet;
use std::sync::Arc;
use std::time::Instant;

use prost::Message;
use sqlx::PgPool;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{instrument, warn};

use rio_proto::DirectoryService;
use rio_proto::castore::Directory;
use rio_proto::types::{GetDirectoryRequest, HasBitmap, HasBlobsRequest, HasDirectoriesRequest};

/// BFS frontier batch cap. ~33 round trips for a chromium-scale
/// closure (8k dirs); matches PG's parameter limit headroom.
const GET_DIRECTORY_BATCH: usize = 256;

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

pub struct DirectoryServiceImpl {
    pool: PgPool,
    /// HMAC verifier for assignment tokens. Same key the dispatch
    /// signer uses (NOT the service-token key). The builder presents
    /// `x-rio-assignment-token`; `claims.tenant` carries the tenant.
    hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>,
}

impl DirectoryServiceImpl {
    pub fn new(pool: PgPool, hmac_verifier: Option<Arc<rio_auth::hmac::HmacVerifier>>) -> Self {
        Self {
            pool,
            hmac_verifier,
        }
    }

    /// Resolve the caller's tenant: JWT extension (gateway path),
    /// else HMAC assignment-token `claims.tenant` (builder path). No
    /// tenant → `UNAUTHENTICATED`; never fall back to anonymous.
    // r[impl store.castore.tenant-scope]
    fn castore_tenant_id<T>(&self, request: &Request<T>) -> Result<uuid::Uuid, Status> {
        if let Some(jwt) = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub)
        {
            return Ok(jwt);
        }
        let tok = request
            .metadata()
            .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());
        if let (Some(verifier), Some(tok)) = (&self.hmac_verifier, tok) {
            return match verifier.verify::<rio_auth::hmac::AssignmentClaims>(tok) {
                Ok(claims) => match claims.tenant.as_deref() {
                    None => Err(Status::unauthenticated(
                        "assignment token has no tenant claim",
                    )),
                    Some(t) => t.parse().map_err(|_| {
                        Status::unauthenticated("assignment token tenant is not a UUID")
                    }),
                },
                // Don't echo why the token failed — sig-vs-expiry is an
                // oracle. The legitimate caller's fix is the same.
                Err(_) => Err(Status::unauthenticated("assignment token rejected")),
            };
        }
        Err(Status::unauthenticated(
            "DirectoryService requires a tenant: send a JWT or an HMAC \
             assignment token",
        ))
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
            let body: Option<(Vec<u8>,)> = sqlx::query_as(
                "SELECT d.body FROM directories d \
                  JOIN directory_tenants t ON t.digest = d.digest \
                 WHERE d.digest = $1 AND t.tenant_id = $2",
            )
            .bind(digest.as_slice())
            .bind(tenant)
            .fetch_optional(&self.pool)
            .await
            .map_err(internal)?;
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
                        let rows: Vec<(Vec<u8>,)> = match sqlx::query_as(
                            "SELECT d.body FROM directories d \
                              JOIN directory_tenants t ON t.digest = d.digest \
                             WHERE d.digest = ANY($1::bytea[]) AND t.tenant_id = $2",
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

    #[instrument(skip(self, request), fields(rpc = "HasDirectories"))]
    async fn has_directories(
        &self,
        request: Request<HasDirectoriesRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        let digests = parse_digests(&request.into_inner().digests)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasDirectories")
            .record(digests.len() as f64);
        let present = self.has_in(HasTable::Directories, &digests, tenant).await?;
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
    }

    /// Presence bitmap over `file_blobs` × `file_blob_tenants`. JOINs
    /// `file_blobs` (not just the tenant junction) so a junction row
    /// orphaned by a GC race doesn't read as "present".
    #[instrument(skip(self, request), fields(rpc = "HasBlobs"))]
    async fn has_blobs(
        &self,
        request: Request<HasBlobsRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let tenant = self.castore_tenant_id(&request)?;
        let digests = parse_digests(&request.into_inner().digests)?;
        metrics::histogram!("rio_store_directory_has_batch_size", "rpc" => "HasBlobs")
            .record(digests.len() as f64);
        let present = self.has_in(HasTable::FileBlobs, &digests, tenant).await?;
        Ok(Response::new(HasBitmap {
            bitmap: build_bitmap(&digests, &present),
        }))
    }
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
            Self::Directories => "directories d JOIN directory_tenants t ON t.digest = d.digest",
            Self::FileBlobs => "file_blobs d JOIN file_blob_tenants t ON t.digest = d.digest",
        }
    }
}

impl DirectoryServiceImpl {
    /// `SELECT DISTINCT d.digest FROM <table×junction> WHERE digest =
    /// ANY($1) AND tenant_id = $2`.
    async fn has_in(
        &self,
        table: HasTable,
        digests: &[[u8; 32]],
        tenant: uuid::Uuid,
    ) -> Result<HashSet<[u8; 32]>, Status> {
        if digests.is_empty() {
            return Ok(HashSet::new());
        }
        let slices: Vec<&[u8]> = digests.iter().map(|d| d.as_slice()).collect();
        let rows: Vec<(Vec<u8>,)> = sqlx::query_as(&format!(
            "SELECT DISTINCT d.digest FROM {from} \
             WHERE d.digest = ANY($1::bytea[]) AND t.tenant_id = $2",
            from = table.join_clause(),
        ))
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

fn internal(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "DirectoryService PG error");
    Status::internal("directory query failed")
}

fn corrupt(e: impl std::fmt::Display) -> Status {
    warn!(error = %e, "DirectoryService: corrupt directory body");
    Status::data_loss("corrupt directory body")
}
