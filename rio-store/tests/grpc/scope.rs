//! Closure-scoped castore reads (`r[store.castore.closure-scope]`,
//! `r[store.castore.scope-establish]`, ADR-022 P0591) — gRPC-level
//! allow/deny matrix.
//!
//! Two builds of the SAME tenant with disjoint input closures: build
//! A's assignment token may read exactly A's closure. Out-of-scope
//! digests answer the same `NOT_FOUND` an absent digest gets (no
//! existence oracle); the deny metrics carry the real reason. JWT
//! callers (the gateway delta-sync path) stay tenant-wide, the chunk
//! RPCs stay digest-keyed, `log` mode serves while counting
//! would-denies, and a never-presented replica is carried by the
//! pins+references derivation fallback.

use super::*;

use bytes::Bytes;
use prost::Message as _;
use tokio_stream::StreamExt as _;
use tonic::Status;

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier, TokenRole};
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry};
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{
    GetChunkRequest, GetDirectoryRequest, HasBlobsRequest, HasDirectoriesRequest,
    PresentClosureRequest, ReadBlobRequest, StatBlobRequest, get_directory_request,
};
use rio_proto::{ChunkServiceServer, DirectoryServiceServer};
use rio_store::backend::MemoryChunkBackend;
use rio_store::cas::ChunkCache;
use rio_store::grpc::scope::{CastoreScope, ScopeMode};
use rio_store::grpc::{ChunkServiceImpl, DirectoryServiceImpl};
use rio_store::manifest::{Manifest, ManifestEntry};
use rio_store::test_helpers::{path_hash, seed_tenant};
use rio_test_support::fixtures::test_store_path;
use rio_test_support::metrics::CountingRecorder;

const SCOPE_KEY: &[u8] = b"closure-scope-test-key-32-bytes!";

/// One seeded "build input" path with production keying: every junction
/// row is keyed by `sha256(full store path string)` — exactly what
/// `StorePath::sha256_digest()` (and therefore the ScopeSet) computes.
struct SeededPath {
    /// Full `/nix/store/...` string (what a closure entry carries).
    path: String,
    /// `sha256(path)` — the junction key.
    hash: Vec<u8>,
    dir_digest: [u8; 32],
    file_digest: [u8; 32],
    file_body: Vec<u8>,
}

/// Seed narinfo + complete manifest + manifest_data (single chunk) +
/// file_blobs + a directory body + path_tenants for `tenant`, and put
/// the chunk bytes in `chunks` so `ReadBlob` can stream end-to-end.
async fn seed_path(
    pool: &sqlx::PgPool,
    chunks: &MemoryChunkBackend,
    tenant: uuid::Uuid,
    name: &str,
) -> SeededPath {
    let path = test_store_path(name);
    let hash = path_hash(&path);
    let file_body: Vec<u8> = format!("contents of {name}").into_bytes();
    let file_digest: [u8; 32] = blake3::hash(&file_body).into();

    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
         VALUES ($1, $2, $1, $3)",
    )
    .bind(&hash)
    .bind(&path)
    .bind(file_body.len() as i64)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
        .bind(&hash)
        .execute(pool)
        .await
        .unwrap();
    // Whole "NAR" = the file body, as one chunk at offset 0.
    chunks
        .put(&file_digest, Bytes::copy_from_slice(&file_body))
        .await
        .unwrap();
    let chunk_list = Manifest {
        entries: vec![ManifestEntry {
            hash: file_digest,
            size: file_body.len() as u32,
        }],
    }
    .serialize();
    sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
        .bind(&hash)
        .bind(&chunk_list)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size) \
         VALUES ($1, $2, 0, $3)",
    )
    .bind(file_digest.as_slice())
    .bind(&hash)
    .bind(file_body.len() as i64)
    .execute(pool)
    .await
    .unwrap();

    // One directory body containing the file, linked to this path.
    let dir = Directory {
        directories: vec![],
        files: vec![FileEntry {
            name: b"payload".to_vec(),
            digest: file_digest.to_vec(),
            size: file_body.len() as u64,
            executable: false,
        }],
        symlinks: vec![],
    };
    let dir_digest: [u8; 32] = blake3::hash(&dir.encode_to_vec()).into();
    sqlx::query("INSERT INTO directories (digest, body) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(dir_digest.as_slice())
        .bind(dir.encode_to_vec())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO directory_paths (digest, store_path_hash) VALUES ($1, $2)")
        .bind(dir_digest.as_slice())
        .bind(&hash)
        .execute(pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
        .bind(&hash)
        .bind(tenant)
        .execute(pool)
        .await
        .unwrap();

    SeededPath {
        path,
        hash,
        dir_digest,
        file_digest,
        file_body,
    }
}

/// Assignment token whose `input_closure_digest` attests `closure`
/// (empty closure slice ⇒ unattested token, the pre-P0589 shape).
fn scoped_token(tenant: uuid::Uuid, drv_hash: &str, closure: &[String]) -> String {
    let digest = if closure.is_empty() {
        String::new()
    } else {
        let mut sorted = closure.to_vec();
        sorted.sort_unstable();
        AssignmentClaims::digest_input_closure(&sorted)
    };
    HmacSigner::from_key(SCOPE_KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "scope-test".into(),
        drv_hash: drv_hash.into(),
        expected_outputs: vec![],
        is_ca: false,
        expiry_unix: 9_999_999_999,
        tenant: Some(tenant.to_string()),
        role: TokenRole::Builder,
        input_closure_digest: digest,
    })
}

