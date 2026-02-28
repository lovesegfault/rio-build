//! Write opcode handlers (add-to-store, add-text, add-multiple).

use super::*;

/// wopAddToStoreNar (39): Receive a store path with NAR content via framed stream.
#[instrument(skip_all)]
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

    debug!(path = %path_str, nar_size = nar_size, "wopAddToStoreNar");

    if nar_size > wire::MAX_FRAMED_TOTAL {
        stderr_err!(stderr, "nar_size {nar_size} exceeds maximum for {path_str}");
    }

    // Validate store path
    let path = match StorePath::parse(&path_str) {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "invalid store path '{path_str}': {e}"),
    };

    // Validate narHash hex
    let nar_hash_bytes = match hex::decode(&nar_hash_str) {
        Ok(b) => b,
        Err(e) => stderr_err!(stderr, "invalid narHash hex '{nar_hash_str}': {e}"),
    };

    // Read NAR data via framed stream
    let nar_data = match wire::read_framed_stream(reader).await {
        Ok(data) => data,
        Err(e) => stderr_err!(stderr, "failed to read framed NAR for '{path_str}': {e}"),
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

    // Read NAR data as narSize plain bytes (NOT a nested framed stream).
    // The outer framed stream has already been reassembled; Nix's
    // `addToStore(info, source)` reads narSize bytes directly.
    if nar_size > rio_common::limits::MAX_NAR_SIZE {
        return Err(anyhow::anyhow!(
            "entry '{path_str}': nar_size {nar_size} exceeds MAX_NAR_SIZE {}",
            rio_common::limits::MAX_NAR_SIZE
        ));
    }
    let nar_size_usize = usize::try_from(nar_size)
        .map_err(|_| anyhow::anyhow!("entry '{path_str}': nar_size {nar_size} overflows usize"))?;
    let pos = cursor.position() as usize;
    let buf = cursor.get_ref();
    let end = pos.checked_add(nar_size_usize).ok_or_else(|| {
        anyhow::anyhow!("entry '{path_str}': nar_size {nar_size} overflows offset")
    })?;
    if end > buf.len() {
        return Err(anyhow::anyhow!(
            "entry '{path_str}': truncated NAR (expected {nar_size} bytes, {} remaining)",
            buf.len().saturating_sub(pos)
        ));
    }
    let nar_data = buf[pos..end].to_vec();
    cursor.set_position(end as u64);

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

    let mut ref_paths = Vec::with_capacity(references.len());
    for s in &references {
        match StorePath::parse(s) {
            Ok(p) => ref_paths.push(p),
            Err(e) => stderr_err!(
                stderr,
                "invalid reference path '{s}' for wopAddToStore: {e}"
            ),
        }
    }

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
    wire::write_strings(w, wire::NO_STRINGS).await?;
    wire::write_string(w, &ca).await?;

    Ok(())
}

/// wopAddTextToStore (8): Legacy text file import.
#[instrument(skip_all)]
pub(super) async fn handle_add_text_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
                stderr_err!(
                    stderr,
                    "invalid reference path '{s}' for wopAddTextToStore: {e}"
                );
            }
        }
    }

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
///
/// Wire format (per Nix `daemon.cc` case `AddMultipleToStore`):
///   repair: bool
///   dontCheckSigs: bool
///   [framed stream (chunked, terminated by 0-length chunk):
///     num_paths: u64      ← count prefix INSIDE the framed stream
///     for i in 0..num_paths:
///       ValidPathInfo (9 fields — see parse_add_multiple_entry)
///       NAR data (narSize plain bytes, NOT nested-framed)
///   ]
///
/// This was previously parsed WRONG (no count prefix, NAR read as nested
/// framed). The bug was masked by a byte-level test written to match the
/// buggy parser rather than the spec — caught by the VM test running real
/// `nix copy --to ssh-ng://`.
#[instrument(skip_all)]
pub(super) async fn handle_add_multiple_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
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
        Err(e) => stderr_err!(
            stderr,
            "wopAddMultipleToStore: failed to read framed stream: {e}"
        ),
    };

    let mut cursor = std::io::Cursor::new(stream_data.as_slice());

    // Count prefix: Nix `Store::addMultipleToStore(Source &)` reads this
    // first (`readNum<uint64_t>(source)`) before the per-entry loop.
    let num_paths = match wire::read_u64(&mut cursor).await {
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

    debug!(
        num_paths = num_paths,
        "wopAddMultipleToStore: processing entries"
    );

    for _ in 0..num_paths {
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

#[cfg(test)]
mod tests {
    use super::parse_cam_str;
    use rio_nix::hash::HashAlgo;

    /// parse_cam_str: text:sha256 → (is_text=true, is_recursive=false, SHA256)
    #[test]
    fn test_parse_cam_str_text_sha256() {
        let (is_text, is_recursive, algo) = parse_cam_str("text:sha256").unwrap();
        assert!(is_text);
        assert!(!is_recursive);
        assert_eq!(algo, HashAlgo::SHA256);
    }

    /// parse_cam_str: fixed:r:sha256 → (is_text=false, is_recursive=true, SHA256)
    #[test]
    fn test_parse_cam_str_fixed_recursive_sha256() {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:r:sha256").unwrap();
        assert!(!is_text);
        assert!(is_recursive);
        assert_eq!(algo, HashAlgo::SHA256);
    }

    /// parse_cam_str: fixed:git:sha1 → (is_text=false, is_recursive=true, SHA1)
    /// git: prefix is treated as recursive (same as r:)
    #[test]
    fn test_parse_cam_str_fixed_git_sha1() {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:git:sha1").unwrap();
        assert!(!is_text);
        assert!(is_recursive, "git: should be treated as recursive");
        assert_eq!(algo, HashAlgo::SHA1);
    }

    /// parse_cam_str: fixed:sha256 (flat) → (is_text=false, is_recursive=false, SHA256)
    #[test]
    fn test_parse_cam_str_fixed_flat_sha256() {
        let (is_text, is_recursive, algo) = parse_cam_str("fixed:sha256").unwrap();
        assert!(!is_text);
        assert!(!is_recursive, "no r:/git: prefix should be flat");
        assert_eq!(algo, HashAlgo::SHA256);
    }

    /// parse_cam_str: unknown method → Err
    #[test]
    fn test_parse_cam_str_rejects_unknown_method() {
        assert!(parse_cam_str("bogus:sha256").is_err());
        assert!(parse_cam_str("").is_err());
        assert!(parse_cam_str("sha256").is_err()); // missing method prefix
    }

    /// parse_cam_str: unknown hash algo → Err
    #[test]
    fn test_parse_cam_str_rejects_unknown_algo() {
        assert!(parse_cam_str("text:md5").is_err());
        assert!(parse_cam_str("fixed:r:md5").is_err());
        assert!(parse_cam_str("fixed:blake2b").is_err());
    }
}
