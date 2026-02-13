//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation, and writes the response via the STDERR streaming loop.

use std::collections::HashSet;

use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, warn};

use crate::store::Store;

/// Client build options received via wopSetOptions.
///
/// Fields are populated during Phase 1a and propagated to the scheduler
/// in Phase 2a via gRPC `SubmitBuildRequest`.
#[derive(Debug, Clone)]
#[allow(dead_code)]
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

/// Dispatch an opcode to the appropriate handler.
///
/// Returns `Ok(())` on success. On protocol errors, sends STDERR_ERROR
/// and returns `Ok(())` (the connection stays open). Returns `Err` only
/// for I/O errors that prevent further communication.
pub async fn handle_opcode<R, W>(
    opcode: u64,
    reader: &mut R,
    writer: &mut W,
    store: &dyn Store,
    options: &mut Option<ClientOptions>,
    temp_roots: &mut HashSet<String>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut stderr = StderrWriter::new(writer);
    let start = std::time::Instant::now();

    let op_name = WorkerOp::from_u64(opcode)
        .map(|op| op.name())
        .unwrap_or("unknown");
    metrics::counter!("rio_opcodes_total", "opcode" => op_name).increment(1);

    let result = match WorkerOp::from_u64(opcode) {
        Some(WorkerOp::IsValidPath) => handle_is_valid_path(reader, &mut stderr, store).await,
        Some(WorkerOp::QueryPathInfo) => handle_query_path_info(reader, &mut stderr, store).await,
        Some(WorkerOp::QueryValidPaths) => {
            handle_query_valid_paths(reader, &mut stderr, store).await
        }
        Some(WorkerOp::AddTempRoot) => handle_add_temp_root(reader, &mut stderr, temp_roots).await,
        Some(WorkerOp::SetOptions) => handle_set_options(reader, &mut stderr, options).await,
        Some(WorkerOp::NarFromPath) => handle_nar_from_path(reader, &mut stderr, store).await,
        Some(WorkerOp::QueryPathFromHashPart) => {
            handle_query_path_from_hash_part(reader, &mut stderr).await
        }
        Some(WorkerOp::AddSignatures) => handle_add_signatures(reader, &mut stderr).await,
        Some(WorkerOp::QueryMissing) => handle_query_missing(reader, &mut stderr).await,
        Some(op) => {
            warn!(
                opcode = opcode,
                name = op.name(),
                "unimplemented opcode, closing connection"
            );
            stderr
                .error(&StderrError::simple(format!(
                    "operation {} ({}) is not yet implemented",
                    op.name(),
                    opcode
                )))
                .await?;
            // Must close the connection: the opcode's payload is still in the
            // stream and we don't know its format, so we can't drain it.
            // The client will reconnect automatically.
            Err(anyhow::anyhow!(
                "unimplemented opcode {} ({}), closing connection to avoid stream desynchronization",
                op.name(),
                opcode
            ))
        }
        None => {
            warn!(opcode = opcode, "unknown opcode, closing connection");
            stderr
                .error(&StderrError::simple(format!(
                    "unsupported operation {opcode}"
                )))
                .await?;
            Err(anyhow::anyhow!(
                "unknown opcode {opcode}, closing connection to avoid stream desynchronization"
            ))
        }
    };

    let elapsed = start.elapsed();
    metrics::histogram!("rio_opcode_duration_seconds", "opcode" => op_name)
        .record(elapsed.as_secs_f64());

    if result.is_err() {
        metrics::counter!("rio_errors_total", "type" => "protocol").increment(1);
    }

    result
}

/// wopIsValidPath (1): Check if a store path exists.
async fn handle_is_valid_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopIsValidPath");

    let valid = match StorePath::parse(&path_str) {
        Ok(path) => store.is_valid_path(&path).await?,
        Err(_) => false,
    };

    stderr.finish().await?;
    wire::write_bool(stderr.inner_mut(), valid).await?;
    Ok(())
}

