//! Completion handling: retry/poison thresholds, dep-chain release, duplicate idempotence.
// r[verify sched.completion.idempotent]
// r[verify sched.state.transitions]
// r[verify sched.state.terminal-idempotent]

use super::*;
use tracing_test::traced_test;

// r[verify sched.ca.cutoff-compare]
/// CA early-cutoff compare: on successful completion of an `is_ca`
/// derivation, `handle_success_completion` looks up each output's
/// nar_hash in the content index. All-match → `ca_output_unchanged =
/// true`. The metric is labeled `{outcome=match|miss}`.
///
/// Three scenarios in one test (shared MockStore + actor setup, which
/// is the expensive part):
///   1. Output hash matches a seeded content-index entry → `true`,
///      counter{match} += 1.
///   2. Output hash doesn't match anything → `false`, counter{miss}
///      += 1.
///   3. Non-CA derivation → hook skipped entirely, counter untouched,
///      flag stays `false` regardless of whether the hash would've
///      matched. Proves the `is_ca` guard.
///
/// AND-fold correctness (multi-output with [match, miss] → `false`)
/// is covered by sending two BuiltOutputs in scenario 2.
#[tokio::test]
async fn ca_completion_hash_compare_sets_unchanged_and_counts() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));
    let _db = test_db;

    // Seed a content-index entry: MockStore's content_lookup scans
    // stored paths for nar_hash == content_hash. The nar_hash here is
    // what the worker reports as `output_hash` on completion.
    let seeded_out = test_store_path("ca-seeded-out");
    let (_nar, seeded_hash) = store.seed_with_content(&seeded_out, b"reproducible");

    let _rx = connect_worker(&handle, "ca-worker", "x86_64-linux", 4).await?;

    let match_key = "rio_scheduler_ca_hash_compares_total{outcome=match}";
    let miss_key = "rio_scheduler_ca_hash_compares_total{outcome=miss}";

    // ─── Scenario 1: CA + matching hash → unchanged=true ───────────
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-match");
    let mut node = make_test_node("ca-match", "x86_64-linux");
    node.is_content_addressed = true;
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Precondition: merged as is_ca.
    let pre = handle
        .debug_query_derivation("ca-match")
        .await?
        .expect("exists");
    assert!(pre.is_ca, "precondition: merged with is_ca=true");
    assert!(!pre.ca_output_unchanged, "default false before completion");

    let match_before = recorder.get(match_key);
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "ca-worker".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: seeded_out.clone(),
                    // output_hash = seeded nar_hash → ContentLookup finds it.
                    output_hash: seeded_hash.to_vec(),
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    let info = handle
        .debug_query_derivation("ca-match")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        info.ca_output_unchanged,
        "CA + all-outputs-matched → ca_output_unchanged=true"
    );
    assert_eq!(
        recorder.get(match_key) - match_before,
        1,
        "one matched output → counter{{outcome=match}} +1.\nCounters: {:#?}",
        recorder.all_keys()
    );

    // ─── Scenario 2: CA + mixed [match, miss] → AND-fold → false ───
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ca-mixed");
    let mut node = make_test_node("ca-mixed", "x86_64-linux");
    node.is_content_addressed = true;
    node.output_names = vec!["out".into(), "dev".into()];
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let miss_before = recorder.get(miss_key);
    let match_before2 = recorder.get(match_key);
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "ca-worker".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![
                    // First output matches (same seeded hash).
                    rio_proto::types::BuiltOutput {
                        output_name: "out".into(),
                        output_path: seeded_out.clone(),
                        output_hash: seeded_hash.to_vec(),
                    },
                    // Second output: unknown hash → miss.
                    rio_proto::types::BuiltOutput {
                        output_name: "dev".into(),
                        output_path: test_store_path("ca-mixed-dev"),
                        output_hash: vec![0xabu8; 32],
                    },
                ],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    let info = handle
        .debug_query_derivation("ca-mixed")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Completed);
    assert!(
        !info.ca_output_unchanged,
        "AND-fold: [match, miss] → ca_output_unchanged=false (not last-iter-wins)"
    );
    assert_eq!(
        recorder.get(match_key) - match_before2,
        1,
        "mixed: one match recorded"
    );
    assert_eq!(
        recorder.get(miss_key) - miss_before,
        1,
        "mixed: one miss recorded"
    );

    // ─── Scenario 3: non-CA → hook skipped ─────────────────────────
    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("ia-skip");
    let node = make_test_node("ia-skip", "x86_64-linux"); // is_content_addressed=false
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let match_before3 = recorder.get(match_key);
    let miss_before3 = recorder.get(miss_key);
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "ca-worker".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                // Hash WOULD match if the hook ran — proves the
                // is_ca guard, not a coincidental miss.
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("ia-skip-out"),
                    output_hash: seeded_hash.to_vec(),
                }],
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;

    let info = handle
        .debug_query_derivation("ia-skip")
        .await?
        .expect("exists");
    assert!(!info.is_ca);
    assert!(
        !info.ca_output_unchanged,
        "non-CA → hook skipped, flag stays false"
    );
    assert_eq!(
        recorder.get(match_key),
        match_before3,
        "non-CA → no match increment"
    );
    assert_eq!(
        recorder.get(miss_key),
        miss_before3,
        "non-CA → no miss increment"
    );

    Ok(())
}

