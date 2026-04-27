//! Build executor: receives WorkAssignment from scheduler, runs builds.
//!
//! Flow:
//! 1. Set up overlay for the build
//! 2. Generate synthetic SQLite DB with input closure metadata
//! 3. Spawn `nix-daemon --stdio` in overlay
//! 4. Client handshake + wopSetOptions + wopBuildDerivation
//! 5. Stream logs via LogBatcher -> BuildLogBatch -> scheduler stream
//! 6. On completion: upload outputs to store via PutPath
//! 7. Send CompletionReport on scheduler stream
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
use rio_proto::types::{BuildResult as ProtoBuildResult, ExecutorMessage, WorkAssignment};
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
    pub fuse_mount_point: std::path::PathBuf,
    pub overlay_base_dir: std::path::PathBuf,
    pub executor_id: String,
    pub log_limits: LogLimits,
    /// Timeout for the local nix-daemon subprocess build. Used when the
    /// client didn't specify `BuildOptions.build_timeout`. Intentionally
    /// long (default 2h) — some builds genuinely take that long; the
    /// purpose is to bound blast radius of a truly stuck daemon.
    pub daemon_timeout: Duration,
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
    /// stays consistent with what the heartbeat advertises.
    pub systems: Arc<[String]>,
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
pub const DEFAULT_DAEMON_TIMEOUT: Duration = Duration::from_secs(7200);

