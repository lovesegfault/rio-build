//! ADR-022 castore RPC surface integration tests (P0573 / P0577).
//!
//! Spins up a `DirectoryService` server with HMAC tenant resolution,
//! seeds the `directories` / `directory_paths` / `file_blobs` /
//! `path_tenants` tables directly (the indexer pipeline that normally
//! populates them is exercised by the `nar_index` tests), and
//! drives `GetDirectory` / `HasDirectories` / `HasBlobs` / `ReadBlob`
//! end-to-end.

use std::sync::Arc;

use prost::Message;
use sha2::Digest as _;
use tokio_stream::StreamExt;
use tonic::transport::{Channel, Server};

use rio_auth::hmac::{AssignmentClaims, HmacSigner, HmacVerifier};
use rio_proto::DirectoryServiceServer;
use rio_proto::castore::{Directory, DirectoryEntry, FileEntry};
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::types::{
    GetDirectoryRequest, HasBlobsRequest, HasDirectoriesRequest, ReadBlobRequest, StatBlobRequest,
    StatBlobResponse, get_directory_request,
};
use rio_store::MIGRATOR;
use rio_store::backend::{ChunkBackend, MemoryChunkBackend};
use rio_store::cas::ChunkCache;
use rio_store::grpc::DirectoryServiceImpl;
use rio_store::manifest::{Manifest, ManifestEntry};
use rio_store::test_helpers::seed_tenant;
use rio_test_support::TestDb;

const KEY: &[u8] = b"directory-test-key-32-bytes-aaaa";

/// HMAC assignment token whose `tenant` claim drives the tenant scope.
fn token(tenant: uuid::Uuid) -> String {
    HmacSigner::from_key(KEY.to_vec()).sign(&AssignmentClaims {
        executor_id: "test".into(),
        drv_hash: "00".repeat(32),
        expected_outputs: vec![],
        is_ca: false,
        expiry_unix: 9_999_999_999,
        tenant: Some(tenant.to_string()),
        input_closure_digest: String::new(),
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

/// Encode and persist a `Directory` body for `tenant`. Returns the
/// digest. Uses a synthetic blake3-of-name digest so tests can pick
/// stable, distinct values without computing the canonical encoding.
async fn put_dir(
    pool: &sqlx::PgPool,
    tenant: uuid::Uuid,
    name: &str,
    children: &[(&str, [u8; 32])],
    files: &[(&str, [u8; 32], u64)],
) -> [u8; 32] {
    let dir = Directory {
        directories: children
            .iter()
            .map(|(n, d)| DirectoryEntry {
                name: n.as_bytes().to_vec(),
                digest: d.to_vec(),
                size: 0,
            })
            .collect(),
        files: files
            .iter()
            .map(|(n, d, s)| FileEntry {
                name: n.as_bytes().to_vec(),
                digest: d.to_vec(),
                size: *s,
                executable: false,
            })
            .collect(),
        symlinks: vec![],
    };
    let digest: [u8; 32] = blake3::hash(name.as_bytes()).into();
    // Tenancy is read-time via `directory_paths` → `path_tenants`,
    // so a directory needs a backing manifest row owned by `tenant`.
    let path_hash = seed_owned_path(pool, tenant, &format!("dir-{name}")).await;
    sqlx::query("INSERT INTO directories (digest, body) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(digest.as_slice())
        .bind(dir.encode_to_vec())
        .execute(pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO directory_paths (digest, store_path_hash) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(digest.as_slice())
    .bind(&path_hash)
    .execute(pool)
    .await
    .unwrap();
    digest
}

/// Insert `narinfo` + `manifests` (`'complete'`) + `path_tenants` for a
/// synthetic path named `name`, owned by `tenant`. Returns the path hash.
async fn seed_owned_path(pool: &sqlx::PgPool, tenant: uuid::Uuid, name: &str) -> Vec<u8> {
    let path_hash = sha2::Sha256::digest(name.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
         VALUES ($1, $2, $1, 0) ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .bind(format!("/nix/store/{name}"))
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete') \
         ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .execute(pool)
    .await
    .unwrap();
    sqlx::query(
        "INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .bind(tenant)
    .execute(pool)
    .await
    .unwrap();
    path_hash
}

/// Persist a `file_blobs` row tied to a `path_tenants`-owned path.
async fn put_blob(pool: &sqlx::PgPool, tenant: uuid::Uuid, name: &str) -> [u8; 32] {
    let digest: [u8; 32] = blake3::hash(name.as_bytes()).into();
    let path_hash = seed_owned_path(pool, tenant, name).await;
    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset) VALUES ($1, $2, 0) \
         ON CONFLICT DO NOTHING",
    )
    .bind(digest.as_slice())
    .bind(&path_hash)
    .execute(pool)
    .await
    .unwrap();
    digest
}

struct Fixture {
    db: TestDb,
    client: DirectoryServiceClient<Channel>,
    server: tokio::task::JoinHandle<()>,
    tenant_a: uuid::Uuid,
    tenant_b: uuid::Uuid,
    /// Backend behind the server's `ChunkCache` — tests seed chunked
    /// fixtures by `put()`ting the chunk bytes here directly.
    chunks: Arc<MemoryChunkBackend>,
}

impl Drop for Fixture {
    fn drop(&mut self) {
        self.server.abort();
    }
}

async fn fixture() -> Fixture {
    fixture_with_chunk_cache(true).await
}

/// Like [`fixture`] but lets the test choose whether the server gets a
/// chunk backend. `Fixture.chunks` is always populated (tests seed it
/// directly); only the server-side `ChunkCache` wiring is optional.
async fn fixture_with_chunk_cache(with_cache: bool) -> Fixture {
    let db = TestDb::new(&MIGRATOR).await;
    let tenant_a = seed_tenant(&db.pool, "dir-a").await;
    let tenant_b = seed_tenant(&db.pool, "dir-b").await;
    let chunks = Arc::new(MemoryChunkBackend::new());
    let cache =
        with_cache.then(|| Arc::new(ChunkCache::new(Arc::clone(&chunks) as Arc<dyn ChunkBackend>)));
    let svc = DirectoryServiceImpl::new(
        db.pool.clone(),
        Some(Arc::new(HmacVerifier::from_key(KEY.to_vec()))),
        cache,
        // No signer: the sig-visibility fallback tests derive trust
        // from `tenant_upstreams.trusted_keys` alone.
        None,
    );
    let router = Server::builder().add_service(DirectoryServiceServer::new(svc));
    let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
    let channel = Channel::from_shared(format!("http://{addr}"))
        .unwrap()
        .connect()
        .await
        .unwrap();
    Fixture {
        db,
        client: DirectoryServiceClient::new(channel),
        server,
        tenant_a,
        tenant_b,
        chunks,
    }
}

/// Two-level tree: root → {sub, leaf-file}. Recursive multi-root walk
/// returns both bodies; non-recursive returns one; cross-tenant calls
/// see nothing.
// r[verify store.castore.directory-rpc]
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn get_directory_recursive_and_tenant_scoped() {
    let mut f = fixture().await;
    let leaf: [u8; 32] = blake3::hash(b"leaf-file").into();
    let sub = put_dir(&f.db.pool, f.tenant_a, "sub", &[], &[("leaf", leaf, 4)]).await;
    let root = put_dir(&f.db.pool, f.tenant_a, "root", &[("sub", sub)], &[]).await;
    let tok_a = token(f.tenant_a);
    let tok_b = token(f.tenant_b);

    // Recursive from root: both bodies.
    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: true,
                digests: vec![],
            },
            &tok_a,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 2, "root + sub");
    let names: std::collections::HashSet<Vec<u8>> = bodies
        .iter()
        .flat_map(|d| d.directories.iter().map(|e| e.name.clone()))
        .chain(
            bodies
                .iter()
                .flat_map(|d| d.files.iter().map(|f| f.name.clone())),
        )
        .collect();
    assert!(names.contains(b"sub".as_slice()) && names.contains(b"leaf".as_slice()));

    // Non-recursive: exactly the requested body.
    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(sub.to_vec())),
                recursive: false,
                digests: vec![],
            },
            &tok_a,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 1);

    // Multi-root via `digests`: same set, deduped.
    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: None,
                recursive: true,
                digests: vec![root.to_vec(), sub.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 2, "dedup across roots");

    // Cross-tenant: B sees nothing.
    let err = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: false,
                digests: vec![],
            },
            &tok_b,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    // No token → UNAUTHENTICATED.
    let err = f
        .client
        .get_directory(GetDirectoryRequest {
            by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
            recursive: false,
            digests: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated);
}

