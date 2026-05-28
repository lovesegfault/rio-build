//! Output upload to rio-store after build completion.
//!
//! Scans the overlay upper layer for new store paths and uploads them
//! all in one `StoreService.PutPathChunked` client stream (ADR-022 §6,
//! P0586). The fused single-pass output walk lives in [`walk`], the
//! `Begin`-frame assembly + `HasChunks` probe + chunk stream with retry
//! in [`chunked`], and the shared mechanics in [`common`].
// r[impl builder.upload.multi-output]

use std::os::unix::fs::FileTypeExt;
use std::path::Path;

use tonic::transport::Channel;
use tracing::instrument;

use rio_proto::StoreServiceClient;
use rio_proto::types::FindMissingPathsRequest;
use rio_proto::validated::ValidatedPathInfo;

use crate::store_fetch::StoreClients;

mod chunked;
pub(crate) mod common;
mod walk;

#[cfg(test)]
mod agreement_tests;

use common::MAX_UPLOAD_RETRIES;

/// Errors from upload operations.
///
/// Transient store errors that survive the whole retry budget surface
/// as `UploadExhausted`; deterministic store rejections and local prep
/// failures (path parse, output walk) surface as `UploadRejected`.
/// Both carry the underlying `tonic::Status`.
#[derive(Debug, thiserror::Error)]
pub enum UploadError {
    /// Every attempt in the retry budget failed with a retryable
    /// status. NOTE: `nix/tests/scenarios/chaos.nix` greps journald for
    /// the literal `upload failed after` substring to detect retry
    /// exhaustion — keep that substring if rewording.
    #[error("upload failed after {MAX_UPLOAD_RETRIES} retries for {path}: {source}")]
    UploadExhausted { path: String, source: tonic::Status },
    /// The store rejected the upload with a deterministic status
    /// (`InvalidArgument`, `FailedPrecondition`, `PermissionDenied`, …)
    /// or local preparation (store-path parse, output walk) failed.
    /// Retrying would fail identically, so at most one attempt was made
    /// — kept distinct from [`UploadError::UploadExhausted`] so triage
    /// can tell "the store said no" from "the store kept being
    /// unreachable".
    #[error("upload rejected (terminal, not retried) for {path}: {source}")]
    UploadRejected { path: String, source: tonic::Status },
    #[error("I/O error: {0}")]
    Io(#[from] std::io::Error),
    /// Ref-scan returned a path that fails `StorePath::parse`. The
    /// candidate set is built from validated input-closure paths, so
    /// this indicates a bug in `CandidateSet::resolve` (or a mismatched
    /// store-prefix). Surfaced as an error — silently dropping the ref
    /// would publish a path with a broken reference graph.
    #[error("ref-scan returned unparseable store path {path:?}")]
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

/// Upload all new outputs from the overlay upper layer.
///
/// Pipeline:
/// 1. **Idempotency pre-check** (`r[builder.upload.idempotent-precheck+2]`):
///    `FindMissingPaths` over the scanned set. When EVERY output is
///    already present, the walk and the upload are skipped entirely —
///    `QueryPathInfo` supplies each `ValidatedPathInfo` with zero disk
///    reads. When only some are present, all outputs still go in the one
///    `Begin` frame (candidate-set rationale at the collect_outputs call
///    site, executor/outputs.rs).
/// 2. **`PutPathChunked`** ([`chunked::upload_outputs_chunked`]): walk
///    every output, probe `HasChunks`, stream `Begin` + novel chunk
///    bodies in one RPC committed atomically server-side.
///
/// `input_closure` is echoed verbatim into the `Begin` frame (it must
/// hash to the assignment token's `input_closure_digest`) and doubles as
/// the reference-scan candidate set.
///
/// Results are in scanned-output order; callers should still match by
/// `.store_path` rather than position.
#[instrument(skip_all)]
pub async fn upload_all_outputs(
    clients: &StoreClients,
    upper_store: &Path,
    assignment_token: &str,
    deriver: &str,
    input_closure: &[String],
) -> Result<Vec<ValidatedPathInfo>, UploadError> {
    let outputs = scan_new_outputs(upper_store)?;
    if outputs.is_empty() {
        return Ok(Vec::new());
    }

    // r[impl builder.upload.idempotent-precheck+2]
    if let Some(already_present) = all_outputs_already_present(&clients.store, &outputs).await {
        return Ok(already_present);
    }

    chunked::upload_outputs_chunked(
        clients,
        upper_store,
        &outputs,
        assignment_token,
        deriver,
        input_closure,
    )
    .await
}

/// Idempotency pre-check: when EVERY scanned output is already complete
/// in the store, fetch each `ValidatedPathInfo` via `QueryPathInfo` and
/// skip the walk + upload entirely.
///
/// Best-effort and fail-open: any error (`FindMissingPaths` unavailable,
/// `QueryPathInfo` disagreeing with the presence answer) returns `None`
/// and the caller uploads everything — the store's idempotent
/// `PutPathChunked` reports `created = false` for paths it already has,
/// so correctness never depends on this check. The skip saves the
/// full-output disk walk and the gRPC stream when a derivation is
/// re-dispatched after its outputs already landed.
async fn all_outputs_already_present(
    store_client: &StoreServiceClient<Channel>,
    basenames: &[String],
) -> Option<Vec<ValidatedPathInfo>> {
    let store_paths: Vec<String> = basenames
        .iter()
        .map(|b| format!("/nix/store/{b}"))
        .collect();
    let mut client = store_client.clone();
    let mut req = tonic::Request::new(FindMissingPathsRequest {
        store_paths: store_paths.clone(),
    });
    rio_proto::interceptor::inject_current(req.metadata_mut());

    let missing = match rio_common::grpc::with_timeout_status(
        "FindMissingPaths",
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        client.find_missing_paths(req),
    )
    .await
    {
        Ok(resp) => resp.into_inner().missing_paths,
        Err(e) => {
            tracing::warn!(
                error = %e,
                "idempotent pre-check: FindMissingPaths failed; uploading everything \
                 (the store's idempotent PutPathChunked catches duplicates)"
            );
            return None;
        }
    };
    if !missing.is_empty() {
        return None;
    }

    let mut infos = Vec::with_capacity(store_paths.len());
    for store_path in &store_paths {
        // Present in store — fetch nar_hash/nar_size instead of re-walking.
        // QueryPathInfo is cheap (~1 PG row read); the walk is a full
        // disk read of the output.
        match rio_proto::client::query_path_info_opt(
            &mut client,
            store_path,
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
                infos.push(info);
            }
            // FindMissingPaths said present but QueryPathInfo disagrees
            // (TOCTOU or transient). Fall back to the upload path.
            Ok(None) | Err(_) => {
                tracing::warn!(
                    store_path = %store_path,
                    "idempotent pre-check: FindMissingPaths said present but \
                     QueryPathInfo disagreed; uploading everything"
                );
                return None;
            }
        }
    }
    Some(infos)
}

// r[verify builder.upload.multi-output]
#[cfg(test)]
mod tests {
    use super::*;
    use std::fs;
    use std::sync::atomic::Ordering;

