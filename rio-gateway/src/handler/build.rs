//! Build event → STDERR translation and build opcode handlers.

use std::collections::{BTreeSet, HashMap, HashSet};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::Derivation;
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::derived_path::{DerivedPath, OutputSpec};
use rio_nix::protocol::stderr::{
    ActivityType, ResultField, ResultType, StderrError, StderrWriter, verbosity,
};
use rio_nix::protocol::wire;
use rio_nix::store_path::StorePath;
use rio_proto::{SchedulerServiceClient, StoreServiceClient, types};
use tokio::io::{AsyncRead, AsyncWrite};
use tonic::transport::Channel;
use tracing::{debug, instrument, warn};

use super::grpc::{grpc_is_valid_path, resolve_floating_outputs};
use super::{GatewayError, PROGRAM_NAME, SessionContext, with_jwt};
use crate::drv_cache::resolve_derivation;
use crate::quota::{QuotaCache, QuotaVerdict, human_bytes};
use crate::translate;

/// Error from `process_build_events`. Distinguishes transport
/// errors and stream EOF-without-terminal (both reconnect-worthy
/// failover signatures — see the per-variant docs) from `Wire`
/// (client-side disconnect — not retried).
#[derive(Debug, thiserror::Error)]
enum StreamProcessError {
    /// gRPC-level error (connection reset, timeout). Scheduler
    /// may have failed over — reconnecting via WatchBuild has
    /// a good chance of resuming.
    #[error("build event stream error: {0}")]
    Transport(tonic::Status),
    /// Stream returned `Ok(None)` (EOF) without a Completed/
    /// Failed/Cancelled event. This IS the primary failover
    /// signature: k8s pod kill → SIGTERM → graceful shutdown →
    /// TCP FIN → clean stream close. NOT a Transport error.
    /// Reconnect-worthy for the same reason as Transport.
    #[error("build event stream ended unexpectedly (scheduler disconnected?)")]
    EofWithoutTerminal,
    /// Error writing STDERR to the client (WireError). The Nix
    /// client disconnected or the SSH channel closed. NOT
    /// reconnect-worthy — scheduler is fine, client is gone.
    #[error("client disconnected: {0}")]
    Wire(#[from] rio_nix::protocol::wire::WireError),
}

// r[impl gw.reject.build-mode]
/// Read `build_mode` from the wire and reject anything other than `Normal`.
///
/// rio does not implement Repair (force-rebuild a corrupted path) or Check
/// (rebuild-and-diff for reproducibility) — `SubmitBuildRequest` has no mode
/// field. Erroring is correct: a silent Normal build gives the user a
/// false-positive determinism/repair result (`nix build --rebuild` would
/// always "pass").
///
/// Called at the exact wire position of the `build_mode` field for each of
/// the three build opcodes — DO NOT reorder relative to other reads.
async fn read_build_mode_normal_only<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    opcode: &str,
) -> anyhow::Result<()> {
    let raw = wire::read_u64(reader).await?;
    match BuildMode::try_from(raw) {
        Ok(BuildMode::Normal) => Ok(()),
        Ok(mode) => {
            stderr_err!(
                stderr,
                "{opcode}: rio-gateway does not support build mode {mode:?} (--repair/--rebuild); use a local store"
            );
        }
        Err(_) => {
            stderr_err!(stderr, "{opcode}: unsupported build mode {raw}");
        }
    }
}

/// Check the per-tenant rate limit before `SubmitBuild`. On violation:
/// sends `STDERR_ERROR` with a wait-hint and returns `Ok(true)` so the
/// caller short-circuits. On pass: returns `Ok(false)`.
///
/// The bool-return shape (instead of `Result`) avoids an error type
/// for what is a normal rate-limited outcome — the SSH connection
/// stays open, the client retries after the hinted delay. The session
/// loop in `session.rs` continues to the next opcode.
///
/// `STDERR_ERROR` is the terminal frame for THIS OPCODE, not the
/// session. The Nix client's `processStderr()` returns the error up
/// to the calling code (e.g., `nix build` prints it and exits 1), but
/// the SSH channel stays open for a retry.
#[instrument(skip_all, fields(tenant = tenant_name.map(|n| n.as_str())))]
async fn rate_limit_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    limiter: &crate::ratelimit::TenantLimiter,
    tenant_name: Option<&NormalizedName>,
) -> anyhow::Result<bool> {
    // `NormalizedName` derefs to str; `Option::map(Deref)` gives
    // `Option<&str>` which is what the limiter takes. `None` (single-
    // tenant mode) and `Some("")` both bucket under `__anon__` there,
    // but the `Some("")` case is unreachable now: `NormalizedName`
    // can't be empty by construction.
    match limiter.check(tenant_name.map(|n| n.as_str())) {
        Ok(()) => Ok(false),
        Err(wait) => {
            // Display string for the error message. `None` →
            // single-tenant mode → "anon". `Some(n)` is guaranteed
            // non-empty/trimmed so the name goes straight in.
            let tenant_disp = tenant_name.map(|n| n.as_str()).unwrap_or("anon");
            // Round up: 0.3s → "~1s". The wait is an advisory hint, not
            // an exact contract — a client that retries exactly at the
            // hinted second may still be a hair early.
            let secs = wait.as_secs().max(1);
            warn!(
                tenant = %tenant_disp,
                wait_secs = secs,
                "rate limit: rejecting build submit"
            );
            metrics::counter!("rio_gateway_errors_total", "type" => "rate_limited").increment(1);
            stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!(
                        "rate limit: too many builds from tenant '{tenant_disp}' — retry in ~{secs}s"
                    ),
                ))
                .await?;
            Ok(true)
        }
    }
}

// r[impl store.gc.tenant-quota-enforce]
/// Check the per-tenant store quota before `SubmitBuild`. Sibling to
/// [`rate_limit_check`]: same `STDERR_ERROR`+early-return shape, same
/// connection-stays-open semantics so the user can GC and retry
/// without reconnecting.
///
/// Quota is eventually-enforcing. `tenant_store_bytes` is cached
/// 30s in `quota_cache`; a few MB of race-window overflow is
/// acceptable per the spec. MVP: rejects on CURRENT overflow only
/// (`used > limit`), not predictive (`estimated_new_bytes = 0`).
///
/// `store_client` is the RPC fallback on cache miss/stale. Fail-open:
/// a transient store error logs + returns `Ok(false)` — quota is a
/// resource gate, not a security gate; a stuck store shouldn't
/// deadlock the build pipeline.
#[instrument(skip_all, fields(tenant = tenant_name.map(|n| n.as_str())))]
async fn quota_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    quota_cache: &QuotaCache,
    store_client: &mut StoreServiceClient<Channel>,
    tenant_name: Option<&NormalizedName>,
    jwt_token: Option<&str>,
) -> anyhow::Result<bool> {
    match quota_cache
        .check(store_client, tenant_name, jwt_token)
        .await
    {
        QuotaVerdict::Under { .. } | QuotaVerdict::Unlimited => Ok(false),
        QuotaVerdict::Over { used, limit } => {
            // `Over` is unreachable for `None` (quota_cache.check
            // early-returns Unlimited on None), but the display
            // branch stays defensive: "anon" if it ever fires.
            let tenant_disp = tenant_name.map(|n| n.as_str()).unwrap_or("anon");
            warn!(
                tenant = %tenant_disp,
                used_bytes = used,
                limit_bytes = limit,
                "quota: rejecting build submit (over gc_max_store_bytes)"
            );
            metrics::counter!(
                "rio_gateway_quota_rejections_total",
                "tenant" => tenant_disp.to_string()
            )
            .increment(1);
            stderr
                .error(&StderrError::simple(
                    "rio-gateway",
                    format!(
                        "tenant '{tenant_disp}' over store quota: {} / {} — \
                         run `nix store gc` or request a quota increase",
                        human_bytes(used),
                        human_bytes(limit)
                    ),
                ))
                .await?;
            Ok(true)
        }
    }
}

/// Run the declared-output-path binding gate
/// ([`translate::validate_output_path_bindings`]) on the blocking
/// pool. The gate is O(total .drv bytes) of ATerm serialization +
/// SHA-256 — far too much to run on the session reactor task — so the
/// drv cache is `mem::take`n (O(1), no clone of a potentially-GB map),
/// moved into `spawn_blocking` together with the nodes, and restored
/// afterwards.
///
/// On a blocking-pool panic (`JoinError`) the cache is left empty:
/// degraded but correct — the session re-fetches .drvs from the store
/// on demand. The verdict is returned separately from the transport
/// error so both call sites keep their existing rejection delivery
/// (BuildResult `InputRejected` for wopBuildDerivation /
/// wopBuildPathsWithResults, `stderr_err!` for wopBuildPaths).
async fn enforce_output_path_bindings(
    nodes: Vec<types::DerivationNode>,
    drv_cache: &mut HashMap<StorePath, Derivation>,
) -> anyhow::Result<(Vec<types::DerivationNode>, Result<(), String>)> {
    let cache = std::mem::take(drv_cache);
    let (nodes, cache, verdict) = tokio::task::spawn_blocking(move || {
        let verdict = translate::validate_output_path_bindings(&nodes, &cache);
        (nodes, cache, verdict)
    })
    .await?;
    *drv_cache = cache;
    Ok((nodes, verdict))
}

/// STDERR-activity state that survives `process_build_events`
/// reconnects. Hoisted to `submit_and_process_build` so a WatchBuild
/// resume after scheduler failover keeps the activity-ID map intact —
/// otherwise the gateway can't `stop_activity` derivations whose
/// `Started` arrived on the previous stream, and nom shows them as
/// stuck forever.
/// Paired activity IDs for one derivation's upstream substitute. The
/// child `actCopyPath` carries `resProgress` (download bytes) so nom
/// renders a bar; the parent `actSubstitute` is what nom shows in its
/// "substituting" line. Both start on `Substituting` and stop together
/// (child first) on the terminal `Cached`/`Started`/`Completed`/`Failed`.
#[derive(Debug, Clone, Copy)]
struct SubstAids {
    subst: u64,
    copy: u64,
}

struct BuildActivityState {
    /// Per-derivation activity IDs (for `actBuild` start/stop and for
    /// attaching `BuildLogLine`/`SetPhase` results to the right build).
    drv: HashMap<String, u64>,
    /// Per-derivation `actSubstitute` + child `actCopyPath` activity
    /// IDs. Started by `DerivationEventKind::Substituting`, stopped by
    /// `Cached` (success) or `Started` (fetch failed → fell through to
    /// build). r[gw.activity.subst-progress]
    subst: HashMap<String, SubstAids>,
    /// Top-level `actBuilds` activity ID. `None` until `BuildStarted`
    /// arrives. `Progress`/`SetExpected` results attach here.
    builds_root: Option<u64>,
    /// Running count of `actCopyPath` activities started. Re-emitted as
    /// `SetExpected{actCopyPath, N}` on the root each time it
    /// increments so nom's "X/Y copied" denominator tracks. The
    /// scheduler doesn't know upfront how many drvs will go
    /// `Substituting` (it's discovered as the DAG runs), so this grows
    /// monotonically rather than being set once.
    subst_expected: u64,
    /// Stable cluster identifier emitted as `actBuild` field 1
    /// (`machineName`). NOT per-pod — see the comment at the
    /// `Status::Started` arm. Read once from `RIO_GATEWAY_MACHINE_NAME`.
    machine_name: String,
}

impl Default for BuildActivityState {
    fn default() -> Self {
        Self {
            drv: HashMap::default(),
            subst: HashMap::default(),
            builds_root: None,
            subst_expected: 0,
            machine_name: rio_common::config::env_or("RIO_GATEWAY_MACHINE_NAME", String::new()),
        }
    }
}

/// Stop both halves of a [`SubstAids`] pair, child (`actCopyPath`)
/// before parent (`actSubstitute`). Factored out of the four
/// `relay_derivation_status` arms that close a substitute on terminal.
async fn stop_subst_pair<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    aids: SubstAids,
) -> Result<(), StreamProcessError> {
    stderr.stop_activity(aids.copy).await?;
    stderr.stop_activity(aids.subst).await?;
    Ok(())
}

// r[impl gw.stderr.result.build-log-line]
/// Relay one `LogBatch` to the client. Lines attach to the per-drv
/// activity as `STDERR_RESULT{aid, BuildLogLine, [line]}` so nom and
/// `--log-format bar` show the last line under the owning build;
/// fallback to `STDERR_NEXT` when no activity exists for this drv
/// (logs arriving before `Derivation::Started`, or gateway-originated
/// diagnostics like the `trace_id` line).
async fn relay_log_batch<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &BuildActivityState,
    log_batch: types::BuildLogBatch,
) -> Result<(), StreamProcessError> {
    let aid = act.drv.get(&log_batch.derivation_path).copied();
    for line in &log_batch.lines {
        // Log display, not parse-path. Build log lines are
        // arbitrary builder output (may contain invalid UTF-8
        // from whatever the build script printed); lossy
        // replacement is the correct behavior for display.
        #[allow(clippy::disallowed_methods)]
        let text = String::from_utf8_lossy(line);
        match aid {
            Some(aid) => {
                stderr
                    .result(
                        aid,
                        ResultType::BuildLogLine,
                        &[ResultField::String(text.into_owned())],
                    )
                    .await?;
            }
            None => stderr.log(&text).await?,
        }
    }
    Ok(())
}

