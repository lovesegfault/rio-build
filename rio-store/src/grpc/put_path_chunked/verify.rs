//! The §6.3 sequential receive walk for `PutPathChunked`.
//!
//! One task walks every output's `chunk_manifest` in order and consumes
//! the `Chunk` frame for each novel digest's global first occurrence.
//! Every received body is BLAKE3-verified against its claimed digest
//! and length-checked against the manifest before anything trusts it
//! ([`recv_novel_chunk`]); backend PUTs are pipelined through
//! [`UploadPipeline`]. The builder's `nar_hash`/`nar_size`/references
//! claims are committed as claimed — the builder's fused walk computed
//! them from the same bytes it uploaded, and the store does not
//! regenerate the NAR to re-derive them
//! (`r[store.integrity.verify-on-put+3]`). Whole-file digests are NOT
//! committed as claimed: they key the digest-addressed `file_blobs`
//! dedup namespace `ReadBlob`/`StatBlob`/`HasBlobs` resolve across
//! tenants, so every file run is recomputed here while its bytes are
//! in hand; runs whose bytes are not all on this stream are handed to
//! [`super::file_digest`] for the metadata/refetch proof. Deduped
//! chunks are never fetched by THIS walk: their digests were verified
//! when they were first uploaded, and the commit transaction's
//! presence proof (`lock_chunks_for_commit`) is what binds them to
//! this manifest.
//!
//! There is **no whole-NAR buffer** and no `nar_bytes_budget` charge:
//! the working set is one chunk frame in hand (bounded by the gRPC
//! message cap) plus at most `chunk_upload_max_concurrent` chunk
//! bodies whose backend PUTs are still in flight. That is the point of
//! this RPC — the standing 40 GiB-RSS hazard of the buffered `PutPath`
//! path does not exist here. The per-run file hasher adds O(1) state
//! per output (one incremental BLAKE3 over bodies already in memory).
//!
//! Because `validate_begin` proved `Begin.novel` is ordered by global
//! first occurrence, the next `Chunk` frame the builder must send is
//! always exactly `novel[next_novel]` — a single cursor replaces the
//! receive loop / oneshot map / window semaphore a per-output driver
//! design would need.

use std::collections::HashSet;
use std::sync::Arc;

use tonic::{Status, Streaming};

use rio_proto::types::{ChunkData, PutPathChunkedRequest, put_path_chunked_request};

use crate::backend::ChunkBackend;
use crate::cas;

use super::file_digest::{DeferredRun, file_digest_mismatch};
use super::validate::ValidatedBegin;

/// Outcome of the §6.3 walk. `Err(Status)` from [`run_verify`] is
/// reserved for protocol violations that are unambiguously the
/// client's fault (`INVALID_ARGUMENT`) or server bugs (`INTERNAL`);
/// everything that maps to the §6.3 verdict table comes back as a
/// variant here so the handler owns the status-code + metric mapping.
pub(super) enum Verdict {
    /// Every novel chunk arrived, hashed to its digest, and was
    /// confirmed written to the backend.
    Match {
        /// The novel digests that were actually written to the chunk
        /// backend (the commit transaction stamps `uploaded_at` for
        /// exactly these).
        uploaded: HashSet<[u8; 32]>,
        /// File runs whose whole-file digest could NOT be recomputed
        /// inline (some chunk's bytes were not received on this
        /// stream). The handler MUST prove these via
        /// [`super::file_digest::verify_deferred_runs`] before commit.
        deferred: Vec<DeferredRun>,
    },
    /// The stream ended before every `novel` digest arrived.
    Incomplete,
    /// A transient backend failure (S3 fault). Retryable.
    Unavailable(String),
}

