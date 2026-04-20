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

    // Build 1: Scheduled. shared-x is a leaf; filler-y → filler-y-dep
    // gives filler-y critical-path = 2×DEFAULT_DURATION_SECS vs
    // shared-x's 1×, so WITHOUT the Interactive boost filler-y-dep and
    // filler-y deterministically outrank shared-x. (Two equal-priority
    // leaves would tiebreak on seq from compute_initial_states
    // iterating a HashSet — ~50% false-pass on regression.) No worker
    // connected yet, so all push into the ready queue and stay.
    let build_lo = Uuid::new_v4();
    merge_dag(
        &handle,
        build_lo,
        vec![
            make_node("shared-x"),
            make_node("filler-y"),
            make_node("filler-y-dep"),
        ],
        vec![make_test_edge("filler-y", "filler-y-dep")],
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
    // (via build_hi's interest), filler-y/filler-y-dep do not. Without
    // the bump, filler-y-dep (critical-path 2×) deterministically pops
    // first; with the +1e9 boost shared-x wins (dominates any
    // critical-path base — a 100k-node chain at 1h each is 3.6e8; 1e9
    // still wins).
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
            && d.kind() == rio_proto::types::DerivationEventKind::Cached
        {
            break d.output_paths;
        }
    };
    assert_eq!(
        cached_paths,
        vec![realized_path],
        "cache-hit must report the REALIZED path, not the \"\" placeholder"
    );

    Ok(())
}

