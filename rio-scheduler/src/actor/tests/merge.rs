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
        vec![make_node("shared-x"), make_node("filler-y")],
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
            nodes: vec![make_node("shared-x")],
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
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);

    // Merge with expected_output_paths set so check_cached_outputs runs.
    let build_id = Uuid::new_v4();
    let mut node = make_node("cache-err");
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
// r[verify sched.breaker.cache-check+2]
#[tokio::test]
async fn test_cache_check_circuit_breaker_opens_then_closes() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);

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
        let mut node = make_node(&tag);
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
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    store.faults.fail_find_missing.store(true, Ordering::SeqCst);

    let mut seq = 0u32;
    let mut do_merge = |label: &str| {
        seq += 1;
        let tag = format!("{label}-{seq}");
        let mut node = make_node(&tag);
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
            .fetch_one(&_db.pool)
            .await?;
    assert!(
        !tripped_exists,
        "tripped build_id {trip_id} should NOT have an orphan row in PG"
    );

    let reject_exists: bool =
        sqlx::query_scalar("SELECT EXISTS (SELECT 1 FROM builds WHERE build_id = $1)")
            .bind(reject_id)
            .fetch_one(&_db.pool)
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
    .fetch_one(&_db.pool)
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
    let mut node = make_node("ca-cache-hit");
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
            && let Some(rio_proto::types::derivation_event::Status::Cached(c)) = d.status
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
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    let modular_hash = [0x55u8; 32];
    let stale_path = test_store_path("ca-gcd-out");

    // Realisation exists (a prior build registered it), but the path is
    // NOT in MockStore.paths (GC'd). The CA cache-check should find the
    // realisation, then the store-existence verify should reject it.
    crate::ca::insert_realisation(&_db.pool, &modular_hash, "out", &stale_path, &[0x22u8; 32])
        .await?;

    let mut node = make_node("ca-stale-real");
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
/// content-addressed path computed from outputHash). No realisation row.
///
/// - **substitutable**: output not in rio-store but IS substitutable
///   upstream → MUST cache-hit via path-based lane. I-203 regression:
///   filtering on `ca_modular_hash.len() != 32` excluded these →
///   dispatched to fetcher → hit dead origin URL.
/// - **missing**: plain-missing → proceeds to Ready, dispatches to fetcher.
#[rstest::rstest]
#[case::substitutable(true, rio_proto::types::BuildState::Succeeded, 1)]
#[case::missing(false, rio_proto::types::BuildState::Active, 0)]
#[tokio::test]
async fn test_fixed_ca_fod_path_based_lane(
    #[case] substitutable: bool,
    #[case] expect_state: rio_proto::types::BuildState,
    #[case] expect_cached: u32,
) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut fetcher_rx =
        connect_fetcher_classed(&handle, "f-ca-fod", "x86_64-linux", "tiny").await?;

    let fod_out = test_store_path("ca-fod-out");
    if substitutable {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(fod_out.clone());
    }

    // Production shape: FOD ⇒ is_content_addressed + 32-byte modular
    // hash + known expected_output_path. NO realisation row in PG.
    let mut node = make_node("ca-fod");
    node.is_content_addressed = true;
    node.is_fixed_output = true;
    node.ca_modular_hash = [0x42u8; 32].to_vec();
    node.expected_output_paths = vec![fod_out.clone()];

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, expect_state as i32);
    assert_eq!(status.cached_derivations, expect_cached);

    if substitutable {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(qpi.contains(&fod_out), "path-based lane eager-fetches");
    } else {
        let assn = recv_assignment(&mut fetcher_rx).await;
        assert!(
            assn.drv_path.ends_with("ca-fod.drv"),
            "missing → dispatches"
        );
    }
    Ok(())
}

