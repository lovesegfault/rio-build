//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation, and writes the response via the STDERR streaming loop.

use std::collections::{HashMap, HashSet};

use rio_nix::derivation::Derivation;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarNode};
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::client;
use rio_nix::protocol::derived_path::DerivedPath;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, error, instrument, warn};

use crate::store::Store;

const PROGRAM_NAME: &str = "rio-build";

/// Default timeout for local daemon build operations (1 hour).
const DAEMON_BUILD_TIMEOUT: std::time::Duration = std::time::Duration::from_secs(3600);

/// Client build options received via wopSetOptions.
///
/// Fields are populated during Phase 1a and propagated to the scheduler
/// in Phase 2a via gRPC `SubmitBuildRequest`. All fields are private;
/// accessors will be added in Phase 2a when the scheduler consumes them.
#[derive(Debug, Clone)]
#[allow(dead_code)] // fields consumed in Phase 2a
pub struct ClientOptions {
    keep_failed: bool,
    keep_going: bool,
    try_fallback: bool,
    verbosity: u64,
    max_build_jobs: u64,
    max_silent_time: u64,
    verbose_build: bool,
    build_cores: u64,
    use_substitutes: bool,
    overrides: Vec<(String, String)>,
}

/// Dispatch an opcode to the appropriate handler.
///
/// Returns `Ok(())` on success. On protocol errors, sends STDERR_ERROR
/// and returns `Ok(())` (the connection stays open). Returns `Err` only
/// for I/O errors that prevent further communication.
#[instrument(skip_all, fields(opcode))]
pub async fn handle_opcode<R, W>(
    opcode: u64,
    reader: &mut R,
    writer: &mut W,
    store: &dyn Store,
    options: &mut Option<ClientOptions>,
    temp_roots: &mut HashSet<StorePath>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()>
where
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
{
    let mut stderr = StderrWriter::new(writer);
    let start = std::time::Instant::now();

    let op = WorkerOp::from_u64(opcode);
    let op_name = op.map(|o| o.name()).unwrap_or("unknown");
    tracing::Span::current().record("opcode", op_name);
    metrics::counter!("rio_gateway_opcodes_total", "opcode" => op_name).increment(1);

    let result = match op {
        Some(WorkerOp::IsValidPath) => handle_is_valid_path(reader, &mut stderr, store).await,
        Some(WorkerOp::AddToStore) => {
            handle_add_to_store(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::AddTextToStore) => {
            handle_add_text_to_store(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::EnsurePath) => handle_ensure_path(reader, &mut stderr, store).await,
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
        Some(WorkerOp::QueryMissing) => handle_query_missing(reader, &mut stderr, store).await,
        Some(WorkerOp::AddToStoreNar) => {
            handle_add_to_store_nar(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::AddMultipleToStore) => {
            handle_add_multiple_to_store(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::BuildDerivation) => handle_build_derivation(reader, &mut stderr).await,
        Some(WorkerOp::BuildPaths) => {
            handle_build_paths(reader, &mut stderr, store, drv_cache).await
        }
        Some(WorkerOp::BuildPathsWithResults) => {
            handle_build_paths_with_results(reader, &mut stderr, store, drv_cache).await
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
    // Best-effort: if sending the error also fails, we still propagate the original.
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

/// wopIsValidPath (1): Check if a store path exists.
#[instrument(skip_all)]
async fn handle_is_valid_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopIsValidPath");

    let valid = match StorePath::parse(&path_str) {
        Ok(path) => match store.is_valid_path(&path).await {
            Ok(v) => v,
            Err(e) => return send_store_error(stderr, e).await,
        },
        Err(_) => false,
    };

    stderr.finish().await?;
    wire::write_bool(stderr.inner_mut(), valid).await?;
    Ok(())
}

/// wopEnsurePath (10): Ensure a store path is valid/available.
///
/// In the real nix-daemon, this may trigger substitution. For rio-build,
/// all paths are either already uploaded or unavailable, so this is
/// equivalent to checking validity and returning success unconditionally.
#[instrument(skip_all)]
async fn handle_ensure_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopEnsurePath");

    // Check if the path is valid — log if missing but don't error,
    // since the real daemon would attempt substitution.
    if let Ok(path) = StorePath::parse(&path_str) {
        match store.is_valid_path(&path).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(path = %path_str, "wopEnsurePath: path not in store (no substituters)");
            }
            Err(e) => {
                warn!(path = %path_str, error = %e, "wopEnsurePath: store error checking path");
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
    store: &dyn Store,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopQueryPathInfo");

    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => {
            warn!(path = %path_str, error = %e, "invalid store path in wopQueryPathInfo, returning not-found");
            stderr.finish().await?;
            wire::write_bool(stderr.inner_mut(), false).await?;
            return Ok(());
        }
    };

    let info = match store.query_path_info(&path).await {
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

            // deriver
            wire::write_string(w, &info.deriver().map_or(String::new(), |d| d.to_string())).await?;
            // narHash (nix-daemon sends raw hex digest, no algorithm prefix)
            wire::write_string(w, &info.nar_hash().to_hex()).await?;
            // references
            let refs: Vec<String> = info.references().iter().map(|r| r.to_string()).collect();
            wire::write_strings(w, &refs).await?;
            // registrationTime
            wire::write_u64(w, info.registration_time()).await?;
            // narSize
            wire::write_u64(w, info.nar_size()).await?;
            // ultimate
            wire::write_bool(w, info.ultimate()).await?;
            // sigs
            wire::write_strings(w, info.sigs()).await?;
            // ca
            wire::write_string(w, info.ca().unwrap_or("")).await?;
        }
    }

    Ok(())
}

/// wopQueryValidPaths (31): Batch validity check.
#[instrument(skip_all)]
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
        .filter_map(|s| match StorePath::parse(s) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(path = %s, error = %e, "dropping unparseable path in wopQueryValidPaths");
                None
            }
        })
        .collect();

    let valid = match store.query_valid_paths(&paths).await {
        Ok(v) => v,
        Err(e) => return send_store_error(stderr, e).await,
    };
    let valid_strs: Vec<String> = valid.iter().map(|p| p.to_string()).collect();

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

    // nix-daemon sends only STDERR_LAST for SetOptions — no result value.
    stderr.finish().await?;
    Ok(())
}

/// wopNarFromPath (38): Export path as NAR via STDERR_WRITE chunks.
///
/// Note: the canonical nix-daemon sends STDERR_LAST then streams raw NAR
/// bytes without framing. rio-build intentionally uses STDERR_WRITE chunks
/// instead, which the Nix client also understands. This simplifies gateway
/// streaming when NAR data is reassembled from distributed storage.
/// See `docs/src/components/gateway.md` for details.
#[instrument(skip_all)]
async fn handle_nar_from_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
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
    let nar = match store.nar_from_path(&path).await {
        Ok(nar) => nar,
        Err(e) => return send_store_error(stderr, e).await,
    };

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
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("path '{}' is not valid", path_str),
                ))
                .await?;
            return Err(anyhow::anyhow!("path '{}' has no NAR data", path_str));
        }
    }

    Ok(())
}