/// Bitmap responses: bit i ⇔ digests[i] present and tenant-visible.
// r[verify store.castore.directory-rpc]
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn has_directories_and_blobs_bitmaps() {
    let mut f = fixture().await;
    let d1 = put_dir(&f.db.pool, f.tenant_a, "d1", &[], &[]).await;
    let d2 = put_dir(&f.db.pool, f.tenant_a, "d2", &[], &[]).await;
    let unknown: [u8; 32] = blake3::hash(b"unknown").into();
    let blob = put_blob(&f.db.pool, f.tenant_a, "blob1").await;
    let tok_a = token(f.tenant_a);
    let tok_b = token(f.tenant_b);

    // [d1, unknown, d2] → bits 0,2 set, bit 1 clear → 0b101 = 5.
    let r = f
        .client
        .has_directories(with_token(
            HasDirectoriesRequest {
                digests: vec![d1.to_vec(), unknown.to_vec(), d2.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0b101]);

    // 9 entries → 2 bytes; bit 8 (first of byte 1) is set, rest clear.
    let mut digests = vec![unknown.to_vec(); 8];
    digests.push(d1.to_vec());
    let r = f
        .client
        .has_directories(with_token(HasDirectoriesRequest { digests }, &tok_a))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0x00, 0x01]);

    // HasBlobs: [blob, unknown] → 0b01.
    let r = f
        .client
        .has_blobs(with_token(
            HasBlobsRequest {
                digests: vec![blob.to_vec(), unknown.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0b01]);

    // Cross-tenant: B sees zeros.
    let r = f
        .client
        .has_directories(with_token(
            HasDirectoriesRequest {
                digests: vec![d1.to_vec()],
            },
            &tok_b,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0]);
    let r = f
        .client
        .has_blobs(with_token(
            HasBlobsRequest {
                digests: vec![blob.to_vec()],
            },
            &tok_b,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0]);
}

/// Wrong-length digests are rejected before touching PG.
// r[verify store.castore.directory-rpc]
#[tokio::test]
async fn rejects_bad_digest_length() {
    let mut f = fixture().await;
    let tok = token(f.tenant_a);
    let err = f
        .client
        .has_directories(with_token(
            HasDirectoriesRequest {
                digests: vec![vec![0u8; 16]],
            },
            &tok,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);
}

/// `GetDirectory` rejects empty and ambiguous requests; `Has*`
/// returns an empty bitmap for an empty list.
// r[verify store.castore.directory-rpc]
#[tokio::test]
async fn rejects_malformed_get_directory() {
    let mut f = fixture().await;
    let d = put_dir(&f.db.pool, f.tenant_a, "d", &[], &[]).await;
    let tok = token(f.tenant_a);

    // No digests at all.
    let err = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: None,
                recursive: true,
                digests: vec![],
            },
            &tok,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Non-recursive multi-root is ambiguous.
    let err = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(d.to_vec())),
                recursive: false,
                digests: vec![d.to_vec()],
            },
            &tok,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    // Empty Has* list → empty bitmap.
    let r = f
        .client
        .has_directories(with_token(HasDirectoriesRequest { digests: vec![] }, &tok))
        .await
        .unwrap()
        .into_inner();
    assert!(r.bitmap.is_empty());
}

/// Synthetic NAR for `ReadBlob`: 64 bytes of framing, the file body,
/// 32 bytes of trailing framing. `nar_offset = 64` in every test.
struct BlobNar {
    nar: Vec<u8>,
    file: Vec<u8>,
    file_digest: [u8; 32],
}

const BLOB_NAR_OFFSET: usize = 64;

fn make_blob_nar(file_len: usize, seed: u8) -> BlobNar {
    let file: Vec<u8> = (0..file_len)
        .map(|i| (i as u8).wrapping_add(seed))
        .collect();
    let mut nar = vec![0xAAu8; BLOB_NAR_OFFSET];
    nar.extend_from_slice(&file);
    nar.extend_from_slice(&[0xBBu8; 32]);
    let file_digest = blake3::hash(&file).into();
    BlobNar {
        nar,
        file,
        file_digest,
    }
}