// r[verify sched.merge.stale-completed-verify+3]
/// CA realisation cache-check: realisation row in PG ± path in store.
///
/// - **stale** (I-048): realisation row exists but path GC'd from store
///   → FindMissingPaths reports missing → filtered → Active. Without
///   verify, would flip Completed and ping-pong against I-047's reset.
/// - **miss**: no realisation row at all → Active.
#[rstest::rstest]
#[case::stale_realisation(true)]
#[case::no_realisation(false)]
#[tokio::test]
async fn test_ca_cache_miss(#[case] seed_stale: bool) -> TestResult {
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    let modular_hash = [0x55u8; 32];
    if seed_stale {
        // Realisation exists but path NOT in MockStore.paths (GC'd).
        crate::ca::insert_realisation(
            &db.pool,
            &modular_hash,
            "out",
            &test_store_path("ca-gcd-out"),
            &[0x22u8; 32],
        )
        .await?;
    }

    let mut node = make_node("ca-miss");
    node.is_content_addressed = true;
    node.ca_modular_hash = modular_hash.to_vec();
    node.expected_output_paths = vec![String::new()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "must NOT cache-hit (seed_stale={seed_stale})"
    );
    assert_eq!(status.cached_derivations, 0);
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
    let mut fetcher_rx = connect_executor_kind(
        &handle,
        "f-ca-fod",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;

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
    // r[sched.substitute.detached]: substitutable lane spawns the fetch;
    // SubstituteComplete arrives via mailbox. barrier() alone races it.
    if substitutable {
        settle_substituting(&handle, &["ca-fod"]).await;
    } else {
        barrier(&handle).await;
    }

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
            .fail_query_path_info_permanent
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let mut node = make_node("sub-probe");
    node.expected_output_paths = vec![out_path.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    // r[sched.substitute.detached]: substitutable lane spawns the fetch;
    // settle for the spawned task to post SubstituteComplete. The
    // not-substitutable case never enters Substituting → bare barrier.
    if substitutable {
        settle_substituting(&handle, &["sub-probe"]).await;
    } else {
        barrier(&handle).await;
    }

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

/// Transient `Unavailable` from QPI is absorbed by the retry loop:
/// 2 transient failures → 3rd attempt succeeds → SubstituteComplete
/// `{ok=true}` → build Succeeded. Guards `SUBSTITUTE_FETCH_BACKOFF`
/// wiring + `is_transient` arm at dispatch.rs.
#[tokio::test]
async fn test_substitute_fetch_transient_retry() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("transient-retry");
    store.state.substitutable.write().unwrap().push(out.clone());
    store
        .faults
        .fail_query_path_info_n_times
        .store(2, std::sync::atomic::Ordering::SeqCst);

    let mut node = make_node("transient-retry-drv");
    node.expected_output_paths = vec![out.clone()];
    let drv_hash = node.drv_hash.clone();
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;

    // 2 transient failures × backoff(0..1) = 250+500ms before the
    // 3rd attempt succeeds. Real-time wait — start_paused would
    // also pause the ephemeral-PG actor setup.
    tokio::time::sleep(std::time::Duration::from_millis(1500)).await;
    settle_substituting(&handle, &[&drv_hash]).await;

    let remaining = store
        .faults
        .fail_query_path_info_n_times
        .load(std::sync::atomic::Ordering::SeqCst);
    assert_eq!(
        remaining, 0,
        "retry loop should consume both transient faults"
    );
    assert!(
        store.calls.qpi_calls.read().unwrap().contains(&out),
        "3rd attempt (success) should record qpi_calls"
    );
    let st = handle
        .debug_query_derivation(&drv_hash)
        .await?
        .expect("drv exists");
    assert_eq!(
        st.status,
        crate::state::DerivationStatus::Completed,
        "transient failures absorbed by retry → Completed (not demoted to Ready)"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+3]
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
    // r[sched.substitute.detached]: top-down no longer awaits QPI inline;
    // the pruned root goes through pending_substitute → spawned fetch
    // → SubstituteComplete via mailbox. settle_substituting waits for
    // that round-trip; the inline-QPI code is deleted so the actor
    // cannot have blocked on the closure walk.
    settle_substituting(&handle, &["hello"]).await;

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

// r[verify sched.merge.substitute-topdown+3]
/// Top-down + deferred-fetch failure: when the prune commits and the
/// detached `query_path_info` then fails, the build MUST fail with a
/// resubmit-directing error — NOT dispatch the root as a build.
///
/// Before the fix: `handle_substitute_complete{ok=false}` set
/// `substitute_tried=true`, computed `all_deps_completed(R)` = true
/// (vacuous — deps were pruned), pushed R Ready, and the next
/// dispatch pass routed R to a worker. Worker walks `inputDrvs`,
/// finds none in store → ENOENT → Failed → retry → Poisoned.
#[tokio::test]
async fn test_topdown_pruned_root_substitute_fail_does_not_dispatch_build() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Seed: root output substitutable (so topdown FMP says "all
    // available" and prune fires). QPI then fails permanently
    // (Internal — non-transient → ok=false on first try).
    let root_out = test_store_path("td-fail-hello");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    store
        .faults
        .fail_query_path_info_permanent
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // hello (root) → glibc, gcc, stdenv. Deps NOT seeded.
    let mut root = make_node("td-fail-hello");
    root.expected_output_paths = vec![root_out.clone()];
    let mut glibc = make_node("td-fail-glibc");
    glibc.expected_output_paths = vec![test_store_path("td-fail-glibc-out")];
    let mut gcc = make_node("td-fail-gcc");
    gcc.expected_output_paths = vec![test_store_path("td-fail-gcc-out")];
    let mut stdenv = make_node("td-fail-stdenv");
    stdenv.expected_output_paths = vec![test_store_path("td-fail-stdenv-out")];

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![root, glibc, gcc, stdenv],
        vec![
            make_test_edge("td-fail-hello", "td-fail-glibc"),
            make_test_edge("td-fail-hello", "td-fail-gcc"),
            make_test_edge("td-fail-hello", "td-fail-stdenv"),
        ],
        false,
    )
    .await?;
    settle_substituting(&handle, &["td-fail-hello"]).await;
    barrier(&handle).await;

    // Root was stamped topdown_pruned, then SubstituteComplete{ok=
    // false} → build Failed (not Active, not Succeeded).
    let r = expect_drv(&handle, "td-fail-hello").await;
    assert!(
        r.topdown_pruned,
        "topdown prune fired → root stamped topdown_pruned"
    );
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "topdown-pruned root with failed substitute MUST NOT be \
         dispatched/Ready (deps were dropped); got {:?}",
        r.status
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "topdown-pruned root + ok=false → build Failed (was: stayed Active \
         or root dispatched as build → ENOENT → Poisoned)"
    );
    assert!(
        status.error_summary.contains("topdown") && status.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        status.error_summary
    );
    // Deps never entered the DAG (prune fired).
    for dep in ["td-fail-glibc", "td-fail-gcc", "td-fail-stdenv"] {
        assert!(
            handle.debug_query_derivation(dep).await?.is_none(),
            "dep {dep} should be pruned, not in global DAG"
        );
    }
    // total_derivations = 1 confirms prune committed (not 4).
    assert_eq!(status.total_derivations, 1);
    Ok(())
}

// r[verify sched.merge.substitute-topdown+3]
/// `topdown_pruned` flag persistence bypass: B1 topdown-prunes R; while
/// R's fetch is in-flight, B2 full-merges R WITH its deps. R is
/// pre-existing `Substituting` so `dag.merge` doesn't reset it; the
/// `topdown_pruned` flag persists. Fetch then fails. Before the fix:
/// `handle_substitute_complete` saw `topdown_pruned=true` and failed
/// EVERY interested build — including B2, whose deps ARE in the DAG.
/// After: gate on `get_children(R).is_empty()`; R has children → flag
/// cleared, fall through to normal Queued handling.
///
/// Race staged deterministically via `debug_force_status`/
/// `debug_set_topdown_pruned` + injected `SubstituteComplete{ok=false}`
/// (see `r[sched.substitute.detached]` — the actor only checks `status
/// == Substituting`, so an injected message is indistinguishable from
/// the spawned task's).
#[tokio::test]
async fn test_topdown_pruned_flag_ignored_after_full_merge_adds_deps() -> TestResult {
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // B2: full-merge {app, R, glibc} with app→R, R→glibc. None
    // substitutable → topdown falls through. R newly_inserted with
    // child glibc. (B1's topdown-prune-then-B2-adds-deps end state is
    // identical to this; staging it directly avoids the spawn-task
    // race.)
    let mut app = make_node("tdp-app");
    app.expected_output_paths = vec![test_store_path("tdp-app-out")];
    let mut r = make_node("tdp-r");
    r.expected_output_paths = vec![test_store_path("tdp-r-out")];
    let mut glibc = make_node("tdp-glibc");
    glibc.expected_output_paths = vec![test_store_path("tdp-glibc-out")];

    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![app, r, glibc],
        vec![
            make_test_edge("tdp-app", "tdp-r"),
            make_test_edge("tdp-r", "tdp-glibc"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Stage: R was topdown-pruned by an earlier build (B1) and is
    // mid-fetch when B2 merged. R has child glibc (B2's full merge
    // added it). topdown_pruned persists from B1.
    handle
        .debug_force_status("tdp-r", DerivationStatus::Substituting)
        .await?;
    handle.debug_set_topdown_pruned("tdp-r", true).await?;
    let pre = expect_drv(&handle, "tdp-r").await;
    assert!(pre.topdown_pruned, "precondition: flag set");
    assert_eq!(pre.status, DerivationStatus::Substituting);

    // B1's deferred fetch fails → SubstituteComplete{ok=false}.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdp-r".into(),
            ok: false,
        })
        .await?;
    barrier(&handle).await;

    // B2 MUST stay Active: R has children (glibc) → "deps were
    // dropped" invariant doesn't hold → no fail-fast. R falls through
    // to Queued (glibc not Completed → all_deps_completed=false).
    let s2 = query_status(&handle, b2).await?;
    assert_eq!(
        s2.state,
        rio_proto::types::BuildState::Active as i32,
        "B2 full-merged R with deps → R has DAG children → topdown \
         fail-fast must NOT fire (was: collaterally Failed via stale \
         topdown_pruned flag)"
    );
    let post = expect_drv(&handle, "tdp-r").await;
    assert_eq!(
        post.status,
        DerivationStatus::Queued,
        "R falls through to normal Substituting→Queued (deps not done)"
    );
    assert!(
        !post.topdown_pruned,
        "flag cleared once R gained children (invariant moot)"
    );
    assert!(post.substitute_tried, "one-shot fall-through still applies");
    Ok(())
}

// r[verify sched.merge.substitute-topdown+3]
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
    // r[sched.substitute.detached]: the bottom-up fetch is spawned; let
    // SubstituteComplete land before checking qpi_calls.
    settle_substituting(&handle, &["glibc-ft"]).await;
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&glibc_out),
        "bottom-up should fetch substitutable dep on fall-through; qpi_calls={qpi:?}"
    );

    Ok(())
}

