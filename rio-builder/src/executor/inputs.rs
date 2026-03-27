//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.
// r[impl worker.fod.verify-hash]

use std::path::Path;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::derivation::Derivation;
use rio_proto::StoreServiceClient;

use crate::synth_db::SynthPathInfo;

use super::{ExecutorError, MAX_PARALLEL_FETCHES};

/// Hash algorithm for FOD output verification. Maps from Nix's
/// `outputHashAlgo` string (sha1, sha256, sha512; recursive variants
/// prefixed "r:").
#[derive(Debug, Clone, Copy)]
enum FodHashAlgo {
    Sha1,
    Sha256,
    Sha512,
}

impl FodHashAlgo {
    /// Parse from Nix's outputHashAlgo. Strips the "r:" recursive
    /// prefix (the prefix determines hash MODE not ALGO).
    ///
    /// Returns None for unknown algos — caller should log+skip rather
    /// than false-reject a valid output whose algo we don't support.
    fn from_nix_str(s: &str) -> Option<Self> {
        match s.strip_prefix("r:").unwrap_or(s) {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// Digest a byte slice with this algo. Returns the raw digest.
    fn digest(self, data: &[u8]) -> Vec<u8> {
        use sha2::Digest;
        match self {
            Self::Sha1 => sha1::Sha1::digest(data).to_vec(),
            Self::Sha256 => sha2::Sha256::digest(data).to_vec(),
            Self::Sha512 => sha2::Sha512::digest(data).to_vec(),
        }
    }
}

/// Writer adapter that feeds every byte written into a digest.
/// Used with `dump_path_streaming` to hash a NAR without materializing it.
struct DigestWriter<D: sha2::Digest> {
    digest: D,
}

impl<D: sha2::Digest> std::io::Write for DigestWriter<D> {
    fn write(&mut self, buf: &[u8]) -> std::io::Result<usize> {
        self.digest.update(buf);
        Ok(buf.len())
    }
    fn flush(&mut self) -> std::io::Result<()> {
        Ok(())
    }
}

/// Compute the NAR hash of a local filesystem path using the
/// specified algo. Streams through `dump_path_streaming` — no
/// NAR buffering (O(1) memory). Blocking I/O; call via
/// `spawn_blocking` in async contexts.
fn compute_local_nar_hash(path: &Path, algo: FodHashAlgo) -> anyhow::Result<Vec<u8>> {
    use anyhow::Context;
    use sha2::Digest;

    // Each arm instantiates DigestWriter<D> with the right digest type.
    // Can't box dyn Digest (associated types), so match per-arm.
    Ok(match algo {
        FodHashAlgo::Sha1 => {
            let mut w = DigestWriter {
                digest: sha1::Sha1::new(),
            };
            rio_nix::nar::dump_path_streaming(path, &mut w)
                .with_context(|| format!("NAR streaming failed for {}", path.display()))?;
            w.digest.finalize().to_vec()
        }
        FodHashAlgo::Sha256 => {
            let mut w = DigestWriter {
                digest: sha2::Sha256::new(),
            };
            rio_nix::nar::dump_path_streaming(path, &mut w)
                .with_context(|| format!("NAR streaming failed for {}", path.display()))?;
            w.digest.finalize().to_vec()
        }
        FodHashAlgo::Sha512 => {
            let mut w = DigestWriter {
                digest: sha2::Sha512::new(),
            };
            rio_nix::nar::dump_path_streaming(path, &mut w)
                .with_context(|| format!("NAR streaming failed for {}", path.display()))?;
            w.digest.finalize().to_vec()
        }
    })
}

/// Verify FOD output hashes match the declared outputHash (defense-in-depth;
/// nix-daemon also verifies, but we re-check BEFORE upload).
///
/// Dispatches on outputHashAlgo (sha1/sha256/sha512) and computes the
/// hash LOCALLY before upload — a bad output is rejected before it
/// enters the store.
///
/// For `r:<algo>` (recursive): hash the NAR serialization of the output
/// path. For `<algo>` (flat): hash the file contents directly.
///
/// `upper_store` is `{overlay_upper}/nix/store` — callers pass
/// `OverlayMount::upper_store()`.
///
/// Blocking I/O (filesystem reads + hashing). Call via `spawn_blocking`.
pub(super) fn verify_fod_hashes(drv: &Derivation, upper_store: &Path) -> anyhow::Result<()> {
    use anyhow::{Context, bail};

    for output in drv.outputs() {
        // Only FOD outputs have a declared hash
        if output.hash().is_empty() {
            continue;
        }

        let expected = hex::decode(output.hash())
            .with_context(|| format!("FOD outputHash is not valid hex: {}", output.hash()))?;

        // Dispatch on outputHashAlgo. Unknown algo →
        // skip (log warn, don't false-reject). nix-daemon's own
        // verification still runs; we're just defense-in-depth.
        let Some(algo) = FodHashAlgo::from_nix_str(output.hash_algo()) else {
            tracing::warn!(
                output = output.name(),
                hash_algo = output.hash_algo(),
                "FOD output uses unsupported hash algo — skipping worker-side verification \
                 (nix-daemon still verifies)"
            );
            continue;
        };

        let is_recursive = output.hash_algo().starts_with("r:");

        let store_basename = output
            .path()
            .strip_prefix(rio_nix::store_path::STORE_PREFIX)
            .with_context(|| format!("invalid output path: {}", output.path()))?;
        let fs_path = upper_store.join(store_basename);

        let computed = if is_recursive {
            // Compute NAR hash locally (before upload) so a bad
            // output is rejected without entering the store.
            compute_local_nar_hash(&fs_path, algo)?
        } else {
            // Flat hash — read file and hash contents directly.
            let content = std::fs::read(&fs_path)
                .with_context(|| format!("failed to read FOD output file {}", fs_path.display()))?;
            algo.digest(&content)
        };

        if computed != expected {
            bail!(
                "FOD {} hash mismatch for '{}': expected {}, got {}",
                if is_recursive { "NAR" } else { "flat" },
                output.name(),
                output.hash(),
                hex::encode(&computed)
            );
        }
    }
    Ok(())
}

/// Fetch metadata for all input paths from the store.
#[instrument(skip_all, fields(input_count = input_paths.len()))]
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
                    Ok(None) => {
                        tracing::warn!(path = %path, "input path not found in store");
                        Err(ExecutorError::MetadataFetch {
                            path,
                            source: tonic::Status::not_found("path missing from store"),
                        })
                    }
                    Err(e) => {
                        tracing::warn!(path = %path, error = %e, "failed to fetch input path metadata");
                        Err(ExecutorError::MetadataFetch { path, source: e })
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
/// Fallback when the scheduler sends `drv_content: empty` (cache-hit node
/// or inline budget exceeded). The .drv is a single regular file in the
/// store, so we fetch its NAR and extract the ATerm content via
/// `extract_single_file`.
#[instrument(skip_all, fields(drv_path = %drv_path))]
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
#[instrument(skip_all)]
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
        // References are Vec<StorePath> (from ValidatedPathInfo); convert
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

// r[verify worker.fod.verify-hash]
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
    // FOD output hash verification
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

    use rio_test_support::fixtures::seed_store_output as seed_output;

    #[test]
    fn test_verify_fod_recursive_sha256_ok() -> anyhow::Result<()> {
        // Recursive mode computes the LOCAL NAR hash. Seed a real
        // file, compute its NAR hash, use that as the expected hash.
        let content = b"recursive fod sha256 test content";
        let (_tmp, store_dir) = seed_output("test-fod", content)?;

        // Compute the NAR hash of the seeded file.
        let actual_hash = compute_local_nar_hash(&store_dir.join("test-fod"), FodHashAlgo::Sha256)?;
        let drv = make_fod_drv(
            "/nix/store/test-fod",
            "r:sha256",
            &hex::encode(&actual_hash),
        );

        assert!(verify_fod_hashes(&drv, &store_dir).is_ok());
        Ok(())
    }

    #[test]
    fn test_verify_fod_recursive_sha256_mismatch() -> anyhow::Result<()> {
        let (_tmp, store_dir) = seed_output("test-fod", b"actual content")?;
        // Declare a WRONG hash (all-zero digest).
        let wrong_hash = hex::encode([0u8; 32]);
        let drv = make_fod_drv("/nix/store/test-fod", "r:sha256", &wrong_hash);

        let result = verify_fod_hashes(&drv, &store_dir);
        assert!(result.is_err());
        assert!(
            result.unwrap_err().to_string().contains("mismatch"),
            "error should mention hash mismatch"
        );
        Ok(())
    }

    #[test]
    fn test_verify_fod_flat_sha256_ok() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        let content = b"hello world flat fod content";
        let expected: [u8; 32] = Sha256::digest(content).into();

        let (_tmp, store_dir) = seed_output("test-flat-fod", content)?;
        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &hex::encode(expected));

        assert!(verify_fod_hashes(&drv, &store_dir).is_ok());
        Ok(())
    }

