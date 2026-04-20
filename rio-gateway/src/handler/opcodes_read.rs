//! Read-only opcode handlers (query, ensure, temp-root, options).

use std::collections::{HashMap, HashSet};

use rio_common::grpc::DEFAULT_GRPC_TIMEOUT;
use rio_nix::protocol::derived_path::{DerivedPath, OutputSpec};
use rio_nix::protocol::pathinfo;
use rio_nix::protocol::stderr::StderrWriter;
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::types;
use tokio::io::{AsyncRead, AsyncWrite};
use tracing::{debug, error, info, instrument, warn};

use super::grpc::{
    grpc_is_valid_path, grpc_query_path_info, grpc_query_realisation, resolve_floating_outputs,
};
use super::{PROGRAM_NAME, SessionContext, read_store_path, with_jwt};
use crate::drv_cache::resolve_derivation;

/// JWT for store lookups — but NOT for `.drv` paths.
///
/// `.drv` files are build INPUTS, not tenant-owned OUTPUTS. A `.drv`
/// uploaded via one context (e.g., `nix copy` with default key) then
/// checked via another (e.g., `nix build` with tenant key) has no
/// `path_tenants` row for the checking tenant → tenant-filtered
/// `QueryPathInfo` returns NotFound → client sees "not a valid store
/// path" for a `.drv` it just uploaded. Observed in vm-lifecycle-core
/// gc-sweep subtest.
///
/// Output paths (everything that isn't a `.drv`) keep tenant-scoped
/// visibility — `r[store.tenant.narinfo-filter]` applies there.
// r[impl gw.jwt.anon-drv-lookup]
fn jwt_unless_drv<'a>(jwt_token: Option<&'a str>, path: &StorePath) -> Option<&'a str> {
    if path.is_derivation() {
        None
    } else {
        jwt_token
    }
}