/// Bounded pipeline of in-flight novel-chunk PUTs.
///
/// PUTs are spawned tasks, so they progress while the walk awaits the
/// next stream frame — the overlap that turns ~22 ms-per-chunk serial
/// ingest into `max_concurrent`-wide pipelined ingest. Bodies are
/// digest-verified before submission; only the backend write is
/// deferred.
// r[impl store.cas.upload-bounded+2]
struct UploadPipeline {
    backend: Arc<dyn ChunkBackend>,
    in_flight: tokio::task::JoinSet<([u8; 32], Result<(), String>)>,
    /// Digests handed to the pipeline (confirmed or still in flight) —
    /// the first-occurrence guard. A digest is PUT exactly once.
    submitted: HashSet<[u8; 32]>,
    /// Digests whose PUT completed successfully — the only digests the
    /// commit transaction may stamp `uploaded_at` for.
    uploaded: HashSet<[u8; 32]>,
    max_concurrent: usize,
}

impl UploadPipeline {
    fn new(backend: Arc<dyn ChunkBackend>, max_concurrent: usize, novel_count: usize) -> Self {
        Self {
            backend,
            in_flight: tokio::task::JoinSet::new(),
            submitted: HashSet::with_capacity(novel_count),
            uploaded: HashSet::with_capacity(novel_count),
            // .max(1): a zero bound would deadlock submit(); Config
            // rejects 0 at startup but library callers bypass Config.
            max_concurrent: max_concurrent.max(1),
        }
    }

    /// First-occurrence check: true iff `digest` was already handed to
    /// the pipeline (confirmed or still in flight).
    fn seen(&self, digest: &[u8; 32]) -> bool {
        self.submitted.contains(digest)
    }

    /// Spawn one PUT, first joining completed ones until below the
    /// concurrency bound. An error from any joined PUT surfaces here.
    async fn submit(&mut self, digest: [u8; 32], body: bytes::Bytes) -> Result<(), String> {
        while self.in_flight.len() >= self.max_concurrent {
            self.join_one().await?;
        }
        self.submitted.insert(digest);
        let backend = Arc::clone(&self.backend);
        self.in_flight.spawn(async move {
            // zstd-at-rest: `body` was digest-verified as plaintext by
            // recv_novel_chunk; the STORED form is compressed (digest
            // space unchanged — r[store.cas.zstd-at-rest]).
            let stored = cas::compress_chunk(&body);
            let result = backend
                .put(&digest, stored)
                .await
                .map_err(|e| format!("chunk upload failed: {e:#}"));
            (digest, result)
        });
        Ok(())
    }

    async fn join_one(&mut self) -> Result<(), String> {
        match self.in_flight.join_next().await {
            None => Ok(()),
            Some(Ok((digest, result))) => {
                result?;
                self.uploaded.insert(digest);
                Ok(())
            }
            Some(Err(join)) => Err(format!("chunk upload task failed: {join}")),
        }
    }

    /// Join everything still in flight and hand back the confirmed
    /// set. MUST run before the commit step — `uploaded` only ever
    /// contains digests whose PUT succeeded.
    async fn drain(mut self) -> Result<HashSet<[u8; 32]>, String> {
        while !self.in_flight.is_empty() {
            self.join_one().await?;
        }
        Ok(self.uploaded)
    }
}

