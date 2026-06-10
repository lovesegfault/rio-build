//! Build executor with a per-build castore-FUSE store for rio-build.
//!
//! Pulls the one assignment this pod was spawned for from the
//! scheduler, runs the build using nix-daemon within an
//! overlayfs-over-castore-FUSE environment, uploads results to the
//! store, and reports the outcome.
//!
//! # Architecture
//!
//! ```text
//! rio-builder binary
//! +-- gRPC clients
//! |   +-- ExecutorService.PullAssignment / ReportOutcome (unaries)
//! |   +-- Store/Directory/ChunkService (fetch inputs, upload outputs)
//! +-- Castore-FUSE (castore_fuse/), one session per build
//! |   +-- rio-mountd UDS handshake (/dev/fuse fd handoff)
//! |   +-- lookup/getattr/readdir from the prefetched Directory DAG
//! |   +-- open() -> node-SSD backing cache + kernel passthrough
//! +-- Build executor (executor/)
//! |   +-- Overlay management (overlay.rs, lower = castore mountpoint)
//! |   +-- Synthetic DB generation (synth_db.rs)
//! |   +-- Log streaming (log_stream.rs)
//! |   +-- Output upload (upload/)
//! +-- Pull loop (runtime/pull.rs: pull -> build -> report -> exit)
//! ```

