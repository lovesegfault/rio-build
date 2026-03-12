//! AdminService gRPC implementation.
//!
//! `GetBuildLogs`, `ClusterStatus`, `DrainWorker`, and `TriggerGC`
//! are fully implemented. The remaining RPCs return `UNIMPLEMENTED`
//! and land in phase4 (dashboard). Stubbing them here means the
//! tonic server wiring is already in place — adding each later is
//! a pure body-swap with no main.rs churn.
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
    /// Store gRPC address for TriggerGC proxy. The scheduler's
    /// `AdminService.TriggerGC` collects extra_roots via
    /// `ActorCommand::GcRoots`, then proxies to the store's
    /// `StoreAdminService.TriggerGC`. GC runs IN the store (it
    /// owns the chunk backend); scheduler contributes roots.
    store_addr: String,
}

impl AdminServiceImpl {
    pub fn new(
        log_buffers: Arc<LogBuffers>,
        s3: Option<(S3Client, String)>,
        pool: PgPool,
        actor: ActorHandle,
        store_addr: String,
    ) -> Self {
        Self {
            log_buffers,
            s3,
            pool,
            actor,
            started_at: Instant::now(),
            store_addr,
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
    /// `TODO(phase4)`: add a best-effort store-size field populated
    /// via a separate slow-refresh background task, NOT inline here.
    #[instrument(skip(self, request), fields(rpc = "ClusterStatus"))]
    async fn cluster_status(
        &self,
        request: Request<()>,
    ) -> Result<Response<ClusterStatusResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
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

    /// Proxy to store's `StoreAdminService.TriggerGC` after
    /// populating `extra_roots` from the scheduler's live builds.
    ///
    /// Flow:
    /// 1. `ActorCommand::GcRoots` → collect expected_output_paths
    ///    from all non-terminal derivations. These may not be in
    ///    narinfo yet (worker hasn't uploaded); the store's mark
    ///    phase includes them as root seeds so in-flight outputs
    ///    aren't collected.
    /// 2. Connect to store, call TriggerGC with the populated
    ///    extra_roots + client's dry_run/grace_period.
    /// 3. Proxy the store's GCProgress stream back to the client.
    ///
    /// If store is unreachable: UNAVAILABLE (not UNIMPLEMENTED —
    /// the RPC IS implemented, store is just down). Client retries.
    #[instrument(skip(self, request), fields(rpc = "TriggerGC"))]
    async fn trigger_gc(
        &self,
        request: Request<GcRequest>,
    ) -> Result<Response<Self::TriggerGCStream>, Status> {
        rio_proto::interceptor::link_parent(&request);
        self.check_actor_alive()?;
        let mut req = request.into_inner();

        // Step 1: collect extra_roots from the actor. send_unchecked
        // bypasses backpressure — GC is operator-initiated, rare,
        // and should work even when the scheduler is saturated.
        let (tx, rx) = oneshot::channel();
        self.actor
            .send_unchecked(ActorCommand::GcRoots { reply: tx })
            .await
            .map_err(|e| Status::internal(format!("actor send failed: {e}")))?;
        let mut extra_roots = rx
            .await
            .map_err(|_| Status::internal("actor GcRoots reply dropped"))?;

        // Merge with any client-provided extra_roots (unusual but
        // allowed — maybe the client has additional pins).
        req.extra_roots.append(&mut extra_roots);
        let extra_count = req.extra_roots.len();

        debug!(
            dry_run = req.dry_run,
            grace_hours = ?req.grace_period_hours,
            extra_roots = extra_count,
            "proxying TriggerGC to store with live-build roots"
        );

        // Step 2: connect to store admin service. Same TLS config
        // as connect_store (OnceLock CLIENT_TLS).
        let mut store_admin = rio_proto::client::connect_store_admin(&self.store_addr)
            .await
            .map_err(|e| Status::unavailable(format!("store admin connect failed: {e}")))?;

        // Step 3: proxy the call. The store's stream becomes OUR
        // stream — we wrap it in a forwarding task.
        let store_stream = store_admin
            .trigger_gc(req)
            .await
            .map_err(|e| Status::internal(format!("store TriggerGC failed: {e}")))?
            .into_inner();

        // Forward store's progress stream to the client. A small
        // channel + forwarding task: the store stream isn't
        // directly compatible with our TriggerGCStream type (we
        // declare it as ReceiverStream).
        let (tx, rx) = mpsc::channel::<Result<GcProgress, Status>>(8);
        tokio::spawn(async move {
            let mut store_stream = store_stream;
            loop {
                match store_stream.message().await {
                    Ok(Some(progress)) => {
                        if tx.send(Ok(progress)).await.is_err() {
                            // Client disconnected. Let the store-
                            // side GC finish (it's already running);
                            // just stop forwarding.
                            break;
                        }
                    }
                    Ok(None) => {
                        // Store stream EOF (GC complete). Drop tx
                        // → client sees stream end.
                        break;
                    }
                    Err(e) => {
                        // Store error mid-stream. Forward the error;
                        // client decides whether to retry.
                        let _ = tx.send(Err(e)).await;
                        break;
                    }
                }
            }
        });

        Ok(Response::new(ReceiverStream::new(rx)))
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
        rio_proto::interceptor::link_parent(&request);
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
mod tests;
