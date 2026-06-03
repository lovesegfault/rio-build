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
///
/// Also pins the cascade's event contract: each cascaded ancestor's
/// `DerivationFailed{DEPENDENCY_FAILED}` message is the shared
/// `rio_proto::dependency_failed_summary` shape naming the trigger (the
/// replay engine's `classify_reason` parses exactly that shape for
/// closure-membership and source-rot attribution), and a merge of
/// healthy nodes emits NO failed events (the negative control for the
/// merge-seeding emission).
// r[verify sched.event.derivation-terminal]
#[tokio::test]
async fn test_keepgoing_poisoned_dependency_cascades_failure() -> TestResult {
    // Worker with capacity 1: only the leaf gets dispatched initially.
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("cascade-worker", "x86_64-linux").await?;

    // Chain: A depends on B depends on C. C is the leaf.
    let build_id = Uuid::new_v4();
    let mut rx = merge_dag(
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

    // Negative control for merge-time seeding: every dep is healthy, so
    // the merge itself must emit zero DerivationFailed events.
    assert_eq!(
        drain_failed_derivation_events(&mut rx).len(),
        0,
        "a merge with no terminally-failed deps must not emit failed events"
    );

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

    // The cascaded ancestors' events carry the shared producer shape:
    // `dependency '<trigger>' failed: <reason>` — pinned via the
    // formatter (empty-reason call yields the prefix), never a
    // hand-written string, so the message the replay engine's
    // classifier parses cannot drift from this producer.
    let failed = drain_failed_derivation_events(&mut rx);
    let cascade_prefix = rio_proto::dependency_failed_summary(&test_drv_path("cascadeC"), "");
    for tag in ["cascadeA", "cascadeB"] {
        let path = test_drv_path(tag);
        let ev = failed
            .iter()
            .find(|d| d.derivation_path == path)
            .unwrap_or_else(|| panic!("no DerivationFailed event for cascaded ancestor {tag}"));
        assert_eq!(
            ev.failure_status(),
            rio_proto::types::BuildResultStatus::DependencyFailed,
            "{tag}: cascaded ancestors carry DEPENDENCY_FAILED"
        );
        assert!(
            ev.error_message.starts_with(&cascade_prefix),
            "{tag}: cascade message must be the shared dependency-failed summary \
             naming the trigger; got: {:?}",
            ev.error_message
        );
    }
    Ok(())
}

/// When a new build depends on an already-poisoned derivation (from a
/// prior build), compute_initial_states must mark the new node
/// DependencyFailed immediately. Without this check, it would go to
/// Queued and hang forever (never Ready, never cascaded since cascade
/// only runs on *transition to* Poisoned).
///
/// The seeded node must also EMIT its terminal: merge-time resolution
/// without a `DerivationFailed` event leaves the gateway's per-root
/// relay terminal-less — the root then inherits the DAG-level blanket
/// and the replay engine charges it a sibling's failure text instead of
/// the dependency attribution the cascade arm provides.
// r[verify sched.event.derivation-terminal]
// r[verify sched.merge.dep-failed-transitive+2]
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
    let mut rx2 = merge_dag(
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

    // The seeded node's own terminal event, in the shared producer
    // shape naming the poisoned dependency. Built via the formatter —
    // never a hand-written string — so the message the replay engine's
    // classifier parses cannot drift from this producer.
    let failed = drain_failed_derivation_events(&mut rx2);
    let parent_path = test_drv_path("preparent");
    let seeded: Vec<_> = failed
        .iter()
        .filter(|d| d.derivation_path == parent_path)
        .collect();
    assert_eq!(
        seeded.len(),
        1,
        "exactly one DerivationFailed for the merge-seeded node; got {failed:?}"
    );
    assert_eq!(
        seeded[0].failure_status(),
        rio_proto::types::BuildResultStatus::DependencyFailed,
    );
    assert_eq!(
        seeded[0].error_message,
        rio_proto::dependency_failed_summary(
            &test_drv_path("preleaf"),
            "already poisoned when this build merged",
        ),
        "the seeded terminal names the poisoned dependency via the shared summary"
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
///
/// The resubmitted node must also EMIT a terminal to the NEW build's
/// stream: its original failure events went to the earlier build only,
/// so without a merge-time emission the new build's relay records no
/// terminal for it (the pre-existing `Completed`→`DerivationCached` arm
/// already does this for the success siblings). `CACHED_FAILURE` is the
/// honest status — the failure is remembered from an earlier attempt,
/// not re-executed.
// r[verify sched.event.derivation-terminal]
#[tokio::test]
async fn test_resubmit_poisoned_node_itself_fails_fast() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("resub-poison-w", "x86_64-linux").await?;

    // Build 1: poison the leaf.
    let build1 = Uuid::new_v4();
    let mut rx1 =
        merge_single_node(&handle, build1, "resub-poison", PriorityClass::Scheduled).await?;
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
    // Build 1's own failure events are already buffered; drain so the
    // cross-stream assertion below starts clean.
    let _ = drain_failed_derivation_events(&mut rx1);

    // Build 2: same single node, no dependents. The poisoned node
    // is existing (not newly_inserted) — the merge-loop failure arm
    // must catch it.
    let build2 = Uuid::new_v4();
    let mut rx2 =
        merge_single_node(&handle, build2, "resub-poison", PriorityClass::Scheduled).await?;

    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "single-node resubmit of a Poisoned derivation must fail fast, not hang Active"
    );

    // Build 2's stream carries the node's own terminal: CACHED_FAILURE
    // (previously failed, result cached) — never silent.
    let failed = drain_failed_derivation_events(&mut rx2);
    assert_eq!(
        failed.len(),
        1,
        "the resubmitted still-poisoned node must emit exactly one terminal \
         to the new build; got {failed:?}"
    );
    assert_eq!(failed[0].derivation_path, test_drv_path("resub-poison"));
    assert_eq!(
        failed[0].failure_status(),
        rio_proto::types::BuildResultStatus::CachedFailure,
        "a pre-existing still-poisoned node is a remembered failure, not a new one"
    );
    assert!(
        !failed[0].error_message.is_empty(),
        "the cached-failure terminal must say why (poison + how it clears)"
    );

    // The emission targets the NEW build only — build 1 already saw the
    // original failure; a duplicate would double-report it there.
    assert_eq!(
        drain_failed_derivation_events(&mut rx1).len(),
        0,
        "the merge-time cached-failure terminal must not re-emit to prior builds"
    );
    Ok(())
}

/// Transitive merge seeding under keepGoing=true emits one terminal per
/// resolved node: a chain Z→Y→X with X pre-poisoned (at the resubmit
/// limit) seeds BOTH Y and Z DependencyFailed in the same merge (the
/// `will_fail` topological leg), and each carries its own
/// `DerivationFailed` event — Y naming the poisoned X, Z naming the
/// just-seeded Y — while pre-existing X reports `CACHED_FAILURE`. The
/// build settles Failed with every root carrying a terminal, which is
/// exactly the property the gateway's keep-going per-root verdicts rely
/// on for merge-resolved roots.
// r[verify sched.event.derivation-terminal]
// r[verify sched.merge.dep-failed-transitive+2]
#[tokio::test]
async fn test_transitive_merge_seed_emits_terminal_per_node() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("seed-chain-w", "x86_64-linux").await?;

    // Build 1: poison the leaf X, pin at the resubmit limit so the
    // merge below sees it still Poisoned (not reset-retried).
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "seedX", PriorityClass::Scheduled).await?;
    complete_failure(
        &handle,
        "seed-chain-w",
        &test_drv_path("seedX"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "seedX failed",
    )
    .await?;
    assert!(
        handle
            .debug_force_poisoned("seedX", crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );

    // Build 2 (keepGoing): chain Z → Y → X. Y and Z are newly inserted;
    // X is pre-existing Poisoned.
    let build2 = Uuid::new_v4();
    let mut rx2 = merge_dag(
        &handle,
        build2,
        vec![make_node("seedZ"), make_node("seedY"), make_node("seedX")],
        vec![
            make_test_edge("seedZ", "seedY"),
            make_test_edge("seedY", "seedX"),
        ],
        true,
    )
    .await?;

    // All three resolved at merge → keep-going build settles Failed.
    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keep-going build whose whole DAG resolved at merge must settle Failed"
    );

    let failed = drain_failed_derivation_events(&mut rx2);
    let by_path = |tag: &str| {
        let path = test_drv_path(tag);
        failed
            .iter()
            .filter(|d| d.derivation_path == path)
            .collect::<Vec<_>>()
    };

    // Y: seeded against the directly-poisoned X.
    let y = by_path("seedY");
    assert_eq!(y.len(), 1, "exactly one terminal for seeded Y: {failed:?}");
    assert_eq!(
        y[0].failure_status(),
        rio_proto::types::BuildResultStatus::DependencyFailed
    );
    assert_eq!(
        y[0].error_message,
        rio_proto::dependency_failed_summary(
            &test_drv_path("seedX"),
            "already poisoned when this build merged",
        )
    );

    // Z: seeded against Y, which failed within this same call (the
    // `will_fail` leg) — its terminal names Y, the node Z actually
    // depends on, keeping the trigger inside Z's recorded closure for
    // the replay engine's membership check.
    let z = by_path("seedZ");
    assert_eq!(z.len(), 1, "exactly one terminal for seeded Z: {failed:?}");
    assert_eq!(
        z[0].failure_status(),
        rio_proto::types::BuildResultStatus::DependencyFailed
    );
    assert_eq!(
        z[0].error_message,
        rio_proto::dependency_failed_summary(
            &test_drv_path("seedY"),
            "its own dependency already failed when this build merged",
        )
    );

    // X: pre-existing poisoned → remembered failure to the new build.
    let x = by_path("seedX");
    assert_eq!(
        x.len(),
        1,
        "exactly one terminal for pre-existing X: {failed:?}"
    );
    assert_eq!(
        x[0].failure_status(),
        rio_proto::types::BuildResultStatus::CachedFailure
    );
    Ok(())
}