/// Persist a `(narinfo, manifests, path_tenants, file_blobs)` row set
/// whose manifest is either inline (whole NAR in
/// `manifests.inline_blob`) or chunked (`manifest_data.chunk_list` +
/// chunks in `Fixture.chunks`). Returns the `file_digest`.
async fn seed_blob(f: &Fixture, name: &str, b: &BlobNar, chunk_size: Option<usize>) -> [u8; 32] {
    let path_hash = sha2::Sha256::digest(name.as_bytes()).to_vec();
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size) \
         VALUES ($1, $2, $1, $3)",
    )
    .bind(&path_hash)
    .bind(format!("/nix/store/{name}"))
    .bind(b.nar.len() as i64)
    .execute(&f.db.pool)
    .await
    .unwrap();

    if let Some(chunk_size) = chunk_size {
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&path_hash)
            .execute(&f.db.pool)
            .await
            .unwrap();
        let mut entries = Vec::new();
        for piece in b.nar.chunks(chunk_size) {
            let hash: [u8; 32] = blake3::hash(piece).into();
            f.chunks
                .put(&hash, rio_store::cas::compress_chunk(piece))
                .await
                .unwrap();
            entries.push(ManifestEntry {
                hash,
                size: piece.len() as u32,
            });
        }
        let chunk_list = Manifest { entries }.serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&path_hash)
            .bind(&chunk_list)
            .execute(&f.db.pool)
            .await
            .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(&path_hash)
        .bind(&b.nar)
        .execute(&f.db.pool)
        .await
        .unwrap();
    }

    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(b.file_digest.as_slice())
    .bind(&path_hash)
    .bind(BLOB_NAR_OFFSET as i64)
    .bind(b.file.len() as i64)
    .execute(&f.db.pool)
    .await
    .unwrap();
    // Tenancy: read-time via `path_tenants` on the `file_blobs` row's
    // `store_path_hash`.
    sqlx::query(
        "INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .bind(f.tenant_a)
    .execute(&f.db.pool)
    .await
    .unwrap();
    b.file_digest
}

/// Drive `ReadBlob` and concatenate the response frames.
async fn read_blob(f: &mut Fixture, digest: [u8; 32], tok: &str) -> Result<Vec<u8>, tonic::Status> {
    let resp = f
        .client
        .read_blob(with_token(
            ReadBlobRequest {
                file_digest: digest.to_vec(),
            },
            tok,
        ))
        .await?;
    let mut body = Vec::new();
    let mut stream = resp.into_inner();
    while let Some(frame) = stream.next().await {
        body.extend_from_slice(&frame?.data);
    }
    Ok(body)
}

/// Inline-manifest path: server slices `manifests.inline_blob` and
/// streams the file body.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_inline_manifest() {
    let mut f = fixture().await;
    let b = make_blob_nar(700, 1);
    let digest = seed_blob(&f, "rb-inline", &b, None).await;
    let tok = token(f.tenant_a);

    let body = read_blob(&mut f, digest, &tok).await.unwrap();
    assert_eq!(body, b.file);
    assert_eq!(<[u8; 32]>::from(blake3::hash(&body)), digest);
}

/// Chunked-manifest path: file straddles eight chunks with NAR framing
/// on both sides. First/last chunks are sliced to the file boundary;
/// middle chunks pass through whole.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_chunked_straddles_boundaries() {
    let mut f = fixture().await;
    // 100-byte chunks → file at offset 64 spans chunks 0-7: starts
    // mid-chunk-0, ends mid-chunk-7 at byte 764 (64 + 700), with NAR
    // framing on both sides.
    let b = make_blob_nar(700, 2);
    let digest = seed_blob(&f, "rb-chunked", &b, Some(100)).await;
    let tok = token(f.tenant_a);

    let body = read_blob(&mut f, digest, &tok).await.unwrap();
    assert_eq!(body, b.file);
    assert_eq!(<[u8; 32]>::from(blake3::hash(&body)), digest);
}

/// Single-chunk file: skip and take both apply to the same chunk when
/// the whole NAR fits in one.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_single_chunk_skip_and_take() {
    let mut f = fixture().await;
    // Whole NAR in one 1 MiB chunk; file is 16 bytes at offset 64.
    let b = make_blob_nar(16, 3);
    let digest = seed_blob(&f, "rb-single", &b, Some(1 << 20)).await;
    let tok = token(f.tenant_a);

    let body = read_blob(&mut f, digest, &tok).await.unwrap();
    assert_eq!(body, b.file);
}

/// Cross-tenant: a digest tenant A produced is NotFound for tenant B,
/// same status as an unknown digest, so the RPC isn't a presence oracle.
// r[verify store.castore.blob-read]
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn read_blob_tenant_scoped() {
    let mut f = fixture().await;
    let b = make_blob_nar(64, 4);
    let digest = seed_blob(&f, "rb-tenant", &b, None).await;
    let tok_b = token(f.tenant_b);

    let err = read_blob(&mut f, digest, &tok_b).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    // Unknown digest: same code.
    let unknown: [u8; 32] = blake3::hash(b"nope").into();
    let tok_a = token(f.tenant_a);
    let err = read_blob(&mut f, unknown, &tok_a).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// Zero-byte file: the chunk plan is empty and the stream closes with
/// no frames, both inline and chunked.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_zero_byte_file() {
    let mut f = fixture().await;
    let b = make_blob_nar(0, 5);
    for (name, chunk) in [("rb-zero-inline", None), ("rb-zero-chunked", Some(50))] {
        let digest = seed_blob(&f, name, &b, chunk).await;
        let tok = token(f.tenant_a);
        let body = read_blob(&mut f, digest, &tok).await.unwrap();
        assert!(body.is_empty());
        assert_eq!(<[u8; 32]>::from(blake3::hash(&body)), digest);
    }
}