/// wopQueryPathFromHashPart (29): Stubbed — returns empty string (no match).
#[instrument(skip_all)]
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
#[instrument(skip_all)]
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

/// wopRegisterDrvOutput (42): Stubbed — accepts and discards CA derivation output registration.
///
/// Modern Nix clients may send this after a content-addressed build. Accepting
/// it as a no-op prevents unexpected connection drops. Full CA support is
/// planned for Phase 2c/5.
///
/// Wire format (protocol >= 1.31, which is always true since we target 1.37+):
/// - Client sends one string: a JSON-encoded `Realisation` object containing
///   `{"id":"sha256:<hex>!<outputName>","outPath":"/nix/store/...","signatures":[],"dependentRealisations":{}}`
/// - Daemon responds with only STDERR_LAST (no result value).
///
/// Ref: NixOS/nix src/libstore/daemon.cc (WorkerProto::Op::RegisterDrvOutput)
#[instrument(skip_all)]
async fn handle_register_drv_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    // Protocol >= 1.31: single JSON string containing the full Realisation
    let _realisation_json = wire::read_string(reader).await?;
    debug!("wopRegisterDrvOutput (stubbed, accepting)");

    // Nix daemon sends only STDERR_LAST, no result value
    stderr.finish().await?;
    Ok(())
}

/// wopQueryRealisation (43): Stubbed — returns empty set of realisations.
///
/// Modern Nix clients may query this for CA derivation outputs. Returning
/// an empty set is safe and prevents connection drops.
#[instrument(skip_all)]
async fn handle_query_realisation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let _output_id = wire::read_string(reader).await?;
    debug!("wopQueryRealisation (stubbed, returning empty)");

    stderr.finish().await?;
    // Return count = 0 (no realisations)
    wire::write_u64(stderr.inner_mut(), 0).await?;
    Ok(())
}

