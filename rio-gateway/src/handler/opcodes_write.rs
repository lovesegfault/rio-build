//! Write opcode handlers (add-to-store, add-text, add-multiple).

use super::*;
use rio_proto::validated::ValidatedPathInfo;
use tokio::io::AsyncReadExt;

/// Build a `ValidatedPathInfo` for a freshly-computed path (AddToStore/AddTextToStore).
/// Uses defaults for fields not provided by the wire: deriver=None,
/// registration_time=0, ultimate=true, signatures=[].
///
/// Takes pre-parsed `StorePath` and `Vec<StorePath>` references — callers
/// already parse both for their own validation, so re-parsing from strings
/// here would be redundant.
fn path_info_for_computed(
    store_path: StorePath,
    nar_hash: [u8; 32],
    nar_size: u64,
    references: Vec<StorePath>,
    content_address: String,
) -> ValidatedPathInfo {
    ValidatedPathInfo {
        store_path,
        store_path_hash: Vec::new(),
        deriver: None,
        nar_hash,
        nar_size,
        references,
        registration_time: 0,
        ultimate: true,
        signatures: Vec::new(),
        content_address: Some(content_address),
    }
}

/// Parse reference path strings into `StorePath`s, formatting the first error with context.
fn parse_reference_paths(refs: &[String], context: &str) -> Result<Vec<StorePath>, GatewayError> {
    refs.iter()
        .map(|s| {
            StorePath::parse(s).map_err(|e| GatewayError::InvalidReference {
                path: s.clone(),
                context: context.to_string(),
                source: e,
            })
        })
        .collect()
}

// r[impl gw.opcode.add-to-store-nar]
// r[impl gw.opcode.add-to-store-nar.framing]
// r[impl gw.wire.narhash-hex]
/// wopAddToStoreNar (39): Receive a store path with NAR content via framed stream.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_add_to_store_nar<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin>(
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
    tracing::Span::current().record("path", path_str.as_str());

    debug!(path = %path_str, nar_size = nar_size, "wopAddToStoreNar");

    if nar_size > rio_common::limits::MAX_NAR_SIZE {
        stderr_err!(stderr, "nar_size {nar_size} exceeds maximum for {path_str}");
    }

    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "invalid store path '{path_str}': {e}"),
    };

    let nar_hash_bytes = match hex::decode(&nar_hash_str) {
        Ok(b) => b,
        Err(e) => stderr_err!(stderr, "invalid narHash hex '{nar_hash_str}': {e}"),
    };

    // Build raw PathInfo and validate via TryFrom. This catches:
    //   - bad reference paths (wire gives us unvalidated strings)
    //   - nar_hash wrong length (hex-decode succeeded but result isn't 32 bytes)
    // We already parsed `path` above (it's a valid StorePath), so the
    // store_path field can't fail — we pass the string form and re-parse
    // inside TryFrom for code uniformity rather than constructing
    // ValidatedPathInfo piecewise here.
    let raw_info = types::PathInfo {
        store_path: path_str.clone(),
        store_path_hash: Vec::new(),
        deriver: deriver_str,
        nar_hash: nar_hash_bytes.clone(),
        nar_size,
        references,
        registration_time,
        ultimate,
        signatures: sigs,
        content_address: ca_str,
    };
    let info = match ValidatedPathInfo::try_from(raw_info) {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "wopAddToStoreNar for '{path_str}': {e}"),
    };

    // Wrap reader in FramedStreamReader for the NAR bytes. max_total =
    // nar_size (client-declared). MAX_FRAMED_TOTAL == MAX_NAR_SIZE, so
    // the nar_size check above is the effective gate; this clamp is
    // defense-in-depth. A lying client sending more than declared trips
    // the reader's limit.
    // After this point, ANY early return leaves the outer reader mid-frame
    // — caller MUST drop the connection (which it does: stderr_err! → Err
    // → session loop aborts).
    let mut framed = wire::FramedStreamReader::new(&mut *reader, nar_size);

    // .drv files are small (typically <10KB, max ~10MB observed). Buffer
    // them so try_cache_drv can parse the ATerm. Everything else streams.
    if path.is_derivation() && nar_size <= DRV_NAR_BUFFER_LIMIT {
        let mut nar_data = vec![0u8; nar_size as usize];
        if let Err(e) = framed.read_exact(&mut nar_data).await {
            stderr_err!(stderr, "failed to read framed NAR for '{path_str}': {e}");
        }
        try_cache_drv(&path, &nar_data, drv_cache);
        if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
            return send_store_error(stderr, e).await;
        }
    } else {
        if path.is_derivation() {
            warn!(
                path = %path, nar_size,
                "oversize .drv NAR — streaming (not cached; resolve_derivation fetches from store later)"
            );
        }
        if let Err(e) =
            grpc_put_path_streaming(store_client, info, &mut framed, nar_size, nar_hash_bytes).await
        {
            return send_store_error(stderr, e).await;
        }
    }

    // Drain to sentinel. After nar_size bytes, only the u64(0) frame
    // terminator should remain — FramedStreamReader consumes it on the
    // next read attempt and returns EOF (0 bytes).
    let mut probe = [0u8; 1];
    match framed.read(&mut probe).await {
        Ok(0) => {}
        Ok(_) => stderr_err!(
            stderr,
            "wopAddToStoreNar: trailing data after NAR for '{path_str}'"
        ),
        Err(e) => stderr_err!(stderr, "wopAddToStoreNar: frame sentinel read: {e}"),
    }

    stderr.finish().await?;
    Ok(())
}