/// No chunk backend: chunked manifests fail FAILED_PRECONDITION up
/// front, inline manifests still serve.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_chunked_no_backend_failed_precondition() {
    let mut f = fixture_with_chunk_cache(false).await;

    // Inline still resolves without a backend.
    let b_inline = make_blob_nar(8, 6);
    let d_inline = seed_blob(&f, "rb-noc-inline", &b_inline, None).await;
    let tok = token(f.tenant_a);
    let body = read_blob(&mut f, d_inline, &tok).await.unwrap();
    assert_eq!(body, b_inline.file);

    // Chunked errors without one.
    let b_chunked = make_blob_nar(200, 7);
    let d_chunked = seed_blob(&f, "rb-noc-chunked", &b_chunked, Some(50)).await;
    let err = read_blob(&mut f, d_chunked, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);
}

/// Corrupt `manifest_data.chunk_list` returns DATA_LOSS, not Internal:
/// the handler reports the bad row instead of crashing.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_corrupt_chunk_list_data_loss() {
    let mut f = fixture().await;
    let b = make_blob_nar(200, 8);
    let digest = seed_blob(&f, "rb-corrupt", &b, Some(50)).await;
    sqlx::query("UPDATE manifest_data SET chunk_list = $1")
        .bind(b"not a manifest".as_slice())
        .execute(&f.db.pool)
        .await
        .unwrap();
    let tok = token(f.tenant_a);

    let err = read_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
}

/// `file_blobs.size` overrunning the NAR returns DATA_LOSS rather
/// than panicking on an out-of-bounds slice.
// r[verify store.castore.blob-read]
#[tokio::test]
async fn read_blob_size_overruns_nar_data_loss() {
    let mut f = fixture().await;
    let b = make_blob_nar(32, 9);
    let digest = seed_blob(&f, "rb-overrun", &b, None).await;
    sqlx::query("UPDATE file_blobs SET size = $1")
        .bind(1_000_000i64)
        .execute(&f.db.pool)
        .await
        .unwrap();
    let tok = token(f.tenant_a);

    let err = read_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
}

async fn stat_blob_with(
    f: &mut Fixture,
    digest: [u8; 32],
    tok: &str,
    send_chunks: bool,
) -> Result<StatBlobResponse, tonic::Status> {
    Ok(f.client
        .stat_blob(with_token(
            StatBlobRequest {
                file_digest: digest.to_vec(),
                send_chunks,
            },
            tok,
        ))
        .await?
        .into_inner())
}

/// Drive `StatBlob` with `send_chunks=true`.
async fn stat_blob(
    f: &mut Fixture,
    digest: [u8; 32],
    tok: &str,
) -> Result<StatBlobResponse, tonic::Status> {
    stat_blob_with(f, digest, tok, true).await
}

/// Reassemble the file body from a `StatBlobResponse` the way the
/// FUSE fill task would: fetch each chunk from the backend, slice the
/// first/last per the response offsets.
async fn reassemble_stat(f: &Fixture, resp: &StatBlobResponse) -> Vec<u8> {
    let n = resp.chunks.len();
    let mut body = Vec::new();
    for (i, c) in resp.chunks.iter().enumerate() {
        let digest: [u8; 32] = c.digest.as_slice().try_into().unwrap();
        let stored = f.chunks.get(&digest).await.unwrap().unwrap();
        let bytes = rio_store::cas::decode_stored_chunk(&digest, stored).unwrap();
        assert_eq!(
            bytes.len() as u64,
            c.size,
            "ChunkMeta.size is plaintext size"
        );
        let start = if i == 0 {
            resp.first_chunk_skip as usize
        } else {
            0
        };
        let end = if i + 1 == n {
            resp.last_chunk_take as usize
        } else {
            bytes.len()
        };
        body.extend_from_slice(&bytes[start..end]);
    }
    body
}

/// Multi-chunk file straddling NAR-framed boundaries: the chunk
/// window covers exactly the file's bytes once first/last are sliced.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_chunked_window() {
    let mut f = fixture().await;
    // 100-byte chunks, 700-byte file at offset 64: spans chunks 0-7.
    let b = make_blob_nar(700, 21);
    let digest = seed_blob(&f, "sb-chunked", &b, Some(100)).await;
    let tok = token(f.tenant_a);

    let resp = stat_blob(&mut f, digest, &tok).await.unwrap();
    assert_eq!(resp.chunks.len(), 8);
    assert_eq!(resp.first_chunk_skip, 64);
    // File ends at NAR byte 764; chunk 7 starts at 700; 764-700=64.
    assert_eq!(resp.last_chunk_take, 64);
    let body = reassemble_stat(&f, &resp).await;
    assert_eq!(body, b.file);
    assert_eq!(<[u8; 32]>::from(blake3::hash(&body)), digest);
}

/// Single-chunk window: skip and take both apply to the same chunk.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_single_chunk() {
    let mut f = fixture().await;
    let b = make_blob_nar(16, 22);
    let digest = seed_blob(&f, "sb-single", &b, Some(1 << 20)).await;
    let tok = token(f.tenant_a);

    let resp = stat_blob(&mut f, digest, &tok).await.unwrap();
    assert_eq!(resp.chunks.len(), 1);
    assert_eq!(resp.first_chunk_skip, 64);
    assert_eq!(resp.last_chunk_take, 80);
    let body = reassemble_stat(&f, &resp).await;
    assert_eq!(body, b.file);
}

/// Zero-byte file: empty chunk list, both offsets 0.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_zero_byte_file() {
    let mut f = fixture().await;
    let b = make_blob_nar(0, 23);
    let digest = seed_blob(&f, "sb-zero", &b, Some(50)).await;
    let tok = token(f.tenant_a);

    let resp = stat_blob(&mut f, digest, &tok).await.unwrap();
    assert!(resp.chunks.is_empty());
    assert_eq!(resp.first_chunk_skip, 0);
    assert_eq!(resp.last_chunk_take, 0);
}

