//! Test scaffolding shared by the castore-FUSE unit tests.
//!
//! A scriptable in-process mountd stand-in speaking the real UDS wire
//! protocol and a canned `DirectoryService`. Compiled only for tests
//! (`#[cfg(test)]` at the `mod` declaration) — production code never
//! links any of this.

use std::path::Path;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::sync::{Arc, Mutex};

use nix::sys::socket::{
    AddressFamily, Backlog, SockFlag, SockType, UnixAddr, bind, listen, socket,
};
use std::os::fd::{AsRawFd, FromRawFd, OwnedFd};
use tonic::{Request as TRequest, Response, Status};

use rio_proto::DirectoryService;
use rio_proto::types::{ReadBlobRequest, StatBlobRequest, StatBlobResponse};

use super::mountd_client::MountdClient;
use super::mountd_proto::{self as proto, Reply, Req, Request, Resp};

/// A mountd stand-in that answers every request via a caller-supplied
/// reply policy and records the requests it saw. Autonomous (unlike the
/// request-by-request `FakeDaemon` in `mountd_client::tests`) because
/// the fill task sends `PromoteChunks` at its own pace from a detached
/// task.
pub(crate) struct RecordingMountd {
    pub(crate) requests: Arc<Mutex<Vec<Req>>>,
    _listener: OwnedFd,
}

impl RecordingMountd {
    /// Replies `Resp::Ok` to everything.
    pub(crate) fn spawn(sock_path: &Path) -> (Self, MountdClient) {
        Self::spawn_with(sock_path, |_| Resp::Ok)
    }

    /// Replies according to `policy` (called once per request, in
    /// arrival order).
    pub(crate) fn spawn_with(
        sock_path: &Path,
        policy: impl Fn(&Req) -> Resp + Send + 'static,
    ) -> (Self, MountdClient) {
        let listener = socket(
            AddressFamily::Unix,
            SockType::SeqPacket,
            SockFlag::empty(),
            None,
        )
        .unwrap();
        bind(listener.as_raw_fd(), &UnixAddr::new(sock_path).unwrap()).unwrap();
        listen(&listener, Backlog::new(1).unwrap()).unwrap();
        let client = MountdClient::connect(sock_path).unwrap();
        let conn = nix::sys::socket::accept(listener.as_raw_fd()).unwrap();
        // SAFETY: accept(2) just returned a fresh fd we own.
        let conn = unsafe { OwnedFd::from_raw_fd(conn) };
        let requests = Arc::new(Mutex::new(Vec::new()));
        let seen = Arc::clone(&requests);
        std::thread::spawn(move || {
            while let Ok(frame) = proto::recv_frame(conn.as_raw_fd()) {
                let req: Request = proto::decode(&frame.bytes).unwrap();
                seen.lock().unwrap().push(req.req.clone());
                let bytes = proto::encode(&Reply {
                    seq: req.seq,
                    resp: policy(&req.req),
                })
                .unwrap();
                let _ = proto::send_frame(conn.as_raw_fd(), &bytes, &[]);
            }
        });
        (
            Self {
                requests,
                _listener: listener,
            },
            client,
        )
    }

    pub(crate) fn promoted_chunks(&self) -> Vec<[u8; 32]> {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter_map(|r| match r {
                Req::PromoteChunks { chunk_digests } => Some(chunk_digests.clone()),
                _ => None,
            })
            .flatten()
            .collect()
    }

    pub(crate) fn saw_promote(&self, digest: &[u8; 32]) -> bool {
        self.promote_count(digest) > 0
    }

    /// How many `Promote{digest}` requests arrived for `digest`.
    pub(crate) fn promote_count(&self, digest: &[u8; 32]) -> usize {
        self.requests
            .lock()
            .unwrap()
            .iter()
            .filter(|r| matches!(r, Req::Promote { digest: d } if d == digest))
            .count()
    }
}

/// The real DirectoryService refuses anonymous callers; mirroring that
/// here means a fetch path that drops the assignment token fails these
/// tests instead of only failing in the VM.
fn require_token<T>(req: &TRequest<T>) -> Result<(), Status> {
    match req.metadata().get(rio_proto::ASSIGNMENT_TOKEN_HEADER) {
        Some(_) => Ok(()),
        None => Err(Status::unauthenticated("no assignment token")),
    }
}