fn with_token<T>(req: T, tok: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(req);
    r.metadata_mut().insert(
        rio_proto::ASSIGNMENT_TOKEN_HEADER,
        tok.parse().expect("token is ASCII"),
    );
    r
}

/// Two same-tenant builds (A and B) with disjoint single-path closures,
/// served by a DirectoryService + ChunkService pair sharing one chunk
/// cache, with the closure scope in the given `mode`.
struct ScopeFixture {
    db: TestDb,
    dir: DirectoryServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
    tenant: uuid::Uuid,
    a: SeededPath,
    b: SeededPath,
    /// Tenant injected by the fake-JWT interceptor, when built via
    /// [`Self::with_jwt`].
    jwt_tenant: Option<uuid::Uuid>,
}

impl Drop for ScopeFixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl ScopeFixture {
    async fn new(mode: ScopeMode) -> Self {
        Self::build(mode, None).await
    }

    /// Like [`Self::new`] but the server also injects a fake JWT for
    /// `jwt_tenant` into request extensions (the gateway-path shape) —
    /// calls WITHOUT an assignment token then authenticate as that
    /// tenant, tenant-wide.
    async fn with_jwt(mode: ScopeMode, jwt_tenant_name: &str) -> (Self, uuid::Uuid) {
        let f = Self::build(mode, Some(jwt_tenant_name.to_string())).await;
        let jwt = f.jwt_tenant.expect("built with a JWT tenant");
        (f, jwt)
    }