    #[test]
    fn test_verify_fod_flat_sha256_mismatch() -> anyhow::Result<()> {
        use sha2::{Digest, Sha256};
        let wrong: [u8; 32] = Sha256::digest(b"different content").into();

        let (_tmp, store_dir) = seed_output("test-flat-fod", b"actual content")?;
        let drv = make_fod_drv("/nix/store/test-flat-fod", "sha256", &hex::encode(wrong));

        assert!(verify_fod_hashes(&drv, &store_dir).is_err());
        Ok(())
    }

    /// sha1 FODs verify correctly (algo dispatch, not hardcoded sha256).
    #[test]
    fn test_verify_fod_flat_sha1_ok() -> anyhow::Result<()> {
        use sha1::Digest;
        let content = b"sha1 flat fod content";
        let expected = sha1::Sha1::digest(content);

        let (_tmp, store_dir) = seed_output("test-sha1-fod", content)?;
        let drv = make_fod_drv("/nix/store/test-sha1-fod", "sha1", &hex::encode(expected));

        assert!(
            verify_fod_hashes(&drv, &store_dir).is_ok(),
            "sha1 FOD should verify (algo dispatch; hardcoded sha256 would false-reject)"
        );
        Ok(())
    }

    /// sha512 FODs verify correctly.
    #[test]
    fn test_verify_fod_flat_sha512_ok() -> anyhow::Result<()> {
        use sha2::{Digest, Sha512};
        let content = b"sha512 flat fod content";
        let expected = Sha512::digest(content);

        let (_tmp, store_dir) = seed_output("test-sha512-fod", content)?;
        let drv = make_fod_drv(
            "/nix/store/test-sha512-fod",
            "sha512",
            &hex::encode(expected),
        );

        assert!(verify_fod_hashes(&drv, &store_dir).is_ok());
        Ok(())
    }