/// TransientFailure: retry on a different worker up to max_retries (default 2).
#[tokio::test]
async fn test_transient_retry_different_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register two workers
    let _rx1 = connect_worker(&handle, "worker-a", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "worker-b", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_retry = test_drv_path("retry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "retry-hash", PriorityClass::Scheduled).await?;

    // Get initial worker assignment
    let info1 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    let first_worker = info1.assigned_worker.clone().expect("assigned to a worker");
    assert_eq!(info1.retry_count, 0);

    // Send TransientFailure from the first worker
    complete_failure(
        &handle,
        &first_worker,
        &p_retry,
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "network hiccup",
    )
    .await?;

    // Should be retried: retry_count=1. backoff_until is set so
    // the derivation stays Ready (not immediately re-dispatched).
    // failed_workers contains first_worker so when backoff elapses,
    // retry goes to the OTHER worker.
    let info2 = handle
        .debug_query_derivation("retry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info2.retry_count, 1,
        "transient failure should increment retry_count"
    );
    // Status: Ready with backoff_until set. NOT Assigned — backoff
    // blocks immediate re-dispatch. If it IS Assigned, dispatch
    // raced us between complete_failure and query — still fine for
    // the test, the worker must be different (failed_workers
    // exclusion).
    match info2.status {
        DerivationStatus::Ready => {
            // Backoff active: assigned_worker cleared.
            assert!(
                info2.assigned_worker.is_none(),
                "Ready after failure → assigned_worker cleared"
            );
        }
        DerivationStatus::Assigned => {
            // Raced: dispatch happened. Worker MUST be different
            // (failed_workers exclusion).
            let retry_worker = info2.assigned_worker.expect("Assigned → worker set");
            assert_ne!(
                retry_worker, first_worker,
                "failed_workers exclusion: retry must go to a DIFFERENT worker"
            );
        }
        other => panic!("expected Ready or Assigned, got {other:?}"),
    }
    Ok(())
}

/// max_retries (default 2) exhausted → poison (the `retry_count >=
/// max_retries` branch, distinct from POISON_THRESHOLD 3-distinct-
/// workers).
///
/// Uses `debug_force_assign` between failures to bypass backoff_until
/// and failed_workers exclusion (which correctly prevent immediate
/// same-worker retry). The test drives the state machine directly to
/// test the completion handler's max_retries logic, not dispatch.
#[tokio::test]
async fn test_transient_failure_max_retries_poisons() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("flaky-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let p_maxretry = test_drv_path("maxretry-hash");
    let _event_rx =
        merge_single_node(&handle, build_id, "maxretry-hash", PriorityClass::Scheduled).await?;

    // Default RetryPolicy::max_retries = 2. Fail 3 times:
    // retry_count 0 -> 1 (retry), 1 -> 2 (retry), 2 >= 2 -> Poisoned.
    //
    // debug_force_assign before each failure (except the first —
    // initial dispatch happened) bypasses backoff so the completion
    // handler sees Assigned state and processes the failure.
    for attempt in 0..3 {
        if attempt > 0 {
            // Force back to Assigned: backoff would block real
            // dispatch, and failed_workers now excludes flaky-worker.
            let ok = handle
                .debug_force_assign("maxretry-hash", "flaky-worker")
                .await?;
            assert!(ok, "force-assign should succeed for Ready derivation");
        }
        complete_failure(
            &handle,
            "flaky-worker",
            &p_maxretry,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("attempt {attempt} failed"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation("maxretry-hash")
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 transient failures (retry_count >= max_retries=2) should poison"
    );
    Ok(())
}

/// Derivation poisoned after POISON_THRESHOLD (3) distinct worker failures.
#[tokio::test]
async fn test_poison_threshold_after_distinct_workers() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Register 4 workers so the derivation can be re-dispatched after each failure.
    let _rx1 = connect_worker(&handle, "poison-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "poison-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "poison-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "poison-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "poison-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send TransientFailure from 3 DISTINCT workers. After the 3rd, poison.
    //
    // debug_force_assign between failures: backoff_until prevents
    // immediate re-dispatch after each failure. The test drives
    // the assignment directly — we're testing the poison-threshold
    // logic (3 distinct failed_workers → poisoned), not dispatch
    // timing.
    for (i, worker) in ["poison-w1", "poison-w2", "poison-w3"].iter().enumerate() {
        if i > 0 {
            // After the first failure, dispatch won't re-assign
            // until backoff elapses. Force it. The worker is
            // distinct each time so failed_workers exclusion
            // wouldn't block (if backoff weren't in play).
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed");
        }
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "derivation should be Poisoned after {} distinct worker failures",
        PoisonConfig::default().threshold
    );

    // Build should be Failed.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should fail after derivation is poisoned"
    );
    Ok(())
}

// r[verify sched.retry.per-worker-budget]
/// InfrastructureFailure is a worker-local problem (FUSE EIO, cgroup
/// setup fail, OOM-kill of the build process) — NOT the build's fault.
/// 3× InfrastructureFailure on distinct workers → failed_workers stays
/// EMPTY, derivation NOT poisoned. Contrast with the TransientFailure
/// test above, where 3 distinct failures → poison.
#[tokio::test]
async fn test_infrastructure_failure_does_not_count_toward_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // 4 workers so re-dispatch always has a candidate.
    let _rx1 = connect_worker(&handle, "infra-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_worker(&handle, "infra-w2", "x86_64-linux", 1).await?;
    let _rx3 = connect_worker(&handle, "infra-w3", "x86_64-linux", 1).await?;
    let _rx4 = connect_worker(&handle, "infra-w4", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "infra-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× InfrastructureFailure from distinct workers. In the
    // TransientFailure case this would poison; here it must not.
    // handle_infrastructure_failure does reset_to_ready WITHOUT
    // failed_workers insert and WITHOUT backoff — so immediate
    // re-dispatch to whatever worker wins. We drive via
    // debug_force_assign to make the per-worker assertion
    // deterministic (avoid racing dispatch).
    for (i, worker) in ["infra-w1", "infra-w2", "infra-w3"].iter().enumerate() {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, worker).await?;
            assert!(ok, "force-assign should succeed (no backoff, no exclusion)");
        }
        complete_failure(
            &handle,
            worker,
            &drv_path,
            rio_proto::build_types::BuildResultStatus::InfrastructureFailure,
            &format!("infra failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    // Exit criterion: 3× InfrastructureFailure → failed_workers.is_empty()
    assert!(
        info.failed_workers.is_empty(),
        "InfrastructureFailure must NOT insert into failed_workers, got {:?}",
        info.failed_workers
    );
    assert_eq!(
        info.failure_count, 0,
        "InfrastructureFailure must NOT increment failure_count"
    );
    assert_eq!(
        info.retry_count, 0,
        "InfrastructureFailure must NOT increment retry_count (no retry penalty)"
    );
    // Exit criterion: NOT poisoned. Ready-or-Assigned (no backoff →
    // immediate re-dispatch may have won the race).
    assert!(
        matches!(
            info.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "3× InfrastructureFailure → NOT poisoned, got {:?}",
        info.status
    );

    // 4th attempt: now send TransientFailure. This DOES count — proving
    // the derivation is still live and the counting path still works.
    let ok = handle.debug_force_assign(drv_hash, "infra-w4").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "infra-w4",
        &drv_path,
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "now this one counts",
    )
    .await?;

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.failed_workers.len(),
        1,
        "1× TransientFailure after 3× InfrastructureFailure → exactly 1 failed worker"
    );
    assert_eq!(info.failure_count, 1);
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "1 failed worker < threshold(3), still not poisoned"
    );
    Ok(())
}

/// With `require_distinct_workers=false`, 3× TransientFailure on the
/// SAME worker → poisoned. Contrast with default distinct mode where
/// same-worker repeats don't count (HashSet insert is idempotent).
/// Primary use case: single-worker dev deployments, where 3 distinct
/// workers will never exist.
#[tokio::test]
async fn test_non_distinct_mode_counts_same_worker() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Configure: require_distinct_workers = false, threshold = 3.
    // Also max_retries = 5 so we hit the poison-threshold branch,
    // not the max_retries branch (default max_retries=2 would
    // poison at retry_count>=2, masking what we're testing).
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_poison_config(PoisonConfig {
            threshold: 3,
            require_distinct_workers: false,
        })
    });
    let _db = db;

    let _rx = connect_worker(&handle, "solo-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "nondistinct-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // 3× TransientFailure on the SAME worker. In default distinct
    // mode: failed_workers={solo-worker} stays at len()=1 < 3 → not
    // poisoned (blocked by max_retries instead). In non-distinct mode:
    // failure_count goes 1→2→3, and 3 >= threshold → poisoned.
    for i in 0..3 {
        if i > 0 {
            // Backoff + failed_workers exclusion block real dispatch
            // back to solo-worker. Force it.
            let ok = handle.debug_force_assign(drv_hash, "solo-worker").await?;
            assert!(ok, "force-assign should succeed");
        }
        complete_failure(
            &handle,
            "solo-worker",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("same-worker failure {i}"),
        )
        .await?;

        let info = handle
            .debug_query_derivation(drv_hash)
            .await?
            .expect("exists");
        // failed_workers (HashSet) stays at 1 — same worker every time.
        assert_eq!(
            info.failed_workers.len(),
            1,
            "HashSet: same worker inserted once, stays len()=1"
        );
        // failure_count increments regardless.
        assert_eq!(
            info.failure_count,
            i + 1,
            "non-distinct: failure_count increments unconditionally"
        );
    }

    // Exit criterion: 3× same-worker TransientFailure under
    // require_distinct_workers=false → POISONED.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "non-distinct mode: 3× same-worker failures → poisoned (failure_count={} >= threshold=3)",
        info.failure_count
    );
    Ok(())
}

