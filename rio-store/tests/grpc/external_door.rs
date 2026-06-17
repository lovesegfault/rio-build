//! External castore door (ADR-024 P2) — service-level auth proof.
//!
//! The helm door (`castore-door.yaml`) routes exactly four gRPC
//! surfaces to rio-store: `DirectoryService` (whole), `ChunkService`
//! (whole), `DrvBlobService` (whole), and
//! `StoreService/PutPathChunked`. The door carries no auth of its own
//! — these tests prove the production server stack behind it (the
//! same `jwt_interceptor` layer + HMAC verifiers main.rs wires)
//! authenticates every routed RPC:
//!
//! - a tenant-JWT client round-trips `Has*` (negotiation) and chunk
//!   retrieval through the door surface;
//! - every routed RPC rejects an anonymous caller
//!   (`UNAUTHENTICATED`, or `PERMISSION_DENIED` for the
//!   assignment-token-gated upload);
//! - a forged or expired JWT is rejected at the interceptor.
//!
//! The render side (the routed-surface allowlist itself) is pinned by
//! `nix/tests/helm/28-castore-door.sh`.

use super::*;

use std::sync::RwLock;

use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::store::directory_service_client::DirectoryServiceClient;
use rio_proto::store::drv_blob_service_client::DrvBlobServiceClient;
use rio_proto::types::{
    GetChunkRequest, GetChunksRequest, GetDirectoryRequest, GetDrvBlobRequest, HasBlobsRequest,
    HasChunksRequest, HasDirectoriesRequest, HasDrvsRequest, PutDrvBlobsRequest,
    PutPathChunkedRequest, ReadBlobRequest, StatBlobRequest, get_directory_request,
};
use rio_proto::{ChunkServiceServer, DirectoryServiceServer, DrvBlobServiceServer};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{ChunkServiceImpl, DirectoryServiceImpl, DrvBlobServiceImpl};
use rio_store::test_helpers::seed_tenant;

/// Server-side ed25519 seed: tokens signed with this key verify.
const JWT_SEED: [u8; 32] = [0x07; 32];
/// A key the server does NOT hold — tokens signed with it must be
/// rejected exactly like anonymous requests, with UNAUTHENTICATED.
const FORGED_JWT_SEED: [u8; 32] = [0x08; 32];

/// HMAC assignment key — armed so `PutPathChunked` enforces its
/// production posture (anonymous → PERMISSION_DENIED, never accepted).
const HMAC_KEY: &[u8] = b"external-door-test-key-32-bytes!";

fn tenant_jwt(seed: &[u8; 32], sub: uuid::Uuid, exp: i64) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(seed);
    rio_auth::jwt::sign(
        &rio_auth::jwt::TenantClaims {
            sub,
            iat: 1_700_000_000,
            exp,
            jti: uuid::Uuid::nil().to_string(),
        },
        &key,
    )
    .expect("sign cannot fail with a valid key")
}

fn with_jwt<T>(msg: T, token: &str) -> tonic::Request<T> {
    let mut r = tonic::Request::new(msg);
    r.metadata_mut().insert(
        rio_common::grpc::TENANT_TOKEN_HEADER,
        token.parse().expect("JWT is ASCII"),
    );
    r
}

/// The production door stack: jwt_interceptor layer (pubkey armed) +
/// StoreService (HMAC armed) + ChunkService + DirectoryService +
/// DrvBlobService, all sharing one cache/backend — the same wiring
/// as rio-store main.rs.
struct DoorSession {
    db: TestDb,
    tenant: uuid::Uuid,
    store: StoreServiceClient<Channel>,
    chunk: ChunkServiceClient<Channel>,
    directory: DirectoryServiceClient<Channel>,
    drv_blob: DrvBlobServiceClient<Channel>,
    backend: Arc<MemoryChunkBackend>,
    server: tokio::task::JoinHandle<()>,
}

impl Drop for DoorSession {
    fn drop(&mut self) {
        self.server.abort();
    }
}