// r[impl gw.stderr.activity+2]
/// Relay one `DerivationEvent` (Started/Completed/Failed/Cached/Queued)
/// to the client as `actBuild` start/stop activity frames. Mutates
/// `act.drv` to track which activity-id belongs to which derivation
/// across reconnects.
async fn relay_derivation_status<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    drv_event: types::DerivationEvent,
) -> Result<(), StreamProcessError> {
    match drv_event.kind() {
        types::DerivationEventKind::Substituting => {
            if act.subst.contains_key(&drv_event.derivation_path) {
                return Ok(());
            }
            // actSubstitute fields per upstream
            // `substitution-goal.cc`: [storePath, substituterUri].
            // The store picks the upstream — the scheduler doesn't
            // know which yet (S2's first SubstituteProgress carries
            // it), so the URI starts empty (nom omits the "from
            // <uri>" suffix when fields[1] is empty).
            let out = drv_event
                .output_paths
                .first()
                .cloned()
                .unwrap_or_else(|| drv_event.derivation_path.clone());
            let subst = stderr
                .start_activity(
                    ActivityType::Substitute,
                    &format!("substituting '{out}'"),
                    verbosity::INFO,
                    act.builds_root.unwrap_or(0),
                    &[
                        ResultField::String(out.clone()),
                        ResultField::String(String::new()),
                    ],
                )
                .await?;
            // r[impl gw.activity.subst-progress]
            // Child actCopyPath fields per upstream
            // `store-api.cc copyStorePath`: [storePath, from, to].
            // `to` is the cluster's stable identifier (same source as
            // actBuild machineName); `from` is empty until the first
            // SubstituteProgress arrives — nom renders the bar from
            // resProgress alone, the from/to are cosmetic. The text
            // says "fetching closure of" (not "copying path") because
            // resProgress carries CLOSURE-aggregate bytes from
            // `walk_substitute_closure`, not single-path bytes.
            let copy = stderr
                .start_activity(
                    ActivityType::CopyPath,
                    &format!("fetching closure of '{out}'"),
                    verbosity::INFO,
                    subst,
                    &[
                        ResultField::String(out.clone()),
                        ResultField::String(String::new()),
                        ResultField::String(act.machine_name.clone()),
                    ],
                )
                .await?;
            act.subst
                .insert(drv_event.derivation_path.clone(), SubstAids { subst, copy });
            // Bump the root's CopyPath expected so nom's "X/Y copied"
            // denominator tracks. Idempotent across reconnects: the
            // contains_key guard above means we only count first-time
            // Substituting per drv per gateway-session.
            act.subst_expected += 1;
            if let Some(root) = act.builds_root {
                stderr
                    .result(
                        root,
                        ResultType::SetExpected,
                        &[
                            ResultField::Int(ActivityType::CopyPath as u64),
                            ResultField::Int(act.subst_expected),
                        ],
                    )
                    .await?;
            }
        }
        types::DerivationEventKind::Started => {
            // A failed substitute fetch reverts to Ready → may later
            // dispatch as a build. Close the dangling actSubstitute +
            // actCopyPath pair so nom doesn't show it stuck forever.
            if let Some(aids) = act.subst.remove(&drv_event.derivation_path) {
                stop_subst_pair(stderr, aids).await?;
            }
            // Re-dispatch (in-connection reassign, or replay after a
            // gateway↔scheduler reconnect) sends Started again for a
            // drv we already track. The existing aid is still valid on
            // the client (client→gateway never dropped), so reuse it.
            // Emitting a fresh start_activity makes nom count the
            // re-dispatch as a new build (live QA: 27 unique drvs →
            // 43 starts → "43/29"). I-206's prior start-then-stop
            // balanced the running count but still inflated total.
            if act.drv.contains_key(&drv_event.derivation_path) {
                debug!(
                    drv = %drv_event.derivation_path,
                    "duplicate Started; reusing existing activity"
                );
                return Ok(());
            }
            // actBuild fields per upstream
            // `derivation-building-goal.cc`:
            // [drvPath, machineName, curRound, nrRounds].
            // nom reads fields[0] for the drv name and
            // fields[1] for the "on <machine>" suffix;
            // rounds are fixed (1,1) — rio doesn't repeat.
            //
            // machineName: NOT the executor pod ID — that
            // leaks ephemeral pod names to the client and
            // breaks the "cluster is one machine" abstraction
            // (a build with 200 pods would show 200 machine
            // names cycling). Use a stable cluster identifier
            // from RIO_GATEWAY_MACHINE_NAME (helm sets it to
            // the cluster name); empty default = upstream's
            // local-build semantics (nom shows no suffix).
            // `drv_event.executor_id` intentionally unused — see above.
            let aid = stderr
                .start_activity(
                    ActivityType::Build,
                    &format!("building '{}'", drv_event.derivation_path),
                    verbosity::INFO,
                    act.builds_root.unwrap_or(0),
                    &[
                        ResultField::String(drv_event.derivation_path.clone()),
                        ResultField::String(act.machine_name.clone()),
                        ResultField::Int(1),
                        ResultField::Int(1),
                    ],
                )
                .await?;
            act.drv.insert(drv_event.derivation_path.clone(), aid);
        }
        types::DerivationEventKind::Completed => {
            // Terminal: close any dangling actSubstitute + actCopyPath.
            // Substituting → Completed shouldn't happen via the normal
            // scheduler FSM, but terminal-arm symmetry costs nothing
            // and guards future scheduler changes.
            if let Some(aids) = act.subst.remove(&drv_event.derivation_path) {
                stop_subst_pair(stderr, aids).await?;
            }
            if let Some(aid) = act.drv.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
                debug!(aid, drv = %drv_event.derivation_path, "stop_activity sent");
            } else {
                // Completed for a drv we never (or no
                // longer) have an aid for — path-key
                // mismatch (dispatch.rs drv_path vs
                // completion.rs path_or_hash_fallback), or
                // Started was dropped by a state-channel
                // Lagged window. Display-only; the
                // act.drv terminal-drain below covers it.
                debug!(
                    drv = %drv_event.derivation_path,
                    tracked = act.drv.len(),
                    "Completed with no tracked activity"
                );
            }
        }
        types::DerivationEventKind::Failed => {
            // Terminal: close any dangling actSubstitute. Scheduler
            // path Substituting → (silent revert to Queued via
            // `handle_substitute_complete(ok=false)` with
            // `!all_deps_completed`) → DependencyFailed cascade emits
            // Failed without ever emitting Started/Cached — the subst
            // aid was never closed and nom showed a stuck
            // "substituting 'X'" line until build terminus.
            if let Some(aids) = act.subst.remove(&drv_event.derivation_path) {
                stop_subst_pair(stderr, aids).await?;
            }
            if let Some(aid) = act.drv.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
            } else {
                debug!(
                    drv = %drv_event.derivation_path,
                    tracked = act.drv.len(),
                    "Failed with no tracked activity (I-206)"
                );
            }
            // Log failure via STDERR_NEXT, with a copy-pasteable
            // `rio-cli logs` hint for drvs that actually ran. No
            // `--build-id` needed — logs are keyed by `(drv_hash,
            // exec_id)` and `rio-cli logs <drv>` resolves the latest
            // execution, which is the one that just failed. The drv
            // path is single-quoted so the line is shell-safe to
            // copy-paste even when the drv name contains shell
            // metacharacters.
            //
            // Gated on `failure_status != DEPENDENCY_FAILED`: a
            // cascaded ancestor never executed (the trigger drv
            // failed first), so there is no log keyed by this drv —
            // `rio-cli logs '<cascaded>'` would resolve to NotFound,
            // or worse, a *prior* build's stale log of the same drv.
            // The trigger drv (the one with the actual log) gets its
            // own `Failed` event with a real `failure_status` and
            // emits the hint. In a `--keep-going` build with N
            // cascaded ancestors, suppressing N misleading hints is
            // the difference between a copy-pasteable failure tail
            // and noise.
            let cascaded =
                drv_event.failure_status == types::BuildResultStatus::DependencyFailed as i32;
            let hint = if cascaded {
                String::new()
            } else {
                format!("\n  ↳ rio-cli logs '{}'", drv_event.derivation_path)
            };
            stderr
                .log(&format!(
                    "derivation '{}' failed: {}{hint}",
                    drv_event.derivation_path, drv_event.error_message
                ))
                .await?;
        }
        types::DerivationEventKind::Cached => {
            // Substituting → Cached: close the actSubstitute +
            // actCopyPath pair. Merge-time cache hits never went
            // Substituting → no aids → no-op.
            if let Some(aids) = act.subst.remove(&drv_event.derivation_path) {
                stop_subst_pair(stderr, aids).await?;
            }
        }
        // Queued: no activity to start/stop, no STDERR.
        types::DerivationEventKind::Queued => {}
    }
    Ok(())
}

// r[impl gw.activity.stop-parity]
/// Emit `stop_activity` for every aid still tracked in `act.drv` /
/// `act.subst`. Called once at build terminus before the root
/// `actBuilds` stop.
///
/// A non-empty drain set means an upstream `DerivationEvent` was lost:
/// scheduler state-channel `Lagged` (rare post-split), a Started/
/// Completed path-key mismatch (`dispatch.rs drv_path` vs `completion.rs
/// path_or_hash_fallback`), or a future bug. Without this the client's
/// nom shows the drv stuck at its last phase forever — the Apr-7
/// large-shallow repro had 44 `start` / 34 `stop` on the wire.
async fn drain_unstopped_activities<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
) -> Result<(), StreamProcessError> {
    if !act.drv.is_empty() || !act.subst.is_empty() {
        tracing::warn!(
            drv = act.drv.len(),
            subst = act.subst.len(),
            "draining unstopped activities at terminal (upstream event loss)"
        );
    }
    for (drv, aids) in act.subst.drain() {
        stderr.stop_activity(aids.copy).await?;
        stderr.stop_activity(aids.subst).await?;
        debug!(subst = aids.subst, copy = aids.copy, %drv,
               "stop_activity sent (terminal drain, subst pair)");
    }
    for (drv, aid) in act.drv.drain() {
        stderr.stop_activity(aid).await?;
        debug!(aid, %drv, "stop_activity sent (terminal drain)");
    }
    Ok(())
}

// r[impl gw.activity.subst-progress]
/// Relay one `SubstituteProgress` event as `resProgress` on the drv's
/// `actCopyPath` child. nom renders `[done/expected]` as a download bar.
/// No-op if no `actSubstitute` pair is tracked for this drv (the
/// `Substituting` state event was lost or arrived after this — both
/// display-only races, harmless).
async fn relay_substitute_progress<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &BuildActivityState,
    p: types::SubstituteProgress,
) -> Result<(), StreamProcessError> {
    let Some(aids) = act.subst.get(&p.derivation_path) else {
        return Ok(());
    };
    // resProgress fields: [done, expected, running, failed]. The latter
    // two are 0 — actCopyPath is single-transfer, not an aggregate.
    stderr
        .result(
            aids.copy,
            ResultType::Progress,
            &[
                ResultField::Int(p.bytes_done),
                ResultField::Int(p.bytes_expected),
                ResultField::Int(0),
                ResultField::Int(0),
            ],
        )
        .await?;
    Ok(())
}

