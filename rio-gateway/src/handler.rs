//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation via gRPC delegation, and writes the response via the STDERR
//! streaming loop.
//!
//! Compared to the Phase 1b handler in rio-build, all `Store` trait calls
//! are replaced with `StoreServiceClient` gRPC calls, and all build operations
//! delegate to the scheduler via `SchedulerServiceClient`.

use std::collections::{HashMap, HashSet};

use rio_nix::derivation::Derivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarNode};
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::derived_path::DerivedPath;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{ActivityType, StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::scheduler::scheduler_service_client::SchedulerServiceClient;
use rio_proto::store::store_service_client::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{debug, error, instrument, warn};

use crate::translate;

const PROGRAM_NAME: &str = "rio-gateway";

/// Client build options received via wopSetOptions.
///
/// Propagated to the scheduler via gRPC `SubmitBuildRequest`.
#[derive(Debug, Clone)]
pub struct ClientOptions {
    pub keep_failed: bool,
    pub keep_going: bool,
    pub try_fallback: bool,
    pub verbosity: u64,
    pub max_build_jobs: u64,
    pub max_silent_time: u64,
    pub verbose_build: bool,
    pub build_cores: u64,
    pub use_substitutes: bool,
    pub overrides: Vec<(String, String)>,
}

impl ClientOptions {
    /// Extract the build timeout from overrides, defaulting to 0 (no timeout).
    pub fn build_timeout(&self) -> u64 {
        self.overrides
            .iter()
            .find(|(k, _)| k == "build-timeout")
            .and_then(|(_, v)| v.parse::<u64>().ok())
            .unwrap_or(0)
    }
}

/// Dispatch an opcode to the appropriate handler.
///
/// Returns `Ok(())` on success. On errors, sends `STDERR_ERROR` to the
/// client and returns `Err`, which terminates the session.
#[instrument(skip_all, fields(opcode))]
#[allow(clippy::too_many_arguments)]
pub async fn handle_opcode<R, W>(
    opcode: u64,
    reader: &mut R,
    writer: &mut W,
    store_client: &mut StoreServiceClient<Channel>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    options: &mut Option<ClientOptions>,
    temp_roots: &mut HashSet<StorePath>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
    has_seen_build_paths_with_results: &mut bool,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin + Send,
    W: AsyncWrite + Unpin,
{
    let mut stderr = StderrWriter::new(writer);
    let start = std::time::Instant::now();

    let op = WorkerOp::from_u64(opcode);
    let op_name = op.map(|o| o.name()).unwrap_or("unknown");
    tracing::Span::current().record("opcode", op_name);
    metrics::counter!("rio_gateway_opcodes_total", "opcode" => op_name).increment(1);

    let result = match op {
        Some(WorkerOp::IsValidPath) => {
            handle_is_valid_path(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::AddToStore) => {
            handle_add_to_store(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::AddTextToStore) => {
            handle_add_text_to_store(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::EnsurePath) => handle_ensure_path(reader, &mut stderr, store_client).await,
        Some(WorkerOp::QueryPathInfo) => {
            handle_query_path_info(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::QueryValidPaths) => {
            handle_query_valid_paths(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::AddTempRoot) => handle_add_temp_root(reader, &mut stderr, temp_roots).await,
        Some(WorkerOp::SetOptions) => handle_set_options(reader, &mut stderr, options).await,
        Some(WorkerOp::NarFromPath) => {
            handle_nar_from_path(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::QueryPathFromHashPart) => {
            handle_query_path_from_hash_part(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::AddSignatures) => {
            handle_add_signatures(reader, &mut stderr, store_client).await
        }
        Some(WorkerOp::QueryMissing) => {
            handle_query_missing(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::AddToStoreNar) => {
            handle_add_to_store_nar(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::AddMultipleToStore) => {
            handle_add_multiple_to_store(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(reader, &mut stderr, store_client, drv_cache).await
        }
        Some(WorkerOp::BuildDerivation) => {
            handle_build_derivation(
                reader,
                &mut stderr,
                store_client,
                scheduler_client,
                options,
                drv_cache,
                has_seen_build_paths_with_results,
                active_build_ids,
            )
            .await
        }
        Some(WorkerOp::BuildPaths) => {
            handle_build_paths(
                reader,
                &mut stderr,
                store_client,
                scheduler_client,
                options,
                drv_cache,
                active_build_ids,
            )
            .await
        }
        Some(WorkerOp::BuildPathsWithResults) => {
            *has_seen_build_paths_with_results = true;
            handle_build_paths_with_results(
                reader,
                &mut stderr,
                store_client,
                scheduler_client,
                options,
                drv_cache,
                active_build_ids,
            )
            .await
        }
        Some(WorkerOp::RegisterDrvOutput) => handle_register_drv_output(reader, &mut stderr).await,
        Some(WorkerOp::QueryRealisation) => handle_query_realisation(reader, &mut stderr).await,
        None => {
            warn!(opcode = opcode, "unknown opcode, closing connection");
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("unsupported operation {opcode}"),
                ))
                .await?;
            Err(anyhow::anyhow!(
                "unknown opcode {opcode}, closing connection to avoid stream desynchronization"
            ))
        }
    };

    let elapsed = start.elapsed();
    metrics::histogram!("rio_gateway_opcode_duration_seconds", "opcode" => op_name)
        .record(elapsed.as_secs_f64());

    if result.is_err() {
        metrics::counter!("rio_gateway_errors_total", "type" => "protocol").increment(1);
    }

    result
}

/// Send a store error as STDERR_ERROR to the client, then return the error.
async fn send_store_error<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    err: anyhow::Error,
) -> anyhow::Result<()> {
    if let Err(send_err) = stderr
        .error(&StderrError::simple(
            PROGRAM_NAME,
            format!("store error: {err}"),
        ))
        .await
    {
        warn!(error = %send_err, "failed to send store error to client");
    }
    Err(err)
}

// ---------------------------------------------------------------------------
// Store-query helpers using gRPC
// ---------------------------------------------------------------------------

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<types::PathInfo>> {
    let req = types::QueryPathInfoRequest {
        store_path: store_path.to_string(),
    };
    match store_client.query_path_info(req).await {
        Ok(resp) => Ok(Some(resp.into_inner())),
        Err(status) if status.code() == tonic::Code::NotFound => Ok(None),
        Err(e) => Err(anyhow::anyhow!("gRPC QueryPathInfo failed: {e}")),
    }
}

/// Check validity via QueryPathInfo -- returns true if path exists.
async fn grpc_is_valid_path(
    store_client: &mut StoreServiceClient<Channel>,
    path: &StorePath,
) -> anyhow::Result<bool> {
    Ok(grpc_query_path_info(store_client, &path.to_string())
        .await?
        .is_some())
}

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    info: types::PathInfo,
    nar_data: Vec<u8>,
) -> anyhow::Result<bool> {
    let metadata_msg = types::PutPathRequest {
        msg: Some(types::put_path_request::Msg::Metadata(
            types::PutPathMetadata { info: Some(info) },
        )),
    };

    // Send metadata first, then NAR data in chunks
    let chunk_size = 64 * 1024;
    let mut messages = vec![metadata_msg];
    for chunk in nar_data.chunks(chunk_size) {
        messages.push(types::PutPathRequest {
            msg: Some(types::put_path_request::Msg::NarChunk(chunk.to_vec())),
        });
    }

    let stream = tokio_stream::iter(messages);
    let resp = store_client
        .put_path(stream)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC PutPath failed: {e}"))?;

    Ok(resp.into_inner().created)
}

use rio_common::limits::MAX_NAR_SIZE;

/// Fetch NAR data from store via gRPC GetPath.
/// Returns (PathInfo, NAR bytes) or None if not found.
async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<Option<(types::PathInfo, Vec<u8>)>> {
    let req = types::GetPathRequest {
        store_path: store_path.to_string(),
    };
    let mut stream = match store_client.get_path(req).await {
        Ok(resp) => resp.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => return Ok(None),
        Err(e) => return Err(anyhow::anyhow!("gRPC GetPath failed: {e}")),
    };

    let mut info = None;
    let mut nar_data = Vec::new();

    while let Some(msg) = stream
        .message()
        .await
        .map_err(|e| anyhow::anyhow!("gRPC GetPath stream error: {e}"))?
    {
        match msg.msg {
            Some(types::get_path_response::Msg::Info(i)) => {
                info = Some(i);
            }
            Some(types::get_path_response::Msg::NarChunk(chunk)) => {
                let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
                if new_len > MAX_NAR_SIZE {
                    return Err(anyhow::anyhow!(
                        "NAR for {store_path} exceeds maximum {} bytes (received {}+)",
                        MAX_NAR_SIZE,
                        new_len
                    ));
                }
                nar_data.extend_from_slice(&chunk);
            }
            None => {}
        }
    }

    match info {
        Some(i) => Ok(Some((i, nar_data))),
        None => Ok(None),
    }
}

/// Build a proto PathInfo from local data (for PutPath).
#[allow(clippy::too_many_arguments)]
fn make_proto_path_info(
    store_path: &str,
    deriver: &str,
    nar_hash: &[u8],
    nar_size: u64,
    references: &[String],
    registration_time: u64,
    ultimate: bool,
    sigs: &[String],
    ca: &str,
) -> types::PathInfo {
    types::PathInfo {
        store_path: store_path.to_string(),
        store_path_hash: Vec::new(),
        deriver: deriver.to_string(),
        nar_hash: nar_hash.to_vec(),
        nar_size,
        references: references.to_vec(),
        registration_time,
        ultimate,
        signatures: sigs.to_vec(),
        content_address: ca.to_string(),
    }
}

// ---------------------------------------------------------------------------
// Derivation cache helpers
// ---------------------------------------------------------------------------

/// If `path` is a `.drv`, parse the ATerm from NAR data and cache it.
fn try_cache_drv(
    path: &StorePath,
    nar_data: &[u8],
    drv_cache: &mut HashMap<StorePath, Derivation>,
) {
    if !path.is_derivation() {
        return;
    }
    let drv_bytes = match rio_nix::nar::extract_single_file(nar_data) {
        Ok(bytes) => bytes,
        Err(e) => {
            warn!(path = %path, error = %e, "failed to extract .drv from NAR");
            return;
        }
    };
    let drv_text = match String::from_utf8(drv_bytes) {
        Ok(text) => text,
        Err(e) => {
            warn!(path = %path, error = %e, "failed to decode .drv as UTF-8");
            return;
        }
    };
    match Derivation::parse(&drv_text) {
        Ok(drv) => {
            debug!(path = %path, "cached parsed derivation");
            drv_cache.insert(path.clone(), drv);
        }
        Err(e) => {
            warn!(path = %path, error = %e, "failed to parse .drv ATerm");
        }
    }
}

/// Look up a derivation from session cache, or fetch from store via gRPC,
/// parse the ATerm, and cache it.
pub(crate) async fn resolve_derivation(
    drv_path: &StorePath,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<Derivation> {
    if let Some(cached) = drv_cache.get(drv_path) {
        return Ok(cached.clone());
    }

    let (_info, nar_data) = match grpc_get_path(store_client, &drv_path.to_string()).await? {
        Some(r) => r,
        None => {
            return Err(anyhow::anyhow!(
                "derivation '{}' not found in store",
                drv_path
            ));
        }
    };

    let drv_bytes = rio_nix::nar::extract_single_file(&nar_data)
        .map_err(|e| anyhow::anyhow!("failed to extract .drv '{}' from NAR: {e}", drv_path))?;

    let drv_text = String::from_utf8(drv_bytes)
        .map_err(|e| anyhow::anyhow!("invalid UTF-8 in .drv '{}': {e}", drv_path))?;

    let drv = Derivation::parse(&drv_text)
        .map_err(|e| anyhow::anyhow!("failed to parse .drv '{}': {e}", drv_path))?;

    drv_cache.insert(drv_path.clone(), drv.clone());
    Ok(drv)
}

// ---------------------------------------------------------------------------
// Build event → STDERR translation
// ---------------------------------------------------------------------------

/// Process a BuildEvent stream from the scheduler and translate events
/// into STDERR protocol messages for the Nix client.
///
/// Returns the final BuildResult on success, or an error message on failure.
#[allow(unused_assignments, clippy::ptr_arg)]
async fn process_build_events<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    event_stream: &mut tonic::codec::Streaming<types::BuildEvent>,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<BuildEventOutcome> {
    let mut drv_activity_ids: HashMap<String, u64> = HashMap::new();
    let mut last_sequence: u64 = 0;

    while let Some(event) = event_stream
        .message()
        .await
        .map_err(|e| anyhow::anyhow!("build event stream error: {e}"))?
    {
        last_sequence = event.sequence;

        // Update active_build_ids with latest sequence
        if let Some(entry) = active_build_ids
            .iter_mut()
            .find(|(id, _)| *id == event.build_id)
        {
            entry.1 = last_sequence;
        }

        match event.event {
            Some(types::build_event::Event::Log(log_batch)) => {
                for line in &log_batch.lines {
                    let text = String::from_utf8_lossy(line);
                    stderr.log(&text).await?;
                }
            }
            Some(types::build_event::Event::Derivation(drv_event)) => {
                match drv_event.status {
                    Some(types::derivation_event::Status::Started(_)) => {
                        // start_activity auto-assigns the ID
                        let aid = stderr
                            .start_activity(
                                ActivityType::Build,
                                &format!("building '{}'", drv_event.derivation_path),
                                0, // level
                                0, // parent
                            )
                            .await?;
                        drv_activity_ids.insert(drv_event.derivation_path.clone(), aid);
                    }
                    Some(types::derivation_event::Status::Completed(_)) => {
                        if let Some(aid) = drv_activity_ids.remove(&drv_event.derivation_path) {
                            stderr.stop_activity(aid).await?;
                        }
                    }
                    Some(types::derivation_event::Status::Failed(ref failed)) => {
                        if let Some(aid) = drv_activity_ids.remove(&drv_event.derivation_path) {
                            stderr.stop_activity(aid).await?;
                        }
                        // Log failure via STDERR_NEXT
                        stderr
                            .log(&format!(
                                "derivation '{}' failed: {}",
                                drv_event.derivation_path, failed.error_message
                            ))
                            .await?;
                    }
                    Some(types::derivation_event::Status::Cached(_)) => {
                        // No activity to start/stop for cached derivations
                    }
                    Some(types::derivation_event::Status::Queued(_)) => {
                        // No STDERR message for queued state
                    }
                    None => {}
                }
            }
            Some(types::build_event::Event::Started(started)) => {
                debug!(
                    total = started.total_derivations,
                    cached = started.cached_derivations,
                    "build started"
                );
            }
            Some(types::build_event::Event::Progress(prog)) => {
                debug!(
                    completed = prog.completed,
                    running = prog.running,
                    queued = prog.queued,
                    total = prog.total,
                    "build progress"
                );
            }
            Some(types::build_event::Event::Completed(completed)) => {
                return Ok(BuildEventOutcome::Completed {
                    output_paths: completed.output_paths,
                });
            }
            Some(types::build_event::Event::Failed(failed)) => {
                return Ok(BuildEventOutcome::Failed {
                    error_message: failed.error_message,
                    failed_derivation: failed.failed_derivation,
                });
            }
            Some(types::build_event::Event::Cancelled(cancelled)) => {
                return Ok(BuildEventOutcome::Cancelled {
                    reason: cancelled.reason,
                });
            }
            None => {}
        }
    }

    // Stream ended without a terminal event. Send STDERR_ERROR so the Nix
    // client sees a clear failure instead of just a dropped connection.
    let err_msg = "build event stream ended unexpectedly (scheduler disconnected?)";
    let stderr_err = rio_nix::protocol::stderr::StderrError::simple("rio-gateway", err_msg);
    if let Err(e) = stderr.error(&stderr_err).await {
        tracing::warn!(error = %e, "failed to send STDERR_ERROR for unexpected stream end");
    }
    Err(anyhow::anyhow!(err_msg))
}

/// Outcome of processing a build event stream.
enum BuildEventOutcome {
    Completed {
        output_paths: Vec<String>,
    },
    Failed {
        error_message: String,
        #[allow(dead_code)] // Informational, may be used for error context in future
        failed_derivation: String,
    },
    Cancelled {
        reason: String,
    },
}

/// Submit a build to the scheduler and process events, returning a BuildResult.
async fn submit_and_process_build<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    request: types::SubmitBuildRequest,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<BuildResult> {
    let mut event_stream = scheduler_client
        .submit_build(request)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC SubmitBuild failed: {e}"))?
        .into_inner();

    // Extract build_id from first event if available
    // (the first event should be BuildStarted)
    // We'll track the build_id as we process events

    // Peek at first message to get build_id
    let first = event_stream
        .message()
        .await
        .map_err(|e| anyhow::anyhow!("build event stream error: {e}"))?;

    let build_id = match &first {
        Some(ev) => ev.build_id.clone(),
        None => return Err(anyhow::anyhow!("empty build event stream")),
    };

    active_build_ids.push((build_id.clone(), 0));

    // Process the first event
    if let Some(ev) = &first {
        if let Some(types::build_event::Event::Started(ref started)) = ev.event {
            debug!(
                build_id = %build_id,
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
        }
        if let Some(types::build_event::Event::Completed(ref completed)) = ev.event {
            // Remove from active builds
            active_build_ids.retain(|(id, _)| *id != build_id);
            return Ok(build_result_from_completed(&completed.output_paths));
        }
        if let Some(types::build_event::Event::Failed(ref failed)) = ev.event {
            active_build_ids.retain(|(id, _)| *id != build_id);
            return Ok(BuildResult::failure(
                BuildStatus::MiscFailure,
                failed.error_message.clone(),
            ));
        }
    }

    // Process remaining events
    let outcome = process_build_events(stderr, &mut event_stream, active_build_ids).await;

    // Remove from active builds
    active_build_ids.retain(|(id, _)| *id != build_id);

    match outcome {
        Ok(BuildEventOutcome::Completed { output_paths }) => {
            Ok(build_result_from_completed(&output_paths))
        }
        Ok(BuildEventOutcome::Failed { error_message, .. }) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            error_message,
        )),
        Ok(BuildEventOutcome::Cancelled { reason }) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            format!("build cancelled: {reason}"),
        )),
        Err(e) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            format!("build stream error: {e}"),
        )),
    }
}

fn build_result_from_completed(_output_paths: &[String]) -> BuildResult {
    BuildResult::new(
        BuildStatus::Built,
        String::new(),
        1, // timesBuilt
        false,
        0,
        0,
        None,
        None,
        Vec::new(),
    )
}

// ---------------------------------------------------------------------------
// Opcode handlers
// ---------------------------------------------------------------------------

/// wopIsValidPath (1): Check if a store path exists.
#[instrument(skip_all)]
async fn handle_is_valid_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopIsValidPath");

    let valid = match StorePath::parse(&path_str) {
        Ok(path) => match grpc_is_valid_path(store_client, &path).await {
            Ok(v) => v,
            Err(e) => return send_store_error(stderr, e).await,
        },
        Err(e) => {
            debug!(path = %path_str, error = %e, "wopIsValidPath: unparseable store path");
            false
        }
    };

    stderr.finish().await?;
    wire::write_bool(stderr.inner_mut(), valid).await?;
    Ok(())
}

/// wopEnsurePath (10): Ensure a store path is valid/available.
#[instrument(skip_all)]
async fn handle_ensure_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopEnsurePath");

    if let Ok(path) = StorePath::parse(&path_str).inspect_err(|e| {
        debug!(path = %path_str, error = %e, "wopEnsurePath: unparseable store path");
    }) {
        match grpc_is_valid_path(store_client, &path).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(path = %path_str, "wopEnsurePath: path not in store (no substituters)");
            }
            Err(e) => {
                error!(path = %path_str, error = %e, "wopEnsurePath: store error");
                return send_store_error(
                    stderr,
                    anyhow::anyhow!("store error checking '{}': {e}", path_str),
                )
                .await;
            }
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopQueryPathInfo (26): Return full path metadata.
#[instrument(skip_all)]
async fn handle_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopQueryPathInfo");

    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => {
            warn!(path = %path_str, error = %e, "invalid store path in wopQueryPathInfo");
            stderr.finish().await?;
            wire::write_bool(stderr.inner_mut(), false).await?;
            return Ok(());
        }
    };

    let info = match grpc_query_path_info(store_client, &path.to_string()).await {
        Ok(info) => info,
        Err(e) => return send_store_error(stderr, e).await,
    };

    stderr.finish().await?;
    let w = stderr.inner_mut();

    match info {
        None => {
            wire::write_bool(w, false).await?;
        }
        Some(info) => {
            wire::write_bool(w, true).await?;
            wire::write_string(w, &info.deriver).await?;
            // narHash: convert raw bytes to hex string
            wire::write_string(w, &hex::encode(&info.nar_hash)).await?;
            wire::write_strings(w, &info.references).await?;
            wire::write_u64(w, info.registration_time).await?;
            wire::write_u64(w, info.nar_size).await?;
            wire::write_bool(w, info.ultimate).await?;
            wire::write_strings(w, &info.signatures).await?;
            wire::write_string(w, &info.content_address).await?;
        }
    }

    Ok(())
}

/// wopQueryValidPaths (31): Batch validity check.
#[instrument(skip_all)]
async fn handle_query_valid_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_strs = wire::read_strings(reader).await?;
    let _substitute = wire::read_bool(reader).await?;

    debug!(count = path_strs.len(), "wopQueryValidPaths");

    // Use FindMissingPaths and invert to get valid paths
    let req = types::FindMissingPathsRequest {
        store_paths: path_strs.clone(),
    };
    let resp = store_client
        .find_missing_paths(req)
        .await
        .map_err(|e| anyhow::anyhow!("gRPC FindMissingPaths failed: {e}"));

    let missing_set: HashSet<String> = match resp {
        Ok(r) => r.into_inner().missing_paths.into_iter().collect(),
        Err(e) => return send_store_error(stderr, e).await,
    };

    let valid_strs: Vec<String> = path_strs
        .into_iter()
        .filter(|p| !missing_set.contains(p))
        .collect();

    stderr.finish().await?;
    wire::write_strings(stderr.inner_mut(), &valid_strs).await?;
    Ok(())
}

