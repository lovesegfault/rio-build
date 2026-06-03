//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.
// r[impl builder.fod.verify-hash+2]

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
    /// Returns None for unknown algos — the caller (`verify_fod_hashes`)
    /// fails closed and rejects the output rather than shipping
    /// unverified content; the gateway pre-screens the same algorithm
    /// set at submission (`fod_algo_verifiable`), so a None here in
    /// production means that gate was bypassed or has drifted — or that
    /// the derivation was admitted under the gateway's realized-outputs
    /// exemption and its outputs were lost (e.g. GC'd) between the
    /// submission-time probe and dispatch, in which case failing the
    /// build here is exactly the intended fail-closed behavior.
    fn from_nix_str(s: &str) -> Option<Self> {
        match s.strip_prefix("r:").unwrap_or(s) {
            "sha1" => Some(Self::Sha1),
            "sha256" => Some(Self::Sha256),
            "sha512" => Some(Self::Sha512),
            _ => None,
        }
    }

    /// The corresponding `rio_nix::hash::HashAlgo` — used to decode the
    /// DECLARED hash with the shared length-discriminated parser, while
    /// this enum drives the local hash COMPUTATION.
    fn as_nix_hash_algo(self) -> rio_nix::hash::HashAlgo {
        match self {
            Self::Sha1 => rio_nix::hash::HashAlgo::SHA1,
            Self::Sha256 => rio_nix::hash::HashAlgo::SHA256,
            Self::Sha512 => rio_nix::hash::HashAlgo::SHA512,
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
fn compute_local_flat_hash(path: &Path, algo: FodHashAlgo) -> anyhow::Result<Vec<u8>> {
    fn with<D: sha2::Digest>(path: &Path) -> anyhow::Result<Vec<u8>> {
        use anyhow::Context;
        let mut f = std::fs::File::open(path)
            .with_context(|| format!("failed to open FOD output {}", path.display()))?;
        let mut w = DigestWriter { digest: D::new() };
        std::io::copy(&mut f, &mut w)?;
        Ok(w.digest.finalize().to_vec())
    }
    match algo {
        FodHashAlgo::Sha1 => with::<sha1::Sha1>(path),
        FodHashAlgo::Sha256 => with::<sha2::Sha256>(path),
        FodHashAlgo::Sha512 => with::<sha2::Sha512>(path),
    }
}

/// Verify FOD output hashes match the declared outputHash.
///
/// This is the **sole** verifier and the integrity boundary that makes
/// the fetcher's open egress safe (`fetcher.upload.hash-verify-before`):
/// nothing downstream re-checks the content, so it is fail-closed — an
/// output whose declared `outputHashAlgo` we cannot verify is rejected,
/// never skipped.
///
/// Dispatches on outputHashAlgo (sha1/sha256/sha512) and computes the
/// hash LOCALLY before upload — a bad output is rejected before it
/// enters the store.
///
/// For `r:<algo>` (recursive): hash the NAR serialization of the output
/// path. For `<algo>` (flat): hash the file contents directly — and,
/// like CppNix, require the output to be exactly one non-executable
/// regular file (an executable and a non-executable file with the same
/// bytes would collide on one store path, so the shape is part of the
/// contract, not just the bytes).
///
/// `upper_store` is `{overlay_upper}/nix/store` — callers pass
/// `OverlayMount::upper_store()`.
///
/// Blocking I/O (filesystem reads + hashing). Call via `spawn_blocking`.
pub(super) fn verify_fod_hashes(drv: &Derivation, upper_store: &Path) -> anyhow::Result<()> {
    use anyhow::{Context, bail};

    for output in drv.outputs() {
        // Only fixed outputs carry a declared hash (typed dispatch).
        let rio_nix::derivation::OutputKind::Fixed {
            path: typed_path, ..
        } = output.kind()
        else {
            continue;
        };

        // Dispatch on outputHashAlgo. Unknown algo → reject. This gate
        // is the only content verification between an egress-open
        // fetcher and the signed cache; an algorithm we cannot verify
        // (md5, or garbage in a hand-written .drv) must fail the build
        // rather than ship unverified content. The gateway additionally
        // rejects such derivations at submission so the failure lands on
        // the client instead of burning a fetcher pod.
        let Some(algo) = FodHashAlgo::from_nix_str(output.hash_algo()) else {
            bail!(
                "FOD output '{}' declares unsupported hash algorithm '{}' \
                 (supported: sha1, sha256, sha512, each optionally prefixed 'r:'); \
                 refusing to upload unverified fetched content",
                output.name(),
                output.hash_algo(),
            );
        };

        // Decode the declared hash with the shared length-discriminated
        // parser (base16 / nixbase32 / base64) — the same function every
        // other component uses, so a declaration accepted at submission
        // can never fail to decode here.
        // r[impl nix.hash.fod-decode+1]
        let expected =
            rio_nix::hash::NixHash::parse_nonsri_unprefixed(algo.as_nix_hash_algo(), output.hash())
                .with_context(|| {
                    format!(
                        "FOD outputHash {:?} is not a valid base16, nixbase32, or base64 hash",
                        output.hash()
                    )
                })?
                .digest()
                .to_vec();

        let is_recursive = output.hash_algo().starts_with("r:");

        // The basename comes from the TYPED declared path — no free
        // string re-derivation over a declared output path remains.
        let fs_path = upper_store.join(typed_path.basename());

        let computed = if is_recursive {
            // Compute NAR hash locally (before upload) so a bad
            // output is rejected without entering the store.
            compute_local_nar_hash(&fs_path, algo)?
        } else {
            // CppNix rejects a flat fixed-output that is not exactly one
            // non-executable regular file even when the bytes hash
            // correctly (`derivation-builder.cc`, CAFixed/flat branch):
            // an executable and a non-executable file with identical
            // bytes would otherwise collide on the same store path. The
            // floating-CA pipeline already enforces the same rule
            // (`CaFlatNotSingleFile` in `native_result/ca.rs`); mirror
            // it here. lstat so a symlink is reported as a symlink
            // instead of being followed to its target's bytes.
            let md = std::fs::symlink_metadata(&fs_path)
                .with_context(|| format!("failed to stat FOD output {}", fs_path.display()))?;
            if md.file_type().is_symlink() || !md.is_file() {
                bail!(
                    "FOD flat output '{}' must be a single regular file \
                     (outputHashMode=flat), but {} is a {}",
                    output.name(),
                    output.path(),
                    if md.file_type().is_symlink() {
                        "symlink"
                    } else {
                        "non-regular file"
                    },
                );
            }
            {
                use std::os::unix::fs::PermissionsExt;
                let mode = md.permissions().mode();
                if mode & 0o111 != 0 {
                    bail!(
                        "FOD flat output '{}' must be a non-executable regular \
                         file (outputHashMode=flat), but {} has mode {:o}; an \
                         executable and a non-executable file with the same \
                         bytes would collide on one store path, so CppNix \
                         rejects this shape",
                        output.name(),
                        output.path(),
                        mode & 0o7777,
                    );
                }
            }
            // Flat hash — stream file contents through a digest
            // sink. Same O(1)-memory contract as the recursive
            // branch above (see compute_local_flat_hash doc).
            compute_local_flat_hash(&fs_path, algo)?
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

/// I-110c: one `BatchGetManifest` for the full input closure, then
/// prime the FUSE cache's hint map so each JIT FUSE `GetPath` carries
/// `manifest_hint` and the store skips its two PG lookups. ~1600 PG
/// hits/builder → ≤2.
///
/// Hints for paths that turn out to be already on local disk are
/// dropped by the cache-hit fast path in `ensure_cached` /
/// `prefetch_path_blocking` — same code that decides hit-vs-miss, so
/// the map drains as JIT lookups fire with no leak.
///
/// Any error degrades to a no-op — each per-path `GetPath` then
/// queries PG as before. Prefetch is an optimization; it never fails
/// the build.
// r[impl builder.warmgate.manifest-prime]
#[instrument(skip_all, fields(input_count = input_paths.len()))]
pub(super) async fn prefetch_manifests(
    store_client: &StoreServiceClient<Channel>,
    fuse_cache: &crate::fuse::cache::Cache,
    input_paths: &[String],
) {
    if input_paths.is_empty() {
        return;
    }
    // No local-cache filter: already-cached paths get their unused
    // hint dropped by the cache-hit fast path in `ensure_cached` /
    // `prefetch_path_blocking` — same code that decides hit-vs-miss,
    // so no leak and no race.

    let mut client = store_client.clone();
    match rio_proto::client::batch_get_manifest(
        &mut client,
        input_paths.to_vec(),
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
    )
    .await
    {
        Ok(entries) => {
            let hints = entries.into_iter().filter_map(|(path, hint)| {
                let basename = rio_nix::store_path::basename(&path)?.to_owned();
                Some((basename, hint?))
            });
            fuse_cache.prime_manifest_hints(hints);
            tracing::debug!(paths = input_paths.len(), "manifest prefetch primed");
        }
        Err(status) => {
            // Any failure (Unavailable, DeadlineExceeded, …) — log and
            // continue. The per-path JIT GetPath has its own retry;
            // this is a best-effort optimization.
            tracing::warn!(
                error = %status,
                "BatchGetManifest failed; per-path GetPath will query PG"
            );
        }
    }
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
) -> Result<(Derivation, String), ExecutorError> {
    // .drv files are small (KB range), but wrap in stream timeout: this is
    // the first gRPC call after setup_overlay, so a stalled store would hang
    // the build with an overlay mount held indefinitely.
    //
    // Returns the parsed derivation AND its ATerm text: the text feeds
    // the request glue's graph-derivation table (the glue itself holds
    // no filesystem capability — every input byte arrives as a
    // parameter resolved here, at the resolve step).
    let text = fetch_drv_text(store_client, drv_path).await?;
    let drv = Derivation::parse(&text)
        .map_err(|e| ExecutorError::InvalidDerivation(format!("failed to parse .drv: {e}")))?;
    Ok((drv, text))
}

/// Assemble the request glue's graph-derivation table: the main
/// derivation's text plus the texts the glue's `exportReferencesGraph`
/// expansion will actually read — exactly the [`DrvTextDemand`]
/// derived from the build's own declaration. The full input closure is
/// deliberately NOT a parameter: a resolver that cannot see
/// `input_paths` cannot regress into fetching by closure membership.
///
/// Scale arithmetic (the I-110 multiplication this replaces): a
/// nixpkgs-scale closure carries on the order of 2,000 transitive
/// `.drv`s, and the previous closure-membership prefetch fetched every
/// one of them not retained from the input-drv loop — per build,
/// whether or not anything would read them, across hundreds of
/// ephemeral builders. Demand is empty for every build with no
/// `exportReferencesGraph` declaration, which is nearly all of them;
/// `rio_builder_graph_drv_fetch_total` counts the demanded fetches and
/// is the post-wipe resurgence alarm (expected ~0 fleet-wide).
///
/// Texts are fetched RAW (no parse): the previous eager
/// `Derivation::parse` discarded its result — the glue lazily
/// re-parses at consumption with its own consumption-scoped error
/// (`ExportRefsDrvUnreadable`).
///
/// Failures: store stream faults and missing demanded paths are
/// worker-local input-materialization faults (`MetadataFetch` →
/// `InfrastructureFailure` → scheduler re-dispatch). Extract/UTF-8
/// failures are permanent (`InvalidDerivation`) — justified by
/// consumption: every demanded byte WILL be read by the glue, and a
/// `.drv` that is not UTF-8 ATerm can never become readable by
/// retrying elsewhere.
// r[impl builder.result.input-materialization-is-infra+4]
// r[impl builder.glue.pure]
// r[impl builder.glue.drv-table-demand]
pub(super) async fn fetch_demanded_graph_drvs(
    store_client: &StoreServiceClient<Channel>,
    main_drv_path: &str,
    main_drv_text: &str,
    mut table: std::collections::BTreeMap<String, String>,
    demand: &super::glue::refs_graph::DrvTextDemand,
) -> Result<std::collections::BTreeMap<String, String>, ExecutorError> {
    use futures_util::stream::{StreamExt as _, TryStreamExt as _};

    table.insert(main_drv_path.to_owned(), main_drv_text.to_owned());

    let demanded: Vec<String> = demand
        .iter()
        .filter(|p| !table.contains_key(*p))
        .map(str::to_owned)
        .collect();
    if demanded.is_empty() {
        return Ok(table);
    }
    metrics::counter!("rio_builder_graph_drv_fetch_total").increment(demanded.len() as u64);
    // info, not debug: this is the journal-observable half of
    // `rio_builder_graph_drv_fetch_total` (expected ~0 fleet-wide; the
    // VM suite asserts the line is ABSENT across a whole scenario, ERG
    // builds included). Reaching here at all is rare even for ERG
    // builds: Nix's drvPath context makes a declared graph's `.drv`s
    // direct inputDrvs, whose texts the input-drv loop already
    // retained — this fetch only fires for graphs whose closure
    // reaches a `.drv` that is NOT an input drv (e.g. an output
    // embedding a derivation path).
    tracing::info!(
        n_demanded = demanded.len(),
        "fetching declaration-demanded graph .drv texts for the request glue"
    );
    let fetched: Vec<(String, String)> = futures_util::stream::iter(demanded)
        .map(|path| {
            let mut client = store_client.clone();
            async move {
                let text = fetch_drv_text(&mut client, &path).await?;
                Ok::<_, ExecutorError>((path, text))
            }
        })
        .buffer_unordered(super::MAX_PARALLEL_FETCHES)
        .try_collect()
        .await?;
    table.extend(fetched);
    Ok(table)
}

/// Fetch a `.drv`'s ATerm text from the store WITHOUT parsing it (the
/// demanded-graph table wants bytes; the glue parses at consumption).
/// Same classification as [`fetch_drv_from_store`]: stream faults and
/// missing paths are `MetadataFetch` (infra); extract/UTF-8 failures
/// are permanent — and so is exceeding `MAX_DRV_NAR_BYTES`: a
/// "derivation" bigger than the class cap is hostile or corrupt input
/// that no re-dispatch will shrink. The cap also bounds buffering:
/// with [`super::MAX_PARALLEL_FETCHES`]-way concurrency the previous
/// general-NAR bound (4 GiB) let one build buffer tens of GiB of
/// tenant-controlled bytes (round-16 bug_095); the collector's leading
/// `Info.nar_size` pre-check rejects an oversized declaration before
/// pulling a single chunk.
async fn fetch_drv_text(
    store_client: &mut StoreServiceClient<Channel>,
    drv_path: &str,
) -> Result<String, ExecutorError> {
    let result = rio_proto::client::get_path_nar(
        store_client,
        drv_path,
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        rio_common::limits::MAX_DRV_NAR_BYTES,
        None,
        &[],
    )
    .await
    .map_err(|e| match e {
        rio_proto::client::NarCollectError::SizeExceeded { got, limit } => {
            ExecutorError::InvalidDerivation(format!(
                ".drv NAR for {drv_path} is {got} bytes, exceeding the \
                 {limit}-byte derivation-text cap"
            ))
        }
        rio_proto::client::NarCollectError::Stream(s) => ExecutorError::MetadataFetch {
            path: drv_path.to_string(),
            source: s,
        },
        other => ExecutorError::MetadataFetch {
            path: drv_path.to_string(),
            source: tonic::Status::internal(other.to_string()),
        },
    })?;

    let Some((_, nar_data)) = result else {
        return Err(ExecutorError::MetadataFetch {
            path: drv_path.to_string(),
            source: tonic::Status::not_found(".drv not found in store"),
        });
    };

    let bytes = rio_nix::nar::extract_single_file(&nar_data).map_err(|e| {
        ExecutorError::InvalidDerivation(format!("failed to extract .drv from NAR: {e}"))
    })?;
    String::from_utf8(bytes).map_err(|e| {
        ExecutorError::InvalidDerivation(format!(".drv content is not valid UTF-8: {e}"))
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

    // I-106: keep the full PathInfo from each BFS query so downstream
    // consumers (the request glue's closure planning, FUSE prefetch
    // sizing, output policy checks) don't have to re-query the same
    // ~800 paths. Under ephemeral-builder load that second pass was a
    // ~800 × N-builders QueryPathInfo burst that exhausted the store's
    // PG pool.
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

// r[verify builder.fod.verify-hash+2]
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
        // Declared output paths must parse as store paths (typed
        // boundary), so the on-disk name carries a valid hash prefix.
        let store_name = format!("gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-{basename}");
        let (_tmp, store_dir) = seed_output(&store_name, &content)?;

        let declared = if declare_correct {
            correct_fod_hash(&store_dir, &store_name, &content, algo)?
        } else {
            // Wrong hash: all-zero digest of the correct width.
            let width = correct_fod_hash(&store_dir, &store_name, &content, algo)?.len();
            "0".repeat(width)
        };
        let drv = make_fod_drv(&format!("/nix/store/{store_name}"), algo, &declared);

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
        let (_tmp, store_dir) =
            seed_output("gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-large", &content)?;
        let expected = hex::encode(sha2::Sha256::digest(&content));
        let drv = make_fod_drv(
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-large",
            "sha256",
            &expected,
        );
        verify_fod_hashes(&drv, &store_dir)
    }

    /// CppNix rejects a flat FOD output that is not a single
    /// NON-EXECUTABLE regular file even when the bytes hash correctly:
    /// executability is part of the contract because two files with
    /// identical bytes but different modes would collide on one store
    /// path.
    #[test]
    fn test_verify_fod_flat_executable_rejected() -> anyhow::Result<()> {
        use std::os::unix::fs::PermissionsExt;
        let content = b"#!/bin/sh\necho fetched\n";
        let (_tmp, store_dir) = seed_output("gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-exec", content)?;
        std::fs::set_permissions(
            store_dir.join("gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-exec"),
            std::fs::Permissions::from_mode(0o755),
        )?;
        // The declared hash is CORRECT for the bytes — only the shape is wrong.
        let declared = correct_fod_hash(
            &store_dir,
            "gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-exec",
            content,
            "sha256",
        )?;
        let drv = make_fod_drv(
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-exec",
            "sha256",
            &declared,
        );
        let err = verify_fod_hashes(&drv, &store_dir)
            .expect_err("executable flat FOD must be rejected like CppNix");
        assert!(
            err.to_string().contains("non-executable"),
            "error should explain the shape rule: {err}"
        );
        Ok(())
    }

    /// A symlink pointing at content whose bytes match the declared flat
    /// hash is still rejected: the output must BE a regular file, not
    /// point at one (CppNix lstats the output).
    #[test]
    fn test_verify_fod_flat_symlink_rejected() -> anyhow::Result<()> {
        let content = b"symlink target bytes";
        let (_tmp, store_dir) = seed_output("test-flat-target", content)?;
        std::os::unix::fs::symlink(
            store_dir.join("test-flat-target"),
            store_dir.join("gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-link"),
        )?;
        let declared = correct_fod_hash(
            &store_dir,
            "gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-link",
            content,
            "sha256",
        )?;
        let drv = make_fod_drv(
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-flat-link",
            "sha256",
            &declared,
        );
        let err = verify_fod_hashes(&drv, &store_dir)
            .expect_err("symlinked flat FOD must be rejected like CppNix");
        assert!(
            err.to_string().contains("regular file"),
            "error should mention the regular-file requirement: {err}"
        );
        Ok(())
    }

    /// Unknown algo (e.g., md5): fail-closed. This function is the sole
    /// content verifier for fetched outputs; an algorithm it cannot
    /// verify must reject the build, not skip the check.
    #[test]
    fn test_verify_fod_unknown_algo_rejected() -> anyhow::Result<()> {
        let (_tmp, store_dir) = seed_output("test-md5-fod", b"content")?;
        let drv = make_fod_drv(
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-md5-fod",
            "md5",
            "deadbeefdeadbeefdeadbeefdeadbeef",
        );

        let err = verify_fod_hashes(&drv, &store_dir)
            .expect_err("unknown algo must be rejected, not skipped");
        assert!(
            err.to_string().contains("unsupported hash algorithm 'md5'"),
            "error must name the algorithm: {err}"
        );
        Ok(())
    }

    #[test]
    fn test_verify_fod_non_fod_skipped() -> anyhow::Result<()> {
        // Non-FOD (no hash) should be skipped without error
        let drv = make_fod_drv(
            "/nix/store/gywi7jcdg67ms6vxnypxpn2rp2jm7ydi-non-fod",
            "",
            "",
        );
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
    /// fields are populated (not just path), proving downstream consumers
    /// (glue closure planning, FUSE prefetch sizing, policy checks) can
    /// use this directly.
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
            "nar_size populated (FUSE prefetch sizing and closure-size \
             policy checks need this) — proves we kept the full PathInfo, \
             not just the path string"
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

    /// I-110c: `prefetch_manifests` issues ONE BatchGetManifest then
    /// primes the FUSE cache's hint map (keyed by basename), and
    /// `fetch_extract_insert`'s GetPath carries the hint.
    #[tokio::test]
    async fn test_prefetch_manifests_primes_hint_cache() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let (store, client) = spawn_and_connect().await?;
        let (p_a, p_b) = (tp("hint-a"), tp("hint-b"));
        seed_with_refs(&store, &p_a, &[]);
        seed_with_refs(&store, &p_b, &[]);

        let dir = tempfile::tempdir()?;
        let cache = crate::fuse::cache::Cache::new(dir.path().join("c"))?;

        prefetch_manifests(&client, &cache, &[p_a.clone(), p_b.clone()]).await;

        assert_eq!(
            store.calls.batch_manifest_calls.load(Ordering::SeqCst),
            1,
            "one BatchGetManifest for the whole closure"
        );
        let b_a = rio_nix::store_path::basename(&p_a).unwrap();
        let b_b = rio_nix::store_path::basename(&p_b).unwrap();
        let hint_a = cache.take_manifest_hint(b_a).expect("hint primed for a");
        assert_eq!(
            hint_a.info.as_ref().map(|i| i.store_path.as_str()),
            Some(p_a.as_str()),
            "hint keyed by basename, info matches full path"
        );
        assert!(cache.take_manifest_hint(b_b).is_some());
        assert!(
            cache.take_manifest_hint(b_a).is_none(),
            "take removes on read"
        );

        // The hint-carry-on-GetPath e2e is covered by
        // `fuse::fetch::tests::test_prefetch_success_roundtrip` (which
        // builds `StoreClients` directly). After dataplane2 changed
        // `prefetch_path_blocking` to take `StoreClients` and JIT-fetch
        // deleted the warm path, this test's scope is the
        // BatchGetManifest → hint-map prime, asserted above.
        let _ = (store, client);
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

        let (drv, _text) = fetch_drv_from_store(&mut client, &drv_path)
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

    /// No double-fetch by construction: a table that already covers
    /// every demanded `.drv` (retained from the input-drv loop + the
    /// main text) makes the demand fetch a pure merge — proven by
    /// running it against a store where any fetch would 404.
    // r[verify builder.glue.pure]
    #[tokio::test]
    async fn demanded_texts_already_retained_pure_merge() -> anyhow::Result<()> {
        use crate::executor::glue::refs_graph::DrvTextDemand;
        let (_store, client) = spawn_and_connect().await?; // EMPTY store
        let main_drv = tp("main.drv");
        let dep_drv = tp("dep.drv");
        let mut table = std::collections::BTreeMap::new();
        table.insert(dep_drv.clone(), "Derive-dep".to_owned());

        // Both demanded texts are retained → pure merge, zero RPCs
        // (any fetch against the empty store would fail).
        let demand = DrvTextDemand::from_paths_for_tests([main_drv.clone(), dep_drv.clone()]);
        let table =
            fetch_demanded_graph_drvs(&client, &main_drv, "Derive-main", table, &demand).await?;
        assert_eq!(
            table.get(&main_drv).map(String::as_str),
            Some("Derive-main")
        );
        assert_eq!(table.get(&dep_drv).map(String::as_str), Some("Derive-dep"));
        assert_eq!(table.len(), 2);
        Ok(())
    }

    /// round-16 bug_095: a "derivation" whose declared NAR size
    /// exceeds MAX_DRV_NAR_BYTES is rejected as PERMANENT
    /// `InvalidDerivation` (hostile/corrupt input — re-dispatch cannot
    /// shrink it), not infra `MetadataFetch`; and the collector's
    /// Info-message pre-check means the rejection is byte-free (the
    /// mock serves the forged declared size with a tiny actual NAR —
    /// no oversized body is ever streamed or buffered).
    #[tokio::test]
    async fn oversized_drv_text_is_permanent_invalid_derivation() -> anyhow::Result<()> {
        use rio_common::limits::MAX_DRV_NAR_BYTES;
        let (store, mut client) = spawn_and_connect().await?;
        let big_drv = tp("huge.drv");
        let (nar, hash) = make_nar(b"Derive-tiny-but-lying");
        let mut info = make_path_info(&big_drv, &nar, hash);
        info.nar_size = MAX_DRV_NAR_BYTES + 1; // forged declared size
        store.seed(info, nar);

        let err = fetch_drv_text(&mut client, &big_drv)
            .await
            .expect_err("over-cap declared .drv NAR must be rejected");
        match &err {
            ExecutorError::InvalidDerivation(msg) => {
                assert!(
                    msg.contains(&MAX_DRV_NAR_BYTES.to_string()),
                    "rejection names the derivation-text cap: {msg}"
                );
            }
            other => panic!("expected permanent InvalidDerivation, got {other:?}"),
        }

        // Control: an honest small .drv on the same store still fetches.
        let ok_drv = tp("ok.drv");
        let (nar2, hash2) = make_nar(b"Derive-honest");
        store.seed(make_path_info(&ok_drv, &nar2, hash2), nar2);
        let text = fetch_drv_text(&mut client, &ok_drv).await?;
        assert_eq!(text, "Derive-honest");
        Ok(())
    }

    /// THE bug_081 kill shot: with no exportReferencesGraph
    /// declaration the demand is empty, so input resolution fetches
    /// NOTHING — proven against an EMPTY store where any fetch 404s.
    /// The replaced closure-membership prefetch deterministically
    /// errored here (it fetched every closure `.drv` whether or not
    /// anything would read it).
    // r[verify builder.glue.drv-table-demand]
    #[tokio::test]
    async fn no_declaration_fetches_nothing_at_depth_3() -> anyhow::Result<()> {
        use crate::executor::glue::refs_graph::DrvTextDemand;
        let (_store, client) = spawn_and_connect().await?; // EMPTY store
        let main_drv = tp("main.drv");
        // A 3-deep closure of .drvs, none retained, none in the store.
        // Old code: 3 fetches → 3 NotFound failures. New code cannot
        // fetch them BY TYPE: they are not in the demand.
        let demand = DrvTextDemand::from_paths_for_tests([]);
        let table = fetch_demanded_graph_drvs(
            &client,
            &main_drv,
            "Derive-main",
            std::collections::BTreeMap::new(),
            &demand,
        )
        .await?;
        assert_eq!(table.len(), 1, "only the main drv text");
        Ok(())
    }

    /// An undemanded sibling `.drv` is never fetched: only the
    /// demanded path is in the store; success proves the sibling
    /// (whose fetch would 404 → MetadataFetch error) was never
    /// requested.
    // r[verify builder.glue.drv-table-demand]
    #[tokio::test]
    async fn undemanded_sibling_never_fetched() -> anyhow::Result<()> {
        use crate::executor::glue::refs_graph::DrvTextDemand;
        let (store, client) = spawn_and_connect().await?;
        let main_drv = tp("main.drv");
        let demanded = tp("demanded.drv");
        let aterm = r#"Derive([("out","/nix/store/llllllllllllllllllllllllllllllll-r","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let (nar, hash) = make_nar(aterm.as_bytes());
        store.seed(make_path_info(&demanded, &nar, hash), nar);
        // The sibling exists in the build's closure conceptually, but
        // is NOT seeded — a fetch of it would fail the resolve.

        let demand = DrvTextDemand::from_paths_for_tests([demanded.clone()]);
        let table = fetch_demanded_graph_drvs(
            &client,
            &main_drv,
            "Derive-main",
            std::collections::BTreeMap::new(),
            &demand,
        )
        .await?;
        assert_eq!(table.get(&demanded).map(String::as_str), Some(aterm));
        assert_eq!(table.len(), 2, "main + demanded only");
        Ok(())
    }

    /// 2,000-drv closure, all demanded texts retained: zero RPCs
    /// (empty store), instant. The scale IS the verification — the
    /// I-110 shape was per-build whole-closure refetch.
    // r[verify builder.glue.drv-table-demand]
    #[tokio::test]
    async fn two_thousand_drv_demand_retained_zero_rpcs() -> anyhow::Result<()> {
        use crate::executor::glue::refs_graph::DrvTextDemand;
        let (_store, client) = spawn_and_connect().await?; // EMPTY store
        let main_drv = tp("main.drv");
        let mut table = std::collections::BTreeMap::new();
        let mut demanded = Vec::new();
        for i in 0..2000 {
            let p = tp(&format!("dep-{i}.drv"));
            table.insert(p.clone(), format!("Derive-{i}"));
            demanded.push(p);
        }
        let demand = DrvTextDemand::from_paths_for_tests(demanded);
        let table =
            fetch_demanded_graph_drvs(&client, &main_drv, "Derive-main", table, &demand).await?;
        assert_eq!(table.len(), 2001);
        Ok(())
    }

    /// A demanded `.drv` the table does not cover is fetched RAW; a
    /// store failure there is a MetadataFetch infrastructure fault
    /// (the resolve-step classification — never a glue rejection),
    /// and a demanded path whose bytes are not UTF-8 ATerm is
    /// permanent (consumption-backed: the glue WILL read these bytes,
    /// and they can never become readable by retrying elsewhere).
    // r[verify builder.result.input-materialization-is-infra+4]
    // r[verify builder.glue.drv-table-demand]
    #[tokio::test]
    async fn demanded_fetch_and_error_classification() -> anyhow::Result<()> {
        use crate::executor::glue::refs_graph::DrvTextDemand;
        let (store, client) = spawn_and_connect().await?;
        let main_drv = tp("main.drv");
        let demanded = tp("residual.drv");
        let aterm = r#"Derive([("out","/nix/store/llllllllllllllllllllllllllllllll-r","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#;
        let (nar, hash) = make_nar(aterm.as_bytes());
        store.seed(make_path_info(&demanded, &nar, hash), nar);

        // Demanded + present: fetched into the table, raw text.
        let table = fetch_demanded_graph_drvs(
            &client,
            &main_drv,
            "Derive-main",
            std::collections::BTreeMap::new(),
            &DrvTextDemand::from_paths_for_tests([demanded.clone()]),
        )
        .await?;
        assert_eq!(table.get(&demanded).map(String::as_str), Some(aterm));

        // Demanded + missing: MetadataFetch (infra), not InvalidDerivation.
        let missing = tp("missing.drv");
        let err = fetch_demanded_graph_drvs(
            &client,
            &main_drv,
            "Derive-main",
            std::collections::BTreeMap::new(),
            &DrvTextDemand::from_paths_for_tests([missing.clone()]),
        )
        .await
        .expect_err("missing demanded .drv must fail the resolve step");
        assert!(
            matches!(err, ExecutorError::MetadataFetch { ref path, .. } if *path == missing),
            "got: {err}"
        );

        // Demanded + non-UTF-8 bytes: permanent (InvalidDerivation).
        let invalid = tp("invalid.drv");
        let (bad_nar, bad_hash) = make_nar(&[0xff, 0xfe, 0x00, 0x80]);
        store.seed(make_path_info(&invalid, &bad_nar, bad_hash), bad_nar);
        let err = fetch_demanded_graph_drvs(
            &client,
            &main_drv,
            "Derive-main",
            std::collections::BTreeMap::new(),
            &DrvTextDemand::from_paths_for_tests([invalid.clone()]),
        )
        .await
        .expect_err("non-UTF-8 demanded .drv is a permanent input fault");
        assert!(
            matches!(err, ExecutorError::InvalidDerivation(_)),
            "got: {err}"
        );

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
            err.to_string().contains("failed to extract .drv from NAR"),
            "got: {err}"
        );
        Ok(())
    }
}
