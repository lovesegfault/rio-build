//! Integration tests for the wrong-kind gate (ADR-019 defense-in-depth).
//!
//! The scheduler's `hard_filter` should never misroute, but a bug or
//! stale-generation race must not grant a builder internet access. The
//! gate re-derives `is_fod` from the `.drv` itself and refuses
//! cross-kind assignments BEFORE overlay setup or build execution.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rio_builder::executor::{DEFAULT_BUILD_TIMEOUT, ExecutorEnv, ExecutorError, execute_build};
use rio_builder::log_stream::LogLimits;
use rio_proto::StoreServiceClient;
use rio_proto::types::ExecutorKind;
use rio_proto::types::WorkAssignment;

/// Minimal non-FOD ATerm: empty hashAlgo/hash in the output tuple →
/// `Derivation::is_fixed_output()` returns `false`.
const NON_FOD_DRV: &[u8] = br#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-simple-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple-test"),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-simple-test"),("system","x86_64-linux")])"#;

/// Minimal FOD ATerm: `sha256` + hash populated → `is_fixed_output()`
/// returns `true`.
const FOD_DRV: &[u8] = br#"Derive([("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/1a4dmaqd1jgkj2kk6azvzqlvk8qvpq31-fixed"),("outputHash","abcdef0123456789"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;

fn make_env(kind: ExecutorKind, dir: &std::path::Path) -> ExecutorEnv {
    ExecutorEnv {
        fuse_mount_point: dir.to_path_buf(),
        overlay_base_dir: dir.to_path_buf(),
        executor_id: "test-executor".into(),
        log_limits: LogLimits::UNLIMITED,
        build_timeout: DEFAULT_BUILD_TIMEOUT,
        max_silent_time: 0,
        cgroup_parent: dir.to_path_buf(),
        executor_kind: kind,
        systems: Arc::from(["x86_64-linux".into()]),
        hw_class: None,
        fuse_cache: None,
        fuse_fetch_timeout: std::time::Duration::from_secs(60),
        cancelled: Arc::new(AtomicBool::new(false)),
        sandbox: Arc::new(rio_builder::executor::SandboxEnvConfig::default()),
    }
}

/// `assignment_flag` is what the SCHEDULER claims (`is_fixed_output`).
/// The gate must ignore this and re-derive from `drv_content` — the
/// `*_ignores_lying_scheduler_flag_*` tests below pass a flag that LIES
/// to prove that.
fn make_assignment(drv_content: &[u8], assignment_flag: bool) -> WorkAssignment {
    WorkAssignment {
        drv_path: rio_test_support::fixtures::test_drv_path("kind-gate"),
        drv_content: drv_content.to_vec(),
        is_fixed_output: assignment_flag,
        assignment_token: "tok".into(),
        ..Default::default()
    }
}

async fn run(kind: ExecutorKind, drv: &[u8], assignment_flag: bool) -> Result<(), ExecutorError> {
    let dir = tempfile::tempdir().unwrap();
    let env = make_env(kind, dir.path());
    let assignment = make_assignment(drv, assignment_flag);
    // dead_channel: never dials — the gate fires before any gRPC call.
    let mut store = StoreServiceClient::new(rio_test_support::grpc::dead_channel());
    let (raw_log_tx, _rx) = tokio::sync::mpsc::channel(1);
    let log_tx = rio_builder::log_stream::SheddingLogSender::new(raw_log_tx);
    execute_build(&assignment, &env, &mut store, &log_tx, 0)
        .await
        .result
        .map(|_| ())
}

// r[verify builder.executor.kind-gate]
#[tokio::test]
async fn wrong_kind_fetcher_refuses_non_fod() {
    let err = run(ExecutorKind::Fetcher, NON_FOD_DRV, false)
        .await
        .expect_err("fetcher must refuse non-FOD");
    let ExecutorError::WrongKind {
        is_fod,
        executor_kind,
    } = err
    else {
        panic!("expected WrongKind, got {err:?}");
    };
    assert!(!is_fod);
    assert_eq!(executor_kind, ExecutorKind::Fetcher);
}

