//! Opcode dispatch and handler implementations for the Nix worker protocol.
//!
//! Each handler reads its opcode-specific payload from the stream, performs
//! the operation, and writes the response via the STDERR streaming loop.

use std::collections::{HashMap, HashSet};

use rio_nix::derivation::Derivation;
use rio_nix::hash::NixHash;
use rio_nix::protocol::derived_path::DerivedPath;
use rio_nix::protocol::opcodes::WorkerOp;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use sha2::{Digest, Sha256};
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, instrument, warn};

use crate::store::Store;

const PROGRAM_NAME: &str = "rio-build";

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
        Some(WorkerOp::QueryDerivationOutputMap) => {
            handle_query_derivation_output_map(reader, &mut stderr, store, drv_cache).await
        }
        Some(op) => {
            warn!(
                opcode = opcode,
                name = op.name(),
                "unimplemented opcode, closing connection"
            );
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!(
                        "operation {} ({}) is not yet implemented",
                        op.name(),
                        opcode
                    ),
                ))
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

    // Pull NAR data via STDERR_READ loop
    let chunk_size: u64 = 64 * 1024; // 64 KiB chunks
    let mut nar_data = Vec::with_capacity(nar_size as usize);
    let mut remaining = nar_size;

    while remaining > 0 {
        let request_size = remaining.min(chunk_size);
        stderr.read_request(request_size).await?;

        // Client responds with u64(len) + data + padding (same as wire string format)
        let chunk = wire::read_bytes(reader).await?;
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

    let nar_hash = match NixHash::parse(&format!("sha256:{nar_hash_str}")) {
        Ok(h) => h,
        Err(e) => {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("invalid narHash '{nar_hash_str}': {e}"),
                ))
                .await?;
            return Err(anyhow::anyhow!("invalid narHash: {e}"));
        }
    };

    let deriver = if deriver_str.is_empty() {
        None
    } else {
        StorePath::parse(&deriver_str).ok()
    };

    let ref_paths: Vec<StorePath> = references
        .iter()
        .filter_map(|s| StorePath::parse(s).ok())
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

    // Store the path
    store.add_path(info, nar_data.clone()).await?;

    // If this is a .drv file, parse and cache it
    if path.is_derivation() {
        match rio_nix::nar::extract_single_file(&nar_data) {
            Ok(drv_bytes) => {
                let drv_text = String::from_utf8_lossy(&drv_bytes);
                match Derivation::parse(&drv_text) {
                    Ok(drv) => {
                        debug!(path = %path, "cached parsed derivation");
                        drv_cache.insert(path, drv);
                    }
                    Err(e) => {
                        warn!(path = %path_str, error = %e, "failed to parse .drv ATerm, skipping cache");
                    }
                }
            }
            Err(e) => {
                warn!(path = %path_str, error = %e, "failed to extract .drv from NAR, skipping cache");
            }
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
        match store.nar_from_path(&drv_path).await? {
            Some(nar_data) => {
                let drv_bytes = rio_nix::nar::extract_single_file(&nar_data)
                    .map_err(|e| anyhow::anyhow!("failed to extract .drv from NAR: {e}"))?;
                let drv_text = String::from_utf8_lossy(&drv_bytes);
                let drv = Derivation::parse(&drv_text)
                    .map_err(|e| anyhow::anyhow!("failed to parse .drv: {e}"))?;
                drv_cache.insert(drv_path.clone(), drv.clone());
                drv
            }
            None => {
                stderr
                    .error(&StderrError::simple(
                        PROGRAM_NAME,
                        format!("derivation '{drv_path_str}' not found in store"),
                    ))
                    .await?;
                return Err(anyhow::anyhow!("derivation not found: {drv_path_str}"));
            }
        }
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