impl DoorSession {
    async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "door-tenant").await;
        let backend = mem_backend();
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));
        let hmac = Arc::new(rio_auth::hmac::HmacVerifier::from_key(HMAC_KEY.to_vec()));

        let store_service = StoreServiceImpl::new(db.pool.clone())
            .with_chunk_cache(Arc::clone(&cache))
            .with_hmac_verifier(Arc::clone(&hmac));
        let chunk_service = ChunkServiceImpl::new(
            db.pool.clone(),
            Some(Arc::clone(&cache)),
            Some(Arc::clone(&hmac)),
        );
        let directory_service =
            DirectoryServiceImpl::new(db.pool.clone(), Some(Arc::clone(&hmac)), Some(cache), None);
        let drv_blob_service = DrvBlobServiceImpl::new(db.pool.clone(), Some(hmac));

        let pubkey = ed25519_dalek::SigningKey::from_bytes(&JWT_SEED).verifying_key();
        let router = Server::builder()
            // The SAME permissive-on-absent JWT layer main.rs installs:
            // present tokens are verified (bad signature/expiry →
            // UNAUTHENTICATED before any handler runs); absent tokens
            // pass through to the per-RPC identity gates.
            .layer(tonic::service::InterceptorLayer::new(
                rio_auth::jwt_interceptor::jwt_interceptor(Some(Arc::new(RwLock::new(pubkey)))),
            ))
            .add_service(StoreServiceServer::new(store_service))
            .add_service(ChunkServiceServer::new(chunk_service))
            .add_service(DirectoryServiceServer::new(directory_service))
            .add_service(DrvBlobServiceServer::new(drv_blob_service));
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;

        let channel = Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok(Self {
            db,
            tenant,
            store: StoreServiceClient::new(channel.clone()),
            chunk: ChunkServiceClient::new(channel.clone()),
            directory: DirectoryServiceClient::new(channel.clone()),
            drv_blob: DrvBlobServiceClient::new(channel),
            backend,
            server,
        })
    }

    fn jwt(&self) -> String {
        tenant_jwt(&JWT_SEED, self.tenant, 9_999_999_999)
    }
}

/// Builder assignment token signed with [`HMAC_KEY`]: the production
/// upload identity, carrying `tenant` so manifest completion binds
/// the `chunk_tenants` junction rows `HasChunks` answers from.
fn assignment_token_for(tenant: uuid::Uuid, outputs: Vec<String>) -> String {
    rio_auth::hmac::HmacSigner::from_key(HMAC_KEY.to_vec()).sign(
        &rio_auth::hmac::AssignmentClaims {
            executor_id: "door-test".into(),
            drv_hash: "00".repeat(32),
            expected_outputs: outputs,
            is_ca: false,
            expiry_unix: 9_999_999_999,
            tenant: Some(tenant.to_string()),
            input_closure_digest: String::new(),
        },
    )
}

