use super::*;

// ===========================================================================
// Build opcode tests
// ===========================================================================

/// wopBuildPaths (9): reads strings(paths) + u64(build_mode), writes u64(1).
#[tokio::test]
async fn test_build_paths_success() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    // Seed a .drv in store so translate::reconstruct_dag can resolve it.
    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire_send!(&mut h.stream;
        u64: 9,                                  // wopBuildPaths
        // DerivedPath format: "drv_path!output_name" for Built paths
        strings: &[format!("{drv_path}!out")],
        u64: 0,                                  // build_mode = Normal
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "BuildPaths returns u64(1) on success");

    // Verify scheduler received the submit request.
    let submits = h.scheduler.submit_calls.read().unwrap().clone();
    assert_eq!(submits.len(), 1, "scheduler should receive one SubmitBuild");

    h.finish().await;
    Ok(())
}

/// wopBuildPaths with scheduler error: should send STDERR_ERROR.
#[tokio::test]
async fn test_build_paths_scheduler_error_returns_stderr_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        submit_error: Some(tonic::Code::Unavailable),
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire_send!(&mut h.stream;
        u64: 9,                                  // wopBuildPaths
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(!err.message.is_empty());

    h.finish().await;
    Ok(())
}

/// wopBuildPaths when the scheduler stream closes without a terminal event
/// (BuildCompleted/BuildFailed/BuildCancelled). Regression for commit b2d3863:
/// process_build_events must NOT send STDERR_ERROR itself — it returns Err
/// and lets the CALLER send STDERR_ERROR (opcode 9) or STDERR_LAST + failure
/// (opcode 46). Before b2d3863, process_build_events sent STDERR_ERROR inside
/// the loop, causing a double-STDERR_ERROR or STDERR_ERROR-then-STDERR_LAST
/// invalid frame sequence depending on the opcode.
///
/// For opcode 9, the correct behavior is: EXACTLY ONE STDERR_ERROR, then the
/// session closes. We verify this by using drain_stderr_expecting_error which
/// reads until it gets STDERR_ERROR; if process_build_events had already sent
/// one AND the handler sent another, the test harness's handler_task would
/// fail or the stream would desync.
#[tokio::test]
async fn test_build_paths_stream_closed_without_terminal_single_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        close_stream_early: true,
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-early-close.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    wire_send!(&mut h.stream;
        u64: 9,                                  // wopBuildPaths
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // For opcode 9: caller (handle_build_paths) sends STDERR_ERROR on failure.
    // process_build_events must NOT also have sent one (that's the b2d3863 fix).
    // drain_stderr_expecting_error reads one STDERR_ERROR and stops; if there
    // were two, the leftover bytes would desync the stream and h.finish()
    // would fail.
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("stream ended") || err.message.contains("disconnect"),
        "error should mention stream ended / scheduler disconnect: {}",
        err.message
    );

    h.finish().await;
    Ok(())
}

/// wopBuildPathsWithResults (46): reads strings + build_mode, writes
/// u64(count) + per-entry (string:DerivedPath, BuildResult).
#[tokio::test]
async fn test_build_paths_with_results_keyed_format() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);

    let derived_path = format!("{drv_path}!out");
    wire_send!(&mut h.stream;
        u64: 46,                                 // wopBuildPathsWithResults
        strings: std::slice::from_ref(&derived_path),
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    // KeyedBuildResult: u64(count) + per-entry (string:path, BuildResult)
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1, "one DerivedPath requested, one result");

    // DerivedPath echoed back
    let path = wire::read_string(&mut h.stream).await?;
    assert_eq!(path, derived_path, "DerivedPath should be echoed back");

    // BuildResult: status + errorMsg + timesBuilt + isNonDeterministic +
    // startTime + stopTime + cpuUser(tag+val) + cpuSystem(tag+val) +
    // builtOutputs(count + per-output pair)
    let status = wire::read_u64(&mut h.stream).await?;
    assert_eq!(status, 0, "BuildStatus::Built = 0");
    let _error_msg = wire::read_string(&mut h.stream).await?;
    let _times_built = wire::read_u64(&mut h.stream).await?;
    let _is_non_det = wire::read_bool(&mut h.stream).await?;
    let _start_time = wire::read_u64(&mut h.stream).await?;
    let _stop_time = wire::read_u64(&mut h.stream).await?;
    // cpuUser: tag + optional value
    let cpu_user_tag = wire::read_u64(&mut h.stream).await?;
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await?;
    }
    // cpuSystem: tag + optional value
    let cpu_system_tag = wire::read_u64(&mut h.stream).await?;
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await?;
    }
    // builtOutputs
    let built_outputs_count = wire::read_u64(&mut h.stream).await?;
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await?;
        let _realisation_json = wire::read_string(&mut h.stream).await?;
    }

    h.finish().await;
    Ok(())
}