/// Two `file_blobs` rows for one digest resolve deterministically:
/// the lowest `store_path_hash` referrer wins, so resolution cannot be
/// steered by insertion order. The second row binds the digest to a
/// DIFFERENT path's window (the upload path now rejects creating such
/// a row — this models a legacy/poisoned binding), making the winner
/// observable through the returned chunk list.
#[tokio::test]
async fn stat_blob_two_referrers_deterministic_winner() {
    let mut f = fixture().await;
    let b1 = make_blob_nar(300, 31);
    let digest = seed_blob(&f, "det-a", &b1, Some(100)).await;
    let b2 = make_blob_nar(300, 32);
    let d2 = seed_blob(&f, "det-b", &b2, Some(100)).await;
    assert_ne!(digest, d2);
    // Bind b1's digest to det-b's window too.
    let hash_a = sha2::Sha256::digest(b"det-a").to_vec();
    let hash_b = sha2::Sha256::digest(b"det-b").to_vec();
    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(digest.as_slice())
    .bind(&hash_b)
    .bind(BLOB_NAR_OFFSET as i64)
    .bind(300i64)
    .execute(&f.db.pool)
    .await
    .unwrap();

    let tok = token(f.tenant_a);
    let resp = stat_blob(&mut f, digest, &tok).await.unwrap();
    // The winner's first window chunk identifies which row resolved:
    // the two NARs differ in content, so their chunk digests differ.
    let expected_nar = if hash_a < hash_b { &b1.nar } else { &b2.nar };
    let first_chunk: [u8; 32] = blake3::hash(&expected_nar[0..100]).into();
    assert_eq!(
        resp.chunks[0].digest,
        first_chunk.to_vec(),
        "resolution must pick the lowest store_path_hash referrer"
    );
}

/// `send_chunks=false` is a presence probe: empty response on a known
/// digest, NotFound otherwise.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_presence_probe() {
    let mut f = fixture().await;
    let b = make_blob_nar(64, 24);
    let digest = seed_blob(&f, "sb-probe", &b, Some(100)).await;
    let tok = token(f.tenant_a);

    let resp = stat_blob_with(&mut f, digest, &tok, false).await.unwrap();
    assert!(resp.chunks.is_empty());

    let unknown: [u8; 32] = blake3::hash(b"sb-nope").into();
    let err = stat_blob_with(&mut f, unknown, &tok, false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// Inline manifests have no chunk list: FAILED_PRECONDITION steers
/// the caller to ReadBlob. Presence probe doesn't classify, so it
/// still succeeds.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_inline_failed_precondition() {
    let mut f = fixture().await;
    let b = make_blob_nar(64, 25);
    let digest = seed_blob(&f, "sb-inline", &b, None).await;
    let tok = token(f.tenant_a);

    let err = stat_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::FailedPrecondition);

    let resp = stat_blob_with(&mut f, digest, &tok, false).await.unwrap();
    assert!(resp.chunks.is_empty());
}

/// Cross-tenant: a digest tenant A produced is NotFound for tenant B,
/// same status as an unknown digest.
// r[verify store.castore.blob-stat]
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn stat_blob_tenant_scoped() {
    let mut f = fixture().await;
    let b = make_blob_nar(64, 26);
    let digest = seed_blob(&f, "sb-tenant", &b, Some(100)).await;
    let tok_b = token(f.tenant_b);

    let err = stat_blob(&mut f, digest, &tok_b).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
}

/// `file_blobs.size` overrunning a *chunked* NAR is DATA_LOSS — the
/// inline-path overrun test doesn't reach `build_chunk_plan`, which
/// `read_blob` and `stat_blob` share.
// r[verify store.castore.blob-stat]
// r[verify store.castore.blob-read]
#[tokio::test]
async fn stat_blob_size_overruns_chunked_data_loss() {
    let mut f = fixture().await;
    let b = make_blob_nar(200, 27);
    let digest = seed_blob(&f, "sb-overrun", &b, Some(50)).await;
    sqlx::query("UPDATE file_blobs SET size = $1")
        .bind(1_000_000i64)
        .execute(&f.db.pool)
        .await
        .unwrap();
    let tok = token(f.tenant_a);

    let err = stat_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
    let err = read_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
}

/// A manifest with neither `inline_blob` nor `manifest_data` is
/// corrupt PG state — the commit txn always writes one. Both RPCs
/// report DATA_LOSS.
// r[verify store.castore.blob-stat]
// r[verify store.castore.blob-read]
#[tokio::test]
async fn stat_blob_neither_inline_nor_chunked_data_loss() {
    let mut f = fixture().await;
    let b = make_blob_nar(64, 28);
    let digest = seed_blob(&f, "sb-neither", &b, None).await;
    sqlx::query("UPDATE manifests SET inline_blob = NULL")
        .execute(&f.db.pool)
        .await
        .unwrap();
    let tok = token(f.tenant_a);

    let err = stat_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
    let err = read_blob(&mut f, digest, &tok).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::DataLoss);
}

/// Reassemble across chunk geometries and verify blake3 round-trips.
/// Fixed cases rather than proptest: each shrink would re-seed a PG
/// fixture, and these cases already cover the boundary classes.
// r[verify store.castore.blob-stat]
#[tokio::test]
async fn stat_blob_window_round_trips_varied_geometry() {
    let mut f = fixture().await;
    let tok = token(f.tenant_a);
    // (file_len, chunk_size): file < / == / > chunk, chunk-aligned
    // ends, 1-byte chunks, multi-chunk straddle.
    let cases = [
        (1, 64),
        (63, 64),
        (64, 64),
        (65, 64),
        (128, 64),
        (700, 100),
        (700, 1),
        (1, 4096),
        (5000, 333),
    ];
    for (i, &(file_len, chunk_size)) in cases.iter().enumerate() {
        let b = make_blob_nar(file_len, 30 + i as u8);
        let digest = seed_blob(&f, &format!("sb-prop-{i}"), &b, Some(chunk_size)).await;
        let resp = stat_blob(&mut f, digest, &tok).await.unwrap();
        let body = reassemble_stat(&f, &resp).await;
        assert_eq!(
            body, b.file,
            "case {i}: file_len={file_len} chunk={chunk_size}"
        );
        assert_eq!(
            <[u8; 32]>::from(blake3::hash(&body)),
            digest,
            "case {i}: digest mismatch"
        );
    }
}

// ──────────────── sig-visibility fallback (substitution-only paths) ────────────────
//
// Substituted paths deliberately get ZERO `path_tenants` rows
// (substitute.rs persists with `tenant: None`); their cross-tenant
// visibility is signature-gated. The castore read surface must honor
// the SAME per-caller predicate as the validity surface
// (`sig_visibility_gate`), or a path is reported valid but every
// castore read of it fails — the valid-but-unreadable loop
// `r[store.tenant.valid-paths-filter]` forbids.