// r[impl gw.stderr.result.progress]
/// Relay `BuildStarted` / `Progress` events to the top-level
/// `actBuilds` activity. `Started` opens the root activity (idempotent
/// across reconnects) and emits `SetExpected{actBuild, N}`; `Progress`
/// emits `resProgress{done, expected, running, failed}`.
async fn relay_build_progress<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    act: &mut BuildActivityState,
    ev: &types::build_event::Event,
) -> Result<(), StreamProcessError> {
    match ev {
        types::build_event::Event::Started(started) => {
            debug!(
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
            // Top-level actBuilds activity. nom and progress-bar
            // aggregate per-drv actBuild children under this; the
            // SetExpected{actBuild, N} result drives the "N/M
            // built" denominator. Idempotent: a second BuildStarted
            // (replayed via WatchBuild after reconnect) re-uses the
            // existing root rather than emitting a duplicate.
            if act.builds_root.is_none() {
                let aid = stderr
                    .start_activity(ActivityType::Builds, "", verbosity::DEBUG, 0, &[])
                    .await?;
                act.builds_root = Some(aid);
            }
            let to_build = u64::from(
                started
                    .total_derivations
                    .saturating_sub(started.cached_derivations),
            );
            if let Some(aid) = act.builds_root {
                stderr
                    .result(
                        aid,
                        ResultType::SetExpected,
                        &[
                            ResultField::Int(ActivityType::Build as u64),
                            ResultField::Int(to_build),
                        ],
                    )
                    .await?;
            }
        }
        types::build_event::Event::Progress(prog) => {
            debug!(
                completed = prog.completed,
                running = prog.running,
                queued = prog.queued,
                total = prog.total,
                "build progress"
            );
            // resProgress fields: [done, expected, running, failed].
            // `expected` is `total` (NOT total-cached): upstream
            // progress-bar takes `max(SetExpected, Progress.expected)`
            // so this is informational.
            if let Some(aid) = act.builds_root {
                stderr
                    .result(
                        aid,
                        ResultType::Progress,
                        &[
                            ResultField::Int(u64::from(prog.completed)),
                            ResultField::Int(u64::from(prog.total)),
                            ResultField::Int(u64::from(prog.running)),
                            ResultField::Int(0),
                        ],
                    )
                    .await?;
            }
        }
        _ => unreachable!("relay_build_progress only handles Started/Progress"),
    }
    Ok(())
}

/// Process a BuildEvent stream from the scheduler and translate events
/// into STDERR protocol messages for the Nix client.
///
/// Returns the final BuildResult on success, or a typed error.
#[instrument(skip_all)]
async fn process_build_events<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    event_stream: &mut tonic::codec::Streaming<types::BuildEvent>,
    active_build_ids: &mut HashMap<String, u64>,
    reconnect_attempts: &mut u32,
    act: &mut BuildActivityState,
) -> Result<BuildEventOutcome, StreamProcessError> {
    while let Some(event) = event_stream
        .message()
        .await
        .map_err(StreamProcessError::Transport)?
    {
        // Reset reconnect counter on first SUCCESSFUL event from
        // the stream (not on WatchBuild Ok() — that only proves
        // the scheduler accepted the RPC, not that the stream is
        // healthy). Without this, a scheduler that accepts but
        // immediately drops the stream would cause an infinite
        // 1s-sleep loop: 0→1→Ok()→reset→0→1→... Matches the
        // controller's reconnect pattern (reset on Ok(Some(ev)),
        // not on stream-open Ok()).
        *reconnect_attempts = 0;

        // Update active_build_ids with latest sequence
        if let Some(seq) = active_build_ids.get_mut(&event.build_id) {
            *seq = event.sequence;
        }

        use types::build_event::Event;
        match event.event {
            Some(Event::Log(log_batch)) => relay_log_batch(stderr, act, log_batch).await?,
            Some(Event::Phase(phase)) => {
                // r[impl gw.stderr.result.set-phase]
                // Builder forwarded the daemon's SetPhase. Attach to the
                // owning per-drv activity so nom shows the phase column.
                if let Some(&aid) = act.drv.get(&phase.derivation_path) {
                    stderr
                        .result(
                            aid,
                            ResultType::SetPhase,
                            &[ResultField::String(phase.phase)],
                        )
                        .await?;
                }
            }
            Some(Event::Derivation(drv_event)) => {
                relay_derivation_status(stderr, act, drv_event).await?;
            }
            Some(Event::SubstituteProgress(p)) => {
                relay_substitute_progress(stderr, act, p).await?;
            }
            Some(ref ev @ (Event::Started(_) | Event::Progress(_))) => {
                relay_build_progress(stderr, act, ev).await?;
            }
            Some(Event::InputsResolved(_)) => {
                // Scheduler's store cache-check done; dispatch begins
                // next. The info to print (N to build = total - cached)
                // arrived in Started above as SetExpected.
                debug!("build inputs resolved");
            }
            Some(Event::Completed(_)) => return Ok(BuildEventOutcome::Completed),
            Some(Event::Failed(failed)) => {
                return Ok(BuildEventOutcome::Failed {
                    status: failed.status(),
                    error_message: failed.error_message,
                });
            }
            Some(Event::Cancelled(cancelled)) => {
                return Ok(BuildEventOutcome::Cancelled {
                    reason: cancelled.reason,
                });
            }
            None => {}
        }
    }

    // Stream ended without a terminal event (scheduler disconnected).
    // Do NOT send STDERR_ERROR here: submit_and_process_build catches this
    // Err and converts it to Ok(BuildResult::failure), which callers then
    // send via STDERR_LAST + BuildResult. Sending STDERR_ERROR here first
    // would produce an invalid STDERR_ERROR -> STDERR_LAST frame sequence.
    //
    // EofWithoutTerminal: clean stream close (Ok(None)). This IS
    // what a scheduler failover looks like — k8s pod kill → SIGTERM
    // → graceful shutdown → TCP FIN. The caller's reconnect loop
    // retries this the same as Transport.
    Err(StreamProcessError::EofWithoutTerminal)
}

/// Outcome of processing a build event stream.
enum BuildEventOutcome {
    Completed,
    Failed {
        status: types::BuildResultStatus,
        error_message: String,
    },
    Cancelled {
        reason: String,
    },
}

/// Issue the `SubmitBuild` RPC and read its initial response metadata
/// (`x-rio-build-id`, `x-rio-trace-id`). Records `build_id` in
/// `active_build_ids` at seq=0 so a stream error before event 0 is
/// reconnectable. Emits the trace-id diagnostic line to the client.
///
/// On `SubmitBuild` failure, logs a `STDERR_NEXT` diagnostic (NOT
/// `STDERR_ERROR` — see comment) before returning the error so callers
/// that convert `Err` → `BuildResult::failure` produce the correct
/// `STDERR_LAST + BuildResult` sequence.
async fn submit_initial<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    request: tonic::Request<types::SubmitBuildRequest>,
    active_build_ids: &mut HashMap<String, u64>,
) -> anyhow::Result<(String, tonic::codec::Streaming<types::BuildEvent>)> {
    let resp = match rio_common::grpc::with_timeout(
        "SubmitBuild",
        rio_common::grpc::SUBMIT_BUILD_TIMEOUT,
        // no-jwt: `request` is `tonic::Request<_>` — caller wraps via
        // with_jwt at the submit_and_process_build entry point.
        scheduler_client.submit_build(request),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            // STDERR_NEXT diagnostic before the Err propagates. Callers
            // map this to BuildResult::failure (opcodes 36/46) or
            // stderr_err! (opcode 9) — the client SEES the failure
            // either way, but without this line the only context is
            // the tonic Status string with no indication it was the
            // INITIAL submit that failed (vs. a mid-stream event, vs.
            // reconnect exhaustion — all three produce anyhow Errs).
            // NOT STDERR_ERROR: two of three callers (opcodes 36/46)
            // convert Err → BuildResult::failure → STDERR_LAST.
            // Sending STDERR_ERROR here would produce the exact
            // ERROR→LAST desync remediation 07 fixes.
            let _ = stderr.log(&format!("SubmitBuild RPC failed: {e}\n")).await;
            return Err(GatewayError::Scheduler(format!("SubmitBuild failed: {e}")).into());
        }
    };

    // build_id from initial response metadata. Scheduler sets this
    // AFTER MergeDag commits (grpc/mod.rs:~480) — if we have it, the
    // build IS durable and WatchBuild can resume it. Reconnect
    // protection is total: even zero stream events is recoverable.
    let header_build_id = resp
        .metadata()
        .get(rio_proto::BUILD_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    // r[impl obs.trace.scheduler-id-in-metadata]
    // x-rio-trace-id: the SCHEDULER handler span's trace_id. Prefer this
    // over our own — the scheduler's #[instrument] span was created
    // before link_parent() ran, so it has its OWN trace_id (LINKED to
    // ours, not parented). That trace extends through worker via the
    // WorkAssignment.traceparent data-carry; ours has only gateway
    // spans. Read BEFORE into_inner() consumes metadata.
    let header_trace_id = resp
        .metadata()
        .get(rio_proto::TRACE_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .filter(|s| !s.is_empty())
        .map(str::to_owned);
    let event_stream = resp.into_inner();

    let build_id = match header_build_id {
        Some(id) => {
            // No event consumed yet. seq=0 is correct —
            // process_build_events updates on every event received
            // (see get_mut + *seq = event.sequence inside the loop).
            // r[impl gw.reconnect.since-seq]
            active_build_ids.insert(id.clone(), 0);
            id
        }
        None => {
            // The scheduler ALWAYS sets this header AFTER MergeDag
            // commits. Absence is a scheduler bug, not a recoverable
            // condition — there is no build_id to WatchBuild against.
            return Err(GatewayError::Scheduler(
                "scheduler did not set x-rio-build-id header".into(),
            )
            .into());
        }
    };
    tracing::Span::current().record("build_id", &build_id);

    // Surface the build_id once per `nix build` via STDERR_NEXT so the
    // user can find their build in the dashboard or `rio-cli builds`.
    // The build_id is no longer load-bearing for `rio-cli logs` — logs
    // are keyed by `(drv_hash, exec_id)`, which the user already has
    // from the failure output and the worker's `rio: exec` log header —
    // but it's still the handle for build tracking, cancellation, and
    // dashboard links. With the header path this fires BEFORE event 0,
    // so the user gets the handle the moment the build is accepted.
    //
    // The trace_id is appended when OTel is wired — gives operators a
    // grep handle for Tempo when debugging a user's build. PRIORITIZE
    // the scheduler's trace_id (x-rio-trace-id header, read above) over
    // our own — the scheduler span is the one that actually spans the
    // full scheduler→worker chain (data-carry per
    // r[sched.trace.assignment-traceparent]). Our own trace only has
    // gateway spans. Fallback to our own for legacy schedulers that
    // don't set the header. Empty trace (no OTel tracer configured —
    // current_trace_id_hex returns "" for TraceId::INVALID, header
    // absent with no OTel on the scheduler side) drops the suffix; the
    // build_id line is still emitted unconditionally.
    let trace_id = header_trace_id.unwrap_or_else(rio_proto::interceptor::current_trace_id_hex);
    let trace_suffix = if trace_id.is_empty() {
        String::new()
    } else {
        format!(" (trace {trace_id})")
    };
    let _ = stderr
        .log(&format!("rio: build {build_id}{trace_suffix}\n"))
        .await;

    Ok((build_id, event_stream))
}

/// Submit a build to the scheduler and process events, returning a BuildResult.
#[instrument(
    skip_all,
    fields(tenant = %request.tenant_name, build_id = tracing::field::Empty)
)]
async fn submit_and_process_build<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    request: types::SubmitBuildRequest,
    active_build_ids: &mut HashMap<String, u64>,
    jwt_token: Option<&str>,
) -> anyhow::Result<BuildResult> {
    // Gateway is the trace ROOT (Nix doesn't speak W3C trace context).
    // with_jwt injects the enclosing span's context + tenant JWT — this
    // is THE hop that makes distributed tracing work; without it,
    // scheduler spans are orphaned root traces.
    let request = with_jwt(request, jwt_token)?;

    let (build_id, mut event_stream) =
        submit_initial(stderr, scheduler_client, request, active_build_ids).await?;

    // Process remaining events, with reconnect on stream error.
    // Scheduler failover/restart drops the stream; we reconnect
    // via WatchBuild(build_id, since_sequence=last_seen) with
    // backoff (1s/2s/4s/8s/16s cap, max 10). The scheduler replays
    // events from build_event_log past that sequence.
    //
    // Without reconnect: scheduler restart mid-build → client's
    // `nix build` fails with MiscFailure even though the build
    // itself completes fine on a worker. With reconnect: client
    // doesn't notice the scheduler blip.
    //
    // 10 attempts = 1+2+4+8+16 + 5×16 = 111s total. 5 attempts
    // (=31s) was too tight: a force-killed leader's REPLACEMENT pod
    // needs ~20-30s (start + lease acquire on 5s
    // tick). Found by vm-le-build-k3s under the replacement-wins-race
    // path — standby-wins was fast enough to mask it.
    // r[impl gw.reconnect.backoff]
    const MAX_RECONNECT: u32 = 10;
    const RECONNECT_BACKOFF: rio_common::backoff::Backoff = rio_common::backoff::Backoff {
        base: std::time::Duration::from_secs(1),
        mult: 2.0,
        cap: std::time::Duration::from_secs(16),
        jitter: rio_common::backoff::Jitter::None,
    };
    let mut reconnect_attempts = 0u32;
    // Activity-ID state survives reconnects so a WatchBuild resume can
    // stop_activity derivations whose Started arrived on the prior
    // stream and keep attaching log lines / phase to the right aid.
    let mut act = BuildActivityState::default();

    let outcome = loop {
        match process_build_events(
            stderr,
            &mut event_stream,
            active_build_ids,
            &mut reconnect_attempts,
            &mut act,
        )
        .await
        {
            Ok(outcome) => break Ok(outcome),
            // Wire error: NOT reconnect-worthy. Client disconnected
            // (SSH closed) — scheduler is fine, there's no one to
            // send the result to. Surface immediately.
            Err(e @ StreamProcessError::Wire(_)) => {
                break Err(e);
            }
            // Transport OR EofWithoutTerminal: both are failover
            // signatures. Transport = RST / tonic connection error.
            // EofWithoutTerminal = scheduler cleanly closed the
            // stream mid-build — THIS is what k8s pod kill looks
            // like (SIGTERM → graceful shutdown → TCP FIN →
            // Ok(None), not Err). vm-le-build-k3s proved the
            // prior "crash = Transport" assumption wrong.
            Err(
                e @ (StreamProcessError::Transport(_) | StreamProcessError::EofWithoutTerminal),
            ) => {
                reconnect_attempts += 1;
                if reconnect_attempts > MAX_RECONNECT {
                    break Err(e);
                }

                // active_build_ids tracks last seq (updated on
                // every event above). 0 if no events arrived yet —
                // WatchBuild with since=0 replays from the start.
                let since_seq = active_build_ids.get(&build_id).copied().unwrap_or(0);

                let backoff = RECONNECT_BACKOFF.duration(reconnect_attempts - 1);
                tracing::warn!(
                    %build_id,
                    error = %e,
                    attempt = reconnect_attempts,
                    since_seq,
                    backoff_secs = backoff.as_secs(),
                    "BuildEvent stream error; reconnecting via WatchBuild"
                );
                // Also surface to the client via STDERR — they see
                // "reconnecting..." instead of a hang.
                let _ = stderr
                    .log(&format!(
                        "scheduler connection lost (attempt {}/{}); reconnecting...",
                        reconnect_attempts, MAX_RECONNECT
                    ))
                    .await;
                tokio::time::sleep(backoff).await;

                // Reconnect: need a fresh scheduler client. The
                // original was moved into this function; we can't
                // easily get the address here. Clone the existing
                // client — tonic clients ARE cheap to clone (Arc
                // internally), and the underlying channel may have
                // auto-reconnected. If that fails (channel dead),
                // WatchBuild will Err and we retry.
                // r[impl gw.jwt.propagate]
                // Reconnect goes through with_jwt like the initial submit
                // at :678 — otherwise the resumed stream's scheduler span
                // is an orphan root trace AND carries no x-rio-tenant-token
                // (hard auth failure once scheduler-side WatchBuild authz
                // lands → every failover burns through MAX_RECONNECT).
                let watch_req = with_jwt(
                    types::WatchBuildRequest {
                        build_id: build_id.clone(),
                        since_sequence: since_seq,
                    },
                    jwt_token,
                )?;
                match scheduler_client.watch_build(watch_req).await {
                    Ok(resp) => {
                        tracing::info!(%build_id, since_seq, "reconnected via WatchBuild");
                        event_stream = resp.into_inner();
                        // DON'T reset reconnect_attempts here —
                        // WatchBuild Ok() only proves the scheduler
                        // accepted the RPC. The stream might error
                        // immediately (scheduler accepts, then drops
                        // — infinite 1s-loop). Reset happens on
                        // FIRST EVENT inside process_build_events.
                        // Loop continues: next process_build_events
                        // reads from the new stream.
                    }
                    Err(wb_err) => {
                        // WatchBuild failed. Could be: scheduler
                        // still down (transient — next loop iter
                        // retries), OR build not found (recovery
                        // didn't reconstruct it — terminal). We
                        // can't distinguish without the error code
                        // check; for simplicity, treat both as
                        // retryable and let MAX_RECONNECT cap it.
                        tracing::warn!(%build_id, error = %wb_err,
                                      "WatchBuild reconnect attempt failed");
                        // Don't break yet — next iteration of the
                        // loop will try process_build_events on the
                        // DEAD stream, which immediately Errs →
                        // another backoff+retry. After MAX we exit.
                    }
                }
            }
        }
    };

    // P0331 trace (Design A — bug confirmed, fix in T2):
    //
    // Unconditional removal here defeats CancelBuild-on-disconnect for
    // mid-opcode client drops. The trace:
    //
    //   1. Client disconnects mid-build → response-task's write-half send
    //      fails → outbound pipe reader drops → next stderr.log write
    //      in process_build_events gets BrokenPipe → WireError
    //   2. :372 breaks with outcome = Err(StreamProcessError::Wire(_))
    //   3. THIS LINE removes build_id — map now empty
    //   4. :474 converts Err → Ok(BuildResult::failure), caller at :881
    //      gets Ok, proceeds to :940 stderr.finish() → BrokenPipe again
    //   5. handle_build_paths_with_results returns Err
    //   6. session.rs:147 `handle_opcode(...)?` — the ? exits run_protocol
    //      directly; the :107 EOF-cancel arm is NEVER reached (that arm
    //      only catches opcode-READ errors at :90, not handler-execution
    //      errors). :138 generic-Err is also opcode-READ-only.
    //   7. server.rs channel_eof/channel_close do NOT run CancelBuild —
    //      channel_close → ChannelSession::Drop → proto_task.abort(),
    //      no cancel logic anywhere.
    //
    // Result: build leaks until r[sched.backstop.timeout+4]. For a 6h
    // nixpkgs build, that's a 6h worker-slot leak per dropped client.
    //
    // Fix is two-part (both needed — step 3 and step 6 compound):
    //   - Guard this remove on !Wire error (keep build_id in map)
    //   - session.rs: run cancel loop on handler-Err too, not just EOF
    //
    // Transport/EofWithoutTerminal errors still remove: scheduler is
    // down, client is alive, cancel would have nowhere to go anyway.
    if !matches!(outcome, Err(StreamProcessError::Wire(_))) {
        active_build_ids.remove(&build_id);
    }

    // Best-effort terminal drain: a Wire error means the client is
    // gone (write would BrokenPipe), and the writer may already be
    // poisoned. nom tolerates unstopped activities (closes on EOF),
    // but stop-parity makes the live display correct.
    if !matches!(outcome, Err(StreamProcessError::Wire(_))) {
        let _ = drain_unstopped_activities(stderr, &mut act).await;
    }

    // Close the top-level actBuilds activity. Best-effort: a Wire
    // error means the client is gone (write would BrokenPipe), and
    // the writer may already be poisoned anyway. nom tolerates an
    // unclosed actBuilds (it closes on EOF).
    if let (Some(aid), false) = (
        act.builds_root,
        matches!(outcome, Err(StreamProcessError::Wire(_))),
    ) {
        let _ = stderr.stop_activity(aid).await;
    }

    match outcome {
        Ok(BuildEventOutcome::Completed) => Ok(BuildResult::success()),
        Ok(BuildEventOutcome::Failed {
            status,
            error_message,
        }) => Ok(BuildResult::failure(status.into(), error_message)),
        Ok(BuildEventOutcome::Cancelled { reason }) => Ok(BuildResult::failure(
            BuildStatus::TransientFailure,
            format!("build cancelled: {reason}"),
        )),
        Err(e) => Ok(BuildResult::failure(
            BuildStatus::TransientFailure,
            format!("build stream error (reconnect exhausted): {e}"),
        )),
    }
}

