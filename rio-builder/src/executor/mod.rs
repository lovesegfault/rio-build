//! Build executor: receives WorkAssignment from scheduler, runs builds.
//!
//! Flow:
//! 1. Set up overlay for the build
//! 2. Generate synthetic SQLite DB with input closure metadata
//! 3. Spawn `nix-daemon --stdio` in overlay
//! 4. Client handshake + wopSetOptions + wopBuildDerivation
//! 5. Stream logs via LogBatcher -> BuildLogBatch -> rio-store AppendLog
//! 6. On completion: upload outputs to store via PutPath
//! 7. Send CompletionReport into the build-task sink (the pull loop
//!    forwards it through ReportOutcome)
//! 8. Tear down overlay
//!
//! FOD handling: detect fixed-output derivations via `is_fixed_output`
//! flag on WorkAssignment, skip network namespace isolation.
//!
//! Phase modules (each owns one step of the flow above):
//! - `inputs`: drv fetch, input-closure BFS, FOD hash verify
//! - `sandbox`: synth DB, nix.conf
//! - `daemon`: nix-daemon spawn + STDERR loop
//! - `monitors`: per-build cgroup CPU/OOM watchers + drain
//! - `outputs`: FOD verify gate, upload, daemon→proto result mapping

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use rio_nix::derivation::{Derivation, DerivationLike};
use rio_proto::StoreServiceClient;
use rio_proto::types::{
    BuildPhase, BuildResult as ProtoBuildResult, CompletionReport, WorkAssignment,
};
use rio_proto::validated::ValidatedPathInfo;

use crate::log_stream::{LogBatcher, LogLimits};
use crate::overlay;
use crate::upload;

mod daemon;
mod inputs;
mod monitors;
mod outputs;
mod sandbox;

use daemon::{DaemonBuildOpts, run_daemon_build, spawn_daemon_in_namespace};
use inputs::{compute_input_closure, fetch_drv_from_store, prefetch_manifests};
use monitors::{drain_build_cgroup, spawn_cgroup_monitors};
use outputs::{BuildOutputs, collect_outputs};
use sandbox::prepare_sandbox;

/// Max concurrent gRPC calls for input metadata/drv fetches.
/// Bounds memory (each in-flight QueryPathInfo response is small; each
/// GetPath .drv stream is typically <10 KB). 16 saturates a LAN without
/// thundering the store.
const MAX_PARALLEL_FETCHES: usize = 16;

/// Per-worker immutable configuration for `execute_build`.
///
/// These don't change per-assignment — they're set once at startup from
/// CLI/config. Bundled to avoid `too_many_arguments` on `execute_build`
/// (the alternative was `#[allow]`, which we don't use in prod code).
///
/// Distinct from `BuildSpawnContext` (lib.rs): that holds Arc-shared
/// state (stream_tx, running_build) that needs per-call cloning; this
/// holds `Copy`/cheap-to-copy config that can just be passed by value.
#[derive(Clone)]
pub struct ExecutorEnv {
    /// Mount point of the FUSE store view builds read inputs from.
    pub fuse_mount_point: std::path::PathBuf,
    /// Base directory for per-build overlayfs upper/work dirs.
    pub overlay_base_dir: std::path::PathBuf,
    /// This pod's executor identity, echoed in every report.
    pub executor_id: String,
    /// Per-build log line/byte limits enforced by the stderr loop.
    pub log_limits: LogLimits,
    /// FUSE cache directory (the H9″ occupancy instrument samples
    /// statvfs here once per build completion — live_057-d).
    pub fuse_cache_dir: std::path::PathBuf,
    /// Timeout for the local nix-daemon subprocess build. Used when the
    /// client didn't specify `BuildOptions.build_timeout`. Intentionally
    /// long (default 2h) — some builds genuinely take that long; the
    /// purpose is to bound blast radius of a truly stuck daemon.
    /// Ceiling-bounded BY TYPE (bug_117, R24): `BoundedSecs` saturates
    /// at the shared one-year absurdity ceiling at construction, so
    /// the config lane cannot deliver an `Instant + Duration`-
    /// overflowing value to the stderr loop — same guarantee the wire
    /// lane gets from `WireSecs` below, carried by the field type
    /// instead of a per-consumer mint.
    pub daemon_timeout: rio_common::config::BoundedSecs,
    /// Silence timeout default (seconds). Used when the assignment's
    /// `BuildOptions.max_silent_time` is 0. 0 = disabled.
    ///
    /// Why this exists: Nix ssh-ng clients (protocol 1.38) do NOT send
    /// `wopSetOptions` to the gateway, so client-side `--max-silent-time`
    /// cannot propagate via the BuildOptions path. This config is the
    /// operator's fleet-wide default.
    pub max_silent_time: u64,
    /// Parent cgroup for per-build sub-cgroups. This is
    /// `cgroup::delegated_root()` — the PARENT of the worker's own
    /// cgroup (with `DelegateSubgroup=builds`, worker lives in
    /// `.../service/builds/`; delegated_root is `.../service/`).
    /// Per-build cgroups go here as SIBLINGS. main.rs computes it
    /// ONCE at startup (and calls `enable_subtree_controllers` on
    /// it, which fail-fasts on delegation misconfig). Each build
    /// gets a sub-cgroup named by drv hash. cgroup v2 is a hard
    /// requirement — no Option.
    pub cgroup_parent: std::path::PathBuf,
    /// Builder (airgapped, arbitrary code) or Fetcher (open egress,
    /// FOD-only). The wrong-kind gate in [`execute_build`] checks
    /// `drv.is_fixed_output()` against this BEFORE daemon spawn —
    /// defense-in-depth against scheduler misroutes (ADR-019).
    pub executor_kind: rio_proto::types::ExecutorKind,
    /// Advertised target systems (resolved `RIO_SYSTEMS`). Threaded to
    /// `setup_nix_conf` so the per-build daemon's `extra-platforms`
    /// stays consistent with the advertised target set.
    pub systems: Arc<[String]>,
    /// Resolved `hw_class` (controller-stamped pod annotation,
    /// downward-API volume). Read once per assignment by
    /// `BuildSpawnContext::executor_env` from the shared
    /// `Arc<Mutex<Option<String>>>` so [`execute_build`] gets a plain
    /// `Option<&str>` for the `rio:` banner header. `None` when
    /// non-k8s, the annotator hasn't stamped yet (first ~1s after
    /// pod bind), the resolve poll timed out, or the resolve→bench
    /// task was still running when the first assignment arrived
    /// (`spawn_build_task` waits at most `HW_BENCH_INLINE_WAIT` for
    /// it — the build never blocks on the bench). The banner renders
    /// the system without the `/{hw_class}` suffix in all of these
    /// cases.
    pub hw_class: Option<String>,
    /// Handle to the FUSE local cache. The executor calls
    /// `register_inputs` (JIT allowlist, I-043 redesign) and
    /// `prefetch_manifests` (I-110c PG-skip hints) on it after
    /// `compute_input_closure` and before daemon spawn. `None` in
    /// tests that don't mount FUSE — both calls are skipped.
    pub fuse_cache: Option<Arc<crate::fuse::cache::Cache>>,
    /// Base per-fetch gRPC timeout for the FUSE cache's `GetPath`
    /// (`builder.toml fuse_fetch_timeout_secs`, default 60s). JIT
    /// `lookup` uses `jit_fetch_timeout(this, nar_size)` per path so
    /// large inputs get a size-proportional budget (I-178). Same
    /// value passed to the `PrefetchHint` handler.
    pub fuse_fetch_timeout: Duration,
    /// Cancel flag for THIS build. Set by [`crate::runtime::try_cancel_build`]
    /// before it writes `cgroup.kill`. I-166: threaded into the executor
    /// so the pre-cgroup phase (overlay → resolve → prefetch → warm) can
    /// poll it and abort with [`ExecutorError::Cancelled`] instead of
    /// burning compute until `activeDeadlineSeconds`. Same `Arc` as the
    /// one in `BuildSlot.cancel` and the one `spawn_build_task` reads to
    /// classify the final `Err`.
    pub cancelled: Arc<AtomicBool>,
}

/// Default daemon build timeout: 2 hours. See `ExecutorEnv.daemon_timeout`.
/// Derived from the SHARED const (merged_bug_034 sweep) so the
/// scheduler's token-expiry fallback and this default cannot drift —
/// the scheduler previously mirrored a literal `7200` with a "can't
/// reference the const cross-crate" apology.
pub const DEFAULT_DAEMON_TIMEOUT: Duration =
    Duration::from_secs(rio_common::clamped::DAEMON_DEFAULT_TIMEOUT_SECS);