    async fn build(mode: ScopeMode, jwt_tenant_name: Option<String>) -> Self {
        let db = TestDb::new(&MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "scope-tenant").await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let a = seed_path(&db.pool, &backend, tenant, "scope-a").await;
        let b = seed_path(&db.pool, &backend, tenant, "scope-b").await;

        let jwt_tenant = match &jwt_tenant_name {
            Some(name) => Some(seed_tenant(&db.pool, name).await),
            None => None,
        };

        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let svc = DirectoryServiceImpl::new(
            db.pool.clone(),
            Some(Arc::new(HmacVerifier::from_key(SCOPE_KEY.to_vec()))),
            Arc::clone(&cache),
        )
        .with_castore_scope(CastoreScope::new(
            mode,
            rio_store::grpc::scope::DEFAULT_CACHE_CAPACITY_BYTES,
            std::time::Duration::from_secs(600),
        ));
        let chunk_svc = ChunkServiceImpl::new(db.pool.clone(), cache);

        let (addr, server) = match jwt_tenant {
            None => {
                let router = Server::builder()
                    .add_service(DirectoryServiceServer::new(svc))
                    .add_service(ChunkServiceServer::new(chunk_svc));
                rio_test_support::grpc::spawn_grpc_server_layered(router).await
            }
            Some(jwt) => {
                // Same shape main.rs's JWT interceptor produces after
                // verification: TenantClaims in request extensions.
                let fake_jwt = move |mut req: tonic::Request<()>| {
                    req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
                        sub: jwt,
                        iat: 1_700_000_000,
                        exp: 9_999_999_999,
                        jti: "scope-test-jwt".into(),
                    });
                    Ok(req)
                };
                let router = Server::builder()
                    .layer(tonic::service::InterceptorLayer::new(fake_jwt))
                    .add_service(DirectoryServiceServer::new(svc))
                    .add_service(ChunkServiceServer::new(chunk_svc));
                rio_test_support::grpc::spawn_grpc_server_layered(router).await
            }
        };
        let channel = Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        Self {
            db,
            dir: DirectoryServiceClient::new(channel.clone()),
            chunk: ChunkServiceClient::new(channel),
            server,
            tenant,
            a,
            b,
            jwt_tenant,
        }
    }

    /// Token for "build A" (attests A's single-path closure).
    fn token_a(&self) -> String {
        scoped_token(
            self.tenant,
            "drv-scope-a",
            std::slice::from_ref(&self.a.path),
        )
    }

    /// Present build A's closure (the builder-at-mount step).
    async fn present_a(&mut self) {
        let tok = self.token_a();
        self.dir
            .present_closure(with_token(
                PresentClosureRequest {
                    closure: vec![self.a.path.clone()],
                },
                &tok,
            ))
            .await
            .expect("PresentClosure for build A");
    }

    async fn get_dir(&mut self, digest: [u8; 32], tok: &str) -> Result<Vec<Directory>, Status> {
        let resp = self
            .dir
            .get_directory(with_token(
                GetDirectoryRequest {
                    by_what: Some(get_directory_request::ByWhat::Digest(digest.to_vec())),
                    recursive: false,
                    digests: vec![],
                },
                tok,
            ))
            .await?;
        resp.into_inner().collect::<Result<Vec<_>, _>>().await
    }

    async fn read_blob(&mut self, digest: [u8; 32], tok: &str) -> Result<Vec<u8>, Status> {
        let mut stream = self
            .dir
            .read_blob(with_token(
                ReadBlobRequest {
                    file_digest: digest.to_vec(),
                },
                tok,
            ))
            .await?
            .into_inner();
        let mut body = Vec::new();
        while let Some(frame) = stream.next().await {
            body.extend_from_slice(&frame?.data);
        }
        Ok(body)
    }

    async fn stat_blob(&mut self, digest: [u8; 32], tok: &str) -> Result<(), Status> {
        self.dir
            .stat_blob(with_token(
                StatBlobRequest {
                    file_digest: digest.to_vec(),
                    send_chunks: false,
                },
                tok,
            ))
            .await
            .map(|_| ())
    }

    async fn has_dirs(&mut self, digests: Vec<Vec<u8>>, tok: &str) -> Vec<u8> {
        self.dir
            .has_directories(with_token(HasDirectoriesRequest { digests }, tok))
            .await
            .expect("HasDirectories")
            .into_inner()
            .bitmap
    }

    async fn has_blobs(&mut self, digests: Vec<Vec<u8>>, tok: &str) -> Vec<u8> {
        self.dir
            .has_blobs(with_token(HasBlobsRequest { digests }, tok))
            .await
            .expect("HasBlobs")
            .into_inner()
            .bitmap
    }
}

