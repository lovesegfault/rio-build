//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.
// r[impl builder.fod.verify-hash]

use std::path::Path;

use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::derivation::Derivation;
use rio_proto::StoreServiceClient;
use rio_proto::validated::ValidatedPathInfo;

use super::ExecutorError;

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
    fn with<D: sha2::Digest>(path: &Path) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;
        let mut w = DigestWriter { digest: D::new() };
        rio_nix::nar::dump_path_streaming(path, &mut w)
            .with_context(|| format!("NAR streaming failed for {}", path.display()))?;
        Ok(w.digest.finalize().to_vec())
    }
    match algo {
        FodHashAlgo::Sha1 => with::<sha1::Sha1>(path),
        FodHashAlgo::Sha256 => with::<sha2::Sha256>(path),
        FodHashAlgo::Sha512 => with::<sha2::Sha512>(path),
    }
}

/// Compute the flat (raw-content) hash of a local file using the
/// specified algo. Streams via `io::copy` → [`DigestWriter`] — O(1)
/// memory regardless of file size. Blocking I/O; call via
/// `spawn_blocking` in async contexts.
///
/// nixpkgs `fetchurl` is flat-hashed by default and routinely pulls
/// multi-GB blobs (CUDA runfiles, JDK bundles, model weights) into
/// fetcher pods sized at `LOCAL_MEM_BYTES` ≈ 2 GiB; a `fs::read` here
/// would OOM the pod after the download already succeeded.
///
/// Opens `rel` (the output's store basename) relative to an
/// `upper_store` directory fd via `openat2(RESOLVE_BENEATH |
/// RESOLVE_NO_SYMLINKS)` and rejects non-regular files: a malicious
/// build could leave a symlink at the output path — or at any parent
/// component it controls — pointing at an arbitrary host file whose
/// contents hash to the declared outputHash (buildah CVE-2024-1753
/// class); verification would then attest content the build never
/// produced. The kernel guarantees resolution never follows a symlink
/// in ANY component (`RESOLVE_NO_SYMLINKS`, including the final one)
/// and never escapes `upper_store` via `..` (`RESOLVE_BENEATH`) —
/// strictly stronger than the previous final-component-only
/// `O_NOFOLLOW`. `O_NONBLOCK` keeps a build-planted writer-less FIFO
/// from hanging the open; the fstat check then rejects it. Linux-only,
/// like the rest of the builder (FUSE + io_uring).
fn compute_local_flat_hash(
    upper_store: &Path,
    rel: &Path,
    algo: FodHashAlgo,
) -> anyhow::Result<Vec<u8>> {
    fn with<D: sha2::Digest>(upper_store: &Path, rel: &Path) -> anyhow::Result<Vec<u8>> {
        use anyhow::{Context, bail};
        use nix::fcntl::{OFlag, OpenHow, ResolveFlag, open, openat2};
        use nix::sys::stat::Mode;

        let display = upper_store.join(rel);
        // The upper store dir itself is builder-owned (overlay upper),
        // not build-writable — resolving it normally once is safe.
        let dirfd = open(
            upper_store,
            OFlag::O_RDONLY | OFlag::O_DIRECTORY | OFlag::O_CLOEXEC,
            Mode::empty(),
        )
        .with_context(|| format!("failed to open upper store {}", upper_store.display()))?;
        // O_NONBLOCK: a blocking open of a build-planted FIFO with no
        // writer sleeps forever; with O_NONBLOCK it returns at once and
        // the fstat below rejects it (no effect on regular-file reads).
        let how = OpenHow::new()
            .flags(OFlag::O_RDONLY | OFlag::O_CLOEXEC | OFlag::O_NONBLOCK)
            .resolve(ResolveFlag::RESOLVE_BENEATH | ResolveFlag::RESOLVE_NO_SYMLINKS);
        let fd = openat2(&dirfd, rel, how).with_context(|| {
            format!(
                "failed to open FOD output {} (symlinks are rejected)",
                display.display()
            )
        })?;
        let mut f = std::fs::File::from(fd);
        // fstat the opened fd (not the path — no TOCTOU window) to
        // reject the remaining non-regular kinds (directory, FIFO,
        // device): flat hashing is only defined over regular files.
        let meta = f
            .metadata()
            .with_context(|| format!("failed to stat FOD output {}", display.display()))?;
        if !meta.is_file() {
            bail!(
                "FOD output {} is not a regular file ({:?})",
                display.display(),
                meta.file_type()
            );
        }
        let mut w = DigestWriter { digest: D::new() };
        std::io::copy(&mut f, &mut w)?;
        Ok(w.digest.finalize().to_vec())
    }
    match algo {
        FodHashAlgo::Sha1 => with::<sha1::Sha1>(upper_store, rel),
        FodHashAlgo::Sha256 => with::<sha2::Sha256>(upper_store, rel),
        FodHashAlgo::Sha512 => with::<sha2::Sha512>(upper_store, rel),
    }
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

        let store_basename = rio_nix::store_path::basename(output.path())
            .with_context(|| format!("invalid output path: {}", output.path()))?;
        let fs_path = upper_store.join(store_basename);

        let computed = if is_recursive {
            // Compute NAR hash locally (before upload) so a bad
            // output is rejected without entering the store.
            compute_local_nar_hash(&fs_path, algo)?
        } else {
            // Flat hash — stream file contents through a digest
            // sink. Same O(1)-memory contract as the recursive
            // branch above (see compute_local_flat_hash doc).
            // Resolved relative to upper_store so the kernel-enforced
            // RESOLVE_BENEATH/RESOLVE_NO_SYMLINKS guards apply.
            compute_local_flat_hash(upper_store, Path::new(store_basename), algo)?
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
        &[],
    )
    .await
    .map_err(|e| ExecutorError::MetadataFetch {
        path: drv_path.to_string(),
        source: match e {
            rio_proto::client::NarCollectError::Stream(s) => s,
            other => tonic::Status::internal(other.to_string()),
        },
    })?;

    let Some((_, nar_data)) = result else {
        return Err(ExecutorError::MetadataFetch {
            path: drv_path.to_string(),
            source: tonic::Status::not_found(".drv not found in store"),
        });
    };

    Derivation::parse_from_nar(&nar_data).map_err(|e| {
        ExecutorError::InvalidDerivation(format!("failed to parse .drv from NAR: {e}"))
    })
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
/// BatchQueryPathInfo — one RPC per BFS LAYER (typical closure has ~5-15
/// layers). I-110: previously one QueryPathInfo per PATH (~800/build);
/// with 246 ephemeral builders that was ~196k RPCs, saturating the
/// store's PG pool (acquire times → 11s → FUSE circuit-breaker → EIO).
/// Paths not yet in the store (e.g., outputs of not-yet-built input
/// drvs) are skipped — FUSE will lazy-fetch them at build time.
///
/// `resolved_input_srcs` MUST be `drv.input_srcs()` ∪ the resolved
/// output paths of every `input_drv`. The internal seed only adds
/// `drv_path` + `input_drvs().keys()` (the .drv files); the caller
/// supplies the OUTPUTS so the BFS walks their runtime references.
/// I-043: a .drv file's narinfo references DON'T include its outputs
/// (outputs are in the ATerm structure, not the NAR content). Seeding
/// only `input_drvs().keys()` meant the BFS walked dep.drv → its
/// references but NEVER dep.drv's OUTPUT → output's references.
/// Transitive runtime deps (autotools-hook via stdenv-the-output) were
/// never reached → not in JIT allowlist → ENOENT → build fails.
#[instrument(skip_all)]
pub(super) async fn compute_input_closure(
    store_client: &StoreServiceClient<Channel>,
    drv: &Derivation,
    drv_path: &str,
    resolved_input_srcs: &std::collections::BTreeSet<String>,
) -> Result<Vec<ValidatedPathInfo>, ExecutorError> {
    use std::collections::HashSet;

    // I-106: keep the full PathInfo from each BFS query so callers
    // (synth_db generation in prepare_sandbox) don't have to re-query
    // the same ~800 paths. Under ephemeral-builder load that second
    // pass was a ~800 × N-builders QueryPathInfo burst that exhausted
    // the store's PG pool.
    let mut closure: HashSet<String> = HashSet::new();
    let mut metadata: Vec<ValidatedPathInfo> = Vec::new();
    let mut frontier: Vec<String> = Vec::new();

    // Seed: the .drv itself, input_drv paths (so nix-daemon can read them),
    // and resolved_input_srcs (input_srcs ∪ input_drv OUTPUTS — the caller
    // has already fetched each input .drv and extracted output paths).
    frontier.push(drv_path.to_string());
    frontier.extend(drv.input_drvs().keys().cloned());
    frontier.extend(resolved_input_srcs.iter().cloned());

    // BFS by layer. One BatchQueryPathInfo per layer (I-110); layer
    // count is typically 5-15 (dep depth).
    let mut layer_idx = 0u32;
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

        // Fetch this layer in ONE batch RPC. Each result is the full
        // ValidatedPathInfo (kept for the caller — I-106) or None on
        // not-found. References for the next layer come from
        // info.references.
        let layer_start = std::time::Instant::now();
        let n_paths = batch.len();
        let results: Vec<(String, Option<ValidatedPathInfo>)> =
            query_layer(store_client, batch).await?;
        tracing::debug!(
            layer = layer_idx,
            n_paths,
            elapsed = ?layer_start.elapsed(),
            closure_size = closure.len(),
            "compute_input_closure: BFS layer complete"
        );
        layer_idx += 1;

        // Add found paths to closure, collect their refs for next layer.
        for (path, info) in results {
            let Some(info) = info else {
                // Path not in store. Legitimate for an output of a
                // not-yet-built input drv (rare — scheduler gates
                // dispatch on dep completion). Previously also hit for
                // transitive runtime refs of substituted paths
                // (BatchQueryPathInfo is local-only; rustc-1.94.0 via
                // rustc-wrapper) — now closed at the source by the
                // scheduler's `walk_substitute_closure` BFS.
                // A path skipped here is NOT in the JIT allowlist, so
                // FUSE returns ENOENT (not lazy-fetch — the builder
                // carries no tenant context to substitute on miss).
                tracing::debug!(path = %path, "input not in store; dropped from JIT allowlist");
                continue;
            };
            for r in &info.references {
                if !closure.contains(r.as_str()) {
                    frontier.push(r.to_string());
                }
            }
            closure.insert(info.store_path.to_string());
            metadata.push(info);
        }
    }

    Ok(metadata)
}