/// wopBuildDerivation (36): reads drv_path + BasicDerivation + build_mode,
/// writes BuildResult. BasicDerivation format is: output_count +
/// per-output(name, path, hash_algo, hash) + input_srcs + platform + builder +
/// args + env_pairs.
#[tokio::test]
async fn test_build_derivation_basic_format() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";

    wire_send!(&mut h.stream;
        u64: 36,                                 // wopBuildDerivation
        string: drv_path,                        // drv path
        // BasicDerivation: outputs
        u64: 1,                                  // 1 output
        string: "out",                           // name
        string: "/nix/store/zzz-output",         // path
        string: "",                              // hash_algo (input-addressed)
        string: "",                              // hash
        // input_srcs
        strings: wire::NO_STRINGS,
        // platform
        string: "x86_64-linux",
        // builder
        string: "/bin/sh",
        // args
        strings: &["-c", "echo hi"],
        // env pairs (count + flat key/value strings; no string_pairs kind)
        u64: 1,
        string: "out",
        string: "/nix/store/zzz-output",
        // build_mode
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;

    // BuildResult: status + errorMsg + timesBuilt + isNonDet + start + stop +
    // cpuUser(tag+val?) + cpuSystem(tag+val?) + builtOutputs
    let status = wire::read_u64(&mut h.stream).await?;
    // Mock sent send_completed: true, so status should be Built (0), not
    // just "any valid status". Previously: assert!(status <= 14) accepted
    // failures as passing.
    assert_eq!(
        status, 0,
        "status should be Built (0) since mock sent completed, got {status}"
    );
    let error_msg = wire::read_string(&mut h.stream).await?;
    assert!(error_msg.is_empty(), "error_msg should be empty on success");
    let _times_built = wire::read_u64(&mut h.stream).await?;
    let _is_non_det = wire::read_bool(&mut h.stream).await?;
    let _start_time = wire::read_u64(&mut h.stream).await?;
    let _stop_time = wire::read_u64(&mut h.stream).await?;
    let cpu_user_tag = wire::read_u64(&mut h.stream).await?;
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await?;
    }
    let cpu_system_tag = wire::read_u64(&mut h.stream).await?;
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await?;
    }
    let built_outputs_count = wire::read_u64(&mut h.stream).await?;
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await?;
        let _realisation_json = wire::read_string(&mut h.stream).await?;
    }

    h.finish().await;
    Ok(())
}

/// BuildPathsWithResults (46) with an invalid build mode should still return
/// results (not STDERR_ERROR) — the handler treats unknown modes as Normal.
/// But invalid DerivedPath strings DO cause per-entry failures.
#[tokio::test]
async fn test_build_paths_with_results_invalid_derived_path() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    wire_send!(&mut h.stream;
        u64: 46,
        // One invalid DerivedPath (unparseable).
        strings: &["garbage!not@a#path"],
        u64: 0,                                  // buildMode = Normal
    );

    // Per CLAUDE.md: "per-entry errors in batch opcodes push
    // BuildResult::failure and continue, not abort". So we should get
    // STDERR_LAST + a failed BuildResult, not STDERR_ERROR.
    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1, "should get one result for one input path");
    let _path = wire::read_string(&mut h.stream).await?;
    // BuildResult: first field is status (u64).
    let status = wire::read_u64(&mut h.stream).await?;
    // Should be a failure status (NOT Built=0).
    assert_ne!(
        status, 0,
        "invalid DerivedPath should produce failure status"
    );
    // Drain remaining BuildResult fields.
    let _err_msg = wire::read_string(&mut h.stream).await?;
    let _times = wire::read_u64(&mut h.stream).await?;
    let _nondet = wire::read_bool(&mut h.stream).await?;
    let _start = wire::read_u64(&mut h.stream).await?;
    let _stop = wire::read_u64(&mut h.stream).await?;
    let tag1 = wire::read_u64(&mut h.stream).await?;
    if tag1 == 1 {
        let _ = wire::read_u64(&mut h.stream).await?;
    }
    let tag2 = wire::read_u64(&mut h.stream).await?;
    if tag2 == 1 {
        let _ = wire::read_u64(&mut h.stream).await?;
    }
    let outputs = wire::read_u64(&mut h.stream).await?;
    for _ in 0..outputs {
        let _ = wire::read_string(&mut h.stream).await?;
        let _ = wire::read_string(&mut h.stream).await?;
    }

    h.finish().await;
    Ok(())
}

