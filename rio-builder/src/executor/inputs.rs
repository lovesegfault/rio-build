//! Input fetching: .drv from store, metadata, input closure, FOD hash verification.
// r[impl builder.fod.verify-hash]

use std::future::Future;
use std::path::{Path, PathBuf};
use std::sync::Arc;
use std::sync::atomic::{AtomicUsize, Ordering};
use std::time::Duration;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use tonic::transport::Channel;
use tracing::instrument;

use rio_nix::derivation::Derivation;
use rio_proto::StoreServiceClient;

use crate::fuse::cache::Cache;
use crate::fuse::fetch::{PrefetchSkip, prefetch_path_blocking};
use crate::synth_db::SynthPathInfo;

use super::{ExecutorError, MAX_PARALLEL_FETCHES};

/// FUSE-warm concurrency. Bounds concurrent `GetPath` gRPC fetches fired
/// at the store during warm. I-165c: no longer tied to `fuse_threads`
/// (warm doesn't go through the kernel FUSE layer), but kept moderate —
/// the I-165 thundering-herd that motivated the deadline was 86 builders
/// warming simultaneously; per-builder fan-out stays small so the
/// aggregate doesn't re-saturate the store. See [`warm_inputs_in_fuse`].
pub(super) const WARM_FUSE_CONCURRENCY: usize = 4;

/// Floor of the overall deadline for [`warm_inputs_in_fuse`]. Well above
/// happy-path p99 warm (`rio_builder_input_warm_duration_seconds`
/// histogram), well below the 300s store-side `GRPC_STREAM_TIMEOUT`.
/// I-165: without a deadline, a saturated store backs up every fuser
/// thread in `block_on(GetPath)` and warm hangs for `fetch_timeout ×
/// ceil(inputs / fetch_sem)` — observed 47 min at 86-builder thundering
/// herd. Warm is best-effort; a partial warm that later hits the I-043
/// negative-dentry race is recoverable, a 47-min uncancellable hang is
/// not.
///
/// I-178: this is now the FLOOR. The effective deadline is
/// [`warm_overall_deadline`] — `max(this, Σnar_size / (MIN_THROUGHPUT ×
/// CONCURRENCY) + 30s)` — so a single 1.9 GB input doesn't detach the
/// warm thread mid-fetch.
pub(super) const WARM_OVERALL_DEADLINE: Duration = Duration::from_secs(90);

/// Minimum expected store→builder throughput for warm-timeout sizing.
/// I-178: 15 MiB/s is a conservative floor — half the ~30 MB/s observed
/// in cluster (`rio_builder_fuse_fetch_bytes_total` ÷
/// `rio_builder_fuse_fetch_duration_seconds`). A 1.9 GB NAR at this
/// floor needs ≈127 s; the previous flat 60 s per-path timeout aborted
/// the fetch mid-stream → daemon ENOENT → PermanentFailure poison.
///
/// Tune DOWN if `rio_builder_input_materialization_failures_total` is
/// sustained nonzero (means real throughput is below this floor —
/// cross-AZ builders, S3 throttle).
pub(super) const WARM_MIN_THROUGHPUT_BPS: u64 = 15 * 1024 * 1024;

/// Per-path warm timeout: `max(base, nar_size / MIN_THROUGHPUT)`.
///
/// `base` is `fuse_fetch_timeout` (60 s) so small paths are unchanged
/// from pre-I-178 behavior. Large paths get a size-proportional budget.
/// This is the WARM path only — the FUSE-callback path
/// (`ensure_cached`'s `self.fetch_timeout`) stays at the flat 60 s per
/// the I-165 circuit-breaker rationale (config.rs:89-105).
// r[impl builder.input.warm-bounded+3]
pub(super) fn warm_per_path_timeout(base: Duration, nar_size: u64) -> Duration {
    base.max(Duration::from_secs(
        nar_size.div_ceil(WARM_MIN_THROUGHPUT_BPS),
    ))
}

/// Overall warm deadline: `max(WARM_OVERALL_DEADLINE, Σnar_size /
/// (MIN_THROUGHPUT × CONCURRENCY) + 30s slop)`.
///
/// Without this a single large input would still detach at the 90 s
/// wall even though its per-path timeout is 127 s — survivable post-
/// I-165c (the detached userspace thread completes and the daemon's
/// `WaitFor` wakes to a populated cache), but it spuriously bumps
/// `rio_builder_input_warm_timeout_total` and burns a blocking-pool
/// slot across the daemon-spawn boundary.
pub(super) fn warm_overall_deadline(total_nar_bytes: u64) -> Duration {
    let throughput_bound = Duration::from_secs(
        total_nar_bytes.div_ceil(WARM_MIN_THROUGHPUT_BPS * WARM_FUSE_CONCURRENCY as u64),
    ) + Duration::from_secs(30);
    WARM_OVERALL_DEADLINE.max(throughput_bound)
}

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

