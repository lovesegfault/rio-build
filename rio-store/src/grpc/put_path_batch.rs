//! PutPathBatch: atomic multi-output upload.
//!
//! Independent `PutPath` calls can't share a transaction — each is a
//! separate gRPC handler invocation pulling its own pool connection.
//! The worker's `buffer_unordered(4)` sends 4 independent streams; if
//! output-2 fails after output-1 committed, output-1 stays visible
//! (verified by `gt13_multi_output_not_atomic`).
//!
//! This RPC carries ALL outputs on ONE stream. The handler accumulates
//! and validates every output, then commits them in ONE `sqlx::Transaction`.
//! Any failure → rollback → zero rows. That's the atomicity the spec
//! requires (`r[store.atomic.multi-output]`).
//!
//! ## Inline-only (v1 bound)
//!
//! Multi-output derivations typically split into small pieces — `out`
//! has the binaries, `dev` has headers (~MB), `doc`/`man` are small.
//! The chunked path's S3 uploads can't live inside a DB transaction
//! anyway (the spec says "blob-store writes are NOT rolled back"). If
//! a multi-output output is ≥ `INLINE_THRESHOLD`, this handler rejects
//! it with `FAILED_PRECONDITION` and the client falls back to
//! independent `PutPath`.
// r[impl store.atomic.multi-output]

use std::collections::BTreeMap;

use super::*;
use rio_common::limits::MAX_BATCH_OUTPUTS;
use rio_proto::types::{PutPathBatchRequest, PutPathBatchResponse, PutPathTrailer};

/// Per-output accumulation state. One of these per `output_index` seen
/// on the stream.
#[derive(Default)]
struct OutputAccum {
    /// Validated PathInfo from the metadata message. `None` until
    /// metadata arrives; `Some` means metadata was received and passed
    /// validation.
    info: Option<ValidatedPathInfo>,
    /// Computed store_path_hash (SHA-256 of the store path string).
    store_path_hash: Vec<u8>,
    /// Accumulated NAR bytes. Bounded by `MAX_NAR_SIZE` per output.
    nar_data: Vec<u8>,
    /// The trailer, once received. Used to verify hash/size.
    trailer: Option<PutPathTrailer>,
    /// Idempotency: this output was already `status='complete'` before
    /// we started. No placeholder, no commit for it — just
    /// `created=false` in the response.
    already_complete: bool,
}

impl StoreServiceImpl {
    pub(super) async fn put_path_batch_impl(
        &self,
        request: Request<Streaming<PutPathBatchRequest>>,
    ) -> Result<Response<PutPathBatchResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();

        // HMAC check once for the whole batch. Claims (if any) apply to
        // every output — each output's path must be in expected_outputs.
        let hmac_claims = self.verify_assignment_token(&request)?;

        // JWT tenant-id extraction — same mechanism as PutPath (see
        // put_path.rs for the full interceptor/dual-mode commentary).
        // One JWT per batch; all outputs in the batch get signed with
        // the same tenant's key. This is correct: a batch is one
        // derivation's outputs, one build, one tenant.
        let tenant_id: Option<uuid::Uuid> = request
            .extensions()
            .get::<rio_common::jwt::TenantClaims>()
            .map(|c| c.sub);

        let mut stream = request.into_inner();

        // BTreeMap for deterministic iteration order (output 0, 1, 2, …).
        // The response `created` array is indexed by output_index, so we
        // need to fill it in order regardless of stream arrival order.
        let mut outputs: BTreeMap<u32, OutputAccum> = BTreeMap::new();
        // NAR byte budget permits — same backpressure as PutPath. Held
        // for the whole handler so drop-on-exit releases them.
        let mut _held_permits: Vec<tokio::sync::SemaphorePermit<'_>> = Vec::new();
        // store_path_hashes of placeholders WE inserted (and thus own
        // and must clean up on error). Separate from `outputs` so the
        // bail! macro can borrow it while a phase-2/3 loop holds a
        // mutable borrow of `outputs` — borrow-checker can't see that
        // abort_batch only needs this subset.
        let mut owned_placeholders: Vec<Vec<u8>> = Vec::new();

        // Macro: on any error after placeholders are inserted, clean them
        // up before returning. Tokio can't do async Drop so this is
        // explicit. Borrows `&owned_placeholders` only — disjoint from
        // `&mut outputs` held by phase-2/3 loops.
        macro_rules! bail {
            ($status:expr) => {{
                self.abort_batch(&owned_placeholders).await;
                return Err($status);
            }};
        }