/// Trust `key_entry` for `tenant` via a `tenant_upstreams` row — the
/// trust source `tenant_trusted_set` reads. No substituter is wired;
/// fixtures are pre-seeded, not miss-then-fetch.
async fn trust_upstream_key(pool: &sqlx::PgPool, tenant: uuid::Uuid, url: &str, key_entry: &str) {
    sqlx::query("INSERT INTO tenant_upstreams (tenant_id, url, trusted_keys) VALUES ($1, $2, $3)")
        .bind(tenant)
        .bind(url)
        .bind(vec![key_entry.to_string()])
        .execute(pool)
        .await
        .unwrap();
}

/// Insert a `narinfo` row for a substitution-only path: ZERO
/// `path_tenants` rows, optionally signed by `signer` over the exact
/// stored `(store_path, nar_hash, nar_size, references)` tuple — what
/// the sig-visibility predicate verifies. `store_path_hash` is sha256
/// of the FULL store path, matching the gate's hashing convention so
/// the validity and read surfaces look at the same junction rows.
async fn seed_subst_only_narinfo(
    pool: &sqlx::PgPool,
    store_path: &str,
    nar_size: i64,
    signer: Option<&rio_store::signing::Signer>,
) -> Vec<u8> {
    let path_hash = sha2::Sha256::digest(store_path.as_bytes()).to_vec();
    let nar_hash: [u8; 32] = path_hash.as_slice().try_into().unwrap(); // synthetic
    let sigs: Vec<String> = signer
        .map(|s| {
            let fp = rio_nix::narinfo::fingerprint(store_path, &nar_hash, nar_size as u64, &[]);
            vec![s.sign(&fp)]
        })
        .unwrap_or_default();
    sqlx::query(
        "INSERT INTO narinfo (store_path_hash, store_path, nar_hash, nar_size, signatures) \
         VALUES ($1, $2, $1, $3, $4) ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .bind(store_path)
    .bind(nar_size)
    .bind(&sigs)
    .execute(pool)
    .await
    .unwrap();
    path_hash
}

/// [`seed_blob`] shape for a SUBSTITUTION-ONLY path: same
/// narinfo/manifests/file_blobs rows, signed narinfo, no
/// `path_tenants` row. `store_path` must be a full store path.
async fn seed_subst_only_blob(
    f: &Fixture,
    store_path: &str,
    b: &BlobNar,
    chunk_size: Option<usize>,
    signer: Option<&rio_store::signing::Signer>,
) -> [u8; 32] {
    let path_hash =
        seed_subst_only_narinfo(&f.db.pool, store_path, b.nar.len() as i64, signer).await;
    if let Some(chunk_size) = chunk_size {
        sqlx::query("INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete')")
            .bind(&path_hash)
            .execute(&f.db.pool)
            .await
            .unwrap();
        let mut entries = Vec::new();
        for piece in b.nar.chunks(chunk_size) {
            let hash: [u8; 32] = blake3::hash(piece).into();
            f.chunks
                .put(&hash, rio_store::cas::compress_chunk(piece))
                .await
                .unwrap();
            entries.push(ManifestEntry {
                hash,
                size: piece.len() as u32,
            });
        }
        let chunk_list = Manifest { entries }.serialize();
        sqlx::query("INSERT INTO manifest_data (store_path_hash, chunk_list) VALUES ($1, $2)")
            .bind(&path_hash)
            .bind(&chunk_list)
            .execute(&f.db.pool)
            .await
            .unwrap();
    } else {
        sqlx::query(
            "INSERT INTO manifests (store_path_hash, status, inline_blob) \
             VALUES ($1, 'complete', $2)",
        )
        .bind(&path_hash)
        .bind(&b.nar)
        .execute(&f.db.pool)
        .await
        .unwrap();
    }
    sqlx::query(
        "INSERT INTO file_blobs (digest, store_path_hash, nar_offset, size) \
         VALUES ($1, $2, $3, $4)",
    )
    .bind(b.file_digest.as_slice())
    .bind(&path_hash)
    .bind(BLOB_NAR_OFFSET as i64)
    .bind(b.file.len() as i64)
    .execute(&f.db.pool)
    .await
    .unwrap();
    b.file_digest
}

/// [`put_dir`] shape for a SUBSTITUTION-ONLY backing path.
async fn seed_subst_only_dir(
    f: &Fixture,
    name: &str,
    children: &[(&str, [u8; 32])],
    signer: Option<&rio_store::signing::Signer>,
) -> [u8; 32] {
    let dir = Directory {
        directories: children
            .iter()
            .map(|(n, d)| DirectoryEntry {
                name: n.as_bytes().to_vec(),
                digest: d.to_vec(),
                size: 0,
            })
            .collect(),
        files: vec![],
        symlinks: vec![],
    };
    let digest: [u8; 32] = blake3::hash(name.as_bytes()).into();
    let store_path = format!("/nix/store/{name}");
    let path_hash = seed_subst_only_narinfo(&f.db.pool, &store_path, 0, signer).await;
    sqlx::query(
        "INSERT INTO manifests (store_path_hash, status) VALUES ($1, 'complete') \
         ON CONFLICT DO NOTHING",
    )
    .bind(&path_hash)
    .execute(&f.db.pool)
    .await
    .unwrap();
    sqlx::query("INSERT INTO directories (digest, body) VALUES ($1, $2) ON CONFLICT DO NOTHING")
        .bind(digest.as_slice())
        .bind(dir.encode_to_vec())
        .execute(&f.db.pool)
        .await
        .unwrap();
    sqlx::query(
        "INSERT INTO directory_paths (digest, store_path_hash) VALUES ($1, $2) \
         ON CONFLICT DO NOTHING",
    )
    .bind(digest.as_slice())
    .bind(&path_hash)
    .execute(&f.db.pool)
    .await
    .unwrap();
    digest
}

/// THE bug_072 regression: a substitution-only path whose signature
/// verifies against tenant A's trusted keys is VALID to A
/// (sig-visibility gate) — so every castore read RPC must serve it to
/// A too, or the scheduler never re-registers it and castore mounts
/// of substituted inputs fail forever. Tenant B (no matching trusted
/// key) must stay denied on every RPC: the fallback is per-caller,
/// never "substituted ⇒ global".
// r[verify store.castore.tenant-scope+3]
// r[verify store.substitute.tenant-sig-visibility+2]
#[tokio::test]
async fn sig_visible_substituted_paths_readable() {
    let mut f = fixture().await;
    let k = rio_store::signing::Signer::from_seed("key-subst-k", &[0x4Bu8; 32]);
    trust_upstream_key(
        &f.db.pool,
        f.tenant_a,
        "https://cache-k.example",
        &k.trusted_key_entry(),
    )
    .await;
    // Tenant B trusts a DIFFERENT key — nonempty trusted set, so the
    // denial below exercises sig-verify failure, not the empty-set
    // fast path.
    trust_upstream_key(
        &f.db.pool,
        f.tenant_b,
        "https://cache-j.example",
        "key-j:aaaa",
    )
    .await;

    let b_inline = make_blob_nar(700, 60);
    let blob_inline =
        seed_subst_only_blob(&f, "/nix/store/subst-rb-inline", &b_inline, None, Some(&k)).await;
    let b_chunked = make_blob_nar(300, 61);
    let blob_chunked = seed_subst_only_blob(
        &f,
        "/nix/store/subst-rb-chunked",
        &b_chunked,
        Some(100),
        Some(&k),
    )
    .await;
    let child = seed_subst_only_dir(&f, "subst-child", &[], Some(&k)).await;
    let root = seed_subst_only_dir(&f, "subst-root", &[("c", child)], Some(&k)).await;

    let tok_a = token(f.tenant_a);
    let tok_b = token(f.tenant_b);

    // — Tenant A (sig-visible): every read RPC serves the path —
    let body = read_blob(&mut f, blob_inline, &tok_a)
        .await
        .expect("ReadBlob must honor sig-visibility for substitution-only paths");
    assert_eq!(body, b_inline.file);

    let resp = stat_blob_with(&mut f, blob_chunked, &tok_a, false)
        .await
        .expect("StatBlob probe must honor sig-visibility");
    assert!(resp.chunks.is_empty());
    let resp = stat_blob(&mut f, blob_chunked, &tok_a)
        .await
        .expect("StatBlob send_chunks must honor sig-visibility");
    let reassembled = reassemble_stat(&f, &resp).await;
    assert_eq!(reassembled, b_chunked.file);

    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: false,
                digests: vec![],
            },
            &tok_a,
        ))
        .await
        .expect("non-recursive GetDirectory must honor sig-visibility");
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 1);

    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: true,
                digests: vec![],
            },
            &tok_a,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 2, "recursive walk reaches sig-visible child");

    let r = f
        .client
        .has_blobs(with_token(
            HasBlobsRequest {
                digests: vec![blob_inline.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0b1], "HasBlobs must honor sig-visibility");
    let r = f
        .client
        .has_directories(with_token(
            HasDirectoriesRequest {
                digests: vec![root.to_vec(), child.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(
        r.bitmap,
        vec![0b11],
        "HasDirectories must honor sig-visibility"
    );

    // — Tenant B (key not trusted): everything stays denied —
    let err = read_blob(&mut f, blob_inline, &tok_b).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "B: ReadBlob denied");
    let err = stat_blob(&mut f, blob_chunked, &tok_b).await.unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "B: StatBlob denied");
    let err = stat_blob_with(&mut f, blob_chunked, &tok_b, false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "B: probe denied");
    let err = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: false,
                digests: vec![],
            },
            &tok_b,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound, "B: GetDirectory denied");
    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(root.to_vec())),
                recursive: true,
                digests: vec![],
            },
            &tok_b,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert!(bodies.is_empty(), "B: recursive walk streams nothing");
    let r = f
        .client
        .has_blobs(with_token(
            HasBlobsRequest {
                digests: vec![blob_inline.to_vec()],
            },
            &tok_b,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0], "B: HasBlobs zero");
    let r = f
        .client
        .has_directories(with_token(
            HasDirectoriesRequest {
                digests: vec![root.to_vec()],
            },
            &tok_b,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0], "B: HasDirectories zero");
}

