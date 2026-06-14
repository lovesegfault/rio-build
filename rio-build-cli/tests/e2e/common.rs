//! In-process cluster harness: REAL rio-store services (ephemeral
//! postgres via rio-test-support, memory chunk backend, the production
//! JWT interceptor AND an armed HMAC verifier) + the purpose-built
//! scheduler stub. The coordinator under test connects to it exactly
//! like a production client: tenant JWT on every RPC, negotiation and
//! uploads against the real castore surface under the production auth
//! posture (source uploads ride the PutPathChunked tenant-JWT rung).

use std::collections::HashMap;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex, RwLock};

use rio_build_cli::acks::ClusterAckTable;
use rio_build_cli::coordinator::clients::Clients;
use rio_build_cli::coordinator::{Coordinator, CoordinatorOpts, RunSummary};
use rio_build_cli::evalchan::EvalChannel;
use rio_proto::evaljob::ResultFrame;
use rio_proto::types::{
    DrvBlob, GetDrvBlobRequest, HasBitmap, HasDrvsRequest, PutDrvBlobsRequest, PutDrvBlobsResponse,
};
use rio_proto::{
    ChunkServiceServer, DirectoryServiceServer, DrvBlobService, DrvBlobServiceServer,
    SchedulerServiceServer, StoreServiceServer,
};
use rio_store::backend::{ChunkBackend, MemoryChunkBackend};
use rio_store::cas::ChunkCache;
use rio_store::grpc::{
    ChunkServiceImpl, DirectoryServiceImpl, DrvBlobServiceImpl, StoreServiceImpl,
};
use rio_store::test_helpers::seed_tenant;
use rio_test_support::TestDb;
use tonic::transport::Server;

use crate::stub_parent::{self, StubParent};
use crate::stub_scheduler::StubScheduler;

/// Server-side ed25519 seed: tokens signed with this key verify.
const JWT_SEED: [u8; 32] = [0x07; 32];

/// HMAC assignment key — armed on every store service so the cluster
/// runs the production auth posture: the coordinator's PutPathChunked
/// goes through the tenant-JWT source-upload rung
/// (`r[store.put.chunked-jwt-source]`), and builder-posture uploads
/// (the fetch test's output seed) need a real assignment token.
const HMAC_KEY: &[u8] = b"e2e-cluster-hmac-key-32-bytes!!!";

fn tenant_jwt(sub: uuid::Uuid) -> String {
    let key = ed25519_dalek::SigningKey::from_bytes(&JWT_SEED);
    rio_auth::jwt::sign(
        &rio_auth::jwt::TenantClaims {
            sub,
            iat: 1_700_000_000,
            exp: 9_999_999_999,
            jti: uuid::Uuid::nil().to_string(),
        },
        &key,
    )
    .expect("sign cannot fail with a valid key")
}

/// `DrvBlobService` pass-through that counts negotiation/upload RPCs —
/// the warm-path test's "zero uploads" assertion needs call counts,
/// not just end state.
pub struct CountingDrvBlob {
    inner: DrvBlobServiceImpl,
    pub has_calls: Arc<AtomicUsize>,
    pub put_calls: Arc<AtomicUsize>,
}

#[tonic::async_trait]
impl DrvBlobService for CountingDrvBlob {
    async fn put_drv_blobs(
        &self,
        request: tonic::Request<PutDrvBlobsRequest>,
    ) -> Result<tonic::Response<PutDrvBlobsResponse>, tonic::Status> {
        self.put_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.put_drv_blobs(request).await
    }

    async fn get_drv_blob(
        &self,
        request: tonic::Request<GetDrvBlobRequest>,
    ) -> Result<tonic::Response<DrvBlob>, tonic::Status> {
        self.inner.get_drv_blob(request).await
    }

    async fn has_drvs(
        &self,
        request: tonic::Request<HasDrvsRequest>,
    ) -> Result<tonic::Response<HasBitmap>, tonic::Status> {
        self.has_calls.fetch_add(1, Ordering::SeqCst);
        self.inner.has_drvs(request).await
    }
}

pub struct TestCluster {
    pub db: TestDb,
    pub tenant: uuid::Uuid,
    pub clients: Clients,
    pub sched: StubScheduler,
    pub drv_has_calls: Arc<AtomicUsize>,
    pub drv_put_calls: Arc<AtomicUsize>,
    pub cas: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
}