/// Run the §6.3 receive walk over the remaining stream frames: consume
/// the `Chunk` frame for each novel digest's global first occurrence
/// (in `Begin.novel` order), digest-verify it, and pipeline its
/// backend PUT. Outputs whose digests are all deduped contribute no
/// stream traffic and no backend reads — the walk only ever touches
/// bodies the builder sent.
///
/// The walk iterates each output's `file_runs` (validate_begin proved
/// they partition `chunk_manifest` in order, so the receive sequence
/// is unchanged) and recomputes the whole-file BLAKE3 for every run of
/// a non-skipped output whose bytes all arrive on this stream — a
/// mismatch with the claimed `FileRun::digest` rejects the upload
/// before anything is committed. Runs with any chunk NOT in hand (a
/// deduped digest, or a repeat of a novel digest whose body was
/// already pipelined away) are returned as `deferred` for the
/// metadata/refetch proof in [`super::file_digest`].
// r[impl store.chunk.self-verify]
// r[impl store.integrity.verify-on-put+3]
pub(super) async fn run_verify(
    stream: &mut Streaming<PutPathChunkedRequest>,
    validated: &ValidatedBegin,
    skipped: &[bool],
    backend: &Arc<dyn ChunkBackend>,
    chunk_upload_max_concurrent: usize,
) -> Result<Verdict, Status> {
    let novel_set: HashSet<[u8; 32]> = validated.novel.iter().copied().collect();
    let mut next_novel = 0usize;
    let mut pipeline = UploadPipeline::new(
        Arc::clone(backend),
        chunk_upload_max_concurrent,
        validated.novel.len(),
    );
    let mut deferred: Vec<DeferredRun> = Vec::new();

    for (oi, out) in validated.outputs.iter().enumerate() {
        // Idempotent-skipped outputs commit no `file_blobs` rows, so
        // their digest claims bind nothing — receive their novel
        // chunks (the wire protocol is position-based) but skip the
        // recompute. CA outputs are not yet claimable here (phase C),
        // so `skipped[oi]` is false for them and they ARE verified.
        let check = !skipped[oi];
        for (ri, run) in out.file_runs.iter().enumerate() {
            // `Some` while every chunk so far arrived on this stream —
            // the only case where the run's bytes are in hand. An
            // empty run hashes zero bytes, so an empty file's claimed
            // digest must be blake3("").
            let mut hasher = if check {
                Some(blake3::Hasher::new())
            } else {
                None
            };
            for j in run.chunks.clone() {
                let (digest, len) = out.chunk_manifest[j];
                if novel_set.contains(&digest) && !pipeline.seen(&digest) {
                    let body =
                        match recv_novel_chunk(stream, &digest, len, validated, &mut next_novel)
                            .await?
                        {
                            Some(b) => b,
                            None => return Ok(Verdict::Incomplete),
                        };
                    if let Some(mut h) = hasher.take() {
                        // `Bytes` clone is a refcount bump; the update
                        // re-reads a buffer already in memory.
                        let b = body.clone();
                        hasher = Some(cas::cpu_bound(move || {
                            h.update(&b);
                            h
                        }));
                    }
                    if let Err(e) = pipeline.submit(digest, body).await {
                        return Ok(Verdict::Unavailable(e));
                    }
                } else {
                    // Bytes not on this stream (deduped, or a repeat
                    // occurrence) — the inline recompute can't finish.
                    hasher = None;
                }
            }
            match hasher {
                Some(h) => {
                    let actual: [u8; 32] = h.finalize().into();
                    if actual != run.digest {
                        return Err(file_digest_mismatch(oi, &run.digest, &actual));
                    }
                }
                None if check => deferred.push(DeferredRun {
                    output: oi,
                    run: ri,
                }),
                None => {}
            }
        }
    }

    // The walk visits every (output, manifest position) pair in global
    // first-occurrence order, so a completed walk has consumed every
    // novel digest; an early stream end was already returned as
    // Incomplete from inside the loop.
    if next_novel < validated.novel.len() {
        return Ok(Verdict::Incomplete);
    }

    // Exactly `novel` Chunk frames are permitted. An extra frame is a
    // protocol violation, not a verdict.
    if next_message(stream).await?.is_some() {
        return Err(Status::invalid_argument(
            "PutPathChunked: extra frame after all novel chunks were received",
        ));
    }

    // Every PUT must be confirmed before the verdict reaches the
    // commit step — `uploaded` only contains digests whose write
    // succeeded, so a straggler failure is an Unavailable here, never
    // a committed manifest referencing a chunk that was never stored.
    let uploaded = match pipeline.drain().await {
        Ok(u) => u,
        Err(e) => return Ok(Verdict::Unavailable(e)),
    };

    Ok(Verdict::Match { uploaded, deferred })
}