// r[verify sched.substitute.detached]
/// Substitutable nodes go `Substituting` (detached fetch spawned),
/// not synchronously `Completed` at merge. The closure-invariant
/// gate (output references ⊆ inputDrv outputs) is now enforced by
/// the store-side `ensure_references` walk, not by the scheduler's
/// apply_cached_hits fixed-point: a `SubstituteComplete{ok=true}`
/// means the full reference closure IS in store. Second half: when
/// BOTH wrapper2 and rustc2 are substitutable, both spawn → both
/// complete → build2 succeeds.
#[tokio::test]
async fn test_cache_hit_gates_on_inputdrv_completion() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // git → wrapper → rustc. Only wrapper's output is substitutable.
    let wrapper_out = test_store_path("wrapper-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(wrapper_out.clone());

    let mut git = make_test_node("git", "x86_64-linux");
    git.expected_output_paths = vec![test_store_path("git-out")];
    let mut wrapper = make_test_node("wrapper", "x86_64-linux");
    wrapper.expected_output_paths = vec![wrapper_out];
    let mut rustc = make_test_node("rustc", "x86_64-linux");
    rustc.expected_output_paths = vec![test_store_path("rustc-out")];

    let wrapper_hash = wrapper.drv_hash.clone();
    let git_hash = git.drv_hash.clone();
    let rustc_hash = rustc.drv_hash.clone();

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![git, wrapper, rustc],
        vec![
            make_test_edge("git", "wrapper"),
            make_test_edge("wrapper", "rustc"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    // r[sched.substitute.detached] — substitutable nodes go to
    // Substituting (detached fetch) instead of cached_hits, so the
    // closure gate is enforced store-side (`ensure_references`), not
    // by the apply_cached_hits fixed-point. The mock store doesn't
    // simulate closure-walk failure, so wrapper completes once the
    // spawned task lands; assert the detached path WAS taken (wrapper
    // was never synchronously Completed at merge — it's now
    // Substituting or, if the spawn already settled, Completed).
    settle_substituting(&handle, &[&wrapper_hash]).await;
    let w = handle
        .debug_query_derivation(&wrapper_hash)
        .await?
        .expect("wrapper in DAG");
    assert_eq!(
        w.status,
        DerivationStatus::Completed,
        "substitutable wrapper completed via detached fetch"
    );
    let r = handle
        .debug_query_derivation(&rustc_hash)
        .await?
        .expect("rustc in DAG");
    assert_eq!(r.status, DerivationStatus::Ready, "rustc has no deps");
    // git was promoted to Ready by wrapper's SubstituteComplete →
    // promote_newly_ready (git's only child wrapper is now Completed).
    let g = handle
        .debug_query_derivation(&git_hash)
        .await?
        .expect("git in DAG");
    assert_eq!(g.status, DerivationStatus::Ready);

    // Fixed-point: when BOTH wrapper2 and rustc2 are substitutable,
    // the worklist re-walk completes the chain in one merge pass.
    let wrapper2_out = test_store_path("wrapper2-out");
    let rustc2_out = test_store_path("rustc2-out");
    {
        let mut sub = store.state.substitutable.write().unwrap();
        sub.push(wrapper2_out.clone());
        sub.push(rustc2_out.clone());
    }
    let mut wrapper2 = make_test_node("wrapper2", "x86_64-linux");
    wrapper2.expected_output_paths = vec![wrapper2_out];
    let mut rustc2 = make_test_node("rustc2", "x86_64-linux");
    rustc2.expected_output_paths = vec![rustc2_out];
    let build2 = Uuid::new_v4();
    merge_dag(
        &handle,
        build2,
        vec![wrapper2, rustc2],
        vec![make_test_edge("wrapper2", "rustc2")],
        false,
    )
    .await?;
    barrier(&handle).await;
    let w2_hash = make_node("wrapper2").drv_hash;
    let r2_hash = make_node("rustc2").drv_hash;
    settle_substituting(&handle, &[&w2_hash, &r2_hash]).await;
    let status2 = query_status(&handle, build2).await?;
    assert_eq!(
        status2.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "both substitutable → detached fetch completes the chain"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+3]
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
    settle_substituting(&handle, &["hello-a"]).await;

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
    settle_substituting(&handle, &["glibc-a"]).await;

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
    /// Output GC'd but substitutable upstream → detached fetch spawned
    /// (Completed→Ready→Substituting), comes back to Completed (I-202).
    Substitutable,
    /// Output GC'd, substitutable, but QPI fails → SubstituteComplete
    /// {ok=false} → reverts to Ready, re-dispatches.
    SubFetchFail,
    /// FindMissingPaths itself fails → fail-open, stays Completed.
    StoreUnreachable,
}

// r[verify sched.merge.stale-completed-verify+3]
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
                .fail_query_path_info_permanent
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

    // r[sched.substitute.detached] — the fetch is spawned, not awaited.
    // Let the spawned task post SubstituteComplete before checking.
    if matches!(gc, GcState::Substitutable | GcState::SubFetchFail) {
        let fod_hash = make_node("fod-dep").drv_hash;
        settle_substituting(&handle, &[&fod_hash]).await;
        // SubstituteComplete{ok=true} → Completed → app-b Ready, OR
        // {ok=false} → Ready → fod-dep dispatches. Either way the
        // spare worker gets an assignment now; tick to drain dirty.
        tick(&handle).await?;
    }

    if matches!(gc, GcState::Substitutable) {
        let qpi = store.calls.qpi_calls.read().unwrap().clone();
        assert!(
            qpi.contains(&fod_out),
            "stale-completed verify should fetch substitutable output (detached); qpi_calls={qpi:?}"
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
    settle_substituting(&handle, &["reprobe-ready"]).await;

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
    settle_substituting(&handle, &["reprobe-poison"]).await;

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
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[rstest::rstest]
#[case::under_limit(1, DerivationStatus::Ready, rio_proto::types::BuildState::Active)]
#[case::at_limit(
    crate::state::POISON_RESUBMIT_RETRY_LIMIT,
    DerivationStatus::Poisoned,
    rio_proto::types::BuildState::Failed
)]
#[tokio::test]
async fn test_resubmit_poisoned_retry_limit_bound(
    #[case] resubmit_cycles: u32,
    #[case] expect_status: DerivationStatus,
    #[case] expect_build_state: rio_proto::types::BuildState,
) -> TestResult {
    let (_db, handle, _task) = setup().await;
    let tag = "i169-resubmit";

    // Build #1: single node, force-poison at given resubmit_cycles.
    let node = make_node(tag);
    merge_dag(&handle, Uuid::new_v4(), vec![node.clone()], vec![], false).await?;
    assert!(handle.debug_force_poisoned(tag, resubmit_cycles).await?);
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
            info.retry.resubmit_cycles,
            resubmit_cycles + 1,
            "resubmit_cycles incremented so the bound accumulates"
        );
        assert_eq!(
            info.retry.count, 0,
            "per-cycle retry budget reset on resubmit"
        );
    }
    assert_eq!(
        query_status(&handle, build2).await?.state,
        expect_build_state as i32,
        "build #2 state"
    );
    Ok(())
}

