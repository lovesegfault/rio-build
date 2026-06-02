//! PutPath: upload a store path (streaming NAR data with metadata).
//!
//! Write-ahead flow:
//! 1. Receive first message: PutPathMetadata with PathInfo
//! 2. Check idempotency: if path already complete, return success
// r[impl store.put.wal-manifest]
// r[impl store.put.idempotent]
// r[impl store.integrity.verify-on-put]
//! 3. Insert manifest placeholder with status='uploading'
//! 4. Accumulate NAR chunks (bounded by MAX_NAR_SIZE)
//! 5. Verify SHA-256 matches trailer's declared nar_hash
//! 6. Store NAR (inline or chunked) + flip status to 'complete'
//!
//! Trailer-only: metadata.nar_hash MUST be empty; hash arrives in the
//! PutPathTrailer after all chunks (stream-and-hash — the client doesn't
//! know the hash until after transmitting all bytes).

use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, warn};

use rio_proto::types::{PutPathRequest, PutPathResponse};

use crate::metadata;

use super::{StoreServiceImpl, putpath_metadata_status};

pub(super) mod common;
pub(super) use common::{
    PlaceholderClaim, apply_trailer, validate_put_metadata, verify_ca_store_path,
    verify_drv_text_path, verify_nar,
};

/// Drain remaining messages from a streaming request.
///
/// Must be called before returning early from PutPath to avoid leaving
/// unconsumed data on the gRPC transport. Bounded by DEFAULT_GRPC_TIMEOUT
/// to prevent a slow client from holding the handler indefinitely.
async fn drain_stream(stream: &mut Streaming<PutPathRequest>) {
    let drain = async {
        while let Ok(Some(_)) = stream.message().await {
            // discard
        }
    };
    if tokio::time::timeout(rio_common::grpc::DEFAULT_GRPC_TIMEOUT, drain)
        .await
        .is_err()
    {
        warn!(
            timeout = ?rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            "drain_stream timed out; client may be sending slowly"
        );
    }
}

impl StoreServiceImpl {
    /// HMAC assignment-token gate. Returns:
    /// - `Ok(Some(claims))` — valid `x-rio-assignment-token`; caller
    ///   checks the requested path ∈ `claims.expected_outputs`.
    /// - `Ok(None)` — TWO distinct reasons (callers must NOT restate;
    ///   cite this fn instead — R7-m001 found 5 divergent restatements):
    ///     1. **dev-mode**: `hmac_verifier` unset → accept all.
    ///     2. **service-token bypass**: valid `x-rio-service-token`
    ///        whose `caller` ∈ `service_bypass_callers`
    ///        (gateway/scheduler) → no per-build assignment token
    ///        required.
    /// - `Err(PERMISSION_DENIED)` — token missing/invalid/expired, or
    ///   service-token caller not allowlisted.
    pub(super) fn verify_assignment_token<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_auth::hmac::AssignmentClaims>, Status> {
        self.verify_assignment_token_inner(request, false)
    }

    // r[impl store.put.ia-deriver-proof+2]
    /// PutPath/PutPathBatch variant of [`Self::verify_assignment_token`]:
    /// the WRITE side. The scheduler's service token is NOT a bypass
    /// here — the scheduler has no PutPath flow (its token serves the
    /// read-side substitution probe), and honoring it would let a
    /// scheduler credential skip every registration gate including the
    /// IA deriver proof. Capability split, not a knob: hardcoded,
    /// unconditional.
    pub(super) fn verify_assignment_token_put<T>(
        &self,
        request: &Request<T>,
    ) -> Result<Option<rio_auth::hmac::AssignmentClaims>, Status> {
        self.verify_assignment_token_inner(request, true)
    }