    use rio_nix::nar;
    use rio_test_support::fixtures::{
        seed_store_output as make_output_file, test_drv_path, test_store_basename,
    };
    use rio_test_support::grpc::{spawn_mock_store, spawn_mock_store_inproc_channel};
    use sha2::{Digest, Sha256};

    /// Two distinct valid nixbase32 hashes for building test candidate paths.
    /// Must differ from TEST_HASH (aaaa...) used by test_store_basename, so
    /// the CandidateSet's hash→path map doesn't collide.
    const DEP_HASH_A: &str = "7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
    const DEP_HASH_B: &str = "v5sv61sszx301i0x6xysaqzla09nksnd";

    /// Spawn the TCP MockStore and wrap its address in the production
    /// `StoreClients` bundle (store + chunk over one channel).
    async fn spawn_clients() -> anyhow::Result<(
        rio_test_support::grpc::MockStore,
        StoreClients,
        tokio::task::JoinHandle<()>,
    )> {
        let (store, addr, handle) = spawn_mock_store().await?;
        let ch = tonic::transport::Channel::from_shared(format!("http://{addr}"))?
            .connect()
            .await?;
        Ok((store, StoreClients::from_channel(ch), handle))
    }

    /// In-process duplex variant for `start_paused` retry tests (real
    /// TCP + paused time gives spurious DeadlineExceeded — see
    /// `spawn_mock_store_inproc`).
    async fn spawn_clients_inproc()
    -> anyhow::Result<(rio_test_support::grpc::MockStore, StoreClients)> {
        let (store, ch) = spawn_mock_store_inproc_channel().await?;
        Ok((store, StoreClients::from_channel(ch)))
    }

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

