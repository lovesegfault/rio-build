//! Mechanics shared across the upload pipeline (and a few callers
//! outside it): bounded joins for `spawn_blocking` disk work, the
//! assignment-token header, retry constants, and result construction.
//!
//! Everything here is `pub(crate)`/`pub(super)` — the public surface is
//! [`upload_all_outputs`](super::upload_all_outputs).

use std::time::Duration;

use rio_common::backoff::{Backoff, Jitter};
use rio_nix::store_path::StorePath;
use rio_proto::validated::ValidatedPathInfo;

use super::UploadError;

/// Post-rx-drop join window: covers scheduler latency for the
/// "rx-drop observed" case and bounds the "parked in syscall" case to a
/// value the operator will see before pod deadlines fire
/// (`activeDeadlineSeconds` etc.).
pub(crate) const DUMP_JOIN_SLACK: Duration = Duration::from_secs(30);

/// Await a `spawn_blocking` dump `JoinHandle` with a deadline. On timeout
/// the blocking thread leaks (tokio limitation — `spawn_blocking` is
/// non-abortable); the worker regains control and fails the build instead
/// of hanging.
///
/// **Use ONLY when this is the operation's primary timeout** (no
/// preceding bounded await ran concurrently with the handle): `budget`
/// is the operation's own timeout, [`DUMP_JOIN_SLACK`] is added on top.
/// If a bounded gRPC await has already run concurrently with the dump,
/// use [`await_dump_after_rx_drop`] instead — re-waiting `budget` here
/// would double-count it (the dump task already had `budget` of
/// wall-clock).
///
/// This guard is for the case where the blocking thread is parked in
/// `open(2)`/`read(2)` (FIFO in `$out`, wedged FUSE/overlay, suspended dm
/// device) and never reaches a channel send to observe rx-drop.
pub(crate) async fn await_dump_bounded<T>(
    what: &'static str,
    budget: Duration,
    handle: tokio::task::JoinHandle<T>,
) -> Result<T, tonic::Status> {
    let deadline = budget + DUMP_JOIN_SLACK;
    tokio::time::timeout(deadline, handle)
        .await
        .map_err(|_| {
            tonic::Status::deadline_exceeded(format!(
                "{what} stuck after {}s (disk read hung? FIFO in $out?)",
                deadline.as_secs()
            ))
        })?
        .map_err(|e| tonic::Status::internal(format!("{what} panicked: {e}")))
}

/// Await a dump `JoinHandle` AFTER its consumer `rx` has been dropped
/// (i.e. after a bounded gRPC await that ran concurrently with the dump
/// has returned). Waits only [`DUMP_JOIN_SLACK`].
///
/// At this point the dump task is in exactly one of:
///   (a) **finished** — joins instantly;
///   (b) **at a `blocking_send`/`tx.send`** — sees rx-drop, returns in ms;
///   (c) **parked in `open(2)`/`read(2)`** — never progresses; no finite
///       timeout helps it complete.
///
/// Nothing needs the gRPC budget (the dump already had that wall-clock
/// concurrently); only the slack is meaningful. Re-waiting the budget
/// here doubles the wedged-read hang — long enough to exceed
/// `activeDeadlineSeconds`, so the pod is SIGKILLed before the
/// diagnostic ever reaches the CompletionReport.
pub(crate) async fn await_dump_after_rx_drop<T>(
    what: &'static str,
    handle: tokio::task::JoinHandle<T>,
) -> Result<T, tonic::Status> {
    tokio::time::timeout(DUMP_JOIN_SLACK, handle)
        .await
        .map_err(|_| {
            tonic::Status::deadline_exceeded(format!(
                "{what} stuck {}s after rx dropped (disk read hung? FIFO in $out?)",
                DUMP_JOIN_SLACK.as_secs()
            ))
        })?
        .map_err(|e| tonic::Status::internal(format!("{what} panicked: {e}")))
}

