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

use k8s_openapi::api::batch::v1::{Job, JobSpec, JobStatus};
use k8s_openapi::api::core::v1::{Pod, PodStatus, PodTemplateSpec};
use k8s_openapi::apimachinery::pkg::apis::meta::v1::Time;
use kube::api::{Api, DeleteParams, ObjectList, ObjectMeta};

use crate::fixtures::{ApiServerVerifier, Scenario};
use crate::reconcilers::pool::job::{
    JobCensus, SpawnOutcome, delete_job_with_synthesized_report, is_active_job, job_census,
    reap_excess_pending, reap_orphan_running, spawn_for_each, synthesized_report_for_job,
    try_spawn_job,
};
use crate::reconcilers::pool::jobs::{
    INTENT_ID_ANNOTATION, INTENT_SELECTOR_ANNOTATION, reap_stale_for_intents,
};
use rio_crds::pool::ExecutorKind;
use rio_proto::types::{AttemptTerminalReason, OpenAttempt, SpawnIntent};

// r[verify ctrl.pool.ephemeral+1]
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
    let ctx = super::test_ctx(client.clone());
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
        &ctx,
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
    // HELP text + observability.typ claim only `pool` — assert no other
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
    let ctx = super::test_ctx(client.clone());
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
        // Pending, fingerprint matches the current intent → the
        // intended dedupe. (Every LEGACY-format annotation drifts once
        // against the v2 RenderInputs fingerprint — the documented
        // one-time post-deploy churn — so "matching" means stamping
        // what `build_job` stamps today.)
        job(
            "rio-builder-p-bbb",
            Some(
                crate::reconcilers::pool::candidate::RenderInputs::from_intent(&SpawnIntent {
                    intent_id: "bbb".into(),
                    node_selector: [("karpenter.sh/capacity-type".into(), "on-demand".into())]
                        .into(),
                    ..Default::default()
                })
                .fingerprint()
                .leak(),
            ),
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

    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &intents,
        &ctx,
        "p",
        ExecutorKind::Builder,
    )
    .await;
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
    let ctx = super::test_ctx(client.clone());
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
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &intents,
        &ctx,
        "p",
        ExecutorKind::Builder,
    )
    .await;
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
    let ctx = super::test_ctx(client.clone());
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
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &none, Some(2), &ctx, "p").await,
        0
    );
    // queued=None → fail-closed (scheduler unreachable; spawn treats
    // as 0 fail-open, reap MUST NOT — would nuke every Pending Job
    // on a scheduler restart).
    assert_eq!(
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &none, None, &ctx, "p").await,
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
    let ctx = super::test_ctx(client.clone());
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

    let reaped = reap_excess_pending(
        &jobs_api,
        &pods_api,
        &jobs,
        &HashSet::new(),
        Some(0),
        &ctx,
        "p",
    )
    .await;
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
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let intent = |id: &str| SpawnIntent {
        intent_id: id.into(),
        ..Default::default()
    };
    // jA,jB Pending old (live, matching selector ""); jC Pending old
    // (orphan); jD Running (orphan, NOT reaped here).
    // Live Jobs carry the CURRENT fingerprint of a default intent (the
    // legacy "" stamp would read as drift under the v2 RenderInputs
    // form and turn this into a drift test instead of an orphan test).
    let default_fp =
        crate::reconcilers::pool::candidate::RenderInputs::from_intent(&SpawnIntent::default())
            .fingerprint();
    let job = |name: &str, ready: i32| {
        let mut j = pending_job(name, ready, 30);
        j.metadata.annotations = Some(BTreeMap::from([(
            INTENT_SELECTOR_ANNOTATION.into(),
            default_fp.clone(),
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
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &intents,
        &ctx,
        "p",
        ExecutorKind::Builder,
    )
    .await;
    assert_eq!(reaped, HashSet::from(["rio-builder-p-ccc".into()]));
    guard.verified().await;

    // Fail-closed: intents=[] → want.is_empty() early-return → no
    // reap (scheduler error must not nuke every Pending Job).
    let (client2, verifier2) = ApiServerVerifier::new();
    let ctx2 = super::test_ctx(client2.clone());
    let jobs_api2: Api<Job> = Api::namespaced(client2, "rio");
    let guard = verifier2.run(vec![]);
    let reaped = reap_stale_for_intents(
        &jobs_api2,
        &existing,
        &[],
        &ctx2,
        "p",
        ExecutorKind::Builder,
    )
    .await;
    assert!(reaped.is_empty(), "scheduler error → no orphan-reap");
    guard.verified().await;
}

// ───────────────────────────────────────────────────────────────────
// Synthesize-on-delete (ctrl.job.synthesize-on-delete)
// ───────────────────────────────────────────────────────────────────

/// A Running Job carrying the `rio.build/intent-id` pod-template
/// annotation, as `build_job` spawns it.
fn running_job_for_intent(name: &str, intent_id: &str) -> Job {
    let mut j = pending_job(name, 1, 600);
    j.spec = Some(JobSpec {
        template: PodTemplateSpec {
            metadata: Some(ObjectMeta {
                annotations: Some(BTreeMap::from([(
                    INTENT_ID_ANNOTATION.to_string(),
                    intent_id.to_string(),
                )])),
                ..Default::default()
            }),
            ..Default::default()
        },
        ..Default::default()
    });
    j
}

/// One open pull-mode attempt as the pull-filtered `ListOpenAttempts`
/// view returns it (executor identity == the attested intent id).
fn pull_attempt(intent_id: &str, exec_id: &str, source_node: &str) -> OpenAttempt {
    OpenAttempt {
        intent_id: intent_id.into(),
        executor_id: intent_id.into(),
        exec_id: exec_id.into(),
        source_node: source_node.into(),
        ..Default::default()
    }
}

/// The expected one-foreground-DELETE scenario for `name`.
pub(super) fn delete_scenario(name: &str) -> Scenario {
    Scenario {
        method: http::Method::DELETE,
        path_contains: Box::leak(format!("/namespaces/rio/jobs/{name}").into_boxed_str()),
        body_contains: Some(r#""propagationPolicy":"Foreground""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }
}

/// Spawn an in-process MockAdmin (acks every unary) and build a Ctx
/// whose admin client points at it, so the synthesized
/// `ReportAttemptOutcome` actually lands and the OA1 histogram sample
/// is observable.
pub(super) async fn ctx_with_mock_admin(
    client: kube::Client,
) -> (
    std::sync::Arc<crate::reconcilers::Ctx>,
    rio_test_support::grpc::MockAdmin,
    tokio::task::JoinHandle<()>,
) {
    let (mock, addr, handle) = rio_test_support::grpc::spawn_mock_admin()
        .await
        .expect("spawn mock admin");
    let channel = tonic::transport::Channel::from_shared(format!("http://{addr}"))
        .expect("mock admin uri")
        .connect_lazy();
    (super::test_ctx_with_admin(client, channel), mock, handle)
}

// r[verify ctrl.job.synthesize-on-delete]
/// The pure synthesize decision: `Some` exactly when an open pull-mode
/// attempt covers the Job (so a no-attempt delete attempts no RPC by
/// construction), carrying the attempt's exec_id / intent and the
/// requested reason; a stream Job mid-build is `None` because the
/// pull-filtered view never lists stream attempts.
#[test]
fn synthesized_report_decision_pull_only() {
    let job = running_job_for_intent("rio-builder-p-pull1", "drv-pull-1");

    // (a) Covered by an open pull-mode attempt → one request, keyed by
    // the attempt's exec_id, carrying the AD2c node attribution. The
    // attempt's executor is the Job's own pod (merged_bug_298: the
    // constructor binds owner identity, not just intent).
    let mut owned = pull_attempt("drv-pull-1", "exec-1", "node-a");
    owned.executor_id = "rio-builder-p-pull1-a1b2c".into();
    let attempts = vec![owned];
    let req = synthesized_report_for_job(&job, AttemptTerminalReason::Reaped, &attempts)
        .expect("open pull attempt → synthesize");
    assert_eq!(req.exec_id, "exec-1");
    assert_eq!(req.intent_id, "drv-pull-1");
    assert_eq!(req.job_name, "rio-builder-p-pull1");
    assert_eq!(req.node_name, "node-a");
    assert_eq!(req.reason, i32::from(AttemptTerminalReason::Reaped));

    // (b) No open attempt → None (no RPC is even attempted).
    assert!(
        synthesized_report_for_job(&job, AttemptTerminalReason::Reaped, &[]).is_none(),
        "no open attempt → nothing synthesized"
    );

    // (c) Mixed fleet: a stream Job mid-build has an active assignment
    // in the ledger, but the pull-filtered view never lists it — the
    // attempt list only carries OTHER (pull) intents → None.
    let stream_job = running_job_for_intent("rio-builder-p-strm1", "drv-stream-9");
    assert!(
        synthesized_report_for_job(
            &stream_job,
            AttemptTerminalReason::Reaped,
            &[pull_attempt("drv-pull-1", "exec-1", "")]
        )
        .is_none(),
        "stream-dispatch Jobs are invisible to the pull-filtered view → no synthesis"
    );
}

// r[verify ctrl.job.synthesize-on-delete]
/// merged_bug_298 red: two Jobs cover one intent (cross-pool respawn).
/// Deleting pool B's Job must bind pool B's OWN attempt — never close
/// pool A's healthy one (charge-free verdict against the wrong
/// executor).
#[test]
fn synthesized_report_binds_owner_not_just_intent() {
    let job_b = running_job_for_intent("rio-builder-b-x", "drv-shared-1");
    let mut a = pull_attempt("drv-shared-1", "exec-a", "node-1");
    a.executor_id = "rio-builder-a-y-7k2fq".into(); // pool A's pod
    let mut b = pull_attempt("drv-shared-1", "exec-b", "node-2");
    b.executor_id = "rio-builder-b-x-9mz4h".into(); // pool B's pod
    let req = synthesized_report_for_job(&job_b, AttemptTerminalReason::Reaped, &[a, b])
        .expect("pool B's own attempt is open");
    assert_eq!(
        req.exec_id, "exec-b",
        "the synthesized verdict must bind the deleting Job's own attempt"
    );
}

// r[verify ctrl.job.synthesize-on-delete]
/// Deleting a Job that still has an open pull-mode attempt synthesizes
/// the `ReportAttemptOutcome` (observed via the OA1 histogram sample
/// the helper records only after the report is ACKED) and still issues
/// the foreground DELETE. The decision fn returning at most one request
/// per delete plus the once-per-Job sample gate carry the
/// "exactly one" half.
#[tokio::test]
async fn delete_job_synthesizes_report_for_open_pull_attempt() {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&recorder);

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let covered = running_job_for_intent("rio-builder-p-pull1", "drv-pull-1");
    // merged_bug_298: the attempt's executor is the Job's own pod.
    let mut owned = pull_attempt("drv-pull-1", "exec-1", "node-a");
    owned.executor_id = "rio-builder-p-pull1-a1b2c".into();
    let attempts = vec![owned];

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-pull1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &covered,
        "rio-builder-p-pull1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &attempts,
    )
    .await
    .expect("delete succeeds");
    guard.verified().await;

    assert!(
        recorder.histogram_touched("rio_controller_job_terminal_report_seconds"),
        "the synthesize→ack→OA1-sample path must have run for the covered Job"
    );
}

// r[verify ctrl.job.synthesize-on-delete]
/// Deleting a Job with NO covering open pull-mode attempt (here: a
/// stream Job mid-build — the pull-filtered view never lists stream
/// attempts) attempts no `ReportAttemptOutcome` at all: with a working
/// mock admin (every report would be acked and sampled), the OA1
/// histogram stays untouched, and the foreground DELETE is the only
/// effect — today's deletion exactly.
#[tokio::test]
async fn delete_job_without_open_attempt_attempts_no_rpc() {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&recorder);

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let stream_job = running_job_for_intent("rio-builder-p-strm1", "drv-stream-9");
    // The pull-filtered view lists only OTHER (pull) intents.
    let attempts = vec![pull_attempt("drv-pull-1", "exec-1", "")];

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-strm1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &stream_job,
        "rio-builder-p-strm1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &attempts,
    )
    .await
    .expect("delete succeeds");
    guard.verified().await;

    assert!(
        !recorder.histogram_touched("rio_controller_job_terminal_report_seconds"),
        "no covering pull attempt → no report attempted → no OA1 sample"
    );
}

// r[verify ctrl.job.synthesize-on-delete]
/// The synthesis is best-effort: with the admin channel dead, the
/// foreground DELETE still goes out and the helper returns the delete
/// result (the establishment sweep is the fallback classifier).
#[tokio::test]
async fn delete_job_report_failure_does_not_block_delete() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let covered = running_job_for_intent("rio-builder-p-pull1", "drv-pull-1");
    let attempts = vec![pull_attempt("drv-pull-1", "exec-1", "")];

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-pull1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &covered,
        "rio-builder-p-pull1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &attempts,
    )
    .await
    .expect("delete proceeds despite the failed report");
    guard.verified().await;
}

