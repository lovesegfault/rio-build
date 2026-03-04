//! Read-only opcode handlers (query, ensure, temp-root, options).

use super::*;

/// wopIsValidPath (1): Check if a store path exists.
#[instrument(skip_all)]
pub(super) async fn handle_is_valid_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
pub(super) async fn handle_ensure_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
pub(super) async fn handle_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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

    let info = match grpc_query_path_info(store_client, path.as_str()).await {
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
            // deriver: Option<StorePath> → empty string if None (wire convention)
            wire::write_string(w, info.deriver.as_ref().map(|d| d.as_str()).unwrap_or("")).await?;
            // narHash: [u8;32] → hex string
            wire::write_string(w, &hex::encode(info.nar_hash)).await?;
            // references: Vec<StorePath> → Vec<&str> via Deref
            let ref_strs: Vec<&str> = info.references.iter().map(|r| r.as_str()).collect();
            wire::write_strings(w, &ref_strs).await?;
            wire::write_u64(w, info.registration_time).await?;
            wire::write_u64(w, info.nar_size).await?;
            wire::write_bool(w, info.ultimate).await?;
            wire::write_strings(w, &info.signatures).await?;
            // content_address: Option<String> → empty string if None
            wire::write_string(w, info.content_address.as_deref().unwrap_or("")).await?;
        }
    }

    Ok(())
}

/// wopQueryValidPaths (31): Batch validity check.
#[instrument(skip_all)]
pub(super) async fn handle_query_valid_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
    let resp = rio_common::grpc::with_timeout(
        "FindMissingPaths",
        DEFAULT_GRPC_TIMEOUT,
        store_client.find_missing_paths(req),
    )
    .await;

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
pub(super) async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
pub(super) async fn handle_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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

/// wopNarFromPath (38): Export path as raw NAR bytes AFTER STDERR_LAST.
///
/// Nix client: `processStderr(ex)` (no sink) → `copyNAR(from, sink)`.
/// The stderr loop exits on STDERR_LAST; the NAR is read as raw bytes after.
/// Previously this used STDERR_WRITE (like wopExportPaths), but narFromPath's
/// client does NOT pass a sink to processStderr → 'error: no sink'.
#[instrument(skip_all)]
pub(super) async fn handle_nar_from_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
            stderr_err!(stderr, "invalid store path '{path_str}': {e}");
        }
    };

    // Fetch the FULL NAR before sending STDERR_LAST. We can't stream
    // incrementally because a mid-stream gRPC error would leave the client's
    // copyNAR() with a truncated NAR and no way to signal the error (it's
    // already past the stderr loop). For Phase 2a this is acceptable; Phase 2b
    // should add NAR framing (length-prefixed) or use wopExportPaths instead.
    let req = types::GetPathRequest {
        store_path: path.to_string(),
    };
    let mut stream =
        match tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, store_client.get_path(req)).await {
            Ok(Ok(resp)) => resp.into_inner(),
            Ok(Err(status)) if status.code() == tonic::Code::NotFound => {
                stderr_err!(stderr, "path '{path_str}' is not valid");
            }
            Ok(Err(e)) => stderr_err!(stderr, "gRPC GetPath failed: {e}"),
            Err(_) => stderr_err!(
                stderr,
                "gRPC GetPath timed out after {DEFAULT_GRPC_TIMEOUT:?}"
            ),
        };

    let (_info, nar_data) =
        match rio_proto::client::collect_nar_stream(&mut stream, rio_common::limits::MAX_NAR_SIZE)
            .await
        {
            Ok(v) => v,
            Err(e) => stderr_err!(stderr, "gRPC GetPath for {path_str}: {e}"),
        };

    // STDERR_LAST first, then raw NAR bytes. Client's copyNAR reads until
    // the NAR's closing ')' sentinel — no length prefix.
    stderr.finish().await?;
    let w = stderr.inner_mut();
    tokio::io::AsyncWriteExt::write_all(w, &nar_data).await?;
    Ok(())
}