// r[impl gw.opcode.build-derivation+2]
// r[impl gw.hook.single-node-dag+2]
// r[impl gw.hook.ifd-detection+3]
/// wopBuildDerivation (36): Build a derivation via scheduler.
///
/// Receives an inline BasicDerivation (no inputDrvs). Recovers the full
/// Derivation from drv_cache to reconstruct the DAG.
#[instrument(skip_all, fields(path = tracing::field::Empty))]
pub(super) async fn handle_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let negotiated_version = ctx.negotiated_version;
    let SessionContext {
        store_client,
        scheduler_client,
        drv_cache,
        has_seen_build_paths_with_results,
        active_build_ids,
        tenant_name,
        jwt,
        limiter,
        quota_cache,
        ..
    } = ctx;
    let (drv_path_str, drv_path) = match super::read_store_path(reader).await {
        Ok(v) => v,
        Err(e) => stderr_err!(stderr, "wopBuildDerivation: {e}"),
    };
    tracing::Span::current().record("path", drv_path_str.as_str());
    let Ok(basic_drv) = read_basic_derivation(reader).await else {
        stderr_err!(stderr, "wopBuildDerivation: failed to read BasicDerivation");
    };
    read_build_mode_normal_only(reader, stderr, "wopBuildDerivation").await?;

    debug!(
        path = %drv_path_str,
        platform = %basic_drv.platform(),
        builder = %basic_drv.builder(),
        "wopBuildDerivation"
    );

    let is_ifd_hint = !*has_seen_build_paths_with_results;

    // r[impl gw.reject.nochroot]
    // Check __noChroot on the BasicDerivation DIRECTLY. validate_dag
    // (called below) checks drv_cache entries, but if the full drv
    // isn't available (single-node fallback below), the drv is never
    // in the cache → __noChroot check is skipped. The
    // BasicDerivation wire format DOES include env; we have it here.
    //
    // A malicious client could send __noChroot=1 via wopBuildDerivation
    // (which sends an inline BasicDerivation, not a store path) to
    // escape the sandbox. This catches it at the gateway.
    if translate::StructuredEnv::new(basic_drv.env()).bool("__noChroot") == Some(true) {
        warn!(drv_path = %drv_path_str, "rejecting __noChroot via inline BasicDerivation");
        stderr_err!(
            stderr,
            "derivation requests __noChroot (sandbox escape) — not permitted"
        );
    }

    // r[impl gw.reject.unsupported-hash-algo+2]
    // Same treatment for unverifiable hash algorithms as validate_dag
    // applies to the cached DAG: the builder's FOD hash gate and
    // floating-CA finalization are both fail-closed, so an
    // `outputHashAlgo` they cannot handle — declared with a hash (FOD)
    // or without one (floating-CA) — can only ever fail the build after
    // burning a pod… unless the declared outputs are already realized,
    // in which case the node cache-cuts and never dispatches. The
    // realization probe decides; rejection keeps the existing
    // STDERR_ERROR delivery. The inline BasicDerivation is all we have
    // on the single-node fallback path.
    let mut inline_offender_realized = false;
    if let Some(out) = basic_drv.outputs().iter().find(|o| {
        (!o.hash().is_empty() || !o.hash_algo().is_empty())
            && !translate::fod_algo_verifiable(o.hash_algo())
    }) {
        // The floating-CA shape rule applies to offenders too.
        if let Err(reason) = translate::validate_floating_ca_shape(
            &drv_path_str,
            basic_drv
                .outputs()
                .iter()
                .map(|o| (o.name(), o.path(), o.hash_algo(), o.hash())),
        ) {
            warn!(
                drv_path = %drv_path_str,
                reason = %reason,
                "rejecting inline derivation: floating-CA output declares a path"
            );
            stderr_err!(stderr, "{reason}");
        }
        let offender = translate::UnverifiableFodOffender {
            drv_path: drv_path_str.clone(),
            output_name: out.name().to_string(),
            algo: out.hash_algo().to_string(),
            declared_paths: basic_drv
                .outputs()
                .iter()
                .map(|o| o.path().to_string())
                .collect(),
        };
        if let Err(reason) = translate::reject_unrealized_fod_offenders(
            std::slice::from_ref(&offender),
            store_client,
        )
        .await
        {
            warn!(
                drv_path = %drv_path_str,
                output = out.name(),
                algo = out.hash_algo(),
                "rejecting unverifiable outputHashAlgo via inline BasicDerivation"
            );
            stderr_err!(stderr, "{reason}");
        }
        // All declared outputs are already realized: the node will
        // cache-cut at the scheduler. Skip the declared-hash binding
        // below — the algo cannot be parsed for it, and there is
        // nothing to build from this derivation.
        inline_offender_realized = true;
    }

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    let (nodes, edges) = match &full_drv {
        Ok(drv) => {
            // wopBuildDerivation carries no output selection on the wire
            // — `None` keeps the root's wanted_output_names empty
            // (= every declared output wanted).
            match translate::reconstruct_dag(&drv_path, drv, None, store_client, drv_cache).await {
                Ok((n, e)) => (n, e),
                Err(dag_err) => {
                    // Degrading to a 1-node DAG here is wrong: an
                    // input-addressed root with no inputs would dispatch
                    // and fail on the builder with "input not found".
                    // The errors here (transitive-input cap exceeded,
                    // child .drv resolve failure mid-BFS) are
                    // user-actionable — surface them.
                    warn!(error = %dag_err, "DAG reconstruction failed");
                    stderr_err!(stderr, "cannot build '{drv_path_str}': {dag_err}");
                }
            }
        }
        Err(e) => {
            // r[impl gw.reject.output-path-mismatch]
            // Declared-hash (fixed-output) outputs on the inline
            // BasicDerivation get the same trusted-plane binding the
            // cached path gets via validate_dag: declared path must
            // derive from the declared hash, single-'out' shape rule.
            // Without this, a junk outputHash on an inline derivation
            // would exempt an arbitrary declared path from validation.
            // Skipped only when the realization probe above exempted an
            // unverifiable-algo offender (its algo cannot be parsed and
            // nothing will be built from it).
            if !inline_offender_realized
                && let Err(reason) = translate::validate_declared_hash_outputs(
                    &drv_path_str,
                    basic_drv
                        .outputs()
                        .iter()
                        .map(|o| (o.name(), o.path(), o.hash_algo(), o.hash())),
                )
            {
                warn!(
                    drv_path = %drv_path_str,
                    reason = %reason,
                    "rejecting inline derivation: declared fixed-output path does not bind"
                );
                stderr_err!(stderr, "{reason}");
            }
            // r[impl gw.reject.output-path-mismatch]
            // The precedent for inline checks is the __noChroot check
            // above; this one closes the same bypass for output paths.
            //
            // CppNix's daemon refuses wopBuildDerivation for
            // input-addressed derivations from untrusted clients
            // outright ("you are not privileged to build
            // input-addressed derivations"): with only an inline
            // BasicDerivation there is nothing binding declared
            // input-addressed output paths to the derivation, so
            // accepting them here would let a client squat any
            // not-yet-built store path by never uploading the .drv
            // (the path-binding gate only sees cached full
            // derivations). Mirror that posture fail-closed: the
            // single-node fallback is acceptable only when every
            // statically-declared output is content-bound
            // (fixed-output / floating-CA — their paths are governed
            // by the content-hash rules). Declared paths that are not
            // store paths cannot alias a real store object and keep
            // failing later exactly as before.
            //
            // Conforming clients never hit this for IA derivations:
            // the handshake reports NotTrusted (2), so build-remote in
            // Nix >= 2.16 and Lix copies the .drv closure and drives
            // wopBuildPathsWithResults instead. This branch remains
            // reachable for pre-2.16 hook clients, hand-rolled
            // clients, and replayed old flows — the error text tells
            // them what to do.
            if let Some(out) = basic_drv
                .outputs()
                .iter()
                .find(|o| o.hash_algo().is_empty() && StorePath::parse(o.path()).is_ok())
            {
                warn!(
                    drv_path = %drv_path_str,
                    output = out.name(),
                    declared = out.path(),
                    "rejecting inline input-addressed derivation: full .drv unavailable, \
                     declared output path cannot be validated"
                );
                stderr_err!(
                    stderr,
                    "cannot build '{}': the full derivation is not in the store ({}) and \
                     inline input-addressed derivations cannot be validated — upload the \
                     .drv first (output '{}' declares '{}'). Nix >= 2.16 and Lix handle \
                     this automatically because the gateway reports itself untrusted; \
                     otherwise use --store ssh-ng:// or `nix copy --derivation` before \
                     building",
                    drv_path_str,
                    e,
                    out.name(),
                    out.path()
                );
            }
            // r[impl gw.hook.inline-drv-content]
            // Content-bound fallback: the .drv exists in no store (the
            // client never uploaded it), so the worker can only execute
            // this build if the serialized derivation rides along in
            // the submission. Oversized derivations are rejected with
            // remediation — there is no other delivery path for them.
            debug!(
                error = %e,
                "full derivation not available, using single-node DAG with inline drv_content"
            );
            match translate::build_fallback_node(&drv_path_str, &basic_drv) {
                Ok(mut node) => {
                    // Single-node fallback skips reconstruct_dag (which is
                    // where the BFS root gets flagged), so mark the requested
                    // target here. Behaviour-neutral for a 1-node submission
                    // (it is trivially a structural root) but keeps the
                    // "client named it" marker consistent across opcodes.
                    node.explicitly_requested = true;
                    (vec![node], Vec::new())
                }
                Err(reason) => {
                    warn!(
                        drv_path = %drv_path_str,
                        reason = %reason,
                        "rejecting oversized inline single-node fallback"
                    );
                    stderr_err!(stderr, "{reason}");
                }
            }
        }
    };

    let priority_class = if is_ifd_hint { "interactive" } else { "ci" };

    // Cheap validation BEFORE anything else (no point rate-limiting or
    // hashing a DAG we can reject from the node list alone):
    // __noChroot check + early MAX_DAG_NODES + declared-hash binding;
    // unverifiable-algo offenders come back classified and are decided
    // by the bounded realization probe right after.
    let offenders = match translate::validate_dag(&nodes, drv_cache) {
        Ok(offenders) => offenders,
        Err(reason) => {
            warn!(reason = %reason, "rejecting build: DAG validation failed");
            // Do NOT send STDERR_ERROR here — it is a terminal frame.
            // The client receives the rejection via BuildResult.errorMsg
            // after STDERR_LAST. See build.rs:160-164 for the inverse
            // invariant (STDERR_ERROR → STDERR_LAST is equally invalid).
            let failure = BuildResult::failure(BuildStatus::InputRejected, reason);
            stderr.finish().await?;
            write_build_result(stderr.inner_mut(), &failure, negotiated_version).await?;
            return Ok(());
        }
    };
    // r[impl gw.reject.unsupported-hash-algo+2]
    // Unverifiable-algo offenders are exempt only when every declared
    // output is already realized (the node cache-cuts and never
    // dispatches); otherwise the submission is rejected fail-closed,
    // with the same delivery as a validate_dag rejection.
    if let Err(reason) = translate::reject_unrealized_fod_offenders(&offenders, store_client).await
    {
        warn!(reason = %reason, "rejecting build: unverifiable outputHashAlgo not already realized");
        let failure = BuildResult::failure(BuildStatus::InputRejected, reason);
        stderr.finish().await?;
        write_build_result(stderr.inner_mut(), &failure, negotiated_version).await?;
        return Ok(());
    }

    // Rate limit + quota BEFORE the expensive work (output-path
    // binding, FindMissingPaths/inline, the scheduler RPC + stream).
    // A rate-limited / over-quota client gets STDERR_ERROR and must
    // not be able to make the gateway hash its closure first; the
    // connection stays open.
    if rate_limit_check(stderr, limiter, tenant_name.as_ref()).await? {
        return Ok(());
    }
    if quota_check(
        stderr,
        quota_cache,
        store_client,
        tenant_name.as_ref(),
        jwt.token(),
    )
    .await?
    {
        return Ok(());
    }

    // Declared-output-path binding gate: O(closure) hashing, so it runs
    // on the blocking pool and only after the cheap gates above.
    // Rejection delivery matches the validate_dag rejection: BuildResult
    // InputRejected after STDERR_LAST.
    let (nodes, binding_verdict) = enforce_output_path_bindings(nodes, drv_cache).await?;
    if let Err(reason) = binding_verdict {
        warn!(reason = %reason, "rejecting build: declared output paths do not bind");
        let failure = BuildResult::failure(BuildStatus::InputRejected, reason);
        stderr.finish().await?;
        write_build_result(stderr.inner_mut(), &failure, negotiated_version).await?;
        return Ok(());
    }

    // Inline .drv content for will-dispatch nodes. Mutable because
    // this fills node.drv_content in-place. On store error: skips
    // silently (safe degrade; worker fetches).
    let mut nodes = nodes;
    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    let request =
        translate::build_submit_request(nodes, edges, priority_class, tenant_name.as_ref());

    let mut build_result = match submit_and_process_build(
        stderr,
        scheduler_client,
        request,
        active_build_ids,
        jwt.token(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "build submission failed");
            BuildResult::failure(
                BuildStatus::TransientFailure,
                format!("scheduler error: {e}"),
            )
        }
    };

    // Verify the declared outputs against the store before reporting
    // success (same machinery as opcodes 9/46): builtOutputs come from
    // result_with_wanted_outputs — declared paths for IA/fixed-CA outputs,
    // registered realisations for floating-CA — and a missing or unrealized
    // output demotes the result to an honest failure instead of shipping an
    // empty outPath the client asserts on (nix-build.cc:722). Needs the full
    // Derivation (with inputDrvs) for hash_derivation_modulo; the inline
    // BasicDerivation lacks inputDrvs so the modular hash would diverge from
    // CppNix for non-leaf drvs. If full_drv resolve failed (single-node
    // fallback above), there is nothing to verify against — leave
    // builtOutputs empty, no worse than before, and IA output paths are
    // still recoverable client-side from the BasicDerivation it sent.
    // Store errors during verification abort via stderr_err! inside
    // check_targets_against_store, before stderr.finish().
    // r[impl gw.opcode.build-results-honest+2]
    if build_result.status.is_success()
        && let Ok(drv) = &full_drv
    {
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        // wopBuildDerivation has no per-target output selection — every
        // declared output is wanted.
        let targets = HashMap::from([(
            0usize,
            TargetDemand {
                drv_path: drv_path_str.clone(),
                drv: drv.clone(),
                spec: OutputSpec::All,
            },
        )]);
        let checks = check_targets_against_store(stderr, ctx, &targets, &mut hash_cache).await?;
        if let Some(check) = checks.get(&0) {
            if check.missing.is_empty() {
                build_result = result_with_wanted_outputs(
                    build_result,
                    &targets[&0],
                    check,
                    &ctx.drv_cache,
                    &mut hash_cache,
                );
            } else {
                // Wrong-success: the scheduler says Built but the store does
                // not hold what the client is owed.
                warn!(
                    drv = %drv_path_str,
                    missing = ?check.missing,
                    "demoting successful wopBuildDerivation result: outputs not in store"
                );
                build_result = BuildResult::failure(
                    BuildStatus::MiscFailure,
                    format!(
                        "build completed but requested outputs are not in the store: {}",
                        check.missing.join("; ")
                    ),
                );
            }
        }
    }

    debug!(
        status = ?build_result.status,
        error_msg = %build_result.error_msg,
        "wopBuildDerivation result"
    );

    stderr.finish().await?;
    write_build_result(stderr.inner_mut(), &build_result, negotiated_version).await?;
    Ok(())
}