// r[verify ctrl.ephemeral.reap-orphan-running+5]
/// Obligation-(i) posture: the orphan reap's only busy source is the
/// durable open-attempt view, and an unreadable view (RPC error /
/// scheduler unreachable) means NO reap this tick — fail-closed. With
/// the dead admin channel every RPC fails, so the Running Job past
/// the grace must NOT be deleted.
#[tokio::test]
async fn reap_orphan_running_fail_closed_on_view_error() {
    let (client, _verifier) = ApiServerVerifier::new();
    // Dead admin channel: ListOpenAttempts fails.
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // Running (ready=1), 600 s old — past the 300 s orphan grace.
    let job = running_job_for_intent("rio-builder-p-orph1", "drv-orph-1");
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p").await;
    assert_eq!(
        reaped, 0,
        "unreadable open-attempt view → fail-closed → no orphan reap"
    );
    // No DELETE was issued: the fail-closed return happens before any
    // kube call, so the un-driven mock apiserver is never touched (a
    // wrongly-attempted DELETE would hang this test against the
    // never-answering verifier instead of passing).
}

// r[verify ctrl.ephemeral.reap-orphan-running+5]
// r[verify ctrl.job.busy-from-open-attempts+2]
// r[verify ctrl.job.orphan-leader-age]
/// Positive control for the fail-closed test above, and the busy-view
/// re-key: with a readable (empty) open-attempt view served by a
/// leader past the grace, the aged uncovered Job IS reaped — absence
/// from the durable view is authoritative once the leader has
/// observed a full grace window of pulls.
#[tokio::test]
async fn reap_orphan_running_reaps_on_readable_view() {
    let (client, verifier) = ApiServerVerifier::new();
    // In-process MockAdmin: ListOpenAttempts answers Ok with an empty
    // view from a long-tenured leader (well past the 300 s grace).
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    mock.open_attempts.write().unwrap().leader_for_secs = 3600;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let job = running_job_for_intent("rio-builder-p-orph2", "drv-orph-2");
    let guard = verifier.run(vec![delete_scenario("rio-builder-p-orph2")]);
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p").await;
    guard.verified().await;
    assert_eq!(
        reaped, 1,
        "readable view with no covering attempt → the orphan is reaped"
    );
}

