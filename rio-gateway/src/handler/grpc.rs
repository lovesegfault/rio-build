//! Store-query helpers using gRPC.
//!
//! All helpers take `jwt_token: Option<&str>` and attach it as
//! `x-rio-tenant-token` via [`with_jwt`] / [`jwt_metadata`] (the former
//! wraps a `tonic::Request`, the latter feeds `rio_proto::client::*`
//! helpers; both share one header-construction path). Without the JWT,
//! store-side tenant-scoped operations (substitution, narinfo
//! visibility gate) short-circuit — see `r[gw.jwt.issue]`.

use std::collections::HashMap;

use rio_common::grpc::{DEFAULT_GRPC_TIMEOUT, GRPC_STREAM_TIMEOUT};
use rio_common::limits::MAX_NAR_SIZE;
use rio_nix::derivation::Derivation;
use rio_nix::store_path::StorePath;
use rio_proto::client::NAR_CHUNK_SIZE;
use rio_proto::validated::ValidatedPathInfo;
use rio_proto::{StoreServiceClient, types};
use tokio::io::{AsyncRead, AsyncReadExt};
use tonic::transport::Channel;

use super::{GatewayError, attach_service_token, jwt_metadata, with_jwt};
use crate::translate;

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
pub(crate) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    rio_proto::client::query_path_info_opt(
        store_client,
        store_path,
        DEFAULT_GRPC_TIMEOUT,
        &jwt_metadata(jwt_token),
    )
    .await
    .map_err(|e| GatewayError::Store(format!("QueryPathInfo failed: {e}")).into())
}

/// QueryRealisation with NotFound→None mapping. Any non-NotFound status
/// is returned as Err — caller MUST `stderr_err!` it. Never swallow.
///
/// Chokepoint for the four CA-aware opcode handlers (40, 41, 43, 46) and
/// opcode 36's output-enrichment. `NotFound` is the *only* store status
/// that maps to wire-level "no result"; `Unavailable` / `DeadlineExceeded`
/// / `Internal` are infrastructure errors that the client must see via
/// `STDERR_ERROR` — otherwise (per the doc-comment on
/// `handle_query_derivation_output_map`) the client receives `outPath=""`
/// → `assert(maybeOutputPath)` at nix-build.cc:722 with no indication
/// the store was unreachable.
pub(super) async fn grpc_query_realisation(
    client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    drv_hash: [u8; 32],
    output_name: &str,
) -> anyhow::Result<Option<types::Realisation>> {
    let req = with_jwt(
        types::QueryRealisationRequest {
            drv_hash: drv_hash.to_vec(),
            output_name: output_name.to_string(),
        },
        jwt_token,
    )?;
    match rio_common::grpc::with_timeout(
        "QueryRealisation",
        DEFAULT_GRPC_TIMEOUT,
        client.query_realisation(req),
    )
    .await
    {
        Ok(resp) => Ok(Some(resp.into_inner())),
        Err(e)
            if e.downcast_ref::<tonic::Status>()
                .is_some_and(|s| s.code() == tonic::Code::NotFound) =>
        {
            Ok(None)
        }
        Err(e) => Err(e),
    }
}

