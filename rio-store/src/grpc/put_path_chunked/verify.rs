//! The §6.3 sequential verify walk for `PutPathChunked`.
//!
//! One task walks every output's `chunk_manifest` in order, regenerates
//! the NAR framing from the validated Directory tree, splices each
//! chunk body in (novel chunks straight off the stream, deduped chunks
//! from the CAS), and recomputes per output the NAR SHA-256, the
//! reference scan, and per regular file the whole-file BLAKE3 — all
//! against the builder's claims.
//!
//! There is **no whole-NAR buffer** and no `nar_bytes_budget` charge:
//! the working set is one chunk frame in hand (bounded by the gRPC
//! message cap) plus the SHA-256/refscan/BLAKE3 accumulator state and
//! at most `chunk_upload_max_concurrent` chunk bodies whose backend
//! PUTs are still in flight (see [`UploadPipeline`]). That is the
//! point of this RPC — the standing 40 GiB-RSS hazard of the buffered
//! `PutPath` path does not exist here.
//!
//! The verification walk is strictly sequential over the byte stream
//! (SHA-256/refscan ordering demands it); only the backend PUTs
//! overlap. Serial PUTs were the live bottleneck: one S3 PUT per
//! ~22 ms caps ingest at ~3 MB/s, so a 2.3 GiB output (~30 k FastCDC
//! chunks) takes ~11 minutes — structurally unable to fit the
//! client's 300 s stream budget.
//!
//! Because `validate_begin` proved `Begin.novel` is ordered by global
//! first occurrence, the next `Chunk` frame the builder must send is
//! always exactly `novel[next_novel]` — a single cursor replaces the
//! receive loop / oneshot map / window semaphore a per-output driver
//! design would need.
//
// TODO: bounded prefetch (§6.3) — overlap cas::get for deduped chunks
// with stream.next() waits; sequential fetches make a highly-deduped
// output O(n_chunks × S3_RTT). The same change should add admission
// control: a fully-deduped Begin referencing MAX_CHUNKS already-durable
// digests sends no chunk bodies but demands that many S3 GETs, and
// nothing bounds how many such streams run concurrently (there is no
// nar_bytes_budget charge here by design).

use std::collections::HashSet;
use std::io::Write as _;
use std::sync::Arc;

use sha2::{Digest, Sha256};
use tonic::{Status, Streaming};

use rio_nix::refscan::{CandidateSet, RefScanSink};
use rio_proto::types::{ChunkData, PutPathChunkedRequest, put_path_chunked_request};

use crate::backend::ChunkBackend;
use crate::cas::{self, ChunkCache};

use super::validate::ValidatedBegin;

/// Why a non-skipped output failed verification. Carried in
/// [`Verdict::Mismatch`] so the handler can pick the right counter and
/// log line.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub(super) enum MismatchReason {
    /// Recomputed NAR SHA-256 (or NAR byte count) differs from the
    /// claim.
    NarHash,
    /// Scanned reference set differs from the claimed one.
    Refs,
    /// A regular file's whole-file BLAKE3 over the spliced chunk bodies
    /// differs from the `FileEntry.digest` the Directory body claims.
    FileDigest,
    /// A deduped chunk fetched from the CAS has a different length than
    /// the manifest claims for it.
    ChunkLength,
}

impl MismatchReason {
    /// Human label for log/error messages.
    pub(super) fn as_str(self) -> &'static str {
        match self {
            MismatchReason::NarHash => "nar_hash",
            MismatchReason::Refs => "references",
            MismatchReason::FileDigest => "file_digest",
            MismatchReason::ChunkLength => "chunk_length",
        }
    }
}

/// Server-recomputed values for one non-skipped output, produced only
/// when every check passed. The commit transaction persists THESE, not
/// the builder's claims (they are equal at this point, but the
/// recomputed values are the attested ones).
#[derive(Debug)]
pub(super) struct OutputComputed {
    /// SHA-256 over the regenerated NAR byte stream
    /// (`r[store.put.narhash-sync]`).
    pub nar_hash: [u8; 32],
    /// Byte count of the regenerated NAR.
    pub nar_size: u64,
}

/// Outcome of the §6.3 walk. `Err(Status)` from [`run_verify`] is
/// reserved for protocol violations that are unambiguously the
/// client's fault (`INVALID_ARGUMENT`) or server bugs (`INTERNAL`);
/// everything that maps to the §6.3 verdict table comes back as a
/// variant here so the handler owns the status-code + metric mapping.
pub(super) enum Verdict {
    /// Every non-skipped output matched its claims.
    Match {
        /// One entry per output, `None` for idempotent-skipped ones.
        computed: Vec<Option<OutputComputed>>,
        /// The novel digests that were actually written to the chunk
        /// backend (the commit transaction stamps `uploaded_at` for
        /// exactly these).
        uploaded: HashSet<[u8; 32]>,
    },
    /// At least one claim failed for `output_idx`.
    Mismatch {
        output_idx: usize,
        reason: MismatchReason,
    },
    /// The stream ended before every `novel` digest arrived.
    Incomplete,
    /// A transient CAS failure (S3 fault, or a deduped chunk GC'd
    /// between the builder's `HasChunks` probe and now). Retryable.
    Unavailable(String),
}

