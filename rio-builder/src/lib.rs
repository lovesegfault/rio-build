//! Build executor with FUSE store for rio-build.
//!
//! Receives build assignments from the scheduler, runs builds using
//! nix-daemon within an overlayfs+FUSE environment, and uploads
//! results to the store.
//!
//! # Architecture
//!
//! ```text
//! rio-builder binary
//! +-- gRPC clients
//! |   +-- ExecutorService.BuildExecution (bidi stream to scheduler)
//! |   +-- ExecutorService.Heartbeat (periodic to scheduler)
//! |   +-- StoreService (fetch inputs, upload outputs)
//! +-- FUSE daemon (fuse/)
//! |   +-- Mount /nix/store via fuser 0.17
//! |   +-- lookup/getattr -> StoreService.QueryPathInfo
//! |   +-- read/readdir -> SSD cache or StoreService.GetPath
//! |   +-- Ephemeral local-disk cache (cache.rs)
//! +-- Build executor (executor.rs)
//! |   +-- Overlay management (overlay.rs)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload.rs)
//! +-- Heartbeat loop (runtime.rs, 10s interval)
//! ```

pub(crate) mod banner;
pub mod castore_fuse;
pub mod cgroup;
pub mod config;
pub mod executor;
#[cfg(feature = "test-fixtures")]
pub mod fixture;
pub mod fuse;
pub mod health;
pub mod hw_bench;
pub mod hw_class;
pub mod log_stream;
pub(crate) mod overlay;
pub mod quota;
pub mod runtime;
pub mod store_fetch;
pub(crate) mod synth_db;
// `pub` (not `pub(crate)`) for tests/chunked_upload.rs — the
// functional-tier test drives `upload_all_outputs` directly against a
// real `StoreServiceImpl`. Everything inside except the entry point and
// the error type stays `pub(crate)`/`pub(super)`.
pub mod upload;

/// Recover the guard from a poisoned [`std::sync`] lock result.
///
/// Poisoning happens when a thread panics while holding the lock; in
/// this crate every guarded value is either rebuilt on the next tick or
/// only read on a shutdown path, so a partially-updated value is benign.
/// 30+ call sites previously spelled this out as
/// `.unwrap_or_else(|e| e.into_inner())`.
pub(crate) trait IgnorePoison<T> {
    fn ignore_poison(self) -> T;
}
impl<T> IgnorePoison<T> for Result<T, std::sync::PoisonError<T>> {
    fn ignore_poison(self) -> T {
        self.unwrap_or_else(std::sync::PoisonError::into_inner)
    }
}

/// Histogram bucket boundaries for `rio_builder_upload_references_count`.
///
/// Reference COUNT (not seconds) per output upload — `references.len()`
/// after NAR scan. Typical leaf derivation: 0–5 refs. glibc-class: ~40.
/// Toolchains: 100–300. Default Prometheus buckets `[0.005..10.0]` are
/// useless here — every output with >10 refs lands in `+Inf`. Per-path,
/// so the top bucket is 500, not 20K.
const REFERENCES_COUNT_BUCKETS: &[f64] = &[1.0, 5.0, 10.0, 25.0, 50.0, 100.0, 250.0, 500.0];

