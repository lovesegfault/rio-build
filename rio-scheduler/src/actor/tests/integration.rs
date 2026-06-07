use super::*;

use rio_store::grpc::StoreServiceImpl;
use rio_store::test_helpers::{put_path, spawn_store_service};

// -----------------------------------------------------------------------
// Scheduler-side cache check (TOCTOU fix)
// -----------------------------------------------------------------------

/// Spin up an in-process rio-store on an ephemeral port.
pub(super) async fn setup_inproc_store(
    pool: sqlx::PgPool,
) -> anyhow::Result<(StoreServiceClient<Channel>, tokio::task::JoinHandle<()>)> {
    // Inline storage in manifests.inline_blob (no chunk backend needed).
    spawn_store_service(StoreServiceImpl::new(pool)).await
}

/// Build a minimal single-file NAR and upload it to the store (trailer mode).
pub(super) async fn put_test_path(
    client: &mut StoreServiceClient<Channel>,
    store_path: &str,
) -> anyhow::Result<()> {
    let (nar, _hash) = rio_test_support::fixtures::make_nar(b"hello");
    let info = rio_test_support::fixtures::make_path_info_for_nar(store_path, &nar);
    put_path(client, info, nar).await?;
    Ok(())
}

#[tokio::test]
async fn test_scheduler_cache_check_skips_build() -> TestResult {
    let sched_db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&sched_db.pool).await;
    let store_db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&store_db.pool).await;

    // Start in-process store and pre-populate the expected output path.
    let (mut store_client, _store_server) = setup_inproc_store(store_db.pool.clone()).await?;
    let cached_output = test_store_path("cached-output");
    put_test_path(&mut store_client, &cached_output).await?;

    // Spawn actor WITH the store client — cache check will run.
    let (handle, _task) = setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone()));

    // Merge a single-node DAG with expected_output_paths pointing at the
    // pre-populated path. No worker needed — scheduler should find it
    // cached and complete immediately.
    let build_id = Uuid::new_v4();
    let mut node = make_node("cached-hash");
    node.expected_output_paths = vec![cached_output.to_string()];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Derivation should have gone Created → Completed (scheduler cache hit).
    let info = expect_drv(&handle, "cached-hash").await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "scheduler cache check should mark derivation as Completed"
    );
    assert_eq!(info.output_paths, vec![cached_output]);

    // Build should be Succeeded (all 1 derivation cached).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.cached_derivations, 1);
    assert_eq!(
        status.completed_derivations, 1,
        "completed should count cached exactly once (no double-counting)"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build with all-cached derivations should be Succeeded"
    );
    Ok(())
}

#[tokio::test]
async fn test_scheduler_cache_check_skipped_without_store() -> TestResult {
    // No store client — setup() uses setup_actor(pool, None).
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let mut node = make_node("uncached-hash");
    // expected_output_paths set but store client is None — should NOT short-circuit
    node.expected_output_paths = vec![test_store_path("uncached-out")];

    let _event_rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Without store client, derivation should proceed normally to dispatch.
    let info = expect_drv(&handle, "uncached-hash").await;
    assert!(
        matches!(
            info.status,
            DerivationStatus::Assigned | DerivationStatus::Ready
        ),
        "derivation should be dispatched normally without store client, got {:?}",
        info.status
    );
    Ok(())
}

// -----------------------------------------------------------------------
// DB fault injection
// -----------------------------------------------------------------------

/// A cyclic DAG submission must not leak into the actor's in-memory maps.
/// Regression test for the reorder fix: merge() now runs BEFORE the map
/// inserts, so a CycleDetected error leaves no trace in
/// build_events/builds.
#[tokio::test]
async fn test_cyclic_merge_does_not_leak_in_memory_state() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    // A depends on B, B depends on A — cycle.
    let nodes = vec![make_node("cycA"), make_node("cycB")];
    let edges = vec![
        make_test_edge("cycA", "cycB"),
        make_test_edge("cycB", "cycA"),
    ];

    let result = merge_dag(&handle, build_id, nodes, edges, false).await;
    assert!(
        result.is_err(),
        "cyclic DAG should be rejected with an error"
    );

    // The build must NOT be in the actor's maps (it was never inserted,
    // or it was rolled back). QueryBuildStatus should return NotFound.
    let status_result = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "build should not be in actor maps after cyclic merge failure; got {status_result:?}"
    );

    // The DAG should have no trace of the cyclic nodes.
    let drv_a = handle.debug_query_derivation("cycA").await?;
    assert!(
        drv_a.is_none(),
        "cycA should not exist in DAG after cycle rollback"
    );
    let drv_b = handle.debug_query_derivation("cycB").await?;
    assert!(
        drv_b.is_none(),
        "cycB should not exist in DAG after cycle rollback"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// Backpressure hysteresis
// -----------------------------------------------------------------------

/// Backpressure should activate at 80%, stay active, and only deactivate
/// at 60% (hysteresis). Before the fix, ActorHandle used a simple 80%
/// threshold with no hysteresis -> flapping under load near 80%.
#[tokio::test]
async fn test_backpressure_hysteresis() {
    let db = TestDb::new(&MIGRATOR).await;
    crate::actor::tests::seed_default_tenant(&db.pool).await;
    let scheduler_db = SchedulerDb::new(db.pool.clone());
    let mut actor = DagActor::new(
        scheduler_db,
        DagActorConfig::default(),
        DagActorPlumbing::default(),
    );
    let flag = actor.backpressure_flag();

    // Simulate queue at 50% — below high watermark, not active.
    actor.update_backpressure(5000, 10000);
    assert!(!flag.is_active(), "50%: should not be active");

    // 85% — above high watermark, activates.
    actor.update_backpressure(8500, 10000);
    assert!(flag.is_active(), "85%: should activate");

    // 70% — between watermarks, STILL active (hysteresis).
    actor.update_backpressure(7000, 10000);
    assert!(
        flag.is_active(),
        "70%: should STAY active (hysteresis between 60% and 80%)"
    );

    // 55% — below low watermark, deactivates.
    actor.update_backpressure(5500, 10000);
    assert!(
        !flag.is_active(),
        "55%: should deactivate (below 60% low watermark)"
    );

    // 70% again — below high watermark, STILL inactive (hysteresis).
    actor.update_backpressure(7000, 10000);
    assert!(
        !flag.is_active(),
        "70%: should STAY inactive (hysteresis between 60% and 80%)"
    );
}

/// ActorHandle::send() and ::is_backpressured() should honor the shared
/// hysteresis flag, not compute their own threshold.
#[tokio::test]
async fn test_handle_uses_shared_backpressure_flag() {
    let (_db, handle, _task) = setup().await;

    // Initially not backpressured (empty queue).
    assert!(!handle.is_backpressured());

    // Directly set the shared flag (simulating actor's hysteresis decision).
    handle.backpressure.set_for_test(true);
    assert!(
        handle.is_backpressured(),
        "handle should read the shared flag"
    );

    // send() should reject under backpressure.
    let (reply_tx, _) = oneshot::channel();
    let result = handle
        .send(ActorCommand::QueryBuildStatus {
            build_id: Uuid::new_v4(),
            caller_tenant: None,
            reply: reply_tx,
        })
        .await;
    assert!(
        matches!(result, Err(ActorError::Backpressure)),
        "send() should reject when shared flag is set"
    );

    // Clear flag; send() should succeed.
    handle.backpressure.set_for_test(false);
    assert!(!handle.is_backpressured());
}