/// Maximum number of upload retry attempts. Aligned with the
/// gateway's PutPath retry (`rio-gateway/src/handler/grpc.rs`): both
/// hit the same store-side placeholder contention (I-068/I-125b), so
/// they share curve+budget. 8 attempts × full-jitter ≤~6 s — was 3
/// attempts × no-jitter, which thundering-herded under deep-256x.
///
/// The `Aborted: concurrent PutPath` placeholder contention shares this
/// budget: each such failure re-runs the FindMissingPaths pre-check, so
/// a concurrent uploader that has since finished is adopted as an
/// idempotent skip without burning the remaining attempts; one still in
/// flight waits on the store's drop-path cleanup to release the stale
/// placeholder within the retry window.
pub(crate) const MAX_UPLOAD_RETRIES: u32 = 8;

/// Upload retry curve. See [`MAX_UPLOAD_RETRIES`] for the
/// gateway-alignment rationale.
pub(super) const UPLOAD_BACKOFF: Backoff = Backoff {
    base: Duration::from_millis(50),
    mult: 2.0,
    cap: Duration::from_secs(2),
    jitter: Jitter::Full,
};

/// Channel buffer between the blocking chunk reader (spawn_blocking)
/// and the async gRPC send. At `FASTCDC_MAX_BYTES` (256 KiB) per chunk
/// frame this is ~1 MiB of backpressure headroom — enough to absorb
/// jitter between disk read and network send without blocking either
/// side for long.
pub(super) const STREAM_CHANNEL_BUF: usize = 4;

/// Construct a `ValidatedPathInfo` for a freshly-uploaded output.
///
/// `references` are full `/nix/store/...` paths from the ref-scan
/// candidate set, which was built from already-validated input-closure
/// paths plus declared output paths — `StorePath::parse` cannot fail on
/// them. A parse failure here is an invariant violation (CandidateSet
/// returned a path it didn't validate) and is surfaced as
/// [`UploadError::InvalidReference`] rather than silently dropped:
/// dropping would corrupt the output's reference graph and break GC
/// reachability. `deriver` may be empty (dev mode), which maps to
/// `None`. Fields not known at upload time (`registration_time`,
/// `signatures`, `content_address`, …) are left default; the store
/// fills them server-side.
pub(super) fn uploaded_info(
    store_path: StorePath,
    nar_hash: [u8; 32],
    nar_size: u64,
    references: Vec<String>,
    deriver: &str,
) -> Result<ValidatedPathInfo, UploadError> {
    let references = references
        .into_iter()
        .map(|r| StorePath::parse(&r).map_err(|_| UploadError::InvalidReference { path: r }))
        .collect::<Result<Vec<_>, _>>()?;
    Ok(ValidatedPathInfo {
        store_path,
        store_path_hash: Vec::new(),
        deriver: StorePath::parse(deriver).ok(),
        nar_hash,
        nar_size,
        references,
        registration_time: 0,
        ultimate: false,
        signatures: Vec::new(),
        content_address: None,
    })
}

/// Attach the assignment token as `x-rio-assignment-token` gRPC
/// metadata. Store with `hmac_verifier` set will check it; store
/// without = ignore (the header is just extra metadata). Empty token =
/// no header (scheduler without `hmac_signer`, dev mode).
///
/// `parse()` for `AsciiMetadataValue` — assignment tokens are
/// base64url.base64url, always ASCII. If parse fails (non-ASCII bytes
/// somehow — scheduler bug or memory corruption), the store WILL reject
/// the upload with `PermissionDenied` when `hmac_verifier` is set.
/// Silently omitting the header would turn that into a confusing
/// "rejected, no token" error with no worker-side trace; fail loud
/// instead.
pub(crate) fn attach_assignment_token<T>(
    req: &mut tonic::Request<T>,
    assignment_token: &str,
) -> Result<(), tonic::Status> {
    rio_proto::interceptor::inject_current(req.metadata_mut());
    if assignment_token.is_empty() {
        return Ok(());
    }
    match assignment_token.parse() {
        Ok(v) => {
            req.metadata_mut()
                .insert(rio_proto::ASSIGNMENT_TOKEN_HEADER, v);
            Ok(())
        }
        Err(_) => {
            tracing::error!(
                token_len = assignment_token.len(),
                "assignment token failed MetadataValue parse — upload will be rejected"
            );
            Err(tonic::Status::invalid_argument(
                "assignment token is not a valid ASCII metadata value",
            ))
        }
    }
}