#[tokio::test]
async fn wrong_kind_builder_refuses_fod() {
    let err = run(ExecutorKind::Builder, FOD_DRV, true)
        .await
        .expect_err("builder must refuse FOD (airgap boundary)");
    let ExecutorError::WrongKind {
        is_fod,
        executor_kind,
    } = err
    else {
        panic!("expected WrongKind, got {err:?}");
    };
    assert!(is_fod);
    assert_eq!(executor_kind, ExecutorKind::Builder);
}

/// Sanity: matching kind proceeds PAST the gate. We can't assert
/// success (overlay setup needs CAP_SYS_ADMIN), but the error must NOT
/// be `WrongKind` — it should be `Overlay` (the next step) or later.
#[tokio::test]
async fn wrong_kind_gate_passes_on_match() {
    for (kind, drv, is_fod) in [
        (ExecutorKind::Builder, NON_FOD_DRV, false),
        (ExecutorKind::Fetcher, FOD_DRV, true),
    ] {
        // Any outcome OTHER than WrongKind means the gate let it
        // through. The test environment lacks CAP_SYS_ADMIN so overlay
        // setup fails downstream — that's expected.
        if let Err(ExecutorError::WrongKind { .. }) = run(kind, drv, is_fod).await {
            panic!("matching kind {kind:?} should pass the gate");
        }
    }
}

/// wkr-fod-flag-trust: gate uses drv-derived is_fod, NOT the
/// scheduler-sent flag. Scheduler mislabels non-FOD as
/// `is_fixed_output=true` → Fetcher must STILL refuse (drv says non-FOD).
///
/// Kills the mutation `let is_fod = assignment.is_fixed_output`: under
/// it, `is_fod=true`, gate `true!=true` passes → `expect_err` fails.
// r[verify builder.executor.kind-gate]
#[tokio::test]
async fn wrong_kind_gate_ignores_lying_scheduler_flag_fetcher() {
    let err = run(
        ExecutorKind::Fetcher,
        NON_FOD_DRV,
        /*assignment_flag=*/ true,
    )
    .await
    .expect_err("fetcher must refuse non-FOD regardless of scheduler flag");
    let ExecutorError::WrongKind { is_fod, .. } = err else {
        panic!("expected WrongKind, got {err:?}")
    };
    assert!(
        !is_fod,
        "gate must report drv-derived is_fod=false, not scheduler's true"
    );
}

/// Mirror: scheduler mislabels FOD as `is_fixed_output=false` → Builder
/// must STILL refuse (drv says FOD; airgap boundary).
///
/// Kills the mutation `let is_fod = assignment.is_fixed_output`: under
/// it, `is_fod=false`, gate `false!=false` passes → `expect_err` fails.
#[tokio::test]
async fn wrong_kind_gate_ignores_lying_scheduler_flag_builder() {
    let err = run(
        ExecutorKind::Builder,
        FOD_DRV,
        /*assignment_flag=*/ false,
    )
    .await
    .expect_err("builder must refuse FOD regardless of scheduler flag");
    let ExecutorError::WrongKind { is_fod, .. } = err else {
        panic!("expected WrongKind, got {err:?}")
    };
    assert!(
        is_fod,
        "gate must report drv-derived is_fod=true, not scheduler's false"
    );
}

