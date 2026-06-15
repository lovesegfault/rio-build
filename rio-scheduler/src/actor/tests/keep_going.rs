//! keepGoing fail-fast vs keep-going semantics, and DependencyFailed cascade.
// r[verify sched.build.keep-going]

use super::*;

/// 2-node DAG, hashA fails permanently. With `keep_going=false` the
/// build fails immediately; with `keep_going=true` it stays Active
/// until hashB completes, THEN fails.
#[rstest::rstest]
#[case::fails_fast(false, rio_proto::types::BuildState::Failed)]
#[case::waits_all(true, rio_proto::types::BuildState::Active)]
#[tokio::test]
async fn test_keepgoing_two_node_fail_one(
    #[case] keep_going: bool,
    #[case] expect_mid: rio_proto::types::BuildState,
) -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("kg-w1", "x86_64-linux").await?;
    let mut rx2 = connect_executor(&handle, "kg-w2", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![make_node("hashA"), make_node("hashB")],
        vec![],
        keep_going,
    )
    .await?;

    let a1 = recv_assignment(&mut rx).await;
    let _a2 = recv_assignment(&mut rx2).await;
    let (w_a, w_b) = if a1.drv_path.contains("hashA") {
        ("kg-w1", "kg-w2")
    } else {
        ("kg-w2", "kg-w1")
    };

    complete_failure(
        &handle,
        w_a,
        &test_drv_path("hashA"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;

    assert_eq!(
        query_status(&handle, build_id).await?.state,
        expect_mid as i32,
        "after hashA fails: keep_going={keep_going} → {expect_mid:?}"
    );

    if keep_going {
        // Complete hashB → build now fails (all resolved, one failed).
        complete_success_empty(&handle, w_b, &test_drv_path("hashB")).await?;
        assert_eq!(
            query_status(&handle, build_id).await?.state,
            rio_proto::types::BuildState::Failed as i32,
            "build should fail after all derivations resolve"
        );
    }
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
        setup_with_worker("cascade-worker", "x86_64-linux").await?;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_node("cascadeA"),
            make_node("cascadeB"),
            make_node("cascadeC"),
        ],
        vec![
            make_test_edge("cascadeA", "cascadeB"),
            make_test_edge("cascadeB", "cascadeC"),
        ],
        true, // keep_going
    )
    .await?;

    // Sanity: C is the only Ready/Assigned derivation; A and B are Queued.
    let info_a = expect_drv(&handle, "cascadeA").await;
    let info_b = expect_drv(&handle, "cascadeB").await;
    assert_eq!(info_a.status, DerivationStatus::Queued);
    assert_eq!(info_b.status, DerivationStatus::Queued);

    // Poison C via PermanentFailure.
    complete_failure(
        &handle,
        "cascade-worker",
        &test_drv_path("cascadeC"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "compile error",
    )
    .await?;

    // B and A should now be DependencyFailed (cascaded transitively).
    let info_b = expect_drv(&handle, "cascadeB").await;
    assert_eq!(
        info_b.status,
        DerivationStatus::DependencyFailed,
        "immediate parent B should be DependencyFailed after C poisoned"
    );
    let info_a = expect_drv(&handle, "cascadeA").await;
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
        setup_with_worker("poison-worker", "x86_64-linux").await?;

    // Build 1: single leaf, poisoned via PermanentFailure.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "preleaf", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "poison-worker",
        &test_drv_path("preleaf"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "preleaf failed",
    )
    .await?;

    // Verify preleaf is Poisoned.
    let leaf = expect_drv(&handle, "preleaf").await;
    assert_eq!(leaf.status, DerivationStatus::Poisoned);
    // I-169: Poisoned now resets on resubmit when resubmit_cycles < limit.
    // This test exercises the dep-STILL-poisoned fail-fast path, so pin
    // resubmit_cycles at the limit to keep preleaf Poisoned across the merge.
    assert!(
        handle
            .debug_force_poisoned("preleaf", crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );

    // Build 2: new node depending on the poisoned preleaf.
    // keepGoing=false: build should fail immediately at merge.
    let build2 = Uuid::new_v4();
    let _rx2 = merge_dag(
        &handle,
        build2,
        vec![make_node("preparent"), make_node("preleaf")],
        vec![make_test_edge("preparent", "preleaf")],
        false,
    )
    .await?;

    // preparent must be DependencyFailed (not stuck Queued).
    let parent = expect_drv(&handle, "preparent").await;
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

/// A merge that fail-fasts on a still-poisoned dependency must attribute
/// the failure to the poisoned CULPRIT — its drv path, persisted reason
/// and originating execution — not to the cascaded DependencyFailed
/// ancestor the reconcile pass surfaces first.
// r[verify sched.merge.failfast-culprit]
#[tokio::test]
async fn test_failfast_attributes_poisoned_culprit() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("culprit-worker", "x86_64-linux").await?;

    // Build 1: poison the leaf with a real builder error.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "culprit-leaf", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "culprit-worker",
        &test_drv_path("culprit-leaf"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "gcc: fatal error: boom",
    )
    .await?;
    // Pin resubmit_cycles at the limit so the leaf stays Poisoned across
    // the next merge (same shape as the prepoisoned-dep test above).
    assert!(
        handle
            .debug_force_poisoned("culprit-leaf", crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );

    // Build 2: a new parent depending on the poisoned leaf → fail-fast at
    // merge. The reconcile pass surfaces the PARENT (DependencyFailed);
    // attribution must descend to the leaf.
    let build2 = Uuid::new_v4();
    let mut rx2 = merge_dag(
        &handle,
        build2,
        vec![make_node("culprit-parent"), make_node("culprit-leaf")],
        vec![make_test_edge("culprit-parent", "culprit-leaf")],
        false,
    )
    .await?;

    let failed = loop {
        let ev = tokio::time::timeout(std::time::Duration::from_secs(5), rx2.recv())
            .await
            .expect("BuildFailed within 5s")?;
        if let Some(rio_proto::types::build_event::Event::Failed(f)) = ev.event {
            break f;
        }
    };
    let leaf_path = test_drv_path("culprit-leaf");
    assert_eq!(
        failed.culprit_derivation, leaf_path,
        "culprit must be the poisoned leaf, not the cascaded parent"
    );
    assert_eq!(
        failed.culprit_error_message, "gcc: fatal error: boom",
        "culprit_error_message must carry the persisted builder error"
    );
    assert!(
        !failed.culprit_exec_id.is_empty(),
        "culprit_exec_id must name the original execution"
    );
    assert!(
        failed.culprit_failed_at.is_some(),
        "culprit_failed_at must carry the poison timestamp"
    );
    // The headline summary names the culprit too (not just the new fields).
    assert!(
        failed.error_message.contains(&leaf_path) && failed.error_message.contains("boom"),
        "error_message must name the culprit and its reason, got: {}",
        failed.error_message
    );
    assert_eq!(failed.failed_derivation, leaf_path);
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
        setup_with_worker("resub-poison-w", "x86_64-linux").await?;

    // Build 1: poison the leaf.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "resub-poison", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "resub-poison-w",
        &test_drv_path("resub-poison"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;

    let pre = expect_drv(&handle, "resub-poison").await;
    assert_eq!(pre.status, DerivationStatus::Poisoned);
    // I-169: Poisoned now resets on resubmit when resubmit_cycles < limit.
    // This test exercises the at-limit fail-fast path; the under-limit
    // reset path is in actor::tests::merge.
    assert!(
        handle
            .debug_force_poisoned("resub-poison", crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );

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
