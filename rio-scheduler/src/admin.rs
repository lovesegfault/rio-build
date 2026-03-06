//! AdminService gRPC implementation.
//!
//! `GetBuildLogs` (phase2b) and `ClusterStatus` (phase3a) are fully
//! implemented. `DrainWorker` lands in E2. The remaining RPCs return
//! `UNIMPLEMENTED` and will land in phase4 (dashboard). Stubbing them
//! here means the tonic server wiring is already in place — adding each
//! one later is a pure body-swap with no main.rs churn.
//!
//! `GetBuildLogs` has two data sources (per `observability.md:44-50`):
//!
//! | Build State | Source |
//! |---|---|
//! | Active | Ring buffer (in-memory, most recent) |
//! | Completed | S3 (gzipped blob, seekable via PG `build_logs.s3_key`) |
//!
//! We check the ring buffer FIRST: if the derivation is still active, the
//! ring buffer has the freshest lines (the S3 blob, if any, is a 30s-stale
//! periodic snapshot). Only if the ring buffer is empty do we fall back to
//! S3 — which means the derivation finished and the flusher drained it.

use std::io::Read;
use std::sync::Arc;
use std::time::{Instant, SystemTime};

use aws_sdk_s3::Client as S3Client;
use flate2::read::GzDecoder;
use sqlx::PgPool;
use tokio::sync::{mpsc, oneshot};
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, instrument};

use rio_proto::AdminService;
use rio_proto::types::{
    BuildLogChunk, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    DrainWorkerRequest, DrainWorkerResponse, GcProgress, GcRequest, GetBuildLogsRequest,
    ListBuildsRequest, ListBuildsResponse, ListWorkersRequest, ListWorkersResponse,
};

use crate::actor::{ActorCommand, ActorHandle};
use crate::logs::LogBuffers;

/// Chunk size for streaming S3-fetched log lines back to the client.
/// The whole log is gunzipped into memory first (we need to do line
/// splitting on decompressed data), then re-chunked for the gRPC stream.
/// 256 lines/chunk balances message count vs. per-message size.
const CHUNK_LINES: usize = 256;

pub struct AdminServiceImpl {
    /// Shared with `SchedulerGrpc` — same Arc, same DashMap.
    log_buffers: Arc<LogBuffers>,
    /// `None` when `RIO_LOG_S3_BUCKET` is unset. In that case, completed-
    /// build logs are unserveable (the flusher never wrote them). Ring
    /// buffer still serves active builds.
    s3: Option<(S3Client, String)>, // (client, bucket)
    pool: PgPool,
    /// For `ClusterStatus` / `DrainWorker` — sends query commands into
    /// the actor event loop. `ClusterSnapshot` bypasses backpressure
    /// (`send_unchecked`): the autoscaler needs a reading especially
    /// when saturated. Dropping the query under load would blind the
    /// controller exactly when it needs to scale up.
    actor: ActorHandle,
    /// Process start time. `ClusterStatusResponse.uptime_since` wants a
    /// wall-clock `Timestamp`, but we don't want to capture `SystemTime`
    /// at startup and risk it being wrong if the system clock jumps
    /// forward during boot. Instead: `Instant` is monotonic; compute
    /// `SystemTime::now() - started_at.elapsed()` at request time.
    /// That's the correct "when did we start, in CURRENT wall-clock
    /// terms" answer even across NTP adjustments.
    started_at: Instant,
}

impl AdminServiceImpl {
    pub fn new(
        log_buffers: Arc<LogBuffers>,
        s3: Option<(S3Client, String)>,
        pool: PgPool,
        actor: ActorHandle,
    ) -> Self {
        Self {
            log_buffers,
            s3,
            pool,
            actor,
            started_at: Instant::now(),
        }
    }

    /// Actor-dead check. Same pattern as `SchedulerGrpc::check_actor_alive`
    /// (grpc/mod.rs:~180) — if the actor panicked, all commands would
    /// hang on a closed channel. Return UNAVAILABLE early instead.
    fn check_actor_alive(&self) -> Result<(), Status> {
        if !self.actor.is_alive() {
            return Err(Status::unavailable(
                "scheduler actor not running (internal error)",
            ));
        }
        Ok(())
    }

    /// Try the ring buffer. Returns `Some` if the derivation has any lines
    /// buffered (i.e., it's active or just-completed-not-yet-drained).
    fn try_ring_buffer(&self, drv_path: &str, since: u64) -> Option<Vec<BuildLogChunk>> {
        let lines = self.log_buffers.read_since(drv_path, since);
        if lines.is_empty() {
            return None;
        }
        // Group into CHUNK_LINES-sized chunks. Each chunk carries the
        // first_line_number of its first line for client-side ordering.
        let mut chunks = Vec::new();
        for group in lines.chunks(CHUNK_LINES) {
            let first_line = group[0].0;
            chunks.push(BuildLogChunk {
                derivation_path: drv_path.to_string(),
                lines: group.iter().map(|(_n, bytes)| bytes.clone()).collect(),
                first_line_number: first_line,
                is_complete: false, // ring buffer = still active
            });
        }
        // Mark the LAST chunk is_complete=false too — the derivation is
        // still running. The client should re-poll for more.
        Some(chunks)
    }