/// wopQueryMissing (40): Report what needs building.
///
/// Receives a collection of `DerivedPath` strings (which may contain `!*` or
/// `!out,dev` output specifiers). Checks the store for the base paths and
/// categorizes missing paths:
/// - **willBuild**: `Built` derivation paths whose base `.drv` is missing
/// - **unknown**: `Opaque` (non-derivation) paths that are missing
/// - **willSubstitute**: always empty (rio-build has no substituters)
#[instrument(skip_all)]
async fn handle_query_missing<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    debug!(count = raw_paths.len(), "wopQueryMissing");

    // Parse DerivedPath strings and extract base store paths for batch lookup.
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

    let store_paths: Vec<StorePath> = derived
        .iter()
        .map(|(_, dp)| dp.store_path().clone())
        .collect();

    let valid_set: HashSet<StorePath> = match store.query_valid_paths(&store_paths).await {
        Ok(v) => v.into_iter().collect(),
        Err(e) => return send_store_error(stderr, e).await,
    };

    let mut will_build = Vec::new();
    let mut unknown = Vec::new();

    // Categorize missing paths per nix-daemon semantics:
    // - Built paths (e.g., /nix/store/...-foo.drv!out) go to willBuild — the
    //   derivation needs to be built to produce the output.
    // - Opaque paths (plain store paths without output spec) go to unknown —
    //   they aren't derivations, so they can't be built, only substituted.
    //   Since rio-build has no substituters, unknown is the correct bucket.
    for (raw, dp) in &derived {
        if valid_set.contains(dp.store_path()) {
            continue;
        }
        match dp {
            DerivedPath::Built { .. } => will_build.push(raw.clone()),
            DerivedPath::Opaque(_) => unknown.push(raw.clone()),
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_strings(w, &will_build).await?;
    // willSubstitute: always empty (rio-build doesn't use external substituters)
    wire::write_strings(w, &[]).await?;
    wire::write_strings(w, &unknown).await?;
    // downloadSize: 0
    wire::write_u64(w, 0).await?;
    // narSize: 0
    wire::write_u64(w, 0).await?;
    Ok(())
}

/// wopAddToStoreNar (39): Receive a store path with NAR content via STDERR_READ pull loop.
///
/// Protocol >= 1.25 (always present for 1.37+):
/// 1. Read metadata fields (path, deriver, narHash, references, registrationTime,
///    narSize, ultimate, sigs, ca, repair)
/// 2. Pull NAR data via STDERR_READ: send STDERR_READ(count) → client responds
///    with u64(len) + data + padding → repeat until narSize bytes received
/// 3. Validate NAR hash, store path, cache .drv if applicable
#[instrument(skip_all)]
async fn handle_add_to_store_nar<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    use crate::store::PathInfoBuilder;

    // Read metadata fields (same order as wopAddToStoreNar wire format)
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

    debug!(
        path = %path_str,
        nar_size = nar_size,
        "wopAddToStoreNar"
    );

    // Validate nar_size before allocation (cap at 1 GiB to prevent OOM)
    if nar_size > wire::MAX_FRAMED_TOTAL {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!(
                    "nar_size {nar_size} exceeds maximum {} for {path_str}",
                    wire::MAX_FRAMED_TOTAL
                ),
            ))
            .await?;
        return Err(anyhow::anyhow!("nar_size exceeds maximum"));
    }

    // Pull NAR data via STDERR_READ loop
    let chunk_size: u64 = 64 * 1024; // 64 KiB chunks
    let mut nar_data = Vec::with_capacity(nar_size as usize);
    let mut remaining = nar_size;

    while remaining > 0 {
        let request_size = remaining.min(chunk_size);
        stderr.read_request(request_size).await?;

        // Client responds with u64(len) + data + padding (same as wire string format)
        let chunk = wire::read_bytes(reader).await?;
        if chunk.is_empty() {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("empty chunk received in STDERR_READ loop for {path_str}"),
                ))
                .await?;
            return Err(anyhow::anyhow!(
                "client sent empty chunk in STDERR_READ loop for {path_str}"
            ));
        }
        nar_data.extend_from_slice(&chunk);
        remaining = remaining.saturating_sub(chunk.len() as u64);
    }

    // Validate NAR hash
    let computed_hash = {
        let mut hasher = Sha256::new();
        hasher.update(&nar_data);
        let digest = hasher.finalize();
        hex::encode(digest)
    };

    // Parse the declared hash — nix-daemon sends just the hex digest
    let declared_hex = &nar_hash_str;
    if computed_hash != *declared_hex {
        warn!(
            path = %path_str,
            declared = %declared_hex,
            computed = %computed_hash,
            "NAR hash mismatch"
        );
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!(
                    "NAR hash mismatch for {path_str}: declared {declared_hex}, computed {computed_hash}"
                ),
            ))
            .await?;
        return Err(anyhow::anyhow!("NAR hash mismatch for {path_str}"));
    }

    // Build PathInfo
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

    let nar_hash = match hex::decode(&nar_hash_str) {
        Ok(digest) => match NixHash::new(rio_nix::hash::HashAlgo::SHA256, digest) {
            Ok(h) => h,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("invalid narHash digest '{nar_hash_str}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("invalid narHash: {e}"));
            }
        },
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

    let deriver = if deriver_str.is_empty() {
        None
    } else {
        match StorePath::parse(&deriver_str) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(path = %path_str, deriver = %deriver_str, error = %e,
                      "wopAddToStoreNar: unparseable deriver path, treating as unknown");
                None
            }
        }
    };

    let ref_paths: Vec<StorePath> = references
        .iter()
        .filter_map(|s| match StorePath::parse(s) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(path = %path_str, reference = %s, error = %e,
                      "wopAddToStoreNar: unparseable reference path, dropping");
                None
            }
        })
        .collect();

    let ca = if ca_str.is_empty() {
        None
    } else {
        Some(ca_str)
    };

    let info = match PathInfoBuilder::new(path.clone(), nar_hash, nar_size)
        .deriver(deriver)
        .references(ref_paths)
        .registration_time(registration_time)
        .ultimate(ultimate)
        .sigs(sigs)
        .ca(ca)
        .build()
    {
        Ok(info) => info,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to build PathInfo for '{path_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("PathInfo build failed: {e}"));
        }
    };

    // Store the path
    if let Err(e) = store.add_path(info, nar_data.clone()).await {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("failed to store path '{path_str}': {e}"),
            ))
            .await?;
        return Err(anyhow::anyhow!("store error: {e}"));
    }

    // If this is a .drv file, parse and cache it
    try_cache_drv(&path, &nar_data, drv_cache);

    // Send success
    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// If `path` is a `.drv`, extract from NAR, parse ATerm, and cache in drv_cache.
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
            warn!(path = %path, error = %e, "failed to extract .drv from NAR, skipping cache");
            return;
        }
    };
    let drv_text = match String::from_utf8(drv_bytes) {
        Ok(text) => text,
        Err(e) => {
            warn!(path = %path, error = %e, "failed to decode .drv as UTF-8, skipping cache");
            return;
        }
    };
    match Derivation::parse(&drv_text) {
        Ok(drv) => {
            debug!(path = %path, "cached parsed derivation");
            drv_cache.insert(path.clone(), drv);
        }
        Err(e) => {
            warn!(path = %path, error = %e, "failed to parse .drv ATerm, skipping cache");
        }
    }
}