/// wopAddTempRoot (11): Register a temporary GC root.
#[instrument(skip_all)]
async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    temp_roots: &mut HashSet<StorePath>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopAddTempRoot");

    match StorePath::parse(&path_str) {
        Ok(path) => {
            temp_roots.insert(path);
        }
        Err(e) => {
            warn!(path = %path_str, error = %e, "invalid store path in wopAddTempRoot, ignoring");
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopSetOptions (19): Accept client build configuration.
#[instrument(skip_all)]
async fn handle_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    options: &mut Option<ClientOptions>,
) -> anyhow::Result<()> {
    let keep_failed = wire::read_bool(reader).await?;
    let keep_going = wire::read_bool(reader).await?;
    let try_fallback = wire::read_bool(reader).await?;
    let verbosity = wire::read_u64(reader).await?;
    let max_build_jobs = wire::read_u64(reader).await?;
    let max_silent_time = wire::read_u64(reader).await?;
    let _obsolete_use_build_hook = wire::read_u64(reader).await?;
    let verbose_build = wire::read_bool(reader).await?;
    let _obsolete_log_type = wire::read_u64(reader).await?;
    let _obsolete_print_build_trace = wire::read_u64(reader).await?;
    let build_cores = wire::read_u64(reader).await?;
    let use_substitutes = wire::read_bool(reader).await?;

    let overrides = wire::read_string_pairs(reader).await?;

    debug!(
        verbosity = verbosity,
        max_build_jobs = max_build_jobs,
        build_cores = build_cores,
        overrides_count = overrides.len(),
        "wopSetOptions"
    );

    *options = Some(ClientOptions {
        keep_failed,
        keep_going,
        try_fallback,
        verbosity,
        max_build_jobs,
        max_silent_time,
        verbose_build,
        build_cores,
        use_substitutes,
        overrides,
    });

    stderr.finish().await?;
    Ok(())
}

/// wopNarFromPath (38): Export path as NAR via STDERR_WRITE chunks.
#[instrument(skip_all)]
async fn handle_nar_from_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopNarFromPath");

    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => {
            debug!(path = %path_str, error = %e, "invalid store path in wopNarFromPath");
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid store path '{path_str}': {e}"),
                ))
                .await?;
            return Ok(());
        }
    };

    // Stream NAR from store via gRPC GetPath
    let req = types::GetPathRequest {
        store_path: path.to_string(),
    };
    let mut stream = match store_client.get_path(req).await {
        Ok(resp) => resp.into_inner(),
        Err(status) if status.code() == tonic::Code::NotFound => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("path '{}' is not valid", path_str),
                ))
                .await?;
            return Ok(());
        }
        Err(e) => {
            return send_store_error(stderr, anyhow::anyhow!("gRPC GetPath failed: {e}")).await;
        }
    };

    // Stream NAR chunks via STDERR_WRITE. On mid-stream error, send
    // STDERR_ERROR so the client gets a proper error instead of a partial
    // NAR followed by connection drop.
    loop {
        match stream.message().await {
            Ok(Some(msg)) => match msg.msg {
                Some(types::get_path_response::Msg::NarChunk(chunk)) => {
                    stderr.write_data(&chunk).await?;
                }
                Some(types::get_path_response::Msg::Info(_)) => {
                    // PathInfo in first message, skip for NAR streaming
                }
                None => {}
            },
            Ok(None) => break, // stream complete
            Err(e) => {
                return send_store_error(
                    stderr,
                    anyhow::anyhow!("gRPC GetPath stream error mid-transfer: {e}"),
                )
                .await;
            }
        }
    }

    stderr.finish().await?;
    Ok(())
}

