//! Castore-FUSE unit tests that need a gRPC store and/or a mountd
//! peer: the `build_tree` prefetch round-trip, the `open()` mode
//! dispatch, and the JIT-fetch fill race. None of these mount a real
//! kernel filesystem — they drive [`tree::build_tree`] and
//! [`open::OpenPath`] directly, which is everything `Filesystem::open`
//! does short of the `reply.*` calls.

use std::collections::HashMap;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use tokio_stream::wrappers::ReceiverStream;
use tonic::transport::Server;
use tonic::{Request, Response, Status};

use rio_proto::castore::Directory;
use rio_proto::types::{
    BlobChunk, GetDirectoryRequest, HasBitmap, HasBlobsRequest, HasDirectoriesRequest,
    ReadBlobRequest, StatBlobRequest, StatBlobResponse,
};
use rio_proto::{DirectoryService, DirectoryServiceServer};
use rio_test_support::grpc::spawn_grpc_server;

use super::mountd_proto::{self as proto, Reply, Req, Resp};
use super::open::{OpenCase, OpenConfig, OpenPath};
use super::tree::{self, build_tree};
use crate::store_fetch::StoreClients;

// ─── Mock DirectoryService ─────────────────────────────────────────────

/// Seeded `GetDirectory`/`ReadBlob` server that counts calls so tests
/// can assert "exactly one RPC happened".
#[derive(Clone, Default)]
struct MockCastore {
    state: Arc<MockState>,
}

#[derive(Default)]
struct MockState {
    /// Directory bodies returned (in insertion order) by any
    /// `GetDirectory` call.
    dirs: std::sync::Mutex<Vec<Directory>>,
    /// `file_digest → bytes` served by `ReadBlob`.
    blobs: std::sync::Mutex<HashMap<[u8; 32], Vec<u8>>>,
    get_directory_calls: AtomicUsize,
    read_blob_calls: AtomicUsize,
    /// The digests field-3 multi-root extension of the last
    /// `GetDirectory` request (first digest + `digests`), for the
    /// one-RPC-for-the-whole-closure assertion.
    last_get_directory_roots: std::sync::Mutex<Vec<Vec<u8>>>,
    /// Artificial delay before answering `GetDirectory` (for the
    /// prefetch-timeout test).
    get_directory_delay: std::sync::Mutex<Duration>,
}

impl MockCastore {
    fn seed_dirs(&self, dirs: &HashMap<[u8; 32], Directory>) {
        let mut ordered: Vec<_> = dirs.values().cloned().collect();
        // Deterministic stream order (the client dedups by digest, so
        // order does not matter for correctness — only for test
        // reproducibility).
        ordered.sort_by_key(prost::Message::encode_to_vec);
        *self.state.dirs.lock().unwrap() = ordered;
    }

    fn seed_blob(&self, digest: [u8; 32], bytes: Vec<u8>) {
        self.state.blobs.lock().unwrap().insert(digest, bytes);
    }

    fn read_blob_calls(&self) -> usize {
        self.state.read_blob_calls.load(Ordering::SeqCst)
    }
}

#[tonic::async_trait]
impl DirectoryService for MockCastore {
    type GetDirectoryStream = ReceiverStream<Result<Directory, Status>>;

