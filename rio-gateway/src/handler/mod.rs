//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation via gRPC delegation, and writes the response via the STDERR
//! streaming loop.
//!
//! Store operations delegate to rio-store via `StoreServiceClient` gRPC calls;
//! build operations delegate to rio-scheduler via `SchedulerServiceClient`.

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
use rio_proto::SchedulerServiceClient;
use rio_proto::StoreServiceClient;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{debug, error, instrument, warn};

use rio_common::grpc::{DEFAULT_GRPC_TIMEOUT, GRPC_STREAM_TIMEOUT};
use rio_common::limits::MAX_NAR_SIZE;

use crate::translate;

const PROGRAM_NAME: &str = "rio-gateway";

/// Send a formatted error via STDERR_ERROR and return Err.
///
/// Expands to: send the message to the client, then `return Err(anyhow!(msg))`.
/// The same message is used for both the client-visible error and the internal
/// anyhow error. Use inside `match ... { Err(e) => stderr_err!(stderr, "... {e}") }`.
macro_rules! stderr_err {
    ($stderr:expr, $($arg:tt)*) => {{
        let __msg = format!($($arg)*);
        $stderr
            .error(&::rio_nix::protocol::stderr::StderrError::simple(
                PROGRAM_NAME,
                __msg.clone(),
            ))
            .await?;
        return Err(::anyhow::anyhow!(__msg));
    }};
}

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

/// Per-session mutable state, threaded through all opcode handlers.
///
/// Holds the gRPC clients and protocol-session-scoped state (options,
/// temp roots, drv cache, build tracking). Constructed once per session
/// in [`crate::session::run_protocol`].
pub struct SessionContext {
    pub store_client: StoreServiceClient<Channel>,
    pub scheduler_client: SchedulerServiceClient<Channel>,
    pub options: Option<ClientOptions>,
    pub temp_roots: HashSet<StorePath>,
    pub drv_cache: HashMap<StorePath, Derivation>,
    /// IFD detection: wopBuildDerivation without prior wopBuildPathsWithResults
    /// is likely an IFD or build-hook request.
    pub has_seen_build_paths_with_results: bool,
    /// Active build IDs for scheduler failover: build_id → last_sequence.
    pub active_build_ids: HashMap<String, u64>,
}

impl SessionContext {
    pub fn new(
        store_client: StoreServiceClient<Channel>,
        scheduler_client: SchedulerServiceClient<Channel>,
    ) -> Self {
        Self {
            store_client,
            scheduler_client,
            options: None,
            temp_roots: HashSet::new(),
            drv_cache: HashMap::new(),
            has_seen_build_paths_with_results: false,
            active_build_ids: HashMap::new(),
        }
    }
}