    fn verify_assignment_token_inner<T>(
        &self,
        request: &Request<T>,
        put_side: bool,
    ) -> Result<Option<rio_auth::hmac::AssignmentClaims>, Status> {
        let Some(verifier) = &self.hmac_verifier else {
            // Verifier not configured = dev mode, accept all.
            return Ok(None);
        };

        // Service-token bypass (transport-agnostic — works over
        // plaintext-on-WireGuard). A valid token whose `caller` is in
        // the allowlist short-circuits — no assignment token required.
        if let Some(sv) = &self.service_verifier
            && let Some(tok) = request
                .metadata()
                .get(rio_proto::SERVICE_TOKEN_HEADER)
                .and_then(|v| v.to_str().ok())
        {
            match sv.verify::<rio_auth::hmac::ServiceClaims>(tok) {
                Ok(claims)
                    if self
                        .service_bypass_callers
                        .iter()
                        .any(|a| a == &claims.caller)
                        && !(put_side && claims.caller == "rio-scheduler") =>
                {
                    metrics::counter!(
                        "rio_store_service_token_accepted_total",
                        "caller" => claims.caller,
                    )
                    .increment(1);
                    return Ok(None);
                }
                Ok(claims) => {
                    metrics::counter!("rio_store_hmac_rejected_total",
                                     "reason" => "service_caller_not_allowlisted")
                    .increment(1);
                    return Err(Status::permission_denied(format!(
                        "service-token caller {:?} has no bypass for this method \
                         (scheduler tokens carry probe rights only; PutPath requires \
                         a per-build assignment token)",
                        claims.caller
                    )));
                }
                Err(e) => {
                    metrics::counter!("rio_store_hmac_rejected_total",
                                     "reason" => "service_token_invalid")
                    .increment(1);
                    return Err(Status::permission_denied(format!("service token: {e}")));
                }
            }
        }

        let token = request
            .metadata()
            .get(rio_proto::ASSIGNMENT_TOKEN_HEADER)
            .and_then(|v| v.to_str().ok());

        match token {
            Some(t) => verifier.verify(t).map(Some).map_err(|e| {
                warn!(error = %e, "PutPath: assignment token verification failed");
                metrics::counter!("rio_store_hmac_rejected_total", "reason" => "invalid_token")
                    .increment(1);
                Status::permission_denied(format!("assignment token: {e}"))
            }),
            None => {
                metrics::counter!("rio_store_hmac_rejected_total", "reason" => "missing_token")
                    .increment(1);
                Err(Status::permission_denied(format!(
                    "assignment token required ({} header)",
                    rio_proto::ASSIGNMENT_TOKEN_HEADER
                )))
            }
        }
    }

