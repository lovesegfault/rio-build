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

use std::collections::HashMap;
use std::path::Path;
use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicUsize, Ordering};
use std::time::Duration;

use tokio::sync::mpsc;
use tonic::transport::Channel;
use tracing::instrument;

use futures_util::stream::{self, StreamExt, TryStreamExt};
use rio_nix::derivation::{Derivation, DerivationLike};
use rio_proto::StoreServiceClient;
use rio_proto::types::{
    BuildResult as ProtoBuildResult, BuildResultStatus, BuiltOutput, ExecutorMessage,
    WorkAssignment,
};

use crate::log_stream::{LogBatcher, LogLimits};
use crate::overlay;
use crate::synth_db::{self, SynthDrvOutput, SynthPathInfo};
use crate::upload;

mod daemon;
mod inputs;

use daemon::{run_daemon_build, spawn_daemon_in_namespace};
use inputs::{compute_input_closure, fetch_drv_from_store, prefetch_manifests, verify_fod_hashes};

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
/// state (stream_tx, running_builds) that needs per-call cloning; this
/// holds `Copy`/cheap-to-copy config that can just be passed by value.
#[derive(Clone)]
pub struct ExecutorEnv {
    pub fuse_mount_point: std::path::PathBuf,
    pub overlay_base_dir: std::path::PathBuf,
    pub executor_id: String,
    pub log_limits: LogLimits,
    /// Threshold for leaked overlay mounts before refusing new builds.
    /// A leaked mount means `umount2` failed in `OverlayMount::Drop` —
    /// typically the mount is stuck busy (open file handles, zombie
    /// nix-daemon). After N leaks the worker is degraded; refusing builds
    /// and reporting `InfrastructureFailure` lets the scheduler reassign
    /// to a healthy worker, and the supervisor can restart this one.
    pub max_leaked_mounts: usize,
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
    /// Handle to the FUSE local cache. The executor calls
    /// `register_inputs` (JIT allowlist, I-043 redesign) and
    /// `prefetch_manifests` (I-110c PG-skip hints) on it after
    /// `compute_input_closure` and before daemon spawn. `None` in
    /// tests that don't mount FUSE — both calls are skipped, FUSE
    /// `lookup` falls back to legacy `JitClass::NotArmed`.
    pub fuse_cache: Option<Arc<crate::fuse::cache::Cache>>,
    /// Base per-fetch gRPC timeout for the FUSE cache's `GetPath`
    /// (`worker.toml fuse_fetch_timeout_secs`, default 60s). JIT
    /// `lookup` uses `jit_fetch_timeout(this, nar_size)` per path so
    /// large inputs get a size-proportional budget (I-178). Same
    /// value passed to the `PrefetchHint` handler.
    pub fuse_fetch_timeout: Duration,
    /// Cancel flag for THIS build. Set by [`crate::runtime::try_cancel_build`]
    /// before it writes `cgroup.kill`. I-166: threaded into the executor
    /// so the pre-cgroup phase (overlay → resolve → prefetch → warm) can
    /// poll it and abort with [`ExecutorError::Cancelled`] instead of
    /// burning compute until `activeDeadlineSeconds`. Same `Arc` as the
    /// one in `runtime::CancelRegistry` and the one `spawn_build_task`
    /// reads to classify the final `Err`.
    pub cancelled: Arc<AtomicBool>,
}

/// Default daemon build timeout: 2 hours. See `ExecutorEnv.daemon_timeout`.
pub const DEFAULT_DAEMON_TIMEOUT: Duration = Duration::from_secs(7200);

/// Worker nix.conf content for sandbox builds.
///
/// The ConfigMap `rio-nix-conf` in the Helm chart's configmaps.yaml can
/// override this at `/etc/rio/nix-conf/nix.conf` — operators customize
/// without image rebuild. `setup_nix_conf` checks for the override
/// first; this is the fallback when the mount is absent (VM tests,
/// local dev).
///
/// `ca-derivations`: required for content-addressed outputs (Phase 2c
/// CA support). The ConfigMap also lists `nix-command` for pod
/// diagnostics (`nix store info` etc), but it's NOT needed for builds
/// — the daemon receives pre-evaluated .drv files via worker-protocol
/// opcodes, no `nix` CLI involvement. Dropped here to reduce attack
/// surface in the sandbox-spawned daemon.
///
/// This constant must stay in sync with infra/helm/rio-build/templates/
/// configmaps.yaml —
/// a mismatch means K8s deployments get different behavior than VM
/// tests (which use native NixOS modules, not this path).
const WORKER_NIX_CONF: &str = "\
builders =
substitute = false
sandbox = true
sandbox-fallback = false
restrict-eval = true
experimental-features = ca-derivations
";

/// Path where operators can mount a nix.conf override (via the
/// `rio-nix-conf` ConfigMap). If present, `setup_nix_conf` copies
/// THIS instead of using `WORKER_NIX_CONF`. Lets operators customize
/// experimental-features, sandbox paths, etc without image rebuild.
const NIX_CONF_OVERRIDE_PATH: &str = "/etc/rio/nix-conf/nix.conf";

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
    /// `InfrastructureFailure` so the scheduler reassigns to a larger
    /// size class instead of marking the drv permanently failed.
    #[error("cgroup OOM during build; promoting size class")]
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
}

/// Max local retry attempts for transient daemon failures before
/// reporting InfrastructureFailure to the scheduler. Bounded so a
/// persistent crash (bad synth DB, broken nix binary) doesn't spin
/// indefinitely.
pub const DAEMON_RETRY_MAX: u32 = 3;

/// Base delay for exponential backoff between daemon retry attempts.
/// Sequence: 500ms, 1s, 2s. Total worst-case retry overhead ~3.5s
/// — small vs the scheduler round-trip (re-dispatch + re-fetch
/// closure + re-generate synth DB).
pub const DAEMON_RETRY_BASE_DELAY: Duration = Duration::from_millis(500);

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
    /// Total bytes uploaded (sum of NAR sizes). 0 on failure.
    pub output_size_bytes: u64,
    /// Peak CPU cores-equivalent, polled 1Hz from cgroup `cpu.stat`
    /// usage_usec. `delta_usec / elapsed_usec`, max over build lifetime.
    /// Tree-wide. 0.0 = build failed before any sample (exited <1s).
    pub peak_cpu_cores: f64,
}