// ───────────────────────────────────────────────────────────────────
// AD2 spawn gate (NoEligibleSource)
// ───────────────────────────────────────────────────────────────────

// r[verify sched.dispatch.fleet-exhaust+5]
/// The spawn-gate exhaustion predicate over the candidate set: fires
/// exactly when a non-empty admitting universe is fully covered by the
/// intent's exclusions — including the single-node small-fleet case —
/// and never on an empty universe (provisioning transient), an intent
/// without exclusions, or when a schedulable-but-NotReady node remains
/// admissible (merged_bug_124(a): the pre-fix Ready-only universe
/// poisoned through node restarts).
#[test]
fn no_eligible_source_predicate() {
    use crate::reconcilers::pool::candidate::{CandidateNode, no_eligible_source};

    let nodes = |names: &[&str]| -> Vec<CandidateNode> {
        names
            .iter()
            .map(|n| CandidateNode {
                name: (*n).into(),
                labels: Default::default(),
                schedulable: true,
            })
            .collect()
    };
    let intent_with = |excluded: &[&str]| -> SpawnIntent {
        SpawnIntent {
            intent_id: "drv-gated".into(),
            excluded_nodes: excluded.iter().map(|n| n.to_string()).collect(),
            ..Default::default()
        }
    };

    // (b) every node the pool could use is excluded → gated.
    assert!(no_eligible_source(
        &intent_with(&["n1", "n2"]),
        &nodes(&["n1", "n2"])
    ));
    // A non-excluded node exists → spawnable. This holds REGARDLESS of
    // the node's Ready condition now — `spawnable_nodes_for_pool` keeps
    // NotReady nodes and only drops cordoned ones.
    assert!(!no_eligible_source(
        &intent_with(&["n1"]),
        &nodes(&["n1", "n2"])
    ));
    // (c) the small-fleet clause: a single-node universe whose one node
    // is excluded gates immediately (streak persistence rate-limits the
    // REPORT, not the verdict).
    assert!(no_eligible_source(&intent_with(&["n1"]), &nodes(&["n1"])));
    // No exclusions → never gated.
    assert!(!no_eligible_source(&intent_with(&[]), &nodes(&["n1"])));
    // Empty universe → defer (autoscaling may mint a fresh node), never
    // a NoEligibleSource report.
    assert!(!no_eligible_source(&intent_with(&["n1"]), &nodes(&[])));
}

