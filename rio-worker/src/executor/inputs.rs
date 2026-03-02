//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.

use std::path::Path;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use tonic::transport::Channel;

use rio_nix::derivation::Derivation;
use rio_proto::store::store_service_client::StoreServiceClient;

use crate::synth_db::{SynthPathInfo, path_info_to_synth};
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
                    Ok(Some(info)) => Ok(path_info_to_synth(&info)),
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

    let nar_data = match result {
        Some((_, nar)) => nar,
        None => {
            return Err(ExecutorError::BuildFailed(format!(
                ".drv not found in store: {drv_path}"
            )));
        }
    };

    let drv_bytes = rio_nix::nar::extract_single_file(&nar_data)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to extract .drv from NAR: {e}")))?;

    let drv_text = String::from_utf8(drv_bytes)
        .map_err(|e| ExecutorError::BuildFailed(format!(".drv is not valid UTF-8: {e}")))?;

    Derivation::parse(&drv_text)
        .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse derivation: {e}")))
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
        let batch: Vec<String> = frontier
            .drain(..)
            .filter(|p| !closure.contains(p))
            .collect::<HashSet<_>>()
            .into_iter()
            .collect();
        if batch.is_empty() {
            break;
        }

        // Fetch this layer concurrently. Each result is
        // (path, Option<references>); None means NotFound.
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
                        Ok(Some(info)) => Ok((path, Some(info.references))),
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
    fn test_nar_wrapped_drv_parseable() {
        // Minimal valid ATerm derivation (no inputs, one output).
        let drv_text = r#"Derive([("out","/nix/store/00000000000000000000000000000000-test","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","/nix/store/00000000000000000000000000000000-test")])"#;

        // Wrap in NAR as a single regular file (same as a .drv in the store).
        let nar_node = rio_nix::nar::NarNode::Regular {
            executable: false,
            contents: drv_text.as_bytes().to_vec(),
        };
        let mut nar_bytes = Vec::new();
        rio_nix::nar::serialize(&mut nar_bytes, &nar_node).unwrap();

        // Extract + parse (the tail of fetch_drv_from_store).
        let extracted =
            rio_nix::nar::extract_single_file(&nar_bytes).expect("should extract single-file NAR");
        let text = String::from_utf8(extracted).expect("should be UTF-8");
        let drv = Derivation::parse(&text).expect("should parse as ATerm");

        assert_eq!(drv.outputs().len(), 1);
        assert_eq!(drv.outputs()[0].name(), "out");
        assert_eq!(drv.platform(), "x86_64-linux");
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
    fn test_verify_fod_output_hash_recursive_ok() {
        // r:sha256 — NAR hash comparison
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: hex::decode(expected_hash).unwrap(),
            nar_size: 100,
        };

        let tmp = tempfile::tempdir().unwrap();
        assert!(verify_fod_hashes(&drv, &[upload], tmp.path()).is_ok());
    }

    #[test]
    fn test_verify_fod_output_hash_recursive_mismatch() {
        let expected_hash = "abcdef0123456789abcdef0123456789abcdef0123456789abcdef0123456789";
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", expected_hash);

        // Upload has DIFFERENT hash
        let upload = upload::UploadResult {
            store_path: "/nix/store/test-fod".into(),
            nar_hash: vec![0u8; 32], // all zeros, != expected
            nar_size: 100,
        };

        let tmp = tempfile::tempdir().unwrap();
        let result = verify_fod_hashes(&drv, &[upload], tmp.path());
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("mismatch"),
            "error should mention hash mismatch"
        );
    }

    #[test]
    fn test_verify_fod_output_hash_flat_ok() {
        use sha2::{Digest, Sha256};

        // Flat sha256 — file content hash
        let content = b"hello world flat fod content";
        let expected_hash_bytes: [u8; 32] = Sha256::digest(content).into();
        let expected_hash = hex::encode(expected_hash_bytes);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &expected_hash);

        // Write the file to overlay/nix/store/test-flat-fod
        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("test-flat-fod"), content).unwrap();

        // Uploads not used for flat hash verification
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
    }

    #[test]
    fn test_verify_fod_output_hash_flat_mismatch() {
        use sha2::{Digest, Sha256};

        let content = b"actual content";
        let wrong_hash: [u8; 32] = Sha256::digest(b"different content").into();
        let wrong_hash_hex = hex::encode(wrong_hash);

        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &wrong_hash_hex);

        let tmp = tempfile::tempdir().unwrap();
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir).unwrap();
        std::fs::write(store_dir.join("test-flat-fod"), content).unwrap();

        let result = verify_fod_hashes(&drv, &[], tmp.path());
        assert!(result.is_err());
    }

    #[test]
    fn test_verify_fod_non_fod_skipped() {
        // Non-FOD (no hash) should be skipped without error
        let drv = make_fod_drv("/nix/store/test-non-fod", "", "");
        let tmp = tempfile::tempdir().unwrap();
        assert!(verify_fod_hashes(&drv, &[], tmp.path()).is_ok());
    }
}
