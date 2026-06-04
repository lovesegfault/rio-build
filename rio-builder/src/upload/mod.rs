//! Output upload to rio-store after build completion.
//!
//! Takes the outputs the native result pipeline recorded (paths,
//! reference sets, content addresses), serializes each as a NAR from the
//! overlay upper layer, computes SHA-256, and uploads via
//! `StoreService.PutPath` gRPC with retry on failure.
//!
//! Submodules:
//! - `common`: streaming-tee sink, trailer-mode `PathInfo`,
//!   assignment-token header — mechanics shared by single + batch.
//! - `single`: per-output `PutPath` with retry + I-125b concurrent-put
//!   adoption.
//! - `batch`: atomic multi-output `PutPathBatch`.
// r[impl builder.upload.multi-output]

use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use futures_util::stream::{self, StreamExt};
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::store_path::StorePath;
use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;
use rio_proto::validated::ValidatedPathInfo;

mod batch;
pub(crate) mod common;
mod single;

use batch::upload_outputs_batch;
use common::{MAX_PARALLEL_UPLOADS, MAX_UPLOAD_RETRIES, PreparedOutput, UPLOAD_BACKOFF};
use single::upload_output;

// Re-exports for the test module's `use super::*` (private mechanics
// exercised directly by unit tests).
#[cfg(test)]
use {
    common::HashingChannelWriter,
    rio_nix::nar,
    rio_proto::types::put_path_request,
    sha2::{Digest, Sha256},
    single::{CONCURRENT_PUT_POLL_ATTEMPTS, is_concurrent_put_path},
    std::io::Write,
    tokio::sync::mpsc,
};

/// Errors from upload operations.
///
/// NAR serialization happens inside the retry loop (each retry is a
/// fresh disk read), so NAR errors surface as `UploadExhausted` wrapping
/// a `tonic::Status::internal("NAR serialization failed...")`.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    #[error("upload failed after {MAX_UPLOAD_RETRIES} retries for {path}: {source}")]
    UploadExhausted { path: String, source: tonic::Status },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// A reference recorded by the result pipeline fails
    /// `StorePath::parse`. The pipeline resolves references against
    /// validated input-closure and output paths, so this indicates a bug
    /// upstream (or a mismatched store-prefix). Surfaced as an error —
    /// silently dropping the ref would publish a path with a broken
    /// reference graph.
    #[error("unparseable store path in recorded references: {path:?}")]
    InvalidReference { path: String },
}

/// Scan the overlay upper layer for new store paths.
///
/// Returns basenames of paths under `/nix/store/` in the upper layer
/// that represent build outputs.
///
/// `upper_store` is `{overlay_upper}/nix/store` — callers pass
/// `OverlayMount::upper_store()`.
pub fn scan_new_outputs(upper_store: &Path) -> std::io::Result<Vec<String>> {
    let read_dir = match std::fs::read_dir(upper_store) {
        Ok(iter) => iter,
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => return Ok(Vec::new()),
        Err(e) => return Err(e),
    };

    let mut outputs = Vec::new();
    for entry in read_dir {
        let entry = entry?;
        // Store paths are UTF-8 (nix enforces this). A non-UTF-8 name
        // here is a violation — surface as InvalidData rather than
        // lossy-decode and push a wrong path.
        let name = entry.file_name().into_string().map_err(|_| {
            std::io::Error::new(
                std::io::ErrorKind::InvalidData,
                "non-UTF-8 filename in upper store",
            )
        })?;
        // Skip hidden files and the .links directory.
        if name.starts_with('.') {
            continue;
        }
        // Skip overlayfs whiteouts: a build that `rm`s a lower-layer
        // store path (read-only input) leaves a 0/0 chardev in the
        // upper. NAR-dumping a chardev would fail with EACCES/ENXIO and
        // poison the whole upload; the whiteout is not an output.
        if entry.file_type()?.is_char_device() {
            tracing::debug!(name, "skipping overlay whiteout in upper store");
            continue;
        }
        outputs.push(name);
    }

    // read_dir order is filesystem-dependent; sort for deterministic behavior.
    outputs.sort();
    Ok(outputs)
}

/// One build output to upload, exactly as the native result pipeline
/// recorded it (`ProcessedOutputs`): the realized store path plus the
/// reference set and content-address descriptor the pipeline computed.
///
/// The upload registers EXACTLY this. It does not rediscover outputs by
/// scanning the overlay upper layer (a stray store path a build leaves
/// behind is never uploaded) and it does not re-scan references (the
/// pipeline's post-`unsafeDiscardReferences`, post-CA-remapping set is
/// what gets registered — diverging from it here would re-introduce the
/// references CppNix drops).
#[derive(Debug, Clone)]
pub struct OutputToUpload {
    /// Full `/nix/store/...` path (final, post-CA-finalization).
    pub store_path: String,
    /// Where the output's bytes live on the worker host (the overlay
    /// upper layer entry; for finalized floating-CA outputs, the
    /// renamed-in-place final basename — see `native_result::ca`). The
    /// NAR dump streams from here.
    pub host_path: std::path::PathBuf,
    /// Sorted full-store-path references to register, post
    /// `unsafeDiscardReferences`. A self-referencing output lists its
    /// own final path here.
    pub references: Vec<String>,
    /// `fixed:[r:]<algo>:<hash>` descriptor for content-addressed
    /// outputs — floating-CA finalization or a fixed-output derivation's
    /// declared hash; `None` for input-addressed outputs.
    pub content_address: Option<String>,
}

