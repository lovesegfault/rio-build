//! `PutPathChunked`: builder-side chunked multi-output upload (ADR-022 §6).
//!
//! Stream shape: `Begin` (validated by [`validate`]) then one `Chunk`
//! frame per `Begin.novel` entry, in `novel` order. The store never
//! buffers a whole NAR — each novel chunk is BLAKE3-verified and
//! written to the chunk backend on arrival, and each output's NAR byte
//! stream is *reconstructed* from the validated Directory tree plus the
//! chunk bodies to independently recompute `nar_hash` and `references`
//! before anything becomes visible.
//!
//! Flow:
//! 1. [`validate::validate_begin`] — §6.2 bounds, tree attestation,
//!    `novel` ordering. Nothing is written before this passes.
//! 2. Budget acquire + per-output placeholder claim (non-CA only; CA
//!    paths are claimed post-verify per `r[sec.authz.ca-path-derived+2]`).
//! 3. [`verify_walk`] — single sequential walk over every output's
//!    segments, splicing novel chunks from the stream and deduped
//!    chunks from the CAS into per-output SHA-256 + refscan sinks.
//! 4. Verdict → commit txn (all outputs, one transaction) or reject.
// r[impl store.put.chunked]
// r[impl store.chunk.self-verify]
// r[impl store.put.narhash-sync]
// r[impl store.put.refs-sync]
// r[impl store.integrity.verify-on-put]
// r[impl store.atomic.multi-output]

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;

use bytes::Bytes;
use sha2::{Digest, Sha256};
use tonic::{Request, Response, Status, Streaming};
use tracing::{debug, warn};