/// Per-crate histogram bucket overrides, passed to
/// `rio_common::server::bootstrap` → `init_metrics`. Every
/// `describe_histogram!` in this crate must have an entry here OR be in
/// the `DEFAULT_BUCKETS_OK` exemption list (`tests/metrics_registered.rs`);
/// histograms not listed fall through to the global `[0.005..10.0]` default.
pub const HISTOGRAM_BUCKETS: &[(&str, &[f64])] = &[
    (
        "rio_builder_build_duration_seconds",
        rio_common::observability::BUILD_DURATION_BUCKETS,
    ),
    (
        "rio_builder_upload_references_count",
        REFERENCES_COUNT_BUCKETS,
    ),
    (
        // I-212 size-cap → JIT path means GB-scale NARs are fetched on
        // demand; fuse/fetch/mod.rs documents 60-127s for those. The
        // [0.005..10.0] default would put every >10s sample in {le="+Inf"}
        // and saturate `histogram_quantile(0.99,…)` at 10.0. Mirrors
        // rio-store SUBSTITUTE_DURATION_BUCKETS — same operation
        // (NAR fetch + drain), opposite end.
        "rio_builder_fuse_fetch_duration_seconds",
        &[0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0, 60.0, 120.0],
    ),
    (
        // Spans five decades: BackingOpen/BackingClose are a single
        // ioctl (sub-ms, the 0.0005/0.001 buckets), Mount is a handful
        // of syscalls (~ms), and Promote of a multi-GiB staged file is
        // tens of seconds of copy+blake3. The [0.005..10.0] default
        // loses both tails.
        "rio_mountd_request_seconds",
        &[
            0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0,
        ],
    ),
    (
        // Bimodal: a backing-cache hit is one stat + one mountd
        // round-trip (sub-ms to low ms); a miss is a whole-file
        // ReadBlob bounded by jit_fetch_timeout (60 s). The default
        // [0.005..10.0] buckets put every miss in {le="+Inf"}.
        "rio_builder_castore_fuse_open_seconds",
        &[
            0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0,
        ],
    ),
    (
        // One multi-root GetDirectory(recursive) stream per build —
        // chromium-scale closures are ~5 MiB of Directory bodies, so
        // the tail is network-bound seconds, not the sub-second default.
        "rio_builder_castore_dag_prefetch_seconds",
        &[0.01, 0.05, 0.1, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0],
    ),
];