// bug_313 class close: doc-theft-by-insertion — a new item inserted
// directly under an existing doc block steals it, always leaving the
// VICTIM undocumented. With missing_docs denied, that whole class is a
// docs-gate red instead of a review hope.
#![deny(missing_docs)]

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
pub mod log_upload;
pub(crate) mod overlay;
pub mod quota;
pub mod runtime;
pub mod store_fetch;
pub(crate) mod synth_db;
pub(crate) mod upload;

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
        // Same five-decade spread as rio_mountd_request_seconds: a
        // cache-hit open is one UDS round-trip (sub-ms), a cold open
        // is a whole-file fetch + Promote (seconds to minutes for the
        // jit_fetch_timeout-scaled tail).
        "rio_builder_castore_fuse_open_seconds",
        &[
            0.0005, 0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0, 10.0, 30.0, 60.0,
        ],
    ),
    (
        // One GetDirectory(recursive) stream per build at mount time.
        // Chromium-scale (8.2k dirs) is ~5 MiB / low seconds; the
        // dag_prefetch_timeout default (30 s) is the ceiling.
        "rio_builder_castore_dag_prefetch_seconds",
        &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0, 30.0],
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

    describe_counter!(
        "rio_builder_builds_total",
        "Total builds executed (labeled by outcome: success/failure/cancelled/timed_out/log_limit/infra_failure)"
    );
    describe_counter!(
        "rio_builder_quota_limit_unreadable_total",
        "Completions whose post-build quota LIMIT read failed while the \
         during-build monitor HAD witnessed usage (bug_046: the \
         (one-shot-failed, monitor-witnessed) fold cell — quota record \
         gone after teardown, or fs error). The sizing peak survives on \
         its own product; exhaustion classification (which needs the \
         limit regardless) is off for that completion. Distinct from \
         rio_builder_quota_evidence_absent_total, which now fires only \
         when BOTH producers yielded nothing. A sustained rate here \
         with a quiet absent counter means teardown races the one-shot \
         — sizing is healthy, attribution is degraded."
    );
    describe_gauge!(
        "rio_builder_quota_enforcement",
        "Project-quota enforcement posture observed on the overlay \
         emptyDir (D-2: the DiskFull lane's dormancy made visible). \
         Label mode = enforcing (a real sub-sentinel hard limit is set; \
         the DiskFull classifier CAN fire), non_enforcing (kubelet's \
         AssignQuota -1 sentinel: usage tracking only, the \
         hostUsers:false fleet shape), no_limit (the builder's \
         monitoring-only mint under hostUsers:true), or unavailable \
         (decline modes 1-3). At the deployed posture builder pods \
         report non_enforcing or no_limit; an enforcing reading is the \
         readback that proves the future enforcing flip."
    );
    describe_counter!(
        "rio_builder_quota_evidence_absent_total",
        "Completions whose project-quota disk evidence was ABSENT — \
         BOTH producers yielded nothing (bug_046: the one-shot \
         quota::status AND the 1 Hz during-build monitor; live060-b: \
         the node lacks the prjquota precondition, so peak_disk_bytes \
         is None and the disk sizer cannot learn from this pod). One \
         increment per completion; a once-per-pod WARN \
         names the precondition. Expect this rate to trend to ZERO as \
         prjquota provisioning (live060-a) rolls out; a nonzero \
         steady-state rate means builder nodes are running without \
         the disk-evidence producer."
    );
    describe_gauge!(
        "rio_builder_builds_active",
        "Currently running builds on this worker"
    );
    describe_counter!(
        "rio_builder_uploads_total",
        "Output uploads (labeled by status: success = output committed via \
         PutPathChunked, exhausted = retry budget spent without a commit)"
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
         nonzero = closure inputs are failing to materialize from the \
         castore-FUSE lower (store fetch errors, integrity failures, or a \
         tripped fetch circuit breaker); correlate with \
         rio_builder_castore_fuse_eio_total and rio-store health."
    );
    describe_counter!(
        "rio_builder_overlay_teardown_failures_total",
        "Overlay unmount failures (leaked mount); alert if rate > 0"
    );
    describe_counter!(
        "rio_builder_upload_bytes_total",
        "Novel chunk bytes streamed to the store via PutPathChunked. \
         Chunks the store already holds durably are deduplicated and \
         never counted, so this measures actual upload data movement, \
         not output nar_size."
    );
    describe_counter!(
        "rio_builder_upload_skipped_idempotent_total",
        "Outputs skipped as already present in the store: the \
         FindMissingPaths pre-check found every output complete, or the \
         store reported created = false at PutPathChunked commit. High \
         sustained rate = scheduler dispatching already-built \
         derivations (race or CA early-cutoff). This counter measures \
         the worker-side disk-read + chunk-stream savings."
    );
    describe_gauge!(
        "rio_builder_fuse_circuit_open",
        "1.0 when the castore-FUSE fetch circuit breaker is open (store \
         unreachable or degraded). Opens after 5 consecutive fetch failures \
         OR 720s since last successful fetch with ≥1 failure since. Half-open \
         after 30s (one probe fetch allowed)."
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
    // r[impl obs.metric.castore-fuse]
    describe_counter!(
        "rio_builder_castore_fuse_upcalls_total",
        "Castore-FUSE upcalls by op (lookup/getattr/readdir/readlink/open/read). \
         Cold-path only by design: infinite cache TTLs mean the kernel answers \
         repeats from dcache/icache. A high steady-state rate = the caches are \
         not absorbing (TTL regression or memory-pressure eviction)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_uring_requests_total",
        "Castore-FUSE requests delivered over the fuse-over-io_uring rings — \
         the session's only request transport (upcalls_total carries the \
         per-op split). Zero during a build means the mount never engaged."
    );
    describe_histogram!(
        "rio_builder_castore_fuse_open_seconds",
        "Wall-clock from open() upcall to reply, labeled by the open_case_total \
         taxonomy: case=hit|wait_fetching|miss_small|miss_stream. hit and \
         wait_fetching reply from the node cache (wait_fetching first blocks \
         on a concurrent open's in-flight fetch); miss_small is a whole-file \
         fetch + Promote; miss_stream replies at the first chunk and fills in \
         the background."
    );
    describe_counter!(
        "rio_builder_castore_fuse_open_case_total",
        "open() decision by case: hit (backing cache), miss_small (whole-file \
         fetch), miss_stream (above the streaming threshold; replies at the \
         first chunk and fills in the background), wait_fetching (joined a \
         concurrent open's in-flight fetch of the same digest)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_open_mode_total",
        "open() reply mode: passthrough (kernel reads the backing file directly) \
         or keep_cache (userspace read() fallback). passthrough=0 in steady state \
         means FUSE_PASSTHROUGH negotiation failed — a 10-100x data-path \
         regression that is otherwise invisible."
    );
    describe_counter!(
        "rio_builder_castore_fuse_fetch_bytes_total",
        "Bytes sourced by castore-FUSE open() to materialize a file, labeled by \
         tier: remote (rio-store ReadBlob/GetChunks) or node_ssd (the \
         mountd-owned node chunk cache)."
    );
    describe_counter!(
        "rio_builder_castore_fuse_integrity_fail_total",
        "Fetched content whose blake3 did not match the requested file_digest. \
         The bytes are discarded and the open fails EIO. Nonzero = store-side \
         corruption or a chunk-reassembly bug; investigate immediately."
    );
    describe_counter!(
        "rio_builder_castore_fuse_eio_total",
        "castore-FUSE open()s that returned an error to the kernel (fetch \
         failure, integrity mismatch, mountd rejection, backing-id ceiling). \
         Every one of these fails a build."
    );
    describe_histogram!(
        "rio_builder_castore_dag_prefetch_seconds",
        "GetDirectory(recursive) wall-clock per build at mount time (one \
         multi-root call for the whole input closure)."
    );
    describe_counter!(
        "rio_builder_cgroup_leak_total",
        "Per-build cgroup rmdir failures on Drop (typically EBUSY — \
         processes still in the tree). Leaked cgroups are harmless empty \
         pseudo-dirs under /sys/fs/cgroup; pod restart clears them. \
         Sustained rate = builds not reaping cleanly (investigate \
         cgroup.kill timing or zombie builders)."
    );
    describe_counter!(
        "rio_builder_log_append_reconnects_total",
        "AppendLog stream reconnects (the log stream to rio-store died and \
         the un-acked tail was replayed into a fresh session). Nonzero \
         during store deploys is expected; sustained = the store is \
         flapping or a replica cannot commit chunks."
    );
    describe_counter!(
        "rio_builder_log_drain_abandoned_total",
        "Builds whose log upload abandoned un-acked lines, labeled by \
         reason: deadline_expired (the post-completion drain deadline \
         hit — store unavailable too long), superseded (the execution \
         was re-dispatched; this attempt's tail is gone), cap_exhausted \
         (per-execution log cap; overflow is discarded by design but \
         still disclosed), panic (the upload task died mid-flight), \
         uploader_dead (lines produced after the upload task died — \
         counted by the producer-side ledger, never silently dropped). \
         Each increment is durable log loss for one build — the lines \
         exist nowhere. A zero-loss abandon (the store already holds \
         the complete log) does NOT count. Alert on any increase."
    );
}
