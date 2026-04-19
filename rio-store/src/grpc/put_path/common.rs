//! Shared PutPath / PutPathBatch flow steps.
//!
//! Both upload RPCs walk the same write-ahead state machine:
//!
//! 1. **authorize** — HMAC token check + JWT tenant extraction
//!    ([`StoreServiceImpl::authorize`]; per-path allowlist applied in
//!    [`validate_put_metadata`])
//! 2. **claim_placeholder** — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//!    ([`StoreServiceImpl::claim_placeholder`])
//! 3. **ingest** — accumulate NAR bytes ([`StoreServiceImpl::accumulate_chunk`]),
//!    apply trailer ([`apply_trailer`]), verify SHA-256 ([`verify_nar`]).
//!    PutPath drains a linear stream via
//!    [`StoreServiceImpl::ingest_nar_stream`]; PutPathBatch demuxes by
//!    `output_index` then verifies each.
//! 4. **finalize** — sign + persist inline-or-chunked
//!    ([`StoreServiceImpl::finalize_single`] for the standalone path;
//!    PutPathBatch stages then commits in one tx)
//!
//! These were duplicated across `put_path_impl` and
//! `put_path_batch_impl` and had already drifted once (batch lacked
//! chunked support). Factoring here keeps both impls thin wrappers
//! around the same state machine.

use bytes::Bytes;
use tonic::{Request, Status, Streaming};
use tracing::{error, warn};

use rio_proto::types::{PutPathRequest, PutPathTrailer, put_path_request};
use rio_proto::validated::ValidatedPathInfo;

use rio_common::grpc::StatusExt;
use rio_common::limits::MAX_NAR_SIZE;

use crate::cas;
use crate::grpc::{StoreServiceImpl, putpath_metadata_status, storage_error};
use crate::ingest;
use crate::metadata;

/// Re-export of [`crate::ingest::PlaceholderClaim`] — the write-ahead
/// state machine lives in `ingest` (shared with `Substituter`); this
/// keeps existing `grpc::` callers' paths stable.
pub(in crate::grpc) use crate::ingest::PlaceholderClaim;

/// gRPC PutPath/PutPathBatch hooks for the shared ingest core.
const PUTPATH_HOOKS: ingest::IngestHooks = ingest::IngestHooks {
    stale_reclaimed_metric: "rio_store_putpath_stale_reclaimed_total",
    ctx_label: "PutPath",
};

/// How the NAR was persisted. Batch uses this to pick the right
/// `complete_manifest_*_in_tx` variant inside its atomic tx.
pub(in crate::grpc) enum NarPersist {
    /// `nar_data.len() < INLINE_THRESHOLD` (or no chunk backend).
    /// Bytes carried so the batch tx can write `inline_blob`.
    Inline(Bytes),
    /// `nar_data.len() >= INLINE_THRESHOLD` and a chunk backend is
    /// configured. Chunks already uploaded + refcounted via
    /// [`cas::stage_chunked`]; only the `status='complete'` flip
    /// remains.
    ChunkedStaged,
}

/// Auth context for PutPath / PutPathBatch: HMAC assignment claims
/// (path allowlist) + JWT tenant id (signing-key selection). Both
/// extracted from request metadata BEFORE `into_inner()` consumes it.
pub(in crate::grpc) struct PutAuth {
    pub hmac_claims: Option<rio_auth::hmac::AssignmentClaims>,
    pub tenant_id: Option<uuid::Uuid>,
}