// r[verify sched.merge.poisoned-resubmit-bounded+2]
// r[verify sched.substitute.detached]
/// I-094 substitutable lane: a `Poisoned` node at the resubmit limit
/// whose output is upstream-substitutable (NOT locally present) on
/// resubmit must transition `Poisoned → Substituting → Completed` and
/// the build must succeed. Before the fix, `(Poisoned, Substituting)`
/// was rejected → node stayed Poisoned → `reconcile_preexisting`
/// fail-fasted the build. The locally-present case (routed via
/// `cached_hits` → `Poisoned → Completed`) already worked; kept here
/// as a regression-guard so both lanes stay aligned.
#[rstest::rstest]
#[case::substitutable_upstream(false)]
#[case::locally_present(true)]
#[tokio::test]
async fn test_resubmit_poisoned_at_limit_substitutable(
    #[case] locally_present: bool,
) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let tag = "i094-sub-poison";
    let out = test_store_path("i094-sub-poison-out");
    let mut node = make_node(tag);
    node.expected_output_paths = vec![out.clone()];

    // Build #1: merge, then force-poison at the limit so resubmit
    // does NOT reset (`is_retriable_on_resubmit() == false`).
    merge_dag(&handle, Uuid::new_v4(), vec![node.clone()], vec![], false).await?;
    assert!(
        handle
            .debug_force_poisoned(tag, crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Poisoned,
        "precondition"
    );

    // Output appears: either upstream cache (substitutable) or in-store.
    if locally_present {
        store.seed_with_content(&out, b"i094-contents");
    } else {
        store.state.substitutable.write().unwrap().push(out.clone());
    }

    // Build #2: resubmit. Single-node → topdown short-circuit doesn't
    // apply; goes through existing_reprobe → check_cached_outputs.
    let build2 = Uuid::new_v4();
    merge_dag(&handle, build2, vec![node], vec![], false).await?;
    if !locally_present {
        settle_substituting(&handle, &[tag]).await;
    }
    barrier(&handle).await;

    let info = expect_drv(&handle, tag).await;
    assert_eq!(
        info.status,
        DerivationStatus::Completed,
        "Poisoned → {} → Completed",
        if locally_present {
            "Completed"
        } else {
            "Substituting"
        }
    );
    assert_eq!(info.retry.resubmit_cycles, 0, "poison retry cleared");
    assert_eq!(
        query_status(&handle, build2).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build #2 should succeed via re-probe"
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
                stream_epoch: stream_epoch_for(&format!("w{w}")),
                seen_drvs: vec![],
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

/// I-208 (D4 form): a FOD whose DB row pre-exists with
/// `floor_mem_bytes=8GiB` (promoted by a prior run's failures, then
/// the build terminated and the node left memory) MUST come back at
/// floor.mem=8GiB when re-merged. Regression: `try_from_node` set
/// `floor=zeros` and the upsert's RETURNING didn't carry the floor
/// columns, so the next SpawnIntent was probe-default and re-OOM'd.
#[tokio::test]
async fn merge_hydrates_resource_floor_from_db() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, _| {});

    // Pre-seed: prior run promoted this FOD to floor.mem=8GiB, then
    // went terminal. New build re-merges it; ON CONFLICT RETURNING
    // must bring the floor back into the freshly-constructed in-memory
    // state.
    let mut fod = make_node("i208-fod");
    fod.is_fixed_output = true;
    fod.expected_output_paths = vec![test_store_path("i208-out")];
    sqlx::query(
        "INSERT INTO derivations
             (drv_hash, drv_path, system, status, is_fixed_output,
              floor_mem_bytes, floor_disk_bytes, floor_deadline_secs,
              expected_output_paths, output_names)
         VALUES ($1, $2, 'x86_64-linux', 'completed', true,
                 8589934592, 0, 0,
                 ARRAY[$3], ARRAY['out'])",
    )
    .bind(&fod.drv_hash)
    .bind(&fod.drv_path)
    .bind(&fod.expected_output_paths[0])
    .execute(&db.pool)
    .await?;

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![fod], vec![], false).await?;

    let info = expect_drv(&handle, "i208-fod").await;
    assert_eq!(
        info.sched.resource_floor.mem_bytes,
        8 << 30,
        "I-208: floor_mem_bytes from DB hydrated onto re-merged node"
    );
    Ok(())
}