/// Registers prometheus metric descriptions. The help strings here are
/// the source for `docs/ref/metrics.typ` — see
/// `xtask/src/regen/docs_data.rs::metrics()` for the data-flow.
///
/// Also registers the `rio_mountd_*` descriptions: rio-mountd is a
/// separate binary in this crate, and `tests/metrics_registered.rs`
/// cross-references every `metrics::*!` emission in `src/` against this
/// one function. Describing a metric a given binary never emits is
/// harmless (an extra `# HELP` line at worst).
// r[impl obs.metric.builder]
pub fn describe_metrics() {
    use metrics::{describe_counter, describe_gauge, describe_histogram};

    castore_fuse::mountd::describe_metrics();

    // ── castore-FUSE (ADR-022 §14) ────────────────────────────────────
    // r[impl obs.metric.castore-fuse]
    describe_counter!(
        "rio_builder_castore_fuse_upcalls_total",
        "Castore-FUSE kernel upcalls by op (lookup/getattr/readdir/readlink/open/read). \
         The single cold-metadata counter: with every cache TTL infinite, lookup/readdir \
         fire ~once per distinct dentry per build (the dcache and FOPEN_CACHE_DIR absorb \
         repeats); op=\"read\">0 means passthrough is not engaging."
    );
    describe_counter!(
        "rio_builder_castore_fuse_open_mode_total",
        "Castore-FUSE open() replies by mode: passthrough (reads bypass FUSE entirely) \
         or keep_cache (userspace pread per uncached read). passthrough=0 with \
         keep_cache>0 means FUSE_PASSTHROUGH was not negotiated — check the kernel \
         version and RIO_DISABLE_PASSTHROUGH."
    );
    describe_counter!(
        "rio_builder_castore_fuse_open_case_total",
        "Castore-FUSE open() dispatch decisions: hit (backing cache), miss_small \
         (whole-file JIT fetch), miss_stream (above stream_threshold; streaming in \
         P0575), wait_fetching (another open of the same digest was already filling)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_chunk_source_total",
        "Where castore-FUSE fill bytes came from, by src: remote (rio-store) or \
         node_ssd (the shared chunk cache, P0575). The node_ssd fraction is the \
         cross-build dedup ratio."
    );
    describe_histogram!(
        "rio_builder_castore_fuse_open_seconds",
        "Castore-FUSE open() upcall-to-reply latency, labeled by hit \
         (node_ssd = backing-cache hit, remote = JIT fetch)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_fetch_bytes_total",
        "Bytes fetched on behalf of castore-FUSE open(), labeled by hit \
         (remote = from rio-store; node_ssd = from the shared node cache, P0575)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_integrity_fail_total",
        "Castore-FUSE fetches whose bytes did not blake3-hash to the requested \
         file_digest. The bytes are discarded and the open fails with EIO. \
         Nonzero = corrupt store content or a transport integrity failure — \
         investigate immediately."
    );
    describe_counter!(
        "rio_builder_castore_fuse_eio_total",
        "EIO replies returned to the kernel from the castore-FUSE open()/read() path \
         (fetch timeout, integrity failure, mountd error, open ceiling). Every one \
         fails a build (as an infrastructure failure)."
    );
    describe_gauge!(
        "rio_builder_castore_fuse_circuit_open",
        "1.0 when the castore-FUSE fetch circuit breaker is open (rio-store \
         unreachable or degraded). Opens after 5 consecutive fetch failures OR 720s \
         since last successful fetch with at least one failure since. Half-open after \
         30s (one probe fetch allowed)."
    );
    describe_histogram!(
        "rio_builder_castore_dag_prefetch_seconds",
        "Mount-time GetDirectory(recursive=true) Directory-DAG prefetch wall-clock \
         per build (one multi-root call for the whole input closure)."
    );

    describe_counter!(
        "rio_builder_builds_total",
        "Total builds executed (labeled by outcome: success/failure/cancelled/timed_out/log_limit/infra_failure)"
    );
    describe_gauge!(
        "rio_builder_builds_active",
        "Currently running builds on this worker"
    );
    describe_counter!(
        "rio_builder_uploads_total",
        "Output uploads (labeled by status: success/adopted/exhausted; \
         adopted = concurrent uploader won, result polled via QueryPathInfo)"
    );
    describe_histogram!(
        "rio_builder_build_duration_seconds",
        "Per-derivation build time"
    );
    describe_counter!(
        "rio_builder_fuse_jit_lookup_total",
        "Top-level FUSE lookup outcomes under JIT fetch (I-043 redesign), \
         labeled by outcome: reject (not in registered input set → fast \
         ENOENT, no store contact), fetch (registered input materialized), \
         eio (registered input fetch FAILED → EIO so overlay can't \
         negative-cache). reject/fetch ratio ≈ closure utilization; eio \
         nonzero = store degraded."
    );
    describe_gauge!(
        "rio_builder_jit_inputs_registered",
        "Size of the JIT FUSE allowlist (known_inputs.len()) at daemon spawn. \
         Equals compute_input_closure's output count for this build."
    );
    describe_counter!(
        "rio_builder_log_lines_suppressed_total",
        "Log lines dropped by rate suppression (log_rate_limit). Build \
         continues with a `[rio: N lines suppressed …]` marker. Nonzero is \
         normal for bursty builds (kernel oldconfig, autoconf); sustained \
         high rate = pathological producer."
    );
    describe_counter!(
        "rio_builder_cgroup_oom_total",
        "Builds killed by the cgroup OOM watcher (memory.events oom_kill \
         incremented during build). Reported as InfrastructureFailure for \
         scheduler resource_floor bump (I-196). Nonzero = pool's memory \
         limit is undersized for its workload."
    );
    describe_counter!(
        "rio_builder_input_materialization_failures_total",
        "Daemon MiscFailure reclassified as InfrastructureFailure because the \
         missing path is in the build's input closure (I-178). Sustained \
         nonzero = JIT_MIN_THROUGHPUT_BPS is set above actual store→builder \
         throughput; lower the floor."
    );
    describe_counter!(
        "rio_builder_fuse_cache_hits_total",
        "FUSE cache hits (local symlink_metadata succeeded)"
    );
    describe_counter!(
        "rio_builder_fuse_cache_misses_total",
        "FUSE cache misses (fetch from remote store required)"
    );
    describe_histogram!(
        "rio_builder_fuse_fetch_duration_seconds",
        "Store path fetch latency (gRPC GetPath + stream drain)"
    );
    describe_counter!(
        "rio_builder_overlay_teardown_failures_total",
        "Overlay unmount failures (leaked mount); alert if rate > 0"
    );
    describe_counter!(
        "rio_builder_prefetch_total",
        "PrefetchHint outcomes. result=fetched|already_cached|already_in_flight|not_input|size_cap|error|malformed|panic. \
         error = store fetch failed (debug-only log; build's own FUSE ops surface \
         the real problem if store is flaky)."
    );
    describe_counter!(
        "rio_builder_prefetch_filtered_total",
        "PrefetchHint paths skipped by the I-212 filter, by reason. \
         reason=not_input: JIT allowlist armed and path is not a declared \
         input (build can never read it). reason=size_cap: warm-gate batch \
         (allowlist not yet armed) and QueryPathInfo.nar_size exceeds the cap — \
         scheduler over-includes sibling outputs (e.g., 2.9 GB clang-debug); \
         the build fetches it on-demand via JIT lookup if it turns out to be \
         a real input."
    );
    describe_counter!(
        "rio_builder_upload_bytes_total",
        "Bytes uploaded to store via PutPath (nar_size on success)"
    );
    describe_counter!(
        "rio_builder_upload_chunks_total",
        "Output chunk digests by PutPathChunked outcome. kind=novel: absent \
         from the store per HasChunks → body sent as a Chunk frame. \
         kind=deduped: already durable → only the manifest entry sent. \
         deduped/(novel+deduped) is the builder-observed dedup ratio; near \
         zero on a cold store, rising as the CAS fills."
    );
    describe_counter!(
        "rio_builder_upload_skipped_idempotent_total",
        "Output uploads skipped by the FindMissingPaths pre-check \
         (path already complete in store). High sustained rate = \
         scheduler dispatching already-built derivations (race or \
         CA early-cutoff). The store's PutPath idempotency would \
         no-op these server-side anyway; this counter measures the \
         worker-side disk-read + NAR-stream savings."
    );
    describe_counter!(
        "rio_builder_fuse_fetch_bytes_total",
        "Bytes fetched from store via FUSE misses (nar_data.len())"
    );
    describe_counter!(
        "rio_builder_fuse_fallback_reads_total",
        "Userspace read() callbacks served. When passthrough is ON (default), \
         the kernel handles reads directly and this counter stays near zero — \
         nonzero means open_backing() failed for some file. When passthrough \
         is OFF (RIO_FUSE_PASSTHROUGH=false), every read comes through here. \
         Sustained nonzero rate with passthrough ON = investigate open_backing."
    );
    describe_counter!(
        "rio_builder_fuse_index_divergence_total",
        "FUSE cache index/disk divergences detected and self-healed. Nonzero \
         means something rm'd cache files out from under the in-memory index \
         (manual debugging, disk cleanup scripts, interrupted extract). \
         The path is purged and re-fetched; investigate if sustained."
    );
    describe_gauge!(
        "rio_builder_fuse_circuit_open",
        "1.0 when the FUSE fetch circuit breaker is open (store unreachable \
         or degraded). Opens after 5 consecutive fetch failures OR 720s since \
         last successful fetch with ≥1 failure since. Half-open after 30s \
         (one probe fetch allowed)."
    );
    describe_gauge!(
        "rio_builder_cpu_fraction",
        "Worker cgroup CPU utilization: delta cpu.stat usage_usec / wall-clock µs. \
         1.0 = one core fully used; >1.0 on multi-core."
    );
    describe_gauge!(
        "rio_builder_memory_fraction",
        "Worker cgroup memory utilization: memory.current / memory.max. \
         0.0 if memory.max is unbounded ('max' literal)."
    );
    describe_histogram!(
        "rio_builder_upload_references_count",
        "Reference count per output upload (references.len() after scan). \
         Distribution of dependency fan-out per built path. Zero-heavy = \
         mostly leaves; high p99 = wide transitive closures."
    );
    describe_counter!(
        "rio_builder_stale_assignments_rejected_total",
        "Assignments rejected due to stale generation (from a deposed \
         scheduler leader). Nonzero during leader transitions is expected; \
         sustained = scheduler lease flapping."
    );
    describe_counter!(
        "rio_builder_cgroup_leak_total",
        "Per-build cgroup rmdir failures on Drop (typically EBUSY — \
         processes still in the tree). Leaked cgroups are harmless empty \
         pseudo-dirs under /sys/fs/cgroup; pod restart clears them. \
         Sustained rate = builds not reaping cleanly (investigate \
         cgroup.kill timing or zombie builders)."
    );
}