/// Read `cpu.stat` `usage_usec` from a cgroup path. Free fn (not a
/// method on BuildCgroup) so the CPU poll task can clone the PATH
/// and call this without holding a `&BuildCgroup` across the
/// `run_daemon_build` await.
///
/// Thin wrapper over the pure parser in cgroup.rs. `None` on read
/// fail (cgroup directory removed mid-poll — shouldn't happen, the
/// executor drops BuildCgroup AFTER the poll task is aborted).
fn read_cpu_stat(cgroup_path: &Path) -> Option<u64> {
    let content = std::fs::read_to_string(cgroup_path.join("cpu.stat")).ok()?;
    crate::cgroup::parse_cpu_stat_usage_usec(&content)
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
    leak_counter: &Arc<AtomicUsize>,
) -> Result<ExecutionResult, ExecutorError> {
    // Destructure for ergonomics — the body was written with these as
    // params, and rewriting every `fuse_mount_point` to `env.fuse_mount_point`
    // would be noise. The compiler inlines this away.
    let fuse_mount_point: &Path = &env.fuse_mount_point;
    let overlay_base_dir: &Path = &env.overlay_base_dir;
    let executor_id: &str = &env.executor_id;
    let log_limits = env.log_limits;

    let drv_path = &assignment.drv_path;
    let build_id = sanitize_build_id(drv_path);

    tracing::info!(
        drv_path = %drv_path,
        build_id = %build_id,
        is_fod = assignment.is_fixed_output,
        "starting build"
    );

    // Refuse new builds once the worker has leaked too many overlay mounts.
    // A leaked mount (umount2 failed in OverlayMount::Drop) usually means
    // the mount is stuck busy — open file handles, zombie nix-daemon, FUSE
    // hang. After N leaks the worker is degraded; returning
    // InfrastructureFailure here lets the scheduler reassign to a healthy
    // worker, and the supervisor can restart this one. We check at ENTRY,
    // not exit: a build that completes successfully shouldn't have its
    // result overridden just because its own teardown later fails — the
    // NEXT build is what gets refused.
    let leaked = leak_counter.load(Ordering::Relaxed);
    let threshold = env.max_leaked_mounts;
    if leaked >= threshold {
        tracing::error!(
            leaked,
            threshold,
            drv_path = %drv_path,
            "refusing build: leaked overlay mount threshold exceeded; worker needs restart"
        );
        return Ok(ExecutionResult {
            drv_path: drv_path.clone(),
            result: ProtoBuildResult {
                status: BuildResultStatus::InfrastructureFailure.into(),
                error_msg: format!(
                    "worker has {leaked} leaked overlay mounts (threshold {threshold}); refusing new builds"
                ),
                ..Default::default()
            },
            assignment_token: assignment.assignment_token.clone(),
            // Infra failure before daemon spawn → cgroup never
            // created, never populated. All resource fields = 0.
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        });
    }

    // 1. Parse the derivation. Scheduler inlines drv_content for
    // missing-output nodes; empty means cache-hit or
    // inline-budget exceeded, so fall back to store fetch.
    let drv = if assignment.drv_content.is_empty() {
        fetch_drv_from_store(store_client, drv_path).await?
    } else {
        // Strict UTF-8 — matches the else-branch (parse_from_nar uses
        // strict from_utf8 at derivation/mod.rs:168). Lossy would silently
        // produce U+FFFD → ATerm parse fails with a confusing "unexpected
        // character" instead of the real UTF-8 error. P0017's 2f807a4
        // eliminated this pattern; 395e826f reintroduced it one day after
        // P0020 closed. Clippy disallowed-methods (P0290) prevents round 3.
        let drv_text = std::str::from_utf8(&assignment.drv_content).map_err(|e| {
            ExecutorError::BuildFailed(format!("drv content is not valid UTF-8: {e}"))
        })?;
        Derivation::parse(drv_text)
            .map_err(|e| ExecutorError::BuildFailed(format!("failed to parse derivation: {e}")))?
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
        return Err(ExecutorError::WrongKind {
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

    // 2. Set up overlay. `setup_overlay` is synchronous (mkdir + stat +
    // overlayfs mount syscall); run on the blocking pool so a slow mount
    // (e.g., FUSE lower stalled on remote fetch) doesn't starve the Tokio
    // worker thread and block the heartbeat loop.
    let fuse_mp = fuse_mount_point.to_path_buf();
    let overlay_base = overlay_base_dir.to_path_buf();
    let build_id_owned = build_id.clone();
    let leak_counter_owned = Arc::clone(leak_counter);
    let overlay_mount = tokio::task::spawn_blocking(move || {
        overlay::setup_overlay(&fuse_mp, &overlay_base, &build_id_owned, leak_counter_owned)
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

    // 4. Populate sandbox: synth DB, nix.conf, FOD output whiteouts.
    prepare_sandbox(&overlay_mount, &drv, drv_path, input_metadata, is_fod).await?;

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
        // `RIO_BUILDER_JIT_FETCH=0` disarms (forces `NotArmed` →
        // legacy gRPC-anything) for one-revert rollback during stress.
        // Default on. Remove the gate after one stress cycle green.
        let jit_enabled = std::env::var("RIO_BUILDER_JIT_FETCH")
            .map(|v| v != "0")
            .unwrap_or(true);
        if jit_enabled {
            cache.register_inputs(input_sized.iter().filter_map(|(p, sz)| {
                Some((
                    p.strip_prefix(rio_nix::store_path::STORE_PREFIX)?
                        .to_owned(),
                    *sz,
                ))
            }));
            metrics::gauge!("rio_builder_jit_inputs_registered")
                .set(cache.known_inputs_len() as f64);
        }
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
    // Extract BuildOptions. The scheduler computes these per-derivation
    // from the intersecting builds' options (actor/build.rs min_nonzero
    // for timeouts, max for cores). `None` → daemon defaults: unbounded
    // silence, nproc cores. 0 → 0 on the wire = unbounded/all-cores to
    // the daemon — the scheduler's min_nonzero already handles the
    // 0-means-unset semantics; we pass through verbatim.
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
    // r[impl builder.cores.cgroup-clamp]
    // I-196: NEVER pass build_cores=0 to the daemon. 0 means "use
    // nproc", and nproc inside a pod sees ALL node cores (cgroup CPU
    // quota throttles scheduling, doesn't hide CPUs). On a 16-core
    // node a `tiny` (0.5-core, 1Gi) builder would run `make -j16` →
    // 16×cc1×~100MB → cgroup OOM-loop. Clamp to the pod's cpu.max
    // (delegated-root cgroup), and cap any client-requested value at
    // the same ceiling — a client asking for --cores 64 on a 2-core
    // pod gets 2.
    let effective_cores = u64::from(crate::cgroup::effective_cores(&env.cgroup_parent));
    let build_cores = match opts.map(|o| o.build_cores).filter(|&c| c > 0) {
        Some(client) => client.min(effective_cores),
        None => effective_cores,
    };
    tracing::debug!(
        effective_cores,
        build_cores,
        client_requested = opts.map(|o| o.build_cores),
        "build_cores clamped to cgroup cpu.max"
    );

    tracing::info!(drv_path = %drv_path, "spawning nix-daemon in mount namespace");
    let mut daemon = spawn_daemon_in_namespace(&overlay_mount).await?;
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
    let build_cgroup = crate::cgroup::BuildCgroup::create(&env.cgroup_parent, &build_id)
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

    // CPU polling task. Runs concurrently with run_daemon_build
    // below (which awaits). Samples cpu.stat usage_usec every second,
    // computes instantaneous cores = delta_usec/elapsed_usec, tracks
    // max. The cgroup's usage_usec is tree-cumulative, so this
    // captures the builder's CPU too.
    //
    // Stores max as f64 bits in an AtomicU64 — there's no AtomicF64.
    // compare_exchange loop for max (fetch_max on u64 bits would
    // compare BIT PATTERNS, not float values — 2.0_f64.to_bits() >
    // 8.0_f64.to_bits() is NOT guaranteed). Standard f64-atomic
    // pattern.
    //
    // Clone the cgroup PATH (not the BuildCgroup — moving it would
    // put Drop in the task, which we don't want; Drop must run after
    // daemon.wait() below).
    let peak_cpu_atomic = Arc::new(std::sync::atomic::AtomicU64::new(0));
    let cpu_poll_path = build_cgroup.path().to_path_buf();
    let cpu_poll_peak = Arc::clone(&peak_cpu_atomic);
    let cpu_poll = tokio::spawn(async move {
        let mut interval = tokio::time::interval(Duration::from_secs(1));
        // First tick fires immediately — skip it, we want a 1s baseline.
        interval.tick().await;
        let mut prev_usec = read_cpu_stat(&cpu_poll_path);
        let mut prev_instant = std::time::Instant::now();
        loop {
            interval.tick().await;
            let now_usec = read_cpu_stat(&cpu_poll_path);
            let now_instant = std::time::Instant::now();
            // Both samples must be Some. If the first read failed
            // (cgroup not populated yet — daemon hasn't forked),
            // prev is None and we just advance. If THIS read fails
            // (cgroup removed? shouldn't happen until Drop), skip.
            if let (Some(prev), Some(now)) = (prev_usec, now_usec) {
                let delta_usec = now.saturating_sub(prev);
                let elapsed_usec = now_instant.duration_since(prev_instant).as_micros() as u64;
                // elapsed_usec is ~1_000_000 (1s interval) but
                // jitters. Guard /0 for the impossible case where
                // two ticks fire at the same instant.
                if elapsed_usec > 0 {
                    let cores = delta_usec as f64 / elapsed_usec as f64;
                    // Compare-exchange max: load, if cores > current,
                    // try to swap. Loop until success or current >= cores.
                    let mut current_bits = cpu_poll_peak.load(Ordering::Relaxed);
                    loop {
                        if f64::from_bits(current_bits) >= cores {
                            break; // already higher, done
                        }
                        match cpu_poll_peak.compare_exchange_weak(
                            current_bits,
                            cores.to_bits(),
                            Ordering::Relaxed,
                            Ordering::Relaxed,
                        ) {
                            Ok(_) => break,                       // we set it
                            Err(actual) => current_bits = actual, // raced, retry
                        }
                    }
                }
            }
            prev_usec = now_usec;
            prev_instant = now_instant;
        }
    });

    // Abort guard: if run_daemon_build panics (or any `?` between here
    // and the explicit abort below early-returns), cpu_poll would leak
    // as a 1Hz polling task reading a dead cgroup path forever. Same
    // pattern as stderr_loop.rs:285. `.abort()` on a completed task is
    // a no-op, so the explicit abort below is harmless redundancy.
    let cpu_poll_abort = cpu_poll.abort_handle();
    let _cpu_poll_guard = scopeguard::guard((), move |()| cpu_poll_abort.abort());

    // r[impl builder.oom.cgroup-watch]
    // OOM watcher (I-196 defense-in-depth). Polls the POD-level
    // `memory.events` (delegated root — where k8s set memory.max; the
    // per-build sub-cgroup has no limit of its own) for `oom_kill`
    // increments. When the kernel OOM-kills a build process, make
    // typically respawns it → loop that burns the silence timeout.
    // Detect the first kill, cgroup.kill the build to break the loop,
    // and flag it so the result becomes CgroupOom (→ Infrastructure-
    // Failure → scheduler promotes size class) instead of a confusing
    // Wire(UnexpectedEof) or silence-timeout BuildFailed.
    //
    // Baseline captured at spawn: a prior build's OOM (or the FUSE
    // warm getting killed) shouldn't count. `None` baseline (file
    // unreadable — memory controller off, or non-k8s test env) → the
    // task idles harmlessly; the build_cores clamp above is the
    // primary fix anyway.
    let oom_detected = Arc::new(AtomicBool::new(false));
    let oom_watch = {
        let parent = env.cgroup_parent.clone();
        let kill_path = build_cgroup.path().to_path_buf();
        let flag = Arc::clone(&oom_detected);
        let baseline = crate::cgroup::read_oom_kill(&parent);
        tokio::spawn(async move {
            let Some(baseline) = baseline else {
                return; // can't watch — no memory.events
            };
            let mut interval = tokio::time::interval(Duration::from_secs(1));
            interval.tick().await; // skip immediate fire
            loop {
                interval.tick().await;
                let Some(n) = crate::cgroup::read_oom_kill(&parent) else {
                    continue; // transient read fail; keep polling
                };
                if n > baseline {
                    tracing::warn!(
                        baseline,
                        current = n,
                        "cgroup oom_kill incremented during build; killing build cgroup"
                    );
                    flag.store(true, Ordering::SeqCst);
                    // Break the make-respawn loop. run_daemon_build
                    // sees daemon EOF; the flag check below converts
                    // that into CgroupOom.
                    let _ = std::fs::write(kill_path.join("cgroup.kill"), "1");
                    return;
                }
            }
        })
    };
    let oom_watch_abort = oom_watch.abort_handle();
    let _oom_watch_guard = scopeguard::guard((), move |()| oom_watch_abort.abort());

    // All daemon I/O is in a helper so we can ALWAYS kill on error.
    // The cgroup setup above (create/add_process)
    // is NOT inside this helper — its `?` paths rely on the
    // kill_on_drop set in spawn_daemon_in_namespace as a safety
    // net. The explicit kill below remains the primary cleanup
    // (graceful, bounded wait for reap); kill_on_drop covers early
    // returns between spawn and here.
    let batcher = LogBatcher::new(drv_path.clone(), executor_id.to_string(), log_limits);
    let build_result = run_daemon_build(
        &mut daemon,
        drv_path,
        &basic_drv,
        timeout,
        max_silent_time,
        build_cores,
        batcher,
        log_tx,
    )
    .await;

    // Stop CPU polling. The last sample is up to 1s stale; good
    // enough (peak CPU doesn't change in the last second of a
    // multi-minute build). abort() doesn't wait — the task is
    // pure read, no cleanup needed. The scopeguard above also
    // aborts on scope exit; this explicit call is the happy-path
    // fast stop (guard fires redundantly after, which is a no-op
    // on an already-aborted handle).
    cpu_poll.abort();
    oom_watch.abort();
    let peak_cpu_cores = f64::from_bits(peak_cpu_atomic.load(Ordering::Acquire));

    // OOM override: if the watcher fired, run_daemon_build returned
    // some Err (Wire(UnexpectedEof) from the cgroup.kill, or possibly
    // a daemon-reported MiscFailure if the daemon caught the child
    // death first). Replace it with CgroupOom so runtime.rs reports
    // InfrastructureFailure with the size-class hint, NOT a transient-
    // retry (UnexpectedEof would hit `is_daemon_transient` → 3× local
    // retry → 3× more OOM-loops) and NOT BuildFailed (drv isn't broken).
    let build_result = if oom_detected.load(Ordering::SeqCst) {
        metrics::counter!("rio_builder_cgroup_oom_total").increment(1);
        Err(ExecutorError::CgroupOom)
    } else {
        build_result
    };

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

    // daemon.kill() above SIGKILLs the nix-daemon process only. The
    // builder is a GRANDCHILD (forked by the daemon during wopBuildDerivation)
    // and is not in the daemon's process group — it lives on in the
    // cgroup. On the success path the builder has already exited (build
    // finished → daemon sent STDERR_LAST → we got here); on the timeout/
    // error path it's still running a `sleep 3600` or a stuck compiler.
    //
    // cgroup.kill walks the tree: SIGKILLs everything, including sub-
    // cgroups the daemon may have created. Idempotent — writing "1" to
    // an empty cgroup is a no-op — so we call it unconditionally rather
    // than branching on build_result.is_err().
    //
    // r[impl builder.cgroup.kill-on-teardown]
    if let Err(e) = build_cgroup.kill() {
        // ENOENT shouldn't happen (we hold the BuildCgroup, Drop hasn't
        // run); EACCES would mean delegation is broken. Log and fall
        // through — rmdir will fail EBUSY and warn again, but we don't
        // want to upgrade a successful build to an error here.
        tracing::warn!(error = %e, "build_cgroup.kill() failed");
    }
    // cgroup.kill is async: write returns before procs are gone. Poll
    // cgroup.procs until empty or 2s elapsed (same budget as daemon.wait
    // above; SIGKILL → exit is ~ms, 2s is vast headroom for a zombie-
    // reparented tree). Sync read on blocking pool — 200 iterations of
    // a single-line procfs read, negligible.
    let cgroup_path_for_poll = build_cgroup.path().to_path_buf();
    let drained = tokio::task::spawn_blocking(move || {
        for _ in 0..200 {
            match std::fs::read_to_string(cgroup_path_for_poll.join("cgroup.procs")) {
                Ok(s) if s.trim().is_empty() => return true,
                Ok(_) => std::thread::sleep(Duration::from_millis(10)),
                // ENOENT: cgroup already gone (shouldn't happen — we
                // hold the BuildCgroup — but treat as drained).
                Err(_) => return true,
            }
        }
        false
    })
    .await
    .unwrap_or(false);
    if !drained {
        tracing::warn!(
            cgroup = %build_cgroup.path().display(),
            "cgroup still has processes 2s after cgroup.kill; rmdir will EBUSY"
        );
    }

    // Defuse: explicit kill+drain above already ran; guard is redundant.
    scopeguard::ScopeGuard::into_inner(cgroup_kill_guard);
    // build_cgroup drops here. rmdir succeeds if the drain above emptied
    // it; otherwise Drop warns EBUSY + leaks (cleared on pod restart).
    drop(build_cgroup);

    // (Final log flush happens inside read_build_stderr_loop — it owns
    // the batcher by-value.)

    // NOW propagate any daemon error (after kill).
    let build_result = build_result?;

    // 10. Collect outputs: FOD verify, upload, map to proto BuildResult.
    let BuildOutputs {
        proto_result,
        output_size_bytes,
    } = collect_outputs(
        &build_result,
        store_client,
        &overlay_mount,
        &drv,
        drv_path,
        is_fod,
        &input_paths,
        &assignment.assignment_token,
    )
    .await?;

    // 11. Tear down overlay (explicit, before Drop).
    // The leak-threshold check happens at ENTRY (above), not here: we don't
    // override a successful build result just because its own teardown fails.
    // Teardown failure increments the counter (in OverlayMount::Drop); the
    // NEXT build is what gets refused. The metric is also incremented in
    // Drop (centralized so ?-early-returns and panics also count).
    let merged_path = overlay_mount.merged_dir().to_path_buf();
    if let Err(e) = overlay::teardown_overlay(overlay_mount) {
        tracing::error!(
            error = %e,
            merged = %merged_path.display(),
            "overlay teardown failed; mount leaked"
        );
        // Metric incremented in Drop (see overlay.rs).
    }

    Ok(ExecutionResult {
        drv_path: drv_path.clone(),
        result: proto_result,
        assignment_token: assignment.assignment_token.clone(),
        peak_memory_bytes,
        output_size_bytes,
        peak_cpu_cores,
    })
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
    input_metadata: Vec<SynthPathInfo>,
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
    .map_err(|e| ExecutorError::BuildFailed(format!("failed to build BasicDerivation: {e}")))?;

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
    let input_paths: Vec<String> = input_metadata.iter().map(|m| m.path.clone()).collect();
    // I-178: project (path, nar_size) for the JIT FUSE allowlist
    // (`register_inputs`). SynthPathInfo already has nar_size from
    // BatchQueryPathInfo (authoritative; the ManifestHint.info.nar_size
    // is best-effort). Two projections from input_metadata is cheaper
    // than passing &input_metadata around — the other consumers
    // (prefetch_manifests, ref-scan) want plain &[String].
    let input_sized: Vec<(String, u64)> = input_metadata
        .iter()
        .map(|m| (m.path.clone(), m.nar_size))
        .collect();

    Ok(ResolvedInputs {
        basic_drv,
        input_paths,
        input_sized,
        input_metadata,
    })
}

/// Populate the sandbox: synthetic SQLite DB, nix.conf, FOD whiteouts.
///
/// Runs after overlay mount setup and input resolution. All side-effects
/// on the overlay's upper layer — no state returned. The overlay_mount
/// is held by the caller (execute_build) for later daemon spawn + upload
/// + teardown.
///
/// Steps:
/// 1. Generate synthetic DB from `synth_paths` (ValidPaths +
///    DerivationOutputs) so nix-daemon's isValidPath()/queryPartial
///    DerivationOutputMap() work without a real store.
/// 2. Write nix.conf (sandbox=true, substitute=false).
/// 3. For FODs: mknod whiteout char-devs for declared output paths so
///    daemon post-fail stat gets ENOENT from upper without probing FUSE.
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
async fn prepare_sandbox(
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    synth_paths: Vec<SynthPathInfo>,
    is_fod: bool,
) -> Result<(), ExecutorError> {
    // Generate synthetic DB from caller-supplied metadata (I-106:
    // captured during compute_input_closure's BFS, no second QPI pass).
    // CRITICAL: populate DerivationOutputs so nix-daemon's
    // queryPartialDerivationOutputMap(drvPath) returns our output paths.
    // Without it, initialOutputs[out].known is None → nix-daemon builds at
    // makeFallbackPath() (hash of "rewrite:<drvPath>:name:out" + zero hash),
    // but the builder's $out (from BasicDerivation env) is the REAL path →
    // output path mismatch → "builder failed to produce output path".
    //
    // CA floating outputs have an empty path (computed post-build from the
    // NAR hash). Inserting an empty-string path makes nix-daemon's
    // queryStaticPartialDerivationOutputMap call parseStorePath("") which
    // aborts the daemon (core dump). Real Nix never writes DerivationOutputs
    // rows for floating-CA — the output path is unknown until the build
    // finishes and the daemon writes a Realisations row instead. Filter them
    // here; nix-daemon computes scratchPath internally for CA outputs and
    // doesn't need the DerivationOutputs hint.
    let drv_outputs: Vec<SynthDrvOutput> = drv
        .outputs()
        .iter()
        .filter(|o| !o.path().is_empty())
        .map(|o| SynthDrvOutput {
            drv_path: drv_path.to_string(),
            output_name: o.name().to_string(),
            output_path: o.path().to_string(),
        })
        .collect();
    let db_dir = overlay::prepare_nix_state_dirs(&overlay_mount.upper_synth_db())?;
    let db_path = db_dir.join("db.sqlite");
    synth_db::generate_db(&db_path, &synth_paths, &drv_outputs).await?;

    // Set up nix.conf in overlay
    setup_nix_conf(&overlay_mount.upper_nix_conf())?;

    // Whiteout declared output paths in the overlay upper layer.
    //
    // r[impl builder.fod.verify-hash]
    //
    // P0308: FOD BuildResult propagation hang. When a builder exits
    // nonzero WITHOUT creating `$out` (wget 403 → exit 1, typical FOD
    // failure), nix-daemon's post-build cleanup — `deletePath(outputPath)`
    // in LocalDerivationGoal — stats `/nix/store/<output-basename>`.
    //
    // The overlay resolves this lookup layer by layer:
    //   upper ({upper}/nix/store/)  → ENOENT (builder never wrote it)
    //   lower (FUSE)                → lookup() → ensure_cached() → gRPC
    //
    // The FUSE gRPC should return ENOENT quickly, but empirically in the
    // k3s fixture it blocks — the daemon's stat syscall hangs, the daemon
    // never writes STDERR_LAST, and nix-build waits until `timeout 90`.
    //
    // Success path is unaffected: builder wrote `$out` → upper has it →
    // overlay resolves immediately, FUSE never probed.
    //
    // The whiteout fix: mknod a char device 0/0 for each output path
    // DIRECTLY IN THE UPPER DIR — bypassing overlay semantics entirely.
    //
    // Why not create-then-delete via the merged dir? Overlayfs only
    // writes a whiteout when `ovl_lower_positive()` returns true, i.e.
    // when at least one lower has the name. Here the FUSE lower ENOENTs
    // (output not yet in rio-store), so unlink via merged takes the
    // `ovl_remove_upper` path — plain unlink, no whiteout.
    // Empirically verified on Linux 6.12: create+rm via
    // merged for a name absent from all lowers leaves upper EMPTY.
    //
    // A char device 0/0 placed directly in the upperdir IS the
    // whiteout format (Documentation/filesystems/overlayfs.rst). The
    // kernel's merged-view lookup sees it and returns ENOENT without
    // consulting lowers — regardless of what lowers would say.
    // Post-whiteout:
    //
    //   - Daemon's pre-build deletePath(output): lstat → ENOENT (whiteout)
    //     → nothing to delete → returns. Whiteout survives.
    //   - Builder creates $out as FILE: open(O_CREAT)/link()/rename-file via
    //     merged → overlayfs replaces the whiteout with a real file in upper.
    //     Success path unchanged. **mkdir() onto a whiteout → EIO** (verified
    //     Linux 6.12; rename-dir → EXDEV) — the whiteout SURVIVES. Hence the
    //     is_fod guard below: fod-fetch.nix (the hang's repro) uses
    //     outputHashMode=flat → `wget -O $out` → file. Non-FOD `mkdir $out`
    //     callers (lifecycle.nix:171,218,227) skip the whiteout entirely.
    //     Recursive-mode FODs (NAR-hash dir outputs) are a known gap; not
    //     the plan's target, and the FUSE-spin hang is FOD-hash-verify-path
    //     specific — non-FOD failures don't probe $out the same way, so
    //     they never needed this whiteout.
    //   - Builder fails, $out never created: whiteout remains → daemon's
    //     post-fail stat gets ENOENT from upper → FUSE never probed →
    //     daemon proceeds → STDERR_LAST + BuildResult{PermanentFailure}.
    //
    // mknod(S_IFCHR) needs CAP_MKNOD. We hold it: this runs in the
    // worker's initial namespace before spawn_daemon_in_namespace, with
    // the same caps that mount the overlay (CAP_SYS_ADMIN ⊇ CAP_MKNOD
    // in practice; both granted to the worker pod). The syscall goes
    // straight to the upper's backing fs (local SSD) — the overlay and
    // FUSE are never consulted, so no spawn_blocking needed.
    //
    // `{upper}/nix/store/` is the overlayfs upperdir (overlay.rs:201).
    // It's a fresh empty dir per-build (mkdir_all at overlay setup),
    // so EEXIST is impossible here.
    if is_fod {
        let upper_store = overlay_mount.upper_store();
        // drv.is_fixed_output() ⇒ exactly one output named "out"
        // (derivation/mod.rs:211); loop body runs once.
        for out in drv.outputs() {
            // Output paths are always absolute store paths. An empty
            // path (CA derivations with unknown output paths) can't be
            // whitedout — skip and let the daemon's own logic handle it.
            let Some(basename) = out
                .path()
                .strip_prefix(rio_nix::store_path::STORE_PREFIX)
                .filter(|b| !b.is_empty())
            else {
                continue;
            };
            let whiteout = upper_store.join(basename);
            nix::sys::stat::mknod(
                &whiteout,
                nix::sys::stat::SFlag::S_IFCHR,
                nix::sys::stat::Mode::empty(),
                0, // dev_t 0 (major 0, minor 0) — the overlayfs whiteout signature
            )
            .map_err(|errno| {
                // EPERM → missing CAP_MKNOD (pod securityContext regression).
                // EROFS → upper not writable (overlay misconfigured).
                // Either way the daemon spawn would fail; fail early with context.
                ExecutorError::DaemonSetup(format!(
                    "mknod whiteout for output {basename:?} at {}: {errno}",
                    whiteout.display()
                ))
            })?;
            // Diagnostic: verify the whiteout is visible as ENOENT through
            // the MERGED view. The mknod above writes directly to the
            // upperdir backing fs; this stat goes through overlayfs. If it
            // doesn't return ENOENT, the char-dev-0/0 whiteout approach
            // isn't being honored in this environment (nested overlay,
            // unusual mount options, kernel quirk) — the build will still
            // proceed but the FOD-failure hang fix won't take effect.
            // See TODO(P0311-T10) in netpol.nix (pre-ADR-019).
            let merged_check = overlay_mount.merged_dir().join(basename);
            match std::fs::symlink_metadata(&merged_check) {
                Err(e) if e.kind() == std::io::ErrorKind::NotFound => {
                    tracing::debug!(
                        output = basename,
                        upper = %whiteout.display(),
                        "FOD output whiteout created and visible as ENOENT via merged"
                    );
                }
                other => {
                    tracing::warn!(
                        output = basename,
                        upper = %whiteout.display(),
                        merged = %merged_check.display(),
                        result = ?other,
                        "FOD output whiteout NOT visible as ENOENT via merged — \
                         daemon post-fail stat may fall through to FUSE and hang"
                    );
                }
            }
        }
    }

    Ok(())
}

/// Collected build outputs: proto BuildResult + total uploaded bytes.
struct BuildOutputs {
    /// Proto BuildResult to send to the scheduler in CompletionReport.
    proto_result: ProtoBuildResult,
    /// Sum of uploaded NAR sizes. 0 on build failure or output rejection.
    output_size_bytes: u64,
}

/// Collect build outputs: FOD verify, upload, map to proto BuildResult.
///
/// Runs after daemon teardown. On success: verifies FOD hashes (if
/// applicable), uploads outputs to the store, maps upload results to
/// proto BuiltOutput entries. On build failure: maps the nix-daemon
/// BuildStatus to the proto equivalent.
///
/// Reference-scan candidate set = input_paths ∪ drv.outputs() ∪
/// build_result.built_outputs (the last for floating-CA self-refs).
#[instrument(skip_all, fields(drv_path = %drv_path, is_fod))]
#[allow(clippy::too_many_arguments)]
async fn collect_outputs(
    build_result: &rio_nix::protocol::build::BuildResult,
    store_client: &mut StoreServiceClient<Channel>,
    overlay_mount: &overlay::OverlayMount,
    drv: &Derivation,
    drv_path: &str,
    is_fod: bool,
    input_paths: &[String],
    assignment_token: &str,
) -> Result<BuildOutputs, ExecutorError> {
    if !build_result.status.is_success() {
        // I-178: daemon ENOENT on a closure input is worker-local
        // materialization failure (warm timeout / FUSE EIO / I-043
        // negative-dentry), NOT a build defect. Reclassify so the
        // scheduler retries instead of poisoning. Checked BEFORE the
        // generic nix_failure_to_proto collapse (MiscFailure →
        // PermanentFailure).
        if is_input_materialization_failure(
            build_result.status,
            &build_result.error_msg,
            input_paths,
        ) {
            // r[impl obs.metric.input-materialization-failures]
            metrics::counter!("rio_builder_input_materialization_failures_total").increment(1);
            tracing::warn!(
                drv_path = %drv_path,
                error = %build_result.error_msg,
                "daemon ENOENT on closure input — reclassifying MiscFailure → \
                 InfrastructureFailure (warm timeout / FUSE EIO / I-043 race)"
            );
            return Ok(BuildOutputs {
                proto_result: ProtoBuildResult {
                    status: BuildResultStatus::InfrastructureFailure.into(),
                    error_msg: format!(
                        "input materialization failed (I-043/I-178): {}",
                        build_result.error_msg
                    ),
                    ..Default::default()
                },
                output_size_bytes: 0,
            });
        }
        tracing::warn!(
            drv_path = %drv_path,
            status = ?build_result.status,
            error = %build_result.error_msg,
            "build failed"
        );
        return Ok(BuildOutputs {
            proto_result: ProtoBuildResult {
                status: nix_failure_to_proto(build_result.status).into(),
                error_msg: build_result.error_msg.clone(),
                ..Default::default()
            },
            output_size_bytes: 0,
        });
    }

    // FOD defense-in-depth BEFORE upload: verify_fod_hashes
    // computes local NAR hashes (via dump_path_streaming +
    // digest sink) so a bad output is rejected WITHOUT entering
    // the store. Verifying after upload would mean the bad
    // output is already content-indexed and manifest-complete
    // before the mismatch is noticed.
    //
    // spawn_blocking: verify_fod_hashes does sync filesystem I/O
    // (fs::read for flat, dump_path_streaming for recursive) +
    // hashing. Typical FOD outputs are small (fetchurl), so
    // this is fast.
    if is_fod {
        let drv_for_verify = drv.clone();
        let upper_store_for_verify = overlay_mount.upper_store();
        let verify_result = tokio::task::spawn_blocking(move || {
            verify_fod_hashes(&drv_for_verify, &upper_store_for_verify)
        })
        .await
        .map_err(|e| ExecutorError::BuildFailed(format!("FOD verify task panicked: {e}")))?;

        if let Err(e) = verify_result {
            tracing::error!(
                drv_path = %drv_path,
                error = %e,
                "FOD output hash verification failed — NOT uploading"
            );
            // Build DID run (FOD verification is post-build). Caller
            // already has peak_memory_bytes/peak_cpu_cores from the
            // cgroup; they're meaningful even though we reject output.
            return Ok(BuildOutputs {
                proto_result: ProtoBuildResult {
                    status: BuildResultStatus::OutputRejected.into(),
                    error_msg: format!("FOD output hash verification failed: {e}"),
                    ..Default::default()
                },
                output_size_bytes: 0,
            });
        }
    }

    tracing::info!(drv_path = %drv_path, "build succeeded, uploading outputs");

    // Upload outputs.
    //
    // Reference-scan candidate set = input_paths ∪ drv.outputs():
    //   - input_paths: the TRANSITIVE input closure, built above via
    //     compute_input_closure (BFS over QueryPathInfo.references,
    //     seeded from input_srcs + inputDrv outputs). This matches
    //     Nix's computeFSClosure — see derivation-building-goal.cc:444,450
    //     and derivation-builder.cc:1335-1344 in Nix 2.31.3. A build can
    //     legitimately embed any path reachable from its inputs: e.g.
    //     hello-2.12.2 references glibc, which is NOT a direct input
    //     but comes via closure(stdenv). Scanning only direct inputs
    //     would drop those references.
    //   - drv.outputs(): self-references and cross-output references are
    //     legal (e.g., a -dev output referencing the lib output's rpath,
    //     or a binary embedding its own store path in an rpath).
    let mut ref_candidates: Vec<String> = input_paths.to_vec();
    ref_candidates.extend(
        drv.outputs()
            .iter()
            .filter(|o| !o.path().is_empty())
            .map(|o| o.path().to_string()),
    );
    // Floating-CA: .drv has path = ""; the real path comes from
    // the daemon's BuildResult. Needed for self-references.
    ref_candidates.extend(
        build_result
            .built_outputs
            .iter()
            .map(|bo| bo.out_path.clone()),
    );

    let proto_result = match upload::upload_all_outputs(
        store_client,
        &overlay_mount.upper_store(),
        // Pass the assignment token as gRPC metadata on each
        // PutPath. Store with hmac_verifier checks it. Empty
        // token (scheduler without hmac_signer, dev mode) →
        // no header → store with verifier=None accepts.
        assignment_token,
        drv_path,
        &ref_candidates,
    )
    .await
    {
        Ok(upload_results) => {
            // Sum NAR sizes for the CompletionReport. Feeds
            // build_history.ema_output_size_bytes — not used for
            // routing yet but useful for dashboards / capacity.
            let output_size_bytes: u64 = upload_results.iter().map(|r| r.nar_size).sum();

            // Map store_path → output_name. Upload results are
            // unordered (buffer_unordered), and even the prior
            // sequential scan had undefined order (read_dir).
            //
            // Two sources:
            //  - drv.outputs(): works for IA and fixed-CA, where
            //    the .drv has the output path baked in.
            //  - build_result.built_outputs: for floating-CA
            //    (__contentAddressed = true), the .drv has
            //    path = "" (computed post-build from NAR hash).
            //    The daemon's BuildResult carries the realized
            //    path in its Realisation entries; the output
            //    name is the suffix of drv_output_id after '!'.
            //
            // Without the second source, CA builds fail the
            // lookup below with "not in derivation outputs" —
            // the upload scanned the real /nix/store/<hash>-name
            // but path_to_name only had "" → name.
            let mut path_to_name: HashMap<&str, &str> = drv
                .outputs()
                .iter()
                .filter(|o| !o.path().is_empty())
                .map(|o| (o.path(), o.name()))
                .collect();
            for bo in &build_result.built_outputs {
                if let Some(name) = bo.drv_output_id.rsplit('!').next() {
                    path_to_name.insert(bo.out_path.as_str(), name);
                }
            }
            // wkr-scan-unfiltered (21-p2-p3-rollup Batch B): if the
            // lookup misses, scan_new_outputs picked up a stray file
            // under /nix/store (tempfile leak, .drv, etc.) that is NOT
            // a declared derivation output. Prior behavior: warn and
            // upload anyway with basename-as-output-name → phantom
            // output reported to scheduler. Now: fail the build. The
            // stray upload has already hit the store (upload_all_outputs
            // ran first) but it's unreferenced and GC-eligible; the
            // scheduler won't mark this drv Built so nothing downstream
            // can depend on the phantom.
            let built_outputs: Vec<BuiltOutput> = upload_results
                .iter()
                .map(|result| {
                    let output_name = path_to_name
                        .get(result.store_path.as_str())
                        .map(|s| s.to_string())
                        .ok_or_else(|| {
                            tracing::warn!(
                                store_path = %result.store_path,
                                "uploaded path not in derivation outputs — rejecting build"
                            );
                            ExecutorError::BuildFailed(format!(
                                "uploaded path {} not in derivation outputs (stray file in overlay upper /nix/store?)",
                                result.store_path
                            ))
                        })?;
                    Ok::<_, ExecutorError>(BuiltOutput {
                        output_name,
                        output_path: result.store_path.clone(),
                        output_hash: result.nar_hash.to_vec(),
                    })
                })
                .collect::<Result<Vec<_>, _>>()?;

            // start/stop_time: nix-daemon's BuildResult already has
            // these as Unix epoch seconds (rio-nix/build.rs:118-120).
            // Scheduler guards update_build_history on BOTH being
            // Some (completion.rs:182) — without them, EMA duration
            // can't be computed and the WHOLE build_history write
            // is skipped (peak_memory_bytes included). VM tests
            // bypass this via direct psql INSERT — only live
            // scheduler-worker integration exercises it.
            //
            // 0 → None: nix-daemon sends 0 on some error paths.
            // A real build at 1970-01-01 doesn't exist.
            let to_proto_ts = |secs: u64| {
                (secs > 0).then_some(prost_types::Timestamp {
                    seconds: secs as i64,
                    nanos: 0,
                })
            };
            return Ok(BuildOutputs {
                proto_result: ProtoBuildResult {
                    status: BuildResultStatus::Built.into(),
                    error_msg: String::new(),
                    times_built: build_result.times_built,
                    start_time: to_proto_ts(build_result.start_time),
                    stop_time: to_proto_ts(build_result.stop_time),
                    built_outputs,
                },
                output_size_bytes,
            });
        }
        Err(e) => {
            tracing::error!(drv_path = %drv_path, error = %e, "output upload failed");
            ProtoBuildResult {
                status: BuildResultStatus::InfrastructureFailure.into(),
                error_msg: format!("output upload failed: {e}"),
                ..Default::default()
            }
        }
    };

    Ok(BuildOutputs {
        proto_result,
        output_size_bytes: 0,
    })
}

/// Write nix.conf to the overlay upper layer.
///
/// Checks for an operator override at [`NIX_CONF_OVERRIDE_PATH`]
/// first (mounted from the `rio-nix-conf` ConfigMap in K8s). If
/// present, copies it verbatim; else uses [`WORKER_NIX_CONF`].
///
/// Override use case: operator wants to add e.g. `extra-sandbox-
/// paths = /some/secret` or tweak `sandbox-build-dir`. ConfigMap
/// edit + pod restart, no image rebuild.
fn setup_nix_conf(upper_nix_conf: &Path) -> Result<(), ExecutorError> {
    std::fs::create_dir_all(upper_nix_conf).map_err(ExecutorError::NixConf)?;

    // Try the override first. `read` (not `read_to_string`) —
    // nix.conf is ASCII but we're just copying bytes, no reason
    // to UTF-8-validate. ENOENT OR empty = not mounted → fallback.
    // Any OTHER error (permission denied, I/O) → bubble up
    // (something's wrong with the mount).
    //
    // The mount is a DIRECTORY (no subPath): `optional: true`
    // ConfigMap + missing ConfigMap → K8s mounts an empty dir →
    // read("dir/nix.conf") → clean ENOENT → fallback. Directory
    // mount (no subPath): subPath creates an empty file/dir when
    // the ConfigMap is missing → empty nix.conf → Nix defaults →
    // substitute=true → cache.nixos.org lookup → airgap DNS
    // timeout (600s+ hang).
    let content = match std::fs::read(NIX_CONF_OVERRIDE_PATH) {
        Ok(bytes) if !bytes.is_empty() => {
            tracing::debug!(
                path = NIX_CONF_OVERRIDE_PATH,
                "using nix.conf override from ConfigMap mount"
            );
            bytes
        }
        // Empty OR NotFound: ConfigMap not applied, or key missing.
        // Either way, compiled-in fallback.
        Ok(_) => WORKER_NIX_CONF.as_bytes().to_vec(),
        Err(e) if e.kind() == std::io::ErrorKind::NotFound => WORKER_NIX_CONF.as_bytes().to_vec(),
        Err(e) => return Err(ExecutorError::NixConf(e)),
    };

    std::fs::write(upper_nix_conf.join("nix.conf"), content).map_err(ExecutorError::NixConf)?;
    Ok(())
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

/// True iff the daemon's `MiscFailure` is `getting attributes of path
/// '<p>': No such file or directory` where `<p>` is in the build's
/// input closure.
///
/// I-178: that pattern means the daemon's sandbox-setup `lstat(input)`
/// hit overlay → FUSE → ENOENT (warm timeout, FUSE EIO, or the I-043
/// negative-dentry race). The input was verified present in rio-store
/// by `compute_input_closure` (BatchQueryPathInfo only returns found
/// paths); its absence at sandbox-setup is a worker-local
/// materialization failure, NOT a build defect. Reporting
/// `PermanentFailure` poisons the derivation; `InfrastructureFailure`
/// lets the scheduler retry on a fresh worker.
///
/// String-matching the daemon's error is brittle but the message is
/// stable since Nix 2.3 (`libstore/posix-fs-canonicalise.cc`). The
/// `<p> ∈ input_paths` membership check is the load-bearing guard — a
/// genuinely-missing path NOT in the closure stays `PermanentFailure`.
///
/// I-178b: live cluster output is ANSI-colored AND the path the daemon
/// reports is the OVERLAY path (`/var/rio/overlays/<build_id>/nix/store/
/// <hash>-<name>`), not the bare `/nix/store/<hash>-<name>` we have in
/// `input_paths`. So: strip ANSI escapes first, then match by BASENAME
/// only — `<hash>-<name>` is unique (the nixbase32 hash makes
/// collisions practically impossible) and is the trailing path component
/// in both overlay and store-path forms. The errno suffix is NOT
/// matched: ENOENT (`No such file or directory`) and EIO (`Input/output
/// error`, see I-179) are both worker-local materialization failures.
///
// r[impl builder.result.input-enoent-is-infra+2]
pub(crate) fn is_input_materialization_failure(
    nix_status: rio_nix::protocol::build::BuildStatus,
    error_msg: &str,
    input_paths: &[String],
) -> bool {
    use rio_nix::protocol::build::BuildStatus;
    use std::sync::LazyLock;

    // ANSI SGR escapes: ESC [ <params> m. nix-daemon emits 31;1 (bold red)
    // and 35;1 (bold magenta) around `error:` and the quoted path. Stripping
    // BEFORE the substring split means the `'...'` extract sees clean text.
    static ANSI: LazyLock<regex::Regex> =
        LazyLock::new(|| regex::Regex::new(r"\x1b\[[0-9;]*m").expect("static regex"));

    if nix_status != BuildStatus::MiscFailure {
        return false;
    }
    let stripped = ANSI.replace_all(error_msg, "");
    // nix-daemon's input-stat path emits one of several phrasings
    // depending on which libutil helper failed (`lstat`/`stat`/
    // `getFileType`/`readDirectory`):
    //   • "getting attributes of path '<p>': <strerror>"  (lstat)
    //   • "getting status of '<p>': <strerror>"            (stat)
    //   • "reading directory '<p>': <strerror>"            (readdir)
    //   • "opening file '<p>': <strerror>"                 (open)
    // I-189: matching only the first missed `getting status of`. All
    // four indicate the same materialization failure when `<p>` is a
    // closure input. Closure ≤ ~2k entries; linear scan is fine.
    const MARKERS: &[&str] = &[
        "getting attributes of path '",
        "getting status of '",
        "reading directory '",
        "opening file '",
    ];
    let Some(rest) = MARKERS.iter().find_map(|m| stripped.split(m).nth(1)) else {
        return false;
    };
    let Some(path) = rest.split('\'').next() else {
        return false;
    };
    // Basename match: the daemon reports the overlay path (I-178b); the
    // closure has store paths. Both end in `<hash>-<name>`. An empty
    // basename (trailing slash — shouldn't happen, but defensive) must
    // not vacuously match every closure entry.
    let basename = path.rsplit('/').next().unwrap_or(path);
    if basename.is_empty() {
        return false;
    }
    input_paths
        .iter()
        .any(|p| p.rsplit('/').next().unwrap_or(p) == basename)
}

/// Map a Nix daemon BuildStatus (failure path only — caller has already
/// branched on is_success()) to the proto BuildResultStatus reported to
/// the scheduler.
///
/// Exhaustive: no `_` arm. Adding a new BuildStatus variant in rio-nix
/// is a compile error here until the mapping decision is made.
///
// r[impl builder.status.nix-to-proto]
pub(crate) fn nix_failure_to_proto(
    nix: rio_nix::protocol::build::BuildStatus,
) -> BuildResultStatus {
    use rio_nix::protocol::build::BuildStatus as Nix;
    match nix {
        // Success variants: caller branched on is_success(), these are
        // unreachable. Return Built anyway (not a panic — if the caller
        // contract is ever violated, a wrong-but-success status is less
        // damaging than a worker crash mid-build).
        Nix::Built | Nix::Substituted | Nix::AlreadyValid | Nix::ResolvesToAlreadyValid => {
            debug_assert!(
                false,
                "nix_failure_to_proto called with success status {nix:?}"
            );
            BuildResultStatus::Built
        }

        // 1:1 mappings — proto variant exists with identical semantics.
        Nix::PermanentFailure => BuildResultStatus::PermanentFailure,
        Nix::TransientFailure => BuildResultStatus::TransientFailure,
        Nix::CachedFailure => BuildResultStatus::CachedFailure,
        Nix::DependencyFailed => BuildResultStatus::DependencyFailed,
        Nix::LogLimitExceeded => BuildResultStatus::LogLimitExceeded,
        Nix::OutputRejected => BuildResultStatus::OutputRejected,
        Nix::InputRejected => BuildResultStatus::InputRejected,
        Nix::TimedOut => BuildResultStatus::TimedOut,
        Nix::NotDeterministic => BuildResultStatus::NotDeterministic,

        // Intentional collapse: MiscFailure is nix-daemon's own catch-all
        // (used when it can't classify). PermanentFailure is the honest
        // proto equivalent — "it failed, we don't know why, don't retry."
        Nix::MiscFailure => BuildResultStatus::PermanentFailure,

        // Intentional collapse: NoSubstituters means "I was asked to
        // substitute and couldn't find a substituter." Our workers run
        // with `substitute = false` (WORKER_NIX_CONF) — we never ask the
        // daemon to substitute. If we see this, something is misconfigured;
        // PermanentFailure + the error_msg is the right signal.
        Nix::NoSubstituters => BuildResultStatus::PermanentFailure,
    }
}

#[cfg(test)]
mod tests {
    use super::*;

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
        // size-class promotion (I-196).
        assert!(!ExecutorError::CgroupOom.is_daemon_transient());
    }

    #[test]
    fn test_worker_nix_conf_content() {
        assert!(WORKER_NIX_CONF.contains("sandbox = true"));
        assert!(WORKER_NIX_CONF.contains("substitute = false"));
        assert!(WORKER_NIX_CONF.contains("builders ="));
        assert!(WORKER_NIX_CONF.contains("sandbox-fallback = false"));
    }

    #[test]
    fn test_setup_nix_conf() -> anyhow::Result<()> {
        let dir = tempfile::tempdir()?;
        let conf_dir = dir.path().join("etc/nix");
        setup_nix_conf(&conf_dir)?;

        let conf_path = conf_dir.join("nix.conf");
        assert!(conf_path.exists());
        let content = std::fs::read_to_string(&conf_path)?;
        assert!(content.contains("sandbox = true"));
        Ok(())
    }

    /// When the leak counter is at or over threshold, execute_build must
    /// short-circuit with InfrastructureFailure BEFORE touching the overlay.
    /// This test does NOT require CAP_SYS_ADMIN: it sets the counter over
    /// the threshold and asserts the short-circuit path, which runs before
    /// setup_overlay is ever called.
    #[tokio::test]
    async fn test_execute_build_refuses_when_leaked_exceeds_threshold() -> anyhow::Result<()> {
        // Set well over any plausible threshold (default is 3).
        let leak_counter = Arc::new(AtomicUsize::new(999));

        let assignment = WorkAssignment {
            drv_path: rio_test_support::fixtures::test_drv_path("test"),
            assignment_token: "token-123".into(),
            ..Default::default()
        };

        // store_client: execute_build short-circuits before any gRPC call, so
        // we pass a client pointed at a garbage endpoint. connect_lazy() does
        // not dial until first use.
        let channel = Channel::from_static("http://127.0.0.1:1").connect_lazy();
        let mut store_client = StoreServiceClient::new(channel);

        let (log_tx, _log_rx) = mpsc::channel(1);
        let dir = tempfile::tempdir()?;

        let env = ExecutorEnv {
            fuse_mount_point: dir.path().to_path_buf(),
            overlay_base_dir: dir.path().to_path_buf(),
            executor_id: "test-worker".into(),
            log_limits: LogLimits::UNLIMITED,
            max_leaked_mounts: 3,
            daemon_timeout: DEFAULT_DAEMON_TIMEOUT,
            max_silent_time: 0,
            // Short-circuit path never reaches cgroup setup (bails at
            // the leak-threshold check). Tempdir is fine.
            cgroup_parent: dir.path().to_path_buf(),
            executor_kind: rio_proto::types::ExecutorKind::Builder,
            fuse_cache: None,
            fuse_fetch_timeout: Duration::from_secs(60),
            cancelled: Arc::new(AtomicBool::new(false)),
        };
        let result =
            execute_build(&assignment, &env, &mut store_client, &log_tx, &leak_counter).await;

        // Must be Ok(ExecutionResult{InfrastructureFailure}), NOT Err —
        // Ok ensures CompletionReport is sent so the scheduler reassigns.
        let exec = result.expect("short-circuit path returns Ok, not Err");
        assert_eq!(exec.drv_path, assignment.drv_path);
        assert_eq!(exec.assignment_token, "token-123");
        assert_eq!(
            exec.result.status,
            BuildResultStatus::InfrastructureFailure as i32,
            "should report InfrastructureFailure"
        );
        assert!(
            exec.result.error_msg.contains("leaked overlay mount"),
            "error message should mention leaked mounts, got: {}",
            exec.result.error_msg
        );
        assert!(
            exec.result.error_msg.contains("999"),
            "error message should include the leak count, got: {}",
            exec.result.error_msg
        );

        // Counter unchanged — short-circuit doesn't touch the overlay.
        assert_eq!(leak_counter.load(Ordering::Relaxed), 999);
        Ok(())
    }

    /// Every non-success Nix BuildStatus maps to a proto status. No
    /// variant hits a `_` arm — the mapping fn is exhaustive so adding
    /// a Nix variant is a compile error.
    ///
    /// Stronger than "compiles": asserts the MAPPING DECISIONS stay
    /// stable. If someone changes TimedOut → TransientFailure (which
    /// would reintroduce the reassignment storm), this test fails.
    ///
    // r[verify builder.status.nix-to-proto]
    #[test]
    fn test_nix_failure_to_proto_is_exhaustive_and_stable() {
        use rio_nix::protocol::build::BuildStatus as Nix;
        use rio_proto::build_types::BuildResultStatus as Proto;

        // 1:1 mappings — each Nix failure gets its OWN proto variant.
        let one_to_one = [
            (Nix::PermanentFailure, Proto::PermanentFailure),
            (Nix::TransientFailure, Proto::TransientFailure),
            (Nix::CachedFailure, Proto::CachedFailure),
            (Nix::DependencyFailed, Proto::DependencyFailed),
            (Nix::LogLimitExceeded, Proto::LogLimitExceeded),
            (Nix::OutputRejected, Proto::OutputRejected),
            (Nix::InputRejected, Proto::InputRejected),
            (Nix::TimedOut, Proto::TimedOut),
            (Nix::NotDeterministic, Proto::NotDeterministic),
        ];
        for (nix, want) in one_to_one {
            assert_eq!(
                nix_failure_to_proto(nix),
                want,
                "1:1 mapping broke for {nix:?}"
            );
        }

        // Intentional collapses — documented reasons in the fn body.
        assert_eq!(
            nix_failure_to_proto(Nix::MiscFailure),
            Proto::PermanentFailure
        );
        assert_eq!(
            nix_failure_to_proto(Nix::NoSubstituters),
            Proto::PermanentFailure
        );
    }

    /// I-178: daemon `MiscFailure` with `getting attributes of path
    /// '<input>': No such file or directory` is a worker-local
    /// materialization failure (warm timeout / FUSE EIO / I-043 race),
    /// not a build defect. The membership check is load-bearing — a
    /// path NOT in the closure stays PermanentFailure.
    ///
    /// I-178b: the live cluster message is ANSI-colored and reports the
    /// OVERLAY path, not the store path. Strip ANSI; match by basename.
    // r[verify builder.result.input-enoent-is-infra+2]
    #[test]
    fn test_is_input_materialization_failure() {
        use rio_nix::protocol::build::BuildStatus as Nix;

        let input = "/nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source".to_string();
        let other = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-hello".to_string();
        let closure = vec![input.clone(), other.clone()];
        let enoent = format!(
            "while setting up the build environment: getting attributes of \
             path '{input}': No such file or directory"
        );

        // MiscFailure + matching path → true (reclassify to infra).
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, &enoent, &closure),
            "ENOENT on closure input must reclassify"
        );

        // I-178b regression: literal cluster output. ANSI SGR escapes
        // around `error:` and the quoted path; the path is the OVERLAY
        // path (`/var/rio/overlays/<build_id>/nix/store/<basename>`),
        // not the bare store path. Basename match must catch it.
        let ansi_overlay = "\u{1b}[31;1merror:\u{1b}[0m\n       \
             … while setting up the build environment\n\n       \
             \u{1b}[31;1merror:\u{1b}[0m getting attributes of path \
             '\u{1b}[35;1m/var/rio/overlays/\
             vwb2lprckpd4kbg67sczakiqqqd4jxzy-llvm-tblgen-src-21_1_8_drv\
             /nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source\u{1b}[0m': \
             \u{1b}[35;1mNo such file or directory\u{1b}[0m";
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, ansi_overlay, &closure),
            "I-178b: ANSI-wrapped overlay path must reclassify by basename"
        );

        // I-179 coupling: EIO suffix (not ENOENT) is also a
        // materialization failure — the matcher keys on the prefix +
        // path, not the strerror suffix.
        let eio = format!("getting attributes of path '{input}': Input/output error");
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, &eio, &closure),
            "EIO on closure input must reclassify (I-179 wait_for_fetcher)"
        );

        // I-189: nix's stat() helper says "getting status of" (not
        // "getting attributes of path"). Literal cluster output:
        // `getting status of '<overlay>/nix/store/<basename>': EIO`.
        let stat_eio = "while setting up the build environment: getting status of \
             '/var/rio/overlays/jrk1q0f3isaddmfgawh7k391fzsa0mc9-glibc_drv\
             /nix/store/54f75pjisgz20ql6azwmck1v779xs0a9-source': \
             Input/output error";
        assert!(
            is_input_materialization_failure(Nix::MiscFailure, stat_eio, &closure),
            "I-189: 'getting status of' phrasing must reclassify"
        );
        for marker in ["reading directory '", "opening file '"] {
            let msg = format!("{marker}{input}': Input/output error");
            assert!(
                is_input_materialization_failure(Nix::MiscFailure, &msg, &closure),
                "marker {marker:?} must reclassify"
            );
        }

        // MiscFailure + path NOT in closure → false (genuine missing
        // dep — leave as PermanentFailure).
        let foreign = "getting attributes of path \
                       '/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-ghost': \
                       No such file or directory";
        assert!(
            !is_input_materialization_failure(Nix::MiscFailure, foreign, &closure),
            "ENOENT on non-closure path must NOT reclassify"
        );

        // Non-MiscFailure status + matching path → false (status guard).
        assert!(
            !is_input_materialization_failure(Nix::PermanentFailure, &enoent, &closure),
            "status guard: only MiscFailure is reclassified"
        );

        // MiscFailure + unrelated message → false.
        assert!(
            !is_input_materialization_failure(
                Nix::MiscFailure,
                "builder for '/nix/store/...-foo.drv' failed with exit code 1",
                &closure,
            ),
            "unrelated MiscFailure message must NOT reclassify"
        );

        // Empty closure → false (vacuous membership).
        assert!(!is_input_materialization_failure(
            Nix::MiscFailure,
            &enoent,
            &[]
        ));
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
        use rio_proto::build_types::BuildResultStatus as Proto;

        let mapped = nix_failure_to_proto(Nix::TimedOut);
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