/// One requested `DerivedPath::Built` target of a `wopBuildPaths` /
/// `wopBuildPathsWithResults` batch: the resolved `.drv` plus the output
/// selection the client sent for it (`!out,dev` / `!*`). Carried from
/// request parsing to result reporting so the per-target store verification
/// knows exactly which outputs the client is owed.
struct TargetDemand {
    drv_path: String,
    drv: Derivation,
    spec: OutputSpec,
}

/// Store-verified view of one target's wanted outputs, produced by
/// [`check_targets_against_store`].
struct TargetOutputsCheck {
    /// Wanted outputs the store confirms are NOT available: missing store
    /// paths, or floating-CA outputs with no realisation. Human-readable,
    /// used verbatim in the client-facing error message.
    missing: Vec<String>,
    /// Positive evidence that EVERY wanted output resolved to a concrete
    /// store path the store reports as present. Required to report a target
    /// successful when the aggregate outcome was a failure — absence of
    /// evidence (an unverifiable path) is not enough to override it.
    confirmed_present: bool,
    /// Output name → realized store path for floating-CA outputs (from the
    /// Realisations table). Reused to build `builtOutputs` without a second
    /// round of store queries.
    realized: HashMap<String, String>,
    /// The wanted output names this target's spec resolves to
    /// (`Names` as sent, `All` → every declared output name).
    wanted_names: Vec<String>,
    /// `(output name, expected store path)` pairs awaiting the batched
    /// FindMissingPaths answer. Drained when the verdict is finalized.
    checkable: Vec<(String, String)>,
    /// At least one wanted output could not be mapped to a queryable store
    /// path (a name the `.drv` does not declare, or a declared path that is
    /// not a parseable store path — the store rejects the whole
    /// FindMissingPaths batch on such a path, and it can never have been
    /// ingested either). Unverifiable outputs never count as missing (the
    /// target defers to the scheduler outcome, exactly the pre-verification
    /// behavior) but they block `confirmed_present`.
    unverifiable: bool,
}

// r[impl gw.opcode.build-results-honest+2]
/// Resolve every target's wanted outputs to concrete store paths and ask the
/// store — ONE batched `FindMissingPaths` over the union, tenant-scoped via
/// the session JWT like `wopQueryValidPaths` — which of them actually exist.
///
/// Expected paths: the declared `.drv` path for input-addressed / fixed-CA
/// outputs, the Realisations row for floating-CA outputs. A wanted
/// floating-CA output with NO realisation is recorded as missing
/// immediately — the alternative (today's behavior before this check) is a
/// "successful" entry whose `builtOutputs` carry an empty `outPath`, which
/// stock clients reject (`Realisation JSON has empty 'outPath'` /
/// nix-build.cc:722 assert).
///
/// Store errors (realisation lookup or FindMissingPaths failure/timeout)
/// abort the whole opcode via `stderr_err!` — the gateway never falls back
/// to trusting the scheduler's word about what is in the store. Callers run
/// this BEFORE `stderr.finish()` (r[gw.stderr.error-before-return+2]).
async fn check_targets_against_store<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
    targets: &HashMap<usize, TargetDemand>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> anyhow::Result<HashMap<usize, TargetOutputsCheck>> {
    let SessionContext {
        store_client,
        drv_cache,
        jwt,
        ..
    } = ctx;

    let mut checks: HashMap<usize, TargetOutputsCheck> = HashMap::new();
    // Deduplicated union of every target's checkable paths — one store
    // round-trip for the whole batch. BTreeSet for a deterministic request.
    let mut to_query: BTreeSet<String> = BTreeSet::new();

    // Deterministic per-target order so log/error output is stable.
    let mut indices: Vec<usize> = targets.keys().copied().collect();
    indices.sort_unstable();

    for idx in indices {
        let demand = &targets[&idx];
        // Floating-CA outputs: declared path is "" and the real path lives
        // in the Realisations table (the scheduler wrote it before emitting
        // Completed). Non-NotFound store errors abort the opcode.
        let (_, realized) = match resolve_floating_outputs(
            &demand.drv,
            &demand.drv_path,
            store_client,
            jwt.token(),
            drv_cache,
            hash_cache,
        )
        .await
        {
            Ok(v) => v,
            Err(e) => stderr_err!(
                stderr,
                "store error querying realisation for {}: {e}",
                demand.drv_path
            ),
        };

        let wanted_names: Vec<String> = match &demand.spec {
            OutputSpec::All => demand
                .drv
                .outputs()
                .iter()
                .map(|o| o.name().to_string())
                .collect(),
            OutputSpec::Names(names) => names.clone(),
        };

        let mut missing = Vec::new();
        let mut checkable = Vec::new();
        let mut unverifiable = false;
        for name in &wanted_names {
            let Some(output) = demand
                .drv
                .outputs()
                .iter()
                .find(|o| o.name() == name.as_str())
            else {
                // Requested output name the .drv does not declare. Nothing
                // to ask the store about — defer to the scheduler outcome.
                unverifiable = true;
                continue;
            };
            let expected = if output.path().is_empty() {
                match realized.get(name) {
                    Some(p) => p.clone(),
                    None => {
                        missing.push(format!(
                            "floating-CA output '{name}' of {} has no realisation",
                            demand.drv_path
                        ));
                        continue;
                    }
                }
            } else {
                output.path().to_string()
            };
            if StorePath::parse(&expected).is_err() {
                unverifiable = true;
                continue;
            }
            to_query.insert(expected.clone());
            checkable.push((name.clone(), expected));
        }

        checks.insert(
            idx,
            TargetOutputsCheck {
                missing,
                confirmed_present: false,
                realized,
                wanted_names,
                checkable,
                unverifiable,
            },
        );
    }

    let missing_set: HashSet<String> = if to_query.is_empty() {
        HashSet::new()
    } else {
        // Tenant-scoped like wopQueryValidPaths: dropping the JWT would
        // bypass the cross-tenant visibility gate and could "verify" a
        // path the requesting tenant cannot actually fetch.
        let req = with_jwt(
            types::FindMissingPathsRequest {
                store_paths: to_query.into_iter().collect(),
            },
            jwt.token(),
        )?;
        match rio_common::grpc::with_timeout(
            "FindMissingPaths",
            super::opcodes_read::GATEWAY_FMP_TIMEOUT,
            store_client.find_missing_paths(req),
        )
        .await
        {
            Ok(r) => r.into_inner().missing_paths.into_iter().collect(),
            Err(e) => stderr_err!(stderr, "store error verifying build outputs: {e}"),
        }
    };

    for check in checks.values_mut() {
        for (name, path) in std::mem::take(&mut check.checkable) {
            if missing_set.contains(&path) {
                check.missing.push(format!(
                    "output '{name}' ({path}) is not valid in the store"
                ));
            }
        }
        // Promotion over a failed aggregate needs positive evidence for
        // every wanted output; an empty wanted set or any unverifiable
        // output leaves the scheduler outcome authoritative.
        check.confirmed_present =
            check.missing.is_empty() && !check.unverifiable && !check.wanted_names.is_empty();
    }

    Ok(checks)
}