/// Same-tenant disjoint-closure allow/deny matrix over all five castore
/// read RPCs under enforce: build A's token (with A's closure
/// presented) reads A's digests and gets the no-oracle `NOT_FOUND` for
/// B's; the presence bitmaps clear B's bits; the deny counter carries
/// the real reason.
// r[verify store.castore.closure-scope]
// r[verify store.castore.scope-establish]
#[tokio::test]
async fn enforce_allows_own_closure_and_denies_disjoint_closure() {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    f.present_a().await;
    let tok_a = f.token_a();
    assert_eq!(
        recorder.get("rio_store_castore_scope_established_total{}"),
        1,
        "presentation established the scope"
    );

    // In scope: all five RPCs serve A's digests.
    let a_dir = f.a.dir_digest;
    let a_file = f.a.file_digest;
    let bodies = f.get_dir(a_dir, &tok_a).await.expect("A's directory body");
    assert_eq!(bodies.len(), 1);
    let body = f.read_blob(a_file, &tok_a).await.expect("A's file bytes");
    assert_eq!(body, f.a.file_body);
    f.stat_blob(a_file, &tok_a).await.expect("A's stat");
    assert_eq!(
        f.has_dirs(vec![a_dir.to_vec(), f.b.dir_digest.to_vec()], &tok_a)
            .await,
        vec![0b01],
        "presence bit set for A's dir, cleared for B's"
    );
    assert_eq!(
        f.has_blobs(vec![a_file.to_vec(), f.b.file_digest.to_vec()], &tok_a)
            .await,
        vec![0b01],
        "presence bit set for A's blob, cleared for B's"
    );

    // Out of scope: B's digests (same tenant!) answer NOT_FOUND with the
    // exact same message an absent digest gets — no existence oracle.
    let b_dir = f.b.dir_digest;
    let b_file = f.b.file_digest;
    let absent: [u8; 32] = blake3::hash(b"never-uploaded").into();
    let denied_dir = f.get_dir(b_dir, &tok_a).await.expect_err("B dir denied");
    let absent_dir = f.get_dir(absent, &tok_a).await.expect_err("absent dir");
    assert_eq!(denied_dir.code(), tonic::Code::NotFound);
    assert_eq!(absent_dir.code(), tonic::Code::NotFound);
    assert_eq!(
        denied_dir.message(),
        absent_dir.message(),
        "out-of-scope and absent must be indistinguishable on the wire"
    );
    let denied_blob = f
        .read_blob(b_file, &tok_a)
        .await
        .expect_err("B blob denied");
    let absent_blob = f.read_blob(absent, &tok_a).await.expect_err("absent blob");
    assert_eq!(denied_blob.code(), tonic::Code::NotFound);
    assert_eq!(denied_blob.message(), absent_blob.message());
    let denied_stat = f
        .stat_blob(b_file, &tok_a)
        .await
        .expect_err("B stat denied");
    assert_eq!(denied_stat.code(), tonic::Code::NotFound);

    // The deny counter (not the wire status) carries the real reason:
    // one per denied single-digest read (B dir, B blob, B stat).
    assert_eq!(
        recorder.get("rio_store_castore_scope_denied_total{reason=out_of_scope}"),
        3,
        "saw counters: {:?}",
        recorder.all_keys()
    );
    // The genuinely-absent digests must NOT count as denies.
    assert_eq!(
        recorder.get("rio_store_castore_scope_would_deny_total{reason=out_of_scope}"),
        0
    );
}

/// A popular (content-shared) digest contained in BOTH an in-closure
/// path and an out-of-closure path stays readable: the scope predicate
/// rides the containing-path join, so the serving query resolves
/// through the in-closure referrer instead of being starved by the
/// arbitrary `LIMIT 1` referrer choice landing on the out-of-closure
/// one.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn shared_digest_readable_through_in_closure_referrer() {
    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    // A second, out-of-closure path carrying the SAME bytes as build
    // A's file (the "popular path" shape: identical content in two
    // store paths). Its manifest references the same content-addressed
    // chunk, so no extra backend seeding is needed.
    let popular = test_store_path("scope-popular");
    let popular_hash = path_hash(&popular);
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
         VALUES ($1, $2, $1, $3)",
    )
    .bind(&popular_hash)
    .bind(&popular)
    .bind(f.a.file_body.len() as i64)
    .execute(&f.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
        .bind(&popular_hash)
        .execute(&f.db.pool)
        .await
        .unwrap();
    let chunk_list = Manifest {
        entries: vec![ManifestEntry {
            hash: f.a.file_digest,
            size: f.a.file_body.len() as u32,
        }],
    }
    .serialize();
    sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
        .bind(&popular_hash)
        .bind(&chunk_list)
        .execute(&f.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size) \
         VALUES ($1, $2, 0, $3)",
    )
    .bind(f.a.file_digest.as_slice())
    .bind(&popular_hash)
    .bind(f.a.file_body.len() as i64)
    .execute(&f.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
        .bind(&popular_hash)
        .bind(f.tenant)
        .execute(&f.db.pool)
        .await
        .unwrap();

    f.present_a().await;
    let tok_a = f.token_a();
    let body = f
        .read_blob(f.a.file_digest, &tok_a)
        .await
        .expect("digest shared with an out-of-closure path is still readable in scope");
    assert_eq!(body, f.a.file_body);
    f.stat_blob(f.a.file_digest, &tok_a)
        .await
        .expect("stat resolves through the in-closure referrer");
}