/// Receive the next `Chunk` frame and validate it against the position
/// the walk is at: the frame's digest must equal both the manifest
/// position's digest and `novel[next_novel]`, the body length must
/// equal the manifest's claimed length, and the body must hash to the
/// digest. Any disagreement is a protocol violation
/// (`INVALID_ARGUMENT`); `Ok(None)` means the stream ended (the caller
/// maps that to [`Verdict::Incomplete`]).
// r[impl store.integrity.verify-on-put+3]
async fn recv_novel_chunk(
    stream: &mut Streaming<PutPathChunkedRequest>,
    expected_digest: &[u8; 32],
    expected_len: u32,
    validated: &ValidatedBegin,
    next_novel: &mut usize,
) -> Result<Option<bytes::Bytes>, Status> {
    let Some(msg) = next_message(stream).await? else {
        return Ok(None);
    };
    let frame: ChunkData = match msg {
        put_path_chunked_request::Msg::Chunk(c) => c,
        put_path_chunked_request::Msg::Begin(_) => {
            return Err(Status::invalid_argument(
                "PutPathChunked: duplicate Begin frame mid-stream",
            ));
        }
    };
    // Length-check the wire digest before comparing or echoing it: a
    // frame with a multi-megabyte `digest` field would otherwise be
    // hex-encoded (2× allocation) into the error message below.
    if frame.digest.len() != 32 {
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: Chunk frame digest must be 32 bytes, got {}",
            frame.digest.len()
        )));
    }
    // The §6.2 ordering invariant: the next frame is always exactly
    // novel[next_novel], which is also the digest at the current walk
    // position (this fn is only called at first occurrences of novel
    // digests, in walk order).
    let expected_novel = validated.novel.get(*next_novel).ok_or_else(|| {
        Status::invalid_argument("PutPathChunked: Chunk frame after novel was exhausted")
    })?;
    if frame.digest.as_slice() != expected_digest.as_slice() || expected_digest != expected_novel {
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: Chunk frame out of order: expected novel[{}] = {}, got {}",
            *next_novel,
            hex::encode(expected_novel),
            hex::encode(&frame.digest),
        )));
    }
    if frame.data.len() != expected_len as usize {
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: chunk {} body is {} bytes, manifest claims {expected_len}",
            hex::encode(expected_digest),
            frame.data.len(),
        )));
    }
    let data = frame.data;
    // BLAKE3 over up to FASTCDC_MAX_BYTES — cheap, but keep it off the
    // async reactor the same way every other ingest hash is.
    let actual = cas::cpu_bound(|| *blake3::hash(&data).as_bytes());
    if actual != *expected_digest {
        return Err(Status::invalid_argument(format!(
            "PutPathChunked: chunk body does not hash to its claimed digest {}",
            hex::encode(expected_digest),
        )));
    }
    *next_novel += 1;
    Ok(Some(data))
}