// r[verify sched.dispatch.fleet-exhaust+5]
// r[verify ctrl.pool.no-eligible-persist+3]
/// A gated intent produces exactly one acked `NoEligibleSource` report
/// carrying the intent's `resubmit_cycle` echo (124(b): the scheduler
/// ack-no-poisons a stale echo); the gated intent is the one removed
/// from the spawn set, so no Job is created for it. Cross-tick
/// idempotency is scheduler-side: the acked report poisons the
/// derivation, so it stops appearing as an intent at all.
#[tokio::test]
async fn gated_intent_reports_no_eligible_source_with_cycle_echo() {
    use crate::reconcilers::pool::candidate::{
        CandidateNode, exhausted_streak_step, no_eligible_source,
    };
    use crate::reconcilers::pool::jobs::report_no_eligible_source;

    let (client, _verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client).await;

    let gated_intent = SpawnIntent {
        intent_id: "drv-gated".into(),
        excluded_nodes: vec!["n1".into()],
        resubmit_cycle: 4,
        ..Default::default()
    };
    let open_intent = SpawnIntent {
        intent_id: "drv-open".into(),
        ..Default::default()
    };
    let candidates = vec![CandidateNode {
        name: "n1".into(),
        labels: Default::default(),
        schedulable: true,
    }];

    // The partition the reconcile applies: gated intents leave the
    // spawn set (no Job is built for them), open ones stay.
    let to_spawn = [gated_intent.clone(), open_intent.clone()];
    let (gated, spawnable_intents): (Vec<&SpawnIntent>, Vec<&SpawnIntent>) = to_spawn
        .iter()
        .partition(|i| no_eligible_source(i, &candidates));
    assert_eq!(gated.len(), 1);
    assert_eq!(spawnable_intents.len(), 1);
    assert_eq!(spawnable_intents[0].intent_id, "drv-open");

    // Persistence: the verdict alone does not report on ticks 1-2.
    let (s1, r1) = exhausted_streak_step(None);
    let (s2, r2) = exhausted_streak_step(Some(s1));
    let (_, r3) = exhausted_streak_step(Some(s2));
    assert!(!r1 && !r2 && r3, "report fires on the third gated tick");

    // Exactly one report goes out for the one gated intent, acked by
    // the (mock) scheduler, echoing the verdict's resubmit_cycle.
    let acked = report_no_eligible_source(&ctx, "p", &gated).await;
    assert_eq!(
        acked, 1,
        "exactly one NoEligibleSource report per gated intent"
    );
    let calls = mock.outcome_calls.read().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].intent_id, "drv-gated");
    assert_eq!(
        calls[0].resubmit_cycle, 4,
        "the verdict echoes the cycle it was computed against"
    );
}