/// For each floating-CA output of `drv`, query the Realisations table.
///
/// Returns `(modular_hash, name→output_path)`. NotFound entries are absent
/// from the map (caller falls back to `""` / `forced_build`).
/// `modular_hash` is `None` iff [`compute_modular_hash_cached`] failed
/// (already `warn!`-logged) — IA outputs are still resolvable from the
/// `.drv`, only floating-CA stays empty.
///
/// Non-NotFound store errors propagate as `Err` — caller `stderr_err!`s.
///
/// Shared resolver for opcodes 36/40/41/46; before this extraction each
/// caller open-coded the same `compute_modular_hash_cached → per-output
/// QueryRealisation` loop with inconsistent error handling (two of the
/// four swallowed non-NotFound — see [`grpc_query_realisation`]).
///
/// [`compute_modular_hash_cached`]: crate::translate::compute_modular_hash_cached
pub(super) async fn resolve_floating_outputs(
    drv: &Derivation,
    drv_path: &str,
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> anyhow::Result<(Option<[u8; 32]>, HashMap<String, String>)> {
    let mut realized: HashMap<String, String> = HashMap::new();
    let has_floating = drv.outputs().iter().any(|o| o.path().is_empty());
    if !has_floating {
        return Ok((None, realized));
    }
    let Some(hash) = translate::compute_modular_hash_cached(drv, drv_path, drv_cache, hash_cache)
    else {
        // compute_modular_hash_cached already warn!-logged. IA outputs
        // still get their .drv paths; CA outputs stay unresolved.
        return Ok((None, realized));
    };
    for out in drv.outputs() {
        if !out.path().is_empty() {
            continue;
        }
        match grpc_query_realisation(store_client, jwt_token, hash, out.name()).await? {
            Some(r) => {
                realized.insert(out.name().to_string(), r.output_path);
            }
            None => {
                tracing::info!(
                    drv_hash = %hex::encode(hash),
                    output = %out.name(),
                    "no realisation for floating-CA output (not yet built)"
                );
            }
        }
    }
    Ok((Some(hash), realized))
}

/// Check validity via QueryPathInfo -- returns true if path exists.
pub(super) async fn grpc_is_valid_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    path: &StorePath,
) -> anyhow::Result<bool> {
    Ok(grpc_query_path_info(store_client, jwt_token, path.as_str())
        .await?
        .is_some())
}

/// Max attempts for `Code::Aborted` retry in [`grpc_put_path`]. The
/// store returns Aborted when another upload holds the placeholder for
/// this path (I-068) or on PG serialization/deadlock conflicts — both
/// clear in one round-trip (.drv NARs are KB). GC no longer blocks
/// PutPath at all (I-192).
///
/// 50 ms base, ×2, full jitter, 2 s cap. 8 attempts → ≤~6 s budget —
/// generous for the remaining (fast-clearing) cases; kept as a safety
/// margin rather than tightened. Shared with rio-builder's PutPath
/// retry (`upload.rs`): both hit the same store-side placeholder
/// contention, so they use the same curve+budget.
const PUT_PATH_ABORTED_MAX_ATTEMPTS: u32 = 8;
const PUT_PATH_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(50),
    mult: 2.0,
    cap: std::time::Duration::from_secs(2),
    jitter: rio_common::backoff::Jitter::Full,
};

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
///
/// Retries on `Code::Aborted` (concurrent same-path upload — store's
/// `put_path.rs` returns this when another writer holds the placeholder
/// row). I-068: with the I-052 32-way pipeline × N clients × shared
/// closure, collisions are guaranteed; before this retry the gateway
/// surfaced Aborted as a hard wopAddMultipleToStore failure and the
/// client died mid-push.
///
/// `nar_data` is held as `Arc<[u8]>` so each retry rebuilds the request
/// stream without copying the buffer. `info` is `Clone` (cheap — strings
/// and Vecs already heap-allocated).
// r[impl gw.put.aborted-retry]
pub(super) async fn grpc_put_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    service_signer: Option<&rio_auth::hmac::HmacSigner>,
    info: ValidatedPathInfo,
    nar_data: Vec<u8>,
) -> anyhow::Result<bool> {
    let nar: std::sync::Arc<[u8]> = nar_data.into();
    let mut attempt = 0u32;
    loop {
        let stream =
            rio_proto::client::chunk_nar_for_put(info.clone(), std::sync::Arc::clone(&nar));
        let mut req = with_jwt(stream, jwt_token)?;
        attach_service_token(&mut req, service_signer);
        let result = rio_common::grpc::with_timeout_status(
            "PutPath",
            GRPC_STREAM_TIMEOUT,
            store_client.put_path(req),
        )
        .await;
        match result {
            Ok(resp) => return Ok(resp.into_inner().created),
            Err(status) if status.code() == tonic::Code::Aborted => {
                attempt += 1;
                // I-168: dashboard-visible retry budget (was log-only).
                metrics::counter!(
                    "rio_gateway_putpath_aborted_retries_total",
                    "attempt" => attempt.to_string(),
                )
                .increment(1);
                if attempt >= PUT_PATH_ABORTED_MAX_ATTEMPTS {
                    tracing::warn!(
                        store_path = %info.store_path,
                        attempts = attempt,
                        "PutPath: store still Aborted after retry budget; surfacing"
                    );
                    return Err(status.into());
                }
                // FULL jitter (`U(0, capᵃ]`): N clients retrying the
                // SAME path don't re-collide in lockstep, and the
                // I-068 placeholder case stays fast (first retry
                // ≤50 ms) while the I-168 mark-busy case gets a
                // multi-second window. `attempt-1` so attempt=1 uses
                // mult⁰ = base.
                let delay = PUT_PATH_BACKOFF.duration(attempt - 1);
                tracing::debug!(
                    store_path = %info.store_path,
                    attempt,
                    backoff = ?delay,
                    msg = %status.message(),
                    "PutPath: store Aborted; retrying with exponential backoff"
                );
                tokio::time::sleep(delay).await;
            }
            Err(status) => return Err(status.into()),
        }
    }
}