/// Negative control for non-distinct mode: with DEFAULT config
/// (require_distinct_workers=true), 3× same-worker TransientFailure
/// does NOT poison via the threshold path — failed_workers.len()
/// stays at 1. (max_retries=2 default still poisons, but via a
/// different branch — this test isolates the distinct-workers logic
/// by raising max_retries.)
#[tokio::test]
async fn test_distinct_mode_same_worker_does_not_poison_via_threshold() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Default PoisonConfig (distinct=true, threshold=3) — but raise
    // max_retries so we can observe 3 same-worker failures WITHOUT
    // hitting max_retries. This is the control for the non-distinct
    // test above: same inputs, opposite config, opposite outcome.
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |mut a| {
        a.retry_policy.max_retries = 10;
        a
    });
    let _db = db;

    let _rx = connect_worker(&handle, "ctrl-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "ctrl-drv";
    let drv_path = test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        if i > 0 {
            let ok = handle.debug_force_assign(drv_hash, "ctrl-worker").await?;
            assert!(ok);
        }
        complete_failure(
            &handle,
            "ctrl-worker",
            &drv_path,
            rio_proto::build_types::BuildResultStatus::TransientFailure,
            &format!("ctrl failure {i}"),
        )
        .await?;
    }

    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
    assert_eq!(info.failed_workers.len(), 1, "HashSet: same worker = 1");
    assert_eq!(info.failure_count, 3, "flat count still increments");
    // NOT poisoned: distinct mode uses failed_workers.len()=1 < 3.
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "distinct mode (default): 3× same-worker → NOT poisoned via threshold \
         (failed_workers.len()=1 < 3); would need 3 DISTINCT workers"
    );
    Ok(())
}