/// Yield to the tokio scheduler after this many framing bytes with no
/// intervening chunk fetch. Content-free subtrees produce arbitrarily
/// long runs of `Framing` pieces with no await point; without a
/// periodic yield one verify walk monopolizes a worker thread for the
/// whole expansion. 4 MiB ≈ a few ms of SHA-256 — coarse enough to be
/// free, fine enough that the executor stays responsive.
const FRAMING_YIELD_BYTES: usize = 4 * 1024 * 1024;

/// Bounded pipeline of in-flight novel-chunk PUTs.
///
/// PUTs are spawned tasks, so they progress while the walk awaits the
/// next stream frame — the overlap that turns ~22 ms-per-chunk serial
/// ingest into `max_concurrent`-wide pipelined ingest. The walk's
/// verification order is untouched: bodies are hashed/scanned inline
/// before submission, and only the backend write is deferred.
///
/// `pending` doubles as the read-your-writes shim: an intra-stream
/// repeat of a novel chunk (zero-filled pages etc.) can be walked
/// before its first occurrence's PUT has landed, and the CAS fetch the
/// dedup branch falls back to would miss. It holds at most
/// `max_concurrent` bodies (submission blocks at capacity), each
/// bounded by the gRPC message cap — the memory-bound contract in the
/// module doc.
// r[impl store.cas.upload-bounded]
struct UploadPipeline {
    backend: Arc<dyn ChunkBackend>,
    in_flight: tokio::task::JoinSet<([u8; 32], Result<(), String>)>,
    /// Bodies whose PUT has been spawned but not yet confirmed.
    pending: std::collections::HashMap<[u8; 32], bytes::Bytes>,
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
            pending: std::collections::HashMap::new(),
            uploaded: HashSet::with_capacity(novel_count),
            // .max(1): a zero bound would deadlock submit(); Config
            // rejects 0 at startup but library callers bypass Config.
            max_concurrent: max_concurrent.max(1),
        }
    }

    /// First-occurrence check: true iff `digest` was already handed to
    /// the pipeline (confirmed or still in flight). A digest is PUT
    /// exactly once.
    fn seen(&self, digest: &[u8; 32]) -> bool {
        self.uploaded.contains(digest) || self.pending.contains_key(digest)
    }

    /// The body of a submitted-but-unconfirmed PUT, if any. Repeat
    /// occurrences read this instead of racing the CAS.
    fn body_in_flight(&self, digest: &[u8; 32]) -> Option<bytes::Bytes> {
        self.pending.get(digest).cloned()
    }

    /// Spawn one PUT, first joining completed ones until below the
    /// concurrency bound. An error from any joined PUT surfaces here.
    async fn submit(&mut self, digest: [u8; 32], body: bytes::Bytes) -> Result<(), String> {
        while self.in_flight.len() >= self.max_concurrent {
            self.join_one().await?;
        }
        self.pending.insert(digest, body.clone());
        let backend = Arc::clone(&self.backend);
        self.in_flight.spawn(async move {
            let result = backend
                .put(&digest, body)
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
                // Evict on failure too: `body_in_flight` must never
                // serve a body whose write failed.
                self.pending.remove(&digest);
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
        debug_assert!(
            self.pending.is_empty(),
            "drained pipeline still has pending bodies"
        );
        Ok(self.uploaded)
    }
}

/// Run the §6.3 sequential verify walk over the remaining stream
/// frames.
///
/// `skipped[i]` marks outputs that already have a `'complete'`
/// manifest: their framing is not regenerated and their bodies are not
/// hashed, but their `chunk_manifest` positions are still walked so
/// that a novel digest whose global first occurrence falls inside a
/// skipped output is still consumed from the stream (the builder
/// computed `novel` before it knew about the skip).
// r[impl store.chunk.self-verify]
// r[impl store.put.narhash-sync]
// r[impl store.put.refs-sync]
// r[impl store.integrity.verify-on-put]
pub(super) async fn run_verify(
    stream: &mut Streaming<PutPathChunkedRequest>,
    validated: &ValidatedBegin,
    skipped: &[bool],
    backend: &Arc<dyn ChunkBackend>,
    chunk_cache: &Arc<ChunkCache>,
    max_nodes: usize,
    chunk_upload_max_concurrent: usize,
) -> Result<Verdict, Status> {
    let novel_set: HashSet<[u8; 32]> = validated.novel.iter().copied().collect();
    let mut next_novel = 0usize;
    // First-occurrence tracking + the eventual commit set both live in
    // the pipeline: a novel digest is received and PUT exactly once,
    // at its first manifest position.
    let mut pipeline = UploadPipeline::new(
        Arc::clone(backend),
        chunk_upload_max_concurrent,
        validated.novel.len(),
    );

    // r[impl store.put.refs-sync]
    // One candidate set for every output: the attested input closure
    // plus every sibling output path. Identical to what the builder's
    // fused walk and Nix's scanForReferences see.
    let candidates = CandidateSet::from_paths(
        validated
            .input_closure
            .iter()
            .map(|p| p.as_str())
            .chain(validated.outputs.iter().map(|o| o.store_path.as_str())),
    );

    let mut computed: Vec<Option<OutputComputed>> = Vec::with_capacity(validated.outputs.len());

    for (i, out) in validated.outputs.iter().enumerate() {
        if skipped[i] {
            // Consume any novel chunks whose first occurrence lands in
            // this output; everything else needs no body at all (the
            // existing manifest already proves the content).
            for (digest, len) in &out.chunk_manifest {
                if novel_set.contains(digest) && !pipeline.seen(digest) {
                    let body =
                        match recv_novel_chunk(stream, digest, *len, validated, &mut next_novel)
                            .await?
                        {
                            Some(b) => b,
                            None => return Ok(Verdict::Incomplete),
                        };
                    if let Err(e) = pipeline.submit(*digest, body).await {
                        return Ok(Verdict::Unavailable(e));
                    }
                }
            }
            computed.push(None);
            continue;
        }

        let mut sha = Sha256::new();
        let mut scanner = RefScanSink::new(candidates.hashes());
        let mut nar_len = 0u64;
        let mut file_idx = 0usize;
        let mut framing_since_yield = 0usize;

        // The piece iterator is lazy on purpose: collecting it would
        // materialize every framing byte of the output up front, which
        // for a node-heavy tree is exactly the unbounded buffer this
        // RPC exists to avoid. Each `next()` emits at most one node's
        // framing (≤ FRAMING_FLUSH_BYTES + one entry name) — cheap
        // enough to run inline between awaits.
        for piece in
            crate::castore_nar::nar_pieces(&out.root_node, &validated.directories, max_nodes)
        {
            let piece = piece.map_err(|e| {
                // validate_begin already proved this walk succeeds.
                Status::internal(format!(
                    "PutPathChunked: outputs[{i}] framing walk failed after validation: {e}"
                ))
            })?;
            match piece {
                crate::castore_nar::NarPiece::Framing(bytes) => {
                    sha.update(&bytes);
                    scanner
                        .write_all(&bytes)
                        .expect("RefScanSink is infallible");
                    nar_len += bytes.len() as u64;
                    // Bail the moment the regenerated stream exceeds the
                    // claimed size instead of at the end of the walk. A
                    // Directory DAG with shared subtrees expands a
                    // few-MB Begin frame into gigabytes of framing
                    // (`MAX_NAR_ENTRIES` nodes × a max-length symlink
                    // target each); without this check all of it is
                    // SHA-256'd and Boyer-Moore-scanned before the size
                    // mismatch is noticed. With it, the work an output
                    // can demand is bounded by the `nar_size` it claims
                    // (≤ MAX_NAR_SIZE — the same ceiling a legacy
                    // PutPath pays to *receive* that many bytes).
                    if nar_len > out.nar_size {
                        return Ok(Verdict::Mismatch {
                            output_idx: i,
                            reason: MismatchReason::NarHash,
                        });
                    }
                    framing_since_yield += bytes.len();
                    if framing_since_yield >= FRAMING_YIELD_BYTES {
                        framing_since_yield = 0;
                        tokio::task::yield_now().await;
                    }
                }
                crate::castore_nar::NarPiece::Contents { digest, size } => {
                    let run = &out.file_runs[file_idx];
                    // validate_begin built file_runs from the same walk
                    // over the same tree; a disagreement is a server
                    // bug, not client input.
                    if run.digest != digest || run.size != size {
                        return Err(Status::internal(
                            "PutPathChunked: file_runs out of sync with the framing walk",
                        ));
                    }
                    let mut file_hasher = blake3::Hasher::new();
                    for (chunk_digest, chunk_len) in &out.chunk_manifest[run.chunks.clone()] {
                        let body =
                            if novel_set.contains(chunk_digest) && !pipeline.seen(chunk_digest) {
                                let body = match recv_novel_chunk(
                                    stream,
                                    chunk_digest,
                                    *chunk_len,
                                    validated,
                                    &mut next_novel,
                                )
                                .await?
                                {
                                    Some(b) => b,
                                    None => return Ok(Verdict::Incomplete),
                                };
                                if let Err(e) = pipeline.submit(*chunk_digest, body.clone()).await {
                                    return Ok(Verdict::Unavailable(e));
                                }
                                body
                            } else if let Some(body) = pipeline.body_in_flight(chunk_digest) {
                                // An intra-stream repeat of a novel chunk
                                // whose PUT is still in flight: the CAS may
                                // not have it yet, but the pipeline holds
                                // the (already digest-verified) body.
                                body
                            } else {
                                // A chunk that is supposed to already exist
                                // in the CAS (deduped against a prior
                                // upload, or a repeat of a novel chunk
                                // received earlier in this stream whose PUT
                                // has confirmed). The cache provides
                                // singleflight + LRU + BLAKE3 verification.
                                // The error is always a retryable
                                // Unavailable — a missing or corrupt
                                // deduped chunk means the CAS lost it, not
                                // that the builder lied.
                                match chunk_cache.get_verified(chunk_digest).await {
                                    Ok(b) => b,
                                    Err(e) => {
                                        return Ok(Verdict::Unavailable(format!(
                                            "deduped chunk fetch failed: {e}"
                                        )));
                                    }
                                }
                            };
                        if body.len() != *chunk_len as usize {
                            // The §6.2 manifest lengths are self-
                            // consistent with the tree but not bound to
                            // actual CAS object sizes until here.
                            return Ok(Verdict::Mismatch {
                                output_idx: i,
                                reason: MismatchReason::ChunkLength,
                            });
                        }
                        sha.update(&body);
                        scanner.write_all(&body).expect("RefScanSink is infallible");
                        file_hasher.update(&body);
                        nar_len += body.len() as u64;
                    }
                    // r[impl store.put.verify-file-digest]
                    // The NAR-level SHA-256 hashes the same bytes either
                    // way; this is what stops a Directory body from
                    // mapping a file digest to content that does not
                    // hash to it (the cross-tenant `file_digest →
                    // content` namespace ReadBlob serves from).
                    if *file_hasher.finalize().as_bytes() != run.digest {
                        return Ok(Verdict::Mismatch {
                            output_idx: i,
                            reason: MismatchReason::FileDigest,
                        });
                    }
                    file_idx += 1;
                }
            }
        }

        // r[impl store.put.narhash-sync]
        let nar_hash: [u8; 32] = sha.finalize().into();
        if nar_hash != out.nar_hash || nar_len != out.nar_size {
            return Ok(Verdict::Mismatch {
                output_idx: i,
                reason: MismatchReason::NarHash,
            });
        }

        // r[impl store.put.refs-sync]
        // The commit transaction persists the *claimed* references
        // (already parsed); this equality check is what entitles it to.
        let scanned = candidates.resolve(&scanner.into_found());
        let mut claimed: Vec<String> = out.references.iter().map(|r| r.to_string()).collect();
        claimed.sort();
        if scanned != claimed {
            return Ok(Verdict::Mismatch {
                output_idx: i,
                reason: MismatchReason::Refs,
            });
        }

        computed.push(Some(OutputComputed {
            nar_hash,
            nar_size: nar_len,
        }));
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

    Ok(Verdict::Match { computed, uploaded })
}

/// Receive the next `Chunk` frame and validate it against the position
/// the walk is at: the frame's digest must equal both the manifest
/// position's digest and `novel[next_novel]`, the body length must
/// equal the manifest's claimed length, and the body must hash to the
/// digest. Any disagreement is a protocol violation
/// (`INVALID_ARGUMENT`); `Ok(None)` means the stream ended (the caller
/// maps that to [`Verdict::Incomplete`]).
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

    // r[verify store.cas.upload-bounded]
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
    async fn pipeline_pending_body_serves_intra_stream_repeats() {
        // A repeat occurrence walked while the first occurrence's PUT
        // is still in flight must read the held body — the CAS may not
        // have the chunk yet (read-your-writes shim).
        let backend = Arc::new(HighWaterBackend::default());
        let mut p = UploadPipeline::new(Arc::clone(&backend) as Arc<dyn ChunkBackend>, 8, 2);
        let d = digest(1);
        let body = Bytes::from_static(b"repeated-content");
        p.submit(d, body.clone()).await.unwrap();
        assert!(p.seen(&d), "submitted digest counts as seen immediately");
        assert_eq!(
            p.body_in_flight(&d).as_deref(),
            Some(body.as_ref()),
            "in-flight body must be readable for repeats"
        );
        let uploaded = p.drain().await.unwrap();
        assert!(uploaded.contains(&d));
    }
}