// r[impl gw.opcode.mandatory-set]
// r[impl gw.compat.unknown-opcode-close]
/// Dispatch an opcode to the appropriate handler.
///
/// Returns `Ok(())` on success. On errors, sends `STDERR_ERROR` to the
/// client and returns `Err`, which terminates the session.
#[instrument(skip_all, fields(opcode))]
pub async fn handle_opcode<R, W>(
    opcode: u64,
    reader: &mut R,
    writer: &mut W,
    ctx: &mut SessionContext,
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
            handle_is_valid_path(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::AddToStore) => {
            handle_add_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddTextToStore) => {
            handle_add_text_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::EnsurePath) => {
            handle_ensure_path(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::QueryPathInfo) => {
            handle_query_path_info(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::QueryValidPaths) => {
            handle_query_valid_paths(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::AddTempRoot) => {
            handle_add_temp_root(reader, &mut stderr, &mut ctx.temp_roots).await
        }
        Some(WorkerOp::SetOptions) => {
            handle_set_options(reader, &mut stderr, &mut ctx.options).await
        }
        Some(WorkerOp::NarFromPath) => {
            handle_nar_from_path(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::QueryPathFromHashPart) => {
            handle_query_path_from_hash_part(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::AddSignatures) => {
            handle_add_signatures(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::QueryMissing) => {
            handle_query_missing(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddToStoreNar) => {
            handle_add_to_store_nar(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::AddMultipleToStore) => {
            handle_add_multiple_to_store(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(
                reader,
                &mut stderr,
                &mut ctx.store_client,
                &mut ctx.drv_cache,
            )
            .await
        }
        Some(WorkerOp::BuildDerivation) => handle_build_derivation(reader, &mut stderr, ctx).await,
        Some(WorkerOp::BuildPaths) => handle_build_paths(reader, &mut stderr, ctx).await,
        Some(WorkerOp::BuildPathsWithResults) => {
            ctx.has_seen_build_paths_with_results = true;
            handle_build_paths_with_results(reader, &mut stderr, ctx).await
        }
        Some(WorkerOp::RegisterDrvOutput) => {
            handle_register_drv_output(reader, &mut stderr, &mut ctx.store_client).await
        }
        Some(WorkerOp::QueryRealisation) => {
            handle_query_realisation(reader, &mut stderr, &mut ctx.store_client).await
        }
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

/// If `path` is a `.drv`, parse the ATerm from NAR data and cache it.
/// Round 4 Z24: cap drv_cache at MAX_TRANSITIVE_INPUTS. The cache
/// is session-scoped; a client uploading 100k .drv files would
/// consume ~100k * (avg drv size ~1KB parsed) = ~100MB per session.
/// MAX_TRANSITIVE_INPUTS (10k) matches the BFS limit in
/// translate::reconstruct_dag — a DAG bigger than that would be
/// rejected anyway, so caching more .drvs is wasted.
///
/// Returns true if inserted, false if cap hit. Caller decides what
/// to do (try_cache_drv logs + continues; resolve_derivation
/// returns an error to the client).
fn insert_drv_bounded(
    drv_cache: &mut HashMap<StorePath, Derivation>,
    path: StorePath,
    drv: Derivation,
) -> bool {
    if drv_cache.len() >= crate::translate::MAX_TRANSITIVE_INPUTS && !drv_cache.contains_key(&path)
    {
        return false;
    }
    drv_cache.insert(path, drv);
    true
}

fn try_cache_drv(
    path: &StorePath,
    nar_data: &[u8],
    drv_cache: &mut HashMap<StorePath, Derivation>,
) {
    if !path.is_derivation() {
        return;
    }
    match Derivation::parse_from_nar(nar_data) {
        Ok(drv) => {
            if insert_drv_bounded(drv_cache, path.clone(), drv) {
                debug!(path = %path, "cached parsed derivation");
            } else {
                // Log once-ish (every subsequent insert fails too).
                // The upload itself succeeds; only the cache skip.
                warn!(
                    path = %path,
                    cap = crate::translate::MAX_TRANSITIVE_INPUTS,
                    "drv_cache at cap; not caching (upload still proceeds)"
                );
            }
        }
        Err(e) => {
            warn!(path = %path, error = %e, "failed to parse .drv from NAR");
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

    let (_info, nar_data) = grpc_get_path(store_client, drv_path.as_str())
        .await?
        .ok_or_else(|| anyhow::anyhow!("derivation '{}' not found in store", drv_path))?;

    let drv = Derivation::parse_from_nar(&nar_data)
        .map_err(|e| anyhow::anyhow!("failed to parse .drv '{}': {e}", drv_path))?;

    // Round 4 Z24: bound drv_cache. resolve_derivation is called
    // from BFS in translate::reconstruct_dag — cap hit means the
    // DAG is too large (MAX_TRANSITIVE_INPUTS enforced by the BFS
    // itself, but the cache could grow beyond that across multiple
    // builds in one session). Error propagates as DAG failure.
    if !insert_drv_bounded(drv_cache, drv_path.clone(), drv.clone()) {
        return Err(anyhow::anyhow!(
            "per-session derivation cache full ({} entries, cap {})",
            drv_cache.len(),
            crate::translate::MAX_TRANSITIVE_INPUTS
        ));
    }
    Ok(drv)
}

// ---------------------------------------------------------------------------
// Submodules (defined AFTER stderr_err! macro so it is visible to them)
// ---------------------------------------------------------------------------

mod build;
mod grpc;
mod opcodes_read;
mod opcodes_write;

use build::*;
use grpc::*;
use opcodes_read::*;
use opcodes_write::*;