// ───────────────────────────────────────────────────────────────────
// Pod-terminal report identity (the intent annotation)
// ───────────────────────────────────────────────────────────────────

/// A pod carrying the intent annotation.
fn annotated_pod(intent_id: &str) -> Pod {
    Pod {
        metadata: ObjectMeta {
            name: Some("rio-builder-p-pod1".into()),
            annotations: Some(BTreeMap::from([(
                INTENT_ID_ANNOTATION.to_string(),
                intent_id.to_string(),
            )])),
            ..Default::default()
        },
        spec: Some(k8s_openapi::api::core::v1::PodSpec {
            containers: vec![k8s_openapi::api::core::v1::Container {
                name: "executor".into(),
                ..Default::default()
            }],
            ..Default::default()
        }),
        ..Default::default()
    }
}

// r[verify ctrl.report.attempt-outcome]
/// The unified pod-terminal report carries the attempt identity: the
/// pod's / Job template's intent annotation when present, empty (Job/
/// pod-name-only resolution) when it is not. The former
/// RIO_DISPATCH_MODE gate is retired — every executor pod is a pull
/// pod, so the annotation is attached unconditionally.
#[test]
fn report_intent_id_carries_the_intent_annotation() {
    use crate::reconcilers::pool::job::{report_intent_id_for_job, report_intent_id_for_pod};

    // Pod call site (report_terminated_pods).
    assert_eq!(
        report_intent_id_for_pod(&annotated_pod("drv-p")),
        "drv-p",
        "a pod's report carries its intent annotation"
    );
    assert_eq!(
        report_intent_id_for_pod(&Pod::default()),
        "",
        "a pod with no annotation resolves by name only"
    );

    // Job call site (report_deadline_exceeded_jobs).
    assert_eq!(
        report_intent_id_for_job(&running_job_for_intent("rio-builder-p-job1", "drv-j")),
        "drv-j",
        "a Job's deadline report carries its template's intent annotation"
    );
    assert_eq!(
        report_intent_id_for_job(&Job::default()),
        "",
        "a Job with no annotation resolves by name only"
    );
}

