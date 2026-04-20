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
//! (refcount-zero after `PlaceholderGuard` drop-reap, GC-eligible).
//! Bound: ≤1 NAR-size of orphaned blob per failed output.
// r[impl store.atomic.multi-output]

use std::collections::BTreeMap;

use sha2::Digest;
use tonic::{Request, Response, Status, Streaming};
use tracing::warn;

use rio_proto::types::{
    PutPathBatchRequest, PutPathBatchResponse, PutPathTrailer, put_path_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::metadata::{self};

use super::put_path::common::{NarPersist, PlaceholderGuard};
use super::put_path::{
    PlaceholderClaim, apply_trailer, validate_put_metadata, verify_ca_store_path, verify_nar,
};
use super::{StoreServiceImpl, putpath_metadata_status};
use rio_common::limits::{MAX_BATCH_OUTPUTS, MAX_NAR_SIZE};

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
    /// Incremental NAR digest, fed by `accumulate_chunk`. Finalized in
    /// phase 2 for [`verify_nar`].
    hasher: sha2::Sha256,
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
    /// Ownership token from `PlaceholderClaim::Owned` — set in phase-2
    /// for every output that is NOT `already_complete`. Threaded into
    /// phase-3's `complete_manifest_in_conn` so the status flip is
    /// claim-gated (`r[store.put.placeholder-claim+2]`).
    claim: Option<uuid::Uuid>,
}

impl StoreServiceImpl {
    pub(super) async fn put_path_batch_impl(
        &self,
        request: Request<Streaming<PutPathBatchRequest>>,
    ) -> Result<Response<PutPathBatchResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        // Duration histogram via scopeguard so ALL exits record —
        // mirrors `put_path_impl` exactly (same metric name → same
        // semantics; observability.md SLI assumes it).
        let start = std::time::Instant::now();
        let _duration_guard = scopeguard::guard((), move |()| {
            metrics::histogram!("rio_store_put_path_duration_seconds")
                .record(start.elapsed().as_secs_f64());
        });

        let auth = self.authorize(&request)?;
        let mut stream = request.into_inner();

        // --- Phase 1: drain the stream, route by output_index ---
        let (mut outputs, _held_permits) = self
            .drain_batch_stream(&mut stream, auth.hmac_claims.as_ref())
            .await?;
        if outputs.is_empty() {
            return Err(Status::invalid_argument("PutPathBatch: empty stream"));
        }
        let n_outputs = outputs.len();

        // PlaceholderGuards for placeholders WE inserted (and thus own).
        // Each guard heartbeats while held and reaps on Drop — so a
        // `return Err` (or future-drop on RST_STREAM) anywhere in
        // phase-2/3 cleans up every owned placeholder without an
        // explicit cleanup loop. Separate from `outputs` so it can be
        // pushed while a phase-2/3 loop holds `&mut outputs`.
        let mut placeholder_guards: Vec<PlaceholderGuard> = Vec::new();
        // Count of outputs for which `claim_placeholder` already fired
        // `{result="exists"}` (AlreadyComplete arm). The error metric
        // unit is per-store-path; on bail! we increment by the number
        // NOT yet resolved as `exists`.
        let mut n_exists_emitted: usize = 0;
        macro_rules! bail {
            ($status:expr) => {{
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment((n_outputs - n_exists_emitted) as u64);
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
            let computed: [u8; 32] = std::mem::take(&mut accum.hasher).finalize().into();
            if let Err(e) = verify_nar(computed, accum.nar_data.len() as u64, info, &ctx) {
                bail!(e);
            }
            // r[impl sec.authz.ca-path-derived+2]
            if let Err(e) = verify_ca_store_path(info, auth.hmac_claims.as_ref(), &ctx) {
                bail!(e);
            }

            let refs_str: Vec<String> = info.references.iter().map(|r| r.to_string()).collect();
            let claim = match self
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
                    n_exists_emitted += 1;
                    continue;
                }
                Ok(PlaceholderClaim::Owned(c)) => c,
                Ok(PlaceholderClaim::Concurrent) => bail!(Status::aborted(format!(
                    "{ctx}: {}; retry",
                    rio_proto::CONCURRENT_PUTPATH_MSG
                ))),
                Err(e) => bail!(putpath_metadata_status(
                    "PutPathBatch: claim_placeholder",
                    e
                )),
            };
            placeholder_guards
                .push(self.spawn_placeholder_guard(accum.store_path_hash.clone(), claim));
            accum.claim = Some(claim);

            info.store_path_hash = accum.store_path_hash.clone();
            let nar_data = std::mem::take(&mut accum.nar_data);
            match self.stage_nar_for_batch(info, claim, nar_data).await {
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

        for g in placeholder_guards {
            g.defuse();
        }
        Ok(Response::new(PutPathBatchResponse { created }))
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
        // Cumulative charged permits across ALL outputs (NOT raw bytes
        // — `accumulate_chunk` floors each chunk at MIN_NAR_CHUNK_CHARGE,
        // so tracking raw bytes undercounts up to 256× and a tiny-chunk
        // stream exhausts the budget while `total_bytes` is still tiny).
        // Per-output raw bytes are capped at MAX_NAR_SIZE inside
        // `accumulate_chunk`; without a cumulative charged-permit cap,
        // MAX_BATCH_OUTPUTS × MAX_NAR_SIZE = 64 GiB could be demanded
        // against a 32 GiB budget — `acquire_many` would block on permits
        // THIS task holds (self-deadlock).
        // r[impl store.put.nar-bytes-budget+3]
        let mut total_charged: u64 = 0;

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
                    // Server-derived in validate_put_metadata (step 7).
                    accum.store_path_hash = info.store_path_hash.clone();
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
                    total_charged = total_charged
                        .saturating_add(rio_common::limits::nar_chunk_charge(chunk.len()));
                    if total_charged >= MAX_NAR_SIZE {
                        // FailedPrecondition (not InvalidArgument) so
                        // the builder's batch fallback at
                        // upload/mod.rs falls through to independent
                        // PutPath — each output then gets its own
                        // MAX_NAR_SIZE without a cumulative cap.
                        return Err(Status::failed_precondition(format!(
                            "PutPathBatch: cumulative charged permits {total_charged} exceed \
                             MAX_NAR_SIZE {MAX_NAR_SIZE}; fall back to per-output PutPath"
                        )));
                    }
                    let permit = self
                        .accumulate_chunk(&mut accum.nar_data, &mut accum.hasher, &chunk, &ctx)
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
    /// return; caller's `PlaceholderGuard`s reap placeholders on Drop +
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
            let claim = accum
                .claim
                .expect("set in phase-2 for non-already_complete");
            if let Err(e) =
                metadata::complete_manifest_in_conn(&mut tx, &info, claim, inline_blob.as_deref())
                    .await
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