    /// Fetch from S3, gunzip, split lines, chunk. The whole blob comes
    /// into memory during gunzip — acceptable for build logs (bounded
    /// by the worker's `log_size_limit` of 100 MiB, which gzips to
    /// ~10 MiB). True streaming gunzip would need an async GzDecoder
    /// that yields lines, which flate2 doesn't have natively.
    async fn try_s3(
        &self,
        build_id: &uuid::Uuid,
        drv_hash: &str,
        since: u64,
    ) -> Result<Option<Vec<BuildLogChunk>>, Status> {
        let Some((s3, bucket)) = &self.s3 else {
            // No S3 configured. Can't serve completed logs.
            return Ok(None);
        };

        // PG lookup: find the s3_key. The flusher wrote this row with
        // is_complete=true on the final flush.
        let row: Option<(String, i64)> = sqlx::query_as(
            "SELECT s3_key, line_count FROM build_logs
             WHERE build_id = $1 AND drv_hash = $2 AND is_complete = true",
        )
        .bind(build_id)
        .bind(drv_hash)
        .fetch_optional(&self.pool)
        .await
        .map_err(|e| Status::internal(format!("PG query failed: {e}")))?;

        let Some((s3_key, _line_count)) = row else {
            return Ok(None); // not in S3 either → truly not found
        };

        debug!(s3_key = %s3_key, "serving build log from S3");

        // S3 GET + full-body drain.
        let resp = s3
            .get_object()
            .bucket(bucket)
            .key(&s3_key)
            .send()
            .await
            .map_err(|e| Status::unavailable(format!("S3 GetObject failed: {e}")))?;
        let gzipped = resp
            .body
            .collect()
            .await
            .map_err(|e| Status::unavailable(format!("S3 body read failed: {e}")))?
            .into_bytes();

        // Gunzip in spawn_blocking — same rationale as the flusher's gzip.
        let drv_hash_owned = drv_hash.to_string();
        let chunks =
            tokio::task::spawn_blocking(move || gunzip_and_chunk(&gzipped, &drv_hash_owned, since))
                .await
                .map_err(|e| Status::internal(format!("gunzip task panicked: {e}")))?
                .map_err(|e| Status::internal(format!("gunzip failed: {e}")))?;

        Ok(Some(chunks))
    }
}

/// Gunzip the blob, split on `\n`, apply `since` filtering, chunk.
/// Standalone so spawn_blocking can take it without `self`.
///
/// `drv_label` goes into `BuildLogChunk.derivation_path`. The S3 path uses
/// `drv_hash` but the proto field is called `derivation_path` — we put the
/// hash there since that's all we have at this point (the ring-buffer path
/// uses the real drv_path, but for completed builds the DAG entry is gone).
fn gunzip_and_chunk(
    gzipped: &[u8],
    drv_label: &str,
    since: u64,
) -> std::io::Result<Vec<BuildLogChunk>> {
    let mut decoder = GzDecoder::new(gzipped);
    let mut raw = Vec::new();
    decoder.read_to_end(&mut raw)?;

    // Split on \n. The flusher joins with \n so every line gets a trailing
    // \n — split() gives an empty last element, which we skip.
    // Line numbers are the index into this split, starting from 0 (the
    // flusher writes them in buffer order, which IS line-number order).
    let mut chunks = Vec::new();
    let mut buf: Vec<Vec<u8>> = Vec::with_capacity(CHUNK_LINES);
    let mut chunk_first_line = since;

    for (n, line) in raw.split(|b| *b == b'\n').enumerate() {
        let n = n as u64;
        if n < since {
            continue; // client already has this line
        }
        // Skip the trailing empty element from the final \n. (An ACTUAL
        // empty log line is uncommon but valid — we keep those. The
        // distinguisher is whether it's the very last element.)
        // We don't know we're at the last element until the iterator ends,
        // so: collect all, then strip a trailing empty. Simpler: check if
        // this is the last byte position. But `split()` doesn't give us
        // that. Simplest: if line is empty AND it's past line_count, skip.
        // Actually: the flusher writes `line\nline\nline\n` — split gives
        // [line, line, line, ""]. The "" is always the last element.
        // We handle it after the loop.
        buf.push(line.to_vec());
        if buf.len() >= CHUNK_LINES {
            chunks.push(BuildLogChunk {
                derivation_path: drv_label.to_string(),
                lines: std::mem::take(&mut buf),
                first_line_number: chunk_first_line,
                is_complete: false,
            });
            chunk_first_line = n + 1;
        }
    }
    // Strip the trailing empty from the final-\n split artifact.
    if buf.last().is_some_and(|l| l.is_empty()) {
        buf.pop();
    }
    if !buf.is_empty() {
        chunks.push(BuildLogChunk {
            derivation_path: drv_label.to_string(),
            lines: buf,
            first_line_number: chunk_first_line,
            is_complete: false,
        });
    }
    // Mark the LAST chunk is_complete=true. From S3 = derivation finished.
    if let Some(last) = chunks.last_mut() {
        last.is_complete = true;
    }
    Ok(chunks)
}