        // --- Phase 1: drain the stream, route by output_index ---
        while let Some(msg) = stream.message().await.transpose() {
            let msg = match msg {
                Ok(m) => m,
                Err(e) => bail!(e),
            };
            let idx = msg.output_index;

            // Bound output count. Checked on every message because the
            // highest index can arrive at any point in the stream.
            if idx as usize >= MAX_BATCH_OUTPUTS {
                bail!(Status::invalid_argument(format!(
                    "output_index {idx} exceeds MAX_BATCH_OUTPUTS ({MAX_BATCH_OUTPUTS})"
                )));
            }

            let inner = msg
                .inner
                .and_then(|i| i.msg)
                .ok_or_else(|| Status::invalid_argument("PutPathBatchRequest.inner must be set"))?;

            let accum = outputs.entry(idx).or_default();

            match inner {
                put_path_request::Msg::Metadata(meta) => {
                    if accum.info.is_some() {
                        bail!(Status::invalid_argument(format!(
                            "output {idx}: duplicate metadata"
                        )));
                    }
                    let raw_info = meta.info.ok_or_else(|| {
                        Status::invalid_argument(format!(
                            "output {idx}: PutPathMetadata missing PathInfo"
                        ))
                    })?;
                    // Shared 6-step validation (trailer-only, bounds,
                    // placeholder, TryFrom, HMAC path-in-claims) — same
                    // helper PutPath uses. Phase 1 hasn't inserted any
                    // placeholders yet, so `?` is safe (the static
                    // ?-grep test slices from the phase-2 marker below).
                    let info = validate_put_metadata(
                        raw_info,
                        hmac_claims.as_ref(),
                        &format!("output {idx}"),
                    )?;

                    accum.store_path_hash = info.store_path.sha256_digest().to_vec();
                    accum.info = Some(info);
                }
                put_path_request::Msg::NarChunk(chunk) => {
                    if accum.info.is_none() {
                        bail!(Status::invalid_argument(format!(
                            "output {idx}: nar_chunk before metadata"
                        )));
                    }
                    if accum.trailer.is_some() {
                        bail!(Status::invalid_argument(format!(
                            "output {idx}: nar_chunk after trailer"
                        )));
                    }
                    let new_len = (accum.nar_data.len() as u64).saturating_add(chunk.len() as u64);
                    if new_len >= MAX_NAR_SIZE {
                        bail!(Status::invalid_argument(format!(
                            "output {idx}: NAR exceeds MAX_NAR_SIZE"
                        )));
                    }
                    // Global NAR byte budget — same semaphore PutPath uses.
                    let permit = self
                        .nar_bytes_budget
                        .acquire_many(chunk.len() as u32)
                        .await
                        .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;
                    _held_permits.push(permit);
                    accum.nar_data.extend_from_slice(&chunk);
                }
                put_path_request::Msg::Trailer(t) => {
                    if accum.trailer.is_some() {
                        bail!(Status::invalid_argument(format!(
                            "output {idx}: duplicate trailer"
                        )));
                    }
                    accum.trailer = Some(t);
                }
            }
        }

        if outputs.is_empty() {
            return Err(Status::invalid_argument("PutPathBatch: empty stream"));
        }