/// Completing a child releases its parent to Ready in a dependency chain.
#[tokio::test]
async fn test_dependency_chain_releases_parent() -> TestResult {
    let (_db, handle, _task, mut stream_rx) =
        setup_with_worker("chain-worker", "x86_64-linux", 1).await?;

    // A depends on B. B is Ready (leaf), A is Queued.
    let build_id = Uuid::new_v4();
    let p_chain_a = test_drv_path("chainA");
    let p_chain_b = test_drv_path("chainB");
    let _rx = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("chainA", "x86_64-linux"),
            make_test_node("chainB", "x86_64-linux"),
        ],
        vec![make_test_edge("chainA", "chainB")],
        false,
    )
    .await?;

    // B is dispatched first (leaf). A is Queued waiting for B.
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert_eq!(info_a.status, DerivationStatus::Queued);

    // Worker receives B's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(assigned_path, p_chain_b);

    // Complete B.
    complete_success_empty(&handle, "chain-worker", &p_chain_b).await?;

    // A should now transition Queued -> Ready -> Assigned (dispatched).
    let info_a = handle
        .debug_query_derivation("chainA")
        .await?
        .expect("exists");
    assert!(
        matches!(
            info_a.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "A should be Ready or Assigned after B completes, got {:?}",
        info_a.status
    );

    // Worker should receive A's assignment.
    let assigned_path = recv_assignment(&mut stream_rx).await.drv_path;
    assert_eq!(
        assigned_path, p_chain_a,
        "A should be dispatched after B completes"
    );
    Ok(())
}

