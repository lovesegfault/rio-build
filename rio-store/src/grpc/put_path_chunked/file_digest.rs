//! Deferred whole-file digest verification for `PutPathChunked`
//! (`r[store.integrity.verify-on-put+3]`).
//!
//! Whole-file BLAKE3 digests key the digest-addressed `file_blobs`
//! namespace that `ReadBlob`/`StatBlob`/`HasBlobs` resolve with no
//! caller binding beyond tenancy — a forged `digest → content` row
//! poisons reads for every tenant that can see any referrer of the
//! digest. The §6.3 receive walk recomputes the digest inline for
//! runs whose bytes all arrive on the stream; this module proves the
//! rest — runs containing deduped chunks (or repeats of novel chunks
//! whose bodies were already pipelined to the backend):
//!
//! 1. **Window agreement** (no bytes): if the claimed digest already
//!    has a committed chunked binding and the claimed chunk run is
//!    byte-identical in `(digest, size)` sequence to that binding's
//!    window, the content is identical (chunk digests determine
//!    content), so the claim inherits the prior proof. This is the
//!    fully-deduped-re-upload fast path — zero backend reads, which
//!    matters because a large fully-deduped upload re-fetching every
//!    chunk serially once outlived the client's stream timeout.
//! 2. **Refetch** (bytes): otherwise the run's chunks are fetched
//!    back from the backend (every PUT was confirmed before this
//!    runs), each body re-verified against its chunk digest, and the
//!    whole-file BLAKE3 recomputed. This is the incremental-build
//!    case: a modified file mixes deduped chunks into a digest the
//!    store has never committed, so only the bytes can prove it.
//!
//! A recompute mismatch is the forgery signal: `INVALID_ARGUMENT`,
//! nothing committed. Bindings committed before this gate existed
//! were claim-trusted; the window-agreement induction is anchored on
//! the deploy-time assumption that those rows are honest (legacy
//! `PutPath`/substituter rows always were — their digests are
//! server-computed from NAR bytes by `nar_ls`).

use std::collections::{BTreeSet, HashMap};
use std::sync::Arc;

use futures_util::StreamExt;
use sqlx::PgPool;
use tonic::Status;

use crate::backend::ChunkBackend;
use crate::cas;
use crate::manifest::{Manifest, ManifestEntry};
use crate::metadata;

use super::validate::{FileRun, ValidatedBegin};

/// A file run the receive walk could not recompute inline: indices
/// into `validated.outputs[output].file_runs[run]`.
pub(super) struct DeferredRun {
    pub output: usize,
    pub run: usize,
}

/// Ordered refetch width for the byte-proof path. Mirrors `ReadBlob`'s
/// prefetch shape; bounds deferred-verification memory to
/// `REFETCH_PREFETCH_K × FASTCDC_MAX_BYTES` (2 MiB).
const REFETCH_PREFETCH_K: usize = 8;

/// The forgery rejection: shared by the inline (verify walk) and
/// deferred paths so both emit the same status and metric.
pub(super) fn file_digest_mismatch(output: usize, claimed: &[u8; 32], actual: &[u8; 32]) -> Status {
    metrics::counter!("rio_store_integrity_failures_total", "site" => "put_path_chunked")
        .increment(1);
    Status::invalid_argument(format!(
        "PutPathChunked: outputs[{output}] file content hashes to {}, not the claimed \
         file digest {}",
        hex::encode(actual),
        hex::encode(claimed),
    ))
}

/// Prove every deferred run's `digest → chunk-run` binding (window
/// agreement, else refetch). `Err` is terminal for the RPC: a forged
/// digest is `INVALID_ARGUMENT`; a chunk that disappeared since the
/// walk (GC race / S3 fault) is `UNAVAILABLE` for the builder to
/// retry, mirroring the commit-time presence-proof failure — and its
/// `chunks` presence row is demoted so the retry's `HasChunks` answers
/// honestly instead of repeating the skip that hit the miss.
// r[impl store.integrity.verify-on-put+3]
pub(super) async fn verify_deferred_runs(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    validated: &ValidatedBegin,
    deferred: &[DeferredRun],
) -> Result<(), Status> {
    if deferred.is_empty() {
        return Ok(());
    }

    let windows = trusted_windows(pool, validated, deferred).await?;
    for d in deferred {
        let out = &validated.outputs[d.output];
        let run = &out.file_runs[d.run];
        let claimed = &out.chunk_manifest[run.chunks.clone()];
        if let Some(window) = windows.get(&run.digest)
            && window_matches(window, claimed)
        {
            continue;
        }
        // No committed binding, or one with a DIFFERENT window — only
        // the bytes can arbitrate (the honest causes are a never-seen
        // digest or a chunking-parameter change; the forgery cause is
        // the poisoning attempt).
        refetch_and_verify(pool, backend, d.output, run, claimed).await?;
    }
    Ok(())
}