/// Security invariant: the fallback applies ONLY to substitution-only
/// paths. A path with a `path_tenants` row for ANOTHER tenant is
/// junction-gated, full stop — a trusted signature must NOT bypass
/// the I-217 isolation policy (built-by-another ⇒ hidden), matching
/// `sig_visibility_gate`'s precedence.
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn sig_fallback_requires_substitution_only() {
    let mut f = fixture().await;
    let k = rio_store::signing::Signer::from_seed("key-subst-k", &[0x4Bu8; 32]);
    trust_upstream_key(
        &f.db.pool,
        f.tenant_a,
        "https://cache-k.example",
        &k.trusted_key_entry(),
    )
    .await;

    // Signed by K (which A trusts) but BUILT by tenant B: junction row
    // for B only.
    let b = make_blob_nar(64, 62);
    let blob = seed_subst_only_blob(&f, "/nix/store/built-by-b", &b, None, Some(&k)).await;
    let dir = seed_subst_only_dir(&f, "built-by-b-dir", &[], Some(&k)).await;
    for path in ["/nix/store/built-by-b", "/nix/store/built-by-b-dir"] {
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(sha2::Sha256::digest(path.as_bytes()).as_slice())
            .bind(f.tenant_b)
            .execute(&f.db.pool)
            .await
            .unwrap();
    }
    let tok_a = token(f.tenant_a);
    let tok_b = token(f.tenant_b);

    // A: denied everywhere despite trusting the signature.
    let err = read_blob(&mut f, blob, &tok_a).await.unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::NotFound,
        "built-by-another path must stay junction-gated for ReadBlob"
    );
    let err = stat_blob_with(&mut f, blob, &tok_a, false)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);
    let r = f
        .client
        .has_blobs(with_token(
            HasBlobsRequest {
                digests: vec![blob.to_vec()],
            },
            &tok_a,
        ))
        .await
        .unwrap()
        .into_inner();
    assert_eq!(r.bitmap, vec![0], "HasBlobs must not sig-bypass junctions");
    let err = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(dir.to_vec())),
                recursive: false,
                digests: vec![],
            },
            &tok_a,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::NotFound);

    // B (the owner): junction path still works.
    let body = read_blob(&mut f, blob, &tok_b).await.unwrap();
    assert_eq!(body, b.file);
}

