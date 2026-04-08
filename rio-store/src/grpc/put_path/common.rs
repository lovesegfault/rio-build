//! Shared PutPath / PutPathBatch flow steps.
//!
//! Both upload RPCs walk the same write-ahead state machine:
//!
//! 1. **authorize** — HMAC token check + JWT tenant extraction
//!    ([`StoreServiceImpl::verify_assignment_token`] +
//!    [`validate_put_metadata`]'s path-in-claims check)
//! 2. **claim_placeholder** — idempotency check, insert
//!    `status='uploading'` row, hot-path stale-reclaim
//!    ([`StoreServiceImpl::claim_placeholder`])
//! 3. **ingest** — accumulate NAR bytes (per-RPC; PutPath drains a
//!    linear stream, PutPathBatch demuxes by `output_index`), apply
//!    trailer ([`apply_trailer`]), verify SHA-256
//! 4. **finalize** — sign + persist inline-or-chunked
//!    ([`StoreServiceImpl::persist_nar`] for the standalone-tx path;
//!    PutPathBatch open-codes the in-tx variant for atomic
//!    multi-output)
//!
//! These were duplicated across `put_path_impl` and
//! `put_path_batch_impl` and had already drifted once (batch lacked
//! chunked support). Factoring here keeps both impls thin wrappers
//! around the same state machine.

use super::*;

/// Result of [`StoreServiceImpl::claim_placeholder`].
pub(in crate::grpc) enum PlaceholderClaim {
    /// Path is already `status='complete'`. Caller returns
    /// `created=false` without writing anything.
    AlreadyComplete,
    /// We inserted (or stale-reclaimed-then-inserted) the
    /// `status='uploading'` placeholder. Caller now OWNS it and MUST
    /// reap on any error path (`abort_upload` / `abort_batch`).
    Owned,
    /// Another uploader holds a live (heartbeating) placeholder.
    /// Caller returns `Status::aborted` so the client retries.
    Concurrent,
}

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
    hmac_claims: Option<&rio_common::hmac::AssignmentClaims>,
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
    // skip the membership check: the output path is computed post-build
    // from the NAR hash, so expected_outputs is [""] at sign time.
    // Threat model holds — token is still bound to drv_hash (worker
    // can't upload for a derivation it wasn't assigned), and
    // r[store.integrity.verify-on-put] below hashes the NAR stream
    // independently.
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

impl StoreServiceImpl {
    // r[impl store.put.idempotent]
    // r[impl store.put.stale-reclaim]
    /// Idempotency check + `status='uploading'` placeholder insert +
    /// hot-path stale-reclaim. The shared step-2/step-3 of the
    /// write-ahead flow.
    ///
    /// Flow:
    /// 1. `check_manifest_complete` → [`PlaceholderClaim::AlreadyComplete`]
    /// 2. `insert_manifest_uploading` → if inserted: [`PlaceholderClaim::Owned`]
    /// 3. ON CONFLICT no-op: try `reap_one` with the stale threshold
    ///    (I-207 — a fetcher that died mid-upload leaves a placeholder
    ///    the orphan scanner won't reap for 15min, but the scheduler
    ///    retries within seconds). If reap succeeded, re-insert.
    /// 4. Still not inserted → [`PlaceholderClaim::Concurrent`] (live
    ///    uploader's heartbeat keeps `updated_at` fresh, so reap_one's
    ///    threshold check protected it).
    ///
    /// `ctx_label` prefixes the warn! lines for log disambiguation.
    /// Metrics (`exists` / `stale_reclaimed` / `concurrent_upload`) are
    /// emitted here so callers don't have to.
    pub(in crate::grpc) async fn claim_placeholder(
        &self,
        store_path_hash: &[u8],
        store_path: &str,
        refs: &[String],
        ctx_label: &str,
    ) -> Result<PlaceholderClaim, metadata::MetadataError> {
        if metadata::check_manifest_complete(&self.pool, store_path_hash).await? {
            metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
            return Ok(PlaceholderClaim::AlreadyComplete);
        }

        // STRUCTURAL: insert_manifest_uploading takes references and
        // writes them into the placeholder narinfo. Mark's CTE walks them
        // from commit → the closure is GC-protected without holding a
        // session lock for the full upload. The lock is transaction-scoped
        // (pg_try_advisory_xact_lock_shared inside the tx, ~ms). On lock
        // contention (mark running), returns MetadataError::Serialization
        // → metadata_status maps to Status::aborted and the worker retries.
        let mut inserted =
            metadata::insert_manifest_uploading(&self.pool, store_path_hash, store_path, refs)
                .await?;

        if !inserted {
            let threshold = crate::substitute::SUBSTITUTE_STALE_THRESHOLD.as_secs() as i64;
            match crate::gc::orphan::reap_one(
                &self.pool,
                store_path_hash,
                Some(threshold),
                self.chunk_backend.as_ref(),
            )
            .await
            {
                Ok(true) => {
                    warn!(%store_path, "{ctx_label}: stale 'uploading' placeholder — reclaimed");
                    metrics::counter!("rio_store_putpath_stale_reclaimed_total").increment(1);
                    inserted = metadata::insert_manifest_uploading(
                        &self.pool,
                        store_path_hash,
                        store_path,
                        refs,
                    )
                    .await
                    .unwrap_or(false);
                }
                Ok(false) => {} // not stale → live concurrent uploader
                Err(e) => warn!(error = %e,
                    "{ctx_label}: stale-reclaim failed (proceeding to concurrent-abort)"),
            }
        }

        if !inserted {
            metrics::counter!("rio_store_putpath_retries_total",
                "reason" => "concurrent_upload")
            .increment(1);
            return Ok(PlaceholderClaim::Concurrent);
        }

        Ok(PlaceholderClaim::Owned)
    }

