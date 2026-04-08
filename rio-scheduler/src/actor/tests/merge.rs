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
            jwt_token: None,
        },
    )
    .await?;

    // Connect a 1-slot worker. Heartbeat/PrefetchComplete triggers
    // dispatch_ready, which pops the highest-priority node.
    let mut rx = connect_executor(&handle, "prio-w", "x86_64-linux").await?;

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
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
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
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
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
                jwt_token: None,
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
    store
        .faults
        .fail_find_missing
        .store(false, Ordering::SeqCst);

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
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);
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
                jwt_token: None,
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
    // No store client — exercises the I-048 fail-open: CA realisation
    // verify can't reach the store, so the realisation is trusted.
    // With a store client, the realized path would be verified
    // (test_ca_cache_miss_stale_realisation covers that case).
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

// r[verify sched.merge.stale-completed-verify]
/// I-048: a CA node whose realisation row points to a GC'd output must
/// NOT cache-hit. The realisations table is scheduler-local; store GC
/// doesn't touch it. Without the store-existence verify, this node
/// flips to Completed and ping-pongs against I-047's pre-existing-
/// completed reset on subsequent merges.
///
/// Seed the realisation in PG but NOT the path in MockStore. With a
/// store client present, FindMissingPaths reports missing → the
/// realisation is filtered → the node proceeds to Ready.
#[tokio::test]
async fn test_ca_cache_miss_stale_realisation() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let modular_hash = [0x55u8; 32];
    let stale_path = test_store_path("ca-gcd-out");

    // Realisation exists (a prior build registered it), but the path is
    // NOT in MockStore.paths (GC'd). The CA cache-check should find the
    // realisation, then the store-existence verify should reject it.
    crate::ca::insert_realisation(
        &test_db.pool,
        &modular_hash,
        "out",
        &stale_path,
        &[0x22u8; 32],
    )
    .await?;

    let mut node = make_test_node("ca-stale-real", "x86_64-linux");
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    node.expected_output_paths = vec![String::new()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // The realisation was filtered → node is NOT cached → build is
    // Active waiting for it to dispatch.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "stale realisation must NOT cache-hit; node should proceed to Ready"
    );
    assert_eq!(
        status.cached_derivations, 0,
        "stale realisation must not count as cached"
    );

    Ok(())
}

// r[verify sched.merge.ca-fod-substitute]
/// Fixed-CA FOD: `ca_modular_hash` is 32 bytes (every FOD per
/// translate.rs:343) AND `expected_output_paths` is non-empty (the
/// content-addressed path computed from outputHash). No realisation
/// row exists. The output is NOT in rio-store but IS substitutable
/// from upstream → MUST cache-hit via the path-based lane.
///
/// Regression for I-203: filtering on `ca_modular_hash.len() != 32`
/// excluded these from the path-based lane → dispatched to a fetcher
/// → hit the (dead) origin URL.
#[tokio::test]
async fn test_fixed_ca_fod_substitutable_is_cache_hit() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let fod_out = test_store_path("chromium-buildtools-source");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(fod_out.clone());

    // Production shape: FOD ⇒ is_content_addressed + 32-byte modular
    // hash + known expected_output_path. NO realisation row in PG.
    let mut node = make_test_node("ca-fod-sub", "x86_64-linux");
    node.is_content_addressed = true;
    node.is_fixed_output = true;
    node.ca_modular_hash = [0x42u8; 32].to_vec();
    node.expected_output_paths = vec![fod_out.clone()];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "fixed-CA FOD with known path + substitutable output MUST cache-hit \
         via path-based lane; got state={}",
        status.state
    );
    assert_eq!(status.cached_derivations, 1);

    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&fod_out),
        "path-based lane should eager-fetch substitutable FOD output; \
         qpi_calls={qpi:?}"
    );

    Ok(())
}

/// Negative case for `r[sched.merge.ca-fod-substitute]`: same fixed-CA
/// FOD shape but the output is plain-missing (not substitutable). Node
/// proceeds to Ready and dispatches to a fetcher.
#[tokio::test]
async fn test_fixed_ca_fod_not_substitutable_dispatches() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // FOD ⇒ Fetcher pool, not Builder.
    let mut fetcher_rx =
        connect_fetcher_classed(&handle, "f-ca-fod", "x86_64-linux", "tiny").await?;

    let fod_out = test_store_path("not-in-any-cache");
    let mut node = make_test_node("ca-fod-miss", "x86_64-linux");
    node.is_content_addressed = true;
    node.is_fixed_output = true;
    node.ca_modular_hash = [0x43u8; 32].to_vec();
    node.expected_output_paths = vec![fod_out];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let assn = recv_assignment(&mut fetcher_rx).await;
    assert!(
        assn.drv_path.ends_with("ca-fod-miss.drv"),
        "fixed-CA FOD not in store/upstream → must dispatch; got {}",
        assn.drv_path
    );

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.cached_derivations, 0);

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

// r[verify sched.merge.substitute-probe]
// r[verify sched.merge.substitute-fetch]
/// Substitutable paths are treated as cache hits at merge time, AND
/// the scheduler eagerly fetches them before marking the derivation
/// completed.
///
/// A path NOT in the store but reported as `substitutable_paths` by
/// FindMissingPaths should transition the derivation straight to
/// Completed — no build dispatch. The scheduler MUST fire
/// QueryPathInfo for each substitutable path (eager fetch): the
/// builder's later FUSE GetPath calls carry no JWT so lazy fetch
/// doesn't work — ENOENT on inputs the scheduler claimed were cached.
///
/// Before P0472: scheduler only read `missing_paths`, ignored
/// `substitutable_paths` → dispatched builds for paths cache.nixos.org
/// already had.
/// Before P0473: scheduler marked substitutable paths completed but
/// never fetched them → builder ENOENT on FUSE access.
#[tokio::test]
async fn test_substitutable_path_is_cache_hit() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Seed: path is NOT in MockStore.paths (→ missing) but IS in
    // MockStore.substitutable (→ substitutable_paths in response).
    let sub_path = test_store_path("hello-2.12.3");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(sub_path.clone());

    let mut node = make_test_node("sub-probe", "x86_64-linux");
    node.expected_output_paths = vec![sub_path.clone()];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // Build should have Succeeded: the only derivation cache-hit via
    // substitution probe. No dispatch, no worker needed.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "substitutable path should cache-hit → build completes immediately; \
         got state={}",
        status.state
    );

    // Eager fetch: scheduler MUST have fired QueryPathInfo for the
    // substitutable path. MockStore records every QPI call.
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&sub_path),
        "scheduler should eager-fetch substitutable path via QueryPathInfo; \
         qpi_calls={qpi:?}"
    );

    Ok(())
}