/// Recursive GetDirectory scope-filters the seed frontier only: an
/// out-of-scope seed is silently absent (same as a non-tenant-visible
/// seed today), while children discovered during the descent inherit
/// scope from their authorized parent (the descent keeps the per-batch
/// tenant-only join).
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn recursive_walk_filters_seeds_only_and_descends_by_containment() {
    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;

    // Child directory linked ONLY to B's (out-of-closure, same tenant)
    // path; parent (in A's closure) references it. A scope-filtered
    // descent would drop the child; containment-based inheritance must
    // stream it.
    let child = Directory {
        directories: vec![],
        files: vec![],
        symlinks: vec![],
    };
    let child_digest: [u8; 32] = blake3::hash(&child.encode_to_vec()).into();
    sqlx::query("INSERT INTO directories (digest, body) VALUES ($1, $2)")
        .bind(child_digest.as_slice())
        .bind(child.encode_to_vec())
        .execute(&f.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO directory_paths (digest, store_path_hash) VALUES ($1, $2)")
        .bind(child_digest.as_slice())
        .bind(&f.b.hash)
        .execute(&f.db.pool)
        .await
        .unwrap();
    let parent = Directory {
        directories: vec![DirectoryEntry {
            name: b"child".to_vec(),
            digest: child_digest.to_vec(),
            size: 0,
        }],
        files: vec![],
        symlinks: vec![],
    };
    let parent_digest: [u8; 32] = blake3::hash(&parent.encode_to_vec()).into();
    sqlx::query("INSERT INTO directories (digest, body) VALUES ($1, $2)")
        .bind(parent_digest.as_slice())
        .bind(parent.encode_to_vec())
        .execute(&f.db.pool)
        .await
        .unwrap();
    sqlx::query("INSERT INTO directory_paths (digest, store_path_hash) VALUES ($1, $2)")
        .bind(parent_digest.as_slice())
        .bind(&f.a.hash)
        .execute(&f.db.pool)
        .await
        .unwrap();

    f.present_a().await;
    let tok_a = f.token_a();

    // Seeds: the in-scope parent and B's (out-of-scope) directory. The
    // walk streams the parent and its contained child; B's seed is
    // silently skipped.
    let resp = f
        .dir
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(
                    parent_digest.to_vec(),
                )),
                recursive: true,
                digests: vec![f.b.dir_digest.to_vec()],
            },
            &tok_a,
        ))
        .await
        .expect("recursive walk starts")
        .into_inner();
    let bodies: Vec<Directory> = resp
        .collect::<Result<Vec<_>, _>>()
        .await
        .expect("stream completes");
    let digests: std::collections::HashSet<[u8; 32]> = bodies
        .iter()
        .map(|d| blake3::hash(&d.encode_to_vec()).into())
        .collect();
    assert!(
        digests.contains(&parent_digest),
        "in-scope seed streams its body"
    );
    assert!(
        digests.contains(&child_digest),
        "descent inherits scope by containment (child linked only to another path)"
    );
    assert!(
        !digests.contains(&f.b.dir_digest),
        "out-of-scope seed is silently absent from the stream"
    );
}

/// JWT (gateway/user) callers keep tenant-wide reads in every mode —
/// the gateway's delta-sync Has* path must be unaffected by enforce.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn jwt_callers_stay_tenant_wide_under_enforce() {
    let (mut f, _jwt) = ScopeFixture::with_jwt(ScopeMode::Enforce, "scope-jwt-tenant").await;
    // The JWT tenant is a different tenant than the seeded paths' owner,
    // so re-attribute both paths to it (the gateway reads its own
    // tenant's paths).
    let jwt = f.jwt_tenant.expect("jwt fixture");
    for hash in [f.a.hash.clone(), f.b.hash.clone()] {
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(&hash)
            .bind(jwt)
            .execute(&f.db.pool)
            .await
            .unwrap();
    }

    // No assignment token at all: the fake-JWT interceptor injects the
    // tenant claims, and both builds' digests are visible.
    let bitmap = f
        .dir
        .has_directories(tonic::Request::new(HasDirectoriesRequest {
            digests: vec![f.a.dir_digest.to_vec(), f.b.dir_digest.to_vec()],
        }))
        .await
        .expect("JWT HasDirectories")
        .into_inner()
        .bitmap;
    assert_eq!(bitmap, vec![0b11], "JWT caller sees the whole tenant");
    let bitmap = f
        .dir
        .has_blobs(tonic::Request::new(HasBlobsRequest {
            digests: vec![f.a.file_digest.to_vec(), f.b.file_digest.to_vec()],
        }))
        .await
        .expect("JWT HasBlobs")
        .into_inner()
        .bitmap;
    assert_eq!(bitmap, vec![0b11]);
    let bodies = f
        .dir
        .get_directory(tonic::Request::new(GetDirectoryRequest {
            by_what: Some(get_directory_request::ByWhat::Digest(
                f.b.dir_digest.to_vec(),
            )),
            recursive: false,
            digests: vec![],
        }))
        .await
        .expect("JWT GetDirectory")
        .into_inner()
        .collect::<Result<Vec<_>, _>>()
        .await
        .expect("body");
    assert_eq!(bodies.len(), 1);
}

