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
    /// Failed/Cancelled event. Scheduler closed the stream
    /// gracefully but the build didn't finish from our view.
    /// Reconnecting is unlikely to help (if it were a failover,
    /// we'd see a transport error, not a clean close).
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
        // 1s-sleep loop: 0→1→Ok()→reset→0→1→... Matches controller
        // build.rs:599 pattern (reset on Ok(Some(ev))).
        *reconnect_attempts = 0;

        // Update active_build_ids with latest sequence
        if let Some(seq) = active_build_ids.get_mut(&event.build_id) {
            *seq = event.sequence;
        }

        match event.event {
            Some(types::build_event::Event::Log(log_batch)) => {
                for line in &log_batch.lines {
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
    // EofWithoutTerminal (not Transport): this is a clean stream close
    // (Ok(None)), not a connection drop. Reconnecting via WatchBuild
    // is unlikely to help — if it were a scheduler crash/failover,
    // we'd see a transport error. Clean close + incomplete build
    // suggests the scheduler intentionally dropped us (bug or
    // resource exhaustion). Don't retry; surface the failure.
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

    let mut event_stream = rio_common::grpc::with_timeout(
        "SubmitBuild",
        DEFAULT_GRPC_TIMEOUT,
        scheduler_client.submit_build(request),
    )
    .await?
    .into_inner();

    // Extract build_id from first event if available
    // (the first event should be BuildStarted)
    // We'll track the build_id as we process events

    // Peek at first message to get build_id
    let first = event_stream
        .message()
        .await
        .map_err(|e| anyhow::anyhow!("build event stream error: {e}"))?;

    let build_id = match &first {
        Some(ev) => ev.build_id.clone(),
        None => return Err(anyhow::anyhow!("empty build event stream")),
    };

    active_build_ids.insert(build_id.clone(), 0);

    // Process the first event
    if let Some(ev) = &first {
        if let Some(types::build_event::Event::Started(ref started)) = ev.event {
            debug!(
                build_id = %build_id,
                total = started.total_derivations,
                cached = started.cached_derivations,
                "build started"
            );
        }
        if let Some(types::build_event::Event::Completed(_)) = ev.event {
            // Remove from active builds
            active_build_ids.remove(&build_id);
            return Ok(BuildResult::success());
        }
        if let Some(types::build_event::Event::Failed(ref failed)) = ev.event {
            active_build_ids.remove(&build_id);
            return Ok(BuildResult::failure(
                BuildStatus::MiscFailure,
                failed.error_message.clone(),
            ));
        }
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
    // doesn't notice the scheduler blip (~15s failover).
    const MAX_RECONNECT: u32 = 5;
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
            // EOF or wire error: NOT reconnect-worthy. EOF =
            // scheduler closed gracefully but incompletely (not a
            // crash — crash would be Transport). Wire = client
            // gone (SSH closed). Surface immediately.
            Err(e @ (StreamProcessError::EofWithoutTerminal | StreamProcessError::Wire(_))) => {
                break Err(e);
            }
            Err(e @ StreamProcessError::Transport(_)) => {
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

    // Remove from active builds
    active_build_ids.remove(&build_id);

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

    // IFD detection: if we haven't seen wopBuildPathsWithResults on this session,
    // this is likely an IFD or build-hook request
    let is_ifd_hint = !*has_seen_build_paths_with_results;

    // Recover full Derivation from drv_cache (BasicDerivation has no inputDrvs).
    // The .drv should have been uploaded via wopAddToStoreNar before this call.
    let drv_path = match StorePath::parse(&drv_path_str) {
        Ok(p) => p,
        Err(e) => stderr_err!(stderr, "invalid drv path '{drv_path_str}': {e}"),
    };

    // Try to get the full derivation with inputDrvs
    let full_drv = resolve_derivation(&drv_path, store_client, drv_cache).await;

    // Reconstruct the DAG
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
        stderr
            .error(&rio_nix::protocol::stderr::StderrError::simple(
                "DAGValidationFailed",
                format!("build rejected: {reason}"),
            ))
            .await?;
        // BuildResult::failure so the wire protocol gets a clean
        // STDERR_LAST + result sequence (caller expects one even
        // on rejection).
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

    let request = translate::build_submit_request(nodes, edges, options.as_ref(), priority_class);

    let build_result =
        match submit_and_process_build(stderr, scheduler_client, request, active_build_ids).await {
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

    // Deduplicate nodes by drv_path
    let mut seen: HashSet<String> = HashSet::new();
    all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

    // Validate BEFORE inlining: __noChroot check + early MAX_DAG_NODES.
    if let Err(reason) = translate::validate_dag(&all_nodes, drv_cache) {
        warn!(reason = %reason, "rejecting build: DAG validation failed");
        stderr_err!(stderr, "build rejected: {reason}");
    }

    // Inline .drv content for will-dispatch nodes (after dedup so we
    // don't serialize the same derivation twice).
    translate::filter_and_inline_drv(&mut all_nodes, drv_cache, store_client).await;

    let request = translate::build_submit_request(all_nodes, all_edges, options.as_ref(), "ci");

    let build_result =
        match submit_and_process_build(stderr, scheduler_client, request, active_build_ids).await {
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
// r[impl gw.stderr.error-before-return]
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
        // Deduplicate nodes
        let mut seen: HashSet<String> = HashSet::new();
        all_nodes.retain(|n| seen.insert(n.drv_path.clone()));

        // Validate BEFORE inlining: __noChroot check + early
        // MAX_DAG_NODES. On reject: STDERR_ERROR + per-path failure,
        // no SubmitBuild.
        let build_result = if let Err(reason) = translate::validate_dag(&all_nodes, drv_cache) {
            warn!(reason = %reason, "rejecting build: DAG validation failed");
            stderr
                .error(&rio_nix::protocol::stderr::StderrError::simple(
                    "DAGValidationFailed",
                    format!("build rejected: {reason}"),
                ))
                .await?;
            BuildResult::failure(BuildStatus::MiscFailure, reason)
        } else {
            // Inline .drv content for will-dispatch nodes.
            translate::filter_and_inline_drv(&mut all_nodes, drv_cache, store_client).await;

            let request =
                translate::build_submit_request(all_nodes, all_edges, options.as_ref(), "ci");

            submit_and_process_build(stderr, scheduler_client, request, active_build_ids)
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
        // All paths were opaque
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