/// Duplicate ProcessCompletion is an idempotent no-op.
#[tokio::test]
async fn test_duplicate_completion_idempotent() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("idem-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "idem-hash";
    let drv_path = test_drv_path(drv_hash);
    let mut event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Send completion TWICE.
    for _ in 0..2 {
        complete_success_empty(&handle, "idem-worker", &drv_path).await?;
    }

    // completed_count should be 1, not 2.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.completed_derivations, 1,
        "duplicate completion should not double-count"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should still succeed (idempotent)"
    );

    // Count BuildCompleted events: should be exactly 1 (not 2).
    // Drain available events without blocking.
    let mut completed_events = 0;
    while let Ok(event) = event_rx.try_recv() {
        if matches!(
            event.event,
            Some(rio_proto::types::build_event::Event::Completed(_))
        ) {
            completed_events += 1;
        }
    }
    assert_eq!(
        completed_events, 1,
        "BuildCompleted event should fire exactly once"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Completion edge cases: unknown drv, wrong state, unknown status
// ---------------------------------------------------------------------------

/// ProcessCompletion for a drv_key the actor has never seen → warn
/// + ignore. Could happen after stale worker reconnect with a build
/// from a previous scheduler generation.
#[tokio::test]
#[traced_test]
async fn test_completion_unknown_drv_key_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Completion for a drv that was never merged.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "ghost-worker".into(),
            drv_key: "never-existed-drv-hash".into(),
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown derivation") || logs_contain("not in DAG"),
        "expected warn for unknown drv_key"
    );
    Ok(())
}

/// Completion for a drv in Ready (never dispatched) → warn + ignore.
/// A worker can't complete something it wasn't assigned.
#[tokio::test]
#[traced_test]
async fn test_completion_for_non_running_state_ignored() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge but DON'T connect a worker — drv stays Ready.
    let build_id = Uuid::new_v4();
    merge_single_node(&handle, build_id, "ready-drv", PriorityClass::Scheduled).await?;

    // Send completion. Drv is Ready, not Assigned/Running.
    complete_success_empty(&handle, "phantom-w", &test_drv_path("ready-drv")).await?;
    barrier(&handle).await;

    assert!(
        logs_contain("not in assigned") || logs_contain("unexpected state"),
        "expected warn for wrong-state completion"
    );

    // Drv still Ready (completion ignored).
    let info = handle
        .debug_query_derivation("ready-drv")
        .await?
        .expect("drv exists");
    assert!(
        !matches!(info.status, DerivationStatus::Completed),
        "completion from wrong state should be ignored, status={:?}",
        info.status
    );
    Ok(())
}

/// Unknown BuildResultStatus value (e.g. from a newer worker) → warn,
/// treat as transient failure. Don't panic, don't get stuck.
#[tokio::test]
#[traced_test]
async fn test_unknown_build_status_treated_as_transient() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("unk-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("unk-status");
    merge_single_node(&handle, build_id, "unk-status", PriorityClass::Scheduled).await?;

    // Wait for dispatch (drv → Assigned).
    barrier(&handle).await;

    // Send completion with an invalid status int (9999).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "unk-w".into(),
            drv_key: drv_path.clone(),
            result: rio_proto::build_types::BuildResult {
                status: 9999, // not a valid enum
                error_msg: "mystery".into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("unknown BuildResultStatus")
            || logs_contain("Unspecified")
            || logs_contain("unknown status"),
        "expected warn for unknown status enum"
    );
    Ok(())
}