        // --- Phase 2: per-output validation + placeholder insert ---
        // Each output gets the same metadata → trailer-apply → hash-verify
        // flow PutPath does. Placeholder inserts happen here (before the
        // commit tx) because they're idempotent-safe: `nar_size=0` +
        // `status='uploading'` guards mean `delete_manifest_uploading`
        // can't touch a concurrent winner.
        for (idx, accum) in outputs.iter_mut() {
            let Some(info) = accum.info.as_mut() else {
                bail!(Status::invalid_argument(format!(
                    "output {idx}: stream closed without metadata"
                )));
            };
            let Some(t) = accum.trailer.as_ref() else {
                bail!(Status::invalid_argument(format!(
                    "output {idx}: stream closed without trailer"
                )));
            };

            // bail! (not `?`): prior loop iterations may have pushed to
            // owned_placeholders.
            if let Err(e) = apply_trailer(info, t, &format!("output {idx}")) {
                bail!(e);
            }

            // Hash verification — the security check.
            let digest = crate::validate::NarDigest::from_bytes(&accum.nar_data);
            if let Err(e) = validate_nar_digest(&digest, &info.nar_hash, info.nar_size) {
                warn!(output_index = %idx, error = %e, "PutPathBatch: NAR validation failed");
                bail!(Status::invalid_argument(format!(
                    "output {idx}: NAR validation failed: {e}"
                )));
            }

            // v1 bound: inline only. See module doc.
            if accum.nar_data.len() >= cas::INLINE_THRESHOLD {
                bail!(Status::failed_precondition(format!(
                    "output {idx}: NAR size {} >= INLINE_THRESHOLD ({}); \
                     PutPathBatch v1 is inline-only — use independent PutPath calls",
                    accum.nar_data.len(),
                    cas::INLINE_THRESHOLD
                )));
            }

            // Idempotency: if already complete, skip placeholder + commit
            // for this output. Other outputs proceed normally.
            match metadata::check_manifest_complete(&self.pool, &accum.store_path_hash).await {
                Ok(true) => {
                    accum.already_complete = true;
                    metrics::counter!("rio_store_put_path_total", "result" => "exists")
                        .increment(1);
                    continue;
                }
                Ok(false) => {}
                Err(e) => bail!(internal_error("PutPathBatch: check_manifest_complete", e)),
            }

            // Insert placeholder. Same references-on-placeholder semantics
            // as PutPath (GC mark protection from the instant this commits).
            let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
            let mut inserted = match metadata::insert_manifest_uploading(
                &self.pool,
                &accum.store_path_hash,
                info.store_path.as_str(),
                &refs_str,
            )
            .await
            {
                Ok(i) => i,
                Err(e) => bail!(putpath_metadata_status(
                    "PutPathBatch: insert_manifest_uploading",
                    e
                )),
            };
            // r[impl store.put.stale-reclaim]
            // Same hot-path reclaim as PutPath (I-207). reap_one's
            // threshold check guards a live concurrent uploader.
            if !inserted {
                let threshold = crate::substitute::SUBSTITUTE_STALE_THRESHOLD.as_secs() as i64;
                match crate::gc::orphan::reap_one(
                    &self.pool,
                    &accum.store_path_hash,
                    Some(threshold),
                    self.chunk_backend.as_ref(),
                )
                .await
                {
                    Ok(true) => {
                        warn!(store_path = %info.store_path,
                              "PutPathBatch: stale 'uploading' placeholder — reclaimed");
                        metrics::counter!("rio_store_putpath_stale_reclaimed_total").increment(1);
                        inserted = metadata::insert_manifest_uploading(
                            &self.pool,
                            &accum.store_path_hash,
                            info.store_path.as_str(),
                            &refs_str,
                        )
                        .await
                        .unwrap_or(false);
                    }
                    Ok(false) => {} // not stale → live concurrent uploader
                    Err(e) => warn!(error = %e,
                        "PutPathBatch: stale-reclaim failed (proceeding to concurrent-abort)"),
                }
            }

            if !inserted {
                // Concurrent uploader owns the slot. For a batch, we can't
                // partially proceed — bail the whole batch. Retriable.
                metrics::counter!("rio_store_putpath_retries_total",
                    "reason" => "concurrent_upload")
                .increment(1);
                bail!(Status::aborted(format!(
                    "output {idx}: concurrent upload in progress; retry"
                )));
            }
            owned_placeholders.push(accum.store_path_hash.clone());
        }

        // Resolve the tenant's signer ONCE. All outputs in the batch
        // share the same tenant (one JWT → one Claims.sub). The old
        // per-output `maybe_sign` call inside the phase-3 loop did N
        // identical get_active_signer queries while the tx below holds
        // manifests row locks. N=10 → ~10ms extra lock-hold; N=100
        // (possible for a many-output derivation) → ~100ms.
        //
        // Fallback on TenantKeyLookup Err matches maybe_sign: warn +
        // cluster key. One warn instead of N — same end state.
        //
        // `None` iff `self.signer()` is None (signing disabled). Phase-3
        // then skips signing entirely (same as maybe_sign's early return).
        let resolved_signer: Option<(crate::signing::Signer, bool)> = match self.signer() {
            None => None,
            Some(ts) => match ts.resolve_once(tenant_id).await {
                Ok(pair) => Some(pair),
                Err(e) => {
                    warn!(
                        error = %e,
                        ?tenant_id,
                        "PutPathBatch: tenant-key lookup failed; batch will sign with cluster key"
                    );
                    Some((ts.cluster().clone(), false))
                }
            },
        };