#[tonic::async_trait]
impl AdminService for AdminServiceImpl {
    type GetBuildLogsStream = ReceiverStream<Result<BuildLogChunk, Status>>;
    type TriggerGCStream = ReceiverStream<Result<GcProgress, Status>>;

    #[instrument(skip(self, request), fields(rpc = "GetBuildLogs"))]
    async fn get_build_logs(
        &self,
        request: Request<GetBuildLogsRequest>,
    ) -> Result<Response<Self::GetBuildLogsStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let req = request.into_inner();

        // Validate: need at least derivation_path. build_id is needed only
        // for the S3 path (PG lookup is keyed on it); ring buffer is keyed
        // on drv_path alone.
        if req.derivation_path.is_empty() {
            return Err(Status::invalid_argument(
                "derivation_path is required (build_id is optional if the \
                 derivation is still active)",
            ));
        }

        // Step 1: Ring buffer (active or just-completed-not-yet-drained).
        if let Some(chunks) = self.try_ring_buffer(&req.derivation_path, req.since_line) {
            debug!(
                drv_path = %req.derivation_path,
                chunks = chunks.len(),
                "serving from ring buffer"
            );
            return Ok(Response::new(chunks_to_stream(chunks)));
        }

        // Step 2: S3 (completed). Need build_id for the PG lookup.
        if req.build_id.is_empty() {
            return Err(Status::not_found(format!(
                "derivation {:?} has no active ring buffer and build_id was \
                 not provided for S3 lookup. If the build completed, retry \
                 with build_id.",
                req.derivation_path
            )));
        }
        let build_id: uuid::Uuid = req
            .build_id
            .parse()
            .map_err(|e| Status::invalid_argument(format!("invalid build_id UUID: {e}")))?;

        // For the S3 path, we need drv_hash, not drv_path. The spec's
        // S3 key format is `logs/{build_id}/{drv_hash}.log.gz`. The client
        // typically has drv_path (that's what the gateway speaks). We
        // could resolve drv_path→drv_hash via the actor, but the DAG entry
        // is likely gone by now (CleanupTerminalBuild removes it ~30s after
        // completion). Instead: accept EITHER in derivation_path. If it
        // parses as a store path, use the hash-name part; otherwise assume
        // it's already a hash.
        //
        // This is a soft interface contract. The phase4 dashboard will know
        // both and pass drv_hash explicitly.
        let drv_hash = extract_drv_hash(&req.derivation_path);