// ===========================================================================
// process_build_events coverage via scripted MockScheduler
// ===========================================================================
//
// These tests exercise handler/build.rs:process_build_events, which translates
// the full BuildEvent oneof into STDERR frames. Before these tests, only the
// three coarse outcomes (Completed/Failed/stream-closed) were covered; the
// per-event match arms (Log, Derivation lifecycle, Progress, Cancelled) had
// zero coverage. Scripted events let us hit every arm.

use rio_nix::protocol::client::{StderrMessage, read_stderr_message};
use rio_proto::types::{self, build_event};

/// Build a BuildEvent wrapping just the oneof; MockScheduler auto-fills
/// build_id and sequence.
fn ev(e: build_event::Event) -> types::BuildEvent {
    types::BuildEvent {
        build_id: String::new(),
        sequence: 0,
        timestamp: None,
        event: Some(e),
    }
}

/// Seed a minimal .drv and return its store path. Every scripted-event test
/// needs this so translate::reconstruct_dag has something to resolve.
fn seed_minimal_drv(h: &GatewaySession) -> &'static str {
    let drv_text = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;
    let (drv_nar, drv_hash) = make_nar(drv_text.as_bytes());
    let drv_path = "/nix/store/00000000000000000000000000000000-scripted.drv";
    h.store
        .seed(make_path_info(drv_path, &drv_nar, drv_hash), drv_nar);
    drv_path
}

/// Collect all stderr messages until STDERR_LAST.
async fn collect_stderr_frames(stream: &mut tokio::io::DuplexStream) -> Vec<StderrMessage> {
    let mut frames = Vec::new();
    loop {
        let msg = read_stderr_message(stream).await.expect("read stderr");
        if matches!(msg, StderrMessage::Last) {
            break;
        }
        frames.push(msg);
    }
    frames
}

/// BuildLogBatch lines become STDERR_NEXT frames.
#[tokio::test]
async fn test_build_paths_log_events_become_stderr_next() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Log(types::BuildLogBatch {
                derivation_path: String::new(),
                worker_id: String::new(),
                lines: vec![b"building foo".to_vec(), b"linking".to_vec()],
                first_line_number: 0,
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9, // wopBuildPaths
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let frames = collect_stderr_frames(&mut h.stream).await;
    let logs: Vec<&str> = frames
        .iter()
        .filter_map(|m| match m {
            StderrMessage::Next(s) => Some(s.as_str()),
            _ => None,
        })
        .collect();
    assert_eq!(logs, vec!["building foo", "linking"]);

    // Opcode 9 response: u64(1) on success
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1);
    h.finish().await;
    Ok(())
}

/// DerivationEvent{Started -> Completed} maps to STDERR_START_ACTIVITY /
/// STDERR_STOP_ACTIVITY with matching IDs.
#[tokio::test]
async fn test_build_paths_derivation_lifecycle_activities() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let target = "/nix/store/aaa-activity-test.drv".to_string();
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: target.clone(),
                status: Some(types::derivation_event::Status::Started(
                    types::DerivationStarted {
                        worker_id: "w1".into(),
                    },
                )),
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: target.clone(),
                status: Some(types::derivation_event::Status::Completed(
                    types::DerivationCompleted {
                        output_paths: vec![],
                    },
                )),
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let frames = collect_stderr_frames(&mut h.stream).await;
    // Expect exactly [StartActivity, StopActivity] in order with matching IDs.
    assert_eq!(frames.len(), 2, "frames: {frames:?}");
    let start_id = match &frames[0] {
        StderrMessage::StartActivity {
            id,
            activity_type,
            text,
            ..
        } => {
            assert_eq!(*activity_type, 106, "ActivityType::Build");
            assert!(text.contains("aaa-activity-test.drv"));
            *id
        }
        other => panic!("expected StartActivity, got {other:?}"),
    };
    match &frames[1] {
        StderrMessage::StopActivity { id } => assert_eq!(*id, start_id),
        other => panic!("expected StopActivity, got {other:?}"),
    }

    let _ = wire::read_u64(&mut h.stream).await?;
    h.finish().await;
    Ok(())
}

