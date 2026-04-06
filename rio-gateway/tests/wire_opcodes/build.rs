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

/// ATerm derivation with __noChroot=1 in env. Triggers validate_dag rejection.
/// Seeded into the store so resolve_derivation populates drv_cache; the inline
/// BasicDerivation sent on the wire stays CLEAN so the :466 inline check passes.
const NOCHROOT_DRV_ATERM: &str = r#"Derive([("out","/nix/store/zzz-output","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hi"],[("__noChroot","1"),("out","/nix/store/zzz-output")])"#;

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

/// Consume the tail of a BuildResult after status+errorMsg have been read.
/// Shared by the two DAG-reject regression tests below.
async fn drain_build_result_tail(stream: &mut tokio::io::DuplexStream) -> anyhow::Result<()> {
    let _ = wire::read_u64(stream).await?; // timesBuilt
    let _ = wire::read_bool(stream).await?; // isNonDet
    let _ = wire::read_u64(stream).await?; // start
    let _ = wire::read_u64(stream).await?; // stop
    if wire::read_u64(stream).await? == 1 {
        let _ = wire::read_u64(stream).await?; // cpuUser
    }
    if wire::read_u64(stream).await? == 1 {
        let _ = wire::read_u64(stream).await?; // cpuSystem
    }
    let outs = wire::read_u64(stream).await?; // builtOutputs count
    for _ in 0..outs {
        let _ = wire::read_string(stream).await?;
        let _ = wire::read_string(stream).await?;
    }
    Ok(())
}

// r[verify gw.stderr.error-before-return+2]
// r[verify gw.reject.nochroot]
/// wopBuildDerivation (36): DAG-validation failure (cached drv has __noChroot)
/// sends STDERR_LAST + failure BuildResult, NOT STDERR_ERROR.
///
/// Regression for phase4a remediation 07: prior code sent STDERR_ERROR then
/// STDERR_LAST, leaving stale bytes in the TCP buffer that would corrupt the
/// next opcode on a pooled connection.
///
/// Trigger mechanics: seed a __noChroot ATerm in the mock store so
/// resolve_derivation populates drv_cache, but send a CLEAN inline
/// BasicDerivation (no __noChroot) so the build.rs:466 inline check passes
/// and control reaches the build.rs:515 validate_dag path.
#[tokio::test]
async fn test_build_derivation_dag_reject_clean_stderr_last() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let drv_path = "/nix/store/00000000000000000000000000000001-nochroot.drv";
    // Seed the POISONED aterm — validate_dag reads from drv_cache, which is
    // populated by resolve_derivation from the store. The inline BasicDerivation
    // sent below is clean (no __noChroot) so the inline check passes.
    h.store
        .seed_with_content(drv_path, NOCHROOT_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 36,                                 // wopBuildDerivation
        string: drv_path,
        u64: 1,                                  // 1 output
        string: "out",
        string: "/nix/store/zzz-output",
        string: "",                              // hash_algo
        string: "",                              // hash
        strings: wire::NO_STRINGS,               // input_srcs
        string: "x86_64-linux",
        string: "/bin/sh",
        strings: &["-c", "echo hi"],
        u64: 1,                                  // 1 env pair (NOT __noChroot)
        string: "out",
        string: "/nix/store/zzz-output",
        u64: 0,                                  // build_mode
    );

    // KEY ASSERTION: drain_stderr_until_last panics on STDERR_ERROR.
    // Before this fix, the handler sent STDERR_ERROR first → panic
    // "unexpected STDERR_ERROR: build rejected: ...". After the fix,
    // STDERR_LAST arrives cleanly.
    drain_stderr_until_last(&mut h.stream).await?;

    // BuildResult: failure status, errorMsg mentions sandbox/noChroot.
    let status = wire::read_u64(&mut h.stream).await?;
    assert_ne!(status, 0, "status must be a failure code (not Built=0)");
    let error_msg = wire::read_string(&mut h.stream).await?;
    assert!(
        error_msg.contains("noChroot") || error_msg.contains("sandbox"),
        "errorMsg should mention the rejection reason, got: {error_msg:?}"
    );
    drain_build_result_tail(&mut h.stream).await?;

    // NO-STALE-BYTES ASSERTION: send a second opcode on the same stream.
    // If the handler had written extra bytes after STDERR_ERROR (the bug),
    // they would be sitting in the buffer right now and this opcode's
    // response would be garbage. wopSetOptions is the simplest ping:
    // fixed payload, responds with just STDERR_LAST.
    wire_send!(&mut h.stream;
        u64: 19,                                 // wopSetOptions
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0,                                  // overrides count
    );
    // If stale bytes were present, the first read wouldn't be STDERR_LAST.
    drain_stderr_until_last(&mut h.stream).await?;

    // Scheduler must NOT have been called — rejection is pre-submit.
    assert_eq!(
        h.scheduler.submit_calls.read().unwrap().len(),
        0,
        "DAG rejection happens BEFORE SubmitBuild"
    );

    h.finish().await;
    Ok(())
}