/// `digest → committed chunk window` for every distinct deferred
/// digest that has one: resolve each digest's canonical referrer row,
/// load each distinct referrer's chunk list once, slice the window.
/// Digests with no committed chunked binding, an inline-only referrer,
/// or an unsliceable window (corrupt referrer metadata) are simply
/// absent — the caller falls back to the byte proof.
async fn trusted_windows(
    pool: &PgPool,
    validated: &ValidatedBegin,
    deferred: &[DeferredRun],
) -> Result<HashMap<[u8; 32], Vec<ManifestEntry>>, Status> {
    let digests: Vec<Vec<u8>> = deferred
        .iter()
        .map(|d| validated.outputs[d.output].file_runs[d.run].digest.to_vec())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let rows = metadata::trusted_file_windows(pool, &digests)
        .await
        .map_err(|e| super::putpath_metadata_status("PutPathChunked: file windows", e))?;

    let referrers: Vec<Vec<u8>> = rows
        .iter()
        .map(|(_, p, _, _)| p.clone())
        .collect::<BTreeSet<_>>()
        .into_iter()
        .collect();
    let lists = metadata::chunk_lists_for_paths(pool, &referrers)
        .await
        .map_err(|e| super::putpath_metadata_status("PutPathChunked: chunk lists", e))?;
    let manifests: HashMap<Vec<u8>, IndexedManifest> = lists
        .into_iter()
        .filter_map(|(p, list)| Some((p, IndexedManifest::new(Manifest::deserialize(&list).ok()?))))
        .collect();

    let mut windows = HashMap::with_capacity(rows.len());
    for (digest, referrer, offset, size) in rows {
        let Ok(digest) = <[u8; 32]>::try_from(digest.as_slice()) else {
            continue;
        };
        let (Ok(offset), Ok(size)) = (u64::try_from(offset), u64::try_from(size)) else {
            continue;
        };
        if let Some(window) = manifests
            .get(&referrer)
            .and_then(|m| m.window(offset, size))
        {
            windows.insert(digest, window.to_vec());
        }
    }
    Ok(windows)
}

/// A deserialized chunk list plus the start-offset cumsum, so each
/// window slice is two binary searches instead of a linear scan (a
/// fully-deduped upload resolves one window per file).
struct IndexedManifest {
    manifest: Manifest,
    /// `starts[i]` = blob-stream offset where entry `i` begins;
    /// `starts[n]` = total stream length.
    starts: Vec<u64>,
}

impl IndexedManifest {
    fn new(manifest: Manifest) -> Self {
        let mut starts = Vec::with_capacity(manifest.entries.len() + 1);
        let mut acc = 0u64;
        for e in &manifest.entries {
            starts.push(acc);
            acc += u64::from(e.size);
        }
        starts.push(acc);
        Self { manifest, starts }
    }

    /// The exact entry range covering `[offset, offset + size)`, or
    /// `None` if either bound is not a chunk boundary (file boundaries
    /// are chunk boundaries for every honestly-committed binding).
    fn window(&self, offset: u64, size: u64) -> Option<&[ManifestEntry]> {
        let start = self.starts.binary_search(&offset).ok()?;
        let end = self
            .starts
            .binary_search(&(offset.checked_add(size)?))
            .ok()?;
        (start <= end && end <= self.manifest.entries.len())
            .then(|| &self.manifest.entries[start..end])
    }
}

/// Claimed run == committed window, entry for entry.
fn window_matches(window: &[ManifestEntry], claimed: &[([u8; 32], u32)]) -> bool {
    window.len() == claimed.len()
        && window
            .iter()
            .zip(claimed)
            .all(|(w, (d, s))| w.hash == *d && w.size == *s)
}