/// Error type for executor operations.
///
/// No `#[from] anyhow::Error` — every variant has a typed source. A
/// catch-all `#[from] anyhow::Error` would mean any `?` on an
/// anyhow::Result anywhere in execute_build silently becomes the wrong
/// variant. Typed sources make the compiler catch misattribution at
/// the `?` site.
#[derive(Debug, thiserror::Error)]
pub enum ExecutorError {
    #[error("overlay setup failed: {0}")]
    Overlay(#[from] overlay::OverlayError),
    #[error("overlay setup task panicked: {0}")]
    OverlayTaskPanic(tokio::task::JoinError),
    #[error("synthetic DB generation failed: {0}")]
    SynthDb(#[from] sqlx::Error),
    #[error("failed to write nix.conf: {0}")]
    NixConf(#[source] std::io::Error),
    #[error("daemon spawn failed: {0}")]
    DaemonSpawn(std::io::Error),
    #[error("daemon handshake failed: {0}")]
    Handshake(#[from] rio_nix::protocol::handshake::HandshakeError),
    #[error("daemon setup failed: {0}")]
    DaemonSetup(String),
    #[error("build failed: {0}")]
    BuildFailed(String),
    /// The assignment's `.drv` content is malformed (UTF-8, ATerm parse,
    /// BasicDerivation conversion). Deterministic per-derivation: every
    /// pod sees the same bytes, so retry-on-another-pod is pointless.
    /// Maps to `InputRejected` instead of `InfrastructureFailure`.
    #[error("invalid derivation: {0}")]
    InvalidDerivation(String),
    #[error("upload failed: {0}")]
    Upload(#[from] upload::UploadError),
    #[error("gRPC error: {0}")]
    Grpc(#[from] tonic::Status),
    #[error("input metadata fetch failed for {path}: {source}")]
    MetadataFetch { path: String, source: tonic::Status },
    #[error("wire protocol error: {0}")]
    Wire(#[from] rio_nix::protocol::wire::WireError),
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
    #[error(
        "wrong executor kind: derivation is_fod={is_fod} but this executor is {executor_kind:?}"
    )]
    WrongKind {
        is_fod: bool,
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
    pub result: Result<ExecutionResult, ExecutorError>,
    /// 0 only for pre-cgroup setup errors (drv parse, WrongKind,
    /// overlay, daemon-spawn). Populated for `CgroupOom` /
    /// post-handshake `Wire` / `Upload` / `BuildFailed`.
    pub peak_memory_bytes: u64,
    pub peak_cpu_cores: f64,
    /// `None` = no prjquota OR pre-cgroup error. Sampled BEFORE
    /// `build_result?` so an OOM'd build also reports it.
    pub peak_disk_bytes: Option<u64>,
}

impl ExecuteOutcome {
    /// Pre-cgroup setup error: cgroup never populated, peaks genuinely 0.
    fn pre_cgroup(e: ExecutorError) -> Self {
        Self {
            result: Err(e),
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            peak_disk_bytes: None,
        }
    }

    /// Fixture short-circuit: scripted peaks live on the
    /// `ExecutionResult`; copy them out so the `Err`-path consumers
    /// (which only exist on real builds) see consistent shape.
    #[cfg(feature = "test-fixtures")]
    fn fixture(r: ExecutionResult) -> Self {
        Self {
            peak_memory_bytes: r.peak_memory_bytes,
            peak_cpu_cores: r.peak_cpu_cores,
            peak_disk_bytes: r.peak_disk_bytes,
            result: Ok(r),
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
/// Per observability.md:152 (trace structure), child spans are:
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
    log_tx: &mpsc::Sender<ExecutorMessage>,
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
            Err(e) => return ExecuteOutcome::pre_cgroup(e),
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
            Err(e) => return ExecuteOutcome::pre_cgroup(e),
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
        return ExecuteOutcome::pre_cgroup(ExecutorError::WrongKind {
            is_fod,
            executor_kind: env.executor_kind,
        });
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
            // worker thread and block the heartbeat loop.
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
            // r[impl builder.cancel.pre-cgroup-deferred]
            // I-166: the cgroup doesn't exist yet (created post-spawn below),
            // so a Cancel that arrived during overlay/resolve/prepare landed
            // as ENOENT in `try_cancel_build` — which now LEAVES the flag
            // set. Check it here. The pre-cgroup window is now overlay →
            // resolve → prepare_sandbox → register + prefetch (sub-second);
            // the cancel_poll select that covered the warm hang is no longer
            // needed.
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
            let opts = resolve_build_opts(assignment, env, effective_cores);

            let outcome = run_daemon_lifecycle(
                &overlay_mount,
                env,
                &build_id,
                drv_path,
                &basic_drv,
                opts,
                log_tx,
            )
            .await?;

            Ok(PreDaemon::Ran {
                overlay_mount,
                input_paths,
                outcome,
            })
        }
        .await;

    let (overlay_mount, input_paths, build_result, peak_memory_bytes, peak_cpu_cores) = match pre {
        Err(e) => return ExecuteOutcome::pre_cgroup(e),
        #[cfg(feature = "test-fixtures")]
        Ok(PreDaemon::Fixture(r)) => return ExecuteOutcome::fixture(r),
        Ok(PreDaemon::Ran {
            overlay_mount,
            input_paths,
            outcome:
                DaemonOutcome {
                    build_result,
                    peak_memory_bytes,
                    peak_cpu_cores,
                },
        }) => (
            overlay_mount,
            input_paths,
            build_result,
            peak_memory_bytes,
            peak_cpu_cores,
        ),
    };

    // ── Post-daemon: peaks now in scope; carry across every Err. ──────
    // r[impl builder.cgroup.memory-peak+2]
    // Sample prjquota BEFORE the build_result/collect_outputs
    // early-returns — `dqb_curspace` is current bytes (overlay still
    // mounted on this path) so an OOM'd build also reports
    // peak_disk_bytes. Previously sampled only at line 11 (teardown),
    // which the `?` at build_result skipped.
    let peak_disk_bytes = crate::quota::current_bytes(&env.overlay_base_dir)
        .ok()
        .flatten();
    let post_err = |e| ExecuteOutcome {
        result: Err(e),
        peak_memory_bytes,
        peak_cpu_cores,
        peak_disk_bytes,
    };

    // 10. Collect outputs (borrows &overlay_mount; must precede teardown).
    // The daemon error is NOT propagated yet — teardown below must run
    // on the spawn_blocking pool for both Ok AND Err. A `return
    // post_err(e)` here would drop `overlay_mount` synchronously on this
    // tokio worker (multi-GB `remove_dir_all`), and `Wire(UnexpectedEof)`
    // is `is_daemon_transient()` → the retry loop at runtime/mod.rs calls
    // back in instead of exiting, so the worker would block across the
    // retry and starve heartbeats.
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
    // heartbeat-starvation concern as setup_overlay above.
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
    let (_build_result, BuildOutputs { proto_result }) = match collect_result {
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
            fixture_resources: None,
        }),
        peak_memory_bytes,
        peak_cpu_cores,
        peak_disk_bytes,
    }
}

/// Effective per-build option triple after applying assignment →
/// worker-config → cgroup-clamp precedence.
struct BuildOpts {
    timeout: Duration,
    max_silent_time: u64,
    build_cores: u64,
}

/// Compute the effective build options for this assignment.
///
/// The scheduler computes `BuildOptions` per-derivation from the
/// intersecting builds' options (`actor/build.rs` `min_nonzero` for
/// timeouts, max for cores). `None` → daemon defaults: unbounded
/// silence, nproc cores. 0 → 0 on the wire = unbounded/all-cores to
/// the daemon — the scheduler's `min_nonzero` already handles the
/// 0-means-unset semantics; we pass through verbatim.
fn resolve_build_opts(
    assignment: &WorkAssignment,
    env: &ExecutorEnv,
    effective_cores: u32,
) -> BuildOpts {
    let opts = assignment.build_options.as_ref();
    let timeout = opts
        .and_then(|o| (o.build_timeout > 0).then(|| Duration::from_secs(o.build_timeout)))
        .unwrap_or(env.daemon_timeout);
    // Assignment's max_silent_time wins if nonzero; else the worker
    // config default. Same 0-means-unset semantics as build_timeout above.
    // Config default exists because Nix ssh-ng clients don't send
    // wopSetOptions to the gateway — the BuildOptions path is dead until
    // gateway-side propagation lands.
    let max_silent_time = opts
        .map(|o| o.max_silent_time)
        .filter(|&v| v > 0)
        .unwrap_or(env.max_silent_time);
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
    }
}

/// Result of [`run_daemon_lifecycle`]: the inner build result (NOT yet
/// `?`-propagated — cgroup teardown must run regardless) plus the
/// resource samples read from the per-build cgroup before it was dropped.
struct DaemonOutcome {
    build_result: Result<rio_nix::protocol::build::BuildResult, ExecutorError>,
    peak_memory_bytes: u64,
    peak_cpu_cores: f64,
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
async fn run_daemon_lifecycle(
    overlay_mount: &overlay::OverlayMount,
    env: &ExecutorEnv,
    build_id: &str,
    drv_path: &str,
    basic_drv: &rio_nix::derivation::BasicDerivation,
    opts: BuildOpts,
    log_tx: &mpsc::Sender<ExecutorMessage>,
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

    let monitors = spawn_cgroup_monitors(&build_cgroup, &env.cgroup_parent);

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // The cgroup setup above (create/add_process)
    // is NOT inside this helper — its `?` paths rely on the
    // kill_on_drop set in spawn_daemon_in_namespace as a safety
    // net. The explicit kill below remains the primary cleanup
    // (graceful, bounded wait for reap); kill_on_drop covers early
    // returns between spawn and here.
    let batcher = LogBatcher::new(drv_path.to_owned(), env.executor_id.clone(), env.log_limits);
    let build_result = run_daemon_build(
        &mut daemon,
        drv_path,
        basic_drv,
        DaemonBuildOpts {
            build_timeout: opts.timeout,
            max_silent_time: opts.max_silent_time,
            build_cores: opts.build_cores,
        },
        batcher,
        log_tx,
    )
    .await;

    // Stop both monitors and read their results. The last CPU sample
    // is up to 1s stale; good enough (peak CPU doesn't change in the
    // last second of a multi-minute build). The scopeguards inside
    // `monitors` also abort on drop; this explicit stop is the happy-
    // path fast stop (guard fires redundantly after, which is a no-op
    // on an already-aborted handle).
    let (peak_cpu_cores, oom_detected) = monitors.stop();
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
    match (oom_detected, &build_result) {
        (true, Err(_)) => {
            metrics::counter!("rio_builder_cgroup_oom_total").increment(1);
            Err(ExecutorError::CgroupOom)
        }
        (true, Ok(_)) => {
            metrics::counter!("rio_builder_cgroup_oom_total").increment(1);
            tracing::warn!(
                "oom_kill incremented but build succeeded; keeping Ok(Built) \
                 (script tolerated the OOM-killed child)"
            );
            build_result
        }
        (false, _) => build_result,
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
            executor_id: "t".into(),
            log_limits: crate::log_stream::LogLimits::UNLIMITED,
            daemon_timeout: DEFAULT_DAEMON_TIMEOUT,
            max_silent_time: 0,
            cgroup_parent: "/tmp".into(),
            executor_kind: rio_proto::types::ExecutorKind::Builder,
            systems: Arc::from(["x86_64-linux".to_string()]),
            fuse_cache: None,
            fuse_fetch_timeout: Duration::from_secs(60),
            cancelled: Arc::new(AtomicBool::new(false)),
        }
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
        assert_eq!(resolve_build_opts(&a, &env, 8).build_cores, 4);

        // assigned > cgroup ceiling → clamped (defense vs sched/kubelet drift).
        let a = WorkAssignment {
            assigned_cores: Some(16),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8).build_cores, 8);

        // No assigned_cores → pre-ADR-023 fallback: client capped at cgroup.
        let a = WorkAssignment {
            assigned_cores: None,
            build_options: Some(rio_proto::types::BuildOptions {
                build_cores: 64,
                ..Default::default()
            }),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8).build_cores, 8);

        // assigned_cores=0 treated as unset (proto3 optional Some(0) is
        // possible if scheduler explicitly sends 0; never pass 0 to nix).
        let a = WorkAssignment {
            assigned_cores: Some(0),
            ..Default::default()
        };
        assert_eq!(resolve_build_opts(&a, &env, 8).build_cores, 8);
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