/// wopQueryPathFromHashPart (29): Resolve a store path from its hash part.
///
/// Uses QueryPathInfo with a constructed path. Since the gRPC store doesn't
/// have a dedicated hash-part lookup, we query FindMissingPaths as a
/// workaround. The real implementation would need a dedicated RPC.
#[instrument(skip_all)]
async fn handle_query_path_from_hash_part<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let hash_part = wire::read_string(reader).await?;
    debug!(hash_part = %hash_part, "wopQueryPathFromHashPart");

    // Construct a query path from the hash part. The store service should
    // support this via QueryPathInfo with a hash-part prefix query.
    // For now, use the hash_part directly as a store path prefix lookup.
    let result = grpc_query_path_info(store_client, &format!("/nix/store/{hash_part}")).await;

    let path_str = match result {
        Ok(Some(info)) if !info.store_path.is_empty() => info.store_path,
        Ok(_) => String::new(),
        Err(e) => return send_store_error(stderr, e).await,
    };

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), &path_str).await?;
    Ok(())
}

/// wopAddSignatures (37): Add signatures to an existing store path.
///
/// Since the gRPC store doesn't have a dedicated AddSignatures RPC,
/// this is a no-op that reads the wire data and returns success.
#[instrument(skip_all)]
async fn handle_add_signatures<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    _store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    let sigs = wire::read_strings(reader).await?;
    debug!(path = %path_str, count = sigs.len(), "wopAddSignatures");

    // Signatures are deferred to a later phase. Accept and discard.
    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopRegisterDrvOutput (42): Stub for CA derivation output registration.