        // --- Phase 3: ONE transaction, N completions, one commit ---
        //
        // THE atomicity guarantee (store.atomic.multi-output spec).
        // `pool.begin()` → N × complete_manifest_inline_in_tx → commit.
        // Any error inside the tx → drop → auto-rollback → `abort_batch`
        // cleans placeholders → zero 'complete' rows. Tx covers DB rows
        // ONLY; blob-store writes (inline_blob here) are inside the same
        // tx so they're covered too, but chunked blobs (not v1) would be
        // orphaned — refcount-zero, GC-eligible. Bound: ≤1 NAR-size per
        // failure.
        let mut tx = match self.pool.begin().await {
            Ok(t) => t,
            Err(e) => bail!(internal_error("PutPathBatch: begin transaction", e)),
        };

        let mut created =
            vec![false; (*outputs.keys().last().expect("non-empty: checked at :185") as usize) + 1];
        for (idx, accum) in outputs.iter_mut() {
            if accum.already_complete {
                // created[idx] stays false (idempotency hit).
                continue;
            }
            // Clone (not take) — the post-commit content-index loop needs
            // info.nar_hash + store_path_hash again. Both are 32 bytes;
            // cheap to clone. nar_data stays the move-optimized take.
            let mut info = accum.info.clone().expect("validated in phase 2");
            info.store_path_hash = accum.store_path_hash.clone();
            // Sign with the pre-resolved signer (see resolve_once above).
            // Sync — no DB hit inside the tx.
            if let Some((signer, was_tenant)) = &resolved_signer {
                self.sign_with_resolved(signer, *was_tenant, &mut info);
            }

            let nar_data = Bytes::from(std::mem::take(&mut accum.nar_data));
            if let Err(e) = metadata::complete_manifest_inline_in_tx(&mut tx, &info, nar_data).await
            {
                // tx drops here → auto-rollback. Placeholders still need
                // explicit cleanup (they were committed in phase 2's
                // separate per-placeholder txs).
                drop(tx);
                bail!(putpath_metadata_status(
                    "PutPathBatch: complete_manifest_inline",
                    e
                ));
            }
            created[*idx as usize] = true;
        }

        if let Err(e) = tx.commit().await {
            bail!(internal_error("PutPathBatch: commit", e));
        }

        // Content-index each created output. Same best-effort semantics as
        // PutPath (see put_path.rs content_index::insert call):
        // failure doesn't fail the upload
        // (paths are addressable by store_path); CA ContentLookup just
        // won't find them until a future single-path re-upload indexes
        // them. Done AFTER tx commits so ContentLookup's INNER JOIN on
        // manifests.status='complete' always sees a complete row.
        // r[impl store.put.wal-manifest]
        for (idx, accum) in &outputs {
            if accum.already_complete {
                continue; // indexed by a previous upload
            }
            let info = accum
                .info
                .as_ref()
                .expect("validated in phase 2, not taken");
            if let Err(e) =
                crate::content_index::insert(&self.pool, &info.nar_hash, &accum.store_path_hash)
                    .await
            {
                warn!(
                    output_index = %idx,
                    store_path = %info.store_path.as_str(),
                    error = %e,
                    "PutPathBatch: content_index insert failed \
                     (path still addressable by store_path)"
                );
            }
            // Bytes counter per created output (put_path.rs bytes_total parity).
            // r[impl obs.metric.transfer-volume]
            metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        }

        // Success. Count each created output for metrics parity with PutPath.
        for c in &created {
            if *c {
                metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
            }
        }

        metrics::histogram!("rio_store_put_path_duration_seconds")
            .record(start.elapsed().as_secs_f64());
        Ok(Response::new(PutPathBatchResponse { created }))
    }

    /// Clean up every placeholder we own. Best-effort: errors are logged
    /// (no way to surface them to a client we're already erroring out to).
    /// Takes owned store_path_hashes (not the full `OutputAccum` map) so
    /// the caller's `bail!` macro can invoke this while a `for … iter_mut()`
    /// loop holds `&mut outputs`.
    async fn abort_batch(&self, owned_placeholders: &[Vec<u8>]) {
        for hash in owned_placeholders {
            if let Err(e) = metadata::delete_manifest_uploading(&self.pool, hash).await {
                error!(store_path_hash = %hex::encode(hash), error = %e,
                       "PutPathBatch abort: failed to clean up placeholder");
            }
        }
        metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
    }
}