/// Stat each input path through the FUSE mount before nix-daemon spawns.
///
/// I-043: overlayfs caches negative dentries from its lowers.
/// nix-daemon's first `stat(/nix/store/{input})` goes overlay → upper
/// miss → FUSE lower; if FUSE returns ENOENT (path uploaded by another
/// builder mid-build, not yet in this builder's FUSE cache), the
/// overlay caches a negative dentry. The daemon's RETRY hits that
/// cache — FUSE `lookup()` is never called again. Live: zlib failed
/// `build input ... does not exist` for autotools-hook with zero FUSE
/// fetch logs during the failure window.
///
/// Warming materializes every input in FUSE before the overlay sees a
/// stat for it. Stat goes through the FUSE mount (NOT the overlay
/// merged dir), so the kernel's FUSE-layer dcache gets a positive
/// entry; the overlay's later lookup of the same name reuses that
/// FUSE-layer dentry without re-entering FUSE userspace.
///
/// I-165c: warming is two-stage. (1) `prefetch_path_blocking` materializes
/// the path on local disk via the cache's own gRPC+NAR-extract pipeline —
/// the SAME code FUSE `lookup()` would call, but invoked directly so the
/// `spawn_blocking` thread is in USERSPACE `block_on(gRPC)`, not kernel
/// D-state `request_wait_answer`. (2) Once the path is on disk, ONE
/// `symlink_metadata()` through the FUSE mount populates the kernel's
/// FUSE-layer dentry — that stat hits ops.rs `lookup()`'s local-disk fast
/// path (`child_path.symlink_metadata()` on `cache_dir`) and returns in
/// microseconds, never reaching `ensure_cached`.
///
/// Why this matters: I-165's deadline detaches in-flight `spawn_blocking`
/// futures on expiry. With the OLD stat-through-own-FUSE warm, those
/// detached threads were in kernel D-state — uncancellable, and if the
/// build later died by SIGKILL (OOM), `FuseMount::Drop` never ran → no
/// fusectl abort → D-state threads pinned the process forever (pod showed
/// `Running` masking the OOMKilled container). With direct prefetch, any
/// detached thread is in userspace, bounded by `fetch_timeout`, and dies
/// cleanly on SIGKILL.
///
/// I-110c: [`prefetch_manifests`] primes the FUSE cache's hint map BEFORE
/// this loop so each `GetPath` (now from `prefetch_path_blocking` instead
/// of FUSE-triggered `ensure_cached`) carries its manifest and the store
/// skips PG. ~1600 PG hits/builder → ≤2. The S3 chunk fetch stays
/// per-path (it's content; can't avoid).
///
/// `compute_input_closure` already verified each path exists in the
/// store via BatchQueryPathInfo (BFS only adds found paths), so the only window for
/// ENOENT here is a sub-200ms visibility race — exactly what
/// `fetch_extract_insert`'s NotFound re-probe covers. Warm failures
/// don't fail the build (the daemon will surface the real error if a
/// path is truly gone); they're WARN'd so a sustained nonzero metric
/// flags a wider problem upstream.
///
/// I-165: bounded by `deadline` ([`WARM_OVERALL_DEADLINE`] in production).
/// On expiry, in-flight `spawn_blocking` prefetches are detached (≤
/// `WARM_FUSE_CONCURRENCY` userspace threads leaked into the blocking
/// pool until their `fetch_timeout` fires); the build proceeds with
/// whatever subset warmed in time.
///
/// `fuse_cache: None` (unit tests without a live FUSE mount) skips stage
/// (1) and stats the tempdir directly — preserves the existing test shell
/// without needing a `Cache`+gRPC fixture for closure-walk coverage.
#[instrument(skip_all, fields(input_count = inputs.len()))]
pub(super) async fn warm_inputs_in_fuse(
    fuse_mount_point: &Path,
    fuse_cache: Option<&Arc<Cache>>,
    store_client: &StoreServiceClient<Channel>,
    base_fetch_timeout: Duration,
    inputs: &[(String, u64)],
    deadline: Duration,
) {
    // r[impl builder.input.warm-bounded+3]
    let cache = fuse_cache.cloned();
    let client = store_client.clone();
    warm_inputs_bounded(
        fuse_mount_point,
        inputs,
        deadline,
        move |basename, fuse_path, nar_size| {
            let cache = cache.clone();
            let client = client.clone();
            // I-178: per-path timeout scales with NAR size so a 1.9 GB
            // input gets ≈127 s instead of the flat 60 s that aborted
            // it mid-stream. `base_fetch_timeout` (the FUSE-callback
            // 60 s) is the floor so small paths are unchanged.
            //
            // TODO(I-180): `collect_nar_stream` buffers the whole NAR
            // in RAM before extraction; a tiny (1 Gi) builder OOMs on
            // a 1.8 GB input regardless of timeout. I-177's
            // size_class_floor promotes such builds; I-180 is the
            // streaming-extract structural fix.
            let per_path_timeout = warm_per_path_timeout(base_fetch_timeout, nar_size);
            async move {
                // Stage 1: materialize via the cache's own fetch path. The
                // spawn_blocking thread parks in userspace `block_on(gRPC)`
                // (Cache methods use `Handle::block_on` internally — calling
                // from async would nested-runtime panic). NOT kernel D-state.
                if let Some(cache) = cache {
                    let rt = tokio::runtime::Handle::current();
                    let bn = basename.clone();
                    let on_disk = tokio::task::spawn_blocking(move || {
                        prefetch_path_blocking(&cache, &client, &rt, per_path_timeout, &bn)
                    })
                    .await
                    .map_err(WarmErr::Join)?
                    .map_err(|errno| {
                        WarmErr::Io(
                            basename,
                            std::io::Error::from_raw_os_error(i32::from(errno)),
                        )
                    })?;
                    // AlreadyInFlight: another fetcher (PrefetchHint handler
                    // or a concurrent FUSE lookup) owns it. SKIP the dentry
                    // stat — it would D-state behind that fetcher's
                    // `block_on(GetPath)`. Safe for I-043: the daemon's own
                    // overlay→FUSE stat will hit `ensure_cached` → `WaitFor`
                    // → block until the fetcher finishes → return POSITIVE
                    // (overlay never sees ENOENT to negative-cache). Count
                    // as warmed: the path IS being materialized.
                    if matches!(on_disk, Some(PrefetchSkip::AlreadyInFlight)) {
                        return Ok(());
                    }
                    // Ok(None) = fetched; Some(AlreadyCached) = already on
                    // disk. Either way: stage 2's stat hits the fast path.
                }
                // Stage 2: populate the kernel FUSE-layer dentry. Path is on
                // local disk (stage 1 just put it there or confirmed it), so
                // FUSE `lookup()` returns from its `child_path.symlink_
                // metadata()` fast path in microseconds — no `ensure_cached`,
                // no gRPC, no D-state risk.
                tokio::task::spawn_blocking(move || {
                    fuse_path
                        .symlink_metadata()
                        .map(|_| ())
                        .map_err(|e| WarmErr::Io(fuse_path.display().to_string(), e))
                })
                .await
                .map_err(WarmErr::Join)?
            }
        },
    )
    .await;
}