/// An external client holding a tenant JWT round-trips the door's
/// negotiation + retrieval surface: HasChunks answers presence over
/// the junction the tenant's own completed upload wrote, GetChunk
/// returns the uncompressed bytes, and the tenant-scoped Has* RPCs
/// answer instead of rejecting.
///
/// The chunks become tenant-visible through the REAL completion path
/// — a chunked upload under the tenant's assignment token, whose
/// manifest-completion transaction marks the chunks durable AND
/// writes the `chunk_tenants` rows — not a hand-inserted junction
/// row, so this proves the presence bit under junction semantics
/// end-to-end (`r[store.chunk.has-chunks-tenant]`).
// r[verify store.chunk.has-chunks-authenticated+1]
// r[verify store.chunk.has-chunks-tenant]
#[tokio::test]
async fn door_tenant_jwt_round_trips_has_and_get() -> TestResult {
    let mut s = DoorSession::new().await?;
    let jwt = s.jwt();

    // Production write path: chunked upload (≥256 KiB NARs FastCDC-
    // chunk) authenticated as the door tenant. Completion flips the
    // chunks durable and binds them to the tenant.
    let (nar, info, _) = make_large_nar(60, 512 * 1024);
    let path = info.store_path.to_string();
    let token = assignment_token_for(s.tenant, vec![path]);
    assert!(put_path_with_token(&mut s.store, info, nar, &token).await?);

    let hashes: Vec<Vec<u8>> = sqlx::query_scalar("SELECT blake3_hash FROM chunks")
        .fetch_all(&s.db.pool)
        .await?;
    assert!(hashes.len() >= 2, "512 KiB NAR must chunk into >1 chunk");
    let hash: [u8; 32] = hashes[0].as_slice().try_into().expect("blake3 is 32 bytes");

    // Negotiation: HasChunks bitmap — the tenant's own chunk sets bit
    // 0, an unknown digest leaves bit 1 clear.
    let unknown = *blake3::hash(b"never uploaded").as_bytes();
    let bitmap = s
        .chunk
        .has_chunks(with_jwt(
            HasChunksRequest {
                digests: vec![hash.to_vec(), unknown.to_vec()],
            },
            &jwt,
        ))
        .await?
        .into_inner()
        .bitmap;
    assert_eq!(bitmap, vec![0b01], "bit 0 set (present), bit 1 clear");

    // The stored object is the zstd-at-rest form (the production
    // writer compressed it), so the retrieval below also proves the
    // door serves decoded plaintext, not stored bytes.
    let stored = s.backend.get(&hash).await?.expect("uploaded chunk");
    assert_eq!(
        &stored[..4],
        &[0x28, 0xB5, 0x2F, 0xFD],
        "stored form is zstd-framed"
    );

    // Retrieval: GetChunk streams the uncompressed bytes back —
    // digests are over the UNCOMPRESSED content, so hashing the
    // response proves the decode.
    let mut stream = s
        .chunk
        .get_chunk(with_jwt(
            GetChunkRequest {
                digest: hash.to_vec(),
            },
            &jwt,
        ))
        .await?
        .into_inner();
    let mut got = Vec::new();
    while let Some(resp) = stream.message().await? {
        got.extend_from_slice(&resp.data);
    }
    assert_eq!(
        *blake3::hash(&got).as_bytes(),
        hash,
        "round-trip serves the uncompressed bytes"
    );

    // Tenant-scoped negotiation RPCs ANSWER for a JWT caller (empty
    // store → all-clear bitmaps) instead of rejecting — the same
    // requests are UNAUTHENTICATED without the token (next test).
    let dirs = s
        .directory
        .has_directories(with_jwt(
            HasDirectoriesRequest {
                digests: vec![hash.to_vec()],
            },
            &jwt,
        ))
        .await?
        .into_inner();
    assert_eq!(dirs.bitmap, vec![0]);
    let blobs = s
        .directory
        .has_blobs(with_jwt(
            HasBlobsRequest {
                digests: vec![hash.to_vec()],
            },
            &jwt,
        ))
        .await?
        .into_inner();
    assert_eq!(blobs.bitmap, vec![0]);
    Ok(())
}