#[instrument(skip_all)]
async fn handle_register_drv_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let _realisation_json = wire::read_string(reader).await?;
    debug!("wopRegisterDrvOutput (stubbed, accepting)");
    stderr.finish().await?;
    Ok(())
}

/// wopQueryRealisation (43): Stub returning empty set.
#[instrument(skip_all)]
async fn handle_query_realisation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let _output_id = wire::read_string(reader).await?;
    debug!("wopQueryRealisation (stubbed, returning empty)");
    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 0).await?;
    Ok(())
}

/// wopQueryMissing (40): Report what needs building.
#[instrument(skip_all)]
async fn handle_query_missing<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    debug!(count = raw_paths.len(), "wopQueryMissing");

    let derived: Vec<(String, DerivedPath)> = raw_paths
        .into_iter()
        .filter_map(|s| match DerivedPath::parse(&s) {
            Ok(dp) => Some((s, dp)),
            Err(e) => {
                warn!(path = %s, error = %e, "dropping unparseable DerivedPath in wopQueryMissing");
                None
            }
        })
        .collect();

    // Collect store paths for batch lookup
    let store_paths: Vec<String> = derived
        .iter()
        .map(|(_, dp)| dp.store_path().to_string())
        .collect();

    let req = types::FindMissingPathsRequest {
        store_paths: store_paths.clone(),
    };
    let missing_set: HashSet<String> = match store_client.find_missing_paths(req).await {
        Ok(r) => r.into_inner().missing_paths.into_iter().collect(),
        Err(e) => {
            return send_store_error(stderr, anyhow::anyhow!("gRPC FindMissingPaths: {e}")).await;
        }
    };

    let mut will_build = Vec::new();
    let mut unknown = Vec::new();

    for (raw, dp) in &derived {
        let sp_str = dp.store_path().to_string();
        if !missing_set.contains(&sp_str) {
            continue;
        }
        match dp {
            DerivedPath::Built { drv, .. } => {
                // For Built paths, walk the derivation to find outputs that need building.
                // For simplicity in phase 2a, we report the raw DerivedPath string.
                let _ = resolve_derivation(drv, store_client, drv_cache).await;
                will_build.push(raw.clone());
            }
            DerivedPath::Opaque(_) => unknown.push(raw.clone()),
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_strings(w, &will_build).await?;
    wire::write_strings(w, &[]).await?; // willSubstitute: always empty
    wire::write_strings(w, &unknown).await?;
    wire::write_u64(w, 0).await?; // downloadSize
    wire::write_u64(w, 0).await?; // narSize
    Ok(())
}

/// wopAddToStoreNar (39): Receive a store path with NAR content via framed stream.
#[instrument(skip_all)]
async fn handle_add_to_store_nar<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    let deriver_str = wire::read_string(reader).await?;
    let nar_hash_str = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;
    let registration_time = wire::read_u64(reader).await?;
    let nar_size = wire::read_u64(reader).await?;
    let ultimate = wire::read_bool(reader).await?;
    let sigs = wire::read_strings(reader).await?;
    let ca_str = wire::read_string(reader).await?;
    let _repair = wire::read_bool(reader).await?;
    let _dont_check_sigs = wire::read_bool(reader).await?;

    debug!(path = %path_str, nar_size = nar_size, "wopAddToStoreNar");

    if nar_size > wire::MAX_FRAMED_TOTAL {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("nar_size {nar_size} exceeds maximum for {path_str}"),
            ))
            .await?;
        return Err(anyhow::anyhow!("nar_size exceeds maximum"));
    }

    // Validate store path
    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid store path '{path_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid store path: {e}"));
        }
    };

    // Validate narHash hex
    let nar_hash_bytes = match hex::decode(&nar_hash_str) {
        Ok(b) => b,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid narHash hex '{nar_hash_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid narHash hex: {e}"));
        }
    };

    // Read NAR data via framed stream
    let nar_data = match wire::read_framed_stream(reader).await {
        Ok(data) => data,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to read framed NAR for '{path_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("failed to read framed NAR: {e}"));
        }
    };

    // Upload to store via gRPC
    let info = make_proto_path_info(
        &path_str,
        &deriver_str,
        &nar_hash_bytes,
        nar_size,
        &references,
        registration_time,
        ultimate,
        &sigs,
        &ca_str,
    );

    // Cache .drv before uploading (we have the NAR data buffered)
    try_cache_drv(&path, &nar_data, drv_cache);

    if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    Ok(())
}