// r[verify sched.merge.substitute-fetch]
/// A substitutable path whose eager fetch fails is demoted to
/// cache-miss — the derivation falls through to normal dispatch
/// instead of being marked completed against a phantom hit.
#[tokio::test]
async fn test_substitutable_fetch_failure_demotes_to_miss() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let sub_path = test_store_path("fetch-fails");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(sub_path.clone());
    // Inject QPI failure: FindMissingPaths reports substitutable, but
    // the eager fetch errors → path must drop from the cache-hit set.
    store
        .faults
        .fail_query_path_info
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let mut node = make_test_node("sub-fetch-fail", "x86_64-linux");
    node.expected_output_paths = vec![sub_path];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // Build Active — NOT Succeeded. The fetch failed so the path
    // was demoted; derivation is waiting for a worker.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "failed eager fetch should demote substitutable path to miss; \
         got state={}",
        status.state
    );

    Ok(())
}

// r[verify sched.merge.substitute-probe]
/// Negative: a missing path NOT in substitutable_paths stays missing.
///
/// Guards against accidentally treating ALL missing paths as
/// substitutable. Only paths the store's HEAD probe confirmed count.
#[tokio::test]
async fn test_missing_non_substitutable_stays_missing() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (_store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // No seeding: path is missing AND not substitutable.
    let mut node = make_test_node("truly-missing", "x86_64-linux");
    node.expected_output_paths = vec![test_store_path("truly-missing-out")];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    // Build Active — derivation is Ready, waiting for a worker.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "missing non-substitutable path should NOT cache-hit"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown]
/// Top-down short-circuit: when the root is substitutable, deps are
/// pruned from the merge — only the root's NAR is fetched, build
/// completes immediately.
///
/// Scenario mirrors `rsb -L p#hello`: root=hello, deps=glibc,gcc,
/// stdenv. hello's output is in cache.nixos.org. Before this fix:
/// scheduler would FindMissingPaths for all 4, eager-fetch all 4
/// NARs. After: FindMissingPaths for just the root, eager-fetch
/// just the root NAR, prune deps from the DAG.
#[tokio::test]
async fn test_topdown_root_substitutable_prunes_deps() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Seed: root output substitutable. Dep outputs NOT seeded (not
    // needed — top-down should never check them).
    let root_out = test_store_path("hello-2.12.3");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());

    // DAG: hello (root) → glibc, gcc, stdenv (deps).
    let mut root = make_test_node("hello", "x86_64-linux");
    root.expected_output_paths = vec![root_out.clone()];
    let mut glibc = make_test_node("glibc", "x86_64-linux");
    glibc.expected_output_paths = vec![test_store_path("glibc-out")];
    let mut gcc = make_test_node("gcc", "x86_64-linux");
    gcc.expected_output_paths = vec![test_store_path("gcc-out")];
    let mut stdenv = make_test_node("stdenv", "x86_64-linux");
    stdenv.expected_output_paths = vec![test_store_path("stdenv-out")];

    let nodes = vec![root, glibc, gcc, stdenv];
    let edges = vec![
        make_test_edge("hello", "glibc"),
        make_test_edge("hello", "gcc"),
        make_test_edge("hello", "stdenv"),
    ];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // Build Succeeded: root cached via top-down, deps pruned.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "root substitutable → build completes immediately; got state={}",
        status.state
    );

    // ONLY the root fetched. Deps never queried.
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&root_out),
        "root NAR should be eager-fetched; qpi_calls={qpi:?}"
    );
    for dep in ["glibc-out", "gcc-out", "stdenv-out"] {
        let dep_path = test_store_path(dep);
        assert!(
            !qpi.contains(&dep_path),
            "dep {dep} should NOT be fetched when root is cached; qpi_calls={qpi:?}"
        );
    }

    // Total derivations reported = 1 (root only), not 4.
    assert_eq!(
        status.total_derivations, 1,
        "pruned DAG should report root count, not original submission size"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown]
/// Top-down negative: root NOT substitutable → fall through to
/// full bottom-up check. All nodes merged, deps processed normally.
#[tokio::test]
async fn test_topdown_root_missing_falls_through() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    // Seed: dep output substitutable, root NOT. Top-down sees root
    // missing → falls through → bottom-up finds glibc substitutable.
    let glibc_out = test_store_path("glibc-fallthru");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(glibc_out.clone());

    let mut root = make_test_node("app", "x86_64-linux");
    root.expected_output_paths = vec![test_store_path("app-out")];
    let mut glibc = make_test_node("glibc-ft", "x86_64-linux");
    glibc.expected_output_paths = vec![glibc_out.clone()];

    let nodes = vec![root, glibc];
    let edges = vec![make_test_edge("app", "glibc-ft")];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // Build Active (not Succeeded): root not cached, must build.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "root not substitutable → fall through to full merge"
    );

    // Full DAG merged — 2 derivations, not pruned to 1.
    assert_eq!(
        status.total_derivations, 2,
        "fall-through should merge the full DAG"
    );

    // Bottom-up still fires: glibc fetched via check_cached_outputs.
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&glibc_out),
        "bottom-up should fetch substitutable dep on fall-through; qpi_calls={qpi:?}"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown]
