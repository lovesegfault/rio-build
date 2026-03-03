use super::*;

// ===========================================================================
// Build opcode tests
// ===========================================================================

/// wopBuildPaths (9): reads strings(paths) + u64(build_mode), writes u64(1).
#[tokio::test]
async fn test_build_paths_success() {
    let mut h = TestHarness::setup().await;
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

    drain_stderr_until_last(&mut h.stream).await;
    let result = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(result, 1, "BuildPaths returns u64(1) on success");

    // Verify scheduler received the submit request.
    let submits = h.scheduler.submit_calls.read().unwrap().clone();
    assert_eq!(submits.len(), 1, "scheduler should receive one SubmitBuild");

    h.finish().await;
}

/// wopBuildPaths with scheduler error: should send STDERR_ERROR.
#[tokio::test]
async fn test_build_paths_scheduler_error_returns_stderr_error() {
    let mut h = TestHarness::setup().await;
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

    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(!err.message.is_empty());

    h.finish().await;
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
async fn test_build_paths_stream_closed_without_terminal_single_error() {
    let mut h = TestHarness::setup().await;
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
    let err = drain_stderr_expecting_error(&mut h.stream).await;
    assert!(
        err.message.contains("stream ended") || err.message.contains("disconnect"),
        "error should mention stream ended / scheduler disconnect: {}",
        err.message
    );

    h.finish().await;
}

/// wopBuildPathsWithResults (46): reads strings + build_mode, writes
/// u64(count) + per-entry (string:DerivedPath, BuildResult).
#[tokio::test]
async fn test_build_paths_with_results_keyed_format() {
    let mut h = TestHarness::setup().await;
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

    drain_stderr_until_last(&mut h.stream).await;

    // KeyedBuildResult: u64(count) + per-entry (string:path, BuildResult)
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 1, "one DerivedPath requested, one result");

    // DerivedPath echoed back
    let path = wire::read_string(&mut h.stream).await.unwrap();
    assert_eq!(path, derived_path, "DerivedPath should be echoed back");

    // BuildResult: status + errorMsg + timesBuilt + isNonDeterministic +
    // startTime + stopTime + cpuUser(tag+val) + cpuSystem(tag+val) +
    // builtOutputs(count + per-output pair)
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(status, 0, "BuildStatus::Built = 0");
    let _error_msg = wire::read_string(&mut h.stream).await.unwrap();
    let _times_built = wire::read_u64(&mut h.stream).await.unwrap();
    let _is_non_det = wire::read_bool(&mut h.stream).await.unwrap();
    let _start_time = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop_time = wire::read_u64(&mut h.stream).await.unwrap();
    // cpuUser: tag + optional value
    let cpu_user_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    // cpuSystem: tag + optional value
    let cpu_system_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    // builtOutputs
    let built_outputs_count = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await.unwrap();
        let _realisation_json = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}

/// wopBuildDerivation (36): reads drv_path + BasicDerivation + build_mode,
/// writes BuildResult. BasicDerivation format is: output_count +
/// per-output(name, path, hash_algo, hash) + input_srcs + platform + builder +
/// args + env_pairs.
#[tokio::test]
async fn test_build_derivation_basic_format() {
    let mut h = TestHarness::setup().await;
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

    drain_stderr_until_last(&mut h.stream).await;

    // BuildResult: status + errorMsg + timesBuilt + isNonDet + start + stop +
    // cpuUser(tag+val?) + cpuSystem(tag+val?) + builtOutputs
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    // Mock sent send_completed: true, so status should be Built (0), not
    // just "any valid status". Previously: assert!(status <= 14) accepted
    // failures as passing.
    assert_eq!(
        status, 0,
        "status should be Built (0) since mock sent completed, got {status}"
    );
    let error_msg = wire::read_string(&mut h.stream).await.unwrap();
    assert!(error_msg.is_empty(), "error_msg should be empty on success");
    let _times_built = wire::read_u64(&mut h.stream).await.unwrap();
    let _is_non_det = wire::read_bool(&mut h.stream).await.unwrap();
    let _start_time = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop_time = wire::read_u64(&mut h.stream).await.unwrap();
    let cpu_user_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_user_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let cpu_system_tag = wire::read_u64(&mut h.stream).await.unwrap();
    if cpu_system_tag == 1 {
        let _val = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let built_outputs_count = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..built_outputs_count {
        let _drv_output_id = wire::read_string(&mut h.stream).await.unwrap();
        let _realisation_json = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}

/// BuildPathsWithResults (46) with an invalid build mode should still return
/// results (not STDERR_ERROR) — the handler treats unknown modes as Normal.
/// But invalid DerivedPath strings DO cause per-entry failures.
#[tokio::test]
async fn test_build_paths_with_results_invalid_derived_path() {
    let mut h = TestHarness::setup().await;

    wire_send!(&mut h.stream;
        u64: 46,
        // One invalid DerivedPath (unparseable).
        strings: &["garbage!not@a#path"],
        u64: 0,                                  // buildMode = Normal
    );

    // Per CLAUDE.md: "per-entry errors in batch opcodes push
    // BuildResult::failure and continue, not abort". So we should get
    // STDERR_LAST + a failed BuildResult, not STDERR_ERROR.
    drain_stderr_until_last(&mut h.stream).await;
    let count = wire::read_u64(&mut h.stream).await.unwrap();
    assert_eq!(count, 1, "should get one result for one input path");
    let _path = wire::read_string(&mut h.stream).await.unwrap();
    // BuildResult: first field is status (u64).
    let status = wire::read_u64(&mut h.stream).await.unwrap();
    // Should be a failure status (NOT Built=0).
    assert_ne!(
        status, 0,
        "invalid DerivedPath should produce failure status"
    );
    // Drain remaining BuildResult fields.
    let _err_msg = wire::read_string(&mut h.stream).await.unwrap();
    let _times = wire::read_u64(&mut h.stream).await.unwrap();
    let _nondet = wire::read_bool(&mut h.stream).await.unwrap();
    let _start = wire::read_u64(&mut h.stream).await.unwrap();
    let _stop = wire::read_u64(&mut h.stream).await.unwrap();
    let tag1 = wire::read_u64(&mut h.stream).await.unwrap();
    if tag1 == 1 {
        let _ = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let tag2 = wire::read_u64(&mut h.stream).await.unwrap();
    if tag2 == 1 {
        let _ = wire::read_u64(&mut h.stream).await.unwrap();
    }
    let outputs = wire::read_u64(&mut h.stream).await.unwrap();
    for _ in 0..outputs {
        let _ = wire::read_string(&mut h.stream).await.unwrap();
        let _ = wire::read_string(&mut h.stream).await.unwrap();
    }

    h.finish().await;
}
