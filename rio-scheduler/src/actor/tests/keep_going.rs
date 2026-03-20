//! keepGoing fail-fast vs keep-going semantics, and DependencyFailed cascade.
// r[verify sched.build.keep-going]

use super::*;

/// keepGoing=false: on PermanentFailure, the entire build fails immediately.
#[tokio::test]
async fn test_keepgoing_false_fails_fast() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("test-worker", "x86_64-linux", 2).await?;

    // Merge a two-node DAG with keepGoing=false
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("hashA", "x86_64-linux"),
            make_test_node("hashB", "x86_64-linux"),
        ],
        vec![],
        false, // keep_going=false (critical)
    )
    .await?;

    // Send PermanentFailure for hashA
    complete_failure(
        &handle,
        "test-worker",
        &test_drv_path("hashA"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;

    // Build should be Failed (not waiting for hashB)
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail fast on PermanentFailure with keepGoing=false"
    );
    Ok(())
}

/// keepGoing=true: build waits for all derivations, fails only at the end.
#[tokio::test]
async fn test_keepgoing_true_waits_all() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("test-worker", "x86_64-linux", 2).await?;

    // Merge a two-node DAG with keepGoing=true
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("hashX", "x86_64-linux"),
            make_test_node("hashY", "x86_64-linux"),
        ],
        vec![],
        true, // keep_going=true (critical)
    )
    .await?;

    // Send PermanentFailure for hashX
    complete_failure(
        &handle,
        "test-worker",
        &test_drv_path("hashX"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "failed",
    )
    .await?;

    // Build should still be Active (waiting for hashY)
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active with keepGoing=true and pending derivations"
    );

    // Complete hashY successfully
    complete_success_empty(&handle, "test-worker", &test_drv_path("hashY")).await?;

    // Now build should be Failed (all resolved, one failed)
    let status2 = query_status(&handle, build_id).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after all derivations resolve with keepGoing=true"
    );
    Ok(())
}

/// keepGoing=true with a dependency chain: poisoning a leaf must cascade
/// DependencyFailed to all ancestors so the build terminates. Without the
/// cascade, parents stay Queued forever and completed+failed never reaches
/// total -> build hangs.
#[tokio::test]
async fn test_keepgoing_poisoned_dependency_cascades_failure() -> TestResult {
    // Worker with capacity 1: only the leaf gets dispatched initially.
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("cascade-worker", "x86_64-linux", 1).await?;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("cascadeA", "x86_64-linux"),
            make_test_node("cascadeB", "x86_64-linux"),
            make_test_node("cascadeC", "x86_64-linux"),
        ],
        vec![
            make_test_edge("cascadeA", "cascadeB"),
            make_test_edge("cascadeB", "cascadeC"),
        ],
        true, // keep_going
    )
    .await?;

    // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await?
        .expect("exists");
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);
    assert_eq!(info_b.status, DerivationStatus::Queued);

    // Poison C via PermanentFailure.
    complete_failure(
        &handle,
        "cascade-worker",
        &test_drv_path("cascadeC"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;

    // B and A should now be DependencyFailed (cascaded transitively).
    let info_b = handle
        .debug_query_derivation("cascadeB")
        .await?
        .expect("exists");
    assert_eq!(
        info_b.status,
        DerivationStatus::DependencyFailed,
        "immediate parent B should be DependencyFailed after C poisoned"
    );
    let info_a = handle
        .debug_query_derivation("cascadeA")
        .await?
        .expect("exists");
    assert_eq!(
        info_a.status,
        DerivationStatus::DependencyFailed,
        "transitive parent A should also be DependencyFailed"
    );

    // Build should terminate as Failed (all 3 derivations resolved:
    // 1 Poisoned + 2 DependencyFailed counted in failed).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keepGoing build with poisoned dependency chain should terminate as Failed, not hang"
    );
    assert_eq!(
        status.failed_derivations, 3,
        "1 Poisoned + 2 DependencyFailed should all count as failed"
    );
    Ok(())
}

/// When a new build depends on an already-poisoned derivation (from a
/// prior build), compute_initial_states must mark the new node
/// DependencyFailed immediately. Without this check, it would go to
/// Queued and hang forever (never Ready, never cascaded since cascade
/// only runs on *transition to* Poisoned).
#[tokio::test]
async fn test_merge_with_prepoisoned_dep_marks_dependency_failed() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("poison-worker", "x86_64-linux", 1).await?;

    // Build 1: single leaf, poisoned via PermanentFailure.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "preleaf", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "poison-worker",
        &test_drv_path("preleaf"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "preleaf failed",
    )
    .await?;

    // Verify preleaf is Poisoned.
    let leaf = handle
        .debug_query_derivation("preleaf")
        .await?
        .expect("exists");
    assert_eq!(leaf.status, DerivationStatus::Poisoned);

    // Build 2: new node depending on the poisoned preleaf.
    // keepGoing=false: build should fail immediately at merge.
    let build2 = Uuid::new_v4();
    let _rx2 = merge_dag(
        &handle,
        build2,
        vec![
            make_test_node("preparent", "x86_64-linux"),
            make_test_node("preleaf", "x86_64-linux"),
        ],
        vec![make_test_edge("preparent", "preleaf")],
        false,
    )
    .await?;

    // preparent must be DependencyFailed (not stuck Queued).
    let parent = handle
        .debug_query_derivation("preparent")
        .await?
        .expect("exists");
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "new node depending on pre-poisoned dep must be DependencyFailed, not stuck Queued"
    );

    // Build 2 must be Failed (!keepGoing + dep failure).
    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build depending on pre-poisoned dep must fail immediately (!keepGoing)"
    );
    Ok(())
}

/// Single-node resubmit of a still-Poisoned derivation (within TTL,
/// no ClearPoison) must fail the build immediately.
///
/// Unlike the _prepoisoned_dep_ test above, there is no new dependent
/// for compute_initial_states to mark DependencyFailed — the poisoned
/// node IS the entire DAG. Before the fix, the existing-node loop only
/// checked `== Completed`, so first_dep_failed stayed None and the build
/// sat Active with completed=0, failed=0, total=1.
#[tokio::test]
async fn test_resubmit_poisoned_node_itself_fails_fast() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("resub-poison-w", "x86_64-linux", 1).await?;

    // Build 1: poison the leaf.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "resub-poison", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "resub-poison-w",
        &test_drv_path("resub-poison"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;

    let pre = handle
        .debug_query_derivation("resub-poison")
        .await?
        .expect("exists");
    assert_eq!(pre.status, DerivationStatus::Poisoned);

    // Build 2: same single node, no dependents. The poisoned node
    // is existing (not newly_inserted) — the merge-loop failure arm
    // must catch it.
    let build2 = Uuid::new_v4();
    let _rx2 = merge_single_node(&handle, build2, "resub-poison", PriorityClass::Scheduled).await?;

    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "single-node resubmit of a Poisoned derivation must fail fast, not hang Active"
    );
    Ok(())
}