/// Validate a raw PathInfo message for PutPath/PutPathBatch.
///
/// Shared validation shared by both upload RPCs: (1) nar_hash-empty
/// enforcement (trailer-only mode), (2) references bound,
/// (3) signatures bound, (4) placeholder hash fill,
/// (5) ValidatedPathInfo::try_from, (6) HMAC path-in-claims check.
///
/// `ctx_label` goes into error messages for client-side disambiguation
/// ("PutPath" vs "output N").
///
/// Returns the validated info; on HMAC path-not-in-claims failure,
/// increments the `hmac_rejected_total{reason=path_not_in_claims}`
/// counter before erroring.
// r[impl sec.boundary.grpc-hmac]
pub(in crate::grpc) fn validate_put_metadata(
    mut raw_info: rio_proto::types::PathInfo,
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<ValidatedPathInfo, Status> {
    // Step 1: trailer-only enforcement. metadata.nar_hash must be empty;
    // hash arrives in the PutPathTrailer after all chunks. Both gateway
    // (chunk_nar_for_put) and worker (single-pass tee upload) send
    // trailers. A non-empty nar_hash means an un-updated client.
    if !raw_info.nar_hash.is_empty() {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: metadata.nar_hash must be empty (hash-upfront mode removed; \
             send hash in PutPathTrailer)"
        )));
    }

    // Step 4: placeholder so TryFrom passes (it hard-fails on empty
    // nar_hash). Overwritten after trailer. 32 zero bytes — unambiguously
    // NOT a real SHA-256 (would be the hash of a specific ~2^256-rare
    // preimage). nar_size is also 0 here — real value from trailer.
    raw_info.nar_hash = vec![0u8; 32];

    // Steps 2-3: bound repeated fields BEFORE per-element validation
    // (TryFrom validates each reference's syntax but doesn't bound the
    // count; an attacker could send 10M valid references and we'd parse
    // them all before failing).
    rio_common::grpc::check_bound(
        "references",
        raw_info.references.len(),
        rio_common::limits::MAX_REFERENCES,
    )?;
    rio_common::grpc::check_bound(
        "signatures",
        raw_info.signatures.len(),
        rio_common::limits::MAX_SIGNATURES,
    )?;

    // Step 5: centralized validation — store_path parses, nar_hash is
    // 32 bytes (placeholder), each reference parses.
    let info = ValidatedPathInfo::try_from(raw_info).status_invalid(ctx_label)?;

    // Step 6: HMAC path-in-claims check. None = verifier disabled OR
    // mTLS bypass (gateway) → no check. Floating-CA (claims.is_ca) →
    // skip the membership check here: the output path is computed
    // post-build from the NAR hash, so expected_outputs is [""] at
    // sign time. Authorization for the CA case is enforced AFTER the
    // NAR is hashed by `verify_ca_store_path` (r[sec.authz.ca-path-
    // derived]): the store recomputes the CA path from the
    // server-verified `nar_hash` and rejects on mismatch.
    if let Some(claims) = hmac_claims
        && !claims.is_ca
    {
        let path_str = info.store_path.as_str();
        if !claims.expected_outputs.iter().any(|o| o == path_str) {
            warn!(
                store_path = %path_str,
                executor_id = %claims.executor_id,
                drv_hash = %claims.drv_hash,
                "{ctx_label}: path not in assignment's expected_outputs",
            );
            metrics::counter!(
                "rio_store_hmac_rejected_total",
                "reason" => "path_not_in_claims"
            )
            .increment(1);
            return Err(Status::permission_denied(format!(
                "{ctx_label}: path not authorized by assignment token"
            )));
        }
    }

    Ok(info)
}

/// Apply a PutPathTrailer to a ValidatedPathInfo: 32-byte hash check,
/// nar_size bound, then overwrite the placeholder hash+size on `info`.
/// Caller handles async cleanup on error (abort_upload / bail!).
/// Callers that need the hash read `info.nar_hash` after the call.
pub(in crate::grpc) fn apply_trailer(
    info: &mut ValidatedPathInfo,
    t: &PutPathTrailer,
    ctx_label: &str,
) -> Result<(), Status> {
    let hash: [u8; 32] = t.nar_hash.as_slice().try_into().map_err(|_| {
        Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_hash must be 32 bytes (SHA-256), got {}",
            t.nar_hash.len()
        ))
    })?;
    if t.nar_size > MAX_NAR_SIZE {
        return Err(Status::invalid_argument(format!(
            "{ctx_label}: trailer nar_size {} exceeds maximum {MAX_NAR_SIZE}",
            t.nar_size
        )));
    }
    info.nar_hash = hash;
    info.nar_size = t.nar_size;
    Ok(())
}

/// Read the first PutPath message; must be `Metadata` carrying a
/// `PathInfo`. Shared step-1 of the write-ahead flow.
pub(in crate::grpc) async fn read_first_metadata(
    stream: &mut Streaming<PutPathRequest>,
) -> Result<rio_proto::types::PathInfo, Status> {
    let first = stream
        .message()
        .await?
        .ok_or_else(|| Status::invalid_argument("empty PutPath stream"))?;
    match first.msg {
        Some(put_path_request::Msg::Metadata(meta)) => meta
            .info
            .ok_or_else(|| Status::invalid_argument("PutPathMetadata missing PathInfo")),
        Some(put_path_request::Msg::NarChunk(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not nar_chunk",
        )),
        Some(put_path_request::Msg::Trailer(_)) => Err(Status::invalid_argument(
            "first PutPath message must be metadata, not trailer",
        )),
        None => Err(Status::invalid_argument("PutPath message has no content")),
    }
}