/// DerivationEvent::Failed stops the activity AND emits a STDERR_NEXT log line.
#[tokio::test]
async fn test_build_paths_derivation_failed_emits_log_and_stop() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let target = "/nix/store/bbb-failed.drv".to_string();
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: target.clone(),
                status: Some(types::derivation_event::Status::Started(
                    types::DerivationStarted {
                        worker_id: "w1".into(),
                    },
                )),
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: target.clone(),
                status: Some(types::derivation_event::Status::Failed(
                    types::DerivationFailed {
                        error_message: "boom".into(),
                        status: types::BuildResultStatus::PermanentFailure.into(),
                    },
                )),
            })),
            ev(build_event::Event::Failed(types::BuildFailed {
                error_message: "build failed".into(),
                failed_derivation: target.clone(),
            })),
        ]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // Failed -> opcode 9 sends STDERR_ERROR. Before that: Start, Stop, Next(log).
    let mut saw_start = false;
    let mut saw_stop = false;
    let mut saw_failed_log = false;
    loop {
        let msg = read_stderr_message(&mut h.stream)
            .await
            .expect("read stderr");
        match msg {
            StderrMessage::StartActivity { .. } => saw_start = true,
            StderrMessage::StopActivity { .. } => saw_stop = true,
            StderrMessage::Next(s) if s.contains("failed: boom") => saw_failed_log = true,
            StderrMessage::Error(_) => break,
            StderrMessage::Last => panic!("unexpected LAST before ERROR"),
            _ => {}
        }
    }
    assert!(
        saw_start && saw_stop && saw_failed_log,
        "start={saw_start} stop={saw_stop} failed_log={saw_failed_log}"
    );

    h.finish().await;
    Ok(())
}

/// BuildCancelled returns a BuildResult with MiscFailure + reason in error_msg.
/// Use opcode 46 (BuildPathsWithResults) so we can read the BuildResult back.
#[tokio::test]
async fn test_build_paths_with_results_cancelled_outcome() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Cancelled(types::BuildCancelled {
                reason: "user abort".into(),
            })),
        ]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 46, // wopBuildPathsWithResults
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    // KeyedBuildResult: count, then (DerivedPath, BuildResult) per entry.
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1);
    let _derived_path = wire::read_string(&mut h.stream).await?;
    let status = wire::read_u64(&mut h.stream).await?;
    let error_msg = wire::read_string(&mut h.stream).await?;
    // BuildStatus::MiscFailure = 9
    assert_eq!(status, 9, "MiscFailure");
    assert!(
        error_msg.contains("build cancelled") && error_msg.contains("user abort"),
        "error_msg: {error_msg}"
    );

    h.finish().await;
    Ok(())
}

/// Progress + Derivation{Cached,Queued} are silent — no STDERR frames.
#[tokio::test]
async fn test_build_paths_silent_events_no_stderr() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 3,
                cached_derivations: 1,
            })),
            ev(build_event::Event::Progress(types::BuildProgress {
                completed: 1,
                running: 1,
                queued: 1,
                total: 3,
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: "/nix/store/cached.drv".into(),
                status: Some(types::derivation_event::Status::Cached(
                    types::DerivationCached {
                        output_paths: vec![],
                    },
                )),
            })),
            ev(build_event::Event::Derivation(types::DerivationEvent {
                derivation_path: "/nix/store/queued.drv".into(),
                status: Some(types::derivation_event::Status::Queued(
                    types::DerivationQueued {},
                )),
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // First frame MUST be STDERR_LAST — none of the silent events emit anything.
    let frames = collect_stderr_frames(&mut h.stream).await;
    assert!(
        frames.is_empty(),
        "silent events should emit no stderr frames, got: {frames:?}"
    );
    let _ = wire::read_u64(&mut h.stream).await?;
    h.finish().await;
    Ok(())
}

/// First event is Completed (no Started) → short-circuit return.
#[tokio::test]
async fn test_build_paths_first_event_completed_short_circuit() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![ev(build_event::Event::Completed(
            types::BuildCompleted {
                output_paths: vec!["/nix/store/cached".into()],
            },
        ))]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "success");
    h.finish().await;
    Ok(())
}

/// First event is Failed (no Started) → short-circuit failure.
#[tokio::test]
async fn test_build_paths_first_event_failed_short_circuit() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![ev(build_event::Event::Failed(types::BuildFailed {
            error_message: "instant fail".into(),
            failed_derivation: String::new(),
        }))]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // opcode 9 sends STDERR_ERROR on failure
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(err.message.contains("instant fail"), "got: {}", err.message);
    h.finish().await;
    Ok(())
}