// r[impl gw.opcode.build-results-honest+2]
/// Build the success-side `BuildResult` for ONE verified target: status and
/// timing from `base`, `builtOutputs` covering exactly the wanted outputs
/// (drvHashModulo ids; floating-CA paths from the realisations resolved
/// during verification — no further store I/O). If the modular hash is
/// uncomputable (already `warn!`-logged) the result is returned without
/// builtOutputs — the same degrade the pre-verification enrichment had.
fn result_with_wanted_outputs(
    base: BuildResult,
    demand: &TargetDemand,
    check: &TargetOutputsCheck,
    drv_cache: &HashMap<StorePath, Derivation>,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> BuildResult {
    let Some(hash) = translate::compute_modular_hash_cached(
        &demand.drv,
        &demand.drv_path,
        drv_cache,
        hash_cache,
    ) else {
        return base;
    };
    let hash_hex = hex::encode(hash);
    let mut result = base.with_outputs_from_drv(&demand.drv, &hash_hex, &check.realized);
    // builtOutputs honesty: cover exactly the outputs the client asked for.
    // Membership is decided on the DrvOutput ids ("sha256:<hash>!<name>")
    // rather than re-parsing names back out of them.
    let wanted_ids: HashSet<String> = check
        .wanted_names
        .iter()
        .map(|n| format!("sha256:{hash_hex}!{n}"))
        .collect();
    result
        .built_outputs
        .retain(|o| wanted_ids.contains(&o.drv_output_id));
    result
}

/// Dedup DAG nodes by `drv_path` and edges by `(parent, child)`.
///
/// Multi-root builds with shared deps walk the shared subgraph once per
/// root, producing duplicate nodes/edges. The scheduler tolerates dups
/// (MergeDag is idempotent; `derivation_edges` PK is `(parent,child)` so
/// dups are `ON CONFLICT DO NOTHING`) but they waste bytes + PG RTTs.
///
/// Duplicates are identical EXCEPT for the demand fields —
/// `wanted_output_names` and `explicitly_requested` are computed from
/// the *request target / consumers reachable in one root's BFS* rather
/// than from the `.drv` itself, so each root's copy carries only that
/// root's demand. Retain-first must therefore UNION the dropped
/// duplicate's wanted set into the retained node: dropping it un-wants
/// whatever the other root asked for (root A wants `child^out`,
/// root B wants `child^dev` — keeping only A's copy would let the
/// scheduler treat a missing `dev` output as ignorable). The union is
/// [`rio_common::wanted_outputs::union_wanted_saturating`] — the same
/// saturating union (empty = "all declared outputs wanted", all ∪ X =
/// all) that `DerivationState::union_wanted` and the PG upsert's
/// union-on-conflict apply when the duplicates arrive as separate
/// submissions instead. `explicitly_requested` is ORed for the same
/// reason: a target requested in ANY copy stays a requested target
/// (this is exactly the copy that records "the client also named this
/// dependency as a target of its own").
fn dedup_dag(nodes: &mut Vec<types::DerivationNode>, edges: &mut Vec<types::DerivationEdge>) {
    let mut keep: HashMap<String, usize> = HashMap::new();
    let mut deduped: Vec<types::DerivationNode> = Vec::with_capacity(nodes.len());
    for node in nodes.drain(..) {
        match keep.entry(node.drv_path.clone()) {
            std::collections::hash_map::Entry::Vacant(e) => {
                e.insert(deduped.len());
                deduped.push(node);
            }
            std::collections::hash_map::Entry::Occupied(e) => {
                let kept = &mut deduped[*e.get()];
                rio_common::wanted_outputs::union_wanted_saturating(
                    &mut kept.wanted_output_names,
                    &node.wanted_output_names,
                );
                kept.explicitly_requested |= node.explicitly_requested;
            }
        }
    }
    *nodes = deduped;
    let mut seen_edges = HashSet::new();
    edges.retain(|e| seen_edges.insert((e.parent_drv_path.clone(), e.child_drv_path.clone())));
}

/// Outcome of [`submit_dag`] — the shared DAG-submit pipeline for
/// `wopBuildPaths` and `wopBuildPathsWithResults`.
enum DagSubmitOutcome {
    /// Rate-limited or quota-exceeded. `STDERR_ERROR` already sent by
    /// the respective check; caller should `return Ok(())`.
    Gated,
    /// `validate_dag` or the output-path binding gate rejected the DAG
    /// before submission. No `STDERR_ERROR` sent — caller decides
    /// whether to surface as `stderr_err!` (wopBuildPaths) or as a
    /// per-path `BuildResult::failure(InputRejected, …)`
    /// (wopBuildPathsWithResults).
    Rejected(String),
    /// Build was submitted and the scheduler returned a result
    /// (success OR failure — caller inspects `.status`).
    Built(BuildResult),
}

/// Shared DAG-submit pipeline:
/// `dedup → validate (cheap) → realization probe (offenders only) →
/// rate-limit → quota → output-path binding (blocking pool) →
/// inline-drv → SubmitBuild`.
///
/// Runs every gate between DAG reconstruction and the scheduler RPC.
/// Gate ORDER is fixed here so the two build-paths opcodes cannot drift:
/// cheap validation first (no I/O, no hashing), then the bounded
/// unverifiable-algo realization probe (a single `FindMissingPaths`,
/// fired only when `validate_dag` classified offenders — same I/O
/// class as the GetPath BFS both flows already performed), then
/// rate/quota (may send `STDERR_ERROR`) so a limited client cannot
/// trigger the expensive work, then the declared-output-path binding
/// gate on the blocking pool, then inline (store I/O), then submit.
/// `handle_build_derivation` follows the same order.
///
/// Returns `Err` only when `submit_and_process_build` itself errors
/// (scheduler transport/timeout); caller decides whether that is
/// session-terminal (`stderr_err!`) or a per-path `TransientFailure`.
///
/// `priority_class` is decided by the caller: `wopBuildPaths` is always
/// `"ci"`, `wopBuildPathsWithResults` passes `"interactive"` for the
/// hook-shaped first request of a session (`gw.hook.ifd-detection+3`).
async fn submit_dag<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
    mut nodes: Vec<types::DerivationNode>,
    mut edges: Vec<types::DerivationEdge>,
    priority_class: &'static str,
) -> anyhow::Result<DagSubmitOutcome> {
    let SessionContext {
        store_client,
        scheduler_client,
        drv_cache,
        active_build_ids,
        tenant_name,
        jwt,
        limiter,
        quota_cache,
        ..
    } = ctx;

    dedup_dag(&mut nodes, &mut edges);

    let offenders = match translate::validate_dag(&nodes, drv_cache) {
        Ok(offenders) => offenders,
        Err(reason) => {
            warn!(reason = %reason, "rejecting build: DAG validation failed");
            return Ok(DagSubmitOutcome::Rejected(reason));
        }
    };
    // r[impl gw.reject.unsupported-hash-algo+2]
    // Unverifiable-algo offenders: exempt only when already realized
    // (single bounded FindMissingPaths probe, fail-closed).
    if let Err(reason) = translate::reject_unrealized_fod_offenders(&offenders, store_client).await
    {
        warn!(reason = %reason, "rejecting build: unverifiable outputHashAlgo not already realized");
        return Ok(DagSubmitOutcome::Rejected(reason));
    }

    if rate_limit_check(stderr, limiter, tenant_name.as_ref()).await? {
        return Ok(DagSubmitOutcome::Gated);
    }
    if quota_check(
        stderr,
        quota_cache,
        store_client,
        tenant_name.as_ref(),
        jwt.token(),
    )
    .await?
    {
        return Ok(DagSubmitOutcome::Gated);
    }

    // Declared-output-path binding: expensive (O(closure) hashing), so
    // it sits behind rate-limit/quota and runs on the blocking pool.
    let (returned_nodes, binding_verdict) =
        enforce_output_path_bindings(std::mem::take(&mut nodes), drv_cache).await?;
    nodes = returned_nodes;
    if let Err(reason) = binding_verdict {
        warn!(reason = %reason, "rejecting build: declared output paths do not bind");
        return Ok(DagSubmitOutcome::Rejected(reason));
    }

    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    let request =
        translate::build_submit_request(nodes, edges, priority_class, tenant_name.as_ref());
    let result = submit_and_process_build(
        stderr,
        scheduler_client,
        request,
        active_build_ids,
        jwt.token(),
    )
    .await?;
    Ok(DagSubmitOutcome::Built(result))
}

/// Resolve a `.drv` and reconstruct its full transitive DAG. Shared
/// `DerivedPath::Built` arm of the two `wopBuildPaths*` handlers — both
/// do `resolve_derivation` → `reconstruct_dag` → extend nodes/edges,
/// differing only in error sink (`stderr_err!` abort vs. per-path
/// `BuildResult::failure`).
///
/// `outputs` is the request's `^out,dev` / `^*` selection for THIS
/// root — it seeds the root node's `wanted_output_names` (`^*` keeps
/// the empty all-wanted sentinel).
async fn resolve_built_dag(
    drv: &StorePath,
    outputs: &OutputSpec,
    ctx: &mut SessionContext,
) -> anyhow::Result<(
    Vec<types::DerivationNode>,
    Vec<types::DerivationEdge>,
    Derivation,
)> {
    let drv_obj = resolve_derivation(drv, &mut ctx.store_client, &mut ctx.drv_cache).await?;
    let (nodes, edges) = translate::reconstruct_dag(
        drv,
        &drv_obj,
        Some(outputs),
        &mut ctx.store_client,
        &mut ctx.drv_cache,
    )
    .await?;
    Ok((nodes, edges, drv_obj))
}

