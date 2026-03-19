//! Read-only opcode handlers (query, ensure, temp-root, options).

use super::*;

// r[impl gw.opcode.is-valid-path]
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

// r[impl gw.opcode.mandatory-set]
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

// r[impl gw.opcode.query-path-info]
// r[impl gw.wire.narhash-hex]
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
            wire::write_string(w, info.deriver.as_deref().unwrap_or("")).await?;
            // narHash: [u8;32] → hex string
            wire::write_string(w, &hex::encode(info.nar_hash)).await?;
            // references: Vec<StorePath> — StorePath: AsRef<str>
            wire::write_strings(w, &info.references).await?;
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

// r[impl gw.opcode.query-valid-paths]
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

    debug!(
        queried = missing_set.len() + valid_strs.len(),
        valid = valid_strs.len(),
        missing = missing_set.len(),
        "wopQueryValidPaths: result"
    );

    stderr.finish().await?;
    wire::write_strings(stderr.inner_mut(), &valid_strs).await?;
    Ok(())
}

// r[impl gw.opcode.mandatory-set]
/// wopAddTempRoot (11): read-and-ack no-op.
///
/// Temp roots are a local-daemon concept — "don't GC this path while
/// my session holds it." rio's GC is store-side with explicit pins
/// (see `rio-store/src/gc/`); a gateway-session-scoped set is invisible
/// to it. We MUST still consume the path off the wire and ack with `1`,
/// or the client desyncs.
///
/// Previously this inserted into a `HashSet<StorePath>` that nothing
/// read — unbounded growth on `nix copy` of large closures.
#[instrument(skip_all)]
pub(super) async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    debug!(path = %path_str, "wopAddTempRoot");

    // Still validate — a malformed path is a client bug worth logging,
    // even though we do nothing with the result.
    if let Err(e) = StorePath::parse(&path_str) {
        warn!(path = %path_str, error = %e, "invalid store path in wopAddTempRoot, ignoring");
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

// r[impl gw.opcode.set-options.field-order]
// r[impl gw.opcode.set-options.propagation]
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

// r[impl gw.opcode.nar-from-path]
// r[impl gw.opcode.nar-from-path.raw-bytes]
/// wopNarFromPath (38): Export path as raw NAR bytes AFTER STDERR_LAST.
///
/// Nix client: `processStderr(ex)` (no sink) → `copyNAR(from, sink)`.
/// The stderr loop exits on STDERR_LAST; the NAR is read as raw bytes after.
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
    // incrementally: a mid-stream gRPC error would leave the client's
    // copyNAR() with a truncated NAR and no way to signal the error (it's
    // already past the stderr loop). The Nix protocol has no framing for
    // this opcode — raw NAR bytes follow STDERR_LAST directly — so buffering
    // is the only correct behavior.
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

// r[impl gw.opcode.mandatory-set]
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

    // The store validates hash_part (32 chars, nixbase32 charset) and
    // does the LIKE prefix query. NOT_FOUND → empty string to Nix;
    // other gRPC errors → STDERR_ERROR.
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

// r[impl gw.opcode.mandatory-set]
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

    // The store appends to narinfo.signatures TEXT[]. NOT_FOUND →
    // STDERR_ERROR (matches the real daemon: signing a path you don't
    // have is an error, not a silent no-op — `nix store sign` expects
    // to hear about it).
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

/// Parse a Nix DrvOutput id string into (drv_hash, output_name).
///
/// Wire format: `"sha256:<hex>!<name>"` — hex (not nixbase32), exclamation
/// separator. This is the same format Nix uses in BuildResult builtOutputs
/// (see rio-nix BuildResult::built_outputs encoding). The `sha256:` prefix is
/// literal; Nix doesn't support other hash algos here.
///
/// Returns `None` for malformed input. Caller logs + soft-fails (accept
/// and discard for Register, empty-set for Query) — a bad id from a buggy
/// client shouldn't crash the session.
fn parse_drv_output_id(id: &str) -> Option<([u8; 32], String)> {
    let (hash_part, output_name) = id.split_once('!')?;
    let hex_part = hash_part.strip_prefix("sha256:")?;
    let bytes = hex::decode(hex_part).ok()?;
    let drv_hash: [u8; 32] = bytes.as_slice().try_into().ok()?;
    // Empty output name is never valid (would be "sha256:...!").
    if output_name.is_empty() {
        return None;
    }
    Some((drv_hash, output_name.to_string()))
}

/// wopRegisterDrvOutput (42): Register a CA derivation output mapping.
///
/// Wire: ONE string = Realisation JSON. The JSON has four fields we care
/// about: `id` (DrvOutput string), `outPath`, `signatures`,
/// `dependentRealisations` (ignored — phase 5's early cutoff uses it, not
/// us). Response: nothing after STDERR_LAST (no result data).
///
/// Soft-fail on malformed JSON/id: log + return success. Hard-failing
/// here would break buggy clients; a bad registration just means the
/// cache-hit doesn't happen — degraded, not broken.
// r[impl gw.opcode.mandatory-set]
#[instrument(skip_all)]
pub(super) async fn handle_register_drv_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let json = wire::read_string(reader).await?;
    debug!(json_len = json.len(), "wopRegisterDrvOutput");

    // Parse the Realisation JSON. serde_json::Value (not a struct derive)
    // because the Nix JSON schema has fields we don't care about
    // (dependentRealisations) and fields we can't validate at parse time
    // (id needs the !-split). Value lets us pull out exactly what we need
    // and ignore the rest — forward-compatible with whatever Nix adds.
    let parsed: serde_json::Value = match serde_json::from_str(&json) {
        Ok(v) => v,
        Err(e) => {
            warn!(json = %json, error = %e, "wopRegisterDrvOutput: malformed JSON, discarding");
            stderr.finish().await?;
            return Ok(());
        }
    };

    let id = parsed
        .get("id")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    // Wire outPath is a BASENAME ("<hashpart>-<name>") per CppNix
    // StorePath::to_string(). Our gRPC/PG repr uses full paths. Prepend
    // the prefix here — the gateway is the translation boundary. Idempotent
    // guard: if a (buggy or future) client sends the full path, don't
    // double-prepend. Reference: rio-nix/src/protocol/build.rs:350-351
    // (same transform for BuildResult.builtOutputs read path).
    let out_path_raw = parsed
        .get("outPath")
        .and_then(|v| v.as_str())
        .unwrap_or_default();
    let out_path = if out_path_raw.starts_with(rio_nix::store_path::STORE_PREFIX) {
        out_path_raw.to_string()
    } else {
        format!("{}{out_path_raw}", rio_nix::store_path::STORE_PREFIX)
    };

    let Some((drv_hash, output_name)) = parse_drv_output_id(id) else {
        warn!(id = %id, "wopRegisterDrvOutput: malformed DrvOutput id, discarding");
        stderr.finish().await?;
        return Ok(());
    };

    // signatures: optional array of strings. Default to empty if missing/wrong-type.
    let signatures: Vec<String> = parsed
        .get("signatures")
        .and_then(|v| v.as_array())
        .map(|arr| {
            arr.iter()
                .filter_map(|s| s.as_str().map(String::from))
                .collect()
        })
        .unwrap_or_default();

    // output_hash: the Nix Realisation JSON doesn't include it directly.
    // We could QueryPathInfo for the outPath and pull nar_hash... but that's
    // a roundtrip we can defer. Store zeros for now — the store's
    // RegisterRealisation validates output_hash is 32 bytes but doesn't
    // check it's the RIGHT hash (it can't, without the NAR). Phase 5's
    // realisation signing will need the real hash; until then, this is
    // metadata the store has elsewhere (narinfo.nar_hash for outPath).
    //
    // TODO(P0253): populate output_hash from QueryPathInfo(outPath).nar_hash
    // before signing realisations. Zeros are fine for cache-hit purposes —
    // QueryRealisation only uses (drv_hash, output_name) as the key.
    let output_hash = [0u8; 32];

    let req = types::RegisterRealisationRequest {
        realisation: Some(types::Realisation {
            drv_hash: drv_hash.to_vec(),
            output_name,
            output_path: out_path,
            output_hash: output_hash.to_vec(),
            signatures,
        }),
    };

    // gRPC error here IS a hard fail (store unreachable, not client input).
    if let Err(e) = rio_common::grpc::with_timeout(
        "RegisterRealisation",
        DEFAULT_GRPC_TIMEOUT,
        store_client.register_realisation(req),
    )
    .await
    {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    Ok(())
}

/// wopQueryRealisation (43): Look up a CA derivation output mapping.
///
/// Wire: ONE string = DrvOutput id (`"sha256:<hex>!<name>"`).
/// Response: u64 count + per-entry Realisation JSON string. Count is 0
/// (cache miss, normal) or 1 (found). Nix's wire format allows >1 but in
/// practice the store returns at most one — the (drv_hash, output_name) PK
/// is unique.
// r[impl gw.opcode.mandatory-set]
#[instrument(skip_all)]
pub(super) async fn handle_query_realisation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
) -> anyhow::Result<()> {
    let id = wire::read_string(reader).await?;
    debug!(id = %id, "wopQueryRealisation");

    // Malformed id → empty set (same soft-fail rationale as Register).
    let Some((drv_hash, output_name)) = parse_drv_output_id(&id) else {
        warn!(id = %id, "wopQueryRealisation: malformed DrvOutput id, returning empty");
        stderr.finish().await?;
        wire::write_u64(stderr.inner_mut(), 0).await?;
        return Ok(());
    };

    let req = types::QueryRealisationRequest {
        drv_hash: drv_hash.to_vec(),
        output_name,
    };
    let result = rio_common::grpc::with_timeout(
        "QueryRealisation",
        DEFAULT_GRPC_TIMEOUT,
        store_client.query_realisation(req),
    )
    .await;

    stderr.finish().await?;
    let w = stderr.inner_mut();

    match result {
        Ok(resp) => {
            let r = resp.into_inner();
            // Wire outPath is a BASENAME — CppNix Realisation::fromJSON
            // feeds it to StorePath::parse, which rejects '/' with
            // "illegal base-32 char '/'". Strip the prefix. Reference:
            // rio-nix/src/protocol/build.rs:415-418 (same transform for
            // BuildResult.builtOutputs write path). unwrap_or defensive:
            // if the store somehow returns a basename, pass it through.
            let out_path_basename = r
                .output_path
                .strip_prefix(rio_nix::store_path::STORE_PREFIX)
                .unwrap_or(&r.output_path);
            // Reconstruct the Nix Realisation JSON. id is the same string
            // we got; outPath from the store; signatures from the store;
            // dependentRealisations is always empty (phase 5 populates it).
            //
            // serde_json::json! macro gives us correct escaping for free —
            // outPath could theoretically contain JSON-special chars (it
            // won't, store paths are nixbase32+name, but defensive).
            let json = serde_json::json!({
                "id": id,
                "outPath": out_path_basename,
                "signatures": r.signatures,
                "dependentRealisations": {}
            });
            wire::write_u64(w, 1).await?;
            wire::write_string(w, &json.to_string()).await?;
        }
        // NOT_FOUND is a cache miss → empty set. Normal, not an error.
        Err(e)
            if e.downcast_ref::<tonic::Status>()
                .is_some_and(|s| s.code() == tonic::Code::NotFound) =>
        {
            wire::write_u64(w, 0).await?;
        }
        // Other gRPC errors (store unreachable) ARE errors. But we've
        // already sent STDERR_LAST above — too late for STDERR_ERROR.
        // Return empty set + log. The next opcode on this session will
        // hit the same store and fail properly via its own error path.
        //
        // Why not check the error BEFORE stderr.finish()? Because we'd
        // need to buffer the response. This is a rare degraded path
        // (store blip during a CA cache check); the consequence is one
        // missed cache hit, not corruption.
        Err(e) => {
            warn!(error = %e, "wopQueryRealisation: store error after STDERR_LAST, returning empty");
            wire::write_u64(w, 0).await?;
        }
    }

    Ok(())
}