/// Stream a single entry from the wopAddMultipleToStore framed stream.
///
/// Wire format (per Nix `Store::addMultipleToStore(Source &, ...)` in
/// store-api.cc, called with protocol version 16):
///   path: string
///   deriver: string (empty if none)
///   narHash: string (hex — `Hash::parseAny(.., SHA256)`)
///   references: [string]
///   registrationTime: u64
///   narSize: u64
///   ultimate: bool
///   sigs: [string]
///   ca: string (empty if none)
///   NAR: narSize plain bytes (NOT framed — `addToStore(info, source)` reads
///        narSize bytes directly from the already-framed outer stream)
async fn stream_one_entry<R: AsyncRead + Unpin>(
    framed: &mut R,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(framed).await?;
    let deriver_str = wire::read_string(framed).await?;
    let nar_hash_str = wire::read_string(framed).await?;
    let references = wire::read_strings(framed).await?;
    let registration_time = wire::read_u64(framed).await?;
    let nar_size = wire::read_u64(framed).await?;
    let ultimate = wire::read_bool(framed).await?;
    let sigs = wire::read_strings(framed).await?;
    let ca_str = wire::read_string(framed).await?;

    debug!(path = %path_str, nar_size, "wopAddMultipleToStore entry");

    let path = StorePath::parse(&path_str).map_err(|e| GatewayError::InvalidStorePath {
        path: path_str.clone(),
        source: e,
    })?;

    let nar_hash_bytes = hex::decode(&nar_hash_str).map_err(|e| GatewayError::InvalidHex {
        context: format!("entry '{path_str}' narHash"),
        source: e,
    })?;

    if nar_size > rio_common::limits::MAX_NAR_SIZE {
        return Err(GatewayError::NarTooLarge {
            context: format!("entry '{path_str}'"),
            got: nar_size,
            max: rio_common::limits::MAX_NAR_SIZE,
        }
        .into());
    }

    let raw_info = types::PathInfo {
        store_path: path_str.clone(),
        store_path_hash: Vec::new(),
        deriver: deriver_str,
        nar_hash: nar_hash_bytes.clone(),
        nar_size,
        references,
        registration_time,
        ultimate,
        signatures: sigs,
        content_address: ca_str,
    };
    let info =
        ValidatedPathInfo::try_from(raw_info).map_err(|e| GatewayError::InvalidPathInfo {
            context: format!("entry '{path_str}'"),
            source: e,
        })?;

    // .drv files are small (typically <10KB, max ~10MB observed). Buffer
    // them so try_cache_drv can parse the ATerm. Everything else streams.
    if path.is_derivation() && nar_size <= DRV_NAR_BUFFER_LIMIT {
        let mut nar_data = vec![0u8; nar_size as usize];
        framed
            .read_exact(&mut nar_data)
            .await
            .map_err(|e| GatewayError::NarRead {
                context: format!("entry '{path_str}'"),
                source: e,
            })?;
        try_cache_drv(&path, &nar_data, drv_cache);
        grpc_put_path(store_client, info, nar_data)
            .await
            .map_err(|e| GatewayError::Store(format!("entry '{path_str}': {e}")))?;
    } else {
        if path.is_derivation() {
            warn!(
                path = %path, nar_size,
                "oversize .drv NAR — streaming (not cached; resolve_derivation fetches from store later)"
            );
        }
        grpc_put_path_streaming(store_client, info, framed, nar_size, nar_hash_bytes)
            .await
            .map_err(|e| GatewayError::Store(format!("entry '{path_str}': {e}")))?;
    }

    Ok(())
}

