//! Build event → STDERR translation and build opcode handlers.

use std::collections::{HashMap, HashSet};

use rio_common::tenant::NormalizedName;
use rio_nix::derivation::Derivation;
use rio_nix::protocol::build::{
    BuildMode, BuildResult, BuildStatus, read_basic_derivation, write_build_result,
};
use rio_nix::protocol::derived_path::DerivedPath;
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
/// errors (scheduler connection dropped — reconnect-worthy) from
/// stream EOF without terminal (scheduler closed gracefully but
/// incompletely — NOT reconnect-worthy, the build state is lost).
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

/// STDERR-activity state that survives `process_build_events`
/// reconnects. Hoisted to `submit_and_process_build` so a WatchBuild
/// resume after scheduler failover keeps the activity-ID map intact —
/// otherwise the gateway can't `stop_activity` derivations whose
/// `Started` arrived on the previous stream, and nom shows them as
/// stuck forever.
struct BuildActivityState {
    /// Per-derivation activity IDs (for `actBuild` start/stop and for
    /// attaching `BuildLogLine`/`SetPhase` results to the right build).
    drv: HashMap<String, u64>,
    /// Per-derivation `actSubstitute` activity IDs. Started by
    /// `DerivationEventKind::Substituting`, stopped by `Cached`
    /// (success) or `Started` (fetch failed → fell through to build).
    subst: HashMap<String, u64>,
    /// Top-level `actBuilds` activity ID. `None` until `BuildStarted`
    /// arrives. `Progress`/`SetExpected` results attach here.
    builds_root: Option<u64>,
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
            machine_name: rio_common::config::env_or("RIO_GATEWAY_MACHINE_NAME", String::new()),
        }
    }
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
            // know which, so the URI is empty (nom omits the
            // "from <uri>" suffix when fields[1] is empty).
            let out = drv_event
                .output_paths
                .first()
                .cloned()
                .unwrap_or_else(|| drv_event.derivation_path.clone());
            let aid = stderr
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
            act.subst.insert(drv_event.derivation_path.clone(), aid);
        }
        types::DerivationEventKind::Started => {
            // A failed substitute fetch reverts to Ready → may later
            // dispatch as a build. Close the dangling actSubstitute so
            // nom doesn't show it stuck forever.
            if let Some(aid) = act.subst.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
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
            // Terminal: close any dangling actSubstitute. Substituting
            // → Completed shouldn't happen via the normal scheduler
            // FSM, but terminal-arm symmetry costs nothing and guards
            // future scheduler changes.
            if let Some(aid) = act.subst.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
            }
            if let Some(aid) = act.drv.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
            } else {
                // I-206 witness: Completed for a drv we
                // never (or no longer) have an aid for —
                // path-key mismatch, or Started was dropped
                // by a Lagged window. Display-only; the
                // root actBuilds stop at terminal clears
                // children. Logged so the next repro
                // captures which case.
                debug!(
                    drv = %drv_event.derivation_path,
                    tracked = act.drv.len(),
                    "Completed with no tracked activity (I-206)"
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
            if let Some(aid) = act.subst.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
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
            // Log failure via STDERR_NEXT
            stderr
                .log(&format!(
                    "derivation '{}' failed: {}",
                    drv_event.derivation_path, drv_event.error_message
                ))
                .await?;
        }
        types::DerivationEventKind::Cached => {
            // Substituting → Cached: close the actSubstitute. Merge-
            // time cache hits never went Substituting → no aid → no-op.
            if let Some(aid) = act.subst.remove(&drv_event.derivation_path) {
                stderr.stop_activity(aid).await?;
            }
        }
        // Queued: no activity to start/stop, no STDERR.
        types::DerivationEventKind::Queued => {}
    }
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

    // Emit trace_id to the client via STDERR_NEXT — gives operators a
    // grep handle for Tempo when debugging a user's build. With the
    // header path this now fires BEFORE event 0 — operator gets the
    // Tempo handle the moment the build is accepted, not after the
    // first event arrives. PRIORITIZE the scheduler's trace_id
    // (x-rio-trace-id header, read above) over our own — the scheduler
    // span is the one that actually spans the full scheduler→worker
    // chain (data-carry per r[sched.trace.assignment-traceparent]).
    // Our own trace only has gateway spans. Fallback to our own for
    // legacy schedulers that don't set the header. Empty-guard
    // suppresses output when no OTel tracer is configured
    // (current_trace_id_hex returns "" for TraceId::INVALID and the
    // header is absent with no OTel on the scheduler side either).
    let trace_id = header_trace_id.unwrap_or_else(rio_proto::interceptor::current_trace_id_hex);
    if !trace_id.is_empty() {
        let _ = stderr.log(&format!("rio trace_id: {trace_id}\n")).await;
    }

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
    // needs ~20-30s (start + mTLS cert mount + lease acquire on 5s
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
    //   1. Client disconnects mid-build → response-task's handle.data()
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
    // Result: build leaks until r[sched.backstop.timeout+3]. For a 6h
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
// r[impl gw.hook.single-node-dag]
// r[impl gw.hook.ifd-detection+2]
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

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    let (nodes, edges) = match &full_drv {
        Ok(drv) => {
            match translate::reconstruct_dag(&drv_path, drv, store_client, drv_cache).await {
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
            debug!(error = %e, "full derivation not available, using single-node DAG");
            (
                vec![translate::build_node(&drv_path_str, &basic_drv)],
                Vec::new(),
            )
        }
    };

    let priority_class = if is_ifd_hint { "interactive" } else { "ci" };

    // Validate BEFORE inlining (no point doing FindMissingPaths +
    // inline for a DAG we're about to reject). __noChroot check +
    // early MAX_DAG_NODES.
    if let Err(reason) = translate::validate_dag(&nodes, drv_cache) {
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

    // Inline .drv content for will-dispatch nodes. Mutable because
    // this fills node.drv_content in-place. On store error: skips
    // silently (safe degrade; worker fetches).
    let mut nodes = nodes;
    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    // Rate limit + quota BEFORE SubmitBuild. Checked after wire reads
    // + validation (those are cheap; the expensive part is the
    // scheduler RPC + stream). A rate-limited / over-quota client
    // gets STDERR_ERROR; the connection stays open.
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

    // Populate builtOutputs (matching opcode 46). Without this, the
    // ssh-ng:// build-hook path receives `built_outputs=[]` and cannot
    // locate floating-CA output paths → nix-build.cc:722 assert. Needs
    // the full Derivation (with inputDrvs) for hash_derivation_modulo;
    // the inline BasicDerivation lacks inputDrvs so the modular hash
    // would diverge from CppNix for non-leaf drvs. If full_drv resolve
    // failed (single-node fallback above), leave builtOutputs empty —
    // no worse than before, and IA output paths are still recoverable
    // client-side from the BasicDerivation it sent.
    if build_result.status.is_success()
        && let Ok(drv) = &full_drv
    {
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        match enrich_build_result_with_outputs(
            build_result.clone(),
            drv,
            drv_path.as_str(),
            ctx,
            &mut hash_cache,
        )
        .await
        {
            Ok(r) => build_result = r,
            // Build succeeded but the store is unreachable for the
            // realisation lookup. Better to report the store outage
            // than to write empty builtOutputs and let the client
            // assert. We're before stderr.finish().
            Err(e) => stderr_err!(
                stderr,
                "store error querying realisation for {drv_path_str}: {e}"
            ),
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

/// Populate `build_result.built_outputs` from `drv` + Realisations lookup.
///
/// Shared by opcodes 36 and 46. Floating-CA outputs have `path=""` in the
/// `.drv` — the real store path is computed post-build from the NAR hash.
/// Queries the store's Realisations table (scheduler's `insert_realisation`
/// wrote it before the `Completed` event). Without this, the client
/// receives empty `outPath` → `maybeOutputPath == nullopt` → assert at
/// nix-build.cc:722.
///
/// Non-NotFound store errors propagate as `Err` — caller `stderr_err!`s.
/// If `compute_modular_hash_cached` fails (already `warn!`-logged), the
/// result is returned unchanged: IA outputs can't be enriched without the
/// hash either, and the caller's degrade-gracefully path is to push the
/// bare `BuildResult`.
async fn enrich_build_result_with_outputs(
    build_result: BuildResult,
    drv: &Derivation,
    drv_path: &str,
    ctx: &mut SessionContext,
    hash_cache: &mut HashMap<String, [u8; 32]>,
) -> anyhow::Result<BuildResult> {
    let (hash, realized) = resolve_floating_outputs(
        drv,
        drv_path,
        &mut ctx.store_client,
        ctx.jwt.token(),
        &ctx.drv_cache,
        hash_cache,
    )
    .await?;
    // resolve_floating_outputs only computes the modular hash when the drv
    // HAS floating outputs. For pure-IA drvs we still need the hash for the
    // builtOutputs id (`sha256:<hex>!<name>`).
    let hash = match hash {
        Some(h) => h,
        None => {
            match translate::compute_modular_hash_cached(drv, drv_path, &ctx.drv_cache, hash_cache)
            {
                Some(h) => h,
                None => return Ok(build_result),
            }
        }
    };
    Ok(build_result.with_outputs_from_drv(drv, &hex::encode(hash), &realized))
}

/// Dedup DAG nodes by `drv_path` and edges by `(parent, child)`.
///
/// Multi-root builds with shared deps walk the shared subgraph once per
/// root, producing duplicate nodes/edges. The scheduler tolerates dups
/// (MergeDag is idempotent; `derivation_edges` PK is `(parent,child)` so
/// dups are `ON CONFLICT DO NOTHING`) but they waste bytes + PG RTTs.
fn dedup_dag(nodes: &mut Vec<types::DerivationNode>, edges: &mut Vec<types::DerivationEdge>) {
    let mut seen = HashSet::new();
    nodes.retain(|n| seen.insert(n.drv_path.clone()));
    let mut seen_edges = HashSet::new();
    edges.retain(|e| seen_edges.insert((e.parent_drv_path.clone(), e.child_drv_path.clone())));
}

/// Outcome of [`submit_dag`] — the shared DAG-submit pipeline for
/// `wopBuildPaths` and `wopBuildPathsWithResults`.
enum DagSubmitOutcome {
    /// Rate-limited or quota-exceeded. `STDERR_ERROR` already sent by
    /// the respective check; caller should `return Ok(())`.
    Gated,
    /// `validate_dag` rejected the DAG before submission. No
    /// `STDERR_ERROR` sent — caller decides whether to surface as
    /// `stderr_err!` (wopBuildPaths) or as a per-path
    /// `BuildResult::failure(InputRejected, …)` (wopBuildPathsWithResults).
    Rejected(String),
    /// Build was submitted and the scheduler returned a result
    /// (success OR failure — caller inspects `.status`).
    Built(BuildResult),
}

/// Shared DAG-submit pipeline:
/// `dedup → validate → rate-limit → quota → inline-drv → SubmitBuild`.
///
/// Runs every gate between DAG reconstruction and the scheduler RPC.
/// Gate ORDER is fixed here so the two build-paths opcodes cannot drift:
/// validate first (cheap, no I/O), then rate/quota (may send
/// `STDERR_ERROR`), then inline (store I/O), then submit. Prior to this
/// extraction the two handlers ran inline at different points relative
/// to rate/quota — harmless but inconsistent.
///
/// Returns `Err` only when `submit_and_process_build` itself errors
/// (scheduler transport/timeout); caller decides whether that is
/// session-terminal (`stderr_err!`) or a per-path `TransientFailure`.
async fn submit_dag<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
    mut nodes: Vec<types::DerivationNode>,
    mut edges: Vec<types::DerivationEdge>,
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

    if let Err(reason) = translate::validate_dag(&nodes, drv_cache) {
        warn!(reason = %reason, "rejecting build: DAG validation failed");
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

    translate::filter_and_inline_drv(&mut nodes, drv_cache, store_client).await;

    let request = translate::build_submit_request(nodes, edges, "ci", tenant_name.as_ref());
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
async fn resolve_built_dag(
    drv: &StorePath,
    ctx: &mut SessionContext,
) -> anyhow::Result<(
    Vec<types::DerivationNode>,
    Vec<types::DerivationEdge>,
    Derivation,
)> {
    let drv_obj = resolve_derivation(drv, &mut ctx.store_client, &mut ctx.drv_cache).await?;
    let (nodes, edges) =
        translate::reconstruct_dag(drv, &drv_obj, &mut ctx.store_client, &mut ctx.drv_cache)
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

    for raw in &raw_paths {
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
            DerivedPath::Built { drv, .. } => match resolve_built_dag(drv, ctx).await {
                Ok((nodes, edges, _)) => {
                    all_nodes.extend(nodes);
                    all_edges.extend(edges);
                }
                Err(e) => stderr_err!(stderr, "DAG reconstruction failed for '{drv}': {e}"),
            },
        }
    }

    if !all_nodes.is_empty() {
        match submit_dag(stderr, ctx, all_nodes, all_edges).await {
            Ok(DagSubmitOutcome::Gated) => return Ok(()),
            Ok(DagSubmitOutcome::Rejected(reason)) => {
                stderr_err!(stderr, "build rejected: {reason}")
            }
            Ok(DagSubmitOutcome::Built(r)) if !r.status.is_success() => {
                stderr_err!(stderr, "build failed: {}", r.error_msg)
            }
            Ok(DagSubmitOutcome::Built(_)) => {}
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
    let raw_paths = wire::read_strings(reader).await?;
    read_build_mode_normal_only(reader, stderr, "wopBuildPathsWithResults").await?;

    tracing::Span::current().record("count", raw_paths.len());
    debug!(count = raw_paths.len(), "wopBuildPathsWithResults");

    let mut results = Vec::new();

    // Collect all derivation paths to build together
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut opaque_results: HashMap<usize, BuildResult> = HashMap::new();
    // Track idx → (drvPath, Derivation) for successful Built paths so we can
    // populate builtOutputs per-derivation after the build completes.
    // Without builtOutputs, the client can't map the derivation to its outputs
    // and falls back to some NAR-based verification → "error: no sink".
    let mut drv_for_idx: HashMap<usize, (String, Derivation)> = HashMap::new();

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
            DerivedPath::Built { drv, .. } => match resolve_built_dag(drv, ctx).await {
                Ok((nodes, edges, drv_obj)) => {
                    all_nodes.extend(nodes);
                    all_edges.extend(edges);
                    drv_for_idx.insert(idx, (drv.to_string(), drv_obj));
                }
                Err(e) => {
                    opaque_results.insert(
                        idx,
                        BuildResult::failure(BuildStatus::MiscFailure, e.to_string()),
                    );
                }
            },
        }
    }

    if !all_nodes.is_empty() {
        let build_result = match submit_dag(stderr, ctx, all_nodes, all_edges).await {
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

        // Apply the build result to all derivation paths, enriching each with
        // its builtOutputs (drvHashModulo!outputName → Realisation JSON).
        // Uses drv_cache as the resolver for hash_derivation_modulo's transitive deps.
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        for (idx, _raw) in raw_paths.iter().enumerate() {
            if let Some(opaque) = opaque_results.remove(&idx) {
                results.push(opaque);
            } else if build_result.status.is_success()
                && let Some((drv_path, drv_obj)) = drv_for_idx.get(&idx)
            {
                match enrich_build_result_with_outputs(
                    build_result.clone(),
                    drv_obj,
                    drv_path,
                    ctx,
                    &mut hash_cache,
                )
                .await
                {
                    Ok(r) => results.push(r),
                    // Non-NotFound store error during realisation lookup
                    // — this is one batch-wide store outage, aborting is
                    // correct (the next opcode would hit the same dead
                    // store anyway). We're before stderr.finish().
                    Err(e) => stderr_err!(
                        stderr,
                        "store error querying realisation for {drv_path}: {e}"
                    ),
                }
            } else {
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
    use rio_nix::protocol::stderr::{STDERR_START_ACTIVITY, STDERR_STOP_ACTIVITY};

    fn ev(kind: types::DerivationEventKind, drv: &str, outs: &[&str]) -> types::DerivationEvent {
        types::DerivationEvent {
            derivation_path: drv.into(),
            kind: kind as i32,
            output_paths: outs.iter().map(|s| s.to_string()).collect(),
            ..Default::default()
        }
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
        let aid = *act.subst.get(drv).expect("subst aid tracked");

        relay_derivation_status(&mut stderr, &mut act, ev(Cached, drv, &[out]))
            .await
            .unwrap();
        assert!(act.subst.is_empty(), "Cached must remove subst aid");

        // Wire: START(Substitute) + fields[out, ""], then STOP(aid).
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_START_ACTIVITY);
        let got_aid = wire::read_u64(&mut r).await.unwrap();
        assert_eq!(got_aid, aid);
        let _level = wire::read_u64(&mut r).await.unwrap();
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ActivityType::Substitute as u64
        );
        let text = wire::read_string(&mut r).await.unwrap();
        assert!(text.contains(out), "text should name the output path");
        // fields: 2 strings — [out, ""]
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 2);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 1); // type=string
        assert_eq!(wire::read_string(&mut r).await.unwrap(), out);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 1);
        assert_eq!(wire::read_string(&mut r).await.unwrap(), "");
        let _parent = wire::read_u64(&mut r).await.unwrap();
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_STOP_ACTIVITY);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), aid);

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
        let subst_aid = *act.subst.get(drv).unwrap();

        relay_derivation_status(&mut stderr, &mut act, ev(Started, drv, &[]))
            .await
            .unwrap();
        assert!(act.subst.is_empty(), "Started must clear subst aid");
        assert!(act.drv.contains_key(drv), "Started must track build aid");

        // Wire order: START(Substitute), STOP(subst_aid), START(Build).
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_START_ACTIVITY);
        let _ = wire::read_u64(&mut r).await.unwrap(); // aid
        let _ = wire::read_u64(&mut r).await.unwrap(); // level
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ActivityType::Substitute as u64
        );
        // skip text + fields(2 strings) + parent
        let _ = wire::read_string(&mut r).await.unwrap();
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 2);
        for _ in 0..2 {
            let _ = wire::read_u64(&mut r).await.unwrap();
            let _ = wire::read_string(&mut r).await.unwrap();
        }
        let _ = wire::read_u64(&mut r).await.unwrap();

        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_STOP_ACTIVITY);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), subst_aid);

        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_START_ACTIVITY);
        let _ = wire::read_u64(&mut r).await.unwrap();
        let _ = wire::read_u64(&mut r).await.unwrap();
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ActivityType::Build as u64
        );
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
        let subst_aid = *act.subst.get(drv).unwrap();

        let mut fail_ev = ev(Failed, drv, &[]);
        fail_ev.error_message = "dependency failed".into();
        relay_derivation_status(&mut stderr, &mut act, fail_ev)
            .await
            .unwrap();
        assert!(
            act.subst.is_empty(),
            "Failed must clear subst aid (pre-fix: leaked → stuck nom line)"
        );
        assert!(act.drv.is_empty(), "drv was never Started");

        // Wire order: START(Substitute), STOP(subst_aid), STDERR_NEXT(log).
        // STOP must precede the failure log so nom clears the
        // substituting line before printing the error.
        let mut r = std::io::Cursor::new(buf);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_START_ACTIVITY);
        let _ = wire::read_u64(&mut r).await.unwrap(); // aid
        let _ = wire::read_u64(&mut r).await.unwrap(); // level
        assert_eq!(
            wire::read_u64(&mut r).await.unwrap(),
            ActivityType::Substitute as u64
        );
        let _ = wire::read_string(&mut r).await.unwrap();
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), 2);
        for _ in 0..2 {
            let _ = wire::read_u64(&mut r).await.unwrap();
            let _ = wire::read_string(&mut r).await.unwrap();
        }
        let _ = wire::read_u64(&mut r).await.unwrap(); // parent

        assert_eq!(wire::read_u64(&mut r).await.unwrap(), STDERR_STOP_ACTIVITY);
        assert_eq!(wire::read_u64(&mut r).await.unwrap(), subst_aid);

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
}
