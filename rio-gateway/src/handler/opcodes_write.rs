//! Write opcode handlers (add-to-store, add-text, add-multiple).

use rio_common::limits::MAX_NAR_SIZE;
use rio_nix::hash::{HashAlgo, NixHash};
use rio_nix::nar::{self, NarNode};
use rio_nix::protocol::pathinfo;
use rio_nix::protocol::stderr::{StderrError, StderrWriter};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::types;
use rio_proto::validated::ValidatedPathInfo;
use tokio::io::{AsyncRead, AsyncReadExt, AsyncWrite};
use tokio::task::JoinSet;
use tracing::{Instrument, debug, instrument, warn};

use super::grpc::{grpc_put_path, grpc_put_path_streaming};
use super::{GatewayError, PROGRAM_NAME, SessionContext};
use crate::drv_cache::{DRV_NAR_BUFFER_LIMIT, try_cache_drv};

// r[impl gw.wire.framed-max-total+2]
// Guard against future drift: if MAX_NAR_SIZE is bumped without
// raising MAX_FRAMED_TOTAL, compilation fails. The wopAddToStoreNar
// handler gates on nar_size ≤ MAX_NAR_SIZE before constructing the
// FramedStreamReader; if MAX_FRAMED_TOTAL < MAX_NAR_SIZE, the
// reader's internal clamp silently shrinks the effective limit and
// NARs between the two bounds fail mid-stream with a confusing
// framed-total error instead of the upfront size-gate message.
const _: () = assert!(
    wire::MAX_FRAMED_TOTAL >= MAX_NAR_SIZE,
    "MAX_FRAMED_TOTAL must be >= MAX_NAR_SIZE so the nar_size gate is effective"
);

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