// r[impl gw.opcode.query-missing]
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

    let derived: Vec<DerivedPath> = raw_paths
        .into_iter()
        .filter_map(|s| match DerivedPath::parse(&s) {
            Ok(dp) => Some(dp),
            Err(e) => {
                warn!(path = %s, error = %e, "dropping unparseable DerivedPath in wopQueryMissing");
                None
            }
        })
        .collect();

    let store_paths: Vec<String> = derived
        .iter()
        .map(|dp| dp.store_path().to_string())
        .collect();

    let req = types::FindMissingPathsRequest { store_paths };
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

    // wopQueryMissing response is three StorePathSets (willBuild,
    // willSubstitute, unknown) — plain store paths, NOT DerivedPath
    // wire strings. For Built paths, report the .drv; echoing the raw
    // `...drv!out` string makes the Nix client fail StorePath::parse
    // on '!'.
    let mut will_build = Vec::new();
    let mut unknown = Vec::new();

    for dp in derived {
        let sp = dp.store_path();
        if !missing_set.contains(sp.as_str()) {
            continue;
        }
        match dp {
            DerivedPath::Built { drv, .. } => {
                if let Err(e) = resolve_derivation(&drv, store_client, drv_cache).await {
                    tracing::warn!(drv = %drv, error = %e, "failed to resolve derivation in wopQueryMissing");
                }
                will_build.push(drv.as_str().to_string());
            }
            DerivedPath::Opaque(path) => unknown.push(path.as_str().to_string()),
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

// r[impl gw.opcode.query-derivation-output-map]
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

#[cfg(test)]
mod tests {
    use super::parse_drv_output_id;

    #[test]
    fn parse_valid_id() {
        let hex = "aa".repeat(32);
        let id = format!("sha256:{hex}!out");
        let (hash, name) = parse_drv_output_id(&id).unwrap();
        assert_eq!(hash, [0xAA; 32]);
        assert_eq!(name, "out");
    }

    #[test]
    fn parse_multi_word_output_name() {
        // Multi-output derivations: "out", "dev", "doc", "man", etc.
        // Names are identifiers, no special chars, but test that split_once
        // doesn't eat characters.
        let hex = "00".repeat(32);
        let (_, name) = parse_drv_output_id(&format!("sha256:{hex}!dev")).unwrap();
        assert_eq!(name, "dev");
    }

    #[test]
    fn parse_rejects_missing_bang() {
        assert!(parse_drv_output_id("sha256:aabb").is_none());
    }

    #[test]
    fn parse_rejects_wrong_prefix() {
        // blake3: isn't a thing Nix sends. Only sha256: on this wire.
        let hex = "aa".repeat(32);
        assert!(parse_drv_output_id(&format!("blake3:{hex}!out")).is_none());
    }

    #[test]
    fn parse_rejects_bad_hex() {
        // "zz" is not hex.
        assert!(parse_drv_output_id(&format!("sha256:{}!out", "zz".repeat(32))).is_none());
    }

    #[test]
    fn parse_rejects_wrong_hash_length() {
        // 63 hex chars = 31.5 bytes — can't try_into [u8;32].
        let short = "a".repeat(63);
        assert!(parse_drv_output_id(&format!("sha256:{short}!out")).is_none());
        // 66 hex chars = 33 bytes — same problem.
        let long = "a".repeat(66);
        assert!(parse_drv_output_id(&format!("sha256:{long}!out")).is_none());
    }

    #[test]
    fn parse_rejects_empty_output_name() {
        // "sha256:...!" — valid prefix but nothing after !. Nix never
        // sends this (output names are required) but a buggy client might.
        let hex = "aa".repeat(32);
        assert!(parse_drv_output_id(&format!("sha256:{hex}!")).is_none());
    }
}
