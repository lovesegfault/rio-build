//! Merge error paths: DB-failure rollback, cache-check store errors, circuit breaker.
// r[verify sched.merge.toctou-serial]

use super::*;

// ===========================================================================
// Shared-node priority bump on higher-priority merge
// ===========================================================================

/// When a higher-priority (Interactive) build merges a DAG node already
/// present from a lower-priority (Scheduled) build, the shared node's
/// effective priority bumps to max(old, new). Dispatch order observes
/// the bump: the shared node jumps ahead of Scheduled-only siblings.
///
/// Mechanism: merge adds the new build_id to the node's
/// `interested_builds`. The merge's trailing `dispatch_ready()` pops the
/// queue, finds no worker, defers, and re-pushes via `push_ready` —
/// which recomputes `queue_priority` and now sees an Interactive
/// interested build → adds `INTERACTIVE_BOOST`. So when a worker
/// connects, the shared node is at the top of the heap.
///
// r[verify sched.merge.shared-priority-max]
#[tokio::test]
async fn test_shared_node_priority_bumps_on_higher_pri_merge() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Build 1: Scheduled, two independent leaves. No worker connected yet,
    // so both push into the ready queue at Scheduled priority and stay.
    let build_lo = Uuid::new_v4();
    merge_dag(
        &handle,
        build_lo,
        vec![
            make_test_node("shared-x", "x86_64-linux"),
            make_test_node("filler-y", "x86_64-linux"),
        ],
        vec![],
        false,
    )
    .await?;

    // Build 2: Interactive, ONLY the shared node. Merge dedup keys on
    // drv_hash (= tag), so "shared-x" maps to the SAME DAG node. Merge
    // adds build_hi to its interested_builds; dispatch_ready re-pushes
    // it with INTERACTIVE_BOOST. "filler-y" is NOT in this build, so it
    // stays at Scheduled priority.
    let build_hi = Uuid::new_v4();
    merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_hi,
            tenant_id: None,
            priority_class: PriorityClass::Interactive,
            nodes: vec![make_test_node("shared-x", "x86_64-linux")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
        },
    )
    .await?;

    // Connect a 1-slot worker. Heartbeat/PrefetchComplete triggers
    // dispatch_ready, which pops the highest-priority node.
    let mut rx = connect_worker(&handle, "prio-w", "x86_64-linux", 1).await?;

    // First assignment MUST be shared-x: it carries INTERACTIVE_BOOST
    // (via build_hi's interest), filler-y does not. Without the bump,
    // both would tie at Scheduled base priority and pop order would be
    // nondeterministic (HashMap iteration in compute_initial_states).
    // The +1e9 boost makes it deterministic — it dominates any
    // critical-path base difference (a 100k-node chain at 1h each is
    // 3.6e8; 1e9 still wins).
    let first = recv_assignment(&mut rx).await;
    assert_eq!(
        first.drv_path,
        test_drv_path("shared-x"),
        "shared node with Interactive interest should dispatch before \
         Scheduled-only filler — priority bump to max(interested builds)"
    );

    Ok(())
}

// ===========================================================================
// actor/merge.rs cleanup + cache-check error paths
// ===========================================================================

/// When DB persistence fails mid-merge, cleanup_failed_merge rolls back
/// all in-memory state. The build_id should be unknown afterward.
#[tokio::test]
async fn test_merge_db_failure_rolls_back_memory() -> TestResult {
    let (db, handle, _task) = setup().await;

    // Close pool BEFORE merge so insert_build fails immediately.
    db.pool.close().await;

    let build_id = Uuid::new_v4();
    let reply = merge_single_node(&handle, build_id, "rollback", PriorityClass::Scheduled).await;

    // Merge should have failed.
    assert!(
        matches!(
            reply.as_ref().err().and_then(|e| e.downcast_ref()),
            Some(ActorError::Database(_))
        ),
        "expected Database error, got {reply:?}"
    );

    // And the build should NOT exist in memory (rollback worked).
    let status_result = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "rolled-back build should be NotFound, got {status_result:?}"
    );
    Ok(())
}

/// check_cached_outputs store error is non-fatal: merge proceeds with
/// empty-set result (everything assumed uncached).
#[tokio::test]
async fn test_check_cached_outputs_store_error_non_fatal() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    store.fail_find_missing.store(true, Ordering::SeqCst);
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Merge with expected_output_paths set so check_cached_outputs runs.
    let build_id = Uuid::new_v4();
    let mut node = make_test_node("cache-err", "x86_64-linux");
    node.expected_output_paths = vec![test_store_path("expected-out")];

    // Merge should SUCCEED despite the store error.
    let reply = merge_dag(&handle, build_id, vec![node], vec![], false).await;
    assert!(reply.is_ok(), "store error should be non-fatal: {reply:?}");

    // Build should exist and be Active.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Active as i32);
    Ok(())
}