/// Top-down: deps pruned from this build are NOT in the global DAG,
/// so a later build that needs them triggers its own cache-check.
///
/// Guards against the shared-DAG correctness bug where marking
/// deps as Completed without fetching would poison later builds
/// that actually need the dep NAR.
#[tokio::test]
async fn test_topdown_pruned_deps_not_in_global_dag() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let hello_out = test_store_path("hello-shared");
    let glibc_out = test_store_path("glibc-shared");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(hello_out.clone());
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(glibc_out.clone());

    // Build A: hello → glibc. hello substitutable → glibc pruned.
    let mut hello = make_test_node("hello-a", "x86_64-linux");
    hello.expected_output_paths = vec![hello_out.clone()];
    let mut glibc_a = make_test_node("glibc-a", "x86_64-linux");
    glibc_a.expected_output_paths = vec![glibc_out.clone()];

    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![hello, glibc_a],
        vec![make_test_edge("hello-a", "glibc-a")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let status_a = query_status(&handle, build_a).await?;
    assert_eq!(
        status_a.state,
        rio_proto::types::BuildState::Succeeded as i32
    );

    // Clear QPI tracking between builds.
    store.calls.qpi_calls.write().unwrap().clear();

    // Build B: app → glibc. app NOT substitutable → falls through
    // → full merge → glibc is newly_inserted (NOT pre-existing from
    // A, because A pruned it) → check_cached_outputs fetches glibc.
    let mut app = make_test_node("app-b", "x86_64-linux");
    app.expected_output_paths = vec![test_store_path("app-b-out")];
    let mut glibc_b = make_test_node("glibc-a", "x86_64-linux");
    glibc_b.expected_output_paths = vec![glibc_out.clone()];

    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![app, glibc_b],
        vec![make_test_edge("app-b", "glibc-a")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // glibc fetched by Build B's bottom-up — proves it wasn't
    // stuck as phantom-Completed from Build A's prune.
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&glibc_out),
        "Build B should fetch glibc (pruned from A, newly-inserted in B); \
         qpi_calls={qpi:?}"
    );

    Ok(())
}

// ===========================================================================
// I-047: pre-existing Completed with GC'd output → reset to Ready
// ===========================================================================