// (merge_reheaps_preexisting_ready_on_priority_raise removed —
// duplicate of test_shared_node_priority_bumps_on_higher_pri_merge
// with a non-deterministic baseline; that test now carries the
// deterministic baseline + r[verify sched.merge.shared-priority-max].)

// ===========================================================================
// C3/C4/C5 regressions: stale-reset dep-gating, re-probe fan-out,
// deferred re-probe on Poisoned-at-limit
// ===========================================================================

// r[verify sched.merge.stale-completed-verify+3]
/// I-047 dep-gating: when GC sweeps a chain {A→B}, both reset; A goes
/// to `Queued` (NOT `Ready`) so it cannot dispatch ahead of B. Without
/// the two-pass reset, A and B both reset to `Ready` and A can dispatch
/// while B is still Ready/Substituting → worker ENOENT on B's output.
#[tokio::test]
async fn test_stale_reset_chain_gates_parent_at_queued() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut w1 = connect_executor(&handle, "sr-w1", "x86_64-linux").await?;

    // Build 1: C → A → B. Complete B (worker, real output_paths), then
    // force A to Completed with output_paths set (avoids the one-shot-
    // worker dance for a 3-level chain). Hold C so build 1 stays Active
    // and A/B stay in DAG.
    let a_out = test_store_path("sr-a-out");
    let b_out = test_store_path("sr-b-out");
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("sr-c"), make_node("sr-a"), make_node("sr-b")],
        vec![
            make_test_edge("sr-c", "sr-a"),
            make_test_edge("sr-a", "sr-b"),
        ],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut w1).await;
    assert!(assn.drv_path.ends_with("sr-b.drv"));
    store.seed_with_content(&b_out, b"b");
    complete_success(&handle, "sr-w1", &assn.drv_path, &b_out).await?;
    barrier(&handle).await;
    // A is now Ready (B completed). Force it to Completed with
    // output_paths so it's a verify_preexisting_completed candidate.
    store.seed_with_content(&a_out, b"a");
    handle
        .debug_force_status("sr-a", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("sr-a", vec![a_out.clone()])
        .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "sr-a").await.status,
        DerivationStatus::Completed
    );
    assert_eq!(
        expect_drv(&handle, "sr-b").await.status,
        DerivationStatus::Completed
    );

    // GC both A and B's outputs (NOT substitutable).
    store.state.paths.write().unwrap().remove(&a_out);
    store.state.paths.write().unwrap().remove(&b_out);

    // Build 2 references C, A, B (all pre-existing). Stale-verify finds
    // both outputs gone → two-pass: B→Ready (leaf), A→Queued (dep B is
    // in reset_set).
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("sr-c"), make_node("sr-a"), make_node("sr-b")],
        vec![
            make_test_edge("sr-c", "sr-a"),
            make_test_edge("sr-a", "sr-b"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    let a = expect_drv(&handle, "sr-a").await;
    let b = expect_drv(&handle, "sr-b").await;
    assert_eq!(
        b.status,
        DerivationStatus::Ready,
        "leaf B (no deps in reset_set) → Ready"
    );
    assert_eq!(
        a.status,
        DerivationStatus::Queued,
        "A's dep B was also reset → A gates at Queued (NOT Ready); was: A \
         reset to Ready, could dispatch ahead of B → worker ENOENT"
    );
    assert!(a.output_paths.is_empty(), "reset clears output_paths");
    Ok(())
}

// r[verify sched.merge.stale-completed-verify+3]
/// I-047 covers `Skipped` too: a pre-existing `Skipped` node with GC'd
/// output_paths resets the same as `Completed`. Skipped carries real
/// output_paths and unlocks dependents via `all_deps_completed`; before
/// the fix, the candidate filter skipped Skipped → dependents unlocked
/// against a gone output.
#[tokio::test]
async fn test_stale_skipped_output_reset() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Build 1: app → dep. Complete dep so it's Completed with output_paths.
    let dep_out = test_store_path("sk-dep-out");
    let mut w1 = connect_executor(&handle, "sk-w1", "x86_64-linux").await?;
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("sk-app"), make_node("sk-dep")],
        vec![make_test_edge("sk-app", "sk-dep")],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut w1).await;
    assert!(assn.drv_path.ends_with("sk-dep.drv"));
    store.seed_with_content(&dep_out, b"d");
    complete_success(&handle, "sk-w1", &assn.drv_path, &dep_out).await?;
    let mut w2 = connect_executor(&handle, "sk-w2", "x86_64-linux").await?;
    let _hold_app = recv_assignment(&mut w2).await;
    barrier(&handle).await;

    // Force dep to Skipped (CA-cutoff equivalent). The transition table
    // doesn't allow Completed→Skipped directly; debug_force_status sets
    // it without validation (test-only).
    handle
        .debug_force_status("sk-dep", DerivationStatus::Skipped)
        .await?;
    let pre = expect_drv(&handle, "sk-dep").await;
    assert_eq!(pre.status, DerivationStatus::Skipped, "precondition");
    assert!(!pre.output_paths.is_empty(), "Skipped carries output_paths");

    // GC dep's output.
    store.state.paths.write().unwrap().remove(&dep_out);

    // Build 2: app2 → dep (pre-existing Skipped). Stale-verify must
    // reset dep.
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("sk-app2"), make_node("sk-dep")],
        vec![make_test_edge("sk-app2", "sk-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let dep = expect_drv(&handle, "sk-dep").await;
    assert!(
        matches!(
            dep.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "GC'd Skipped output → reset (Ready/Queued); was: filter skipped \
         Skipped, status stayed Skipped → app2 unlocked against gone output. \
         got {:?}",
        dep.status
    );
    assert!(
        dep.output_paths.is_empty(),
        "reset clears output_paths; got {:?}",
        dep.output_paths
    );
    Ok(())
}

// r[verify sched.merge.dedup]
/// Re-probe completion fan-out: B1 merges {X} (Ready, no worker). X's
/// output is then seeded locally. B2 merges {X}: re-probe finds X in
/// store → X transitions →Completed. B1 must ALSO be notified
/// (update_build_counts + check_build_completion) — B1 Succeeds. Before
/// the fix: B1 stayed Active, completed_count=0, hung until failover.
#[tokio::test]
async fn test_reprobe_completion_fans_out_to_earlier_build() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let x_out = test_store_path("fanout-x-out");
    let mut x = make_node("fanout-x");
    x.expected_output_paths = vec![x_out.clone()];

    // B1: X not in store → Ready (no worker connected).
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![x.clone()], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "fanout-x").await.status,
        DerivationStatus::Ready
    );

    // X's output now locally present (NOT substitutable: cached_hits
    // lane, not pending_substitute lane).
    store.seed_with_content(&x_out, b"x");

    // B2: re-probe X (existing Ready, in existing_reprobe) → cached_hits.
    let b2 = Uuid::new_v4();
    merge_dag(&handle, b2, vec![x], vec![], false).await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "fanout-x").await.status,
        DerivationStatus::Completed,
        "re-probe found X locally → Completed"
    );
    let s2 = query_status(&handle, b2).await?;
    assert_eq!(s2.state, rio_proto::types::BuildState::Succeeded as i32);
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "r[sched.merge.dedup]: re-probe completion of shared X must fan out \
         to B1 (was: B1 stayed Active, completed_count=0, hung)"
    );
    Ok(())
}