use rio_nix::refscan::RefScanSink;
use rio_proto::types::{
    PutPathChunkedChunk, PutPathChunkedRequest, PutPathResponse, put_path_chunked_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::backend::ChunkBackend;
use crate::cas::ChunkCache;
use crate::chunker::CHUNK_MAX;
use crate::metadata;

use super::put_path::{PlaceholderClaim, common::PlaceholderGuard};
use super::{StoreServiceImpl, putpath_metadata_status, storage_error};

mod commit;
mod validate;

#[cfg(test)]
mod tests;

use validate::{NarSegment, ValidatedBegin, ValidatedOutput};

/// Bounded prefetch window for deduped-chunk `cas::get`s issued ahead
/// of the verify walk. The fetched bytes land in the shared moka LRU
/// (which already does singleflight + BLAKE3 verify), so the walk's own
/// `get_verified` for the same digest is a memory hit. 32 × CHUNK_MAX =
/// 8 MiB of prefetched data in the worst case.
const PREFETCH_WINDOW: usize = 32;

/// Outcome of the verify walk, before the per-output hash/refs compare.
enum WalkOutcome {
    /// Every position was fed; per-output accumulators are final.
    Complete,
    /// The stream ended before `novel` was exhausted.
    Incomplete,
    /// A `cas::get`/`cas::put` failed transiently (S3 fault, or a
    /// deduped chunk GC'd between the builder's `HasChunks` and now).
    Unavailable(String),
    /// A deduped chunk's stored length disagreed with `manifest_len`.
    /// The builder's manifest doesn't describe the bytes that exist —
    /// the recomputed NAR could never match.
    Mismatch(String),
    /// A regular file's spliced contents hash to something other than
    /// the claimed `FileEntry.digest`. Distinct from [`Self::Mismatch`]
    /// because the attack it blocks is different: the NAR hash can
    /// still be correct while the castore digest is poisoned to alias
    /// another file's identity in `file_blobs`.
    FileDigestMismatch(String),
}

/// Per-output verify accumulators. `None` for idempotent-skipped
/// outputs (nothing to verify — the path is already `'complete'`).
struct OutputAcc {
    hasher: Sha256,
    scanner: RefScanSink,
}

impl StoreServiceImpl {
    // r[impl store.put.chunked-wire]
    pub(super) async fn put_path_chunked_impl(
        &self,
        request: Request<Streaming<PutPathChunkedRequest>>,
    ) -> Result<Response<PutPathResponse>, Status> {
        rio_proto::interceptor::link_parent(&request);
        let start = std::time::Instant::now();
        let _duration_guard = scopeguard::guard((), move |()| {
            metrics::histogram!("rio_store_put_path_duration_seconds")
                .record(start.elapsed().as_secs_f64());
        });

        let auth = self.authorize(&request)?;
        let mut stream = request.into_inner();

        // PutPathChunked is meaningless without a chunk backend: the
        // entire point is per-chunk S3 writes. Reject before reading
        // the stream so the builder fails fast on a misconfigured
        // store instead of streaming gigabytes into an error.
        let Some(cache) = self.chunk_cache.clone() else {
            return Err(Status::failed_precondition(
                "PutPathChunked requires a chunk backend; this store is inline-only",
            ));
        };
        let backend = cache.backend();

        // --- Begin -----------------------------------------------------
        let first = stream
            .message()
            .await?
            .ok_or_else(|| Status::invalid_argument("empty PutPathChunked stream"))?;
        let begin = match first.msg {
            Some(put_path_chunked_request::Msg::Begin(b)) => b,
            Some(put_path_chunked_request::Msg::Chunk(_)) => {
                return Err(Status::invalid_argument(
                    "first PutPathChunked message must be Begin, not Chunk",
                ));
            }
            None => {
                return Err(Status::invalid_argument(
                    "PutPathChunked message has no content",
                ));
            }
        };
        let validated = validate::validate_begin(&begin, auth.hmac_claims.as_ref())?;
        drop(begin);

        // Budget: deduped-chunk fetch bytes + materialized framing.
        // Self-consistent with the attested tree (not builder-attested);
        // the verify walk asserts each actual `cas::get` length.
        // Clamped to u32::MAX (4 GiB - 1) — the semaphore API takes
        // u32, and a single request holding 4 GiB of a 32 GiB default
        // budget is the intended admission behaviour for a worst-case
        // high-dedup upload.
        let _budget_permit = self
            .nar_bytes_budget
            .acquire_many(u32::try_from(validated.budget_bytes).unwrap_or(u32::MAX))
            .await
            .map_err(|_| Status::resource_exhausted("NAR buffer budget closed"))?;

        let is_ca = auth.hmac_claims.as_ref().is_some_and(|c| c.is_ca);

        // --- Placeholders ---------------------------------------------
        // Per output: already-complete → skip; non-CA → claim an
        // 'uploading' placeholder carrying the references
        // (r[store.put.wal-manifest], r[store.put.placeholder-refs]);
        // CA → defer the claim to post-verify
        // (r[sec.authz.ca-path-derived+2] — the path is content-derived
        // and must not be squattable before the content is proven).
        // r[impl store.put.idempotent]
        let n = validated.outputs.len();
        let mut skipped = vec![false; n];
        let mut claims: Vec<Option<uuid::Uuid>> = vec![None; n];
        let mut guards: Vec<PlaceholderGuard> = Vec::new();
        for (i, out) in validated.outputs.iter().enumerate() {
            if metadata::check_manifest_complete(&self.pool, &out.info.store_path_hash)
                .await
                .map_err(|e| putpath_metadata_status("PutPathChunked: idempotency check", e))?
            {
                debug!(store_path = %out.info.store_path, "PutPathChunked: output already complete");
                metrics::counter!("rio_store_put_path_total", "result" => "exists").increment(1);
                skipped[i] = true;
                continue;
            }
            if is_ca {
                continue;
            }
            match self.claim_chunked_placeholder(out).await? {
                Some(claim) => {
                    guards.push(
                        self.spawn_placeholder_guard(out.info.store_path_hash.clone(), claim),
                    );
                    claims[i] = Some(claim);
                }
                None => {
                    // Re-checked complete after a concurrent uploader
                    // won the race — treat as skipped.
                    skipped[i] = true;
                }
            }
        }

        // Write-ahead chunk registration: every digest the drain or the
        // verify walk may S3-PUT (novel chunks from the stream +
        // server-generated framing runs of non-skipped outputs) gets a
        // refcount-0 `chunks` row BEFORE the first PutObject. On any
        // non-commit outcome those objects are then visible to
        // `sweep_orphan_chunks`' `refcount = 0` scan instead of being
        // leaked with no row at all. The commit transaction's refcount
        // UPSERT takes them from 0 to their real count.
        // r[impl store.chunk.grace-ttl]
        let pending: Vec<(Vec<u8>, i64)> = validated
            .novel
            .iter()
            .map(|d| (d.to_vec(), i64::from(validated.manifest_len[d])))
            .chain(
                validated
                    .outputs
                    .iter()
                    .zip(&skipped)
                    .filter(|(_, skip)| !**skip)
                    .flat_map(|(o, _)| o.segments.iter())
                    .filter_map(|seg| match seg {
                        NarSegment::Framing { bytes, digest } => {
                            Some((digest.to_vec(), bytes.len() as i64))
                        }
                        NarSegment::FileContents { .. } => None,
                    }),
            )
            .collect();
        metadata::register_pending_chunks(&self.pool, &pending)
            .await
            .map_err(|e| putpath_metadata_status("PutPathChunked: register_pending_chunks", e))?;
        // Keep the pending rows inside the orphan-sweep grace window
        // for as long as this request is alive: an upload whose verify
        // walk outlives CHUNK_GRACE_SECS (300s) would otherwise have
        // its refcount-0 rows swept and their S3 objects drained
        // mid-flight, and the commit would then reference missing keys.
        // Every 30s tick re-stamps `created_at` on rows still at
        // refcount 0; the task is aborted when the handler returns
        // (committed, rejected, or dropped) so a dead upload's rows age
        // into sweep eligibility normally. The same cadence-vs-grace
        // margin as the placeholder heartbeat (10 missed ticks).
        // r[impl store.chunk.grace-ttl]
        let _chunk_heartbeat = scopeguard::guard(
            {
                let pool = self.pool.clone();
                let hashes: Vec<Vec<u8>> = pending.iter().map(|(h, _)| h.clone()).collect();
                rio_common::task::spawn_monitored("putpath-chunked-grace-heartbeat", async move {
                    let mut tick = tokio::time::interval(std::time::Duration::from_secs(30));
                    tick.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Delay);
                    tick.tick().await; // first tick fires immediately; skip it
                    loop {
                        tick.tick().await;
                        if let Err(e) = metadata::heartbeat_pending_chunks(&pool, &hashes).await {
                            warn!(error = %e, "pending-chunk grace heartbeat failed");
                        }
                    }
                })
            },
            |h| h.abort(),
        );

        // All outputs already complete: drain the remaining Chunk
        // frames (each verified + cas::put — idempotent content-
        // addressed writes that a re-driven builder already computed)
        // and return without entering the verify walk.
        if skipped.iter().all(|s| *s) {
            self.drain_chunked_stream(&mut stream, &validated, &backend)
                .await?;
            return Ok(Response::new(PutPathResponse { created: false }));
        }

        // --- Verify ----------------------------------------------------
        // r[impl store.put.drop-cleanup+2]
        // The PlaceholderGuards reap on drop, so any error return from
        // here on cleans up every owned placeholder.
        let mut accs: Vec<Option<OutputAcc>> = validated
            .outputs
            .iter()
            .zip(&skipped)
            .map(|(_, skip)| {
                (!skip).then(|| OutputAcc {
                    hasher: Sha256::new(),
                    scanner: RefScanSink::new(validated.candidates.hashes()),
                })
            })
            .collect();

        let outcome = self
            .verify_walk(&mut stream, &validated, &mut accs, &cache, &backend)
            .await?;

        // --- Verdict ---------------------------------------------------
        match outcome {
            WalkOutcome::Complete => {}
            WalkOutcome::Incomplete => {
                metrics::counter!("rio_store_putpath_incomplete_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::failed_precondition(
                    "stream ended before all novel chunks were received",
                ));
            }
            WalkOutcome::Unavailable(msg) => {
                metrics::counter!("rio_store_putpath_verify_unavailable_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::unavailable(format!(
                    "chunk store unavailable during verify: {msg}"
                )));
            }
            WalkOutcome::Mismatch(msg) => {
                metrics::counter!("rio_store_narhash_mismatch_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::failed_precondition(format!(
                    "chunk length mismatch during verify: {msg}"
                )));
            }
            WalkOutcome::FileDigestMismatch(msg) => {
                metrics::counter!("rio_store_file_digest_mismatch_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::failed_precondition(format!(
                    "file digest mismatch during verify: {msg}"
                )));
            }
        }

        // Per-output verdict: computed nar_hash + scanned refs vs the
        // claimed values. Any mismatch fails the WHOLE RPC — partial
        // registration of a multi-output derivation breaks the
        // "all outputs or none" contract.
        let mut computed_hashes: Vec<Option<[u8; 32]>> = vec![None; n];
        for (i, acc) in accs.into_iter().enumerate() {
            let Some(acc) = acc else { continue };
            let out = &validated.outputs[i];
            let computed: [u8; 32] = acc.hasher.finalize().into();
            let scanned = validated.candidates.resolve(&acc.scanner.into_found());
            let claimed: Vec<String> = out
                .info
                .references
                .iter()
                .map(|r| r.as_str().to_owned())
                .collect();
            // r[impl store.put.narhash-sync]
            if computed != out.info.nar_hash {
                warn!(
                    store_path = %out.info.store_path,
                    deriver = %begin_deriver(&out.info),
                    builder = %executor_id(auth.hmac_claims.as_ref()),
                    claimed = %hex::encode(out.info.nar_hash),
                    computed = %hex::encode(computed),
                    "PutPathChunked: NAR hash mismatch"
                );
                metrics::counter!("rio_store_narhash_mismatch_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::failed_precondition(format!(
                    "output {i}: NAR hash mismatch: declared {}, computed {}",
                    hex::encode(out.info.nar_hash),
                    hex::encode(computed)
                )));
            }
            // r[impl store.put.refs-sync]
            if scanned != claimed {
                warn!(
                    store_path = %out.info.store_path,
                    deriver = %begin_deriver(&out.info),
                    builder = %executor_id(auth.hmac_claims.as_ref()),
                    claimed = ?claimed,
                    computed = ?scanned,
                    "PutPathChunked: reference mismatch"
                );
                metrics::counter!("rio_store_refs_mismatch_total").increment(1);
                metrics::counter!("rio_store_put_path_total", "result" => "error")
                    .increment(unskipped(&skipped));
                return Err(Status::failed_precondition(format!(
                    "output {i}: reference mismatch: declared {claimed:?}, scanned {scanned:?}"
                )));
            }
            computed_hashes[i] = Some(computed);
        }

        // --- CA path recompute + deferred placeholder claim -----------
        // r[impl store.put.chunked-ca]
        // r[impl sec.authz.ca-path-derived+2]
        if is_ca {
            for (i, out) in validated.outputs.iter().enumerate() {
                if skipped[i] {
                    continue;
                }
                super::put_path::verify_ca_store_path(
                    &out.info,
                    auth.hmac_claims.as_ref(),
                    "PutPathChunked",
                )?;
                match self.claim_chunked_placeholder(out).await? {
                    Some(claim) => {
                        guards.push(
                            self.spawn_placeholder_guard(out.info.store_path_hash.clone(), claim),
                        );
                        claims[i] = Some(claim);
                    }
                    None => skipped[i] = true,
                }
            }
            if skipped.iter().all(|s| *s) {
                return Ok(Response::new(PutPathResponse { created: false }));
            }
        }

        // --- Commit ----------------------------------------------------
        let resolved_signer = self.resolve_batch_signer(auth.tenant_id).await;
        self.commit_chunked(
            &validated,
            &skipped,
            &claims,
            auth.hmac_claims.as_ref(),
            resolved_signer.as_ref(),
        )
        .await?;

        for g in guards {
            g.defuse();
        }
        for (i, out) in validated.outputs.iter().enumerate() {
            if skipped[i] {
                continue;
            }
            metrics::counter!("rio_store_put_path_total", "result" => "created").increment(1);
            metrics::counter!("rio_store_put_path_bytes_total").increment(out.info.nar_size);
        }
        Ok(Response::new(PutPathResponse { created: true }))
    }

    /// `claim_placeholder` for one chunked output, mapping the three
    /// claim outcomes to the chunked handler's needs: `Owned` →
    /// `Some(claim)`, `AlreadyComplete` → `None` (caller marks the
    /// output skipped), `Concurrent` → `Aborted` (the builder retries
    /// the whole RPC — re-driving is idempotent).
    async fn claim_chunked_placeholder(
        &self,
        out: &ValidatedOutput,
    ) -> Result<Option<uuid::Uuid>, Status> {
        let refs_str: Vec<String> = out.info.references.iter().map(|r| r.to_string()).collect();
        match self
            .claim_placeholder(
                &out.info.store_path_hash,
                out.info.store_path.as_str(),
                &refs_str,
                "PutPathChunked",
            )
            .await
        {
            Ok(PlaceholderClaim::Owned(c)) => Ok(Some(c)),
            Ok(PlaceholderClaim::AlreadyComplete) => Ok(None),
            Ok(PlaceholderClaim::Concurrent) => {
                if let Ok(true) =
                    metadata::check_manifest_complete(&self.pool, &out.info.store_path_hash).await
                {
                    return Ok(None);
                }
                Err(Status::aborted(format!(
                    "{} for {}; retry",
                    rio_proto::CONCURRENT_PUTPATH_MSG,
                    out.info.store_path
                )))
            }
            Err(e) => Err(putpath_metadata_status(
                "PutPathChunked: claim_placeholder",
                e,
            )),
        }
    }

    /// Drain the remaining `Chunk` frames of an all-outputs-skipped
    /// stream. Each frame is still order-checked, BLAKE3-verified, and
    /// written to the backend — a re-driven builder already computed
    /// these bytes, the writes are content-addressed (cannot poison),
    /// and persisting them keeps the re-drive path's S3 state identical
    /// to the first drive's. Refcount-0 chunks written here are
    /// `r[store.chunk.grace-ttl]` sweep fodder if nothing ever
    /// references them.
    async fn drain_chunked_stream(
        &self,
        stream: &mut Streaming<PutPathChunkedRequest>,
        validated: &ValidatedBegin,
        backend: &Arc<dyn ChunkBackend>,
    ) -> Result<(), Status> {
        let mut next_novel = 0usize;
        let drain = async {
            while let Some(msg) = stream.message().await? {
                let chunk = expect_chunk(msg)?;
                let (digest, bytes) =
                    check_novel_frame(&chunk, validated, next_novel).map_err(|e| *e)?;
                next_novel += 1;
                backend
                    .put(&digest, bytes)
                    .await
                    .map_err(|e| storage_error("PutPathChunked: drain cas::put", e))?;
            }
            Ok::<_, Status>(())
        };
        match tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, drain).await {
            Ok(r) => r,
            Err(_) => Err(Status::deadline_exceeded(
                "PutPathChunked drain timed out; client is sending too slowly",
            )),
        }
    }

    /// The single sequential verify walk (§6.3). Iterates every
    /// output's NAR segments in order, feeding framing bytes and chunk
    /// bodies into that output's SHA-256 + refscan accumulators. Novel
    /// chunks come from the stream (order-enforced, BLAKE3-verified,
    /// written to the backend on arrival); everything else comes from
    /// the chunk cache. Returns early on the first protocol violation
    /// (`Err`), and reports Incomplete/Unavailable/Mismatch via
    /// [`WalkOutcome`] so the caller can map them to the §6.3 verdict
    /// table.
    // r[impl store.chunk.self-verify]
    #[allow(clippy::too_many_arguments)]
    async fn verify_walk(
        &self,
        stream: &mut Streaming<PutPathChunkedRequest>,
        validated: &ValidatedBegin,
        accs: &mut [Option<OutputAcc>],
        cache: &Arc<ChunkCache>,
        backend: &Arc<dyn ChunkBackend>,
    ) -> Result<WalkOutcome, Status> {
        let mut seen: HashSet<[u8; 32]> = HashSet::with_capacity(validated.novel.len());
        // Framing chunks track their own seen-set: sharing `seen` with
        // the novel cursor would let a framing run whose bytes happen
        // to equal a novel content chunk poison the "first occurrence"
        // test and desynchronize the stream.
        let mut seen_framing: HashSet<[u8; 32]> = HashSet::new();
        let mut next_novel = 0usize;

        // Bounded prefetch of upcoming non-novel chunk fetches into the
        // shared moka LRU so S3 reads overlap with `stream.next()`
        // waits. Purely an optimization: the walk's own `get_verified`
        // is authoritative and the prefetch task's errors are ignored.
        // Aborted on drop (when this function returns). Skipped
        // outputs' chunks are excluded — the walk never fetches them.
        let verified: Vec<bool> = accs.iter().map(|a| a.is_some()).collect();
        let prefetch = spawn_prefetch(validated, &verified, cache);
        let _abort_prefetch = scopeguard::guard(prefetch, |h| h.abort());

        for (out_idx, out) in validated.outputs.iter().enumerate() {
            let mut cursor = 0usize; // position in out.chunk_manifest
            for seg in &out.segments {
                match seg {
                    NarSegment::Framing { bytes, digest } => {
                        // Server-generated framing runs are persisted
                        // as ordinary CAS chunks so the manifest's
                        // chunk list concatenates to the full NAR (the
                        // GetPath / GC invariant). Deduped per stream;
                        // identical framing across uploads dedups by
                        // content key in S3. Skipped outputs don't
                        // commit a manifest, so their framing isn't
                        // persisted (a non-skipped output sharing the
                        // same framing bytes uploads its own copy).
                        if accs[out_idx].is_some()
                            && seen_framing.insert(*digest)
                            && let Err(e) = backend.put(digest, Bytes::copy_from_slice(bytes)).await
                        {
                            return Ok(WalkOutcome::Unavailable(format!(
                                "cas::put(framing {}): {e}",
                                hex::encode(digest)
                            )));
                        }
                        if let Some(acc) = &mut accs[out_idx] {
                            acc.hasher.update(bytes);
                            // RefScanSink::write never fails.
                            let _ = acc.scanner.write_all(bytes);
                        }
                    }
                    NarSegment::FileContents {
                        n_chunks,
                        file_digest,
                    } => {
                        // Per-file BLAKE3 over the spliced contents.
                        // The claimed `FileEntry.digest` is what the
                        // commit persists into `file_blobs` and what
                        // `ReadBlob`/`StatBlob`/`HasBlobs` resolve
                        // content by, with no read-side verification —
                        // an unverified claim would let a builder
                        // register an arbitrary digest → its own bytes
                        // and have the store serve them for another
                        // path's file. Skipped outputs don't fetch
                        // bodies and don't write `file_blobs`, so the
                        // check is scoped to verified outputs.
                        let mut file_hasher = accs[out_idx].is_some().then(blake3::Hasher::new);
                        for _ in 0..*n_chunks {
                            let (digest, len) = out.chunk_manifest[cursor];
                            cursor += 1;
                            let body = if validated.novel_set.contains(&digest)
                                && !seen.contains(&digest)
                            {
                                // First occurrence of a novel digest:
                                // the next stream frame MUST carry it.
                                let Some(msg) = stream.message().await? else {
                                    return Ok(WalkOutcome::Incomplete);
                                };
                                let chunk = expect_chunk(msg)?;
                                let (digest, bytes) =
                                    match check_novel_frame(&chunk, validated, next_novel) {
                                        Ok(x) => x,
                                        Err(e) => return Err(*e),
                                    };
                                next_novel += 1;
                                seen.insert(digest);
                                // r[impl store.cas.upload-bounded]
                                // One chunk in flight per stream — the
                                // bound is structural here, not a
                                // semaphore.
                                if let Err(e) = backend.put(&digest, bytes.clone()).await {
                                    return Ok(WalkOutcome::Unavailable(format!(
                                        "cas::put({}): {e}",
                                        hex::encode(digest)
                                    )));
                                }
                                // Warm the shared LRU so a repeat
                                // occurrence (same digest later in any
                                // output) is a memory hit instead of a
                                // backend round-trip.
                                cache.insert_local(&digest, bytes.clone()).await;
                                Some(bytes)
                            } else if accs[out_idx].is_none() {
                                // Idempotent-skipped output: nothing to
                                // verify, so don't fetch the body. The
                                // already-complete manifest may have
                                // been chunked differently (legacy
                                // whole-NAR FastCDC), in which case
                                // these per-file digests don't exist in
                                // the CAS — fetching them would fail an
                                // upload that has nothing to do with
                                // this output.
                                None
                            } else {
                                // Deduped or repeat: fetch from the
                                // CAS. `get_verified` BLAKE3-checks the
                                // bytes against the digest.
                                match cache.get_verified(&digest).await {
                                    Ok(b) => {
                                        if b.len() as u64 != u64::from(len) {
                                            return Ok(WalkOutcome::Mismatch(format!(
                                                "chunk {} is {} bytes in the CAS but the \
                                                 manifest declares {}",
                                                hex::encode(digest),
                                                b.len(),
                                                len
                                            )));
                                        }
                                        Some(b)
                                    }
                                    Err(e) => {
                                        return Ok(WalkOutcome::Unavailable(format!(
                                            "cas::get({}): {e}",
                                            hex::encode(digest)
                                        )));
                                    }
                                }
                            };
                            if let (Some(acc), Some(body)) = (&mut accs[out_idx], &body) {
                                acc.hasher.update(body);
                                let _ = acc.scanner.write_all(body);
                            }
                            if let (Some(h), Some(body)) = (&mut file_hasher, &body) {
                                h.update(body);
                            }
                        }
                        if let Some(h) = file_hasher {
                            let computed = *h.finalize().as_bytes();
                            if computed != *file_digest {
                                return Ok(WalkOutcome::FileDigestMismatch(format!(
                                    "FileEntry claims digest {} but its contents hash to {}",
                                    hex::encode(file_digest),
                                    hex::encode(computed)
                                )));
                            }
                        }
                    }
                }
            }
        }

        // After the walk: any extra frame is a protocol violation; an
        // unexhausted novel list means the builder closed early.
        if next_novel < validated.novel.len() {
            return Ok(WalkOutcome::Incomplete);
        }
        if stream.message().await?.is_some() {
            return Err(Status::invalid_argument(
                "extra Chunk frame after all novel chunks were received",
            ));
        }
        Ok(WalkOutcome::Complete)
    }
}