/// Parse a single entry from the wopAddMultipleToStore reassembled byte stream.
async fn parse_add_multiple_entry(
    cursor: &mut std::io::Cursor<&[u8]>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(cursor).await?;
    let deriver_str = wire::read_string(cursor).await?;
    let nar_hash_str = wire::read_string(cursor).await?;
    let references = wire::read_strings(cursor).await?;
    let registration_time = wire::read_u64(cursor).await?;
    let nar_size = wire::read_u64(cursor).await?;
    let ultimate = wire::read_bool(cursor).await?;
    let sigs = wire::read_strings(cursor).await?;
    let ca_str = wire::read_string(cursor).await?;

    debug!(path = %path_str, nar_size = nar_size, "wopAddMultipleToStore entry");

    let path = StorePath::parse(&path_str)
        .map_err(|e| anyhow::anyhow!("invalid store path '{path_str}': {e}"))?;

    let nar_hash_bytes = hex::decode(&nar_hash_str)
        .map_err(|e| anyhow::anyhow!("entry '{path_str}': invalid narHash hex: {e}"))?;

    // Read inner NAR data via framed stream
    let nar_data = wire::read_framed_stream(cursor)
        .await
        .map_err(|e| anyhow::anyhow!("entry '{path_str}': failed to read inner NAR: {e}"))?;

    // Cache .drv before uploading
    try_cache_drv(&path, &nar_data, drv_cache);

    let info = make_proto_path_info(
        &path_str,
        &deriver_str,
        &nar_hash_bytes,
        nar_size,
        &references,
        registration_time,
        ultimate,
        &sigs,
        &ca_str,
    );

    grpc_put_path(store_client, info, nar_data)
        .await
        .map_err(|e| anyhow::anyhow!("entry '{path_str}': store error: {e}"))?;

    Ok(())
}

