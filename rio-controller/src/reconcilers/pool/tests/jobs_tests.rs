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
    DemandCoverage, INTENT_ID_ANNOTATION, INTENT_SELECTOR_ANNOTATION, IntentPage, WantMap,
    reap_stale_for_intents,
};
use rio_crds::pool::ExecutorKind;
use rio_proto::types::{AttemptTerminalReason, OpenAttempt, SpawnIntent};

/// Complete-view want-map over `intents` --- the pre-round-10 call
/// shape for the legacy reap scenarios (their demand views were
/// implicitly complete). Incomplete-view scenarios mint their own
/// coverage via the production [`WantMap::for_pool`] with
/// [`DemandCoverage::Incomplete`].
fn want_complete(intents: &[SpawnIntent], pool: &str, kind: ExecutorKind) -> WantMap {
    WantMap::for_pool(
        &IntentPage::for_test(intents.to_vec()),
        DemandCoverage::Complete,
        pool,
        kind,
    )
}

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
    // merged_bug_022: deletes now require a SUCCESSFUL view fetch (a
    // genuinely-empty ledger), never an error-born empty view.
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
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
        &pkey(),
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
    // merged_bug_022: deletes now require a SUCCESSFUL view fetch (a
    // genuinely-empty ledger), never an error-born empty view.
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    fn job(name: &str, sel: Option<&str>, ready: i32, succeeded: i32) -> Job {
        Job {
            metadata: ObjectMeta {
                name: Some(name.into()),
                // live_051(e): the strike map keys on the uid the
                // apiserver stamps on every object.
                uid: Some(apiserver_uid()),
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

    // live_051(e) DISCLOSED expectation flip: the attempt-affecting
    // arms (terminal/selector-drift) reap on the SECOND consecutive
    // classification — the first pass records strikes and defers.
    let strike_pass = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert!(
        strike_pass.is_empty(),
        "strike 1 defers the attempt-affecting arms (live_051(e))"
    );
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
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
    // merged_bug_022: deletes now require a SUCCESSFUL view fetch (a
    // genuinely-empty ledger), never an error-born empty view.
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let drifted = |name: &str| Job {
        metadata: ObjectMeta {
            name: Some(name.into()),
            // live_051(e): strike keying needs the apiserver uid.
            uid: Some(apiserver_uid()),
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
    // live_051(e) flip: strike pass first, reap on the second.
    let strike_pass = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert!(strike_pass.is_empty(), "strike 1 defers (live_051(e))");
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
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
        reap_excess_pending(
            &jobs_api,
            &pods_api,
            &jobs,
            &none,
            Some(2),
            &ctx,
            "p",
            &pkey()
        )
        .await,
        0
    );
    // queued=None → fail-closed (scheduler unreachable; spawn treats
    // as 0 fail-open, reap MUST NOT — would nuke every Pending Job
    // on a scheduler restart).
    assert_eq!(
        reap_excess_pending(&jobs_api, &pods_api, &jobs, &none, None, &ctx, "p", &pkey()).await,
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
    // merged_bug_022: deletes now require a SUCCESSFUL view fetch (a
    // genuinely-empty ledger), never an error-born empty view.
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
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
        &pkey(),
    )
    .await;
    guard.verified().await;
    assert_eq!(reaped, 0, "Running pod and list-error both skip DELETE");
}

// r[verify ctrl.pool.reconcile]
/// `job_census` excludes terminating Jobs from `active` (a Job
/// foreground-deleted on a prior tick doesn't burn a headroom slot
/// while the 45 s `PULL_MODE_TGPS_SECS` grace and the job-tracking
/// finalizer keep it Terminating) and computes `ready` distinctly from `active`
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
    // merged_bug_022: deletes now require a SUCCESSFUL view fetch (a
    // genuinely-empty ledger), never an error-born empty view.
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
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
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
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
        &want_complete(&[], "p", ExecutorKind::Builder),
        &ctx2,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert!(reaped.is_empty(), "scheduler error → no orphan-reap");
    guard.verified().await;
}

// ───────────────────────────────────────────────────────────────────
// Synthesize-on-delete (ctrl.job.synthesize-on-delete)
// ───────────────────────────────────────────────────────────────────

/// live_051(e): mint a FRESH attempts-view witness over hand-built
/// open attempts (the production constructor — `minted_now` is the
/// sole mint; the freshness arm is exercised via `backdate_for_test`).
fn fresh_witness(
    attempts: Vec<rio_proto::types::OpenAttempt>,
) -> crate::reconcilers::pool::job::AttemptsPair {
    crate::reconcilers::pool::job::AttemptsPair::at_selection(
        crate::reconcilers::pool::job::AttemptsViewWitness::minted_now(
            rio_proto::types::ListOpenAttemptsResponse {
                attempts,
                ..Default::default()
            },
        ),
    )
}

/// A fresh apiserver-shaped object uid (RFC-4122 v4 layout — the only
/// shape the apiserver emits; `metadata.uid` is set on EVERY object it
/// serves). Counter-suffixed so every mint is distinct, exactly like a
/// replacement object under a reused deterministic name. Witness
/// provenance (R13): fixtures mint uids through this helper, never
/// literal `"uid-1"` strings.
fn apiserver_uid() -> String {
    use std::sync::atomic::{AtomicU64, Ordering};
    static NEXT: AtomicU64 = AtomicU64::new(1);
    let n = NEXT.fetch_add(1, Ordering::Relaxed);
    format!("00000000-0000-4000-8000-{n:012x}")
}

/// A Running Job carrying the `rio.build/intent-id` pod-template
/// annotation, as `build_job` spawns it and the apiserver returns it
/// (uid populated — a LIST/watch never serves a uid-less object; each
/// call mints a fresh uid, exactly like a same-named replacement).
fn running_job_for_intent(name: &str, intent_id: &str) -> Job {
    let mut j = pending_job(name, 1, 600);
    j.metadata.uid = Some(apiserver_uid());
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

/// One open BUILD pull-mode attempt exactly as the production mint
/// persists it and the pull-filtered `ListOpenAttempts` view returns
/// it: `executor_id` IS the attested intent id (`ExecutorId::from(
/// intent_id)`, pull.rs — the request carries no pod name) and the
/// kind axis says Build. Witness provenance (R13): tests MUST mint
/// attempts through this helper or [`materialization_attempt`] — a
/// hand-rolled pod-shaped `executor_id` is a shape the scheduler
/// cannot emit, classifies `Foreign`, and fails any owns()-asserting
/// test by construction.
fn pull_attempt(intent_id: &str, exec_id: &str, source_node: &str) -> OpenAttempt {
    OpenAttempt {
        intent_id: intent_id.into(),
        executor_id: intent_id.into(),
        exec_id: exec_id.into(),
        source_node: source_node.into(),
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        ..Default::default()
    }
}

/// One open MATERIALIZATION attempt as the production mint persists
/// it: `executor_id == "{intent}@{instance}"` (the `(intent, replica)`
/// pair — distinct per store replica) and kind MATERIALIZATION. For
/// negative tests: a build-lifecycle verdict must never bind one.
fn materialization_attempt(
    intent_id: &str,
    instance: &str,
    exec_id: &str,
    source_node: &str,
) -> OpenAttempt {
    OpenAttempt {
        intent_id: intent_id.into(),
        executor_id: format!("{intent_id}@{instance}"),
        exec_id: exec_id.into(),
        source_node: source_node.into(),
        attempt_kind: rio_proto::types::AttemptKind::Materialization as i32,
        ..Default::default()
    }
}

/// The streak/breaker pool key the test reconciles run under
/// (namespace `rio`, pool `p` --- matching the fixtures).
fn pkey() -> crate::reconcilers::pool::candidate::PoolKey {
    crate::reconcilers::pool::candidate::PoolKey::new("rio", "p")
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

// r[verify ctrl.job.synthesize-on-delete+4]
/// W9-AO (multi-job wave): job k != trigger must also veto
/// against the DECIDING view. Pre-fix: j1's stale-age refetch resets
/// the witness age and replaces the view, so j2's delete skips the
/// veto entirely and synthesizes Reaped for j2's LIVE attempt (the
/// 169-Reaped/543-wave shape relocated to sibling iterations).
#[tokio::test]
async fn w9_ao_every_job_in_the_wave_vetoes_against_the_deciding_view() {
    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let j1 = terminal_job_for_intent("rio-builder-p-w1", "drv-w-1");
    let j2 = terminal_job_for_intent("rio-builder-p-w2", "drv-w-2");
    // The deciding view: EMPTY (both selections premised
    // non-coverage), and the wave has run long.
    let mut witness = fresh_witness(vec![]);
    witness.backdate_for_test(std::time::Duration::from_secs(3));
    // BOTH pulls land during the wave: the live ledger now covers
    // both intents.
    mock.open_attempts.write().unwrap().attempts = vec![
        pull_attempt("drv-w-1", "exec-w-1", "node-a"),
        pull_attempt("drv-w-2", "exec-w-2", "node-b"),
    ];

    // ZERO apiserver scenarios: EVERY delete in the wave must defer.
    let guard = verifier.run(vec![]);
    let o1 = crate::reconcilers::pool::job::delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &j1,
        "rio-builder-p-w1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &mut witness,
        &pkey(),
    )
    .await
    .expect("deferred delete is Ok");
    assert_eq!(
        o1,
        crate::reconcilers::pool::job::SynthesizedDelete::Deferred {
            fresh_attempt: true
        },
        "the refresh-tripping job's veto (the wave-8-covered face)"
    );
    let o2 = crate::reconcilers::pool::job::delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &j2,
        "rio-builder-p-w2",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &mut witness,
        &pkey(),
    )
    .await
    .expect("deferred delete is Ok");
    guard.verified().await;
    assert_eq!(
        o2,
        crate::reconcilers::pool::job::SynthesizedDelete::Deferred {
            fresh_attempt: true
        },
        "left: j2 deleted with its just-pulled attempt closed as Reaped \
         (the rolling witness contains it, so the veto never fires for \
         k != trigger) / right: every job's veto evaluates against the \
         deciding view and j2 survives"
    );
    assert!(
        mock.outcome_calls.read().unwrap().is_empty(),
        "zero Reaped synthesis anywhere in the wave: {:?}",
        mock.outcome_calls.read().unwrap()
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
/// W9-AP: a FAILED ListOpenAttempts defers ALL adjudication — no
/// deletes-as-NoOpenAttempt, no verdict-free-death backoff tax. The
/// fixture drives a REAL RPC failure through the production adapter
/// (the ctx admin channel points at an unreachable address), the
/// exact failover-churn population: pre-fix the error became a
/// maximally-fresh EMPTY witness and a strike-2 wave deleted every
/// job as NoOpenAttempt while taxing every intent's respawn.
#[tokio::test]
async fn w9_ap_failed_listing_defers_all_adjudication() {
    let (client, verifier) = ApiServerVerifier::new();
    // The production adapter against a dead endpoint: every
    // ListOpenAttempts RPC genuinely fails (no fault-injection lane).
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // A terminal Job for a still-wanted intent: the attempt-affecting
    // arm with the two-tick strike.
    let j = terminal_job_for_intent("rio-builder-p-ap1", "drv-ap-1");
    let intents = vec![SpawnIntent {
        intent_id: "drv-ap-1".into(),
        ..Default::default()
    }];

    // Tick 1: strike 1 recorded, defers before any fetch.
    let guard = verifier.run(vec![]);
    let reaped = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&j),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert!(reaped.is_empty(), "strike 1 defers");

    // Tick 2: strike 2 → the wave reaches the lazy fetch → the fetch
    // FAILS → every remaining delete defers. Zero apiserver deletes.
    let reaped = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&j),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    guard.verified().await;
    assert!(
        reaped.is_empty(),
        "a failed listing must not delete as NoOpenAttempt"
    );
    // No backoff tax: the intent's respawn is NOT blocked (the
    // verdict-free-death lane never stepped — there was no delete).
    assert!(
        !ctx.exhausted_streak.lock().respawn_blocked(
            &pkey(),
            "drv-ap-1",
            std::time::Instant::now()
        ),
        "no verdict-free-death tax from an error-born view"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
/// live_051(e) R1b — the live casualty shape, at the production reap
/// conduit: a JUST-pulled attempt (open in the scheduler ledger when
/// the wave's lazy view is fetched) is NOT closed charge-free as
/// Reaped on the first tick its Job classifies stale. PROPOSITION
/// CERTIFIED (structural): zero deletes and ZERO Reaped synthesis on
/// the first classification — the two-tick strike holds the reap
/// while the round-trip completes. Pre-fix red (the measured live
/// shape — 169 attempts Reaped within ~200ms of open; the synthesized
/// report named them; client-visible "cancelled"): the first tick
/// deleted the Job and synthesized Reaped for the live attempt.
#[tokio::test]
async fn first_tick_reap_never_synthesizes_for_a_live_attempt() {
    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // The pod pulled moments ago: the attempt is open in the ledger
    // the wave's lazy view will fetch.
    mock.open_attempts.write().unwrap().attempts =
        vec![pull_attempt("race2", "exec-race-2", "node-a")];
    // Mass-resubmit shape: the standing Job flips terminal in the
    // same tick the worker pulls the resubmitted attempt.
    let job = terminal_job_for_intent("rio-builder-p-race2", "race2");

    // ZERO apiserver scenarios: the first tick must not delete.
    let guard = verifier.run(vec![]);
    let reaped = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&[intent_named("race2")], "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    guard.verified().await;

    let synthesized = mock.outcome_calls.read().unwrap().len();
    assert_eq!(
        (reaped.len(), synthesized),
        (0, 0),
        "left: (1, 1) — the Job deleted and the just-pulled attempt \
         closed charge-free as Reaped ~200ms after open (the \
         synthesized report named it; client-visible cancelled) / \
         right: (0, 0) — strike 1 defers; the attempt survives"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
/// live_051(e) R2 — DISCLOSED expectation flip (the strike law; never
/// claimed as a red): a genuinely attempt-less stale Job reaps on the
/// SECOND consecutive strike with its synthesized-report machinery
/// intact (kill-isolation for the bug_028 breaker economics — W2),
/// and a deferred Job is re-decided next tick (strike monotonicity,
/// W3 — no infinite-defer arm exists in the alphabet).
#[tokio::test]
async fn attemptless_stale_job_reaps_on_the_second_strike() {
    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // Attempt-less: the scheduler ledger is empty for this intent.
    mock.open_attempts.write().unwrap().attempts.clear();
    let job = terminal_job_for_intent("rio-builder-p-strike1", "strike1");
    let intents = [intent_named("strike1")];

    // Tick 1: strike — zero deletes (the verifier holds the delete
    // scenario un-consumed).
    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-builder-p-strike1",
        body_contains: Some(r#""propagationPolicy":"Background""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);
    let first = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    assert!(
        first.is_empty(),
        "strike 1 defers the terminal-arm reap (live_051(e))"
    );

    // Tick 2: the second consecutive classification reaps — fresh
    // view still shows no cover, so the chokepoint proceeds and the
    // breaker notes the verdict-free death exactly as before.
    let second = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    guard.verified().await;
    assert_eq!(
        second,
        HashSet::from(["rio-builder-p-strike1".to_string()]),
        "strike 2 reaps the genuinely attempt-less stale Job"
    );
    let blocked =
        ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "strike1", std::time::Instant::now());
    assert!(
        blocked,
        "the verdict-free death still steps the respawn record \
         (the bug_028 breaker economics survive the strike gate)"
    );
}

/// W9-AQ (merged_bug_033): strikes expire structurally, not
/// positionally. A strike at tick t followed by k EMPTY-want passes
/// (idle pool; the fail-closed scheduler-error arm polls as queued=0
/// — exactly the early return that used to bypass the function-tail
/// retain) does NOT escalate on the first post-gap classification:
/// the gap reset consecutiveness BY VALUE, so the post-gap pass is
/// strike 1 (defers) and only a genuinely adjacent second pass
/// reaps. Pre-fix red (the frozen strike): the post-gap pass reaped
/// on a non-consecutive "strike 2" — transcript in the commit body.
#[tokio::test]
async fn w9_aq_strikes_reset_across_empty_want_gap_ticks() {
    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // Attempt-less terminal Job for a still-wanted intent: the
    // attempt-affecting arm with the two-tick confirmation.
    mock.open_attempts.write().unwrap().attempts.clear();
    let job = terminal_job_for_intent("rio-builder-p-aq1", "aq1");
    let intents = [intent_named("aq1")];

    // ONE delete scenario total: it must be consumed by the final
    // adjacent-pass reap, NEVER by the post-gap pass.
    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-builder-p-aq1",
        body_contains: Some(r#""propagationPolicy":"Background""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);

    // Tick t: strike 1 — defers.
    let first = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    assert!(first.is_empty(), "strike 1 defers");

    // Ticks t+1 .. t+3: the outage/idle gap — reap runs with NO
    // intents and early-returns. Each empty pass IS a tick.
    for _ in 0..3 {
        let gap = reap_stale_for_intents(
            &jobs_api,
            std::slice::from_ref(&job),
            &want_complete(&[], "p", ExecutorKind::Builder),
            &ctx,
            &crate::fixtures::test_pool("p", ExecutorKind::Builder),
            "p",
            &key,
        )
        .await;
        assert!(gap.is_empty(), "an empty-want pass reaps nothing");
    }

    // Post-gap classification: the count RESET to 1 — defers.
    let post_gap = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    assert!(
        post_gap.is_empty(),
        "left: {{rio-builder-p-aq1}} (the frozen strike reaps on a \
         non-consecutive \"strike 2\" right after the outage heals) / \
         right: {{}} (the gap reset the confirmation; this pass is \
         strike 1)"
    );

    // The genuinely adjacent second pass reaps — the two-tick law
    // still confirms on back-to-back classifications.
    let adjacent = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&intents, "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    guard.verified().await;
    assert_eq!(
        adjacent,
        HashSet::from(["rio-builder-p-aq1".to_string()]),
        "two adjacent-by-value classifications still reap"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
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
    // attempt carries the executor identity the scheduler actually
    // mints for a build pull: the attested intent id itself
    // (`ExecutorId::from(intent_id)`, pull.rs — never a pod name).
    let attempts = vec![pull_attempt("drv-pull-1", "exec-1", "node-a")];
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

    // (d) Negative pin: a Foreign-shaped attempt for the SAME intent is
    // never owned. A pod-shaped executor_id is a shape the scheduler
    // cannot mint (r13-allow(refusal-probe): deliberately unproducible
    // input; the assertion IS the refusal) — the classifier maps it to
    // `Foreign` and the synthesis must refuse to bind it.
    let foreign = OpenAttempt {
        intent_id: "drv-pull-1".into(),
        executor_id: "rio-builder-p-pull1-a1b2c".into(),
        exec_id: "exec-9".into(),
        source_node: "node-a".into(),
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        ..Default::default()
    };
    assert!(
        synthesized_report_for_job(&job, AttemptTerminalReason::Reaped, &[foreign]).is_none(),
        "a pod-shaped (unmintable) executor identity is Foreign → never owned"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
/// bug_071 (preserving merged_bug_298's INTENT — never close an
/// attempt you don't own — under producible shapes): with the minted
/// identity alphabet, a Job delete binds the BUILD pull attempt for
/// its own intent and NEVER a materialization claim for the same
/// intent (`{intent}@{instance}`, a store replica's attempt — closing
/// it would charge the wrong executor class). The one-winner pull
/// arbiter guarantees at most one open BUILD attempt per intent, so
/// the m298 two-same-intent-build-attempts scenario is unconstructible
/// scheduler-side.
#[test]
fn synthesized_report_binds_build_attempt_never_materialization() {
    let job = running_job_for_intent("rio-builder-b-x", "drv-shared-1");
    // Build pull + materialization claim for the SAME intent: the
    // verdict binds the build attempt's exec_id.
    let attempts = vec![
        pull_attempt("drv-shared-1", "exec-build", "node-1"),
        materialization_attempt("drv-shared-1", "store-1", "exec-mat", "node-2"),
    ];
    let req = synthesized_report_for_job(&job, AttemptTerminalReason::Reaped, &attempts)
        .expect("the build pull attempt is open");
    assert_eq!(
        req.exec_id, "exec-build",
        "the synthesized verdict must bind the build attempt, never the materialization claim"
    );

    // Materialization claim ONLY → None: a Job delete must not close a
    // store replica's claim even when it is the only open attempt.
    let only_mat = vec![materialization_attempt(
        "drv-shared-1",
        "store-1",
        "exec-mat",
        "node-2",
    )];
    assert!(
        synthesized_report_for_job(&job, AttemptTerminalReason::Reaped, &only_mat).is_none(),
        "a materialization claim alone must never be bound by a Job-delete verdict"
    );
}

mod minted_identity_props {
    use proptest::prelude::*;

    use crate::reconcilers::pool::job::MintedPullIdentity;

    /// Identity-ish strings over the production charset plus the '@'
    /// separator and the chars pod names use — covers store paths,
    /// `intent@instance` composites, pod shapes, and junk.
    fn arb_id() -> impl Strategy<Value = String> {
        proptest::string::string_regex("[a-z0-9./@-]{0,24}").expect("valid regex")
    }

    proptest! {
        // r[verify ctrl.job.synthesize-on-delete+4]
        /// bug_071 closure-set law: the classifier is TOTAL over
        /// arbitrary (executor_id, intent_id, kind) triples and never
        /// panics; `Build` holds ONLY under shape∧kind agreement;
        /// `Materialization` ONLY under the `{intent}@{instance}`
        /// shape with the matching kind; pod-shaped ids (a dash-joined
        /// suffix on a non-intent prefix — unmintable) are always
        /// `Foreign`, as is every unknown kind value (fail-closed for
        /// future variants).
        #[test]
        fn minted_identity_total_and_foreign_on_unminted_shapes(
            executor_id in arb_id(),
            intent_id in arb_id(),
            kind in proptest::num::i32::ANY,
        ) {
            use rio_proto::types::AttemptKind as K;
            let got = MintedPullIdentity::classify(&executor_id, &intent_id, kind);
            let build_kind = kind == K::Build as i32 || kind == K::Unspecified as i32;
            let build_shape = !intent_id.is_empty() && executor_id == intent_id;
            let mat_shape = !intent_id.is_empty()
                && executor_id.strip_prefix(intent_id.as_str())
                    .and_then(|r| r.strip_prefix('@'))
                    .is_some_and(|i| !i.is_empty());
            let expected = if build_shape && build_kind {
                MintedPullIdentity::Build
            } else if mat_shape && kind == K::Materialization as i32 {
                MintedPullIdentity::Materialization
            } else {
                MintedPullIdentity::Foreign
            };
            prop_assert_eq!(got, expected);
            // Unknown kind values never classify owned, whatever the
            // shape (fail-closed: the mirror above forces Foreign there,
            // so `expected` already pins it — this names the law).
            if !build_kind && kind != K::Materialization as i32 {
                prop_assert_eq!(got, MintedPullIdentity::Foreign);
            }
            // Pod-shaped ids (job-name prefix + dash + dashless suffix,
            // prefix != intent) are Foreign under EVERY kind.
            let pod_shaped = format!("rio-builder-p-{}", "a1b2c");
            if pod_shaped != intent_id {
                prop_assert_eq!(
                    MintedPullIdentity::classify(&pod_shaped, &intent_id, kind),
                    MintedPullIdentity::Foreign
                );
            }
        }
    }
}

// r[verify ctrl.job.synthesize-on-delete+4]
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
    // bug_071: the attempt carries the production-minted identity (the
    // attested intent id) — the owner binding is the Build classifier,
    // not a pod-name shape.
    let mut attempts = fresh_witness(vec![pull_attempt("drv-pull-1", "exec-1", "node-a")]);

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-pull1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &covered,
        "rio-builder-p-pull1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &mut attempts,
        &pkey(),
    )
    .await
    .expect("delete succeeds");
    guard.verified().await;

    assert!(
        recorder.histogram_touched("rio_controller_job_terminal_report_seconds"),
        "the synthesize→ack→OA1-sample path must have run for the covered Job"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
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
    let mut attempts = fresh_witness(vec![pull_attempt("drv-pull-1", "exec-1", "")]);

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-strm1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &stream_job,
        "rio-builder-p-strm1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &mut attempts,
        &pkey(),
    )
    .await
    .expect("delete succeeds");
    guard.verified().await;

    assert!(
        !recorder.histogram_touched("rio_controller_job_terminal_report_seconds"),
        "no covering pull attempt → no report attempted → no OA1 sample"
    );
}

// r[verify ctrl.job.synthesize-on-delete+4]
/// The synthesis is best-effort: with the admin channel dead, the
/// foreground DELETE still goes out and the helper returns the delete
/// result (the establishment sweep is the fallback classifier).
#[tokio::test]
async fn delete_job_report_failure_does_not_block_delete() {
    let (client, verifier) = ApiServerVerifier::new();
    let ctx = super::test_ctx(client.clone());
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let covered = running_job_for_intent("rio-builder-p-pull1", "drv-pull-1");
    let mut attempts = fresh_witness(vec![pull_attempt("drv-pull-1", "exec-1", "")]);

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-pull1")]);
    delete_job_with_synthesized_report(
        &jobs_api,
        &ctx,
        &covered,
        "rio-builder-p-pull1",
        &DeleteParams::foreground(),
        AttemptTerminalReason::Reaped,
        &mut attempts,
        &pkey(),
    )
    .await
    .expect("delete proceeds despite the failed report");
    guard.verified().await;
}

/// bug_089 red: the OA1 terminal-report sample gate must key on the
/// apiserver OBJECT (metadata.uid), never the reusable deterministic
/// Job name. `reap_stale_for_intents` background-deletes terminal Jobs
/// early precisely so a same-named replacement spawns; a name-keyed
/// gate silently suppresses the replacement object's legitimate first
/// sample for up to the 1200 s TTL window. Two same-named Jobs with
/// distinct uids (each minted fresh by the fixture, exactly as the
/// apiserver does for a replacement) must each sample once.
///
/// Sample count is asserted as gate-map cardinality plus the histogram
/// touch-set: the call site records into the OA1 histogram exactly
/// when the gate admits, and the shared CountingRecorder captures
/// histograms as a touch-set only (no per-record counts), so the map
/// IS the per-sample witness.
#[tokio::test]
async fn terminal_sample_gate_keys_on_object_uid_not_name() {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&recorder);

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // Two GENERATIONS of the same deterministic Job name: the fixture
    // mints a fresh uid per call — job2 is the replacement object the
    // early reap makes possible.
    let job1 = running_job_for_intent("rio-builder-p-rep1", "drv-rep-1");
    let job2 = running_job_for_intent("rio-builder-p-rep1", "drv-rep-1");
    assert_ne!(
        job1.metadata.uid, job2.metadata.uid,
        "fixture mints fresh uids"
    );
    let mut attempts = fresh_witness(vec![pull_attempt("drv-rep-1", "exec-1", "node-a")]);

    let guard = verifier.run(vec![
        delete_scenario("rio-builder-p-rep1"),
        delete_scenario("rio-builder-p-rep1"),
    ]);
    for job in [&job1, &job2] {
        delete_job_with_synthesized_report(
            &jobs_api,
            &ctx,
            job,
            "rio-builder-p-rep1",
            &DeleteParams::foreground(),
            AttemptTerminalReason::Reaped,
            &mut attempts,
            &pkey(),
        )
        .await
        .expect("delete succeeds");
    }
    guard.verified().await;

    assert!(
        recorder.histogram_touched("rio_controller_job_terminal_report_seconds"),
        "the synthesize→ack→OA1-sample path must have run"
    );
    let sampled = ctx.terminal_report_sampled.lock().len();
    assert_eq!(
        sampled, 2,
        "histogram sampled once for a replacement object with a fresh uid"
    );
}

/// bug_089 companion pin: the SAME object presented twice (a failed
/// delete retried next tick re-presents the SAME uid) still dedupes to
/// exactly one sample — the uid re-key must not break the retry-path
/// dedup the gate exists for.
#[tokio::test]
async fn terminal_sample_gate_still_dedupes_same_object() {
    let recorder = rio_test_support::metrics::CountingRecorder::default();
    let _g = metrics::set_default_local_recorder(&recorder);

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let job = running_job_for_intent("rio-builder-p-rep2", "drv-rep-2");
    let mut attempts = fresh_witness(vec![pull_attempt("drv-rep-2", "exec-2", "node-b")]);

    let guard = verifier.run(vec![
        delete_scenario("rio-builder-p-rep2"),
        delete_scenario("rio-builder-p-rep2"),
    ]);
    for _ in 0..2 {
        delete_job_with_synthesized_report(
            &jobs_api,
            &ctx,
            &job,
            "rio-builder-p-rep2",
            &DeleteParams::foreground(),
            AttemptTerminalReason::Reaped,
            &mut attempts,
            &pkey(),
        )
        .await
        .expect("delete succeeds");
    }
    guard.verified().await;

    assert_eq!(
        ctx.terminal_report_sampled.lock().len(),
        1,
        "the same object (same uid) must sample exactly once"
    );
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
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p", &pkey()).await;
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
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p", &pkey()).await;
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
// r[verify ctrl.pool.no-eligible-persist+5]
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

    // The partition the reconcile applies (bug_028 order: over the
    // FULL existing-names-filtered wanted set, BEFORE the headroom
    // truncate): gated intents leave the spawn set (no Job is built
    // for them, no headroom slot burned), open ones stay.
    let wanted = [gated_intent.clone(), open_intent.clone()];
    let (gated, spawnable_intents): (Vec<&SpawnIntent>, Vec<&SpawnIntent>) = wanted
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
    // the (mock) scheduler, echoing the verdict's resubmit_cycle. The
    // ack now arrives PAIRED with its reset witness, minted at the ack
    // site (merged_bug_080(2b)).
    let acked = report_no_eligible_source(&ctx, "p", &gated).await;
    assert_eq!(
        acked.iter().map(|(id, _)| id.as_str()).collect::<Vec<_>>(),
        vec!["drv-gated"],
        "exactly one NoEligibleSource report per gated intent, acked by id \
         (bug_028: the acked ids feed the futility breaker reset lane)"
    );
    let calls = mock.outcome_calls.read().unwrap();
    assert_eq!(calls.len(), 1);
    assert_eq!(calls[0].intent_id, "drv-gated");
    assert_eq!(
        calls[0].resubmit_cycle, 4,
        "the verdict echoes the cycle it was computed against"
    );
}

// r[verify ctrl.pool.no-eligible-persist+5]
/// bug_028: the AD2 gate evaluates ALL wanted intents, never just the
/// headroom window --- an exhausted intent behind a full pool accrues
/// its verdict instead of stalling behind the ceiling. Drives the
/// extracted fold helper (`evaluate_spawn_gate`); the pre-fix red was
/// recorded against the inline take-then-gate shape (the helper
/// extraction is the disclosed strawman), verbatim:
/// `the exhausted intent must be EVALUATED (appear in gated) even
/// when headroom-truncated out of the spawn window; gated = []`.
#[test]
fn gate_evaluates_all_wanted_not_just_window() {
    use crate::reconcilers::pool::candidate::{CandidateNode, PoolStreaks};
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let hi = SpawnIntent {
        intent_id: "drv-hi".into(),
        ..Default::default()
    };
    let exhausted = SpawnIntent {
        intent_id: "drv-exhausted".into(),
        excluded_nodes: vec!["n1".into()],
        ..Default::default()
    };
    let candidates = vec![CandidateNode {
        name: "n1".into(),
        labels: Default::default(),
        schedulable: true,
    }];
    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();

    // Scheduler priority order: the unexcluded intent first. The
    // ceiling-1 take(headroom) happens AFTER the gate in the
    // reconcile; the helper takes the FULL wanted set by construction
    // --- the window cannot reach the fold.
    let universe = GateUniverse::Nodes(candidates);
    let mut fired = Vec::new();
    for n in 0..3u64 {
        let now = t0 + std::time::Duration::from_secs(n * 10);
        let tick = streaks.begin_tick(&key);
        let outcome = evaluate_spawn_gate(
            vec![hi.clone(), exhausted.clone()],
            &universe,
            &mut streaks,
            tick,
            &key,
            DemandCoverage::Complete,
            now,
        );
        // The exhausted intent is EVALUATED and withheld every fold:
        // it never appears spawnable (and so never burns a headroom
        // slot), while the unexcluded intent always does.
        assert_eq!(
            outcome
                .spawnable
                .iter()
                .map(|i| i.intent_id.as_str())
                .collect::<Vec<_>>(),
            vec!["drv-hi"],
            "fold {n}: gated intent must be withheld, open intent spawnable"
        );
        fired = outcome.to_report;
    }
    // Third consecutive evaluated fold spanning the 20s floor: the
    // verdict fires --- proof the gate evaluated the exhausted intent
    // on every fold even though the spawn window only ever had room
    // for the higher-priority one.
    assert_eq!(
        fired
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["drv-exhausted"],
        "the exhausted intent's verdict must accrue across folds despite the window"
    );
}

/// bug_028 futility breaker red: an intent whose Jobs die VERDICT-FREE
/// every cycle (the live exhibit: 759 intents respawned 2-8x with zero
/// NoEligibleSource verdicts over 41 min --- total builder failure
/// converted to EC2 spend at reconcile cadence) must respawn on the
/// exponential backoff schedule, not every tick. Paused-clock loop at
/// the 10 s reconcile cadence; each spawn's Job dies verdict-free and
/// the next tick's reap notes the death BEFORE the fold --- with
/// evaluated-not-gated folds on EVERY tick in between (the backoff
/// steady state, per FS-6: a drop-on-evaluated-not-gated wiring would
/// clear the record each tick and fail this immediately). Recorded
/// red (breaker neutered --- strawman, the gate cannot exist pre-fix):
/// `left: 25 right: 5 --- verdict-free respawns must follow the
/// exponential backoff schedule, not the reconcile cadence`.
// r[verify ctrl.pool.respawn-backoff+2]
#[test]
fn verdictless_respawn_backs_off_per_intent() {
    use crate::reconcilers::pool::candidate::PoolStreaks;
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    let intent = SpawnIntent {
        intent_id: "drv-loop".into(),
        ..Default::default()
    };

    let mut spawns = 0u32;
    let mut pending_death = false;
    for n in 0..25u64 {
        let now = t0 + std::time::Duration::from_secs(n * 10);
        // The reap observes the prior tick's verdict-free death
        // BEFORE the fold (reap_stale_for_intents runs first in the
        // reconcile), so the backoff gates the same-tick respawn.
        if pending_death {
            streaks.note_verdict_free_death(&key, "drv-loop", now);
            pending_death = false;
        }
        let tick = streaks.begin_tick(&key);
        let outcome = evaluate_spawn_gate(
            vec![intent.clone()],
            &GateUniverse::NoExclusions,
            &mut streaks,
            tick,
            &key,
            DemandCoverage::Complete,
            now,
        );
        if !outcome.spawnable.is_empty() {
            spawns += 1;
            pending_death = true; // the spawned Job dies before the next tick
        }
    }
    // Exponential schedule at base 10 s, cap 80 s: spawns at ticks
    // 0, 2, 5, 10, 19 --- five over 25 ticks (vs 25 at cadence).
    assert_eq!(
        spawns, 5,
        "verdict-free respawns must follow the exponential backoff schedule, \
         not the reconcile cadence"
    );
}

/// bug_028 futility breaker companion green: a NAMED resolution ---
/// here the acked NoEligibleSource verdict lane the reconcile calls
/// after `report_no_eligible_source` acks --- resets the backoff
/// immediately; the breaker must never mask a real verdict lane.
/// merged_bug_080(2b): the reset rides the typed witness, minted from
/// the ack response exactly as the production arm mints it (every
/// poison-arm ack carries `attempt_resolved=false` by design --- the
/// ack itself is the premise).
// r[verify ctrl.pool.respawn-backoff+2]
#[test]
fn verdict_resets_respawn_backoff() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    let intent = SpawnIntent {
        intent_id: "drv-loop".into(),
        ..Default::default()
    };

    // Three verdict-free deaths: the backoff floor is now 40 s.
    for n in 0..3u64 {
        streaks.note_verdict_free_death(&key, "drv-loop", t0 + std::time::Duration::from_secs(n));
    }
    let now = t0 + std::time::Duration::from_secs(10);
    let tick = streaks.begin_tick(&key);
    let blocked = evaluate_spawn_gate(
        vec![intent.clone()],
        &GateUniverse::NoExclusions,
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        now,
    );
    assert!(
        blocked.spawnable.is_empty(),
        "mid-backoff the intent must be withheld"
    );

    // The verdict lands (the same typed mint + reset call the
    // reconcile makes for each acked NoEligibleSource id): the record
    // clears and the very next tick spawns at full cadence.
    let ack = rio_proto::types::ReportAttemptOutcomeResponse::default();
    streaks.note_resolution(
        &key,
        "drv-loop",
        VerdictWitness::from_acked_no_eligible_source(&ack),
        std::time::Instant::now(),
    );
    let tick = streaks.begin_tick(&key);
    let unblocked = evaluate_spawn_gate(
        vec![intent],
        &GateUniverse::NoExclusions,
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        now + std::time::Duration::from_secs(10),
    );
    assert_eq!(
        unblocked
            .spawnable
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["drv-loop"],
        "a named resolution must reset the backoff --- the breaker never masks a verdict lane"
    );
}

// ───────────────────────────────────────────────────────────────────
// bug_075: ListFailed fail-open vs live exhaustion evidence
// ───────────────────────────────────────────────────────────────────

/// Two completed Nodes-arm folds that gate `drv-x` (t0, t0+10s) ---
/// the shared red-test prefix: a live two-tick streak.
fn two_gated_folds(
    streaks: &mut crate::reconcilers::pool::candidate::PoolStreaks,
    key: &crate::reconcilers::pool::candidate::PoolKey,
    t0: std::time::Instant,
) -> SpawnIntent {
    use crate::reconcilers::pool::candidate::CandidateNode;
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let gated = SpawnIntent {
        intent_id: "drv-x".into(),
        excluded_nodes: vec!["n1".into()],
        ..Default::default()
    };
    let candidates = vec![CandidateNode {
        name: "n1".into(),
        labels: Default::default(),
        schedulable: true,
    }];
    for n in 0..2u64 {
        let now = t0 + std::time::Duration::from_secs(n * 10);
        let tick = streaks.begin_tick(key);
        let outcome = evaluate_spawn_gate(
            vec![gated.clone()],
            &GateUniverse::Nodes(candidates.clone()),
            streaks,
            tick,
            key,
            DemandCoverage::Complete,
            now,
        );
        assert!(
            outcome.spawnable.is_empty(),
            "prefix fold {n}: the gated intent must be withheld"
        );
    }
    gated
}

// r[verify ctrl.pool.no-eligible-persist+5]
/// bug_075 red 1 (recorded verbatim in the close commit): a fold-skip
/// (ListFailed) tick MUST NOT spawn an intent whose retained streak is
/// live --- spawning makes it structurally unobservable (existing-name
/// exclusion) for >=180s while the streak expires at 120s, destroying
/// the evidence the retain law preserved. Pre-fix the arm returned
/// every wanted intent spawnable: `left: ["drv-x"], right: []`.
///
/// Witness-strength: certifies the production fold's ListFailed arm
/// withholds a LIVE-streak intent (the new census cell), driven
/// through `evaluate_spawn_gate` with streaks built only by production
/// noting/step calls.
#[test]
fn list_failed_arm_withholds_live_streak_intents() {
    use crate::reconcilers::pool::candidate::PoolStreaks;
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    let gated = two_gated_folds(&mut streaks, &key, t0);

    // Node LIST fails at t0+20s: the streak (touched t0+10s) is live.
    let tick = streaks.begin_tick(&key);
    let outcome = evaluate_spawn_gate(
        vec![gated],
        &GateUniverse::ListFailed,
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        t0 + std::time::Duration::from_secs(20),
    );
    assert_eq!(
        outcome
            .spawnable
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        Vec::<&str>::new(),
        "a fold-skip tick must withhold intents carrying live exhaustion streaks"
    );
}

// r[verify ctrl.pool.no-eligible-persist+5]
/// bug_075 red 2 (recorded verbatim in the close commit): one node-LIST
/// blip mid-streak must not livelock the poison report. Pre-fix the
/// t0+20s fail-open spawn froze evaluation (existing-name exclusion,
/// mirrored here per the `verdictless_respawn_backs_off_per_intent`
/// precedent) past the 120s expiry, the streak restarted from scratch,
/// and the next gated fold did NOT fire: `left: [], right: ["drv-x"]`.
/// Post-fix the withhold keeps the intent jobless, the t0+30s fold
/// gates it (streak 3, floor 30s >= 20s) and the verdict fires.
///
/// Witness-strength: certifies streak CONTINUITY across a fold-skip
/// tick end-to-end through the production fold --- the firing law
/// completes after the blip, not merely "the intent was withheld".
#[test]
fn list_failed_blip_preserves_streak_to_fire() {
    use crate::reconcilers::pool::candidate::{CandidateNode, PoolStreaks};
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    let gated = two_gated_folds(&mut streaks, &key, t0);
    let candidates = vec![CandidateNode {
        name: "n1".into(),
        labels: Default::default(),
        schedulable: true,
    }];

    // t0+20s: node LIST fails. Pre-fix this spawned drv-x; the harness
    // mirrors the reconcile's existing-name exclusion --- a spawned
    // intent leaves `wanted` until its Job ends (>=180s deadline, far
    // past this test's window).
    let tick = streaks.begin_tick(&key);
    let blip = evaluate_spawn_gate(
        vec![gated.clone()],
        &GateUniverse::ListFailed,
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        t0 + std::time::Duration::from_secs(20),
    );
    let job_held = !blip.spawnable.is_empty();

    // t0+30s: the LIST recovers. Post-fix drv-x is still jobless and
    // wanted; the completed fold gates it (streak 3) and the verdict
    // fires (floor: 30s since first_gated >= 20s). Pre-fix drv-x is
    // job-held --- excluded from wanted --- so nothing fires.
    let wanted = if job_held {
        vec![]
    } else {
        vec![gated.clone()]
    };
    let tick = streaks.begin_tick(&key);
    let outcome = evaluate_spawn_gate(
        wanted,
        &GateUniverse::Nodes(candidates),
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        t0 + std::time::Duration::from_secs(30),
    );
    assert_eq!(
        outcome
            .to_report
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["drv-x"],
        "exhaustion report after the blip --- the streak must survive a \
         fold-skip tick and fire on the next gated fold"
    );
}

// r[verify ctrl.pool.no-eligible-persist+5]
/// bug_075 companion polarity pin (GREEN both sides --- disclosed, not
/// a red): the withhold is self-limiting by the SAME staleness law as
/// the retain --- once the streak entry is older than the orphan
/// window it is dead evidence, the intent re-reads as fail-open, and a
/// PERSISTENT node-LIST failure cannot wedge spawn forever.
///
/// Witness-strength: certifies the ListFailed x stale-streak census
/// cell (fail-open restored at expiry); the live-streak withhold is
/// red 1's proposition, deliberately not re-asserted here so this pin
/// stays green on pre-fix code too (polarity disclosure).
#[test]
fn persistent_list_failure_restores_fail_open_after_expiry() {
    use crate::reconcilers::pool::candidate::PoolStreaks;
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    let gated = two_gated_folds(&mut streaks, &key, t0);

    // t0+140s: the entry (touched t0+10s) is 130s old --- past the
    // 120s orphan window. Fail-open applies again.
    let tick = streaks.begin_tick(&key);
    let outcome = evaluate_spawn_gate(
        vec![gated],
        &GateUniverse::ListFailed,
        &mut streaks,
        tick,
        &key,
        DemandCoverage::Complete,
        t0 + std::time::Duration::from_secs(140),
    );
    assert_eq!(
        outcome
            .spawnable
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["drv-x"],
        "a stale streak is dead evidence: persistent LIST failure must \
         restore fail-open at the orphan expiry"
    );
}

// r[verify ctrl.pool.no-eligible-persist+5]
/// bug_075 census (R15): the spawn decision is a total function over
/// (universe arm x per-intent evidence state). Walks EVERY cell of the
/// product through the REAL `evaluate_spawn_gate` --- the member lists
/// are `GateUniverse::ALL_DISCRIMINANTS` and `EvidenceState::ALL`,
/// each pinned exhaustive by a same-file match, so a new arm or
/// evidence state is a compile error at its pin AND a panic here until
/// its cells are stated. Pre-fix red on exactly the
/// ListFailed x live-streak cell (1 of 12 non-vacuous cells --- the
/// pigeonhole exhibit), recorded verbatim in the close commit.
///
/// Witness-strength: certifies the PRODUCTION fold's disposition per
/// cell (not a parallel table) --- every state is built through
/// production noting/step/death calls, observations only via
/// `from_partition` inside the fold itself.
///
/// Cell law (post-fix):
///   - NoWanted x *: vacuous (empty wanted --- nothing spawns).
///   - NoExclusions x {no-evidence, live-streak, stale-streak}: spawn
///     (a completed fold's evaluated-ungated read IS observed
///     recovery); x in-backoff: withhold.
///   - Nodes(gated) x {no-evidence, live-streak, stale-streak}:
///     withhold + step (the gate withholds from tick 1);
///     x in-backoff: withhold.
///   - ListFailed x no-evidence: spawn (fail-open preserved);
///     x live-streak: WITHHOLD (the bug_075 cell);
///     x stale-streak: spawn (staleness re-opens fail-open);
///     x in-backoff: withhold (the universal post-arm filter).
#[test]
fn spawn_decision_census_covers_arm_by_evidence_product() {
    use crate::reconcilers::pool::candidate::{CandidateNode, EvidenceState, PoolStreaks};
    use crate::reconcilers::pool::jobs::{GateUniverse, evaluate_spawn_gate};

    let key = pkey();
    let candidates = vec![CandidateNode {
        name: "n1".into(),
        labels: Default::default(),
        schedulable: true,
    }];

    for disc in GateUniverse::ALL_DISCRIMINANTS {
        for state in EvidenceState::ALL {
            let t0 = std::time::Instant::now();
            let mut streaks = PoolStreaks::default();
            // The intent under test: excluded from the only candidate,
            // so the Nodes arm reads it gated (the arm's canonical
            // exhaustion shape; an ungated Nodes read is the
            // NoExclusions row's trivially-complete semantics).
            let gated = SpawnIntent {
                intent_id: "drv-x".into(),
                excluded_nodes: vec!["n1".into()],
                ..Default::default()
            };
            // Build the evidence state through PRODUCTION calls only,
            // then probe the cell at `now`.
            let now = match state {
                EvidenceState::NoEvidence => t0,
                // Two gated folds: entry touched at t0+10s; probe at
                // t0+20s (live).
                EvidenceState::LiveStreak => {
                    two_gated_folds(&mut streaks, &key, t0);
                    t0 + std::time::Duration::from_secs(20)
                }
                // Same prefix; probe at t0+140s (130s since touch ---
                // past the 120s orphan window).
                EvidenceState::StaleStreak => {
                    two_gated_folds(&mut streaks, &key, t0);
                    t0 + std::time::Duration::from_secs(140)
                }
                // One verdict-free death 5s ago: inside the 10s
                // first-death backoff floor.
                EvidenceState::InBackoff => {
                    streaks.note_verdict_free_death(&key, "drv-x", t0);
                    t0 + std::time::Duration::from_secs(5)
                }
            };
            assert_eq!(
                streaks.evidence_state(&key, "drv-x", now),
                state,
                "fixture must construct the {} state it claims",
                state.label()
            );

            let (universe, wanted) = match disc {
                "NoWanted" => (GateUniverse::NoWanted, vec![]),
                "NoExclusions" => {
                    // The NoExclusions arm requires an exclusion-free
                    // wanted set by construction; same intent id so
                    // the evidence state carries.
                    let open = SpawnIntent {
                        intent_id: "drv-x".into(),
                        ..Default::default()
                    };
                    (GateUniverse::NoExclusions, vec![open])
                }
                "Nodes" => (GateUniverse::Nodes(candidates.clone()), vec![gated.clone()]),
                "ListFailed" => (GateUniverse::ListFailed, vec![gated.clone()]),
                other => panic!("new GateUniverse arm {other}: state its census cells here"),
            };
            // The discriminant pin round-trips: the constructed
            // universe IS the cell's arm (a stale ALL_DISCRIMINANTS
            // entry cannot silently walk the wrong arm).
            assert_eq!(universe.discriminant(), disc);
            let spawns = match (disc, state) {
                ("NoWanted", _) => false, // vacuous: empty wanted
                ("NoExclusions", EvidenceState::InBackoff) => false,
                ("NoExclusions", _) => true,
                ("Nodes", _) => false, // gated: withheld from tick 1
                ("ListFailed", EvidenceState::NoEvidence) => true,
                ("ListFailed", EvidenceState::LiveStreak) => false, // the bug_075 cell
                ("ListFailed", EvidenceState::StaleStreak) => true,
                ("ListFailed", EvidenceState::InBackoff) => false,
                (other, s) => panic!(
                    "census cell ({other}, {}) has no stated disposition",
                    s.label()
                ),
            };
            let tick = streaks.begin_tick(&key);
            let outcome = evaluate_spawn_gate(
                wanted,
                &universe,
                &mut streaks,
                tick,
                &key,
                DemandCoverage::Complete,
                now,
            );
            assert_eq!(
                outcome.spawnable.iter().any(|i| i.intent_id == "drv-x"),
                spawns,
                "census cell ({disc}, {}): expected spawns={spawns}",
                state.label()
            );
        }
    }
}

// ───────────────────────────────────────────────────────────────────
// merged_bug_080(2a): respawn-record lifetime across cycle phases
// ───────────────────────────────────────────────────────────────────

/// merged_bug_080(2a) census alphabet (R15): the CLOSED set of cycle
/// phases a (pool, intent) pair occupies between two verdict-free
/// deaths. `ALL` is pinned exhaustive by the same-file match in
/// [`CyclePhase::production_loop_shape`]'s caller (the census test):
/// adding a phase is a compile error there until its survival law is
/// stated. The phase determines which PRODUCTION call the reconcile
/// makes for the intent each tick --- the census drives exactly that
/// call shape.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
enum CyclePhase {
    /// The intent is jobless and the fold evaluates it: `step`'s
    /// evaluated-tick touch refreshes the record (pre-existing lane).
    JoblessEvaluated,
    /// A Pending Job holds the name: the intent left `wanted`, only
    /// the LIST refresh (`note_job_alive`) can reach the record.
    JobPending,
    /// A Running Job holds the name: same structural lane as Pending
    /// (the listing is phase-blind for the refresh).
    JobRunning,
    /// A terminal Job is listed but not yet reaped (<= the 600 s
    /// JOB_TTL window): still in the listing, still refreshed.
    TerminalUnreaped,
    /// The reap just consumed the terminal Job this tick:
    /// `note_verdict_free_death` re-anchors (touched = now).
    ReapedAwaitingRespawn,
    /// Jobless AND un-evaluated (fold-skip silence): NO refresh ---
    /// the 120 s orphan expiry is EXACTLY this phase's law.
    JoblessUnevaluated,
}

impl CyclePhase {
    const ALL: [CyclePhase; 6] = [
        CyclePhase::JoblessEvaluated,
        CyclePhase::JobPending,
        CyclePhase::JobRunning,
        CyclePhase::TerminalUnreaped,
        CyclePhase::ReapedAwaitingRespawn,
        CyclePhase::JoblessUnevaluated,
    ];
}

/// Drive ONE tick of the production loop shape for `phase` against
/// `streaks` (paused clock; `drv-other` is the sibling intent whose
/// completed folds keep the global expiry retain running, mirroring
/// sibling-pool/intent activity in the reconcile).
fn phase_tick(
    streaks: &mut crate::reconcilers::pool::candidate::PoolStreaks,
    key: &crate::reconcilers::pool::candidate::PoolKey,
    phase: CyclePhase,
    now: std::time::Instant,
) {
    use crate::reconcilers::pool::candidate::Observation;
    match phase {
        CyclePhase::JoblessEvaluated => {
            // The fold evaluates drv-x (un-gated) and drv-other.
            let ours = intent_named("drv-x");
            let other = intent_named("drv-other");
            let obs = Observation::from_partition(&[], &[ours, other]);
            let tick = streaks.begin_tick(key);
            let _ = streaks.step(
                tick,
                &obs,
                crate::reconcilers::pool::jobs::DemandCoverage::Complete,
                now,
            );
        }
        CyclePhase::JobPending | CyclePhase::JobRunning | CyclePhase::TerminalUnreaped => {
            // The Job LIST sees the same-named Job (any phase) and the
            // reconcile refreshes from the listing BEFORE the fold;
            // the fold then evaluates only the sibling (drv-x is
            // excluded by the existing-name filter).
            streaks.note_job_alive(key, std::iter::once("drv-x"), now);
            let other = intent_named("drv-other");
            let obs = Observation::from_partition(&[], &[other]);
            let tick = streaks.begin_tick(key);
            let _ = streaks.step(
                tick,
                &obs,
                crate::reconcilers::pool::jobs::DemandCoverage::Complete,
                now,
            );
        }
        CyclePhase::ReapedAwaitingRespawn => {
            // The reap notes the verdict-free death (re-anchor), then
            // the fold runs over the sibling.
            streaks.note_verdict_free_death(key, "drv-x", now);
            let other = intent_named("drv-other");
            let obs = Observation::from_partition(&[], &[other]);
            let tick = streaks.begin_tick(key);
            let _ = streaks.step(
                tick,
                &obs,
                crate::reconcilers::pool::jobs::DemandCoverage::Complete,
                now,
            );
        }
        CyclePhase::JoblessUnevaluated => {
            // Fold-skip silence for drv-x: only the sibling folds.
            let other = intent_named("drv-other");
            let obs = Observation::from_partition(&[], &[other]);
            let tick = streaks.begin_tick(key);
            let _ = streaks.step(
                tick,
                &obs,
                crate::reconcilers::pool::jobs::DemandCoverage::Complete,
                now,
            );
        }
    }
}

/// Production-shaped intent with a chosen id (the jobs_tests sibling
/// of candidate.rs's `intent_for`).
fn intent_named(id: &str) -> SpawnIntent {
    SpawnIntent {
        intent_id: id.into(),
        ..Default::default()
    }
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2a) red 1 (recorded verbatim in the close commit):
/// a respawn record MUST survive the Job-alive phase. Paused clock:
/// verdict-free death at t0 (deaths=1, backoff 10 s); the Job is
/// listed alive [t0, t0+200 s] with the intent excluded from wanted
/// (un-evaluated folds at 10 s cadence stepping the SIBLING so the
/// global expiry retain runs); second verdict-free death at t0+200 s.
/// deaths must escalate to 2 (backoff 20 s). Pre-fix the record
/// expired at t0+120 s and the second death minted a FRESH record
/// (deaths=1, backoff 10 s): `left: 10, right: 20`.
///
/// Witness-strength: certifies record SURVIVAL through the job-held
/// phase measured by the escalated backoff (deaths accumulate), not
/// by map internals. Strawman disclosure: pre-fix the refresh lane
/// does not exist --- the red was recorded with `note_job_alive`'s
/// body no-op'd (the API must exist for the census to compile).
#[test]
fn job_alive_phase_does_not_expire_respawn_record() {
    use crate::reconcilers::pool::candidate::PoolStreaks;

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    streaks.note_verdict_free_death(&key, "drv-x", t0);

    // Job alive (Pending/Running) for 200 s at 10 s cadence.
    for n in 1..=20u64 {
        phase_tick(
            &mut streaks,
            &key,
            CyclePhase::JobPending,
            t0 + std::time::Duration::from_secs(n * 10),
        );
    }
    let t_death2 = t0 + std::time::Duration::from_secs(200);
    streaks.note_verdict_free_death(&key, "drv-x", t_death2);

    // deaths=2 ⇒ the backoff floor is 20 s: blocked at +15 s, free at
    // +25 s. A fresh (expired-and-reminted) record would be deaths=1
    // ⇒ 10 s: already free at +15 s.
    let backoff_secs =
        if streaks.respawn_blocked(&key, "drv-x", t_death2 + std::time::Duration::from_secs(15)) {
            20
        } else {
            10
        };
    assert_eq!(
        backoff_secs, 20,
        "the second verdict-free death must ESCALATE the backoff (record \
         survived the job-held phase), not restart it"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2a) red 2 (recorded verbatim in the close commit):
/// same law through the TERMINAL-UNREAPED phase --- a terminal Job
/// listed for the 600 s JOB_TTL window is the observable artifact of
/// the terminal phase and must refresh the record. Pre-fix shape:
/// `left: 10, right: 20`.
///
/// Witness-strength: certifies the TerminalUnreaped refresh lane
/// specifically (the JOB_TTL=600 alive floor, the census's 120<600
/// miss cell); same escalated-backoff observable as red 1.
#[test]
fn respawn_record_survives_terminal_unreaped_window() {
    use crate::reconcilers::pool::candidate::PoolStreaks;

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    streaks.note_verdict_free_death(&key, "drv-x", t0);

    // Terminal-but-listed for 600 s at 10 s cadence (the JOB_TTL
    // phase).
    for n in 1..=60u64 {
        phase_tick(
            &mut streaks,
            &key,
            CyclePhase::TerminalUnreaped,
            t0 + std::time::Duration::from_secs(n * 10),
        );
    }
    let t_death2 = t0 + std::time::Duration::from_secs(600);
    streaks.note_verdict_free_death(&key, "drv-x", t_death2);

    let backoff_secs =
        if streaks.respawn_blocked(&key, "drv-x", t_death2 + std::time::Duration::from_secs(15)) {
            20
        } else {
            10
        };
    assert_eq!(
        backoff_secs, 20,
        "the record must survive the terminal-unreaped window (JOB_TTL \
         600 s > 120 s expiry) and escalate on the next death"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2a) census (R15): the record's retention horizon
/// dominates EVERY cycle phase in which the intent is structurally
/// un-evaluated --- exhaustively over the closed [`CyclePhase`]
/// alphabet (the same-file `ALL` pin + this match: a new phase fails
/// compilation here until its survival law is stated). Each cell
/// drives >120 s of the phase's PRODUCTION loop shape on a paused
/// clock and asserts the record's survival law for that phase:
/// refresh lanes keep it (deaths escalate on the next death), the
/// jobless-unevaluated cell EXPIRES it (the orphan semantics --- the
/// expiry's one remaining job). Pre-fix red on the JobPending,
/// JobRunning, and TerminalUnreaped cells (3 of 6 --- the pigeonhole
/// exhibit; the book's working estimate said 2 of 6, the executed
/// census says 3: Pending and Running are distinct alphabet cells
/// even though one refresh lane covers both).
///
/// Witness-strength: certifies REFRESH-LANE COVERAGE per phase
/// through production calls only (note_job_alive / step /
/// note_verdict_free_death), not a parallel table.
#[test]
fn cycle_phase_census_refreshes_every_observable_phase() {
    use crate::reconcilers::pool::candidate::PoolStreaks;

    for phase in CyclePhase::ALL {
        let mut streaks = PoolStreaks::default();
        let key = pkey();
        let t0 = std::time::Instant::now();
        streaks.note_verdict_free_death(&key, "drv-x", t0);

        // >120 s of the phase's production loop shape (13 ticks x 10 s).
        for n in 1..=13u64 {
            phase_tick(
                &mut streaks,
                &key,
                phase,
                t0 + std::time::Duration::from_secs(n * 10),
            );
        }
        let t_probe = t0 + std::time::Duration::from_secs(130);

        // Survival law per cell: a surviving record escalates
        // (deaths>=2 ⇒ backoff >= 20 s); an expired one restarts
        // (deaths=1 ⇒ 10 s).
        let survives = match phase {
            CyclePhase::JoblessEvaluated => true,
            CyclePhase::JobPending => true,
            CyclePhase::JobRunning => true,
            CyclePhase::TerminalUnreaped => true,
            CyclePhase::ReapedAwaitingRespawn => true,
            CyclePhase::JoblessUnevaluated => false,
        };
        streaks.note_verdict_free_death(&key, "drv-x", t_probe);
        let escalated =
            streaks.respawn_blocked(&key, "drv-x", t_probe + std::time::Duration::from_secs(15));
        // ReapedAwaitingRespawn ticks call note_verdict_free_death 13
        // times (deaths pile up) --- still "survives" (escalated).
        assert_eq!(
            escalated, survives,
            "cycle-phase census cell {phase:?}: record survival law violated"
        );
    }
}

// ───────────────────────────────────────────────────────────────────
// merged_bug_080(2b): reset only on verdict-bearing evidence
// ───────────────────────────────────────────────────────────────────

/// A terminal (succeeded) Job carrying the intent annotation, as the
/// apiserver lists it through the JOB_TTL window.
fn terminal_job_for_intent(name: &str, intent_id: &str) -> Job {
    let mut j = running_job_for_intent(name, intent_id);
    j.status = Some(JobStatus {
        ready: Some(0),
        succeeded: Some(1),
        ..Default::default()
    });
    j
}

/// A Job whose `activeDeadlineSeconds` fired: `Failed/DeadlineExceeded`
/// condition set, intent annotation carried (the
/// `report_deadline_exceeded_jobs` input shape).
fn deadline_exceeded_job(name: &str, intent_id: &str) -> Job {
    let mut j = running_job_for_intent(name, intent_id);
    j.status = Some(JobStatus {
        ready: Some(0),
        failed: Some(1),
        conditions: Some(vec![k8s_openapi::api::batch::v1::JobCondition {
            type_: "Failed".into(),
            status: "True".into(),
            reason: Some("DeadlineExceeded".into()),
            ..Default::default()
        }]),
        ..Default::default()
    });
    j
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2b) red 4 (recorded verbatim in the close commit): a
/// charge-free ack MUST NOT reset a respawn record. The reap steps the
/// record (verdict-free death — the production noting call), then the
/// same-tick `report_deadline_exceeded_jobs` lands its report on the
/// mock admin, which acks `ReportAttemptOutcomeResponse::default()` —
/// `attempt_resolved=false`, the exact charge-free shape the scheduler
/// returns for a never-pulled Job's deadline report (no matching
/// attempt). The record must still block. Pre-fix every `Ok(_)` ack
/// reset it: `left: false, right: true`.
///
/// Witness-strength (consumption-only scope): certifies the CONTROLLER
/// CONSUMES `attempt_resolved` — a false bit resets nothing through
/// the production report conduit; producer truthfulness per arm is
/// S3's `report_ack_attempt_resolved_per_arm_census` (WO-S3-P), not
/// re-proven here.
#[tokio::test]
async fn noop_ack_does_not_reset_respawn_record() {
    use crate::reconcilers::pool::job::report_deadline_exceeded_jobs;

    let (client, _verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client).await;
    let key = pkey();

    // The reap observed a verdict-free death this tick (deaths=1,
    // 10 s floor).
    let t0 = std::time::Instant::now();
    ctx.exhausted_streak
        .lock()
        .note_verdict_free_death(&key, "drv-ddl", t0);

    // Same-tick deadline report over the PRE-reap listing; the mock
    // acks charge-free (attempt_resolved=false by design — the
    // rolling-skew default a pre-field scheduler also produces).
    let job = deadline_exceeded_job("rio-builder-p-ddl1", "drv-ddl");
    report_deadline_exceeded_jobs(&ctx, &[job], &key).await;
    assert_eq!(
        mock.outcome_calls.read().unwrap().len(),
        1,
        "the deadline report reached the wire (conduit sanity)"
    );

    let blocked = ctx.exhausted_streak.lock().respawn_blocked(
        &key,
        "drv-ddl",
        t0 + std::time::Duration::from_secs(5),
    );
    assert!(
        blocked,
        "an acknowledgment carrying no attempt-resolution witness must \
         not reset the respawn record (blocked: false, expected true)"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2b) red 5 (recorded verbatim in the close commit): a
/// worker-closed death is NOT verdict-free. The terminal reap finds no
/// OPEN attempt (the attempt already closed — the scheduler holds an
/// adjudicated verdict) but the served `recently_closed` window
/// carries the BUILD close for the intent; the reap must not mint a
/// respawn record. Pre-fix `open_attempts_best_effort` discarded
/// `recently_closed` and the healthy adjudicated retry was taxed:
/// `left: true, right: false`.
///
/// Witness-strength (consumption-only scope): certifies the reap's
/// death CLASSIFICATION consumes the full view (open + recently-closed
/// build attempts) through the production reap conduit; the window's
/// content is the scheduler's (S3-certified) word.
#[tokio::test]
async fn worker_closed_death_is_not_verdict_free() {
    use rio_proto::types::CloseCause;

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // The attempt closed worker-side moments ago: no open attempt,
    // one recently-closed BUILD entry (explicit kind — the
    // consumption lane this red certifies).
    mock.open_attempts.write().unwrap().recently_closed = vec![rio_proto::types::ClosedAttempt {
        intent_id: "wkc".into(),
        exec_id: "exec-w1".into(),
        cause: CloseCause::Completed as i32,
        closed_age_secs: 30,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
    }];

    // Terminal Job for a STILL-WANTED intent → the reap's "terminal"
    // arm (background delete) and the verdict-free classification.
    let job = terminal_job_for_intent("rio-builder-p-wkc", "wkc");
    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-builder-p-wkc",
        body_contains: Some(r#""propagationPolicy":"Background""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);
    // live_051(e) flip: strike pass, then the reap pass.
    let strike_pass = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&[intent_named("wkc")], "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    assert!(strike_pass.is_empty(), "strike 1 defers (live_051(e))");
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &[job],
        &want_complete(&[intent_named("wkc")], "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    guard.verified().await;
    assert_eq!(reaped, HashSet::from(["rio-builder-p-wkc".into()]));

    let blocked =
        ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "wkc", std::time::Instant::now());
    assert!(
        !blocked,
        "a death covered by a recently-closed BUILD attempt is not \
         verdict-free — the scheduler adjudicated it; taxing the retry \
         violates the breaker's own doc (blocked: true, expected false)"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2b) red 6 — the kind axis, differential form: the
/// recently-closed reset lane must ride the BUILD conjunct. Same
/// scenario twice, only `attempt_kind` differs: MATERIALIZATION and
/// UNSPECIFIED closes leave the record blocking (fail-closed — a store
/// replica's close is not build progress, and an UNSPECIFIED kind from
/// a pre-field scheduler must not enable spend); an explicit BUILD
/// close resets it. The negative half is green both sides (disclosed:
/// pre-fix no recently-closed consult exists at all); the BUILD half
/// is the recorded red — `left: true, right: false`.
///
/// Witness-strength (consumption-only scope): certifies the mint-4
/// kind gate at the cancel-arm consult — reset iff
/// `attempt_kind == BUILD` — through the production cancel-arm view
/// read; which kinds the scheduler stamps is S3's per-arm table.
#[tokio::test]
async fn materialization_close_does_not_reset_build_record() {
    use crate::reconcilers::pool::job::cancel_closed_attempt_jobs;
    use rio_proto::types::CloseCause;

    let (client, _verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // Two verdict-free deaths: 20 s floor — mid-backoff throughout.
    // merged_bug_036: the deaths PREDATE the close (t0 backdated 60 s;
    // the close below is 10 s old) — the chronology law resets only on
    // a close that postdates the latest recorded death, and this
    // test's BUILD half is exactly that motivating "close outran the
    // reap" world.
    let t0 = std::time::Instant::now() - std::time::Duration::from_secs(60);
    for n in 0..2u64 {
        ctx.exhausted_streak.lock().note_verdict_free_death(
            &key,
            "drv-kind",
            t0 + std::time::Duration::from_secs(n),
        );
    }
    let probe = t0 + std::time::Duration::from_secs(5);
    // One active Job binding NO close (cause Completed never selects;
    // the intent is disjoint anyway) — the consult still runs.
    let bystander = running_job_for_intent("rio-builder-p-other1", "drv-other");
    let closed_kind = |kind: i32| rio_proto::types::ClosedAttempt {
        intent_id: "drv-kind".into(),
        exec_id: "exec-k1".into(),
        cause: CloseCause::Completed as i32,
        closed_age_secs: 10,
        attempt_kind: kind,
    };

    // Negative half: MATERIALIZATION and UNSPECIFIED do not mint.
    mock.open_attempts.write().unwrap().recently_closed = vec![
        closed_kind(rio_proto::types::AttemptKind::Materialization as i32),
        closed_kind(rio_proto::types::AttemptKind::Unspecified as i32),
    ];
    cancel_closed_attempt_jobs(&jobs_api, std::slice::from_ref(&bystander), &ctx, "p", &key).await;
    assert!(
        ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "drv-kind", probe),
        "non-BUILD recently-closed kinds must not reset (fail-closed \
         spend-enabling lane; blocked: false, expected true)"
    );

    // BUILD half (the recorded red): an explicit BUILD close resets.
    mock.open_attempts.write().unwrap().recently_closed =
        vec![closed_kind(rio_proto::types::AttemptKind::Build as i32)];
    cancel_closed_attempt_jobs(&jobs_api, &[bystander], &ctx, "p", &key).await;
    assert!(
        !ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "drv-kind", probe),
        "a recently-closed BUILD attempt is a named resolution — the \
         scheduler adjudicated this intent's attempt (blocked: true, \
         expected false)"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_036 R1: a close cannot mask the death of a Job created
/// AFTER it. PROPOSITION CERTIFIED: the chronology conjunct at the
/// constructor (mint 4b), over production-shaped Jobs (real
/// `metadata.creation_timestamp`) and wire ClosedAttempt values,
/// through the production reap conduit — a recently-closed BUILD
/// entry at age 70s does NOT cover the verdict-free death of a
/// terminal Job created 20s ago, so the death is noted and the
/// ladder gates the respawn.
#[tokio::test]
async fn pre_creation_close_does_not_cover_a_death() {
    use rio_proto::types::CloseCause;

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // The close PREDATES the Job: closed 70s ago; Job created 20s ago.
    mock.open_attempts.write().unwrap().recently_closed = vec![rio_proto::types::ClosedAttempt {
        intent_id: "precre".into(),
        exec_id: "exec-pc1".into(),
        cause: CloseCause::Completed as i32,
        closed_age_secs: 70,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
    }];
    let mut job = terminal_job_for_intent("rio-builder-p-precre", "precre");
    {
        use k8s_openapi::jiff::{SignedDuration, Timestamp};
        job.metadata.creation_timestamp =
            Some(Time(Timestamp::now() - SignedDuration::from_secs(20)));
    }

    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-builder-p-precre",
        body_contains: Some(r#""propagationPolicy":"Background""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);
    // live_051(e) two-tick confirmation: strike pass, then the reap.
    let first = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&[intent_named("precre")], "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    assert!(first.is_empty(), "strike 1 defers (live_051(e))");
    let reaped = reap_stale_for_intents(
        &jobs_api,
        std::slice::from_ref(&job),
        &want_complete(&[intent_named("precre")], "p", ExecutorKind::Builder),
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &key,
    )
    .await;
    guard.verified().await;
    assert_eq!(reaped, HashSet::from(["rio-builder-p-precre".to_string()]));

    let blocked =
        ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "precre", std::time::Instant::now());
    assert!(
        blocked,
        "left: false (a close that predates the Job masked its death — \
         deaths == 0, full-cadence respawn) / right: true (deaths == 1; \
         the ladder gates the respawn)"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// W9-AT, note_resolution face (bug_122): the windowed-reset
/// chronology guard is INVARIANT in witness staleness. Fixed physical
/// config: deaths at now−30s and now−14s (count 2 ⇒ 20s window, so
/// retention is observable through respawn_blocked); a close whose
/// TRUE age at evaluation is 5s — ambiguous within the 10s slack of
/// the 14s-old death (5+10 ≥ 14) ⇒ RETAIN wholesale, at every s. The
/// wire stamp is what a fetch s seconds ago recorded (5−s); the mint
/// rebases it back. Pre-fix: the frozen stamp at s=2 read 3+10 < 14
/// ⇒ the record was ERASED by a close that does not provably
/// postdate the death (the same frozen-age inequality as the
/// captured bind/cover reds — one rebase helper closes all three
/// consumers).
#[test]
fn w9_at_note_resolution_retention_invariant_in_staleness() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};

    for s in [0u64, 2, 4] {
        let mut streaks = PoolStreaks::default();
        let key = pkey();
        let now = std::time::Instant::now();
        streaks.note_verdict_free_death(&key, "nr", now - std::time::Duration::from_secs(30));
        streaks.note_verdict_free_death(&key, "nr", now - std::time::Duration::from_secs(14));
        assert!(
            streaks.respawn_blocked(&key, "nr", now),
            "premise: two deaths ⇒ 20s window ⇒ blocked at now"
        );
        let witness = VerdictWitness::from_recently_closed_build(
            &rio_proto::types::ClosedAttempt {
                intent_id: "nr".into(),
                exec_id: "exec-nr".into(),
                cause: rio_proto::types::CloseCause::Completed as i32,
                closed_age_secs: 5 - s,
                attempt_kind: rio_proto::types::AttemptKind::Build as i32,
            },
            std::time::Duration::from_secs(s),
        )
        .expect("BUILD kind mints");
        streaks.note_resolution(&key, "nr", witness, now);
        assert!(
            streaks.respawn_blocked(&key, "nr", now),
            "left (s={s}): record erased — the frozen stamp read the \
             close as provably postdating the 14s-old death / right: \
             retained — the rebased age is ambiguous within the slack \
             at every s"
        );
    }
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_036 R2: a close cannot erase deaths recorded AFTER it.
/// PROPOSITION CERTIFIED: the chronology conjunct at the record — the
/// reset loop fed an in-window close at age 70s does NOT remove a
/// record whose latest death is 10s old (close_age + skew slack ≥
/// death age ⇒ retain wholesale).
#[test]
fn older_close_does_not_erase_newer_deaths() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let now = std::time::Instant::now();
    // The latest death is 5s old — inside its own 10s floor, so the
    // retained record is observable through respawn_blocked.
    streaks.note_verdict_free_death(&key, "newer", now - std::time::Duration::from_secs(5));

    let witness = VerdictWitness::from_recently_closed_build(
        &rio_proto::types::ClosedAttempt {
            intent_id: "newer".into(),
            exec_id: "exec-n1".into(),
            cause: rio_proto::types::CloseCause::Completed as i32,
            closed_age_secs: 70,
            attempt_kind: rio_proto::types::AttemptKind::Build as i32,
        },
        std::time::Duration::ZERO,
    )
    .expect("BUILD kind mints");
    streaks.note_resolution(&key, "newer", witness, now);

    assert!(
        streaks.respawn_blocked(&key, "newer", now),
        "left: record erased (the next tick respawns at full cadence) / \
         right: retained (the close predates the death it would erase)"
    );
}

// r[verify sec.executor.identity-token+3]
/// live_053 / D-053-1 red (owner-signed, 2026-06-10): an ERRING token
/// mint spawns NOTHING this tick. PROPOSITION CERTIFIED: through the
/// production transport-failure shape (dead channel — the same
/// timeout class as the live 134s scheduler stall that expired the 5s
/// mint RPC twice and launched 257 unauthenticatable-by-construction
/// builders), `mint_spawn_tokens` returns None — no token witness —
/// and the reconcile spawn match's None arm is `Vec::new()` with
/// `spawn_for_each` structurally unreachable from it (the coupling is
/// the typed match at the single consumer), so zero Jobs are created
/// and the intents stay queued scheduler-side (no Job, no ack). The
/// keyless-dev and empty-set arms stay on the Some path (parity
/// pinned below).
#[tokio::test]
async fn erring_token_mint_spawns_nothing_this_tick() {
    use crate::reconcilers::pool::jobs::mint_spawn_tokens;
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};
    use std::collections::HashMap;

    // W9-CB (E1): the driven mint failure must also INCREMENT the
    // skipped-tick counter — the PromQL face of the fail-closed law.
    let rec = DebuggingRecorder::new();
    let _g = ::metrics::set_default_local_recorder(&rec);

    // Production-shaped transport failure: the admin channel is dead
    // (connect_lazy to a closed port — every RPC errs like a
    // timeout/unavailable, the live shape).
    let (client, _verifier) = ApiServerVerifier::new();
    let ctx = super::test_ctx(client.clone());

    let intents = vec![intent_named("tok1"), intent_named("tok2")];
    let tokens = mint_spawn_tokens(&ctx, "p", &intents).await;
    assert!(
        tokens.is_none(),
        "left: Some(empty) — the erring mint read as a token-less \
         spawn authorization (257 dead builders) / right: None — no \
         witness, no spawn this tick"
    );

    // Parity legs: an EMPTY spawn set never round-trips (a vacuous
    // Some), and a healthy keyless scheduler answers Some with the
    // keyless discriminator set (MockAdmin::new() = the keyless
    // scheduler's truthful answer — bug_121).
    let empty = mint_spawn_tokens(&ctx, "p", &[]).await.expect("vacuous");
    assert_eq!(
        (empty.tokens, empty.keyless),
        (HashMap::new(), false),
        "empty set skips the RPC (vacuous witness, conservative face)"
    );
    let (client2, _v2) = ApiServerVerifier::new();
    let (ctx2, _mock, _h) = ctx_with_mock_admin(client2).await;
    let healthy = mint_spawn_tokens(&ctx2, "p", &intents)
        .await
        .expect("healthy keyless mint rides Some");
    assert_eq!(
        (healthy.tokens.len(), healthy.keyless),
        (0, true),
        "keyless dev parity: Ok(empty map, keyless=true) rides the \
         Some arm into the Keyless letter — token-less spawn by law"
    );

    // W9-CB: exactly ONE skipped tick counted, at the failing pool —
    // the vacuous and healthy legs above must not increment (a count
    // on a served tick would turn the symptom series into noise).
    // ppppp: snapshot exactly once, after all legs.
    let snap = rec.snapshotter().snapshot().into_vec();
    let skipped = snap.into_iter().find_map(|(k, _, _, v)| {
        let key = k.key();
        (key.name() == "rio_controller_spawn_mint_skipped_ticks_total"
            && key.labels().any(|l| l.key() == "pool" && l.value() == "p"))
        .then_some(v)
    });
    match skipped {
        Some(DebugValue::Counter(n)) => assert_eq!(
            n, 1,
            "one driven mint failure = one skipped tick; served legs silent"
        ),
        other => panic!(
            "left: {other:?} / right: Counter(1) — the mint-failure \
             skipped tick never reached the counter (E1's PromQL face)"
        ),
    }
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_036 overflow red: a wire `closed_age_secs` near
/// u64::MAX must saturate, never wrap — a wrapped tiny age INVERTS
/// both chronology guards (a record that must be retained gets
/// dropped; a close that covers nothing reads as covering).
/// PROPOSITION CERTIFIED: with closed_age_secs = u64::MAX - 1, the
/// record is RETAINED (saturation = arbitrarily old close) and the
/// reap-mask mint fails (no Job predates it).
#[test]
fn near_max_close_age_saturates_and_retains() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};

    let hostile = |intent: &str| rio_proto::types::ClosedAttempt {
        intent_id: intent.into(),
        exec_id: "exec-ov1".into(),
        cause: rio_proto::types::CloseCause::Completed as i32,
        closed_age_secs: u64::MAX - 1,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
    };

    // Reset lane: the record survives the hostile age.
    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let now = std::time::Instant::now();
    streaks.note_verdict_free_death(&key, "ovf", now - std::time::Duration::from_secs(5));
    let w = VerdictWitness::from_recently_closed_build(&hostile("ovf"), std::time::Duration::ZERO)
        .expect("BUILD mints");
    streaks.note_resolution(&key, "ovf", w, now);
    assert!(
        streaks.respawn_blocked(&key, "ovf", now),
        "left: record erased (the wrapped tiny close_age read as \
         postdating the death) / right: retained (saturated age)"
    );

    // Reap-mask lane: the hostile age covers nothing.
    let job = terminal_job_for_intent("rio-builder-p-ovf", "ovf");
    assert!(
        VerdictWitness::covers_job_death(&hostile("ovf"), &job, std::time::Duration::ZERO)
            .is_none(),
        "left: Some (the wrapped age postdated every Job) / right: None"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_036 R3: the restored envelope — respawn count under
/// sustained verdict-free fast-crash is LADDER-bounded inside a
/// renewable close window. PROPOSITION CERTIFIED structurally (spawn
/// DECISIONS counted over a synthetic tick clock, never wall-clock):
/// one adjudicated close at T=0 rides the scheduler's 120s
/// recently-closed window while generations fast-crash at the ~10s
/// reconcile cadence; the per-tick reset loop re-delivers the close
/// every tick, and the 10/20/40/80s floors hold from the first
/// UNCOVERED death.
#[test]
fn fast_crash_generations_meet_the_ladder_inside_a_close_window() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let base = std::time::Instant::now();
    let closed = |age: u64| rio_proto::types::ClosedAttempt {
        intent_id: "ladder".into(),
        exec_id: "exec-l1".into(),
        cause: rio_proto::types::CloseCause::Completed as i32,
        closed_age_secs: age,
        attempt_kind: rio_proto::types::AttemptKind::Build as i32,
    };

    let mut spawns = 0u32;
    for tick in 0..=12u64 {
        let now = base + std::time::Duration::from_secs(tick * 10);
        // The close happened at T=0; its wire age grows per tick and
        // stays inside the scheduler's 120s window for the whole run.
        let w = VerdictWitness::from_recently_closed_build(
            &closed(tick * 10),
            std::time::Duration::ZERO,
        )
        .expect("BUILD kind mints");
        streaks.note_resolution(&key, "ladder", w, now);
        if !streaks.respawn_blocked(&key, "ladder", now) {
            // The pool respawns; the generation fast-crashes
            // verdict-free within the tick.
            spawns += 1;
            streaks.note_verdict_free_death(&key, "ladder", now);
        }
    }
    assert!(
        spawns <= 5,
        "left: 13 full-cadence spawns (each tick's in-window close \
         erased the deaths recorded after it — the ladder neutralized \
         for a renewable 120s) / right: ladder-bounded ({spawns} ≤ 5: \
         the 10/20/40/80s floors hold from the first uncovered death)"
    );
}

// r[verify ctrl.pool.respawn-backoff+2]
/// merged_bug_080(2b) red 7 — the green companion to red 4: a
/// RESOLVING ack (`attempt_resolved=true`) still resets — the breaker
/// must never tax a real verdict. Both mint polarities at the seam:
/// the true bit mints the witness and the typed reset clears the
/// record; the default (false) bit mints NOTHING — the type-level
/// half of red 4.
///
/// Polarity disclosure: the in-process MockAdmin acks
/// `attempt_resolved=false` BY DESIGN (the WO-S3-P consumption-mock
/// note), so the true polarity drives the production mint
/// (`VerdictWitness::from_resolved_ack`) + reset conduit directly on
/// the wire response shape — the same typed calls the three job.rs
/// ack arms make. Witness-strength (consumption-only scope):
/// certifies the controller's consumption of the bit, both
/// polarities; which arms the scheduler stamps true is S3's per-arm
/// table. No pre-fix red exists for this test: the witness type does
/// not compile pre-fix (the pre-fix green half is the existing
/// `verdict_resets_respawn_backoff` shape).
#[test]
fn resolved_ack_still_resets() {
    use crate::reconcilers::pool::candidate::{PoolStreaks, VerdictWitness};

    let mut streaks = PoolStreaks::default();
    let key = pkey();
    let t0 = std::time::Instant::now();
    streaks.note_verdict_free_death(&key, "drv-res", t0);
    let probe = t0 + std::time::Duration::from_secs(5);
    assert!(
        streaks.respawn_blocked(&key, "drv-res", probe),
        "mid-backoff before the verdict"
    );

    // The charge-free polarity mints nothing (the type-level half of
    // red 4: there is no witness to reset with).
    assert!(
        VerdictWitness::from_resolved_ack(&rio_proto::types::ReportAttemptOutcomeResponse {
            attempt_resolved: false,
        })
        .is_none(),
        "a charge-free ack must not mint a reset witness"
    );

    // The resolving polarity mints, and the reset lane clears.
    let witness =
        VerdictWitness::from_resolved_ack(&rio_proto::types::ReportAttemptOutcomeResponse {
            attempt_resolved: true,
        })
        .expect("a resolved ack mints the witness");
    streaks.note_resolution(&key, "drv-res", witness, std::time::Instant::now());
    assert!(
        !streaks.respawn_blocked(&key, "drv-res", probe),
        "a resolving ack resets immediately — the breaker never taxes \
         a real verdict"
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
        ..Default::default()
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

    let selected = select_closed_attempt_jobs(&active, &open, &window, std::time::Duration::ZERO);
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
        select_closed_attempt_jobs(&active, &open, &[], std::time::Duration::ZERO).is_empty(),
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
        ..Default::default()
    }];
    assert!(
        select_closed_attempt_jobs(&active, &[], &window, std::time::Duration::ZERO).is_empty(),
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
        ..Default::default()
    }];

    let guard = verifier.run(vec![delete_scenario("rio-builder-p-edge1")]);
    let cancelled = cancel_closed_attempt_jobs(&jobs_api, &[job], &ctx, "test-pool", &pkey()).await;
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
    let cancelled = cancel_closed_attempt_jobs(&jobs_api, &[job], &ctx, "test-pool", &pkey()).await;
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
    let reaped = reap_orphan_running(&jobs_api, &[job], &HashSet::new(), &ctx, "p", &pkey()).await;
    assert_eq!(
        reaped, 0,
        "a leader younger than the orphan grace must not reap \
         never-pulled pods (they get one full grace against the NEW leader)"
    );
}

// r[verify sec.executor.identity-token+3]
/// W9-AR (bug_121): no witness, no spawn — PER INTENT. A mixed
/// HMAC-mode mint (token for A; B OMITTED — the two-RPC read-read
/// race; keyless=false) spawns A this tick and SKIPS B entirely: no
/// Job, no NameCollision, no verdict-free backoff tax. B spawns next
/// tick once minted — the race window closes without the
/// dead-builder detour. The verifier's strict scenario sequence pins
/// exactly TWO creates across both ticks (A at tick 1, B at tick 2).
///
/// Pre-fix red (strawman-DISCLOSED: the shipped fold was inline in
/// reconcile — its decision line
/// `executor_tokens.get(&intent.intent_id).map(String::as_str)`
/// quoted and driven through production spawn_for_each): B's create
/// went out token-less — the doomed Job. Transcript in the commit
/// body.
#[tokio::test]
async fn w9_ar_omitted_intent_skips_then_spawns_once_minted() {
    use crate::reconcilers::pool::jobs::{
        TokenDisposition, filter_spawnable_by_token, mint_spawn_tokens,
    };

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, mock, _h) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");
    let key = pkey();

    // HMAC-mode mint: token for A only; B omitted.
    {
        let mut m = mock.mint_tokens.write().unwrap();
        m.keyless = false;
        m.tokens = [("aaa".to_string(), "tok-a".to_string())].into();
    }
    let intents = vec![intent_named("aaa"), intent_named("bbb")];

    // Exactly TWO creates total across both ticks.
    let guard = verifier.run(vec![
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

    // The production closure's letter consumption, minimal Job body
    // (the law under test is which intents reach the spawn and with
    // what token evidence — not the Job spec, covered elsewhere).
    let drive_tick = |grants: crate::reconcilers::pool::jobs::TokenGrants,
                      spawnable: Vec<SpawnIntent>,
                      jobs_api: Api<Job>| async move {
        let mut seen: Vec<(String, Option<String>)> = Vec::new();
        let spawned = spawn_for_each(&jobs_api, &spawnable, &HashSet::new(), "p", |intent| {
            let token = match grants.disposition(&intent.intent_id) {
                TokenDisposition::Token(t) => Some(t),
                TokenDisposition::Keyless => None,
                TokenDisposition::Omitted => panic!("Omitted reached the spawn"),
            };
            seen.push((intent.intent_id.clone(), token.map(str::to_owned)));
            Ok(Job {
                metadata: ObjectMeta {
                    name: Some(format!("rio-builder-p-{}", intent.intent_id)),
                    ..Default::default()
                },
                ..Default::default()
            })
        })
        .await;
        (spawned, seen)
    };

    // Tick 1: mint → per-intent fold → spawn. B is Omitted.
    let grants = mint_spawn_tokens(&ctx, "p", &intents).await.expect("Ok");
    let spawnable = filter_spawnable_by_token("p", &grants, &intents);
    assert_eq!(
        spawnable
            .iter()
            .map(|i| i.intent_id.as_str())
            .collect::<Vec<_>>(),
        vec!["aaa"],
        "the Omitted intent is filtered BEFORE the spawn"
    );
    let (spawned, seen) = drive_tick(grants, spawnable, jobs_api.clone()).await;
    assert_eq!(spawned.len(), 1, "A spawns this tick");
    assert_eq!(
        seen,
        vec![("aaa".to_string(), Some("tok-a".to_string()))],
        "exactly A reaches the spawn, WITH its token"
    );
    // No NameCollision possible (no Job exists for B) and no backoff
    // tax (no verdict-free death was noted — nothing was reaped).
    assert!(
        !ctx.exhausted_streak
            .lock()
            .respawn_blocked(&key, "bbb", std::time::Instant::now()),
        "skipping must not tax B's respawn record"
    );

    // Tick 2: the scheduler now mints B (the drv re-presented).
    mock.mint_tokens
        .write()
        .unwrap()
        .tokens
        .insert("bbb".to_string(), "tok-b".to_string());
    let grants2 = mint_spawn_tokens(&ctx, "p", &intents).await.expect("Ok");
    let spawnable2 = filter_spawnable_by_token("p", &grants2, &intents);
    // A's Job already exists — production passes existing_names as
    // the skip set; replicate it so only B creates.
    let spawnable2: Vec<SpawnIntent> = spawnable2
        .into_iter()
        .filter(|i| i.intent_id != "aaa")
        .collect();
    let (spawned2, seen2) = drive_tick(grants2, spawnable2, jobs_api).await;
    guard.verified().await;
    assert_eq!(spawned2.len(), 1, "B spawns next tick");
    assert_eq!(
        seen2,
        vec![("bbb".to_string(), Some("tok-b".to_string()))],
        "B spawns once minted — the race closed without the detour"
    );
}

// r[verify sec.executor.identity-token+3]
/// W9-AS (bug_121): the discriminator's dual face — keyless dev mode
/// (`keyless=true`, empty map) spawns EVERY intent token-less,
/// exactly as today, knob-free. The keyless-deployment validation
/// lane (hazard ggggg: keyless e2e flows are a standing population —
/// dev parity breaks are red here, not in prod) is the cited
/// population. Pre-fix this face was indistinguishable on the wire
/// from whole-batch omission; the law now keys on the typed letter.
#[tokio::test]
async fn w9_as_keyless_mode_spawns_every_intent_tokenless() {
    use crate::reconcilers::pool::jobs::{
        TokenDisposition, filter_spawnable_by_token, mint_spawn_tokens,
    };

    let (client, verifier) = ApiServerVerifier::new();
    // MockAdmin::new() defaults keyless=true with an empty map — the
    // truthful keyless scheduler.
    let (ctx, _mock, _h) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let intents = vec![intent_named("aaa"), intent_named("bbb")];
    let guard = verifier.run(vec![
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

    let grants = mint_spawn_tokens(&ctx, "p", &intents).await.expect("Ok");
    assert!(grants.keyless, "the mock declares keyless");
    let spawnable = filter_spawnable_by_token("p", &grants, &intents);
    assert_eq!(spawnable.len(), 2, "keyless filters nothing");
    let mut seen: Vec<(String, Option<String>)> = Vec::new();
    let spawned = spawn_for_each(&jobs_api, &spawnable, &HashSet::new(), "p", |intent| {
        let token = match grants.disposition(&intent.intent_id) {
            TokenDisposition::Token(t) => Some(t),
            TokenDisposition::Keyless => None,
            TokenDisposition::Omitted => panic!("Omitted under keyless"),
        };
        seen.push((intent.intent_id.clone(), token.map(str::to_owned)));
        Ok(Job {
            metadata: ObjectMeta {
                name: Some(format!("rio-builder-p-{}", intent.intent_id)),
                ..Default::default()
            },
            ..Default::default()
        })
    })
    .await;
    guard.verified().await;
    assert_eq!(spawned.len(), 2, "both spawn under keyless");
    assert_eq!(
        seen,
        vec![("aaa".to_string(), None), ("bbb".to_string(), None),],
        "token-less by LAW (the Keyless letter), not by accident"
    );
}

// r[verify ctrl.pool.demand-completeness]
/// W9-AW, round-10 per-class form (the reap-authority law table): a
/// `queued` count may drive excess-Pending reaping only when the
/// scheduler answered, the placeable gate is armed, AND the demand is
/// BOUNDABLE. A complete page is exact; a truncated page understates
/// demand — a pending Job whose intent fell outside the page would
/// read as excess and be reaped while still wanted — so the truncated
/// arm consumes the SUM OF THE TYPED POPULATION CLASSES (Ready +
/// forecast aggregates; over-counting under-reaps), and a truncated
/// view where BOTH classes read zero (incoherent server) fail-closes
/// like scheduler-unreachable. The table walks the full cube × the
/// (ready, forecast) class product, pinning the merged_bug_006 rows:
/// a forecast-only aggregate BOUNDS the reap (the all-forecast page no
/// longer silently disables it — the old single-aggregate incoherence
/// premise died with the typed classes), and a mixed page is bounded
/// by the CLASS SUM, never the Ready class alone.
///
/// Pre-fix red (merged_bug_006, captured at the old 5-arg law before
/// the forecast axis landed): the forecast-only row
/// `(complete=false, ready=0, forecast=20)` returned `None` (reap
/// silently disabled) and the mixed row `(ready=20, forecast=15)`
/// returned `Some(20)` — under-bounding true wanted demand by the
/// whole forecast class; a forecast-backed Pending Job off-page read
/// as excess.
#[test]
fn reap_authority_requires_complete_demand_view() {
    use crate::reconcilers::pool::jobs::reap_queued_known;
    for sched_ok in [false, true] {
        for armed in [false, true] {
            for complete in [false, true] {
                for ready_upper in [0u32, 20] {
                    for forecast_upper in [0u32, 15] {
                        let got = reap_queued_known(
                            sched_ok,
                            armed,
                            complete,
                            7,
                            ready_upper,
                            forecast_upper,
                        );
                        let classes = ready_upper + forecast_upper;
                        let want = if !(sched_ok && armed) {
                            None
                        } else if complete {
                            Some(7) // exact post-filter count; classes ignored
                        } else if classes > 0 {
                            // max(page, class sum) — the superset bound;
                            // forecast-only (ready=0) still bounds.
                            Some(7u32.max(classes))
                        } else {
                            None // truncated + both classes zero: unboundable
                        };
                        assert_eq!(
                            got, want,
                            "authority law (sched_ok={sched_ok}, armed={armed}, \
                             complete={complete}, ready={ready_upper}, \
                             forecast={forecast_upper})"
                        );
                    }
                }
            }
        }
    }

    // The saturation corner: u32::MAX + u32::MAX must clamp, not wrap
    // into a small bound that would re-open over-reaping.
    assert_eq!(
        reap_queued_known(true, true, false, 7, u32::MAX, u32::MAX),
        Some(u32::MAX),
        "class sum saturates"
    );
}

/// The aggregate-bound projection ([`aggregate_upper_for`]): systems
/// absent from the map contribute zero, present ones sum, and the sum
/// SATURATES into `u32` (a u64 wire total beyond u32::MAX still yields
/// a usable — maximal — upper bound instead of wrapping into a small
/// number that would re-open over-reaping).
#[test]
fn aggregate_upper_sums_pool_systems_saturating() {
    use crate::reconcilers::pool::jobs::aggregate_upper_for;
    use std::collections::HashMap;
    let m: HashMap<String, u64> = [
        ("x86_64-linux".to_string(), 30u64),
        ("aarch64-linux".to_string(), 12),
        ("riscv64-linux".to_string(), u64::MAX),
    ]
    .into();
    let sys = |xs: &[&str]| xs.iter().map(|s| s.to_string()).collect::<Vec<_>>();
    assert_eq!(
        aggregate_upper_for(&sys(&["x86_64-linux", "aarch64-linux"]), &m),
        42,
        "present systems sum"
    );
    assert_eq!(
        aggregate_upper_for(&sys(&["x86_64-linux", "powerpc-none"]), &m),
        30,
        "absent systems contribute zero"
    );
    assert_eq!(aggregate_upper_for(&sys(&[]), &m), 0, "no systems => 0");
    assert_eq!(
        aggregate_upper_for(&sys(&["riscv64-linux", "x86_64-linux"]), &m),
        u32::MAX,
        "saturates, never wraps"
    );
}

// ───────────────────────────────────────────────────────────────────
// Round-10 WO-S4-1: the demand-completeness chokepoint (R26 banner)
// ───────────────────────────────────────────────────────────────────

// r[verify ctrl.pool.demand-completeness]
/// **W10-AF (merged_bug_006).** PROPOSITION: under a >page-limit
/// backlog (truncated view), a forecast-backed Pending Job whose
/// intent rotated off the page is NOT excess — the truncated-arm
/// bound sums BOTH typed population classes, so the bound covers it
/// and `select_excess_pending` selects nothing. Composition walked at
/// the law's own quantifier: the wire response (the production
/// `from_response` constructor), the authority law, and the excess
/// selection — end to end from the bytes the scheduler sends.
///
/// Pre-fix red (the 5-arg Ready-only law, captured with the forecast
/// class severed): the bound read max(2, 3) = 3 against 5 still-wanted
/// Pending Jobs — the two oldest (both forecast-backed, off-page)
/// selected for deletion while still wanted:
///   left: ["rio-old-fc-1", "rio-old-fc-2"] / right: []
#[test]
fn w10_af_forecast_backed_job_survives_truncated_bound() {
    use crate::reconcilers::pool::job::select_excess_pending;
    use crate::reconcilers::pool::jobs::{PoolDemandView, reap_queued_known};

    // The scheduler's answer at a 2-intent page over a 3-Ready +
    // 2-forecast backlog (page truncated): both classes aggregated
    // full-population.
    let resp = rio_proto::types::GetSpawnIntentsResponse {
        intents: vec![intent_named("on-page-r"), intent_named("on-page-f")],
        queued_by_system: [("x86_64-linux".to_string(), 3u64)].into(),
        forecast_by_system: [("x86_64-linux".to_string(), 2u64)].into(),
        ice_masked_cells: vec![],
        truncated: true,
    };
    let (page, evidence) =
        PoolDemandView::from_response(resp, &["x86_64-linux".to_string()]).split();
    let queued = page.len_page() as u32;
    let bound = reap_queued_known(
        true,
        true,
        evidence.coverage() == DemandCoverage::Complete,
        queued,
        evidence.ready_upper(),
        evidence.forecast_upper(),
    );
    assert_eq!(
        bound,
        Some(5),
        "truncated bound = max(page 2, ready 3 + forecast 2)"
    );

    // 5 still-wanted Pending Jobs (2 forecast-backed ones the oldest —
    // their intents are off-page). Post-fix: NOTHING is excess.
    let jobs = vec![
        pending_job("rio-old-fc-1", 0, 120),
        pending_job("rio-old-fc-2", 0, 100),
        pending_job("rio-r1", 0, 60),
        pending_job("rio-r2", 0, 50),
        pending_job("rio-r3", 0, 40),
    ];
    let excess = select_excess_pending(
        &jobs,
        &HashSet::new(),
        bound.expect("boundable"),
        std::time::Duration::ZERO,
    );
    let names: Vec<&str> = excess
        .iter()
        .filter_map(|j| j.metadata.name.as_deref())
        .collect();
    assert_eq!(
        names,
        Vec::<&str>::new(),
        "the class-sum bound covers every still-wanted Pending Job \
         (no forecast-backed Job reaped while wanted)"
    );
}

// r[verify ctrl.pool.demand-completeness]
/// **W10-AF, the fail-closed face (merged_bug_006 secondary).** An
/// ALL-FORECAST truncated page no longer silently disables the excess
/// reap: the forecast class bounds it. (The old single-aggregate law
/// read ready-agg 0 ⇒ `None` — fail-closed but silent, wrongly
/// branded "incoherent server"; the incoherence premise is now
/// per-class: BOTH classes zero.) RECORDED: truncated + both classes
/// zero stays the fail-closed `None` arm.
#[test]
fn w10_af_all_forecast_page_keeps_reap_bounded() {
    use crate::reconcilers::pool::jobs::{PoolDemandView, reap_queued_known};
    let resp = rio_proto::types::GetSpawnIntentsResponse {
        intents: vec![intent_named("fc-a")],
        queued_by_system: [].into(),
        forecast_by_system: [("x86_64-linux".to_string(), 4u64)].into(),
        ice_masked_cells: vec![],
        truncated: true,
    };
    let (page, ev) = PoolDemandView::from_response(resp, &["x86_64-linux".to_string()]).split();
    assert_eq!(
        reap_queued_known(
            true,
            true,
            ev.coverage() == DemandCoverage::Complete,
            page.len_page() as u32,
            ev.ready_upper(),
            ev.forecast_upper(),
        ),
        Some(4),
        "forecast-only truncated page is BOUNDED by its own class \
         (pre-fix: None — the reap silently disabled)"
    );
    // The honest incoherence arm survives at the per-class quantifier.
    assert_eq!(
        reap_queued_known(true, true, false, 1, 0, 0),
        None,
        "truncated + BOTH classes zero = incoherent server, fail-closed"
    );
}

// r[verify ctrl.pool.demand-completeness]
/// **W10-AG (merged_bug_029).** PROPOSITION: a still-wanted Pending
/// Job whose intent fell off the priority head (>page-limit backlog)
/// is NOT foreground-deleted by the orphan-pending arm — on an
/// INCOMPLETE view absence is unknowable ([`WantVerdict::Unknowable`])
/// and the destructive arm SUSPENDS (typed `orphan-suspended` letter,
/// counted), re-judged next tick. The verifier carries ZERO delete
/// scenarios, so the pre-fix arm's foreground DELETE has no backend —
/// the discriminating observable is the typed letter (absent pre-fix,
/// exactly-one post-fix) plus the structurally-empty reaped set.
///
/// Pre-fix red (the coverage-blind want-map — absence judged off the
/// bare page; the arm classified OrphanPending and issued the
/// foreground DELETE against the scenario-less verifier):
///   panicked at 'orphan-suspended letter not counted: None'
#[tokio::test]
async fn w10_ag_orphan_reap_suspends_on_incomplete_view() {
    use metrics_util::debugging::DebuggingRecorder;

    let rec = DebuggingRecorder::new();
    let _g = ::metrics::set_default_local_recorder(&rec);

    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    // One on-page intent (its Job lives); one OFF-PAGE Pending Job,
    // old enough for the grace. View INCOMPLETE (truncated page).
    let on_page = intent_named("onpage");
    let want = WantMap::for_pool(
        &IntentPage::for_test(vec![on_page]),
        DemandCoverage::Incomplete,
        "p",
        ExecutorKind::Builder,
    );
    let existing = vec![pending_job("rio-offpage-job", 0, 30)];

    // ZERO scenarios: any DELETE is a guard failure.
    let guard = verifier.run(vec![]);
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want,
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert!(
        reaped.is_empty(),
        "off-page absence is unknowable — the orphan arm suspends \
         instead of foreground-deleting a still-wanted Job"
    );
    guard.verified().await;

    // The suspension is OBSERVABLE: the typed letter counted once.
    // ppppp: snapshot exactly once.
    let snap = rec.snapshotter().snapshot().into_vec();
    let suspended = snap.into_iter().find_map(|(k, _, _, v)| {
        let key = k.key();
        (key.name() == "rio_controller_reap_dispositions_total"
            && key
                .labels()
                .any(|l| l.key() == "disposition" && l.value() == "orphan-suspended"))
        .then_some(v)
    });
    match suspended {
        Some(metrics_util::debugging::DebugValue::Counter(n)) => {
            assert_eq!(n, 1, "exactly one suspension letter this pass")
        }
        other => panic!("orphan-suspended letter not counted: {other:?}"),
    }
}

// r[verify ctrl.pool.demand-completeness]
/// The COMPLETE-view inverse of W10-AG: same off-page Pending Job,
/// but the view is complete — true negative evidence; the orphan arm
/// acts exactly as before (foreground delete after the grace). Pins
/// that the suspension narrows to incomplete views only (the 10s
/// grace semantics unchanged for complete views).
#[tokio::test]
async fn w10_ag_complete_view_orphan_reap_unchanged() {
    let (client, verifier) = ApiServerVerifier::new();
    let (ctx, _mock, _admin_handle) = ctx_with_mock_admin(client.clone()).await;
    let jobs_api: Api<Job> = Api::namespaced(client, "rio");

    let want = want_complete(&[intent_named("onpage")], "p", ExecutorKind::Builder);
    let existing = vec![pending_job("rio-orphan-job", 0, 30)];
    let guard = verifier.run(vec![Scenario {
        method: http::Method::DELETE,
        path_contains: "/namespaces/rio/jobs/rio-orphan-job",
        body_contains: Some(r#""propagationPolicy":"Foreground""#),
        status: 200,
        body_json: serde_json::to_string(&Job::default()).unwrap(),
    }]);
    let reaped = reap_stale_for_intents(
        &jobs_api,
        &existing,
        &want,
        &ctx,
        &crate::fixtures::test_pool("p", ExecutorKind::Builder),
        "p",
        &pkey(),
    )
    .await;
    assert_eq!(reaped, HashSet::from(["rio-orphan-job".to_string()]));
    guard.verified().await;
}

// r[verify ctrl.pool.demand-completeness]
/// **W10-AI, the demand-visibility half (round-10 merged_bug_012,
/// R25).** A window-DEFERRED intent holding a Pending Job: the gate
/// fold keeps it DEMAND-VISIBLE (want-map membership ⇒ the orphan arm
/// reads `Wanted`, no delete) while the spawn lane excludes it (the
/// fold's ternary letter — not spawnable this tick). Pre-fix the
/// deferred letter folded into the absent arm: stripped from the
/// page, its still-wanted Pending Job foreground-deleted at the 10s
/// grace.
///
/// Pre-fix red (Deferred arm severed to the pre-round-10 strip):
///   panicked at 'demand lane: the deferred intent survives the gate
///   fold (pre-fix: stripped)'
///     left: 1  right: 2
#[tokio::test]
async fn w10_ai_deferred_intent_stays_demand_visible_not_spawnable() {
    use crate::reconcilers::nodeclaim_pool::{FfdDisposition, PlaceableGate, PlacedTick};
    use crate::reconcilers::pool::jobs::apply_placeable_gate;

    // FFD tick: "reg" placed-on-registered; "def" window-deferred
    // (holds a Pending Job from an earlier tick).
    let gate = PlaceableGate::from_tick(PlacedTick::for_test(["reg"], ["def"]));
    let mut page = IntentPage::for_test(vec![intent_named("reg"), intent_named("def")]);
    let tick = apply_placeable_gate(&mut page, &gate).expect("armed");

    // Demand lane: BOTH survive the fold.
    assert_eq!(
        page.len_page(),
        2,
        "demand lane: the deferred intent survives the gate fold \
         (pre-fix: stripped)"
    );

    // Absence lane: the deferred intent's Job reads Wanted — the
    // orphan arm cannot classify it absent (the pre-fix delete path).
    let want = want_complete(
        &page.iter_page().cloned().collect::<Vec<_>>(),
        "p",
        ExecutorKind::Builder,
    );
    let def_job = crate::reconcilers::pool::pod::job_name(
        "p",
        ExecutorKind::Builder,
        "def", // intent_suffix("def") == "def" (lowercase alnum, <12)
    );
    assert!(
        matches!(
            want.verdict(&def_job),
            crate::reconcilers::pool::jobs::WantVerdict::Wanted(_)
        ),
        "the deferred intent's Pending Job is WANTED demand"
    );

    // Spawn lane: deferred is NOT spawnable this tick (the ternary
    // letter at the consumer — rustc-exhaustive, no if-let fold).
    assert_eq!(tick.disposition("reg"), FfdDisposition::PlacedRegistered);
    assert_eq!(tick.disposition("def"), FfdDisposition::Deferred);
    assert_eq!(tick.disposition("ghost"), FfdDisposition::Unplaced);
}

// r[verify ctrl.pool.demand-completeness]
/// **W10-AK (R25 consumer census).** Every consumer of the FFD tick
/// outcome matches on the FULL disposition alphabet — rustc
/// exhaustiveness at the typed letter (the `apply_placeable_gate`
/// fold names all four variants; zero `if let` escape hatches over
/// `FfdDisposition` anywhere in the pool plane). Source-scan census
/// over the EMBEDDED sources (the (wwwww) form).
#[test]
fn w10_ak_disposition_consumers_match_exhaustively() {
    let jobs_src = include_str!("../jobs.rs");
    let prod = jobs_src
        .split("#[cfg(test)]\nmod ")
        .next()
        .unwrap_or(jobs_src);
    for variant in [
        "FfdDisposition::PlacedRegistered",
        "FfdDisposition::PlacedInFlight",
        "FfdDisposition::Deferred",
        "FfdDisposition::Unplaced",
    ] {
        assert!(
            prod.contains(variant),
            "{variant} must be NAMED at the gate fold (a vanished arm \
             means a wildcard or if-let crept in — the R25 reject)"
        );
    }
    assert_eq!(
        prod.matches("if let FfdDisposition").count()
            + prod
                .matches("if let crate::reconcilers::nodeclaim_pool::FfdDisposition")
                .count(),
        0,
        "no if-let escape hatch between the FFD letter producer and \
         the demand law (R25)"
    );
}

// r[verify ctrl.pool.demand-completeness]
// r[verify ctrl.pool.ack-spawned-soundness]
/// **W10-AL (merged_bug_049, the re-ack half).** PROPOSITION at the
/// off-page quantifier: after a scheduler restart under >page-limit
/// backlog, EVERY pending Job re-acks — the lane derives from the
/// controller's own Job LIST (the local complete inventory), not the
/// page. Off-page Jobs reconstruct their echo from the durable cell
/// stamp (`rio.build/intent-cells`); on-page Jobs send the
/// full-fidelity page copy; pre-upgrade (unstamped) Jobs degrade to
/// the bare-id no-arm echo (the priced one-generation residual);
/// reaped names are excluded; freshly-spawned intents chain.
///
/// Pre-fix red (the page-filter form — off-page lane severed):
///   assertion failed: off-page pending Job re-acked from the LIST
///   (pre-fix: absent — dispatched_cells never re-armed, the
///   heartbeat-edge ICE clear dead after restart)
#[test]
fn w10_al_re_ack_derives_from_job_list_independent_of_paging() {
    use crate::reconcilers::pool::jobs::{INTENT_CELLS_ANNOTATION, assemble_re_acks};

    let pending_with = |name: &str, intent_id: &str, cells: Option<&str>| {
        let mut j = pending_job(name, 0, 30);
        let mut anns = BTreeMap::from([(INTENT_ID_ANNOTATION.to_string(), intent_id.to_string())]);
        if let Some(c) = cells {
            anns.insert(INTENT_CELLS_ANNOTATION.to_string(), c.to_string());
        }
        j.spec = Some(JobSpec {
            template: PodTemplateSpec {
                metadata: Some(ObjectMeta {
                    annotations: Some(anns),
                    ..Default::default()
                }),
                ..Default::default()
            },
            ..Default::default()
        });
        j
    };

    // Page holds ONLY "onpage" (the >2048 regime: everything else
    // rotated off). Jobs: on-page, off-page stamped, off-page
    // pre-upgrade, and one reaped-this-tick.
    let page = IntentPage::for_test(vec![intent_named("onpage")]);
    let jobs = vec![
        pending_with("rio-builder-p-onpage", "onpage", Some("m7i:spot")),
        pending_with(
            "rio-builder-p-offpage",
            "offpage",
            Some("m7i:spot,c8g:on-demand"),
        ),
        pending_with("rio-builder-p-legacy", "legacy", None),
        pending_with("rio-builder-p-reaped", "reapedid", Some("m7i:spot")),
    ];
    let reaped: HashSet<String> = ["rio-builder-p-reaped".to_string()].into();
    let spawned = vec![intent_named("fresh")];

    let acks = assemble_re_acks(&page, "p", ExecutorKind::Builder, &jobs, &reaped, spawned);
    let by_id: std::collections::HashMap<&str, &SpawnIntent> =
        acks.iter().map(|i| (i.intent_id.as_str(), i)).collect();

    assert!(
        by_id.contains_key("offpage"),
        "off-page pending Job re-acked from the LIST (pre-fix: absent \
         — dispatched_cells never re-armed, the heartbeat-edge ICE \
         clear dead after restart)"
    );
    let off = by_id["offpage"];
    assert_eq!(
        off.hw_class_names,
        vec!["m7i".to_string(), "c8g".to_string()],
        "reconstructed names from the durable stamp"
    );
    assert_eq!(
        off.node_affinity.len(),
        2,
        "paired minimal terms (names.len == terms.len by construction \
         — the scheduler's skew refusal is unreachable from this lane)"
    );
    assert_eq!(
        off.node_affinity[1].match_expressions[0].values,
        vec!["on-demand".to_string()],
        "capacity value rides the In requirement the arm decode reads"
    );
    assert!(by_id.contains_key("onpage"), "on-page re-ack (page copy)");
    let legacy = by_id["legacy"];
    assert!(
        legacy.hw_class_names.is_empty() && legacy.node_affinity.is_empty(),
        "pre-upgrade Job degrades to the bare-id no-arm echo (priced \
         one-generation residual)"
    );
    assert!(
        !by_id.contains_key("reapedid"),
        "names reaped this tick are excluded (ctrl.pool.ack-spawned-\
         soundness)"
    );
    assert!(by_id.contains_key("fresh"), "spawned intents chain");
    assert_eq!(acks.len(), 4, "exactly the four lawful acks");
}
