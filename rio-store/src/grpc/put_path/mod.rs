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
pub(super) use common::{PlaceholderClaim, apply_trailer, validate_put_metadata, verify_nar};

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
    /// Verify the `x-rio-assignment-token` metadata header.
    ///
    /// Returns:
    /// - `Ok(None)` if verifier disabled (dev mode) → no check
    /// - `Ok(None)` if service-token bypass (caller in allowlist) → no check
    /// - `Ok(Some(claims))` if assignment token valid → caller checks path ∈ claims
    /// - `Err(PERMISSION_DENIED)` if token missing/invalid/expired
    pub(super) fn verify_assignment_token<T>(
        &self,
        request: &Request<T>,
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
                        .any(|a| a == &claims.caller) =>
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
                        "service-token caller {:?} not in allowlist",
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
        let store_path_hash = if info.store_path_hash.is_empty() {
            info.store_path.sha256_digest().to_vec()
        } else {
            info.store_path_hash.clone()
        };
        debug!(store_path = %info.store_path.as_str(), "PutPath: received metadata");

        let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
        match self
            .claim_placeholder(
                &store_path_hash,
                info.store_path.as_str(),
                &refs_str,
                "PutPath",
            )
            .await
        {
            Ok(PlaceholderClaim::Owned) => {}
            Ok(PlaceholderClaim::AlreadyComplete) => {
                debug!(store_path = %info.store_path.as_str(), "PutPath: path already complete");
                drain_stream(&mut stream).await;
                return Ok(Response::new(PutPathResponse { created: false }));
            }
            Ok(PlaceholderClaim::Concurrent) => {
                drain_stream(&mut stream).await;
                if let Ok(true) =
                    metadata::check_manifest_complete(&self.pool, &store_path_hash).await
                {
                    debug!(store_path = %info.store_path, "PutPath: concurrent upload won the race");
                    metrics::counter!("rio_store_put_path_total", "result" => "exists")
                        .increment(1);
                    return Ok(Response::new(PutPathResponse { created: false }));
                }
                debug!(store_path = %info.store_path, "PutPath: concurrent upload in progress, aborting");
                return Err(Status::aborted(
                    "concurrent PutPath in progress for this path; retry",
                ));
            }
            Err(e) => {
                drain_stream(&mut stream).await;
                return Err(putpath_metadata_status("PutPath: claim_placeholder", e));
            }
        }

        let placeholder_guard = self.spawn_placeholder_guard(store_path_hash.clone());

        let (nar_data, _held_permits) = match self.ingest_nar_stream(&mut stream, &mut info).await {
            Ok(x) => x,
            Err(e) => {
                self.abort_upload(&store_path_hash).await;
                return Err(e);
            }
        };

        info.store_path_hash = store_path_hash;
        self.finalize_single(info, nar_data, auth.tenant_id).await?;
        scopeguard::ScopeGuard::into_inner(placeholder_guard);
        Ok(Response::new(PutPathResponse { created: true }))
    }
}
