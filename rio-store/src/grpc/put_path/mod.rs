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
    IngestAuthority, PlaceholderClaim, apply_trailer, validate_put_metadata, verify_ca_store_path,
    verify_nar,
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
    /// HMAC assignment-token gate — the SOLE producer of
    /// [`IngestAuthority`] (callers must NOT restate the admit
    /// reasons; cite this fn instead — R7-m001 found 5 divergent
    /// restatements):
    /// - `Builder(claims)` — valid `x-rio-assignment-token`; caller
    ///   checks the requested path ∈ `claims.expected_outputs`.
    /// - `ServiceBypass{caller}` — valid `x-rio-service-token` whose
    ///   `caller` ∈ `service_bypass_callers` (gateway/scheduler) → no
    ///   per-build assignment token required. PutPath* accept this;
    ///   AppendHwPerfSample REJECTS it (the divergent policy is a
    ///   visible match arm at each consumer, not an ambiguous None).
    /// - `DevMode` — `hmac_verifier` unset → accept all.
    /// - `Err(PERMISSION_DENIED)` — token missing/invalid/expired, or
    ///   service-token caller not allowlisted.
    pub(super) fn verify_assignment_token<T>(
        &self,
        request: &Request<T>,
    ) -> Result<IngestAuthority, Status> {
        let Some(verifier) = &self.hmac_verifier else {
            // Verifier not configured = dev mode, accept all.
            return Ok(IngestAuthority::DevMode);
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
                        "caller" => claims.caller.clone(),
                    )
                    .increment(1);
                    return Ok(IngestAuthority::ServiceBypass {
                        caller: claims.caller,
                    });
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
            Some(t) => verifier
                .verify(t)
                .map(IngestAuthority::Builder)
                .map_err(|e| {
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

        let (raw_info, declared) = common::read_first_metadata(&mut stream).await?;
        // r[impl store.put.declared-reserve]
        // r[impl store.budget.cost-axis]
        // N1 declared-mode gate: bound the declaration BEFORE the
        // claim (a hostile declaration must not cost a placeholder
        // round-trip). The reservation itself happens inside
        // `ingest_nar_stream` — after the IA claim, before the first
        // chunk — so the park holds no budget and the claim/park
        // ordering matches the substitute leg (claim, then reserve).
        //
        // DECLARED-MODE AUTHORITY (merged_bug_005): declared mode is
        // open to every token-holding caller, but its COST is typed —
        // the reservation charges the HMAC-claims-signed tenant (or
        // the capped unattributed bucket) via `DeclaredCharge`, so
        // authority-to-cost is total over `IngestAuthority`:
        // Builder(tenant) → that tenant's 8 GiB aggregate;
        // Builder(tenant-less) / ServiceBypass / DevMode → the shared
        // capped nil bucket. The is_ca face is the priced residual
        // (RC-2(iii)): is_ca callers claim POST-ingest, so their
        // claims never bound concurrent declared streams — each
        // stream still pays the ledger, so the un-bounded STREAM
        // COUNT pins at most TENANT_RESERVATION_CAP bytes per signed
        // tenant, never the pool.
        //
        // `declared_nar_size` sink census (every consumer of the wire
        // field, RC-2(iii) — each priced or refusing):
        //   1. producer: rio-proto client::store::chunk_nar_for_put
        //      (:247 — buffered senders declare; trailer must equal);
        //   2. extraction: common.rs read_first_metadata
        //      (`> 0` opts in);
        //   3. THIS pre-claim size gate (refuses >= MAX_NAR_SIZE);
        //   4. the cost-axis reservation: reserve_declared →
        //      budget::DeclaredCharge (ledger + cap BEFORE any grant);
        //   5. the mid-stream binding bound (over-delivery refuses at
        //      the crossing chunk);
        //   6. the trailer equality (under/lying-short refuse);
        //   7. PutPathBatch REJECTS the field outright (not supported
        //      on the batch plane — a refusing sink).
        if let Some(d) = declared
            && d >= rio_common::limits::MAX_NAR_SIZE
        {
            drain_stream(&mut stream).await;
            return Err(Status::invalid_argument(format!(
                "PutPath: declared_nar_size {d} exceeds size bound {}",
                rio_common::limits::MAX_NAR_SIZE
            )));
        }
        let mut info = validate_put_metadata(raw_info, auth.builder_claims(), "PutPath")?;
        // Server-derived in validate_put_metadata (step 7) — never the
        // wire value. r[sec.boundary.grpc-hmac].
        let store_path_hash = info.store_path_hash.clone();
        debug!(store_path = %info.store_path.as_str(), "PutPath: received metadata");

        // r[impl sec.authz.ca-path-derived+2]
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
        let is_ca_caller = auth.builder_claims().is_some_and(|c| c.is_ca);

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

        let (nar_data, hold) = match self
            .ingest_nar_stream(&mut stream, &mut info, auth.builder_claims(), declared)
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
        // drain_stream in the non-Owned arms. bug_114: this PG
        // round-trip runs while the handler HOLDS budget permits — it
        // rides the hold's one envelope clock, never a bare await
        // (pre-fix it sat in the gap between the stream clock and the
        // persist clock with no deadline at all).
        if claim.is_none() {
            let claiming =
                self.claim_or_return(&store_path_hash, info.store_path.as_str(), &refs_str);
            let claimed = match &hold {
                Some(h) => h.bounded("PutPath ca-claim", claiming).await??,
                None => claiming.await?,
            };
            match claimed {
                Some(c) => {
                    placeholder_guard =
                        Some(self.spawn_placeholder_guard(store_path_hash.clone(), c));
                    claim = Some(c);
                }
                None => return Ok(Response::new(PutPathResponse { created: false })),
            }
        }
        let claim = claim.expect("claim populated in IA pre-ingest or CA post-ingest arm");

        self.finalize_single(info, claim, nar_data, auth.tenant_id, hold.as_ref())
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