    /// Persist a validated, hash-verified NAR for ONE output. Branches
    /// on `nar_data.len()` vs [`cas::INLINE_THRESHOLD`]: inline goes
    /// to `manifests.inline_blob` in one tx; chunked goes through
    /// [`cas::put_chunked`] (FastCDC + S3 + refcounts, own write-ahead
    /// + rollback).
    ///
    /// Caller must have an [`PlaceholderClaim::Owned`] placeholder for
    /// `info.store_path_hash`. On chunked-path failure the placeholder
    /// is consumed by `put_chunked`'s internal rollback; on inline
    /// failure the caller still owns it and must `abort_upload`.
    /// Returns `true` iff the chunked branch was taken (so the caller
    /// knows NOT to `abort_upload` on error — already cleaned up).
    pub(in crate::grpc) async fn persist_nar(
        &self,
        info: &ValidatedPathInfo,
        nar_data: Vec<u8>,
        ctx_label: &str,
    ) -> Result<bool, Status> {
        let use_chunked = self.chunk_backend.is_some() && nar_data.len() >= cas::INLINE_THRESHOLD;

        if use_chunked {
            let backend = self.chunk_backend.as_ref().expect("checked is_some above");
            match cas::put_chunked(
                &self.pool,
                backend,
                info,
                &nar_data,
                self.chunk_upload_max_concurrent,
            )
            .await
            {
                Ok(stats) => {
                    debug!(
                        store_path = %info.store_path.as_str(),
                        total_chunks = stats.total_chunks,
                        deduped = stats.deduped_chunks,
                        ratio = stats.dedup_ratio(),
                        "{ctx_label}: chunked upload completed"
                    );
                    metrics::gauge!("rio_store_chunk_dedup_ratio").set(stats.dedup_ratio());
                    Ok(true)
                }
                // storage_error (not status_internal): distinguishes
                // BackendAuthError → FailedPrecondition so the builder
                // fails fast instead of retrying forever. Error metric
                // is the caller's responsibility (abort_upload /
                // abort_batch) so it counts once regardless of branch.
                Err(e) => Err(storage_error(ctx_label, e)),
            }
        } else {
            metadata::complete_manifest_inline(&self.pool, info, Bytes::from(nar_data))
                .await
                .map_err(|e| putpath_metadata_status(ctx_label, e))?;
            debug!(store_path = %info.store_path.as_str(), "{ctx_label}: inline upload completed");
            Ok(false)
        }
    }

    /// Batch-phase staging: for outputs ≥ [`cas::INLINE_THRESHOLD`],
    /// upload chunks + increment refcounts via [`cas::stage_chunked`]
    /// WITHOUT flipping `status='complete'`. Returns the
    /// [`NarPersist`] discriminant so the batch's atomic tx can pick
    /// `complete_manifest_inline_in_tx` vs
    /// `complete_manifest_chunked_in_tx`.
    ///
    /// On `stage_chunked` error this output's placeholder is already
    /// rolled back; the batch's `abort_batch` handles other outputs'
    /// placeholders.
    pub(in crate::grpc) async fn stage_nar_for_batch(
        &self,
        info: &ValidatedPathInfo,
        nar_data: Vec<u8>,
    ) -> Result<NarPersist, Status> {
        let use_chunked = self.chunk_backend.is_some() && nar_data.len() >= cas::INLINE_THRESHOLD;

        if use_chunked {
            let backend = self.chunk_backend.as_ref().expect("checked is_some above");
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