// r[impl gw.opcode.build-paths]
/// wopBuildPaths (9): Build a set of derivations.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_build_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let raw_paths = wire::read_strings(reader).await?;
    read_build_mode_normal_only(reader, stderr, "wopBuildPaths").await?;

    tracing::Span::current().record("count", raw_paths.len());
    debug!(count = raw_paths.len(), "wopBuildPaths");

    // Collect all derivation paths and reconstruct a combined DAG
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    // Per-target demand for the post-build store verification — opcode 9
    // returns no per-path results, but its bare success word makes the same
    // promise that every requested output now exists.
    let mut targets: HashMap<usize, TargetDemand> = HashMap::new();

    for (idx, raw) in raw_paths.iter().enumerate() {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => stderr_err!(stderr, "invalid DerivedPath '{raw}': {e}"),
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                match grpc_is_valid_path(&mut ctx.store_client, ctx.jwt.token(), path).await {
                    Ok(true) => { /* exists, fine */ }
                    Ok(false) => {
                        stderr_err!(stderr, "path '{path}' is not valid and cannot be built");
                    }
                    Err(e) => stderr_err!(stderr, "store error: {e}"),
                }
            }
            DerivedPath::Built { drv, outputs } => {
                match resolve_built_dag(drv, outputs, ctx).await {
                    Ok((nodes, edges, drv_obj)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        targets.insert(
                            idx,
                            TargetDemand {
                                drv_path: drv.to_string(),
                                drv: drv_obj,
                                spec: outputs.clone(),
                            },
                        );
                    }
                    Err(e) => stderr_err!(stderr, "DAG reconstruction failed for '{drv}': {e}"),
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        // Remote-store-mode DAG submissions are CI-shaped: the goal DAG
        // arrives via opcode 9/46 with the IFD trigger being a separate
        // wopBuildDerivation — see gw.hook.ifd-detection+3.
        match submit_dag(stderr, ctx, all_nodes, all_edges, "ci").await {
            Ok(DagSubmitOutcome::Gated) => return Ok(()),
            Ok(DagSubmitOutcome::Rejected(reason)) => {
                stderr_err!(stderr, "build rejected: {reason}")
            }
            Ok(DagSubmitOutcome::Built(r)) if !r.status.is_success() => {
                stderr_err!(stderr, "build failed: {}", r.error_msg)
            }
            Ok(DagSubmitOutcome::Built(_)) => {
                // r[impl gw.opcode.build-results-honest+2]
                // The scheduler says the DAG completed; gate the success
                // word on the store actually holding every requested
                // output (same verification as wopBuildPathsWithResults).
                let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
                let checks =
                    check_targets_against_store(stderr, ctx, &targets, &mut hash_cache).await?;
                let mut indices: Vec<usize> = checks.keys().copied().collect();
                indices.sort_unstable();
                let missing: Vec<String> = indices
                    .iter()
                    .filter_map(|i| checks.get(i))
                    .flat_map(|c| c.missing.iter().cloned())
                    .collect();
                if !missing.is_empty() {
                    stderr_err!(
                        stderr,
                        "build completed but requested outputs are not in the store: {}",
                        missing.join("; ")
                    );
                }
            }
            Err(e) => stderr_err!(stderr, "build failed: {e}"),
        }
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

// r[impl gw.opcode.build-paths-with-results]
// r[impl gw.stderr.error-before-return+2]
/// wopBuildPathsWithResults (46): Build paths and return per-path BuildResult.
#[instrument(skip_all, fields(count = tracing::field::Empty))]
pub(super) async fn handle_build_paths_with_results<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    // r[impl gw.hook.ifd-detection+3]
    // Hook-shape classification. Stock build-remote (Nix >= 2.16, Lix)
    // delegates one derivation per SSH session in build-hook mode: it
    // copies the .drv closure and drives a single
    // wopBuildPathsWithResults whose only target is `<drv>!*`
    // (DerivedPath::Built with OutputSpec::All). That request gets the
    // interactive priority class the wopBuildDerivation flow already
    // gets — a developer is blocked on it right now. Everything else
    // stays "ci": opcode 9, named-output targets ("drv!out" — the
    // remote-store-mode CI shape), multi-target or mixed batches, and
    // any second bPWR on the same session (a CI driver pushing many
    // DAGs through one connection). The flag is captured-then-set HERE
    // (not in the dispatcher) so this call can observe whether it is
    // the session's first.
    let first_bpwr_of_session = !ctx.has_seen_build_paths_with_results;
    ctx.has_seen_build_paths_with_results = true;

    let raw_paths = wire::read_strings(reader).await?;
    read_build_mode_normal_only(reader, stderr, "wopBuildPathsWithResults").await?;

    tracing::Span::current().record("count", raw_paths.len());
    debug!(count = raw_paths.len(), "wopBuildPathsWithResults");

    let hook_shaped = raw_paths.len() == 1
        && matches!(
            DerivedPath::parse(&raw_paths[0]),
            Ok(DerivedPath::Built {
                outputs: rio_nix::protocol::derived_path::OutputSpec::All,
                ..
            })
        );
    let priority_class: &'static str = if first_bpwr_of_session && hook_shaped {
        "interactive"
    } else {
        "ci"
    };

    let mut results = Vec::new();

    // Collect all derivation paths to build together
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut opaque_results: HashMap<usize, BuildResult> = HashMap::new();
    // Track idx → TargetDemand (drvPath, Derivation, OutputSpec) for Built
    // paths so the per-target store verification and builtOutputs
    // population after the build know exactly what each entry asked for.
    // Without builtOutputs, the client can't map the derivation to its outputs
    // and falls back to some NAR-based verification → "error: no sink".
    let mut drv_for_idx: HashMap<usize, TargetDemand> = HashMap::new();

    for (idx, raw) in raw_paths.iter().enumerate() {
        let dp = match DerivedPath::parse(raw) {
            Ok(dp) => dp,
            Err(e) => {
                opaque_results.insert(
                    idx,
                    BuildResult::failure(
                        BuildStatus::InputRejected,
                        format!("invalid path '{raw}': {e}"),
                    ),
                );
                continue;
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                let result =
                    match grpc_is_valid_path(&mut ctx.store_client, ctx.jwt.token(), path).await {
                        Ok(true) => BuildResult {
                            status: BuildStatus::AlreadyValid,
                            ..Default::default()
                        },
                        Ok(false) => BuildResult::failure(
                            BuildStatus::NoSubstituters,
                            format!("path '{}' not valid", path),
                        ),
                        Err(e) => BuildResult::failure(
                            BuildStatus::TransientFailure,
                            format!("store error: {e}"),
                        ),
                    };
                opaque_results.insert(idx, result);
            }
            DerivedPath::Built { drv, outputs } => {
                match resolve_built_dag(drv, outputs, ctx).await {
                    Ok((nodes, edges, drv_obj)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        drv_for_idx.insert(
                            idx,
                            TargetDemand {
                                drv_path: drv.to_string(),
                                drv: drv_obj,
                                spec: outputs.clone(),
                            },
                        );
                    }
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, e.to_string()),
                        );
                    }
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        let build_result = match submit_dag(stderr, ctx, all_nodes, all_edges, priority_class).await
        {
            Ok(DagSubmitOutcome::Gated) => return Ok(()),
            Ok(DagSubmitOutcome::Rejected(reason)) => {
                BuildResult::failure(BuildStatus::InputRejected, reason)
            }
            Ok(DagSubmitOutcome::Built(r)) => r,
            Err(e) => {
                warn!(error = %e, "wopBuildPathsWithResults: build submission failed");
                metrics::counter!("rio_gateway_errors_total", "type" => "scheduler_submit")
                    .increment(1);
                BuildResult::failure(
                    BuildStatus::TransientFailure,
                    format!("scheduler error: {e}"),
                )
            }
        };

        // r[impl gw.opcode.build-results-honest+2]
        // Honest per-target results: a target is reported successful only
        // when the store confirms the outputs the client asked for actually
        // exist (defense in depth on top of the scheduler-side guarantees),
        // and a target whose outputs ARE all present is not blanket-failed
        // by an unrelated failure elsewhere in the batch. One batched
        // FindMissingPaths; store errors abort the opcode (stderr_err!
        // inside, before stderr.finish()).
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        let mut checks =
            check_targets_against_store(stderr, ctx, &drv_for_idx, &mut hash_cache).await?;

        for (idx, _raw) in raw_paths.iter().enumerate() {
            if let Some(opaque) = opaque_results.remove(&idx) {
                results.push(opaque);
                continue;
            }
            let (Some(demand), Some(check)) = (drv_for_idx.get(&idx), checks.remove(&idx)) else {
                results.push(build_result.clone());
                continue;
            };
            if build_result.status.is_success() {
                if check.missing.is_empty() {
                    // Verified (or unverifiable — defer to the scheduler):
                    // success, with builtOutputs covering exactly the
                    // wanted outputs.
                    results.push(result_with_wanted_outputs(
                        build_result.clone(),
                        demand,
                        &check,
                        &ctx.drv_cache,
                        &mut hash_cache,
                    ));
                } else {
                    // Wrong-success: the aggregate says Built but this
                    // target's requested outputs are not in the store.
                    warn!(
                        drv = %demand.drv_path,
                        missing = ?check.missing,
                        "demoting successful wopBuildPathsWithResults entry: outputs not in store"
                    );
                    results.push(BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!(
                            "build completed but requested outputs are not in the store: {}",
                            check.missing.join("; ")
                        ),
                    ));
                }
            } else if check.confirmed_present {
                // Partial outcome: the aggregate failed, but every output
                // THIS target asked for is present in the store — report
                // the target honestly as built.
                results.push(result_with_wanted_outputs(
                    BuildResult::success(),
                    demand,
                    &check,
                    &ctx.drv_cache,
                    &mut hash_cache,
                ));
            } else {
                // Unverified target under a failed aggregate: keep the
                // mapped failure status + error message.
                results.push(build_result.clone());
            }
        }
    } else {
        for idx in 0..raw_paths.len() {
            results.push(opaque_results.remove(&idx).unwrap_or_else(|| {
                BuildResult::failure(BuildStatus::MiscFailure, "unknown path".to_string())
            }));
        }
    }

    stderr.finish().await?;
    let w = stderr.inner_mut();

    wire::write_u64(w, results.len() as u64).await?;
    for (raw, result) in raw_paths.iter().zip(results.iter()) {
        wire::write_string(w, raw).await?;
        write_build_result(w, result, ctx.negotiated_version).await?;
    }

    Ok(())
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_nix::protocol::stderr::{STDERR_RESULT, STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};

    fn ev(kind: types::DerivationEventKind, drv: &str, outs: &[&str]) -> types::DerivationEvent {
        types::DerivationEvent {
            derivation_path: drv.into(),
            kind: kind as i32,
            output_paths: outs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
    }

    // r[verify sched.merge.wanted-outputs+2]
    /// Multi-root submissions concatenate per-root node lists, so a drv
    /// reachable from two roots appears twice — each copy carrying the
    /// wanted set computed from THAT root's consumers/spec only.
    /// Retain-first dedup must UNION the dropped duplicate's
    /// `wanted_output_names` into the retained node: dropping it
    /// un-wants whatever the other root demanded (root A wants
    /// `child^out`, root B wants `child^dev` — keeping only A's copy
    /// would let the scheduler treat a missing `dev` as ignorable).
    /// Empty saturates the union (empty = "all declared outputs
    /// wanted", and all ∪ X = all).
    #[test]
    fn dedup_dag_unions_wanted_output_names() {
        let mk = |wanted: &[&str]| types::DerivationNode {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            drv_hash: "x".into(),
            wanted_output_names: wanted.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        };

        // {out} ∪ {dev} = {dev, out} (sorted union).
        let mut nodes = vec![mk(&["out"]), mk(&["dev"])];
        let mut edges = Vec::new();
        dedup_dag(&mut nodes, &mut edges);
        assert_eq!(nodes.len(), 1, "duplicates by drv_path collapse to one");
        assert_eq!(
            nodes[0].wanted_output_names,
            vec!["dev", "out"],
            "retained node must carry the UNION of all duplicates' wanted sets"
        );

        // {out} ∪ {} = {} — the empty (= all-wanted) sentinel saturates.
        let mut nodes = vec![mk(&["out"]), mk(&[])];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "all ∪ {{out}} = all (empty saturates)"
        );

        // {} ∪ {out} = {} — saturation is order-independent.
        let mut nodes = vec![mk(&[]), mk(&["out"])];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(
            nodes[0].wanted_output_names,
            Vec::<String>::new(),
            "{{out}} ∪ all = all (empty saturates regardless of which copy is retained)"
        );

        // Distinct drv_paths are untouched (no spurious cross-node union).
        let mut nodes = vec![
            types::DerivationNode {
                drv_path: "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-y.drv".into(),
                wanted_output_names: vec!["out".into()],
                ..Default::default()
            },
            mk(&["dev"]),
        ];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(nodes.len(), 2);
        assert_eq!(nodes[0].wanted_output_names, vec!["out"]);
        assert_eq!(nodes[1].wanted_output_names, vec!["dev"]);
    }

    /// Retain-first dedup must OR `explicitly_requested` across
    /// duplicates: a node the client named as a build target in ANY
    /// per-target walk stays flagged, no matter which copy is retained.
    /// Dropping the flag would re-expose the requested target to the
    /// scheduler's roots-only prune (it is a non-root of the combined
    /// submission, so only the flag keeps it in the demand set).
    #[test]
    fn dedup_dag_ors_explicitly_requested() {
        let mk = |requested: bool| types::DerivationNode {
            drv_path: "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-x.drv".into(),
            drv_hash: "x".into(),
            explicitly_requested: requested,
            ..Default::default()
        };

        // Flagged copy is the DROPPED duplicate — flag must survive.
        let mut nodes = vec![mk(false), mk(true)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert_eq!(nodes.len(), 1);
        assert!(
            nodes[0].explicitly_requested,
            "flag set on a dropped duplicate must be ORed into the retained node"
        );

        // Flagged copy is the RETAINED one — stays flagged.
        let mut nodes = vec![mk(true), mk(false)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert!(nodes[0].explicitly_requested);

        // No copy flagged — stays unflagged (no spurious promotion).
        let mut nodes = vec![mk(false), mk(false)];
        dedup_dag(&mut nodes, &mut Vec::new());
        assert!(!nodes[0].explicitly_requested);
    }

    /// Multi-target request where one requested target lies INSIDE the
    /// other's inputDrv closure (`nix build .#app .#lib^dev` with
    /// app → lib): the combined, deduped submission must carry exactly
    /// one `lib` node that (a) is marked `explicitly_requested`, (b)
    /// wants the union of the client's selector and the consumer-derived
    /// names, and (c) keeps its incoming edge — i.e. the request's
    /// "build lib too" intent survives even though lib is not a
    /// structural root of the combined DAG.
    #[tokio::test]
    async fn multi_target_dedup_keeps_inner_target_flagged_and_wanted() {
        let app_path = StorePath::parse("/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app.drv")
            .expect("valid test store path");
        let lib_path = StorePath::parse("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib.drv")
            .expect("valid test store path");

        // lib declares out+dev; app's inputDrvs entry names only {out}.
        let lib_drv = Derivation::parse(
            r#"Derive([("dev","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib-dev","",""),("out","/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib-out","","")],[],[],"x86_64-linux","/bin/sh",[],[])"#,
        )
        .expect("test ATerm parses");
        let app_drv = Derivation::parse(
            r#"Derive([("out","/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-app-out","","")],[("/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-lib.drv",["out"])],[],"x86_64-linux","/bin/sh",[],[])"#,
        )
        .expect("test ATerm parses");

        // Both targets resolved into the per-session cache (production
        // does this via resolve_derivation before reconstruct_dag).
        let mut store = StoreServiceClient::new(rio_test_support::grpc::dead_channel());
        let mut cache = HashMap::new();
        cache.insert(app_path.clone(), app_drv.clone());
        cache.insert(lib_path.clone(), lib_drv.clone());

        // Per-target loop of handle_build_paths{,_with_results}: one
        // resolved sub-DAG per Built target, concatenated…
        let (mut nodes, mut edges) = translate::reconstruct_dag(
            &app_path,
            &app_drv,
            Some(&OutputSpec::All),
            &mut store,
            &mut cache,
        )
        .await
        .expect("app DAG reconstructs");
        let (lib_nodes, lib_edges) = translate::reconstruct_dag(
            &lib_path,
            &lib_drv,
            Some(&OutputSpec::Names(vec!["dev".into()])),
            &mut store,
            &mut cache,
        )
        .await
        .expect("lib DAG reconstructs");
        nodes.extend(lib_nodes);
        edges.extend(lib_edges);

        // …then deduped into ONE submission (submit_dag's first step).
        dedup_dag(&mut nodes, &mut edges);

        assert_eq!(nodes.len(), 2, "app + lib, lib's two copies collapsed");
        let lib = nodes
            .iter()
            .find(|n| n.drv_path == lib_path.as_str())
            .expect("lib node present");
        assert!(
            lib.explicitly_requested,
            "the client named lib as a build target — the combined \
             submission must keep it marked even though it is not a \
             structural root"
        );
        assert_eq!(
            lib.wanted_output_names,
            vec!["dev", "out"],
            "lib's wanted set = client selector (^dev) ∪ consumer-derived (out)"
        );
        let app = nodes
            .iter()
            .find(|n| n.drv_path == app_path.as_str())
            .expect("app node present");
        assert!(app.explicitly_requested, "app is a requested target too");
        assert!(
            edges
                .iter()
                .any(|e| e.parent_drv_path == app_path.as_str()
                    && e.child_drv_path == lib_path.as_str()),
            "lib keeps its incoming edge from app in the combined submission"
        );
    }

    /// Wire helper: read one full `STDERR_START_ACTIVITY` frame and
    /// return `(aid, ActivityType)`. Consumes text + N fields + parent.
    /// Panics if the next frame is not START_ACTIVITY.
    async fn read_start_activity<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> (u64, u64) {
        assert_eq!(wire::read_u64(r).await.unwrap(), STDERR_START_ACTIVITY);
        let aid = wire::read_u64(r).await.unwrap();
        let _level = wire::read_u64(r).await.unwrap();
        let act_type = wire::read_u64(r).await.unwrap();
        let _text = wire::read_string(r).await.unwrap();
        let nfields = wire::read_u64(r).await.unwrap();
        for _ in 0..nfields {
            match wire::read_u64(r).await.unwrap() {
                0 => {
                    let _ = wire::read_u64(r).await.unwrap();
                }
                1 => {
                    let _ = wire::read_string(r).await.unwrap();
                }
                t => panic!("unknown field type {t}"),
            }
        }
        let _parent = wire::read_u64(r).await.unwrap();
        (aid, act_type)
    }

    /// Wire helper: read one `STDERR_STOP_ACTIVITY` frame and return its aid.
    async fn read_stop_activity<R: tokio::io::AsyncRead + Unpin>(r: &mut R) -> u64 {
        assert_eq!(wire::read_u64(r).await.unwrap(), STDERR_STOP_ACTIVITY);
        wire::read_u64(r).await.unwrap()
    }

    // r[verify gw.stderr.activity+2]
    /// Substituting → start_activity(actSubstitute, [out, ""]); Cached →
    /// stop_activity(same aid). A merge-time Cached (no preceding
    /// Substituting) writes nothing.
    #[tokio::test]
    async fn relay_substituting_then_cached_renders_act_substitute() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-foo.drv";
        let out = "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-foo";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_derivation_status(&mut stderr, &mut act, ev(Substituting, drv, &[out]))
            .await
            .unwrap();
        let aids = *act.subst.get(drv).expect("subst aids tracked");
        assert_eq!(act.subst_expected, 1);

        relay_derivation_status(&mut stderr, &mut act, ev(Cached, drv, &[out]))
            .await
            .unwrap();
        assert!(act.subst.is_empty(), "Cached must remove subst aids");

        // r[verify gw.activity.subst-progress]
        // Wire: START(Substitute), START(CopyPath child), no SetExpected
        // (builds_root None), then STOP(copy), STOP(subst).
        let mut r = std::io::Cursor::new(buf);
        let (sa, st) = read_start_activity(&mut r).await;
        assert_eq!(sa, aids.subst);
        assert_eq!(st, ActivityType::Substitute as u64);
        let (ca, ct) = read_start_activity(&mut r).await;
        assert_eq!(ca, aids.copy);
        assert_eq!(ct, ActivityType::CopyPath as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.copy, "child first");
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);

        // Merge-time Cached (no preceding Substituting) writes nothing.
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        let mut act2 = BuildActivityState::default();
        relay_derivation_status(&mut stderr2, &mut act2, ev(Cached, drv, &[out]))
            .await
            .unwrap();
        assert!(
            buf2.is_empty(),
            "Cached without prior Substituting is silent"
        );
    }

    // r[verify gw.activity.subst-progress]
    /// Substituting → SubstituteProgress → Cached: progress emits a
    /// `STDERR_RESULT{copy_aid, resProgress, [done, expected, 0, 0]}`
    /// frame between START(CopyPath) and STOP(copy). This is what nom
    /// renders as the download bar.
    #[tokio::test]
    async fn relay_substitute_progress_emits_res_progress_on_copy() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/pppppppppppppppppppppppppppppppc-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_derivation_status(&mut stderr, &mut act, ev(Substituting, drv, &[]))
            .await
            .unwrap();
        let aids = *act.subst.get(drv).unwrap();

        relay_substitute_progress(
            &mut stderr,
            &act,
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 12_345_678,
                bytes_expected: 99_999_999,
                upstream_uri: "https://cache.example.test".into(),
            },
        )
        .await
        .unwrap();

        relay_derivation_status(&mut stderr, &mut act, ev(Cached, drv, &[]))
            .await
            .unwrap();

        let mut r = std::io::Cursor::new(buf);
        let (_, st) = read_start_activity(&mut r).await;
        assert_eq!(st, ActivityType::Substitute as u64);
        let (ca, ct) = read_start_activity(&mut r).await;
        assert_eq!(ct, ActivityType::CopyPath as u64);
        // STDERR_RESULT{aid, type, nfields, fields...}
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_RESULT);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), ca, "on copy aid");
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ResultType::Progress as u64
        );
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 4);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 0); // type=int
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 12_345_678);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 0);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 99_999_999);
        // skip running, failed
        for _ in 0..2 {
            let _ = wire::read_u64(&mut r).await.unwrap();
            let _ = wire::read_u64(&mut r).await.unwrap();
        }
        assert_eq!(read_stop_activity(&mut r).await, aids.copy);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);

        // Progress for an untracked drv is a silent no-op.
        let mut buf2 = Vec::new();
        let mut w2 = &mut buf2;
        let mut stderr2 = StderrWriter::new(&mut w2);
        relay_substitute_progress(
            &mut stderr2,
            &BuildActivityState::default(),
            types::SubstituteProgress {
                derivation_path: drv.into(),
                bytes_done: 1,
                bytes_expected: 2,
                upstream_uri: String::new(),
            },
        )
        .await
        .unwrap();
        assert!(buf2.is_empty(), "no aids → no resProgress");
    }

    /// A failed substitute fetch reverts to Ready → eventually
    /// dispatches as a build → Started must close the dangling
    /// actSubstitute before opening actBuild.
    #[tokio::test]
    async fn relay_started_after_substituting_stops_subst_first() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/cccccccccccccccccccccccccccccccc-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_derivation_status(&mut stderr, &mut act, ev(Substituting, drv, &[]))
            .await
            .unwrap();
        let aids = *act.subst.get(drv).unwrap();

        relay_derivation_status(&mut stderr, &mut act, ev(Started, drv, &[]))
            .await
            .unwrap();
        assert!(act.subst.is_empty(), "Started must clear subst aids");
        assert!(act.drv.contains_key(drv), "Started must track build aid");

        // Wire order: START(Substitute), START(CopyPath), STOP(copy),
        // STOP(subst), START(Build).
        let mut r = std::io::Cursor::new(buf);
        let (_, st) = read_start_activity(&mut r).await;
        assert_eq!(st, ActivityType::Substitute as u64);
        let (_, ct) = read_start_activity(&mut r).await;
        assert_eq!(ct, ActivityType::CopyPath as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.copy);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);
        let (_, bt) = read_start_activity(&mut r).await;
        assert_eq!(bt, ActivityType::Build as u64);
    }

    /// Substituting → (silent revert to Queued) → DependencyFailed
    /// cascade → Failed. The Failed arm must close the dangling
    /// actSubstitute aid; pre-fix it only touched `act.drv`, leaving
    /// nom showing a stuck "substituting 'X'" line.
    #[tokio::test]
    async fn relay_substituting_then_failed_stops_subst() {
        use rio_nix::protocol::stderr::STDERR_NEXT;
        use types::DerivationEventKind::*;
        let drv = "/nix/store/dddddddddddddddddddddddddddddddd-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_derivation_status(&mut stderr, &mut act, ev(Substituting, drv, &[]))
            .await
            .unwrap();
        let aids = *act.subst.get(drv).unwrap();

        let mut fail_ev = ev(Failed, drv, &[]);
        fail_ev.error_message = "dependency failed".into();
        relay_derivation_status(&mut stderr, &mut act, fail_ev)
            .await
            .unwrap();
        assert!(
            act.subst.is_empty(),
            "Failed must clear subst aids (pre-fix: leaked → stuck nom line)"
        );
        assert!(act.drv.is_empty(), "drv was never Started");

        // Wire order: START(Substitute), START(CopyPath), STOP(copy),
        // STOP(subst), STDERR_NEXT(log). STOP must precede the failure
        // log so nom clears the substituting line before printing the
        // error.
        let mut r = std::io::Cursor::new(buf);
        let (_, st) = read_start_activity(&mut r).await;
        assert_eq!(st, ActivityType::Substitute as u64);
        let (_, ct) = read_start_activity(&mut r).await;
        assert_eq!(ct, ActivityType::CopyPath as u64);
        assert_eq!(read_stop_activity(&mut r).await, aids.copy);
        assert_eq!(read_stop_activity(&mut r).await, aids.subst);

        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_NEXT);
        let msg = wire::read_string(&mut r).await.unwrap();
        assert!(msg.contains("failed"));
    }

    /// Substituting → Completed (defensive — not a normal scheduler
    /// FSM transition). Terminal arm closes any tracked subst aid.
    #[tokio::test]
    async fn relay_substituting_then_completed_stops_subst() {
        use types::DerivationEventKind::*;
        let drv = "/nix/store/5555555555555555555555555555555c-foo.drv";

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        relay_derivation_status(&mut stderr, &mut act, ev(Substituting, drv, &[]))
            .await
            .unwrap();
        assert!(act.subst.contains_key(drv));

        relay_derivation_status(&mut stderr, &mut act, ev(Completed, drv, &[]))
            .await
            .unwrap();
        assert!(
            act.subst.is_empty(),
            "Completed must clear subst aid (terminal-arm symmetry)"
        );
    }

    // r[verify gw.activity.stop-parity]
    /// Three drvs Started, only one Completed (the other two
    /// completions were lost upstream). `drain_unstopped_activities`
    /// must emit `stop_activity` for the leaked aids and clear the
    /// maps.
    #[tokio::test]
    async fn drain_unstopped_activities_emits_stop_for_leaked() {
        use types::DerivationEventKind::*;
        let drvs = [
            "/nix/store/aaaaaaaaaaaaaaaaaaaaaaaaaaaaaaaa-a.drv",
            "/nix/store/bbbbbbbbbbbbbbbbbbbbbbbbbbbbbbbb-b.drv",
            "/nix/store/cccccccccccccccccccccccccccccccc-c.drv",
        ];

        let mut buf = Vec::new();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        let mut act = BuildActivityState::default();

        for d in &drvs {
            relay_derivation_status(&mut stderr, &mut act, ev(Started, d, &[]))
                .await
                .unwrap();
        }
        let aid_a = *act.drv.get(drvs[0]).unwrap();
        let aid_b = *act.drv.get(drvs[1]).unwrap();
        let aid_c = *act.drv.get(drvs[2]).unwrap();

        // One Completed arrives normally; aids b/c leak.
        relay_derivation_status(&mut stderr, &mut act, ev(Completed, drvs[0], &[]))
            .await
            .unwrap();
        assert_eq!(act.drv.len(), 2, "two aids leaked into terminal");

        let pre_drain_len = buf.len();
        let mut w = &mut buf;
        let mut stderr = StderrWriter::new(&mut w);
        drain_unstopped_activities(&mut stderr, &mut act)
            .await
            .unwrap();
        assert!(act.drv.is_empty() && act.subst.is_empty(), "maps drained");

        // Wire after the drain point: exactly two STOP_ACTIVITY frames
        // for aids b and c (order is HashMap iteration — accept either).
        let mut r = std::io::Cursor::new(&buf[pre_drain_len..]);
        let mut stopped = Vec::new();
        for _ in 0..2 {
            assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_STOP_ACTIVITY);
            stopped.push(wire::read_u64(&mut r).await.unwrap());
        }
        stopped.sort_unstable();
        let mut expected = [aid_b, aid_c];
        expected.sort_unstable();
        assert_eq!(stopped, expected, "leaked aids must be stopped at terminal");
        // No extra bytes — drain emits ONLY the leaked stops, never
        // a duplicate of aid_a (already stopped via Completed).
        assert!(
            r.position() as usize == buf.len() - pre_drain_len,
            "no surplus frames after drain"
        );
        let _ = aid_a;
    }
}