/// Re-probe chain both-cached: pre-existing {X→Y} both Queued; X.out
/// and Y.out then seeded locally. Build B merges {X,Y}: re-probe
/// fixed-point completes Y then X. The post-loop `reprobe_unlocked`
/// handler captured X (find_newly_ready(Y) saw X Queued) — without the
/// explicit Queued re-check it would reset X Completed→Ready via the
/// I-047 carve-out and push_ready it.
#[tokio::test]
async fn test_reprobe_chain_both_cached_no_ready_reset() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut rx = connect_executor(&handle, "rc-w", "x86_64-linux").await?;

    let x_out = test_store_path("rc-x-out");
    let y_out = test_store_path("rc-y-out");
    let mut x = make_node("rc-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut y = make_node("rc-y");
    y.expected_output_paths = vec![y_out.clone()];
    let mut ydep = make_node("rc-ydep");
    ydep.expected_output_paths = vec![test_store_path("rc-ydep-out")];

    // Build A: X→Y→ydep. ydep dispatches; complete it so Y is Queued→
    // Ready; X stays Queued (Y not yet completed). Actually we want
    // both X and Y in pre-dispatch states for the re-probe set: ydep
    // assigned but NOT completed → Y stays Queued, X stays Queued.
    let ba = Uuid::new_v4();
    merge_dag(
        &handle,
        ba,
        vec![x.clone(), y.clone(), ydep.clone()],
        vec![
            make_test_edge("rc-x", "rc-y"),
            make_test_edge("rc-y", "rc-ydep"),
        ],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut rx).await;
    assert!(assn.drv_path.ends_with("rc-ydep.drv"));
    // Complete ydep → rc-w drains (one-shot). Y promotes to Ready but
    // stays unassigned (no idle worker). X stays Queued (Y not
    // Completed). Both Y(Ready) and X(Queued) are in existing_reprobe
    // for build B.
    store.seed_with_content(&test_store_path("rc-ydep-out"), b"yd");
    complete_success(
        &handle,
        "rc-w",
        &assn.drv_path,
        &test_store_path("rc-ydep-out"),
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "rc-y").await.status,
        DerivationStatus::Ready
    );
    assert_eq!(
        expect_drv(&handle, "rc-x").await.status,
        DerivationStatus::Queued
    );

    // Seed X.out and Y.out locally → both in cached_hits on build B.
    store.seed_with_content(&x_out, b"x");
    store.seed_with_content(&y_out, b"y");

    // Uncached sibling root Z so check_roots_topdown's all-or-nothing
    // falls through (X.out is locally present; without Z the prune
    // would reduce build B to {X} only and X would defer on Y).
    let mut z = make_node("rc-z");
    z.expected_output_paths = vec![test_store_path("rc-z-out")];

    let bb = Uuid::new_v4();
    merge_dag(
        &handle,
        bb,
        vec![x.clone(), y.clone(), ydep, z],
        vec![
            make_test_edge("rc-x", "rc-y"),
            make_test_edge("rc-y", "rc-ydep"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    let xs = expect_drv(&handle, "rc-x").await;
    assert_eq!(
        xs.status,
        DerivationStatus::Completed,
        "re-probe fixed-point: X (Queued) and Y (Ready) both cached → both \
         Completed; X must NOT be reset to Ready by reprobe_unlocked (was: \
         find_newly_ready(Y) captured X Queued → post-loop transition(Ready) \
         on now-Completed X succeeded via I-047 carve-out → push_ready)"
    );
    assert_eq!(
        expect_drv(&handle, "rc-y").await.status,
        DerivationStatus::Completed
    );
    Ok(())
}

/// I-094 deferred lane: pre-existing Poisoned-at-limit X whose output
/// is now locally present but inputDrv Y is in-flight. X ∈ cached_hits
/// → fixed-point defers (all_deps_completed(X)=false). X is NOT
/// newly_inserted (at-limit ⇒ is_retriable_on_resubmit=false).
/// seed_initial_states skips it; reconcile_preexisting skips
/// cached_hits keys. When Y completes, find_newly_ready only walks
/// Queued. Net: X stuck Poisoned forever despite output present.
/// Fix: deferred-reprobe stanza resets X →Queued.
#[tokio::test]
async fn test_deferred_reprobe_hit_on_poisoned_at_limit_unsticks() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut rx = connect_executor(&handle, "dr-w", "x86_64-linux").await?;

    let x_out = test_store_path("dr-x-out");
    let y_out = test_store_path("dr-y-out");
    let mut x = make_node("dr-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut y = make_node("dr-y");
    y.expected_output_paths = vec![y_out.clone()];

    // Build 1: X→Y. Y dispatches (leaf). Hold Y running.
    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![x.clone(), y.clone()],
        vec![make_test_edge("dr-x", "dr-y")],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut rx).await;
    assert!(assn.drv_path.ends_with("dr-y.drv"));

    // Force X to Poisoned at the resubmit limit so dag.merge does NOT
    // reset it on resubmit (is_retriable_on_resubmit=false).
    handle
        .debug_force_poisoned("dr-x", POISON_RESUBMIT_RETRY_LIMIT)
        .await?;
    let pre = expect_drv(&handle, "dr-x").await;
    assert_eq!(pre.status, DerivationStatus::Poisoned);
    assert_eq!(pre.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT);

    // X's output now locally present (cached_hits lane).
    store.seed_with_content(&x_out, b"x");

    // Build 2: {X,Y}. X ∈ existing_reprobe (Poisoned), X ∈ cached_hits
    // (output present), all_deps_completed(X)=false (Y Running) →
    // deferred. Stanza resets X →Queued.
    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![x, y],
        vec![make_test_edge("dr-x", "dr-y")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let xs = expect_drv(&handle, "dr-x").await;
    assert_eq!(
        xs.status,
        DerivationStatus::Queued,
        "deferred re-probe on Poisoned-at-limit with output present + dep \
         in-flight → reset to Queued (was: stayed Poisoned forever; \
         find_newly_ready never picks up Poisoned)"
    );
    assert_eq!(
        xs.retry.resubmit_cycles, 0,
        "failure history cleared (output present)"
    );

    // Complete Y → X promotes via find_newly_ready (now that it's Queued).
    store.seed_with_content(&y_out, b"y");
    complete_success(&handle, "dr-w", &assn.drv_path, &y_out).await?;
    barrier(&handle).await;
    let xs2 = expect_drv(&handle, "dr-x").await;
    assert!(
        matches!(
            xs2.status,
            DerivationStatus::Ready | DerivationStatus::Completed
        ),
        "after dep completes, X (Queued) promotes; got {:?}",
        xs2.status
    );
    Ok(())
}

// r[verify sched.merge.reconcile-order]
// r[verify sched.merge.stale-completed-verify+3]
/// bug_089: `apply_cached_hits`' `reprobe_unlocked` advance fired
/// BEFORE `verify_preexisting_completed` reset stale-Completed deps.
/// D depends on {X, Y}. Y is stale-Completed (output GC'd). X is
/// Ready, then its output appears locally → re-probe cache-hit. With
/// the old phase order: 6a's `find_newly_ready(X)` sees Y still
/// Completed → `all_deps_completed(D)=true` → D pushed Ready; 6c then
/// resets Y but D stays Ready → dispatched against missing output.
/// With the fix: 6a only collects `reprobe_unlocked`; 6f re-checks
/// `all_deps_completed(D)` post-6c, finds Y reset → D stays Queued.
#[tokio::test]
async fn test_reprobe_unlocked_deferred_past_stale_reset() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let x_out = test_store_path("ro-x-out");
    let y_out = test_store_path("ro-y-out");
    let mut x = make_node("ro-x");
    x.expected_output_paths = vec![x_out.clone()];
    let mut y = make_node("ro-y");
    y.expected_output_paths = vec![y_out.clone()];
    let mut d = make_node("ro-d");
    d.expected_output_paths = vec![test_store_path("ro-d-out")];

    // Build #1: D → {X, Y}. No outputs seeded → all 3 newly_inserted.
    // Y, X leaves → Ready; D → Queued.
    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![d.clone(), x.clone(), y.clone()],
        vec![
            make_test_edge("ro-d", "ro-x"),
            make_test_edge("ro-d", "ro-y"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    // Precondition setup: Y forced Completed with output_paths set
    // (simulates a prior run that completed Y, then GC swept y_out).
    // y_out is NOT seeded in MockStore → 6c's FMP reports it missing.
    handle
        .debug_force_status("ro-y", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("ro-y", vec![y_out.clone()])
        .await?;
    // X stays Ready (∈ existing_reprobe). D forced Queued (waiting on X).
    handle
        .debug_force_status("ro-d", DerivationStatus::Queued)
        .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "ro-x").await.status,
        DerivationStatus::Ready
    );

    // X's output now locally present → cached_hits lane (NOT
    // substitutable: avoids the pending_substitute split).
    store.seed_with_content(&x_out, b"x");

    // Build #2: same DAG. Uncached sibling root Z so topdown's
    // all-or-nothing falls through (D's output isn't seeded but D is
    // the only "root" without Z; defensive — make_node doesn't seed).
    let mut z = make_node("ro-z");
    z.expected_output_paths = vec![test_store_path("ro-z-out")];
    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![d, x, y, z],
        vec![
            make_test_edge("ro-d", "ro-x"),
            make_test_edge("ro-d", "ro-y"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Y was reset (stale Completed → Ready/Queued; output_paths cleared).
    let ys = expect_drv(&handle, "ro-y").await;
    assert!(
        matches!(
            ys.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "6c reset stale-Completed Y; got {:?}",
        ys.status
    );
    // D stayed Queued: 6f's all_deps_completed re-check (post-6c) sees
    // Y no longer Completed → D NOT advanced. Before: D was Ready.
    let ds = expect_drv(&handle, "ro-d").await;
    assert_eq!(
        ds.status,
        DerivationStatus::Queued,
        "r[sched.merge.reconcile-order]: reprobe_unlocked advance must \
         re-check all_deps_completed AFTER stale-reset; D's dep Y was \
         reset → D stays Queued (was: Ready against Y's stale Completed)"
    );
    // X completed via re-probe.
    assert_eq!(
        expect_drv(&handle, "ro-x").await.status,
        DerivationStatus::Completed
    );
    Ok(())
}

// r[verify sched.merge.reconcile-order]
/// bug_132: `seed_initial_states` ran BEFORE `spawn_substitute_fetches`
/// rescued a reprobe-Poisoned dep. A (newly-inserted) depends on B
/// (hard-Poisoned, retry≥limit so dag.merge does NOT reset). B's
/// output is upstream-substitutable. With the old order: 6d seed reads
/// `any_dep_terminally_failed(A)` → B Poisoned → A=DependencyFailed,
/// `first_dep_failed=Some(A)`; 6e then flips B→Substituting too late.
/// !keep_going build fail-fasts while B's fetch is mid-flight. With
/// the fix: 6d (reprobe_sub spawn) runs FIRST → B is Substituting
/// when 6e seed reads it → A goes Queued, build stays Active.
#[tokio::test]
async fn test_seed_ignores_reprobe_pending_substitute_dep() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let b_out = test_store_path("rs-b-out");
    let mut b = make_node("rs-b");
    b.expected_output_paths = vec![b_out.clone()];
    let mut a = make_node("rs-a");
    a.expected_output_paths = vec![test_store_path("rs-a-out")];

    // Build #1: B alone. Force-poison at the limit so resubmit does
    // NOT reset (`is_retriable_on_resubmit=false`).
    merge_dag(&handle, Uuid::new_v4(), vec![b.clone()], vec![], false).await?;
    assert!(
        handle
            .debug_force_poisoned("rs-b", POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "rs-b").await.status,
        DerivationStatus::Poisoned,
        "precondition"
    );

    // B's output now upstream-substitutable (NOT locally present →
    // pending_substitute lane, not cached_hits).
    store.state.substitutable.write().unwrap().push(b_out);

    // Build #2: {A, B} with edge A→B. !keep_going.
    // B ∈ existing_reprobe (Poisoned), B ∈ pending_substitute. A is
    // newly_inserted. With the fix: 6d reprobe_sub spawns B
    // →Substituting BEFORE 6e seed → A sees B non-terminal → Queued.
    let build2 = Uuid::new_v4();
    merge_dag(
        &handle,
        build2,
        vec![a, b],
        vec![make_test_edge("rs-a", "rs-b")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Build #2 stayed Active (NOT fail-fasted on stale Poisoned B).
    let s = query_status(&handle, build2).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Active as i32,
        "r[sched.merge.reconcile-order]: reprobe-Poisoned B → Substituting \
         BEFORE seed; A must NOT be marked DependencyFailed (was: !keep_going \
         build fail-fasted with 'derivation A failed' while B mid-fetch)"
    );
    let as_ = expect_drv(&handle, "rs-a").await;
    assert_ne!(
        as_.status,
        DerivationStatus::DependencyFailed,
        "A must NOT be DependencyFailed (B was Substituting at seed-time, \
         not Poisoned); got {:?}",
        as_.status
    );
    let bs = expect_drv(&handle, "rs-b").await;
    assert!(
        matches!(
            bs.status,
            DerivationStatus::Substituting | DerivationStatus::Completed
        ),
        "B Poisoned → Substituting via reprobe_sub spawn; got {:?}",
        bs.status
    );

    // Let B's fetch complete → A advances → build succeeds.
    settle_substituting(&handle, &["rs-b"]).await;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "rs-b").await.status,
        DerivationStatus::Completed
    );
    let as2 = expect_drv(&handle, "rs-a").await;
    assert!(
        matches!(
            as2.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "after B completes via substitute, A promotes; got {:?}",
        as2.status
    );
    Ok(())
}
