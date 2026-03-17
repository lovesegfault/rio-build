// r[verify gw.opcode.build-paths]
// r[verify gw.opcode.build-paths-with-results]
// r[verify gw.opcode.build-derivation]
// r[verify gw.wire.derived-path]
// r[verify gw.dag.reconstruct]
// r[verify gw.hook.single-node-dag]
// r[verify gw.hook.ifd-detection]
// r[verify gw.stderr.activity]

use super::*;

/// Minimal valid ATerm derivation text. One output ("out"), no inputs,
/// trivial builder. Used by every test that needs reconstruct_dag to
/// resolve a .drv. Tests choose their own store path; this is just the
/// body.
const TEST_DRV_ATERM: &str = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("out","/nix/store/zzz-output")])"#;

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
    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

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

    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

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
/// (BuildCompleted/BuildFailed/BuildCancelled). Regression guard:
/// process_build_events must NOT send STDERR_ERROR itself — it returns Err
/// and lets the CALLER decide (STDERR_ERROR for opcode 9, STDERR_LAST +
/// failure for opcode 46). Sending STDERR_ERROR inside would produce a
/// double-error / ERROR-then-LAST invalid frame sequence.
///
/// EofWithoutTerminal is reconnect-worthy (k8s pod kill = TCP FIN = EOF,
/// not Transport). This test: SubmitBuild EOF → retry loop (STDERR_NEXT
/// "reconnecting...", NOT STDERR_ERROR) → WatchBuild delivers Completed →
/// STDERR_LAST + success. One 1s backoff; real time (paused time fires
/// the store GetPath 300s timeout while TCP I/O is pending).
///
/// Regression guard: if process_build_events had sent STDERR_ERROR on
/// EOF before my fix, we'd see ERROR before the reconnecting-NEXT, and
/// drain_stderr_until_last would desync.
#[tokio::test]
async fn test_build_paths_eof_triggers_reconnect_not_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        // SubmitBuild: close stream after Started → EofWithoutTerminal.
        close_stream_early: true,
        // WatchBuild: deliver Completed on the retry. Explicit sequence=2:
        // SubmitBuild's Started was seq=1, so gateway reconnects with
        // since_seq=1. Mock's since-filter drops seq≤1; seq=2 survives.
        watch_scripted_events: Some(vec![types::BuildEvent {
            build_id: String::new(),
            sequence: 2,
            timestamp: None,
            event: Some(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec!["/nix/store/zzz-out".into()],
            })),
        }]),
        ..Default::default()
    });

    let drv_path = "/nix/store/00000000000000000000000000000000-early-close.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 9,                                  // wopBuildPaths
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // Drain until STDERR_LAST. We should see a "reconnecting..." NEXT
    // (proves the retry loop entered on EOF) and NO STDERR_ERROR
    // (proves process_build_events didn't send one).
    let frames = collect_stderr_frames(&mut h.stream).await;
    let saw_reconnect = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
    assert!(
        saw_reconnect,
        "should see 'reconnecting...' STDERR_NEXT (EOF \u{2192} retry loop). frames: {frames:?}"
    );
    let saw_error = frames.iter().any(|m| matches!(m, StderrMessage::Error(_)));
    assert!(
        !saw_error,
        "process_build_events should NOT send STDERR_ERROR on EOF (regression guard). frames: {frames:?}"
    );

    // Opcode 9 success: u64(1) after STDERR_LAST.
    let success = wire::read_u64(&mut h.stream).await?;
    assert_eq!(
        success, 1,
        "wopBuildPaths should return success after reconnect"
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

    let drv_path = "/nix/store/00000000000000000000000000000000-test.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

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
    // just "any valid status" — a weak assertion here would let failure
    // states pass silently.
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

    // Batch opcodes: per-entry errors push BuildResult::failure and
    // continue, not abort the whole batch. So we get STDERR_LAST + a
    // failed BuildResult, not STDERR_ERROR.
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
    let drv_path = "/nix/store/00000000000000000000000000000000-scripted.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());
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

/// First event is Cancelled (no Started) → short-circuit failure.
///
/// This path hits submit_and_process_build's first-event peek for Cancelled
/// (lines ~246-252 in handler/build.rs) — a reconnect arriving AFTER the build
/// was already cancelled, where Cancelled is the first event past since_seq.
/// For opcode 46, the BuildResult has MiscFailure + "build cancelled: <reason>".
#[tokio::test]
async fn test_build_paths_first_event_cancelled_short_circuit() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![ev(build_event::Event::Cancelled(
            types::BuildCancelled {
                reason: "early cancel".into(),
            },
        ))]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 46, // wopBuildPathsWithResults — read back the BuildResult
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1);
    let _derived_path = wire::read_string(&mut h.stream).await?;
    let status = wire::read_u64(&mut h.stream).await?;
    let error_msg = wire::read_string(&mut h.stream).await?;
    // BuildStatus::MiscFailure = 9
    assert_eq!(status, 9, "first-event-Cancelled → MiscFailure");
    assert!(
        error_msg.contains("build cancelled") && error_msg.contains("early cancel"),
        "error_msg: {error_msg}"
    );

    h.finish().await;
    Ok(())
}