/// wopAddToStore (7): Legacy content-addressed store path import.
#[instrument(skip_all)]
async fn handle_add_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let name = wire::read_string(reader).await?;
    let cam_str = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;
    let _repair = wire::read_bool(reader).await?;

    debug!(name = %name, cam_str = %cam_str, "wopAddToStore");

    let dump_data = match wire::read_framed_stream(reader).await {
        Ok(data) => data,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to read dump data for '{name}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("failed to read dump data: {e}"));
        }
    };

    let (is_text, is_recursive, hash_algo) = match parse_cam_str(&cam_str) {
        Ok(v) => v,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid content-address method '{cam_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid content-address method: {e}"));
        }
    };

    let content_hash = NixHash::compute(hash_algo, &dump_data);

    let mut ref_paths = Vec::with_capacity(references.len());
    for s in &references {
        match StorePath::parse(s) {
            Ok(p) => ref_paths.push(p),
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("invalid reference path '{s}' for wopAddToStore: {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("invalid reference path '{s}': {e}"));
            }
        }
    }

    let path = if is_text {
        match StorePath::make_text(&name, &content_hash, &ref_paths) {
            Ok(p) => p,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("failed to compute text store path for '{name}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("failed to compute text store path: {e}"));
            }
        }
    } else {
        match StorePath::make_fixed_output(&name, &content_hash, is_recursive) {
            Ok(p) => p,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("failed to compute fixed-output store path for '{name}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!(
                    "failed to compute fixed-output store path: {e}"
                ));
            }
        }
    };

    let nar_data = if is_recursive {
        dump_data
    } else {
        let node = NarNode::Regular {
            executable: false,
            contents: dump_data,
        };
        let mut buf = Vec::new();
        if let Err(e) = nar::serialize(&mut buf, &node) {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to serialize NAR for '{name}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("failed to serialize NAR: {e}"));
        }
        buf
    };

    let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar_data);
    let nar_size = nar_data.len() as u64;

    let ca = {
        let r_prefix = if is_recursive { "r:" } else { "" };
        let method = if is_text { "text" } else { "fixed" };
        let nix32_hash = rio_nix::store_path::nixbase32::encode(content_hash.digest());
        if is_text {
            format!("{method}:{hash_algo}:{nix32_hash}")
        } else {
            format!("{method}:{r_prefix}{hash_algo}:{nix32_hash}")
        }
    };

    try_cache_drv(&path, &nar_data, drv_cache);

    let info = make_proto_path_info(
        &path.to_string(),
        "",
        nar_hash.digest(),
        nar_size,
        &references,
        0,
        true,
        &[],
        &ca,
    );

    if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
        return send_store_error(stderr, e).await;
    }

    // Send STDERR_LAST + ValidPathInfo
    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_string(w, &path.to_string()).await?;
    wire::write_string(w, "").await?;
    wire::write_string(w, &nar_hash.to_hex()).await?;
    wire::write_strings(w, &references).await?;
    wire::write_u64(w, 0).await?;
    wire::write_u64(w, nar_size).await?;
    wire::write_bool(w, true).await?;
    wire::write_strings(w, &[]).await?;
    wire::write_string(w, &ca).await?;

    Ok(())
}