/// The daemon-transient retry loop calls `execute_build` up to
/// `DAEMON_RETRY_MAX + 1` times for one assignment. The `rio:` banner
/// header MUST be sent only on the first attempt (`first_line == 0`);
/// retried attempts continue line numbering from the prior attempt's
/// `final_line_count` — re-emitting the header at line 0 would break
/// the scheduler ring buffer's line-number monotonicity and write
/// duplicate "first lines" for one exec_id (bug_013).
///
/// Fixture trace:
/// `make_env` sets `fuse_mount_point == overlay_base_dir` (same
/// tempdir) so `setup_overlay`'s `lower_dev == upper_dev` check fails
/// deterministically with `OverlayError::SameFilesystem` — no
/// CAP_SYS_ADMIN needed and no chance the build proceeds past the
/// pre-daemon block. The header send is BEFORE that check
/// (executor/mod.rs ~545); the failure happens AFTER (~575); the
/// channel observes exactly the header-or-nothing.
// r[verify obs.log.worker-header]
#[tokio::test]
async fn banner_header_gated_on_first_attempt() {
    use rio_proto::types::executor_message;

    let dir = tempfile::tempdir().unwrap();
    let env = make_env(ExecutorKind::Builder, dir.path());
    let assignment = make_assignment(NON_FOD_DRV, false);
    let mut store = StoreServiceClient::new(rio_test_support::grpc::dead_channel());

    // First attempt (`first_line == 0`): header at line 0.
    let (raw_log_tx, mut rx) = tokio::sync::mpsc::channel(8);
    let log_tx = rio_builder::log_stream::SheddingLogSender::new(raw_log_tx);
    let outcome = execute_build(&assignment, &env, &mut store, &log_tx, 0).await;
    drop(log_tx);
    assert!(
        outcome.result.is_err(),
        "test layout has no overlay-capable filesystem; build must not proceed"
    );
    assert_eq!(
        outcome.final_line_count, 3,
        "header occupies lines 0..3 (banner::HEADER_LINE_COUNT)"
    );
    assert!(
        outcome.footer_result.is_none(),
        "no daemon ran; runtime must not send a footer"
    );
    let msg = rx
        .recv()
        .await
        .expect("header batch must be on the channel");
    let batch = match msg.msg.unwrap() {
        executor_message::Msg::LogBatch(b) => b,
        other => panic!("expected LogBatch, got {other:?}"),
    };
    assert_eq!(batch.first_line_number, 0);
    assert_eq!(batch.lines.len(), 3);
    assert!(
        std::str::from_utf8(&batch.lines[0])
            .unwrap()
            .starts_with("rio: exec"),
        "first banner line is the `rio: exec` marker"
    );
    assert!(
        rx.recv().await.is_none(),
        "exactly one banner batch on first attempt"
    );

    // Retry attempt (`first_line > 0`): no header re-sent; offset held.
    let (raw_log_tx, mut rx) = tokio::sync::mpsc::channel(8);
    let log_tx = rio_builder::log_stream::SheddingLogSender::new(raw_log_tx);
    let outcome = execute_build(&assignment, &env, &mut store, &log_tx, 3).await;
    drop(log_tx);
    assert!(outcome.result.is_err());
    assert_eq!(
        outcome.final_line_count, 3,
        "retry attempt with no daemon output must hold the line offset"
    );
    assert!(outcome.footer_result.is_none());
    assert!(
        rx.recv().await.is_none(),
        "header must NOT be re-sent on a retried attempt"
    );
}

/// Pre-header early returns (drv parse failure, WrongKind) never emit a
/// banner: `final_line_count` is the caller-supplied `first_line` (no
/// lines pushed) and `footer_result` is `None`. Verified for both
/// first-attempt (`first_line == 0`) and retry (`first_line > 0`)
/// because these errors are not daemon-transient: the runtime won't
/// retry them, but the contract is on `pre_cgroup` regardless.
// r[verify obs.log.worker-header]
#[tokio::test]
async fn pre_header_error_carries_caller_offset() {
    let dir = tempfile::tempdir().unwrap();
    let env = make_env(ExecutorKind::Fetcher, dir.path()); // wrong kind for NON_FOD_DRV
    let assignment = make_assignment(NON_FOD_DRV, false);
    let mut store = StoreServiceClient::new(rio_test_support::grpc::dead_channel());

    for first_line in [0u64, 7u64] {
        let (raw_log_tx, mut rx) = tokio::sync::mpsc::channel(1);
        let log_tx = rio_builder::log_stream::SheddingLogSender::new(raw_log_tx);
        let outcome = execute_build(&assignment, &env, &mut store, &log_tx, first_line).await;
        drop(log_tx);
        assert!(matches!(
            outcome.result,
            Err(ExecutorError::WrongKind { .. })
        ));
        assert_eq!(
            outcome.final_line_count, first_line,
            "WrongKind is pre-header: no lines pushed, offset unchanged"
        );
        assert!(outcome.footer_result.is_none());
        assert!(rx.recv().await.is_none(), "no banner before the kind gate");
    }
}