// r[impl store.integrity.verify-on-put]
// r[impl sec.drv.validate]
/// Hash the buffered NAR and check against the trailer-declared
/// `nar_hash` / `nar_size` (already applied to `info` via
/// [`apply_trailer`]). The integrity gate of
/// `r[store.integrity.verify-on-put]` — server computes the digest
/// independently of the client.
///
/// Status messages contain the substrings "size mismatch" / "hash
/// mismatch"; protocol tests assert on those.
pub(in crate::grpc) fn verify_nar(
    nar_data: &[u8],
    info: &ValidatedPathInfo,
    ctx_label: &str,
) -> Result<(), Status> {
    use sha2::{Digest, Sha256};
    let fail = |e: String| {
        warn!(store_path = %info.store_path, error = %e, "{ctx_label}: NAR validation failed");
        Status::invalid_argument(format!("{ctx_label}: NAR validation failed: {e}"))
    };
    let actual_size = nar_data.len() as u64;
    if actual_size != info.nar_size {
        return Err(fail(format!(
            "NAR size mismatch: declared {}, actual {actual_size}",
            info.nar_size
        )));
    }
    let actual_hash: [u8; 32] = Sha256::digest(nar_data).into();
    if actual_hash != info.nar_hash {
        return Err(fail(format!(
            "NAR hash mismatch: declared {}, computed {}",
            hex::encode(info.nar_hash),
            hex::encode(actual_hash)
        )));
    }
    Ok(())
}

// r[impl sec.authz.ca-path-derived]
/// Floating-CA path-authorization gate. When `claims.is_ca` is set,
/// [`validate_put_metadata`] skipped the `store_path ∈
/// expected_outputs` check (the path isn't known at sign time). This
/// is the replacement gate: recompute the CA store path SERVER-SIDE
/// from the NAR hash that [`verify_nar`] just confirmed, and reject if
/// it doesn't match `info.store_path`. A worker holding an
/// `is_ca=true` token therefore cannot upload to any path that isn't
/// the content-derived path of the NAR it actually sent.
///
/// Floating-CA (`__contentAddressed = true`, non-FOD) always uses
/// `nar:sha256` recursive hashing — see `dispatch.rs`'s `is_ca` gate
/// (`state.ca.is_ca && !state.is_fixed_output`). FODs with known
/// output paths go through the IA `expected_outputs` check instead.
///
/// `None`/non-CA claims → no-op (IA already gated, dev/service
/// bypass already trusted).
pub(in crate::grpc) fn verify_ca_store_path(
    info: &ValidatedPathInfo,
    hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ctx_label: &str,
) -> Result<(), Status> {
    let Some(claims) = hmac_claims else {
        return Ok(());
    };
    if !claims.is_ca {
        return Ok(());
    }

    // Self-reference: Nix's `:self` token in the source-type
    // fingerprint isn't yet implemented in rio-nix's
    // `make_fixed_output`. Filter the path-under-construction out of
    // refs and reject explicitly if it was present — none in the
    // current build graph.
    let refs: Vec<rio_nix::store_path::StorePath> = info
        .references
        .iter()
        .filter(|r| r.as_str() != info.store_path.as_str())
        .cloned()
        .collect();
    if refs.len() != info.references.len() {
        return Err(Status::unimplemented(format!(
            "{ctx_label}: self-referencing floating-CA not yet supported \
             (extend make_fixed_output with :self)"
        )));
    }

    // info.nar_hash is the SERVER-COMPUTED hash here (verify_nar has
    // already confirmed it equals SHA-256(stream)).
    let nar_hash =
        rio_nix::hash::NixHash::new(rio_nix::hash::HashAlgo::SHA256, info.nar_hash.to_vec())
            .map_err(|e| Status::internal(format!("{ctx_label}: nar_hash construct: {e}")))?;
    let expected = rio_nix::store_path::StorePath::make_fixed_output(
        info.store_path.name(),
        &nar_hash,
        /* recursive */ true,
        &refs,
    )
    .map_err(|e| Status::invalid_argument(format!("{ctx_label}: CA path derive: {e}")))?;

    if expected.as_str() != info.store_path.as_str() {
        warn!(
            store_path = %info.store_path,
            expected = %expected,
            executor_id = %claims.executor_id,
            drv_hash = %claims.drv_hash,
            "{ctx_label}: is_ca store_path does not match server-derived CA path"
        );
        metrics::counter!(
            "rio_store_hmac_rejected_total",
            "reason" => "ca_path_mismatch"
        )
        .increment(1);
        return Err(Status::permission_denied(format!(
            "{ctx_label}: store_path does not match content-derived CA path"
        )));
    }
    Ok(())
}