/// A merge-seeded DependencyFailed node is skipped by any LATER runtime
/// cascade (the cascade only transitions Queued/Ready/Created), so its
/// terminal event is emitted exactly once — at seed time — never again
/// when an unrelated sibling's failure cascades over the same region.
///
/// DAG: D depends on both X (pre-poisoned at limit → D seeds
/// DependencyFailed at merge) and L (fresh → dispatched, then fails).
/// L's poison cascade reaches D but must not re-emit D's terminal.
// r[verify sched.event.derivation-terminal]
// r[verify sched.merge.dep-failed-transitive+2]
#[tokio::test]
async fn test_merge_seeded_node_not_reemitted_by_later_cascade() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("seed-once-w", "x86_64-linux").await?;

    // Build 1: poison X at the resubmit limit. Consume X's assignment
    // so the recv below sees build 2's dispatch, not this one.
    let build1 = Uuid::new_v4();
    let _rx1 = merge_single_node(&handle, build1, "onceX", PriorityClass::Scheduled).await?;
    let a1 = recv_assignment(&mut stream_rx).await;
    assert!(a1.drv_path.contains("onceX"), "build 1 dispatches X");
    complete_failure(
        &handle,
        "seed-once-w",
        &test_drv_path("onceX"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "onceX failed",
    )
    .await?;
    assert!(
        handle
            .debug_force_poisoned("onceX", crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );

    // Second worker for L: executors are one-shot (I-188 — a worker
    // marks itself draining after ANY completion), so the build-1
    // worker cannot take another assignment.
    let mut stream_rx2 = connect_executor(&handle, "seed-once-w2", "x86_64-linux").await?;

    // Build 2 (keepGoing): D → {X, L}. D seeds DependencyFailed at
    // merge (X already failed); L is independent and dispatches.
    let build2 = Uuid::new_v4();
    let mut rx2 = merge_dag(
        &handle,
        build2,
        vec![make_node("onceD"), make_node("onceX"), make_node("onceL")],
        vec![
            make_test_edge("onceD", "onceX"),
            make_test_edge("onceD", "onceL"),
        ],
        true,
    )
    .await?;
    assert_eq!(
        expect_drv(&handle, "onceD").await.status,
        DerivationStatus::DependencyFailed,
        "D must be seeded DependencyFailed at merge"
    );

    // L dispatches (the only Ready node), then fails permanently. Its
    // poison cascade walks D — already DependencyFailed — and must
    // skip it (transition filter), emitting nothing new for D.
    let assignment = recv_assignment(&mut stream_rx2).await;
    assert!(
        assignment.drv_path.contains("onceL"),
        "only L is dispatchable; got {}",
        assignment.drv_path
    );
    complete_failure(
        &handle,
        "seed-once-w2",
        &test_drv_path("onceL"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "onceL failed",
    )
    .await?;

    let status = query_status(&handle, build2).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "all resolved (D seeded, X cached-failure, L poisoned) → Failed"
    );

    let failed = drain_failed_derivation_events(&mut rx2);
    let d_events: Vec<_> = failed
        .iter()
        .filter(|d| d.derivation_path == test_drv_path("onceD"))
        .collect();
    assert_eq!(
        d_events.len(),
        1,
        "the seeded node's terminal is emitted exactly once (at seed time); \
         the later cascade over L must skip it: {failed:?}"
    );
    assert_eq!(
        d_events[0].error_message,
        rio_proto::dependency_failed_summary(
            &test_drv_path("onceX"),
            "already poisoned when this build merged",
        ),
        "the one terminal is the seed-time one (names X), not a cascade message"
    );
    Ok(())
}