/// Upload the given build outputs from the overlay upper layer.
///
/// Pipeline:
/// 1. **Idempotency pre-check** (`r[builder.upload.idempotent-precheck]`):
///    `FindMissingPaths` filters out outputs the store already has. Skipped
///    outputs get their `ValidatedPathInfo` from `QueryPathInfo` — zero disk
///    reads.
/// 2. **≥2 remaining → `PutPathBatch`** for cross-output atomicity
///    (`r[store.atomic.multi-output]`). On `FailedPrecondition` (an output
///    ≥ INLINE_THRESHOLD, which the v1 batch handler rejects), falls through
///    to step 3 — which LOSES atomicity (pre-P0267 status quo).
/// 3. **≤1 remaining, or batch fallthrough → independent `PutPath`** with
///    `buffer_unordered(MAX_PARALLEL_UPLOADS)`.
///
/// Result order is **not** guaranteed. Callers must not assume results
/// correspond positionally to any input list; use `.store_path` to
/// identify outputs.
#[instrument(skip_all)]
pub async fn upload_all_outputs(
    store_client: &StoreServiceClient<Channel>,
    assignment_token: &str,
    deriver: &str,
    outputs: &[OutputToUpload],
) -> Result<Vec<ValidatedPathInfo>, UploadError> {
    if outputs.is_empty() {
        return Ok(Vec::new());
    }

    // --- Idempotency pre-check -------------------------------------------
    // r[impl builder.upload.idempotent-precheck]
    //
    // Batch-check all outputs against the store BEFORE reading any bytes
    // from disk. Outputs with a `'complete'` manifest are skipped: the
    // disk read, NAR stream, SHA-256, and gRPC stream setup are all
    // wasted work when r[store.put.idempotent] would no-op server-side
    // anyway.
    //
    // Best-effort: on FindMissingPaths error, log + treat ALL as missing.
    // r[store.put.idempotent] catches the duplicates server-side — zero
    // behavior change from before this pre-check existed. This is an
    // optimization, not a correctness requirement.
    //
    // TODO(P0434): manifest-mode bandwidth opt — send manifest-only,
    // store fetches missing chunks from ChunkCache. Gated on measuring
    // rio_store_chunk_cache_hits_total ratio in production. Worker NOT
    // trusted → store must reconstruct NAR to verify, so the "win" is
    // net positive only if ChunkCache hit rate is high (>80%). P0263
    // scoped down to the zero-proto-change path; deferred remainder.
    let basename_of = |store_path: &str| -> String {
        store_path
            .strip_prefix("/nix/store/")
            .unwrap_or(store_path)
            .to_string()
    };
    let basenames: Vec<String> = outputs.iter().map(|o| basename_of(&o.store_path)).collect();
    let store_paths: Vec<String> = outputs.iter().map(|o| o.store_path.clone()).collect();
    let (to_upload, mut skipped_results) =
        partition_by_presence(store_client, &basenames, store_paths).await;
    // ---------------------------------------------------------------------

    // Prep ALL outputs (path parse + the pipeline-recorded reference
    // sets) BEFORE any byte hits the wire. A prep failure on output k
    // returns Err here; the batch producer is prep-free, so partial
    // server commit (`r[store.atomic.multi-output]`) is unrepresentable.
    //
    // References are NOT re-scanned at upload time: the result pipeline
    // already dumped and scanned every output, applied
    // `unsafeDiscardReferences`, and remapped floating-CA self/sibling
    // references to their final paths — that recorded set is what gets
    // registered, exactly as Nix registers it.
    // r[impl builder.upload.references-scanned+2]
    let by_basename: std::collections::HashMap<String, &OutputToUpload> = outputs
        .iter()
        .map(|o| (basename_of(&o.store_path), o))
        .collect();
    let mut prepared = Vec::with_capacity(to_upload.len());
    for b in &to_upload {
        let o = by_basename
            .get(b.as_str())
            .expect("to_upload is a subset of the outputs passed in");
        let parsed = StorePath::parse(&o.store_path).map_err(|e| UploadError::UploadExhausted {
            path: o.store_path.clone(),
            source: tonic::Status::invalid_argument(format!(
                "output store path {:?} is malformed: {e}",
                o.store_path
            )),
        })?;
        // The pipeline resolved these against the input closure and the
        // sibling outputs; re-check the parse so a bug upstream cannot
        // publish a path with a broken reference graph.
        for r in &o.references {
            let _ = StorePath::parse(r)
                .map_err(|_| UploadError::InvalidReference { path: r.clone() })?;
        }
        metrics::histogram!("rio_builder_upload_references_count")
            .record(o.references.len() as f64);
        prepared.push(PreparedOutput {
            store_path: o.store_path.clone(),
            host_path: o.host_path.clone(),
            parsed,
            references: o.references.clone(),
            content_address: o.content_address.clone(),
        });
    }

    // Branch: ≥2 outputs TO UPLOAD → atomic batch; ≤1 → independent
    // (atomicity is vacuous for a single output). The count is against the
    // post-idempotency-pre-check set — if 3 outputs exist but 2 are already
    // in the store, only 1 needs upload and PutPath is sufficient. See
    // store.atomic.multi-output in the store spec.
    // Capacity pre-check (round-17 bug_111): a prepared set wider than the
    // batch capacity cannot succeed server-side (the server answers
    // FailedPrecondition per output_index >= MAX_BATCH_OUTPUTS), so take
    // the documented independent-PutPath fallback BEFORE streaming any
    // NAR, not after N streamed NARs x retries. With MAX_BATCH_OUTPUTS
    // protocol-aligned (= MAX_OUTPUT_NAMES, compile-asserted in batch.rs)
    // this is unreachable from a parsed derivation; it guards internal
    // callers handing a hand-built slice.
    let batch_capacity_ok = prepared.len() <= rio_common::limits::MAX_BATCH_OUTPUTS;
    if !batch_capacity_ok {
        tracing::warn!(
            to_upload = prepared.len(),
            max = rio_common::limits::MAX_BATCH_OUTPUTS,
            "output set exceeds batch capacity; using independent PutPath \
             — partial registration possible"
        );
    }
    if prepared.len() >= 2 && batch_capacity_ok {
        tracing::info!(
            to_upload = prepared.len(),
            skipped = skipped_results.len(),
            "uploading build outputs (atomic batch)"
        );
        // Retry mirrors single.rs's MAX_UPLOAD_RETRIES/UPLOAD_BACKOFF —
        // multi-output derivations hit the same store-side placeholder
        // contention (I-068/I-125b) that motivated the 8-retry budget.
        // Prep is hoisted, so each retry is a fresh gRPC stream + N disk
        // dumps (no re-scan). `Aborted: concurrent` poll-then-adopt is
        // NOT ported: per-path adoption doesn't compose across N paths;
        // I-125a drop-cleanup releases the placeholder before the next
        // attempt's backoff expires.
        let mut last_err = None;
        'batch: for attempt in 0..MAX_UPLOAD_RETRIES {
            if attempt > 0 {
                tokio::time::sleep(UPLOAD_BACKOFF.duration(attempt - 1)).await;
            }
            match upload_outputs_batch(store_client, &prepared, assignment_token, deriver).await {
                Ok(mut results) => {
                    results.append(&mut skipped_results);
                    return Ok(results);
                }
                Err(UploadError::UploadExhausted { source, path })
                    if source.code() == tonic::Code::InvalidArgument =>
                {
                    // R1(e) (round-17 bug_111 rider): a request the server
                    // DETERMINISTICALLY rejects cannot become acceptable by
                    // resending the same bytes — classify permanent at the
                    // producing statement instead of burning
                    // MAX_UPLOAD_RETRIES x N re-streamed NARs. Capacity
                    // overruns are NOT this arm (the server signals those
                    // as FailedPrecondition, below); this is malformed
                    // requests only.
                    return Err(UploadError::UploadExhausted { source, path });
                }
                Err(UploadError::UploadExhausted { source, .. })
                    if source.code() == tonic::Code::FailedPrecondition =>
                {
                    // Defensive fallthrough: the store may signal "use
                    // independent PutPath" via FailedPrecondition. Loses
                    // atomicity (pre-P0267 behavior, which
                    // `gt13_multi_output_not_atomic` documents).
                    tracing::warn!(
                        error = %source,
                        "batch upload rejected with FailedPrecondition; \
                         falling back to independent PutPath — partial registration possible"
                    );
                    last_err = None;
                    break 'batch;
                }
                Err(e) => {
                    tracing::warn!(attempt, error = %e, "batch upload attempt failed");
                    last_err = Some(e);
                }
            }
        }
        if let Some(e) = last_err {
            metrics::counter!("rio_builder_uploads_total", "status" => "exhausted")
                .increment(prepared.len() as u64);
            return Err(e);
        }
        // FailedPrecondition fallthrough → per-output PutPath below.
    }

    tracing::info!(
        to_upload = prepared.len(),
        skipped = skipped_results.len(),
        max_parallel = MAX_PARALLEL_UPLOADS,
        "uploading build outputs (independent PutPath)"
    );

    // Clone the token into each task (it's a String in each async
    // block; MAX_PARALLEL_UPLOADS=4 copies of ~150 bytes — trivial).
    let token = assignment_token.to_string();
    let deriver = deriver.to_string();
    let results: Vec<Result<ValidatedPathInfo, UploadError>> = stream::iter(prepared)
        .map(|p| {
            let mut client = store_client.clone();
            let token = token.clone();
            let deriver = deriver.clone();
            async move { upload_output(&mut client, p, &token, &deriver).await }
        })
        .buffer_unordered(MAX_PARALLEL_UPLOADS)
        .collect()
        .await;

    let mut uploaded: Vec<ValidatedPathInfo> = results.into_iter().collect::<Result<_, _>>()?;
    uploaded.append(&mut skipped_results);
    Ok(uploaded)
}