impl StoreServiceImpl {
    // r[impl sec.boundary.grpc-hmac]
    /// HMAC token verify + JWT tenant extraction. Shared step-0 of the
    /// write-ahead flow. See [`Self::verify_assignment_token`] for the
    /// HMAC verifier semantics (dev-mode/mTLS-bypass/token paths).
    ///
    /// Distinct claim types — don't confuse them:
    /// - `hmac::AssignmentClaims`: worker_id + drv_hash +
    ///   expected_outputs. Restricts WHICH paths this worker may upload.
    ///   Per-assignment.
    /// - `jwt::TenantClaims`: sub (tenant UUID) + iat/exp/jti. Says
    ///   WHOSE tenant key signs the narinfo. Per-session.
    ///
    /// `tenant_id = None` covers: no interceptor wired (dev mode), no
    /// `x-rio-tenant-token` header (dual-mode fallback), or mTLS bypass
    /// (gateway cert) — all cluster-key-correct.
    pub(in crate::grpc) fn authorize<T>(&self, request: &Request<T>) -> Result<PutAuth, Status> {
        let hmac_claims = self.verify_assignment_token(request)?;
        let tenant_id = request
            .extensions()
            .get::<rio_auth::jwt::TenantClaims>()
            .map(|c| c.sub);
        Ok(PutAuth {
            hmac_claims,
            tenant_id,
        })
    }