// r[impl gw.opcode.mandatory-set]
/// wopAddToStore (7): Legacy content-addressed store path import.
#[instrument(skip_all, fields(name = tracing::field::Empty))]
pub(super) async fn handle_add_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let name = wire::read_string(reader).await?;
    let cam_str = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;
    let _repair = wire::read_bool(reader).await?;
    tracing::Span::current().record("name", name.as_str());

    debug!(name = %name, cam_str = %cam_str, "wopAddToStore");

    let dump_data = match wire::read_framed_stream(reader).await {
        Ok(data) => data,
        Err(e) => stderr_err!(stderr, "failed to read dump data for '{name}': {e}"),
    };

    let (is_text, is_recursive, hash_algo) = match parse_cam_str(&cam_str) {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "invalid content-address method '{cam_str}': {e}"),
    };

    let content_hash = NixHash::compute(hash_algo, &dump_data);

    let ref_paths = match parse_reference_paths(&references, "wopAddToStore") {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "{e}"),
    };

    let path = if is_text {
        match StorePath::make_text(&name, &content_hash, &ref_paths) {
            Ok(p) => p,
            Err(e) => stderr_err!(
                stderr,
                "failed to compute text store path for '{name}': {e}"
            ),
        }
    } else {
        match StorePath::make_fixed_output(&name, &content_hash, is_recursive) {
            Ok(p) => p,
            Err(e) => stderr_err!(
                stderr,
                "failed to compute fixed-output store path for '{name}': {e}"
            ),
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
            stderr_err!(stderr, "failed to serialize NAR for '{name}': {e}");
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

    // nar_hash is SHA-256 -> exactly 32 bytes. The try_into cannot fail in
    // practice (NixHash::compute(SHA256, ..) always yields 32 bytes) but we
    // check anyway rather than .unwrap() on a security-relevant field.
    let nar_hash_32: [u8; 32] = match nar_hash.digest().try_into() {
        Ok(h) => h,
        Err(_) => stderr_err!(stderr, "internal: SHA-256 digest wrong length"),
    };
    let info = path_info_for_computed(path.clone(), nar_hash_32, nar_size, ref_paths, ca.clone());

    if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_string(w, path.as_str()).await?;
    wire::write_string(w, "").await?;
    wire::write_string(w, &nar_hash.to_hex()).await?;
    wire::write_strings(w, &references).await?;
    wire::write_u64(w, 0).await?;
    wire::write_u64(w, nar_size).await?;
    wire::write_bool(w, true).await?;
    wire::write_strings(w, wire::NO_STRINGS).await?;
    wire::write_string(w, &ca).await?;

    Ok(())
}

// r[impl gw.opcode.mandatory-set]
/// wopAddTextToStore (8): Legacy text file import.
#[instrument(skip_all, fields(name = tracing::field::Empty))]
pub(super) async fn handle_add_text_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let name = wire::read_string(reader).await?;
    let text = wire::read_string(reader).await?;
    let references = wire::read_strings(reader).await?;
    tracing::Span::current().record("name", name.as_str());

    debug!(name = %name, text_len = text.len(), "wopAddTextToStore");

    let content_hash = NixHash::compute(HashAlgo::SHA256, text.as_bytes());

    let ref_paths = match parse_reference_paths(&references, "wopAddTextToStore") {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "{e}"),
    };

    let path = match StorePath::make_text(&name, &content_hash, &ref_paths) {
        Ok(p) => p,
        Err(e) => stderr_err!(
            stderr,
            "failed to compute text store path for '{name}': {e}"
        ),
    };

    let node = NarNode::Regular {
        executable: false,
        contents: text.into_bytes(),
    };
    let mut nar_data = Vec::new();
    if let Err(e) = nar::serialize(&mut nar_data, &node) {
        stderr_err!(stderr, "failed to serialize NAR for '{name}': {e}");
    }

    let nar_hash = NixHash::compute(HashAlgo::SHA256, &nar_data);
    let nar_size = nar_data.len() as u64;

    let ca = format!(
        "text:sha256:{}",
        rio_nix::store_path::nixbase32::encode(content_hash.digest())
    );

    try_cache_drv(&path, &nar_data, drv_cache);

    let nar_hash_32: [u8; 32] = match nar_hash.digest().try_into() {
        Ok(h) => h,
        Err(_) => stderr_err!(stderr, "internal: SHA-256 digest wrong length"),
    };
    let info = path_info_for_computed(path.clone(), nar_hash_32, nar_size, ref_paths, ca);

    if let Err(e) = grpc_put_path(store_client, info, nar_data).await {
        return send_store_error(stderr, e).await;
    }

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), path.as_str()).await?;

    Ok(())
}