/// Parse a single entry from the wopAddMultipleToStore reassembled byte stream.
///
/// Each entry contains PathInfo metadata fields (path, deriver, narHash,
/// references, registrationTime, narSize, ultimate, sigs, ca — no `repair`
/// flag, unlike wopAddToStoreNar where `repair` is per-request),
/// followed by an inner framed stream containing the NAR data.
async fn parse_add_multiple_entry(
    cursor: &mut std::io::Cursor<&[u8]>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    use crate::store::PathInfoBuilder;

    let path_str = wire::read_string(cursor).await?;
    let deriver_str = wire::read_string(cursor).await?;
    let nar_hash_str = wire::read_string(cursor).await?;
    let references = wire::read_strings(cursor).await?;
    let registration_time = wire::read_u64(cursor).await?;
    let nar_size = wire::read_u64(cursor).await?;
    let ultimate = wire::read_bool(cursor).await?;
    let sigs = wire::read_strings(cursor).await?;
    let ca_str = wire::read_string(cursor).await?;

    // Read inner framed NAR data
    let nar_data = wire::read_framed_stream(cursor).await?;

    debug!(
        path = %path_str,
        nar_size = nar_size,
        nar_actual = nar_data.len(),
        "wopAddMultipleToStore entry"
    );

    // Validate NAR hash
    let computed_hash = {
        let mut hasher = Sha256::new();
        hasher.update(&nar_data);
        hex::encode(hasher.finalize())
    };
    if computed_hash != nar_hash_str {
        warn!(
            path = %path_str,
            declared = %nar_hash_str,
            computed = %computed_hash,
            "NAR hash mismatch in wopAddMultipleToStore"
        );
        return Err(anyhow::anyhow!("NAR hash mismatch for {path_str}"));
    }

    // Build PathInfo
    let path = StorePath::parse(&path_str)
        .map_err(|e| anyhow::anyhow!("invalid store path '{path_str}': {e}"))?;

    let nar_hash_bytes =
        hex::decode(&nar_hash_str).map_err(|e| anyhow::anyhow!("invalid narHash hex: {e}"))?;
    let nar_hash = NixHash::new(rio_nix::hash::HashAlgo::SHA256, nar_hash_bytes)
        .map_err(|e| anyhow::anyhow!("invalid narHash: {e}"))?;

    let deriver = if deriver_str.is_empty() {
        None
    } else {
        match StorePath::parse(&deriver_str) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(path = %path_str, deriver = %deriver_str, error = %e,
                      "wopAddMultipleToStore: unparseable deriver path, treating as unknown");
                None
            }
        }
    };

    let ref_paths: Vec<StorePath> = references
        .iter()
        .filter_map(|s| match StorePath::parse(s) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(path = %path_str, reference = %s, error = %e,
                      "wopAddMultipleToStore: unparseable reference path, dropping");
                None
            }
        })
        .collect();

    let ca = if ca_str.is_empty() {
        None
    } else {
        Some(ca_str)
    };

    let info = PathInfoBuilder::new(path.clone(), nar_hash, nar_size)
        .deriver(deriver)
        .references(ref_paths)
        .registration_time(registration_time)
        .ultimate(ultimate)
        .sigs(sigs)
        .ca(ca)
        .build()?;

    store.add_path(info, nar_data.clone()).await?;
    try_cache_drv(&path, &nar_data, drv_cache);

    Ok(())
}