    // r[impl store.put.nar-bytes-budget]
    /// Append a NAR chunk under both bounds: per-output [`MAX_NAR_SIZE`]
    /// and the GLOBAL `nar_bytes_budget` semaphore. Returns the held
    /// permit; the caller pushes it into a `Vec` so drop-on-any-exit
    /// releases capacity. `await` here backpressures the client via
    /// gRPC flow control when the budget is exhausted.
    ///
    /// `>=` so a single chunk of exactly 2³² bytes is rejected before
    /// it reaches `acquire_many(0)` and silently bypasses the budget.
    /// `chunk.len() as u32` never truncates: chunks are bounded by
    /// `RIO_GRPC_MAX_MESSAGE_SIZE`.
    pub(in crate::grpc) async fn accumulate_chunk<'a>(
        &'a self,
        nar_data: &mut Vec<u8>,
        chunk: &[u8],
        ctx_label: &str,
    ) -> Result<tokio::sync::SemaphorePermit<'a>, Status> {
        let new_len = (nar_data.len() as u64).saturating_add(chunk.len() as u64);
        if new_len >= MAX_NAR_SIZE {
            return Err(Status::invalid_argument(format!(
                "{ctx_label}: NAR chunks exceed size bound {MAX_NAR_SIZE} (received {new_len}+ bytes)"
            )));
        }
        let permit = self
            .nar_bytes_budget
            .acquire_many(chunk.len() as u32)
            .await
            .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;
        nar_data.extend_from_slice(chunk);
        Ok(permit)
    }

    // r[impl store.put.drop-cleanup]
    /// Drop-safety for an [`PlaceholderClaim::Owned`] placeholder: if
    /// the handler future is DROPPED (tonic aborts on client
    /// RST_STREAM) without having called `abort_upload` or flipped to
    /// `'complete'`, this guard's spawn cleans it up. `reap_one`
    /// filters `status='uploading'` so firing after an explicit
    /// abort/complete is a harmless no-op. Defuse with
    /// `ScopeGuard::into_inner` on success.
    pub(in crate::grpc) fn spawn_placeholder_guard(
        &self,
        store_path_hash: Vec<u8>,
    ) -> scopeguard::ScopeGuard<(), impl FnOnce(())> {
        let pool = self.pool.clone();
        let chunk_backend = self.chunk_backend.clone();
        scopeguard::guard((), move |()| {
            tokio::spawn(async move {
                if let Err(e) = crate::gc::orphan::reap_one(
                    &pool,
                    &store_path_hash,
                    None,
                    chunk_backend.as_ref(),
                )
                .await
                {
                    error!(error = %e, "PutPath: drop-path placeholder cleanup failed");
                }
            });
        })
    }

    /// Drain a single-output PutPath stream after metadata: accumulate
    /// chunks ([`Self::accumulate_chunk`]), receive the mandatory
    /// trailer, reject protocol violations (chunk-after-trailer,
    /// duplicate metadata/trailer), then [`apply_trailer`] +
    /// [`verify_nar`] + [`verify_ca_store_path`]. Returns the buffered
    /// NAR and held budget permits.
    ///
    /// Errors do NOT clean up the placeholder — caller wraps the call
    /// and `abort_upload`s on `Err`.
    pub(in crate::grpc) async fn ingest_nar_stream<'a>(
        &'a self,
        stream: &mut Streaming<PutPathRequest>,
        info: &mut ValidatedPathInfo,
        hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ) -> Result<(Vec<u8>, Vec<tokio::sync::SemaphorePermit<'a>>), Status> {
        let mut nar_data = Vec::new();
        let mut trailer: Option<PutPathTrailer> = None;
        let mut held_permits = Vec::new();
        loop {
            let msg = match stream.message().await {
                Ok(Some(m)) => m,
                Ok(None) => break,
                Err(e) => {
                    warn!(store_path = %info.store_path, error = %e, "PutPath: stream read error");
                    return Err(e);
                }
            };
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(chunk)) => {
                    if trailer.is_some() {
                        return Err(Status::invalid_argument(
                            "PutPath: nar_chunk after trailer (trailer must be last)",
                        ));
                    }
                    let permit = self
                        .accumulate_chunk(&mut nar_data, &chunk, "PutPath")
                        .await?;
                    held_permits.push(permit);
                }
                Some(put_path_request::Msg::Trailer(t)) => {
                    if trailer.is_some() {
                        return Err(Status::invalid_argument("PutPath: duplicate trailer"));
                    }
                    trailer = Some(t);
                    // Don't break — keep reading to catch chunk-after-trailer.
                }
                Some(put_path_request::Msg::Metadata(_)) => {
                    warn!(store_path = %info.store_path,
                          "PutPath: duplicate metadata mid-stream, rejecting");
                    return Err(Status::invalid_argument(
                        "PutPath stream contained duplicate metadata (protocol violation)",
                    ));
                }
                None => {}
            }
        }
        let t = trailer.ok_or_else(|| {
            Status::invalid_argument(
                "PutPath: no trailer received \
                 (PutPathTrailer is required as the last message)",
            )
        })?;
        apply_trailer(info, &t, "PutPath")?;
        verify_nar(&nar_data, info, "PutPath")?;
        verify_ca_store_path(info, hmac_claims, "PutPath")?;
        Ok((nar_data, held_permits))
    }

    /// Sign + persist + emit success metrics for a single validated
    /// output. On `persist_nar` error the placeholder is `abort_upload`ed
    /// here; the caller's drop-guard spawn is then a harmless no-op.
    /// `info.store_path_hash` MUST be populated.
    // r[impl obs.metric.transfer-volume]
    pub(in crate::grpc) async fn finalize_single(
        &self,
        mut info: ValidatedPathInfo,
        nar_data: Vec<u8>,
        tenant_id: Option<uuid::Uuid>,
    ) -> Result<(), Status> {
        self.maybe_sign(tenant_id, &mut info).await;
        if let Err(e) = self.persist_nar(&info, nar_data, "PutPath").await {
            self.abort_upload(&info.store_path_hash).await;
            return Err(e);
        }
        metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
        metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        Ok(())
    }

    /// gRPC wrapper around [`ingest::claim_placeholder`]: adds the
    /// PutPath-specific result counters (`put_path_total{result=exists}`,
    /// `putpath_retries_total{reason=concurrent_upload}`) on top of the
    /// shared write-ahead core. `ctx_label` is unused now that the core
    /// emits its own log prefix; kept for call-site readability between
    /// PutPath and PutPathBatch.
    pub(in crate::grpc) async fn claim_placeholder(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        refs: &[String],
        _ctx_label: &str,
    ) -> Result<PlaceholderClaim, metadata::MetadataError> {
        let claim = ingest::claim_placeholder(
            &self.pool,
            self.chunk_backend.as_ref(),
            store_path_hash,
            store_path,
            refs,
            PUTPATH_HOOKS,
        )
        .await?;
        match &claim {
            PlaceholderClaim::AlreadyComplete => {
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
            }
            PlaceholderClaim::Concurrent => {
                metrics::counter!("rio_store_putpath_retries_total",
                    "reason" => "concurrent_upload")
                .increment(1);
            }
            PlaceholderClaim::Owned => {}
        }
        Ok(claim)
    }

    /// gRPC wrapper around [`ingest::persist_nar`]: maps
    /// [`ingest::PersistError`] → `tonic::Status` with the
    /// PutPath-specific code mapping (`storage_error` for the chunked
    /// branch so `BackendAuthError` → `FailedPrecondition` and the
    /// builder fails fast instead of retrying forever;
    /// `putpath_metadata_status` for the inline branch so retriable PG
    /// errors get retriable codes + the `putpath_retries_total`
    /// counter).
    ///
    /// Returns `true` iff the chunked branch was taken (legacy — every
    /// current caller `abort_upload`s on error regardless, which is a
    /// safe no-op when chunked already rolled back).
    pub(in crate::grpc) async fn persist_nar(
        &self,
        info: &ValidatedPathInfo,
        nar_data: Vec<u8>,
        ctx_label: &str,
    ) -> Result<bool, Status> {
        let chunked = cas::should_chunk(self.chunk_backend.as_ref(), nar_data.len()).is_some();
        ingest::persist_nar(
            &self.pool,
            self.chunk_backend.as_ref(),
            info,
            nar_data,
            self.chunk_upload_max_concurrent,
            PUTPATH_HOOKS,
        )
        .await
        .map_err(|e| match e {
            ingest::PersistError::Chunked(e) => storage_error(ctx_label, e),
            ingest::PersistError::Inline(e) => putpath_metadata_status(ctx_label, e),
        })?;
        Ok(chunked)
    }

    /// Batch-phase staging: for outputs ≥ [`cas::INLINE_THRESHOLD`],
    /// upload chunks + increment refcounts via [`cas::stage_chunked`]
    /// WITHOUT flipping `status='complete'`. Returns the
    /// [`NarPersist`] discriminant so the batch's atomic tx can pick
    /// the `inline_blob` arg to [`metadata::complete_manifest_in_conn`].
    ///
    /// On `stage_chunked` error this output's placeholder is already
    /// rolled back; the batch's `abort_batch` handles other outputs'
    /// placeholders.
    pub(in crate::grpc) async fn stage_nar_for_batch(
        &self,
        info: &ValidatedPathInfo,
        nar_data: Vec<u8>,
    ) -> Result<NarPersist, Status> {
        if let Some(backend) = cas::should_chunk(self.chunk_backend.as_ref(), nar_data.len()) {
            let stats = cas::stage_chunked(
                &self.pool,
                backend,
                info,
                &nar_data,
                self.chunk_upload_max_concurrent,
            )
            .await
            .map_err(|e| storage_error("PutPathBatch: stage_chunked", e))?;
            metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
            Ok(NarPersist::ChunkedStaged)
        } else {
            Ok(NarPersist::Inline(Bytes::from(nar_data)))
        }
    }
}

// r[verify sec.drv.validate]
// r[verify store.integrity.verify-on-put]
#[cfg(test)]
mod verify_nar_tests {
    use super::*;
    use rio_test_support::fixtures::{make_path_info_for_nar, test_store_path};

    #[test]
    fn verify_nar_size_and_hash() {
        let data = b"valid nar data";
        let info = make_path_info_for_nar(&test_store_path("v"), data);
        assert!(verify_nar(data, &info, "t").is_ok());

        let e = verify_nar(b"short", &info, "t").unwrap_err();
        assert!(e.message().contains("size mismatch"), "got: {e:?}");

        let e = verify_nar(b"different data", &info, "t").unwrap_err();
        assert!(e.message().contains("hash mismatch"), "got: {e:?}");
    }
}