// r[verify gw.stderr.error-before-return+2]
// r[verify gw.reject.nochroot]
/// wopBuildPathsWithResults (46): DAG-validation failure sends STDERR_LAST +
/// per-path failure results, NOT STDERR_ERROR. Sibling of the opcode-36 test
/// above, covering the second bug site at build.rs:799-806.
#[tokio::test]
async fn test_build_paths_with_results_dag_reject_clean_stderr_last() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    let drv_path = "/nix/store/00000000000000000000000000000002-nochroot.drv";
    h.store
        .seed_with_content(drv_path, NOCHROOT_DRV_ATERM.as_bytes());

    let derived_path = format!("{drv_path}!out");
    wire_send!(&mut h.stream;
        u64: 46,                                 // wopBuildPathsWithResults
        strings: std::slice::from_ref(&derived_path),
        u64: 0,                                  // build_mode = Normal
    );

    // KEY ASSERTION: same as opcode-36 — panics on STDERR_ERROR.
    drain_stderr_until_last(&mut h.stream).await?;

    // KeyedBuildResult: u64(count) + per-entry (string:path, BuildResult)
    let count = wire::read_u64(&mut h.stream).await?;
    assert_eq!(count, 1, "one result for one input path");
    let echoed_path = wire::read_string(&mut h.stream).await?;
    assert_eq!(echoed_path, derived_path);
    let status = wire::read_u64(&mut h.stream).await?;
    assert_ne!(status, 0, "status must be a failure code (not Built=0)");
    let error_msg = wire::read_string(&mut h.stream).await?;
    assert!(
        error_msg.contains("noChroot") || error_msg.contains("sandbox"),
        "errorMsg should mention the rejection reason, got: {error_msg:?}"
    );
    drain_build_result_tail(&mut h.stream).await?;

    // Second-opcode probe — stream alignment check.
    wire_send!(&mut h.stream;
        u64: 19,
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0, u64: 0, u64: 0, u64: 0, u64: 0, u64: 0,
        u64: 0,
    );
    drain_stderr_until_last(&mut h.stream).await?;

    assert_eq!(
        h.scheduler.submit_calls.read().unwrap().len(),
        0,
        "DAG rejection happens BEFORE SubmitBuild"
    );

    h.finish().await;
    Ok(())
}