/// Number of non-skipped outputs, for the per-store-path error counter.
fn unskipped(skipped: &[bool]) -> u64 {
    skipped.iter().filter(|s| !**s).count() as u64
}

/// `info.deriver` as a loggable string.
fn begin_deriver(info: &ValidatedPathInfo) -> &str {
    info.deriver.as_ref().map_or("", |d| d.as_str())
}

/// `claims.executor_id` as a loggable string.
fn executor_id(claims: Option<&rio_auth::hmac::AssignmentClaims>) -> &str {
    claims.map_or("", |c| c.executor_id.as_str())
}

/// Reject any non-`Chunk` frame after `Begin`.
fn expect_chunk(msg: PutPathChunkedRequest) -> Result<PutPathChunkedChunk, Status> {
    match msg.msg {
        Some(put_path_chunked_request::Msg::Chunk(c)) => Ok(c),
        Some(put_path_chunked_request::Msg::Begin(_)) => Err(Status::invalid_argument(
            "duplicate Begin mid-stream (protocol violation)",
        )),
        None => Err(Status::invalid_argument(
            "PutPathChunked message has no content",
        )),
    }
}

/// Validate one novel `Chunk` frame against the wire-order contract:
/// `frame.digest == novel[next_novel]`, `len(bytes) == manifest_len[d]`,
/// `blake3(bytes) == d`. All violations → `INVALID_ARGUMENT` (boxed so
/// the hot loop's `Result` stays one word).
fn check_novel_frame(
    chunk: &PutPathChunkedChunk,
    validated: &ValidatedBegin,
    next_novel: usize,
) -> Result<([u8; 32], Bytes), Box<Status>> {
    let Some(expected) = validated.novel.get(next_novel) else {
        return Err(Box::new(Status::invalid_argument(
            "extra Chunk frame after all novel chunks were received",
        )));
    };
    let digest: [u8; 32] = chunk.digest.as_slice().try_into().map_err(|_| {
        Box::new(Status::invalid_argument(format!(
            "Chunk digest must be 32 bytes, got {}",
            chunk.digest.len()
        )))
    })?;
    if digest != *expected {
        return Err(Box::new(Status::invalid_argument(format!(
            "Chunk frame {next_novel} carries digest {} but novel[{next_novel}] is {} \
             (frames must arrive in Begin.novel order)",
            hex::encode(digest),
            hex::encode(expected)
        ))));
    }
    let declared = validated.manifest_len[&digest];
    if chunk.data.len() != declared as usize {
        return Err(Box::new(Status::invalid_argument(format!(
            "Chunk {} is {} bytes but the manifest declares {}",
            hex::encode(digest),
            chunk.data.len(),
            declared
        ))));
    }
    // Defense in depth: `declared` is already bounded by CHUNK_MAX at
    // validation time, so this can only fire if the two checks drift.
    if chunk.data.len() > CHUNK_MAX {
        return Err(Box::new(Status::invalid_argument(format!(
            "Chunk {} exceeds CHUNK_MAX {CHUNK_MAX}",
            hex::encode(digest)
        ))));
    }
    let actual = *blake3::hash(&chunk.data).as_bytes();
    if actual != digest {
        return Err(Box::new(Status::invalid_argument(format!(
            "Chunk claims digest {} but its bytes hash to {}",
            hex::encode(digest),
            hex::encode(actual)
        ))));
    }
    Ok((digest, Bytes::copy_from_slice(&chunk.data)))
}