        match self.try_s3(&build_id, &drv_hash, req.since_line).await? {
            Some(chunks) => {
                debug!(
                    drv_hash = %drv_hash,
                    chunks = chunks.len(),
                    "serving from S3"
                );
                Ok(Response::new(chunks_to_stream(chunks)))
            }
            None => Err(Status::not_found(format!(
                "no log found for build {build_id} derivation {drv_hash:?} \
                 (not in ring buffer or S3). Either the derivation produced \
                 no output, or the flusher hasn't uploaded yet."
            ))),
        }
    }

    /// Cluster-wide counts for the controller's autoscaling loop.
    ///
    /// The controller computes `desired_replicas = queued_derivations /
    /// target_value` (clamped to `[min, max]`) and patches the worker
    /// StatefulSet. `queued_derivations` is the primary signal — that's
    /// how many ready-to-build derivations are waiting for worker slots.
    /// `running_derivations` is secondary (for "scale-down is safe when
    /// queue=0 AND running is below capacity").
    ///
    /// `store_size_bytes` is 0: the scheduler doesn't track store
    /// size; that would be a store-service RPC and this endpoint is
    /// on the autoscaler's hot path (30s poll). If the dashboard
    /// wants store size, it can query the store's /metrics directly.
    /// `TODO(phase3b)`: add a best-effort store-size field populated
    /// via a separate slow-refresh background task, NOT inline here.
    #[instrument(skip(self, _request), fields(rpc = "ClusterStatus"))]
    async fn cluster_status(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ClusterStatusResponse>, Status> {
        self.check_actor_alive()?;

        let (tx, rx) = oneshot::channel();
        // send_unchecked: autoscaler MUST get a reading under saturation.
        // See ActorCommand::ClusterSnapshot doc for rationale.
        self.actor
            .send_unchecked(ActorCommand::ClusterSnapshot { reply: tx })
            .await
            .map_err(|_| Status::unavailable("actor channel closed"))?;

        // rx.await fails only if the actor dropped the oneshot::Sender
        // without replying — which means the actor task panicked mid-
        // handler (impossible for ClusterSnapshot, it's a pure read
        // with no .await points, but be robust anyway).
        let snap = rx
            .await
            .map_err(|_| Status::unavailable("actor dropped reply"))?;

        // SystemTime::now() - elapsed → "start time in CURRENT wall-clock
        // terms." checked_sub: if elapsed > now (clock jumped way back),
        // UNIX_EPOCH is a less-wrong answer than panicking.
        let uptime_since = SystemTime::now()
            .checked_sub(self.started_at.elapsed())
            .unwrap_or(SystemTime::UNIX_EPOCH);

        Ok(Response::new(ClusterStatusResponse {
            total_workers: snap.total_workers,
            active_workers: snap.active_workers,
            draining_workers: snap.draining_workers,
            pending_builds: snap.pending_builds,
            active_builds: snap.active_builds,
            queued_derivations: snap.queued_derivations,
            running_derivations: snap.running_derivations,
            store_size_bytes: 0,
            uptime_since: Some(prost_types::Timestamp::from(uptime_since)),
        }))
    }

    // -----------------------------------------------------------------------
    // Stubs — return UNIMPLEMENTED. Phase 4 (dashboard) implements these.
    // -----------------------------------------------------------------------

    async fn list_workers(
        &self,
        _request: Request<ListWorkersRequest>,
    ) -> Result<Response<ListWorkersResponse>, Status> {
        Err(Status::unimplemented("ListWorkers: phase4 dashboard"))
    }

    async fn list_builds(
        &self,
        _request: Request<ListBuildsRequest>,
    ) -> Result<Response<ListBuildsResponse>, Status> {
        Err(Status::unimplemented("ListBuilds: phase4 dashboard"))
    }

    async fn trigger_gc(
        &self,
        _request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        Err(Status::unimplemented("TriggerGC: phase4 controller"))
    }

    /// Mark a worker draining: `has_capacity()` returns false, dispatch
    /// skips it. In-flight builds continue. Called by:
    ///   - Worker's SIGTERM handler (step 1 of drain)
    ///   - Controller's WorkerPool finalizer cleanup
    ///
    /// `force=true` reassigns in-flight builds — the worker's nix-daemon
    /// keeps running them (we can't reach into its process tree) but the
    /// scheduler redispatches to fresh workers. Wasteful but unblocks.
    ///
    /// Unknown worker_id → `accepted=false, running=0`. NOT an error:
    /// SIGTERM may race with stream close (WorkerDisconnected removes
    /// the entry). The caller proceeds as if drain succeeded.
    ///
    /// Empty worker_id → InvalidArgument. Catches the proto-default
    /// (empty string) before it gets interpreted as "worker named ''
    /// not found."
    #[instrument(skip(self, request), fields(rpc = "DrainWorker"))]
    async fn drain_worker(
        &self,
        request: Request<DrainWorkerRequest>,
    ) -> Result<Response<DrainWorkerResponse>, Status> {
        self.check_actor_alive()?;
        let req = request.into_inner();

        if req.worker_id.is_empty() {
            return Err(Status::invalid_argument("worker_id is required"));
        }

        let (tx, rx) = oneshot::channel();
        // send_unchecked: drain MUST land even under backpressure. A
        // shutting-down worker accepting new assignments is a feedback
        // loop into MORE load — exactly what we don't want.
        self.actor
            .send_unchecked(ActorCommand::DrainWorker {
                worker_id: req.worker_id.into(),
                force: req.force,
                reply: tx,
            })
            .await
            .map_err(|_| Status::unavailable("actor channel closed"))?;

        let result = rx
            .await
            .map_err(|_| Status::unavailable("actor dropped reply"))?;

        Ok(Response::new(DrainWorkerResponse {
            accepted: result.accepted,
            running_builds: result.running_builds,
        }))
    }

    async fn clear_poison(
        &self,
        _request: Request<ClearPoisonRequest>,
    ) -> Result<Response<ClearPoisonResponse>, Status> {
        Err(Status::unimplemented("ClearPoison: phase4 dashboard"))
    }
}

/// Convert a Vec<Chunk> into a ReceiverStream. The chunks are already
/// fully materialized (we either read the ring buffer or gunzipped S3
/// into memory), so there's no backpressure benefit to streaming — but
/// the gRPC API is streaming, so we honor it.
fn chunks_to_stream(chunks: Vec<BuildLogChunk>) -> ReceiverStream<Result<BuildLogChunk, Status>> {
    let (tx, rx) = mpsc::channel(chunks.len().max(1));
    tokio::spawn(async move {
        for chunk in chunks {
            if tx.send(Ok(chunk)).await.is_err() {
                break; // client disconnected
            }
        }
    });
    ReceiverStream::new(rx)
}