/// Resolve the castore root node for every closure path the build's
/// `/nix/store` must serve, combining the scheduler-attested
/// `WorkAssignment.input_roots` (P0588) with a `GetNarIndexBatch`
/// fallback for paths dispatched without a `root_node` (indexer lag,
/// scheduler PG blip) or not dispatched at all (closure-compute
/// timeout → empty `input_roots`).
///
/// The path universe is `input_roots ∪ input_metadata`: the synth-DB
/// `ValidPaths` set (`input_metadata`, from [`compute_input_closure`])
/// is what nix-daemon will `lstat` at sandbox setup, so every one of
/// those paths must resolve in the castore tree; scheduler-sent roots
/// outside the local closure are kept as-is (harmless extras). A path
/// that needs the fallback but has no locally-known `nar_hash` (i.e.
/// it is not in the local closure either) is dropped with a warning —
/// the daemon never asks for it.
///
/// Errors are infrastructure failures: a closure path with no NAR
/// index cannot be served lazily, so the build is re-queued instead of
/// failing as a build defect.
pub(super) async fn resolve_castore_roots(
    store_client: &StoreServiceClient<Channel>,
    assignment_token: &str,
    assignment_roots: &[rio_proto::types::InputRoot],
    input_metadata: &[ValidatedPathInfo],
) -> Result<Vec<rio_proto::types::InputRoot>, ExecutorError> {
    use std::collections::{HashMap, HashSet};

    let nar_hash_by_path: HashMap<&str, [u8; 32]> = input_metadata
        .iter()
        .map(|m| (m.store_path.as_str(), m.nar_hash))
        .collect();

    let mut resolved: Vec<rio_proto::types::InputRoot> = Vec::new();
    let mut seen: HashSet<&str> = HashSet::new();
    // (store_path, nar_hash) pairs still needing a GetNarIndexBatch
    // round-trip.
    let mut needs: Vec<(String, [u8; 32])> = Vec::new();

    for root in assignment_roots {
        seen.insert(root.store_path.as_str());
        if root.root_node.is_some() {
            resolved.push(root.clone());
        } else if let Some(hash) = nar_hash_by_path.get(root.store_path.as_str()) {
            needs.push((root.store_path.clone(), *hash));
        } else {
            // Scheduler closure entry the local BFS never saw (path not
            // in the store yet) and no root_node either — nothing to
            // mount, and the daemon won't ask for it (it's not in the
            // synth DB). Drop with a warning so scheduler/indexer skew
            // is visible.
            tracing::warn!(
                store_path = %root.store_path,
                "input root has no root_node and no local nar_hash; dropping from castore tree"
            );
        }
    }
    for info in input_metadata {
        if !seen.contains(info.store_path.as_str()) {
            needs.push((info.store_path.to_string(), info.nar_hash));
        }
    }

    if needs.is_empty() {
        return Ok(resolved);
    }

    // One batch RPC for every unresolved path (I-110: never one unary
    // RPC per path — the fallback can be the whole closure when the
    // scheduler's closure compute timed out).
    let mut client = store_client.clone();
    // GetNarIndexBatch is identity-gated server-side (the index is a
    // full file listing with digests); present the same assignment
    // token the castore-FUSE data path sends on GetChunks.
    let mut request = tonic::Request::new(rio_proto::types::GetNarIndexBatchRequest {
        nar_hashes: needs.iter().map(|(_, h)| h.to_vec()).collect(),
    });
    crate::upload::common::attach_assignment_token(&mut request, assignment_token).map_err(
        |status| ExecutorError::MetadataFetch {
            path: needs[0].0.clone(),
            source: status,
        },
    )?;
    let infra = |path: &str, msg: String| ExecutorError::InputRoots {
        path: path.to_owned(),
        reason: msg,
    };
    let consume = async {
        // streaming-open-ban: bound the open itself; the outer
        // tokio::time::timeout below still bounds open+consume together
        // at the same GRPC_STREAM_TIMEOUT, so the inner TimedOut arm is
        // dominated and serves as the structural witness.
        let open = rio_common::transport::bounded_open(
            std::future::pending(),
            rio_common::grpc::GRPC_STREAM_TIMEOUT,
            client.get_nar_index_batch(request),
        )
        .await;
        let mut stream = match open {
            rio_common::transport::OpenOutcome::Opened(r) => r
                .map_err(|status| ExecutorError::MetadataFetch {
                    path: needs[0].0.clone(),
                    source: status,
                })?
                .into_inner(),
            rio_common::transport::OpenOutcome::TimedOut { .. }
            | rio_common::transport::OpenOutcome::Aborted => {
                return Err(infra(
                    &needs[0].0,
                    "GetNarIndexBatch open timed out".to_string(),
                ));
            }
        };
        let mut by_hash: HashMap<Vec<u8>, Option<rio_proto::types::NarIndex>> = HashMap::new();
        while let Some(resp) =
            stream
                .message()
                .await
                .map_err(|status| ExecutorError::MetadataFetch {
                    path: needs[0].0.clone(),
                    source: status,
                })?
        {
            by_hash.insert(resp.nar_hash, resp.index);
        }
        Ok::<_, ExecutorError>(by_hash)
    };
    let by_hash = tokio::time::timeout(rio_common::grpc::GRPC_STREAM_TIMEOUT, consume)
        .await
        .map_err(|_| {
            infra(
                &needs[0].0,
                "GetNarIndexBatch timed out resolving castore root nodes".to_string(),
            )
        })??;

    for (path, hash) in &needs {
        let index = by_hash.get(hash.as_slice()).and_then(|i| i.as_ref());
        let Some(index) = index else {
            // The path is in the store (compute_input_closure saw it)
            // but has no NAR index row, so the castore-FUSE cannot
            // serve it. Eager indexing at PutPath (P0557) makes this
            // unreachable for freshly-uploaded paths; hitting it means
            // pre-P0557 data or a store-side regression.
            return Err(infra(
                path,
                "store has no NAR index for this path — was it uploaded before eager \
                 indexing (P0557), or is the index backfill still running?"
                    .to_string(),
            ));
        };
        let root_node = crate::castore_fuse::session::root_node_from_nar_index(index)
            .map_err(|e| infra(path, format!("corrupt NAR index: {e}")))?;
        resolved.push(rio_proto::types::InputRoot {
            store_path: path.clone(),
            root_node: Some(root_node),
        });
    }

    Ok(resolved)
}