// r[impl gw.opcode.add-to-store-nar+2]
// r[impl gw.opcode.add-to-store-nar.framing+2]
/// wopAddToStoreNar (39): Receive a store path with NAR content via framed stream.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_add_to_store_nar<R: AsyncRead + Unpin + Send, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt_token.as_deref();
    let service_signer = ctx.service_signer.as_deref();
    let drv_cache = &mut ctx.drv_cache;
    let EntryHead {
        path,
        path_str,
        info,
        nar_size,
        nar_hash_bytes,
    } = match read_entry_head(reader).await {
        Ok(h) => h,
        Err(e) => stderr_err!(stderr, "wopAddToStoreNar: {e}"),
    };
    let _repair = wire::read_bool(reader).await?;
    let _dont_check_sigs = wire::read_bool(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
    debug!(path = %path_str, nar_size, "wopAddToStoreNar");

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
        if let Err(e) = grpc_put_path(store_client, jwt_token, service_signer, info, nar_data).await
        {
            stderr_err!(stderr, "store error: {e}");
        }
    } else {
        if path.is_derivation() {
            warn!(
                path = %path, nar_size,
                "oversize .drv NAR — streaming (not cached; resolve_derivation fetches from store later)"
            );
        }
        if let Err(e) = grpc_put_path_streaming(
            store_client,
            jwt_token,
            service_signer,
            info,
            &mut framed,
            nar_size,
            nar_hash_bytes,
        )
        .await
        {
            stderr_err!(stderr, "store error: {e}");
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

/// Max NAR size to buffer for pipelined PutPath. Entries above this drain
/// the in-flight set and stream synchronously. Reuses [`DRV_NAR_BUFFER_LIMIT`]
/// — 16 MiB covers all `.drv` files plus typical sources (patches, scripts).
/// At [`ADD_MULTIPLE_PIPELINE_DEPTH`] in flight × 16 MiB = 512 MiB worst-case
/// buffered. Typical entries are KB-range so the real footprint is tiny.
const ADD_MULTIPLE_PIPELINE_BUFFER: u64 = DRV_NAR_BUFFER_LIMIT;

/// Max in-flight PutPath calls per `wopAddMultipleToStore` stream. The wire
/// read stays sequential (entry N+1's metadata follows N's NAR bytes); only
/// the store-side gRPC calls overlap. Store p50 ≈ 25ms — 32-way pipeline
/// brings a 45k-entry closure from ~31min sequential to ~1min.
const ADD_MULTIPLE_PIPELINE_DEPTH: usize = 32;

/// One entry's wire metadata, parsed and validated. NAR bytes still on the wire.
struct EntryHead {
    path: StorePath,
    path_str: String,
    info: ValidatedPathInfo,
    nar_size: u64,
    nar_hash_bytes: Vec<u8>,
}

/// Read and validate one entry's metadata from the wire.
///
/// Shared by two opcodes that send the same `path` + [`ValidPathInfo`]
/// head:
///
/// - `wopAddToStoreNar` (39): reads from the OUTER stream; the NAR
///   payload that follows is FRAMED (`FramedStreamReader`).
/// - `wopAddMultipleToStore` (44): reads from inside an
///   already-framed stream; NAR payload is `narSize` plain bytes
///   (NOT nested-framed — `addToStore(info, source)` reads directly
///   from the already-framed outer stream).
///
/// Wire format (`ValidPathInfo::read` in store-api.cc, protocol ≥16):
///   path: string
///   [`ValidPathInfo`] (8 fields — see `rio_nix::protocol::pathinfo`)
///   NAR: see per-opcode note above
///
/// Returns with the NAR bytes still unread — caller decides buffer vs stream.
///
/// [`ValidPathInfo`]: rio_nix::protocol::pathinfo::ValidPathInfo
async fn read_entry_head<R: AsyncRead + Unpin>(framed: &mut R) -> anyhow::Result<EntryHead> {
    let path_str = wire::read_string(framed).await?;
    let body = pathinfo::read_valid_path_info(framed).await?;

    debug!(path = %path_str, nar_size = body.nar_size, "read PathInfo wire head");

    let path = StorePath::parse(&path_str).map_err(|e| GatewayError::InvalidStorePath {
        path: path_str.clone(),
        source: e,
    })?;

    if body.nar_size > MAX_NAR_SIZE {
        return Err(GatewayError::NarTooLarge {
            context: format!("entry '{path_str}'"),
            got: body.nar_size,
            max: MAX_NAR_SIZE,
        }
        .into());
    }

    let nar_size = body.nar_size;
    let nar_hash_bytes = body.nar_hash.clone();
    let raw_info = types::PathInfo {
        store_path: path_str.clone(),
        store_path_hash: Vec::new(),
        deriver: body.deriver.unwrap_or_default(),
        nar_hash: body.nar_hash,
        nar_size: body.nar_size,
        references: body.references,
        registration_time: body.registration_time,
        ultimate: body.ultimate,
        signatures: body.signatures,
        content_address: body.content_address.unwrap_or_default(),
    };
    let info =
        ValidatedPathInfo::try_from(raw_info).map_err(|e| GatewayError::InvalidPathInfo {
            context: format!("entry '{path_str}'"),
            source: e,
        })?;

    Ok(EntryHead {
        path,
        path_str,
        info,
        nar_size,
        nar_hash_bytes,
    })
}

/// Drain a JoinSet of `(index, Result)` pairs, returning the lowest-indexed
/// error if any. With pipelining, completion order is nondeterministic; the
/// lowest index is what sequential processing would have reported first.
async fn drain_put_tasks(
    tasks: &mut JoinSet<(u64, anyhow::Result<()>)>,
) -> Option<(u64, anyhow::Error)> {
    let mut first: Option<(u64, anyhow::Error)> = None;
    while let Some(joined) = tasks.join_next().await {
        match joined {
            Ok((idx, Err(e))) => {
                if first.as_ref().is_none_or(|(j, _)| idx < *j) {
                    first = Some((idx, e));
                }
            }
            Ok((_, Ok(()))) => {}
            Err(je) => {
                // Cancelled or panicked. Report at u64::MAX so any real
                // wire-index error wins lowest-index. Shouldn't happen in
                // practice — we never call abort, and grpc_put_path doesn't
                // panic on store errors.
                if first.is_none() {
                    first = Some((u64::MAX, anyhow::anyhow!("PutPath task join: {je}")));
                }
            }
        }
    }
    first
}

// r[impl gw.opcode.mandatory-set]
/// wopAddToStore (7): Legacy content-addressed store path import.
#[instrument(skip_all, fields(name = tracing::field::Empty))]
pub(super) async fn handle_add_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt_token.as_deref();
    let service_signer = ctx.service_signer.as_deref();
    let drv_cache = &mut ctx.drv_cache;
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
        match StorePath::make_fixed_output(&name, &content_hash, is_recursive, &ref_paths) {
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

    if let Err(e) = grpc_put_path(store_client, jwt_token, service_signer, info, nar_data).await {
        stderr_err!(stderr, "store error: {e}");
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_string(w, path.as_str()).await?;
    pathinfo::write_valid_path_info(
        w,
        &pathinfo::ValidPathInfo {
            deriver: None,
            nar_hash: nar_hash.digest().to_vec(),
            references,
            registration_time: 0,
            nar_size,
            ultimate: true,
            signatures: Vec::new(),
            content_address: Some(ca),
        },
    )
    .await?;

    Ok(())
}

// r[impl gw.opcode.mandatory-set]
/// wopAddTextToStore (8): Legacy text file import.
#[instrument(skip_all, fields(name = tracing::field::Empty))]
pub(super) async fn handle_add_text_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt_token.as_deref();
    let service_signer = ctx.service_signer.as_deref();
    let drv_cache = &mut ctx.drv_cache;
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

    if let Err(e) = grpc_put_path(store_client, jwt_token, service_signer, info, nar_data).await {
        stderr_err!(stderr, "store error: {e}");
    }

    stderr.finish().await?;
    wire::write_string(stderr.inner_mut(), path.as_str()).await?;

    Ok(())
}

/// Parse a content-address method string. Errors are formatted for the
/// Nix client (sole caller `stderr_err!`s the result; nothing matches on
/// variants).
fn parse_cam_str(cam_str: &str) -> Result<(bool, bool, HashAlgo), String> {
    let algo = |s: &str| s.parse::<HashAlgo>().map_err(|e| e.to_string());
    if let Some(s) = cam_str.strip_prefix("text:") {
        Ok((true, false, algo(s)?))
    } else if let Some(rest) = cam_str.strip_prefix("fixed:") {
        if let Some(s) = rest
            .strip_prefix("r:")
            .or_else(|| rest.strip_prefix("git:"))
        {
            Ok((false, true, algo(s)?))
        } else {
            Ok((false, false, algo(rest)?))
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
///       ValidPathInfo (9 fields — see stream_one_entry)
///       NAR data (narSize plain bytes, NOT nested-framed)
///   ]
// r[impl gw.opcode.add-multiple.batch+2]
// r[impl gw.opcode.add-multiple.unaligned-frames]
// r[impl gw.opcode.add-multiple.dont-check-sigs-ignored]
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_add_multiple_to_store<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let service_signer = ctx.service_signer.clone();
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt_token.as_deref();
    let drv_cache = &mut ctx.drv_cache;
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

    // Pipeline: wire-read sequential (entry N+1's metadata follows N's NAR
    // bytes — can't read ahead), store-call concurrent. The store does NOT
    // validate references at PutPath time (rio-store/src/metadata/inline.rs:
    // refs go into a `text[]` column, no FK, no existence check — they're
    // walked by GC mark, not validated at insert), so reordering is safe
    // even though Nix sends entries in topological order.
    //
    // I-052: live-measured ~20 paths/sec sequential (store p50=25ms). At
    // 45k entries that's ~31 minutes before any build starts. 32-way
    // overlap targets ~1 minute.
    let mut tasks: JoinSet<(u64, anyhow::Result<()>)> = JoinSet::new();
    let jwt_owned: Option<String> = jwt_token.map(str::to_owned);
    let span = tracing::Span::current();

    let mut fail: Option<(u64, anyhow::Error)> = None;
    for i in 0..num_paths {
        // Wire reads stay on this task — strictly sequential.
        let head = match read_entry_head(&mut framed).await {
            Ok(h) => h,
            Err(e) => {
                fail = Some((i, e));
                break;
            }
        };

        if head.nar_size <= ADD_MULTIPLE_PIPELINE_BUFFER {
            // Buffer the NAR off the wire (sequential), then spawn the
            // store call. drv_cache write happens HERE before the spawn —
            // no shared-mutable concurrency.
            let mut nar_data = vec![0u8; head.nar_size as usize];
            if let Err(e) = framed.read_exact(&mut nar_data).await {
                fail = Some((
                    i,
                    GatewayError::NarRead {
                        context: format!("entry '{}'", head.path_str),
                        source: e,
                    }
                    .into(),
                ));
                break;
            }
            try_cache_drv(&head.path, &nar_data, drv_cache);

            // Backpressure: at depth, await one before spawning. If THAT
            // result is an error, stop reading — bounds wasted wire-reads
            // to ~PIPELINE_DEPTH past the failing index.
            if tasks.len() >= ADD_MULTIPLE_PIPELINE_DEPTH
                && let Some(joined) = tasks.join_next().await
            {
                match joined {
                    Ok((idx, Err(e))) => {
                        fail = Some((idx, e));
                        break;
                    }
                    Err(je) => {
                        fail = Some((u64::MAX, anyhow::anyhow!("PutPath task join: {je}")));
                        break;
                    }
                    Ok((_, Ok(()))) => {}
                }
            }

            let mut client = store_client.clone();
            let jwt = jwt_owned.clone();
            let svc = service_signer.clone();
            let path_str = head.path_str;
            let info = head.info;
            tasks.spawn(
                async move {
                    let r =
                        grpc_put_path(&mut client, jwt.as_deref(), svc.as_deref(), info, nar_data)
                            .await
                            .map(|_| ())
                            .map_err(|e| {
                                GatewayError::Store(format!("entry '{path_str}': {e}")).into()
                            });
                    (i, r)
                }
                .instrument(span.clone()),
            );
        } else {
            // Oversize: drain in-flight first (preserves the "all prior
            // entries committed before this one starts" property the
            // sequential code had — not strictly required for refs, but
            // keeps memory bounded), then stream synchronously.
            if let Some(f) = drain_put_tasks(&mut tasks).await {
                fail = Some(f);
                break;
            }
            if head.path.is_derivation() {
                warn!(
                    path = %head.path, nar_size = head.nar_size,
                    "oversize .drv NAR — streaming (not cached; resolve_derivation fetches from store later)"
                );
            }
            if let Err(e) = grpc_put_path_streaming(
                store_client,
                jwt_token,
                service_signer.as_deref(),
                head.info,
                &mut framed,
                head.nar_size,
                head.nar_hash_bytes,
            )
            .await
            {
                fail = Some((
                    i,
                    GatewayError::Store(format!("entry '{}': {e}", head.path_str)).into(),
                ));
                break;
            }
        }
    }

    // Drain remaining in-flight. If a store-call error here is lower-index
    // than a wire-read error from the loop, the store-call error wins —
    // that's what sequential processing would have reported first.
    if let Some((idx, e)) = drain_put_tasks(&mut tasks).await
        && fail.as_ref().is_none_or(|(j, _)| idx < *j)
    {
        fail = Some((idx, e));
    }

    if let Some((idx, e)) = fail {
        stderr
            .error(&StderrError::simple(
                PROGRAM_NAME,
                format!("wopAddMultipleToStore entry {idx}/{num_paths} failed: {e}"),
            ))
            .await?;
        return Err(e);
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