/// Circuit breaker: after 5 consecutive cache-check failures, the 6th
/// SubmitBuild is REJECTED with StoreUnavailable. This is the difference
/// from the test above — one failure is non-fatal, five trips the breaker.
///
/// Then: store recovers → 7th merge succeeds (half-open probe closes).
/// This proves both the open transition AND the close-on-recovery path.
#[tokio::test]
async fn test_cache_check_circuit_breaker_opens_then_closes() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    store.fail_find_missing.store(true, Ordering::SeqCst);
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Helper: merge a single-node DAG. Each call MUST use a unique tag —
    // make_test_node derives drv_hash from tag, and merging the SAME node
    // twice gives empty newly_inserted on the second merge → cache check
    // skipped (check_paths empty) → no probe → no failure recorded. The
    // test would silently pass for the wrong reason.
    //
    // expected_output_paths must also be non-empty or the cache check skips
    // the store call entirely.
    let mut seq = 0u32;
    let mut do_merge = |label: &str| {
        seq += 1;
        // Unique tag per call — different drv_hash → always newly_inserted.
        let tag = format!("{label}-{seq}");
        let mut node = make_test_node(&tag, "x86_64-linux");
        node.expected_output_paths = vec![test_store_path("expected-out")];
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id: Uuid::new_v4(),
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
            },
            reply: reply_tx,
        };
        (cmd, reply_rx)
    };

    // Merges 1-4: under threshold (OPEN_THRESHOLD = 5). Each fails the
    // cache check but proceeds with empty cache-hit set. Wasteful but
    // tolerable for a handful of submissions.
    for i in 1..=4 {
        let (cmd, rx) = do_merge("under");
        handle.send_unchecked(cmd).await?;
        let reply = rx.await?;
        assert!(
            reply.is_ok(),
            "merge #{i} should succeed (breaker still closed): {reply:?}"
        );
    }

    // Merge 5: this trips the breaker. consecutive_failures hits 5 = threshold.
    // record_failure() returns true → Err(StoreUnavailable) → merge rolled back.
    let (cmd, rx) = do_merge("trip");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        matches!(reply, Err(ActorError::StoreUnavailable)),
        "merge #5 should trip breaker open, got: {reply:?}"
    );

    // Merge 6: breaker still open. The probe fails (store still broken) →
    // stays open → rejected. Proves the breaker doesn't spuriously close.
    let (cmd, rx) = do_merge("still-open");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        matches!(reply, Err(ActorError::StoreUnavailable)),
        "merge #6 should still be rejected (breaker stays open): {reply:?}"
    );

    // === Store recovers ===
    store.fail_find_missing.store(false, Ordering::SeqCst);

    // Merge 7: half-open probe succeeds. record_success() closes the breaker.
    // The merge proceeds normally (empty cache-hit set because nothing's
    // seeded in MockStore, but that's fine — the point is it's ACCEPTED).
    let (cmd, rx) = do_merge("recovered");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(
        reply.is_ok(),
        "merge #7 should succeed after store recovery (probe closes breaker): {reply:?}"
    );

    Ok(())
}