impl TestCluster {
    pub async fn new() -> anyhow::Result<Self> {
        let db = TestDb::new(&rio_store::MIGRATOR).await;
        let tenant = seed_tenant(&db.pool, "e2e-tenant").await;
        let backend = Arc::new(MemoryChunkBackend::new());
        let cache = Arc::new(ChunkCache::new(
            Arc::clone(&backend) as Arc<dyn ChunkBackend>
        ));

        // Production auth posture everywhere: JWT interceptor + armed
        // HMAC verifier on every service. The coordinator holds only a
        // tenant JWT, so its source uploads exercise the
        // PutPathChunked tenant-JWT rung, not dev mode.
        let hmac = Arc::new(rio_auth::hmac::HmacVerifier::from_key(HMAC_KEY.to_vec()));
        let store_service = StoreServiceImpl::new(db.pool.clone())
            .with_chunk_cache(Arc::clone(&cache))
            .with_hmac_verifier(Arc::clone(&hmac));
        let chunk_service = ChunkServiceImpl::new(
            db.pool.clone(),
            Some(Arc::clone(&cache)),
            Some(Arc::clone(&hmac)),
        );
        let directory_service = DirectoryServiceImpl::new(
            db.pool.clone(),
            Some(Arc::clone(&hmac)),
            Some(Arc::clone(&cache)),
            None,
        );
        let drv_blob = CountingDrvBlob {
            inner: DrvBlobServiceImpl::new(db.pool.clone(), Some(hmac)),
            has_calls: Arc::new(AtomicUsize::new(0)),
            put_calls: Arc::new(AtomicUsize::new(0)),
        };
        let drv_has_calls = Arc::clone(&drv_blob.has_calls);
        let drv_put_calls = Arc::clone(&drv_blob.put_calls);
        let sched = StubScheduler::new(db.pool.clone());

        let max = rio_common::grpc::max_message_size();
        let pubkey = ed25519_dalek::SigningKey::from_bytes(&JWT_SEED).verifying_key();
        let router = Server::builder()
            .layer(tonic::service::InterceptorLayer::new(
                rio_auth::jwt_interceptor::jwt_interceptor(Some(Arc::new(RwLock::new(pubkey)))),
            ))
            .add_service(
                StoreServiceServer::new(store_service)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(
                ChunkServiceServer::new(chunk_service)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(
                DirectoryServiceServer::new(directory_service)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(
                DrvBlobServiceServer::new(drv_blob)
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max),
            )
            .add_service(
                SchedulerServiceServer::new(sched.clone())
                    .max_decoding_message_size(max)
                    .max_encoding_message_size(max)
                    // The coordinator compresses SubmitBuild (ADR-024).
                    .accept_compressed(tonic::codec::CompressionEncoding::Zstd)
                    .send_compressed(tonic::codec::CompressionEncoding::Zstd),
            );
        let (addr, server) = rio_test_support::grpc::spawn_grpc_server_layered(router).await;

        let jwt = tenant_jwt(tenant);
        // `connect` takes bare host:port (the rio-proto client layer
        // prepends the scheme).
        let endpoint = addr.to_string();
        let clients = Clients::connect(&endpoint, &endpoint, Some(jwt)).await?;

        Ok(Self {
            db,
            tenant,
            clients,
            sched,
            drv_has_calls,
            drv_put_calls,
            cas: tempfile::tempdir()?,
            _server: server,
        })
    }

    /// A coordinator wired to this cluster: ack table under the
    /// cluster's CAS root (persists across coordinators — that's the
    /// warm path), null renderer.
    pub fn coordinator(&self, tweak: impl FnOnce(&mut CoordinatorOpts)) -> Coordinator {
        let mut opts = CoordinatorOpts::default();
        tweak(&mut opts);
        Coordinator {
            clients: self.clients.clone(),
            acks: Arc::new(Mutex::new(ClusterAckTable::open(
                self.cas.path(),
                "e2e-cluster|tenant",
                std::time::Duration::from_secs(3600),
            ))),
            cas_root: self.cas.path().to_path_buf(),
            opts,
            render: rio_build_cli::render::RenderHandle::null(),
        }
    }

    /// Run a full coordinator invocation against a scripted stub
    /// parent. Returns the summary and what the parent saw.
    pub async fn run(
        &self,
        coordinator: &mut Coordinator,
        script: HashMap<String, Vec<ResultFrame>>,
        attrs: &[&str],
    ) -> anyhow::Result<(RunSummary, StubParent)> {
        self.run_expanding(coordinator, script, HashMap::new(), attrs)
            .await
    }

    /// Like [`TestCluster::run`], but attrs in `expansions` answer with
    /// an `AttrsetExpansion` frame (the `.#checks`-style installable).
    pub async fn run_expanding(
        &self,
        coordinator: &mut Coordinator,
        script: HashMap<String, Vec<ResultFrame>>,
        expansions: HashMap<String, rio_proto::evaljob::AttrsetExpansion>,
        attrs: &[&str],
    ) -> anyhow::Result<(RunSummary, StubParent)> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        let parent = stub_parent::spawn_expanding(theirs, script, expansions);
        let chan = EvalChannel::from_std(ours)?;
        // Keep the sender alive for the whole run: dropping it must
        // not read as an interrupt.
        let (_interrupt_tx, interrupt_rx) = tokio::sync::mpsc::unbounded_channel();
        let summary = coordinator
            .run(
                chan,
                attrs.iter().map(|s| s.to_string()).collect(),
                interrupt_rx,
            )
            .await?;
        Ok((summary, parent))
    }

    /// Like [`TestCluster::run`], but fire `interrupts` interrupt
    /// signals once the stub scheduler holds at least `after_builds`
    /// builds (plus a short grace so the coordinator drains the
    /// Started event carrying the build id — the run loop also biases
    /// its internal queue over the interrupt for the same reason).
    pub async fn run_interrupted(
        &self,
        coordinator: &mut Coordinator,
        script: HashMap<String, Vec<ResultFrame>>,
        attrs: &[&str],
        after_builds: usize,
        interrupts: usize,
    ) -> anyhow::Result<RunSummary> {
        let (ours, theirs) = std::os::unix::net::UnixStream::pair()?;
        let _parent = stub_parent::spawn(theirs, script);
        let chan = EvalChannel::from_std(ours)?;
        let (tx, rx) = tokio::sync::mpsc::unbounded_channel();
        let sched = self.sched.clone();
        tokio::spawn(async move {
            loop {
                if sched.build_ids().len() >= after_builds {
                    tokio::time::sleep(std::time::Duration::from_millis(200)).await;
                    for _ in 0..interrupts {
                        let _ = tx.send(());
                    }
                    return;
                }
                tokio::time::sleep(std::time::Duration::from_millis(10)).await;
            }
        });
        coordinator
            .run(chan, attrs.iter().map(|s| s.to_string()).collect(), rx)
            .await
    }

    /// Upload a path the way a BUILDER would (assignment token signed
    /// for exactly this path, tenant claim bound) so the
    /// `path_tenants` junction binds and the tenant-scoped read side
    /// (`GetPath` sig-visibility gate) serves it — the same provenance
    /// a build output has in production. The armed store enforces the
    /// `expected_outputs` allowlist on this call.
    pub async fn put_path_as_builder(
        &self,
        info: rio_proto::validated::ValidatedPathInfo,
        nar: Vec<u8>,
    ) -> anyhow::Result<bool> {
        use rio_proto::types::{PutPathMetadata, PutPathRequest, PutPathTrailer, put_path_request};
        let mut info: rio_proto::types::PathInfo = info.into();
        let token = rio_auth::hmac::HmacSigner::from_key(HMAC_KEY.to_vec()).sign(
            &rio_auth::hmac::AssignmentClaims {
                executor_id: "e2e-builder".into(),
                drv_hash: "00".repeat(32),
                expected_outputs: vec![info.store_path.clone()],
                is_ca: false,
                expiry_unix: u64::MAX,
                tenant: Some(self.tenant.to_string()),
                input_closure_digest: String::new(),
            },
        );
        let trailer = PutPathTrailer {
            nar_hash: std::mem::take(&mut info.nar_hash),
            nar_size: std::mem::take(&mut info.nar_size),
        };
        let frames = vec![
            PutPathRequest {
                msg: Some(put_path_request::Msg::Metadata(PutPathMetadata {
                    info: Some(info),
                    declared_nar_size: 0,
                })),
            },
            PutPathRequest {
                msg: Some(put_path_request::Msg::NarChunk(nar)),
            },
            PutPathRequest {
                msg: Some(put_path_request::Msg::Trailer(trailer)),
            },
        ];
        let mut store = self.clients.store.clone();
        let mut req = self.clients.req(tokio_stream::iter(frames))?;
        req.metadata_mut().insert(
            rio_proto::ASSIGNMENT_TOKEN_HEADER,
            token.parse().expect("token is ASCII"),
        );
        let resp = store.put_path(req).await?;
        Ok(resp.into_inner().created)
    }

    pub async fn drv_blob_count(&self) -> i64 {
        sqlx::query_scalar("SELECT COUNT(*) FROM drv_blobs")
            .fetch_one(&self.db.pool)
            .await
            .expect("count query")
    }

    pub async fn delete_drv_blob(&self, digest: &[u8; 32]) {
        sqlx::query("DELETE FROM drv_blob_tenants WHERE digest = $1")
            .bind(digest.as_slice())
            .execute(&self.db.pool)
            .await
            .expect("delete junction");
        sqlx::query("DELETE FROM drv_blobs WHERE digest = $1")
            .bind(digest.as_slice())
            .execute(&self.db.pool)
            .await
            .expect("delete blob");
    }
}

/// A one-frame script: all fixtures + sources in a single final batch.
pub fn single_frame(
    attr: &str,
    fixtures: &[&crate::drvgen::DrvFixture],
    sources: Vec<rio_proto::evaljob::SourceRoot>,
    root: &crate::drvgen::DrvFixture,
) -> ResultFrame {
    ResultFrame {
        attr: attr.into(),
        nodes: fixtures.iter().map(|f| f.node.clone()).collect(),
        drv_blobs: fixtures.iter().map(|f| f.blob.clone()).collect(),
        source_roots: sources,
        root_drv_digest: root.digest.to_vec(),
    }
}