    pub(super) async fn put_path_impl(
        &self,
        request: Request<Streaming<PutPathRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();
        let _duration_guard = scopeguard::guard((), move |()| {
            metrics::histogram!("rio_store_put_path_duration_seconds")
                .record(start.elapsed().as_secs_f64());
        });

        let auth = self.authorize(&request)?;
        let mut stream = request.into_inner();

        let raw_info = common::read_first_metadata(&mut stream).await?;
        let mut info = validate_put_metadata(raw_info, auth.hmac_claims.as_ref(), "PutPath")?;
        // Server-derived in validate_put_metadata (step 7) — never the
        // wire value. r[sec.boundary.grpc-hmac].
        let store_path_hash = info.store_path_hash.clone();
        debug!(store_path = %info.store_path.as_str(), "PutPath: received metadata");

        // r[impl sec.authz.ca-path-derived+9]
        // For is_ca tokens, validate_put_metadata skips the
        // `store_path ∈ expected_outputs` membership check (the path is
        // content-derived). The CA authorization gate —
        // `verify_ca_store_path` inside `ingest_nar_stream` — needs the
        // full NAR, so it runs AFTER stream drain. Claiming the
        // placeholder BEFORE that gate would let an is_ca worker squat
        // an `'uploading'` row for ANY syntactically-valid path
        // (including other tenants' IA outputs), drip-feed chunks while
        // `spawn_placeholder_guard` heartbeats it fresh, and force
        // legitimate uploaders into `Aborted` until token expiry. So:
        // IA → claim BEFORE ingest (path is HMAC-bound); CA → claim
        // AFTER ingest (path is now content-bound). Matches
        // PutPathBatch's verify-then-claim ordering.
        //
        // Fixed-output uploads (is_ca=false WITH a `fixed:` descriptor)
        // follow the IA ordering: membership already restricts the
        // claimable paths to this worker's own assignment, and the CA
        // gate still runs post-ingest — a content/path mismatch aborts
        // the upload and releases the placeholder before finalize.
        let is_ca_caller = auth.hmac_claims.as_ref().is_some_and(|c| c.is_ca);

        let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
        let (mut claim, mut placeholder_guard) = if is_ca_caller {
            (None, None)
        } else {
            match self
                .claim_or_return(&store_path_hash, info.store_path.as_str(), &refs_str)
                .await
            {
                Ok(Some(c)) => {
                    let g = self.spawn_placeholder_guard(store_path_hash.clone(), c);
                    // r[impl store.put.ia-deriver-proof+2]
                    // The proof gate runs ONLY for uploads that own a
                    // fresh placeholder (idempotency precedence): an
                    // already-complete path returned `created: false`
                    // above without consulting the proof — re-upload of
                    // complete content is governed by idempotency, and
                    // demanding a registration proof for a no-op
                    // re-registration rejected honest re-uploads whose
                    // deriver was registered claimslessly (bug_092,
                    // fix-child of 8930770dd — pattern R5: the gate made
                    // lifecycle position load-bearing without a
                    // population audit). Proof failure releases the
                    // placeholder before propagating, so a denied upload
                    // leaves no `'uploading'` squat behind.
                    if let Err(e) = self
                        .verify_ia_registration_proof(&info, auth.hmac_claims.as_ref(), "PutPath")
                        .await
                    {
                        g.defuse();
                        self.abort_upload(&store_path_hash, c).await;
                        drain_stream(&mut stream).await;
                        return Err(e);
                    }
                    (Some(c), Some(g))
                }
                Ok(None) => {
                    drain_stream(&mut stream).await;
                    return Ok(Response::new(PutPathResponse { created: false }));
                }
                Err(e) => {
                    drain_stream(&mut stream).await;
                    return Err(e);
                }
            }
        };

        let (nar_data, _held_permits) = match self
            .ingest_nar_stream(&mut stream, &mut info, auth.hmac_claims.as_ref())
            .await
        {
            Ok(x) => x,
            Err(e) => {
                if let Some(c) = claim {
                    self.abort_upload(&store_path_hash, c).await;
                }
                return Err(e);
            }
        };

        // CA: NOW claim — verify_ca_store_path has bound store_path to
        // server-hashed content. Stream is already drained, so no
        // drain_stream in the non-Owned arms.
        if claim.is_none() {
            match self
                .claim_or_return(&store_path_hash, info.store_path.as_str(), &refs_str)
                .await?
            {
                Some(c) => {
                    placeholder_guard =
                        Some(self.spawn_placeholder_guard(store_path_hash.clone(), c));
                    claim = Some(c);
                }
                None => return Ok(Response::new(PutPathResponse { created: false })),
            }
        }
        let claim = claim.expect("claim populated in IA pre-ingest or CA post-ingest arm");

        self.finalize_single(info, claim, nar_data, auth.tenant_id)
            .await?;
        if let Some(g) = placeholder_guard {
            g.defuse();
        }
        Ok(Response::new(PutPathResponse { created: true }))
    }

    /// `claim_placeholder` + the AlreadyComplete/Concurrent fast-path
    /// arms, factored so the IA (pre-ingest) and CA (post-ingest) call
    /// sites share one match. Returns:
    ///   - `Ok(Some(claim))` → we own the placeholder; proceed.
    ///   - `Ok(None)` → path is already complete (or a concurrent
    ///     uploader just won) → caller returns `created: false`.
    ///   - `Err(Aborted)` → concurrent `'uploading'` placeholder held by
    ///     someone else; caller propagates so client retries.
    ///
    /// Does NOT drain the stream — IA caller drains on `Ok(None)`/`Err`
    /// (stream not yet read); CA caller doesn't (stream already drained
    /// by `ingest_nar_stream`).
    async fn claim_or_return(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        refs: &[String],
    ) -> Result<Option<uuid::Uuid>, Status> {
        match self
            .claim_placeholder(store_path_hash, store_path, refs, "PutPath")
            .await
        {
            Ok(PlaceholderClaim::Owned(claim)) => Ok(Some(claim)),
            Ok(PlaceholderClaim::AlreadyComplete) => {
                debug!(%store_path, "PutPath: path already complete");
                Ok(None)
            }
            Ok(PlaceholderClaim::Concurrent) => {
                if let Ok(true) =
                    metadata::check_manifest_complete(&self.pool, store_path_hash).await
                {
                    debug!(%store_path, "PutPath: concurrent upload won the race");
                    metrics::counter!("rio_store_put_path_total", "result" => "exists")
                        .increment(1);
                    return Ok(None);
                }
                debug!(%store_path, "PutPath: concurrent upload in progress, aborting");
                Err(Status::aborted(format!(
                    "{} for this path; retry",
                    rio_proto::CONCURRENT_PUTPATH_MSG
                )))
            }
            Err(e) => Err(putpath_metadata_status("PutPath: claim_placeholder", e)),
        }
    }
}