// ───────────────────────────────────────────────────────────────────
// AD5 cancel arm (pull-mode pools only)
// ───────────────────────────────────────────────────────────────────

// r[verify ctrl.job.cancel-close-cause+2]
/// The cancel arm's selection rule, exhaustively over the close-cause
/// witness: a CANCELLED entry for an uncovered active Job selects it;
/// COMPLETED and FAILED entries never select (the Job-status
/// propagation lag window is untouchable by type); a CANCELLED entry
/// whose intent has a fresh open attempt (re-dispatch) never selects;
/// bare absence (no entry at all — a pod that never pulled) never
/// selects; an empty window selects nothing.
#[test]
fn cancel_arm_selects_only_cancelled_close_causes() {
    use crate::reconcilers::pool::job::select_closed_attempt_jobs;
    use rio_proto::types::CloseCause;

    let closed = |intent: &str, cause: CloseCause| rio_proto::types::ClosedAttempt {
        intent_id: intent.into(),
        exec_id: format!("exec-{intent}"),
        cause: cause as i32,
        closed_age_secs: 5,
    };

    let cancelled = running_job_for_intent("rio-builder-p-can1", "drv-cancelled");
    let completed = running_job_for_intent("rio-builder-p-done1", "drv-done");
    let failed = running_job_for_intent("rio-builder-p-fail1", "drv-failed");
    let redispatched = running_job_for_intent("rio-builder-p-re1", "drv-redispatched");
    let never_pulled = running_job_for_intent("rio-builder-p-wait1", "drv-waiting");
    let active = [
        &cancelled,
        &completed,
        &failed,
        &redispatched,
        &never_pulled,
    ];

    let window = vec![
        closed("drv-cancelled", CloseCause::Cancelled),
        closed("drv-done", CloseCause::Completed),
        closed("drv-failed", CloseCause::Failed),
        closed("drv-redispatched", CloseCause::Cancelled),
    ];
    // The re-dispatched intent has a FRESH open attempt covering it.
    let open = vec![pull_attempt("drv-redispatched", "exec-r2", "node-2")];

    let selected = select_closed_attempt_jobs(&active, &open, &window);
    assert_eq!(
        selected.len(),
        1,
        "exactly the uncovered CANCELLED close is selected"
    );
    assert_eq!(
        selected[0].metadata.name.as_deref(),
        Some("rio-builder-p-can1"),
        "cause discrimination: completed/failed closes and covered \
         re-dispatches are untouchable"
    );

    // An empty window selects nothing, whatever is active.
    assert!(
        select_closed_attempt_jobs(&active, &open, &[]).is_empty(),
        "no closes in the window → no cancellations"
    );
}

