//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.

use std::path::Path;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use tonic::transport::Channel;

use rio_nix::derivation::Derivation;
use rio_proto::store::store_service_client::StoreServiceClient;

use crate::synth_db::SynthPathInfo;
use crate::upload;

use super::{ExecutorError, MAX_PARALLEL_FETCHES};

/// Verify FOD output hashes match the declared outputHash (defense-in-depth;
/// nix-daemon also verifies, but we re-check before accepting).
///
/// For `r:sha256` (recursive): compare the upload's NAR hash against outputHash.
/// For `sha256` (flat): read the file from the overlay upper layer, hash its
/// contents directly, and compare.
pub(super) fn verify_fod_hashes(
    drv: &Derivation,
    uploads: &[upload::UploadResult],
    overlay_upper: &Path,
) -> anyhow::Result<()> {
    use anyhow::{Context, bail};
    use sha2::{Digest, Sha256};

    for output in drv.outputs() {
        // Only FOD outputs have a declared hash
        if output.hash().is_empty() {
            continue;
        }

        let expected = hex::decode(output.hash())
            .with_context(|| format!("FOD outputHash is not valid hex: {}", output.hash()))?;
        let is_recursive = output.hash_algo().starts_with("r:");

        if is_recursive {
            // NAR hash — match against upload result
            let upload = uploads
                .iter()
                .find(|u| u.store_path == output.path())
                .with_context(|| format!("FOD output '{}' not found in uploads", output.name()))?;
            if upload.nar_hash != expected {
                bail!(
                    "FOD NAR hash mismatch for '{}': expected {}, got {}",
                    output.name(),
                    output.hash(),
                    hex::encode(&upload.nar_hash)
                );
            }
        } else {
            // Flat hash — read file from overlay upper and hash contents
            let store_basename = output
                .path()
                .strip_prefix("/nix/store/")
                .with_context(|| format!("invalid output path: {}", output.path()))?;
            let file_path = overlay_upper.join("nix/store").join(store_basename);
            let content = std::fs::read(&file_path).with_context(|| {
                format!("failed to read FOD output file {}", file_path.display())
            })?;
            let computed: [u8; 32] = Sha256::digest(&content).into();
            if computed.as_slice() != expected {
                bail!(
                    "FOD flat hash mismatch for '{}': expected {}, got {}",
                    output.name(),
                    output.hash(),
                    hex::encode(computed)
                );
            }
        }
    }
    Ok(())
}

/// Fetch metadata for all input paths from the store.
pub(super) async fn fetch_input_metadata(
    store_client: &StoreServiceClient<Channel>,
    input_paths: &[String],
) -> Result<Vec<SynthPathInfo>, ExecutorError> {
    stream::iter(input_paths.iter().cloned())
        .map(|path| {
            let mut client = store_client.clone();
            async move {
                match rio_proto::client::query_path_info_opt(
                    &mut client,
                    &path,
                    rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                )
                .await
                {
                    Ok(Some(info)) => Ok(SynthPathInfo::from(info)),
                    Ok(None) | Err(_) => {
                        tracing::warn!(
                            path = %path,
                            "failed to fetch input path metadata (not found or error)"
                        );
                        Err(ExecutorError::MetadataFetch {
                            path,
                            source: tonic::Status::not_found("path missing from store"),
                        })
                    }
                }
            }
        })
        // buffered (not unordered): preserves order, negligible cost for
        // defensive compatibility with synth_db::generate_db.
        .buffered(MAX_PARALLEL_FETCHES)
        .try_collect()
        .await
}

