//! AdminService gRPC implementation.
//!
//! First real AdminService impl (phase2b). Only `GetBuildLogs` is fully
//! implemented; the other six RPCs return `UNIMPLEMENTED`. They'll land
//! in phase 4 (dashboard) — stubbing them here means the tonic server
//! wiring is already in place, and adding each one later is a pure
//! body-swap with no main.rs/wiring churn.
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

use aws_sdk_s3::Client as S3Client;
use flate2::read::GzDecoder;
use sqlx::PgPool;
use tokio::sync::mpsc;
use tokio_stream::wrappers::ReceiverStream;
use tonic::{Request, Response, Status};
use tracing::{debug, instrument};

use rio_proto::AdminService;
use rio_proto::types::{
    BuildLogChunk, ClearPoisonRequest, ClearPoisonResponse, ClusterStatusResponse,
    DrainWorkerRequest, DrainWorkerResponse, GcProgress, GcRequest, GetBuildLogsRequest,
    ListBuildsRequest, ListBuildsResponse, ListWorkersRequest, ListWorkersResponse,
};

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
}

impl AdminServiceImpl {
    pub fn new(log_buffers: Arc<LogBuffers>, s3: Option<(S3Client, String)>, pool: PgPool) -> Self {
        Self {
            log_buffers,
            s3,
            pool,
        }
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

    // -----------------------------------------------------------------------
    // Stubs — return UNIMPLEMENTED. Phase 4 (dashboard) implements these.
    // -----------------------------------------------------------------------

    async fn cluster_status(
        &self,
        _request: Request<()>,
    ) -> Result<Response<ClusterStatusResponse>, Status> {
        Err(Status::unimplemented("ClusterStatus: phase4 dashboard"))
    }

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

    async fn drain_worker(
        &self,
        _request: Request<DrainWorkerRequest>,
    ) -> Result<Response<DrainWorkerResponse>, Status> {
        Err(Status::unimplemented("DrainWorker: phase3a controller"))
    }

    async fn clear_poison(
        &self,
        _request: Request<ClearPoisonRequest>,
    ) -> Result<Response<ClearPoisonResponse>, Status> {
        Err(Status::unimplemented("ClearPoison: phase3a controller"))
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
    use aws_sdk_s3::operation::get_object::GetObjectOutput;
    use aws_sdk_s3::primitives::ByteStream;
    use aws_smithy_mocks::{RuleMode, mock, mock_client};
    use rio_proto::types::BuildLogBatch;
    use rio_test_support::TestDb;
    use tokio_stream::StreamExt;

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
        let db = TestDb::new(&crate::MIGRATOR).await;
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/abc-test.drv",
            0,
            &[b"line0", b"line1", b"line2"],
        ));

        let svc = AdminServiceImpl::new(buffers, None, db.pool.clone());

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
        let db = TestDb::new(&crate::MIGRATOR).await;
        let buffers = Arc::new(LogBuffers::new());
        buffers.push(&mk_batch(
            "/nix/store/abc-test.drv",
            0,
            &[b"l0", b"l1", b"l2", b"l3", b"l4"],
        ));

        let svc = AdminServiceImpl::new(buffers, None, db.pool.clone());

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
        let buffers = Arc::new(LogBuffers::new());
        let svc = AdminServiceImpl::new(buffers, Some((s3, "test-bucket".into())), db.pool.clone());

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
        let db = TestDb::new(&crate::MIGRATOR).await;
        let buffers = Arc::new(LogBuffers::new());
        // No S3 configured, buffer empty.
        let svc = AdminServiceImpl::new(buffers, None, db.pool.clone());

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
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = AdminServiceImpl::new(Arc::new(LogBuffers::new()), None, db.pool.clone());

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
        let db = TestDb::new(&crate::MIGRATOR).await;
        let svc = AdminServiceImpl::new(Arc::new(LogBuffers::new()), None, db.pool.clone());

        assert_eq!(
            svc.cluster_status(Request::new(()))
                .await
                .unwrap_err()
                .code(),
            tonic::Code::Unimplemented
        );
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
            svc.drain_worker(Request::new(DrainWorkerRequest::default()))
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
}