type BoxStream<T> = std::pin::Pin<Box<dyn tokio_stream::Stream<Item = Result<T, Status>> + Send>>;

/// `DirectoryService` serving a single canned `StatBlob` answer and a
/// canned `ReadBlob` frame sequence. The methods the open/fill paths
/// never call are `unimplemented!` so a refactor that starts calling
/// them fails loudly here.
#[derive(Clone)]
pub(crate) struct FakeDirectory {
    pub(crate) stat: Result<StatBlobResponse, tonic::Code>,
    /// Frames `read_blob` streams, in order.
    pub(crate) blob_frames: Vec<Vec<u8>>,
    /// When set, the `read_blob` stream never terminates after the last
    /// frame — for tests that need a fill parked mid-stream.
    pub(crate) hang_at_end: bool,
    /// When set, each `read_blob` call consumes one permit before any
    /// frame is sent — for tests that need to park a fill *before* its
    /// first chunk and release it at a chosen point in the schedule.
    pub(crate) read_blob_gate: Option<Arc<tokio::sync::Semaphore>>,
    /// Frames actually yielded to the transport across all `read_blob`
    /// calls — lets a test assert the client stopped consuming early.
    pub(crate) frames_served: Arc<AtomicUsize>,
}

impl FakeDirectory {
    /// Canned directory whose `read_blob` streams `blob` as two frames,
    /// so the fill's high-water mark demonstrably advances per frame,
    /// not once at EOF.
    pub(crate) fn new(stat: Result<StatBlobResponse, tonic::Code>, blob: Vec<u8>) -> Self {
        let mid = blob.len() / 2;
        Self {
            stat,
            blob_frames: vec![blob[..mid].to_vec(), blob[mid..].to_vec()],
            hang_at_end: false,
            read_blob_gate: None,
            frames_served: Arc::new(AtomicUsize::new(0)),
        }
    }
}

#[tonic::async_trait]
impl DirectoryService for FakeDirectory {
    type GetDirectoryStream = BoxStream<rio_proto::castore::Directory>;
    type ReadBlobStream = BoxStream<rio_proto::types::BlobChunk>;

    async fn get_directory(
        &self,
        _: TRequest<rio_proto::types::GetDirectoryRequest>,
    ) -> Result<Response<Self::GetDirectoryStream>, Status> {
        unimplemented!("not part of the open() data path")
    }

    async fn has_directories(
        &self,
        _: TRequest<rio_proto::types::HasDirectoriesRequest>,
    ) -> Result<Response<rio_proto::types::HasBitmap>, Status> {
        unimplemented!("not part of the open() data path")
    }

    async fn has_blobs(
        &self,
        _: TRequest<rio_proto::types::HasBlobsRequest>,
    ) -> Result<Response<rio_proto::types::HasBitmap>, Status> {
        unimplemented!("not part of the open() data path")
    }

    async fn read_blob(
        &self,
        request: TRequest<ReadBlobRequest>,
    ) -> Result<Response<Self::ReadBlobStream>, Status> {
        use tokio_stream::StreamExt;
        require_token(&request)?;
        if let Some(gate) = &self.read_blob_gate {
            gate.acquire()
                .await
                .map_err(|_| Status::cancelled("read_blob gate closed"))?
                .forget();
        }
        let served = Arc::clone(&self.frames_served);
        let frames = tokio_stream::iter(self.blob_frames.clone()).map(move |data| {
            served.fetch_add(1, Ordering::SeqCst);
            Ok(rio_proto::types::BlobChunk { data })
        });
        if self.hang_at_end {
            Ok(Response::new(Box::pin(
                frames.chain(tokio_stream::pending()),
            )))
        } else {
            Ok(Response::new(Box::pin(frames)))
        }
    }

    async fn stat_blob(
        &self,
        request: TRequest<StatBlobRequest>,
    ) -> Result<Response<StatBlobResponse>, Status> {
        require_token(&request)?;
        match &self.stat {
            Ok(resp) => Ok(Response::new(resp.clone())),
            Err(code) => Err(Status::new(*code, "injected")),
        }
    }
}