/// After CancelBuild transitions a drv to Cancelled, a later
/// Cancelled completion from the worker is expected → no-op, debug
/// log. This is the "worker acknowledges the cancel signal" path.
#[tokio::test]
#[traced_test]
async fn test_cancelled_completion_after_cancel_is_noop() -> TestResult {
    let (_db, handle, _task, mut _rx) = setup_with_worker("cancel-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_path = test_drv_path("cancel-drv");
    merge_single_node(&handle, build_id, "cancel-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await; // dispatch

    // Cancel the build (transitions drv → Cancelled, sends CancelSignal).
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "user request".into(),
            reply: reply_tx,
        })
        .await?;
    let _cancelled = reply_rx.await??;

    // Worker reports Cancelled (acknowledging the signal).
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "cancel-w".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Cancelled.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // Drv still Cancelled (no spurious state change).
    let info = handle
        .debug_query_derivation("cancel-drv")
        .await?
        .expect("drv exists");
    assert_eq!(info.status, DerivationStatus::Cancelled);

    // The state check at handle_completion's top catches this —
    // drv is already Cancelled (not Assigned/Running), so it hits
    // the "not in assigned/running state, ignoring" early-return.
    // This IS the no-op path for acknowledging a cancel.
    assert!(
        logs_contain("not in assigned/running state"),
        "expected early-return for already-Cancelled state"
    );
    Ok(())
}

