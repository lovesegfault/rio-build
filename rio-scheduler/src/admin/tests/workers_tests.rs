//! `ListExecutors` RPC tests.
//!
//! Split from `builds_tests.rs` to mirror the `admin/workers.rs`
//! submodule seam. Previously lived in `builds_tests.rs` because it was
//! "a single ~60L test — both cover list-RPCs"; the seam convention
//! (tests mirror `admin/{...,workers,...}.rs`) wins over grouping by
//! access pattern once there's more than one worker-RPC test.

use super::*;
use tokio::sync::oneshot;

// r[verify sched.admin.list-executors]
#[tokio::test]
async fn test_list_workers_with_filter() -> anyhow::Result<()> {
    use crate::actor::tests::connect_executor;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Fully registered worker.
    let _rx1 = connect_executor(&actor, "alive-worker", "x86_64-linux").await?;

    // Drain a second worker.
    let _rx2 = connect_executor(&actor, "drain-worker", "aarch64-linux").await?;
    let (tx, rx) = oneshot::channel();
    actor
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "drain-worker".into(),
            force: false,
            reply: tx,
        })
        .await?;
    let _ = rx.await?;

    // No filter → both.
    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 2);

    // filter=alive → only alive-worker.
    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "alive".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 1);
    let w = &resp.executors[0];
    assert_eq!(w.executor_id, "alive-worker");
    assert_eq!(w.status, "alive");
    assert_eq!(w.systems, vec!["x86_64-linux".to_string()]);
    assert!(!w.busy);
    assert!(w.connected_since.is_some());
    assert!(w.last_heartbeat.is_some());

    // filter=draining → only drain-worker.
    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "draining".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 1);
    assert_eq!(resp.executors[0].executor_id, "drain-worker");
    assert_eq!(resp.executors[0].status, "draining");

    Ok(())
}

/// Regression: `status_filter="degraded"` previously fell through to the
/// lenient `_ => true` arm (the match enumerated three of four producer
/// values) and returned ALL executors — broken precisely when the filter
/// is needed (store incident, I-056b). Now `KNOWN_STATUSES` couples
/// producer and consumer.
#[tokio::test]
async fn test_list_workers_degraded_filter() -> anyhow::Result<()> {
    use crate::actor::tests::{connect_executor, connect_executor_with};

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Two fully-registered workers; one reports a sick store.
    let _rx1 = connect_executor(&actor, "healthy", "x86_64-linux").await?;
    let _rx2 = connect_executor_with(&actor, "sick-store", "x86_64-linux", true, |hb| {
        hb.store_degraded = true;
    })
    .await?;

    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "degraded".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(
        resp.executors.len(),
        1,
        "degraded filter must match exactly, not fall through to all"
    );
    assert_eq!(resp.executors[0].executor_id, "sick-store");
    assert_eq!(resp.executors[0].status, "degraded");

    // Unknown filter still lenient (operator typos shouldn't hide executors).
    let resp = svc
        .list_executors(Request::new(ListExecutorsRequest {
            status_filter: "garbage".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.executors.len(), 2, "unknown filter → all (lenient)");

    Ok(())
}