// r[verify gw.reject.nochroot]
/// wopBuildDerivation (36): __noChroot=1 in the INLINE BasicDerivation's
/// env is rejected at the handler's early check (build.rs:592-613), before
/// resolve_derivation / validate_dag ever run.
///
/// This is the complement of test_build_derivation_dag_reject_clean_stderr_last
/// above: that test seeds a poisoned ATerm in the store and sends a CLEAN
/// inline BasicDerivation, so the inline check passes and validate_dag fires
/// on the drv_cache entry. THIS test poisons the inline wire bytes directly
/// and seeds NOTHING in the store — the inline check is the only thing that
/// can catch it (resolve_derivation would fail-missing, falling back to
/// single_node_from_basic, which puts nothing in drv_cache).
///
/// Wire behavior differs from the validate_dag path: the inline check uses
/// stderr_err! which sends STDERR_ERROR and returns Err from the handler.
/// The validate_dag path (phase4a remediation 07) instead sends STDERR_LAST
/// + failure BuildResult. Both reject; the inline path is terminal-error,
/// the validate_dag path is clean-failure-result. This test asserts the
/// STDERR_ERROR shape explicitly.
#[tokio::test]
async fn test_build_derivation_inline_nochroot_rejected() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;

    // Store is EMPTY — no seeded .drv. If the inline check were missing,
    // resolve_derivation would fail → single_node_from_basic fallback →
    // validate_dag finds nothing in drv_cache → __noChroot slips through
    // to the scheduler. The inline check is the ONLY barrier for this
    // attack shape (client sends a pre-parsed poisoned BasicDerivation).
    let drv_path = "/nix/store/00000000000000000000000000000003-inline-evil.drv";

    wire_send!(&mut h.stream;
        u64: 36,                                 // wopBuildDerivation
        string: drv_path,
        // BasicDerivation:
        u64: 1,                                  // 1 output
        string: "out",
        string: "/nix/store/zzz-output",
        string: "",                              // hash_algo (input-addressed)
        string: "",                              // hash
        strings: wire::NO_STRINGS,               // input_srcs
        string: "x86_64-linux",
        string: "/bin/sh",
        strings: &["-c", "echo evil"],
        u64: 1,                                  // 1 env pair: __noChroot=1
        string: "__noChroot",
        string: "1",
        u64: 0,                                  // build_mode
    );

    // stderr_err! → STDERR_ERROR (NOT STDERR_LAST + BuildResult). The
    // handler returns Err(anyhow!) immediately after sending the error;
    // no BuildResult bytes follow.
    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("sandbox escape"),
        "error message should mention sandbox escape: {:?}",
        err.message
    );
    assert!(
        err.message.contains("__noChroot"),
        "error message should name the env key: {:?}",
        err.message
    );

    // Rejection is pre-submit — scheduler never sees it. Stronger than
    // the validate_dag tests: here the store was never even queried.
    assert_eq!(
        h.scheduler.submit_calls.read().unwrap().len(),
        0,
        "inline __noChroot rejection happens BEFORE SubmitBuild"
    );

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

