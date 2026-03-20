//! `ListBuilds`/`ListWorkers` RPC tests.
//!
//! Split from the 1732L monolithic `admin/tests.rs` (P0386) to mirror the
//! `admin/{builds,workers}.rs` submodule seams. `list_workers` test lives
//! here (not a separate `workers_tests.rs`) because it's a single ~60L
//! test — both cover list-RPCs, grouping by access pattern.

use super::*;
use rio_test_support::seed_tenant;
use tokio::sync::oneshot;

// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_filter_and_pagination() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());

    // Seed 3 builds directly via db helper (bypasses the actor).
    use crate::state::{BuildOptions, PriorityClass};
    for i in 0..3 {
        sched_db
            .insert_build(
                uuid::Uuid::new_v4(),
                None,
                PriorityClass::Scheduled,
                false,
                &BuildOptions::default(),
            )
            .await?;
        // Small sleep so submitted_at ordering is deterministic.
        if i < 2 {
            tokio::time::sleep(std::time::Duration::from_millis(5)).await;
        }
    }
    // Transition one to succeeded.
    let builds: Vec<(uuid::Uuid,)> =
        sqlx::query_as("SELECT build_id FROM builds ORDER BY submitted_at LIMIT 1")
            .fetch_all(&db.pool)
            .await?;
    sqlx::query("UPDATE builds SET status = 'succeeded', finished_at = now() WHERE build_id = $1")
        .bind(builds[0].0)
        .execute(&db.pool)
        .await?;

    // No filter → 3 builds, total_count=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 3);
    assert_eq!(resp.total_count, 3);

    // filter=pending → 2.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            status_filter: "pending".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2);
    assert_eq!(resp.total_count, 2);

    // Pagination: limit=1 offset=1 → 1 build (second-newest), total=3.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 1,
            offset: 1,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1);
    assert_eq!(resp.total_count, 3, "total_count unaffected by pagination");

    // Pagination: offset past end → empty page, total still correct.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            limit: 10,
            offset: 99,
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert!(
        resp.builds.is_empty(),
        "offset >= total_count → empty page, got {} builds",
        resp.builds.len()
    );
    assert_eq!(
        resp.total_count, 3,
        "total_count unaffected by offset-past-end"
    );

    // tenant_filter: seed a tenant + one build tagged with it.
    let tenant_id = seed_tenant(&db.pool, "filter-test").await;
    sched_db
        .insert_build(
            uuid::Uuid::new_v4(),
            Some(tenant_id),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "filter-test".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "only the tenant-tagged build");
    assert_eq!(resp.total_count, 1);

    // Unknown tenant → InvalidArgument.
    let err = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "nonexistent-tenant".into(),
            ..Default::default()
        }))
        .await
        .unwrap_err();
    assert_eq!(err.code(), tonic::Code::InvalidArgument);

    Ok(())
}

/// Cross-tenant isolation: filtering by tenant A must NOT return tenant B's
/// builds. The test above (`test_list_builds_filter_and_pagination`) proves
/// "tenant filter excludes NULL-tenant builds" (1 tagged vs 3 untagged);
/// this test proves "tenant A filter excludes tenant B builds" — the actual
/// multi-tenant safety property.
// r[verify sched.admin.list-builds]
#[tokio::test]
async fn test_list_builds_cross_tenant_isolation() -> anyhow::Result<()> {
    let (svc, _actor, _task, db) = setup_svc_default().await;
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    use crate::state::{BuildOptions, PriorityClass};

    // Seed two tenants.
    let tenant_a = seed_tenant(&db.pool, "tenant-a").await;
    let tenant_b = seed_tenant(&db.pool, "tenant-b").await;

    // Seed one build per tenant. Capture build_id so we can assert
    // WHICH build appears (not just count — a buggy filter that
    // always returns the first row would pass a count-only check).
    let build_a = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_a,
            Some(tenant_a),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;
    let build_b = uuid::Uuid::new_v4();
    sched_db
        .insert_build(
            build_b,
            Some(tenant_b),
            PriorityClass::Scheduled,
            false,
            &BuildOptions::default(),
        )
        .await?;

    // Filter by tenant A → exactly build_a, NOT build_b.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-a".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-a filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_a.to_string(),
        "tenant-a filter must return build_a, not build_b"
    );
    assert_eq!(resp.total_count, 1);

    // Filter by tenant B → exactly build_b, NOT build_a.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest {
            tenant_filter: "tenant-b".into(),
            ..Default::default()
        }))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 1, "tenant-b filter → exactly one build");
    assert_eq!(
        resp.builds[0].build_id,
        build_b.to_string(),
        "tenant-b filter must return build_b, not build_a"
    );
    assert_eq!(resp.total_count, 1);

    // No filter → both builds visible.
    let resp = svc
        .list_builds(Request::new(ListBuildsRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.builds.len(), 2, "no filter → both tenants' builds");
    assert_eq!(resp.total_count, 2);
    let ids: std::collections::HashSet<_> =
        resp.builds.iter().map(|b| b.build_id.as_str()).collect();
    assert!(ids.contains(build_a.to_string().as_str()));
    assert!(ids.contains(build_b.to_string().as_str()));

    Ok(())
}

// r[verify sched.admin.list-workers]
#[tokio::test]
async fn test_list_workers_with_filter() -> anyhow::Result<()> {
    use crate::actor::tests::connect_worker;

    let (svc, actor, _task, _db) = setup_svc_default().await;

    // Fully registered worker.
    let _rx1 = connect_worker(&actor, "alive-worker", "x86_64-linux", 4).await?;

    // Drain a second worker.
    let _rx2 = connect_worker(&actor, "drain-worker", "aarch64-linux", 2).await?;
    let (tx, rx) = oneshot::channel();
    actor
        .send_unchecked(ActorCommand::DrainWorker {
            worker_id: "drain-worker".into(),
            force: false,
            reply: tx,
        })
        .await?;
    let _ = rx.await?;

    // No filter → both.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest::default()))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 2);

    // filter=alive → only alive-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "alive".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    let w = &resp.workers[0];
    assert_eq!(w.worker_id, "alive-worker");
    assert_eq!(w.status, "alive");
    assert_eq!(w.systems, vec!["x86_64-linux".to_string()]);
    assert_eq!(w.max_builds, 4);
    assert_eq!(w.running_builds, 0);
    assert!(w.connected_since.is_some());
    assert!(w.last_heartbeat.is_some());
    // size_class: connect_worker doesn't set it → empty string.
    // (Proves the field is wired — a worker heartbeating with
    // size_class="medium" would round-trip it here.)
    assert_eq!(w.size_class, "");

    // filter=draining → only drain-worker.
    let resp = svc
        .list_workers(Request::new(ListWorkersRequest {
            status_filter: "draining".into(),
        }))
        .await?
        .into_inner();
    assert_eq!(resp.workers.len(), 1);
    assert_eq!(resp.workers[0].worker_id, "drain-worker");
    assert_eq!(resp.workers[0].status, "draining");

    Ok(())
}