    /// Recursive sha1 (NAR hash computed via sha1).
    #[test]
    fn test_verify_fod_recursive_sha1_ok() -> anyhow::Result<()> {
        let content = b"r:sha1 nar content";
        let (_tmp, store_dir) = seed_output("test-rsha1", content)?;

        let actual = compute_local_nar_hash(&store_dir.join("test-rsha1"), FodHashAlgo::Sha1)?;
        let drv = make_fod_drv("/nix/store/test-rsha1", "r:sha1", &hex::encode(&actual));

        assert!(verify_fod_hashes(&drv, &store_dir).is_ok());
        Ok(())
    }

    /// Unknown algo (e.g., md5 — Nix doesn't support it, but be defensive):
    /// skip verification (log warn) rather than false-reject.
    #[test]
    fn test_verify_fod_unknown_algo_skipped() -> anyhow::Result<()> {
        let (_tmp, store_dir) = seed_output("test-md5-fod", b"content")?;
        // 32-char hex that's NOT the md5 of "content" — would fail
        // if we actually tried to verify. Skip means it passes.
        let drv = make_fod_drv(
            "/nix/store/test-md5-fod",
            "md5",
            "deadbeefdeadbeefdeadbeefdeadbeef",
        );

        // Skipped — should NOT error. nix-daemon's own verify catches
        // the actual mismatch; we just don't double-check unknowns.
        assert!(
            verify_fod_hashes(&drv, &store_dir).is_ok(),
            "unknown algo should be skipped (warn + Ok), not false-rejected"
        );
        Ok(())
    }