/// wopAddToStore (7): Legacy content-addressed store path import.
///
/// Protocol >= 1.25: reads name, content-address method string, references,
/// repair flag, then a framed data stream (NAR or flat file content).
/// Returns STDERR_LAST + full ValidPathInfo.
#[instrument(skip_all)]
async fn handle_add_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    use crate::store::PathInfoBuilder;

    let name = wire::read_string(reader).await?;
    let cam_str = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;
    let _repair = wire::read_bool(reader).await?;

    debug!(name = %name, cam_str = %cam_str, "wopAddToStore");

    // Read the dump data via framed stream
    let dump_data = wire::read_framed_stream(reader).await?;

    // Parse content-address method string: "text:sha256", "fixed:sha256", "fixed:r:sha256"
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

    // Hash the dump content
    let content_hash = NixHash::compute(hash_algo, &dump_data);

    // Compute the store path
    let ref_paths: Vec<StorePath> = references
        .iter()
        .filter_map(|s| match StorePath::parse(s) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(reference = %s, error = %e, "wopAddToStore: unparseable reference path, dropping");
                None
            }
        })
        .collect();

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

    // Build NAR: if the dump is flat content (text or non-recursive), wrap in NAR.
    // If recursive, the dump IS the NAR.
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

    // Compute NAR hash for PathInfo
    let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar_data);
    let nar_size = nar_data.len() as u64;

    // Build content address string for PathInfo
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

    let info = match PathInfoBuilder::new(path.clone(), nar_hash.clone(), nar_size)
        .references(ref_paths)
        .ultimate(true)
        .ca(Some(ca.clone()))
        .build()
    {
        Ok(info) => info,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to build PathInfo for '{}': {e}", path),
                ))
                .await?;
            return Err(anyhow::anyhow!("PathInfo build failed: {e}"));
        }
    };

    if let Err(e) = store.add_path(info, nar_data.clone()).await {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("failed to store path '{}': {e}", path),
            ))
            .await?;
        return Err(anyhow::anyhow!("store error: {e}"));
    }
    try_cache_drv(&path, &nar_data, drv_cache);

    // Send STDERR_LAST + ValidPathInfo
    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_string(w, &path.to_string()).await?; // path
    wire::write_string(w, "").await?; // deriver (none)
    wire::write_string(w, &nar_hash.to_hex()).await?; // narHash (hex, no prefix)
    // references
    let ref_strs: Vec<String> = references;
    wire::write_strings(w, &ref_strs).await?;
    wire::write_u64(w, 0).await?; // registrationTime
    wire::write_u64(w, nar_size).await?; // narSize
    wire::write_bool(w, true).await?; // ultimate
    wire::write_strings(w, &[]).await?; // sigs (empty)
    wire::write_string(w, &ca).await?; // ca

    Ok(())
}

/// wopAddTextToStore (8): Legacy text file import (used by builtins.toFile).
///
/// Wire format: name (string), text content (string), references (string collection).
/// Returns STDERR_LAST + StorePath.
#[instrument(skip_all)]
async fn handle_add_text_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    use crate::store::PathInfoBuilder;

    let name = wire::read_string(reader).await?;
    let text = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;

    debug!(name = %name, text_len = text.len(), "wopAddTextToStore");

    let content_hash = NixHash::compute(HashAlgo::SHA256, text.as_bytes());

    let ref_paths: Vec<StorePath> = references
        .iter()
        .filter_map(|s| match StorePath::parse(s) {
            Ok(p) => Some(p),
            Err(e) => {
                warn!(reference = %s, error = %e, "wopAddTextToStore: unparseable reference path, dropping");
                None
            }
        })
        .collect();

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

    // Wrap text in NAR
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

    let info = match PathInfoBuilder::new(path.clone(), nar_hash, nar_size)
        .references(ref_paths)
        .ultimate(true)
        .ca(Some(ca))
        .build()
    {
        Ok(info) => info,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("failed to build PathInfo for '{}': {e}", path),
                ))
                .await?;
            return Err(anyhow::anyhow!("PathInfo build failed: {e}"));
        }
    };

    if let Err(e) = store.add_path(info, nar_data.clone()).await {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("failed to store path '{}': {e}", path),
            ))
            .await?;
        return Err(anyhow::anyhow!("store error: {e}"));
    }
    try_cache_drv(&path, &nar_data, drv_cache);

    // wopAddTextToStore returns STDERR_LAST + store path string
    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), &path.to_string()).await?;

    Ok(())
}

/// Parse a content-address method string like "text:sha256", "fixed:sha256", "fixed:r:sha256".
///
/// Returns (is_text, is_recursive, hash_algo).
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
            Ok((false, true, algo)) // git mode is treated as recursive
        } else {
            let algo = rest.parse::<HashAlgo>().map_err(|e| e.to_string())?;
            Ok((false, false, algo))
        }
    } else {
        Err(format!("unrecognized content-address method: {cam_str}"))
    }
}

/// wopAddMultipleToStore (44): Receive multiple store paths via framed stream.
///
/// The primary upload path for modern Nix clients (protocol >= 1.32).
/// Receives a framed byte stream containing concatenated entries,
/// each with PathInfo metadata + an inner framed NAR.
#[instrument(skip_all)]
async fn handle_add_multiple_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let _repair = wire::read_bool(reader).await?;
    let _dont_check_sigs = wire::read_bool(reader).await?; // always treated as false

    debug!("wopAddMultipleToStore");

    // Read the outer framed stream into a contiguous buffer
    let stream_data = wire::read_framed_stream(reader).await?;

    // Parse entries sequentially from the reassembled stream
    let mut cursor = std::io::Cursor::new(stream_data.as_slice());
    let total_len = stream_data.len() as u64;

    while cursor.position() < total_len {
        if let Err(e) = parse_add_multiple_entry(&mut cursor, store, drv_cache).await {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("wopAddMultipleToStore entry failed: {e}"),
                ))
                .await?;
            return Err(e);
        }
    }

    // Send success
    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopQueryDerivationOutputMap (41): Return output name → path mappings for a derivation.