// r[verify sched.merge.stale-completed-verify]
/// A pre-existing `Completed` node whose output has been GC'd from
/// the store must reset to `Ready` at merge time, so newly-inserted
/// dependents stay `Queued` instead of unlocking against a missing
/// output. The reset node re-dispatches.
///
/// Production scenario (I-047): FOD outputs are content-addressed and
/// shared across builds. GC may delete a FOD output under one tenant's
/// retention while a later build's DAG still has the node `Completed`.
/// Without this verify, the worker fails on isValidPath when building
/// the dependent.
#[tokio::test]
async fn test_preexisting_completed_with_gcd_output_resets_to_ready() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let mut worker_rx = connect_executor(&handle, "w-gc", "x86_64-linux").await?;

    // Build A: app-a → fod-dep. fod-dep is the FOD-like leaf that
    // will complete and then have its output GC'd. app-a stays Ready
    // (single-slot worker is busy with fod-dep first) so Build A
    // stays Active and fod-dep stays in the global DAG.
    let fod_out = test_store_path("which-2.23.tar.gz");
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![
            make_test_node("app-a", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-a", "fod-dep")],
        false,
    )
    .await?;

    // fod-dep dispatches (it's the leaf). Seed its output so the
    // verify-at-merge sees it as PRESENT initially (sanity), then
    // remove to simulate GC.
    let assn = recv_assignment(&mut worker_rx).await;
    assert!(
        assn.drv_path.ends_with("fod-dep.drv"),
        "expected fod-dep dispatch first (leaf); got {}",
        assn.drv_path
    );
    store.seed_with_content(&fod_out, b"fod-contents");
    complete_success(&handle, "w-gc", &assn.drv_path, &fod_out).await?;
    barrier(&handle).await;

    // app-a now dispatches (fod-dep Completed unlocked it). One-shot:
    // w-gc drained on completion; connect a fresh worker for app-a.
    let mut worker_rx = connect_executor(&handle, "w-gc-appa", "x86_64-linux").await?;
    let _assn_app_a = recv_assignment(&mut worker_rx).await;

    // === GC: delete fod-dep's output from the store ===
    let removed = store.state.paths.write().unwrap().remove(&fod_out);
    assert!(removed.is_some(), "GC sim: fod_out should have been seeded");

    // Third worker so fod-dep can re-dispatch while w-gc-appa is
    // busy with app-a.
    let mut worker_rx2 = connect_executor(&handle, "w-gc2", "x86_64-linux").await?;

    // Build B: app-b → fod-dep. fod-dep is PRE-EXISTING (still in DAG
    // via Build A's interest, status Completed). The verify must catch
    // that fod_out is gone and reset fod-dep to Ready. app-b must
    // then compute as Queued (dep not Completed), not Ready.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![
            make_test_node("app-b", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-b", "fod-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // fod-dep was reset to Ready and re-queued; it dispatches again
    // to the idle worker (w-gc is busy with app-a).
    let reassn = recv_assignment(&mut worker_rx2).await;
    assert!(
        reassn.drv_path.ends_with("fod-dep.drv"),
        "fod-dep should re-dispatch after GC reset; got {}",
        reassn.drv_path
    );

    // Build B is still Active — app-b is Queued waiting on fod-dep.
    // (Before the fix: fod-dep stayed Completed, app-b went Ready,
    // dispatched, and the worker would fail on isValidPath.)
    let status_b = query_status(&handle, build_b).await?;
    assert_eq!(
        status_b.state,
        rio_proto::types::BuildState::Active as i32,
        "Build B must be Active (app-b Queued on re-dispatching fod-dep)"
    );
    assert_eq!(
        status_b.cached_derivations, 0,
        "fod-dep must NOT count as cached for Build B (output was GC'd)"
    );

    Ok(())
}

// r[verify sched.merge.stale-substitutable]
/// I-202: a pre-existing `Completed` node whose GC'd output is
/// substitutable from upstream stays `Completed` (eager-fetched), not
/// reset to `Ready`. Same setup as the GC'd-output test above, but
/// the path is seeded in `MockStore.substitutable` — the verify must
/// fire QueryPathInfo (eager fetch) and skip the reset.
///
/// Before the fix: `verify_preexisting_completed` ignored
/// `substitutable_paths` (and sent no JWT, so the real store returned
/// none anyway). The node reset to Ready and the whole subtree —
/// including FOD sources with dead origin URLs — re-dispatched.
#[tokio::test]
async fn test_preexisting_completed_gcd_but_substitutable_stays_completed() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let mut worker_rx = connect_executor(&handle, "w-sub", "x86_64-linux").await?;

    let fod_out = test_store_path("fod-substitutable");
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![
            make_test_node("app-a", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-a", "fod-dep")],
        false,
    )
    .await?;

    let assn = recv_assignment(&mut worker_rx).await;
    assert!(assn.drv_path.ends_with("fod-dep.drv"));
    store.seed_with_content(&fod_out, b"fod-contents");
    complete_success(&handle, "w-sub", &assn.drv_path, &fod_out).await?;
    barrier(&handle).await;

    // Hold app-a Running so Build A stays Active and fod-dep stays in
    // the global DAG (pre-existing for Build B).
    let mut worker_appa = connect_executor(&handle, "w-sub-appa", "x86_64-linux").await?;
    let _assn_app_a = recv_assignment(&mut worker_appa).await;

    // GC removes the output from rio-store, BUT cache.nixos.org has it.
    store.state.paths.write().unwrap().remove(&fod_out);
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(fod_out.clone());

    // Spare worker — should stay IDLE because fod-dep doesn't reset.
    let mut worker_spare = connect_executor(&handle, "w-sub-spare", "x86_64-linux").await?;

    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![
            make_test_node("app-b", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-b", "fod-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Eager-fetch fired: QueryPathInfo called for fod_out.
    let qpi = store.calls.qpi_calls.read().unwrap().clone();
    assert!(
        qpi.contains(&fod_out),
        "stale-completed verify should eager-fetch the substitutable output; \
         qpi_calls={qpi:?}"
    );

    // fod-dep stayed Completed → app-b is Ready (not Queued) and
    // dispatches to the spare worker. fod-dep itself does NOT
    // re-dispatch.
    let assn_spare = recv_assignment(&mut worker_spare).await;
    assert!(
        assn_spare.drv_path.ends_with("app-b.drv"),
        "fod-dep must stay Completed (substituted, not reset); spare worker \
         should get app-b, not fod-dep; got {}",
        assn_spare.drv_path
    );

    let status_b = query_status(&handle, build_b).await?;
    assert_eq!(
        status_b.cached_derivations, 1,
        "fod-dep should count as cached for Build B (output substituted)"
    );

    Ok(())
}

// r[verify sched.merge.stale-substitutable]
/// Negative case: the GC'd output is reported substitutable, but the
/// eager fetch FAILS (QueryPathInfo errors). The node falls through
/// to reset-to-Ready — same as if it were never substitutable.
#[tokio::test]
async fn test_preexisting_completed_substitute_fetch_fail_resets_to_ready() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let mut worker_rx = connect_executor(&handle, "w-sf", "x86_64-linux").await?;

    let fod_out = test_store_path("fod-sub-fail");
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![
            make_test_node("app-a", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-a", "fod-dep")],
        false,
    )
    .await?;

    let assn = recv_assignment(&mut worker_rx).await;
    store.seed_with_content(&fod_out, b"fod-contents");
    complete_success(&handle, "w-sf", &assn.drv_path, &fod_out).await?;
    barrier(&handle).await;
    let mut worker_appa = connect_executor(&handle, "w-sf-appa", "x86_64-linux").await?;
    let _assn_app_a = recv_assignment(&mut worker_appa).await;

    // GC; substitutable; but QPI is broken → eager-fetch fails.
    store.state.paths.write().unwrap().remove(&fod_out);
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(fod_out.clone());
    store
        .faults
        .fail_query_path_info
        .store(true, Ordering::SeqCst);

    let mut worker_spare = connect_executor(&handle, "w-sf-spare", "x86_64-linux").await?;

    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![
            make_test_node("app-b", "x86_64-linux"),
            make_test_node("fod-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("app-b", "fod-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Eager-fetch failed → fod-dep reset to Ready → re-dispatched.
    let reassn = recv_assignment(&mut worker_spare).await;
    assert!(
        reassn.drv_path.ends_with("fod-dep.drv"),
        "fetch-fail must fall through to reset-to-Ready (re-dispatch fod-dep); \
         got {}",
        reassn.drv_path
    );

    Ok(())
}

// r[verify sched.merge.stale-completed-verify]
/// Fail-open: if the store is unreachable during the stale-Completed
/// verify, skip verification and treat pre-existing Completed as
/// valid. The GC race is rare; blocking merge on store availability
/// would be worse than the original bug.
#[tokio::test]
async fn test_preexisting_completed_verify_fail_open_on_store_error() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use std::sync::atomic::Ordering;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let mut worker_rx = connect_executor(&handle, "w-fo", "x86_64-linux").await?;

    let fod_out = test_store_path("fail-open-out");
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![
            make_test_node("fo-app-a", "x86_64-linux"),
            make_test_node("fo-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("fo-app-a", "fo-dep")],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut worker_rx).await;
    complete_success(&handle, "w-fo", &assn.drv_path, &fod_out).await?;
    barrier(&handle).await;
    let mut worker_rx = connect_executor(&handle, "w-fo-2", "x86_64-linux").await?;
    let _assn_app_a = recv_assignment(&mut worker_rx).await;

    // Store goes unreachable. The verify's FindMissingPaths will fail.
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);

    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![
            make_test_node("fo-app-b", "x86_64-linux"),
            make_test_node("fo-dep", "x86_64-linux"),
        ],
        vec![make_test_edge("fo-app-b", "fo-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Fail-open: fo-dep stays Completed, counts as cached for Build B.
    let status_b = query_status(&handle, build_b).await?;
    assert_eq!(
        status_b.cached_derivations, 1,
        "fail-open: store unreachable → fo-dep stays Completed, counts as cached"
    );

    Ok(())
}

// ===========================================================================
// I-099/I-094: re-probe existing not-done nodes at merge
// ===========================================================================

/// Build #1 inserts node A (not in store, not substitutable) → A is
/// Ready. Upstream cache config is then added (seed substitutable).
/// Build #2 references A → re-probe finds it → A transitions to
/// Completed, build #2 succeeds immediately.
///
/// Sensitivity: without the I-099 fix, build #2's probe only checks
/// newly_inserted (empty — A already in DAG), A stays Ready, build #2
/// is Active waiting for a worker.
#[tokio::test]
async fn test_reprobe_existing_ready_caches_on_second_merge() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let path = test_store_path("reprobe-ready");
    let mut node = make_test_node("reprobe-ready", "x86_64-linux");
    node.expected_output_paths = vec![path.clone()];

    // Build #1: path NOT substitutable → A is Ready (no deps, no cache).
    let build1 = Uuid::new_v4();
    merge_dag(&handle, build1, vec![node.clone()], vec![], false).await?;
    barrier(&handle).await;
    let info = handle
        .debug_query_derivation("reprobe-ready")
        .await?
        .expect("node A in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "precondition: A is Ready after build #1 (no cache, no worker)"
    );

    // Upstream cache now has the path.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(path.clone());

    // Build #2: re-probe should find A in upstream → Completed.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = handle
        .debug_query_derivation("reprobe-ready")
        .await?
        .expect("node A still in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "I-099: existing Ready node re-probed at build #2 merge, found in \
         upstream cache → Completed (was: stayed Ready, never re-checked)"
    );
    let status2 = query_status(&handle, build2).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build #2 should succeed immediately via re-probe cache hit"
    );

    Ok(())
}

/// I-094 fold-in: a Poisoned node whose output later appears in the
/// upstream cache is unpoisoned + completed at the next merge that
/// references it. Prior failure history is moot — we have the output.
///
/// Sensitivity: without the fix, build #2 sees A is Poisoned → sets
/// first_dep_failed → build #2 fails fast.
#[tokio::test]
async fn test_reprobe_existing_poisoned_unpoisons_on_cache_hit() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;

    let test_db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(test_db.pool.clone(), Some(store_client));

    let path = test_store_path("reprobe-poison");
    let mut node = make_test_node("reprobe-poison", "x86_64-linux");
    node.expected_output_paths = vec![path.clone()];

    // Build #1 + worker: assign → PermanentFailure → Poisoned.
    let mut worker_rx = connect_executor(&handle, "rp-worker", "x86_64-linux").await?;
    let build1 = Uuid::new_v4();
    merge_dag(&handle, build1, vec![node.clone()], vec![], false).await?;
    let _ = worker_rx.recv().await.expect("assignment");
    complete_failure(
        &handle,
        "rp-worker",
        &test_drv_path("reprobe-poison"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;
    barrier(&handle).await;
    let info = handle
        .debug_query_derivation("reprobe-poison")
        .await?
        .expect("node in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "precondition: A is Poisoned after PermanentFailure"
    );
    let status1 = query_status(&handle, build1).await?;
    assert_eq!(
        status1.state,
        rio_proto::types::BuildState::Failed as i32,
        "precondition: build #1 failed"
    );

    // Upstream cache now has the path.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(path.clone());

    // Build #2: re-probe should find A in upstream → unpoisoned + Completed.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = handle
        .debug_query_derivation("reprobe-poison")
        .await?
        .expect("node still in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "I-094: Poisoned node re-probed, found in upstream → Completed \
         (was: stayed Poisoned, build #2 failed fast)"
    );
    let status2 = query_status(&handle, build2).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build #2 should succeed via re-probe unpoisoning"
    );

    Ok(())
}

// ===========================================================================
// I-169: Poisoned resubmit bound
// ===========================================================================

/// I-169: a `Poisoned` node with `retry_count < POISON_RESUBMIT_RETRY_LIMIT`
/// resets on explicit resubmit and re-dispatches. The build does NOT
/// fail-fast.
///
/// Sensitivity: before the fix, build #2 sees A is Poisoned →
/// `first_dep_failed` set in the pre-existing-nodes loop → build #2
/// fail-fasts. With the fix, A is reset in `dag.merge` → in
/// `newly_inserted` → skipped by the pre-existing loop → re-dispatched.
// r[verify sched.merge.poisoned-resubmit-bounded]
#[tokio::test]
async fn test_resubmit_resets_poisoned_under_retry_limit() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Build #1: single node, force-poison at retry_count=2.
    let node = make_test_node("i169-under", "x86_64-linux");
    let build1 = Uuid::new_v4();
    merge_dag(&handle, build1, vec![node.clone()], vec![], false).await?;
    assert!(handle.debug_force_poisoned("i169-under", 2).await?);
    barrier(&handle).await;
    let info = handle
        .debug_query_derivation("i169-under")
        .await?
        .expect("node in DAG");
    assert_eq!(info.status, DerivationStatus::Poisoned, "precondition");
    assert_eq!(info.retry.count, 2, "precondition");

    // Build #2: resubmit. Reset path runs in dag.merge BEFORE the
    // pre-existing-nodes fail-fast loop. No worker connected → node sits
    // at Ready (deferred), not Assigned.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = handle
        .debug_query_derivation("i169-under")
        .await?
        .expect("node still in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "I-169: Poisoned with retry_count=2 resets to Ready on resubmit \
         (was: stayed Poisoned, build #2 fail-fasted)"
    );
    assert_eq!(
        info.retry.count, 2,
        "retry_count carried over so the bound accumulates"
    );
    let status2 = query_status(&handle, build2).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Active as i32,
        "build #2 is Active (not fail-fast Failed)"
    );

    Ok(())
}

/// I-169 bound: at `retry_count >= POISON_RESUBMIT_RETRY_LIMIT`, the
/// `Poisoned` node stays Poisoned and the resubmitted build fail-fasts.
// r[verify sched.merge.poisoned-resubmit-bounded]
#[tokio::test]
async fn test_resubmit_fail_fasts_poisoned_at_retry_limit() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;

    let (_db, handle, _task) = setup().await;

    let node = make_test_node("i169-at-limit", "x86_64-linux");
    let build1 = Uuid::new_v4();
    merge_dag(&handle, build1, vec![node.clone()], vec![], false).await?;
    assert!(
        handle
            .debug_force_poisoned("i169-at-limit", POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );
    barrier(&handle).await;

    // Build #2: resubmit. Node is at the limit → NOT reset → pre-existing
    // loop matches Poisoned → first_dep_failed set → build fail-fasts.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = handle
        .debug_query_derivation("i169-at-limit")
        .await?
        .expect("node still in DAG");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "Poisoned at retry limit stays Poisoned on resubmit"
    );
    let status2 = query_status(&handle, build2).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Failed as i32,
        "build #2 fail-fasts on Poisoned-at-limit (24h TTL or ClearPoison to override)"
    );

    Ok(())
}

// ===========================================================================
// Large-DAG merge perf bound (I-139)
// ===========================================================================

/// I-139: end-to-end `handle_merge_dag` perf bound on a 50k-node /
/// ~250k-edge synthetic DAG against a real (ephemeral) PG.
///
/// Before the fix, the initial-states persist phase did one
/// `update_derivation_status` round-trip PER newly-inserted node. At
/// ~1.8ms RTT × 50k = ~90s (the 153k-node production case was ~278s).
/// After batching to three `ANY($1::text[])` updates, this phase is a
/// handful of round-trips regardless of DAG size — the merge is
/// dominated by the (already-batched) UNNEST insert + ANALYZE.
///
/// 30s bound: ephemeral PG initdb is single-disk, debug build, no
/// optimizer; the UNNEST insert of 50k rows + ANALYZE alone is several
/// seconds. The point is "not O(nodes) round-trips", not a microbench.
///
/// No store_client (`setup()` passes None) so `check_cached_outputs`
/// returns empty → all 50k nodes flow through `compute_initial_states`
/// → exactly the path that regressed.
///
/// **Regression guard:** localhost Unix-socket RTT (~35μs) is too fast
/// for the wall-clock delta to discriminate reliably. Instead assert
/// on `pg_stat_database.xact_commit`: each pool-level `execute()`
/// autocommits, so per-node updates show as ~N extra transactions;
/// batched updates are O(1) regardless of N. Bound: `< N/2` (loose
/// enough for the dozen legitimate per-build queries + ANALYZE; tight
/// enough that one-per-node is a hard fail).
#[tokio::test]
async fn test_handle_merge_dag_large_perf_bound() -> TestResult {
    const N: usize = 50_000;
    const FANOUT: usize = 5;

    let (db, handle, _task) = setup().await;

    async fn xact_commit(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()",
        )
        .fetch_one(pool)
        .await
        .expect("pg_stat_database")
    }

    // Same shape as dag/tests.rs make_wide_dag (path helper inlined —
    // that one is module-private).
    let path = |i: usize| format!("/nix/store/{i:032}-n{i}.drv");
    let nodes: Vec<_> = (0..N)
        .map(|i| rio_proto::dag::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_test_node("x", "x86_64-linux")
        })
        .collect();
    let mut edges = Vec::with_capacity(N * FANOUT);
    for i in FANOUT..N {
        for j in 1..=FANOUT {
            edges.push(rio_proto::dag::DerivationEdge {
                parent_drv_path: path(i),
                child_drv_path: path(i - j),
            });
        }
    }

    let build_id = Uuid::new_v4();
    let xact_before = xact_commit(&db.pool).await;
    let t = std::time::Instant::now();
    let _rx = merge_dag(&handle, build_id, nodes, edges, false).await?;
    let elapsed = t.elapsed();
    barrier(&handle).await;
    let xact_delta = xact_commit(&db.pool).await - xact_before;
    eprintln!(
        "I-139 actor bench: {N} nodes / {} edges — handle_merge_dag {elapsed:?}, \
         {xact_delta} PG transactions",
        (N - FANOUT) * FANOUT
    );

    assert!(
        xact_delta < (N / 2) as i64,
        "handle_merge_dag of {N} nodes issued {xact_delta} PG transactions \
         (~1 per node); per-node DB round-trip regression (I-139). \
         Expected O(1) batched updates."
    );
    assert!(
        elapsed.as_secs() < 30,
        "handle_merge_dag of {N} nodes took {elapsed:?} (>30s); \
         per-node DB round-trip regression in initial-states persist (I-139)"
    );

    // Sanity: build is Active, all nodes registered.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Active as i32);
    assert_eq!(status.total_derivations, N as u32);
    Ok(())
}

// ===========================================================================
// Large-DAG completion + re-dispatch perf bound (I-140)
// ===========================================================================

/// I-140: post-merge per-completion + per-heartbeat-dispatch perf bound on
/// a 50k-node / ~250k-edge synthetic DAG against a real (ephemeral) PG.
///
/// The I-139 fix made `handle_merge_dag` fast, but build 019d559a then
/// stalled at 1275/153821 (1244 cached + 31 built) — completions and
/// heartbeats were processed, but admin RPCs timed out at 30s and new
/// builders idle-timed-out after 120s with no assignment. The actor was
/// alive but head-of-line blocked.
///
/// This test drives the actor THROUGH a merge, then exercises the two hot
/// paths that fire on every step of the build:
///   - `Heartbeat` → `dispatch_ready()` (per heartbeat, which is per
///     ephemeral-builder-connect + ~10s thereafter)
///   - `ProcessCompletion` → `handle_success_completion` (per drv done)
///
/// Same xact_commit-delta guard as I-139: per-node DB round-trips on
/// either path show as ~N extra transactions; correct behavior is O(1)
/// regardless of N. Wall-clock bound is loose (debug + ephemeral PG).
#[tokio::test]
async fn test_large_dag_completion_dispatch_perf_bound() -> TestResult {
    const N: usize = 50_000;
    const FANOUT: usize = 5;

    let (db, handle, _task) = setup().await;

    async fn xact_commit(pool: &sqlx::PgPool) -> i64 {
        sqlx::query_scalar(
            "SELECT xact_commit FROM pg_stat_database WHERE datname = current_database()",
        )
        .fetch_one(pool)
        .await
        .expect("pg_stat_database")
    }

    let path = |i: usize| format!("/nix/store/{i:032}-n{i}.drv");
    let nodes: Vec<_> = (0..N)
        .map(|i| rio_proto::dag::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_test_node("x", "x86_64-linux")
        })
        .collect();
    let mut edges = Vec::with_capacity(N * FANOUT);
    for i in FANOUT..N {
        for j in 1..=FANOUT {
            edges.push(rio_proto::dag::DerivationEdge {
                parent_drv_path: path(i),
                child_drv_path: path(i - j),
            });
        }
    }

    let build_id = Uuid::new_v4();
    let _rx = merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // --- Heartbeat → dispatch_ready (no worker → all-defer) -----------
    // Connect a worker. dispatch_ready fires on its Heartbeat AND on
    // PrefetchComplete (connect_executor sends both). With one worker
    // and FANOUT initial Ready leaves, this assigns 1 and defers the
    // rest. The point is the drain-loop cost when ready_queue >> workers.
    let xact_before = xact_commit(&db.pool).await;
    let t = std::time::Instant::now();
    let mut wrx = connect_executor(&handle, "w0", "x86_64-linux").await?;
    let assignment = recv_assignment(&mut wrx).await;
    barrier(&handle).await;
    let dispatch_elapsed = t.elapsed();
    let dispatch_xact = xact_commit(&db.pool).await - xact_before;
    eprintln!(
        "I-140 dispatch bench: {N} nodes — connect+heartbeat+dispatch {dispatch_elapsed:?}, \
         {dispatch_xact} PG xacts, assigned {}",
        assignment.drv_path
    );

    // --- ProcessCompletion → handle_success_completion ----------------
    // Complete the assigned leaf. This walks find_newly_ready (cheap) +
    // update_ancestors (potentially deep) + 2× build_summary
    // (O(N) each) + per-newly-ready persist_status round-trips.
    let xact_before = xact_commit(&db.pool).await;
    let t = std::time::Instant::now();
    complete_success(&handle, "w0", &assignment.drv_path, "/nix/store/out0").await?;
    barrier(&handle).await;
    let complete_elapsed = t.elapsed();
    let complete_xact = xact_commit(&db.pool).await - xact_before;
    eprintln!(
        "I-140 completion bench: {N} nodes — handle_completion {complete_elapsed:?}, \
         {complete_xact} PG xacts"
    );

    // --- Second heartbeat → re-dispatch -------------------------------
    // After completion the worker's slot is free. A heartbeat should
    // assign the next Ready derivation. This is the path that stalled
    // in prod: builder connects, heartbeats, gets nothing for 120s.
    let xact_before = xact_commit(&db.pool).await;
    let t = std::time::Instant::now();
    let mut wrx2 = connect_executor(&handle, "w1", "x86_64-linux").await?;
    let assignment2 = recv_assignment(&mut wrx2).await;
    barrier(&handle).await;
    let redispatch_elapsed = t.elapsed();
    let redispatch_xact = xact_commit(&db.pool).await - xact_before;
    eprintln!(
        "I-140 re-dispatch bench: {N} nodes — heartbeat+dispatch {redispatch_elapsed:?}, \
         {redispatch_xact} PG xacts, assigned {}",
        assignment2.drv_path
    );

    // --- Admin RPC under load -----------------------------------------
    // ClusterSnapshot iterates the full DAG. With 50k nodes this should
    // be tens-of-ms, not the 30s+ timeout seen in prod.
    let t = std::time::Instant::now();
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClusterSnapshot { reply: tx })
        .await?;
    let _snap = rx.await?;
    let snap_elapsed = t.elapsed();
    eprintln!("I-140 ClusterSnapshot bench: {N} nodes — {snap_elapsed:?}");

    // --- Bounds -------------------------------------------------------
    // Each hot-path step must be O(1) PG xacts and O(N)-in-memory at
    // worst — never O(N) PG round-trips, never O(N²) in-memory.
    // 200-xact bound: assign is ~5 round-trips × FANOUT-ish, plus a
    // handful of best-effort writes; loose enough for legitimate
    // per-assignment work, tight enough that an O(N) regression (50k
    // round-trips) is a hard fail.
    for (label, xact, elapsed) in [
        ("dispatch", dispatch_xact, dispatch_elapsed),
        ("completion", complete_xact, complete_elapsed),
        ("re-dispatch", redispatch_xact, redispatch_elapsed),
    ] {
        assert!(
            xact < 200,
            "I-140: {label} on {N}-node DAG issued {xact} PG transactions; \
             O(N) round-trip regression"
        );
        assert!(
            elapsed.as_secs() < 10,
            "I-140: {label} on {N}-node DAG took {elapsed:?} (>10s); \
             head-of-line block in single-threaded actor"
        );
    }
    assert!(
        snap_elapsed.as_secs() < 2,
        "I-140: ClusterSnapshot on {N}-node DAG took {snap_elapsed:?} (>2s)"
    );
    Ok(())
}

