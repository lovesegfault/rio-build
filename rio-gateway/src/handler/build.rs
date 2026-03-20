//! Build event → STDERR translation and build opcode handlers.

use super::*;

/// Error from `process_build_events`. Distinguishes transport
/// errors (scheduler connection dropped — reconnect-worthy) from
/// stream EOF without terminal (scheduler closed gracefully but
/// incompletely — NOT reconnect-worthy, the build state is lost).
#[derive(Debug)]
enum StreamProcessError {
    /// gRPC-level error (connection reset, timeout). Scheduler
    /// may have failed over — reconnecting via WatchBuild has
    /// a good chance of resuming.
    Transport(tonic::Status),
    /// Stream returned `Ok(None)` (EOF) without a Completed/
    /// Failed/Cancelled event. This IS the primary failover
    /// signature: k8s pod kill → SIGTERM → graceful shutdown →
    /// TCP FIN → clean stream close. NOT a Transport error.
    /// Reconnect-worthy for the same reason as Transport.
    EofWithoutTerminal,
    /// Error writing STDERR to the client (WireError). The Nix
    /// client disconnected or the SSH channel closed. NOT
    /// reconnect-worthy — scheduler is fine, client is gone.
    Wire(rio_nix::protocol::wire::WireError),
}

impl From<rio_nix::protocol::wire::WireError> for StreamProcessError {
    fn from(e: rio_nix::protocol::wire::WireError) -> Self {
        Self::Wire(e)
    }
}