    #[test]
    fn test_verify_fod_non_fod_skipped() -> anyhow::Result<()> {
        // Non-FOD (no hash) should be skipped without error
        let drv = make_fod_drv("/nix/store/test-non-fod", "", "");
        let tmp = tempfile::tempdir()?;
        assert!(verify_fod_hashes(&drv, tmp.path()).is_ok());
        Ok(())
    }

    // -----------------------------------------------------------------------
    // gRPC fetch tests via MockStore
    // -----------------------------------------------------------------------

    use rio_test_support::fixtures::{make_nar, make_path_info, test_store_path};
    use rio_test_support::grpc::{MockStore, spawn_mock_store_with_client};

    /// Shorthand for test_store_path — these tests use many paths.
    fn tp(name: &str) -> String {
        test_store_path(name)
    }

    async fn spawn_and_connect() -> anyhow::Result<(MockStore, StoreServiceClient<Channel>)> {
        let (store, client, _h) = spawn_mock_store_with_client().await?;
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
            ExecutorError::MetadataFetch { path, source } => {
                assert_eq!(path, p_missing);
                assert_eq!(source.code(), tonic::Code::NotFound);
            }
            other => panic!("expected MetadataFetch, got {other:?}"),
        }
        Ok(())
    }

    /// Regression: a real gRPC error (e.g., store unavailable) must propagate
    /// with its original status code, NOT be collapsed into a fabricated
    /// NotFound — a naive `Ok(None) | Err(_)` arm would discard the real error.
    #[tokio::test]
    async fn test_fetch_input_metadata_grpc_error_preserves_code() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let p = tp("foo");
        seed_with_refs(&store, &p, &[]);
        // Inject Unavailable on query_path_info.
        store
            .fail_query_path_info
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let err = fetch_input_metadata(&client, std::slice::from_ref(&p))
            .await
            .expect_err("should error on store unavailable");

        match err {
            ExecutorError::MetadataFetch { path, source } => {
                assert_eq!(path, p);
                // The critical assertion: NOT NotFound. The old code would
                // have fabricated NotFound here, masking the real failure.
                assert_eq!(
                    source.code(),
                    tonic::Code::Unavailable,
                    "real gRPC error code must propagate (not be collapsed to NotFound)"
                );
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

    /// Regression: the refscan candidate set MUST be the TRANSITIVE input
    /// closure (what compute_input_closure returns), not just the direct
    /// inputs (resolved_input_srcs). See executor/mod.rs:733 — the
    /// candidate set is `input_paths` (closure), not `resolved_input_srcs`
    /// (direct). If that line regresses to the direct set, a build output
    /// that embeds a transitive dependency (e.g., glibc via closure(stdenv))
    /// would have that reference SILENTLY DROPPED.
    ///
    /// This test exercises the real compute_input_closure → CandidateSet →
    /// RefScanSink path. The `direct_only` scan at the end is the
    /// sensitivity proof: same output bytes, direct-only candidate set →
    /// transitive ref is missed. That's the exact shape of the original bug.
    ///
    // r[verify worker.upload.references-scanned]
    #[tokio::test]
    async fn test_candidate_set_is_transitive_not_direct() -> anyhow::Result<()> {
        use rio_nix::refscan::{CandidateSet, RefScanSink};
        use std::io::Write;

        // Distinct nixbase32 hash parts. tp() uses a single TEST_HASH for
        // all paths — fine for closure BFS (which compares full strings)
        // but CandidateSet keys on the 32-char hash part, so we need real
        // distinct hashes here.
        const H_DIRECT: &str = "7rjj5xmrxb3n63wlk6mzlwxzxbvg7r3a";
        const H_TRANSITIVE: &str = "v5sv61sszx301i0x6xysaqzla09nksnd";
        let p_direct = format!("/nix/store/{H_DIRECT}-stdenv");
        let p_transitive = format!("/nix/store/{H_TRANSITIVE}-glibc");
        let p_drv = tp("hello.drv");

        // Reference graph: direct → transitive. transitive is NOT in
        // drv.input_srcs; it's only reachable via BFS from direct.
        let (store, client) = spawn_and_connect().await?;
        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_direct, std::slice::from_ref(&p_transitive));
        seed_with_refs(&store, &p_transitive, &[]);

        let drv = drv_with_srcs(std::slice::from_ref(&p_direct));

        // --- mod.rs step 1: compute_input_closure (mod.rs:379-380) ---
        let closure = compute_input_closure(&client, &drv, &p_drv).await?;
        let closure_set: std::collections::HashSet<_> = closure.iter().cloned().collect();
        assert!(
            closure_set.contains(&p_transitive),
            "precondition: closure BFS reaches transitive dep"
        );

        // --- mod.rs step 2: merge resolved_input_srcs (mod.rs:384-388) ---
        // drv_with_srcs has no input_drvs, so resolved_input_srcs == input_srcs.
        // (mod.rs:327 clones the BTreeSet into a fresh one and extends it.)
        let resolved_input_srcs: Vec<String> = drv.input_srcs().iter().cloned().collect();
        let input_paths: Vec<String> = {
            let mut s = closure_set;
            s.extend(resolved_input_srcs.iter().cloned());
            s.into_iter().collect()
        };

        // --- mod.rs step 3: ref_candidates = input_paths ∪ outputs (mod.rs:733-734) ---
        let mut ref_candidates = input_paths.clone();
        ref_candidates.extend(drv.outputs().iter().map(|o| o.path().to_string()));

        // Simulated build output: embeds ONLY the transitive dep's path
        // (the way a real binary's RPATH embeds glibc but not stdenv).
        let output_bytes = format!("RPATH={p_transitive}/lib\n");

        // --- THE FIX: scan with closure-based candidates ---
        let cs_closure = CandidateSet::from_paths(&ref_candidates);
        let mut sink = RefScanSink::new(cs_closure.hashes());
        sink.write_all(output_bytes.as_bytes())?;
        let found_with_closure = cs_closure.resolve(&sink.into_found());
        assert_eq!(
            found_with_closure,
            vec![p_transitive.clone()],
            "closure-based candidate set finds transitive ref"
        );

        // --- THE BUG: scan with direct-only candidates ---
        // If mod.rs:733 were `resolved_input_srcs.clone()` instead of
        // `input_paths.clone()`, THIS is the candidate set that would be
        // passed. transitive is not in it → scan silently misses the ref.
        let cs_direct = CandidateSet::from_paths(&resolved_input_srcs);
        let mut sink = RefScanSink::new(cs_direct.hashes());
        sink.write_all(output_bytes.as_bytes())?;
        let found_with_direct = cs_direct.resolve(&sink.into_found());
        assert!(
            found_with_direct.is_empty(),
            "sensitivity proof: direct-only set misses transitive ref \
             (this is the original bug's behavior)"
        );

        // Structural ⊇: input_paths was built by EXTENDING the closure
        // with resolved_input_srcs, so it contains every direct input.
        let input_set: std::collections::HashSet<_> = input_paths.iter().collect();
        for direct in &resolved_input_srcs {
            assert!(
                input_set.contains(direct),
                "input_paths ⊇ resolved_input_srcs (merge at mod.rs:386)"
            );
        }

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