/// I-140: many-worker churn on a large DAG. The single-completion test
/// above is fast because `build_summary` (O(N) full-DAG scan) is only
/// called a handful of times. The production stall was COMPOUNDED:
/// `emit_progress` and `update_build_counts` each call `build_summary`
/// per-assignment + per-completion + per-disconnect, and ephemeral
/// builders churn at scale (controller spawns up to `replicas.max`
/// pods when `queued_derivations` is large).
///
/// This test connects 30 workers, dispatches 30, completes 30,
/// disconnects 30 — one full ephemeral-churn wave. Before the fix, each
/// of the ~90 per-event `build_summary` calls walks the full 50k-node
/// DAG (~25ms debug each ≈ 2.2s total); after the fix the per-event
/// cost is O(1) counts + debounced O(N) progress.
#[tokio::test]
async fn test_large_dag_ephemeral_churn_perf_bound() -> TestResult {
    const N: usize = 50_000;
    const W: usize = 30;

    let (_db, handle, _task) = setup().await;

    // Flat DAG: W independent leaves + (N-W) chained-on-top. The W
    // leaves are all Ready post-merge, so W workers each get one.
    let path = |i: usize| format!("/nix/store/{i:032}-n{i}.drv");
    let nodes: Vec<_> = (0..N)
        .map(|i| rio_proto::dag::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_test_node("x", "x86_64-linux")
        })
        .collect();
    let edges: Vec<_> = (W..N)
        .map(|i| rio_proto::dag::DerivationEdge {
            parent_drv_path: path(i),
            child_drv_path: path(i - W),
        })
        .collect();

    let build_id = Uuid::new_v4();
    let _rx = merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    let t = std::time::Instant::now();
    // --- wave: connect W → dispatch W → complete W → disconnect W ----
    let mut rxs = Vec::with_capacity(W);
    for w in 0..W {
        rxs.push(connect_executor(&handle, &format!("w{w}"), "x86_64-linux").await?);
    }
    let mut assigned = Vec::with_capacity(W);
    for rx in &mut rxs {
        assigned.push(recv_assignment(rx).await.drv_path);
    }
    for (w, drv) in assigned.iter().enumerate() {
        complete_success(&handle, &format!("w{w}"), drv, "/nix/store/out").await?;
    }
    for w in 0..W {
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: format!("w{w}").into(),
            })
            .await?;
    }
    barrier(&handle).await;
    let wave_elapsed = t.elapsed();
    eprintln!(
        "I-140 churn bench: {N} nodes, {W} workers — connect+assign+complete+disconnect \
         wave {wave_elapsed:?} ({:.1}ms/event)",
        wave_elapsed.as_secs_f64() * 1000.0 / (4 * W) as f64
    );

    // 1.5s bound: 4×W=120 events. Pre-fix ≈ 90 build_summary scans
    // (per-assign + 2×per-complete + per-disconnect) × ~20ms each ≈
    // 1.8s debug. Post-fix: per-assign/disconnect emit_progress is
    // debounced (→ ~2 scans total), per-complete shares ONE summary
    // between counts+progress (→ 30 scans) = ~32×20ms ≈ 0.6s. Loose
    // 1.5s bound for CI variance — the point is "doesn't degrade
    // super-linearly with N×W".
    assert!(
        wave_elapsed.as_millis() < 1500,
        "I-140: {W}-worker churn wave on {N}-node DAG took {wave_elapsed:?} (>1.5s); \
         per-event O(N) build_summary scan compounds with ephemeral-builder \
         churn rate — actor mailbox grows unboundedly under load"
    );

    // Correctness: completed_count must reflect the W completions
    // exactly (incremental count must not drift from ground truth).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.completed_derivations, W as u32);
    assert_eq!(status.state, rio_proto::types::BuildState::Active as i32);
    Ok(())
}

