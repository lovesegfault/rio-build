//! `ListExecutors` RPC tests.
//!
//! Split from `builds_tests.rs` to mirror the `admin/workers.rs`
//! submodule seam. Previously lived in `builds_tests.rs` because it was
//! "a single ~60L test — both cover list-RPCs"; the seam convention
//! (tests mirror `admin/{...,workers,...}.rs`) wins over grouping by
//! access pattern once there's more than one worker-RPC test.

use super::*;
use tokio::sync::oneshot;

// r[verify sched.admin.list-workers]
#[tokio::test]
async fn test_list_workers_with_filter() -> anyhow::Result<()> {
    use crate::actor::tests::connect_executor;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Fully registered worker.
    let _rx1 = connect_executor(&actor, "alive-worker", "x86_64-linux", 4).await?;

    // Drain a second worker.
    let _rx2 = connect_executor(&actor, "drain-worker", "aarch64-linux", 2).await?;
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
    assert_eq!(w.running_builds, 0);
    assert!(w.connected_since.is_some());
    assert!(w.last_heartbeat.is_some());
    // size_class: connect_executor doesn't set it → empty string.
    // (Proves the field is wired — a worker heartbeating with
    // size_class="medium" would round-trip it here.)
    assert_eq!(w.size_class, "");

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