/// Error from [`parse_cam_str`].
#[derive(Debug, thiserror::Error)]
enum CamParseError {
    #[error("unrecognized content-address method: {0}")]
    UnknownMethod(String),
    #[error(transparent)]
    InvalidAlgo(#[from] rio_nix::hash::HashError),
}

/// Parse a content-address method string.
fn parse_cam_str(cam_str: &str) -> Result<(bool, bool, HashAlgo), CamParseError> {
    if let Some(algo_str) = cam_str.strip_prefix("text:") {
        Ok((true, false, algo_str.parse()?))
    } else if let Some(rest) = cam_str.strip_prefix("fixed:") {
        if let Some(algo_str) = rest.strip_prefix("r:") {
            Ok((false, true, algo_str.parse()?))
        } else if let Some(algo_str) = rest.strip_prefix("git:") {
            Ok((false, true, algo_str.parse()?))
        } else {
            Ok((false, false, rest.parse()?))
        }
    } else {
        Err(CamParseError::UnknownMethod(cam_str.to_string()))
    }
}

/// wopAddMultipleToStore (44): Receive multiple store paths via framed stream.
///
/// Wire format (per Nix `daemon.cc` case `AddMultipleToStore`):
///   repair: bool
///   dontCheckSigs: bool
///   [framed stream (chunked, terminated by 0-length chunk):
///     num_paths: u64      ← count prefix INSIDE the framed stream
///     for i in 0..num_paths:
///       ValidPathInfo (9 fields — see stream_one_entry)
///       NAR data (narSize plain bytes, NOT nested-framed)
///   ]
// r[impl gw.opcode.add-multiple.batch]
// r[impl gw.opcode.add-multiple.unaligned-frames]
// r[impl gw.opcode.add-multiple.dont-check-sigs-ignored]
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_add_multiple_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    store_client: &mut StoreServiceClient<Channel>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<()> {
    let _repair = wire::read_bool(reader).await?;
    let _dont_check_sigs = wire::read_bool(reader).await?;

    debug!("wopAddMultipleToStore (streaming)");

    // FramedStreamReader de-frames on the fly. All wire::read_* primitives
    // work on it directly (they take R: AsyncRead). After this point, ANY
    // early return leaves the outer reader mid-frame — caller MUST drop the
    // connection (which it does: stderr_err! → Err → session loop aborts).
    let mut framed = wire::FramedStreamReader::new(&mut *reader, wire::MAX_FRAMED_TOTAL);

    // Count prefix: Nix `Store::addMultipleToStore(Source &)` reads this
    // first (`readNum<uint64_t>(source)`) before the per-entry loop.
    let num_paths = match wire::read_u64(&mut framed).await {
        Ok(n) => n,
        Err(e) => stderr_err!(
            stderr,
            "wopAddMultipleToStore: missing num_paths prefix: {e}"
        ),
    };
    if num_paths > wire::MAX_COLLECTION_COUNT {
        stderr_err!(
            stderr,
            "wopAddMultipleToStore: num_paths {num_paths} exceeds MAX_COLLECTION_COUNT {}",
            wire::MAX_COLLECTION_COUNT
        );
    }

    tracing::Span::current().record("count", num_paths);
    debug!(num_paths, "wopAddMultipleToStore: processing entries");

    for i in 0..num_paths {
        if let Err(e) = stream_one_entry(&mut framed, store_client, drv_cache).await {
            stderr
                .error(&StderrError::simple(
                    PROGRAM_NAME,
                    format!("wopAddMultipleToStore entry {i}/{num_paths} failed: {e}"),
                ))
                .await?;
            return Err(e);
        }
    }

    // Drain to sentinel. After num_paths entries, only the u64(0) frame
    // terminator should remain — FramedStreamReader consumes it on the
    // next read attempt and returns EOF (0 bytes). If there's UNEXPECTED
    // data, the client sent more than num_paths entries claimed.
    let mut probe = [0u8; 1];
    match framed.read(&mut probe).await {
        Ok(0) => {}
        Ok(_) => stderr_err!(
            stderr,
            "wopAddMultipleToStore: trailing data after {num_paths} entries"
        ),
        Err(e) => stderr_err!(stderr, "wopAddMultipleToStore: frame sentinel read: {e}"),
    }

    stderr.finish().await?;
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::parse_cam_str;
    use rio_nix::hash::HashAlgo;

    #[test]
    fn test_parse_cam_str_text_sha256() -> anyhow::Result<()> {
        let (is_text, is_recursive, algo) = parse_cam_str("text:sha256").unwrap();
        assert!(is_text);
        assert!(!is_recursive);
        assert_eq!(algo, HashAlgo::SHA256);
        Ok(())
    }

    #[test]
    fn test_parse_cam_str_fixed_recursive_sha256() -> anyhow::Result<()> {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:r:sha256").unwrap();
        assert!(!is_text);
        assert!(is_recursive);
        assert_eq!(algo, HashAlgo::SHA256);
        Ok(())
    }

    #[test]
    fn test_parse_cam_str_fixed_git_sha1() -> anyhow::Result<()> {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:git:sha1").unwrap();
        assert!(!is_text);
        assert!(is_recursive, "git: should be treated as recursive");
        assert_eq!(algo, HashAlgo::SHA1);
        Ok(())
    }

    #[test]
    fn test_parse_cam_str_fixed_flat_sha256() -> anyhow::Result<()> {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:sha256").unwrap();
        assert!(!is_text);
        assert!(!is_recursive, "no r:/git: prefix should be flat");
        assert_eq!(algo, HashAlgo::SHA256);
        Ok(())
    }

    #[test]
    fn test_parse_cam_str_rejects_unknown_method() {
        assert!(parse_cam_str("bogus:sha256").is_err());
        assert!(parse_cam_str("").is_err());
        assert!(parse_cam_str("sha256").is_err()); // missing method prefix
    }

    #[test]
    fn test_parse_cam_str_rejects_unknown_algo() {
        assert!(parse_cam_str("text:md5").is_err());
        assert!(parse_cam_str("fixed:r:md5").is_err());
        assert!(parse_cam_str("fixed:blake2b").is_err());
    }
}