/// Extract a drv_hash-shaped key from a derivation_path-ish input.
///
/// `/nix/store/{32-char-hash}-{name}.drv` → `{32-char-hash}-{name}.drv`
/// Already-hash-shaped input → unchanged.
fn extract_drv_hash(s: &str) -> String {
    s.strip_prefix(rio_nix::store_path::STORE_PREFIX)
        .unwrap_or(s)
        .to_string()
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::actor::tests::setup_actor;
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};
    use rio_proto::types::BuildLogBatch;
    use rio_test_support::TestDb;
    use tokio_stream::StreamExt;

    /// Set up `AdminServiceImpl` with a live actor but no S3.
    ///
    /// The GetBuildLogs tests don't exercise the actor (they hit ring
    /// buffer or S3 directly), but the constructor needs a handle since
    /// E1. `setup_actor` gives a real actor backed by the same PG — no
    /// mocks needed. The `_task` keeps the actor task alive; dropping
    /// the returned tuple drops the handle → channel closes → actor
    /// shuts down cleanly.
    ///
    /// Returns `(svc, actor_handle, task)`. The handle is separate so
    /// ClusterStatus tests can also send actor commands directly.
    async fn setup_svc(
        buffers: Arc<LogBuffers>,
        s3: Option<(S3Client, String)>,
    ) -> (
        AdminServiceImpl,
        ActorHandle,
        tokio::task::JoinHandle<()>,
        TestDb,
    ) {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let (actor, task) = setup_actor(db.pool.clone());
        let svc = AdminServiceImpl::new(buffers, s3, db.pool.clone(), actor.clone());
        (svc, actor, task, db)
    }

    fn mk_batch(drv_path: &str, first_line: u64, lines: &[&[u8]]) -> BuildLogBatch {
        BuildLogBatch {
            derivation_path: drv_path.to_string(),
            lines: lines.iter().map(|l| l.to_vec()).collect(),
            first_line_number: first_line,
            worker_id: "test-worker".into(),
        }
    }

    async fn collect_stream(
        stream: ReceiverStream<Result<BuildLogChunk, Status>>,
    ) -> Vec<BuildLogChunk> {
        stream.filter_map(|r| r.ok()).collect::<Vec<_>>().await
    }

    #[tokio::test]
    async fn get_build_logs_from_ring_buffer() -> anyhow::Result<()> {
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/abc-test.drv",
            0,
            &[b"line0", b"line1", b"line2"],
        ));

        let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

        let resp = svc
            .get_build_logs(Request::new(GetBuildLogsRequest {
                build_id: String::new(), // not needed for ring buffer
                derivation_path: "/nix/store/abc-test.drv".into(),
                since_line: 0,
            }))
            .await?;

        let chunks = collect_stream(resp.into_inner()).await;
        assert_eq!(chunks.len(), 1, "3 lines < CHUNK_LINES → one chunk");
        assert_eq!(chunks[0].lines.len(), 3);
        assert_eq!(chunks[0].lines[0], b"line0");
        assert_eq!(chunks[0].first_line_number, 0);
        assert!(
            !chunks[0].is_complete,
            "ring buffer serve → still active, is_complete=false"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_build_logs_since_line_filters() -> anyhow::Result<()> {
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/abc-test.drv",
            0,
            &[b"l0", b"l1", b"l2", b"l3", b"l4"],
        ));

        let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

        let resp = svc
            .get_build_logs(Request::new(GetBuildLogsRequest {
                build_id: String::new(),
                derivation_path: "/nix/store/abc-test.drv".into(),
                since_line: 3,
            }))
            .await?;

        let chunks = collect_stream(resp.into_inner()).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lines.len(), 2, "since_line=3 → only lines 3,4");
        assert_eq!(chunks[0].first_line_number, 3);
        Ok(())
    }

    #[tokio::test]
    async fn get_build_logs_from_s3_fallback() -> anyhow::Result<()> {
        let db = TestDb::new(&crate::MIGRATOR).await;
        let build_id = uuid::Uuid::new_v4();
        sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'succeeded')")
            .bind(build_id)
            .execute(&db.pool)
            .await?;

        // Gzip a test log the same way the flusher does.
        let gzipped = {
            use flate2::Compression;
            use flate2::write::GzEncoder;
            use std::io::Write;
            let mut enc = GzEncoder::new(Vec::new(), Compression::default());
            for line in ["from-s3-0", "from-s3-1", "from-s3-2"] {
                enc.write_all(line.as_bytes())?;
                enc.write_all(b"\n")?;
            }
            enc.finish()?
        };

        // Seed the PG row the flusher would have written.
        sqlx::query(
            "INSERT INTO build_logs (build_id, drv_hash, s3_key, line_count, byte_size, is_complete)
             VALUES ($1, $2, $3, $4, $5, true)",
        )
        .bind(build_id)
        .bind("abc-test.drv")
        .bind(format!("logs/{build_id}/abc-test.drv.log.gz"))
        .bind(3_i64)
        .bind(gzipped.len() as i64)
        .execute(&db.pool)
        .await?;

        // Mock S3 to return the gzipped blob.
        let rule = mock!(S3Client::get_object).then_output(move || {
            GetObjectOutput::builder()
                .body(ByteStream::from(gzipped.clone()))
                .build()
        });
        let s3 = mock_client!(aws_sdk_s3, RuleMode::MatchAny, &[&rule]);

        // Ring buffer is EMPTY — forces S3 fallback.
        //
        // Can't use setup_svc here: this test seeds PG rows BEFORE
        // constructing svc (the flusher-written build_logs row), and
        // setup_svc creates its own TestDb. Wire manually.
        let buffers = Arc::new(LogBuffers::new());
        let (actor, _task) = setup_actor(db.pool.clone());
        let svc = AdminServiceImpl::new(
            buffers,
            Some((s3, "test-bucket".into())),
            db.pool.clone(),
            actor,
        );

        let resp = svc
            .get_build_logs(Request::new(GetBuildLogsRequest {
                build_id: build_id.to_string(),
                derivation_path: "/nix/store/abc-test.drv".into(),
                since_line: 0,
            }))
            .await?;

        let chunks = collect_stream(resp.into_inner()).await;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lines.len(), 3);
        assert_eq!(chunks[0].lines[0], b"from-s3-0");
        assert_eq!(chunks[0].lines[2], b"from-s3-2");
        assert!(
            chunks[0].is_complete,
            "S3 serve → derivation finished, is_complete=true"
        );
        Ok(())
    }

    #[tokio::test]
    async fn get_build_logs_not_found_in_either() -> anyhow::Result<()> {
        let buffers = Arc::new(LogBuffers::new());
        // No S3 configured, buffer empty.
        let (svc, _actor, _task, _db) = setup_svc(buffers, None).await;

        let result = svc
            .get_build_logs(Request::new(GetBuildLogsRequest {
                build_id: uuid::Uuid::new_v4().to_string(),
                derivation_path: "/nix/store/nowhere.drv".into(),
                since_line: 0,
            }))
            .await;

        let status = result.expect_err("should be NotFound");
        assert_eq!(status.code(), tonic::Code::NotFound);
        Ok(())
    }

    #[tokio::test]
    async fn get_build_logs_empty_drv_path_invalid() -> anyhow::Result<()> {
        let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        let result = svc
            .get_build_logs(Request::new(GetBuildLogsRequest {
                build_id: String::new(),
                derivation_path: String::new(),
                since_line: 0,
            }))
            .await;

        let status = result.expect_err("should be InvalidArgument");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("derivation_path"));
        Ok(())
    }

    #[tokio::test]
    async fn stubs_return_unimplemented() -> anyhow::Result<()> {
        let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        // ClusterStatus (E1) and DrainWorker (E2) are no longer stubs.
        assert_eq!(
            svc.list_workers(Request::new(ListWorkersRequest::default()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
        assert_eq!(
            svc.list_builds(Request::new(ListBuildsRequest::default()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
        assert_eq!(
            svc.trigger_gc(Request::new(GcRequest::default()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
        assert_eq!(
            svc.clear_poison(Request::new(ClearPoisonRequest::default()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
        Ok(())
    }

    #[test]
    fn extract_drv_hash_strips_store_prefix() {
        assert_eq!(extract_drv_hash("/nix/store/abc-foo.drv"), "abc-foo.drv");
        assert_eq!(extract_drv_hash("already-a-hash"), "already-a-hash");
    }

    #[test]
    fn gunzip_and_chunk_roundtrip() -> anyhow::Result<()> {
        // Gzip → gunzip_and_chunk → lines match.
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        for i in 0..5 {
            enc.write_all(format!("line-{i}").as_bytes())?;
            enc.write_all(b"\n")?;
        }
        let gz = enc.finish()?;

        let chunks = gunzip_and_chunk(&gz, "test", 0)?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lines.len(), 5, "trailing \\n artifact stripped");
        assert_eq!(chunks[0].lines[0], b"line-0");
        assert_eq!(chunks[0].lines[4], b"line-4");
        assert!(chunks[0].is_complete);
        Ok(())
    }

    #[test]
    fn gunzip_and_chunk_since_filtering() -> anyhow::Result<()> {
        use flate2::Compression;
        use flate2::write::GzEncoder;
        use std::io::Write;
        let mut enc = GzEncoder::new(Vec::new(), Compression::default());
        for i in 0..5 {
            enc.write_all(format!("l{i}").as_bytes())?;
            enc.write_all(b"\n")?;
        }
        let gz = enc.finish()?;

        let chunks = gunzip_and_chunk(&gz, "test", 3)?;
        assert_eq!(chunks.len(), 1);
        assert_eq!(chunks[0].lines.len(), 2, "since=3 → lines 3,4 only");
        assert_eq!(chunks[0].first_line_number, 3);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // ClusterStatus (E1)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn cluster_status_empty() -> anyhow::Result<()> {
        let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        let resp = svc.cluster_status(Request::new(())).await?.into_inner();

        assert_eq!(resp.total_workers, 0);
        assert_eq!(resp.active_workers, 0);
        assert_eq!(resp.draining_workers, 0);
        assert_eq!(resp.pending_builds, 0);
        assert_eq!(resp.active_builds, 0);
        assert_eq!(resp.queued_derivations, 0);
        assert_eq!(resp.running_derivations, 0);
        assert_eq!(
            resp.store_size_bytes, 0,
            "scheduler doesn't track store size"
        );

        // uptime_since is "now minus elapsed since construction" → within
        // a few hundred ms of now. Wide tolerance (10s) to survive slow CI.
        let uptime = resp.uptime_since.expect("uptime_since always set");
        let now = prost_types::Timestamp::from(SystemTime::now());
        let delta = now.seconds - uptime.seconds;
        assert!(
            (0..10).contains(&delta),
            "uptime_since should be recent (within 10s), got delta={delta}s"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cluster_status_counts_registered_workers() -> anyhow::Result<()> {
        use crate::actor::tests::connect_worker;

        let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        // Stream-only worker (no heartbeat) → total=1, active=0.
        // is_registered() requires BOTH stream_tx AND system; this has
        // only the stream. The autoscaler should NOT count it as
        // available capacity.
        let (stream_tx, _rx1) = mpsc::channel(16);
        actor
            .send_unchecked(ActorCommand::WorkerConnected {
                worker_id: "stream-only".into(),
                stream_tx,
            })
            .await?;

        // Fully registered worker (stream + heartbeat) → active.
        let _rx2 = connect_worker(&actor, "full", "x86_64-linux", 4).await?;

        let resp = svc.cluster_status(Request::new(())).await?.into_inner();

        assert_eq!(resp.total_workers, 2);
        assert_eq!(
            resp.active_workers, 1,
            "only 'full' is registered (stream+heartbeat); 'stream-only' has no heartbeat"
        );
        assert_eq!(resp.draining_workers, 0, "E2 adds draining; 0 until then");
        Ok(())
    }

    #[tokio::test]
    async fn cluster_status_counts_queued_and_running() -> anyhow::Result<()> {
        use crate::actor::tests::{connect_worker, merge_single_node};
        use crate::state::PriorityClass;

        let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        // Worker with max_builds=1: will accept exactly one assignment,
        // leaving the second derivation in ready_queue.
        let mut worker_rx = connect_worker(&actor, "w1", "x86_64-linux", 1).await?;

        // Two independent single-node DAGs. First dispatches (worker has
        // capacity 1), second stays queued.
        let _ev1 =
            merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
        let _ev2 =
            merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

        // Drain the assignment for 'a' — dispatch happened synchronously
        // during merge (dispatch_ready is called after merge completes),
        // but the message is in the channel. Receiving it doesn't change
        // actor state (worker ack → Running requires a separate
        // ProcessCompletion roundtrip we're not doing here), it just
        // proves the assignment went out.
        let msg = worker_rx.recv().await.expect("assignment for first drv");
        assert!(matches!(
            msg.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));

        let resp = svc.cluster_status(Request::new(())).await?.into_inner();

        // First derivation is Assigned (worker slot reserved, not yet acked
        // → Running). Second is in ready_queue (no capacity). BOTH builds
        // transition to Active on merge (merge.rs sets Active as soon as
        // derivations are tracked, not waiting for dispatch).
        assert_eq!(
            resp.active_builds, 2,
            "both builds transitioned to Active on merge"
        );
        assert_eq!(resp.pending_builds, 0);
        assert_eq!(
            resp.queued_derivations, 1,
            "second drv waiting for capacity (worker max_builds=1 full)"
        );
        assert_eq!(
            resp.running_derivations, 1,
            "first drv is Assigned → counts as running (slot reserved)"
        );
        Ok(())
    }

    #[tokio::test]
    async fn cluster_status_actor_dead_returns_unavailable() -> anyhow::Result<()> {
        // Set up, then drop the handle + abort the task → actor channel closes.
        // check_actor_alive() catches this before the oneshot would hang.
        let (svc, actor, task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;
        drop(actor);
        task.abort();
        // Give tokio a tick to process the abort.
        tokio::task::yield_now().await;

        // The svc still holds an ActorHandle clone. is_alive() checks
        // tx.is_closed() which becomes true when the receiver drops
        // (actor task gone). Abort + drop(actor) both contribute — abort
        // kills the task (receiver drops), drop(actor) removes one of
        // the two senders. The svc's clone is the last sender.
        //
        // Poll is_closed via the svc's handle until it flips. Bounded
        // loop (if it never flips in 100 yields, something's wrong).
        for _ in 0..100 {
            if !svc.actor.is_alive() {
                break;
            }
            tokio::task::yield_now().await;
        }

        let result = svc.cluster_status(Request::new(())).await;
        let status = result.expect_err("should be Unavailable");
        assert_eq!(status.code(), tonic::Code::Unavailable);
        assert!(status.message().contains("actor"));
        Ok(())
    }

    // -----------------------------------------------------------------------
    // DrainWorker (E2)
    // -----------------------------------------------------------------------

    #[tokio::test]
    async fn drain_worker_empty_id_invalid() -> anyhow::Result<()> {
        let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        let result = svc
            .drain_worker(Request::new(DrainWorkerRequest {
                worker_id: String::new(),
                force: false,
            }))
            .await;

        let status = result.expect_err("empty worker_id should be InvalidArgument");
        assert_eq!(status.code(), tonic::Code::InvalidArgument);
        assert!(status.message().contains("worker_id"));
        Ok(())
    }

    #[tokio::test]
    async fn drain_worker_unknown_not_error() -> anyhow::Result<()> {
        // Unknown worker → accepted=false, running=0. NOT gRPC error:
        // preStop may race with WorkerDisconnected (SIGTERM → select!
        // break → stream drop → actor removes entry → preStop's drain
        // call arrives to an empty slot). The worker proceeds as if
        // drain succeeded — nothing to wait for.
        let (svc, _actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        let resp = svc
            .drain_worker(Request::new(DrainWorkerRequest {
                worker_id: "ghost".into(),
                force: false,
            }))
            .await?
            .into_inner();

        assert!(!resp.accepted, "unknown worker → accepted=false");
        assert_eq!(resp.running_builds, 0);
        Ok(())
    }

    #[tokio::test]
    async fn drain_worker_stops_dispatch() -> anyhow::Result<()> {
        use crate::actor::tests::{connect_worker, merge_single_node};
        use crate::state::PriorityClass;

        let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        // Worker with max_builds=4: plenty of capacity.
        let mut worker_rx = connect_worker(&actor, "w1", "x86_64-linux", 4).await?;

        // First drv: dispatches normally.
        let _ev1 =
            merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;
        let msg1 = worker_rx.recv().await.expect("first assignment");
        assert!(matches!(
            msg1.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));

        // Drain. running=1 (the drv we just dispatched is Assigned on w1).
        let resp = svc
            .drain_worker(Request::new(DrainWorkerRequest {
                worker_id: "w1".into(),
                force: false,
            }))
            .await?
            .into_inner();
        assert!(resp.accepted);
        assert_eq!(
            resp.running_builds, 1,
            "the first drv is in-flight (Assigned)"
        );

        // Second drv: should NOT dispatch. Worker has capacity (1/4 slots
        // used) but has_capacity() now returns false (draining).
        let _ev2 =
            merge_single_node(&actor, uuid::Uuid::new_v4(), "b", PriorityClass::Scheduled).await?;

        // Can't easily assert "nothing arrived" without a timeout. Instead,
        // check ClusterStatus: the second drv should be queued, not running.
        let status = svc.cluster_status(Request::new(())).await?.into_inner();
        assert_eq!(status.queued_derivations, 1, "second drv waiting (drained)");
        assert_eq!(status.running_derivations, 1, "only first drv on worker");
        assert_eq!(status.draining_workers, 1);
        assert_eq!(
            status.active_workers, 0,
            "draining worker is NOT active — controller sees capacity=0"
        );

        // Idempotent: second drain → same running count, still accepted.
        let resp2 = svc
            .drain_worker(Request::new(DrainWorkerRequest {
                worker_id: "w1".into(),
                force: false,
            }))
            .await?
            .into_inner();
        assert!(resp2.accepted);
        assert_eq!(resp2.running_builds, 1);
        Ok(())
    }

    #[tokio::test]
    async fn drain_worker_force_reassigns() -> anyhow::Result<()> {
        use crate::actor::tests::{connect_worker, merge_single_node};
        use crate::state::PriorityClass;

        let (svc, actor, _task, _db) = setup_svc(Arc::new(LogBuffers::new()), None).await;

        // Two workers: w1 gets the first dispatch, then we force-drain it.
        // The reassigned drv should go to w2 on the next dispatch.
        let mut rx1 = connect_worker(&actor, "w1", "x86_64-linux", 2).await?;
        let mut rx2 = connect_worker(&actor, "w2", "x86_64-linux", 2).await?;

        let _ev =
            merge_single_node(&actor, uuid::Uuid::new_v4(), "a", PriorityClass::Scheduled).await?;

        // ONE of them got it. With two equal-score workers (no bloom,
        // both 0/2 load), best_worker's tiebreak is HashMap iteration
        // order → nondeterministic. Poll both with try_recv to find which.
        let (first_worker, other_rx) = if let Ok(msg) = rx1.try_recv() {
            assert!(matches!(
                msg.msg,
                Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
            ));
            ("w1", &mut rx2)
        } else {
            let msg = rx2
                .try_recv()
                .expect("one of w1/w2 must have the assignment");
            assert!(matches!(
                msg.msg,
                Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
            ));
            ("w2", &mut rx1)
        };

        // Force-drain the worker that got it. running=0 in response:
        // force reassigns then replies, so nothing is left.
        let resp = svc
            .drain_worker(Request::new(DrainWorkerRequest {
                worker_id: first_worker.into(),
                force: true,
            }))
            .await?
            .into_inner();
        assert!(resp.accepted);
        assert_eq!(
            resp.running_builds, 0,
            "force=true reassigned → running=0 (caller doesn't wait)"
        );

        // reassign_derivations pushes to ready_queue but dispatch_ready
        // isn't called from handle_drain_worker — it fires on the NEXT
        // heartbeat/merge/completion. Send a heartbeat to trigger it.
        actor
            .send_unchecked(ActorCommand::Heartbeat {
                worker_id: first_worker.into(),
                systems: vec!["x86_64-linux".into()],
                supported_features: vec![],
                max_builds: 2,
                running_builds: vec![],
                bloom: None,
                size_class: None,
            })
            .await?;

        // The OTHER worker should now get the reassigned drv.
        let msg = other_rx
            .recv()
            .await
            .expect("reassigned drv to other worker");
        assert!(matches!(
            msg.msg,
            Some(rio_proto::types::scheduler_message::Msg::Assignment(_))
        ));

        // ClusterStatus: 1 draining, 1 active, 1 running (on the other
        // worker now), 0 queued.
        let status = svc.cluster_status(Request::new(())).await?.into_inner();
        assert_eq!(status.draining_workers, 1);
        assert_eq!(status.active_workers, 1);
        assert_eq!(
            status.running_derivations, 1,
            "drv re-Assigned to other worker"
        );
        assert_eq!(status.queued_derivations, 0);
        Ok(())
    }
}