/// Empty event stream (scripted_events = Some(vec![])) → first peek gets
/// Ok(None) → gateway returns Err("empty build event stream") → opcode 9
/// sends STDERR_ERROR.
#[tokio::test]
async fn test_build_paths_empty_event_stream_failure() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("empty"),
        "expected 'empty build event stream', got: {}",
        err.message
    );
    h.finish().await;
    Ok(())
}

// ===========================================================================
// Reconnect loop tests (gateway/src/handler/build.rs reconnect backoff)
// ===========================================================================
//
// r[verify gw.reconnect.backoff]
//
// The exhausted test uses `start_paused = true` so tokio auto-advances
// through the 1s/2s/4s/8s/16s backoff sleeps instantly. The success-case
// test does NOT use paused time: it goes through real gRPC I/O (WatchBuild
// creates a new stream + delivers events), and paused-time auto-advance
// fires gRPC timeout wrappers prematurely while the TCP I/O is pending.
// One 1s backoff is tolerable for one test.

/// SubmitBuild stream errors mid-build (transport error after Started event)
/// → gateway reconnects via WatchBuild → WatchBuild delivers Completed →
/// client sees success. Verifies the full reconnect-and-resume happy path.
///
/// Runs with REAL time (not paused) — one 1s backoff. Paused time breaks
/// this test: auto-advance fires gRPC timeouts while WatchBuild's stream
/// TCP I/O is pending.
#[tokio::test]
async fn test_build_paths_reconnect_on_transport_error() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        // SubmitBuild: send Started, then inject a transport error.
        // The gateway's first-event peek consumes Started, then
        // process_build_events tries to read the next event and gets
        // the Err → StreamProcessError::Transport → reconnect loop.
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            // This second event is never sent: error_after_n fires at seq=1.
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        error_after_n: Some((1, tonic::Code::Unavailable)),
        // WatchBuild: deliver Completed. Gateway's process_build_events
        // reads from this fresh stream after the reconnect.
        // Explicit sequence=2: SubmitBuild's Started was seq=1, so
        // gateway reconnects with since_seq=1. Mock's since-filter
        // drops seq≤1; seq=2 survives.
        watch_scripted_events: Some(vec![types::BuildEvent {
            build_id: String::new(),
            sequence: 2,
            timestamp: None,
            event: Some(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec!["/nix/store/zzz-output".into()],
            })),
        }]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // Drain stderr: we'll see a "reconnecting..." log line via STDERR_NEXT,
    // then STDERR_LAST on successful completion.
    let frames = collect_stderr_frames(&mut h.stream).await;
    let saw_reconnect_log = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
    assert!(
        saw_reconnect_log,
        "expected 'reconnecting...' STDERR_NEXT log, got: {frames:?}"
    );

    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "success after reconnect");
    h.finish().await;
    Ok(())
}