/// Fetch one BFS layer's metadata via `BatchQueryPathInfo` (one RPC
/// for the whole layer — I-110).
async fn query_layer(
    store_client: &StoreServiceClient<Channel>,
    batch: Vec<String>,
) -> Result<Vec<(String, Option<ValidatedPathInfo>)>, ExecutorError> {
    let mut client = store_client.clone();
    match rio_proto::client::batch_query_path_info(
        &mut client,
        batch.clone(),
        rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
        &[],
    )
    .await
    {
        Ok(entries) => Ok(entries),
        Err(status) => {
            // Real error (Unavailable, DeadlineExceeded, …) — propagate
            // with a representative path. The original status code is
            // preserved (test_compute_input_closure_grpc_error_preserves_code).
            Err(ExecutorError::MetadataFetch {
                path: batch.into_iter().next().unwrap_or_default(),
                source: status,
            })
        }
    }
}

// r[verify builder.fod.verify-hash]
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
    use rstest::rstest;

    /// Compute the *correct* declared-hash hex for `content` seeded at
    /// `basename` under `store_dir`, given the ATerm hash_algo string.
    /// Recursive ("r:" prefix) → NAR hash; flat → raw-content digest.
    fn correct_fod_hash(
        store_dir: &std::path::Path,
        basename: &str,
        content: &[u8],
        algo: &str,
    ) -> anyhow::Result<String> {
        use sha2::Digest;
        Ok(match algo {
            "r:sha256" => hex::encode(compute_local_nar_hash(
                &store_dir.join(basename),
                FodHashAlgo::Sha256,
            )?),
            "r:sha1" => hex::encode(compute_local_nar_hash(
                &store_dir.join(basename),
                FodHashAlgo::Sha1,
            )?),
            "sha256" => hex::encode(sha2::Sha256::digest(content)),
            "sha512" => hex::encode(sha2::Sha512::digest(content)),
            "sha1" => hex::encode(<sha1::Sha1 as sha1::Digest>::digest(content)),
            other => panic!("test helper: unhandled algo {other}"),
        })
    }

    /// FOD hash verification across {flat, recursive} × {sha1, sha256, sha512}
    /// with both matching and mismatching declared hashes. `declare_correct`
    /// = false means we declare an all-zero digest of the right length and
    /// expect the verifier to reject with "mismatch".
    ///
    /// Covers algo dispatch (a hardcoded-sha256 verifier would false-reject
    /// the sha1/sha512 ok cases).
    #[rstest]
    #[case::recursive_sha256_ok("test-fod", "r:sha256", true)]
    #[case::recursive_sha256_mismatch("test-fod", "r:sha256", false)]
    #[case::flat_sha256_ok("test-flat-fod", "sha256", true)]
    #[case::flat_sha256_mismatch("test-flat-fod", "sha256", false)]
    #[case::flat_sha1_ok("test-sha1-fod", "sha1", true)]
    #[case::flat_sha512_ok("test-sha512-fod", "sha512", true)]
    #[case::recursive_sha1_ok("test-rsha1", "r:sha1", true)]
    fn test_verify_fod(
        #[case] basename: &str,
        #[case] algo: &str,
        #[case] declare_correct: bool,
    ) -> anyhow::Result<()> {
        let content = format!("fod test content for {algo}").into_bytes();
        let (_tmp, store_dir) = seed_output(basename, &content)?;

        let declared = if declare_correct {
            correct_fod_hash(&store_dir, basename, &content, algo)?
        } else {
            // Wrong hash: all-zero digest of the correct width.
            let width = correct_fod_hash(&store_dir, basename, &content, algo)?.len();
            "0".repeat(width)
        };
        let drv = make_fod_drv(&format!("/nix/store/{basename}"), algo, &declared);

        let result = verify_fod_hashes(&drv, &store_dir);
        assert_eq!(
            result.is_ok(),
            declare_correct,
            "algo={algo} declare_correct={declare_correct}: got {result:?}"
        );
        if !declare_correct {
            assert!(
                result.unwrap_err().to_string().contains("mismatch"),
                "error should mention hash mismatch"
            );
        }
        Ok(())
    }

    /// Flat-hash a file much larger than `io::copy`'s internal stack
    /// buffer (8 KiB) — proves the streaming path stitches digest state
    /// across many chunk boundaries and produces the same digest as the
    /// in-memory oracle. Regression test for the `fs::read` OOM: the
    /// structural guarantee that we DON'T allocate the file is the
    /// deletion of `FodHashAlgo::digest(&[u8])`; this test proves
    /// multi-chunk correctness.
    #[test]
    fn test_verify_fod_flat_large_file_streams() -> anyhow::Result<()> {
        use sha2::Digest;
        // 16 MiB of pseudo-random-ish bytes (not all-zero — we want a
        // digest that changes if a chunk is dropped or reordered).
        let content: Vec<u8> = (0..16 * 1024 * 1024).map(|i| (i % 251) as u8).collect();
        let (_tmp, store_dir) = seed_output("test-flat-large", &content)?;
        let expected = hex::encode(sha2::Sha256::digest(&content));
        let drv = make_fod_drv("/nix/store/test-flat-large", "sha256", &expected);
        verify_fod_hashes(&drv, &store_dir)
    }

    /// A malicious build can leave a SYMLINK at the FOD output path
    /// pointing at an arbitrary host file whose contents hash to the
    /// declared outputHash (buildah CVE-2024-1753 class). Flat
    /// verification must reject the symlink — following it would attest
    /// content the build never produced and read host files across the
    /// sandbox boundary.
    #[test]
    fn test_verify_fod_flat_symlink_rejected() -> anyhow::Result<()> {
        use sha2::Digest;
        let content = b"host file contents the build never produced";
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir)?;
        // "Host" file outside the upper store.
        let host_file = tmp.path().join("host-secret");
        std::fs::write(&host_file, content)?;
        // Build-written symlink at the output path.
        std::os::unix::fs::symlink(&host_file, store_dir.join("test-flat-symlink"))?;

        // Declared hash matches the symlink TARGET's contents — a
        // verifier that follows the link would wrongly accept.
        let declared = hex::encode(sha2::Sha256::digest(content));
        let drv = make_fod_drv("/nix/store/test-flat-symlink", "sha256", &declared);

        let err = verify_fod_hashes(&drv, &store_dir)
            .expect_err("flat FOD verification must reject a symlinked output");
        assert!(
            err.to_string().contains("failed to open FOD output"),
            "error should come from the open-time symlink rejection: {err:#}"
        );
        Ok(())
    }

    /// A symlink at an INTERMEDIATE component of the output path
    /// (`upper_store/sub` → outside dir, output at `upper_store/sub/out`)
    /// must be rejected: `O_NOFOLLOW` only guards the FINAL component, so
    /// a path-based open would happily resolve through `sub` and hash a
    /// file outside the upper store. `openat2(RESOLVE_BENEATH |
    /// RESOLVE_NO_SYMLINKS)` rejects symlinks in EVERY component by
    /// kernel guarantee.
    #[test]
    fn test_flat_hash_rejects_intermediate_symlink() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let upper_store = tmp.path().join("upper/nix/store");
        std::fs::create_dir_all(&upper_store)?;
        // "Host" dir outside the upper store, with the real file.
        let outside = tmp.path().join("outside");
        std::fs::create_dir(&outside)?;
        std::fs::write(outside.join("out"), b"host data the build never produced")?;
        // Build-written symlink at an intermediate component.
        std::os::unix::fs::symlink(&outside, upper_store.join("sub"))?;

        // Red-proven on the O_NOFOLLOW-only code: a full-path open
        // resolved through `sub` and returned Ok(hash of host data).
        let res = compute_local_flat_hash(&upper_store, Path::new("sub/out"), FodHashAlgo::Sha256);
        assert!(
            res.is_err(),
            "flat hash must reject a symlink in an intermediate component, got {res:?}"
        );

        // `..` escape is likewise refused by RESOLVE_BENEATH (EXDEV),
        // independent of any userspace name validation.
        let res = compute_local_flat_hash(
            &upper_store,
            Path::new("../../outside/out"),
            FodHashAlgo::Sha256,
        );
        assert!(
            res.is_err(),
            "flat hash must reject a `..` escape from the upper store, got {res:?}"
        );
        Ok(())
    }

    /// A FIFO at the output path must be rejected, not hung on:
    /// `RESOLVE_NO_SYMLINKS` does not exclude FIFOs, and a blocking
    /// `openat2` of a writer-less FIFO sleeps until a writer appears —
    /// a build could wedge the verifier forever. `O_NONBLOCK` makes the
    /// open return immediately so the fstat check can reject the FIFO.
    /// Red-proven: without `O_NONBLOCK` in the openat2 `how.flags`,
    /// this test hangs in the open.
    #[test]
    fn test_verify_fod_flat_fifo_rejected() -> anyhow::Result<()> {
        let tmp = tempfile::tempdir()?;
        let store_dir = tmp.path().join("nix/store");
        std::fs::create_dir_all(&store_dir)?;
        nix::unistd::mkfifo(
            &store_dir.join("test-flat-fifo"),
            nix::sys::stat::Mode::from_bits_truncate(0o644),
        )?;

        let declared = "00".repeat(32);
        let drv = make_fod_drv("/nix/store/test-flat-fifo", "sha256", &declared);

        let err = verify_fod_hashes(&drv, &store_dir)
            .expect_err("flat FOD verification must reject a FIFO output");
        assert!(
            err.to_string().contains("not a regular file"),
            "error should come from the fstat file-type check: {err:#}"
        );
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

    /// `compute_input_closure`'s `resolved_input_srcs` parameter for tests
    /// without input_drvs: just `drv.input_srcs()` (the production caller
    /// adds resolved input_drv outputs, but `drv_with_srcs` builds drvs
    /// with empty input_drvs so there's nothing to resolve).
    fn srcs_of(drv: &Derivation) -> std::collections::BTreeSet<String> {
        drv.input_srcs().clone()
    }

    /// Project closure metadata to a path set for membership assertions.
    fn paths_of(closure: Vec<ValidatedPathInfo>) -> std::collections::HashSet<String> {
        closure
            .into_iter()
            .map(|m| m.store_path.to_string())
            .collect()
    }

    /// I-106: compute_input_closure now returns the full ValidatedPathInfo
    /// captured during BFS, eliminating the second QueryPathInfo pass that
    /// fetch_input_metadata used to do. This test verifies the metadata
    /// fields are populated (not just path), proving the synth_db
    /// generation can use this directly.
    #[tokio::test]
    async fn test_compute_input_closure_returns_full_metadata() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_drv, p_a) = (tp("test.drv"), tp("lib"));
        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_a, &[]);

        let drv = drv_with_srcs(std::slice::from_ref(&p_a));
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;

        let lib = closure
            .iter()
            .find(|m| m.store_path.as_str() == p_a)
            .expect("p_a in closure");
        assert!(
            lib.nar_size > 0,
            "nar_size populated (synth_db needs this) — proves we kept the \
             full PathInfo, not just the path string"
        );
        Ok(())
    }

    /// Regression: a real gRPC error (e.g., store unavailable) must propagate
    /// with its original status code, NOT be collapsed into a fabricated
    /// NotFound — a naive `Ok(None) | Err(_)` arm would discard the real error.
    #[tokio::test]
    async fn test_compute_input_closure_grpc_error_preserves_code() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let p = tp("foo");
        seed_with_refs(&store, &p, &[]);
        // Inject Unavailable on query_path_info.
        store
            .faults
            .fail_query_path_info
            .store(true, std::sync::atomic::Ordering::SeqCst);

        let drv = drv_with_srcs(std::slice::from_ref(&p));
        let err = compute_input_closure(&client, &drv, &tp("test.drv"), &srcs_of(&drv))
            .await
            .expect_err("should error on store unavailable");

        match err {
            ExecutorError::MetadataFetch { source, .. } => {
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
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv))
            .await
            .expect("closure computation should succeed");

        let set = paths_of(closure);
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
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv))
            .await
            .expect("missing ref is non-fatal");

        let set = paths_of(closure);
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
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;

        assert_eq!(closure.len(), 4); // drv, A, B, C (once)
        Ok(())
    }

    /// I-110: closure BFS uses one BatchQueryPathInfo per layer, NOT
    /// one QueryPathInfo per path. For a 4-node chain (drv→A→B→C) the
    /// layer count is ≤4 (could be 3 — drv+A in the seed layer), and
    /// `qpi_calls` (per-path RPC log) stays empty.
    #[tokio::test]
    async fn test_compute_input_closure_uses_batch_rpc() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let (store, client) = spawn_and_connect().await?;
        let (p_drv, p_a, p_b, p_c) = (tp("test.drv"), tp("lib"), tp("dep"), tp("leaf"));
        seed_with_refs(&store, &p_drv, std::slice::from_ref(&p_a));
        seed_with_refs(&store, &p_a, std::slice::from_ref(&p_b));
        seed_with_refs(&store, &p_b, std::slice::from_ref(&p_c));
        seed_with_refs(&store, &p_c, &[]);

        let drv = drv_with_srcs(std::slice::from_ref(&p_a));
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;
        assert_eq!(closure.len(), 4);

        let batch_calls = store.calls.batch_qpi_calls.load(Ordering::SeqCst);
        assert!(
            (1..=4).contains(&batch_calls),
            "one batch RPC per BFS layer (got {batch_calls}); \
             pre-I-110 would be 0 batch + 4 per-path"
        );
        assert!(
            store.calls.qpi_calls.read().unwrap().is_empty(),
            "per-path QueryPathInfo should NOT be called when batch is available"
        );
        Ok(())
    }

    /// I-043 regression: an input_drv's OUTPUT (not in input_srcs, not
    /// in input_drvs.keys, not in any .drv's narinfo references — only
    /// declared in the input .drv's ATerm structure) must be in the
    /// closure, AND its runtime references must be walked.
    ///
    /// Live: closure count=8, autotools-hook missing. autotools-hook
    /// is reached only via stdenv-the-OUTPUT's references; the BFS
    /// previously seeded only stdenv.drv (the FILE), whose narinfo
    /// references do not include its outputs.
    #[tokio::test]
    async fn test_compute_input_closure_walks_input_drv_output_references() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;

        // The shape: main.drv has input_drvs={dep.drv: [out]}. dep.drv's
        // out is `dep_output`. dep_output references `transitive`.
        //
        // dep.drv's narinfo references DO NOT include dep_output (a .drv
        // file's NAR content is the ATerm string; the scanner finds
        // references textually embedded, but the output PATH in the
        // outputs() declaration isn't a reference — it's where we WRITE
        // TO, not what we DEPEND ON). The only way to reach dep_output
        // is via the resolved_input_srcs seed.
        let p_drv = tp("main.drv");
        let p_dep_drv = tp("dep.drv");
        let p_dep_output = tp("stdenv");
        let p_transitive = tp("autotools-hook");

        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_dep_drv, &[]); // .drv file, NO ref to its output
        seed_with_refs(&store, &p_dep_output, std::slice::from_ref(&p_transitive));
        seed_with_refs(&store, &p_transitive, &[]);

        // input_srcs is empty; input_drvs is implicit in the test (we
        // can't easily build a Derivation with input_drvs via ATerm in
        // this test harness, so we simulate the production caller's
        // resolution by passing dep_output in resolved_input_srcs).
        let drv = drv_with_srcs(&[]);
        let resolved: std::collections::BTreeSet<String> = [p_dep_output.clone()].into();

        let closure = compute_input_closure(&client, &drv, &p_drv, &resolved).await?;
        let set = paths_of(closure);

        assert!(
            set.contains(&p_dep_output),
            "input_drv output is in closure (seeded directly)"
        );
        assert!(
            set.contains(&p_transitive),
            "I-043: input_drv output's RUNTIME references are walked. \
             Pre-fix: dep_output was merged AFTER the BFS, so the BFS \
             only saw dep.drv → its (empty) narinfo refs. transitive \
             was never reached → not in JIT allowlist → ENOENT."
        );

        // The pre-fix shape: seed only with input_srcs (empty here),
        // then merge dep_output post-BFS. Prove transitive is missed.
        let pre_fix_closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;
        let mut pre_fix_set = paths_of(pre_fix_closure);
        pre_fix_set.insert(p_dep_output); // post-BFS merge
        assert!(
            !pre_fix_set.contains(&p_transitive),
            "sensitivity proof: pre-fix seed (input_srcs only) + post-BFS \
             merge of dep_output never reaches transitive"
        );

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
    // r[verify builder.upload.references-scanned+2]
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
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;
        let closure_set = paths_of(closure);
        assert!(
            closure_set.contains(&p_transitive),
            "precondition: closure BFS reaches transitive dep"
        );

        // --- mod.rs step 2: input_paths derived from closure metadata ---
        // (resolve_inputs maps store_path; resolved_input_srcs are
        // already in the closure since they seed the BFS.)
        let resolved_input_srcs: Vec<String> = drv.input_srcs().iter().cloned().collect();
        let input_paths: Vec<String> = closure_set.into_iter().collect();

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

    // -----------------------------------------------------------------------
    // resolve_castore_roots (WorkAssignment.input_roots + GetNarIndex fallback)
    // -----------------------------------------------------------------------

    use rio_proto::castore::root_node;
    use rio_proto::types::{InputRoot, NarEntryKind, NarIndex, NarIndexEntry};

    /// A minimal directory-rooted NAR index whose root_digest is `fill`.
    fn dir_index(fill: u8) -> NarIndex {
        NarIndex {
            root_digest: vec![fill; 32],
            entries: vec![NarIndexEntry {
                path: Vec::new(),
                kind: NarEntryKind::Directory.into(),
                dir_digest: vec![fill; 32],
                ..Default::default()
            }],
        }
    }

    fn dir_root_node(fill: u8) -> rio_proto::castore::RootNode {
        rio_proto::castore::RootNode {
            node: Some(root_node::Node::DirDigest(vec![fill; 32])),
        }
    }

    /// Pre-resolved scheduler roots pass through untouched; roots the
    /// scheduler sent without a `root_node` AND closure paths the
    /// scheduler did not send at all are resolved via GetNarIndexBatch.
    /// Guards the real failure mode of the cutover: a path missing from
    /// the castore tree is an ENOENT at sandbox setup.
    #[tokio::test]
    async fn test_resolve_castore_roots_merges_assignment_and_fallback() -> anyhow::Result<()> {
        let (store, client) = spawn_and_connect().await?;
        let (p_pre, p_unrooted, p_local_only) =
            (tp("pre-resolved"), tp("unrooted"), tp("local-only"));

        // Local closure metadata: nar_hash is what keys the fallback.
        let (nar_a, hash_a) = make_nar(b"a");
        let (nar_b, hash_b) = make_nar(b"b");
        let (nar_c, hash_c) = make_nar(b"c");
        let metadata = vec![
            make_path_info(&p_pre, &nar_a, hash_a),
            make_path_info(&p_unrooted, &nar_b, hash_b),
            make_path_info(&p_local_only, &nar_c, hash_c),
        ];
        store.seed_nar_index(hash_b, dir_index(2));
        store.seed_nar_index(hash_c, dir_index(3));

        let assignment_roots = vec![
            InputRoot {
                store_path: p_pre.clone(),
                root_node: Some(dir_root_node(1)),
            },
            InputRoot {
                store_path: p_unrooted.clone(),
                root_node: None,
            },
            // p_local_only deliberately absent from the assignment.
        ];

        let roots =
            resolve_castore_roots(&client, "test-token", &assignment_roots, &metadata).await?;
        let by_path: std::collections::HashMap<_, _> = roots
            .iter()
            .map(|r| (r.store_path.as_str(), r.root_node.clone()))
            .collect();
        assert_eq!(by_path.len(), 3, "all three closure paths resolved");
        assert_eq!(
            by_path[p_pre.as_str()],
            Some(dir_root_node(1)),
            "scheduler-resolved root passes through verbatim (no extra RPC)"
        );
        assert_eq!(
            by_path[p_unrooted.as_str()],
            Some(dir_root_node(2)),
            "root_node:None falls back to the path's NAR index"
        );
        assert_eq!(
            by_path[p_local_only.as_str()],
            Some(dir_root_node(3)),
            "closure path the scheduler omitted is resolved too"
        );
        Ok(())
    }

    /// A closure path with NO NAR index (pre-P0557 upload, backfill
    /// lag) cannot be served by the castore-FUSE — the resolver must
    /// fail with an actionable infra error, not silently mount a tree
    /// missing an input.
    #[tokio::test]
    async fn test_resolve_castore_roots_unindexed_path_is_infra_error() -> anyhow::Result<()> {
        let (_store, client) = spawn_and_connect().await?;
        let p = tp("unindexed");
        let (nar, hash) = make_nar(b"content");
        let metadata = vec![make_path_info(&p, &nar, hash)];
        // NOT seeding a nar_index for `hash`.

        let err = resolve_castore_roots(&client, "test-token", &[], &metadata)
            .await
            .expect_err("missing index must fail the mount, not drop the path");
        match &err {
            ExecutorError::InputRoots { path, reason } => {
                assert_eq!(path, &p);
                assert!(
                    reason.contains("NAR index"),
                    "error must say what is missing: {reason}"
                );
            }
            other => panic!("expected InputRoots, got {other:?}"),
        }
        assert!(
            !err.is_permanent(),
            "indexer lag is node/store-state, not derivation-intrinsic — must stay \
             InfrastructureFailure so the scheduler retries instead of poisoning"
        );
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

        assert!(matches!(
            err,
            ExecutorError::MetadataFetch { ref source, .. }
                if source.code() == tonic::Code::NotFound
        ));
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

        assert!(matches!(err, ExecutorError::InvalidDerivation(_)));
        assert!(
            err.to_string().contains("failed to parse .drv from NAR"),
            "got: {err}"
        );
        Ok(())
    }
}
