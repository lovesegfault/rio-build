//! Ephemeral reconciler spawn-error-handling tests.
//!
//! P0516/P0526: the spawn loop had `Err(e) => return Err(e.into())`
//! at :226 — bailing skipped the status patch. T1 of P0526 extracted
//! the common match into `common::job::try_spawn_job` + `SpawnOutcome`;
//! the loop+log later folded into `common::job::spawn_n` so both
//! reconcilers share warn+continue.
//!
//! The structural guard here proves the bail is gone: `spawn_n` body
//! contains no `return Err`, so the caller's status patch runs even
//! when every spawn fails.

use std::collections::{BTreeMap, HashSet};

use k8s_openapi::api::batch::v1::{Job, JobStatus};
use k8s_openapi::api::core::v1::{Pod, PodStatus};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, ObjectList, ObjectMeta};

use crate::fixtures::{ApiServerVerifier, Scenario};
use crate::reconcilers::pool::job::{
    JobCensus, SpawnOutcome, is_active_job, job_census, orphan_reap_gate, reap_excess_pending,
    spawn_for_each, try_spawn_job,
};
use crate::reconcilers::pool::jobs::{INTENT_SELECTOR_ANNOTATION, reap_stale_for_intents};
use rio_crds::pool::ExecutorKind;
use rio_proto::types::SpawnIntent;

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
    // Same brittleness-is-the-point: anyone reintroducing a
    // bail in spawn_n trips this and must consciously decide the
    // caller's status patch can be skipped.
    let src = include_str!("../job.rs");
    let fn_start = src
        .find("pub(super) async fn spawn_for_each(")
        .expect("spawn_for_each present in job.rs");
    let fn_end = src[fn_start..]
        .find("\n}\n")
        .map(|i| i + fn_start)
        .expect("spawn_for_each body terminates");
    // Filter comment lines: the Failed arm's doc says "was `return
    // Err(e.into())`" to explain the history — we want CODE matches
    // only. treefmt/rustfmt normalizes comment indent, so `trim_start
    // → starts_with("//")` is stable.
    let body: String = src[fn_start..fn_end]
        .lines()
        .filter(|l| !l.trim_start().starts_with("//"))
        .collect::<Vec<_>>()
        .join("\n");

    assert!(
        !body.contains("return Err"),
        "spawn_for_each must warn+continue on create error, not bail — \
         bailing skips the caller's patch_job_pool_status."
    );
    assert!(
        body.contains("ephemeral Job spawn failed; continuing tick"),
        "spawn_for_each should warn on create error (SpawnOutcome::Failed arm)"
    );
}

/// `try_spawn_job` classifies a non-409 API error as `Failed`, not
/// a panic or unhandled propagation. The whole point of the enum
/// (vs `Result`) is that `Failed` forces inline handling — a `?`
/// at a call site is a type error.
///
/// 403 Forbidden stands in for "quota exceeded" — the P0516 scenario.
/// ResourceQuota on
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

/// Mock response for the live `pods.list(job-name=...)` re-check
/// inside `reap_excess_pending`. `phase=None` covers Pending /
/// ContainerCreating; `phase=Some("Running")` triggers the skip.
fn pod_list_scenario(job: &'static str, phase: Option<&str>) -> Scenario {
    let pod = phase.map(|p| Pod {
        metadata: ObjectMeta {
            name: Some(format!("{job}-abcde")),
            ..Default::default()
        },
        status: Some(PodStatus {
            phase: Some(p.into()),
            ..Default::default()
        }),
        ..Default::default()
    });
    Scenario {
        method: http::Method::GET,
        path_contains: "/namespaces/rio/pods",
        body_contains: None,
        status: 200,
        body_json: serde_json::to_string(&ObjectList::<Pod> {
            items: pod.into_iter().collect(),
            metadata: Default::default(),
            types: Default::default(),
        })
        .unwrap(),
    }
}