/// Batch `FindMissingPaths` → partition outputs into (upload, skip).
///
/// For already-present outputs, `QueryPathInfo` fetches the full
/// `ValidatedPathInfo` from the store so callers get a complete result
/// without any disk read. The store already has the authoritative
/// reference set (from whoever originally uploaded); no caller reads
/// `.references` on the returned info (it's only sent TO the store via
/// PutPath).
///
/// Fail-open: any error (FindMissingPaths unavailable, QueryPathInfo returned
/// None for a supposedly-present path) → fall back to uploading. The store's
/// idempotent PutPath handles it. No error propagation — this whole function
/// is an optimization layer.
async fn partition_by_presence(
    store_client: &StoreServiceClient<Channel>,
    basenames: &[String],
    store_paths: Vec<String>,
) -> (Vec<String>, Vec<ValidatedPathInfo>) {
    let mut client = store_client.clone();
    let mut req = tonic::Request::new(FindMissingPathsRequest { store_paths });
    rio_proto::interceptor::inject_current(req.metadata_mut());

    let missing: std::collections::HashSet<String> = match rio_common::grpc::with_timeout_status(
        "FindMissingPaths",
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        client.find_missing_paths(req),
    )
    .await
    {
        Ok(resp) => resp.into_inner().missing_paths.into_iter().collect(),
        Err(e) => {
            tracing::warn!(
                error = %e,
                "idempotent pre-check: FindMissingPaths failed; \
                 falling back to upload-all (store.put.idempotent catches dups)"
            );
            return (basenames.to_vec(), Vec::new());
        }
    };

    let mut to_upload = Vec::with_capacity(basenames.len());
    let mut skipped = Vec::new();
    for basename in basenames {
        let store_path = format!("/nix/store/{basename}");
        if missing.contains(&store_path) {
            to_upload.push(basename.clone());
            continue;
        }
        // Present in store — fetch nar_hash/nar_size instead of re-uploading.
        // QueryPathInfo is cheap (~1 PG row read); re-upload is 2× disk
        // reads + NAR stream + gRPC stream. Worth it even for small outputs.
        match rio_proto::client::query_path_info_opt(
            &mut client,
            &store_path,
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            &[],
        )
        .await
        {
            Ok(Some(info)) => {
                metrics::counter!("rio_builder_upload_skipped_idempotent_total").increment(1);
                tracing::info!(
                    store_path = %store_path,
                    nar_size = info.nar_size,
                    "output already in store; skipping upload"
                );
                skipped.push(info);
            }
            // Present per FindMissingPaths but QueryPathInfo disagrees
            // (TOCTOU — sub-second window, effectively impossible; or
            // store transient). Fall back to upload; don't error.
            Ok(None) | Err(_) => {
                tracing::warn!(
                    store_path = %store_path,
                    "idempotent pre-check: FindMissingPaths said present but \
                     QueryPathInfo disagreed; falling back to upload"
                );
                to_upload.push(basename.clone());
            }
        }
    }
    (to_upload, skipped)
}