/// wopQueryPathFromHashPart (29): Resolve a store path from its hash part.
///
/// Nix sends just the 32-char nixbase32 hash (no /nix/store/ prefix, no
/// name) and expects the full path back. Wire response: one string —
/// the full path if found, empty string if not.
#[instrument(skip_all)]
pub(super) async fn handle_query_path_from_hash_part<
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let hash_part = wire::read_string(reader).await?;
    debug!(hash_part = %hash_part, "wopQueryPathFromHashPart");

    // Dedicated RPC (resolves the old TODO(phase2c) workaround).
    // The store validates hash_part (32 chars, nixbase32 charset) and
    // does the LIKE prefix query. NOT_FOUND → empty string to Nix; other
    // gRPC errors → STDERR_ERROR.
    let req = types::QueryPathFromHashPartRequest { hash_part };
    let path_str = match rio_common::grpc::with_timeout(
        "QueryPathFromHashPart",
        DEFAULT_GRPC_TIMEOUT,
        store_client.query_path_from_hash_part(req),
    )
    .await
    {
        Ok(info) => info.into_inner().store_path,
        // NOT_FOUND is a normal "no such path" result, not an error. Nix
        // expects empty string for that case (that's how `nix-store -q`
        // distinguishes found from not-found).
        Err(e)
            if e.downcast_ref::<tonic::Status>()
                .is_some_and(|s| s.code() == tonic::Code::NotFound) =>
        {
            String::new()
        }
        Err(e) => return send_store_error(stderr, e).await,
    };

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), &path_str).await?;
    Ok(())
}

/// wopAddSignatures (37): Add signatures to an existing store path.
///
/// Wire: path string + strings list. Response: u64(1) on success.
/// The real daemon returns STDERR_ERROR on unknown path; we do the same.
#[instrument(skip_all)]
pub(super) async fn handle_add_signatures<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    let sigs = wire::read_strings(reader).await?;
    debug!(path = %path_str, count = sigs.len(), "wopAddSignatures");

    // Dedicated RPC (resolves the old TODO(phase2c) stub). The store
    // appends to narinfo.signatures TEXT[]. NOT_FOUND → STDERR_ERROR
    // (matches the real daemon: signing a path you don't have is an error,
    // not a silent no-op — `nix store sign` expects to hear about it).
    let req = types::AddSignaturesRequest {
        store_path: path_str,
        signatures: sigs,
    };
    if let Err(e) = rio_common::grpc::with_timeout(
        "AddSignatures",
        DEFAULT_GRPC_TIMEOUT,
        store_client.add_signatures(req),
    )
    .await
    {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

/// wopRegisterDrvOutput (42): Stub for CA derivation output registration.
#[instrument(skip_all)]
pub(super) async fn handle_register_drv_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
pub(super) async fn handle_query_realisation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
pub(super) async fn handle_query_missing<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
    let missing_set: HashSet<String> = match tokio::time::timeout(
        DEFAULT_GRPC_TIMEOUT,
        store_client.find_missing_paths(req),
    )
    .await
    {
        Ok(Ok(r)) => r.into_inner().missing_paths.into_iter().collect(),
        Ok(Err(e)) => stderr_err!(stderr, "gRPC FindMissingPaths: {e}"),
        Err(_) => stderr_err!(
            stderr,
            "gRPC FindMissingPaths timed out after {DEFAULT_GRPC_TIMEOUT:?}"
        ),
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
                if let Err(e) = resolve_derivation(drv, store_client, drv_cache).await {
                    tracing::warn!(drv = %drv, error = %e, "failed to resolve derivation in wopQueryMissing");
                }
                will_build.push(raw.clone());
            }
            DerivedPath::Opaque(_) => unknown.push(raw.clone()),
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_strings(w, &will_build).await?;
    wire::write_strings(w, wire::NO_STRINGS).await?; // willSubstitute: always empty
    wire::write_strings(w, &unknown).await?;
    wire::write_u64(w, 0).await?; // downloadSize
    wire::write_u64(w, 0).await?; // narSize
    Ok(())
}

/// wopQueryDerivationOutputMap (41): Return output name -> path mappings.
#[instrument(skip_all)]
pub(super) async fn handle_query_derivation_output_map<
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
>(
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
            stderr_err!(stderr, "invalid store path '{drv_path_str}': {e}");
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