// r[verify ctrl.ephemeral.reap-excess-pending+3]
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
    let jobs_api: Api<Job> = Api::namespaced(client.clone(), "rio");
    let pods_api: Api<Pod> = Api::namespaced(client, "rio");

    // newest is 15s — past REAP_PENDING_GRACE (10s) so all 3 pending
    // are eligible by age; the count-vs-queued is what's under test.
    let jobs = vec![
        pending_job("rio-builder-med-newest", 0, 15),
        pending_job("rio-builder-med-running", 1, 40),
        pending_job("rio-builder-med-oldest", 0, 90),
        pending_job("rio-builder-med-mid", 0, 45),
    ];

    // Expect: live pod-list (none) → DELETE oldest, live pod-list
    // (Pending pod) → DELETE mid (oldest-first sort). 404 on the
    // first proves warn+continue (still proceeds to delete the second
    // and still counts only the successful one).
    let guard = verifier.run(vec![
        pod_list_scenario("rio-builder-med-oldest", None),
        Scenario::k8s_error(
            http::Method::DELETE,
            "/namespaces/rio/jobs/rio-builder-med-oldest",
            404,
            "NotFound",
            "jobs.batch \"rio-builder-med-oldest\" not found",
        ),
        pod_list_scenario("rio-builder-med-mid", Some("Pending")),
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

    let reaped = reap_excess_pending(
        &jobs_api,
        &pods_api,
        &jobs,
        &HashSet::new(),
        Some(1),
        "med-pool",
    )
    .await;
    guard.verified().await;

    assert_eq!(reaped, 1, "404 not counted; one successful delete");
    assert_eq!(
        recorder.get("rio_controller_ephemeral_jobs_reaped_total{pool=med-pool}"),
        1,
        "metric incremented with pool label; saw keys: {:?}",
        recorder.all_keys(),
    );
    // HELP text + observability.md claim only `pool` — assert no other
    // label sneaks in (regression guard for the phantom-`class` drift).
    let reap_keys: Vec<_> = recorder
        .all_keys()
        .into_iter()
        .filter(|k| k.starts_with("rio_controller_ephemeral_jobs_reaped_total"))
        .collect();
    for k in &reap_keys {
        assert!(!k.contains("class="), "phantom `class` label emitted: {k}");
        assert_eq!(
            k, "rio_controller_ephemeral_jobs_reaped_total{pool=med-pool}",
            "label set must be exactly {{pool}}"
        );
    }
}

/// m027 deferral: a Pending Job whose `rio.build/intent-selector`
/// annotation no longer matches the scheduler's current solve (ICE-
/// backoff spot→on-demand) is foreground-deleted; a terminal Job for
/// a wanted intent is background-deleted; a Pending Job whose
/// selector still matches and a Running Job are NOT deleted (the
/// verifier's strict scenario sequence proves no extra DELETE calls
/// go out). bug_045 prerequisite: `reap_stale_for_intents` sees all
/// intents, not a `queued.sub(active)` prefix.
#[tokio::test]
async fn reap_stale_for_intents_selector_drift_and_terminal() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    fn job(name: &str, sel: Option<&str>, ready: i32, succeeded: i32) -> Job {
        Job {
            metadata: ObjectMeta {
                name: Some(name.into()),
                annotations: sel
                    .map(|s| BTreeMap::from([(INTENT_SELECTOR_ANNOTATION.into(), s.into())])),
                ..Default::default()
            },
            status: Some(JobStatus {
                ready: Some(ready),
                succeeded: Some(succeeded),
                ..Default::default()
            }),
            ..Default::default()
        }
    }
    // Pool=p, kind=Builder → job names "rio-builder-p-{suffix}".
    let existing = vec![
        // Pending, selector=spot → drift vs intent's on-demand.
        job(
            "rio-builder-p-aaa",
            Some("karpenter.sh/capacity-type=spot"),
            0,
            0,
        ),
        // Pending, selector=on-demand → matches; the intended dedupe.
        job(
            "rio-builder-p-bbb",
            Some("karpenter.sh/capacity-type=on-demand"),
            0,
            0,
        ),
        // Running, selector=spot → NOT reaped (may hold assignment).
        job(
            "rio-builder-p-ccc",
            Some("karpenter.sh/capacity-type=spot"),
            1,
            0,
        ),
        // Terminal (succeeded=1), name matches → background-reaped.
        job("rio-builder-p-ddd", None, 0, 1),
        // Pending, NO annotation → drift vs "" (pre-fix Job; reap so
        // it gets re-stamped).
        job("rio-builder-p-eee", None, 0, 0),
    ];
    let intent = |id: &str, cap: &str| SpawnIntent {
        intent_id: id.into(),
        node_selector: [("karpenter.sh/capacity-type".into(), cap.into())].into(),
        ..Default::default()
    };
    let intents = vec![
        intent("aaa", "on-demand"),
        intent("bbb", "on-demand"),
        intent("ccc", "on-demand"),
        intent("ddd", "on-demand"),
        intent("eee", "on-demand"),
    ];

    let guard = verifier.run(vec![
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-p-aaa",
            body_contains: Some(r#""propagationPolicy":"Foreground""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-p-ddd",
            body_contains: Some(r#""propagationPolicy":"Background""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-p-eee",
            body_contains: Some(r#""propagationPolicy":"Foreground""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
    ]);

    let reaped =
        reap_stale_for_intents(&jobs_api, &existing, &intents, "p", ExecutorKind::Builder).await;
    guard.verified().await;
    assert_eq!(
        reaped,
        HashSet::from([
            "rio-builder-p-aaa".into(),
            "rio-builder-p-ddd".into(),
            "rio-builder-p-eee".into(),
        ]),
        "reaped set feeds spawn_for_each skip-filter exclusion"
    );
}

/// Ceiling-saturation livelock: with `ceiling=2` and BOTH active
/// slots occupied by selector-drifted Pending Jobs, headroom=0. The
/// reconciler used to pass the headroom-truncated (empty) slice to
/// `reap_stale_for_intents`, hitting its `want.is_empty()` early-
/// return → nothing reaped → headroom stays 0 forever.
///
/// Fix: reap sees the FULL intent set (reaping frees slots, doesn't
/// consume headroom). This test drives reap+spawn the way the
/// reconciler now does: reap over full intents → both DELETEs fire →
/// reaped names excluded from skip-set → spawn issues both creates.
#[tokio::test]
async fn reap_stale_at_ceiling_saturation() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let drifted = |name: &str| Job {
        metadata: ObjectMeta {
            name: Some(name.into()),
            annotations: Some(BTreeMap::from([(
                INTENT_SELECTOR_ANNOTATION.into(),
                "karpenter.sh/capacity-type=spot".into(),
            )])),
            ..Default::default()
        },
        status: Some(JobStatus {
            ready: Some(0),
            ..Default::default()
        }),
        ..Default::default()
    };
    // ceiling=2, active=2 → headroom=0; both drifted vs on-demand.
    let existing = vec![drifted("rio-builder-p-aaa"), drifted("rio-builder-p-bbb")];
    let intent = |id: &str| SpawnIntent {
        intent_id: id.into(),
        node_selector: [("karpenter.sh/capacity-type".into(), "on-demand".into())].into(),
        ..Default::default()
    };
    let intents = vec![intent("aaa"), intent("bbb")];

    let guard = verifier.run(vec![
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-p-aaa",
            body_contains: Some(r#""propagationPolicy":"Foreground""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
        Scenario {
            method: http::Method::DELETE,
            path_contains: "/namespaces/rio/jobs/rio-builder-p-bbb",
            body_contains: Some(r#""propagationPolicy":"Foreground""#),
            status: 200,
            body_json: serde_json::to_string(&Job::default()).unwrap(),
        },
        Scenario::ok(
            http::Method::POST,
            "/namespaces/rio/jobs",
            serde_json::to_string(&Job::default()).unwrap(),
        ),
        Scenario::ok(
            http::Method::POST,
            "/namespaces/rio/jobs",
            serde_json::to_string(&Job::default()).unwrap(),
        ),
    ]);

    // ceiling=2, active=2 → pre-reap headroom=0.
    let census = job_census(&existing);
    assert_eq!(census.headroom(Some(2), 0), 0);
    // Reap over the FULL intent set (NOT a headroom-truncated slice).
    let reaped =
        reap_stale_for_intents(&jobs_api, &existing, &intents, "p", ExecutorKind::Builder).await;
    assert_eq!(reaped.len(), 2, "both drifted Pending reaped");
    // Freed = reaped that were active → headroom=2 post-reap.
    let freed = existing
        .iter()
        .filter(|j| {
            is_active_job(j)
                && j.metadata
                    .name
                    .as_deref()
                    .is_some_and(|n| reaped.contains(n))
        })
        .count() as i32;
    let headroom = census.headroom(Some(2), freed);

    // Skip-set = existing names minus reaped → empty → spawn fires
    // for both intents this tick.
    let skip: HashSet<String> = existing
        .iter()
        .filter_map(|j| j.metadata.name.clone())
        .filter(|n| !reaped.contains(n))
        .collect();
    let to_spawn: Vec<_> = intents
        .iter()
        .filter(|i| !skip.contains(&format!("rio-builder-p-{}", i.intent_id)))
        .take(headroom)
        .cloned()
        .collect();
    let spawned = spawn_for_each(&jobs_api, &to_spawn, &skip, "p", |i| {
        Ok(Job {
            metadata: ObjectMeta {
                name: Some(format!("rio-builder-p-{}", i.intent_id)),
                ..Default::default()
            },
            ..Default::default()
        })
    })
    .await;
    assert_eq!(spawned.len(), 2, "spawn fires post-reap (skip-set empty)");
    guard.verified().await;
}

/// `spawn_for_each` skips intents whose Job name is already in the
/// existing-names set: no `create()` issued → no per-tick 409 churn
/// for steady-state Running Jobs. The verifier's strict scenario
/// sequence proves exactly ONE POST goes out (for the new intent).
#[tokio::test]
async fn spawn_for_each_skips_existing_names() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let intents = vec![
        SpawnIntent {
            intent_id: "exists".into(),
            ..Default::default()
        },
        SpawnIntent {
            intent_id: "fresh".into(),
            ..Default::default()
        },
    ];
    let skip = HashSet::from(["rio-builder-p-exists".to_owned()]);

    let guard = verifier.run(vec![Scenario {
        method: http::Method::POST,
        path_contains: "/namespaces/rio/jobs",
        body_contains: Some(r#""name":"rio-builder-p-fresh""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);

    let spawned = spawn_for_each(&jobs_api, &intents, &skip, "p", |i| {
        Ok(Job {
            metadata: ObjectMeta {
                name: Some(format!("rio-builder-p-{}", i.intent_id)),
                ..Default::default()
            },
            ..Default::default()
        })
    })
    .await;
    assert_eq!(spawned.len(), 1, "existing skipped; only fresh spawned");
    assert_eq!(
        spawned[0].intent_id, "fresh",
        "skip_existing hits omitted from ack-set; pending_job_names re-ack covers them"
    );
    guard.verified().await;
}

/// `spawn_for_each` returns ONLY intents whose Job was `Spawned`.
/// `Failed` AND `NameCollision` entries are omitted so the caller does
/// not ack them. Acking a failed spawn arms the scheduler's ICE timer
/// for a Job that will never heartbeat → false ICE mark on the
/// `(band, cap)` cell. A 409 on the post-reap path (selector-drift
/// reap → create same name → 409 against still-terminating old Job)
/// has the same shape: the Job that exists won't heartbeat for the
/// new selector. The rare healthy-collision (list-race) is covered by
/// next tick's `pending_job_names` re-ack once the Job lists.
#[tokio::test]
async fn spawn_for_each_acks_spawned_only() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let intents = vec![
        SpawnIntent {
            intent_id: "quota".into(),
            ..Default::default()
        },
        SpawnIntent {
            intent_id: "ok".into(),
            ..Default::default()
        },
        SpawnIntent {
            intent_id: "exists".into(),
            ..Default::default()
        },
    ];
    let skip = HashSet::new();

    // First create → 403 (quota), second → 200, third → 409.
    let guard = verifier.run(vec![
        Scenario::k8s_error(
            http::Method::POST,
            "/namespaces/rio/jobs",
            403,
            "Forbidden",
            "jobs.batch is forbidden: exceeded quota",
        ),
        Scenario::ok(
            http::Method::POST,
            "/namespaces/rio/jobs",
            serde_json::to_string(&Job::default()).unwrap(),
        ),
        Scenario::k8s_error(
            http::Method::POST,
            "/namespaces/rio/jobs",
            409,
            "AlreadyExists",
            "jobs.batch \"rio-builder-p-exists\" already exists",
        ),
    ]);

    let spawned = spawn_for_each(&jobs_api, &intents, &skip, "p", |i| {
        Ok(Job {
            metadata: ObjectMeta {
                name: Some(format!("rio-builder-p-{}", i.intent_id)),
                ..Default::default()
            },
            ..Default::default()
        })
    })
    .await;

    let ids: Vec<_> = spawned.iter().map(|i| i.intent_id.as_str()).collect();
    assert_eq!(
        ids,
        vec!["ok"],
        "Failed (403) AND NameCollision (409 — post-reap terminating Job) \
         omitted; only Spawned acked"
    );
    guard.verified().await;
}

// r[verify ctrl.ephemeral.reap-excess-pending+3]
/// `pending <= queued` → no DELETE calls; `queued = None` (scheduler
/// unreachable) → no DELETE calls. The verifier's empty scenario list
/// asserts zero apiserver requests in both cases.
#[tokio::test]
async fn reap_excess_pending_noop_when_covered_or_unknown() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client.clone(), "rio");
    let pods_api: Api<Pod> = Api::namespaced(client, "rio");

    let jobs = vec![
        pending_job("a", 0, 30),
        pending_job("b", 0, 60),
        pending_job("running", 1, 90),
    ];
    let none = HashSet::new();
    let guard = verifier.run(vec![]);
    // pending=2, queued=2 → covered.
    assert_eq!(
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &none, Some(2), "p").await,
        0
    );
    // queued=None → fail-closed (scheduler unreachable; spawn treats
    // as 0 fail-open, reap MUST NOT — would nuke every Pending Job
    // on a scheduler restart).
    assert_eq!(
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &none, None, "p").await,
        0
    );
    guard.verified().await;
}

// r[verify ctrl.ephemeral.reap-excess-pending+3]
/// Cold-start race: snapshot says `JobStatus.ready==0` (informer lag)
/// but the live pod-phase re-check sees `Running` → DELETE is skipped.
/// Also covers fail-closed on lookup error: a 500 on the pod-list →
/// skip with warn, no DELETE. The verifier's strict sequence (two
/// pod-list GETs, zero DELETEs) proves both.
#[tokio::test]
async fn reap_excess_pending_skips_live_running_pod() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client.clone(), "rio");
    let pods_api: Api<Pod> = Api::namespaced(client, "rio");

    // Both past REAP_PENDING_GRACE; queued=0 → both selected as
    // excess, oldest-first.
    let jobs = vec![
        pending_job("rio-builder-x86-64-coldstart", 0, 50),
        pending_job("rio-builder-x86-64-listfail", 0, 30),
    ];

    let guard = verifier.run(vec![
        pod_list_scenario("rio-builder-x86-64-coldstart", Some("Running")),
        Scenario::k8s_error(
            http::Method::GET,
            "/namespaces/rio/pods",
            500,
            "InternalError",
            "etcd unavailable",
        ),
    ]);

    let reaped =
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &HashSet::new(), Some(0), "p").await;
    guard.verified().await;
    assert_eq!(reaped, 0, "Running pod and list-error both skip DELETE");
}