/// Upload a path to the store, streaming NAR bytes from a reader.
///
/// Reads exactly `nar_size` bytes from `nar_reader` in `NAR_CHUNK_SIZE`
/// chunks and forwards each as a NarChunk. Forwards the client-declared
/// hash in the trailer — store re-hashes and validates (same security
/// property as [`grpc_put_path`]; the gateway is a dumb pipe here).
///
/// `nar_reader` must yield exactly `nar_size` bytes; short read = error.
/// Caller is responsible for the `nar_size <= MAX_NAR_SIZE` check.
///
/// NOT retried on `Aborted` (unlike [`grpc_put_path`]): the reader is
/// consumed and the bytes are forwarded as they arrive, so there's
/// nothing to replay. In practice this path only fires for oversize
/// (>DRV_NAR_BUFFER_LIMIT) entries — the I-068 collision case is .drv
/// files, which always go through the buffered path.
pub(super) async fn grpc_put_path_streaming<R: AsyncRead + Unpin>(
    store_client: &StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    service_signer: Option<&rio_auth::hmac::HmacSigner>,
    info: ValidatedPathInfo,
    nar_reader: &mut R,
    nar_size: u64,
    client_nar_hash: Vec<u8>,
) -> anyhow::Result<bool> {
    // ~1 MiB in flight at 256 KiB chunks.
    const CHANNEL_BUF: usize = 4;

    let (tx, rx) = tokio::sync::mpsc::channel::<types::PutPathRequest>(CHANNEL_BUF);

    // Metadata first. Zero nar_hash/nar_size → trailer mode.
    let mut raw: types::PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;
    tx.send(types::PutPathRequest {
        msg: Some(types::put_path_request::Msg::Metadata(
            types::PutPathMetadata { info: Some(raw) },
        )),
    })
    .await
    .map_err(|_| GatewayError::GrpcStream("PutPath channel closed before metadata".into()))?;

    // Drive the gRPC call. Clone: tonic Channel is Arc-backed.
    // JWT wrapped BEFORE the spawn — jwt_token's lifetime doesn't
    // extend into the 'static task.
    let mut client = store_client.clone();
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = with_jwt(outbound, jwt_token)?;
    attach_service_token(&mut req, service_signer);
    let rpc: tokio::task::JoinHandle<anyhow::Result<tonic::Response<types::PutPathResponse>>> =
        tokio::spawn(async move {
            rio_common::grpc::with_timeout("PutPath", GRPC_STREAM_TIMEOUT, client.put_path(req))
                .await
        });

    // Read exactly nar_size bytes in NAR_CHUNK_SIZE chunks, forward each.
    // Backpressure: tx.send blocks when rpc isn't pulling. On a short read
    // we still drop tx and await rpc so the spawned task completes before
    // we return. A closed channel is NOT a pump error: it means the rpc
    // task has already completed (dropping rx) — but the rpc may have
    // returned Ok(created:false) early (store-side AlreadyComplete /
    // Concurrent-race after `drain_stream` timed out), so the pump must
    // STILL consume nar_size to honor the "reads exactly nar_size bytes"
    // contract callers depend on for wire positioning.
    let pump_result: anyhow::Result<()> = async {
        let mut remaining = nar_size;
        let mut chunk = vec![0u8; NAR_CHUNK_SIZE];
        while remaining > 0 {
            let n = (remaining.min(NAR_CHUNK_SIZE as u64)) as usize;
            nar_reader
                .read_exact(&mut chunk[..n])
                .await
                .map_err(|e| GatewayError::NarRead {
                    context: format!("at {} of {nar_size}", nar_size - remaining),
                    source: e,
                })?;
            remaining -= n as u64;
            if tx
                .send(types::PutPathRequest {
                    msg: Some(types::put_path_request::Msg::NarChunk(chunk[..n].to_vec())),
                })
                .await
                .is_err()
            {
                // rx dropped → rpc task already finished. It MAY have
                // returned Ok(created:false) early, so drain nar_reader
                // to nar_size before returning Ok — otherwise the
                // caller's framed reader is left mid-NAR and the next
                // entry's header parses garbage. If rpc_result is Err,
                // that surfaces via rpc_result? below regardless.
                tokio::io::copy(
                    &mut (&mut *nar_reader).take(remaining),
                    &mut tokio::io::sink(),
                )
                .await
                .map_err(|e| GatewayError::NarRead {
                    context: format!("draining {remaining} of {nar_size} after store early-Ok"),
                    source: e,
                })?;
                return Ok(());
            }
        }

        // Trailer: client-declared hash. Store validates independently.
        if tx
            .send(types::PutPathRequest {
                msg: Some(types::put_path_request::Msg::Trailer(
                    types::PutPathTrailer {
                        nar_hash: client_nar_hash,
                        nar_size,
                    },
                )),
            })
            .await
            .is_err()
        {
            return Ok(());
        }
        Ok(())
    }
    .await;

    drop(tx); // close channel → ReceiverStream yields None → rpc completes

    let rpc_result = rpc
        .await
        .map_err(|e| GatewayError::GrpcStream(format!("PutPath task panicked: {e}")))?;

    // Error priority: pump error (NarRead — client short read) > rpc error.
    // A short read truncates the stream; the useful message is "NAR read at
    // X of Y", not "store rejected incomplete stream". The pump's only Err
    // variant is NarRead — a closed channel returns Ok above, so an early
    // store rejection (auth/quota/validation) surfaces via rpc_result?
    // with the store's actual Status, not a generic "channel closed".
    pump_result?;
    let resp = rpc_result?;
    Ok(resp.into_inner().created)
}

/// Fetch NAR data from store via gRPC GetPath.
/// Returns (PathInfo, NAR bytes) or None if not found.
///
/// Delegates to `rio_proto::client::get_path_nar` — DO NOT inline that
/// helper's await structure here. Under `#[tokio::test(start_paused =
/// true)]`, the exact suspend-point layout determines whether tokio's
/// auto-advance fires the GRPC_STREAM_TIMEOUT before in-process gRPC
/// I/O completes (observed in wire_opcodes::build reconnect tests when
/// P0465 initially inlined this; reverted to delegation).
pub(crate) async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<(ValidatedPathInfo, Vec<u8>)>> {
    rio_proto::client::get_path_nar(
        store_client,
        store_path,
        GRPC_STREAM_TIMEOUT,
        MAX_NAR_SIZE,
        None,
        &jwt_metadata(jwt_token),
    )
    .await
    .map_err(|e| GatewayError::Store(format!("GetPath for {store_path}: {e}")).into())
}