/// When check_cached_outputs fails with StoreUnavailable, the build
/// row must be cleanly deleted — no orphan left in PG.
///
/// If check_cached_outputs ran AFTER persist_merge_to_db +
/// transition_build(Active), cleanup_failed_merge's delete_build
/// would FK-fail silently because build_derivations rows existed.
/// On failover, recovery would resurrect the orphan build and run
/// it — client got StoreUnavailable but the build silently
/// executed later.
///
/// check_cached_outputs runs BEFORE persist so the rollback is
/// in-memory only. Migration 008 also adds CASCADE as
/// defense-in-depth.
#[tokio::test]
async fn test_merge_rollback_on_store_unavailable_no_orphan() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    store.fail_find_missing.store(true, Ordering::SeqCst);
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let mut seq = 0u32;
    let mut do_merge = |label: &str| {
        seq += 1;
        let tag = format!("{label}-{seq}");
        let mut node = make_test_node(&tag, "x86_64-linux");
        node.expected_output_paths = vec![test_store_path("expected-out")];
        let build_id = Uuid::new_v4();
        let (reply_tx, reply_rx) = oneshot::channel();
        let cmd = ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class: PriorityClass::Scheduled,
                nodes: vec![node],
                edges: vec![],
                options: BuildOptions::default(),
                keep_going: false,
                traceparent: String::new(),
                jti: None,
            },
            reply: reply_tx,
        };
        (build_id, cmd, reply_rx)
    };

    // Trip the breaker: 4 under-threshold merges, 5th trips.
    for _ in 1..=4 {
        let (_id, cmd, rx) = do_merge("under");
        handle.send_unchecked(cmd).await?;
        assert!(rx.await?.is_ok());
    }
    let (trip_id, cmd, rx) = do_merge("trip");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(matches!(reply, Err(ActorError::StoreUnavailable)));

    // One more rejected merge for good measure (breaker stays open).
    let (reject_id, cmd, rx) = do_merge("still-open");
    handle.send_unchecked(cmd).await?;
    let reply = rx.await?;
    assert!(matches!(reply, Err(ActorError::StoreUnavailable)));

    // === The actual assertion: NO orphan build rows in PG ===
    // cleanup_failed_merge succeeds because check_cached_outputs
    // runs before persist (rollback is in-memory only).
    let tripped_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM builds WHERE build_id = $1)")
            .bind(trip_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert!(
        !tripped_exists,
        "tripped build_id {trip_id} should NOT have an orphan row in PG"
    );

    let reject_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM builds WHERE build_id = $1)")
            .bind(reject_id)
            .fetch_one(&test_db.pool)
            .await?;
    assert!(
        !reject_exists,
        "rejected build_id {reject_id} should NOT have an orphan row in PG"
    );

    // Also verify no orphan build_derivations for either.
    let orphan_bd: i64 = sqlx::query_scalar(
        "SELECT count(*)::bigint FROM build_derivations WHERE build_id = ANY($1)",
    )
    .bind(&[trip_id, reject_id][..])
    .fetch_one(&test_db.pool)
    .await?;
    assert_eq!(
        orphan_bd, 0,
        "no build_derivations rows should exist for rolled-back builds"
    );

    Ok(())
}

/// GAP-3+4 fix: floating-CA derivations cache-hit at merge time via
/// the `realisations` table, NOT via FindMissingPaths (which would see
/// `expected_output_paths = [""]` and always report missing).
///
/// Seeds a realisation row for `(modular_hash, "out")`, then merges a
/// CA node with that modular_hash. The node should transition straight
/// to Completed with `output_paths` set to the REALIZED path from the
/// realisations table — not the `[""]` placeholder (GAP-4).
#[tokio::test]
async fn test_ca_cache_hit_via_realisations() -> TestResult {
    let test_db = TestDb::new(&MIGRATOR).await;
    // No store client — the CA path uses the realisations table
    // directly, not FindMissingPaths. This also proves the CA check
    // doesn't spuriously depend on store availability.
    let (handle, _task) = setup_actor(test_db.pool.clone());

    let modular_hash = [0x42u8; 32];
    let realized_path = test_store_path("ca-realized-out");

    // Seed the realisations table (as if a prior build had registered it).
    crate::ca::insert_realisation(
        &test_db.pool,
        &modular_hash,
        "out",
        &realized_path,
        &[0x11u8; 32],
    )
    .await?;

    // Merge a floating-CA node with the seeded modular_hash.
    let mut node = make_test_node("ca-cache-hit", "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    node.expected_output_paths = vec![String::new()]; // floating-CA placeholder
    let build_id = Uuid::new_v4();
    let mut ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Build should be Succeeded (single node cache-hit → whole DAG done).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "single-node CA cache-hit should complete the build immediately"
    );

    // GAP-4: the emitted DerivationCached event must carry the REALIZED
    // path, not the [""] placeholder from expected_output_paths.
    let cached_paths = loop {
        let e = ev.recv().await?;
        if let Some(rio_proto::types::build_event::Event::Derivation(d)) = e.event
            && let Some(rio_proto::dag::derivation_event::Status::Cached(c)) = d.status
        {
            break c.output_paths;
        }
    };
    assert_eq!(
        cached_paths,
        vec![realized_path],
        "cache-hit must report the REALIZED path, not the \"\" placeholder"
    );

    Ok(())
}

/// Negative case: CA node with a modular_hash that has NO realisation
/// row → cache-miss → proceeds to Ready (not Completed).
#[tokio::test]
async fn test_ca_cache_miss_no_realisation() -> TestResult {
    let test_db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(test_db.pool.clone());

    let mut node = make_test_node("ca-cache-miss", "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = [0x99u8; 32].to_vec();
    node.expected_output_paths = vec![String::new()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // Build should be Active (not Complete) — the node wasn't cached.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "CA node with no realisation should NOT cache-hit"
    );

    Ok(())
}