// r[verify ctrl.pool.reconcile]
/// `job_census` excludes terminating Jobs from `active` (a Job
/// foreground-deleted on a prior tick doesn't burn a headroom slot for
/// up to TGPS=7200s) and computes `ready` distinctly from `active`
/// (`PoolStatus.ready_replicas` = "passed readinessProbe", NOT "all
/// non-terminal"). Before JobCensus, `is_active_job` (no
/// `deletion_timestamp` filter) was used for both.
#[test]
fn job_census_excludes_terminating_and_distinguishes_ready() {
    use k8s_openapi::jiff::Timestamp;
    let mut terminating = pending_job("terminating", 1, 60);
    terminating.metadata.deletion_timestamp = Some(Time(Timestamp::now()));
    let mut complete = pending_job("complete", 0, 60);
    complete.status.as_mut().unwrap().succeeded = Some(1);
    let jobs = vec![
        pending_job("pending", 0, 30),
        pending_job("running", 1, 60),
        terminating,
        complete,
    ];
    let c = job_census(&jobs);
    assert_eq!(
        c.active, 2,
        "terminating + complete excluded from active (was 3 with bare is_active_job)"
    );
    assert_eq!(
        c.ready, 1,
        "ready counts only is_running_job (was passed as `active` before)"
    );
}