///
/// Looks up the parsed derivation from the session's drv_cache (populated
/// during wopAddToStoreNar), or fetches and parses from the store.
#[instrument(skip_all)]
async fn handle_query_derivation_output_map<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let drv_path_str = wire::read_string(reader).await?;
    debug!(path = %drv_path_str, "wopQueryDerivationOutputMap");

    let drv_path = match StorePath::parse(&drv_path_str) {
        Ok(p) => p,
        Err(e) => {
            warn!(path = %drv_path_str, error = %e, "invalid store path in wopQueryDerivationOutputMap");
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid store path '{drv_path_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid store path: {e}"));
        }
    };

    // Look up derivation: session cache first, then store
    let drv = if let Some(cached) = drv_cache.get(&drv_path) {
        cached.clone()
    } else {
        // Try to fetch from store and parse
        let nar_result = match store.nar_from_path(&drv_path).await {
            Ok(Some(nar_data)) => Ok(nar_data),
            Ok(None) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("derivation '{drv_path_str}' not found in store"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("derivation not found: {drv_path_str}"));
            }
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("store error looking up '{drv_path_str}': {e}"),
                    ))
                    .await?;
                Err(anyhow::anyhow!("store error: {e}"))
            }
        }?;

        let drv_bytes = match rio_nix::nar::extract_single_file(&nar_result) {
            Ok(bytes) => bytes,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("failed to extract .drv from NAR for '{drv_path_str}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("failed to extract .drv from NAR: {e}"));
            }
        };
        let drv_text = String::from_utf8_lossy(&drv_bytes);
        let drv = match Derivation::parse(&drv_text) {
            Ok(drv) => drv,
            Err(e) => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("failed to parse .drv for '{drv_path_str}': {e}"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("failed to parse .drv: {e}"));
            }
        };
        drv_cache.insert(drv_path.clone(), drv.clone());
        drv
    };

    // Build output map from derivation
    let outputs = drv.outputs();

    stderr.finish().await?;
    let w = stderr.inner_mut();

    // Write count + (name, path) pairs
    wire::write_u64(w, outputs.len() as u64).await?;
    for output in outputs {
        wire::write_string(w, output.name()).await?;
        wire::write_string(w, output.path()).await?;
    }

    Ok(())
}

/// wopBuildDerivation (36): Build a derivation via local nix-daemon --stdio.
///
/// 1. Read drvPath + BasicDerivation + buildMode from client
/// 2. Spawn `nix-daemon --stdio` subprocess
/// 3. Perform client handshake with local daemon
/// 4. Forward wopBuildDerivation to local daemon
/// 5. Relay STDERR messages from daemon to remote client
/// 6. Return BuildResult to remote client
#[instrument(skip_all)]
async fn handle_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    // Read client's request
    let drv_path_str = wire::read_string(reader).await?;
    let (outputs, input_srcs, platform, builder, args, env) =
        match read_basic_derivation(reader).await {
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
    let build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            warn!(
                build_mode = build_mode_val,
                "wopBuildDerivation: unknown build mode, defaulting to Normal"
            );
            BuildMode::Normal
        }
    };

    debug!(
        path = %drv_path_str,
        platform = %platform,
        builder = %builder,
        build_mode = ?build_mode,
        "wopBuildDerivation"
    );

    // Reconstruct BasicDerivation for forwarding
    let basic_drv = rio_nix::derivation::BasicDerivation::new(
        outputs,
        input_srcs.iter().cloned().collect(),
        platform,
        builder,
        args,
        env.into_iter().collect(),
    );

    // Build via local daemon (spawn → handshake → setOptions → build → kill)
    let build_result = match build_via_local_daemon(&drv_path_str, &basic_drv, build_mode).await {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "local daemon build failed");
            // Report daemon-level errors as BuildResult failures, not connection errors,
            // so the remote client gets a proper result instead of a connection drop.
            BuildResult::failure(BuildStatus::MiscFailure, format!("local daemon error: {e}"))
        }
    };

    debug!(
        status = ?build_result.status(),
        error_msg = %build_result.error_msg(),
        "wopBuildDerivation result"
    );

    // Send result to remote client
    stderr.finish().await?;
    write_build_result(stderr.inner_mut(), &build_result).await?;
    Ok(())
}

