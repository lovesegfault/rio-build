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
//! ## Chunked staging
//!
//! S3 uploads can't live inside a DB transaction (the spec says
//! "blob-store writes are NOT rolled back"). Outputs ≥
//! `INLINE_THRESHOLD` are staged via [`cas::stage_chunked`] BEFORE the
//! atomic tx (chunks uploaded + refcounted, manifest still
//! `status='uploading'`); the tx then flips inline AND chunked outputs
//! to `'complete'` together. On batch failure the staged chunks orphan
//! (refcount-zero after `abort_batch`'s `reap_one`, GC-eligible).
//! Bound: ≤1 NAR-size of orphaned blob per failed output.
// r[impl store.atomic.multi-output]

use std::collections::BTreeMap;

use super::put_path::common::NarPersist;

use tonic::{Request, Response, Status, Streaming};
use tracing::warn;

use rio_proto::types::{
    PutPathBatchRequest, PutPathBatchResponse, PutPathTrailer, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::metadata::{self};

use super::put_path::{
    PlaceholderClaim, apply_trailer, validate_put_metadata, verify_ca_store_path, verify_nar,
};
use super::{StoreServiceImpl, putpath_metadata_status};
use rio_common::limits::MAX_BATCH_OUTPUTS;

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
    /// Phase-2 staging result. `None` until phase 2 ran; then either
    /// inline bytes or `ChunkedStaged` — feeds `inline_blob` of
    /// [`metadata::complete_manifest_in_conn`].
    staged: Option<NarPersist>,
}

impl StoreServiceImpl {
    pub(super) async fn put_path_batch_impl(
        &self,
        request: Request<Streaming<PutPathBatchRequest>>,
    ) -> Result<Response<PutPathBatchResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();

        let auth = self.authorize(&request)?;
        let mut stream = request.into_inner();

        // --- Phase 1: drain the stream, route by output_index ---
        let (mut outputs, _held_permits) = self
            .drain_batch_stream(&mut stream, auth.hmac_claims.as_ref())
            .await?;
        if outputs.is_empty() {
            return Err(Status::invalid_argument("PutPathBatch: empty stream"));
        }

        // store_path_hashes of placeholders WE inserted (and thus own
        // and must clean up on error). Separate from `outputs` so the
        // bail! macro can borrow it while a phase-2/3 loop holds
        // `&mut outputs`.
        let mut owned_placeholders: Vec<Vec<u8>> = Vec::new();
        macro_rules! bail {
            ($status:expr) => {{
                self.abort_batch(&owned_placeholders).await;
                return Err($status);
            }};
        }

        // --- Phase 2: per-output validation + placeholder insert ---
        for (idx, accum) in outputs.iter_mut() {
            let ctx = format!("output {idx}");
            let Some(info) = accum.info.as_mut() else {
                bail!(Status::invalid_argument(format!(
                    "{ctx}: stream closed without metadata"
                )));
            };
            let Some(t) = accum.trailer.as_ref() else {
                bail!(Status::invalid_argument(format!(
                    "{ctx}: stream closed without trailer"
                )));
            };
            if let Err(e) = apply_trailer(info, t, &ctx) {
                bail!(e);
            }
            if let Err(e) = verify_nar(&accum.nar_data, info, &ctx) {
                bail!(e);
            }
            // r[impl sec.authz.ca-path-derived]
            if let Err(e) = verify_ca_store_path(info, auth.hmac_claims.as_ref(), &ctx) {
                bail!(e);
            }

            let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
            match self
                .claim_placeholder(
                    &accum.store_path_hash,
                    info.store_path.as_str(),
                    &refs_str,
                    "PutPathBatch",
                )
                .await
            {
                Ok(PlaceholderClaim::AlreadyComplete) => {
                    accum.already_complete = true;
                    continue;
                }
                Ok(PlaceholderClaim::Owned) => {
                    owned_placeholders.push(accum.store_path_hash.clone());
                }
                Ok(PlaceholderClaim::Concurrent) => bail!(Status::aborted(format!(
                    "{ctx}: concurrent upload in progress; retry"
                ))),
                Err(e) => bail!(putpath_metadata_status(
                    "PutPathBatch: claim_placeholder",
                    e
                )),
            }

            info.store_path_hash = accum.store_path_hash.clone();
            let nar_data = std::mem::take(&mut accum.nar_data);
            match self.stage_nar_for_batch(info, nar_data).await {
                Ok(p) => accum.staged = Some(p),
                Err(e) => bail!(e),
            }
        }