/// Error type for executor operations.
///
/// No `#[from] anyhow::Error` — every variant has a typed source. A
/// catch-all `#[from] anyhow::Error` would mean any `?` on an
/// anyhow::Result anywhere in execute_build silently becomes the wrong
/// variant. Typed sources make the compiler catch misattribution at
/// the `?` site.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    /// Overlayfs setup failed (mount, upper/work dir creation).
    #[error("overlay setup failed: {0}")]
    Overlay(#[from] overlay::OverlayError),
    /// The blocking overlay-setup task panicked.
    #[error("overlay setup task panicked: {0}")]
    OverlayTaskPanic(tokio::task::JoinError),
    /// Synthetic Nix store SQLite generation failed.
    #[error("synthetic DB generation failed: {0}")]
    SynthDb(#[from] sqlx::Error),
    /// Writing the per-build nix.conf failed.
    #[error("failed to write nix.conf: {0}")]
    NixConf(#[source] std::io::Error),
    /// Spawning the nix-daemon subprocess failed.
    #[error("daemon spawn failed: {0}")]
    DaemonSpawn(std::io::Error),
    /// The daemon wire-protocol handshake failed.
    #[error("daemon handshake failed: {0}")]
    Handshake(#[from] rio_nix::protocol::handshake::HandshakeError),
    /// Post-handshake daemon setup (settings exchange) failed.
    #[error("daemon setup failed: {0}")]
    DaemonSetup(String),
    /// The build itself failed (daemon reported failure).
    #[error("build failed: {0}")]
    BuildFailed(String),
    /// The assignment's `.drv` content is malformed (UTF-8, ATerm parse,
    /// BasicDerivation conversion). Deterministic per-derivation: every
    /// pod sees the same bytes, so retry-on-another-pod is pointless.
    /// Maps to `InputRejected` instead of `InfrastructureFailure`.
    #[error("invalid derivation: {0}")]
    InvalidDerivation(String),
    /// Output upload to the store failed.
    #[error("upload failed: {0}")]
    Upload(#[from] upload::UploadError),
    /// A gRPC call failed (store or scheduler).
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),
    /// Input path metadata fetch failed.
    #[error("input metadata fetch failed for {path}: {source}")]
    MetadataFetch {
        /// The store path whose metadata fetch failed.
        path: String,
        /// The failing gRPC status.
        source: tonic::Status,
    },
    /// Daemon wire-protocol framing error.
    #[error("wire protocol error: {0}")]
    Wire(#[from] rio_nix::protocol::wire::WireError),
    /// Per-build cgroup resource tracking failed.
    #[error("cgroup resource tracking failed: {0}")]
    Cgroup(String),
    /// Pod-level cgroup `memory.events` `oom_kill` incremented during
    /// the build. The kernel killed a build process (cc1, ld, …) for
    /// hitting `memory.max`; make typically respawns it → OOM-loop that
    /// never converges (I-196). Distinct from `BuildFailed` because the
    /// derivation isn't broken — this builder is undersized. Maps to
    /// `InfrastructureFailure` so the scheduler bumps the drv's
    /// `resource_floor` instead of marking it permanently failed.
    #[error("cgroup OOM during build; bumping resource floor")]
    CgroupOom,
    /// The build failed with its overlay prjquota at/over the hard
    /// limit while the node fs had headroom (live_057-a — in-build
    /// ENOSPC; FUSE deliberately surfaces StorageFull as ENOSPC).
    /// The derivation isn't broken — this builder's DISK is
    /// undersized: maps to `InfrastructureFailure` so the scheduler
    /// bumps the drv's disk `resource_floor` (the [`Self::CgroupOom`]
    /// twin; classification predicate at
    /// [`crate::quota::classify_quota_exhaustion`], thresholds
    /// R17-typed beside it). Display pinned to
    /// `rio_proto::DISK_FULL_MSG` (the cross-crate floor-bump
    /// contract; `disk_full_display_contains_proto_constant`).
    #[error("disk full during build (overlay prjquota exhausted); bumping disk floor")]
    DiskFull,
    /// The derivation's FOD-ness does not match this executor kind.
    #[error(
        "wrong executor kind: derivation is_fod={is_fod} but this executor is {executor_kind:?}"
    )]
    WrongKind {
        /// Whether the derivation is fixed-output.
        is_fod: bool,
        /// The kind this executor was spawned as.
        executor_kind: rio_proto::types::ExecutorKind,
    },
    /// Cancel flag observed before the per-build cgroup exists. I-166:
    /// distinct from the post-cgroup path (cgroup.kill → daemon EOF →
    /// `Wire(Io(UnexpectedEof))`) so `is_daemon_transient` doesn't retry
    /// it. `spawn_build_task` maps any `Err` with `cancelled=true` to
    /// `BuildResultStatus::Cancelled` regardless of variant; this
    /// variant just makes the pre-cgroup abort explicit in logs.
    #[error("build cancelled before cgroup creation")]
    Cancelled,
}

impl ExecutorError {
    /// Whether this error indicates a transient daemon-side failure
    /// worth retrying locally before reporting to the scheduler.
    ///
    /// Covers the daemon-crashed-mid-handshake cases:
    /// - `DaemonSpawn`: nix-daemon failed to exec (transient FS/mount race)
    /// - `Handshake`: daemon died before protocol negotiation completed
    /// - `Wire(Io(UnexpectedEof))`: daemon crashed mid-conversation
    ///   (core dump, OOM-kill, SIGABRT) → pipe closed → "early eof"
    ///
    /// Does NOT cover `BuildFailed` (real builder failure — retrying
    /// won't help), `Upload`/`Grpc`/`MetadataFetch` (network-side,
    /// scheduler's retry policy handles re-dispatch with backoff), or
    /// `Overlay`/`SynthDb`/`NixConf` (deterministic setup failures —
    /// same inputs, same failure).
    // r[impl builder.retry.daemon-transient]
    pub fn is_daemon_transient(&self) -> bool {
        match self {
            ExecutorError::DaemonSpawn(_) => true,
            ExecutorError::Handshake(_) => true,
            ExecutorError::Wire(rio_nix::protocol::wire::WireError::Io(e)) => {
                e.kind() == std::io::ErrorKind::UnexpectedEof
            }
            _ => false,
        }
    }

    /// Whether this error is deterministic per-derivation under the
    /// scheduler's routing (same outcome on every pod the scheduler
    /// would pick) and so should map to `InputRejected` rather than
    /// `InfrastructureFailure`. Prevents the scheduler from burning N
    /// ephemeral cold-starts on a derivation that will fail identically
    /// each time before the poison threshold trips.
    ///
    /// Everything NOT matched here stays `InfrastructureFailure`: node-
    /// or network-local conditions (overlay/mount, IO, gRPC, daemon
    /// crashes, cgroup, OOM) where another pod plausibly succeeds.
    pub fn is_permanent(&self) -> bool {
        matches!(
            self,
            // FOD/non-FOD routed to wrong executor kind. Not "same on
            // every pod" literally (a correct-kind pod would succeed),
            // but re-dispatch is deterministic on persisted
            // `is_fixed_output` (dispatch.rs kind_for_drv), so retry
            // hits the same wrong kind — InfrastructureFailure would
            // just fleet-exhaust to the same poison. Short-circuit.
            // I-057: fix the routing input, never the permanence here.
            ExecutorError::WrongKind { .. }
            // .drv content failed UTF-8/ATerm/BasicDerivation parse.
            // The bytes are what they are; every pod parses identically.
            | ExecutorError::InvalidDerivation(_)
        )
    }
}

/// Max local retry attempts for transient daemon failures before
/// reporting InfrastructureFailure to the scheduler. Bounded so a
/// persistent crash (bad synth DB, broken nix binary) doesn't spin
/// indefinitely.
pub const DAEMON_RETRY_MAX: u32 = 3;

/// Backoff between daemon retry attempts. Sequence: 500ms, 1s, 2s.
/// Total worst-case retry overhead ~3.5s — small vs the scheduler
/// round-trip (re-dispatch + re-fetch closure + re-generate synth
/// DB). No jitter: only one daemon per pod, no herd to break.
pub const DAEMON_RETRY_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
    base: Duration::from_millis(500),
    mult: 2.0,
    cap: Duration::from_secs(2),
    jitter: rio_common::backoff::Jitter::None,
};

/// The builder's internal build-task → runtime envelope.
///
/// The spawned build task sends its [`CompletionReport`] (and phase
/// edges) through the process-lifetime build-task sink typed with this
/// enum; the pull loop consumes the sink and forwards the report via
/// `ReportOutcome`. Never serialized — the envelope is builder-local
/// (re-homed from the proto `ExecutorMessage`, which build_types.proto
/// retains until a future proto change removes it). The payloads stay
/// proto types: [`CompletionReport`] is what `ReportOutcome` sends
/// upstream, and [`BuildPhase`] is `BuildEvent.phase`'s payload type.
#[derive(Debug, Clone, PartialEq)]
pub enum BuildTaskMessage {
    /// Build result. The pull loop forwards it through `ReportOutcome`
    /// until the scheduler acknowledges it. Boxed because
    /// `CompletionReport` (~328 B) dwarfs the `Phase` arm — the same
    /// size asymmetry the proto envelope handled by boxing its oneof
    /// (see rio-proto/build.rs).
    Completion(Box<CompletionReport>),
    /// Build phase change (forwarded resSetPhase). Sent unbatched per
    /// `builder.stderr.forward-set-phase`; the pull loop drains and
    /// discards it (no scheduler-side consumer — the stream-era
    /// BuildExecution carrier is removed).
    Phase(BuildPhase),
}

/// Result of executing a single build.
#[derive(Debug)]
pub struct ExecutionResult {
    /// The derivation path that was built.
    pub drv_path: String,
    /// The proto BuildResult.
    pub result: ProtoBuildResult,
    /// Assignment token from the WorkAssignment.
    pub assignment_token: String,
    /// Peak memory in bytes from the per-build cgroup's `memory.peak`.
    /// Tree-wide: daemon + builder + every child compiler. 0 = build
    /// failed before cgroup populated (executor error before spawn).
    pub peak_memory_bytes: u64,
    /// Peak CPU cores-equivalent, polled 1Hz from cgroup `cpu.stat`
    /// usage_usec. `delta_usec / elapsed_usec`, max over build lifetime.
    /// Tree-wide. 0.0 = build failed before any sample (exited <1s).
    pub peak_cpu_cores: f64,
    /// Project-quota `dqb_curspace` for the overlay upper dir, sampled
    /// IMMEDIATELY BEFORE `teardown_overlay()`. `dqb_curspace` is
    /// CURRENT bytes (not a kernel-tracked peak), so reading it after
    /// teardown's `remove_dir_all` returns ≈0 — hence stash here
    /// instead of in `cgroup::final_sample()`. `None` = no prjquota
    /// (tmpfs / node without `-o prjquota`).
    pub peak_disk_bytes: Option<u64>,
    /// bug_286: upload-lane store-unreachability evidence, carried from
    /// `BuildOutputs` (set by the upload `Err` arm via
    /// `upload::is_store_unreachable`). Feeds the `upload_transport`
    /// lane of `StoreEvidenceSet` at completion-stamp time. `false` on
    /// every path that never reached (or never failed) the upload.
    pub store_unreachable: bool,
    /// `RIO_BUILDER_SCRIPT` override for `CompletionReport.final_resources`.
    /// `None` on every real build; `Some` only from
    /// `fixture::scripted_result` (feature `test-fixtures`). When set,
    /// `runtime::result::ok_completion` uses this instead of the cgroup-
    /// poll snapshot so the SLA VM scenario can script
    /// `cpu_limit_cores`/`cpu_seconds_total` deterministically.
    pub fixture_resources: Option<rio_proto::types::ResourceUsage>,
}

/// Result of [`execute_build`]: the inner result PLUS the cgroup peak
/// samples, which survive even when `result` is `Err`. Mirrors
/// `DaemonOutcome` one level up so `runtime::result::err_completion`
/// can report the actual `memory.peak` for a `CgroupOom`'d build (the
/// single most actionable sizing signal) instead of hardcoding 0.
///
/// On `Ok`, the peak fields are duplicated inside `ExecutionResult` —
/// `ok_completion` reads from there; the outer copies are for the `Err`
/// path only.
#[derive(Debug)]
pub struct ExecuteOutcome {
    /// Build outcome: outputs + stats, or the typed failure.
    pub result: Result<ExecutionResult, ExecutorError>,
    /// 0 only for pre-cgroup setup errors (drv parse, WrongKind,
    /// overlay, daemon-spawn). Populated for `CgroupOom` /
    /// post-handshake `Wire` / `Upload` / `BuildFailed`.
    pub peak_memory_bytes: u64,
    /// Peak concurrent CPU usage (cores) sampled from the cgroup.
    pub peak_cpu_cores: f64,
    /// `None` = no prjquota OR pre-cgroup error. Sampled BEFORE
    /// `build_result?` so an OOM'd build also reports it.
    pub peak_disk_bytes: Option<u64>,
    /// Highest line number this attempt's `BuildLogBatch`es reached
    /// (header + LogBatcher output). The runtime daemon-transient retry
    /// loop feeds it back as the next attempt's `first_line` so output
    /// numbering continues monotonically across attempts; once the loop
    /// breaks it places the banner footer here. `0` only for pre-header
    /// errors on the first attempt (drv parse, WrongKind).
    pub final_line_count: u64,
    /// The build process's own outcome string for the banner footer
    /// (`ok` / `failed (...)`; the `cancelled` override happens later,
    /// in `runtime::result::final_footer_result`), computed BEFORE
    /// collect_outputs so a successful build that fails its upload still
    /// reports `ok` here (CompletionReport carries the upload failure).
    /// `None` when no daemon process ran in THIS attempt — pre-daemon
    /// setup error or `RIO_BUILDER_SCRIPT` fixture short-circuit. The
    /// runtime tracks the most recent `Some(...)` across the retry loop
    /// so a footer is sent whenever ANY attempt ran a daemon: an
    /// all-pre-daemon assignment (header only, no output) gets no
    /// footer — that absence is the documented "build never started"
    /// signal. Sending one footer per attempt produces conflicting
    /// `rio: result` lines for one exec_id (bug_013).
    pub footer_result: Option<String>,
}

impl ExecuteOutcome {
    /// Pre-cgroup setup error: cgroup never populated, peaks genuinely 0.
    /// `final_line_count` is the watermark the caller has already pushed
    /// (`first_line` for pre-header errors, `batcher_seed` for
    /// post-header pre-daemon errors); `footer_result` is `None` —
    /// no daemon process ran in this attempt.
    fn pre_cgroup(e: ExecutorError, final_line_count: u64) -> Self {
        Self {
            result: Err(e),
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            peak_disk_bytes: None,
            final_line_count,
            footer_result: None,
        }
    }

    /// Fixture short-circuit: scripted peaks live on the
    /// `ExecutionResult`; copy them out so the `Err`-path consumers
    /// (which only exist on real builds) see consistent shape.
    #[cfg(feature = "test-fixtures")]
    fn fixture(r: ExecutionResult, final_line_count: u64) -> Self {
        Self {
            peak_memory_bytes: r.peak_memory_bytes,
            peak_cpu_cores: r.peak_cpu_cores,
            peak_disk_bytes: r.peak_disk_bytes,
            result: Ok(r),
            final_line_count,
            footer_result: None, // RIO_BUILDER_SCRIPT short-circuit: no daemon ran.
        }
    }
}

/// What `execute_build`'s pre-daemon inner block produced. Exists so
/// the block's `?` sites stay `?` (pre-cgroup errors → peaks=0) while
/// the post-daemon section can carry peaks across `Err`.
enum PreDaemon {
    /// `RIO_BUILDER_SCRIPT` short-circuit (feature `test-fixtures`).
    /// Variant is cfg-gated to its only construction site so the match
    /// stays exhaustive in both feature configurations.
    #[cfg(feature = "test-fixtures")]
    Fixture(ExecutionResult),
    /// Daemon ran; carry locals needed for the post-daemon section.
    Ran {
        overlay_mount: overlay::OverlayMount,
        input_paths: Vec<String>,
        outcome: DaemonOutcome,
    },
}

/// Execute a single build assignment.
///
/// This is the main entry point for building a derivation. It handles
/// the full lifecycle: overlay setup, synthetic DB, daemon invocation,
/// log streaming, output upload, and cleanup.
///
/// This is the ROOT span for the worker's contribution to a build trace.
/// Per observability.typ (trace structure), child spans are:
/// `fetch_drv_from_store`, `compute_input_closure`,
/// `generate_db`, `spawn_daemon_in_namespace`, `run_daemon_build`,
/// `upload_all_outputs`. `drv_path` is the primary identifier (matches
/// scheduler's `drv_key` span field via the derivation hash substring).
#[instrument(
    skip_all,
    fields(
        drv_path = %assignment.drv_path,
        executor_id = %env.executor_id,
        is_fod = assignment.is_fixed_output,
    )
)]
pub async fn execute_build(
    assignment: &WorkAssignment,
    env: &ExecutorEnv,
    store_client: &mut StoreServiceClient<Channel>,
    log_tx: &mpsc::Sender<BuildTaskMessage>,
    upload_tx: &mpsc::Sender<rio_proto::types::BuildLogBatch>,
    first_line: u64,
) -> ExecuteOutcome {
    let drv_path = &assignment.drv_path;
    let build_id = sanitize_build_id(drv_path);

    tracing::info!(
        drv_path = %drv_path,
        build_id = %build_id,
        is_fod = assignment.is_fixed_output,
        "starting build"
    );

    // ── Head section: drv parse + WrongKind gate. ─────────────────────
    // Explicit early-returns (not `?`) because the return type isn't
    // `Result`. These are pre-cgroup → peaks genuinely 0.

    // 1. Parse the derivation. Scheduler inlines drv_content for
    // missing-output nodes; empty means cache-hit or
    // inline-budget exceeded, so fall back to store fetch.
    let drv = if assignment.drv_content.is_empty() {
        match fetch_drv_from_store(store_client, drv_path).await {
            Ok(d) => d,
            Err(e) => return ExecuteOutcome::pre_cgroup(e, first_line),
        }
    } else {
        // Strict UTF-8 — matches the else-branch (parse_from_nar uses
        // strict from_utf8 at derivation/mod.rs:168). Lossy would silently
        // produce U+FFFD → ATerm parse fails with a confusing "unexpected
        // character" instead of the real UTF-8 error. P0017's 2f807a4
        // eliminated this pattern; 395e826f reintroduced it one day after
        // P0020 closed. Clippy disallowed-methods (P0290) prevents round 3.
        let parsed = std::str::from_utf8(&assignment.drv_content)
            .map_err(|e| {
                ExecutorError::InvalidDerivation(format!("drv content is not valid UTF-8: {e}"))
            })
            .and_then(|t| {
                Derivation::parse(t).map_err(|e| {
                    ExecutorError::InvalidDerivation(format!("failed to parse derivation: {e}"))
                })
            });
        match parsed {
            Ok(d) => d,
            Err(e) => return ExecuteOutcome::pre_cgroup(e, first_line),
        }
    };

    // wkr-fod-flag-trust (21-p2-p3-rollup Batch B): the .drv is ground
    // truth — it's what the worker actually executes. The scheduler-sent
    // `assignment.is_fixed_output` is derived from the same .drv on the
    // scheduler side, but drift is possible (stale proto, scheduler bug,
    // inline-budget edge). Compute from `drv` here; warn if the two
    // disagree so scheduler-side drift is visible. The assignment field
    // stays read for one release cycle; remove in a follow-up once this
    // warn! has been silent in prod.
    let is_fod = drv.is_fixed_output();
    if is_fod != assignment.is_fixed_output {
        tracing::warn!(
            drv_path = %drv_path,
            drv_is_fod = is_fod,
            assignment_is_fod = assignment.is_fixed_output,
            "FOD flag disagreement: drv.is_fixed_output() != assignment.is_fixed_output — using drv"
        );
    }

    // r[impl builder.executor.kind-gate]
    // Wrong-kind gate BEFORE overlay setup or daemon spawn. The
    // scheduler's hard_filter should never misroute, but a bug or
    // stale-generation race must not grant a builder internet access
    // even transiently. `is_fod` re-derived from the .drv above
    // (wkr-fod-flag-trust) — ground truth, not the scheduler's word.
    // Running pre-overlay also means a misroute wastes no mount
    // namespace setup and is unit-testable without CAP_SYS_ADMIN.
    if is_fod != (env.executor_kind == rio_proto::types::ExecutorKind::Fetcher) {
        return ExecuteOutcome::pre_cgroup(
            ExecutorError::WrongKind {
                is_fod,
                executor_kind: env.executor_kind,
            },
            first_line,
        );
    }

    metrics::gauge!("rio_builder_builds_active").increment(1.0);
    // rio_builder_builds_total is incremented at completion (main.rs) with
    // an outcome label so SLI queries can compute success rate.
    let build_start = std::time::Instant::now();
    let _build_guard = scopeguard::guard((), move |()| {
        metrics::gauge!("rio_builder_builds_active").decrement(1.0);
        metrics::histogram!("rio_builder_build_duration_seconds")
            .record(build_start.elapsed().as_secs_f64());
    });

    // r[impl obs.log.worker-header]
    // Banner header — the first thing in the build log, ahead of any
    // build output. Sent directly on the per-build log-upload channel
    // (not through `LogBatcher` — that's created inside
    // `run_daemon_lifecycle`, three call frames down); the batcher is
    // seeded with `batcher_seed` (`HEADER_LINE_COUNT` on the first
    // attempt, the prior attempt's `final_line_count` on a retry) so
    // the build's real output numbers continue past the header instead
    // of colliding at line 0.
    //
    // Display-only: see `banner.rs`'s module doc. The lines flow
    // through the normal `BuildLogBatch` → rio-store `AppendLog` →
    // chunked-S3 pipeline and are visible in `rio-cli logs`, the
    // dashboard, and (via the gateway's `TailLog` subscription)
    // `nix build -L` and Nix's post-failure log tail.
    //
    // Sent AFTER the wrong-kind gate so a misroute (FOD → builder pod)
    // doesn't pollute the log with a header for a build that was never
    // going to run. Sent BEFORE overlay setup so a failed mount still
    // leaves a self-describing log (header, no output, no footer).
    //
    // Sent only on the FIRST daemon-transient retry attempt
    // (`first_line == 0`): the runtime retry loop (runtime/mod.rs)
    // re-invokes `execute_build` up to DAEMON_RETRY_MAX more times for
    // one assignment; re-emitting the header per attempt would write
    // duplicate "first lines" at line 0 and break the scheduler ring
    // buffer's line-number monotonicity (bug_013).
    //
    // `exec_id` is the per-execution UUIDv7 from `WorkAssignment` —
    // empty before commit `34956e1f4` plumbed `assign_to_worker`'s
    // mint, but always populated against the current scheduler. The
    // log header is the only place the worker echoes it.
    let batcher_seed = if first_line == 0 {
        send_banner_batch(
            upload_tx,
            drv_path,
            &env.executor_id,
            0,
            crate::banner::header_lines(
                &assignment.exec_id,
                drv.platform(),
                env.hw_class.as_deref(),
                assignment.assigned_cores,
                assignment.assigned_mem_bytes,
                assignment.assigned_disk_bytes,
            ),
        )
        .await;
        crate::banner::HEADER_LINE_COUNT
    } else {
        first_line
    };

    // ── Pre-daemon block: overlay → resolve → sandbox → daemon. ───────
    // Wrapped so the `?` sites stay `?`: every error here is pre-cgroup
    // (cgroup created INSIDE `run_daemon_lifecycle` after daemon spawn),
    // including `run_daemon_lifecycle`'s own outer `Err` (its doc:
    // "returns Err only for setup failures BEFORE the cgroup kill-guard
    // is in place"). Converted to `ExecuteOutcome::pre_cgroup` once at
    // the match below — no per-site churn.
    let pre: Result<PreDaemon, ExecutorError> =
        async {
            // 2. Set up overlay. `setup_overlay` is synchronous (mkdir + stat +
            // overlayfs mount syscall); run on the blocking pool so a slow mount
            // (e.g., FUSE lower stalled on remote fetch) doesn't starve the Tokio
            // worker thread and stall the async runtime.
            let fuse_mp = env.fuse_mount_point.clone();
            let overlay_base = env.overlay_base_dir.clone();
            let build_id_owned = build_id.clone();
            let overlay_mount = tokio::task::spawn_blocking(move || {
                overlay::setup_overlay(&fuse_mp, &overlay_base, &build_id_owned)
            })
            .await
            .map_err(ExecutorError::OverlayTaskPanic)??;

            // 3. Resolve inputDrvs → BasicDerivation + full input closure.
            let ResolvedInputs {
                basic_drv,
                input_paths,
                input_sized,
                input_metadata,
            } = resolve_inputs(&*store_client, &drv, drv_path).await?;

            // r[impl builder.cores.cgroup-clamp+2]
            // Compute once: feeds BOTH nix.conf `cores=` (defense-in-depth)
            // and wopSetOptions build_cores below. I-196/I-197 rationale at
            // crate::cgroup::effective_cores.
            let effective_cores = crate::cgroup::effective_cores(&env.cgroup_parent);

            // RIO_BUILDER_SCRIPT fixture intercept (sla-sizing VM scenario):
            // short-circuit the daemon lifecycle and report scripted telemetry
            // so the explore ladder can be driven without wall-clock minutes
            // per probe. After overlay+input setup so the FUSE/JIT paths still
            // exercise; before sandbox prep so no nix-daemon spawns.
            #[cfg(feature = "test-fixtures")]
            if let Some(pname) = drv.env().get("pname")
                && let Some(o) = crate::fixture::lookup(pname, effective_cores)
            {
                tracing::info!(%pname, effective_cores, wall_secs = o.wall_secs,
            "RIO_BUILDER_SCRIPT: short-circuiting build with scripted telemetry");
                let r = crate::fixture::scripted_result(
                    drv_path,
                    &assignment.assignment_token,
                    effective_cores,
                    o,
                );
                if let Err(e) =
                    tokio::task::spawn_blocking(move || overlay::teardown_overlay(overlay_mount))
                        .await
                        .map_err(ExecutorError::OverlayTaskPanic)?
                {
                    tracing::warn!(error = %e, "fixture-path overlay teardown failed");
                }
                return Ok(PreDaemon::Fixture(r));
            }

            // 4. Populate sandbox: synth DB, nix.conf.
            prepare_sandbox(
                &overlay_mount,
                &drv,
                drv_path,
                input_metadata,
                effective_cores,
                &env.systems,
            )
            .await?;

            // 4b. Arm JIT FUSE fetch (I-043 redesign): register the input
            // closure as the FUSE `lookup()` allowlist. The daemon's first
            // overlay→FUSE `lstat` of each input now blocks-and-fetches in
            // FUSE userspace; on fetch failure `lookup()` returns EIO (NEVER
            // ENOENT — overlay would negative-cache it). Names NOT in the
            // allowlist (`.lock`, `.chroot`, output-path probes) get fast
            // ENOENT without contacting the store.
            //
            // This replaces the pre-daemon `warm_inputs_in_fuse` phase, which
            // fetched the WHOLE closure (~800–1500 paths) up-front — defeating
            // lazy fetch for builds that touch a fraction of their closure.
            // The I-165 47-min hang window is gone with it: register +
            // prefetch_manifests together are <100 ms (one HashMap extend +
            // one BatchGetManifest RPC).
            //
            // r[impl builder.cancel.pre-cgroup-deferred+2]
            // I-166: the cgroup doesn't exist yet (created post-spawn below),
            // so a Cancel that arrived during overlay/resolve/prepare landed
            // as ENOENT in `try_cancel_build` — which now LEAVES the flag
            // set. Check it here AND again after the prefetch RPC below:
            // overlay → resolve → prepare_sandbox is sub-second, but
            // prefetch_manifests is a network RPC bounded only by the
            // store-client timeout — a cancel landing during it would
            // otherwise ride into the daemon spawn (bug_377 rider).
            if env.cancelled.load(Ordering::Acquire) {
                tracing::info!(drv_path = %drv_path, "build cancelled (pre-cgroup)");
                return Err(ExecutorError::Cancelled);
            }
            if let Some(cache) = &env.fuse_cache {
                // r[impl builder.fuse.jit-register]
                cache.register_inputs(input_sized.iter().filter_map(|(p, sz)| {
                    Some((rio_nix::store_path::basename(p)?.to_owned(), *sz))
                }));
                metrics::gauge!("rio_builder_jit_inputs_registered")
                    .set(cache.known_inputs_len() as f64);
                // I-110c: prime manifest hints so each JIT fetch's `GetPath`
                // skips PG. ~1600 PG hits/builder → ≤2. Best-effort — on
                // Unimplemented (old store) or any error, the per-path
                // `GetPath` queries PG as before.
                prefetch_manifests(store_client, cache, &input_paths).await;
            }
            // Second pre-cgroup checkpoint: prefetch_manifests is the one
            // bounded-but-slow RPC in the pre-cgroup window; consult the
            // cancel flag after it so SIGTERM/Cancel during the prefetch
            // aborts before the daemon spawn (bug_377 rider).
            if env.cancelled.load(Ordering::Acquire) {
                tracing::info!(drv_path = %drv_path, "build cancelled (pre-cgroup, post-prefetch)");
                return Err(ExecutorError::Cancelled);
            }

            // 5. Spawn nix-daemon --stdio --store 'local?root={build_dir}'.
            //
            // The daemon reads/writes the chroot store at the per-build dir:
            //   {build_dir}/nix/store      → overlay merged (FUSE inputs ∪ outputs)
            //   {build_dir}/nix/var/nix/db → synthetic SQLite DB
            //   {build_dir}/etc/nix        → WORKER_NIX_CONF (via NIX_CONF_DIR)
            //
            // Its OWN binary + libs come from host `/nix/store` (the builder's
            // namespace) — structurally separate from the per-build store, so a
            // build whose `$out` collides with the daemon's runtime closure
            // (I-060) can't shadow it. nix's nested sandbox bind-mounts inputs
            // from realStoreDir (`{build_dir}/nix/store/...`) to the build's
            // canonical `/nix/store/...`.
            let opts = resolve_build_opts(assignment, env, effective_cores, batcher_seed);

            let outcome = run_daemon_lifecycle(
                &overlay_mount,
                env,
                &build_id,
                drv_path,
                &basic_drv,
                opts,
                log_tx,
                upload_tx,
            )
            .await?;

            Ok(PreDaemon::Ran {
                overlay_mount,
                input_paths,
                outcome,
            })
        }
        .await;

    let (
        overlay_mount,
        input_paths,
        build_result,
        peak_memory_bytes,
        peak_cpu_cores,
        peak_quota_bytes,
        final_line_count,
    ) = match pre {
        Err(e) => return ExecuteOutcome::pre_cgroup(e, batcher_seed),
        #[cfg(feature = "test-fixtures")]
        Ok(PreDaemon::Fixture(r)) => return ExecuteOutcome::fixture(r, batcher_seed),
        Ok(PreDaemon::Ran {
            overlay_mount,
            input_paths,
            outcome:
                DaemonOutcome {
                    build_result,
                    peak_memory_bytes,
                    peak_cpu_cores,
                    peak_quota_bytes,
                    final_line_count,
                },
        }) => (
            overlay_mount,
            input_paths,
            build_result,
            peak_memory_bytes,
            peak_cpu_cores,
            peak_quota_bytes,
            final_line_count,
        ),
    };

    // r[impl obs.log.worker-header]
    // Footer result string — computed BEFORE `collect_outputs` consumes
    // `build_result` below so the eventual footer carries the build
    // process's OWN result (`ok` even when upload fails afterward;
    // `CompletionReport` carries the post-daemon failure). The footer
    // batch itself is sent by the runtime AFTER the daemon-transient
    // retry loop with the most recent attempt's `footer_result` /
    // `final_line_count` — `execute_build` is called once per attempt,
    // and re-emitting the footer per attempt would write conflicting
    // `rio: result` lines for one exec_id (bug_013).
    //
    // Pre-cgroup early returns above (`Err(e)`, `Fixture`) carry
    // `footer_result: None`: those builds never ran a daemon in THIS
    // attempt. The runtime tracks the most recent `Some(...)` across
    // attempts so the footer is sent whenever any attempt produced
    // output. If NO attempt ran a daemon, the runtime sends no footer:
    // header with no output and no footer is the documented signal
    // that the build never started.
    // ── Post-daemon: peaks now in scope; carry across every Err. ──────
    // r[impl builder.cgroup.memory-peak+2]
    // Sample prjquota BEFORE the build_result/collect_outputs
    // early-returns — `dqb_curspace` is current bytes (overlay still
    // mounted on this path) so an OOM'd build also reports
    // peak_disk_bytes. Previously sampled only at line 11 (teardown),
    // which the `?` at build_result skipped.
    // r[impl builder.disk.quota-classified+2]
    // r[impl builder.disk.satisfiable-letter]
    // merged_bug_074: the classification consumes the DURING-BUILD
    // usage peak (the 1Hz monitor max-track, folded with this
    // post-daemon one-shot — keep-failed is unset, so the daemon has
    // already deleted a failed build's scratch by this line and the
    // one-shot under-reads exactly the dominant exhaustion shape) and
    // a node-headroom sample DECOUPLED from the project clamp (under
    // enforced prjquota + PROJINHERIT the kernel clamps statvfs taken
    // inside the project view to `limit − used`, which made the old
    // same-dir conjunct pair kernel-unsatisfiable: quota-at-limit
    // forced node_free ≤ slack < headroom). The fold runs BEFORE the
    // footer renders so the report and the banner agree.
    let quota_status = crate::quota::status(&env.overlay_base_dir).ok().flatten();
    let quota_peak = match (quota_status, peak_quota_bytes) {
        (Some(q), Some(peak)) => Some(crate::quota::QuotaStatus {
            used_bytes: q.used_bytes.max(peak),
            hard_limit_bytes: q.hard_limit_bytes,
        }),
        (Some(q), None) => Some(q),
        // No limit read now (quota record gone / fs error) — a bare
        // usage peak has no exhaustion predicate to feed; the
        // non-quota lane keeps the report.
        (None, _) => None,
    };
    let peak_disk_bytes = quota_peak.map(|q| q.used_bytes);
    let node_free = crate::quota::node_free_bytes_decoupled(
        &env.overlay_base_dir,
        quota_peak.and_then(|q| q.hard_limit_bytes),
    );
    let build_result = apply_disk_override(quota_peak, node_free, build_result);

    // H9″ (live_057-d instrument): fuse-cache occupancy, statfs-
    // derived, refreshed once per build completion beside the quota
    // sample — the measured-RULED fuse-addend trigger's data source
    // (WO-S7-6 reads the series over its soak window).
    if let Some(used) = crate::quota::fs_used_bytes(&env.fuse_cache_dir) {
        metrics::gauge!("rio_builder_fuse_cache_bytes_used").set(used as f64);
    }

    let footer_result = footer_result_str(&build_result);
    let post_err = |e| ExecuteOutcome {
        result: Err(e),
        peak_memory_bytes,
        peak_cpu_cores,
        peak_disk_bytes,
        final_line_count,
        footer_result: Some(footer_result.clone()),
    };

    // 10. Collect outputs (borrows &overlay_mount; must precede teardown).
    // The daemon error is NOT propagated yet — teardown below must run
    // on the spawn_blocking pool for both Ok AND Err. A `return
    // post_err(e)` here would drop `overlay_mount` synchronously on this
    // tokio worker (multi-GB `remove_dir_all`), and `Wire(UnexpectedEof)`
    // is `is_daemon_transient()` → the retry loop at runtime/mod.rs calls
    // back in instead of exiting, so the worker would block across the
    // retry and starve the async runtime.
    let collect_result = match build_result {
        Err(e) => Err(e),
        Ok(br) => collect_outputs(
            &br,
            store_client,
            &overlay_mount,
            &drv,
            drv_path,
            is_fod,
            &input_paths,
            &assignment.assignment_token,
        )
        .await
        .map(|o| (br, o)),
    };

    // 11. Tear down overlay UNCONDITIONALLY via spawn_blocking — covers
    // Ok AND Err. The post_err returns below no longer hold a mounted
    // OverlayMount, so Drop is a no-op (mounted=false after
    // teardown_overlay). teardown_overlay does `remove_dir_all` over
    // upper/nix/store/ — multi-GB / 100k+ inodes for large builds. Same
    // runtime-starvation concern as setup_overlay above.
    //
    // We don't override a successful build result just because its own
    // teardown fails. Teardown failure increments
    // `rio_builder_overlay_unmount_failures_total` (in OverlayMount::Drop,
    // centralized so ?-early-returns and panics also count); with one
    // build per pod a leaked mount is reclaimed when the pod is discarded.
    let merged_path = overlay_mount.merged_dir().to_path_buf();
    match tokio::task::spawn_blocking(move || overlay::teardown_overlay(overlay_mount)).await {
        Err(join_err) => return post_err(ExecutorError::OverlayTaskPanic(join_err)),
        Ok(Err(e)) => {
            tracing::error!(
                error = %e,
                merged = %merged_path.display(),
                "overlay teardown failed; mount leaked"
            );
            // Metric incremented in Drop (see overlay.rs).
        }
        Ok(Ok(())) => {}
    }

    // Propagate any daemon/collect error AFTER teardown ran — WITH peaks
    // attached, not via `?`.
    let (
        _build_result,
        BuildOutputs {
            proto_result,
            store_unreachable,
        },
    ) = match collect_result {
        Ok(pair) => pair,
        Err(e) => return post_err(e),
    };

    ExecuteOutcome {
        result: Ok(ExecutionResult {
            drv_path: drv_path.clone(),
            result: proto_result,
            assignment_token: assignment.assignment_token.clone(),
            peak_memory_bytes,
            peak_cpu_cores,
            peak_disk_bytes,
            store_unreachable,
            fixture_resources: None,
        }),
        peak_memory_bytes,
        peak_cpu_cores,
        peak_disk_bytes,
        final_line_count,
        footer_result: Some(footer_result),
    }
}