/// wopAddTextToStore (8): Legacy text file import.
#[instrument(skip_all)]
async fn handle_add_text_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let name = wire::read_string(reader).await?;
    let text = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;

    debug!(name = %name, text_len = text.len(), "wopAddTextToStore");

    let content_hash = NixHash::compute(HashAlgo::SHA256, text.as_bytes());

    let mut ref_paths = Vec::with_capacity(references.len());
    for s in &references {
        match StorePath::parse(s) {
            Ok(p) => ref_paths.push(p),
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("invalid reference path '{s}' for wopAddTextToStore: {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("invalid reference path '{s}': {e}"));
            }
        }
    }

    let path = match StorePath::make_text(&name, &content_hash, &ref_paths) {
        Ok(p) => p,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to compute text store path for '{name}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("failed to compute text store path: {e}"));
        }
    };

    let node = NarNode::Regular {
        executable: false,
        contents: text.into_bytes(),
    };
    let mut nar_data = Vec::new();
    if let Err(e) = nar::serialize(&mut nar_data, &node) {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("failed to serialize NAR for '{name}': {e}"),
            ))
            .await?;
        return Err(anyhow::anyhow!("failed to serialize NAR: {e}"));
    }

    let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar_data);
    let nar_size = nar_data.len() as u64;

    let ca = format!(
        "text:sha256:{}",
        rio_nix::store_path::nixbase32::encode(content_hash.digest())
    );

    try_cache_drv(&path, &nar_data, drv_cache);

    let info = make_proto_path_info(
        &path.to_string(),
        "",
        nar_hash.digest(),
        nar_size,
        &references,
        0,
        true,
        &[],
        &ca,
    );

    if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), &path.to_string()).await?;

    Ok(())
}

/// Parse a content-address method string.
fn parse_cam_str(cam_str: &str) -> Result<(bool, bool, HashAlgo), String> {
    if let Some(algo_str) = cam_str.strip_prefix("text:") {
        let algo = algo_str.parse::<HashAlgo>().map_err(|e| e.to_string())?;
        Ok((true, false, algo))
    } else if let Some(rest) = cam_str.strip_prefix("fixed:") {
        if let Some(algo_str) = rest.strip_prefix("r:") {
            let algo = algo_str.parse::<HashAlgo>().map_err(|e| e.to_string())?;
            Ok((false, true, algo))
        } else if let Some(algo_str) = rest.strip_prefix("git:") {
            let algo = algo_str.parse::<HashAlgo>().map_err(|e| e.to_string())?;
            Ok((false, true, algo))
        } else {
            let algo = rest.parse::<HashAlgo>().map_err(|e| e.to_string())?;
            Ok((false, false, algo))
        }
    } else {
        Err(format!("unrecognized content-address method: {cam_str}"))
    }
}

/// wopAddMultipleToStore (44): Receive multiple store paths via framed stream.
#[instrument(skip_all)]
async fn handle_add_multiple_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let _repair = wire::read_bool(reader).await?;
    let _dont_check_sigs = wire::read_bool(reader).await?;

    debug!("wopAddMultipleToStore");

    let stream_data = match wire::read_framed_stream(reader).await {
        Ok(data) => data,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("wopAddMultipleToStore: failed to read framed stream: {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("failed to read framed stream: {e}"));
        }
    };

    let mut cursor = std::io::Cursor::new(stream_data.as_slice());
    let total_len = stream_data.len() as u64;

    while cursor.position() < total_len {
        if let Err(e) = parse_add_multiple_entry(&mut cursor, store_client, drv_cache).await {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("wopAddMultipleToStore entry failed: {e}"),
                ))
                .await?;
            return Err(e);
        }
    }

    stderr.finish().await?;
    Ok(())
}

/// wopQueryDerivationOutputMap (41): Return output name -> path mappings.
#[instrument(skip_all)]
async fn handle_query_derivation_output_map<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let drv_path_str = wire::read_string(reader).await?;
    debug!(path = %drv_path_str, "wopQueryDerivationOutputMap");

    let drv_path = match StorePath::parse(&drv_path_str) {
        Ok(p) => p,
        Err(e) => {
            warn!(path = %drv_path_str, error = %e, "invalid store path");
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid store path '{drv_path_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid store path: {e}"));
        }
    };

    let drv = match resolve_derivation(&drv_path, store_client, drv_cache).await {
        Ok(d) => d,
        Err(e) => return send_store_error(stderr, e).await,
    };

    let outputs = drv.outputs();

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_u64(w, outputs.len() as u64).await?;
    for output in outputs {
        wire::write_string(w, output.name()).await?;
        wire::write_string(w, output.path()).await?;
    }

    Ok(())
}

/// wopBuildDerivation (36): Build a derivation via scheduler.
///
/// Receives an inline BasicDerivation (no inputDrvs). Recovers the full
/// Derivation from drv_cache to reconstruct the DAG.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
async fn handle_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    options: &Option<ClientOptions>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
    has_seen_build_paths_with_results: &bool,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<()> {
    let drv_path_str = wire::read_string(reader).await?;
    let basic_drv = match read_basic_derivation(reader).await {
        Ok(v) => v,
        Err(e) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!("wopBuildDerivation: failed to read BasicDerivation: {e}"),
            )
            .await;
        }
    };
    let build_mode_val = match wire::read_u64(reader).await {
        Ok(v) => v,
        Err(e) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!("wopBuildDerivation: failed to read build mode: {e}"),
            )
            .await;
        }
    };
    let _build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!("wopBuildDerivation: unsupported build mode {build_mode_val}"),
            )
            .await;
        }
    };

    debug!(
        path = %drv_path_str,
        platform = %basic_drv.platform(),
        builder = %basic_drv.builder(),
        "wopBuildDerivation"
    );

    // IFD detection: if we haven't seen wopBuildPathsWithResults on this session,
    // this is likely an IFD or build-hook request
    let is_ifd_hint = !has_seen_build_paths_with_results;

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let drv_path = match StorePath::parse(&drv_path_str) {
        Ok(p) => p,
        Err(e) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!("invalid drv path '{drv_path_str}': {e}"),
            )
            .await;
        }
    };

    // Try to get the full derivation with inputDrvs
    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    // Reconstruct the DAG
    let (nodes, edges) = match &full_drv {
        Ok(drv) => {
            match translate::reconstruct_dag(&drv_path, drv, store_client, drv_cache).await {
                Ok((n, e)) => (n, e),
                Err(dag_err) => {
                    warn!(error = %dag_err, "DAG reconstruction failed, using single-node DAG");
                    (
                        translate::single_node_from_basic(&drv_path_str, &basic_drv),
                        Vec::new(),
                    )
                }
            }
        }
        Err(e) => {
            debug!(error = %e, "full derivation not available, using single-node DAG");
            (
                translate::single_node_from_basic(&drv_path_str, &basic_drv),
                Vec::new(),
            )
        }
    };

    let priority_class = if is_ifd_hint {
        "interactive".to_string()
    } else {
        "ci".to_string()
    };

    let request = translate::build_submit_request(nodes, edges, options, &priority_class);

    let build_result =
        match submit_and_process_build(stderr, scheduler_client, request, active_build_ids).await {
            Ok(r) => r,
            Err(e) => {
                warn!(error = %e, "build submission failed");
                BuildResult::failure(BuildStatus::MiscFailure, format!("scheduler error: {e}"))
            }
        };

    debug!(
        status = ?build_result.status(),
        error_msg = %build_result.error_msg(),
        "wopBuildDerivation result"
    );

    stderr.finish().await?;
    write_build_result(stderr.inner_mut(), &build_result).await?;
    Ok(())
}