/// Unattested tokens (empty `input_closure_digest`): enforce denies
/// outright with the opaque rejection (no tenant-wide fallback); log
/// serves and counts the would-deny.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn unattested_token_denied_under_enforce_served_under_log() {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    // enforce: PERMISSION_DENIED before any data-plane query.
    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    let unattested = scoped_token(f.tenant, "drv-unattested", &[]);
    let err = f
        .stat_blob(f.a.file_digest, &unattested)
        .await
        .expect_err("unattested token under enforce");
    assert_eq!(err.code(), tonic::Code::PermissionDenied);
    assert_eq!(
        err.message(),
        "assignment token rejected",
        "rejection must not say why"
    );
    assert_eq!(
        recorder.get("rio_store_castore_scope_denied_total{reason=unattested}"),
        1
    );
    drop(f);

    // log: served (today's tenant-wide behavior) + would_deny counted.
    let mut f = ScopeFixture::new(ScopeMode::Log).await;
    let unattested = scoped_token(f.tenant, "drv-unattested", &[]);
    f.stat_blob(f.a.file_digest, &unattested)
        .await
        .expect("unattested token under log mode is served");
    assert_eq!(
        recorder.get("rio_store_castore_scope_would_deny_total{reason=unattested}"),
        1
    );
}

/// `log` mode never rejects: out-of-scope reads are served unchanged
/// while `would_deny` counts what enforce would have rejected —
/// including the per-bit accounting on the presence RPCs.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn log_mode_serves_out_of_scope_and_counts_would_deny() {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let mut f = ScopeFixture::new(ScopeMode::Log).await;
    f.present_a().await;
    let tok_a = f.token_a();

    // Out-of-scope single-digest reads are served...
    let body = f
        .read_blob(f.b.file_digest, &tok_a)
        .await
        .expect("log mode serves out-of-scope reads");
    assert_eq!(body, f.b.file_body);
    f.stat_blob(f.b.file_digest, &tok_a)
        .await
        .expect("log mode serves out-of-scope stat");
    let bodies = f
        .get_dir(f.b.dir_digest, &tok_a)
        .await
        .expect("log mode serves out-of-scope directory");
    assert_eq!(bodies.len(), 1);
    // ...and the presence bitmaps stay tenant-wide.
    assert_eq!(
        f.has_dirs(
            vec![f.a.dir_digest.to_vec(), f.b.dir_digest.to_vec()],
            &tok_a
        )
        .await,
        vec![0b11]
    );

    // would_deny: 3 single-digest reads + 1 HasDirectories bit.
    assert_eq!(
        recorder.get("rio_store_castore_scope_would_deny_total{reason=out_of_scope}"),
        4,
        "saw counters: {:?}",
        recorder.all_keys()
    );
    assert_eq!(
        recorder.get("rio_store_castore_scope_denied_total{reason=out_of_scope}"),
        0,
        "log mode must never deny"
    );
}

/// `off` keeps today's tenant-wide behavior bit-for-bit, and the
/// default-constructed service (no `with_castore_scope`) is `off`.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn off_mode_keeps_tenant_wide_reads() {
    let mut f = ScopeFixture::new(ScopeMode::Off).await;
    let tok_a = f.token_a();
    // No presentation, disjoint closure: still readable under off.
    let body = f
        .read_blob(f.b.file_digest, &tok_a)
        .await
        .expect("off mode");
    assert_eq!(body, f.b.file_body);
    assert_eq!(
        f.has_blobs(
            vec![f.a.file_digest.to_vec(), f.b.file_digest.to_vec()],
            &tok_a
        )
        .await,
        vec![0b11]
    );
}