// r[verify ctrl.job.cancel-close-cause+2]
/// merged_bug_120's recorded red, kept as the regression pin: a build
/// whose attempt closed COMPLETED and whose Job status has not
/// propagated yet (the teardown-lag window) is NOT selected — the old
/// closed→active edge inference (covered tick N, gone tick N+1 ⇒
/// cancel) selected it because absence carried no cause.
#[test]
fn cancel_arm_normal_completion_in_lag_window_not_selected() {
    use crate::reconcilers::pool::job::select_closed_attempt_jobs;
    use rio_proto::types::CloseCause;

    let job = running_job_for_intent("rio-builder-p-done2", "drv-done2");
    let active = [&job];
    // The attempt closed normally moments ago; no open attempt.
    let window = vec![rio_proto::types::ClosedAttempt {
        intent_id: "drv-done2".into(),
        exec_id: "exec-d2".into(),
        cause: CloseCause::Completed as i32,
        closed_age_secs: 3,
    }];
    assert!(
        select_closed_attempt_jobs(&active, &[], &window).is_empty(),
        "a normal completion in the Job-status propagation lag window \
         must not be selected for cancellation"
    );
}

// r[verify ctrl.job.cancel-close-cause+2]
/// End-to-end over the wire mocks: a CANCELLED entry in the served
/// `recently_closed` window gets its (uncovered) Job
/// foreground-deleted, and only that Job.
#[tokio::test]
async fn cancel_arm_deletes_job_on_cancelled_close() {
    use crate::reconcilers::pool::job::cancel_closed_attempt_jobs;
    use rio_proto::types::CloseCause;

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let job = running_job_for_intent("rio-builder-p-edge1", "drv-edge");
    mock.open_attempts.write().unwrap().recently_closed = vec![rio_proto::types::ClosedAttempt {
        intent_id: "drv-edge".into(),
        exec_id: "exec-e1".into(),
        cause: CloseCause::Cancelled as i32,
        closed_age_secs: 4,
    }];

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-edge1")]);
    let cancelled = cancel_closed_attempt_jobs(&jobs_api, &[job], &ctx, "test-pool").await;
    guard.verified().await;
    assert_eq!(cancelled, 1, "exactly the cancelled-close Job is deleted");
}

// r[verify ctrl.job.cancel-close-cause+2]
/// Fail-closed: a failed `ListOpenAttempts` read produces no cancel
/// decisions, exactly like the orphan reap's posture.
#[tokio::test]
async fn cancel_arm_fail_closed_on_view_error() {
    use crate::reconcilers::pool::job::cancel_closed_attempt_jobs;

    let (client, _verifier) = ApiServerVerifier::new();
    // Dead admin channel: every RPC fails.
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let job = running_job_for_intent("rio-builder-p-fc1", "drv-fc");
    let cancelled = cancel_closed_attempt_jobs(&jobs_api, &[job], &ctx, "test-pool").await;
    assert_eq!(cancelled, 0, "no decisions on a failed view read");
}

/// Structural guard: the cancel arm is wired into the reconcile
/// unconditionally — the former Pool-CR dispatchMode gate retired with
/// the knob (every pool is a pull pool), so removing the call site
/// altogether (e.g. in a refactor) trips this.
#[test]
fn cancel_arm_call_site_is_wired() {
    let src = include_str!("../jobs.rs");
    assert!(
        src.contains("cancel_closed_attempt_jobs("),
        "the cancel arm must stay wired into the reconcile"
    );
}

// r[verify ctrl.job.orphan-leader-age]
/// merged_bug_221's recorded red, kept as the regression pin: a
/// freshly-failed-over leader (leader_for_secs = 0, the mock default)
/// must NOT reap an aged uncovered Running Job — never-pulled pods
/// have no row by construction, so the new leader's empty view is not
/// orphan evidence until it has observed one full grace window. No
/// DELETE is issued (a wrongly-attempted DELETE would hang against
/// the un-driven verifier).
#[tokio::test]
async fn orphan_reap_skips_young_leader() {
    use crate::reconcilers::pool::job::reap_orphan_running;

    let (client, _verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // Aged, uncovered Running Job; the mock leader is brand new.
    let job = running_job_for_intent("rio-builder-p-young1", "drv-young");
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p").await;
    assert_eq!(
        reaped, 0,
        "a leader younger than the orphan grace must not reap \
         never-pulled pods (they get one full grace against the NEW leader)"
    );
}
