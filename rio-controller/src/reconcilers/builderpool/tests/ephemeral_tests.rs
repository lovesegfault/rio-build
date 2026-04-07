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

use k8s_openapi::api::batch::v1::{Job, JobStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, ObjectMeta};

use crate::fixtures::{ApiServerVerifier, Scenario};
use crate::reconcilers::builderpool::job_common::{
    SpawnOutcome, reap_excess_pending, try_spawn_job,
};

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
            name: Some("rio-builder-eph-pool-abc123".into()),
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
        "jobs.batch \"rio-builder-eph-pool-abc123\" already exists",
    )]);

    let job = Job {
        metadata: kube::api::ObjectMeta {
            name: Some("rio-builder-eph-pool-abc123".into()),
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

fn pending_job(name: &str, ready: i32, age_s: i64) -> Job {
    use k8s_openapi::jiff::{SignedDuration, Timestamp};
    Job {
        metadata: ObjectMeta {
            name: Some(name.into()),
            creation_timestamp: Some(Time(Timestamp::now() - SignedDuration::from_secs(age_s))),
            ..Default::default()
        },
        status: Some(JobStatus {
            ready: Some(ready),
            ..Default::default()
        }),
        ..Default::default()
    }
}

// r[verify ctrl.ephemeral.reap-excess-pending]
/// I-183: `reap_excess_pending` issues DELETE for the oldest excess
/// Pending Jobs, increments the metric, and warn+continues on a 404
/// (already gone — concurrent reconcile or TTL).
///
/// Scenario: 3 Pending + 1 Running, queued=1 → DELETE the 2 oldest
/// Pending. The Running Job and the newest Pending are NOT deleted —
/// the verifier's strict scenario sequence proves no extra DELETE
/// calls go out (an unexpected request fails the verifier task).
#[tokio::test]
async fn reap_excess_pending_deletes_oldest_and_counts() {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&recorder);

    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // newest is 15s — past REAP_PENDING_GRACE (10s) so all 3 pending
    // are eligible by age; the count-vs-queued is what's under test.
    let jobs = vec![
        pending_job("rio-builder-med-newest", 0, 15),
        pending_job("rio-builder-med-running", 1, 40),
        pending_job("rio-builder-med-oldest", 0, 90),
        pending_job("rio-builder-med-mid", 0, 45),
    ];

    // Expect DELETE oldest, then mid (oldest-first sort). 404 on the
    // first proves warn+continue (still proceeds to delete the second
    // and still counts only the successful one).
    let guard = verifier.run(vec![
        Scenario::k8s_error(
            http::Method::DELETE,
            "/namespaces/rio/jobs/rio-builder-med-oldest",
            404,
            "NotFound",
            "jobs.batch \"rio-builder-med-oldest\" not found",
        ),
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-med-mid",
            // Foreground propagation: Job stays until pod's
            // job-tracking finalizer is processed. See the
            // DeleteParams::foreground() call site for why.
            body_contains: Some(r#""propagationPolicy":"Foreground""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
    ]);

    let reaped = reap_excess_pending(&jobs_api, &jobs, Some(1), "med-pool", "medium").await;
    guard.verified().await;

    assert_eq!(reaped, 1, "404 not counted; one successful delete");
    assert_eq!(
        recorder.get("rio_controller_ephemeral_jobs_reaped_total{class=medium,pool=med-pool}"),
        1,
        "metric incremented with pool+class labels; saw keys: {:?}",
        recorder.all_keys(),
    );
}

// r[verify ctrl.ephemeral.reap-excess-pending]
/// `pending <= queued` → no DELETE calls; `queued = None` (scheduler
/// unreachable) → no DELETE calls. The verifier's empty scenario list
/// asserts zero apiserver requests in both cases.
#[tokio::test]
async fn reap_excess_pending_noop_when_covered_or_unknown() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let jobs = vec![
        pending_job("a", 0, 30),
        pending_job("b", 0, 60),
        pending_job("running", 1, 90),
    ];
    let guard = verifier.run(vec![]);
    // pending=2, queued=2 → covered.
    assert_eq!(
        reap_excess_pending(&jobs_api, &jobs, Some(2), "p", "c").await,
        0
    );
    // queued=None → fail-closed (scheduler unreachable; spawn treats
    // as 0 fail-open, reap MUST NOT — would nuke every Pending Job
    // on a scheduler restart).
    assert_eq!(
        reap_excess_pending(&jobs_api, &jobs, None, "p", "c").await,
        0
    );
    guard.verified().await;
}