/// Matrix pin: the validity surface (`QueryPathInfo`, which applies
/// `sig_visibility_gate`) and the castore read surface
/// (ReadBlob/HasBlobs) must give the SAME verdict for the same
/// (tenant, path) — `r[store.tenant.valid-paths-filter]`'s "a path
/// MUST NOT be reported valid to a caller whose castore reads of it
/// would fail". Four fixture classes × two tenants.
// r[verify store.tenant.valid-paths-filter]
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn read_surface_matches_sig_visibility_gate() {
    use rio_proto::StoreServiceServer;
    use rio_proto::store::store_service_client::StoreServiceClient;
    use rio_proto::types::QueryPathInfoRequest;
    use rio_store::grpc::StoreServiceImpl;

    /// StoreService with a fake JWT interceptor pinning `tenant_id` —
    /// how the gateway's identity reaches QueryPathInfo in production.
    async fn spawn_store_with_fake_jwt(
        pool: sqlx::PgPool,
        tenant_id: uuid::Uuid,
    ) -> (StoreServiceClient<Channel>, tokio::task::JoinHandle<()>) {
        let fake_interceptor = move |mut req: tonic::Request<()>| {
            req.extensions_mut().insert(rio_auth::jwt::TenantClaims {
                sub: tenant_id,
                iat: 1_700_000_000,
                exp: 9_999_999_999,
                jti: "dir-matrix-fake".into(),
            });
            Ok(req)
        };
        let router = Server::builder()
            .layer(tonic::service::InterceptorLayer::new(fake_interceptor))
            .add_service(StoreServiceServer::new(StoreServiceImpl::new(pool)));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;
        let channel = Channel::from_shared(format!("http://{addr}"))
            .unwrap()
            .connect()
            .await
            .unwrap();
        (StoreServiceClient::new(channel), server)
    }

    let mut f = fixture().await;
    let k = rio_store::signing::Signer::from_seed("key-matrix-k", &[0x6Bu8; 32]);
    trust_upstream_key(
        &f.db.pool,
        f.tenant_a,
        "https://cache-k.example",
        &k.trusted_key_entry(),
    )
    .await;
    // B trusts a different key.
    trust_upstream_key(
        &f.db.pool,
        f.tenant_b,
        "https://cache-j.example",
        "key-j:aaaa",
    )
    .await;

    // Fixture classes. Store paths must parse (QueryPathInfo
    // validates), hence test_store_path().
    let p1 = rio_test_support::fixtures::test_store_path("matrix-subst-signed");
    let p2 = rio_test_support::fixtures::test_store_path("matrix-subst-unsigned");
    let p3 = rio_test_support::fixtures::test_store_path("matrix-built-by-a");
    let p4 = rio_test_support::fixtures::test_store_path("matrix-built-by-b-signed");
    let b1 = make_blob_nar(32, 63);
    let b2 = make_blob_nar(32, 64);
    let b3 = make_blob_nar(32, 65);
    let b4 = make_blob_nar(32, 66);
    let d1 = seed_subst_only_blob(&f, &p1, &b1, None, Some(&k)).await;
    let d2 = seed_subst_only_blob(&f, &p2, &b2, None, None).await;
    let d3 = seed_subst_only_blob(&f, &p3, &b3, None, None).await;
    let d4 = seed_subst_only_blob(&f, &p4, &b4, None, Some(&k)).await;
    for (path, tenant) in [(&p3, f.tenant_a), (&p4, f.tenant_b)] {
        sqlx::query("INSERT INTO path_tenants (store_path_hash, tenant_id) VALUES ($1, $2)")
            .bind(sha2::Sha256::digest(path.as_bytes()).as_slice())
            .bind(tenant)
            .execute(&f.db.pool)
            .await
            .unwrap();
    }

    let cases: [(&str, [u8; 32]); 4] = [(&p1, d1), (&p2, d2), (&p3, d3), (&p4, d4)];
    for tenant in [f.tenant_a, f.tenant_b] {
        let (mut store_client, server) = spawn_store_with_fake_jwt(f.db.pool.clone(), tenant).await;
        let _guard = scopeguard::guard(server, |h| h.abort());
        let tok = token(tenant);
        for (path, digest) in &cases {
            let valid = store_client
                .query_path_info(QueryPathInfoRequest {
                    store_path: (*path).to_string(),
                })
                .await
                .is_ok();
            let readable = read_blob(&mut f, *digest, &tok).await.is_ok();
            let has = f
                .client
                .has_blobs(with_token(
                    HasBlobsRequest {
                        digests: vec![digest.to_vec()],
                    },
                    &tok,
                ))
                .await
                .unwrap()
                .into_inner()
                .bitmap
                == vec![0b1];
            assert_eq!(
                valid, readable,
                "tenant {tenant} path {path}: QueryPathInfo={valid} but ReadBlob={readable} \
                 — validity and castore reads must agree"
            );
            assert_eq!(
                valid, has,
                "tenant {tenant} path {path}: QueryPathInfo={valid} but HasBlobs={has}"
            );
        }
    }
}

/// Three-level DAG with a self-referencing cycle: BFS terminates and
/// streams each body exactly once.
// r[verify store.castore.directory-rpc]
#[tokio::test]
async fn recursive_walk_dedupes_and_terminates_on_cycle() {
    let mut f = fixture().await;
    // c references b; b references c (cycle) and a; a is a leaf.
    let a = put_dir(&f.db.pool, f.tenant_a, "a", &[], &[]).await;
    // Pre-compute digests because put_dir derives from name.
    let bd: [u8; 32] = blake3::hash(b"b").into();
    let cd: [u8; 32] = blake3::hash(b"c").into();
    put_dir(&f.db.pool, f.tenant_a, "b", &[("c", cd), ("a", a)], &[]).await;
    put_dir(&f.db.pool, f.tenant_a, "c", &[("b", bd)], &[]).await;
    let tok = token(f.tenant_a);

    let resp = f
        .client
        .get_directory(with_token(
            GetDirectoryRequest {
                by_what: Some(get_directory_request::ByWhat::Digest(cd.to_vec())),
                recursive: true,
                digests: vec![],
            },
            &tok,
        ))
        .await
        .unwrap();
    let bodies: Vec<Directory> = resp.into_inner().filter_map(|r| r.ok()).collect().await;
    assert_eq!(bodies.len(), 3, "cycle-safe and deduped");
}
