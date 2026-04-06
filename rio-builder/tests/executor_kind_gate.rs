//! Integration tests for the wrong-kind gate (ADR-019 defense-in-depth).
//!
//! The scheduler's `hard_filter` should never misroute, but a bug or
//! stale-generation race must not grant a builder internet access. The
//! gate re-derives `is_fod` from the `.drv` itself and refuses
//! cross-kind assignments BEFORE overlay setup or daemon spawn.

use std::sync::Arc;
use std::sync::atomic::AtomicUsize;

use rio_builder::executor::{DEFAULT_DAEMON_TIMEOUT, ExecutorEnv, ExecutorError, execute_build};
use rio_builder::log_stream::LogLimits;
use rio_proto::StoreServiceClient;
use rio_proto::build_types::WorkAssignment;
use rio_proto::types::ExecutorKind;
use tonic::transport::Channel;

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
        max_leaked_mounts: 3,
        daemon_timeout: DEFAULT_DAEMON_TIMEOUT,
        max_silent_time: 0,
        cgroup_parent: dir.to_path_buf(),
        executor_kind: kind,
    }
}

fn make_assignment(drv_content: &[u8], is_fod: bool) -> WorkAssignment {
    WorkAssignment {
        drv_path: rio_test_support::fixtures::test_drv_path("kind-gate"),
        drv_content: drv_content.to_vec(),
        // Intentionally set the assignment flag to MATCH the drv — the
        // gate uses the re-derived `is_fod`, not this. A mismatch here
        // would trigger the warn! but not change the gate outcome.
        is_fixed_output: is_fod,
        assignment_token: "tok".into(),
        ..Default::default()
    }
}

async fn run(kind: ExecutorKind, drv: &[u8], is_fod: bool) -> Result<(), ExecutorError> {
    let dir = tempfile::tempdir().unwrap();
    let env = make_env(kind, dir.path());
    let assignment = make_assignment(drv, is_fod);
    // connect_lazy: never dials — the gate fires before any gRPC call.
    let channel = Channel::from_static("http://127.0.0.1:1").connect_lazy();
    let mut store = StoreServiceClient::new(channel);
    let (log_tx, _rx) = tokio::sync::mpsc::channel(1);
    let leak = Arc::new(AtomicUsize::new(0));
    execute_build(&assignment, &env, &mut store, &log_tx, &leak)
        .await
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