/// Pull the next non-empty frame off the stream, unwrapping the oneof.
/// A frame with no `msg` set is a protocol violation.
async fn next_message(
    stream: &mut Streaming<PutPathChunkedRequest>,
) -> Result<Option<put_path_chunked_request::Msg>, Status> {
    match stream.message().await? {
        None => Ok(None),
        Some(PutPathChunkedRequest { msg: None }) => Err(Status::invalid_argument(
            "PutPathChunked: frame has no content",
        )),
        Some(PutPathChunkedRequest { msg: Some(m) }) => Ok(Some(m)),
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use bytes::Bytes;
    use std::sync::atomic::{AtomicUsize, Ordering};

    /// Counts in-flight PUTs and their high-water mark; a short sleep
    /// keeps each PUT alive long enough for later submissions to
    /// overlap it. Mirrors `cas::tests::HighWaterBackend`.
    #[derive(Default)]
    struct HighWaterBackend {
        in_flight: AtomicUsize,
        high_water: AtomicUsize,
    }

    #[async_trait::async_trait]
    impl ChunkBackend for HighWaterBackend {
        async fn put(&self, _hash: &[u8; 32], _data: Bytes) -> anyhow::Result<()> {
            let now = self.in_flight.fetch_add(1, Ordering::SeqCst) + 1;
            self.high_water.fetch_max(now, Ordering::SeqCst);
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
            self.in_flight.fetch_sub(1, Ordering::SeqCst);
            Ok(())
        }
        async fn get(&self, _: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
            unimplemented!("pipeline tests use put only")
        }
        async fn exists_batch(&self, _: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            unimplemented!()
        }
        fn key_for(&self, _: &[u8; 32]) -> String {
            unimplemented!()
        }
        async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn put_blob(&self, _: &str, _: Bytes) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_blob(&self, _: &str) -> anyhow::Result<Option<Bytes>> {
            unimplemented!()
        }
        async fn delete_blob(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    /// Fails the PUT for one specific digest; stores nothing.
    struct FailDigestBackend {
        poison: [u8; 32],
    }

    #[async_trait::async_trait]
    impl ChunkBackend for FailDigestBackend {
        async fn put(&self, hash: &[u8; 32], _data: Bytes) -> anyhow::Result<()> {
            if *hash == self.poison {
                anyhow::bail!("injected S3 fault");
            }
            Ok(())
        }
        async fn get(&self, _: &[u8; 32]) -> anyhow::Result<Option<Bytes>> {
            unimplemented!()
        }
        async fn exists_batch(&self, _: &[[u8; 32]]) -> anyhow::Result<Vec<bool>> {
            unimplemented!()
        }
        fn key_for(&self, _: &[u8; 32]) -> String {
            unimplemented!()
        }
        async fn delete_by_key(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn put_blob(&self, _: &str, _: Bytes) -> anyhow::Result<()> {
            unimplemented!()
        }
        async fn get_blob(&self, _: &str) -> anyhow::Result<Option<Bytes>> {
            unimplemented!()
        }
        async fn delete_blob(&self, _: &str) -> anyhow::Result<()> {
            unimplemented!()
        }
    }

    fn digest(i: u8) -> [u8; 32] {
        let mut d = [0u8; 32];
        d[0] = i;
        d
    }

    // r[verify store.cas.upload-bounded+2]
    #[tokio::test]
    async fn pipeline_overlaps_puts_and_respects_the_bound() {
        let backend = Arc::new(HighWaterBackend::default());
        let mut p = UploadPipeline::new(Arc::clone(&backend) as Arc<dyn ChunkBackend>, 8, 32);
        for i in 0..32u8 {
            p.submit(digest(i), Bytes::from_static(b"x")).await.unwrap();
        }
        let uploaded = p.drain().await.unwrap();
        assert_eq!(uploaded.len(), 32, "every confirmed PUT is in the set");
        let hw = backend.high_water.load(Ordering::SeqCst);
        // Structural, not wall-clock: >1 proves PUTs overlapped (the
        // serial regression this pipeline exists to fix would read 1);
        // <=8 proves the concurrency bound held.
        assert!(hw > 1, "PUTs never overlapped (high_water={hw})");
        assert!(hw <= 8, "concurrency bound violated (high_water={hw})");
    }

    #[tokio::test]
    async fn pipeline_put_failure_surfaces_as_error() {
        let backend = Arc::new(FailDigestBackend { poison: digest(3) });
        let mut p = UploadPipeline::new(backend as Arc<dyn ChunkBackend>, 4, 8);
        // The poisoned PUT's failure surfaces at the next join point —
        // a later submit or the final drain — never silently.
        let mut failed = None;
        for i in 0..8u8 {
            if let Err(e) = p.submit(digest(i), Bytes::from_static(b"x")).await {
                failed = Some(e);
                break;
            }
        }
        let err = match failed {
            Some(e) => e,
            None => p
                .drain()
                .await
                .expect_err("poisoned PUT must fail the drain"),
        };
        assert!(
            err.contains("chunk upload failed"),
            "error keeps the Unavailable message shape: {err}"
        );
    }

    #[tokio::test]
    async fn pipeline_seen_guards_repeat_submission() {
        // An intra-stream repeat of a novel digest must not be PUT (or
        // received) twice: `seen` latches at submission, before the
        // PUT confirms.
        let backend = Arc::new(HighWaterBackend::default());
        let mut p = UploadPipeline::new(Arc::clone(&backend) as Arc<dyn ChunkBackend>, 8, 2);
        let d = digest(1);
        p.submit(d, Bytes::from_static(b"repeated-content"))
            .await
            .unwrap();
        assert!(p.seen(&d), "submitted digest counts as seen immediately");
        let uploaded = p.drain().await.unwrap();
        assert!(uploaded.contains(&d));
    }
}
