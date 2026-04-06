//! Ephemeral reconciler spawn-error-handling tests.
//!
//! Sibling to `manifest_tests::spawn_loop_no_early_return_on_error`
//! (P0516): ephemeral.rs had the exact same `Err(e) => return
//! Err(e.into())` bail at :226 that P0516 removed from manifest.rs.
//! Both were rewritten together over job_common at ea64f7f2; manifest
//! got the fix, ephemeral didn't. T1 of P0526 extracted the common
//! match into `job_common::try_spawn_job` + `SpawnOutcome` so both
//! reconcilers share warn+continue.
//!
//! The structural guard here proves the bail is gone from the spawn
//! block — no early return between `---- Spawn decision ----` and
//! `---- Status patch ----` means the status patch at :242 runs even
//! when every spawn fails.

use k8s_openapi::api::batch::v1::Job;
use kube::api::Api;

use crate::fixtures::{ApiServerVerifier, Scenario};
use crate::reconcilers::builderpool::job_common::{SpawnOutcome, try_spawn_job};

// r[verify ctrl.pool.ephemeral]
#[test]
fn ephemeral_spawn_fail_still_patches_status() {
    // STRUCTURAL GUARD: the spawn block (between `---- Spawn decision
    // ----` and `---- Status patch ----`) must contain NO early return
    // on error. The pre-fix `return Err(e.into())` at :226 meant
    // `patch_job_pool_status` at :242 never ran on spawn failure —
    // `.status.replicas` stayed stale, operators saw "everything fine"
    // while the pool spawned nothing.
    //
    // Mutation: re-introduce `return Err(e.into())` (or any `return
    // Err`) in the ephemeral Failed arm → this test FAILS.
    //
    // Sibling of manifest_tests::spawn_loop_no_early_return_on_error
    // (P0516). Same brittleness-is-the-point: anyone reintroducing a
    // bail in this block trips this and must consciously decide the
    // status patch can be skipped.
    let src = include_str!("../ephemeral.rs");
    let spawn_start = src
        .find("---- Spawn decision ----")
        .expect("spawn section marker present");
    let spawn_end = src[spawn_start..]
        .find("---- Status patch ----")
        .map(|i| i + spawn_start)
        .expect("status-patch section follows spawn");
    // Filter comment lines: the Failed arm's doc says "Was `return
    // Err(e.into())`" to explain the history — we want CODE matches
    // only. treefmt/rustfmt normalizes comment indent, so `trim_start
    // → starts_with("//")` is stable.
    let spawn_code: String = src[spawn_start..spawn_end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !spawn_code.contains("return Err"),
        "spawn block must warn+continue on create error, not bail — \
         bailing skips patch_job_pool_status at :242. Pre-fix line \
         was `return Err(e.into())` at :226."
    );
    // Positive: the warn message is there (proves the Failed arm
    // exists and does something observable).
    assert!(
        spawn_code.contains("ephemeral Job spawn failed; continuing tick"),
        "spawn block should warn on create error with a grep-able \
         message (SpawnOutcome::Failed arm)"
    );
}

/// `try_spawn_job` classifies a non-409 API error as `Failed`, not
/// a panic or unhandled propagation. The whole point of the enum
/// (vs `Result`) is that `Failed` forces inline handling — a `?`
/// at a call site is a type error.
///
/// 403 Forbidden stands in for "quota exceeded" — the scenario P0516
/// originally fixed on the manifest side. ResourceQuota on
/// `count/jobs.batch` exhausted → spawn returns 403 → pre-fix bail
/// skipped everything downstream.
///
/// This is the mock-jobs_api scaffolding P0522-T2 will extend for
/// the consecutive-fail threshold test (N failing scenarios in
/// sequence).
#[tokio::test]
async fn try_spawn_job_classifies_api_error_as_failed() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let guard = verifier.run(vec![Scenario::k8s_error(
        http::Method::POST,
        "/namespaces/rio/jobs",
        403,
        "Forbidden",
        "jobs.batch is forbidden: exceeded quota",
    )]);

    let job = Job {
        metadata: kube::api::ObjectMeta {
            name: Some("eph-pool-eph-abc123".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    // The assertion: NOT a panic, NOT a propagated Result::Err — the
    // enum variant. Caller (ephemeral spawn loop) pattern-matches
    // this and logs warn+continue.
    match try_spawn_job(&jobs_api, &job).await {
        SpawnOutcome::Failed(kube::Error::Api(ae)) => {
            assert_eq!(ae.code, 403, "error carries original status");
            assert!(
                ae.message.contains("exceeded quota"),
                "error carries original message for the warn! log"
            );
        }
        SpawnOutcome::Failed(e) => panic!(
            "403 should surface as kube::Error::Api, got other \
             kube::Error variant: {e:?}"
        ),
        SpawnOutcome::Spawned => panic!("403 response classified as Spawned"),
        SpawnOutcome::NameCollision => {
            panic!("403 classified as NameCollision (only 409 should)")
        }
    }

    guard.verified().await;
}

/// 409 AlreadyExists → `NameCollision`, not `Failed`. The
/// distinction matters: `Failed` increments P0522's threshold
/// counter; `NameCollision` is expected-noise (random-suffix
/// collision, concurrent reconcile) and must NOT.
#[tokio::test]
async fn try_spawn_job_classifies_409_as_name_collision() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let guard = verifier.run(vec![Scenario::k8s_error(
        http::Method::POST,
        "/namespaces/rio/jobs",
        409,
        "AlreadyExists",
        "jobs.batch \"eph-pool-eph-abc123\" already exists",
    )]);

    let job = Job {
        metadata: kube::api::ObjectMeta {
            name: Some("eph-pool-eph-abc123".into()),
            ..Default::default()
        },
        ..Default::default()
    };

    assert!(
        matches!(
            try_spawn_job(&jobs_api, &job).await,
            SpawnOutcome::NameCollision
        ),
        "409 AlreadyExists MUST classify as NameCollision (debug-log \
         + retry next tick), not Failed (which feeds P0522 threshold)"
    );

    guard.verified().await;
}