/// Negative case: CA node with a modular_hash that has NO realisation
/// row → cache-miss → proceeds to Ready (not Completed).
#[tokio::test]
async fn test_ca_cache_miss_no_realisation() -> TestResult {
    let test_db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(test_db.pool.clone());

    let mut node = make_node("ca-cache-miss");
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
/// Substitutable-probe matrix at merge time. A path NOT in the store
/// but reported as `substitutable_paths` by FindMissingPaths should
/// cache-hit (eager-fetch via QueryPathInfo, no dispatch). If QPI
/// fails, demote to miss. Missing-and-not-substitutable stays missing.
///
/// Before P0472: scheduler ignored `substitutable_paths` → dispatched
/// builds cache.nixos.org already had. Before P0473: marked
/// substitutable paths completed but never fetched → builder ENOENT on
/// FUSE access (FUSE GetPath carries no JWT so lazy fetch can't work).
#[rstest::rstest]
// substitutable + QPI ok → eager-fetch → Succeeded
#[case::hit(
    "hello-2.12.3",
    true,
    false,
    rio_proto::types::BuildState::Succeeded,
    true
)]
// substitutable + QPI fails → demote to miss → Active
#[case::fetch_fail("fetch-fails", true, true, rio_proto::types::BuildState::Active, false)]
// not substitutable → plain miss → Active (guards "all missing = substitutable")
#[case::missing(
    "truly-missing-out",
    false,
    false,
    rio_proto::types::BuildState::Active,
    false
)]
#[tokio::test]
async fn test_substitutable_probe_matrix(
    #[case] out_tag: &str,
    #[case] substitutable: bool,
    #[case] fail_qpi: bool,
    #[case] expect_state: rio_proto::types::BuildState,
    #[case] expect_qpi_called: bool,
) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out_path = test_store_path(out_tag);
    if substitutable {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out_path.clone());
    }
    if fail_qpi {
        store
            .faults
            .fail_query_path_info
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let mut node = make_node("sub-probe");
    node.expected_output_paths = vec![out_path.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, expect_state as i32, "build state");

    if expect_qpi_called {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            qpi.contains(&out_path),
            "scheduler should eager-fetch substitutable path via QueryPathInfo; qpi_calls={qpi:?}"
        );
    }
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let mut root = make_node("hello");
    root.expected_output_paths = vec![root_out.clone()];
    let mut glibc = make_node("glibc");
    glibc.expected_output_paths = vec![test_store_path("glibc-out")];
    let mut gcc = make_node("gcc");
    gcc.expected_output_paths = vec![test_store_path("gcc-out")];
    let mut stdenv = make_node("stdenv");
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Seed: dep output substitutable, root NOT. Top-down sees root
    // missing → falls through → bottom-up finds glibc substitutable.
    let glibc_out = test_store_path("glibc-fallthru");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(glibc_out.clone());

    let mut root = make_node("app");
    root.expected_output_paths = vec![test_store_path("app-out")];
    let mut glibc = make_node("glibc-ft");
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let mut hello = make_node("hello-a");
    hello.expected_output_paths = vec![hello_out.clone()];
    let mut glibc_a = make_node("glibc-a");
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
    let mut app = make_node("app-b");
    app.expected_output_paths = vec![test_store_path("app-b-out")];
    let mut glibc_b = make_node("glibc-a");
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

/// Store/substitution state of a pre-existing `Completed` output when
/// Build B re-merges it. See [`test_preexisting_completed_gc_matrix`].
enum GcState {
    /// Output GC'd, not substitutable → reset to Ready (I-047).
    Gone,
    /// Output GC'd but substitutable upstream → eager-fetch, stays
    /// Completed (I-202).
    Substitutable,
    /// Output GC'd, substitutable, but QPI fails → falls through to
    /// reset.
    SubFetchFail,
    /// FindMissingPaths itself fails → fail-open, stays Completed.
    StoreUnreachable,
}

