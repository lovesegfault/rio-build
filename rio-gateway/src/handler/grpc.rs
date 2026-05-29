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

/// Max attempts + backoff for [`transient_retry_after`]. 250 ms base,
/// ×4, 4 s cap, ±25% jitter. Retry budget is 2 attempts (one retry).
/// Under sustained admission saturation each attempt blocks
/// `SUBSTITUTE_ADMISSION_WAIT` (25 s) server-side, so worst-case
/// latency before surfacing to the user is ~50 s — bounded, but
/// operators should treat sustained `RESOURCE_EXHAUSTED` here as a
/// scaling signal. Gateway clients are interactive (`nix copy`, IFD
/// evals); the single retry covers transient blips without masking
/// genuine overload.
const STORE_TRANSIENT_MAX_ATTEMPTS: u32 = 2;
const STORE_TRANSIENT_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(250),
    mult: 4.0,
    cap: std::time::Duration::from_secs(4),
    jitter: rio_common::backoff::Jitter::Proportional(0.25),
};

/// `Some(delay)` if `status` is transient (per
/// [`rio_common::grpc::is_transient`]) and `attempt <
/// STORE_TRANSIENT_MAX_ATTEMPTS`; `None` if the caller should surface
/// the error. Logs the retry/surface decision. Shared by
/// [`grpc_query_path_info`] and [`grpc_get_path`] — both traverse
/// `r[store.substitute.admission+2]` server-side, which returns
/// `ResourceExhausted` after its bounded wait under saturation. The
/// in-process materialization executor re-arms through its job budget
/// (`r[store.materialize.executor+5]`); without this retry the gateway
/// surfaced a hard `STDERR_ERROR` → client sees
/// "store error: ResourceExhausted" on a momentary overload.
///
/// NOT a closure-taking `retry(op)` wrapper: `impl AsyncFnMut`
/// capturing `&mut StoreServiceClient` hits the HRTB-`Send` limitation
/// when the calling future is `tokio::spawn`ed (the gateway proto-task
/// is). Same restriction noted at `rio_common::backoff::retry`'s test.
// r[impl gw.store.transient-retry]
fn transient_retry_after(
    rpc: &'static str,
    attempt: u32,
    status: &tonic::Status,
) -> Option<std::time::Duration> {
    if !rio_common::grpc::is_transient(status.code()) {
        return None;
    }
    if attempt >= STORE_TRANSIENT_MAX_ATTEMPTS {
        tracing::warn!(
            rpc, attempts = attempt, code = ?status.code(),
            "store transient status exhausted retry budget; surfacing"
        );
        return None;
    }
    let delay = STORE_TRANSIENT_BACKOFF.duration(attempt - 1);
    tracing::debug!(
        rpc, attempt, backoff = ?delay, code = ?status.code(), msg = %status.message(),
        "store transient status; retrying"
    );
    Some(delay)
}

/// Query PathInfo from store via gRPC. Returns None if NOT_FOUND.
///
/// Retries transient status per [`transient_retry_after`] — store-side
/// `QueryPathInfo` traverses `try_substitute_on_miss`
/// (`r[store.substitute.admission+2]`).
pub(crate) async fn grpc_query_path_info(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<ValidatedPathInfo>> {
    let md = jwt_metadata(jwt_token);
    let mut attempt = 0u32;
    loop {
        match rio_proto::client::query_path_info_opt(
            store_client,
            store_path,
            DEFAULT_GRPC_TIMEOUT,
            &md,
        )
        .await
        {
            Ok(v) => return Ok(v),
            Err(status) => {
                attempt += 1;
                match transient_retry_after("QueryPathInfo", attempt, &status) {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => {
                        return Err(
                            GatewayError::Store(format!("QueryPathInfo failed: {status}")).into(),
                        );
                    }
                }
            }
        }
    }
}

/// QueryRealisation with NotFound→None mapping. Any non-NotFound status
/// is returned as Err — caller MUST `stderr_err!` it. Never swallow.
///
/// Chokepoint for the CA-aware opcode handlers (40, 41, 43) and the
/// build-result store verification (`check_targets_against_store`,
/// reached by opcodes 9/36/46). `NotFound` is the *only* store status
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
/// Shared resolver for opcodes 9/36/40/41/46 (the build opcodes reach it
/// via `check_targets_against_store`); before this extraction each
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
// pub(crate): the alert-seed axis pin in lib.rs
// (putpath_retry_attempt_axis_matches_the_emit_law) derives the seeded
// label product from this bound.
pub(crate) const PUT_PATH_ABORTED_MAX_ATTEMPTS: u32 = 8;
const PUT_PATH_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: std::time::Duration::from_millis(50),
    mult: 2.0,
    cap: std::time::Duration::from_secs(2),
    jitter: rio_common::backoff::Jitter::Full,
};