/// Effective per-build options after applying assignment →
/// worker-config → cgroup-clamp precedence, plus the `LogBatcher` line
/// seed for daemon-transient retry continuity. `batcher_seed` is
/// logging plumbing, not a build option — bundled here so
/// `run_daemon_lifecycle` stays under `clippy::too_many_arguments`.
struct BuildOpts {
    timeout: Duration,
    /// Bounds-typed at this process's assignment seam (merged_bug_034)
    /// — a raw u64 cannot be assigned, so an unclamped wire value is
    /// unrepresentable past `resolve_build_opts`.
    max_silent_time: rio_common::clamped::WireSecs,
    build_cores: u64,
    /// Initial `LogBatcher` line counter: `HEADER_LINE_COUNT` on the
    /// first attempt, the prior attempt's `final_line_count` on a
    /// daemon-transient retry. See `execute_build`'s `first_line` param.
    batcher_seed: u64,
}

/// Compute the effective build options for this assignment.
///
/// The scheduler computes `BuildOptions` per-derivation from the
/// intersecting builds' options (`actor/build.rs`
/// `WireSecs::min_permissive` for timeouts, max for cores). `None` →
/// daemon defaults: unbounded silence, nproc cores. 0 → 0 on the wire
/// = unbounded/all-cores to the daemon — the scheduler's fold already
/// handles the 0-means-unset semantics; we pass values through the
/// seam mint (merged_bug_034: each process clamps its OWN ingress —
/// defense in depth, the scheduler is not trusted to have clamped).
///
/// `batcher_seed` is passed through verbatim — it's not a build option,
/// see [`BuildOpts`].
fn resolve_build_opts(
    assignment: &WorkAssignment,
    env: &ExecutorEnv,
    effective_cores: u32,
    batcher_seed: u64,
) -> BuildOpts {
    use rio_common::clamped::WireSecs;
    let opts = assignment.build_options.as_ref();
    // This process's proto→domain seam: mint through the saturating
    // constructor, then convert — `to_duration_nonzero` is ≤ the
    // one-year ceiling, so the stderr-loop's `Instant + timeout`
    // deadline math is in-range for ANY wire value (u64::MAX
    // included; the pre-fix verbatim `Duration::from_secs` panicked
    // the deadline add). The fallback arm is bounded BY TYPE
    // (bug_117): `env.daemon_timeout` is `BoundedSecs`, so the config
    // lane — the COMMON lane, since ssh-ng assignments carry no
    // build_options — carries the same ceiling from parse.
    let timeout = opts
        .and_then(|o| WireSecs::from_wire(o.build_timeout).to_duration_nonzero())
        .unwrap_or_else(|| env.daemon_timeout.duration());
    // Assignment's max_silent_time wins if nonzero; else the worker
    // config default. Same 0-means-unset semantics as build_timeout above.
    // Config default exists because Nix ssh-ng clients don't send
    // wopSetOptions to the gateway — the BuildOptions path is dead until
    // gateway-side propagation lands. (The config lane is
    // operator-trusted; routing it through the same mint just bounds
    // it at the same absurdity ceiling.)
    let max_silent_time = opts
        .map(|o| WireSecs::from_wire(o.max_silent_time))
        .filter(|w| !w.is_unset())
        .unwrap_or_else(|| WireSecs::from_wire(env.max_silent_time));
    // r[impl builder.cores.cgroup-clamp+2]
    // I-196: NEVER pass build_cores=0 to the daemon. 0 means "use
    // nproc", and nproc inside a pod sees ALL node cores (cgroup CPU
    // quota throttles scheduling, doesn't hide CPUs). On a 16-core
    // node a `tiny` (0.5-core, 1Gi) builder would run `make -j16` →
    // 16×cc1×~100MB → cgroup OOM-loop. Clamp to the pod's cpu.max
    // (I-197: pools set limits.cpu == requests.cpu so cpu.max is
    // always a real quota), and cap any client-requested value at the
    // same ceiling — a client asking for --cores 64 on a 2-core pod
    // gets 2. Computed once in the caller (also written to nix.conf in
    // prepare_sandbox as defense-in-depth).
    let effective_cores = u64::from(effective_cores);
    // r[impl sched.sla.cores-reach-nix-build-cores]
    // ADR-023: scheduler-assigned cores are authoritative when set. The
    // scheduler solved cores=N for the SLA target and provisioned the pod
    // accordingly; passing exactly N to wopSetOptions makes
    // NIX_BUILD_CORES deterministic so the SLA model's
    // cpu_seconds_total / assigned_cores ratio is meaningful. Still
    // clamped to the cgroup ceiling — defense against scheduler/kubelet
    // disagreeing on what was actually provisioned (the cgroup is ground
    // truth). Absent → pre-ADR-023 fallback (client request capped at
    // cgroup ceiling).
    let build_cores = match assignment.assigned_cores {
        Some(n) if n > 0 => u64::from(n).min(effective_cores),
        _ => match opts.map(|o| o.build_cores).filter(|&c| c > 0) {
            Some(client) => client.min(effective_cores),
            None => effective_cores,
        },
    };
    tracing::debug!(
        effective_cores,
        build_cores,
        assigned = assignment.assigned_cores,
        client_requested = opts.map(|o| o.build_cores),
        "build_cores resolved (assigned > client > cgroup-clamp)"
    );
    BuildOpts {
        timeout,
        max_silent_time,
        build_cores,
        batcher_seed,
    }
}