/// Collect stderr frames until STDERR_LAST or STDERR_ERROR (both terminal
/// for a single opcode). Unlike `collect_stderr_frames`, includes the
/// terminal frame in the returned Vec so callers can inspect it.
async fn collect_stderr_frames_terminal(
    stream: &mut tokio::io::DuplexStream,
) -> Vec<StderrMessage> {
    let mut frames = Vec::new();
    loop {
        let msg = read_stderr_message(stream).await.expect("read stderr");
        let is_terminal = matches!(msg, StderrMessage::Last | StderrMessage::Error(_));
        frames.push(msg);
        if is_terminal {
            break;
        }
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
                // critical_path_remaining_secs + assigned_workers: don't
                // care — this test asserts Progress is silent (no stderr).
                ..Default::default()
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

// r[verify gw.reconnect.backoff]
/// Phase4a remediation 20: scheduler accepts SubmitBuild (MergeDag
/// committed, build_id in header) then drops the stream before event 0.
/// Gateway reads build_id from initial metadata → enters reconnect loop
/// → WatchBuild(build_id, since=0) → Completed. Client sees success, not
/// "empty build event stream".
///
/// Before this fix: the first-event peek got Ok(None) →
/// Err("empty build event stream") → opcode 9 sends STDERR_ERROR.
/// The build was already committed but the gateway had no build_id
/// to reconnect with.
///
/// Runs with REAL time (not paused) — one 1s backoff. Paused time
/// fires gRPC timeout wrappers while TCP I/O pends (see :968-969).
#[tokio::test]
async fn test_build_paths_empty_stream_reconnects_via_header() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        // Zero events: tx drops immediately, stream is empty. Header
        // IS set (default). Gateway reads build_id from header, enters
        // the reconnect loop, process_build_events gets EofWithoutTerminal
        // on its first read, reconnect arm fires.
        scripted_events: Some(vec![]),
        // WatchBuild delivers the terminal event. Auto-seq=1 > since=0
        // (gateway's initial insert for header path), so it passes the
        // mock's since-filter.
        watch_scripted_events: Some(vec![ev(build_event::Event::Completed(
            types::BuildCompleted {
                output_paths: vec!["/nix/store/zzz-out".into()],
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

    // PRIMARY ASSERTION: reconnect fired, no STDERR_ERROR. Same shape
    // as test_build_paths_eof_triggers_reconnect_not_error — but that
    // test has Started THEN EOF (first event arrives); here the stream
    // is empty from event 0. Before this fix that distinction mattered;
    // now both paths are identical (header means we don't need event 0).
    let frames = collect_stderr_frames(&mut h.stream).await;
    let saw_reconnect = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
    assert!(
        saw_reconnect,
        "expected 'reconnecting...' STDERR_NEXT (empty stream -> EofWithoutTerminal -> reconnect), got: {frames:?}"
    );
    let saw_error = frames.iter().any(|m| matches!(m, StderrMessage::Error(_)));
    assert!(
        !saw_error,
        "STDERR_ERROR should not be sent. frames: {frames:?}"
    );

    // wopBuildPaths success → u64(1).
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "BuildPaths returns u64(1) on success");

    // Exactly ONE SubmitBuild (Option B's whole point: no resubmit).
    // A hypothetical Option A fix would show 2 here.
    assert_eq!(
        h.scheduler.submit_calls.read().unwrap().len(),
        1,
        "build_id from header means NO resubmit: one SubmitBuild, one WatchBuild"
    );

    h.finish().await;
    Ok(())
}

/// Legacy scheduler (no x-rio-build-id header): gateway falls back to
/// first-event peek. Stream empty → no build_id → Err with diagnostic
/// STDERR_NEXT, then caller's stderr_err!. This is the PRE-fix behaviour,
/// preserved under `suppress_build_id_header` for deploy-order safety.
#[tokio::test]
async fn test_build_paths_empty_stream_legacy_no_header() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(vec![]),
        suppress_build_id_header: true, // ← legacy scheduler
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    // Drain: expect STDERR_NEXT diagnostic ("legacy path, no build_id
    // to reconnect"), THEN STDERR_ERROR (opcode 9 wraps the returned
    // Err with stderr_err!).
    let frames = collect_stderr_frames_terminal(&mut h.stream).await;

    // Diagnostic STDERR_NEXT — this is the bare-? fix. Before, the
    // user saw only the STDERR_ERROR with no context.
    let saw_diag = frames.iter().any(
        |m| matches!(m, StderrMessage::Next(s) if s.contains("legacy") || s.contains("before first event")),
    );
    assert!(
        saw_diag,
        "expected diagnostic STDERR_NEXT on legacy empty-stream. frames: {frames:?}"
    );

    // STDERR_ERROR follows (caller's stderr_err!).
    let err = frames
        .iter()
        .find_map(|m| match m {
            StderrMessage::Error(e) => Some(e),
            _ => None,
        })
        .expect("expected STDERR_ERROR on legacy empty-stream path");
    assert!(
        err.message.contains("empty") || err.message.contains("legacy"),
        "error message: {}",
        err.message
    );

    // NO reconnect attempt — legacy path has no build_id.
    let saw_reconnect = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("reconnecting")));
    assert!(!saw_reconnect, "legacy path cannot reconnect (no build_id)");

    h.finish().await;
    Ok(())
}

/// SubmitBuild RPC itself fails (Unavailable — scheduler down before
/// accept). Gateway emits "SubmitBuild RPC failed" STDERR_NEXT before
/// the caller's stderr_err!. No header, no build_id, nothing committed
/// scheduler-side — correctly NOT a reconnect case.
#[tokio::test]
async fn test_build_paths_submit_rpc_fail_diagnostic() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        submit_error: Some(tonic::Code::Unavailable),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let frames = collect_stderr_frames_terminal(&mut h.stream).await;
    let saw_diag = frames
        .iter()
        .any(|m| matches!(m, StderrMessage::Next(s) if s.contains("SubmitBuild RPC failed")));
    assert!(
        saw_diag,
        "expected 'SubmitBuild RPC failed' STDERR_NEXT. frames: {frames:?}"
    );

    // Terminal frame is STDERR_ERROR (opcode 9 stderr_err!).
    assert!(
        matches!(frames.last(), Some(StderrMessage::Error(_))),
        "expected STDERR_ERROR terminal. frames: {frames:?}"
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

/// P0331: mid-opcode client disconnect must trigger CancelBuild.
///
/// Scenario: client sends wopBuildPathsWithResults, scheduler starts
/// streaming Log events, client drops its stream mid-build. The gateway's
/// next stderr.log write gets BrokenPipe → StreamProcessError::Wire. With
/// the fix: build_id stays in active_build_ids (build.rs guards remove on
/// Wire error) AND session.rs cancels on handler-Err (not just opcode-read
/// EOF). Without the fix: build_id removed before error propagates,
/// session.rs ? exits loop, no CancelBuild, build leaks to backstop timeout.
///
/// Test mechanics: scheduler sends 50 Log events at 20ms intervals (1s
/// total). Client reads the first STDERR_NEXT frame to confirm we're past
/// SubmitBuild and inside the log-forwarding loop, then drops its stream.
/// Gateway's next stderr.log write fails; cancel must arrive within 2s.
///
/// Mutation check: revert the `if !matches!(outcome, Err(Wire))` guard in
/// handler/build.rs and this test must fail (cancels stays empty).
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_mid_opcode_disconnect_cancels_build() -> anyhow::Result<()> {
    // 50 Log events at 20ms apiece = 1s of streaming. Plenty of time
    // for the client to drop mid-stream. Each event is a single short
    // line → a STDERR_NEXT frame on the wire.
    let log_events: Vec<types::BuildEvent> = (0..50)
        .map(|i| {
            ev(build_event::Event::Log(types::BuildLogBatch {
                derivation_path: String::new(),
                worker_id: String::new(),
                lines: vec![format!("building step {i}").into_bytes()],
                first_line_number: 0,
            }))
        })
        .collect();

    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(log_events),
        scripted_event_interval: Some(std::time::Duration::from_millis(20)),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 46, // wopBuildPathsWithResults
        strings: &[format!("{drv_path}!out")],
        u64: 0,  // build_mode = Normal
    );

    // Wait for the first Log event to arrive as STDERR_NEXT. This proves:
    //   (a) SubmitBuild completed (build_id header set, map populated)
    //   (b) process_build_events did at least one loop iteration
    //   (c) gateway is actively writing stderr frames (not blocked on gRPC)
    // Without this anchor the drop could race ahead of SubmitBuild and
    // we'd be testing a different (uninteresting) path.
    let first_frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_stderr_message(&mut h.stream),
    )
    .await
    .expect("first stderr frame within 5s")
    .expect("read stderr frame");
    assert!(
        matches!(first_frame, StderrMessage::Next(ref s) if s.contains("building step")),
        "expected STDERR_NEXT with log line, got: {first_frame:?}"
    );

    // Precondition self-check: confirm the scheduler actually received
    // SubmitBuild. If the test asserted on cancel_calls without this,
    // a harness change that short-circuits before submit would make the
    // test vacuously pass (0 submits → 0 cancels → "passes").
    let submits = h.scheduler.submit_calls.read().unwrap().len();
    assert_eq!(submits, 1, "SubmitBuild must have fired before we drop");

    // Drop the client stream. The server's write-half now gets
    // BrokenPipe on the next write. Same trick as GatewaySession::finish:
    // can't drop() a field of a Drop-impl struct, so replace with a
    // dangling endpoint.
    h.stream = tokio::io::duplex(1).0;

    // Wait for run_protocol to finish (it returns Err on BrokenPipe;
    // the test harness logs that at debug and continues).
    tokio::time::timeout(std::time::Duration::from_secs(5), h.join_server())
        .await
        .expect("server task should finish within 5s after client drop");

    // THE assertion: CancelBuild was sent with the right reason.
    let cancels = h.scheduler.cancel_calls.read().unwrap().clone();
    assert_eq!(
        cancels.len(),
        1,
        "mid-opcode disconnect must send exactly one CancelBuild; got: {cancels:?}"
    );
    // Mock's fixed build_id — see MockScheduler::submit_build.
    assert_eq!(
        cancels[0].0,
        "test-build-00000000-1111-2222-3333-444444444444"
    );
    assert_eq!(cancels[0].1, "client_disconnect");

    Ok(())
}

// r[verify gw.conn.cancel-on-disconnect]
/// P0335: graceful-shutdown signal lets the cancel-on-disconnect
/// machinery run — task is NOT aborted.
///
/// The race P0331 noted but didn't fix: `ChannelSession::Drop` did
/// `proto_task.abort()`. On TCP RST, russh fires `channel_close` close
/// to `channel_eof`. If `channel_close → Drop → abort()` won the race,
/// the task future was dropped before ANY cancel path could run. Fix:
/// Drop fires a `CancellationToken` instead of aborting.
///
/// What this test proves — two halves:
///
/// **Part A (idle path):** handshake, no build, fire token. Task exits
/// cleanly via the select `shutdown.cancelled()` branch. Without that
/// branch, the task blocks on the (still-open) stream waiting for the
/// next opcode until OPCODE_IDLE_TIMEOUT (600s). The 2s timeout below
/// is the discriminator. Mutation: remove the select branch → test
/// times out here.
///
/// **Part B (mid-build path — the production scenario):** start a build,
/// get into the stream loop, then fire token + drop stream — exactly
/// what `ChannelSession::Drop` does (`shutdown.cancel()` +
/// `response_task.abort()` → outbound pipe breaks). Handler's write
/// gets BrokenPipe → `StreamProcessError::Wire` → build_id stays in
/// map (P0331's guard at `handler/build.rs:524`) → handler returns Err
/// → session.rs handler-Err arm cancels. The token-branch itself isn't
/// hit here (handler-Err returns before re-reaching select), but the
/// point is the task WASN'T ABORTED — some cancel path ran.
///
/// Part B's reason is `"client_disconnect"` (handler-Err arm), NOT
/// `"channel_close"` (token branch). This is CORRECT. In production,
/// mid-build channel_close always routes through handler-Err because
/// `response_task.abort()` breaks the pipe. The token-branch is the
/// between-opcode escape hatch (Part A) and a defensive backstop if
/// the map is ever non-empty between opcodes (future handler bug).
///
/// This harness bypasses russh — `GatewaySession` spawns `run_protocol`
/// directly on a DuplexStream. The server.rs `Drop → shutdown.cancel()`
/// line itself is structurally trivial (one method call, no failure
/// path); what we test here is that `run_protocol` HONORS the signal.
#[tokio::test(flavor = "multi_thread", worker_threads = 4)]
async fn test_shutdown_signal_cancels_active_builds() -> anyhow::Result<()> {
    // ─── Part A: idle session, token fires, clean exit ───────────────

    let mut h = GatewaySession::new_with_handshake().await?;

    // Precondition: task is alive, blocked at opcode-read.
    assert!(!h.shutdown.is_cancelled());

    // Fire. Stream stays open — no EOF to fall back on. The ONLY way
    // run_protocol exits within 2s is the select shutdown-branch.
    h.shutdown.cancel();

    tokio::time::timeout(std::time::Duration::from_secs(2), h.join_server())
        .await
        .expect(
            "idle session must exit within 2s of token.cancel() — \
             without the select branch, this blocks until OPCODE_IDLE_TIMEOUT (600s)",
        );

    // Zero cancels — map was empty (no build started). Proves the
    // select branch ran cancel_active_builds (no-op on empty map) and
    // returned Ok. If the task had somehow reached the EOF arm instead
    // (it can't — stream is open), cancels would still be 0, but the
    // 2s timeout above is the real discriminator.
    let cancels_a = h.scheduler.cancel_calls.read().unwrap().len();
    assert_eq!(cancels_a, 0, "no builds were active in part A");

    // ─── Part B: mid-build, token + stream drop, CancelBuild sent ────

    // Fresh session. Same scripted-log-events rig as the sister test
    // above — 50 events at 20ms gives a 1s window to drop mid-stream.
    let log_events: Vec<types::BuildEvent> = (0..50)
        .map(|i| {
            ev(build_event::Event::Log(types::BuildLogBatch {
                derivation_path: String::new(),
                worker_id: String::new(),
                lines: vec![format!("building step {i}").into_bytes()],
                first_line_number: 0,
            }))
        })
        .collect();

    let mut h = GatewaySession::new_with_handshake().await?;
    h.scheduler.set_outcome(MockSchedulerOutcome {
        scripted_events: Some(log_events),
        scripted_event_interval: Some(std::time::Duration::from_millis(20)),
        ..Default::default()
    });
    let drv_path = seed_minimal_drv(&h);

    wire_send!(&mut h.stream;
        u64: 46, // wopBuildPathsWithResults
        strings: &[format!("{drv_path}!out")],
        u64: 0,  // build_mode = Normal
    );

    // Anchor: first STDERR_NEXT proves SubmitBuild fired and we're
    // inside the stream loop with build_id in the map.
    let first_frame = tokio::time::timeout(
        std::time::Duration::from_secs(5),
        read_stderr_message(&mut h.stream),
    )
    .await
    .expect("first stderr frame within 5s")
    .expect("read stderr frame");
    assert!(
        matches!(first_frame, StderrMessage::Next(ref s) if s.contains("building step")),
        "expected STDERR_NEXT, got: {first_frame:?}"
    );

    // Proves-nothing guard: SubmitBuild must have fired. Without this,
    // a harness change that short-circuits before submit → 0 cancels
    // → test passes vacuously.
    let submits = h.scheduler.submit_calls.read().unwrap().len();
    assert_eq!(submits, 1, "SubmitBuild must have fired");
    let cancels_before = h.scheduler.cancel_calls.read().unwrap().len();
    assert_eq!(cancels_before, 0, "no cancels yet");

    // Mimic ChannelSession::Drop exactly: shutdown.cancel() then break
    // the pipe (response_task.abort() equivalent). Order matches
    // server.rs Drop impl.
    h.shutdown.cancel();
    h.stream = tokio::io::duplex(1).0; // pipe break — same trick as finish()

    tokio::time::timeout(std::time::Duration::from_secs(5), h.join_server())
        .await
        .expect("server task should finish within 5s");

    // THE assertion: CancelBuild was sent. The OLD abort()-in-Drop would
    // have dropped the task future before handler-Err arm ran — this
    // would be 0. Now the task runs to completion: handler gets
    // BrokenPipe → Wire error → build_id stays in map → handler-Err
    // arm cancels.
    let cancels = h.scheduler.cancel_calls.read().unwrap().clone();
    assert_eq!(
        cancels.len(),
        1,
        "task must NOT be aborted — some cancel path must run; got: {cancels:?}"
    );
    assert_eq!(
        cancels[0].0,
        "test-build-00000000-1111-2222-3333-444444444444"
    );
    // reason = "client_disconnect" (handler-Err arm), not "channel_close"
    // (token branch). CORRECT for mid-build: the pipe-break makes the
    // handler Err before control returns to the select. The token being
    // cancelled is irrelevant in this path — but it would be LOAD-BEARING
    // if a future change made the handler return Ok on Wire error
    // (select would then catch it with "channel_close").
    assert_eq!(cancels[0].1, "client_disconnect");

    Ok(())
}

// ===========================================================================
// Quota gate (pre-SubmitBuild, store.gc.tenant-quota-enforce)
// ===========================================================================

// r[verify store.gc.tenant-quota-enforce]
/// Over-quota tenant → STDERR_ERROR before SubmitBuild. Same
/// pre-SubmitBuild gate shape as the rate-limit check: the scheduler
/// never sees the request, the connection stays open, and the user
/// gets a human-readable error with used/limit bytes.
///
/// Drives the RPC fetch path end-to-end: `QuotaCache::check` misses,
/// calls `MockStore::tenant_quota`, classifies as `Over`, the
/// handler sends STDERR_ERROR + early-returns. The scheduler's
/// `submit_calls` stays empty.
#[tokio::test]
async fn over_quota_sends_stderr_error() -> anyhow::Result<()> {
    // Non-empty tenant_name so the quota check runs (empty name
    // short-circuits to Unlimited — single-tenant mode).
    let mut h = GatewaySession::new_with_tenant_handshake("team-quota").await?;

    // Seed MockStore: 200 MiB used, 100 MiB limit → Over. The mock's
    // TenantQuota RPC reads this map directly.
    h.store.tenant_quotas.write().unwrap().insert(
        "team-quota".to_string(),
        (200 * 1024 * 1024, Some(100 * 1024 * 1024)),
    );

    let drv_path = "/nix/store/00000000000000000000000000000000-quota.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 9,                                  // wopBuildPaths
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    let err = drain_stderr_expecting_error(&mut h.stream).await?;
    assert!(
        err.message.contains("over store quota"),
        "error should say 'over store quota': {:?}",
        err.message
    );
    assert!(
        err.message.contains("team-quota"),
        "error should name the tenant (proves the limiter keyed on \
         tenant_name, not a generic gate): {:?}",
        err.message
    );
    // Human-readable bytes (200.0 MiB / 100.0 MiB). Don't assert the
    // exact string — just that both MiB values appear.
    assert!(
        err.message.contains("MiB"),
        "error should format bytes human-readably: {:?}",
        err.message
    );

    // Pre-SubmitBuild rejection: scheduler never saw it. This is the
    // load-bearing half — STDERR_ERROR alone can't distinguish
    // "rejected at gateway" from "scheduler rejected it too".
    let submits = h.scheduler.submit_calls.read().unwrap().clone();
    assert!(
        submits.is_empty(),
        "over-quota rejection must be pre-SubmitBuild; scheduler got {} calls",
        submits.len()
    );

    h.finish().await;
    Ok(())
}

/// Under-quota / no-limit / unknown-tenant → quota gate passes. This
/// is the positive control for `over_quota_sends_stderr_error` — a
/// gate that always rejects would pass that test too.
#[tokio::test]
async fn under_quota_passes_through() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_tenant_handshake("team-under").await?;

    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    // 50 used, 100 limit → Under.
    h.store
        .tenant_quotas
        .write()
        .unwrap()
        .insert("team-under".to_string(), (50, Some(100)));

    let drv_path = "/nix/store/00000000000000000000000000000000-under.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(result, 1, "under-quota build should succeed (u64(1))");

    let submits = h.scheduler.submit_calls.read().unwrap().clone();
    assert_eq!(
        submits.len(),
        1,
        "under-quota → SubmitBuild reaches the scheduler"
    );

    h.finish().await;
    Ok(())
}