/// wopBuildPaths (9): Build a set of derivations.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
async fn handle_build_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    options: &Option<ClientOptions>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let _build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!("wopBuildPaths: unsupported build mode {build_mode_val}"),
            )
            .await;
        }
    };

    debug!(count = raw_paths.len(), "wopBuildPaths");

    // Collect all derivation paths and reconstruct a combined DAG
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();

    for raw in &raw_paths {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("invalid DerivedPath '{raw}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("invalid DerivedPath: {e}"));
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                match grpc_is_valid_path(store_client, path).await {
                    Ok(true) => { /* exists, fine */ }
                    Ok(false) => {
                        stderr
                            .error(&StderrError::simple(
                                PROGRAM_NAME,
                                format!("path '{}' is not valid and cannot be built", path),
                            ))
                            .await?;
                        return Err(anyhow::anyhow!("invalid opaque path: {}", path));
                    }
                    Err(e) => return send_store_error(stderr, e).await,
                }
            }
            DerivedPath::Built { drv, .. } => {
                let drv_obj = match resolve_derivation(drv, store_client, drv_cache).await {
                    Ok(d) => d,
                    Err(e) => return send_store_error(stderr, e).await,
                };

                match translate::reconstruct_dag(drv, &drv_obj, store_client, drv_cache).await {
                    Ok((nodes, edges)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                    }
                    Err(e) => {
                        return send_store_error(
                            stderr,
                            anyhow::anyhow!("DAG reconstruction failed for '{}': {e}", drv),
                        )
                        .await;
                    }
                }
            }
        }
    }

    if all_nodes.is_empty() {
        // All paths were opaque and valid -- nothing to build
        stderr.finish().await?;
        wire::write_u64(stderr.inner_mut(), 1).await?;
        return Ok(());
    }

    // Deduplicate nodes by drv_path
    let mut seen: HashSet<String> = HashSet::new();
    all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

    let request = translate::build_submit_request(all_nodes, all_edges, options, "ci");

    let build_result =
        match submit_and_process_build(stderr, scheduler_client, request, active_build_ids).await {
            Ok(r) => r,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("build failed: {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("build failed: {e}"));
            }
        };

    if !build_result.status().is_success() {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("build failed: {}", build_result.error_msg()),
            ))
            .await?;
        return Err(anyhow::anyhow!(
            "build failed: {}",
            build_result.error_msg()
        ));
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopBuildPathsWithResults (46): Build paths and return per-path BuildResult.
#[instrument(skip_all)]
#[allow(clippy::too_many_arguments)]
async fn handle_build_paths_with_results<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    options: &Option<ClientOptions>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
    active_build_ids: &mut Vec<(String, u64)>,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let _build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            return send_store_error(
                stderr,
                anyhow::anyhow!(
                    "wopBuildPathsWithResults: unsupported build mode {build_mode_val}"
                ),
            )
            .await;
        }
    };

    debug!(count = raw_paths.len(), "wopBuildPathsWithResults");

    let mut results = Vec::new();

    // Collect all derivation paths to build together
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut drv_indices: Vec<Option<usize>> = Vec::new(); // maps raw_paths index to build result
    let mut opaque_results: HashMap<usize, BuildResult> = HashMap::new();

    for (idx, raw) in raw_paths.iter().enumerate() {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => {
                opaque_results.insert(
                    idx,
                    BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!("invalid path '{raw}': {e}"),
                    ),
                );
                drv_indices.push(None);
                continue;
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                let result = match grpc_is_valid_path(store_client, path).await {
                    Ok(true) => BuildResult::new(
                        BuildStatus::AlreadyValid,
                        String::new(),
                        0,
                        false,
                        0,
                        0,
                        None,
                        None,
                        Vec::new(),
                    ),
                    Ok(false) => BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!("path '{}' not valid", path),
                    ),
                    Err(e) => {
                        BuildResult::failure(BuildStatus::MiscFailure, format!("store error: {e}"))
                    }
                };
                opaque_results.insert(idx, result);
                drv_indices.push(None);
            }
            DerivedPath::Built { drv, .. } => {
                let drv_obj = match resolve_derivation(drv, store_client, drv_cache).await {
                    Ok(d) => d,
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, format!("{e}")),
                        );
                        drv_indices.push(None);
                        continue;
                    }
                };

                match translate::reconstruct_dag(drv, &drv_obj, store_client, drv_cache).await {
                    Ok((nodes, edges)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        drv_indices.push(Some(idx));
                    }
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, format!("{e}")),
                        );
                        drv_indices.push(None);
                    }
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        // Deduplicate nodes
        let mut seen: HashSet<String> = HashSet::new();
        all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

        let request = translate::build_submit_request(all_nodes, all_edges, options, "ci");

        let build_result =
            submit_and_process_build(stderr, scheduler_client, request, active_build_ids)
                .await
                .unwrap_or_else(|e| {
                    BuildResult::failure(BuildStatus::MiscFailure, format!("scheduler error: {e}"))
                });

        // Apply the build result to all derivation paths
        for (idx, raw) in raw_paths.iter().enumerate() {
            if opaque_results.contains_key(&idx) {
                results.push(opaque_results.remove(&idx).unwrap());
            } else {
                let _ = raw; // suppress unused warning
                results.push(build_result.clone());
            }
        }
    } else {
        // All paths were opaque
        for idx in 0..raw_paths.len() {
            results.push(opaque_results.remove(&idx).unwrap_or_else(|| {
                BuildResult::failure(BuildStatus::MiscFailure, "unknown path".to_string())
            }));
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_u64(w, results.len() as u64).await?;
    for (raw, result) in raw_paths.iter().zip(results.iter()) {
        wire::write_string(w, raw).await?;
        write_build_result(w, result).await?;
    }

    Ok(())
}