// r[verify sched.classify.penalty-overwrite]
/// Misclassification detection: a build routed to "small" (30s cutoff)
/// that completes in 100s (> 2× cutoff) triggers the penalty-overwrite
/// branch (completion.rs:303-324). The detection RECORDS the misclass
/// (metric + EMA overwrite) — next classify() for this pname sees 100s
/// and picks "large".
///
/// dispatch.rs:test_size_class_routing_respects_classification proves
/// `assigned_size_class` is recorded; this proves the detection USES it.
#[tokio::test]
#[traced_test]
async fn test_misclass_detection_on_slow_completion() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "large".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    // Small worker. No EMA pre-seed → classify() defaults to 30s →
    // routes to "small". Exactly the scenario: something LOOKED small,
    // but wasn't.
    let (tx, mut rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::WorkerConnected {
            worker_id: "w-small".into(),
            stream_tx: tx,
        })
        .await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            resources: None,
            bloom: None,
            size_class: Some("small".into()),
            worker_id: "w-small".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            max_builds: 4,
            running_builds: vec![],
        })
        .await?;

    // Merge with pname — the EMA-update block (completion.rs:247-249)
    // gates on pname.is_some(). Without it, misclass detection never fires.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("misc-drv", "x86_64-linux");
    node.pname = "slowthing".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Dispatch → assigned_size_class="small" set on the state.
    let _ = recv_assignment(&mut rx).await;

    // Complete with duration=100s (start=1000, stop=1100 unix seconds).
    // 100 > 2×30 = 60 → misclass branch fires.
    let drv_path = test_drv_path("misc-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "w-small".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: "/nix/store/zzz-slowthing-out".into(),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 1000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 1100,
                    nanos: 0,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            output_size_bytes: 0,
            peak_cpu_cores: 0.0,
        })
        .await?;
    barrier(&handle).await;

    // THE ASSERTION: the warn! in completion.rs:308-315 fired. If the
    // detection branch was dead code (missing assigned_size_class, missing
    // pname, threshold wrong), this would fail.
    assert!(
        logs_contain("misclassification"),
        "expected 'misclassification: actual > 2× cutoff' warn log; \
         detection branch at completion.rs:303-324 should have fired \
         (duration=100s > 2×30s=60s small-class cutoff)"
    );

    // build_history now has the penalty-overwritten EMA. The overwrite
    // uses actual duration directly (no blend), so ema_duration_secs ≈100.
    // ≥ 60 is enough to prove the write happened (update_build_history's
    // normal blend would give 0.3*100+0.7*30 = 51 for the FIRST sample…
    // but actually the first sample has no prior to blend with — let me
    // just check > 2×cutoff, which distinguishes penalty from no-penalty).
    let ema: f64 =
        sqlx::query_scalar("SELECT ema_duration_secs FROM build_history WHERE pname = 'slowthing'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        ema > 60.0,
        "penalty-overwrite should have written actual duration (≈100s), \
         not the blended EMA; got {ema}"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// build_samples write on completion (CutoffRebalancer feed)
// ---------------------------------------------------------------------------

/// Success completion with valid (pname, start_time, stop_time) writes
/// exactly one row to build_samples with the correct (pname, system,
/// duration_secs). Feeds P0229's CutoffRebalancer.
///
/// Gate conditions from completion.rs:283-296:
///   - state.pname.is_some()        ← node.pname set below
///   - result.start_time.is_some()  ← set below
///   - result.stop_time.is_some()   ← set below
///   - 0 < duration_secs < 30 days  ← 5.25s, trivially in-range
///
/// Without ALL of these, insert_build_sample is never reached — the
/// complete_success_empty() helper (no timestamps) silently skips.
#[tokio::test]
async fn test_completion_writes_build_sample() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("bs-worker", "x86_64-linux", 1).await?;

    // Merge with a distinct pname — make_test_node defaults to
    // "test-pkg", which is fine, but a unique pname makes the
    // SELECT below unambiguous if other tests ever share the pool.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("bs-drv", "x86_64-linux");
    node.pname = "sample-pkg".into();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Receive assignment — proves dispatch happened (state → Assigned).
    let _assignment = recv_assignment(&mut stream_rx).await;

    // Precondition: build_samples is empty. Test asserts its own
    // precondition so a stale row from a future test-helper change
    // would fail here, not in the COUNT=1 check below ("proves
    // nothing" self-invalidation guard).
    let pre: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        pre, 0,
        "precondition: build_samples empty before completion"
    );

    // Complete with start=2000.0s, stop=2005.25s → duration=5.25s.
    // peak_memory_bytes=8 MiB, non-zero so it's the interesting case
    // (0 is also valid for build_samples, but non-zero proves the
    // value round-trips).
    let drv_path = test_drv_path("bs-drv");
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            worker_id: "bs-worker".into(),
            drv_key: drv_path,
            result: rio_proto::build_types::BuildResult {
                status: rio_proto::build_types::BuildResultStatus::Built.into(),
                built_outputs: vec![rio_proto::types::BuiltOutput {
                    output_name: "out".into(),
                    output_path: test_store_path("sample-pkg-out"),
                    output_hash: vec![0u8; 32],
                }],
                start_time: Some(prost_types::Timestamp {
                    seconds: 2000,
                    nanos: 0,
                }),
                stop_time: Some(prost_types::Timestamp {
                    seconds: 2005,
                    nanos: 250_000_000,
                }),
                ..Default::default()
            },
            peak_memory_bytes: 8 * 1024 * 1024,
            output_size_bytes: 4096,
            peak_cpu_cores: 1.5,
        })
        .await?;
    barrier(&handle).await;

    // Exit criterion: exactly 1 row, correct (pname, system).
    let rows: Vec<(String, String, f64, i64)> =
        sqlx::query_as("SELECT pname, system, duration_secs, peak_memory_bytes FROM build_samples")
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        rows.len(),
        1,
        "exactly one build_samples row per successful completion"
    );
    let (pname, system, dur, mem) = &rows[0];
    assert_eq!(pname, "sample-pkg");
    assert_eq!(system, "x86_64-linux");
    // duration_secs = stop - start = 2005.25 - 2000.0 = 5.25.
    // f64 arithmetic on exactly-representable values (powers of 2):
    // 0.25 = 2^-2, 5.0 = 5, both exact. But tolerance anyway.
    assert!(
        (dur - 5.25).abs() < 1e-9,
        "duration_secs should be 5.25s (stop=2005.25 - start=2000.0), got {dur}"
    );
    assert_eq!(
        *mem,
        8 * 1024 * 1024,
        "peak_memory_bytes round-trips as i64"
    );

    Ok(())
}

/// Negative: completion WITHOUT timestamps (the complete_success_empty
/// path) writes nothing to build_samples. The sanity gate at
/// completion.rs:285 `let (Some(start), Some(stop)) = ...` rejects
/// Default::default() timestamps (None, None). Proves the gate is
/// live — if someone removes it, this test catches the regression
/// (spurious 0.0s samples would poison the rebalancer's percentiles).
#[tokio::test]
async fn test_completion_no_timestamps_no_sample() -> TestResult {
    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("nt-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nt-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    // complete_success_empty: BuildResult::default() → start_time=None,
    // stop_time=None. The EMA block and build_samples write both gate
    // on these being Some.
    complete_success_empty(&handle, "nt-worker", &test_drv_path("nt-drv")).await?;
    barrier(&handle).await;

    // Build succeeded (completion processed)…
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "completion should succeed even without timestamps"
    );

    // …but build_samples is empty (gate rejected None timestamps).
    let count: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM build_samples")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        count, 0,
        "no timestamps → no build_samples row (sanity gate at completion.rs)"
    );

    Ok(())
}

