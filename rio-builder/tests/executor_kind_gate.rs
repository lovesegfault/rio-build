//! Integration tests for the wrong-kind gate (ADR-019 defense-in-depth).
//!
//! The scheduler's `hard_filter` should never misroute, but a bug or
//! stale-generation race must not grant a builder internet access. The
//! gate re-derives `is_fod` from the `.drv` itself and refuses
//! cross-kind assignments BEFORE overlay setup or daemon spawn.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use rio_builder::executor::{DEFAULT_DAEMON_TIMEOUT, ExecutorEnv, ExecutorError, execute_build};
use rio_builder::log_stream::LogLimits;
use rio_proto::StoreServiceClient;
use rio_proto::types::ExecutorKind;
use rio_proto::types::WorkAssignment;

/// Minimal non-FOD ATerm: empty hashAlgo/hash in the output tuple →
/// `Derivation::is_fixed_output()` returns `false`.
const NON_FOD_DRV: &[u8] = br#"Derive([("out","/nix/store/abc-simple-test","","")],[],[],"x86_64-linux","/bin/sh",["-c","echo hello > $out"],[("builder","/bin/sh"),("name","simple-test"),("out","/nix/store/abc-simple-test"),("system","x86_64-linux")])"#;

/// Minimal FOD ATerm: `sha256` + hash populated → `is_fixed_output()`
/// returns `true`.
const FOD_DRV: &[u8] = br#"Derive([("out","/nix/store/abc-fixed","sha256","abcdef0123456789")],[],[],"x86_64-linux","/bin/sh",["-c","echo"],[("name","fixed"),("out","/nix/store/abc-fixed"),("outputHash","abcdef0123456789"),("outputHashAlgo","sha256"),("system","x86_64-linux")])"#;

fn make_env(kind: ExecutorKind, dir: &std::path::Path) -> ExecutorEnv {
    ExecutorEnv {
        fuse_mount_point: dir.to_path_buf(),
        overlay_base_dir: dir.to_path_buf(),
        executor_id: "test-executor".into(),
        log_limits: LogLimits::UNLIMITED,
        daemon_timeout: DEFAULT_DAEMON_TIMEOUT,
        max_silent_time: 0,
        cgroup_parent: dir.to_path_buf(),
        executor_kind: kind,
        systems: Arc::from(["x86_64-linux".into()]),
        fuse_cache: None,
        fuse_fetch_timeout: std::time::Duration::from_secs(60),
        cancelled: Arc::new(AtomicBool::new(false)),
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
    let (log_tx, _rx) = tokio::sync::mpsc::channel(1);
    execute_build(&assignment, &env, &mut store, &log_tx)
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
