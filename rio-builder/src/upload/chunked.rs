//! `PutPathChunked` client: walk every output once, probe the store
//! for already-durable chunks, and stream one `Begin` frame plus the
//! novel chunk bodies (ADR-022 §6, P0586).
//!
//! All of a derivation's outputs travel in ONE client-stream RPC and
//! commit atomically server-side (`r[store.atomic.multi-output]`).
//! Every per-output computation — NAR SHA-256, reference scan, castore
//! Directory DAG, FastCDC chunk manifest — comes out of the fused walk
//! ([`super::walk`]); only the chunks the store reports as missing are
//! read from disk a second time when they go on the wire.

use std::collections::HashSet;
use std::io::{Read, Seek, SeekFrom};
use std::path::{Path, PathBuf};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::refscan::CandidateSet;
use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::store::chunk_service_client::ChunkServiceClient;
use rio_proto::types::{
    ChunkData, ChunkMeta, ChunkedOutput, HasChunksRequest, PutPathChunkedBegin,
    PutPathChunkedRequest, put_path_chunked_request,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::store_fetch::StoreClients;

use super::UploadError;
use super::common::{
    MAX_UPLOAD_RETRIES, STREAM_CHANNEL_BUF, UPLOAD_BACKOFF, attach_assignment_token,
    await_dump_after_rx_drop, uploaded_info,
};
use super::walk::{WalkedOutput, walk_output};

/// Per-request digest cap for the `HasChunks` durable-presence probe.
/// The store rejects probe batches above its per-manifest chunk cap
/// (`MAX_CHUNKS` = 300 000); 16 384 stays well under that while keeping
/// the worst case (≈300 k chunks for a `MAX_NAR_SIZE` output at the
/// FastCDC minimum) at ~19 round trips.
const HAS_CHUNKS_PROBE_BATCH: usize = 16_384;

/// Per-stream gRPC budget × output count, capped at `MAX_BATCH_OUTPUTS`
/// so a malformed output list can't produce an unbounded deadline.
/// `GRPC_STREAM_TIMEOUT` is doc-sized for ONE 4 GiB NAR; the chunked
/// stream carries at most the novel subset of N outputs' content, so
/// scaling by N is a generous upper bound.
pub(super) fn chunked_stream_timeout(n_outputs: usize) -> Duration {
    rio_common::grpc::GRPC_STREAM_TIMEOUT
        * (n_outputs.min(rio_common::limits::MAX_BATCH_OUTPUTS) as u32).max(1)
}

/// One scanned output, walked and ready to be placed in a `Begin` frame.
pub(crate) struct WalkedTarget {
    /// Basename under `upper_store` (`"abc…-hello"`).
    pub basename: String,
    /// `"/nix/store/{basename}"` — the string sent in `ChunkedOutput`.
    pub store_path: String,
    /// Validated parse of `store_path`.
    pub parsed: StorePath,
    /// Everything the fused walk learned about this output.
    pub walked: WalkedOutput,
}

/// Where to re-read one novel chunk's bytes for its `ChunkData` frame.
/// Entries are in `Begin.novel` order — the wire order the store
/// verifies against.
pub(crate) struct ChunkFramePlan {
    pub digest: [u8; 32],
    /// Path of the file containing the chunk, relative to the upper
    /// store. Re-opened beneath a held upper-store dirfd
    /// ([`open_chunk_source`]) — never by absolute path through the
    /// attacker-written output tree.
    pub rel_path: PathBuf,
    pub offset: u64,
    pub size: u32,
}

/// Upload all scanned outputs via one `PutPathChunked` stream.
///
/// Walks every output first (prep failure on output *k* therefore
/// happens before any byte reaches the wire), then retries the
/// probe→assemble→stream sequence on transient store errors. A failed
/// `PutPathChunked` commits nothing server-side, so each retry restarts
/// from scratch: re-probe `HasChunks` (a chunk may have been GC-claimed
/// or become durable in between) and re-read the novel chunk bytes from
/// disk. The walk results are reused across attempts — the expensive
/// full-output disk read happens once.
// r[impl builder.upload.batch+3]
#[instrument(skip_all, fields(outputs = basenames.len()))]
pub(super) async fn upload_outputs_chunked(
    clients: &StoreClients,
    upper_store: &Path,
    basenames: &[String],
    assignment_token: &str,
    deriver: &str,
    input_closure: &[String],
) -> Result<Vec<ValidatedPathInfo>, UploadError> {
    // r[impl builder.upload.references-scanned+2]
    // Candidate set = echoed input_closure ∪ sibling output paths.
    // This scan is authoritative: the store commits the resolved set
    // as claimed (`r[store.integrity.verify-on-put+3]`); rationale at
    // the collect_outputs call site (executor/outputs.rs).
    let store_paths: Vec<String> = basenames
        .iter()
        .map(|b| format!("/nix/store/{b}"))
        .collect();
    let candidates = std::sync::Arc::new(CandidateSet::from_paths(
        input_closure.iter().chain(store_paths.iter()),
    ));

    // Walk every output BEFORE opening the stream. Serial: the walk is
    // a spawn_blocking disk read and outputs are few (≤ MAX_BATCH_OUTPUTS
    // in practice); parallel walks would just thrash the same disk.
    let mut targets = Vec::with_capacity(basenames.len());
    for (basename, store_path) in basenames.iter().zip(&store_paths) {
        let parsed = StorePath::parse(store_path).map_err(|e| UploadError::UploadRejected {
            path: store_path.clone(),
            source: tonic::Status::invalid_argument(format!(
                "output store path {store_path:?} from overlay upper is malformed: {e}"
            )),
        })?;
        let walked = walk_output(&upper_store.join(basename), &candidates)
            .await
            .map_err(|source| UploadError::UploadRejected {
                path: store_path.clone(),
                source,
            })?;
        metrics::histogram!("rio_builder_upload_references_count")
            .record(walked.references.len() as f64);
        targets.push(WalkedTarget {
            basename: basename.clone(),
            store_path: store_path.clone(),
            parsed,
            walked,
        });
    }

    let timeout = chunked_stream_timeout(targets.len());
    let probe_digests = first_occurrence_digests(&targets);
    tracing::info!(
        outputs = targets.len(),
        deriver = %deriver,
        "uploading build outputs (PutPathChunked)"
    );

    let mut last_err: Option<tonic::Status> = None;
    for attempt in 0..MAX_UPLOAD_RETRIES {
        if attempt > 0 {
            tokio::time::sleep(UPLOAD_BACKOFF.duration(attempt - 1)).await;
        }

        // Re-probe per attempt: an UNAVAILABLE commit abort means a
        // referenced chunk was GC-claimed mid-upload — the durable set
        // has changed and the stale `novel` would lie again.
        let durable = probe_durable_chunks(&clients.chunk, &probe_digests, assignment_token).await;
        let (begin, plan) = build_begin(&targets, deriver, input_closure, &durable);

        match send_chunked(
            &clients.store,
            begin,
            plan,
            upper_store,
            assignment_token,
            timeout,
        )
        .await
        {
            Ok((created, bytes_streamed)) => {
                metrics::counter!("rio_builder_upload_bytes_total").increment(bytes_streamed);
                let mut results = Vec::with_capacity(targets.len());
                for (i, t) in targets.iter().enumerate() {
                    if created.get(i).copied().unwrap_or(false) {
                        metrics::counter!("rio_builder_uploads_total", "status" => "success")
                            .increment(1);
                    } else {
                        // Server-side idempotency hit: the path was
                        // already complete (e.g. an earlier attempt of a
                        // re-dispatched derivation). Same skip semantics
                        // as the FindMissingPaths pre-check, detected at
                        // commit instead.
                        metrics::counter!("rio_builder_upload_skipped_idempotent_total")
                            .increment(1);
                    }
                    results.push(uploaded_info(
                        t.parsed.clone(),
                        t.walked.nar_hash,
                        t.walked.nar_size,
                        t.walked.references.clone(),
                        deriver,
                    )?);
                }
                tracing::info!(
                    outputs = results.len(),
                    bytes_streamed,
                    "chunked upload committed atomically"
                );
                return Ok(results);
            }
            Err(status) if is_retryable(status.code()) => {
                tracing::warn!(attempt, error = %status, "chunked upload attempt failed; retrying upload");
                // Concurrent-uploader patience: an Aborted placeholder
                // contention means another uploader holds (or recently
                // held) one of these paths. If their upload has finished
                // by now, every output is already complete — take the
                // idempotent-skip path instead of burning the remaining
                // attempts re-probing and re-streaming.
                if status.code() == tonic::Code::Aborted
                    && status.message().contains(rio_proto::CONCURRENT_PUTPATH_MSG)
                    && let Some(already_present) =
                        super::all_outputs_already_present(&clients.store, basenames).await
                {
                    tracing::info!(
                        outputs = already_present.len(),
                        "concurrent uploader finished these outputs; \
                         adopting store state instead of retrying"
                    );
                    return Ok(already_present);
                }
                last_err = Some(status);
            }
            Err(status) => {
                // Deterministic rejection (contract violation, auth):
                // retrying re-sends the identical request — fail now.
                tracing::error!(error = %status, "chunked upload rejected; not retrying");
                last_err = Some(status);
                break;
            }
        }
    }

    metrics::counter!("rio_builder_uploads_total", "status" => "exhausted")
        .increment(targets.len() as u64);
    let source = last_err.expect("the attempt loop runs at least once and records its error");
    let path = "<chunked>".to_string();
    // The loop only breaks early on a non-retryable status, so a
    // retryable last error means the whole retry budget was spent;
    // anything else was rejected on its only attempt and must not be
    // reported as retry exhaustion.
    Err(if is_retryable(source.code()) {
        UploadError::UploadExhausted { path, source }
    } else {
        UploadError::UploadRejected { path, source }
    })
}

/// Transient store errors are worth a retry (the upload restarts from
/// scratch — nothing was committed). Deterministic rejections
/// (`InvalidArgument`, `FailedPrecondition`, `PermissionDenied`, …)
/// would fail identically on every attempt.
fn is_retryable(code: tonic::Code) -> bool {
    rio_common::grpc::is_transient(code)
        || matches!(code, tonic::Code::DeadlineExceeded | tonic::Code::Internal)
}

/// Every distinct chunk digest in global first-occurrence order:
/// outputs in `Begin` order, manifest entries in canonical walk order.
/// This is both the probe list and the order `Begin.novel` must follow.
fn first_occurrence_digests(targets: &[WalkedTarget]) -> Vec<[u8; 32]> {
    let mut seen = HashSet::new();
    let mut ordered = Vec::new();
    for t in targets {
        for c in &t.walked.chunk_manifest {
            if seen.insert(c.digest) {
                ordered.push(c.digest);
            }
        }
    }
    ordered
}

/// Probe `ChunkService.HasChunks` for the durable subset of `digests`.
///
/// Best-effort: any error treats the remaining digests as novel — the
/// store accepts already-durable chunks in `novel` (membership is
/// trusted, durability is not), so the only cost is re-streamed bytes.
/// The probe requires a caller identity; in dev mode (empty assignment
/// token, no JWT) the store rejects it and every chunk streams as
/// novel, which is exactly the pre-dedup behavior.
async fn probe_durable_chunks(
    chunk_client: &ChunkServiceClient<Channel>,
    digests: &[[u8; 32]],
    assignment_token: &str,
) -> HashSet<[u8; 32]> {
    let mut durable = HashSet::new();
    let mut client = chunk_client.clone();
    for batch in digests.chunks(HAS_CHUNKS_PROBE_BATCH) {
        let mut req = tonic::Request::new(HasChunksRequest {
            digests: batch.iter().map(|d| d.to_vec()).collect(),
        });
        if let Err(e) = attach_assignment_token(&mut req, assignment_token) {
            tracing::warn!(error = %e, "HasChunks probe skipped (token unusable); treating all chunks as novel");
            return durable;
        }
        match rio_common::grpc::with_timeout_status(
            "HasChunks",
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            client.has_chunks(req),
        )
        .await
        {
            Ok(resp) => {
                let bitmap = resp.into_inner().bitmap;
                for (i, d) in batch.iter().enumerate() {
                    // LSB-first within each byte (the HasBitmap contract).
                    if bitmap
                        .get(i / 8)
                        .is_some_and(|byte| byte & (1 << (i % 8)) != 0)
                    {
                        durable.insert(*d);
                    }
                }
            }
            Err(e) => {
                tracing::warn!(
                    error = %e,
                    probed = durable.len(),
                    "HasChunks probe failed; treating remaining chunks as novel"
                );
                return durable;
            }
        }
    }
    durable
}

/// Assemble the `Begin` frame and the ordered chunk-frame plan from the
/// walked outputs. Pure — no I/O — so the walk↔validate agreement test
/// can drive the real store handler with exactly what production sends.
// r[impl builder.upload.deriver-populated]
pub(crate) fn build_begin(
    targets: &[WalkedTarget],
    deriver: &str,
    input_closure: &[String],
    durable: &HashSet<[u8; 32]>,
) -> (PutPathChunkedBegin, Vec<ChunkFramePlan>) {
    // r[impl builder.upload.chunked-manifest]
    let outputs: Vec<ChunkedOutput> = targets
        .iter()
        .map(|t| ChunkedOutput {
            store_path: t.store_path.clone(),
            nar_hash: t.walked.nar_hash.to_vec(),
            nar_size: t.walked.nar_size,
            references: t.walked.references.clone(),
            root_node: Some(t.walked.root_node.clone()),
            chunk_manifest: t
                .walked
                .chunk_manifest
                .iter()
                .map(|c| ChunkMeta {
                    digest: c.digest.to_vec(),
                    size: u64::from(c.size),
                })
                .collect(),
        })
        .collect();

    // Directory bodies deduplicated across outputs, digest-sorted for a
    // deterministic wire encoding (the store keys them by recomputed
    // digest; order carries no meaning).
    let mut directories: Vec<([u8; 32], rio_proto::castore::Directory)> = Vec::new();
    let mut dir_seen = HashSet::new();
    for t in targets {
        for (digest, body) in &t.walked.directories {
            if dir_seen.insert(*digest) {
                directories.push((*digest, body.clone()));
            }
        }
    }
    directories.sort_by_key(|(digest, _)| *digest);

    // `novel` = HasChunks-false digests in global first-occurrence
    // order; the chunk-frame plan mirrors it one-to-one.
    let mut seen = HashSet::new();
    let mut novel = Vec::new();
    let mut plan = Vec::new();
    for t in targets {
        for c in &t.walked.chunk_manifest {
            if seen.insert(c.digest) && !durable.contains(&c.digest) {
                let src =
                    t.walked.chunk_sources.get(&c.digest).expect(
                        "walk records a source for every manifest digest of its own output",
                    );
                novel.push(c.digest.to_vec());
                plan.push(ChunkFramePlan {
                    digest: c.digest,
                    rel_path: src.upper_rel_path(&t.basename),
                    offset: src.offset,
                    size: src.size,
                });
            }
        }
    }

    (
        PutPathChunkedBegin {
            deriver: deriver.to_string(),
            outputs,
            directories: directories.into_iter().map(|(_, d)| d).collect(),
            novel,
            input_closure: input_closure.to_vec(),
        },
        plan,
    )
}

/// Re-open one walked file for chunk re-reads, beneath the held
/// upper-store dirfd.
///
/// The fused walk read these bytes through fd-relative, symlink-free
/// resolution; this re-open is the only second resolution of the
/// attacker-written output tree (the bytes read here go on the wire).
/// `openat2(RESOLVE_BENEATH | RESOLVE_NO_SYMLINKS)` extends the walk's
/// guarantee to it: a symlink swapped in anywhere under the upper store
/// between walk and upload fails the open instead of being followed to
/// an arbitrary host file, and `..` cannot escape the upper store.
/// `O_NONBLOCK` plus the fstat regular-file check turn a FIFO swap into
/// an immediate error instead of an indefinitely parked upload task.
/// The BLAKE3 re-verify downstream still guards WHAT the bytes are;
/// this guards WHERE they come from.
fn open_chunk_source(
    upper: std::os::fd::BorrowedFd<'_>,
    rel: &Path,
) -> std::io::Result<std::fs::File> {
    use nix::fcntl::{OFlag, OpenHow, ResolveFlag, openat2};
    let how = OpenHow::new()
        .flags(OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
        .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS);
    let fd = openat2(upper, rel, how).map_err(std::io::Error::from)?;
    let f = std::fs::File::from(fd);
    let meta = f.metadata()?;
    if !meta.is_file() {
        return Err(std::io::Error::other(format!(
            "not a regular file ({:?})",
            meta.file_type()
        )));
    }
    Ok(f)
}

/// Drive one `PutPathChunked` stream: the `Begin` frame, then one
/// `ChunkData` per plan entry in order, bytes re-read from disk on the
/// blocking pool (re-opened beneath `upper_store` — see
/// [`open_chunk_source`]). Returns the per-output `created` flags and
/// the number of chunk bytes streamed.
pub(super) async fn send_chunked(
    store_client: &StoreServiceClient<Channel>,
    begin: PutPathChunkedBegin,
    plan: Vec<ChunkFramePlan>,
    upper_store: &Path,
    assignment_token: &str,
    timeout: Duration,
) -> Result<(Vec<bool>, u64), tonic::Status> {
    let (tx, rx) = mpsc::channel::<PutPathChunkedRequest>(STREAM_CHANNEL_BUF);

    // Begin goes out from the async side BEFORE the blocking reader is
    // spawned, so frame order is guaranteed.
    tx.send(PutPathChunkedRequest {
        msg: Some(put_path_chunked_request::Msg::Begin(begin)),
    })
    .await
    .map_err(|_| tonic::Status::internal("upload channel closed before Begin send"))?;

    // Blocking reader: re-read each novel chunk's bytes and forward
    // them. `blocking_send` gives backpressure against the gRPC send —
    // peak memory stays at STREAM_CHANNEL_BUF × FASTCDC_MAX_BYTES.
    let upper_store = upper_store.to_path_buf();
    let reader = tokio::task::spawn_blocking(move || -> Result<u64, tonic::Status> {
        use std::os::fd::AsFd;
        // The upper store dir itself is builder-owned (overlay upper),
        // not build-writable — resolving it normally, once, is safe.
        // Everything under it goes through open_chunk_source.
        let upper = nix::fcntl::open(
            &upper_store,
            nix::fcntl::OFlag::O_RDONLY
                | nix::fcntl::OFlag::O_DIRECTORY
                | nix::fcntl::OFlag::O_CLOEXEC,
            nix::sys::stat::Mode::empty(),
        )
        .map_err(|e| {
            let msg = format!("failed to open upper store {}: {e}", upper_store.display());
            tracing::error!("{msg}");
            tonic::Status::internal(msg)
        })?;
        let mut total: u64 = 0;
        // Chunks of one file are consecutive in novel order, so caching
        // the last opened file avoids re-opening per 64 KiB chunk.
        let mut open: Option<(PathBuf, std::fs::File)> = None;
        for entry in plan {
            if open.as_ref().is_none_or(|(p, _)| *p != entry.rel_path) {
                let f = open_chunk_source(upper.as_fd(), &entry.rel_path).map_err(|e| {
                    let msg = format!(
                        "chunk re-read failed for {}: {e}",
                        upper_store.join(&entry.rel_path).display()
                    );
                    // Logged here as well as returned: when the server
                    // rejects the truncated stream, the gRPC status wins
                    // the error-priority race below and this worker-side
                    // cause would otherwise be lost.
                    tracing::error!("{msg}");
                    tonic::Status::internal(msg)
                })?;
                open = Some((entry.rel_path.clone(), f));
            }
            let (_, file) = open.as_mut().expect("opened above");
            let mut buf = vec![0u8; entry.size as usize];
            file.seek(SeekFrom::Start(entry.offset))
                .and_then(|_| file.read_exact(&mut buf))
                .map_err(|e| {
                    let msg = format!(
                        "chunk re-read failed for {} at offset {}: {e}",
                        upper_store.join(&entry.rel_path).display(),
                        entry.offset
                    );
                    tracing::error!("{msg}");
                    tonic::Status::internal(msg)
                })?;
            // The walk hashed these exact bytes; a mismatch means the
            // output changed (or the disk corrupted) between walk and
            // upload. Failing here gives a worker-side diagnostic
            // instead of an opaque server-side INVALID_ARGUMENT.
            if *blake3::hash(&buf).as_bytes() != entry.digest {
                let msg = format!(
                    "chunk at {} offset {} no longer matches its walked digest",
                    upper_store.join(&entry.rel_path).display(),
                    entry.offset
                );
                tracing::error!("{msg}");
                return Err(tonic::Status::internal(msg));
            }
            total += u64::from(entry.size);
            tx.blocking_send(PutPathChunkedRequest {
                msg: Some(put_path_chunked_request::Msg::Chunk(ChunkData {
                    digest: entry.digest.to_vec(),
                    data: buf.into(),
                })),
            })
            // Receiver dropped = the gRPC call already failed; its
            // status (surfaced below) is the interesting error.
            .map_err(|_| tonic::Status::internal("upload stream closed mid-chunk"))?;
        }
        Ok(total)
    });

    let outbound = tokio_stream::wrappers::ReceiverStream::new(rx);
    let mut req = tonic::Request::new(outbound);
    attach_assignment_token(&mut req, assignment_token)?;
    let mut client = store_client.clone();
    let put_result = rio_common::grpc::with_timeout_status(
        "PutPathChunked",
        timeout,
        client.put_path_chunked(req),
    )
    .await;

    // Bound the reader join: a blocking thread parked in a wedged disk
    // read never observes rx-drop and spawn_blocking is non-abortable.
    // The gRPC budget already elapsed concurrently above, so only the
    // join slack applies.
    let read_result = await_dump_after_rx_drop("chunk reader", reader).await?;

    // Error priority: the gRPC status is the actionable one ("store
    // unreachable" beats "channel closed mid-chunk").
    let created = put_result?.into_inner().created;
    let bytes = read_result?;
    Ok((created, bytes))
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_common::grpc::GRPC_STREAM_TIMEOUT;
    use rio_common::limits::MAX_BATCH_OUTPUTS;

    fn upper_fixture() -> (tempfile::TempDir, std::fs::File, PathBuf) {
        let tmp = tempfile::tempdir().expect("tempdir");
        let upper = tmp.path().join("upper");
        std::fs::create_dir_all(upper.join("abc-out")).expect("mkdir");
        std::fs::write(upper.join("abc-out/data"), b"walked chunk bytes").expect("write");
        let dirfd = std::fs::File::open(&upper).expect("open upper");
        (tmp, dirfd, upper)
    }

    /// Baseline: the hardened re-open reads exactly the walked file.
    #[test]
    fn chunk_reopen_reads_walked_file() {
        use std::io::Read;
        use std::os::fd::AsFd;
        let (_tmp, dirfd, _upper) = upper_fixture();
        let mut f =
            open_chunk_source(dirfd.as_fd(), Path::new("abc-out/data")).expect("open succeeds");
        let mut buf = Vec::new();
        f.read_to_end(&mut buf).expect("read");
        assert_eq!(buf, b"walked chunk bytes");
    }

    /// A symlink swapped in under the output root between walk and
    /// chunk re-read must be rejected at open time — even when its
    /// target has IDENTICAL content (the BLAKE3 re-verify downstream
    /// cannot tell). Red-proven on the path-based re-open
    /// (`File::open(abs_path)`): the open followed the link and the
    /// upload proceeded through attacker-controlled resolution.
    #[test]
    fn chunk_reopen_rejects_symlink_swap() {
        use std::os::fd::AsFd;
        let (tmp, dirfd, upper) = upper_fixture();
        // Host file OUTSIDE the upper store with the same content the
        // walk hashed.
        let outside = tmp.path().join("host-file");
        std::fs::write(&outside, b"walked chunk bytes").expect("write");
        std::fs::remove_file(upper.join("abc-out/data")).expect("rm");
        std::os::unix::fs::symlink(&outside, upper.join("abc-out/data")).expect("symlink");

        let res = open_chunk_source(dirfd.as_fd(), Path::new("abc-out/data"));
        assert!(
            res.is_err(),
            "chunk re-open must reject a swapped-in symlink, got {res:?}"
        );
    }

    /// A FIFO swapped in must fail immediately (`O_NONBLOCK` + fstat),
    /// not park the upload task in a blocking open until the gRPC
    /// timeout fires.
    #[test]
    fn chunk_reopen_rejects_fifo_swap() {
        use std::os::fd::AsFd;
        let (_tmp, dirfd, upper) = upper_fixture();
        std::fs::remove_file(upper.join("abc-out/data")).expect("rm");
        nix::unistd::mkfifo(
            &upper.join("abc-out/data"),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )
        .expect("mkfifo");

        let err = open_chunk_source(dirfd.as_fd(), Path::new("abc-out/data"))
            .expect_err("chunk re-open must reject a swapped-in FIFO");
        assert!(
            err.to_string().contains("not a regular file"),
            "error should come from the fstat file-type check: {err}"
        );
    }

    /// `..` in a plan rel_path cannot escape the upper store
    /// (`RESOLVE_BENEATH` is a kernel guarantee, not name validation).
    #[test]
    fn chunk_reopen_rejects_parent_escape() {
        use std::os::fd::AsFd;
        let (tmp, dirfd, _upper) = upper_fixture();
        std::fs::write(tmp.path().join("escape"), b"outside bytes").expect("write");
        let res = open_chunk_source(dirfd.as_fd(), Path::new("../escape"));
        assert!(
            res.is_err(),
            "chunk re-open must refuse a `..` escape, got {res:?}"
        );
    }

    #[test]
    fn chunked_stream_timeout_scales_with_outputs() {
        assert_eq!(chunked_stream_timeout(1), GRPC_STREAM_TIMEOUT);
        assert_eq!(chunked_stream_timeout(3), GRPC_STREAM_TIMEOUT * 3);
        assert_eq!(
            chunked_stream_timeout(999),
            GRPC_STREAM_TIMEOUT * MAX_BATCH_OUTPUTS as u32
        );
        // Degenerate: empty slice still gets a non-zero budget.
        assert_eq!(chunked_stream_timeout(0), GRPC_STREAM_TIMEOUT);
    }
}