        let resolved_signer = self.resolve_batch_signer(auth.tenant_id).await;

        // --- Phase 3: ONE transaction, N completions, one commit ---
        let created = match self
            .commit_batch(&mut outputs, resolved_signer.as_ref())
            .await
        {
            Ok(c) => c,
            Err(e) => bail!(e),
        };

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
            crate::ingest::abort_placeholder(&self.pool, self.chunk_backend.as_ref(), hash).await;
        }
        metrics::counter!("rio_store_put_path_total", "result" => "error").increment(1);
    }

    /// Phase 1: demux the batch stream into per-`output_index`
    /// [`OutputAccum`]s. No placeholders are inserted yet, so `?` is
    /// safe (the static `?-grep` test slices from the phase-2 marker in
    /// [`Self::put_path_batch_impl`]).
    async fn drain_batch_stream<'a>(
        &'a self,
        stream: &mut Streaming<PutPathBatchRequest>,
        hmac_claims: Option<&rio_auth::hmac::AssignmentClaims>,
    ) -> Result<
        (
            BTreeMap<u32, OutputAccum>,
            Vec<tokio::sync::SemaphorePermit<'a>>,
        ),
        Status,
    > {
        // BTreeMap for deterministic iteration order (output 0, 1, 2, …).
        // The response `created` array is indexed by output_index, so we
        // need to fill it in order regardless of stream arrival order.
        let mut outputs: BTreeMap<u32, OutputAccum> = BTreeMap::new();
        let mut held_permits = Vec::new();

        while let Some(msg) = stream.message().await? {
            let idx = msg.output_index;
            // Bound output count. Checked on every message because the
            // highest index can arrive at any point in the stream.
            if idx as usize >= MAX_BATCH_OUTPUTS {
                return Err(Status::invalid_argument(format!(
                    "output_index {idx} exceeds MAX_BATCH_OUTPUTS ({MAX_BATCH_OUTPUTS})"
                )));
            }
            let inner = msg
                .inner
                .and_then(|i| i.msg)
                .ok_or_else(|| Status::invalid_argument("PutPathBatchRequest.inner must be set"))?;
            let accum = outputs.entry(idx).or_default();
            let ctx = format!("output {idx}");

            match inner {
                put_path_request::Msg::Metadata(meta) => {
                    if accum.info.is_some() {
                        return Err(Status::invalid_argument(format!(
                            "{ctx}: duplicate metadata"
                        )));
                    }
                    let raw_info = meta.info.ok_or_else(|| {
                        Status::invalid_argument(format!("{ctx}: PutPathMetadata missing PathInfo"))
                    })?;
                    let info = validate_put_metadata(raw_info, hmac_claims, &ctx)?;
                    accum.store_path_hash = info.store_path.sha256_digest().to_vec();
                    accum.info = Some(info);
                }
                put_path_request::Msg::NarChunk(chunk) => {
                    if accum.info.is_none() {
                        return Err(Status::invalid_argument(format!(
                            "{ctx}: nar_chunk before metadata"
                        )));
                    }
                    if accum.trailer.is_some() {
                        return Err(Status::invalid_argument(format!(
                            "{ctx}: nar_chunk after trailer"
                        )));
                    }
                    let permit = self
                        .accumulate_chunk(&mut accum.nar_data, &chunk, &ctx)
                        .await?;
                    held_permits.push(permit);
                }
                put_path_request::Msg::Trailer(t) => {
                    if accum.trailer.is_some() {
                        return Err(Status::invalid_argument(format!(
                            "{ctx}: duplicate trailer"
                        )));
                    }
                    accum.trailer = Some(t);
                }
            }
        }
        Ok((outputs, held_permits))
    }

    /// Resolve the tenant's signer ONCE for a batch. All outputs share
    /// the same tenant (one JWT → one Claims.sub), so resolving inside
    /// the phase-3 tx-loop would do N identical `get_active_signer`
    /// queries while holding manifest row locks.
    ///
    /// On lookup error (any `SignerError::TenantKeyLookup` — including
    /// transient PG `Connection`/`PoolTimedOut`), fall back to the
    /// cluster key. This is intentional defense, matching
    /// [`Self::maybe_sign`]: a transient PG hiccup shouldn't fail the
    /// upload; the cluster sig is still cryptographically valid, just
    /// not tenant-scoped. The trade-off: a tenant that ONLY trusts its
    /// own key will see `nix store verify` fail for these paths. The
    /// `rio_store_sign_tenant_key_fallback_total` counter makes the
    /// degradation visible — alert if sustained nonzero.
    ///
    /// `None` iff signing is disabled entirely.
    async fn resolve_batch_signer(
        &self,
        tenant_id: Option<uuid::Uuid>,
    ) -> Option<(crate::signing::Signer, bool)> {
        let ts = self.signer()?;
        match ts.resolve_once(tenant_id).await {
            Ok(pair) => Some(pair),
            Err(e) => {
                warn!(
                    error = %e,
                    ?tenant_id,
                    "PutPathBatch: tenant-key lookup failed; batch will sign with cluster key"
                );
                metrics::counter!("rio_store_sign_tenant_key_fallback_total").increment(1);
                Some((ts.cluster().clone(), false))
            }
        }
    }

    /// Phase 3: open ONE transaction, flip every owned output to
    /// `status='complete'` (inline or chunked-staged), commit, then
    /// emit per-output created/bytes metrics. Tx auto-rollback on early
    /// return; caller `abort_batch`es on `Err` to reap placeholders +
    /// staged chunk refcounts (committed in phase-2's separate txs).
    // r[impl store.put.wal-manifest]
    // r[impl obs.metric.transfer-volume]
    async fn commit_batch(
        &self,
        outputs: &mut BTreeMap<u32, OutputAccum>,
        resolved_signer: Option<&(crate::signing::Signer, bool)>,
    ) -> Result<Vec<bool>, Status> {
        let mut tx = self
            .pool
            .begin()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathBatch: begin transaction", e))?;

        let max_idx = *outputs.keys().last().expect("non-empty: checked by caller") as usize;
        let mut created = vec![false; max_idx + 1];
        for (idx, accum) in outputs.iter_mut() {
            if accum.already_complete {
                continue;
            }
            let mut info = accum.info.clone().expect("validated in phase 2");
            info.store_path_hash = accum.store_path_hash.clone();
            if let Some((signer, was_tenant)) = resolved_signer {
                self.sign_with_resolved(signer, *was_tenant, &mut info);
            }
            let inline_blob = match accum.staged.take().expect("staged in phase 2") {
                NarPersist::Inline(data) => Some(data),
                NarPersist::ChunkedStaged => None,
            };
            if let Err(e) =
                metadata::complete_manifest_in_conn(&mut tx, &info, inline_blob.as_deref()).await
            {
                drop(tx);
                return Err(putpath_metadata_status(
                    "PutPathBatch: complete_manifest",
                    e,
                ));
            }
            created[*idx as usize] = true;
        }

        tx.commit()
            .await
            .map_err(|e| rio_common::grpc::internal("PutPathBatch: commit", e))?;

        for accum in outputs.values() {
            if accum.already_complete {
                continue;
            }
            let info = accum
                .info
                .as_ref()
                .expect("validated in phase 2, not taken");
            metrics::counter!("rio_store_put_path_bytes_total").increment(info.nar_size);
        }
        for c in &created {
            if *c {
                metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
            }
        }
        Ok(created)
    }
}