/// Result of [`run_daemon_lifecycle`]: the inner build result (NOT yet
/// `?`-propagated — cgroup teardown must run regardless) plus the
/// resource samples read from the per-build cgroup before it was dropped.
struct DaemonOutcome {
    build_result: Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
    peak_memory_bytes: u64,
    peak_cpu_cores: f64,
    /// merged_bug_074: the during-build prjquota usage peak from the
    /// 1Hz monitor (max-tracked `dqb_curspace`). `None` = no prjquota
    /// or the build exited before the first tick — the classification
    /// seam falls back to its post-daemon one-shot.
    peak_quota_bytes: Option<u64>,
    /// Lines accounted for by the [`LogBatcher`] (the seeded banner
    /// header offset + everything the stderr loop flushed). Read by
    /// [`execute_build`] to set the footer banner's `first_line_number`
    /// so it follows the build output instead of colliding. Equal to
    /// the seeded `opts.batcher_seed` when [`run_daemon_build`]'s
    /// setup failed before the loop ran (no build output produced).
    final_line_count: u64,
}

/// Spawn `nix-daemon`, attach it to a per-build cgroup, run the build,
/// then unconditionally kill + drain the cgroup.
///
/// Returns `Err` only for setup failures BEFORE the cgroup kill-guard is
/// in place (daemon spawn, cgroup create/add — `kill_on_drop` covers the
/// daemon for those). Any error from `run_daemon_build` itself is carried
/// in `DaemonOutcome.build_result` so the caller can propagate it AFTER
/// the cgroup has been torn down.
#[instrument(skip_all, fields(drv_path = %drv_path))]
#[allow(clippy::too_many_arguments)]
async fn run_daemon_lifecycle(
    overlay_mount: &overlay::OverlayMount,
    env: &ExecutorEnv,
    build_id: &str,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    opts: BuildOpts,
    log_tx: &mpsc::Sender<BuildTaskMessage>,
    upload_tx: &mpsc::Sender<rio_proto::types::BuildLogBatch>,
) -> Result<DaemonOutcome, ExecutorError> {
    tracing::info!(drv_path = %drv_path, "spawning nix-daemon in mount namespace");
    let mut daemon = spawn_daemon_in_namespace(overlay_mount).await?;
    tracing::info!(drv_path = %drv_path, pid = ?daemon.id(), "nix-daemon spawned; starting handshake");

    // Per-build cgroup. Created AFTER spawn (we need the PID) but
    // BEFORE run_daemon_build — critical ordering: the daemon must
    // be in the cgroup BEFORE it forks the builder (step 4 of
    // run_daemon_build's handshake), otherwise the builder inherits
    // the PARENT cgroup and we measure only daemon RSS — right back
    // to the phase2c VmHWM bug. The handshake hasn't started yet
    // (run_daemon_build does it), so this is safe.
    //
    // `?` on both create and add_process: if cgroup setup fails here,
    // the build fails. cgroup v2 is a hard requirement — we already
    // validated the parent cgroup at startup, so failure here is
    // exceptional (stale directory from a crash that we couldn't
    // rmdir because it has a stuck process, or daemon died between
    // spawn and now). Both are real errors the operator should see.
    //
    // build_id = sanitize_build_id(drv_path). nixbase32 hash chars are
    // valid cgroup names; sanitize collapses anything outside
    // [A-Za-z0-9_-] to '_' (drv names can carry `?id=...`, `+`, etc. —
    // see I-167). Same name as the overlay directory — easy to
    // correlate in debugging.
    let build_cgroup = crate::cgroup::BuildCgroup::create(&env.cgroup_parent, build_id)
        .map_err(|e| ExecutorError::Cgroup(format!("create sub-cgroup: {e}")))?;
    let daemon_pid = daemon
        .id()
        .ok_or_else(|| ExecutorError::Cgroup("daemon PID unavailable (died at spawn?)".into()))?;
    build_cgroup
        .add_process(daemon_pid)
        .map_err(|e| ExecutorError::Cgroup(format!("add daemon to cgroup: {e}")))?;

    // Kill-guard: any `?` between here and the explicit drop at the
    // bottom of this function fires this. The explicit kill() + drain
    // + drop below remain the PRIMARY path (they wait for drain; this
    // guard doesn't). scopeguard::guard not defer! — we need to hand
    // it the PathBuf, not borrow build_cgroup.
    let cgroup_kill_path = build_cgroup.path().to_path_buf();
    let cgroup_kill_guard = scopeguard::guard(cgroup_kill_path, |p| {
        // Best-effort. No drain — we're in Drop, can't await. The
        // BuildCgroup's own Drop runs right after this and will EBUSY
        // if the SIGKILL hasn't landed yet; that's the existing leak
        // path, just now with the kill attempted.
        let _ = std::fs::write(p.join("cgroup.kill"), "1");
    });

    let monitors = spawn_cgroup_monitors(&build_cgroup, &env.cgroup_parent, &env.overlay_base_dir);

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // The cgroup setup above (create/add_process)
    // is NOT inside this helper — its `?` paths rely on the
    // kill_on_drop set in spawn_daemon_in_namespace as a safety
    // net. The explicit kill below remains the primary cleanup
    // (graceful, bounded wait for reap); kill_on_drop covers early
    // returns between spawn and here.
    // Seeded with `opts.batcher_seed`: `execute_build` already sent the
    // `rio:` banner header on the first attempt, or a prior attempt's
    // output ended here on a daemon-transient retry. Without this, the
    // build's line 0 collides with `rio: exec` (or with a prior attempt's
    // output) and the dashboard's `since_line` resumption desyncs.
    let batcher = LogBatcher::new(
        drv_path.to_owned(),
        env.executor_id.clone(),
        env.log_limits,
        opts.batcher_seed,
    );
    let (build_result, final_line_count) = run_daemon_build(
        &mut daemon,
        drv_path,
        basic_drv,
        DaemonBuildOpts {
            build_timeout: opts.timeout,
            // Bounded by the WireSecs mint at resolve_build_opts —
            // the stderr loop's `Duration::from_secs(raw)` and its
            // `last_output + silence` add are in-range.
            max_silent_time: opts.max_silent_time.raw(),
            build_cores: opts.build_cores,
        },
        batcher,
        log_tx,
        upload_tx,
    )
    .await;

    // Stop both monitors and read their results. The last CPU sample
    // is up to 1s stale; good enough (peak CPU doesn't change in the
    // last second of a multi-minute build). The scopeguards inside
    // `monitors` also abort on drop; this explicit stop is the happy-
    // path fast stop (guard fires redundantly after, which is a no-op
    // on an already-aborted handle).
    let (peak_cpu_cores, oom_detected, peak_quota_bytes) = monitors.stop();
    let build_result = apply_oom_override(oom_detected, build_result);

    // Read cgroup memory.peak. Kernel-tracked lifetime max of the
    // WHOLE TREE — daemon + builder + every child. One read, no
    // polling. This FIXES the phase2c bug: VmHWM on daemon.id()
    // measured ~10MB (daemon's own RSS) because the builder was a
    // FORKED child, not exec'd — the builder's memory never showed
    // in daemon's /proc.
    //
    // 0 on None (file missing would mean memory controller not
    // enabled, but enable_subtree_controllers at startup would have
    // caught that — this is a belt-and-suspenders default).
    let peak_memory_bytes = build_cgroup.memory_peak().unwrap_or(0);

    // ALWAYS kill the daemon, regardless of success/failure.
    if let Err(e) = daemon.kill().await {
        tracing::warn!(error = %e, "daemon.kill() failed (process may already be dead)");
    }
    // Reap the zombie (bounded wait).
    match tokio::time::timeout(Duration::from_secs(2), daemon.wait()).await {
        Ok(Ok(_)) => {}
        Ok(Err(e)) => tracing::warn!(error = %e, "daemon.wait() failed after kill"),
        Err(_) => tracing::warn!("daemon did not exit within 2s after kill (possible zombie)"),
    }

    drain_build_cgroup(build_cgroup).await;

    // Defuse: explicit kill+drain above already ran; guard is redundant.
    scopeguard::ScopeGuard::into_inner(cgroup_kill_guard);

    // (Final log flush happens inside read_build_stderr_loop — it owns
    // the batcher by-value.)

    Ok(DaemonOutcome {
        build_result,
        peak_memory_bytes,
        peak_cpu_cores,
        peak_quota_bytes,
        final_line_count,
    })
}