/// Derivation fallback (§3.5): a replica that never saw a presentation
/// can still authorize in-closure reads under enforce by rebuilding the
/// scope from `scheduler_live_pins` + `narinfo.references`; out-of-
/// closure digests stay NOT_FOUND. Without pins the read answers
/// FAILED_PRECONDITION + CASTORE_SCOPE_REQUIRED so the builder presents
/// and retries.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn derivation_fallback_covers_unpresented_replica_and_scope_required_otherwise() {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    // Pin build A's root for the token's drv — the dispatch-time seed
    // the scheduler writes best-effort.
    sqlx::query("INSERT INTO scheduler_live_pins (store_path_hash, drv_hash) VALUES ($1, $2)")
        .bind(&f.a.hash)
        .bind("drv-scope-a")
        .execute(&f.db.pool)
        .await
        .unwrap();

    // NOTE: no PresentClosure call anywhere in this test.
    let tok_a = f.token_a();
    let body = f
        .read_blob(f.a.file_digest, &tok_a)
        .await
        .expect("derived scope authorizes the in-closure read");
    assert_eq!(body, f.a.file_body);
    let err = f
        .read_blob(f.b.file_digest, &tok_a)
        .await
        .expect_err("derived scope still excludes the disjoint closure");
    assert_eq!(err.code(), tonic::Code::NotFound);
    assert!(
        recorder.get("rio_store_castore_scope_absent_total{resolution=derived}") >= 1,
        "the fallback resolution is visible in the absent counter: {:?}",
        recorder.all_keys()
    );

    // A build with no pins at all: scope unresolvable ⇒ the documented
    // present-and-retry signal, not a silent tenant-wide fallback.
    let tok_orphan = scoped_token(f.tenant, "drv-no-pins", &[f.a.path.clone()]);
    let err = f
        .stat_blob(f.a.file_digest, &tok_orphan)
        .await
        .expect_err("unresolvable scope under enforce");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
    assert!(
        err.message()
            .contains(rio_proto::CASTORE_SCOPE_REQUIRED_MSG),
        "carries the wire-contract reason: {err:?}"
    );
    assert_eq!(
        recorder.get("rio_store_castore_scope_absent_total{resolution=denied}"),
        1
    );
}

/// PresentClosure rejects a closure that does not hash to the token's
/// signed digest (INVALID_ARGUMENT in every mode), echoes nothing, and
/// caches nothing — the next read still has no scope.
// r[verify store.castore.scope-establish]
#[tokio::test]
async fn present_closure_mismatch_rejected_and_not_cached() {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    let tok_a = f.token_a();
    // Token attests [a]; present [a, b] — a builder trying to widen its
    // own scope.
    let err = f
        .dir
        .present_closure(with_token(
            PresentClosureRequest {
                closure: vec![f.a.path.clone(), f.b.path.clone()],
            },
            &tok_a,
        ))
        .await
        .expect_err("widened presentation must be rejected");
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
    assert!(
        !err.message().contains("/nix/store/"),
        "rejection must not echo store data: {err:?}"
    );
    assert_eq!(recorder.get("rio_store_castore_scope_mismatch_total{}"), 1);
    assert_eq!(
        recorder.get("rio_store_castore_scope_established_total{}"),
        0,
        "nothing was established"
    );

    // The rejected presentation left no scope behind: the next read is
    // the scope-required signal (no pins seeded for this drv).
    let err = f
        .stat_blob(f.a.file_digest, &tok_a)
        .await
        .expect_err("no scope was cached by the rejected presentation");
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    // PresentClosure without any assignment token is rejected too (JWT
    // callers have nothing to present).
    let err = f
        .dir
        .present_closure(tonic::Request::new(PresentClosureRequest {
            closure: vec![f.a.path.clone()],
        }))
        .await
        .expect_err("PresentClosure requires an assignment token");
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

/// The chunk layer stays digest-as-capability: with the directory
/// service in enforce mode, a tokenless ChunkService call still reaches
/// the data plane (NOT_FOUND for an unknown digest — same proof shape
/// the revocation tests use), because possession of a BLAKE3 chunk
/// digest is the read capability there by design.
// r[verify store.castore.closure-scope]
#[tokio::test]
async fn chunk_rpcs_stay_unscoped_under_enforce() {
    let mut f = ScopeFixture::new(ScopeMode::Enforce).await;
    let err = f
        .chunk
        .get_chunk(tonic::Request::new(GetChunkRequest {
            digest: vec![0xEE; 32],
        }))
        .await
        .expect_err("unknown chunk digest");
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "tokenless GetChunk reached the data plane (not an auth rejection): {err:?}"
    );
}