/// Unknown tenant (no `tenant_quotas` entry) → NOT_FOUND from mock
/// → gateway fails-open (Unlimited). Single-tenant mode and
/// operator-forgot-to-seed both flow here; the scheduler's own
/// tenant resolution rejects unknown names independently.
#[tokio::test]
async fn unknown_tenant_fails_open() -> anyhow::Result<()> {
    let mut h = GatewaySession::new_with_tenant_handshake("team-unseeded").await?;

    h.scheduler.set_outcome(MockSchedulerOutcome {
        send_completed: true,
        ..Default::default()
    });

    // NO tenant_quotas entry → MockStore returns NOT_FOUND →
    // QuotaCache caches the negative → classify → Unlimited.

    let drv_path = "/nix/store/00000000000000000000000000000000-unseeded.drv";
    h.store
        .seed_with_content(drv_path, TEST_DRV_ATERM.as_bytes());

    wire_send!(&mut h.stream;
        u64: 9,
        strings: &[format!("{drv_path}!out")],
        u64: 0,
    );

    drain_stderr_until_last(&mut h.stream).await?;
    let result = wire::read_u64(&mut h.stream).await?;
    assert_eq!(
        result, 1,
        "unknown-tenant should pass through (fail-open on NOT_FOUND)"
    );

    h.finish().await;
    Ok(())
}