/// Every RPC the door routes rejects an anonymous caller. The door
/// adds no auth, so this is THE access control for external traffic:
/// a regression here fail-opens the castore surface to the internet.
// r[verify store.castore.tenant-scope+3]
#[tokio::test]
async fn door_rejects_anonymous_on_every_routed_rpc() -> TestResult {
    let mut s = DoorSession::new().await?;
    let digest = vec![0u8; 32];

    use tonic::Code::Unauthenticated;
    // DirectoryService — all five RPCs resolve a tenant first.
    let err = s
        .directory
        .get_directory(GetDirectoryRequest {
            by_what: Some(get_directory_request::ByWhat::Digest(digest.clone())),
            recursive: false,
            digests: vec![],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "GetDirectory: {err:?}");
    let err = s
        .directory
        .has_directories(HasDirectoriesRequest {
            digests: vec![digest.clone()],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "HasDirectories: {err:?}");
    let err = s
        .directory
        .has_blobs(HasBlobsRequest {
            digests: vec![digest.clone()],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "HasBlobs: {err:?}");
    let err = s
        .directory
        .read_blob(ReadBlobRequest {
            file_digest: digest.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "ReadBlob: {err:?}");
    let err = s
        .directory
        .stat_blob(StatBlobRequest {
            file_digest: digest.clone(),
            send_chunks: false,
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "StatBlob: {err:?}");

    // ChunkService — identity required on all three RPCs.
    let err = s
        .chunk
        .get_chunk(GetChunkRequest {
            digest: digest.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "GetChunk: {err:?}");
    let err = s
        .chunk
        .has_chunks(HasChunksRequest {
            digests: vec![digest.clone()],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "HasChunks: {err:?}");
    let (req_tx, req_rx) = mpsc::channel::<GetChunksRequest>(1);
    drop(req_tx);
    let err = s
        .chunk
        .get_chunks(ReceiverStream::new(req_rx))
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "GetChunks: {err:?}");

    // DrvBlobService — all three RPCs resolve a tenant first (same
    // ladder as DirectoryService via resolve_castore_tenant).
    let err = s
        .drv_blob
        .put_drv_blobs(PutDrvBlobsRequest { blobs: vec![] })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "PutDrvBlobs: {err:?}");
    let err = s
        .drv_blob
        .get_drv_blob(GetDrvBlobRequest {
            digest: digest.clone(),
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "GetDrvBlob: {err:?}");
    let err = s
        .drv_blob
        .has_drvs(HasDrvsRequest {
            digests: vec![digest.clone()],
        })
        .await
        .unwrap_err();
    assert_eq!(err.code(), Unauthenticated, "HasDrvs: {err:?}");

    // StoreService/PutPathChunked — the only StoreService method the
    // door routes. With the HMAC verifier armed (production posture)
    // an anonymous upload is refused before the first frame is read.
    let (tx, rx) = mpsc::channel::<PutPathChunkedRequest>(1);
    drop(tx);
    let err = s
        .store
        .put_path_chunked(ReceiverStream::new(rx))
        .await
        .unwrap_err();
    assert_eq!(
        err.code(),
        tonic::Code::PermissionDenied,
        "PutPathChunked: {err:?}"
    );
    Ok(())
}

/// Drive a chunked client stream through the door with ONLY a tenant
/// JWT attached (no assignment token — the `rio build` posture).
async fn send_chunked_jwt(
    client: &mut StoreServiceClient<Channel>,
    begin: rio_proto::types::PutPathChunkedBegin,
    frames: Vec<rio_proto::types::ChunkData>,
    jwt: &str,
) -> Result<Vec<bool>, tonic::Status> {
    use rio_proto::types::put_path_chunked_request;
    let (tx, rx) = mpsc::channel(8);
    tokio::spawn(async move {
        let _ = tx
            .send(PutPathChunkedRequest {
                msg: Some(put_path_chunked_request::Msg::Begin(begin)),
            })
            .await;
        for f in frames {
            let _ = tx
                .send(PutPathChunkedRequest {
                    msg: Some(put_path_chunked_request::Msg::Chunk(f)),
                })
                .await;
        }
    });
    let req = with_jwt(ReceiverStream::new(rx), jwt);
    client
        .put_path_chunked(req)
        .await
        .map(|r| r.into_inner().created)
}

/// ADR-024 P3: a tenant-JWT client (no assignment token) uploads a
/// SOURCE path through the armed door. The upload commits, the
/// `path_tenants` junction binds to the JWT tenant (the tenant's own
/// negotiation sees the root Directory afterwards), and the narinfo
/// records no deriver — the same shape a substituted path has.
// r[verify store.put.chunked-jwt-source]
#[tokio::test]
async fn door_tenant_jwt_uploads_source_path() -> TestResult {
    let mut s = DoorSession::new().await?;
    let jwt = s.jwt();

    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("src");
    crate::put_path_chunked::write_fixture_tree(&root, "nothing");
    let path = "/nix/store/ssssssssssssssssssssssssssssssss-door-src";
    let fx = crate::put_path_chunked::fixture_for_tree(&root, path, vec![]);
    let (mut begin, frames) =
        crate::put_path_chunked::assemble_begin(&[&fx], vec![], &Default::default());
    begin.deriver = String::new(); // sources have no producing derivation

    let created = send_chunked_jwt(&mut s.store, begin, frames, &jwt).await?;
    assert_eq!(created, vec![true]);

    // Junction row bound to the JWT tenant, not dangling.
    let junction: i64 = sqlx::query_scalar(
        "SELECT COUNT(*) FROM path_tenants WHERE store_path_hash = $1 AND tenant_id = $2",
    )
    .bind(
        rio_nix::store_path::StorePath::parse(path)?
            .sha256_digest()
            .to_vec(),
    )
    .bind(s.tenant)
    .fetch_one(&s.db.pool)
    .await?;
    assert_eq!(junction, 1, "junction binds to the JWT tenant");

    // Deriver-less narinfo (the substituted-path shape).
    let deriver: Option<String> =
        sqlx::query_scalar("SELECT deriver FROM narinfo WHERE store_path = $1")
            .bind(path)
            .fetch_one(&s.db.pool)
            .await?;
    assert_eq!(deriver, None, "source upload records no deriver");

    // Negotiation parity: the tenant's next `rio build` sees the root
    // Directory as present and skips the re-upload. The negotiation
    // key is the root Directory digest from the output's RootNode.
    let Some(rio_proto::castore::root_node::Node::DirDigest(root_digest)) =
        fx.output.root_node.as_ref().and_then(|r| r.node.clone())
    else {
        panic!("fixture root is a directory");
    };
    let dirs = s
        .directory
        .has_directories(with_jwt(
            HasDirectoriesRequest {
                digests: vec![root_digest],
            },
            &jwt,
        ))
        .await?
        .into_inner();
    assert_eq!(
        dirs.bitmap,
        vec![0b1],
        "root Directory present after commit"
    );
    Ok(())
}

/// The JWT rung is scoped to sources: a deriver-bound `Begin` under a
/// tenant JWT is rejected with PERMISSION_DENIED — builder provenance
/// (and the expected_outputs allowlist posture that rides with it)
/// requires an assignment token, so a tenant JWT can never bypass it.
// r[verify store.put.chunked-jwt-source]
#[tokio::test]
async fn door_jwt_cannot_claim_deriver_bound_upload() -> TestResult {
    let mut s = DoorSession::new().await?;
    let jwt = s.jwt();

    let dir = tempfile::TempDir::new()?;
    let root = dir.path().join("src");
    crate::put_path_chunked::write_fixture_tree(&root, "nothing");
    let path = "/nix/store/jjjjjjjjjjjjjjjjjjjjjjjjjjjjjjjj-door-forged";
    let fx = crate::put_path_chunked::fixture_for_tree(&root, path, vec![]);
    let (begin, frames) =
        crate::put_path_chunked::assemble_begin(&[&fx], vec![], &Default::default());
    // assemble_begin leaves the fixture DERIVER set — a JWT caller
    // claiming builder provenance.
    assert!(!begin.deriver.is_empty());

    let err = send_chunked_jwt(&mut s.store, begin, frames, &jwt)
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::PermissionDenied, "{err:?}");

    let committed: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM narinfo WHERE store_path = $1")
        .bind(path)
        .fetch_one(&s.db.pool)
        .await?;
    assert_eq!(committed, 0, "nothing committed");
    Ok(())
}

/// A token signed with the wrong key, and an expired token signed
/// with the RIGHT key, are both rejected at the interceptor with
/// UNAUTHENTICATED — they never reach a handler.
#[tokio::test]
async fn door_rejects_forged_and_expired_jwt() -> TestResult {
    let mut s = DoorSession::new().await?;
    let digest = vec![0u8; 32];

    let forged = tenant_jwt(&FORGED_JWT_SEED, s.tenant, 9_999_999_999);
    let err = s
        .chunk
        .has_chunks(with_jwt(
            HasChunksRequest {
                digests: vec![digest.clone()],
            },
            &forged,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "forged: {err:?}");

    let expired = tenant_jwt(&JWT_SEED, s.tenant, 1_700_000_001);
    let err = s
        .directory
        .has_blobs(with_jwt(
            HasBlobsRequest {
                digests: vec![digest],
            },
            &expired,
        ))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::Unauthenticated, "expired: {err:?}");
    Ok(())
}
