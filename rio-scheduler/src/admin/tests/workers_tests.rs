//! `ListExecutors` RPC tests — the open-attempt-backed busy view.
//!
//! Split from `builds_tests.rs` to mirror the `admin/executors.rs`
//! submodule seam. The stream-era registration/draining/degraded
//! battery retired with the executors map; what the surface owes its
//! callers now is: one entry per open pull-mode attempt, `busy=true`,
//! `status="alive"`, the attempt's system/kind, and the same
//! `leader_for_secs` freshness input as before.

use super::*;
use crate::actor::tests::{merge_single_node, pull_attempt, pull_complete_success_empty};
use crate::state::PriorityClass;

// r[verify sched.admin.list-executors+2]
// r[verify sched.admin.list-executors-leader-age+3]
#[tokio::test]
async fn test_list_workers_open_attempt_backed() -> anyhow::Result<()> {
    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two independent single-node DAGs, both taken by pull-mode
    // attempts: the busy view lists exactly the two pulling pods.
    let _ev1 = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "lw-a",
        PriorityClass::Scheduled,
    )
    .await?;
    let _ev2 = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "lw-b",
        PriorityClass::Scheduled,
    )
    .await?;
    let _a = pull_attempt(&actor, "lw-a").await;
    let _b = pull_attempt(&actor, "lw-b").await;

    // No filter → both attempts listed as busy, alive executors.
    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 2, "one entry per open attempt");
    // A pull attempt's executor identity is its attested intent id
    // (the drv hash) — the entry is keyed by it.
    let entry_a = resp
        .executors
        .iter()
        .find(|e| e.executor_id == "lw-a")
        .expect("the lw-a attempt's executor is listed under its attempt identity");
    assert!(entry_a.busy, "an open attempt IS a busy executor");
    assert_eq!(entry_a.status, "alive");
    assert_eq!(
        entry_a.systems,
        vec!["x86_64-linux".to_string()],
        "the entry carries the pulled derivation's system"
    );
    assert_eq!(
        entry_a.kind,
        rio_proto::types::ExecutorKind::Builder as i32,
        "non-FOD attempt → builder kind"
    );
    assert!(entry_a.connected_since.is_some());
    assert!(entry_a.last_heartbeat.is_some());

    // "alive" filter → same set; "draining" (not producible on this
    // surface any more) → empty; unknown filter → lenient (all).
    let alive = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "alive".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(alive.executors.len(), 2);
    let draining = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "draining".into(),
        }))
        .await?
        .into_inner();
    assert!(
        draining.executors.is_empty(),
        "draining is not producible on the open-attempt view; an explicit filter for it \
         must return empty, not fall through to all"
    );
    let lenient = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "garbage".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(
        lenient.executors.len(),
        2,
        "unknown filter → all (lenient; operator typos shouldn't hide executors)"
    );

    // leader_for_secs keeps flowing (the controller's orphan-reap
    // fail-closed input). The test fixture's leadership is fresh, so
    // the exact value is small but the field must be present-and-sane
    // (not silently dropped by the re-implementation).
    assert!(resp.leader_for_secs < 3600, "leader_for_secs is populated");
    Ok(())
}

/// A closed attempt leaves the busy view: after the worker reports its
/// outcome the entry disappears (the successor of the stream-era
/// "drained/disconnected workers leave ListExecutors" behavior).
#[tokio::test]
async fn test_list_workers_drops_closed_attempts() -> anyhow::Result<()> {
    use crate::actor::tests::expect_drv;

    let (svc, actor, _task, _db) = setup_svc_default().await;
    let _ev = merge_single_node(
        &actor,
        uuid::Uuid::new_v4(),
        "lw-c",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = pull_attempt(&actor, "lw-c").await;

    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 1, "open attempt listed");

    pull_complete_success_empty(&actor, "lw-c").await?;
    crate::actor::tests::barrier(&actor).await;
    assert_eq!(
        expect_drv(&actor, "lw-c").await.status,
        crate::state::DerivationStatus::Completed,
        "precondition: the report closed the attempt"
    );

    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert!(
        resp.executors.is_empty(),
        "a reported (closed) attempt no longer appears in the busy view"
    );
    Ok(())
}