// r[impl gw.putpath.emit-law]
/// bug_118: THE single emit site for the PutPath failure series. The
/// emit law binds to the OPERATION, not a callsite — this fn is the
/// only place in the module that names the metric, and every
/// failure-surfacing arm on every PutPath lane routes through one of
/// the two typed surfacing adapters below, which consume the failure
/// en route to the response (an arm that bypasses them has no error
/// value to return — the R24 shape on an observability law; the
/// W11-BI source census pins the discipline). The KEDA store
/// ScaledObject's demand-side scale-collapse inhibitor consumes
/// `sum(rate())` of exactly this series: pre-fix only the buffered
/// lane emitted, so a streaming-dominated store outage (all non-.drv
/// wopAddToStoreNar traffic + oversize AddMultiple) left the
/// inhibitor FLAT — the merged_bug_038 defect re-instantiated one
/// lane over.
fn emit_put_path_failure(class: &'static str) {
    metrics::counter!(
        "rio_gateway_putpath_retry_events_total",
        "class" => class,
    )
    .increment(1);
}

// r[impl gw.putpath.emit-law]
/// Surface one status-bearing PutPath failure observation (buffered
/// lane: every attempt's failure, retried or terminal; streaming
/// lane: the store's terminal Status): emits the class-labeled
/// counter and hands the status back for disposition.
fn surface_put_path_failure(status: tonic::Status) -> tonic::Status {
    emit_put_path_failure(rio_common::grpc::code_class_label(status.code()));
    status
}

// r[impl gw.putpath.emit-law]
/// Surface one statusless PutPath terminal failure (client NAR
/// short-read, task panic, pre-stream channel close): a store status
/// never existed, so the class is `"unknown"` BY DEFINITION — a
/// first-class member of the seeded `CODE_CLASS_LABELS` alphabet,
/// not a catch-all over it (a status-bearing failure downcasts and
/// classifies by code instead).
fn surface_put_path_failure_any(err: anyhow::Error) -> anyhow::Error {
    match err.downcast_ref::<tonic::Status>() {
        Some(s) => emit_put_path_failure(rio_common::grpc::code_class_label(s.code())),
        None => emit_put_path_failure(rio_common::grpc::code_class_label(tonic::Code::Unknown)),
    }
    err
}