/// wopBuildPaths (9): Build a set of derivations.
///
/// For Phase 1b, delegates each derivation to the local nix-daemon sequentially.
#[instrument(skip_all)]
async fn handle_build_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            warn!(
                build_mode = build_mode_val,
                "wopBuildPaths: unknown build mode, defaulting to Normal"
            );
            BuildMode::Normal
        }
    };

    debug!(
        count = raw_paths.len(),
        build_mode = ?build_mode,
        "wopBuildPaths"
    );

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
                match store.is_valid_path(path).await {
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
                // Look up the derivation and build it
                let drv_obj = match drv_cache.get(drv) {
                    Some(d) => d.clone(),
                    None => {
                        // Try store
                        let nar = match store.nar_from_path(drv).await {
                            Ok(Some(nar)) => nar,
                            Ok(None) => {
                                stderr
                                    .error(&StderrError::simple(
                                        PROGRAM_NAME,
                                        format!("derivation '{}' not found", drv),
                                    ))
                                    .await?;
                                return Err(anyhow::anyhow!("derivation not found: {}", drv));
                            }
                            Err(e) => {
                                stderr
                                    .error(&StderrError::simple(
                                        PROGRAM_NAME,
                                        format!("store error looking up '{}': {}", drv, e),
                                    ))
                                    .await?;
                                return Err(anyhow::anyhow!("store error: {e}"));
                            }
                        };
                        let bytes = match rio_nix::nar::extract_single_file(&nar) {
                            Ok(b) => b,
                            Err(e) => {
                                stderr
                                    .error(&StderrError::simple(
                                        PROGRAM_NAME,
                                        format!("failed to extract .drv '{}' from NAR: {}", drv, e),
                                    ))
                                    .await?;
                                return Err(anyhow::anyhow!("NAR extract: {e}"));
                            }
                        };
                        let text = String::from_utf8_lossy(&bytes);
                        let parsed = match Derivation::parse(&text) {
                            Ok(d) => d,
                            Err(e) => {
                                stderr
                                    .error(&StderrError::simple(
                                        PROGRAM_NAME,
                                        format!("failed to parse .drv '{}': {}", drv, e),
                                    ))
                                    .await?;
                                return Err(anyhow::anyhow!("ATerm parse: {e}"));
                            }
                        };
                        drv_cache.insert(drv.clone(), parsed.clone());
                        parsed
                    }
                };

                // Build via local daemon
                let result =
                    build_via_local_daemon(&drv.to_string(), &drv_obj.to_basic(), build_mode).await;
                if let Ok(ref r) = result
                    && !r.status().is_success()
                {
                    stderr
                        .error(&StderrError::simple(
                            PROGRAM_NAME,
                            format!("build of '{}' failed: {}", drv, r.error_msg()),
                        ))
                        .await?;
                    return Err(anyhow::anyhow!("build failed: {}", drv));
                }
                if let Err(e) = result {
                    stderr
                        .error(&StderrError::simple(
                            PROGRAM_NAME,
                            format!("build of '{}' failed: {}", drv, e),
                        ))
                        .await?;
                    return Err(e);
                }
            }
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopBuildPathsWithResults (46): Build paths and return per-path BuildResult.
#[instrument(skip_all)]
async fn handle_build_paths_with_results<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store: &dyn Store,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let build_mode = match BuildMode::try_from(build_mode_val) {
        Ok(m) => m,
        Err(_) => {
            warn!(
                build_mode = build_mode_val,
                "wopBuildPathsWithResults: unknown build mode, defaulting to Normal"
            );
            BuildMode::Normal
        }
    };

    debug!(
        count = raw_paths.len(),
        build_mode = ?build_mode,
        "wopBuildPathsWithResults"
    );

    let mut results = Vec::new();

    for raw in &raw_paths {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => {
                results.push(BuildResult::failure(
                    BuildStatus::MiscFailure,
                    format!("invalid path '{raw}': {e}"),
                ));
                continue;
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => match store.is_valid_path(path).await {
                Ok(true) => {
                    results.push(BuildResult::new(
                        BuildStatus::AlreadyValid,
                        String::new(),
                        0,
                        false,
                        0,
                        0,
                        None,
                        None,
                        Vec::new(),
                    ));
                }
                Ok(false) => {
                    results.push(BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!("path '{}' not valid", path),
                    ));
                }
                Err(e) => {
                    results.push(BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!("store error checking '{}': {e}", path),
                    ));
                }
            },
            DerivedPath::Built { drv, .. } => {
                let drv_obj = match drv_cache.get(drv) {
                    Some(d) => d.clone(),
                    None => {
                        let nar = match store.nar_from_path(drv).await {
                            Ok(Some(nar)) => nar,
                            Ok(None) => {
                                results.push(BuildResult::failure(
                                    BuildStatus::MiscFailure,
                                    format!("derivation '{}' not found", drv),
                                ));
                                continue;
                            }
                            Err(e) => {
                                results.push(BuildResult::failure(
                                    BuildStatus::MiscFailure,
                                    format!("store error looking up '{}': {}", drv, e),
                                ));
                                continue;
                            }
                        };
                        let bytes = match rio_nix::nar::extract_single_file(&nar) {
                            Ok(b) => b,
                            Err(e) => {
                                results.push(BuildResult::failure(
                                    BuildStatus::MiscFailure,
                                    format!("failed to extract .drv '{}': {}", drv, e),
                                ));
                                continue;
                            }
                        };
                        let text = String::from_utf8_lossy(&bytes);
                        match Derivation::parse(&text) {
                            Ok(parsed) => {
                                drv_cache.insert(drv.clone(), parsed.clone());
                                parsed
                            }
                            Err(e) => {
                                results.push(BuildResult::failure(
                                    BuildStatus::MiscFailure,
                                    format!("failed to parse .drv '{}': {}", drv, e),
                                ));
                                continue;
                            }
                        }
                    }
                };

                let mut result =
                    build_via_local_daemon(&drv.to_string(), &drv_obj.to_basic(), build_mode)
                        .await
                        .unwrap_or_else(|e| {
                            BuildResult::failure(
                                BuildStatus::MiscFailure,
                                format!("daemon error: {e}"),
                            )
                        });

                debug!(
                    status = ?result.status(),
                    built_outputs = result.built_outputs().len(),
                    "daemon build result for {}",
                    drv
                );

                // If the daemon returned success but empty builtOutputs
                // (common for AlreadyValid), populate from the derivation.
                if result.status().is_success() && result.built_outputs().is_empty() {
                    result = result.with_outputs_from_drv(&drv_obj, drv);
                    for out in result.built_outputs() {
                        debug!(
                            drv_output_id = %out.drv_output_id,
                            out_path = %out.out_path,
                            "populated output from drv"
                        );
                    }
                }

                results.push(result);
            }
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    // Write count + per-path (DerivedPath, BuildResult) pairs
    // The real daemon writes KeyedBuildResult = DerivedPath + BuildResult
    wire::write_u64(w, results.len() as u64).await?;
    for (raw, result) in raw_paths.iter().zip(results.iter()) {
        // DerivedPath key (same string the client sent)
        wire::write_string(w, raw).await?;
        write_build_result(w, result).await?;
    }

    Ok(())
}