/// Spawn the bounded deduped-chunk prefetch task. Walks the global
/// position sequence, issues `get_verified` for each first occurrence
/// of a non-novel digest with at most [`PREFETCH_WINDOW`] in flight,
/// and discards the results — the value is the side effect of warming
/// the shared moka LRU. Errors are ignored (the verify walk's own
/// fetch is authoritative).
fn spawn_prefetch(
    validated: &ValidatedBegin,
    verified: &[bool],
    cache: &Arc<ChunkCache>,
) -> tokio::task::JoinHandle<()> {
    // The digests to prefetch, in walk order, first occurrence only,
    // restricted to outputs the walk will actually verify.
    let mut seen: HashSet<[u8; 32]> = HashSet::new();
    let to_fetch: Vec<[u8; 32]> = validated
        .outputs
        .iter()
        .zip(verified)
        .filter(|(_, v)| **v)
        .flat_map(|(o, _)| o.chunk_manifest.iter())
        .filter(|(d, _)| !validated.novel_set.contains(d) && seen.insert(*d))
        .map(|(d, _)| *d)
        .collect();
    let cache = Arc::clone(cache);
    rio_common::task::spawn_monitored("putpath-chunked-prefetch", async move {
        use futures_util::StreamExt;
        futures_util::stream::iter(to_fetch)
            .map(|d| {
                let cache = Arc::clone(&cache);
                async move {
                    let _ = cache.get_verified(&d).await;
                }
            })
            .buffer_unordered(PREFETCH_WINDOW)
            .collect::<()>()
            .await;
    })
}