/// Gateway must track first.sequence, not hardcode 0.
///
/// Mock's SubmitBuild sends Started(seq=1), then error. Gateway peeks
/// Started, inserts into active_build_ids. Reconnect reads that value
/// back as since_sequence. The mock records what it received.
///
/// With build.rs:217 hardcoding 0: watch_calls shows (build_id, 0).
/// With build.rs:217 fixed:        watch_calls shows (build_id, 1).
///
/// r[verify gw.reconnect.since-seq]
#[tokio::test]
async fn test_reconnect_sends_first_event_sequence_not_zero() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        // SubmitBuild: Started (auto-fill → seq=1), then transport error
        // at index 1 (before the second event, which is just a placeholder).
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        error_after_n: Some((1, tonic::Code::Unavailable)),
        // WatchBuild: Completed at explicit seq=2 (> since_seq=1 → delivered).
        // Hand-construct instead of ev() so we control sequence directly.
        watch_scripted_events: Some(vec![types::BuildEvent {
            build_id: String::new(),
            sequence: 2,
            timestamp: None,
            event: Some(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec!["/nix/store/zzz".into()],
            })),
        }]),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let _frames = collect_stderr_frames(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "success after reconnect");

    // THE ASSERTION: gateway sent since_sequence=1 (Started's sequence),
    // not 0 (the hardcoded bug).
    let watches = h.scheduler.watch_calls.read().unwrap().clone();
    assert_eq!(watches.len(), 1, "exactly one WatchBuild call");
    assert_eq!(
        watches[0].1, 1,
        "since_sequence must be first.sequence (=1), not hardcoded 0. \
         This is the build.rs:217 bug: active_build_ids.insert(.., 0) \
         should be active_build_ids.insert(.., first.sequence)."
    );

    h.finish().await;
    Ok(())
}

/// SubmitBuild stream errors → gateway tries WatchBuild → WatchBuild fails
/// every time (watch_fail_count > MAX_RECONNECT) → gateway gives up →
/// MiscFailure with "reconnect exhausted".
///
/// Uses opcode 46 so we can read the BuildResult back (opcode 9 would
/// send STDERR_ERROR; here we want the structured failure).
#[tokio::test(start_paused = true)]
async fn test_build_paths_reconnect_exhausted_returns_failure() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    let outcome = MockSchedulerOutcome {
        scripted_events: Some(vec![
            ev(build_event::Event::Started(types::BuildStarted {
                total_derivations: 1,
                cached_derivations: 0,
            })),
            ev(build_event::Event::Completed(types::BuildCompleted {
                output_paths: vec![],
            })),
        ]),
        error_after_n: Some((1, tonic::Code::Unavailable)),
        // watch_fail_count > MAX_RECONNECT (5) → every WatchBuild attempt
        // fails. The gateway's reconnect loop: process_build_events on
        // dead stream → Transport err → attempt++ → backoff+watch_build →
        // watch_build errs → next loop iter → dead stream errs again → ...
        // After 5 attempts it gives up.
        watch_scripted_events: None, // irrelevant — watch_fail_count blocks
        ..Default::default()
    };
    // Set watch_fail_count AFTER Default because Default gives Arc::new(0).
    outcome
        .watch_fail_count
        .store(10, std::sync::atomic::Ordering::SeqCst);
    h.scheduler.set_outcome(outcome);
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 46,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1);
    let _derived_path = wire::read_string(&mut h.stream).await?;
    let status = wire::read_u64(&mut h.stream).await?;
    let error_msg = wire::read_string(&mut h.stream).await?;
    assert_eq!(status, 9, "MiscFailure after reconnect exhausted");
    assert!(
        error_msg.contains("reconnect exhausted") || error_msg.contains("stream error"),
        "error_msg: {error_msg}"
    );

    h.finish().await;
    Ok(())
}