// r[impl gw.opcode.is-valid-path]
/// wopIsValidPath (1): Check if a store path exists.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_is_valid_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let path_str = wire::read_string(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
    debug!(path = %path_str, "wopIsValidPath");

    let valid = match StorePath::parse(&path_str) {
        Ok(path) => {
            let jwt = jwt_unless_drv(jwt_token, &path);
            match grpc_is_valid_path(store_client, jwt, &path).await {
                Ok(v) => v,
                Err(e) => stderr_err!(stderr, "store error: {e}"),
            }
        }
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
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_ensure_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let path_str = wire::read_string(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
    debug!(path = %path_str, "wopEnsurePath");

    if let Ok(path) = StorePath::parse(&path_str).inspect_err(|e| {
        debug!(path = %path_str, error = %e, "wopEnsurePath: unparseable store path");
    }) {
        let jwt = jwt_unless_drv(jwt_token, &path);
        match grpc_is_valid_path(store_client, jwt, &path).await {
            Ok(true) => {}
            Ok(false) => {
                debug!(path = %path_str, "wopEnsurePath: path not in store (no substituters)");
            }
            Err(e) => {
                error!(path = %path_str, error = %e, "wopEnsurePath: store error");
                stderr_err!(stderr, "store error: error checking '{path_str}': {e}");
            }
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

// r[impl gw.opcode.query-path-info]
/// wopQueryPathInfo (26): Return full path metadata.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_query_path_info<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    // r[impl store.tenant.narinfo-filter]
    // JWT propagation: x-rio-tenant-token reaches store's QueryPathInfo
    // handler → tenant-scoped narinfo visibility gate. Without this,
    // the store sees anonymous → gate short-circuits → path invisible
    // even if the tenant's trusted_keys would admit it.
    let jwt_token = ctx.jwt.token();
    let path_str = wire::read_string(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
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

    let jwt = jwt_unless_drv(jwt_token, &path);
    let info = match grpc_query_path_info(store_client, jwt, path.as_str()).await {
        Ok(info) => info,
        Err(e) => stderr_err!(stderr, "store error: {e}"),
    };

    stderr.finish().await?;
    let w = stderr.inner_mut();

    match info {
        None => {
            wire::write_bool(w, false).await?;
        }
        Some(info) => {
            wire::write_bool(w, true).await?;
            pathinfo::write_valid_path_info(
                w,
                &pathinfo::ValidPathInfo {
                    // deriver: Option<StorePath> → Option<String>
                    deriver: info.deriver.map(|d| d.to_string()),
                    nar_hash: info.nar_hash.to_vec(),
                    // references: Vec<StorePath> — StorePath: AsRef<str>
                    references: info.references.iter().map(|r| r.to_string()).collect(),
                    registration_time: info.registration_time,
                    nar_size: info.nar_size,
                    ultimate: info.ultimate,
                    signatures: info.signatures,
                    content_address: info.content_address,
                },
            )
            .await?;
        }
    }

    Ok(())
}

// r[impl gw.opcode.query-valid-paths]
/// wopQueryValidPaths (31): Batch validity check.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_query_valid_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let path_strs = wire::read_strings(reader).await?;
    let _substitute = wire::read_bool(reader).await?;
    tracing::Span::current().record("count", path_strs.len());

    debug!(count = path_strs.len(), "wopQueryValidPaths");

    // Use FindMissingPaths and invert to get valid paths
    let req = with_jwt(
        types::FindMissingPathsRequest {
            store_paths: path_strs.clone(),
        },
        jwt_token,
    )?;
    let resp = rio_common::grpc::with_timeout(
        "FindMissingPaths",
        DEFAULT_GRPC_TIMEOUT,
        store_client.find_missing_paths(req),
    )
    .await;

    let missing_set: HashSet<String> = match resp {
        Ok(r) => r.into_inner().missing_paths.into_iter().collect(),
        Err(e) => stderr_err!(stderr, "store error: {e}"),
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
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_add_temp_root<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    _ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let path_str = wire::read_string(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
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
// r[impl gw.opcode.set-options.propagation+2]
/// wopSetOptions (19): Accept client build configuration.
///
/// UNREACHABLE VIA ssh-ng:// — Nix SSHStore overrides setOptions() with an
/// empty body (ssh-store.cc since 088ef8175, 2018). This handler fires only
/// for unix:// daemon-socket clients. The wire payload MUST still be drained
/// to keep the stream in sync (golden-conformance requirement); the values
/// are logged for diagnostics but otherwise discarded — `SubmitBuildRequest`
/// build options are reachable only via the gRPC path (rio-cli), not
/// `nix-build --option`. See the setoptions-unreachable VM subtest in
/// scheduling.nix.
#[instrument(skip_all)]
pub(super) async fn handle_set_options<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    _ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let _keep_failed = wire::read_bool(reader).await?;
    let _keep_going = wire::read_bool(reader).await?;
    let _try_fallback = wire::read_bool(reader).await?;
    let verbosity = wire::read_u64(reader).await?;
    let max_build_jobs = wire::read_u64(reader).await?;
    let max_silent_time = wire::read_u64(reader).await?;
    let _obsolete_use_build_hook = wire::read_u64(reader).await?;
    let _verbose_build = wire::read_bool(reader).await?;
    let _obsolete_log_type = wire::read_u64(reader).await?;
    let _obsolete_print_build_trace = wire::read_u64(reader).await?;
    let build_cores = wire::read_u64(reader).await?;
    let _use_substitutes = wire::read_bool(reader).await?;

    let overrides = wire::read_string_pairs(reader).await?;

    // Info (not debug) because the build-time knobs in here — max_silent_time,
    // build_cores, overrides — drive worker behavior. Diagnosing "why didn't
    // maxSilentTime fire" requires seeing this in journalctl, which is info+.
    // overrides truncated: can be large, but the first few are usually the
    // relevant ones (user --option args, not nix.conf noise).
    tracing::info!(
        verbosity,
        max_build_jobs,
        max_silent_time,
        build_cores,
        overrides_count = overrides.len(),
        overrides_head = ?overrides.iter().take(8).collect::<Vec<_>>(),
        "wopSetOptions"
    );

    stderr.finish().await?;
    Ok(())
}

// r[impl gw.opcode.nar-from-path]
// r[impl gw.opcode.nar-from-path.raw-bytes]
/// wopNarFromPath (38): Export path as raw NAR bytes AFTER STDERR_LAST.
///
/// Nix client: `processStderr(ex)` (no sink) → `copyNAR(from, sink)`.
/// The stderr loop exits on STDERR_LAST; the NAR is read as raw bytes after.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_nar_from_path<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let (path_str, path) = match read_store_path(reader).await {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "{e}"),
    };
    tracing::Span::current().record("path", path_str.as_str());
    debug!(path = %path_str, "wopNarFromPath");

    // Fetch the FULL NAR before sending STDERR_LAST. We can't stream
    // incrementally: a mid-stream gRPC error would leave the client's
    // copyNAR() with a truncated NAR and no way to signal the error (it's
    // already past the stderr loop). The Nix protocol has no framing for
    // this opcode — raw NAR bytes follow STDERR_LAST directly — so buffering
    // is the only correct behavior.
    let req = with_jwt(
        types::GetPathRequest {
            store_path: path.to_string(),
            manifest_hint: None,
        },
        jwt_unless_drv(jwt_token, &path),
    )?;
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

    // Per-chunk idle timeout: DEFAULT_GRPC_TIMEOUT above only bounds
    // obtaining the Streaming handle, not the per-message reads. h2
    // keepalive PINGs are answered transport-side regardless of
    // application progress, and OPCODE_IDLE_TIMEOUT covers only
    // inter-opcode reads — without this, a store stalled mid-stream
    // holds ≤4 GiB of buffered NAR + the SSH session indefinitely.
    let (_info, nar_data) = match rio_proto::client::collect_nar_stream(
        &mut stream,
        rio_common::limits::MAX_NAR_SIZE,
        Some(rio_common::grpc::GRPC_STREAM_TIMEOUT),
    )
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
#[instrument(skip_all, fields(hash_part = tracing::field::Empty))]
pub(super) async fn handle_query_path_from_hash_part<
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let hash_part = wire::read_string(reader).await?;
    tracing::Span::current().record("hash_part", hash_part.as_str());
    debug!(hash_part = %hash_part, "wopQueryPathFromHashPart");

    // The store validates hash_part (32 chars, nixbase32 charset) and
    // does the LIKE prefix query. NOT_FOUND → empty string to Nix;
    // other gRPC errors → STDERR_ERROR.
    let req = with_jwt(types::QueryPathFromHashPartRequest { hash_part }, jwt_token)?;
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
        Err(e) => stderr_err!(stderr, "store error: {e}"),
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
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_add_signatures<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let path_str = wire::read_string(reader).await?;
    let sigs = wire::read_strings(reader).await?;
    tracing::Span::current().record("path", path_str.as_str());
    debug!(path = %path_str, count = sigs.len(), "wopAddSignatures");

    // The store appends to narinfo.signatures TEXT[]. NOT_FOUND →
    // STDERR_ERROR (matches the real daemon: signing a path you don't
    // have is an error, not a silent no-op — `nix store sign` expects
    // to hear about it).
    let req = with_jwt(
        types::AddSignaturesRequest {
            store_path: path_str,
            signatures: sigs,
        },
        jwt_token,
    )?;
    if let Err(e) = rio_common::grpc::with_timeout(
        "AddSignatures",
        DEFAULT_GRPC_TIMEOUT,
        store_client.add_signatures(req),
    )
    .await
    {
        stderr_err!(stderr, "store error: {e}");
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
#[instrument(skip_all, fields(id = tracing::field::Empty))]
pub(super) async fn handle_register_drv_output<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
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

    // Required fields: id, outPath. Missing → warn with the field
    // name so the operator sees what's wrong (the downstream
    // parse_drv_output_id error for a missing id is "malformed
    // DrvOutput id: ''" — misleading without this context).
    // unwrap_or_default stays so the soft-fail path is unchanged:
    // parse_drv_output_id rejects the empty string and we return
    // Ok(()) after STDERR_LAST, same as before.
    let id_field = parsed.get("id").and_then(|v| v.as_str());
    if id_field.is_none() {
        warn!(field = "id", "RegisterDrvOutput missing required field");
    }
    let id = id_field.unwrap_or_default();
    tracing::Span::current().record("id", id);

    // Wire outPath is a BASENAME ("<hashpart>-<name>") per CppNix
    // StorePath::to_string(). Our gRPC/PG repr uses full paths. Prepend
    // the prefix here — the gateway is the translation boundary. Idempotent
    // guard: if a (buggy or future) client sends the full path, don't
    // double-prepend. Reference: rio-nix/src/protocol/build.rs:350-351
    // (same transform for BuildResult.builtOutputs read path).
    let out_path_field = parsed.get("outPath").and_then(|v| v.as_str());
    if out_path_field.is_none() {
        warn!(
            field = "outPath",
            "RegisterDrvOutput missing required field"
        );
    }
    let out_path_raw = out_path_field.unwrap_or_default();
    let out_path = if out_path_raw.starts_with(rio_nix::store_path::STORE_PREFIX) {
        out_path_raw.to_string()
    } else {
        format!("{}{out_path_raw}", rio_nix::store_path::STORE_PREFIX)
    };
    // Same soft-fail guard as `id` below: validate client input BEFORE
    // the gRPC call so the line-627 assumption ("gRPC error here IS a
    // hard fail — store unreachable, not client input") holds. Without
    // this, a missing/garbage outPath flows through as
    // `RegisterRealisation("/nix/store/")` → store-side
    // `validate_store_path` → `Status::invalid_argument` →
    // session-terminating `stderr_err!`, contradicting the soft-fail
    // policy at the top of this function.
    if StorePath::parse(&out_path).is_err() {
        warn!(out_path = %out_path, "wopRegisterDrvOutput: malformed outPath, discarding");
        stderr.finish().await?;
        return Ok(());
    }

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

    // output_hash: the Nix Realisation JSON doesn't carry it — fetch from
    // QueryPathInfo(outPath).nar_hash. This field is NOT on the CA-cutoff
    // critical path (cutoff is realisation-based);
    // it's for realisation SIGNING — the signed tuple is
    // (drv_hash, output_name, output_path, nar_hash) per store.md:206.
    // Zeros would mean signing attests to nothing about the output content.
    //
    // outPath should already be in the store: Nix's protocol order is build
    // → upload outputs (wopAddToStore/wopAddMultipleToStore) → register drv
    // output (this opcode). If the client sends this BEFORE uploading, or
    // the upload failed silently: warn + fall back to zeros. The registration
    // still lands (cache-hit still works — QueryRealisation keys on
    // (drv_hash, output_name) only); it just won't carry a meaningful hash
    // for signing. Same soft-fail posture as the malformed-JSON path above.
    let output_hash: [u8; 32] = match grpc_query_path_info(store_client, jwt_token, &out_path).await
    {
        Ok(Some(info)) => info.nar_hash,
        Ok(None) => {
            warn!(
                out_path = %out_path,
                "wopRegisterDrvOutput: outPath not in store (uploaded out of order?); \
                 registering with zero output_hash — realisation unsignable"
            );
            [0u8; 32]
        }
        // Store-unreachable IS a hard fail below (RegisterRealisation will
        // hit the same dead store); but a transient here isn't worth bouncing
        // the whole registration when zeros are the prior behavior.
        Err(e) => {
            warn!(
                out_path = %out_path, error = %e,
                "wopRegisterDrvOutput: QueryPathInfo failed; \
                 registering with zero output_hash"
            );
            [0u8; 32]
        }
    };

    let req = with_jwt(
        types::RegisterRealisationRequest {
            realisation: Some(types::Realisation {
                drv_hash: drv_hash.to_vec(),
                output_name,
                output_path: out_path,
                output_hash: output_hash.to_vec(),
                signatures,
            }),
        },
        jwt_token,
    )?;

    // gRPC error here IS a hard fail (store unreachable, not client input).
    if let Err(e) = rio_common::grpc::with_timeout(
        "RegisterRealisation",
        DEFAULT_GRPC_TIMEOUT,
        store_client.register_realisation(req),
    )
    .await
    {
        stderr_err!(stderr, "store error: {e}");
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
#[instrument(skip_all, fields(id = tracing::field::Empty))]
pub(super) async fn handle_query_realisation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let id = wire::read_string(reader).await?;
    tracing::Span::current().record("id", id.as_str());

    // Malformed id → empty set (same soft-fail rationale as Register).
    let Some((drv_hash, output_name)) = parse_drv_output_id(&id) else {
        warn!(id = %id, "wopQueryRealisation: malformed DrvOutput id, returning empty");
        stderr.finish().await?;
        wire::write_u64(stderr.inner_mut(), 0).await?;
        return Ok(());
    };
    // Log the decoded hash so it can be compared against the scheduler's
    // insert_realisation instrument field (also hex-encoded). A mismatch
    // between the two = hash_derivation_modulo divergence from CppNix.
    info!(
        drv_hash = %hex::encode(drv_hash),
        output = %output_name,
        "wopQueryRealisation"
    );

    // Resolve BEFORE stderr.finish() so a non-NotFound store error
    // (Unavailable / DeadlineExceeded) can surface via STDERR_ERROR.
    // The Ok payload is a ~200-byte in-memory JSON — there is nothing
    // to "buffer". Same shape as handle_query_path_from_hash_part.
    let json = match grpc_query_realisation(store_client, jwt_token, drv_hash, &output_name).await {
        Ok(Some(r)) => {
            // Wire outPath is a BASENAME — CppNix Realisation::fromJSON
            // feeds it to StorePath::parse, which rejects '/' with
            // "illegal base-32 char '/'". Strip the prefix. Reference:
            // rio-nix/src/protocol/build.rs:415-418 (same transform for
            // BuildResult.builtOutputs write path). unwrap_or defensive:
            // if the store somehow returns a basename, pass it through.
            let out_path_basename = r
                .output_path
                .strip_prefix(rio_nix::store_path::STORE_PREFIX)
                .unwrap_or(&r.output_path)
                .to_owned();
            // Reconstruct the Nix Realisation JSON. id is the same string
            // we got; outPath from the store; signatures from the store;
            // dependentRealisations is always empty (phase 5 populates it).
            //
            // serde_json::json! macro gives us correct escaping for free —
            // outPath could theoretically contain JSON-special chars (it
            // won't, store paths are nixbase32+name, but defensive).
            Some(
                serde_json::json!({
                    "id": id,
                    "outPath": out_path_basename,
                    "signatures": r.signatures,
                    "dependentRealisations": {}
                })
                .to_string(),
            )
        }
        // NOT_FOUND is a cache miss → empty set. Normal during
        // substituter probes; WRONG if the build just completed (means
        // scheduler's insert_realisation used a different hash — check
        // the `drv_hash` field on its #[instrument] span vs the one
        // logged above).
        Ok(None) => {
            info!(
                drv_hash = %hex::encode(drv_hash),
                output = %output_name,
                "wopQueryRealisation: miss (no realisation)"
            );
            None
        }
        Err(e) => stderr_err!(stderr, "store error: {e}"),
    };

    stderr.finish().await?;
    let w = stderr.inner_mut();
    match json {
        Some(s) => {
            wire::write_u64(w, 1).await?;
            wire::write_string(w, &s).await?;
        }
        None => wire::write_u64(w, 0).await?,
    }

    Ok(())
}

// r[impl gw.opcode.query-missing]
/// wopQueryMissing (40): Report what needs building.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_query_missing<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    // r[impl store.substitute.upstream]
    // JWT propagation: x-rio-tenant-token reaches store's FindMissingPaths
    // handler → try_substitute_on_miss reads tenant_id from Claims →
    // upstream substituter probe. Without this, the store sees anonymous
    // → substitute short-circuits at rio-store/src/grpc/mod.rs
    // tenant_id_or_skip → client is told to BUILD what it could FETCH.
    // This was the P0465 blocker for `cargo xtask k8s -p kind rsb`.
    let jwt_token = ctx.jwt.token();
    let drv_cache = &mut ctx.drv_cache;
    let raw_paths = wire::read_strings(reader).await?;
    tracing::Span::current().record("count", raw_paths.len());
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

    // Resolve DerivedPath → concrete store paths for the FindMissingPaths
    // query. For Opaque, the path IS the query path. For Built, we must
    // resolve the .drv to its OUTPUT paths — querying the .drv path is
    // semantically wrong: the .drv was just uploaded via AddToStore so
    // it's always "present", and the store's substitution probe fires
    // on OUTPUT paths, not .drv paths. Before P0471 this returned
    // willBuild for everything cacheable.
    //
    // `query_paths[i]` maps back to `derived` via `query_src[i]`: the
    // index into `derived` that produced this query path. One Built
    // DerivedPath with N outputs fans out to N query_paths entries,
    // all pointing at the same derived[idx].
    let mut query_paths: Vec<String> = Vec::new();
    let mut query_src: Vec<usize> = Vec::new();
    // .drv paths forced to willBuild without a store query: the .drv
    // couldn't be resolved (not in store — client needs to upload then
    // build) or the output is floating-CA with no realisation yet.
    let mut forced_build: HashSet<String> = HashSet::new();
    // Shared across all .drvs in the request — sub-hashes reused.
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();

    for (idx, dp) in derived.iter().enumerate() {
        match dp {
            DerivedPath::Opaque(p) => {
                query_paths.push(p.to_string());
                query_src.push(idx);
            }
            DerivedPath::Built { drv, outputs } => {
                let drv_data = match resolve_derivation(drv, store_client, drv_cache).await {
                    Ok(d) => d,
                    Err(e) => {
                        tracing::warn!(
                            drv = %drv, error = %e,
                            "wopQueryMissing: .drv not resolvable, reporting willBuild"
                        );
                        forced_build.insert(drv.to_string());
                        continue;
                    }
                };
                // Floating-CA outputs: query the Realisations table
                // (same lookup as opcodes 41/46). If realised, the
                // output path flows through the normal
                // missing/substitutable partition below; only
                // un-realised CA derivations land in willBuild.
                // Non-NotFound store errors → STDERR_ERROR (consistent
                // with the FindMissingPaths error arm below).
                let (_hash, realized) = match resolve_floating_outputs(
                    &drv_data,
                    drv.as_str(),
                    store_client,
                    jwt_token,
                    drv_cache,
                    &mut hash_cache,
                )
                .await
                {
                    Ok(v) => v,
                    Err(e) => stderr_err!(stderr, "store error: {e}"),
                };
                for out in drv_data.outputs() {
                    let matches = match outputs {
                        OutputSpec::All => true,
                        OutputSpec::Names(names) => names.iter().any(|n| n == out.name()),
                    };
                    if !matches {
                        continue;
                    }
                    if out.path().is_empty() {
                        match realized.get(out.name()) {
                            Some(p) => {
                                query_paths.push(p.clone());
                                query_src.push(idx);
                            }
                            None => {
                                forced_build.insert(drv.to_string());
                            }
                        }
                    } else {
                        query_paths.push(out.path().to_string());
                        query_src.push(idx);
                    }
                }
            }
        }
    }

    let req = with_jwt(
        types::FindMissingPathsRequest {
            store_paths: query_paths.clone(),
        },
        jwt_token,
    )?;
    let (missing_set, substitutable_set): (HashSet<String>, HashSet<String>) =
        match tokio::time::timeout(DEFAULT_GRPC_TIMEOUT, store_client.find_missing_paths(req)).await
        {
            Ok(Ok(r)) => {
                let resp = r.into_inner();
                (
                    resp.missing_paths.into_iter().collect(),
                    resp.substitutable_paths.into_iter().collect(),
                )
            }
            Ok(Err(e)) => stderr_err!(stderr, "gRPC FindMissingPaths: {e}"),
            Err(_) => stderr_err!(
                stderr,
                "gRPC FindMissingPaths timed out after {DEFAULT_GRPC_TIMEOUT:?}"
            ),
        };

    // wopQueryMissing response is three StorePathSets (willBuild,
    // willSubstitute, unknown) — plain store paths, NOT DerivedPath
    // wire strings. willSubstitute carries OUTPUT paths (what the
    // client will fetch); willBuild carries .drv paths (what the
    // client will build). Echoing `...drv!out` fails the client's
    // StorePath::parse on '!'.
    //
    // Partition per query_path: present → skip; missing ∩ substitutable
    // → willSubstitute(output_path); missing ∩ ¬substitutable →
    // willBuild(drv) for Built, unknown(path) for Opaque.
    // Substitutable-check first so a path that's both buildable AND
    // substitutable lands in willSubstitute — fetching beats building.
    let mut will_build: HashSet<String> = forced_build;
    let mut will_substitute = Vec::new();
    let mut unknown = Vec::new();

    for (path, &src_idx) in query_paths.iter().zip(query_src.iter()) {
        if !missing_set.contains(path) {
            continue;
        }
        if substitutable_set.contains(path) {
            will_substitute.push(path.clone());
            continue;
        }
        match &derived[src_idx] {
            DerivedPath::Built { drv, .. } => {
                will_build.insert(drv.to_string());
            }
            DerivedPath::Opaque(p) => unknown.push(p.to_string()),
        }
    }

    let will_build: Vec<String> = will_build.into_iter().collect();

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_strings(w, &will_build).await?;
    wire::write_strings(w, &will_substitute).await?;
    wire::write_strings(w, &unknown).await?;
    wire::write_u64(w, 0).await?; // downloadSize
    wire::write_u64(w, 0).await?; // narSize
    Ok(())
}

// r[impl gw.opcode.query-derivation-output-map]
/// wopQueryDerivationOutputMap (41): Return output name -> path mappings.
///
/// nix-build (legacy CLI) sends wopBuildPaths — which returns NO path
/// info — then sends THIS opcode to discover what was built (nix-build.cc
/// calls `store->queryPartialDerivationOutputMap(drvPath)` post-build,
/// then asserts each result is non-nullopt).
///
/// For IA/fixed-CA outputs, the path is in the .drv. For floating-CA
/// outputs, `output.path()` is "" (computed post-build from NAR hash).
/// Writing "" → client reads `std::nullopt` → `assert(maybeOutputPath)`
/// fires at nix-build.cc:722. Mirror CppNix's `queryPartialDerivationOutputMap`
/// (store-api.cc:442-466): query the Realisations table for CA outputs.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_query_derivation_output_map<
    R: AsyncRead + Unpin,
    W: AsyncWrite + Unpin,
>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let store_client = &mut ctx.store_client;
    let jwt_token = ctx.jwt.token();
    let drv_cache = &mut ctx.drv_cache;
    let (drv_path_str, drv_path) = match read_store_path(reader).await {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "{e}"),
    };
    tracing::Span::current().record("path", drv_path_str.as_str());
    info!(path = %drv_path_str, "wopQueryDerivationOutputMap");

    let drv = match resolve_derivation(&drv_path, store_client, drv_cache).await {
        Ok(d) => d,
        Err(e) => stderr_err!(stderr, "store error: {e}"),
    };

    // Floating-CA: .drv has path="" — resolve via Realisations. Shared
    // helper with opcodes 36/40/46; nix-build uses the legacy
    // wopBuildPaths path which doesn't return builtOutputs, so it
    // comes back here instead. Non-NotFound store errors propagate
    // to STDERR_ERROR (we're before stderr.finish()) — passing ""
    // through would make the client `assert(maybeOutputPath)` at
    // nix-build.cc:722 with no indication the store was unreachable.
    let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
    let (_hash, realized) = match resolve_floating_outputs(
        &drv,
        drv_path.as_str(),
        store_client,
        jwt_token,
        drv_cache,
        &mut hash_cache,
    )
    .await
    {
        Ok(v) => v,
        Err(e) => stderr_err!(
            stderr,
            "store error querying realisation for {drv_path_str}: {e}"
        ),
    };

    let outputs = drv.outputs();

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_u64(w, outputs.len() as u64).await?;
    for output in outputs {
        wire::write_string(w, output.name()).await?;
        let path = if output.path().is_empty() {
            realized
                .get(output.name())
                .map(String::as_str)
                .unwrap_or("")
        } else {
            output.path()
        };
        wire::write_string(w, path).await?;
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