/// Per-path warm failure. Carries enough context for the WARN log; the
/// distinction between Io and Join is just log-shape (both increment the
/// same `failed` counter).
enum WarmErr {
    /// `prefetch_path_blocking` errno or `symlink_metadata` error.
    Io(String, std::io::Error),
    /// `spawn_blocking` panicked.
    Join(tokio::task::JoinError),
}

/// Core of [`warm_inputs_in_fuse`] with the per-path operation
/// injected. Split out so the deadline can be unit-tested with a
/// deterministically-stalling `warm_one` — the production path's stall
/// point is `block_on(GetPath)` inside `prefetch_path_blocking`, which
/// needs a live store mock to reproduce; the timeout path is otherwise
/// untestable without that fixture.
async fn warm_inputs_bounded<F, Fut>(
    fuse_mount_point: &Path,
    inputs: &[(String, u64)],
    deadline: Duration,
    warm_one: F,
) where
    F: Fn(String, PathBuf, u64) -> Fut,
    Fut: Future<Output = Result<(), WarmErr>>,
{
    let warm_start = std::time::Instant::now();

    // Owned (basename, fuse_path, nar_size) triples collected up-front
    // so the spawned futures are 'static (closures move owned data in;
    // no borrow on `inputs` survives the .await).
    let warm_paths: Vec<(String, PathBuf, u64)> = inputs
        .iter()
        .filter_map(|(p, nar_size)| {
            // Malformed paths (no /nix/store/ prefix) would have failed
            // compute_input_closure's QueryPathInfo with InvalidArgument
            // already; skip silently as defense-in-depth.
            let basename = p.strip_prefix(rio_nix::store_path::STORE_PREFIX)?;
            Some((
                basename.to_owned(),
                fuse_mount_point.join(basename),
                *nar_size,
            ))
        })
        .collect();
    let total = warm_paths.len();

    // Atomics not a collected Vec: `tokio::time::timeout` drops the
    // inner future on expiry, so a `.collect::<Vec<_>>()` would lose
    // the partial result. Counters survive the drop.
    let warmed = AtomicUsize::new(0);
    let failed = AtomicUsize::new(0);
    let warm_all = stream::iter(warm_paths)
        .map(|(basename, fuse_path, nar_size)| {
            let fut = warm_one(basename, fuse_path, nar_size);
            async {
                match fut.await {
                    Ok(()) => {
                        warmed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(WarmErr::Io(what, e)) => {
                        tracing::warn!(
                            path = %what,
                            error = %e,
                            "FUSE input warm failed; daemon's overlay lookup may negative-cache this input"
                        );
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                    Err(WarmErr::Join(join_err)) => {
                        tracing::warn!(error = %join_err, "FUSE warm task panicked");
                        failed.fetch_add(1, Ordering::Relaxed);
                    }
                }
            }
        })
        .buffer_unordered(WARM_FUSE_CONCURRENCY)
        .for_each(|()| async {});

    let timed_out = tokio::time::timeout(deadline, warm_all).await.is_err();

    let warmed = warmed.load(Ordering::Relaxed);
    let failed = failed.load(Ordering::Relaxed);
    let elapsed = warm_start.elapsed();
    metrics::histogram!("rio_builder_input_warm_duration_seconds").record(elapsed.as_secs_f64());
    metrics::counter!("rio_builder_input_warm_failures_total").increment(failed as u64);
    if timed_out {
        // I-165: count separately from per-path failures so a sustained
        // nonzero rate flags store-side saturation (vs. individual
        // missing-path noise on the failures counter).
        metrics::counter!("rio_builder_input_warm_timeout_total").increment(1);
        let skipped = total.saturating_sub(warmed + failed);
        tracing::warn!(
            warmed,
            failed,
            skipped,
            total,
            deadline_secs = deadline.as_secs(),
            elapsed_ms = elapsed.as_millis(),
            "FUSE input warm deadline exceeded; proceeding with partial warm \
             (un-warmed inputs may hit overlay negative-dentry race)"
        );
    } else {
        tracing::debug!(
            warmed,
            failed,
            elapsed_ms = elapsed.as_millis(),
            "FUSE input warm complete"
        );
    }
}

/// I-110c: one `BatchGetManifest` for the full input closure, then
/// prime the FUSE cache's hint map so each `GetPath` from the warm
/// loop carries `manifest_hint` and the store skips its two PG
/// lookups. ~1600 PG hits/builder → ≤2.
///
/// Hints for paths that turn out to be already on local disk are
/// dropped by the cache-hit fast path in `ensure_cached` /
/// `prefetch_path_blocking` — same code that decides hit-vs-miss, so
/// the map drains during the warm loop with no leak.
///
/// Backward compat: `Unimplemented` (store predates I-110c) and any
/// other error degrade to a no-op — the warm loop's per-path `GetPath`
/// then queries PG as before. Prefetch is an optimization; it never
/// fails the build.
#[instrument(skip_all, fields(input_count = input_paths.len()))]
pub(super) async fn prefetch_manifests(
    store_client: &StoreServiceClient<Channel>,
    fuse_cache: &crate::fuse::cache::Cache,
    input_paths: &[String],
) {
    if input_paths.is_empty() {
        return;
    }
    // No local-cache filter: `Cache::contains` is `#[cfg(test)]` +
    // block_on (would nested-runtime panic from async). Instead,
    // already-cached paths get their unused hint dropped by the
    // cache-hit fast path in `ensure_cached` / `prefetch_path_blocking`
    // — same code that decides hit-vs-miss, so no leak and no race.

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
                let basename = path
                    .strip_prefix(rio_nix::store_path::STORE_PREFIX)?
                    .to_owned();
                Some((basename, hint?))
            });
            fuse_cache.prime_manifest_hints(hints);
            tracing::debug!(paths = input_paths.len(), "manifest prefetch primed");
        }
        Err(status) if status.code() == tonic::Code::Unimplemented => {
            tracing::debug!(
                "store does not support BatchGetManifest; falling back to per-path \
                 GetPath PG lookup for FUSE warm (I-110c)"
            );
        }
        Err(status) => {
            // Any other failure (Unavailable, DeadlineExceeded, …) —
            // log and continue. The per-path GetPath in the warm loop
            // has its own retry; this is a best-effort optimization.
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
) -> Result<Derivation, ExecutorError> {
    // .drv files are small (KB range), but wrap in stream timeout: this is
    // the first gRPC call after setup_overlay, so a stalled store would hang
    // the build with an overlay mount held indefinitely.
    let result = rio_proto::client::get_path_nar(
        store_client,
        drv_path,
        rio_common::grpc::GRPC_STREAM_TIMEOUT,
        rio_common::limits::MAX_NAR_SIZE,
        None,
        &[],
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
/// never reached → never warmed → overlay negative-dentry.
#[instrument(skip_all)]
pub(super) async fn compute_input_closure(
    store_client: &StoreServiceClient<Channel>,
    drv: &Derivation,
    drv_path: &str,
    resolved_input_srcs: &std::collections::BTreeSet<String>,
) -> Result<Vec<SynthPathInfo>, ExecutorError> {
    use std::collections::HashSet;

    // I-106: keep the full PathInfo from each BFS query so callers
    // (synth_db generation in prepare_sandbox) don't have to re-query
    // the same ~800 paths. Under ephemeral-builder load that second
    // pass was a ~800 × N-builders QueryPathInfo burst that exhausted
    // the store's PG pool.
    let mut closure: HashSet<String> = HashSet::new();
    let mut metadata: Vec<SynthPathInfo> = Vec::new();
    let mut frontier: Vec<String> = Vec::new();
    let mut use_batch = true;

    // Seed: the .drv itself, input_drv paths (so nix-daemon can read them),
    // and resolved_input_srcs (input_srcs ∪ input_drv OUTPUTS — the caller
    // has already fetched each input .drv and extracted output paths).
    frontier.push(drv_path.to_string());
    frontier.extend(drv.input_drvs().keys().cloned());
    frontier.extend(resolved_input_srcs.iter().cloned());

    // BFS by layer. One BatchQueryPathInfo per layer (I-110); layer
    // count is typically 5-15 (dep depth).
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
        // SynthPathInfo (kept for the caller — I-106) or None on
        // not-found. References for the next layer come from
        // SynthPathInfo.references (already String).
        //
        // Backward compat: an older store returns Unimplemented for
        // the batch RPC; fall back to the per-path loop. `use_batch`
        // latches to false after the first Unimplemented so we don't
        // retry the batch every layer.
        let results: Vec<(String, Option<SynthPathInfo>)> =
            query_layer(store_client, batch, &mut use_batch).await?;

        // Add found paths to closure, collect their refs for next layer.
        for (path, info) in results {
            let Some(info) = info else {
                // Path not in store yet (output of a not-yet-built
                // input drv). FUSE will lazy-fetch at build time.
                tracing::debug!(path = %path, "input not in store; FUSE will lazy-fetch");
                continue;
            };
            for r in &info.references {
                if !closure.contains(r) {
                    frontier.push(r.clone());
                }
            }
            closure.insert(info.path.clone());
            metadata.push(info);
        }
    }

    Ok(metadata)
}

/// Fetch one BFS layer's metadata. Tries `BatchQueryPathInfo` first
/// (one RPC for the whole layer); on `Unimplemented` (older store
/// binary) latches `use_batch=false` and falls back to N concurrent
/// `QueryPathInfo` calls — the pre-I-110 behaviour.
async fn query_layer(
    store_client: &StoreServiceClient<Channel>,
    batch: Vec<String>,
    use_batch: &mut bool,
) -> Result<Vec<(String, Option<SynthPathInfo>)>, ExecutorError> {
    if *use_batch {
        let mut client = store_client.clone();
        match rio_proto::client::batch_query_path_info(
            &mut client,
            batch.clone(),
            rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
            &[],
        )
        .await
        {
            Ok(entries) => {
                return Ok(entries
                    .into_iter()
                    .map(|(p, info)| (p, info.map(SynthPathInfo::from)))
                    .collect());
            }
            Err(status) if status.code() == tonic::Code::Unimplemented => {
                tracing::warn!(
                    "store does not support BatchQueryPathInfo; falling back to per-path \
                     QueryPathInfo for closure BFS (I-110)"
                );
                *use_batch = false;
                // fall through to per-path loop
            }
            Err(status) => {
                // Real error (Unavailable, DeadlineExceeded, …) — propagate
                // with a representative path. The original status code is
                // preserved (test_compute_input_closure_grpc_error_preserves_code).
                return Err(ExecutorError::MetadataFetch {
                    path: batch.into_iter().next().unwrap_or_default(),
                    source: status,
                });
            }
        }
    }

    // Per-path fallback: pre-I-110 behaviour (N concurrent QueryPathInfo).
    stream::iter(batch)
        .map(|path| {
            let mut client = store_client.clone();
            async move {
                match rio_proto::client::query_path_info_opt(
                    &mut client,
                    &path,
                    rio_common::grpc::DEFAULT_GRPC_TIMEOUT,
                    &[],
                )
                .await
                {
                    Ok(info) => Ok((path, info.map(SynthPathInfo::from))),
                    Err(e) => Err(ExecutorError::MetadataFetch {
                        path: path.clone(),
                        source: e,
                    }),
                }
            }
        })
        .buffer_unordered(MAX_PARALLEL_FETCHES)
        .try_collect()
        .await
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

    /// `compute_input_closure`'s `resolved_input_srcs` parameter for tests
    /// without input_drvs: just `drv.input_srcs()` (the production caller
    /// adds resolved input_drv outputs, but `drv_with_srcs` builds drvs
    /// with empty input_drvs so there's nothing to resolve).
    fn srcs_of(drv: &Derivation) -> std::collections::BTreeSet<String> {
        drv.input_srcs().clone()
    }

    /// Project closure metadata to a path set for membership assertions.
    fn paths_of(closure: Vec<SynthPathInfo>) -> std::collections::HashSet<String> {
        closure.into_iter().map(|m| m.path).collect()
    }

    /// I-106: compute_input_closure now returns the full SynthPathInfo
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
            .find(|m| m.path == p_a)
            .expect("p_a in closure");
        assert!(
            lib.nar_hash.starts_with("sha256:"),
            "nar_hash populated (synth_db needs this) — proves we kept the \
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

        let batch_calls = store.batch_qpi_calls.load(Ordering::SeqCst);
        assert!(
            (1..=4).contains(&batch_calls),
            "one batch RPC per BFS layer (got {batch_calls}); \
             pre-I-110 would be 0 batch + 4 per-path"
        );
        assert!(
            store.qpi_calls.read().unwrap().is_empty(),
            "per-path QueryPathInfo should NOT be called when batch is available"
        );
        Ok(())
    }

    /// I-110 backward compat: store returns Unimplemented for the
    /// batch RPC → builder falls back to per-path QueryPathInfo and
    /// still produces the correct closure.
    #[tokio::test]
    async fn test_compute_input_closure_fallback_on_unimplemented() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let (store, client) = spawn_and_connect().await?;
        store.batch_qpi_unimplemented.store(true, Ordering::SeqCst);

        let (p_drv, p_a, p_b) = (tp("test.drv"), tp("lib"), tp("dep"));
        seed_with_refs(&store, &p_drv, &[]);
        seed_with_refs(&store, &p_a, std::slice::from_ref(&p_b));
        seed_with_refs(&store, &p_b, &[]);

        let drv = drv_with_srcs(std::slice::from_ref(&p_a));
        let closure = compute_input_closure(&client, &drv, &p_drv, &srcs_of(&drv)).await?;

        let set = paths_of(closure);
        assert_eq!(set.len(), 3, "fallback path produces same closure");
        assert!(set.contains(&p_b), "transitive dep reached via fallback");
        assert_eq!(
            store.batch_qpi_calls.load(Ordering::SeqCst),
            0,
            "batch handler returns Unimplemented BEFORE incrementing"
        );
        assert!(
            !store.qpi_calls.read().unwrap().is_empty(),
            "fallback issued per-path QueryPathInfo calls"
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
        let cache = crate::fuse::cache::Cache::new(dir.path().join("c"), 1, None, false).await?;

        prefetch_manifests(&client, &cache, &[p_a.clone(), p_b.clone()]).await;

        assert_eq!(
            store.batch_manifest_calls.load(Ordering::SeqCst),
            1,
            "one BatchGetManifest for the whole closure"
        );
        let b_a = p_a.strip_prefix(rio_nix::store_path::STORE_PREFIX).unwrap();
        let b_b = p_b.strip_prefix(rio_nix::store_path::STORE_PREFIX).unwrap();
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

        // End-to-end: prime again, then fetch via prefetch_path_blocking
        // (the same free-fn fetch_extract_insert delegates to). The
        // MockStore records the hint carried on GetPath.
        prefetch_manifests(&client, &cache, std::slice::from_ref(&p_a)).await;
        let rt = tokio::runtime::Handle::current();
        let cache = std::sync::Arc::new(cache);
        let (cache2, client2, b_a) = (cache.clone(), client.clone(), b_a.to_owned());
        tokio::task::spawn_blocking(move || {
            crate::fuse::fetch::prefetch_path_blocking(
                &cache2,
                &client2,
                &rt,
                std::time::Duration::from_secs(10),
                &b_a,
            )
        })
        .await?
        .map_err(|e| anyhow::anyhow!("prefetch_path_blocking: {e:?}"))?;
        let hints = store.get_path_hints.read().unwrap();
        assert_eq!(hints.len(), 1);
        assert!(
            hints[0].is_some(),
            "GetPath carried the primed manifest_hint"
        );
        Ok(())
    }

    /// I-110c backward compat: store returns Unimplemented for
    /// BatchGetManifest → prefetch is a silent no-op (cache stays
    /// empty), warm loop's per-path GetPath queries PG as before.
    #[tokio::test]
    async fn test_prefetch_manifests_unimplemented_is_noop() -> anyhow::Result<()> {
        use std::sync::atomic::Ordering;
        let (store, client) = spawn_and_connect().await?;
        store
            .batch_manifest_unimplemented
            .store(true, Ordering::SeqCst);
        let p = tp("hint-old-store");
        seed_with_refs(&store, &p, &[]);

        let dir = tempfile::tempdir()?;
        let cache = crate::fuse::cache::Cache::new(dir.path().join("c"), 1, None, false).await?;

        // Must NOT panic / error.
        prefetch_manifests(&client, &cache, std::slice::from_ref(&p)).await;

        let b = p.strip_prefix(rio_nix::store_path::STORE_PREFIX).unwrap();
        assert!(
            cache.take_manifest_hint(b).is_none(),
            "Unimplemented → nothing primed"
        );
        // Empty input → no RPC at all.
        prefetch_manifests(&client, &cache, &[]).await;
        assert_eq!(store.batch_manifest_calls.load(Ordering::SeqCst), 0);
        Ok(())
    }

    /// I-043 regression: an input_drv's OUTPUT (not in input_srcs, not
    /// in input_drvs.keys, not in any .drv's narinfo references — only
    /// declared in the input .drv's ATerm structure) must be in the
    /// closure, AND its runtime references must be walked.
    ///
    /// Live: warm count=8, autotools-hook missing. autotools-hook is
    /// reached only via stdenv-the-OUTPUT's references; the BFS
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
             was never reached → never warmed → overlay negative-dentry."
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
    // r[verify builder.upload.references-scanned]
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
        // (resolve_inputs maps SynthPathInfo.path; resolved_input_srcs are
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

    // -----------------------------------------------------------------------
    // warm_inputs_in_fuse
    // -----------------------------------------------------------------------
    //
    // Tested against a tempdir standing in for the FUSE mount, with
    // `fuse_cache: None` — stage 1 (`prefetch_path_blocking`) is
    // skipped, stage 2's `symlink_metadata()` runs against the tempdir.
    // The real prefetch path (gRPC → NAR extract → SQLite) is covered
    // by `fuse/fetch.rs`'s `test_prefetch_*`; here we test the
    // closure-walk + parallelism + error-tolerance + deadline shell.

    /// `fuse_cache: None` test fixture: lazy `connect_lazy()` client
    /// pointed at a garbage endpoint. Stage 1 is skipped so the client
    /// is never dialed.
    fn dummy_store_client() -> StoreServiceClient<Channel> {
        StoreServiceClient::new(Channel::from_static("http://127.0.0.1:1").connect_lazy())
    }

    const WARM_TEST_FETCH_TIMEOUT: Duration = Duration::from_secs(5);

    /// Paths that exist in the "FUSE" dir produce no warnings; the
    /// function completes without error. Verifies the basename
    /// extraction (strip /nix/store/) joins correctly onto the mount.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_all_present() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inputs = vec![
            (
                "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello".to_string(),
                1024,
            ),
            (
                "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-world".to_string(),
                1024,
            ),
        ];
        for (p, _) in &inputs {
            let basename = p.strip_prefix("/nix/store/").unwrap();
            std::fs::write(dir.path().join(basename), b"x").expect("write");
        }

        warm_inputs_in_fuse(
            dir.path(),
            None,
            &dummy_store_client(),
            WARM_TEST_FETCH_TIMEOUT,
            &inputs,
            WARM_OVERALL_DEADLINE,
        )
        .await;
    }

    /// Missing paths are WARN'd, not fatal. The function completes;
    /// the failure metric is incremented (we can't assert the metric
    /// here without a recorder, but we assert no panic/error).
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_missing_tolerated() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inputs = vec![
            (
                "/nix/store/cccccccccccccccccccccccccccccccc-missing".to_string(),
                0,
            ),
            (
                "/nix/store/dddddddddddddddddddddddddddddddd-also-missing".to_string(),
                0,
            ),
        ];

        warm_inputs_in_fuse(
            dir.path(),
            None,
            &dummy_store_client(),
            WARM_TEST_FETCH_TIMEOUT,
            &inputs,
            WARM_OVERALL_DEADLINE,
        )
        .await;
    }

    /// Malformed paths (no /nix/store/ prefix) are filtered out
    /// silently. They'd have failed compute_input_closure's
    /// QueryPathInfo with InvalidArgument before reaching warm anyway.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_skips_malformed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let inputs = vec![
            ("no-prefix-at-all".to_string(), 0),
            ("/wrong/prefix/foo".to_string(), 0),
            (
                "/nix/store/eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-valid".to_string(),
                1024,
            ),
        ];
        std::fs::write(
            dir.path().join("eeeeeeeeeeeeeeeeeeeeeeeeeeeeeeee-valid"),
            b"x",
        )
        .expect("write");

        warm_inputs_in_fuse(
            dir.path(),
            None,
            &dummy_store_client(),
            WARM_TEST_FETCH_TIMEOUT,
            &inputs,
            WARM_OVERALL_DEADLINE,
        )
        .await;
    }

    /// Mixed present/missing: present ones succeed, missing ones
    /// WARN. This is the realistic case — most inputs are warm from
    /// earlier builds, a few are cold.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_mixed() {
        let dir = tempfile::tempdir().expect("tempdir");
        let present = (
            "/nix/store/ffffffffffffffffffffffffffffffff-present".to_string(),
            1024,
        );
        let missing = (
            "/nix/store/00000000000000000000000000000000-missing".to_string(),
            0,
        );
        std::fs::write(
            dir.path().join("ffffffffffffffffffffffffffffffff-present"),
            b"x",
        )
        .expect("write");

        warm_inputs_in_fuse(
            dir.path(),
            None,
            &dummy_store_client(),
            WARM_TEST_FETCH_TIMEOUT,
            &[present, missing],
            WARM_OVERALL_DEADLINE,
        )
        .await;
    }

    /// I-178: per-path timeout = max(base, nar_size / MIN_THROUGHPUT).
    /// A 2 GB NAR at 15 MiB/s ≈ 131 s; the base 60 s would have aborted
    /// it mid-stream → daemon ENOENT → PermanentFailure poison. A 1 KB
    /// NAR keeps the base.
    // r[verify builder.input.warm-bounded+3]
    #[test]
    fn test_warm_per_path_timeout_scales_with_nar_size() {
        let base = Duration::from_secs(60);

        // Small input: floor at base.
        assert_eq!(warm_per_path_timeout(base, 1024), base);
        assert_eq!(warm_per_path_timeout(base, 0), base);

        // 2 GB input: ceil(2_000_000_000 / 15_728_640) = 128 s > 60 s.
        let two_gb = warm_per_path_timeout(base, 2_000_000_000);
        assert!(
            two_gb >= Duration::from_secs(127),
            "2 GB @ 15 MiB/s floor must get ≥127 s, got {two_gb:?}"
        );
        assert!(
            two_gb < Duration::from_secs(200),
            "sanity upper bound (catches MIN_THROUGHPUT being lowered \
             without revisiting this test): {two_gb:?}"
        );

        // The 1.9 GB NAR from the I-178 incident.
        let i178 = warm_per_path_timeout(base, 1_901_554_624);
        assert!(
            i178 > base,
            "I-178's 1.9 GB input must exceed the 60 s base that poisoned it"
        );
    }

    /// I-178: overall deadline grows with Σnar_size so a single large
    /// input doesn't detach at the 90 s floor while its per-path
    /// timeout is still 127 s.
    #[test]
    fn test_warm_overall_deadline_scales_with_total_size() {
        // Small closure: 90 s floor.
        assert_eq!(warm_overall_deadline(0), WARM_OVERALL_DEADLINE);
        assert_eq!(
            warm_overall_deadline(10 * 1024 * 1024),
            WARM_OVERALL_DEADLINE
        );

        // 8 GB closure at 15 MiB/s × 4 lanes = ~131 s + 30 s slop > 90 s.
        let eight_gb = warm_overall_deadline(8_000_000_000);
        assert!(
            eight_gb > WARM_OVERALL_DEADLINE,
            "8 GB closure must push past the 90 s floor, got {eight_gb:?}"
        );
        // Exit criterion: deadline ≥ Σ / (THROUGHPUT × CONCURRENCY).
        let min_expected = Duration::from_secs(
            8_000_000_000 / (WARM_MIN_THROUGHPUT_BPS * WARM_FUSE_CONCURRENCY as u64),
        );
        assert!(
            eight_gb >= min_expected,
            "deadline {eight_gb:?} must cover throughput bound {min_expected:?}"
        );
    }

    /// I-178: `warm_inputs_bounded` threads `nar_size` to `warm_one` so
    /// the production closure can compute a per-path timeout. Feed one
    /// 2 GB and one 1 KB entry; assert the closure observes each path's
    /// own size and the derived per-path timeout matches.
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_bounded_threads_nar_size() {
        let dir = tempfile::tempdir().expect("tempdir");
        let big = (
            "/nix/store/33333333333333333333333333333333-big".to_string(),
            2_000_000_000u64,
        );
        let small = (
            "/nix/store/44444444444444444444444444444444-small".to_string(),
            1024u64,
        );

        let seen: Arc<std::sync::Mutex<Vec<(String, u64, Duration)>>> =
            Arc::new(std::sync::Mutex::new(Vec::new()));
        let seen_cl = Arc::clone(&seen);
        let base = Duration::from_secs(60);

        warm_inputs_bounded(
            dir.path(),
            &[big, small],
            Duration::from_secs(5),
            move |basename, _fuse_path, nar_size| {
                let seen = Arc::clone(&seen_cl);
                async move {
                    let timeout = warm_per_path_timeout(base, nar_size);
                    seen.lock().unwrap().push((basename, nar_size, timeout));
                    Ok(())
                }
            },
        )
        .await;

        let seen = seen.lock().unwrap();
        let (_, big_size, big_to) = seen
            .iter()
            .find(|(b, _, _)| b.ends_with("-big"))
            .expect("big seen");
        let (_, small_size, small_to) = seen
            .iter()
            .find(|(b, _, _)| b.ends_with("-small"))
            .expect("small seen");

        assert_eq!(*big_size, 2_000_000_000);
        assert!(
            *big_to >= Duration::from_secs(127),
            "2 GB input must get ≥127 s, got {big_to:?}"
        );
        assert_eq!(*small_size, 1024);
        assert_eq!(*small_to, base, "1 KB input keeps base timeout");
    }

    /// I-165 / I-165c: a per-path warm op that hangs indefinitely
    /// (simulating `prefetch_path_blocking` parked in `block_on(
    /// GetPath)` against a saturated store) MUST NOT hang the warm
    /// phase. The overall deadline fires; the function returns.
    ///
    /// The injected `warm_one` is a PURE-ASYNC `pending()` for the
    /// stall path — when `tokio::time::timeout` drops the stream, the
    /// pending future is dropped with NO lingering work. This is the
    /// I-165c structural property: the stall point is now a
    /// cancellable future, not a kernel D-state thread. (The
    /// production path's `spawn_blocking(prefetch_path_blocking)` IS
    /// detached on timeout, but that thread is in userspace
    /// `block_on(gRPC)` bounded by `fetch_timeout` — dies on SIGKILL,
    /// never D-states.)
    ///
    /// multi_thread flavor: the timeout timer needs a free worker to
    /// fire while the fast path's spawn_blocking is in the pool.
    ///
    /// TODO(I-165): the end-to-end VM coverage for this (toxiproxy on
    /// the worker→store GetPath link, assert
    /// `rio_builder_input_warm_timeout_total ≥ 1` within
    /// deadline+slop) needs the chaos fixture extended to proxy
    /// worker→store. Tracked as a Tier-2 followup.
    // r[verify builder.input.warm-bounded+3]
    #[tokio::test(flavor = "multi_thread")]
    async fn test_warm_inputs_deadline_bounds_stalled_fetch() {
        let dir = tempfile::tempdir().expect("tempdir");
        // Two inputs: one resolves instantly, one stalls. Partial-warm
        // accounting should reflect 1 warmed, 1 skipped (not failed —
        // skipped is `total - warmed - failed`).
        let fast = (
            "/nix/store/11111111111111111111111111111111-fast".to_string(),
            1024,
        );
        let slow = (
            "/nix/store/22222222222222222222222222222222-stall".to_string(),
            1024,
        );
        std::fs::write(
            dir.path().join("11111111111111111111111111111111-fast"),
            b"x",
        )
        .expect("write");

        let deadline = Duration::from_millis(300);

        let start = std::time::Instant::now();
        warm_inputs_bounded(
            dir.path(),
            &[fast, slow],
            deadline,
            |basename, p, _nar_size| async move {
                if basename.ends_with("-stall") {
                    // Pure-async hang: dropped cleanly when the timeout
                    // drops the buffer_unordered stream. No spawn_blocking,
                    // no detached thread.
                    std::future::pending::<()>().await;
                }
                tokio::task::spawn_blocking(move || {
                    p.symlink_metadata()
                        .map(|_| ())
                        .map_err(|e| WarmErr::Io(p.display().to_string(), e))
                })
                .await
                .map_err(WarmErr::Join)?
            },
        )
        .await;
        let elapsed = start.elapsed();

        assert!(
            elapsed >= deadline,
            "should not return before deadline (stall didn't take effect?): {elapsed:?}"
        );
        // Generous slop for CI scheduling jitter; the load-bearing
        // assertion is that we returned at all (a pure-async pending()
        // would hang forever without the timeout wiring).
        assert!(
            elapsed < deadline + Duration::from_secs(2),
            "returned well past deadline (timeout not wired?): {elapsed:?}"
        );
    }
}