/// Reclassify a daemon error as `CgroupOom` when `oom_detected` AND
/// the build already failed; preserve `Ok(Built)` even if OOM fired.
///
/// Two paths set `oom_detected`:
/// - the 1Hz watcher writes `cgroup.kill` → daemon EOF →
///   `Err(Wire(UnexpectedEof))`. `is_err()` always true.
/// - `monitors.stop()`'s `final_oom` synchronous read does NOT kill,
///   so a build whose script tolerated a child OOM (`tool || true`,
///   `make -k`, retry-runner) returns `Ok(Built)` here. Discarding
///   that would loop re-dispatch on a build that deterministically
///   succeeds-with-child-OOM, ratcheting the floor until poisoned.
///
/// `Err` → `CgroupOom` keeps the runtime from hitting
/// `is_daemon_transient` (3× local OOM-loop) and from `BuildFailed`
/// (drv isn't broken). `Ok` → kept; metric still emitted because the
/// OOM is a real sizing signal.
// r[impl builder.oom.cgroup-watch+3]
fn apply_oom_override(
    oom_detected: bool,
    build_result: Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
) -> Result<rio_nix::protocol::build::BuildResult, ExecutorError> {
    if !oom_detected {
        return build_result;
    }
    metrics::counter!("rio_builder_cgroup_oom_total").increment(1);
    match &build_result {
        Ok(_) => {
            tracing::warn!(
                "oom_kill incremented but build succeeded; keeping Ok(Built) \
                 (script tolerated the OOM-killed child)"
            );
            build_result
        }
        // merged_bug_078 (the oom TWIN seam, R28 result-plane axis):
        // the retired `(true, Err(_))` wildcard rewrote EVERY error
        // under oom_detected — laundering the cancel law, the
        // network/store lanes, and the permanent lane into a memory
        // sizing signal. The typed allow-list decides per shape: the
        // ordinary daemon failure and the watcher-kill EOF signature
        // (cgroup.kill → daemon EOF — the designed I-196 chain) are
        // the oom letter's lawful claims; every Owned shape keeps its
        // own law.
        Err(_) => match sizing_rewrite_authority(&build_result) {
            SizingRewrite::Rewritable | SizingRewrite::OomKillSignature => {
                Err(ExecutorError::CgroupOom)
            }
            SizingRewrite::Owned(law) => {
                tracing::warn!(
                    owning_law = law,
                    "oom_kill incremented but the failure shape is owned \
                     by another law; classification kept (no memory-floor \
                     laundering)"
                );
                build_result
            }
        },
    }
}

/// R28 (the result-plane axis; merged_bug_078 + the oom twin): the
/// typed classification-authority allow-list BOTH override seams
/// consume. For every shape of the bare
/// `Result<BuildResult, ExecutorError>` between daemon outcome and
/// report assembly: may a sizing override (disk / oom) REWRITE it?
///
/// The axis this seals: `r[builder.timeout.no-reassign]` (and every
/// sibling classification law) is censused at the Nix→Proto mapping
/// seam, but the result plane upstream of the mapping was writable —
/// an override could rewrite a classification-authoritative status
/// one seam above the census while the census stayed green
/// (merged_bug_078's laundering). The allow-list replaces each
/// override's hand-enumerated exemptions with one exhaustive
/// authority: NO catch-all arms (R14) — a new status or error variant
/// fails compilation here until it takes an explicit row, and the
/// review default for a new row is `Owned`.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum SizingRewrite {
    /// The ordinary ENOSPC-consistent daemon-phase failure (the
    /// daemon's failed BuildResult, or a build-environment write
    /// failure inside the quota'd overlay): EITHER sizing axis may
    /// claim it when its own evidence corroborates (the quota
    /// predicate for disk; `oom_detected` for oom).
    Rewritable,
    /// The oom watcher's kill signature ONLY (`cgroup.kill` → daemon
    /// EOF): the OOM axis claims it — that EOF is the watcher's own
    /// mechanical consequence (the designed I-196 chain, and the
    /// reason the override exists at all: it must win before
    /// `is_daemon_transient` turns the kill into a 3× local OOM
    /// loop). The DISK axis may NOT claim it: without an oom_kill the
    /// shape belongs to the daemon-transient retry law.
    OomKillSignature,
    /// Another law owns the shape — neither override may rewrite it;
    /// the str names the owning law for the refusal trace.
    Owned(&'static str),
}

fn sizing_rewrite_authority(
    result: &Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
) -> SizingRewrite {
    use rio_nix::protocol::build::BuildStatus as S;
    use rio_nix::protocol::wire::WireError as W;
    match result {
        Ok(r) => match r.status {
            // Success is never reclassified (a build that succeeded
            // despite touching its quota / tolerating a child OOM is
            // a success).
            S::Built | S::Substituted | S::AlreadyValid | S::ResolvesToAlreadyValid => {
                SizingRewrite::Owned("success — preserved")
            }
            // The sibling sizing/limit laws: each owns its shape end
            // to end (deadline doubling for TimedOut —
            // r[builder.timeout.no-reassign]; the log-limit cap).
            S::TimedOut => SizingRewrite::Owned("timeout law (deadline floor, no-reassign)"),
            S::LogLimitExceeded => SizingRewrite::Owned("log-limit law"),
            // The ordinary daemon failures — the live_057 face that
            // assembled PermanentFailure on in-build ENOSPC.
            S::PermanentFailure | S::MiscFailure | S::TransientFailure => SizingRewrite::Rewritable,
            // A cached PRIOR failure carries no evidence about THIS
            // attempt's resources.
            S::CachedFailure => SizingRewrite::Owned("cached prior failure"),
            S::DependencyFailed => SizingRewrite::Owned("dependency law"),
            S::NotDeterministic => SizingRewrite::Owned("check-mode law"),
            S::InputRejected | S::OutputRejected => SizingRewrite::Owned("validation law"),
            S::NoSubstituters => SizingRewrite::Owned("substitution law"),
        },
        Err(e) => match e {
            // Build-environment writes inside the quota'd overlay —
            // ENOSPC-consistent setup failures.
            ExecutorError::Overlay(_) => SizingRewrite::Rewritable,
            ExecutorError::SynthDb(_) => SizingRewrite::Rewritable,
            ExecutorError::NixConf(_) => SizingRewrite::Rewritable,
            // The daemon's reported failure (the dominant shape).
            ExecutorError::BuildFailed(_) => SizingRewrite::Rewritable,
            // The local daemon-transient retry law owns its shapes:
            // a rewrite here would skip the local retry
            // (runtime/mod.rs `is_daemon_transient` consult) AND
            // misclassify a crash as a sizing signal.
            ExecutorError::DaemonSpawn(_) => {
                SizingRewrite::Owned("daemon-transient retry law (spawn)")
            }
            ExecutorError::Handshake(_) => {
                SizingRewrite::Owned("daemon-transient retry law (handshake)")
            }
            // The one transient shape the OOM axis lawfully claims:
            // the watcher's own cgroup.kill → daemon EOF.
            ExecutorError::Wire(W::Io(io)) if io.kind() == std::io::ErrorKind::UnexpectedEof => {
                SizingRewrite::OomKillSignature
            }
            ExecutorError::Wire(W::Io(_)) => SizingRewrite::Owned("wire i/o law"),
            ExecutorError::Wire(
                W::StringTooLong(_)
                | W::CollectionTooLarge(_)
                | W::NonZeroPadding(_)
                | W::InvalidUtf8(_)
                | W::InvalidNarHash(_)
                | W::FrameTooLarge(_)
                | W::FramedStreamTooLarge(_),
            ) => SizingRewrite::Owned("wire-protocol law"),
            ExecutorError::OverlayTaskPanic(_) => SizingRewrite::Owned("panic — not a signal"),
            ExecutorError::DaemonSetup(_) => SizingRewrite::Owned("daemon setup (pre-build)"),
            ExecutorError::InvalidDerivation(_) => {
                SizingRewrite::Owned("permanent law (InputRejected)")
            }
            // Store / network lanes: their own evidence machinery
            // (bug_286 store evidence, the breaker) owns attribution.
            ExecutorError::Upload(_) => SizingRewrite::Owned("upload/store lane"),
            ExecutorError::Grpc(_) => SizingRewrite::Owned("network lane"),
            ExecutorError::MetadataFetch { .. } => SizingRewrite::Owned("network lane"),
            ExecutorError::Cgroup(_) => SizingRewrite::Owned("cgroup mechanics"),
            // Already sizing letters — idempotent, never re-claimed
            // by the SIBLING axis (the more specific signal wins).
            ExecutorError::CgroupOom => SizingRewrite::Owned("already the oom letter"),
            ExecutorError::DiskFull => SizingRewrite::Owned("already the disk letter"),
            ExecutorError::WrongKind { .. } => SizingRewrite::Owned("kind misroute (permanent)"),
            ExecutorError::Cancelled => SizingRewrite::Owned("cancel law"),
        },
    }
}

/// Reclassify a FAILED result as `DiskFull` when the during-build
/// quota peak shows the overlay at/over its prjquota hard limit and
/// the node fs is not itself exhausted — the [`apply_oom_override`]
/// twin at the result seam (live_057-a; merged_bug_074/078 rebuilt).
/// Precedence and preservation:
///
/// - The rewrite is gated on [`sizing_rewrite_authority`] ==
///   [`SizingRewrite::Rewritable`] (the R28 allow-list BOTH override
///   seams consume): only the ordinary ENOSPC-consistent daemon
///   failures may be claimed. Success, TimedOut, LogLimitExceeded,
///   the daemon-transient shapes, the cancel/network/store/permanent
///   lanes, and the sibling `CgroupOom` letter all keep their own
///   laws — the retired form's bare `failed` predicate rewrote every
///   non-OOM failure, laundering sibling laws into a disk doubling
///   the moment the producer's predicate became satisfiable.
/// - The claim then requires
///   [`crate::quota::classify_quota_exhaustion`] over the during-
///   build usage peak and the DECOUPLED node-headroom sample. No
///   sample / no limit / node exhausted / no decoupled vantage → the
///   result is kept verbatim (the non-quota lane: the node's
///   exhaustion is not the build's sizing signal).
fn apply_disk_override(
    quota: Option<crate::quota::QuotaStatus>,
    node_free_bytes: Option<u64>,
    build_result: Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
) -> Result<rio_nix::protocol::build::BuildResult, ExecutorError> {
    let rewritable = matches!(
        sizing_rewrite_authority(&build_result),
        SizingRewrite::Rewritable
    );
    let quota_attributed = rewritable
        && match (quota, node_free_bytes) {
            (Some(q), Some(free)) => crate::quota::classify_quota_exhaustion(q, free),
            // No quota sample or no decoupled node read → no
            // attribution (the non-quota lane keeps the report).
            _ => false,
        };
    if quota_attributed {
        tracing::warn!(
            used = quota.map(|q| q.used_bytes),
            hard_limit = quota.and_then(|q| q.hard_limit_bytes),
            node_free = node_free_bytes,
            "build failed with overlay prjquota exhausted; classifying \
             DiskFull (disk sizing signal — the scheduler bumps the \
             disk floor)"
        );
        Err(ExecutorError::DiskFull)
    } else {
        build_result
    }
}

/// Resolved build inputs: the BasicDerivation (inputDrvs collapsed into
/// inputSrcs) and the full transitive input closure for the synth DB.
struct ResolvedInputs {
    /// BasicDerivation with inputDrv outputs resolved into inputSrcs.
    /// Sent to nix-daemon via wopBuildDerivation.
    basic_drv: rio_nix::derivation::BasicDerivation,
    /// Full transitive input closure (BFS over QueryPathInfo references,
    /// seeded from input_srcs + resolved inputDrv outputs). Used for
    /// `prefetch_manifests` and the output reference-scan candidate
    /// set. Derived from `input_metadata` (each entry's `.path`).
    input_paths: Vec<String>,
    /// `(path, nar_size)` for every closure path. Used for the FUSE
    /// warm — I-178: per-path timeout and overall deadline scale with
    /// NAR size so a 1.9 GB input isn't aborted at the flat 60 s.
    /// Derived from `input_metadata` alongside `input_paths`.
    input_sized: Vec<(String, u64)>,
    /// PathInfo for every closure path, captured during the BFS so the
    /// synth DB ValidPaths table can be built without a second
    /// QueryPathInfo pass (I-106).
    input_metadata: Vec<ValidatedPathInfo>,
}