    async fn get_directory(
        &self,
        request: Request<GetDirectoryRequest>,
    ) -> Result<Response<Self::GetDirectoryStream>, Status> {
        self.state
            .get_directory_calls
            .fetch_add(1, Ordering::SeqCst);
        let req = request.into_inner();
        let mut roots = Vec::new();
        if let Some(rio_proto::types::get_directory_request::ByWhat::Digest(d)) = req.by_what {
            roots.push(d);
        }
        roots.extend(req.digests);
        *self.state.last_get_directory_roots.lock().unwrap() = roots;

        let delay = *self.state.get_directory_delay.lock().unwrap();
        let bodies = self.state.dirs.lock().unwrap().clone();
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            if !delay.is_zero() {
                tokio::time::sleep(delay).await;
            }
            for body in bodies {
                if tx.send(Ok(body)).await.is_err() {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn has_directories(
        &self,
        _request: Request<HasDirectoriesRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        Err(Status::unimplemented("MockCastore: HasDirectories"))
    }

    async fn has_blobs(
        &self,
        _request: Request<HasBlobsRequest>,
    ) -> Result<Response<HasBitmap>, Status> {
        Err(Status::unimplemented("MockCastore: HasBlobs"))
    }

    type ReadBlobStream = ReceiverStream<Result<BlobChunk, Status>>;

    async fn read_blob(
        &self,
        request: Request<ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        self.state.read_blob_calls.fetch_add(1, Ordering::SeqCst);
        let digest: [u8; 32] = request
            .into_inner()
            .file_digest
            .try_into()
            .map_err(|_| Status::invalid_argument("file_digest must be 32 bytes"))?;
        let Some(bytes) = self.state.blobs.lock().unwrap().get(&digest).cloned() else {
            return Err(Status::not_found("MockCastore: no such blob"));
        };
        let (tx, rx) = tokio::sync::mpsc::channel(16);
        tokio::spawn(async move {
            // Two frames so the client's incremental hashing is
            // exercised across a chunk boundary.
            let mid = bytes.len() / 2;
            for part in [&bytes[..mid], &bytes[mid..]] {
                if tx
                    .send(Ok(BlobChunk {
                        data: part.to_vec(),
                    }))
                    .await
                    .is_err()
                {
                    return;
                }
            }
        });
        Ok(Response::new(ReceiverStream::new(rx)))
    }

    async fn stat_blob(
        &self,
        _request: Request<StatBlobRequest>,
    ) -> Result<Response<StatBlobResponse>, Status> {
        Err(Status::unimplemented("MockCastore: StatBlob (P0570/P0575)"))
    }
}

async fn spawn_mock_castore() -> (MockCastore, StoreClients, tokio::task::JoinHandle<()>) {
    let mock = MockCastore::default();
    let router = Server::builder().add_service(DirectoryServiceServer::new(mock.clone()));
    let (addr, handle) = spawn_grpc_server(router).await;
    let ch = rio_proto::client::connect_channel(&addr.to_string())
        .await
        .expect("connect to mock castore");
    (mock, StoreClients::from_channel(ch), handle)
}

// ─── Fake mountd ───────────────────────────────────────────────────────

/// The daemon end of a socketpair that answers `Promote{digest}` by
/// copying `staging/{hex}` → `cache/{ab}/{hex}` — the same observable
/// effect as the real rio-mountd's verified promote, minus the
/// re-hash (which `vm-mountd` covers against the real daemon).
/// `BackingOpen`/`BackingClose` reply with a synthetic id so the
/// passthrough plumbing is exercisable without `CAP_SYS_ADMIN`.
fn spawn_fake_mountd(
    staging: PathBuf,
    cache: PathBuf,
) -> (
    super::mountd_client::MountdClient,
    std::thread::JoinHandle<()>,
) {
    use nix::sys::socket::{AddressFamily, SockFlag, SockType, socketpair};
    use std::os::fd::AsRawFd;

    let (client_end, daemon_end) = socketpair(
        AddressFamily::Unix,
        SockType::SeqPacket,
        None,
        SockFlag::empty(),
    )
    .expect("socketpair");
    let client = super::mountd_client::MountdClient::from_fd(client_end);

    let handle = std::thread::spawn(move || {
        let mut next_backing_id = 1u32;
        loop {
            let frame = match proto::recv_frame(daemon_end.as_raw_fd()) {
                Ok(f) => f,
                Err(_) => return,
            };
            let req: proto::Request = match proto::decode(&frame.bytes) {
                Ok(r) => r,
                Err(_) => return,
            };
            let resp = match req.req {
                Req::Promote { digest } => {
                    let hex = hex::encode(digest);
                    let src = staging.join(&hex);
                    let shard = cache.join(&hex[..2]);
                    let dst = shard.join(&hex);
                    std::fs::create_dir_all(&shard)
                        .and_then(|()| std::fs::copy(&src, &dst))
                        .and_then(|_| std::fs::remove_file(&src))
                        .map(|_| Resp::Ok)
                        .unwrap_or_else(|e| {
                            Resp::Err(proto::ErrKind::Retryable(format!("fake promote: {e}")))
                        })
                }
                Req::BackingOpen => {
                    let id = next_backing_id;
                    next_backing_id += 1;
                    Resp::BackingId(id)
                }
                Req::BackingClose { .. } => Resp::Ok,
                Req::Mount { .. } | Req::PromoteChunks { .. } => {
                    Resp::Err(proto::ErrKind::Retryable("not implemented in fake".into()))
                }
            };
            let bytes = proto::encode(&Reply { seq: req.seq, resp }).expect("encode fake reply");
            if proto::send_frame(daemon_end.as_raw_fd(), &bytes, &[]).is_err() {
                return;
            }
        }
    });
    (client, handle)
}

// ─── Harness ───────────────────────────────────────────────────────────

struct Harness {
    mock: MockCastore,
    open_path: Arc<OpenPath>,
    staging: PathBuf,
    _tmp: tempfile::TempDir,
    _server: tokio::task::JoinHandle<()>,
    _mountd: std::thread::JoinHandle<()>,
}

const FAST: Duration = Duration::from_secs(5);

async fn harness() -> Harness {
    harness_with(OpenConfig {
        jit_fetch_timeout: FAST,
        mountd_request_timeout: FAST,
        stream_threshold: 1024,
    })
    .await
}

async fn harness_with(cfg: OpenConfig) -> Harness {
    let tmp = tempfile::tempdir().expect("tempdir");
    let cache = tmp.path().join("cache");
    let staging = tmp.path().join("staging");
    std::fs::create_dir_all(&cache).unwrap();
    std::fs::create_dir_all(&staging).unwrap();
    let (mock, clients, server) = spawn_mock_castore().await;
    let (mountd, mountd_thread) = spawn_fake_mountd(staging.clone(), cache.clone());
    let open_path = Arc::new(OpenPath::new(
        cache.clone(),
        staging.clone(),
        clients,
        tokio::runtime::Handle::current(),
        mountd,
        cfg,
    ));
    Harness {
        mock,
        open_path,
        staging,
        _tmp: tmp,
        _server: server,
        _mountd: mountd_thread,
    }
}

/// `ensure_backing` bridges async with `Handle::block_on`, which
/// panics if called from a runtime worker thread — call it the way
/// production does: from a thread that is allowed to block.
async fn ensure_blocking(
    open_path: &Arc<OpenPath>,
    digest: [u8; 32],
    size: u64,
) -> Result<OpenCase, fuser::Errno> {
    let op = Arc::clone(open_path);
    tokio::task::spawn_blocking(move || op.ensure_backing(&digest, size))
        .await
        .expect("ensure_backing panicked")
}

fn seeded_blob(mock: &MockCastore, content: &[u8]) -> [u8; 32] {
    let digest = *blake3::hash(content).as_bytes();
    mock.seed_blob(digest, content.to_vec());
    digest
}

// ─── build_tree ────────────────────────────────────────────────────────

/// The mount-time prefetch issues exactly ONE `GetDirectory` call
/// seeded with every Dir root's digest (the multi-root extension), and
/// the resulting tree answers lookups for every node in the closure.
// r[verify builder.fs.castore-dag-source]
#[tokio::test(flavor = "multi_thread")]
async fn build_tree_prefetches_the_dag_in_one_call() {
    let (mock, clients, _server) = spawn_mock_castore().await;
    let fx = tree::tests::fixture();
    mock.seed_dirs(&fx.dirs);

    let map = build_tree(&clients, &fx.roots, FAST)
        .await
        .expect("build_tree");

    assert_eq!(
        mock.state.get_directory_calls.load(Ordering::SeqCst),
        1,
        "one RPC for the whole closure"
    );
    assert_eq!(
        mock.state.last_get_directory_roots.lock().unwrap().len(),
        1,
        "the fixture has one Dir root; its digest seeds the request"
    );
    // The prefetched tree resolves a deep path.
    let (root_ino, _) = map
        .lookup(fuser::INodeNo::ROOT.0, b"aaaa-hello")
        .expect("root");
    let (bin_ino, _) = map.lookup(root_ino, b"bin").expect("bin");
    assert!(map.lookup(bin_ino, b"hello").is_some());
}

/// A closure with no Dir roots (all single-file/symlink store paths)
/// needs no `GetDirectory` call at all.
#[tokio::test(flavor = "multi_thread")]
async fn build_tree_skips_the_rpc_when_there_are_no_dir_roots() {
    let (mock, clients, _server) = spawn_mock_castore().await;
    let fx = tree::tests::fixture();
    let file_only: Vec<_> = fx
        .roots
        .iter()
        .filter(|(p, _)| !p.ends_with("aaaa-hello"))
        .cloned()
        .collect();
    let map = build_tree(&clients, &file_only, FAST)
        .await
        .expect("build_tree");
    assert_eq!(mock.state.get_directory_calls.load(Ordering::SeqCst), 0);
    assert!(map.lookup(fuser::INodeNo::ROOT.0, b"bbbb-script").is_some());
}

/// A `GetDirectory` stream that does not complete within
/// `dag_prefetch_timeout` is a typed prefetch-timeout error (an
/// infrastructure retry), not a hang.
#[tokio::test(flavor = "multi_thread")]
async fn build_tree_times_out_as_a_prefetch_error() {
    let (mock, clients, _server) = spawn_mock_castore().await;
    let fx = tree::tests::fixture();
    mock.seed_dirs(&fx.dirs);
    *mock.state.get_directory_delay.lock().unwrap() = Duration::from_secs(60);

    let err = build_tree(&clients, &fx.roots, Duration::from_millis(100))
        .await
        .expect_err("must time out");
    assert!(
        matches!(err, tree::TreeError::PrefetchTimeout(_)),
        "got {err:?}"
    );
}

// ─── open() dispatch ───────────────────────────────────────────────────

/// Backing-cache hit: no fetch, no staging activity, `OpenCase::Hit`.
// r[verify builder.fs.digest-fuse-open]
#[tokio::test(flavor = "multi_thread")]
async fn open_cache_hit_does_not_fetch() {
    let h = harness().await;
    let content = b"already cached";
    let digest = *blake3::hash(content).as_bytes();
    let path = h.open_path.cache_path(&digest);
    std::fs::create_dir_all(path.parent().unwrap()).unwrap();
    std::fs::write(&path, content).unwrap();

    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("hit");
    assert_eq!(case, OpenCase::Hit);
    assert_eq!(h.mock.read_blob_calls(), 0, "a hit never touches the store");
}

/// Cache miss below the streaming threshold: whole-file `ReadBlob`,
/// blake3 verify, `Promote` into the shared cache, and the backing
/// entry exists with the exact fetched bytes when `open()` returns.
// r[verify builder.fs.digest-fuse-open]
#[tokio::test(flavor = "multi_thread")]
async fn open_miss_fetches_verifies_and_promotes() {
    let h = harness().await;
    let content = b"jit-fetched file body".to_vec();
    let digest = seeded_blob(&h.mock, &content);

    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("miss_small");
    assert_eq!(case, OpenCase::MissSmall);
    assert_eq!(h.mock.read_blob_calls(), 1);
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).expect("promoted into the cache"),
        content,
        "the backing cache entry is the verified fetched bytes"
    );
    assert!(
        std::fs::read_dir(&h.staging).unwrap().next().is_none(),
        "staging is empty after a successful promote"
    );

    // A second open of the same digest is now a pure cache hit.
    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("now a hit");
    assert_eq!(case, OpenCase::Hit);
    assert_eq!(
        h.mock.read_blob_calls(),
        1,
        "no re-fetch on the second open"
    );
}

/// Above the streaming threshold the dispatch picks `MissStream` —
/// which, until P0575 lands, takes the same whole-file path. The case
/// label is what distinguishes the two in metrics.
#[tokio::test(flavor = "multi_thread")]
async fn open_miss_above_threshold_is_the_stream_case() {
    let h = harness().await;
    let content = vec![0xAB; 4096]; // threshold in the harness is 1024
    let digest = seeded_blob(&h.mock, &content);
    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("miss_stream");
    assert_eq!(case, OpenCase::MissStream);
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// Bytes that do not hash to the requested `file_digest` are never
/// served and never promoted: the open fails with EIO and the staging
/// dir holds no leftover `.partial`.
// r[verify builder.fs.file-digest-integrity]
#[tokio::test(flavor = "multi_thread")]
async fn open_integrity_mismatch_is_eio_and_nothing_is_published() {
    let h = harness().await;
    let content = b"the real content";
    let claimed = *blake3::hash(b"something else entirely").as_bytes();
    h.mock.seed_blob(claimed, content.to_vec());

    let err = ensure_blocking(&h.open_path, claimed, content.len() as u64)
        .await
        .expect_err("integrity mismatch");
    assert_eq!(err.code(), fuser::Errno::EIO.code());
    assert!(
        !h.open_path.cache_path(&claimed).exists(),
        "corrupt bytes must never reach the shared cache"
    );
    assert!(
        std::fs::read_dir(&h.staging).unwrap().next().is_none(),
        "the rejected .partial is cleaned up"
    );
}

/// A blob the store does not have is an EIO on open (the build's input
/// is missing — an infrastructure failure), not a hang.
#[tokio::test(flavor = "multi_thread")]
async fn open_missing_blob_is_eio() {
    let h = harness().await;
    let digest = *blake3::hash(b"never seeded").as_bytes();
    let err = ensure_blocking(&h.open_path, digest, 11)
        .await
        .expect_err("not found");
    assert_eq!(err.code(), fuser::Errno::EIO.code());
}

/// N concurrent opens of the same uncached `file_digest` perform
/// exactly one fetch: one winner streams the blob, every loser waits
/// on the in-process `FillState` and then finds the promoted cache
/// entry.
// r[verify builder.fs.digest-fuse-open]
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn concurrent_opens_of_one_digest_fetch_once() {
    const N: usize = 8;
    let h = harness().await;
    let content = b"contended file".to_vec();
    let digest = seeded_blob(&h.mock, &content);

    let mut tasks = Vec::new();
    for _ in 0..N {
        let op = Arc::clone(&h.open_path);
        let len = content.len() as u64;
        tasks.push(tokio::task::spawn_blocking(move || {
            op.ensure_backing(&digest, len)
        }));
    }
    let mut cases = Vec::new();
    for t in tasks {
        cases.push(t.await.expect("join").expect("every opener succeeds"));
    }
    assert_eq!(
        h.mock.read_blob_calls(),
        1,
        "one fetch for {N} concurrent opens"
    );
    let winners = cases
        .iter()
        .filter(|c| matches!(c, OpenCase::MissSmall))
        .count();
    let waiters = cases
        .iter()
        .filter(|c| matches!(c, OpenCase::WaitFetching | OpenCase::Hit))
        .count();
    assert_eq!(
        winners, 1,
        "exactly one opener performed the fill: {cases:?}"
    );
    assert_eq!(waiters, N - 1, "everyone else waited or hit: {cases:?}");
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// A panic in the fill winner must not wedge the digest: the unwind
/// guard publishes a failure and removes the in-flight `FillState`, so
/// the next opener becomes a fresh winner and succeeds. Without the
/// guard, every later open of this digest would park as a loser for
/// its full wait deadline against a fill that will never finish —
/// silently, because fuser catches callback panics.
///
/// The injected panic is `Handle::block_on` called from inside the
/// runtime (the test calls `ensure_backing` directly from the async
/// context instead of from a blocking thread) — the same call site
/// that panics in production when a late `open()` races runtime
/// shutdown at process teardown.
#[tokio::test(flavor = "multi_thread")]
async fn a_panicking_fill_does_not_wedge_the_digest() {
    // Short timeouts so a regression (the loser path waiting out its
    // full deadline) fails the test in milliseconds, not tens of
    // seconds.
    let h = harness_with(OpenConfig {
        jit_fetch_timeout: Duration::from_millis(250),
        mountd_request_timeout: Duration::from_millis(250),
        stream_threshold: 1024,
    })
    .await;
    let content = b"recovered after a panicked fill".to_vec();
    let digest = seeded_blob(&h.mock, &content);

    let panicked = std::panic::catch_unwind(std::panic::AssertUnwindSafe(|| {
        h.open_path.ensure_backing(&digest, content.len() as u64)
    }));
    assert!(
        panicked.is_err(),
        "block_on from a runtime worker must panic — the panic injection itself broke"
    );

    // The digest is not wedged: the next opener (through the normal
    // blocking-thread path) wins a fresh fill and succeeds. It also
    // exercises the orphan-reclaim path — the panicked fill left its
    // flock-released `.partial` behind.
    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("a fresh open after a panicked fill must succeed");
    assert_eq!(case, OpenCase::MissSmall);
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// An orphaned `.partial` from a fill that died without cleanup is
/// reclaimed (unlink + retry) instead of permanently wedging that
/// digest.
#[tokio::test(flavor = "multi_thread")]
async fn open_reclaims_an_orphaned_partial() {
    let h = harness().await;
    let content = b"previously interrupted".to_vec();
    let digest = seeded_blob(&h.mock, &content);
    // Simulate a crashed fill: a .partial with no flock holder and no
    // FillState entry.
    std::fs::write(
        h.staging.join(format!("{}.partial", hex::encode(digest))),
        b"garbage from a dead fill",
    )
    .unwrap();

    let case = ensure_blocking(&h.open_path, digest, content.len() as u64)
        .await
        .expect("reclaim and refetch");
    assert_eq!(case, OpenCase::MissSmall);
    assert_eq!(
        std::fs::read(h.open_path.cache_path(&digest)).unwrap(),
        content
    );
}

/// Five consecutive fetch failures open the circuit; the sixth open
/// fails fast without another RPC. `r[builder.fs.fetch-circuit]`'s
/// state machine is exhaustively covered in `circuit.rs`'s own tests —
/// this verifies the open path actually consults it.
// r[verify builder.fs.fetch-circuit]
#[tokio::test(flavor = "multi_thread")]
async fn open_fails_fast_once_the_circuit_trips() {
    let h = harness().await;
    // Five distinct digests, none seeded → five NotFound failures.
    for i in 0..5u8 {
        let digest = *blake3::hash(&[i]).as_bytes();
        let _ = ensure_blocking(&h.open_path, digest, 1).await;
    }
    assert_eq!(h.mock.read_blob_calls(), 5);
    assert!(
        h.open_path.circuit.is_open(),
        "5 consecutive failures trip the breaker"
    );

    let digest = *blake3::hash(b"sixth").as_bytes();
    let err = ensure_blocking(&h.open_path, digest, 1)
        .await
        .expect_err("circuit open");
    assert_eq!(err.code(), fuser::Errno::EIO.code());
    assert_eq!(
        h.mock.read_blob_calls(),
        5,
        "the sixth open never reaches the store"
    );
}

// ─── Promote → shared cache layout ─────────────────────────────────────

/// The backing-cache path layout matches what rio-mountd's `Promote`
/// writes: `cache/{ab}/{hex}` where `ab` is the first two hex chars of
/// the file digest. A mismatch here means every promote succeeds but
/// every subsequent open misses.
// r[verify builder.fs.shared-backing-cache]
#[tokio::test]
async fn cache_path_matches_the_mountd_shard_layout() {
    let tmp = tempfile::tempdir().unwrap();
    let (mountd, _t) = spawn_fake_mountd(tmp.path().into(), tmp.path().into());
    let op = OpenPath::new(
        PathBuf::from("/var/rio/cache"),
        PathBuf::from("/var/rio/staging/b1"),
        StoreClients::from_channel(rio_test_support::grpc::dead_channel()),
        tokio::runtime::Handle::current(),
        mountd,
        OpenConfig {
            jit_fetch_timeout: FAST,
            mountd_request_timeout: FAST,
            stream_threshold: 1024,
        },
    );
    let digest: [u8; 32] = *blake3::hash(b"x").as_bytes();
    let hex = hex::encode(digest);
    assert_eq!(
        op.cache_path(&digest),
        Path::new("/var/rio/cache").join(&hex[..2]).join(&hex)
    );
}