// r[verify builder.upload.multi-output]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;

    #[test]
    fn test_scan_new_outputs_empty() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        // upper_store doesn't exist → ENOENT → empty Vec.
        let outputs = scan_new_outputs(&dir.path().join("nonexistent"))?;
        assert!(outputs.is_empty());
        Ok(())
    }

    #[test]
    fn test_scan_new_outputs_with_paths() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let store_dir = dir.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Create in reverse alphabetical order to verify internal sort.
        fs::create_dir(store_dir.join("def-world"))?;
        fs::create_dir(store_dir.join("abc-hello"))?;
        // Hidden files should be skipped
        fs::write(store_dir.join(".links"), "")?;

        // scan_new_outputs sorts internally for deterministic output.
        let outputs = scan_new_outputs(&store_dir)?;
        assert_eq!(outputs, vec!["abc-hello", "def-world"]);
        Ok(())
    }

    #[test]
    fn test_nar_chunk_size() {
        // Verify chunk size is reasonable
        assert_eq!(rio_proto::client::NAR_CHUNK_SIZE, 256 * 1024);
    }

    // -----------------------------------------------------------------------
    // gRPC upload tests via MockStore
    // -----------------------------------------------------------------------

    use rio_test_support::fixtures::{test_drv_path, test_store_basename};
    use rio_test_support::grpc::{spawn_mock_store_inproc, spawn_mock_store_with_client};
    use std::sync::atomic::Ordering;

    use rio_test_support::fixtures::seed_store_output as make_output_file;

    /// Build the `OutputToUpload` an `upload_all_outputs` caller (the
    /// result pipeline) would pass: full store path, on-disk location,
    /// and recorded refs.
    fn out_for(store_dir: &Path, basename: &str, references: &[String]) -> OutputToUpload {
        OutputToUpload {
            store_path: format!("/nix/store/{basename}"),
            host_path: store_dir.join(basename),
            references: references.to_vec(),
            content_address: None,
        }
    }

    /// Prep helper for tests calling `upload_output` directly: builds the
    /// `PreparedOutput` exactly as `upload_all_outputs` does, with the
    /// caller-provided (pipeline-recorded) references.
    fn prep_one(store_dir: &Path, basename: &str, references: &[String]) -> common::PreparedOutput {
        let store_path = format!("/nix/store/{basename}");
        common::PreparedOutput {
            store_path: store_path.clone(),
            host_path: store_dir.join(basename),
            parsed: StorePath::parse(&store_path).expect("test basename is a valid store path"),
            references: references.to_vec(),
            content_address: None,
        }
    }

    #[tokio::test]
    async fn test_upload_output_success() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("hello");
        let (_tmp, store_dir) = make_output_file(&basename, b"hello world")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", "")
            .await
            .expect("upload should succeed");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Hash must match SHA-256 of the NAR serialization.
        let expected_nar = nar::dump_path(&store_dir.join(&basename))?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(result.nar_hash, expected_hash);
        assert_eq!(result.nar_size, expected_nar.len() as u64);

        // MockStore should have recorded exactly one PutPath call.
        let puts = store.calls.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(puts[0].store_path, format!("/nix/store/{basename}"));
        Ok(())
    }

    /// Retry with exponential backoff — first 2 attempts fail, 3rd succeeds.
    /// start_paused auto-advances the clock during sleep() so the 1s+2s
    /// backoff delays don't wall-clock-block the test.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_retries_then_succeeds() -> anyhow::Result<()> {
        let (store, mut client) = spawn_mock_store_inproc().await?;
        store.faults.fail_next_puts.store(2, Ordering::SeqCst);
        let basename = test_store_basename("retry");
        let (_tmp, store_dir) = make_output_file(&basename, b"retry me")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", "")
            .await
            .expect("upload should succeed on 3rd attempt");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Only the successful attempt records the put.
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 1);
        // All injected failures should have been consumed.
        assert_eq!(store.faults.fail_next_puts.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// More failures than MAX_UPLOAD_RETRIES → UploadExhausted.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_exhausts_retries() -> anyhow::Result<()> {
        let (store, mut client) = spawn_mock_store_inproc().await?;
        store
            .faults
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);
        let basename = test_store_basename("exhaust");
        let (_tmp, store_dir) = make_output_file(&basename, b"never uploads")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let err = upload_output(&mut client, p, "", "")
            .await
            .expect_err("upload should exhaust retries");

        assert!(
            matches!(err, UploadError::UploadExhausted { .. }),
            "expected UploadExhausted, got {err:?}"
        );
        // No successful PutPath recorded.
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 0);
        Ok(())
    }

    // r[verify builder.upload.aborted-poll]
    /// I-125b: PutPath returns `Aborted: concurrent PutPath` and the
    /// path then APPEARS in the store (another builder uploaded it) →
    /// upload_output adopts the store's result instead of failing.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_adopts_concurrent_uploader_result() -> anyhow::Result<()> {
        let (store, mut client) = spawn_mock_store_inproc().await?;
        // Every PutPath attempt aborts — proves we never re-upload.
        store
            .faults
            .abort_next_puts
            .store(u32::MAX, Ordering::SeqCst);

        let basename = test_store_basename("adopt");
        let store_path = format!("/nix/store/{basename}");
        let (_tmp, store_dir) = make_output_file(&basename, b"adopted content")?;

        // Seed the store as if a concurrent builder finished first.
        // nar_hash/nar_size here are what the OTHER builder produced;
        // upload_output must surface these (not re-derive locally).
        let (nar, nar_hash) = rio_test_support::fixtures::make_nar(b"adopted content");
        store.seed(
            rio_test_support::fixtures::make_path_info(&store_path, &nar, nar_hash),
            nar.clone(),
        );

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", "")
            .await
            .expect("should adopt concurrent uploader's result");

        assert_eq!(result.store_path, store_path);
        assert_eq!(result.nar_hash, nar_hash);
        assert_eq!(result.nar_size, nar.len() as u64);
        // No successful PutPath landed (all attempts aborted).
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 0);
        // QueryPathInfo was polled.
        assert!(
            store
                .calls
                .qpi_calls
                .read()
                .unwrap()
                .iter()
                .any(|p| p == &store_path)
        );
        Ok(())
    }

    /// I-125b: PutPath returns `Aborted: concurrent PutPath` but the
    /// path NEVER appears (placeholder held by a now-dead builder).
    /// After the poll window, upload_output retries the upload — which
    /// succeeds once the (mocked) lock is released.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_retries_after_concurrent_put_clears() -> anyhow::Result<()> {
        let (store, mut client) = spawn_mock_store_inproc().await?;
        // First PutPath aborts; second succeeds (lock released).
        store.faults.abort_next_puts.store(1, Ordering::SeqCst);

        let basename = test_store_basename("clears");
        let (_tmp, store_dir) = make_output_file(&basename, b"clears eventually")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", "")
            .await
            .expect("should retry upload after poll window");

        assert_eq!(result.store_path, format!("/nix/store/{basename}"));
        // Second attempt landed.
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 1);
        // Polled CONCURRENT_PUT_POLL_ATTEMPTS times, all NotFound.
        assert_eq!(
            store.calls.qpi_calls.read().unwrap().len(),
            CONCURRENT_PUT_POLL_ATTEMPTS as usize
        );
        Ok(())
    }

    /// I-125b negative: Aborted with a DIFFERENT message (not
    /// "concurrent PutPath") must NOT trigger the wait-then-adopt
    /// path — falls through to plain retry. Exercised via the matcher
    /// directly; the MockStore knob only emits the concurrent-PutPath
    /// flavour.
    #[test]
    fn test_is_concurrent_put_path_matcher() {
        // Single PutPath shape.
        assert!(is_concurrent_put_path(&tonic::Status::aborted(format!(
            "{} for this path; retry",
            rio_proto::CONCURRENT_PUTPATH_MSG
        ))));
        // PutPathBatch shape (output-index prefix + suffix).
        assert!(is_concurrent_put_path(&tonic::Status::aborted(format!(
            "output 1: {}; retry",
            rio_proto::CONCURRENT_PUTPATH_MSG
        ))));
        assert!(!is_concurrent_put_path(&tonic::Status::aborted(
            "GC mark in progress"
        )));
        assert!(!is_concurrent_put_path(&tonic::Status::unavailable(
            rio_proto::CONCURRENT_PUTPATH_MSG
        )));
    }

    /// upload_all_outputs runs concurrently; all outputs land in MockStore.
    #[tokio::test]
    async fn test_upload_all_outputs_multiple() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b1, b2, b3) = (
            test_store_basename("one"),
            test_store_basename("two"),
            test_store_basename("three"),
        );
        fs::write(store_dir.join(&b1), b"one")?;
        fs::write(store_dir.join(&b2), b"two")?;
        fs::write(store_dir.join(&b3), b"three")?;

        let results = upload_all_outputs(
            &client,
            "",
            "",
            &[
                out_for(&store_dir, &b1, &[]),
                out_for(&store_dir, &b2, &[]),
                out_for(&store_dir, &b3, &[]),
            ],
        )
        .await
        .expect("all uploads succeed");

        assert_eq!(results.len(), 3);
        // Result order is NOT guaranteed (buffer_unordered). Collect to set.
        let paths: std::collections::HashSet<_> =
            results.iter().map(|r| r.store_path.to_string()).collect();
        assert!(paths.contains(&format!("/nix/store/{b1}")));
        assert!(paths.contains(&format!("/nix/store/{b2}")));
        assert!(paths.contains(&format!("/nix/store/{b3}")));
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 3);
        Ok(())
    }

    /// ENOENT during the streaming dump → UploadExhausted (wraps the NAR
    /// error): the output file vanished between the result pipeline and
    /// the upload. Same ENOENT on every retry, so the budget exhausts.
    #[tokio::test(start_paused = true)]
    async fn test_upload_output_nar_serialize_error() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        // Create nix/store/ dir but NOT the output file.
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Use a VALID basename (32-char hash) so we get past the path
        // validation and into the dump that actually ENOENTs.
        let basename = test_store_basename("nonexistent");
        let (_store, mut client, _h) = spawn_mock_store_with_client().await?;
        let err = upload_output(&mut client, prep_one(&store_dir, &basename, &[]), "", "")
            .await
            .expect_err("should fail NAR serialization in the dump");

        // Same ENOENT every retry → UploadExhausted. Error message still
        // names path + cause.
        assert!(
            matches!(err, UploadError::UploadExhausted { .. }),
            "expected UploadExhausted, got {err:?}"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // HashingChannelWriter — the tee sink
    // -----------------------------------------------------------------------

    /// HashingChannelWriter produces the same SHA-256 as a direct digest.
    /// Runs in spawn_blocking because `blocking_send` panics otherwise.
    #[tokio::test]
    async fn test_hashing_channel_writer_hash_correct() -> anyhow::Result<()> {
        let data = b"hello world, this is tee test data";
        let expected_hash: [u8; 32] = Sha256::digest(data).into();

        let (tx, mut rx) = mpsc::channel(8);
        let data = data.to_vec();
        let (hash, size) = tokio::task::spawn_blocking(move || {
            let mut sink = HashingChannelWriter::new(tx);
            sink.write_all(&data).unwrap();
            sink.finalize()
        })
        .await?;

        assert_eq!(hash, expected_hash, "tee hash should match direct digest");
        assert_eq!(size, 34);

        // Trailer should have arrived.
        let mut saw_trailer = false;
        while let Some(msg) = rx.recv().await {
            if let Some(put_path_request::Msg::Trailer(t)) = msg.msg {
                assert_eq!(t.nar_hash, expected_hash.to_vec());
                assert_eq!(t.nar_size, 34);
                saw_trailer = true;
            }
        }
        assert!(saw_trailer, "trailer should be sent");
        Ok(())
    }

    /// The recorded PutPath call in MockStore has the REAL hash from the
    /// trailer — proves the MockStore trailer-apply logic and the upload
    /// tee produce the same result as a naive dump_path() + digest().
    #[tokio::test]
    async fn test_upload_streaming_mockstore_has_trailer_hash() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("tee-hash");
        let (_tmp, store_dir) = make_output_file(&basename, b"tee upload test data")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", "").await?;

        // The hash returned by upload_output == the hash MockStore recorded
        // == SHA-256 of dump_path(). Three-way consistency.
        let expected_nar = nar::dump_path(&store_dir.join(&basename))?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(result.nar_hash, expected_hash, "worker's hash");

        let puts = store.calls.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].nar_hash,
            expected_hash.to_vec(),
            "MockStore should have the TRAILER's hash, not the empty metadata hash"
        );
        assert_eq!(puts[0].nar_size, expected_nar.len() as u64);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Reference scanning (pre-scan pass → PathInfo.references)
    // -----------------------------------------------------------------------

    /// Two distinct valid nixbase32 hashes for building test candidate paths.
    /// Must differ from TEST_HASH (aaaa...) used by test_store_basename, so
    /// the recorded reference paths stay distinct.
    const DEP_HASH_A: &str = "7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    const DEP_HASH_B: &str = "v5sv61sszx301i0x6xysaqzla09nksnd";

    /// r[verify builder.upload.references-scanned+2]
    /// r[verify builder.upload.deriver-populated]
    ///
    /// End-to-end: the references recorded by the result pipeline are
    /// delivered unchanged → PathInfo.references arrives at MockStore
    /// exactly as provided (no rescan, no additions). Also checks
    /// PathInfo.deriver is populated (was String::new() before this fix).
    #[tokio::test]
    async fn test_upload_output_sends_recorded_references() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("scanned");
        let deriver = test_drv_path("scanned");

        // Two candidate deps. Output contents mention dep-A (as a full store
        // path, the way a real RPATH or shebang would). dep-B is NOT in the
        // output — verifies we don't over-report.
        let dep_a = format!("/nix/store/{DEP_HASH_A}-glibc-2.38");
        let _dep_b = format!("/nix/store/{DEP_HASH_B}-unused");
        let self_path = format!("/nix/store/{basename}");
        let contents = format!("RPATH={dep_a}/lib\nself={self_path}\n");
        let (_tmp, store_dir) = make_output_file(&basename, contents.as_bytes())?;

        // References as the result pipeline recorded them: dep-A + the
        // output's own path (self-references are legal — binaries embed
        // their own store path in rpaths). dep-B is NOT recorded.
        let p = prep_one(&store_dir, &basename, &[dep_a.clone(), self_path.clone()]);
        let result = upload_output(&mut client, p, "", &deriver).await?;

        // Result carries exactly the provided refs — no rescan, no
        // additions, dep-B absent.
        let refs: Vec<String> = result.references.iter().map(|r| r.to_string()).collect();
        assert_eq!(
            refs,
            vec![dep_a.clone(), self_path.clone()],
            "provided refs: dep-A + self, no dep-B"
        );

        // MockStore recorded the PathInfo WITH references + deriver. This is
        // the actual fix — pre-fix, both were always empty (upload.rs:223-224).
        let puts = store.calls.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1);
        assert_eq!(
            puts[0].references,
            vec![dep_a, self_path],
            "PathInfo.references delivered to store"
        );
        assert_eq!(
            puts[0].deriver, deriver,
            "PathInfo.deriver delivered to store"
        );
        Ok(())
    }

    /// Output with no embedded store paths → empty references (but deriver
    /// still populated). Legal for CA paths like fetchurl.
    #[tokio::test]
    async fn test_upload_output_no_references_found() -> anyhow::Result<()> {
        let (store, mut client, _h) = spawn_mock_store_with_client().await?;
        let basename = test_store_basename("noref");
        let deriver = test_drv_path("noref");
        let (_tmp, store_dir) = make_output_file(&basename, b"plain text, no store paths here")?;

        let p = prep_one(&store_dir, &basename, &[]);
        let result = upload_output(&mut client, p, "", &deriver).await?;

        assert!(
            result.references.is_empty(),
            "no refs recorded for this output"
        );
        let puts = store.calls.put_calls.read().unwrap();
        assert!(puts[0].references.is_empty());
        assert_eq!(
            puts[0].deriver, deriver,
            "deriver still set even with zero refs"
        );
        Ok(())
    }

    /// Reference scanning in upload_all_outputs: candidate set built once,
    /// shared across all outputs. Each output gets its own scan result.
    /// Also asserts `rio_builder_upload_references_count` is recorded on
    /// the batch path (bug_248: was only emitted from single.rs).
    #[tokio::test]
    async fn test_upload_all_outputs_per_output_refs() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        // Two outputs: one mentions dep-A, the other mentions dep-B.
        // Use distinct hashes for the outputs so their self-reference
        // paths stay distinct (test_store_basename uses a single TEST_HASH).
        let out1 = format!("{DEP_HASH_A}-out1");
        let out2 = format!("{DEP_HASH_B}-out2");
        let dep_a = format!("/nix/store/{DEP_HASH_A}-out1"); // out1 self-ref
        let dep_b = format!("/nix/store/{DEP_HASH_B}-out2"); // out2 self-ref
        fs::write(store_dir.join(&out1), format!("ref={dep_a}"))?;
        fs::write(store_dir.join(&out2), format!("ref={dep_b}"))?;

        let deriver = test_drv_path("multi");
        let results = upload_all_outputs(
            &client,
            "",
            &deriver,
            &[
                out_for(&store_dir, &out1, std::slice::from_ref(&dep_a)),
                out_for(&store_dir, &out2, std::slice::from_ref(&dep_b)),
            ],
        )
        .await?;

        assert_eq!(results.len(), 2);
        // Find by store_path (buffer_unordered → result order is not guaranteed).
        let r1 = results
            .iter()
            .find(|r| r.store_path.as_str().ends_with("-out1"))
            .expect("out1 result");
        let r2 = results
            .iter()
            .find(|r| r.store_path.as_str().ends_with("-out2"))
            .expect("out2 result");
        let refs = |r: &ValidatedPathInfo| -> Vec<String> {
            r.references.iter().map(|p| p.to_string()).collect()
        };
        assert_eq!(refs(r1), vec![dep_a], "out1 refs itself only");
        assert_eq!(refs(r2), vec![dep_b], "out2 refs itself only");

        // All MockStore PathInfos carry the deriver.
        let puts = store.calls.put_calls.read().unwrap();
        assert_eq!(puts.len(), 2);
        for p in puts.iter() {
            assert_eq!(p.deriver, deriver);
        }
        // bug_248: 2 outputs → batch path; histogram MUST have been
        // recorded (single chokepoint in `prepare_output`). Pre-fix this
        // was 0 samples on batch.
        assert!(
            recorder.histogram_touched("rio_builder_upload_references_count"),
            "references-count histogram must be recorded on the batch path"
        );
        Ok(())
    }

    /// Write > 256KiB through the tee → multiple chunks produced + correct hash.
    #[tokio::test]
    async fn test_hashing_channel_writer_multi_chunk() -> anyhow::Result<()> {
        // 600 KiB of predictable bytes.
        let data: Vec<u8> = (0..600 * 1024).map(|i| (i % 256) as u8).collect();
        let expected_hash: [u8; 32] = Sha256::digest(&data).into();

        let (tx, mut rx) = mpsc::channel(8);
        let data_owned = data.clone();
        let (hash, size) = tokio::task::spawn_blocking(move || {
            let mut sink = HashingChannelWriter::new(tx);
            sink.write_all(&data_owned).unwrap();
            sink.finalize()
        })
        .await?;

        assert_eq!(hash, expected_hash);
        assert_eq!(size, 600 * 1024);

        // Count chunks + reassemble + verify.
        let mut chunk_count = 0;
        let mut reassembled = Vec::new();
        while let Some(msg) = rx.recv().await {
            match msg.msg {
                Some(put_path_request::Msg::NarChunk(c)) => {
                    chunk_count += 1;
                    reassembled.extend_from_slice(&c);
                }
                Some(put_path_request::Msg::Trailer(_)) => break,
                _ => {}
            }
        }
        assert!(chunk_count >= 2, "600KiB at 256KiB/chunk → ≥2 chunks");
        assert_eq!(reassembled, data, "reassembled chunks should == input");
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Idempotent pre-check (FindMissingPaths → skip already-present outputs)
    // -----------------------------------------------------------------------

    /// r[verify builder.upload.idempotent-precheck]
    ///
    /// Output already in store → zero PutPath calls, result carries the
    /// STORE's nar_hash (not a freshly-computed one). This is the exit
    /// criterion: "second identical-NAR upload → zero chunks".
    ///
    /// Disk contents are deliberately DIFFERENT from what's seeded in the
    /// store: the test asserts the returned nar_hash matches the SEEDED
    /// hash, NOT the on-disk NAR's hash. Proves we queried the store
    /// instead of reading disk (the optimization's whole point).
    #[tokio::test]
    async fn test_upload_all_outputs_skips_already_present() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let basename = format!("{DEP_HASH_A}-already-there");
        let store_path = format!("/nix/store/{basename}");

        // Seed: path already complete in store. seed_with_content builds a
        // NAR from "seeded content" and returns its hash — the nar_hash the
        // worker should return for the skipped path.
        let (_seeded_nar, seeded_hash) = store.seed_with_content(&store_path, b"seeded content");

        // Disk: DIFFERENT contents. If the pre-check is broken (falls
        // through to upload), the result's nar_hash would be the disk
        // NAR's hash, not seeded_hash. This is the precondition assert —
        // without distinct contents, the test passes trivially.
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&basename), b"DIFFERENT disk contents")?;
        let disk_nar = nar::dump_path(&store_dir.join(&basename))?;
        let disk_hash: [u8; 32] = Sha256::digest(&disk_nar).into();
        assert_ne!(
            seeded_hash, disk_hash,
            "precondition: seeded vs disk NARs must differ, else this test proves nothing"
        );

        let results =
            upload_all_outputs(&client, "", "", &[out_for(&store_dir, &basename, &[])]).await?;

        // Zero PutPath calls — the skip fired.
        assert_eq!(
            store.calls.put_calls.read().unwrap().len(),
            0,
            "pre-check should skip already-present path; zero PutPath calls"
        );
        // One result, carrying the STORE's hash (not disk's).
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].store_path.as_str(), store_path);
        assert_eq!(
            results[0].nar_hash, seeded_hash,
            "skipped path's result carries the store's nar_hash, not disk's"
        );
        Ok(())
    }

    /// Mixed: one output already present, one missing. Only the missing
    /// one hits PutPath; both appear in results.
    #[tokio::test]
    async fn test_upload_all_outputs_mixed_presence() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;

        let b_present = format!("{DEP_HASH_A}-present");
        let b_missing = format!("{DEP_HASH_B}-missing");
        let path_present = format!("/nix/store/{b_present}");
        let path_missing = format!("/nix/store/{b_missing}");

        let (_nar, seeded_hash) = store.seed_with_content(&path_present, b"already here");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&b_present), b"disk present")?;
        fs::write(store_dir.join(&b_missing), b"disk missing")?;

        let results = upload_all_outputs(
            &client,
            "",
            "",
            &[
                out_for(&store_dir, &b_present, &[]),
                out_for(&store_dir, &b_missing, &[]),
            ],
        )
        .await?;

        // Exactly one PutPath: the missing output.
        let puts = store.calls.put_calls.read().unwrap();
        assert_eq!(puts.len(), 1, "only the missing output hits PutPath");
        assert_eq!(puts[0].store_path, path_missing);

        // Both outputs in results (caller needs ALL outputs reported).
        assert_eq!(results.len(), 2);
        let r_present = results
            .iter()
            .find(|r| r.store_path.as_str() == path_present)
            .expect("present output in results");
        let r_missing = results
            .iter()
            .find(|r| r.store_path.as_str() == path_missing)
            .expect("missing output in results");

        // Present: store's hash. Missing: freshly computed.
        assert_eq!(r_present.nar_hash, seeded_hash);
        let missing_nar = nar::dump_path(&store_dir.join(&b_missing))?;
        let missing_hash: [u8; 32] = Sha256::digest(&missing_nar).into();
        assert_eq!(r_missing.nar_hash, missing_hash);
        Ok(())
    }

    /// FindMissingPaths errors → fall back to upload-all. Best-effort:
    /// store transient doesn't break the upload; r[store.put.idempotent]
    /// catches duplicates server-side. Zero behavior change from the
    /// pre-precheck world.
    #[tokio::test]
    async fn test_upload_all_outputs_find_missing_error_falls_back() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        store.faults.fail_find_missing.store(true, Ordering::SeqCst);

        let basename = format!("{DEP_HASH_A}-fallback");
        let store_path = format!("/nix/store/{basename}");

        // Seed the path — WOULD be skipped if FindMissingPaths worked.
        store.seed_with_content(&store_path, b"seeded");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&basename), b"disk fallback")?;

        let results =
            upload_all_outputs(&client, "", "", &[out_for(&store_dir, &basename, &[])]).await?;

        // FindMissingPaths failed → fell back to upload → PutPath called.
        // (MockStore's put_path doesn't implement the idempotent no-op;
        // it happily overwrites. Real store would no-op. We're testing
        // the WORKER's fail-open, not the store's idempotency.)
        assert_eq!(
            store.calls.put_calls.read().unwrap().len(),
            1,
            "FindMissingPaths error → fall back to upload (fail-open)"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].store_path.as_str(), store_path);
        // Hash is the disk NAR's (we uploaded, didn't skip).
        let disk_nar = nar::dump_path(&store_dir.join(&basename))?;
        let disk_hash: [u8; 32] = Sha256::digest(&disk_nar).into();
        assert_eq!(results[0].nar_hash, disk_hash);
        Ok(())
    }

    /// Empty overlay upper → early return, no FindMissingPaths call.
    /// Guards the `if outputs.is_empty()` branch added to avoid an empty
    /// RPC on derivations that produce nothing in the upper (shouldn't
    /// happen in practice, but the branch is there).
    #[tokio::test]
    async fn test_upload_all_outputs_empty_no_rpc() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        // Arm the failure: if FindMissingPaths were called, the test
        // would still pass (fail-open), but the empty check should
        // short-circuit BEFORE the RPC.
        store.faults.fail_find_missing.store(true, Ordering::SeqCst);
        // MockStore doesn't expose call-count for find_missing_paths.
        // Just assert empty result + zero PutPath. The is_empty() guard
        // is simple enough that existence-in-code is the real assurance.
        let results = upload_all_outputs(&client, "", "", &[]).await?;
        assert!(results.is_empty());
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 0);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Batch atomicity / retry / liveness regressions
    // -----------------------------------------------------------------------

    // r[verify builder.upload.batch+3]
    // r[verify store.atomic.multi-output+1]
    /// bug_392 (adapted): prep failure on output k must NOT leave outputs
    /// 0..k-1 committed server-side. Prep no longer reads disk (references
    /// come precomputed from the result pipeline), so the prep-detectable
    /// failure here is a reference that fails `StorePath::parse` — the
    /// prep loop fails BEFORE any gRPC → put_calls.len() == 0. (A file
    /// that disappears between the result pipeline and the dump is
    /// covered by the store-side batch atomicity, not by this worker-side
    /// guarantee.)
    #[tokio::test(start_paused = true)]
    async fn test_batch_prep_failure_no_partial_commit() -> anyhow::Result<()> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        let b_ok = format!("{DEP_HASH_A}-ok");
        let b_bad = format!("{DEP_HASH_B}-bad-ref");
        fs::write(store_dir.join(&b_ok), b"ok contents")?;
        fs::write(store_dir.join(&b_bad), b"bad ref contents")?;

        let err = upload_all_outputs(
            &client,
            "",
            "",
            &[
                out_for(&store_dir, &b_ok, &[]),
                out_for(&store_dir, &b_bad, &["not-a-store-path".to_string()]),
            ],
        )
        .await
        .expect_err("prep must fail on the unparseable reference");
        assert!(matches!(err, UploadError::InvalidReference { .. }));

        assert_eq!(
            store.calls.put_calls.read().unwrap().len(),
            0,
            "prep failure must not partially commit — zero PutPath/PutPathBatch entries"
        );
        Ok(())
    }

    /// bug_167: batch path retries transient store errors with the same
    /// MAX_UPLOAD_RETRIES budget as the single-output path. Pre-fix: a
    /// single `Unavailable` on a 2-output derivation hit `return Err(e)`
    /// → InfrastructureFailure → full re-build.
    #[tokio::test(start_paused = true)]
    async fn test_upload_all_outputs_batch_retries_transient() -> anyhow::Result<()> {
        let (store, client) = spawn_mock_store_inproc().await?;
        // First two batch RPCs return Unavailable; third succeeds.
        store.faults.fail_next_puts.store(2, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b1, b2) = (format!("{DEP_HASH_A}-one"), format!("{DEP_HASH_B}-two"));
        fs::write(store_dir.join(&b1), b"one")?;
        fs::write(store_dir.join(&b2), b"two")?;

        let results = upload_all_outputs(
            &client,
            "",
            "",
            &[out_for(&store_dir, &b1, &[]), out_for(&store_dir, &b2, &[])],
        )
        .await
        .expect("batch upload should succeed on 3rd attempt");

        assert_eq!(results.len(), 2);
        assert_eq!(
            store.calls.put_calls.read().unwrap().len(),
            2,
            "both outputs committed on the successful attempt"
        );
        assert_eq!(
            store.faults.fail_next_puts.load(Ordering::SeqCst),
            0,
            "all injected failures consumed"
        );
        Ok(())
    }

    // r[verify builder.upload.batch+3]
    /// Round-17 bug_111 R1(e) rider: a deterministic `InvalidArgument`
    /// from the batch RPC is permanent at the producing statement —
    /// exactly ONE attempt, no fallback. Detector shape: the injection
    /// knob is set to 2, so a buggy retry loop would consume the second
    /// injection (or even succeed on attempt 3); the surviving count
    /// proves exactly one attempt fired.
    #[tokio::test(start_paused = true)]
    async fn test_batch_invalid_argument_not_retried() -> anyhow::Result<()> {
        let (store, client) = spawn_mock_store_inproc().await?;
        store.faults.invalid_next_puts.store(2, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b1, b2) = (format!("{DEP_HASH_A}-one"), format!("{DEP_HASH_B}-two"));
        fs::write(store_dir.join(&b1), b"one")?;
        fs::write(store_dir.join(&b2), b"two")?;

        let err = upload_all_outputs(
            &client,
            "",
            "",
            &[out_for(&store_dir, &b1, &[]), out_for(&store_dir, &b2, &[])],
        )
        .await
        .expect_err("deterministic rejection must fail the upload");
        assert!(
            matches!(
                &err,
                UploadError::UploadExhausted { source, .. }
                    if source.code() == tonic::Code::InvalidArgument
            ),
            "permanent classification carries the original status: {err:?}"
        );
        assert_eq!(
            store.faults.invalid_next_puts.load(Ordering::SeqCst),
            1,
            "exactly one batch attempt — no retry burned the second injection"
        );
        assert_eq!(
            store.calls.put_calls.read().unwrap().len(),
            0,
            "nothing committed and no per-output fallback attempted"
        );
        Ok(())
    }

    /// Round-17 bug_111 R1(e) rider, single-output flavor: `InvalidArgument`
    /// on independent PutPath is permanent after exactly one attempt.
    #[tokio::test(start_paused = true)]
    async fn test_single_invalid_argument_not_retried() -> anyhow::Result<()> {
        let (store, client) = spawn_mock_store_inproc().await?;
        store.faults.invalid_next_puts.store(2, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let b1 = format!("{DEP_HASH_A}-solo");
        fs::write(store_dir.join(&b1), b"solo")?;

        let err = upload_all_outputs(&client, "", "", &[out_for(&store_dir, &b1, &[])])
            .await
            .expect_err("deterministic rejection must fail the upload");
        assert!(
            matches!(
                &err,
                UploadError::UploadExhausted { source, .. }
                    if source.code() == tonic::Code::InvalidArgument
            ),
            "permanent classification carries the original status: {err:?}"
        );
        assert_eq!(
            store.faults.invalid_next_puts.load(Ordering::SeqCst),
            1,
            "exactly one PutPath attempt"
        );
        Ok(())
    }

    /// bug_167 (exhaustion): batch retries exhaust → UploadExhausted, NOT
    /// fallthrough to per-output. `Unavailable` ≠ `FailedPrecondition`.
    #[tokio::test(start_paused = true)]
    async fn test_upload_all_outputs_batch_exhausts_transient() -> anyhow::Result<()> {
        let (store, client) = spawn_mock_store_inproc().await?;
        store
            .faults
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b_a, b_b) = (format!("{DEP_HASH_A}-a"), format!("{DEP_HASH_B}-b"));
        fs::write(store_dir.join(&b_a), b"a")?;
        fs::write(store_dir.join(&b_b), b"b")?;

        let err = upload_all_outputs(
            &client,
            "",
            "",
            &[
                out_for(&store_dir, &b_a, &[]),
                out_for(&store_dir, &b_b, &[]),
            ],
        )
        .await
        .expect_err("batch should exhaust retries");
        assert!(matches!(err, UploadError::UploadExhausted { .. }));
        assert_eq!(store.calls.put_calls.read().unwrap().len(), 0);
        Ok(())
    }

    /// bug_388/429/D1: `await_dump_bounded` returns DeadlineExceeded when
    /// the join handle never completes. Uses an async (not blocking)
    /// pending task so paused-time auto-advance fires — a real
    /// spawn_blocking parked in open() inhibits auto-advance, which is
    /// exactly the production hang this guard exists to break.
    #[tokio::test(start_paused = true)]
    async fn test_await_dump_bounded_times_out() {
        use std::time::Duration;
        let h: tokio::task::JoinHandle<Result<(), tonic::Status>> =
            tokio::spawn(async { std::future::pending().await });
        let r = common::await_dump_bounded("test-dump", Duration::from_secs(10), h).await;
        let status = r.expect_err("must time out");
        assert_eq!(status.code(), tonic::Code::DeadlineExceeded);
        assert!(
            status.message().contains("stuck"),
            "message names the hang: {status:?}"
        );
    }

    /// `await_dump_bounded` happy path: task completes → result passes
    /// through unchanged.
    #[tokio::test(start_paused = true)]
    async fn test_await_dump_bounded_ok_passthrough() {
        use std::time::Duration;
        let h = tokio::spawn(async { 42u64 });
        let r = common::await_dump_bounded("test-dump", Duration::from_secs(10), h).await;
        assert_eq!(r.expect("ok"), 42);
    }

    /// bug_129/137: post-rx-drop join must wait ONLY `DUMP_JOIN_SLACK`,
    /// not budget+slack — the budget was already spent concurrently with
    /// the gRPC await. Under paused time, assert the timeout fires at
    /// exactly 30s (not 330s / not 4830s).
    #[tokio::test(start_paused = true)]
    async fn test_await_dump_after_rx_drop_only_slack() {
        let start = tokio::time::Instant::now();
        let h: tokio::task::JoinHandle<()> = tokio::spawn(std::future::pending());
        let r = common::await_dump_after_rx_drop("test", h).await;
        assert_eq!(
            r.expect_err("must time out").code(),
            tonic::Code::DeadlineExceeded
        );
        assert_eq!(
            start.elapsed(),
            common::DUMP_JOIN_SLACK,
            "post-rx-drop join must NOT re-wait the gRPC budget"
        );
    }
    /// The content-address descriptor reaches both the PutPath metadata
    /// (what the store records as the narinfo `CA:` field) and the
    /// returned `ValidatedPathInfo`.
    #[test]
    fn content_address_threads_into_path_info() {
        let store_path = format!("/nix/store/{}", test_store_basename("ca-out"));
        let parsed = rio_nix::store_path::StorePath::parse(&store_path).unwrap();
        let ca = "fixed:r:sha256:1abcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabcabc";

        let info = common::trailer_mode_path_info(&store_path, "drv", &[], Some(ca));
        assert_eq!(info.content_address, ca);
        let info_none = common::trailer_mode_path_info(&store_path, "drv", &[], None);
        assert!(info_none.content_address.is_empty());

        let validated = common::uploaded_info(
            parsed,
            [0u8; 32],
            1,
            Vec::new(),
            "drv",
            Some(ca.to_string()),
        )
        .unwrap();
        assert_eq!(validated.content_address.as_deref(), Some(ca));
    }
}