/// Fetch a .drv file from the store and parse it.
///
/// Used when the scheduler sends `drv_content: empty` (Phase 2a default).
/// The .drv is a single regular file in the store, so we fetch its NAR and
/// extract the ATerm content via `extract_single_file`.
pub(super) async fn fetch_drv_from_store(
    store_client: &mut StoreServiceClient<Channel>,
    drv_path: &str,
) -> Result<Derivation, ExecutorError> {
    // .drv files are small (KB range), but wrap in stream timeout: this is
    // the first gRPC call after setup_overlay, so a stalled store would hang
    // the build with an overlay mount held indefinitely.
    let result = rio_proto::client::get_path_nar(
        store_client,
        drv_path,
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        rio_common::limits::MAX_NAR_SIZE,
    )
    .await
    .map_err(|e| ExecutorError::BuildFailed(format!("GetPath({drv_path}): {e}")))?;

    let Some((_, nar_data)) = result else {
        return Err(ExecutorError::BuildFailed(format!(
            ".drv not found in store: {drv_path}"
        )));
    };

    Derivation::parse_from_nar(&nar_data)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse .drv from NAR: {e}")))
}

/// Compute the input closure for a derivation by querying the store.
///
/// The input closure consists of:
///   - The .drv file itself (nix-daemon reads it)
///   - All `input_srcs` (source store paths)
///   - All outputs of all `input_drvs` (dependency outputs)
///   - Transitively: all references of the above
///
/// We bootstrap from the .drv's own references (which the store computes at
/// upload time from the NAR content) and walk the reference graph via
/// QueryPathInfo. Paths not yet in the store (e.g., outputs of not-yet-built
/// input drvs) are skipped — FUSE will lazy-fetch them at build time.
pub(super) async fn compute_input_closure(
    store_client: &StoreServiceClient<Channel>,
    drv: &Derivation,
    drv_path: &str,
) -> Result<Vec<String>, ExecutorError> {
    use std::collections::HashSet;

    let mut closure: HashSet<String> = HashSet::new();
    let mut frontier: Vec<String> = Vec::new();

    // Seed: the .drv itself, its input_srcs, and input_drv paths.
    // nix-daemon needs to read the .drv; build needs srcs + dep outputs.
    frontier.push(drv_path.to_string());
    frontier.extend(drv.input_srcs().iter().cloned());
    frontier.extend(drv.input_drvs().keys().cloned());

    // BFS by layer. Within each layer all queries are independent, so
    // buffer_unordered. Layer count is typically 5-15 (dep depth).
    while !frontier.is_empty() {
        // Dedupe against closure BEFORE issuing RPCs.
        let batch: Vec<String> = std::mem::take(&mut frontier)
            .into_iter()
            .filter(|p| !closure.contains(p))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if batch.is_empty() {
            break;
        }

        // Fetch this layer concurrently. Each result is
        // (path, Option<references>); None means NotFound.
        // References are Vec<StorePath> now (ValidatedPathInfo); convert
        // to String here since `closure` is a HashSet<String> (closure
        // membership is checked against string keys from the .drv parse).
        let results: Vec<(String, Option<Vec<String>>)> = stream::iter(batch)
            .map(|path| {
                let mut client = store_client.clone();
                async move {
                    match rio_proto::client::query_path_info_opt(
                        &mut client,
                        &path,
                        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                    )
                    .await
                    {
                        Ok(Some(info)) => {
                            let refs = info.references.iter().map(|r| r.to_string()).collect();
                            Ok((path, Some(refs)))
                        }
                        Ok(None) => {
                            // Path not in store yet (output of a not-yet-built
                            // input drv). FUSE will lazy-fetch at build time.
                            tracing::debug!(path = %path, "input not in store; FUSE will lazy-fetch");
                            Ok((path, None))
                        }
                        Err(e) => Err(ExecutorError::MetadataFetch {
                            path: path.clone(),
                            source: e,
                        }),
                    }
                }
            })
            .buffer_unordered(MAX_PARALLEL_FETCHES)
            .try_collect()
            .await?;

        // Add found paths to closure, collect their refs for next layer.
        for (path, refs) in results {
            if let Some(references) = refs {
                closure.insert(path);
                for r in references {
                    if !closure.contains(&r) {
                        frontier.push(r);
                    }
                }
            }
            // NotFound: do NOT add to closure (skip it entirely).
        }
    }

    Ok(closure.into_iter().collect())
}

#[cfg(test)]
mod tests {
    use super::*;