// r[verify sched.fod.floor-survives-merge]
/// I-208: a FOD whose DB row pre-exists with `size_class_floor='small'`
/// (promoted by a prior run's failures, then the build terminated and
/// the node left memory) MUST come back at floor=small when re-merged.
/// Regression: `try_from_node` set `floor=None` and the upsert's
/// RETURNING didn't carry `size_class_floor`, so the FOD snapshot
/// bucketed to `fetcher_size_classes[0]` and the controller re-spawned
/// `tiny` every run — chromium/firefox sources looped on 2Gi-storage
/// tiny fetchers indefinitely.
#[tokio::test]
async fn merge_hydrates_size_class_floor_from_db() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        c.fetcher_size_classes = vec![
            crate::assignment::FetcherSizeClassConfig {
                name: "tiny".into(),
            },
            crate::assignment::FetcherSizeClassConfig {
                name: "small".into(),
            },
        ];
    });

    // Pre-seed: prior run promoted this FOD to floor='small', then went
    // terminal. New build re-merges it; ON CONFLICT RETURNING must
    // bring the floor back into the freshly-constructed in-memory state.
    let mut fod = make_test_node("i208-fod", "x86_64-linux");
    fod.is_fixed_output = true;
    fod.expected_output_paths = vec![test_store_path("i208-out")];
    sqlx::query(
        "INSERT INTO derivations
             (drv_hash, drv_path, system, status, is_fixed_output,
              size_class_floor, expected_output_paths, output_names)
         VALUES ($1, $2, 'x86_64-linux', 'completed', true, 'small',
                 ARRAY[$3], ARRAY['out'])",
    )
    .bind(&fod.drv_hash)
    .bind(&fod.drv_path)
    .bind(&fod.expected_output_paths[0])
    .execute(&db.pool)
    .await?;

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![fod], vec![], false).await?;

    let (_builder, fod_snap) = handle
        .query_unchecked(|reply| ActorCommand::GetSizeClassSnapshot {
            pool_features: None,
            reply,
        })
        .await?;
    let tiny = fod_snap.iter().find(|s| s.name == "tiny").unwrap();
    let small = fod_snap.iter().find(|s| s.name == "small").unwrap();
    assert_eq!(
        tiny.queued, 0,
        "I-208: floor='small' from DB must NOT bucket to tiny"
    );
    assert_eq!(small.queued, 1, "I-208: hydrated floor buckets to small");
    Ok(())
}