/// The byte proof: fetch the run's chunks back from the backend in
/// order (`REFETCH_PREFETCH_K`-wide, one run at a time — bounded
/// memory), re-verify each body against its chunk digest, and require
/// the whole-file BLAKE3 to equal the claimed digest.
async fn refetch_and_verify(
    pool: &PgPool,
    backend: &Arc<dyn ChunkBackend>,
    output: usize,
    run: &FileRun,
    claimed: &[([u8; 32], u32)],
) -> Result<(), Status> {
    let mut fetches = futures_util::stream::iter(claimed.iter().copied())
        .map(|(digest, len)| {
            let backend = Arc::clone(backend);
            async move { (digest, len, backend.get(&digest).await) }
        })
        .buffered(REFETCH_PREFETCH_K);
    let mut hasher = blake3::Hasher::new();
    while let Some((digest, len, fetched)) = fetches.next().await {
        let body = match fetched {
            Ok(Some(b)) => b,
            // The chunk was durable when `novel` was validated (or PUT
            // by this stream and confirmed); a miss now is the GC
            // race / S3 fault window — retryable, same shape as the
            // commit-time presence-proof abort. The presence row is
            // demoted so `HasChunks` stops claiming an object that is
            // provably gone — without this, every retry trusts the
            // same lie and skips the re-upload forever (the production
            // GC-grace-vs-ack-TTL hole).
            // r[impl store.chunk.has-chunks-durable]
            Ok(None) => {
                let digest_hex = hex::encode(digest);
                tracing::warn!(
                    chunk = %digest_hex,
                    "PutPathChunked: durable chunk has no backing object — clearing its \
                     presence so the next upload streams it again"
                );
                metrics::counter!("rio_store_putpath_verify_unavailable_total").increment(1);
                if let Err(e) = metadata::clear_chunk_presence(pool, &digest).await {
                    // Best effort: the reject below already forces the
                    // builder to retry; a failed demote only means the
                    // next attempt may hit the same miss once more.
                    tracing::warn!(chunk = %digest_hex, error = %e,
                        "PutPathChunked: failed to clear presence for missing chunk");
                }
                return Err(Status::unavailable(format!(
                    "PutPathChunked: {}",
                    rio_proto::chunk_reject::missing_chunk_digest_message(&digest_hex),
                )));
            }
            Err(e) => {
                metrics::counter!("rio_store_putpath_verify_unavailable_total").increment(1);
                return Err(Status::unavailable(format!(
                    "PutPathChunked: chunk fetch failed during file-digest \
                     verification: {e:#}",
                )));
            }
        };
        // Guard the recompute against backend corruption so a bad
        // object is reported as server-side data loss, not blamed on
        // the builder as a digest forgery. The backend returns the
        // STORED form (zstd-framed or legacy raw) —
        // `decode_stored_chunk` sniffs/decompresses and verifies the
        // BLAKE3 over the UNCOMPRESSED bytes; the manifest's size
        // claim is checked against the decoded length.
        let (ok, h) = cas::cpu_bound(move || {
            let ok = match cas::decode_stored_chunk(&digest, body) {
                Ok(plain) if plain.len() == len as usize => {
                    hasher.update(&plain);
                    true
                }
                _ => false,
            };
            (ok, hasher)
        });
        hasher = h;
        if !ok {
            return Err(Status::data_loss(format!(
                "PutPathChunked: backend chunk {} does not match its digest",
                hex::encode(digest),
            )));
        }
    }
    let actual: [u8; 32] = hasher.finalize().into();
    if actual != run.digest {
        return Err(file_digest_mismatch(output, &run.digest, &actual));
    }
    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;

    fn entries(spec: &[(u8, u32)]) -> Vec<ManifestEntry> {
        spec.iter()
            .map(|(tag, size)| ManifestEntry {
                hash: [*tag; 32],
                size: *size,
            })
            .collect()
    }

    /// Window slicing accepts exactly chunk-aligned `[offset, size)`
    /// ranges and rejects everything else.
    #[test]
    fn indexed_manifest_window_alignment() {
        let m = IndexedManifest::new(Manifest {
            entries: entries(&[(1, 100), (2, 50), (3, 200)]),
        });
        // Whole stream.
        assert_eq!(m.window(0, 350).unwrap().len(), 3);
        // Interior run.
        let w = m.window(100, 250).unwrap();
        assert_eq!(w.len(), 2);
        assert_eq!(w[0].hash, [2; 32]);
        // Empty run at a boundary.
        assert_eq!(m.window(150, 0).unwrap().len(), 0);
        // Misaligned start / end / overrun.
        assert!(m.window(1, 99).is_none());
        assert!(m.window(0, 120).is_none());
        assert!(m.window(100, 300).is_none());
        // Overflowing size.
        assert!(m.window(100, u64::MAX).is_none());
    }

    #[test]
    fn window_matches_compares_digest_and_size() {
        let w = entries(&[(1, 100), (2, 50)]);
        assert!(window_matches(&w, &[([1; 32], 100), ([2; 32], 50)]));
        // Digest differs.
        assert!(!window_matches(&w, &[([9; 32], 100), ([2; 32], 50)]));
        // Size differs.
        assert!(!window_matches(&w, &[([1; 32], 100), ([2; 32], 51)]));
        // Length differs.
        assert!(!window_matches(&w, &[([1; 32], 100)]));
    }
}
