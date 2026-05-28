use super::*;
use tracing_test::traced_test;

// -----------------------------------------------------------------------
// Worker-Scheduler wiring
// -----------------------------------------------------------------------

/// Bug reproduction: handle_completion uses drv_hash as the lookup key,
/// but grpc.rs passes drv_path. The completion should be resolved via
/// either key. This test sends ProcessCompletion with a drv_PATH and
/// expects the build to transition to Succeeded.
///
/// Expected to FAIL before fix: completion is dropped ("unknown derivation")
/// and the build stays Active forever.
#[tokio::test]
async fn test_completion_resolves_drv_path_to_hash() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge a single-node DAG
    let build_id = Uuid::new_v4();
    let drv_hash = "abc123hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // The pull report intake hands handle_completion the attempt row's
    // drv_PATH (same as the gRPC report path) — the path→hash
    // resolution must complete the build.
    pull_complete_success(&handle, drv_hash, &test_store_path("xyz-foo")).await?;

    // Query build status — should be Succeeded (single derivation, completed)
    let status = query_status(&handle, build_id).await?;

    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should succeed after completion sent with drv_path (got state={:?})",
        rio_proto::types::BuildState::try_from(status.state)
    );
    Ok(())
}

// -----------------------------------------------------------------------
// State machine integrity
// -----------------------------------------------------------------------

/// A completion with InfrastructureFailure status must reach
/// `handle_infrastructure_failure` (NOT the transient-failure path).
/// The gRPC layer synthesizes InfrastructureFailure for None results,
/// so this verifies the full wiring path.
///
/// Proof-of-processing is `failed_builders.is_empty()`: if the match-arm
/// split were wrong and routed to `handle_transient_failure`, it would
/// contain "test-worker". Semantics coverage (3× → not poisoned) is in
/// `tests/completion.rs::test_infrastructure_failure_does_not_count_*`.
#[tokio::test]
#[traced_test]
async fn test_completion_infrastructure_failure_handled() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-fail-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send completion with InfrastructureFailure (what gRPC layer sends
    // for None result)
    pull_complete_failure(
        &handle,
        drv_hash,
        rio_proto::types::BuildResultStatus::InfrastructureFailure,
        "worker sent CompletionReport with no result",
    )
    .await?;

    // The derivation goes reset_to_ready → Ready → re-dispatched (no
    // backoff for infra failures). Key wiring assertion: failed_builders
    // is EMPTY — proving the match arm routed to the infra handler,
    // not the transient handler (which would have inserted test-worker).
    let info = expect_drv(&handle, drv_hash).await;
    assert!(
        info.retry.failed_builders.is_empty(),
        "InfrastructureFailure must route to handle_infrastructure_failure \
         (NOT handle_transient_failure which inserts into failed_builders), got {:?}",
        info.retry.failed_builders
    );
    assert_eq!(
        info.retry.count, 0,
        "InfrastructureFailure carries no retry_count penalty (separate infra_retry_count)"
    );
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "expected Ready or Assigned after infra retry, got {:?}",
        info.status
    );
    // Proof the handler actually ran (not a silent-drop).
    assert!(logs_contain("infrastructure failure"));
    Ok(())
}

/// Malicious/buggy worker timestamps (i64::MIN start, i64::MAX stop) must
/// not panic the actor with integer overflow in the EMA duration computation.
#[tokio::test]
async fn test_completion_with_extreme_timestamps() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let drv_hash = "extreme-ts-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send completion with extreme timestamps that would overflow i64 subtraction.
    // Without saturating-sub: stop.seconds - start.seconds = i64::MAX - i64::MIN overflows (panic in debug).
    pull_report(
        &handle,
        drv_hash,
        pull_payload(rio_proto::types::BuildResult {
            status: rio_proto::types::BuildResultStatus::Built.into(),
            start_time: Some(prost_types::Timestamp {
                seconds: i64::MIN,
                nanos: i32::MIN,
            }),
            stop_time: Some(prost_types::Timestamp {
                seconds: i64::MAX,
                nanos: i32::MAX,
            }),
            built_outputs: vec![rio_proto::types::BuiltOutput {
                output_name: "out".into(),
                output_path: test_store_path("xyz-extreme"),
                output_hash: vec![0u8; 32],
            }],
            ..Default::default()
        }),
    )
    .await?;

    // Verify completion was processed (build succeeded). query_status is
    // the barrier — it queues after ProcessCompletion, so by the time it
    // replies the completion has been handled. If the actor panicked,
    // this errors (channel closed).
    let status = query_status(&handle, build_id).await?;
    assert!(handle.is_alive(), "actor must survive extreme timestamps");
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should succeed despite bogus timestamps"
    );
    Ok(())
}

// -----------------------------------------------------------------------
// Feature completions
// -----------------------------------------------------------------------

/// Interactive (IFD) builds should jump to the front of the ready queue.
#[tokio::test]
async fn test_interactive_builds_pushed_to_front() -> TestResult {
    // Worker with capacity for 1 build at a time
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("test-worker", "x86_64-linux").await?;

    // Merge a "scheduled" build first (should go to back of queue)
    let build_normal = Uuid::new_v4();
    let p_normal = test_drv_path("hash-normal");
    let _rx1 = merge_single_node(
        &handle,
        build_normal,
        "hash-normal",
        PriorityClass::Scheduled,
    )
    .await?;

    // The normal build gets assigned first (only one in queue)
    let first_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(first_path, p_normal);

    // Now merge a second "scheduled" build — goes to back
    let build_scheduled2 = Uuid::new_v4();
    let _rx2 = merge_single_node(
        &handle,
        build_scheduled2,
        "hash-scheduled2",
        PriorityClass::Scheduled,
    )
    .await?;

    // Merge an "interactive" build — should go to FRONT
    let build_ifd = Uuid::new_v4();
    let p_ifd = test_drv_path("hash-ifd");
    let _rx3 =
        merge_single_node(&handle, build_ifd, "hash-ifd", PriorityClass::Interactive).await?;

    // Complete the first build; one-shot worker drains, connect a fresh one.
    complete_success_empty(&handle, "test-worker", &p_normal).await?;
    let mut stream_rx = connect_executor(&handle, "test-worker-2", "x86_64-linux").await?;

    // The next assignment should be the IFD derivation (was pushed to front)
    let second_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(
        second_path, p_ifd,
        "interactive build should be dispatched before scheduled"
    );
    Ok(())
}