    // -----------------------------------------------------------------------
    // fetch_drv_from_store NAR extraction
    // -----------------------------------------------------------------------

    /// Verify the NAR extraction + ATerm parsing pipeline works end-to-end.
    /// This is the core of fetch_drv_from_store (minus the gRPC transport,
    /// which is straightforward streaming).
    #[test]
    fn test_nar_wrapped_drv_parseable() -> anyhow::Result<()> {
        // Minimal valid ATerm derivation (no inputs, one output).
        let drv_text = r#"Derive([("out","/nix/store/00000000000000000000000000000000-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/00000000000000000000000000000000-test")])"#;

        // Wrap in NAR as a single regular file (same as a .drv in the store).
        let nar_node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: drv_text.as_bytes().to_vec(),
        };
        let mut nar_bytes = Vec::new();
        rio_nix::nar::serialize(&mut nar_bytes, &nar_node)?;

        // Extract + parse (the tail of fetch_drv_from_store).
        let extracted =
            rio_nix::nar::extract_single_file(&nar_bytes).expect("should extract single-file NAR");
        let text = String::from_utf8(extracted).expect("should be UTF-8");
        let drv = Derivation::parse(&text).expect("should parse as ATerm");

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
        Ok(())
    }

    /// Empty NAR data should produce a clear error (not silent success or panic).
    #[test]
    fn test_empty_nar_rejected() {
        let result = rio_nix::nar::extract_single_file(&[]);
        assert!(result.is_err(), "empty NAR should fail extraction");
    }

    // -----------------------------------------------------------------------
    // Group 6: FOD output hash verification
    // -----------------------------------------------------------------------

    fn make_fod_drv(
        output_path: &str,
        hash_algo: &str,
        hash_hex: &str,
    ) -> rio_nix::derivation::Derivation {
        // Derivation has no public constructor; parse a minimal ATerm.
        let aterm = format!(
            r#"Derive([("out","{output_path}","{hash_algo}","{hash_hex}")],[],[],"x86_64-linux","/bin/sh",[],[("out","{output_path}")])"#
        );
        rio_nix::derivation::Derivation::parse(&aterm)
            .unwrap_or_else(|e| panic!("invalid test ATerm: {e} -- ATerm was: {aterm}"))
    }

    #[test]
    fn test_verify_fod_output_hash_recursive_ok() -> anyhow::Result<()> {
        // r:sha256 — NAR hash comparison
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: hex::decode(expected_hash)?,
            nar_size: 100,
        };

        let tmp = tempfile::tempdir()?;
        assert!(verify_fod_hashes(&drv, &[upload], tmp.path()).is_ok());
        Ok(())
    }

    #[test]
    fn test_verify_fod_output_hash_recursive_mismatch() -> anyhow::Result<()> {
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        // Upload has DIFFERENT hash
        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: vec![0u8; 32], // all zeros, != expected
            nar_size: 100,
        };

        let tmp = tempfile::tempdir()?;
        let result = verify_fod_hashes(&drv, &[upload], tmp.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("mismatch"),
            "error should mention hash mismatch"
        );
        Ok(())
    }

    #[test]
    fn test_verify_fod_output_hash_flat_ok() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        // Flat sha256 — file content hash
        let content = b"hello world flat fod content";
        let expected_hash_bytes: [u8; 32] = Sha256::digest(content).into();
        let expected_hash = hex::encode(expected_hash_bytes);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &expected_hash);

        // Write the file to overlay/nix/store/test-flat-fod
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir)?;
        std::fs::write(store_dir.join("test-flat-fod"), content)?;

        // Uploads not used for flat hash verification
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
        Ok(())
    }

    #[test]
    fn test_verify_fod_output_hash_flat_mismatch() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};

        let content = b"actual content";
        let wrong_hash: [u8; 32] = Sha256::digest(b"different content").into();
        let wrong_hash_hex = hex::encode(wrong_hash);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &wrong_hash_hex);

        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir)?;
        std::fs::write(store_dir.join("test-flat-fod"), content)?;

        let result = verify_fod_hashes(&drv, &[], tmp.path());
        assert!(result.is_err());
        Ok(())
    }

    #[test]
    fn test_verify_fod_non_fod_skipped() -> anyhow::Result<()> {
        // Non-FOD (no hash) should be skipped without error
        let drv = make_fod_drv("/nix/store/test-non-fod", "", "");
        let tmp = tempfile::tempdir()?;
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // gRPC fetch tests via MockStore
    // -----------------------------------------------------------------------

    use rio_test_support::fixtures::{make_nar, make_path_info, test_store_path};
    use rio_test_support::grpc::{MockStore, spawn_mock_store};

    /// Shorthand for test_store_path — these tests use many paths.
    fn tp(name: &str) -> String {
        test_store_path(name)
    }

    async fn spawn_and_connect() -> anyhow::Result<(MockStore, StoreServiceClient<Channel>)> {
        let (store, addr, _h) = spawn_mock_store().await?;
        let client = rio_proto::client::connect_store(&addr.to_string()).await?;
        Ok((store, client))
    }

    /// Seed a path with the given reference tags. Content is arbitrary;
    /// PathInfo.references is what compute_input_closure walks.
    /// `path` and each `ref` must be a VALID store path (use `tp()`).
    fn seed_with_refs(store: &MockStore, path: &str, refs: &[String]) {
        let (nar, hash) = make_nar(b"content");
        let mut info = make_path_info(path, &nar, hash);
        info.references = refs
            .iter()
            .map(|s| {
                rio_nix::store_path::StorePath::parse(s)
                    .unwrap_or_else(|e| panic!("test ref {s:?} invalid: {e}"))
            })
            .collect();
        store.seed(info, nar);
    }

    /// Build a Derivation with the given input_srcs via ATerm parsing
    /// (Derivation has no public constructor).
    fn drv_with_srcs(srcs: &[String]) -> Derivation {
        let srcs_quoted: Vec<String> = srcs.iter().map(|s| format!(r#""{s}""#)).collect();
        let out = tp("test-out");
        let aterm = format!(
            r#"Derive([("out","{out}","","")],[],[{}],"x86_64-linux","/bin/sh",[],[("out","{out}")])"#,
            srcs_quoted.join(",")
        );
        Derivation::parse(&aterm).unwrap_or_else(|e| panic!("bad ATerm: {e}\n{aterm}"))
    }

    #[tokio::test]
    async fn test_fetch_input_metadata_success() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_foo, p_bar) = (tp("foo"), tp("bar"));
        seed_with_refs(&store, &p_foo, &[]);
        seed_with_refs(&store, &p_bar, &[]);

        let result = fetch_input_metadata(&client, &[p_foo.clone(), p_bar.clone()])
            .await
            .expect("fetch should succeed");

        assert_eq!(result.len(), 2);
        // fetch_input_metadata uses buffered (not unordered) → order preserved.
        assert_eq!(result[0].path, p_foo);
        assert_eq!(result[1].path, p_bar);
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_input_metadata_missing_path_errors() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_present, p_missing) = (tp("present"), tp("missing"));
        seed_with_refs(&store, &p_present, &[]);
        // p_missing is NOT seeded.

        let err = fetch_input_metadata(&client, &[p_present, p_missing.clone()])
            .await
            .expect_err("should error on missing path");

        match err {
            ExecutorError::MetadataFetch { path, .. } => {
                assert_eq!(path, p_missing);
            }
            other => panic!("expected MetadataFetch, got {other:?}"),
        }
        Ok(())
    }

    #[tokio::test]
    async fn test_compute_input_closure_bfs() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_drv, p_a, p_b, p_c) = (tp("test.drv"), tp("lib"), tp("dep"), tp("leaf"));
        // Chain: drv → A → B → C
        seed_with_refs(&store, &p_drv, std::slice::from_ref(&p_a));
        seed_with_refs(&store, &p_a, std::slice::from_ref(&p_b));
        seed_with_refs(&store, &p_b, std::slice::from_ref(&p_c));
        seed_with_refs(&store, &p_c, &[]);

        let drv = drv_with_srcs(std::slice::from_ref(&p_a));
        let closure = compute_input_closure(&client, &drv, &p_drv)
            .await
            .expect("closure computation should succeed");

        let set: std::collections::HashSet<String> = closure.into_iter().collect();
        assert_eq!(set.len(), 4);
        assert!(set.contains(&p_drv));
        assert!(set.contains(&p_a));
        assert!(set.contains(&p_b));
        assert!(set.contains(&p_c));
        Ok(())
    }

    /// A referenced path not in the store is skipped (not an error).
    /// FUSE will lazy-fetch it at build time.
    #[tokio::test]
    async fn test_compute_input_closure_skips_notfound() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_drv, p_a, p_missing) = (tp("test.drv"), tp("lib"), tp("missing"));
        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_a, std::slice::from_ref(&p_missing));
        // p_missing is NOT seeded.

        let drv = drv_with_srcs(std::slice::from_ref(&p_a));
        let closure = compute_input_closure(&client, &drv, &p_drv)
            .await
            .expect("missing ref is non-fatal");

        let set: std::collections::HashSet<String> = closure.into_iter().collect();
        assert_eq!(set.len(), 2, "closure should be {{drv, A}} without B");
        assert!(set.contains(&p_drv));
        assert!(set.contains(&p_a));
        assert!(!set.contains(&p_missing));
        Ok(())
    }

    /// Diamond: A→C, B→C. C must appear once (set semantics + BFS dedup).
    #[tokio::test]
    async fn test_compute_input_closure_dedupes_diamond() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_drv, p_a, p_b, p_c) = (tp("test.drv"), tp("left"), tp("right"), tp("shared"));
        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_a, std::slice::from_ref(&p_c));
        seed_with_refs(&store, &p_b, std::slice::from_ref(&p_c));
        seed_with_refs(&store, &p_c, &[]);

        let drv = drv_with_srcs(&[p_a, p_b]);
        let closure = compute_input_closure(&client, &drv, &p_drv).await?;

        assert_eq!(closure.len(), 4); // drv, A, B, C (once)
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_drv_from_store_success() -> anyhow::Result<()> {
        let (store, mut client) = spawn_and_connect().await?;
        // NAR-wrap a minimal ATerm as a single regular file.
        let out = tp("test-out");
        let drv_text = format!(
            r#"Derive([("out","{out}","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","{out}")])"#
        );
        let (nar, hash) = make_nar(drv_text.as_bytes());
        let drv_path = tp("test.drv");
        store.seed(make_path_info(&drv_path, &nar, hash), nar);

        let drv = fetch_drv_from_store(&mut client, &drv_path)
            .await
            .expect("fetch + parse should succeed");

        assert_eq!(drv.platform(), "x86_64-linux");
        assert_eq!(drv.outputs().len(), 1);
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_drv_from_store_not_found() -> anyhow::Result<()> {
        let (_store, mut client) = spawn_and_connect().await?;

        let missing = tp("nonexistent.drv");
        let err = fetch_drv_from_store(&mut client, &missing)
            .await
            .expect_err("should fail on missing .drv");

        assert!(matches!(err, ExecutorError::BuildFailed(_)));
        assert!(err.to_string().contains("nonexistent.drv"));
        Ok(())
    }

    #[tokio::test]
    async fn test_fetch_drv_from_store_bad_nar() -> anyhow::Result<()> {
        let (store, mut client) = spawn_and_connect().await?;
        // Seed garbage — not a valid NAR.
        let garbage = b"this is definitely not a NAR archive".to_vec();
        let drv_path = tp("bad.drv");
        store.seed(make_path_info(&drv_path, &garbage, [0u8; 32]), garbage);

        let err = fetch_drv_from_store(&mut client, &drv_path)
            .await
            .expect_err("should fail on bad NAR");

        assert!(matches!(err, ExecutorError::BuildFailed(_)));
        assert!(
            err.to_string().contains("failed to parse .drv from NAR"),
            "got: {err}"
        );
        Ok(())
    }
}