/// `JobCensus::headroom` recomputes from `(active − freed)` BEFORE
/// the 0-clamp. The pre-JobCensus `clamp(ceiling − active) + freed`
/// form lost the negative magnitude clamped away: ceiling=10 with
/// active=12 (operator lowered `maxConcurrent` while Jobs live) and
/// freed=12 (selector-drift reaps all) computed `0 + 12 = 12` —
/// overshoots the cap. The single-subtraction form computes `10 −
/// (12 − 12) = 10`.
#[test]
fn headroom_recompute_never_exceeds_ceiling() {
    let c = JobCensus {
        active: 12,
        ready: 0,
    };
    assert_eq!(
        c.headroom(Some(10), 12),
        10,
        "over-committed pool freed=12 must not overshoot ceiling=10"
    );
    assert_eq!(c.headroom(Some(10), 0), 0, "no freed → headroom stays 0");
    assert_eq!(c.headroom(Some(10), 3), 1, "partial free → ceiling − 9 = 1");
    assert_eq!(c.headroom(None, 0), usize::MAX, "uncapped");
}

// r[verify ctrl.ephemeral.reap-excess-pending+3]
/// `reap_stale_for_intents` reaps Pending Jobs whose intent left the
/// set (orphan-by-intent). Before, only `select_excess_pending`'s
/// oldest-first reap caught these, so [A,B,C,D]→[A,B] reaped jA,jB
/// (oldest, still-live, losing in-flight Karpenter provisioning) while
/// orphans jC,jD survived ≥1 extra tick. Running orphans are NOT
/// reaped (`reap_orphan_running` owns them). `intents=[]` is the
/// fail-closed gate via `want.is_empty()`.
#[tokio::test]
async fn reap_stale_for_intents_reaps_orphan_pending() {
    let (client, verifier) = ApiServerVerifier::new();
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let intent = |id: &str| SpawnIntent {
        intent_id: id.into(),
        ..Default::default()
    };
    // jA,jB Pending old (live, matching selector ""); jC Pending old
    // (orphan); jD Running (orphan, NOT reaped here).
    let job = |name: &str, ready: i32| {
        let mut j = pending_job(name, ready, 30);
        j.metadata.annotations = Some(BTreeMap::from([(
            INTENT_SELECTOR_ANNOTATION.into(),
            "".into(),
        )]));
        j
    };
    let existing = vec![
        job("rio-builder-p-aaa", 0),
        job("rio-builder-p-bbb", 0),
        job("rio-builder-p-ccc", 0),
        job("rio-builder-p-ddd", 1),
    ];
    let intents = vec![intent("aaa"), intent("bbb")];

    // Only jC reaped: jA/jB in `want` with matching (default→None)
    // selector → continue; jD Running → `is_pending_job` false.
    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-builder-p-ccc",
        body_contains: Some(r#""propagationPolicy":"Foreground""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);
    let reaped =
        reap_stale_for_intents(&jobs_api, &existing, &intents, "p", ExecutorKind::Builder).await;
    assert_eq!(reaped, HashSet::from(["rio-builder-p-ccc".into()]));
    guard.verified().await;

    // Fail-closed: intents=[] → want.is_empty() early-return → no
    // reap (scheduler error must not nuke every Pending Job).
    let (client2, verifier2) = ApiServerVerifier::new();
    let jobs_api2: Api<Job> = Api::namespaced(client2, "rio");
    let guard = verifier2.run(vec![]);
    let reaped =
        reap_stale_for_intents(&jobs_api2, &existing, &[], "p", ExecutorKind::Builder).await;
    assert!(reaped.is_empty(), "scheduler error → no orphan-reap");
    guard.verified().await;
}

// r[verify ctrl.ephemeral.reap-orphan-running+3]
/// `orphan_reap_gate` returns `None` (skip reap) for `Err` AND
/// `Ok(empty)`. On 2-replica scheduler failover the new leader returns
/// `Ok([])` until workers reconnect; `select_orphan_running`'s
/// `None => true` arm would foreground-delete every Running Job past
/// the 5-min grace.
#[test]
fn orphan_reap_gate_failclosed_on_empty_and_err() {
    use rio_proto::types::ListExecutorsResponse;
    let resp = |execs: Vec<rio_proto::types::ExecutorInfo>| ListExecutorsResponse {
        executors: execs,
        // Past ORPHAN_REAP_GRACE (300s) so the leader-age arm is NOT
        // what's under test here.
        leader_for_secs: 600,
    };
    assert!(
        orphan_reap_gate(Ok(resp(vec![])), "p").is_none(),
        "Ok(empty) → None (new-leader pre-reconnect)"
    );
    assert!(
        orphan_reap_gate(Err(tonic::Status::unavailable("standby")), "p").is_none(),
        "Err → None (unreachable)"
    );
    let exec = rio_proto::types::ExecutorInfo {
        executor_id: "x".into(),
        ..Default::default()
    };
    assert!(
        orphan_reap_gate(Ok(resp(vec![exec])), "p").is_some(),
        "Ok(nonempty, old leader) → Some"
    );
}

// r[verify ctrl.ephemeral.reap-orphan-running+3]
// r[verify sched.admin.list-executors-leader-age]
/// bug_073: during scheduler failover, `self.executors` fills
/// INCREMENTALLY as workers reconnect over a 1-10s spread. A
/// non-empty PARTIAL list cannot prove absence —
/// `select_orphan_running`'s `None => true` arm would
/// foreground-delete every not-yet-reconnected worker's Running Job
/// mid-build. `leader_for_secs < ORPHAN_REAP_GRACE` → fail-closed.
#[test]
fn orphan_reap_gate_failclosed_on_young_leader() {
    use rio_proto::types::{ExecutorInfo, ListExecutorsResponse};
    let one = vec![ExecutorInfo {
        executor_id: "a-1-pqrst".into(),
        ..Default::default()
    }];
    // 10s into failover: one foreign-pool worker reconnected. Gate
    // MUST NOT pass — pool-B workers haven't reconnected yet.
    assert!(
        orphan_reap_gate(
            Ok(ListExecutorsResponse {
                executors: one.clone(),
                leader_for_secs: 10
            }),
            "pool-b"
        )
        .is_none(),
        "young leader (10s < 300s grace) → None even with non-empty list"
    );
    // Past grace: every worker has had ORPHAN_REAP_GRACE to reconnect;
    // absence is now meaningful.
    assert!(
        orphan_reap_gate(
            Ok(ListExecutorsResponse {
                executors: one,
                leader_for_secs: 301
            }),
            "pool-b"
        )
        .is_some(),
        "leader past grace (301s ≥ 300s) → Some"
    );
    // Past grace BUT empty: still fail-closed (belt-and-suspenders).
    assert!(
        orphan_reap_gate(
            Ok(ListExecutorsResponse {
                executors: vec![],
                leader_for_secs: 600
            }),
            "pool-b"
        )
        .is_none(),
        "empty still gates regardless of leader age"
    );
}