/// Build a derivation by spawning a local `nix-daemon --stdio` and
/// forwarding `wopBuildDerivation`.
///
/// Handles the full lifecycle: spawn → handshake → setOptions → build → kill.
/// Applies `DAEMON_BUILD_TIMEOUT` to prevent indefinite hangs.
async fn build_via_local_daemon(
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    build_mode: BuildMode,
) -> anyhow::Result<BuildResult> {
    let mut daemon = tokio::process::Command::new("nix-daemon")
        .arg("--stdio")
        .stdin(std::process::Stdio::piped())
        .stdout(std::process::Stdio::piped())
        .stderr(std::process::Stdio::null())
        .spawn()?;

    let mut daemon_stdin = match daemon.stdin.take() {
        Some(s) => s,
        None => {
            let _ = daemon.kill().await;
            return Err(anyhow::anyhow!("nix-daemon stdin not available"));
        }
    };
    let mut daemon_stdout = match daemon.stdout.take() {
        Some(s) => s,
        None => {
            let _ = daemon.kill().await;
            return Err(anyhow::anyhow!("nix-daemon stdout not available"));
        }
    };

    let build_fut = async {
        let handshake = client::client_handshake(&mut daemon_stdout, &mut daemon_stdin)
            .await
            .map_err(|e| anyhow::anyhow!("daemon handshake failed: {e}"))?;
        debug!(
            version = handshake.negotiated_version(),
            "local daemon handshake"
        );

        client::client_set_options(&mut daemon_stdout, &mut daemon_stdin)
            .await
            .map_err(|e| anyhow::anyhow!("daemon set_options failed: {e}"))?;

        client::client_build_derivation(
            &mut daemon_stdout,
            &mut daemon_stdin,
            drv_path,
            basic_drv,
            build_mode,
        )
        .await
        .map_err(|e| anyhow::anyhow!("daemon build failed: {e}"))
    };

    let timeout = match std::env::var("RIO_DAEMON_TIMEOUT_SECS") {
        Ok(val) => match val.parse::<u64>() {
            Ok(secs) => {
                debug!(timeout_secs = secs, "using configured daemon timeout");
                std::time::Duration::from_secs(secs)
            }
            Err(e) => {
                warn!(value = %val, error = %e,
                      "invalid RIO_DAEMON_TIMEOUT_SECS, using default {}s",
                      DAEMON_BUILD_TIMEOUT.as_secs());
                DAEMON_BUILD_TIMEOUT
            }
        },
        Err(_) => DAEMON_BUILD_TIMEOUT,
    };

    let result = match tokio::time::timeout(timeout, build_fut).await {
        Ok(r) => r,
        Err(_) => {
            error!(drv_path = %drv_path, "local daemon build timed out");
            if let Err(e) = daemon.kill().await {
                warn!(drv_path = %drv_path, error = %e, "failed to kill local daemon after timeout");
            }
            return Err(anyhow::anyhow!(
                "local daemon build timed out after {}s",
                timeout.as_secs()
            ));
        }
    };

    if let Err(e) = daemon.kill().await {
        warn!(drv_path = %drv_path, error = %e, "failed to kill local daemon process");
    }

    result
}