/// Resolve inputDrvs → BasicDerivation + compute full input closure.
///
/// r[impl builder.executor.resolve-input-drvs]
///
/// `drv.to_basic()` only copies static input_srcs (e.g., busybox); it
/// does NOT resolve inputDrvs to their output paths. nix-daemon's
/// sandbox only bind-mounts inputSrcs into the chroot, so without this
/// the builder can't find its input derivations' outputs. Each
/// inputDrv's .drv file is already in rio-store (uploaded by the gateway
/// during SubmitBuild); fetch + parse to get output paths.
///
/// Also computes the full transitive input closure (BFS over
/// QueryPathInfo references) for the synth DB ValidPaths table. The
/// scheduler sends a PrefetchHint (approx_input_closure) before the
/// WorkAssignment so the FUSE cache starts warming; that's a HINT, not
/// a replacement for this computation — the synth DB needs the FULL
/// closure.
#[instrument(skip_all, fields(drv_path = %drv_path))]
async fn resolve_inputs(
    store_client: &StoreServiceClient<Channel>,
    drv: &Derivation,
    drv_path: &str,
) -> Result<ResolvedInputs, ExecutorError> {
    let mut resolved_input_srcs = drv.input_srcs().clone();
    // Collect owned (path, names) pairs up-front so the async closures
    // don't borrow from `drv` (which is not 'static inside spawn_monitored).
    let input_drv_specs: Vec<(String, std::collections::BTreeSet<String>)> = drv
        .input_drvs()
        .iter()
        .map(|(p, n)| (p.clone(), n.clone()))
        .collect();
    let n_input_drvs = input_drv_specs.len();
    let fetch_drvs_start = std::time::Instant::now();
    let fetched: Vec<Vec<String>> = stream::iter(input_drv_specs)
        .map(|(path, names)| {
            let mut client = store_client.clone();
            async move {
                let input_drv = fetch_drv_from_store(&mut client, &path).await?;
                let matching: Vec<String> = input_drv
                    .outputs()
                    .iter()
                    .filter(|out| names.contains(out.name()))
                    // Floating-CA outputs have path="" in the .drv
                    // (computed post-build). Reaching this loop with a
                    // CA input means the scheduler's resolve failed
                    // (RealisationMissing, PG blip) and dispatched
                    // unresolved content. Passing "" to
                    // compute_input_closure → `invalid store path ""` →
                    // InfrastructureFailure → unbounded retry storm
                    // (9748 events observed). Filter here so the build
                    // fails later on the unresolved PLACEHOLDER in
                    // env/args (a proper BuildFailed, not an infra
                    // loop). The scheduler-side fix (insert realisation
                    // at completion) makes this path unreachable in
                    // normal operation; this is defense-in-depth.
                    .filter(|out| {
                        if out.path().is_empty() {
                            tracing::warn!(
                                input_drv = %path,
                                output_name = out.name(),
                                "floating-CA input unresolved by scheduler; \
                                 filtering empty path (build will fail on placeholder)"
                            );
                            false
                        } else {
                            true
                        }
                    })
                    .map(|out| out.path().to_string())
                    .collect();
                Ok::<_, ExecutorError>(matching)
            }
        })
        .buffer_unordered(MAX_PARALLEL_FETCHES)
        .try_collect()
        .await?;
    tracing::debug!(
        n_input_drvs,
        elapsed = ?fetch_drvs_start.elapsed(),
        "resolve_inputs: fetched all input .drv files"
    );
    // Defense: filter empty paths. A floating-CA input derivation's .drv
    // file has `out.path() == ""` (the path is unknown until the build
    // runs). If the scheduler dispatched us WITHOUT resolving inputDrvs
    // to realized paths (maybe_resolve_ca gate miss, or resolve failed),
    // we'd pass `""` to nix-daemon's inputSrcs → bind-mount of "" →
    // build fails with a cryptic ENOENT. Dropping empties here makes the
    // failure mode clearer (the build still fails — it's missing an
    // input — but the log shows the actual missing path, not "").
    //
    // WARN-log indicates a scheduler bug: the scheduler should have
    // resolved CA inputDrvs before dispatch. Zero warns expected in
    // steady-state; any warn here means investigate the scheduler's
    // `maybe_resolve_ca` path.
    let mut dropped_empty = 0usize;
    for paths in fetched {
        for p in paths {
            if p.is_empty() {
                dropped_empty += 1;
            } else {
                resolved_input_srcs.insert(p);
            }
        }
    }
    if dropped_empty > 0 {
        tracing::warn!(
            drv_path = %drv_path,
            dropped = dropped_empty,
            "dropped empty inputDrv output paths (floating-CA input not resolved by scheduler)"
        );
    }
    let basic_drv = rio_nix::derivation::BasicDerivation::new(
        drv.outputs().to_vec(),
        resolved_input_srcs.clone(),
        drv.platform().to_string(),
        drv.builder().to_string(),
        drv.args().to_vec(),
        drv.env().clone(),
    )
    .map_err(|e| {
        ExecutorError::InvalidDerivation(format!("failed to build BasicDerivation: {e}"))
    })?;

    // Compute input closure for the synthetic DB (ValidPaths table)
    // and the FUSE warm. The BFS seeds with resolved_input_srcs so
    // it walks the runtime references of inputDrv OUTPUTS — a .drv
    // file's narinfo references don't include its outputs (those are
    // in the ATerm structure, not the NAR content), so seeding only
    // input_drvs().keys() would miss them. I-043: warm count=8 with
    // the post-BFS merge — autotools-hook (a transitive runtime dep
    // via stdenv-the-output) never reached.
    let input_metadata =
        compute_input_closure(store_client, drv, drv_path, &resolved_input_srcs).await?;
    let input_paths: Vec<String> = input_metadata
        .iter()
        .map(|m| m.store_path.to_string())
        .collect();
    // I-178: project (path, nar_size) for the JIT FUSE allowlist
    // (`register_inputs`). ValidatedPathInfo already has nar_size from
    // BatchQueryPathInfo (authoritative; the ManifestHint.info.nar_size
    // is best-effort). Two projections from input_metadata is cheaper
    // than passing &input_metadata around — the other consumers
    // (prefetch_manifests, ref-scan) want plain &[String].
    let input_sized: Vec<(String, u64)> = input_metadata
        .iter()
        .map(|m| (m.store_path.to_string(), m.nar_size))
        .collect();

    Ok(ResolvedInputs {
        basic_drv,
        input_paths,
        input_sized,
        input_metadata,
    })
}

/// Convert a derivation path to a safe build ID for directory names.
///
/// Public so `spawn_build_task` can predict the cgroup path for the
/// cancel registry (cgroup_parent/sanitize_build_id(drv_path)) without
/// execute_build having to report it back. The cgroup is created
/// DURING execute_build (after daemon spawn, needs PID), so spawn_
/// build_task registers the path PREDICTIVELY before spawning and
/// removes it after. If a cancel arrives before the cgroup exists,
/// cgroup.kill returns ENOENT — try_cancel_build logs and moves on
/// (the build will fail anyway since the daemon dies when the cgroup
/// IS created with a stale kill file — no, cgroup.kill isn't a
/// persistent file, it's a write-once trigger. ENOENT just means no
/// kill happened, the build proceeds. Harmless race — a cancel
/// arriving THAT early is extremely rare and the scheduler will
/// re-send on the next dispatch cycle if the build keeps running).
// r[impl builder.exec.build-id-sanitized]
pub fn sanitize_build_id(drv_path: &str) -> String {
    // /nix/store/abc...-foo.drv -> abc___-foo_drv
    //
    // Derivation names from nixpkgs are NOT constrained to filesystem- or
    // URL-safe characters. fetchpatch against a Gentoo mirror produces e.g.
    // `opensp-1.5.2-c11-using.patch?id=688d9675...drv` (I-167). The build_id
    // becomes an overlay directory name, a cgroup v2 name, and a component of
    // the synth_db sqlite:// URI — so anything outside [A-Za-z0-9_-] is
    // collapsed to `_`. nixbase32 hash chars (0-9 a-z) are already in-set.
    drv_path
        .rsplit('/')
        .next()
        .unwrap_or(drv_path)
        .chars()
        .map(|c| {
            if c.is_ascii_alphanumeric() || c == '_' || c == '-' {
                c
            } else {
                '_'
            }
        })
        .collect()
}

/// Send a worker banner (header or footer) as a `BuildLogBatch`
/// directly on the per-build log-upload channel, bypassing the
/// [`LogBatcher`] (which is created inside [`run_daemon_lifecycle`]
/// and consumed by the stderr loop — not in scope at the call sites).
///
/// Returns whether the batch entered the channel. A closed channel
/// means the uploader task died (panicked); the build continues
/// without log persistence, but the loss is NOT silent
/// (merged_bug_009): the header is display-only, while FOOTER lines
/// participate in the sealed `final_line_count` — the caller must not
/// seal lines that exist nowhere, and either banner's death is
/// disclosed through the single chokepoint as `uploader_dead` (the
/// same population as every other refused send).
///
/// `pub(crate)` because the banner footer is sent from
/// `runtime::spawn_build_task` (after the daemon-transient retry loop —
/// once per assignment) rather than from `execute_build` (once per
/// attempt). See `execute_build`'s `first_line` param doc and bug_013.
// r[impl builder.log.loss-disclosure+5]
pub(crate) async fn send_banner_batch(
    upload_tx: &mpsc::Sender<rio_proto::types::BuildLogBatch>,
    drv_path: &str,
    executor_id: &str,
    first_line_number: u64,
    lines: Vec<Vec<u8>>,
) -> bool {
    let n = lines.len() as u64;
    let batch = rio_proto::types::BuildLogBatch {
        derivation_path: drv_path.to_owned(),
        executor_id: executor_id.to_owned(),
        first_line_number,
        lines,
    };
    if upload_tx.send(batch).await.is_err() {
        tracing::warn!(
            drv_path = %drv_path,
            lines = n,
            "banner batch dropped: log upload channel closed (uploader task died)"
        );
        crate::log_upload::disclose_uploader_dead(n);
        return false;
    }
    true
}