impl std::fmt::Display for StreamProcessError {
    fn fmt(&self, f: &mut std::fmt::Formatter<'_>) -> std::fmt::Result {
        match self {
            Self::Transport(s) => write!(f, "build event stream error: {s}"),
            Self::EofWithoutTerminal => {
                write!(
                    f,
                    "build event stream ended unexpectedly (scheduler disconnected?)"
                )
            }
            Self::Wire(e) => write!(f, "client disconnected: {e}"),
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
async fn rate_limit_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    limiter: &crate::ratelimit::TenantLimiter,
    tenant_name: &str,
) -> anyhow::Result<bool> {
    match limiter.check(Some(tenant_name)) {
        Ok(()) => Ok(false),
        Err(wait) => {
            let tenant_disp = if tenant_name.is_empty() {
                "anon"
            } else {
                tenant_name
            };
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
async fn quota_check<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    quota_cache: &QuotaCache,
    store_client: &mut StoreServiceClient<Channel>,
    tenant_name: &str,
) -> anyhow::Result<bool> {
    match quota_cache.check(store_client, tenant_name).await {
        QuotaVerdict::Under { .. } | QuotaVerdict::Unlimited => Ok(false),
        QuotaVerdict::Over { used, limit } => {
            let tenant_disp = if tenant_name.is_empty() {
                "anon"
            } else {
                tenant_name
            };
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

/// Legacy-path helper: the first event was consumed by the build_id
/// peek. Process it. Returns `Some(BuildResult)` if the first event
/// was terminal (Completed/Failed/Cancelled — caller returns early),
/// `None` otherwise (caller continues into process_build_events).
///
/// Delete when the legacy peek path is removed (after fleet-wide
/// scheduler upgrade; see phase4a remediation 20).
fn handle_peeked_first_event(first: &types::BuildEvent) -> Option<BuildResult> {
    match &first.event {
        Some(types::build_event::Event::Started(started)) => {
            debug!(
                build_id = %first.build_id,
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
            None
        }
        Some(types::build_event::Event::Completed(_)) => Some(BuildResult::success()),
        Some(types::build_event::Event::Failed(failed)) => Some(BuildResult::failure(
            BuildStatus::MiscFailure,
            failed.error_message.clone(),
        )),
        // Cancelled can be the first event on WatchBuild reconnect
        // after the build was already cancelled — scheduler replays
        // from build_event_log past since_sequence.
        Some(types::build_event::Event::Cancelled(cancelled)) => Some(BuildResult::failure(
            BuildStatus::MiscFailure,
            format!("build cancelled: {}", cancelled.reason),
        )),
        _ => None,
    }
}

/// Process a BuildEvent stream from the scheduler and translate events
/// into STDERR protocol messages for the Nix client.
///
/// Returns the final BuildResult on success, or a typed error.
async fn process_build_events<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    event_stream: &mut tonic::codec::Streaming<types::BuildEvent>,
    active_build_ids: &mut HashMap<String, u64>,
    reconnect_attempts: &mut u32,
) -> Result<BuildEventOutcome, StreamProcessError> {
    let mut drv_activity_ids: HashMap<String, u64> = HashMap::new();

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

        match event.event {
            Some(types::build_event::Event::Log(log_batch)) => {
                for line in &log_batch.lines {
                    // Log display, not parse-path. Build log lines are
                    // arbitrary builder output (may contain invalid UTF-8
                    // from whatever the build script printed); lossy
                    // replacement is the correct behavior for display.
                    #[allow(clippy::disallowed_methods)]
                    let text = String::from_utf8_lossy(line);
                    stderr.log(&text).await?;
                }
            }
            Some(types::build_event::Event::Derivation(drv_event)) => {
                match drv_event.status {
                    Some(types::derivation_event::Status::Started(_)) => {
                        // start_activity auto-assigns the ID
                        let aid = stderr
                            .start_activity(
                                ActivityType::Build,
                                &format!("building '{}'", drv_event.derivation_path),
                                0, // level
                                0, // parent
                            )
                            .await?;
                        drv_activity_ids.insert(drv_event.derivation_path.clone(), aid);
                    }
                    Some(types::derivation_event::Status::Completed(_)) => {
                        if let Some(aid) = drv_activity_ids.remove(&drv_event.derivation_path) {
                            stderr.stop_activity(aid).await?;
                        }
                    }
                    Some(types::derivation_event::Status::Failed(ref failed)) => {
                        if let Some(aid) = drv_activity_ids.remove(&drv_event.derivation_path) {
                            stderr.stop_activity(aid).await?;
                        }
                        // Log failure via STDERR_NEXT
                        stderr
                            .log(&format!(
                                "derivation '{}' failed: {}",
                                drv_event.derivation_path, failed.error_message
                            ))
                            .await?;
                    }
                    Some(types::derivation_event::Status::Cached(_)) => {
                        // No activity to start/stop for cached derivations
                    }
                    Some(types::derivation_event::Status::Queued(_)) => {
                        // No STDERR message for queued state
                    }
                    None => {}
                }
            }
            Some(types::build_event::Event::Started(started)) => {
                debug!(
                    total = started.total_derivations,
                    cached = started.cached_derivations,
                    "build started"
                );
            }
            Some(types::build_event::Event::InputsResolved(_)) => {
                // Scheduler's store cache-check done; dispatch begins
                // next. Could surface via stderr.log() ("inputs
                // resolved") for `nix build` UX, but that's TODO —
                // for now, matches the Started/Progress debug-only
                // pattern. The info to print (N to build = total -
                // cached) arrived in Started above.
                debug!("build inputs resolved");
            }
            Some(types::build_event::Event::Progress(prog)) => {
                debug!(
                    completed = prog.completed,
                    running = prog.running,
                    queued = prog.queued,
                    total = prog.total,
                    "build progress"
                );
            }
            Some(types::build_event::Event::Completed(_)) => {
                return Ok(BuildEventOutcome::Completed);
            }
            Some(types::build_event::Event::Failed(failed)) => {
                return Ok(BuildEventOutcome::Failed {
                    error_message: failed.error_message,
                });
            }
            Some(types::build_event::Event::Cancelled(cancelled)) => {
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
    Failed { error_message: String },
    Cancelled { reason: String },
}

/// Submit a build to the scheduler and process events, returning a BuildResult.
async fn submit_and_process_build<W: AsyncWrite + Unpin>(
    stderr: &mut StderrWriter<&mut W>,
    scheduler_client: &mut SchedulerServiceClient<Channel>,
    request: types::SubmitBuildRequest,
    active_build_ids: &mut HashMap<String, u64>,
    jwt_token: Option<&str>,
) -> anyhow::Result<BuildResult> {
    // Trace propagation: gateway is the trace ROOT (no incoming
    // traceparent from the SSH client — Nix doesn't speak W3C trace
    // context). The span enclosing this call (the per-opcode #[instrument]
    // in handle_opcode) is the top of the trace. Inject its context into
    // the outgoing gRPC metadata so the scheduler's SubmitBuild span
    // becomes a child, and everything downstream (actor, store, worker)
    // chains off that. This is THE hop that makes distributed tracing
    // work — without it, scheduler spans are orphaned root traces.
    let mut request = tonic::Request::new(request);
    rio_proto::interceptor::inject_current(request.metadata_mut());

    // JWT propagation: x-rio-tenant-token carries the signed claims.
    // The scheduler's interceptor (P0259) verifies signature+expiry,
    // attaches Claims to request extensions, and the SubmitBuild
    // handler reads jti from there — NO proto body field for jti
    // (zero wire redundancy: jti lives once in the JWT, parsed once
    // by the interceptor, read once by the handler; see
    // r[gw.jwt.issue]). When token is None, header is absent →
    // scheduler falls back to SubmitBuildRequest.tenant_name per
    // r[gw.jwt.dual-mode].
    //
    // try_from on a JWT can't actually fail — jsonwebtoken emits
    // base64url.base64url.base64url (pure ASCII, no control chars,
    // well under the 8KB MetadataValue limit). The ? is defensive:
    // if rio_common::jwt ever changes encoding, we'd rather error
    // than silently drop auth on the floor.
    if let Some(token) = jwt_token {
        request.metadata_mut().insert(
            "x-rio-tenant-token",
            tonic::metadata::MetadataValue::try_from(token)?,
        );
    }

    let resp = match rio_common::grpc::with_timeout(
        "SubmitBuild",
        DEFAULT_GRPC_TIMEOUT,
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
            return Err(anyhow::anyhow!("SubmitBuild failed: {e}"));
        }
    };

    // build_id from initial response metadata. Scheduler sets this
    // AFTER MergeDag commits (grpc/mod.rs:~480) — if we have it, the
    // build IS durable and WatchBuild can resume it. Reconnect
    // protection is total: even zero stream events is recoverable.
    //
    // Fallback to first-event peek if the header is absent — legacy
    // scheduler (pre-phase4a). After one deploy cycle this branch is
    // dead; keep until the fleet is known-upgraded.
    let header_build_id = resp
        .metadata()
        .get(rio_proto::BUILD_ID_HEADER)
        .and_then(|v| v.to_str().ok())
        .map(str::to_owned);
    let mut event_stream = resp.into_inner();

    let build_id = match header_build_id {
        Some(id) => {
            // Header path: no event consumed yet. seq=0 is correct —
            // process_build_events updates on every event received
            // (see get_mut + *seq = event.sequence inside the loop).
            active_build_ids.insert(id.clone(), 0);
            id
        }
        None => {
            // Legacy path. NOT reconnect-protected — that's the bug
            // this remediation closes for the header path.
            tracing::debug!(
                "scheduler did not set x-rio-build-id header (legacy); peeking first event"
            );
            let first = match event_stream.message().await {
                Ok(Some(ev)) => ev,
                Ok(None) => {
                    let _ = stderr
                        .log("scheduler closed stream before first event (legacy path, no build_id to reconnect)\n")
                        .await;
                    return Err(anyhow::anyhow!(
                        "empty build event stream (legacy scheduler, no header)"
                    ));
                }
                Err(e) => {
                    let _ = stderr
                        .log(&format!("build event stream error on first read: {e}\n"))
                        .await;
                    return Err(anyhow::anyhow!("build event stream error: {e}"));
                }
            };
            let id = first.build_id.clone();
            // r[impl gw.reconnect.since-seq]
            // Track the FIRST event's sequence, not hardcoded 0. The
            // real scheduler starts sequences at 1 (0 is the
            // WatchBuildRequest-side "from start" sentinel).
            // Hardcoding 0 meant the very first reconnect after
            // Started(seq=1) would replay that Started. Header path
            // inserts 0 above — correct there because the event is
            // NOT consumed; process_build_events updates on read.
            active_build_ids.insert(id.clone(), first.sequence);
            if let Some(result) = handle_peeked_first_event(&first) {
                active_build_ids.remove(&id);
                return Ok(result);
            }
            id
        }
    };

    // Emit trace_id to the client via STDERR_NEXT — gives operators a
    // grep handle for Tempo when debugging a user's build. With the
    // header path this now fires BEFORE event 0 — operator gets the
    // Tempo handle the moment the build is accepted, not after the
    // first event arrives. Empty-guard suppresses output when no OTel
    // tracer is configured (current_trace_id_hex returns "" for
    // TraceId::INVALID).
    let trace_id = rio_proto::interceptor::current_trace_id_hex();
    if !trace_id.is_empty() {
        let _ = stderr.log(&format!("rio trace_id: {trace_id}\n")).await;
    }

    // Process remaining events, with reconnect on stream error.
    // Scheduler failover/restart drops the stream; we reconnect
    // via WatchBuild(build_id, since_sequence=last_seen) with
    // backoff (1s/2s/4s/8s/16s, max 5). The scheduler replays
    // events from build_event_log past that sequence.
    //
    // Without reconnect: scheduler restart mid-build → client's
    // `nix build` fails with MiscFailure even though the build
    // itself completes fine on a worker. With reconnect: client
    // doesn't notice the scheduler blip.
    //
    // 10 attempts = 1+2+4+8+16 + 5×16 = 111s total (backoff capped
    // at 16s via `.min(4)` shift below). 5 attempts (=31s) was too
    // tight: a force-killed leader's REPLACEMENT pod needs ~20-30s
    // (start + mTLS cert mount + lease acquire on 5s tick). Found
    // by vm-le-build-k3s under the replacement-wins-race
    // path — standby-wins was fast enough to mask it.
    // r[impl gw.reconnect.backoff]
    const MAX_RECONNECT: u32 = 10;
    let mut reconnect_attempts = 0u32;

    let outcome = loop {
        match process_build_events(
            stderr,
            &mut event_stream,
            active_build_ids,
            &mut reconnect_attempts,
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
                    // Max retries exhausted. Give up — surface
                    // MiscFailure to the client.
                    break Err(e);
                }

                // active_build_ids tracks last seq (updated on
                // every event above). 0 if no events arrived yet —
                // WatchBuild with since=0 replays from the start.
                let since_seq = active_build_ids.get(&build_id).copied().unwrap_or(0);

                let backoff = std::time::Duration::from_secs(1 << (reconnect_attempts - 1).min(4));
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
                match scheduler_client
                    .watch_build(types::WatchBuildRequest {
                        build_id: build_id.clone(),
                        since_sequence: since_seq,
                    })
                    .await
                {
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
    // Result: build leaks until r[sched.backstop.timeout]. For a 6h
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

    match outcome {
        Ok(BuildEventOutcome::Completed) => Ok(BuildResult::success()),
        Ok(BuildEventOutcome::Failed { error_message }) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            error_message,
        )),
        Ok(BuildEventOutcome::Cancelled { reason }) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            format!("build cancelled: {reason}"),
        )),
        Err(e) => Ok(BuildResult::failure(
            BuildStatus::MiscFailure,
            format!("build stream error (reconnect exhausted): {e}"),
        )),
    }
}

// r[impl gw.opcode.build-derivation]
// r[impl gw.hook.single-node-dag]
// r[impl gw.hook.ifd-detection]
/// wopBuildDerivation (36): Build a derivation via scheduler.
///
/// Receives an inline BasicDerivation (no inputDrvs). Recovers the full
/// Derivation from drv_cache to reconstruct the DAG.
#[instrument(skip_all)]
pub(super) async fn handle_build_derivation<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let SessionContext {
        store_client,
        scheduler_client,
        options,
        drv_cache,
        has_seen_build_paths_with_results,
        active_build_ids,
        tenant_name,
        jwt_token,
        limiter,
        quota_cache,
        ..
    } = ctx;
    let drv_path_str = wire::read_string(reader).await?;
    let Ok(basic_drv) = read_basic_derivation(reader).await else {
        stderr_err!(stderr, "wopBuildDerivation: failed to read BasicDerivation");
    };
    let Ok(build_mode_val) = wire::read_u64(reader).await else {
        stderr_err!(stderr, "wopBuildDerivation: failed to read build mode");
    };
    let Ok(_build_mode) = BuildMode::try_from(build_mode_val) else {
        stderr_err!(
            stderr,
            "wopBuildDerivation: unsupported build mode {build_mode_val}"
        );
    };

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
    // isn't available (falls back to single_node_from_basic), the drv
    // is never in the cache → __noChroot check is skipped. The
    // BasicDerivation wire format DOES include env; we have it here.
    //
    // A malicious client could send __noChroot=1 via wopBuildDerivation
    // (which sends an inline BasicDerivation, not a store path) to
    // escape the sandbox. This catches it at the gateway.
    if basic_drv
        .env()
        .get("__noChroot")
        .map(|v| v == "1")
        .unwrap_or(false)
    {
        warn!(drv_path = %drv_path_str, "rejecting __noChroot via inline BasicDerivation");
        stderr_err!(
            stderr,
            "derivation requests __noChroot (sandbox escape) — not permitted"
        );
    }

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let drv_path = match StorePath::parse(&drv_path_str) {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "invalid drv path '{drv_path_str}': {e}"),
    };

    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    let (nodes, edges) = match &full_drv {
        Ok(drv) => {
            match translate::reconstruct_dag(&drv_path, drv, store_client, drv_cache).await {
                Ok((n, e)) => (n, e),
                Err(dag_err) => {
                    warn!(error = %dag_err, "DAG reconstruction failed, using single-node DAG");
                    (
                        translate::single_node_from_basic(&drv_path_str, &basic_drv),
                        Vec::new(),
                    )
                }
            }
        }
        Err(e) => {
            debug!(error = %e, "full derivation not available, using single-node DAG");
            (
                translate::single_node_from_basic(&drv_path_str, &basic_drv),
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
        let failure = BuildResult::failure(BuildStatus::MiscFailure, reason);
        stderr.finish().await?;
        write_build_result(stderr.inner_mut(), &failure).await?;
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
    if rate_limit_check(stderr, limiter, tenant_name).await? {
        return Ok(());
    }
    if quota_check(stderr, quota_cache, store_client, tenant_name).await? {
        return Ok(());
    }

    let request = translate::build_submit_request(
        nodes,
        edges,
        options.as_ref(),
        priority_class,
        tenant_name,
    );

    let build_result = match submit_and_process_build(
        stderr,
        scheduler_client,
        request,
        active_build_ids,
        jwt_token.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => {
            warn!(error = %e, "build submission failed");
            BuildResult::failure(BuildStatus::MiscFailure, format!("scheduler error: {e}"))
        }
    };

    debug!(
        status = ?build_result.status,
        error_msg = %build_result.error_msg,
        "wopBuildDerivation result"
    );

    stderr.finish().await?;
    write_build_result(stderr.inner_mut(), &build_result).await?;
    Ok(())
}

// r[impl gw.opcode.build-paths]
/// wopBuildPaths (9): Build a set of derivations.
#[instrument(skip_all)]
pub(super) async fn handle_build_paths<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let SessionContext {
        store_client,
        scheduler_client,
        options,
        drv_cache,
        active_build_ids,
        tenant_name,
        jwt_token,
        limiter,
        quota_cache,
        ..
    } = ctx;
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let Ok(_build_mode) = BuildMode::try_from(build_mode_val) else {
        stderr_err!(
            stderr,
            "wopBuildPaths: unsupported build mode {build_mode_val}"
        );
    };

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
                match grpc_is_valid_path(store_client, path).await {
                    Ok(true) => { /* exists, fine */ }
                    Ok(false) => {
                        stderr_err!(stderr, "path '{path}' is not valid and cannot be built");
                    }
                    Err(e) => return send_store_error(stderr, e).await,
                }
            }
            DerivedPath::Built { drv, .. } => {
                let drv_obj = match resolve_derivation(drv, store_client, drv_cache).await {
                    Ok(d) => d,
                    Err(e) => return send_store_error(stderr, e).await,
                };

                match translate::reconstruct_dag(drv, &drv_obj, store_client, drv_cache).await {
                    Ok((nodes, edges)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                    }
                    Err(e) => stderr_err!(stderr, "DAG reconstruction failed for '{drv}': {e}"),
                }
            }
        }
    }

    if all_nodes.is_empty() {
        // All paths were opaque and valid -- nothing to build
        stderr.finish().await?;
        wire::write_u64(stderr.inner_mut(), 1).await?;
        return Ok(());
    }

    let mut seen: HashSet<String> = HashSet::new();
    all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

    // Also dedup EDGES (nodes were deduped above). Multi-root builds
    // with shared deps (e.g., both root1 and root2 depend on glibc)
    // produce duplicate edges when each root's BFS walks the shared
    // subgraph. Scheduler's MergeDag likely tolerates dups (DAG merge
    // is idempotent) but sending 2× the edges wastes bytes and PG
    // writes (derivation_edges has PRIMARY KEY (parent,child) so
    // dups are ON CONFLICT DO NOTHING — but that's still an RTT).
    let mut seen_edges: HashSet<(String, String)> = HashSet::new();
    all_edges.retain(|e| seen_edges.insert((e.parent_drv_path.clone(), e.child_drv_path.clone())));

    // Validate BEFORE inlining: __noChroot check + early MAX_DAG_NODES.
    if let Err(reason) = translate::validate_dag(&all_nodes, drv_cache) {
        warn!(reason = %reason, "rejecting build: DAG validation failed");
        stderr_err!(stderr, "build rejected: {reason}");
    }

    // Inline .drv content for will-dispatch nodes (after dedup so we
    // don't serialize the same derivation twice).
    translate::filter_and_inline_drv(&mut all_nodes, drv_cache, store_client).await;

    // Rate limit + quota BEFORE SubmitBuild. Same placement as
    // wopBuildDerivation — after wire reads + validation, before the
    // scheduler RPC.
    if rate_limit_check(stderr, limiter, tenant_name).await? {
        return Ok(());
    }
    if quota_check(stderr, quota_cache, store_client, tenant_name).await? {
        return Ok(());
    }

    let request =
        translate::build_submit_request(all_nodes, all_edges, options.as_ref(), "ci", tenant_name);

    let build_result = match submit_and_process_build(
        stderr,
        scheduler_client,
        request,
        active_build_ids,
        jwt_token.as_deref(),
    )
    .await
    {
        Ok(r) => r,
        Err(e) => stderr_err!(stderr, "build failed: {e}"),
    };

    if !build_result.status.is_success() {
        stderr_err!(stderr, "build failed: {}", build_result.error_msg);
    }

    stderr.finish().await?;
    wire::write_u64(stderr.inner_mut(), 1).await?;
    Ok(())
}

// r[impl gw.opcode.build-paths-with-results]
// r[impl gw.stderr.error-before-return+2]
/// wopBuildPathsWithResults (46): Build paths and return per-path BuildResult.
#[instrument(skip_all)]
pub(super) async fn handle_build_paths_with_results<R: AsyncRead + Unpin, W: AsyncWrite + Unpin>(
    reader: &mut R,
    stderr: &mut StderrWriter<&mut W>,
    ctx: &mut SessionContext,
) -> anyhow::Result<()> {
    let SessionContext {
        store_client,
        scheduler_client,
        options,
        drv_cache,
        active_build_ids,
        tenant_name,
        jwt_token,
        limiter,
        quota_cache,
        ..
    } = ctx;
    let raw_paths = wire::read_strings(reader).await?;
    let build_mode_val = wire::read_u64(reader).await?;
    let Ok(_build_mode) = BuildMode::try_from(build_mode_val) else {
        stderr_err!(
            stderr,
            "wopBuildPathsWithResults: unsupported build mode {build_mode_val}"
        );
    };

    debug!(count = raw_paths.len(), "wopBuildPathsWithResults");

    let mut results = Vec::new();

    // Collect all derivation paths to build together
    let mut all_nodes = Vec::new();
    let mut all_edges = Vec::new();
    let mut drv_indices: Vec<Option<usize>> = Vec::new(); // maps raw_paths index to build result
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
                        BuildStatus::MiscFailure,
                        format!("invalid path '{raw}': {e}"),
                    ),
                );
                drv_indices.push(None);
                continue;
            }
        };

        match &dp {
            DerivedPath::Opaque(path) => {
                let result = match grpc_is_valid_path(store_client, path).await {
                    Ok(true) => BuildResult {
                        status: BuildStatus::AlreadyValid,
                        ..Default::default()
                    },
                    Ok(false) => BuildResult::failure(
                        BuildStatus::MiscFailure,
                        format!("path '{}' not valid", path),
                    ),
                    Err(e) => {
                        BuildResult::failure(BuildStatus::MiscFailure, format!("store error: {e}"))
                    }
                };
                opaque_results.insert(idx, result);
                drv_indices.push(None);
            }
            DerivedPath::Built { drv, .. } => {
                let drv_obj = match resolve_derivation(drv, store_client, drv_cache).await {
                    Ok(d) => d,
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, e.to_string()),
                        );
                        drv_indices.push(None);
                        continue;
                    }
                };

                match translate::reconstruct_dag(drv, &drv_obj, store_client, drv_cache).await {
                    Ok((nodes, edges)) => {
                        all_nodes.extend(nodes);
                        all_edges.extend(edges);
                        drv_indices.push(Some(idx));
                        drv_for_idx.insert(idx, (drv.to_string(), drv_obj));
                    }
                    Err(e) => {
                        opaque_results.insert(
                            idx,
                            BuildResult::failure(BuildStatus::MiscFailure, e.to_string()),
                        );
                        drv_indices.push(None);
                    }
                }
            }
        }
    }

    if !all_nodes.is_empty() {
        let mut seen: HashSet<String> = HashSet::new();
        all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

        // Also dedup edges (same rationale as handle_build_paths above).
        let mut seen_edges: HashSet<(String, String)> = HashSet::new();
        all_edges
            .retain(|e| seen_edges.insert((e.parent_drv_path.clone(), e.child_drv_path.clone())));

        // Validate BEFORE inlining: __noChroot check + early
        // MAX_DAG_NODES. On reject: per-path BuildResult::failure,
        // no SubmitBuild. No STDERR_ERROR — the failure BuildResult
        // is delivered via STDERR_LAST + result at the write loop below.
        let build_result = if let Err(reason) = translate::validate_dag(&all_nodes, drv_cache) {
            warn!(reason = %reason, "rejecting build: DAG validation failed");
            BuildResult::failure(BuildStatus::MiscFailure, reason)
        } else if rate_limit_check(stderr, limiter, tenant_name).await? {
            // Rate limit BEFORE SubmitBuild. STDERR_ERROR already
            // sent by rate_limit_check — same early-return as
            // wopBuildDerivation / wopBuildPaths. No per-path result
            // written: the STDERR_ERROR terminates the opcode.
            return Ok(());
        } else if quota_check(stderr, quota_cache, store_client, tenant_name).await? {
            // Quota BEFORE SubmitBuild. Same STDERR_ERROR + early-
            // return shape as rate_limit_check above.
            return Ok(());
        } else {
            // Inline .drv content for will-dispatch nodes.
            translate::filter_and_inline_drv(&mut all_nodes, drv_cache, store_client).await;

            let request = translate::build_submit_request(
                all_nodes,
                all_edges,
                options.as_ref(),
                "ci",
                tenant_name,
            );

            submit_and_process_build(
                stderr,
                scheduler_client,
                request,
                active_build_ids,
                jwt_token.as_deref(),
            )
            .await
            .unwrap_or_else(|e| {
                warn!(error = %e, "wopBuildPathsWithResults: build submission failed");
                metrics::counter!("rio_gateway_errors_total", "type" => "scheduler_submit")
                    .increment(1);
                BuildResult::failure(BuildStatus::MiscFailure, format!("scheduler error: {e}"))
            })
        };

        // Apply the build result to all derivation paths, enriching each with
        // its builtOutputs (drvHashModulo!outputName → Realisation JSON).
        // Uses drv_cache as the resolver for hash_derivation_modulo's transitive deps.
        let mut hash_cache: HashMap<String, [u8; 32]> = HashMap::new();
        for (idx, _raw) in raw_paths.iter().enumerate() {
            if let Some(opaque) = opaque_results.remove(&idx) {
                results.push(opaque);
            } else if build_result.status.is_success() {
                if let Some((drv_path, drv_obj)) = drv_for_idx.get(&idx) {
                    let resolve =
                        |p: &str| StorePath::parse(p).ok().and_then(|sp| drv_cache.get(&sp));
                    match rio_nix::derivation::hash_derivation_modulo(
                        drv_obj,
                        drv_path,
                        &resolve,
                        &mut hash_cache,
                    ) {
                        Ok(hash) => {
                            let hash_hex = hex::encode(hash);
                            results.push(
                                build_result
                                    .clone()
                                    .with_outputs_from_drv(drv_obj, &hash_hex),
                            );
                        }
                        Err(e) => {
                            warn!(
                                drv_path = %drv_path,
                                error = %e,
                                "hash_derivation_modulo failed; builtOutputs will be empty"
                            );
                            results.push(build_result.clone());
                        }
                    }
                } else {
                    results.push(build_result.clone());
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
        write_build_result(w, result).await?;
    }

    Ok(())
}