/// Upload a path to the store via gRPC PutPath (metadata + NAR chunks).
///
/// Retries on `Code::Aborted` (concurrent same-path upload — store's
/// `put_path.rs` returns this when another writer holds the placeholder
/// row). I-068: with the I-052 32-way pipeline × N clients × shared
/// closure, collisions are guaranteed; before this retry the gateway
/// surfaced Aborted as a hard wopAddMultipleToStore failure and the
/// client died mid-push.
///
/// merged_bug_097: non-Aborted failures take the same-file transient
/// lane ([`transient_retry_after`]) — the store's typed sheds
/// (`rio_common::grpc::STORE_SHED_CLASSES`: the NAR-budget
/// `ResourceExhausted` "retry" invitation) previously hit a terminal
/// arm and hard-failed the nix client's push despite the machinery
/// sitting in this file wired only to QueryPathInfo/GetPath. Every
/// failure observation emits the class-labeled
/// `rio_gateway_putpath_retry_events_total` (merged_bug_038, H8″ —
/// the inhibitor's series must move during reachability outages).
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
    let mut transient_attempt = 0u32;
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
        let status = match result {
            Ok(resp) => return Ok(resp.into_inner().created),
            // THE class-labeled emit law (merged_bug_038, H8″; bound
            // to the operation by bug_118): every failure observation
            // at this chokepoint counts, labeled by its typed class —
            // Unavailable/DeadlineExceeded/… cannot be non-emitting
            // arms (R21: the uncounted terminal arm died). The store
            // ScaledObject's demand-side inhibitor consumes this
            // series; the label alphabet is
            // rio_common::grpc::CODE_CLASS_LABELS (boot-seeded per
            // class, lib.rs — bug_322 birth-gap discipline). The
            // surfacing fn IS the emit site (one per module).
            Err(status) => surface_put_path_failure(status),
        };
        if status.code() == tonic::Code::Aborted {
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
        } else {
            // merged_bug_097: the store's typed sheds
            // (STORE_SHED_CLASSES — the NAR-budget ResourceExhausted
            // "retry" invitation) and transport-class blips absorb
            // through the same-file transient machinery the other
            // store unaries use; the classifier (is_transient) is a
            // superset of the shed set BY THE SHARED CONST. The
            // stream rebuilds from the Arc'd buffer, so replay is
            // safe (unlike the streaming path, which stays
            // non-retried by design).
            transient_attempt += 1;
            match transient_retry_after("PutPath", transient_attempt, &status) {
                Some(delay) => tokio::time::sleep(delay).await,
                None => return Err(status.into()),
            }
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
/// NOT replayed on `Aborted` (unlike [`grpc_put_path`]): the reader is
/// consumed and the bytes are forwarded as they arrive, so there is
/// nothing to replay. sh-004: an `Aborted` carrying the I-068
/// placeholder-contention message ([`rio_proto::CONCURRENT_PUTPATH_MSG`])
/// instead enters wait-then-adopt — the pump already drains exactly
/// `nar_size` bytes regardless (the early-Ok wire-positioning contract
/// below), so the lane backs off via [`PUT_PATH_BACKOFF`], polls
/// [`grpc_query_path_info`], and returns `Ok(false)` once the
/// concurrent uploader's path exists. The retry budget and the
/// `rio_gateway_putpath_aborted_retries_total{attempt}` emit are
/// shared with the buffered lane (single-axis schema; same
/// store-side contention, same curve, same dashboard cell).
// r[impl gw.put.aborted-retry]
pub(super) async fn grpc_put_path_streaming<R: AsyncRead + Unpin>(
    store_client: &mut StoreServiceClient<Channel>,
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

    // Metadata first. Zero nar_hash/nar_size in the PathInfo (the
    // trailer carries the authoritative pair), but DECLARED size set
    // (N1): this sender reads exactly `nar_size` bytes from the
    // reader — the size is knowable up front, so the store reserves
    // single-shot pre-stream instead of charging chunk-by-chunk
    // while holding.
    let store_path = info.store_path.clone();
    let mut raw: types::PathInfo = info.into();
    raw.nar_hash = Vec::new();
    raw.nar_size = 0;
    tx.send(types::PutPathRequest {
        msg: Some(types::put_path_request::Msg::Metadata(
            types::PutPathMetadata {
                info: Some(raw),
                declared_nar_size: nar_size,
            },
        )),
    })
    .await
    .map_err(|_| {
        // bug_118: terminal arm — surfaces through the emit law.
        surface_put_path_failure_any(
            GatewayError::GrpcStream("PutPath channel closed before metadata".into()).into(),
        )
    })?;

    // Drive the gRPC call. Clone: tonic Channel is Arc-backed.
    // JWT wrapped BEFORE the spawn — jwt_token's lifetime doesn't
    // extend into the 'static task.
    let mut client = store_client.clone();
    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = with_jwt(outbound, jwt_token)?;
    attach_service_token(&mut req, service_signer);
    // bug_118: with_timeout_status (not with_timeout) so the
    // store's Status survives to the surfacing fn typed — a timeout
    // classifies deadline_exceeded instead of laundering through an
    // anyhow message into "unknown".
    let rpc: tokio::task::JoinHandle<
        Result<tonic::Response<types::PutPathResponse>, tonic::Status>,
    > = tokio::spawn(async move {
        rio_common::grpc::with_timeout_status("PutPath", GRPC_STREAM_TIMEOUT, client.put_path(req))
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

    let rpc_result = rpc.await.map_err(|e| {
        // bug_118: terminal arm (task panic — no store status).
        surface_put_path_failure_any(
            GatewayError::GrpcStream(format!("PutPath task panicked: {e}")).into(),
        )
    })?;

    // Error priority: pump error (NarRead — client short read) > rpc error.
    // A short read truncates the stream; the useful message is "NAR read at
    // X of Y", not "store rejected incomplete stream". The pump's only Err
    // variant is NarRead — a closed channel returns Ok above, so an early
    // store rejection (auth/quota/validation) surfaces via rpc_result?
    // with the store's actual Status, not a generic "channel closed".
    // bug_118: exactly ONE emission per terminal failure — the error
    // that SURFACES is the one surfaced through the emit law (a
    // swallowed rpc error behind a winning pump error stays uncounted
    // by design: one operation, one terminal observation).
    pump_result.map_err(surface_put_path_failure_any)?;
    let status = match rpc_result {
        Ok(resp) => return Ok(resp.into_inner().created),
        // bug_118: the surfacing fn IS the emit site — the store's
        // failure observation counts here, retried-by-adopt or
        // terminal (one operation, one observation; the buffered
        // lane's loop-head emit has the same shape).
        Err(status) => surface_put_path_failure(status),
    };
    // sh-004: wait-then-adopt on the I-068 placeholder-contention
    // Aborted. The reader is already drained to `nar_size` (the pump's
    // rx-dropped arm above), so the framed reader stays positioned for
    // the caller; the lane polls for the concurrent uploader's result
    // instead of replaying. Precedent: rio-builder upload/single.rs
    // is_concurrent_put_path → wait-then-adopt.
    if status.code() == tonic::Code::Aborted
        && status.message().contains(rio_proto::CONCURRENT_PUTPATH_MSG)
    {
        let mut attempt = 0u32;
        loop {
            attempt += 1;
            // Single-axis {attempt} — shared schema with the buffered
            // lane's emit above (same store-side contention, same
            // dashboard cell; no lane label).
            metrics::counter!(
                "rio_gateway_putpath_aborted_retries_total",
                "attempt" => attempt.to_string(),
            )
            .increment(1);
            if attempt >= PUT_PATH_ABORTED_MAX_ATTEMPTS {
                tracing::warn!(
                    %store_path,
                    attempts = attempt,
                    "PutPath (streaming): concurrent uploader still absent \
                     after wait-then-adopt budget; surfacing"
                );
                return Err(status.into());
            }
            let delay = PUT_PATH_BACKOFF.duration(attempt - 1);
            tracing::debug!(
                %store_path,
                attempt,
                backoff = ?delay,
                "PutPath (streaming): store Aborted (concurrent PutPath); \
                 polling for the concurrent uploader's result"
            );
            tokio::time::sleep(delay).await;
            if grpc_query_path_info(store_client, jwt_token, store_path.as_str())
                .await?
                .is_some()
            {
                tracing::debug!(
                    %store_path,
                    attempt,
                    "PutPath (streaming): adopted concurrent uploader's result"
                );
                return Ok(false);
            }
        }
    }
    Err(status.into())
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
/// `GRPC_STREAM_TIMEOUT` is a per-chunk IDLE bound (I-211), not a
/// whole-call deadline — a 4 GiB NAR completes as long as chunks keep
/// arriving.
pub(crate) async fn grpc_get_path(
    store_client: &mut StoreServiceClient<Channel>,
    jwt_token: Option<&str>,
    store_path: &str,
) -> anyhow::Result<Option<(ValidatedPathInfo, Vec<u8>)>> {
    use rio_proto::client::NarCollectError;
    let md = jwt_metadata(jwt_token);
    // r[impl gw.store.transient-retry]
    // Store-side `GetPath` traverses `try_substitute_on_miss` on a local
    // miss; admission rejection arrives as `NarCollectError::Stream(RE)`
    // before any chunks flow, so a retry replays cleanly (no bytes
    // consumed). `SizeExceeded`/`Validation`/`Io` are non-transient.
    let mut attempt = 0u32;
    loop {
        match rio_proto::client::get_path_nar(
            store_client,
            store_path,
            GRPC_STREAM_TIMEOUT,
            MAX_NAR_SIZE,
            &md,
        )
        .await
        {
            Ok(v) => return Ok(v),
            Err(NarCollectError::Stream(s)) => {
                attempt += 1;
                match transient_retry_after("GetPath", attempt, &s) {
                    Some(delay) => tokio::time::sleep(delay).await,
                    None => {
                        return Err(
                            GatewayError::Store(format!("GetPath for {store_path}: {s}")).into(),
                        );
                    }
                }
            }
            Err(e) => {
                return Err(GatewayError::Store(format!("GetPath for {store_path}: {e}")).into());
            }
        }
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use std::sync::atomic::Ordering::SeqCst;

    /// sha256(b"nar!") — the mock store re-hashes and validates the
    /// trailer like the real one.
    const NAR_FIXTURE: &[u8] = b"nar!";
    const NAR_FIXTURE_SHA256: &str =
        "a5a407e7848d7d3863f1dbf17d78856ef27efce7310082eec2157ee03c7b17f3";

    fn put_info(path: &str) -> rio_proto::validated::ValidatedPathInfo {
        let mut nar_hash = vec![0u8; 32];
        hex::decode_to_slice(NAR_FIXTURE_SHA256, &mut nar_hash).expect("fixture hex");
        rio_proto::types::PathInfo {
            store_path: path.into(),
            nar_hash,
            nar_size: NAR_FIXTURE.len() as u64,
            ..Default::default()
        }
        .try_into()
        .expect("valid fixture path")
    }

    /// W10-BP (merged_bug_097): the store's typed NAR-budget shed
    /// (ResourceExhausted + "retry") is an explicit retry invitation
    /// — the producer's "absorbed by the upload plane's retry
    /// machinery" claim binds to STORE_SHED_CLASSES, and this caller's
    /// classifier must be a superset. Parameterized over the shed
    /// const: the consumer-census leg for the gateway PutPath lane.
    #[tokio::test]
    async fn put_path_absorbs_every_store_shed_class() {
        for (i, shed) in rio_common::grpc::STORE_SHED_CLASSES.into_iter().enumerate() {
            let (store, addr, _h) = rio_test_support::grpc::spawn_mock_store()
                .await
                .expect("mock store");
            match shed {
                tonic::Code::ResourceExhausted => {
                    store.faults.shed_next_puts.store(1, SeqCst);
                }
                tonic::Code::Aborted => {
                    store.faults.abort_next_puts.store(1, SeqCst);
                }
                other => panic!("unscripted shed class {other:?}: extend the mock faults"),
            }
            let mut client = rio_proto::StoreServiceClient::connect(format!("http://{addr}"))
                .await
                .expect("connect");
            let info = put_info(&format!(
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa{i}-shed-1.0"
            ));
            let res = grpc_put_path(&mut client, None, None, info, NAR_FIXTURE.to_vec()).await;
            assert!(
                res.is_ok(),
                "left: the {shed:?} shed hits the terminal arm and the push \
                 hard-fails (the lost invited retry) / right: retried and \
                 absorbed; got {res:?}"
            );
        }
    }

    // r[verify gw.putpath.emit-law]
    /// W10-BQ (merged_bug_038, the H8'' emit law): every failure
    /// class observed at the PutPath chokepoint emits the
    /// class-labeled counter — an injected Unavailable outage must
    /// move the series (pre-fix the only emit arm was Aborted-only,
    /// so the inhibitor trigger was structurally flat during the
    /// reachability outages it guards).
    #[test]
    fn put_path_emit_law_counts_every_failure_class() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (store, addr, _h) = rio_test_support::grpc::spawn_mock_store()
                .await
                .expect("mock store");
            // One Unavailable, then success: the retried observation
            // must still be counted.
            store.faults.fail_next_puts.store(1, SeqCst);
            let mut client = rio_proto::StoreServiceClient::connect(format!("http://{addr}"))
                .await
                .expect("connect");
            let info = put_info("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-emit-1.0");
            let res = grpc_put_path(&mut client, None, None, info, NAR_FIXTURE.to_vec()).await;
            assert!(
                res.is_ok(),
                "one Unavailable then success must absorb; got {res:?}"
            );
        });
        assert_eq!(
            rec.get("rio_gateway_putpath_retry_events_total{class=unavailable}"),
            1,
            "left: the inhibitor series is flat during an Unavailable \
             outage (the uncounted terminal arm) / right: the class-labeled \
             emit law counts it; keys seen: {:?}",
            rec.all_keys()
        );
    }

    /// W11-BH (bug_118): the emit law binds to the OPERATION — the
    /// STREAMING lane's failures must move the same class-labeled
    /// series the buffered lane emits. All non-.drv wopAddToStoreNar
    /// traffic plus oversize AddMultiple route through this lane, so
    /// a streaming-dominated store outage pre-fix left the KEDA
    /// scale-collapse inhibitor FLAT while every failure surfaced via
    /// `pump_result?`/`rpc_result?` with zero emission — the
    /// merged_bug_038 defect re-instantiated one lane over.
    ///
    /// Pre-fix red (the streaming-lane outage, counter flat):
    ///   left: the streaming lane surfaces the store failure with
    ///   ZERO emission (inhibitor flat) / right: every PutPath
    ///   terminal failure increments exactly one class cell on every
    ///   lane: `assertion failed ... left: 0 right: 1`.
    // r[verify gw.putpath.emit-law]
    #[test]
    fn put_path_streaming_failures_emit_the_same_law() {
        let rec = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&rec);
        let rt = tokio::runtime::Builder::new_current_thread()
            .enable_all()
            .build()
            .unwrap();
        rt.block_on(async {
            let (store, addr, _h) = rio_test_support::grpc::spawn_mock_store()
                .await
                .expect("mock store");
            // A store outage on the streaming lane: the put fails
            // Unavailable; the lane is non-retried by design, so the
            // failure is TERMINAL — and must still emit.
            store.faults.fail_next_puts.store(1, SeqCst);
            let mut client = rio_proto::StoreServiceClient::connect(format!("http://{addr}"))
                .await
                .expect("connect");
            let info = put_info("/nix/store/cccccccccccccccccccccccccccccccc-strm-1.0");
            let mut nar_hash = vec![0u8; 32];
            hex::decode_to_slice(NAR_FIXTURE_SHA256, &mut nar_hash).expect("fixture hex");
            let mut reader = std::io::Cursor::new(NAR_FIXTURE.to_vec());
            let res = grpc_put_path_streaming(
                &mut client,
                None,
                None,
                info,
                &mut reader,
                NAR_FIXTURE.len() as u64,
                nar_hash,
            )
            .await;
            assert!(
                res.is_err(),
                "the injected outage must surface; got {res:?}"
            );
        });
        assert_eq!(
            rec.get("rio_gateway_putpath_retry_events_total{class=unavailable}"),
            1,
            "left: the streaming lane surfaces the store failure with ZERO \
             emission (inhibitor flat during streaming-dominated outages) / \
             right: every PutPath terminal failure increments exactly one \
             class cell on every lane; keys seen: {:?}",
            rec.all_keys()
        );
    }
    /// W11-BI (bug_118, [GEN-SET]): the lane census — the emit law's
    /// single-site discipline pinned structurally over THIS module's
    /// source. Generator: the module source itself (include_str!);
    /// the census derives the populations by token scan instead of an
    /// author-typed list, so a new failure-surfacing arm that
    /// bypasses the chokepoint moves a counted population and goes
    /// red here.
    ///
    ///   (1) the metric literal appears exactly ONCE in production
    ///       code (inside `emit_put_path_failure` — the law has one
    ///       emit site);
    ///   (2) every `return Err(` in the two PutPath lane fns routes
    ///       through a surfacing adapter (`return Err(status.into())`
    ///       shapes are gone);
    ///   (3) the streaming lane's terminal `?` arms each name a
    ///       surfacing adapter (metadata-send, task-join, pump, rpc —
    ///       four sites, counted from the source).
    // r[verify gw.putpath.emit-law]
    #[test]
    fn put_path_emit_law_census_one_site_every_lane() {
        let src = include_str!("grpc.rs");
        let prod = &src[..src.find("#[cfg(test)]").expect("test module marker")];

        // (1) one emit site.
        assert_eq!(
            prod.matches("\"rio_gateway_putpath_retry_events_total\"")
                .count(),
            1,
            "the emit law has exactly ONE site (emit_put_path_failure); \
             inline emits of the law's series are the bug_118 shape"
        );

        // Slice the two lane fns (production region order: buffered
        // then streaming then get_path).
        let buf_start = prod
            .find("async fn grpc_put_path(")
            .expect("buffered lane fn");
        let strm_start = prod
            .find("async fn grpc_put_path_streaming")
            .expect("streaming lane fn");
        let strm_end = prod.find("async fn grpc_get_path").unwrap_or(prod.len());
        let buffered = &prod[buf_start..strm_start];
        let streaming = &prod[strm_start..strm_end];

        // (2) no naked terminal returns in either lane: every
        // `return Err(` names a surfacing adapter on the same
        // statement.
        for (lane, body) in [("buffered", buffered), ("streaming", streaming)] {
            for (i, _) in body.match_indices("return Err(") {
                let stmt = &body[i..body[i..].find(';').map(|e| i + e).unwrap_or(body.len())];
                assert!(
                    stmt.contains("surface_put_path_failure") || stmt.contains("status.into()"),
                    "{lane} lane: un-surfaced terminal arm: {stmt}"
                );
            }
            // The buffered lane's `status.into()` returns are lawful:
            // the status was ALREADY surfaced at the loop's single
            // observation point (every failure observation counts,
            // retried or terminal — emitting again at the terminal
            // would double-count).
        }
        assert!(
            buffered.contains("Err(status) => surface_put_path_failure(status)"),
            "buffered lane: the loop's failure observation must route \
             through the surfacing fn"
        );

        // (3) the streaming lane's four terminal arms.
        assert_eq!(
            streaming.matches("surface_put_path_failure").count(),
            4,
            "streaming lane: four terminal arms (metadata-send, \
             task-join, pump, rpc) each surface through the law; a \
             changed count means an arm was added or bypassed — \
             re-derive the census"
        );
    }
}
