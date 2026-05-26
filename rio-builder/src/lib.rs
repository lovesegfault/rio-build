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
//! +-- Castore-FUSE (castore_fuse/), one mount per build
//! |   +-- mount.rs: DAG prefetch + mountd handshake + builder mount(2)
//! |   +-- fs.rs/open.rs: metadata from the in-heap tree; open() via the
//! |   |   node-shared backing cache (passthrough) or JIT fetch
//! |   +-- mountd.rs: the privileged per-node broker (DaemonSet)
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
        // A triggered sweep unlinks anywhere from a handful of staging
        // orphans (ms) to hundreds of thousands of cache entries on a
        // nearly-full node SSD (minutes). The [0.005..10.0] default
        // truncates the slow tail that matters for alerting.
        "rio_mountd_sweep_seconds",
        &[0.01, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0, 120.0, 300.0],
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
         or keep_cache (userspace pread per uncached read; also the streaming-fill \
         window of a large-file miss). passthrough=0 with keep_cache>0 outside the \
         streaming window means FUSE_PASSTHROUGH was not negotiated — check the \
         kernel version and RIO_DISABLE_PASSTHROUGH."
    );
    describe_counter!(
        "rio_builder_castore_fuse_open_case_total",
        "Castore-FUSE open() dispatch decisions: hit (backing cache), miss_small \
         (whole-file JIT fetch), miss_stream (above stream_threshold: open() returns \
         after the first chunk while the fill streams in the background), \
         wait_fetching (another open of the same digest was already filling)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_chunk_source_total",
        "Where castore-FUSE fill bytes came from, by src: remote (rio-store) or \
         node_ssd (the shared chunk cache). The node_ssd fraction is the \
         cross-build dedup ratio."
    );
    describe_histogram!(
        "rio_builder_castore_fuse_open_seconds",
        "Castore-FUSE open() upcall-to-reply latency, labeled by hit \
         (node_ssd = backing-cache hit, remote = JIT fetch) and streamed \
         (1 = the open returned inside a P0575 streaming-fill window)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_fetch_bytes_total",
        "Bytes fetched on behalf of castore-FUSE open(), labeled by hit \
         (remote = from rio-store; node_ssd = from the shared node chunk cache)."
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
    describe_counter!(
        "rio_builder_castore_fuse_promote_fail_total",
        "Promote round-trips that failed after a completed castore-FUSE fill (the \
         RaceTimeout cache re-check and single retry already applied). On the \
         whole-file path the open fails with EIO; on the streaming path the open \
         handles keep reading from staging, but the digest is not published to the \
         node cache so later opens re-fetch."
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
        "rio_builder_objects_cache_hit_total",
        "Castore-FUSE open()s whose file_digest was already present in the node-shared \
         backing cache (/var/rio/cache) — no fetch, straight to passthrough. The \
         cross-build amortization signal for the mountd-owned objects cache (P0571)."
    );
    describe_counter!(
        "rio_builder_objects_cache_bytes",
        "Bytes served from the node-shared backing cache on open() hits (the file sizes \
         that did NOT have to be re-fetched from rio-store)."
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
        "rio_builder_overlay_teardown_failures_total",
        "Overlay unmount failures (leaked mount); alert if rate > 0"
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