/// Map a [`run_daemon_lifecycle`] result to the banner footer's
/// `result` string: `ok` or `failed (<reason>)`.
///
/// `cancelled` is deliberately NOT in this function's domain. The error
/// variant cannot decide it: a pre-cgroup cancel
/// ([`ExecutorError::Cancelled`]) routes through
/// [`ExecuteOutcome::pre_cgroup`] (`footer_result: None` — no daemon
/// ran, no footer at all) and never reaches this function, and a
/// post-cgroup cancel kills the daemon, which surfaces here as
/// `Wire(Io(UnexpectedEof))` — indistinguishable from a daemon crash.
/// The runtime's once-per-assignment footer send overrides this string
/// to `cancelled` from the build's cancel flag
/// (`runtime::result::final_footer_result`), the same way
/// `err_completion` decides `BuildResultStatus::Cancelled`.
///
/// Display-only: the precise classification (`InfrastructureFailure`
/// vs `Failed` vs `Cancelled`, retry eligibility, error chain) lives
/// on `CompletionReport`. The footer just lets a human reading the
/// log know the build's own outcome without scrolling to the
/// scheduler's view. `BuildStatus` doesn't carry an exit code (the
/// daemon protocol only reports a status enum), so `failed (exit N)`
/// from the design spec maps to the closest available signal: the
/// status discriminant.
fn footer_result_str(
    build_result: &Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
) -> String {
    use rio_nix::protocol::build::BuildStatus;
    match build_result {
        Ok(br) if br.status.is_success() => "ok".to_string(),
        Ok(br) => match br.status {
            BuildStatus::TimedOut => "failed (timed out)".to_string(),
            BuildStatus::LogLimitExceeded => "failed (log limit exceeded)".to_string(),
            other => format!("failed ({other:?})"),
        },
        // The full error string is on `CompletionReport.error_msg`;
        // the footer is a one-line summary.
        Err(_) => "failed (executor error)".to_string(),
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    /// Contract pin: rio-scheduler `handle_infrastructure_failure`
    /// matches `error_msg.contains(rio_proto::CGROUP_OOM_MSG)` to
    /// trigger `r[sched.sla.reactive-floor]`. thiserror's `#[error]`
    /// attr can't reference a `const` without const-format, so this
    /// test is the cross-crate drift guard — rewording the Display
    /// string at line ~179 fails HERE instead of silently disabling
    /// the floor-bump in production.
    /// W10-CM (live_057-a, the seam matrix): the result seam
    /// reclassifies a failed build to DiskFull exactly when the
    /// quota predicate holds. Pre-fix red (the identity seam — NO
    /// worker-side disk classification existed; grep: zero
    /// statfs/ENOSPC consumers in the result plane): the quota-at-
    /// limit failed build stayed PermanentFailure — the build's-own-
    /// fault lane, retry-then-poison, no floor recovery.
    // r[verify builder.disk.quota-classified+2]
    #[test]
    fn disk_override_classifies_quota_exhausted_failures() {
        use rio_nix::protocol::build::{BuildResult, BuildStatus};
        let gib = 1u64 << 30;
        let at_limit = Some(crate::quota::QuotaStatus {
            used_bytes: 25 * gib,
            hard_limit_bytes: Some(25 * gib),
        });
        let failed = || -> Result<BuildResult, ExecutorError> {
            Ok(BuildResult {
                status: BuildStatus::PermanentFailure,
                ..Default::default()
            })
        };

        // The positive cell: failed + quota at limit + node healthy
        // → DiskFull (InfrastructureFailure at err_completion).
        let got = apply_disk_override(at_limit, Some(50 * gib), failed());
        assert!(
            matches!(got, Err(ExecutorError::DiskFull)),
            "left: the quota-exhausted failure presents as ordinary \
             PermanentFailure (retry-then-poison, floor untouched) / \
             right: classified DiskFull; got {got:?}"
        );

        // Ok(Built) preserved even at the limit.
        let built = apply_disk_override(
            at_limit,
            Some(50 * gib),
            Ok(BuildResult {
                status: BuildStatus::Built,
                ..Default::default()
            }),
        );
        assert!(matches!(built, Ok(ref b) if b.status == BuildStatus::Built));

        // CgroupOom preserved (the more specific signal).
        let oom = apply_disk_override(at_limit, Some(50 * gib), Err(ExecutorError::CgroupOom));
        assert!(matches!(oom, Err(ExecutorError::CgroupOom)));

        // The false-positive corner: below the slack band → kept.
        let below = Some(crate::quota::QuotaStatus {
            used_bytes: 25 * gib - crate::quota::DISK_FULL_QUOTA_SLACK_BYTES - 1,
            hard_limit_bytes: Some(25 * gib),
        });
        let kept = apply_disk_override(below, Some(50 * gib), failed());
        assert!(
            matches!(kept, Ok(ref b) if b.status == BuildStatus::PermanentFailure),
            "below the slack band the failure keeps its own lane"
        );

        // The attribution corner: node fs exhausted → NOT
        // quota-classified (the non-quota lane keeps the report).
        let node_full = apply_disk_override(
            at_limit,
            Some(crate::quota::DISK_FULL_NODE_HEADROOM_BYTES - 1),
            failed(),
        );
        assert!(
            matches!(node_full, Ok(ref b) if b.status == BuildStatus::PermanentFailure),
            "the node's exhaustion is not the build's sizing signal"
        );

        // No sample at all → kept verbatim.
        let none = apply_disk_override(None, None, failed());
        assert!(matches!(none, Ok(ref b) if b.status == BuildStatus::PermanentFailure));
    }

    /// W11-U (merged_bug_078, the §1.6.4-3 JOINT pair; R28
    /// result-plane axis): the override seams may NOT launder a
    /// sibling law's classification into a sizing letter. The
    /// ARMED-PRODUCER state (quota at limit + decoupled node
    /// headroom — kernel-possible only after merged_bug_074's
    /// sampling fix) is constructed at unit level here, never as a
    /// committed tree state (the JOINT pin: no committed pre-fix
    /// tree exists where the laundering runs end-to-end — the
    /// strawman-disclosure form).
    #[test]
    fn overrides_never_launder_owned_classifications() {
        use rio_nix::protocol::build::{BuildResult, BuildStatus};
        let gib = 1u64 << 30;
        let at_limit = Some(crate::quota::QuotaStatus {
            used_bytes: 25 * gib,
            hard_limit_bytes: Some(25 * gib),
        });
        let with_status = |s: BuildStatus| -> Result<BuildResult, ExecutorError> {
            Ok(BuildResult {
                status: s,
                ..Default::default()
            })
        };

        // The disk seam, timeout cell: a timed-out build under quota
        // pressure keeps its OWN law (r[builder.timeout.no-reassign]
        // owns the shape — deadline doubling, never a disk floor).
        let got = apply_disk_override(at_limit, Some(50 * gib), with_status(BuildStatus::TimedOut));
        assert!(
            matches!(got, Ok(ref b) if b.status == BuildStatus::TimedOut),
            "left (pre-fix, armed producer): Ok(TimedOut) under quota \
             pressure rewrites to Err(DiskFull) — the timeout law \
             bypassed one seam above its census / right: TimedOut \
             keeps its own law; got {got:?}"
        );

        // The disk seam, log-limit cell.
        let got = apply_disk_override(
            at_limit,
            Some(50 * gib),
            with_status(BuildStatus::LogLimitExceeded),
        );
        assert!(
            matches!(got, Ok(ref b) if b.status == BuildStatus::LogLimitExceeded),
            "the log-limit law owns its shape; got {got:?}"
        );

        // The disk seam, daemon-transient cell: the local retry law
        // (runtime/mod.rs is_daemon_transient consult) must see the
        // ORIGINAL error — a rewrite here would skip the local retry
        // and misclassify a daemon crash as a disk sizing signal.
        let transient = || {
            Err::<BuildResult, _>(ExecutorError::DaemonSpawn(std::io::Error::other(
                "spawn blip",
            )))
        };
        let got = apply_disk_override(at_limit, Some(50 * gib), transient());
        assert!(
            matches!(got, Err(ExecutorError::DaemonSpawn(_))),
            "the daemon-transient retry law owns its shape; got {got:?}"
        );

        // The oom TWIN seam (the apply_oom_override wildcard): a
        // cancelled build coinciding with an oom_kill keeps the
        // cancel law — pre-fix the `(true, Err(_))` arm rewrote it.
        let got = apply_oom_override(true, Err(ExecutorError::Cancelled));
        assert!(
            matches!(got, Err(ExecutorError::Cancelled)),
            "left (pre-fix): Err(Cancelled) + oom_kill rewrites to \
             CgroupOom through the (true, Err(_)) wildcard — the \
             cancel law laundered into a memory sizing signal / \
             right: the cancel law owns its shape; got {got:?}"
        );

        // The oom twin, network cell: a post-build store error with a
        // tolerated child OOM is a NETWORK failure, not a memory
        // sizing signal.
        let got = apply_oom_override(
            true,
            Err(ExecutorError::Grpc(tonic::Status::unavailable(
                "store blip",
            ))),
        );
        assert!(
            matches!(got, Err(ExecutorError::Grpc(_))),
            "the network lane owns its shape; got {got:?}"
        );

        // The oom twin, timeout cell (the book's cell): a timed-out
        // build coinciding with an oom_kill keeps TimedOut.
        let got = apply_oom_override(true, with_status(BuildStatus::TimedOut));
        assert!(
            matches!(got, Ok(ref b) if b.status == BuildStatus::TimedOut),
            "a timed-out build coinciding with an oom_kill keeps the \
             timeout law; got {got:?}"
        );

        // The oom seam's DESIGNED signals survive: the watcher-kill
        // EOF signature and the ordinary daemon failure under
        // oom_detected still classify CgroupOom.
        let eof = || {
            Err::<BuildResult, _>(ExecutorError::Wire(rio_nix::protocol::wire::WireError::Io(
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "daemon eof"),
            )))
        };
        let got = apply_oom_override(true, eof());
        assert!(
            matches!(got, Err(ExecutorError::CgroupOom)),
            "the cgroup.kill -> daemon EOF chain is the oom watcher's \
             own mechanical signal; got {got:?}"
        );
        let got = apply_oom_override(
            true,
            Err(ExecutorError::BuildFailed("builder exploded".into())),
        );
        assert!(
            matches!(got, Err(ExecutorError::CgroupOom)),
            "an ordinary daemon failure under oom_detected is the \
             designed I-196 attribution; got {got:?}"
        );
    }

    /// W11-V (R28): allow-list totality — every status and executor
    /// error shape has an EXPLICIT rewrite-authority row, across BOTH
    /// override seams. The compiler enforces the alphabet (the
    /// authority fn has no catch-all arm — R14); this test pins one
    /// representative cell per authority class so a future row
    /// flipping silently is a red, not a drift.
    #[test]
    fn sizing_rewrite_authority_rows_pinned() {
        use rio_nix::protocol::build::{BuildResult, BuildStatus};
        let ok = |s: BuildStatus| -> Result<BuildResult, ExecutorError> {
            Ok(BuildResult {
                status: s,
                ..Default::default()
            })
        };
        // Success class: never rewritable.
        for s in [
            BuildStatus::Built,
            BuildStatus::Substituted,
            BuildStatus::AlreadyValid,
            BuildStatus::ResolvesToAlreadyValid,
        ] {
            assert!(
                matches!(sizing_rewrite_authority(&ok(s)), SizingRewrite::Owned(_)),
                "success status {s:?} must be Owned"
            );
        }
        // Owned failure laws (sizing overrides may not claim).
        for s in [
            BuildStatus::TimedOut,
            BuildStatus::LogLimitExceeded,
            BuildStatus::CachedFailure,
            BuildStatus::DependencyFailed,
            BuildStatus::NotDeterministic,
            BuildStatus::InputRejected,
            BuildStatus::OutputRejected,
            BuildStatus::NoSubstituters,
        ] {
            assert!(
                matches!(sizing_rewrite_authority(&ok(s)), SizingRewrite::Owned(_)),
                "status {s:?} must be Owned by its own law"
            );
        }
        // The ENOSPC-consistent ordinary failures: rewritable.
        for s in [
            BuildStatus::PermanentFailure,
            BuildStatus::MiscFailure,
            BuildStatus::TransientFailure,
        ] {
            assert!(
                matches!(sizing_rewrite_authority(&ok(s)), SizingRewrite::Rewritable),
                "status {s:?} is the ordinary ENOSPC-consistent daemon failure"
            );
        }
        // Executor errors: the ENOSPC-consistent build-env writes.
        let synth: Result<BuildResult, ExecutorError> =
            Err(ExecutorError::SynthDb(sqlx::Error::WorkerCrashed));
        assert!(matches!(
            sizing_rewrite_authority(&synth),
            SizingRewrite::Rewritable
        ));
        let nixconf: Result<BuildResult, ExecutorError> =
            Err(ExecutorError::NixConf(std::io::Error::other("enospc")));
        assert!(matches!(
            sizing_rewrite_authority(&nixconf),
            SizingRewrite::Rewritable
        ));
        let buildfailed: Result<BuildResult, ExecutorError> =
            Err(ExecutorError::BuildFailed("x".into()));
        assert!(matches!(
            sizing_rewrite_authority(&buildfailed),
            SizingRewrite::Rewritable
        ));
        // The watcher-kill EOF signature: oom-only.
        let eof: Result<BuildResult, ExecutorError> =
            Err(ExecutorError::Wire(rio_nix::protocol::wire::WireError::Io(
                std::io::Error::new(std::io::ErrorKind::UnexpectedEof, "eof"),
            )));
        assert!(matches!(
            sizing_rewrite_authority(&eof),
            SizingRewrite::OomKillSignature
        ));
        // Owned executor laws.
        let owned: [(Result<BuildResult, ExecutorError>, &str); 6] = [
            (Err(ExecutorError::CgroupOom), "already the oom letter"),
            (Err(ExecutorError::DiskFull), "already the disk letter"),
            (Err(ExecutorError::Cancelled), "cancel law"),
            (
                Err(ExecutorError::DaemonSpawn(std::io::Error::other("x"))),
                "daemon-transient retry law",
            ),
            (
                Err(ExecutorError::Grpc(tonic::Status::unavailable("x"))),
                "network lane",
            ),
            (
                Err(ExecutorError::InvalidDerivation("x".into())),
                "permanent law",
            ),
        ];
        for (r, why) in &owned {
            assert!(
                matches!(sizing_rewrite_authority(r), SizingRewrite::Owned(_)),
                "{why}: {r:?} must be Owned"
            );
        }
    }

    /// W10-CN: contract pin — rio-scheduler matches
    /// `error_msg.contains(rio_proto::DISK_FULL_MSG)` to bump the
    /// disk floor; the Display side is pinned here (the
    /// `cgroup_oom_display_contains_proto_constant` mirror).
    #[test]
    fn disk_full_display_contains_proto_constant() {
        assert!(
            ExecutorError::DiskFull
                .to_string()
                .contains(rio_proto::DISK_FULL_MSG),
            "ExecutorError::DiskFull Display must carry rio_proto::DISK_FULL_MSG \
             (the scheduler's floor-bump match key): {}",
            ExecutorError::DiskFull
        );
    }

    #[test]
    fn cgroup_oom_display_contains_proto_constant() {
        assert!(
            ExecutorError::CgroupOom
                .to_string()
                .contains(rio_proto::CGROUP_OOM_MSG),
            "ExecutorError::CgroupOom Display ({:?}) must contain rio_proto::CGROUP_OOM_MSG ({:?})",
            ExecutorError::CgroupOom.to_string(),
            rio_proto::CGROUP_OOM_MSG,
        );
    }

    /// Pins the footer `result` string domain so `banner::footer_lines`'s
    /// rustdoc example and `footer_renders_failed`'s fixture can't drift
    /// from what production actually emits. The domain is `ok` /
    /// `failed (<reason>)` only — `cancelled` is decided by
    /// `runtime::result::final_footer_result` from the cancel flag, not
    /// here (the error variant can't tell a post-cgroup cancel from a
    /// daemon crash). `BuildStatus` carries no exit code, so the footer
    /// never produces `failed (exit N)` — the failure reason is the
    /// status discriminant or a hand-written phrase.
    ///
    /// One assertion per `match` arm in `footer_result_str` — keep this
    /// 1:1 so a reviewer can verify completeness by counting; add a new
    /// assertion when a new arm is added. The `Err(_)` arm has two
    /// fixtures (`BuildFailed` and `Wire(UnexpectedEof)`), both
    /// asserting the catch-all.
    #[test]
    fn footer_result_str_domain() {
        use rio_nix::protocol::build::{BuildResult, BuildStatus};

        let ok_result = |status| -> Result<BuildResult, ExecutorError> {
            Ok(BuildResult {
                status,
                ..Default::default()
            })
        };

        // Success → "ok".
        assert_eq!(footer_result_str(&ok_result(BuildStatus::Built)), "ok");

        // Special-cased human phrases.
        assert_eq!(
            footer_result_str(&ok_result(BuildStatus::TimedOut)),
            "failed (timed out)"
        );
        assert_eq!(
            footer_result_str(&ok_result(BuildStatus::LogLimitExceeded)),
            "failed (log limit exceeded)"
        );

        // Catch-all: Debug discriminant. This is what `footer_lines`'s
        // rustdoc example shows.
        assert_eq!(
            footer_result_str(&ok_result(BuildStatus::PermanentFailure)),
            "failed (PermanentFailure)"
        );

        // Post-cgroup cancel / daemon crash both surface as
        // Wire(Io(UnexpectedEof)) — the per-attempt mapper cannot tell
        // them apart and renders the catch-all. The runtime's
        // `final_footer_result` overrides this to "cancelled" at the
        // once-per-assignment send when the cancel flag is set.
        assert_eq!(
            footer_result_str(&Err(ExecutorError::Wire(
                rio_nix::protocol::wire::WireError::Io(std::io::Error::new(
                    std::io::ErrorKind::UnexpectedEof,
                    "early eof"
                ))
            ))),
            "failed (executor error)"
        );

        // Any other executor error → catch-all.
        assert_eq!(
            footer_result_str(&Err(ExecutorError::BuildFailed("boom".into()))),
            "failed (executor error)"
        );
    }

    // r[verify builder.exec.build-id-sanitized]
    #[test]
    fn test_sanitize_build_id() {
        assert_eq!(
            sanitize_build_id("/nix/store/abc123-hello.drv"),
            "abc123-hello_drv"
        );
        assert_eq!(sanitize_build_id("simple"), "simple");
        assert_eq!(sanitize_build_id("foo.bar.drv"), "foo_bar_drv");
        // I-167: fetchpatch URLs with query strings leak into drv names.
        assert_eq!(
            sanitize_build_id("/nix/store/abc-foo.patch?id=deadbeef.drv"),
            "abc-foo_patch_id_deadbeef_drv"
        );
        // Every URL-ish metacharacter collapses to `_`.
        assert_eq!(
            sanitize_build_id("a?b=c&d+e%f#g:h.drv"),
            "a_b_c_d_e_f_g_h_drv"
        );
        // nixbase32 + dash survive untouched.
        assert_eq!(
            sanitize_build_id("0123456789abcdfghijklmnpqrsvwxyz-name_drv"),
            "0123456789abcdfghijklmnpqrsvwxyz-name_drv"
        );
    }

    #[test]
    // r[verify builder.retry.daemon-transient]
    fn test_is_daemon_transient() {
        use rio_nix::protocol::wire::WireError;
        use std::io::{Error as IoError, ErrorKind};

        // Retryable: daemon spawn/handshake/early-EOF
        assert!(ExecutorError::DaemonSpawn(IoError::other("spawn failed")).is_daemon_transient());
        assert!(
            ExecutorError::Wire(WireError::Io(IoError::new(
                ErrorKind::UnexpectedEof,
                "early eof"
            )))
            .is_daemon_transient()
        );

        // NOT retryable: other wire I/O errors (broken pipe ≠ daemon crash)
        assert!(
            !ExecutorError::Wire(WireError::Io(IoError::new(ErrorKind::BrokenPipe, "pipe")))
                .is_daemon_transient()
        );
        // NOT retryable: builder failure, deterministic setup
        assert!(!ExecutorError::BuildFailed("exit 1".into()).is_daemon_transient());
        assert!(!ExecutorError::Cgroup("EACCES".into()).is_daemon_transient());
        assert!(!ExecutorError::NixConf(IoError::other("disk full")).is_daemon_transient());
        // NOT retryable: cgroup OOM. Retrying on the same undersized
        // pod just OOM-loops again — must escalate to scheduler for
        // resource_floor bump (I-196).
        assert!(!ExecutorError::CgroupOom.is_daemon_transient());
    }

    #[test]
    fn test_is_permanent() {
        use std::io::Error as IoError;
        // Permanent: derivation-intrinsic, same on every pod.
        assert!(
            ExecutorError::WrongKind {
                is_fod: true,
                executor_kind: rio_proto::types::ExecutorKind::Builder
            }
            .is_permanent()
        );
        assert!(ExecutorError::InvalidDerivation("not UTF-8".into()).is_permanent());

        // NOT permanent: node-/network-local — another pod might succeed.
        assert!(!ExecutorError::DaemonSpawn(IoError::other("spawn")).is_permanent());
        assert!(!ExecutorError::CgroupOom.is_permanent());
        assert!(!ExecutorError::BuildFailed("exit 1".into()).is_permanent());
        assert!(!ExecutorError::Cgroup("EACCES".into()).is_permanent());
        assert!(!ExecutorError::NixConf(IoError::other("disk full")).is_permanent());
        // is_permanent and is_daemon_transient are disjoint.
        assert!(!ExecutorError::InvalidDerivation("x".into()).is_daemon_transient());
    }

    // r[verify builder.oom.cgroup-watch+3]
    /// `apply_oom_override` MUST preserve `Ok(Built)` even when
    /// `oom_detected` is true (the `final_oom` path does not
    /// `cgroup.kill`, so a build that tolerated a child OOM reports
    /// `Built`). Regression: previously the override was unconditional
    /// and discarded the completed outputs → re-dispatch loop.
    #[test]
    fn test_apply_oom_override_preserves_ok_built() {
        use rio_nix::protocol::build::BuildResult;
        use rio_nix::protocol::wire::WireError;
        use std::io::{Error as IoError, ErrorKind};

        // (true, Ok(Built)) → Ok(Built). The bug case.
        let r = apply_oom_override(true, Ok(BuildResult::success()));
        assert!(
            matches!(&r, Ok(br) if br.status == rio_nix::protocol::build::BuildStatus::Built),
            "Ok(Built) must be preserved when oom_detected, got: {r:?}"
        );

        // (true, Err(Wire(UnexpectedEof))) → Err(CgroupOom). Watcher path.
        let r = apply_oom_override(
            true,
            Err(ExecutorError::Wire(WireError::Io(IoError::new(
                ErrorKind::UnexpectedEof,
                "eof",
            )))),
        );
        assert!(matches!(r, Err(ExecutorError::CgroupOom)), "got: {r:?}");

        // (true, Err(BuildFailed)) → Err(CgroupOom). final_oom reclassify.
        let r = apply_oom_override(true, Err(ExecutorError::BuildFailed("exit 137".into())));
        assert!(matches!(r, Err(ExecutorError::CgroupOom)), "got: {r:?}");

        // (false, Ok) → Ok unchanged.
        let r = apply_oom_override(false, Ok(BuildResult::success()));
        assert!(r.is_ok());

        // (false, Err) → Err unchanged (NOT reclassified).
        let r = apply_oom_override(false, Err(ExecutorError::BuildFailed("exit 1".into())));
        assert!(
            matches!(r, Err(ExecutorError::BuildFailed(_))),
            "got: {r:?}"
        );
    }

    fn test_env() -> ExecutorEnv {
        ExecutorEnv {
            fuse_mount_point: "/tmp".into(),
            overlay_base_dir: "/tmp".into(),
            fuse_cache_dir: "/tmp".into(),
            executor_id: "t".into(),
            log_limits: crate::log_stream::LogLimits::UNLIMITED,
            daemon_timeout: rio_common::config::BoundedSecs::from_duration(DEFAULT_DAEMON_TIMEOUT),
            max_silent_time: 0,
            cgroup_parent: "/tmp".into(),
            executor_kind: rio_proto::types::ExecutorKind::Builder,
            systems: Arc::from(["x86_64-linux".to_string()]),
            hw_class: None,
            fuse_cache: None,
            fuse_fetch_timeout: Duration::from_secs(60),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
    }

    // r[verify sched.timeout.per-build+2]
    /// merged_bug_034 red: wire-supplied `u64::MAX` timeout seconds
    /// must convert BOUNDED and survive the stderr-loop deadline
    /// arithmetic. Proposition certified (R16): the builder's
    /// proto→domain conversion is total over the wire domain —
    /// `Instant + timeout` (the stderr_loop deadline shape, verbatim)
    /// cannot overflow-panic on tenant input. The assignment is the
    /// production prost value (R13). Pre-fix: `resolve_build_opts`
    /// converted verbatim (`Duration::from_secs(u64::MAX)`) and the
    /// add PANICS with "overflow when adding duration to instant" —
    /// caught by the build-panic-catcher and MISCLASSIFIED as
    /// InfrastructureFailure, per-attempt task death + retry churn.
    #[test]
    fn max_wire_timeout_converts_bounded_and_survives_deadline_math() {
        let env = test_env();
        let a = WorkAssignment {
            build_options: Some(rio_proto::types::BuildOptions {
                build_timeout: u64::MAX,
                max_silent_time: u64::MAX,
                ..Default::default()
            }),
            ..Default::default()
        };
        let opts = resolve_build_opts(&a, &env, 8, 0);

        // The stderr-loop deadline math, same shapes as
        // daemon/stderr_loop.rs (`Instant::now() + build_timeout`;
        // `last_output + silence`): in-range for any clamped value.
        let build_deadline = tokio::time::Instant::now() + opts.timeout;
        assert!(build_deadline > tokio::time::Instant::now() - Duration::from_secs(1));

        let ceiling = rio_common::clamped::ClampedSecs::MAX_SECS as u64;
        assert!(
            opts.timeout <= Duration::from_secs(ceiling),
            "wire build_timeout must saturate at the shared absurdity \
             ceiling; got {:?}",
            opts.timeout
        );
        let silence_secs = u64::from(opts.max_silent_time);
        assert!(
            silence_secs <= ceiling,
            "wire max_silent_time must saturate at the shared absurdity \
             ceiling; got {silence_secs}"
        );
        let silence_deadline = tokio::time::Instant::now() + Duration::from_secs(silence_secs);
        assert!(silence_deadline > tokio::time::Instant::now() - Duration::from_secs(1));
    }

    /// W10-BG (bug_117): the CONFIG lane joins the wire lane's bound.
    /// The regression matrix covers BOTH lanes × {sane, sentinel} —
    /// the wave-6 close bounded only the wire lane, and ssh-ng
    /// assignments carry no `build_options`, so the config fallback is
    /// the COMMON lane: a `u64::MAX`-class "disable the timeout"
    /// config value reached the stderr-loop `Instant + Duration`
    /// deadline add raw and panicked fleet-wide, misclassified
    /// InfrastructureFailure.
    #[test]
    fn daemon_timeout_config_lane_bounded_like_wire_lane() {
        let ceiling = rio_common::clamped::WireSecs::MAX_SECS;

        // Config-lane sentinel: u64::MAX "disable the timeout".
        // Pre-fix red (env.daemon_timeout was a raw `Duration`;
        // `env.daemon_timeout = Duration::from_secs(u64::MAX)`):
        //   config daemon_timeout must arrive ceiling-bounded at the
        //   deadline math (the wire lane already is); got
        //   18446744073709551615s
        // Post-fix that assignment NO LONGER COMPILES — `BoundedSecs`'
        // inner Duration is private and both constructors saturate, so
        // the sentinel is expressed through the type and arrives
        // clamped.
        let mut env = test_env();
        env.daemon_timeout = rio_common::config::BoundedSecs::from_raw_secs(u64::MAX);
        // ssh-ng shape: no build_options → config fallback is the lane.
        let opts = resolve_build_opts(&WorkAssignment::default(), &env, 8, 0);
        assert!(
            opts.timeout <= Duration::from_secs(ceiling),
            "config daemon_timeout must arrive ceiling-bounded at the \
             deadline math (the wire lane already is); got {:?}",
            opts.timeout
        );
        // The stderr-loop deadline shape survives (the pre-fix panic site).
        let build_deadline = tokio::time::Instant::now() + opts.timeout;
        assert!(build_deadline > tokio::time::Instant::now() - Duration::from_secs(1));

        // Config-lane sane: the 2h default passes through exactly.
        let opts = resolve_build_opts(&WorkAssignment::default(), &test_env(), 8, 0);
        assert_eq!(opts.timeout, DEFAULT_DAEMON_TIMEOUT);

        // Wire-lane sane: a set wire value wins over config, exact.
        let a = WorkAssignment {
            build_options: Some(rio_proto::types::BuildOptions {
                build_timeout: 300,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(
            resolve_build_opts(&a, &test_env(), 8, 0).timeout,
            Duration::from_secs(300)
        );
        // Wire-lane sentinel: covered in depth by
        // `max_wire_timeout_converts_bounded_and_survives_deadline_math`.
    }

    // r[verify sched.sla.cores-reach-nix-build-cores]
    /// ADR-023: `WorkAssignment.assigned_cores` reaches
    /// `wopSetOptions.buildCores` verbatim (clamped to cgroup ceiling).
    /// Precedence: assigned > client-requested > cgroup ceil(cpu.max).
    #[test]
    fn resolve_build_opts_assigned_cores_wins() {
        let env = test_env();
        // Scheduler assigned 4 cores; cgroup ceiling 8 → 4 reaches the daemon.
        let a = WorkAssignment {
            assigned_cores: Some(4),
            build_options: Some(rio_proto::types::BuildOptions {
                build_cores: 64, // client over-asks; ignored when assigned set
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8, 0).build_cores, 4);

        // assigned > cgroup ceiling → clamped (defense vs sched/kubelet drift).
        let a = WorkAssignment {
            assigned_cores: Some(16),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8, 0).build_cores, 8);

        // No assigned_cores → pre-ADR-023 fallback: client capped at cgroup.
        let a = WorkAssignment {
            assigned_cores: None,
            build_options: Some(rio_proto::types::BuildOptions {
                build_cores: 64,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8, 0).build_cores, 8);

        // assigned_cores=0 treated as unset (proto3 optional Some(0) is
        // possible if scheduler explicitly sends 0; never pass 0 to nix).
        let a = WorkAssignment {
            assigned_cores: Some(0),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8, 0).build_cores, 8);
    }

    /// `resolve_inputs` fetches each inputDrv from the store, resolves
    /// the requested output names to concrete store paths, and merges
    /// them into the BasicDerivation's `input_srcs`. Without this,
    /// nix-daemon's sandbox would only bind-mount the static
    /// `input_srcs` (e.g., busybox) — the dependency's outputs would be
    /// invisible to the builder.
    ///
    // r[verify builder.executor.resolve-input-drvs]
    #[tokio::test]
    async fn test_resolve_inputs_merges_input_drv_outputs() -> anyhow::Result<()> {
        use rio_test_support::fixtures::{make_nar, make_path_info, test_store_path};
        use rio_test_support::grpc::spawn_mock_store_with_client;

        let (store, client, _h) = spawn_mock_store_with_client().await?;

        // The dependency's .drv: declares one output "out" at a
        // CONCRETE path. This is what resolve_inputs must extract.
        let dep_out = test_store_path("dep-out");
        let dep_drv_path = test_store_path("dep.drv");
        let dep_aterm = format!(
            r#"Derive([("out","{dep_out}","","")],[],[],"x86_64-linux","/bin/sh",[],[("out","{dep_out}")])"#
        );
        let (dep_nar, dep_hash) = make_nar(dep_aterm.as_bytes());
        store.seed(make_path_info(&dep_drv_path, &dep_nar, dep_hash), dep_nar);

        // Seed the dep's output and the main .drv path so
        // compute_input_closure's BFS doesn't error (NotFound is
        // skipped, but seeding keeps the test deterministic).
        let (out_nar, out_hash) = make_nar(b"dep output content");
        store.seed(make_path_info(&dep_out, &out_nar, out_hash), out_nar);

        // The main derivation: one static input_src (busybox-style),
        // one inputDrv referencing dep.drv's "out". resolve_inputs
        // should fetch dep.drv, read its "out" → dep_out, and add
        // dep_out to the BasicDerivation's input_srcs.
        let static_src = test_store_path("busybox");
        let (src_nar, src_hash) = make_nar(b"busybox binary");
        store.seed(make_path_info(&static_src, &src_nar, src_hash), src_nar);

        let main_out = test_store_path("main-out");
        let main_drv_path = test_store_path("main.drv");
        let main_aterm = format!(
            r#"Derive([("out","{main_out}","","")],[("{dep_drv_path}",["out"])],["{static_src}"],"x86_64-linux","/bin/sh",[],[("out","{main_out}")])"#
        );
        let drv = Derivation::parse(&main_aterm)
            .unwrap_or_else(|e| panic!("test ATerm invalid: {e}\n{main_aterm}"));
        // Seed the main .drv path too (compute_input_closure seeds
        // frontier with drv_path).
        let (main_nar, main_hash) = make_nar(main_aterm.as_bytes());
        store.seed(
            make_path_info(&main_drv_path, &main_nar, main_hash),
            main_nar,
        );

        // Precondition: the .drv's static input_srcs does NOT include
        // the dep output. If it did, the test would pass vacuously.
        assert!(
            !drv.input_srcs().contains(&dep_out),
            "precondition: dep_out must NOT be in static input_srcs"
        );
        assert!(
            drv.input_drvs().contains_key(&dep_drv_path),
            "precondition: inputDrvs must reference dep.drv"
        );

        // === Resolve ===
        let resolved = resolve_inputs(&client, &drv, &main_drv_path).await?;

        // The dep's concrete output path is now in input_srcs.
        assert!(
            resolved.basic_drv.input_srcs().contains(&dep_out),
            "resolved BasicDerivation.input_srcs must contain the \
             inputDrv's concrete output path {dep_out}; got: {:?}",
            resolved.basic_drv.input_srcs()
        );
        // The static src is preserved (merge, not replace).
        assert!(
            resolved.basic_drv.input_srcs().contains(&static_src),
            "static input_srcs must be preserved"
        );
        // And the closure includes the dep output (synth DB seed set).
        assert!(
            resolved.input_paths.contains(&dep_out),
            "input_paths closure must include resolved inputDrv output"
        );

        Ok(())
    }

    /// TimedOut must NOT map to anything the scheduler reassigns. This
    /// is the load-bearing invariant for the reassignment-storm fix.
    ///
    // r[verify builder.timeout.no-reassign]
    #[test]
    fn test_timed_out_is_not_reassignable() {
        use rio_nix::protocol::build::BuildStatus as Nix;
        use rio_proto::types::BuildResultStatus as Proto;

        let mapped = Proto::from(Nix::TimedOut);
        // completion.rs:151-152: these two trigger handle_transient_failure
        // (reassign). TimedOut must not be either.
        assert_ne!(mapped, Proto::TransientFailure, "TimedOut → reassign storm");
        assert_ne!(
            mapped,
            Proto::InfrastructureFailure,
            "TimedOut → reassign storm"
        );
        // And it must not be Unspecified (which ALSO reassigns per
        // completion.rs:176-183).
        assert_ne!(mapped, Proto::Unspecified);
    }
}