    /// Single output → one PutPathChunked call carrying one
    /// ChunkedOutput whose nar_hash/nar_size match the eager
    /// `dump_path` oracle, whose chunk bytes land in the mock's
    /// chunk store (so a later HasChunks sees them), and whose
    /// request metadata carries the assignment token (without it a
    /// production store with an HMAC verifier rejects every upload).
    #[tokio::test]
    async fn test_upload_single_output_chunked() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
        let basename = test_store_basename("hello");
        let (_tmp, store_dir) = make_output_file(&basename, b"hello world")?;
        let deriver = test_drv_path("hello");

        let results =
            upload_all_outputs(&clients, &store_dir, "tok-abc.def", &deriver, &[]).await?;

        assert_eq!(results.len(), 1);
        assert_eq!(
            results[0].store_path.as_str(),
            format!("/nix/store/{basename}")
        );
        let expected_nar = nar::dump_path(&store_dir.join(&basename))?;
        let expected_hash: [u8; 32] = Sha256::digest(&expected_nar).into();
        assert_eq!(results[0].nar_hash, expected_hash);
        assert_eq!(results[0].nar_size, expected_nar.len() as u64);

        let calls = store.calls.put_chunked_calls.read().unwrap();
        assert_eq!(calls.len(), 1, "exactly one PutPathChunked RPC");
        let begin = &calls[0].begin;
        assert_eq!(begin.outputs.len(), 1);
        assert_eq!(begin.outputs[0].nar_hash, expected_hash.to_vec());
        assert_eq!(begin.deriver, deriver);
        assert_eq!(
            calls[0].token.as_deref(),
            Some("tok-abc.def"),
            "x-rio-assignment-token header delivered with the stream"
        );
        // The single regular file is one chunk; its bytes are now in
        // the mock's chunk store keyed by content blake3.
        let chunk_digest = blake3::hash(b"hello world");
        assert!(
            store
                .state
                .chunks
                .read()
                .unwrap()
                .contains_key(chunk_digest.as_bytes().as_slice()),
            "uploaded chunk bytes recorded in the chunk store"
        );
        Ok(())
    }

    // r[verify builder.upload.batch+3]
    /// All outputs of a derivation travel in ONE PutPathChunked stream.
    #[tokio::test]
    async fn test_upload_all_outputs_multiple_one_rpc() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
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

        let results = upload_all_outputs(&clients, &store_dir, "", "", &[])
            .await
            .expect("all uploads succeed");

        assert_eq!(results.len(), 3);
        let paths: std::collections::HashSet<_> =
            results.iter().map(|r| r.store_path.to_string()).collect();
        for b in [&b1, &b2, &b3] {
            assert!(paths.contains(&format!("/nix/store/{b}")));
        }