// r[verify sched.merge.stale-completed-verify]
// r[verify sched.merge.stale-substitutable]
/// Pre-existing `Completed` node verification at merge time.
///
/// Common setup: Build A merges `app-a → fod-dep`, fod-dep completes
/// (`Completed` in DAG), app-a held Running so Build A stays Active and
/// fod-dep stays in the global DAG. Then mutate store state per `gc`
/// and merge Build B (`app-b → fod-dep`). The spare worker receives
/// either `fod-dep` (reset) or `app-b` (stayed Completed).
///
/// Production scenario (I-047): FOD outputs are content-addressed and
/// shared across builds. GC may delete a FOD output under one tenant's
/// retention while a later build's DAG still has the node `Completed`.
/// Without verify, the worker fails on `isValidPath` building the
/// dependent. I-202: but if upstream HAS it, eager-fetch instead of
/// re-dispatching the whole subtree (FOD sources may have dead URLs).
#[rstest::rstest]
// I-047: GC'd, not substitutable → reset → fod-dep re-dispatches, cached=0
#[case::gcd_resets(GcState::Gone, "fod-dep", 0)]
// I-202: GC'd but substitutable → eager-fetch → app-b dispatches, cached=1
#[case::substitutable_stays(GcState::Substitutable, "app-b", 1)]
// substitutable but QPI fails → falls through to reset
#[case::sub_fetch_fail_resets(GcState::SubFetchFail, "fod-dep", 0)]
// FindMissingPaths fails → fail-open → stays Completed, cached=1
#[case::store_unreachable_fail_open(GcState::StoreUnreachable, "app-b", 1)]
#[tokio::test]
async fn test_preexisting_completed_gc_matrix(
    #[case] gc: GcState,
    #[case] expect_spare_drv: &str,
    #[case] expect_cached: u32,
) -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut worker_rx = connect_executor(&handle, "w1", "x86_64-linux").await?;

    // Build A: app-a → fod-dep. fod-dep dispatches first (leaf).
    let fod_out = test_store_path("preexist-fod-out");
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("app-a"), make_node("fod-dep")],
        vec![make_test_edge("app-a", "fod-dep")],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut worker_rx).await;
    assert!(assn.drv_path.ends_with("fod-dep.drv"));
    store.seed_with_content(&fod_out, b"fod-contents");
    complete_success(&handle, "w1", &assn.drv_path, &fod_out).await?;
    barrier(&handle).await;

    // Hold app-a Running so Build A stays Active and fod-dep stays in DAG.
    let mut _w2 = connect_executor(&handle, "w2", "x86_64-linux").await?;
    let _ = recv_assignment(&mut _w2).await;

    // Mutate store per case.
    match gc {
        GcState::Gone => {
            store.state.paths.write().unwrap().remove(&fod_out);
        }
        GcState::Substitutable => {
            store.state.paths.write().unwrap().remove(&fod_out);
            store
                .state
                .substitutable
                .write()
                .unwrap()
                .push(fod_out.clone());
        }
        GcState::SubFetchFail => {
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
        }
        GcState::StoreUnreachable => {
            store.faults.fail_find_missing.store(true, Ordering::SeqCst);
        }
    }

    // Spare worker for Build B's dispatch.
    let mut spare = connect_executor(&handle, "spare", "x86_64-linux").await?;

    // Build B: app-b → fod-dep (pre-existing Completed).
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![make_node("app-b"), make_node("fod-dep")],
        vec![make_test_edge("app-b", "fod-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    if matches!(gc, GcState::Substitutable) {
        let qpi = store.calls.qpi_calls.read().unwrap().clone();
        assert!(
            qpi.contains(&fod_out),
            "stale-completed verify should eager-fetch substitutable output; qpi_calls={qpi:?}"
        );
    }

    let got = recv_assignment(&mut spare).await;
    assert!(
        got.drv_path.ends_with(&format!("{expect_spare_drv}.drv")),
        "spare worker should receive {expect_spare_drv}; got {}",
        got.drv_path
    );

    let status_b = query_status(&handle, build_b).await?;
    assert_eq!(
        status_b.cached_derivations, expect_cached,
        "cached_derivations for Build B"
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let path = test_store_path("reprobe-ready");
    let mut node = make_node("reprobe-ready");
    node.expected_output_paths = vec![path.clone()];

    // Build #1: path NOT substitutable → A is Ready (no deps, no cache).
    let build1 = Uuid::new_v4();
    merge_dag(&handle, build1, vec![node.clone()], vec![], false).await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, "reprobe-ready").await;
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

    let info = expect_drv(&handle, "reprobe-ready").await;
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
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let path = test_store_path("reprobe-poison");
    let mut node = make_node("reprobe-poison");
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
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;
    barrier(&handle).await;
    let info = expect_drv(&handle, "reprobe-poison").await;
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

    let info = expect_drv(&handle, "reprobe-poison").await;
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

/// I-169: `Poisoned` resubmit bound. Under `POISON_RESUBMIT_RETRY_LIMIT`,
/// resubmit resets to Ready (build #2 Active); at the limit, stays
/// Poisoned (build #2 fail-fasts).
///
/// Sensitivity: before the fix, build #2 sees A is Poisoned →
/// `first_dep_failed` set → fail-fasts. With the fix, A is reset in
/// `dag.merge` → in `newly_inserted` → skipped by pre-existing loop.
// r[verify sched.merge.poisoned-resubmit-bounded]
#[rstest::rstest]
#[case::under_limit(2, DerivationStatus::Ready, rio_proto::types::BuildState::Active)]
#[case::at_limit(
    crate::state::POISON_RESUBMIT_RETRY_LIMIT,
    DerivationStatus::Poisoned,
    rio_proto::types::BuildState::Failed
)]
#[tokio::test]
async fn test_resubmit_poisoned_retry_limit_bound(
    #[case] retry_count: u32,
    #[case] expect_status: DerivationStatus,
    #[case] expect_build_state: rio_proto::types::BuildState,
) -> TestResult {
    let (_db, handle, _task) = setup().await;
    let tag = "i169-resubmit";

    // Build #1: single node, force-poison at given retry_count.
    let node = make_node(tag);
    merge_dag(&handle, Uuid::new_v4(), vec![node.clone()], vec![], false).await?;
    assert!(handle.debug_force_poisoned(tag, retry_count).await?);
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Poisoned,
        "precondition"
    );

    // Build #2: resubmit.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = expect_drv(&handle, tag).await;
    assert_eq!(info.status, expect_status, "post-resubmit drv status");
    if expect_status == DerivationStatus::Ready {
        assert_eq!(
            info.retry.count, retry_count,
            "retry_count carried over so the bound accumulates"
        );
    }
    assert_eq!(
        query_status(&handle, build2).await?.state,
        expect_build_state as i32,
        "build #2 state"
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
        .map(|i| rio_proto::types::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_node("x")
        })
        .collect();
    let mut edges = Vec::with_capacity(N * FANOUT);
    for i in FANOUT..N {
        for j in 1..=FANOUT {
            edges.push(rio_proto::types::DerivationEdge {
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
        .map(|i| rio_proto::types::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_node("x")
        })
        .collect();
    let mut edges = Vec::with_capacity(N * FANOUT);
    for i in FANOUT..N {
        for j in 1..=FANOUT {
            edges.push(rio_proto::types::DerivationEdge {
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
    // compute_cluster_snapshot iterates the full DAG. With 50k nodes
    // this should be tens-of-ms, not the 30s+ timeout seen in prod.
    // Tick recomputes + publishes the snapshot; the cached read is
    // O(1), so the elapsed measures the Tick's actor-side scan.
    let t = std::time::Instant::now();
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    let _snap = handle.cluster_snapshot_cached();
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
        .map(|i| rio_proto::types::DerivationNode {
            drv_hash: format!("h{i:08}"),
            drv_path: path(i),
            ..make_node("x")
        })
        .collect();
    let edges: Vec<_> = (W..N)
        .map(|i| rio_proto::types::DerivationEdge {
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
    // Connects past BECAME_IDLE_INLINE_CAP coalesce to dispatch_dirty
    // — drain via one Tick (one dispatch_ready instead of W; tighter
    // than the pre-cap behavior this test bounded).
    handle.send_unchecked(ActorCommand::Tick).await?;
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
        c.fetcher_size_classes = vec!["tiny".into(), "small".into()];
    });

    // Pre-seed: prior run promoted this FOD to floor='small', then went
    // terminal. New build re-merges it; ON CONFLICT RETURNING must
    // bring the floor back into the freshly-constructed in-memory state.
    let mut fod = make_node("i208-fod");
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
        .query_unchecked(|reply| {
            ActorCommand::Admin(AdminQuery::GetSizeClassSnapshot {
                pool_features: None,
                reply,
            })
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

/// A pre-existing Ready derivation that gains interest from a
/// higher-priority build is re-pushed onto the ready queue with its
/// raised priority. Regression: handle_merge_dag's compute_initial only
/// walks newly_inserted; a pre-existing Ready node already sat in the
/// queue under its OLD priority and would dispatch behind lower-
/// priority work even though an Interactive build now wants it.
#[tokio::test]
async fn merge_reheaps_preexisting_ready_on_priority_raise() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Build 1 (Scheduled): two independent nodes. No worker connected
    // so both stay Ready in the queue. "low" gets a longer
    // est_duration via input_srcs_nar_size proxy so its base
    // critical-path priority is higher than "shared" — under build 1
    // alone, "low" would dispatch first.
    let mut shared = make_node("shared");
    shared.input_srcs_nar_size = 1;
    let mut low = make_node("low");
    low.input_srcs_nar_size = 1_000_000_000; // higher closure-size proxy
    let _ev1 = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![shared.clone(), low],
        vec![],
        false,
    )
    .await?;

    // Build 2 (Interactive): references ONLY "shared". This adds
    // interest to a pre-existing Ready node. Interactive boost
    // (queue::INTERACTIVE_BOOST) should now raise "shared" above
    // "low".
    let _ev2 = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: Uuid::new_v4(),
            tenant_id: None,
            priority_class: PriorityClass::Interactive,
            nodes: vec![shared],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // Connect a worker → first dispatch goes to "shared" (Interactive
    // boost) not "low" (higher base priority).
    let mut rx = connect_executor(&handle, "reheap-w", "x86_64-linux").await?;
    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.drv_path,
        test_drv_path("shared"),
        "pre-existing Ready node re-heaped with Interactive boost on merge"
    );
    Ok(())
}