// ---------------------------------------------------------------------------
// path_tenants upsert on completion (per-tenant GC retention)
// ---------------------------------------------------------------------------

/// Two tenants submit the SAME derivation → dedup → 1 execution. On
/// completion, interested_builds = {both} → upsert inserts 2 rows
/// (same store_path_hash, distinct tenant_id). Re-call is idempotent
/// (ON CONFLICT DO NOTHING → rows_affected == 0).
// r[verify sched.gc.path-tenants-upsert]
#[tokio::test]
async fn test_completion_path_tenants_dedup_idempotent() -> TestResult {
    use sha2::Digest;

    let (db, handle, _task, mut stream_rx) =
        setup_with_worker("pt-worker", "x86_64-linux", 1).await?;

    // ── Seed 2 tenants. FK path_tenants→tenants ON DELETE CASCADE
    // means these rows MUST exist before the upsert. ─────────────────
    let tenant_a = rio_test_support::seed_tenant(&db.pool, "pt-tenant-a").await;
    let tenant_b = rio_test_support::seed_tenant(&db.pool, "pt-tenant-b").await;

    // ── Part A: actor flow — 2 builds share 1 derivation → dedup ────
    // Both builds submit the same node (same drv_hash "pt-drv"). The
    // actor dedups on drv_hash: one DerivationState, interested_builds
    // = {build_a, build_b}. One dispatch, one completion.
    let drv_tag = "pt-drv";
    let drv_path = test_drv_path(drv_tag);
    let out_path = test_store_path("pt-out");

    for (build_id, tenant) in [(Uuid::new_v4(), tenant_a), (Uuid::new_v4(), tenant_b)] {
        let (reply_tx, reply_rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                req: MergeDagRequest {
                    build_id,
                    tenant_id: Some(tenant),
                    priority_class: PriorityClass::Scheduled,
                    nodes: vec![make_test_node(drv_tag, "x86_64-linux")],
                    edges: vec![],
                    options: BuildOptions::default(),
                    keep_going: false,
                    traceparent: String::new(),
                },
                reply: reply_tx,
            })
            .await?;
        let _rx = reply_rx.await??;
    }

    // ONE assignment (dedup proof). Second merge saw existing node,
    // just added build_b to interested_builds.
    let assignment = recv_assignment(&mut stream_rx).await;
    assert_eq!(assignment.drv_path, drv_path, "dedup: one dispatch");

    // Complete with a real output_path. complete_success sends
    // built_outputs[0].output_path → completion.rs:260 stores it on
    // state.output_paths → :365 reads it → :406 upserts.
    complete_success(&handle, "pt-worker", &drv_path, &out_path).await?;
    barrier(&handle).await;

    // ── Assertion: 2 rows for out_path's hash (one per tenant) ──────
    let out_hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let rows: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    assert_eq!(
        rows.len(),
        2,
        "completion hook should upsert 1 row per interested tenant; \
         got {} rows for out_path hash",
        rows.len()
    );
    assert!(
        rows.contains(&tenant_a),
        "tenant_a should be in path_tenants"
    );
    assert!(
        rows.contains(&tenant_b),
        "tenant_b should be in path_tenants"
    );

    // ── Part B: direct upsert — 3 paths × 2 tenants = 6 rows ────────
    // Exit-criterion shape: cartesian product fully materialized,
    // idempotent on re-call. Fresh paths (no overlap with Part A).
    let sched_db = crate::db::SchedulerDb::new(db.pool.clone());
    let paths = vec![
        test_store_path("pt-p1"),
        test_store_path("pt-p2"),
        test_store_path("pt-p3"),
    ];
    let tenants = vec![tenant_a, tenant_b];

    let first = sched_db.upsert_path_tenants(&paths, &tenants).await?;
    assert_eq!(first, 6, "3 paths × 2 tenants = 6 rows inserted");

    let second = sched_db.upsert_path_tenants(&paths, &tenants).await?;
    assert_eq!(
        second, 0,
        "re-call with same inputs → 0 new rows (ON CONFLICT DO NOTHING)"
    );

    // Total in table: 2 (Part A) + 6 (Part B) = 8.
    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 8, "2 from actor flow + 6 from direct call");

    Ok(())
}