        let calls = store.calls.put_chunked_calls.read().unwrap();
        assert_eq!(calls.len(), 1, "one RPC for the whole derivation");
        assert_eq!(calls[0].begin.outputs.len(), 3);
        Ok(())
    }

    /// r[verify builder.upload.references-scanned+2]
    /// r[verify builder.upload.deriver-populated]
    ///
    /// End-to-end: output contents embed a closure path → the claimed
    /// references in the Begin frame contain exactly that path (plus the
    /// self-reference), sorted; an output with no embedded paths claims
    /// none; the deriver rides the Begin frame; the per-output
    /// references-count histogram is recorded (bug_248: the emission
    /// site must survive the upload-path rewrite).
    #[tokio::test]
    async fn test_upload_chunked_claims_scanned_references() -> anyhow::Result<()> {
        let recorder = rio_test_support::metrics::CountingRecorder::default();
        let _guard = metrics::set_default_local_recorder(&recorder);
        let (store, clients, _h) = spawn_clients().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        let dep_a = format!("/nix/store/{DEP_HASH_A}-glibc-2.38");
        let dep_b = format!("/nix/store/{DEP_HASH_B}-unused");
        let with_refs = test_store_basename("scanned");
        let self_path = format!("/nix/store/{with_refs}");
        let no_refs = format!("{DEP_HASH_B}-norefs");
        // dep-A appears as an RPATH-style full path; dep-B is in the
        // closure but NOT in any output — must not be over-reported.
        fs::write(
            store_dir.join(&with_refs),
            format!("RPATH={dep_a}/lib\nself={self_path}\n"),
        )?;
        fs::write(store_dir.join(&no_refs), b"plain text, no store paths here")?;

        let deriver = test_drv_path("scanned");
        let closure = vec![dep_a.clone(), dep_b.clone()];
        let results = upload_all_outputs(&clients, &store_dir, "", &deriver, &closure).await?;
        assert_eq!(results.len(), 2);

        let calls = store.calls.put_chunked_calls.read().unwrap();
        let begin = &calls[0].begin;
        assert_eq!(
            begin.deriver, deriver,
            "deriver delivered in the Begin frame"
        );
        assert_eq!(begin.input_closure, closure, "closure echoed verbatim");

        let by_path = |p: &str| {
            begin
                .outputs
                .iter()
                .find(|o| o.store_path == p)
                .unwrap_or_else(|| panic!("output {p} in Begin"))
        };
        assert_eq!(
            by_path(&self_path).references,
            vec![dep_a.clone(), self_path.clone()],
            "scanned refs: dep-A + self, sorted, no dep-B"
        );
        assert!(
            by_path(&format!("/nix/store/{no_refs}"))
                .references
                .is_empty(),
            "output with no embedded paths claims no references"
        );
        // The result infos carry the same references the store was told.
        let r = results
            .iter()
            .find(|r| r.store_path.as_str() == self_path)
            .expect("result for the ref-carrying output");
        let refs: Vec<String> = r.references.iter().map(|p| p.to_string()).collect();
        assert_eq!(refs, vec![dep_a, self_path]);

        assert!(
            recorder.histogram_touched("rio_builder_upload_references_count"),
            "references-count histogram must be recorded on the chunked path"
        );
        Ok(())
    }

    /// Cross-output chunk dedupe: a chunk that is already durable in the
    /// store (HasChunks = true) is excluded from `novel` and never
    /// streamed; shared-but-novel chunks are streamed exactly once, in
    /// global first-occurrence order.
    #[tokio::test]
    async fn test_upload_chunked_dedupes_durable_chunks() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        let durable_content = b"this exact blob is already durable in the store".to_vec();
        let shared_content = b"this blob is shared between both outputs but novel".to_vec();
        let b1 = format!("{DEP_HASH_A}-first");
        let b2 = format!("{DEP_HASH_B}-second");
        // Single-file outputs: each output is one chunk.
        fs::create_dir(store_dir.join(&b1))?;
        fs::write(store_dir.join(&b1).join("durable"), &durable_content)?;
        fs::write(store_dir.join(&b1).join("shared"), &shared_content)?;
        fs::create_dir(store_dir.join(&b2))?;
        fs::write(store_dir.join(&b2).join("shared"), &shared_content)?;
        fs::write(
            store_dir.join(&b2).join("unique"),
            b"only in the second output",
        )?;

        // Pre-seed the durable chunk so HasChunks reports it present.
        let durable_digest = *blake3::hash(&durable_content).as_bytes();
        store
            .state
            .chunks
            .write()
            .unwrap()
            .insert(durable_digest.to_vec(), durable_content.clone());

        upload_all_outputs(&clients, &store_dir, "", "", &[]).await?;

        let calls = store.calls.put_chunked_calls.read().unwrap();
        let call = &calls[0];
        let shared_digest = blake3::hash(&shared_content).as_bytes().to_vec();
        let unique_digest = blake3::hash(b"only in the second output")
            .as_bytes()
            .to_vec();
        // Walk order in b1 is byte-lex: durable, shared. Global
        // first-occurrence order of NOVEL digests: shared (b1), unique (b2).
        assert_eq!(
            call.begin.novel,
            vec![shared_digest.clone(), unique_digest.clone()],
            "novel excludes the durable chunk and follows first-occurrence order"
        );
        assert_eq!(
            call.chunk_digests,
            vec![shared_digest, unique_digest],
            "exactly the novel chunks were streamed, in novel order"
        );
        assert!(
            !call.chunk_digests.contains(&durable_digest.to_vec()),
            "durable chunk bytes never re-sent"
        );
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Idempotent pre-check (FindMissingPaths → skip already-present outputs)
    // -----------------------------------------------------------------------

    /// r[verify builder.upload.idempotent-precheck+2]
    ///
    /// Every output already in store → zero PutPathChunked calls, result
    /// carries the STORE's nar_hash (not a freshly-computed one).
    ///
    /// Disk contents are deliberately DIFFERENT from what's seeded in the
    /// store: the test asserts the returned nar_hash matches the SEEDED
    /// hash, NOT the on-disk NAR's hash. Proves we queried the store
    /// instead of reading disk (the optimization's whole point).
    #[tokio::test]
    async fn test_upload_all_outputs_skips_already_present() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
        let basename = format!("{DEP_HASH_A}-already-there");
        let store_path = format!("/nix/store/{basename}");

        let (_seeded_nar, seeded_hash) = store.seed_with_content(&store_path, b"seeded content");

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

        let results = upload_all_outputs(&clients, &store_dir, "", "", &[]).await?;

        assert_eq!(
            store.calls.put_chunked_calls.read().unwrap().len(),
            0,
            "pre-check should skip already-present path; zero PutPathChunked calls"
        );
        assert_eq!(results.len(), 1);
        assert_eq!(results[0].store_path.as_str(), store_path);
        assert_eq!(
            results[0].nar_hash, seeded_hash,
            "skipped path's result carries the store's nar_hash, not disk's"
        );
        Ok(())
    }

    /// Mixed presence: one output already in the store, one missing.
    /// The upload must NOT drop the present sibling from the Begin frame
    /// (`r[builder.upload.idempotent-precheck+2]` — candidate-set
    /// rationale at the collect_outputs call site); both outputs appear
    /// in the results.
    #[tokio::test]
    async fn test_upload_all_outputs_mixed_presence_sends_full_set() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;

        let b_present = format!("{DEP_HASH_A}-present");
        let b_missing = format!("{DEP_HASH_B}-missing");
        let path_present = format!("/nix/store/{b_present}");
        let path_missing = format!("/nix/store/{b_missing}");

        store.seed_with_content(&path_present, b"already here");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&b_present), b"already here")?;
        // The missing output references its present sibling — the exact
        // shape that breaks if the present output were skipped.
        fs::write(store_dir.join(&b_missing), format!("link={path_present}\n"))?;

        let results = upload_all_outputs(&clients, &store_dir, "", "", &[]).await?;

        let calls = store.calls.put_chunked_calls.read().unwrap();
        assert_eq!(calls.len(), 1);
        let begin_paths: Vec<&str> = calls[0]
            .begin
            .outputs
            .iter()
            .map(|o| o.store_path.as_str())
            .collect();
        assert_eq!(
            begin_paths,
            vec![path_present.as_str(), path_missing.as_str()],
            "both outputs ride the Begin frame even though one is already present"
        );
        // The missing output's claimed reference to its sibling survived.
        assert_eq!(
            calls[0].begin.outputs[1].references,
            vec![path_present.clone()]
        );

        assert_eq!(results.len(), 2);
        let paths: std::collections::HashSet<_> =
            results.iter().map(|r| r.store_path.to_string()).collect();
        assert!(paths.contains(&path_present) && paths.contains(&path_missing));
        Ok(())
    }

    /// FindMissingPaths errors → fall back to upload-all. Best-effort:
    /// store transient doesn't break the upload; the store's idempotent
    /// PutPathChunked catches duplicates server-side.
    #[tokio::test]
    async fn test_upload_all_outputs_find_missing_error_falls_back() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
        store.faults.fail_find_missing.store(true, Ordering::SeqCst);

        let basename = format!("{DEP_HASH_A}-fallback");
        let store_path = format!("/nix/store/{basename}");
        // Seed the path — WOULD be skipped if FindMissingPaths worked.
        store.seed_with_content(&store_path, b"seeded");

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(&basename), b"disk fallback")?;

        let results = upload_all_outputs(&clients, &store_dir, "", "", &[]).await?;

        assert_eq!(
            store.calls.put_chunked_calls.read().unwrap().len(),
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

    /// Empty overlay upper → early return, no RPC at all.
    #[tokio::test]
    async fn test_upload_all_outputs_empty_no_rpc() -> anyhow::Result<()> {
        let (store, clients, _h) = spawn_clients().await?;
        store.faults.fail_find_missing.store(true, Ordering::SeqCst);
        let tmp = tempfile::tempdir()?;
        // upper_store doesn't exist — scan_new_outputs returns empty.

        let results =
            upload_all_outputs(&clients, &tmp.path().join("nonexistent"), "", "", &[]).await?;
        assert!(results.is_empty());
        assert_eq!(store.calls.put_chunked_calls.read().unwrap().len(), 0);
        assert_eq!(store.calls.find_missing_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Atomicity / retry / liveness
    // -----------------------------------------------------------------------

    // r[verify builder.upload.batch+3]
    // r[verify store.atomic.multi-output]
    /// A walk failure on output k must NOT leave outputs 0..k-1
    /// committed server-side: every output is walked BEFORE the stream
    /// opens, so the store sees zero PutPathChunked calls. The failing
    /// output contains a FIFO (`mkfifo $out/pipe` — the canonical
    /// malicious-output shape), which the NAR walk rejects as an
    /// unsupported file type regardless of which uid runs the test.
    #[tokio::test(start_paused = true)]
    async fn test_walk_failure_no_partial_commit() -> anyhow::Result<()> {
        let (store, clients) = spawn_clients_inproc().await?;
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;

        let b_ok = format!("{DEP_HASH_A}-ok");
        let b_bad = format!("{DEP_HASH_B}-has-a-fifo");
        fs::write(store_dir.join(&b_ok), b"ok contents")?;
        let bad_dir = store_dir.join(&b_bad);
        fs::create_dir(&bad_dir)?;
        nix::unistd::mkfifo(&bad_dir.join("pipe"), nix::sys::stat::Mode::S_IRWXU)?;

        let err = upload_all_outputs(&clients, &store_dir, "", "", &[])
            .await
            .expect_err("walk must fail on the unsupported file type");
        assert!(matches!(err, UploadError::UploadRejected { .. }));

        assert_eq!(
            store.calls.put_chunked_calls.read().unwrap().len(),
            0,
            "walk failure must not partially commit — zero PutPathChunked calls"
        );
        Ok(())
    }

    /// Transient store errors are retried with the same budget as the
    /// legacy path. start_paused auto-advances the backoff sleeps.
    #[tokio::test(start_paused = true)]
    async fn test_upload_chunked_retries_transient_then_succeeds() -> anyhow::Result<()> {
        let (store, clients) = spawn_clients_inproc().await?;
        // First two PutPathChunked RPCs return Unavailable; third succeeds.
        store.faults.fail_next_puts.store(2, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        let (b1, b2) = (format!("{DEP_HASH_A}-one"), format!("{DEP_HASH_B}-two"));
        fs::write(store_dir.join(&b1), b"one")?;
        fs::write(store_dir.join(&b2), b"two")?;

        let results = upload_all_outputs(&clients, &store_dir, "", "", &[])
            .await
            .expect("upload should succeed on the 3rd attempt");

        assert_eq!(results.len(), 2);
        assert_eq!(
            store.calls.put_chunked_calls.read().unwrap().len(),
            1,
            "only the successful attempt is recorded"
        );
        assert_eq!(
            store.faults.fail_next_puts.load(Ordering::SeqCst),
            0,
            "all injected failures consumed"
        );
        Ok(())
    }

    /// A deterministic store rejection (FAILED_PRECONDITION — e.g. the
    /// real store's "requires a chunk backend" gate on an inline-only
    /// deployment) must fail the upload after exactly ONE attempt and
    /// surface the store's status, not burn the transient-retry budget
    /// re-sending an identical request.
    #[tokio::test(start_paused = true)]
    async fn test_upload_chunked_terminal_rejection_not_retried() -> anyhow::Result<()> {
        let (store, clients) = spawn_clients_inproc().await?;
        // More injected rejections than the retry budget: the count
        // remaining afterwards is the structural attempt counter.
        store
            .faults
            .reject_next_chunked_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(format!("{DEP_HASH_A}-rejected")), b"bytes")?;

        let err = upload_all_outputs(&clients, &store_dir, "", "", &[])
            .await
            .expect_err("terminal rejection must fail the upload");
        // Triage regression: a one-attempt deterministic rejection must
        // not present itself as retry exhaustion ("after 8 retries").
        let msg = err.to_string();
        assert!(
            msg.contains("not retried") && !msg.contains("upload failed after"),
            "terminal rejection must not read as retry exhaustion: {msg}"
        );
        let UploadError::UploadRejected { source, .. } = err else {
            panic!("expected UploadRejected, got {err:?}");
        };
        assert_eq!(source.code(), tonic::Code::FailedPrecondition);
        assert!(
            source.message().contains("chunk backend"),
            "store's rejection message must survive to the caller: {source}"
        );
        assert_eq!(
            store.faults.reject_next_chunked_puts.load(Ordering::SeqCst),
            MAX_UPLOAD_RETRIES,
            "exactly one PutPathChunked attempt — deterministic rejections are not retried"
        );
        assert_eq!(store.calls.put_chunked_calls.read().unwrap().len(), 0);
        Ok(())
    }

    /// Retries exhaust → UploadExhausted, nothing committed.
    #[tokio::test(start_paused = true)]
    async fn test_upload_chunked_exhausts_retries() -> anyhow::Result<()> {
        let (store, clients) = spawn_clients_inproc().await?;
        store
            .faults
            .fail_next_puts
            .store(MAX_UPLOAD_RETRIES + 1, Ordering::SeqCst);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        fs::create_dir_all(&store_dir)?;
        fs::write(store_dir.join(format!("{DEP_HASH_A}-a")), b"a")?;

        let err = upload_all_outputs(&clients, &store_dir, "", "", &[])
            .await
            .expect_err("upload should exhaust retries");
        assert!(matches!(err, UploadError::UploadExhausted { .. }));
        // chaos.nix detects retry exhaustion by grepping journald for
        // this exact substring; keep Display and that grep in sync.
        assert!(
            err.to_string().contains("upload failed after"),
            "exhaustion Display must keep the chaos.nix grep substring: {err}"
        );
        assert_eq!(store.calls.put_chunked_calls.read().unwrap().len(), 0);
        Ok(())
    }

    // -----------------------------------------------------------------------
    // Bounded joins for spawn_blocking disk work
    // -----------------------------------------------------------------------

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
}