/// wopQueryPathInfo (26): Return full path metadata.
async fn handle_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopQueryPathInfo");

    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(_) => {
            stderr.finish().await?;
            wire::write_bool(stderr.inner_mut(), false).await?;
            return Ok(());
        }
    };

    let info = store.query_path_info(&path).await?;

    stderr.finish().await?;
    let w = stderr.inner_mut();

    match info {
        None => {
            wire::write_bool(w, false).await?;
        }
        Some(info) => {
            wire::write_bool(w, true).await?;

            // deriver
            wire::write_string(
                w,
                &info
                    .deriver
                    .as_ref()
                    .map_or(String::new(), |d| d.to_string()),
            )
            .await?;
            // narHash
            wire::write_string(w, &info.nar_hash.to_colon()).await?;
            // references
            let refs: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
            wire::write_strings(w, &refs).await?;
            // registrationTime
            wire::write_u64(w, info.registration_time).await?;
            // narSize
            wire::write_u64(w, info.nar_size).await?;
            // ultimate
            wire::write_bool(w, info.ultimate).await?;
            // sigs
            wire::write_strings(w, &info.sigs).await?;
            // ca
            wire::write_string(w, info.ca.as_deref().unwrap_or("")).await?;
        }
    }

    Ok(())
}

/// wopQueryValidPaths (31): Batch validity check.
async fn handle_query_valid_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_strs = wire::read_strings(reader).await?;
    let _substitute = wire::read_bool(reader).await?; // read and ignore

    debug!(count = path_strs.len(), "wopQueryValidPaths");

    let paths: Vec<StorePath> = path_strs
        .iter()
        .filter_map(|s| StorePath::parse(s).ok())
        .collect();

    let valid = store.query_valid_paths(&paths).await?;
    let valid_strs: Vec<String> = valid.iter().map(|p| p.to_string()).collect();

    stderr.finish().await?;
    wire::write_strings(stderr.inner_mut(), &valid_strs).await?;
    Ok(())
}

/// wopAddTempRoot (11): Register a temporary GC root.
async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    temp_roots: &mut HashSet<String>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopAddTempRoot");

    temp_roots.insert(path_str);

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopSetOptions (19): Accept client build configuration.
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

    // Protocol >= 1.12: override pairs
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
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopNarFromPath (38): Export path as NAR via STDERR_WRITE chunks.
async fn handle_nar_from_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopNarFromPath");

    let path = StorePath::parse(&path_str)?;
    let nar = store.nar_from_path(&path).await?;

    match nar {
        Some(nar_data) => {
            // Send NAR data via STDERR_WRITE chunks
            // Chunk size: 64KB to avoid huge single messages
            const CHUNK_SIZE: usize = 64 * 1024;
            for chunk in nar_data.chunks(CHUNK_SIZE) {
                stderr.write_data(chunk).await?;
            }
            stderr.finish().await?;
        }
        None => {
            stderr
                .error(&StderrError::simple(format!(
                    "path '{}' is not valid",
                    path_str
                )))
                .await?;
        }
    }

    Ok(())
}

/// wopQueryPathFromHashPart (29): Stubbed — returns empty string (no match).
async fn handle_query_path_from_hash_part<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let _hash_part = wire::read_string(reader).await?;
    debug!("wopQueryPathFromHashPart (stubbed, returning empty)");

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), "").await?;
    Ok(())
}

/// wopAddSignatures (37): Stubbed — accepts and discards signatures.
async fn handle_add_signatures<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let _path = wire::read_string(reader).await?;
    let _sigs = wire::read_strings(reader).await?;
    debug!("wopAddSignatures (stubbed, accepting)");

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopQueryMissing (40): Report what needs building.
///
/// For Phase 1a, returns everything as willBuild (nothing can be substituted,
/// nothing is unknown since we know about all paths in our store).
async fn handle_query_missing<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let paths = wire::read_strings(reader).await?;
    debug!(count = paths.len(), "wopQueryMissing");

    stderr.finish().await?;
    let w = stderr.inner_mut();

    // willBuild: return all requested paths (conservative: assume nothing is cached)
    wire::write_strings(w, &paths).await?;
    // willSubstitute: always empty (rio-build doesn't use external substituters)
    let empty: Vec<String> = vec![];
    wire::write_strings(w, &empty).await?;
    // unknown: empty
    wire::write_strings(w, &empty).await?;
    // downloadSize: 0
    wire::write_u64(w, 0).await?;
    // narSize: 0
    wire::write_u64(w, 0).await?;
    Ok(())
}
