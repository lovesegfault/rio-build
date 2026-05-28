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
// r[verify sched.breaker.cache-check+3]
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

// r[verify sched.merge.stale-completed-verify+5]
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
    // r[sched.substitute.detached+5]: substitutable lane spawns the fetch;
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

// r[verify sched.merge.wanted-outputs+2]
/// THE incident scenario: a multi-output derivation whose only missing
/// output is one nothing wants (glibc-debug) must classify as a cache
/// hit, not fall through to a from-source build dispatch. Three cases:
///
/// 1. `wanted=[out]`, P_out present, P_debug missing+unsubstitutable →
///    cache hit (Completed at merge, build Succeeded). The recorded
///    `output_paths` still cover ALL declared outputs (constraint 4 —
///    the hit VALUES stay `expected_output_paths`).
/// 2. Same store state, `wanted=[]` (the all-wanted sentinel) → NOT a
///    hit; falls through to Ready exactly as today. Pre-migration rows
///    and the BasicDerivation fallback keep the conservative criterion.
/// 3. `wanted=[out,debug]`, P_debug missing-but-substitutable →
///    pending_substitute (detached fetch → Completed), not hits, not a
///    from-source fall-through.
#[tokio::test]
async fn missing_unwanted_output_is_still_a_cache_hit() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // --- Case 1: missing output is unwanted → cache hit -------------
    let out_1 = test_store_path("wo-hit-out");
    let dbg_1 = test_store_path("wo-hit-debug");
    store.seed_with_content(&out_1, b"out");
    // dbg_1 deliberately NOT seeded and NOT substitutable.
    let mut n1 = make_node("wo-hit");
    n1.output_names = vec!["out".into(), "debug".into()];
    n1.expected_output_paths = vec![out_1.clone(), dbg_1.clone()];
    n1.wanted_output_names = vec!["out".into()];
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![n1], vec![], false).await?;
    barrier(&handle).await;

    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "missing UNWANTED output must not condemn the derivation to a \
         from-source build — all wanted outputs are locally present"
    );
    assert_eq!(s1.cached_derivations, 1, "classified as a cache hit");
    let d1 = expect_drv(&handle, "wo-hit").await;
    assert_eq!(d1.status, DerivationStatus::Completed);
    assert_eq!(
        d1.output_paths,
        vec![out_1, dbg_1],
        "the hit VALUES keep recording all declared paths, not just the \
         wanted subset"
    );

    // --- Case 2: empty wanted set = all wanted → NOT a hit ----------
    let out_2 = test_store_path("wo-all-out");
    let dbg_2 = test_store_path("wo-all-debug");
    store.seed_with_content(&out_2, b"out");
    let mut n2 = make_node("wo-all");
    n2.output_names = vec!["out".into(), "debug".into()];
    n2.expected_output_paths = vec![out_2, dbg_2];
    n2.wanted_output_names = vec![];
    let b2 = Uuid::new_v4();
    merge_dag(&handle, b2, vec![n2], vec![], false).await?;
    barrier(&handle).await;

    let s2 = query_status(&handle, b2).await?;
    assert_eq!(
        s2.state,
        rio_proto::types::BuildState::Active as i32,
        "empty wanted set means ALL declared outputs wanted — a missing \
         declared output must keep blocking the cache hit"
    );
    assert_eq!(s2.cached_derivations, 0);
    assert_eq!(
        expect_drv(&handle, "wo-all").await.status,
        DerivationStatus::Ready,
        "falls through to a from-source build"
    );

    // --- Case 3: missing WANTED output is substitutable → pending ---
    let out_3 = test_store_path("wo-sub-out");
    let dbg_3 = test_store_path("wo-sub-debug");
    store.seed_with_content(&out_3, b"out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(dbg_3.clone());
    let mut n3 = make_node("wo-sub");
    n3.output_names = vec!["out".into(), "debug".into()];
    n3.expected_output_paths = vec![out_3, dbg_3.clone()];
    n3.wanted_output_names = vec!["out".into(), "debug".into()];
    let b3 = Uuid::new_v4();
    merge_dag(&handle, b3, vec![n3], vec![], false).await?;
    settle_substituting(&handle, &["wo-sub"]).await;

    let s3 = query_status(&handle, b3).await?;
    assert_eq!(
        s3.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "a missing WANTED output that is substitutable goes through the \
         pending_substitute lane, not hits and not a from-source build"
    );
    let qpi = store.calls.qpi_calls.read().unwrap().clone();
    assert!(
        qpi.contains(&dbg_3),
        "the detached substitute fetch ran for the missing wanted output; \
         qpi_calls={qpi:?}"
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
            .fail_query_path_info_permanent
            .store(true, std::sync::atomic::Ordering::SeqCst);
    }

    let mut node = make_node("sub-probe");
    node.expected_output_paths = vec![out_path.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    // r[sched.substitute.detached+5]: substitutable lane spawns the fetch;
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

/// `FindMissingPaths.indeterminate_paths` (probe got 429/5xx/deadline)
/// is treated optimistically: drv enters Substituting and the closure
/// walk runs. If the path IS actually upstream (probe was a transient
/// 429), the fetch succeeds → Cached. Without this, indeterminate was
/// treated as confirmed-miss and dispatched as a build.
// r[verify sched.merge.substitute-probe-indeterminate]
#[tokio::test]
async fn test_indeterminate_probe_tries_substitute_not_build() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("indet-out");
    // Probe says indeterminate (NOT in `substitutable`); but the
    // closure-walk fetch DOES find it (in `substitutable` for the
    // SubstitutePath RPC). Mirrors the live case: HEAD 429'd but the
    // GET succeeds.
    store.state.indeterminate.write().unwrap().push(out.clone());
    store.state.substitutable.write().unwrap().push(out.clone());

    let mut node = make_node("indet-drv");
    node.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    settle_substituting(&handle, &["indet-drv"]).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "indeterminate → optimistic fetch → Cached, not build dispatch"
    );
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&out),
        "closure walk must run for indeterminate paths; qpi_calls={qpi:?}"
    );
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

// r[verify sched.merge.substitute-topdown+10]
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
    // r[sched.substitute.detached+5]: top-down no longer awaits QPI inline;
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

// r[verify sched.merge.substitute-topdown+10]
/// Top-down negative: an `explicitly_requested` NON-root (a client
/// target folded inside another target's closure by the gateway's
/// multi-target dedup) whose wanted output is NOT available must block
/// the prune entirely — the demand set is roots ∪ flagged nodes, and
/// the criterion must hold for every member.
///
/// app → lib → dep; app's output is substitutable upstream, lib's is
/// missing and not substitutable, lib carries `explicitly_requested`.
/// A roots-only criterion sees app available, prunes lib+dep, and the
/// requested lib is silently never built. The fix falls through to the
/// full merge: 3 derivations, lib gets a real verdict (Queued behind
/// its dep) instead of vanishing.
#[tokio::test]
async fn test_topdown_explicit_target_unavailable_blocks_prune() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Only app's output is substitutable. lib/dep outputs are missing
    // and NOT substitutable.
    let app_out = test_store_path("tde-app-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(app_out.clone());

    let mut app = make_node("tde-app");
    app.expected_output_paths = vec![app_out.clone()];
    let mut lib = make_node("tde-lib");
    lib.expected_output_paths = vec![test_store_path("tde-lib-out")];
    // The gateway folded the client's selector into the wanted set and
    // marked the node as a named build target.
    lib.wanted_output_names = vec!["out".into()];
    lib.explicitly_requested = true;
    let mut dep = make_node("tde-dep");
    dep.expected_output_paths = vec![test_store_path("tde-dep-out")];

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![app, lib, dep],
        vec![
            make_test_edge("tde-app", "tde-lib"),
            make_test_edge("tde-lib", "tde-dep"),
        ],
        false,
    )
    .await?;
    // app is substitutable, so whichever path was taken it ends up in
    // the detached-fetch lane; wait for that to settle before judging.
    settle_substituting(&handle, &["tde-app"]).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.total_derivations, 3,
        "an explicitly requested target with an unavailable wanted output \
         must veto the prune — the full merge keeps all 3 derivations"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "lib still has to be built from source, so the build cannot be done"
    );
    assert_eq!(
        expect_drv(&handle, "tde-lib").await.status,
        DerivationStatus::Queued,
        "the requested target got a real verdict (queued behind its dep)"
    );
    assert_eq!(
        expect_drv(&handle, "tde-dep").await.status,
        DerivationStatus::Ready,
        "lib's dependency closure survived the merge"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Top-down positive: when every demanded node — structural roots AND
/// `explicitly_requested` non-roots — is available upstream, the prune
/// fires and keeps the whole demand set, not just the roots.
///
/// app → lib → dep with app and lib substitutable, lib flagged: the
/// pruned submission is {app, lib} (dep dropped, edges dropped), both
/// are routed through the substitute lane and complete via the
/// detached fetch, and the store saw lib's wanted path — the requested
/// target is fetched, not fabricated.
#[tokio::test]
async fn test_topdown_explicit_target_substitutable_kept_in_prune() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let app_out = test_store_path("tdk-app-out");
    let lib_out = test_store_path("tdk-lib-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(app_out.clone());
        subs.push(lib_out.clone());
    }

    let mut app = make_node("tdk-app");
    app.expected_output_paths = vec![app_out.clone()];
    let mut lib = make_node("tdk-lib");
    lib.expected_output_paths = vec![lib_out.clone()];
    lib.wanted_output_names = vec!["out".into()];
    lib.explicitly_requested = true;
    let mut dep = make_node("tdk-dep");
    dep.expected_output_paths = vec![test_store_path("tdk-dep-out")];

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![app, lib, dep],
        vec![
            make_test_edge("tdk-app", "tdk-lib"),
            make_test_edge("tdk-lib", "tdk-dep"),
        ],
        false,
    )
    .await?;
    settle_substituting(&handle, &["tdk-app", "tdk-lib"]).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.total_derivations, 2,
        "the pruned submission is the demand set: app (root) AND the \
         explicitly requested lib; only dep is dropped"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "both demanded nodes substituted → build completes"
    );

    // Both demanded nodes went through the substitute lane and were
    // stamped by the post-commit topdown loop.
    for hash in ["tdk-app", "tdk-lib"] {
        let d = expect_drv(&handle, hash).await;
        assert_eq!(
            d.status,
            DerivationStatus::Completed,
            "{hash} should complete via the detached fetch"
        );
        assert!(d.topdown_pruned, "{hash} kept by the prune gets stamped");
    }

    // The store actually fetched lib's wanted path (no fabricated
    // success for the requested target), and never touched the
    // dropped dep. Scoped so the read guard ends before the await
    // below (clippy::await_holding_lock is lexical-scope based).
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            qpi.contains(&lib_out),
            "lib's wanted output must be eager-fetched; qpi_calls={qpi:?}"
        );
        assert!(
            qpi.contains(&app_out),
            "app's output must be eager-fetched; qpi_calls={qpi:?}"
        );
        let dep_out = test_store_path("tdk-dep-out");
        assert!(
            !qpi.contains(&dep_out),
            "dep was pruned and must not be fetched; qpi_calls={qpi:?}"
        );
    }
    assert!(
        handle.debug_query_derivation("tdk-dep").await?.is_none(),
        "dep should be pruned from the submission, not in the global DAG"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
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

    // Root was stamped topdown_pruned (the prune committed —
    // total_derivations == 1 below), then SubstituteComplete{ok=false}
    // took the fail-fast arm: build Failed (not Active, not Succeeded)
    // and the stamp is consumed (cleared) so a stale flag cannot re-arm
    // the fail-fast against a later resubmission or failover.
    let r = expect_drv(&handle, "td-fail-hello").await;
    assert!(
        !r.topdown_pruned,
        "fail-fast must clear the topdown_pruned stamp it consumed"
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

// r[verify sched.merge.substitute-topdown+10]
/// The roots-only prune's `topdown_pruned` stamp must survive a leader
/// failover, so it is persisted: once a pruned merge commits, the kept
/// (demanded) node's PG row carries `topdown_pruned = true`; a later
/// full merge that gives that node children **that are already
/// produced** clears the column via the post-reconciliation clear pass
/// after the merge is reconciled (a merge adding only unbuilt children
/// keeps it — see the reap-hazard test below).
#[tokio::test]
async fn test_topdown_pruned_persisted_to_pg_and_cleared_when_children_added() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // B1: root → dep with root's wanted output substitutable upstream
    // → the prune fires, keeps {root}, drops dep and the edge.
    let root_out = test_store_path("tdpg-root-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(root_out.clone());
    let mut root = make_node("tdpg-root");
    root.expected_output_paths = vec![root_out.clone()];
    let mut dep = make_node("tdpg-dep");
    dep.expected_output_paths = vec![test_store_path("tdpg-dep-out")];

    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![root, dep],
        vec![make_test_edge("tdpg-root", "tdpg-dep")],
        false,
    )
    .await?;

    // The kept node's PG row carries the flag as soon as the pruned
    // merge commits (the MergeDag reply is sent post-persist).
    let (pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdpg-root'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pruned,
        "kept node of a committed pruned merge must persist topdown_pruned = true \
         (in-memory only ⇒ lost on failover ⇒ doomed from-source dispatch)"
    );
    // The dropped dep never reached PG at all (prune commits roots-only).
    let dep_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM derivations WHERE drv_hash = 'tdpg-dep'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(dep_rows, 0, "pruned dep must not be persisted");

    // Let B1's detached fetch settle so it can't interleave with B2.
    settle_substituting(&handle, &["tdpg-root"]).await;

    // BC: dep gets built/substituted to completion by an unrelated
    // build, so when B2 later re-adds the edge R → dep, R's child is
    // already produced — the only children shape that may clear the
    // mark.
    let dep_out = test_store_path("tdpg-dep-out");
    store.seed_with_content(&dep_out, b"dep-out");
    let bc = Uuid::new_v4();
    let mut dep_c = make_node("tdpg-dep");
    dep_c.expected_output_paths = vec![dep_out.clone()];
    let _evc = merge_dag(&handle, bc, vec![dep_c], vec![], false).await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdpg-dep").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep is produced before B2 re-adds the edge"
    );

    // B2: a full merge that gives root its dep back: app → root → dep,
    // app's output NOT substitutable → no prune → the edges persist →
    // root now has a child in PG that is already produced (BC settled
    // it) → the flag is cleared in the same transaction that inserted
    // the edges.
    let mut app = make_node("tdpg-app");
    app.expected_output_paths = vec![test_store_path("tdpg-app-out")];
    let mut root_b2 = make_node("tdpg-root");
    root_b2.expected_output_paths = vec![root_out.clone()];
    let mut dep_b2 = make_node("tdpg-dep");
    dep_b2.expected_output_paths = vec![test_store_path("tdpg-dep-out")];

    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![app, root_b2, dep_b2],
        vec![
            make_test_edge("tdpg-app", "tdpg-root"),
            make_test_edge("tdpg-root", "tdpg-dep"),
        ],
        false,
    )
    .await?;

    let (pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdpg-root'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pruned,
        "a full merge that adds an already-produced child must clear \
         topdown_pruned in PG (the closure is in the store, so the \
         substitution-only invariant no longer holds)"
    );
    // The same merge clears the in-memory flag too — the lazy children
    // gate in handle_substitute_complete is a backstop, not the only
    // clearing site.
    assert!(
        !expect_drv(&handle, "tdpg-root").await.topdown_pruned,
        "a full merge that adds an already-produced child must clear the in-memory flag as well"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// A kept (demanded) node whose existing DAG children are ALL already
/// produced (Completed/Skipped) must NOT be stamped `topdown_pruned` —
/// its dependency closure exists in the store, so a from-source
/// dispatch is not doomed and the marker would only create the
/// stale-flag inconsistency the fail-fast clear has to mop up. (A node
/// whose children are still unbuilt IS stamped — see the sibling test
/// below.)
#[tokio::test]
async fn test_topdown_stamp_skips_kept_node_whose_children_are_already_produced() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdc-r-out");
    let d_out = test_store_path("tdc-d-out");
    let mk_r = || {
        let mut n = make_node("tdc-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_d = || {
        let mut n = make_node("tdc-d");
        n.expected_output_paths = vec![d_out.clone()];
        n
    };

    // B0: only D's output is substitutable → full merge (R, the sole
    // demanded node, is not available); D completes via the detached
    // fetch, so R's existing child is PRODUCED by the time B1 prunes.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(d_out.clone());
    let b0 = Uuid::new_v4();
    merge_dag(
        &handle,
        b0,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdc-r", "tdc-d")],
        false,
    )
    .await?;
    settle_substituting(&handle, &["tdc-d"]).await;
    assert_eq!(
        expect_drv(&handle, "tdc-d").await.status,
        DerivationStatus::Completed,
        "fixture premise: R's existing child is already produced"
    );

    // B1: same submission, but R's wanted output is now substitutable →
    // the prune fires and keeps {R}. R's child D is Completed.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdc-r", "tdc-d")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );

    // R's children are all produced → it must NOT carry the marker,
    // neither in memory nor in PG.
    assert!(
        !expect_drv(&handle, "tdc-r").await.topdown_pruned,
        "a kept node whose DAG children are already produced must not be stamped in memory"
    );
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdc-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg_pruned,
        "a kept node whose DAG children are already produced must not be persisted as pruned"
    );

    // Let B1's detached fetch settle before teardown.
    settle_substituting(&handle, &["tdc-r"]).await;
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// A kept (demanded) node whose existing DAG children are still UNBUILT
/// must keep the `topdown_pruned` stamp. Those children can belong to a
/// different build and be reaped unbuilt later (that build cancelled →
/// its sole-interest deps cascade terminal → reaped → `children[R]`
/// scrubbed); an unstamped R would then be childless with a never-
/// produced closure, and a substitute failure would take the generic
/// revert instead of the fail-fast — handing R to a worker from source
/// for the doomed ENOENT dispatch this machinery exists to prevent.
/// While the unbuilt children remain in the DAG, the stamp is inert
/// (every consumption site requires childlessness or a reap-created
/// closure hole).
#[tokio::test]
async fn test_topdown_stamp_kept_when_existing_children_unbuilt() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdu-r-out");
    let mk_r = || {
        let mut n = make_node("tdu-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_d = || {
        let mut n = make_node("tdu-d");
        n.expected_output_paths = vec![test_store_path("tdu-d-out")];
        n
    };

    // B0: nothing substitutable → full merge → R has child D in the
    // DAG, and D is UNBUILT (Ready, no worker connected).
    let b0 = Uuid::new_v4();
    merge_dag(
        &handle,
        b0,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdu-r", "tdu-d")],
        false,
    )
    .await?;
    assert_eq!(
        expect_drv(&handle, "tdu-d").await.status,
        DerivationStatus::Ready,
        "fixture premise: R's existing child is still unbuilt"
    );

    // B1: R's wanted output is now substitutable → the prune fires and
    // keeps {R}, dropping its closure. R's existing child is unbuilt,
    // so the stamp must be applied (in memory and in PG).
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdu-r", "tdu-d")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );

    assert!(
        expect_drv(&handle, "tdu-r").await.topdown_pruned,
        "a kept closure-dropped node whose existing children are unbuilt must \
         keep the stamp in memory (the children can be reaped unbuilt later)"
    );
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdu-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg_pruned,
        "the stamp for a node with unbuilt children must also be persisted"
    );

    // Let B1's detached fetch settle before teardown.
    settle_substituting(&handle, &["tdu-r"]).await;
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// `topdown_pruned` flag persistence bypass: B1 topdown-prunes R; while
/// R's fetch is in-flight, B2 full-merges R WITH its deps. R is
/// pre-existing `Substituting` so `dag.merge` doesn't reset it; the
/// `topdown_pruned` flag persists. Fetch then fails. Before the fix:
/// `handle_substitute_complete` saw `topdown_pruned=true` and failed
/// EVERY interested build — including B2, whose deps ARE in the DAG.
/// After: gate on `get_children(R).is_empty()`; R has children →
/// fail-fast suppressed and R falls through to normal Queued handling;
/// the flag itself is only cleared once those children are all
/// produced.
///
/// Race staged deterministically via `debug_force_status`/
/// `debug_set_topdown_pruned` + injected `SubstituteComplete{ok=false}`
/// (see `r[sched.substitute.detached+5]` — the actor only checks `status
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
            forgiven: vec![],
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
        post.topdown_pruned,
        "flag retained while R's children are unbuilt — they can still be \
         reaped unbuilt; suppression of the fail-fast (not a clear) is what \
         protects B2 here"
    );
    assert!(post.substitute_tried, "one-shot fall-through still applies");
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The reap hazard, end to end: B1's prune stamps childless R and parks
/// its detached fetch; B2 full-merges app→R→dep, which previously
/// cleared the stamp even though dep was UNBUILT; B2 is cancelled and
/// its sole-interest nodes are reaped, scrubbing dep out of
/// `children[R]`; R's fetch then fails. With the stamp eagerly cleared
/// the flag-keyed protections are all skipped and R is dispatched from
/// source (worker ENOENT — the doomed dispatch this machinery exists to
/// prevent). The clear must therefore use the same closure-evidence
/// criterion as the stamp (`closure_vouched`): B2's unbuilt dep
/// must NOT clear the mark, in memory or in PG, and after the reap the
/// walk failure must take the designed resubmit-directing fail-fast.
#[tokio::test]
async fn test_topdown_pruned_kept_when_merge_adds_unbuilt_children_then_reaped() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park B1's detached fetch: QueryPathInfo waits on the gate (never
    // released), so R stays Substituting for the whole test and the
    // injected SubstituteComplete below is accepted.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep with root's wanted output substitutable upstream →
    // the prune fires, keeps {R} (stamped, childless), drops dep.
    let r_out = test_store_path("tdreap-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdreap-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep = || {
        let mut n = make_node("tdreap-dep");
        n.expected_output_paths = vec![test_store_path("tdreap-dep-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep()],
        vec![make_test_edge("tdreap-r", "tdreap-dep")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdreap-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // B2: a full merge that gives R an UNBUILT child (app's output not
    // substitutable → no prune). The mark must survive this merge.
    let mut app = make_node("tdreap-app");
    app.expected_output_paths = vec![test_store_path("tdreap-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep()],
        vec![
            make_test_edge("tdreap-app", "tdreap-r"),
            make_test_edge("tdreap-r", "tdreap-dep"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdreap-r").await.topdown_pruned,
        "a merge that adds only UNBUILT children must keep the in-memory \
         mark — those children can be reaped unbuilt later"
    );
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdreap-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg_pruned,
        "the PG mark must survive an edge-insert whose children are unbuilt \
         (clear_topdown_pruned_for_parents must skip such parents)"
    );

    // Cancel B2 and reap its sole-interest nodes (dep, app). R is shared
    // with B1, so it survives — childless again, mark intact.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdreap-dep").await?.is_none(),
        "B2's sole-interest dep must be reaped (scrubbed from children[R])"
    );
    assert!(
        handle.debug_query_derivation("tdreap-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        expect_drv(&handle, "tdreap-r").await.topdown_pruned,
        "R survives (B1's interest) with the mark intact after the reap"
    );

    // R's parked walk now genuinely fails.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdreap-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    // Designed outcome: the fail-fast, not a from-source dispatch.
    let r = expect_drv(&handle, "tdreap-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "childless topdown-pruned R with a failed substitute must not be \
         dispatchable from source; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Failed as i32,
        "B1 must fail via the resubmit-directing fail-fast (was: doomed \
         from-source dispatch after the eager clear lost the mark)"
    );
    assert!(
        s1.error_summary.contains("topdown") && s1.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s1.error_summary
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The reap hazard with the ORDERING REVERSED: the walk verdict arrives
/// while another build's unbuilt children are still attached, and only
/// then are those children reaped. B1's prune stamps childless R and
/// parks its detached fetch; B2 full-merges app→R→dep (unbuilt children
/// — the mark is kept); R's fetch FAILS while dep is still attached, so
/// the walk-failure handler suppresses the fail-fast (children present)
/// and parks R Queued with the one-shot `substitute_tried` set; B2 is
/// then cancelled and its sole-interest nodes reaped, scrubbing dep out
/// of `children[R]`. R is now a childless, marked, already-walked root
/// that nothing will ever re-evaluate — `find_newly_ready` only fires on
/// completions and R has no children left to complete — so B1 would hang
/// Active forever. The terminal-build reap must re-evaluate the
/// surviving parent and take the same resubmit-directing fail-fast the
/// verdict-after-reap ordering (test above) gets.
#[tokio::test]
async fn test_topdown_pruned_root_fail_fast_when_children_reaped_after_failed_walk() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park B1's detached fetch: QueryPathInfo waits on the gate (never
    // released), so R stays Substituting until the verdict is injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep with root's wanted output substitutable upstream →
    // the prune fires, keeps {R} (stamped, childless), drops dep.
    let r_out = test_store_path("tdvr-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdvr-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep = || {
        let mut n = make_node("tdvr-dep");
        n.expected_output_paths = vec![test_store_path("tdvr-dep-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep()],
        vec![make_test_edge("tdvr-r", "tdvr-dep")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdvr-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // B2: a full merge that gives R an UNBUILT child (app's output not
    // substitutable → no prune). The mark survives this merge.
    let mut app = make_node("tdvr-app");
    app.expected_output_paths = vec![test_store_path("tdvr-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep()],
        vec![
            make_test_edge("tdvr-app", "tdvr-r"),
            make_test_edge("tdvr-r", "tdvr-dep"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdvr-r").await.topdown_pruned,
        "fixture premise: a merge that adds only UNBUILT children keeps the mark"
    );

    // R's parked walk fails WHILE dep is still attached: the handler
    // suppresses the fail-fast (unbuilt children present), keeps the
    // mark, and parks R Queued with the one-shot flag set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdvr-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdvr-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: the suppressed fail-fast parks R Queued behind its unbuilt child"
    );
    assert!(
        mid.topdown_pruned,
        "fixture premise: mark kept while children are unbuilt"
    );
    assert!(
        mid.substitute_tried,
        "fixture premise: the failed walk set the one-shot flag"
    );

    // Cancel B2 and reap its sole-interest nodes (dep, app). R is shared
    // with B1, so it survives — childless again, mark intact, walk spent.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdvr-dep").await?.is_none(),
        "B2's sole-interest dep must be reaped (scrubbed from children[R])"
    );
    assert!(
        handle.debug_query_derivation("tdvr-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );

    // Designed outcome: the reap re-evaluates the surviving parent and
    // takes the resubmit-directing fail-fast — not a silent hang.
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Failed as i32,
        "B1 must fail via the resubmit-directing fail-fast when the reap \
         strands its already-walked pruned root (was: left Active forever)"
    );
    assert!(
        s1.error_summary.contains("topdown") && s1.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s1.error_summary
    );
    let r = expect_drv(&handle, "tdvr-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "childless topdown-pruned R with a spent walk must not be dispatched \
         from source; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Companion to the test above with the children PRODUCED (not reaped
/// unbuilt) before the terminal-build reap: the surviving root must NOT
/// be fail-fasted and the surviving build must not hang.
///
/// Reasoning for the expected outcome: when dep completes (here via its
/// substitute success), the completion-time
/// `clear_topdown_pruned_for_produced_parents` sees R's children all
/// produced and drops the mark, and the inline newly-ready promote lifts
/// R Queued→Ready — R's closure IS in the store, so building it from
/// source is legitimate. At reap time R is therefore childless but
/// UNMARKED (and already Ready), so the reap-time re-evaluation must
/// leave it alone: no fail-fast (that arm requires the mark) and no
/// re-promotion (already Ready). This pins the hook's gating — a hook
/// that fail-fasted any childless `substitute_tried` survivor would
/// wrongly fail B1 here even though R is buildable.
#[tokio::test]
async fn test_topdown_pruned_root_not_failed_when_produced_children_reaped_after_failed_walk()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park detached fetches on the QPI gate (never released): R's walk
    // stays parked until the failure verdict is injected, dep's until
    // its success verdict is injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep with root's wanted output substitutable upstream →
    // the prune fires, keeps {R} (stamped, childless), drops dep.
    let r_out = test_store_path("tdpc-r-out");
    let dep_out = test_store_path("tdpc-dep-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdpc-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep = || {
        let mut n = make_node("tdpc-dep");
        n.expected_output_paths = vec![dep_out.clone()];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep()],
        vec![make_test_edge("tdpc-r", "tdpc-dep")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdpc-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // B2: full merge app→R→dep. dep's output is substitutable upstream,
    // so dep is routed to substitution (parked on the same gate); app is
    // not substitutable → no prune for B2, R keeps the mark.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(dep_out.clone());
    let mut app = make_node("tdpc-app");
    app.expected_output_paths = vec![test_store_path("tdpc-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep()],
        vec![
            make_test_edge("tdpc-app", "tdpc-r"),
            make_test_edge("tdpc-r", "tdpc-dep"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdpc-r").await.topdown_pruned,
        "fixture premise: a merge that adds only UNBUILT children keeps the mark"
    );
    wait_for_status(&handle, "tdpc-dep", DerivationStatus::Substituting).await;

    // R's parked walk fails while dep is still attached and unbuilt: the
    // fail-fast is suppressed, the mark kept, R parked Queued with the
    // one-shot flag set (same shape as the unbuilt-children test above).
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdpc-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdpc-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: the suppressed fail-fast parks R Queued behind its unbuilt child"
    );
    assert!(mid.topdown_pruned && mid.substitute_tried);

    // dep is then PRODUCED (its parked fetch succeeds): the completion
    // clears R's mark (children all produced) and promotes R to Ready.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdpc-dep".into(),
            ok: true,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "tdpc-dep").await.status,
        DerivationStatus::Completed,
        "fixture premise: dep produced via its substitute success"
    );
    let produced = expect_drv(&handle, "tdpc-r").await;
    assert!(
        !produced.topdown_pruned,
        "fixture premise: dep completing makes R's children all produced, so \
         the completion-time clear drops the mark"
    );
    assert_eq!(
        produced.status,
        DerivationStatus::Ready,
        "fixture premise: R is promoted Queued→Ready when its last dep completes"
    );

    // Cancel B2 and reap its sole-interest nodes (dep — produced — and
    // app). R survives the reap childless, unmarked, already Ready.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdpc-dep").await?.is_none(),
        "B2's sole-interest dep must be reaped (it was produced, then orphaned)"
    );
    assert!(
        handle.debug_query_derivation("tdpc-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );

    // Designed outcome: no fail-fast and no hang — R's closure is
    // produced, so it stays dispatchable from source and B1 stays live.
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 must NOT be failed by the reap re-evaluation: R's mark was \
         legitimately cleared when its children became produced"
    );
    let r = expect_drv(&handle, "tdpc-r").await;
    assert!(
        matches!(
            r.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "R must remain dispatchable from source (closure produced), not \
         fail-fasted or stranded; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the completion-time clear stays cleared through the reap"
    );
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// A `CleanupTerminalBuild` drained by an ex-leader must NOT run the
/// survivor re-evaluation: the fail-fast arm writes terminal build state
/// and clears the persisted `topdown_pruned` mark, and the promote arm
/// persists Ready — leader-class writes that would race the new leader's
/// recovery (which restores R from PG and owns its verdict). The rest of
/// the cleanup (in-memory map removal, DAG reap) keeps running on
/// standby as before. Staging mirrors the verdict-then-reap test above;
/// the lease is lost between the cancel and the cleanup.
#[tokio::test]
async fn test_topdown_pruned_survivor_not_fail_fasted_when_cleanup_drains_on_ex_leader()
-> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    let leader = crate::lease::LeaderState::default(); // starts as leader
    let leader_for_actor = leader.clone();
    let (handle, _actor_task) =
        setup_actor_configured(db.pool.clone(), Some(store_client), move |_, p| {
            p.leader = leader_for_actor;
        });

    // Park B1's detached fetch on the QPI gate so R stays Substituting
    // until the failure verdict is injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless).
    let r_out = test_store_path("tdsl-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdsl-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep = || {
        let mut n = make_node("tdsl-dep");
        n.expected_output_paths = vec![test_store_path("tdsl-dep-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep()],
        vec![make_test_edge("tdsl-r", "tdsl-dep")],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdsl-r").await.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );

    // B2: full merge app→R→dep gives R an unbuilt child.
    let mut app = make_node("tdsl-app");
    app.expected_output_paths = vec![test_store_path("tdsl-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep()],
        vec![
            make_test_edge("tdsl-app", "tdsl-r"),
            make_test_edge("tdsl-r", "tdsl-dep"),
        ],
        false,
    )
    .await?;

    // R's walk fails while dep is attached → fail-fast suppressed, mark
    // kept, R parked Queued with the one-shot flag set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdsl-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdsl-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: suppressed fail-fast parks R Queued"
    );
    assert!(mid.topdown_pruned && mid.substitute_tried);

    // Cancel B2 while still leader (the CancelBuild arm is itself
    // leader-gated), then lose the lease BEFORE the cleanup drains.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    let (pre_b1_pg,): (String,) =
        sqlx::query_as("SELECT status::text FROM builds WHERE build_id = $1")
            .bind(b1)
            .fetch_one(&db.pool)
            .await?;
    leader.on_lose();

    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;

    // The ungated reap still ran (proves the command was drained, not
    // silently dropped)…
    assert!(
        handle.debug_query_derivation("tdsl-dep").await?.is_none(),
        "in-memory reap still runs on standby (pre-existing behavior)"
    );
    // …but the survivor re-evaluation must not: R untouched, B1 not
    // failed, nothing written to PG.
    let r = expect_drv(&handle, "tdsl-r").await;
    assert_eq!(
        r.status,
        DerivationStatus::Queued,
        "ex-leader must neither fail-fast (Cancelled) nor promote (Ready) the survivor"
    );
    assert!(
        r.topdown_pruned,
        "ex-leader must not consume the topdown_pruned mark"
    );
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Active as i32,
        "ex-leader must not terminally fail B1 from a stale DAG"
    );
    let (pg_pruned, pg_hole, pg_status): (bool, bool, String) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole, status FROM derivations WHERE drv_hash = 'tdsl-r'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(pg_pruned, "ex-leader must not clear the persisted mark");
    assert!(
        !pg_hole,
        "ex-leader must not stamp the persisted closure_hole breadcrumb (the in-memory \
         reap holes the survivor on standbys too, but the PG write is leader-class)"
    );
    assert_eq!(
        pg_status,
        DerivationStatus::Queued.as_str(),
        "ex-leader must not persist a status change for the survivor"
    );
    let (post_b1_pg,): (String,) =
        sqlx::query_as("SELECT status::text FROM builds WHERE build_id = $1")
            .bind(b1)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        post_b1_pg, pre_b1_pg,
        "ex-leader must not write build state for B1"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The reap-time fail-fast must defer to a substitution walk that is IN
/// FLIGHT at cleanup time, even when `substitute_tried` is already set.
/// The one-shot bit is sticky: after R's first walk failed (suppressed —
/// unbuilt children were attached), a third build's merge re-probes the
/// existing Queued node (`existing_reprobe` has no `substitute_tried`
/// gate) and re-spawns a walk, transitioning R back to Substituting
/// without clearing the bit. If B2's reap then strands R childless, the
/// hook must NOT park it: the in-flight walk's own verdict settles the
/// now-childless root (ok=false → the established fail-fast below,
/// ok=true → completion); parking it at reap time would terminally fail
/// B1/B3 prematurely and the late verdict would be dropped by the
/// not-Substituting guard.
#[tokio::test]
async fn test_topdown_pruned_root_not_failed_at_reap_while_respawned_walk_in_flight() -> TestResult
{
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park every detached fetch on the QPI gate (never released): R's
    // walks stay in flight until verdicts are injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless).
    let r_out = test_store_path("tdrw-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdrw-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep = || {
        let mut n = make_node("tdrw-dep");
        n.expected_output_paths = vec![test_store_path("tdrw-dep-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep()],
        vec![make_test_edge("tdrw-r", "tdrw-dep")],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdrw-r").await.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );

    // B2: full merge app→R→dep gives R an unbuilt child.
    let mut app = make_node("tdrw-app");
    app.expected_output_paths = vec![test_store_path("tdrw-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep()],
        vec![
            make_test_edge("tdrw-app", "tdrw-r"),
            make_test_edge("tdrw-r", "tdrw-dep"),
        ],
        false,
    )
    .await?;

    // R's first walk fails while dep is attached → fail-fast suppressed,
    // mark kept, R parked Queued with the one-shot flag set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdrw-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdrw-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: suppressed fail-fast parks R Queued"
    );
    assert!(mid.topdown_pruned && mid.substitute_tried);

    // B3 references the existing Queued R: the merge re-probe routes it
    // back to substitution (r_out is still substitutable upstream) and
    // the new walk parks on the gate — Substituting, with the sticky
    // `substitute_tried` and the mark still set.
    let b3 = Uuid::new_v4();
    let _ev3 = merge_dag(&handle, b3, vec![mk_r()], vec![], false).await?;
    let respawned = expect_drv(&handle, "tdrw-r").await;
    assert_eq!(
        respawned.status,
        DerivationStatus::Substituting,
        "fixture premise: B3's merge re-probe re-spawned R's walk"
    );
    assert!(
        respawned.topdown_pruned && respawned.substitute_tried,
        "fixture premise: the re-spawn clears neither the mark nor the one-shot flag"
    );

    // Cancel B2 and reap its sole-interest nodes (dep, app) while R's
    // re-spawned walk is still in flight.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdrw-dep").await?.is_none(),
        "B2's sole-interest dep must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdrw-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );

    // The reap must defer to the in-flight walk: R stays Substituting
    // with the mark intact, and neither surviving build is failed.
    let r = expect_drv(&handle, "tdrw-r").await;
    assert_eq!(
        r.status,
        DerivationStatus::Substituting,
        "reap must not park a survivor whose walk is in flight; got {:?}",
        r.status
    );
    assert!(
        r.topdown_pruned,
        "the mark is left for the walk's own verdict to consume"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 must not be failed while R's walk is still in flight"
    );
    assert_eq!(
        query_status(&handle, b3).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B3 must not be failed while R's walk is still in flight"
    );

    // The in-flight walk now genuinely fails: R is childless and marked,
    // so the established fail-fast lands and directs a resubmit.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdrw-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let r = expect_drv(&handle, "tdrw-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "childless topdown-pruned R with a failed walk must not be \
         dispatchable from source; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Failed as i32,
        "B1 fails via the resubmit-directing fail-fast once the walk's own verdict lands"
    );
    assert!(
        s1.error_summary.contains("topdown") && s1.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s1.error_summary
    );
    assert_eq!(
        query_status(&handle, b3).await?.state,
        rio_proto::types::BuildState::Failed as i32,
        "B3 shares R, so the same fail-fast settles it"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The MIXED-children reap shape, ordering A (verdict before the reap):
/// B1's prune stamps R and parks its detached fetch; BC produces a
/// second child dep2 (cache-hit at merge) and its interest keeps dep2
/// alive; B2 full-merges app→R→{dep1 unbuilt, dep2 produced}; R's walk
/// fails while both children are attached (fail-fast suppressed, R
/// parked Queued with the one-shot flag); B2 is then cancelled and its
/// sole-interest nodes reaped. dep1 — never produced — is reaped out
/// from under R while the produced dep2 survives via BC's interest, so
/// R's remaining child set no longer represents its pruned input
/// closure (a closure hole). The reap-time re-evaluation must take the
/// same resubmit-directing fail-fast as the childless shape — NOT
/// promote R Ready over the vacuously-produced survivor and hand it to
/// a worker from source (the doomed ENOENT dispatch this machinery
/// exists to prevent).
#[tokio::test]
async fn test_topdown_pruned_root_fail_fast_when_unproduced_child_reaped_but_produced_child_survives()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park B1's detached fetch: QueryPathInfo waits on the gate (never
    // released), so R stays Substituting until the verdict is injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep1 with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless),
    // drops dep1.
    let r_out = test_store_path("tdmx-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdmx-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep1 = || {
        let mut n = make_node("tdmx-dep1");
        n.expected_output_paths = vec![test_store_path("tdmx-dep1-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep1()],
        vec![make_test_edge("tdmx-r", "tdmx-dep1")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdmx-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // BC: dep2 is produced by an unrelated build (its output is seeded
    // as already present, so BC's merge completes it inline) and BC's
    // interest keeps it in the DAG across B2's later reap.
    let dep2_out = test_store_path("tdmx-dep2-out");
    store.seed_with_content(&dep2_out, b"dep2-out");
    let mk_dep2 = || {
        let mut n = make_node("tdmx-dep2");
        n.expected_output_paths = vec![dep2_out.clone()];
        n
    };
    let bc = Uuid::new_v4();
    let _evc = merge_dag(&handle, bc, vec![mk_dep2()], vec![], false).await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdmx-dep2").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep2 is produced before B2 attaches it to R"
    );

    // B2: a full merge that gives R BOTH children — dep1 unbuilt (B2's
    // sole interest) and dep2 already produced (shared with BC). app's
    // output is not substitutable → no prune for B2, R keeps the mark.
    let mut app = make_node("tdmx-app");
    app.expected_output_paths = vec![test_store_path("tdmx-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep1(), mk_dep2()],
        vec![
            make_test_edge("tdmx-app", "tdmx-r"),
            make_test_edge("tdmx-r", "tdmx-dep1"),
            make_test_edge("tdmx-r", "tdmx-dep2"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdmx-r").await.topdown_pruned,
        "fixture premise: a merge whose added children are not all produced keeps the mark"
    );

    // R's parked walk fails while BOTH children are attached: dep1 is
    // unbuilt, so the handler suppresses the fail-fast, keeps the mark,
    // and parks R Queued with the one-shot flag set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdmx-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdmx-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: the suppressed fail-fast parks R Queued behind its unbuilt child"
    );
    assert!(
        mid.topdown_pruned && mid.substitute_tried,
        "fixture premise: mark kept and one-shot flag set by the failed walk"
    );

    // Cancel B2 and reap its sole-interest nodes: dep1 (never produced)
    // and app go; dep2 survives via BC's interest; R survives via B1's.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdmx-dep1").await?.is_none(),
        "B2's sole-interest unbuilt dep1 must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdmx-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdmx-dep2").await?.is_some(),
        "dep2 must survive the reap (BC still holds interest in it)"
    );

    // Designed outcome: the reap-truncated child set ({dep2}, produced)
    // must not be trusted — the surviving-parent re-evaluation takes the
    // resubmit-directing fail-fast instead of promoting R Ready for a
    // doomed from-source dispatch.
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Failed as i32,
        "B1 must fail via the resubmit-directing fail-fast when an un-produced \
         child is reaped out from under its pruned root (was: R promoted Ready \
         over the surviving produced child and B1 left Active)"
    );
    assert!(
        s1.error_summary.contains("topdown") && s1.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s1.error_summary
    );
    let r = expect_drv(&handle, "tdmx-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "R must not be dispatchable from source after the closure hole; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The mixed-children reap shape with the ORDERING REVERSED: B2's
/// unbuilt dep1 is reaped while R's walk is still in flight (the reap
/// hook rightly defers to the walk — that skip is pinned by the
/// respawned-walk test above), and only then does the walk fail. At
/// verdict time R's surviving children are exactly the produced dep2,
/// so a children-keyed lazy clear would drop the mark and revert R to
/// Ready — but that child set is a reap-truncated view of the pruned
/// closure (dep1 was never produced). The verdict must take the
/// resubmit-directing fail-fast instead.
#[tokio::test]
async fn test_topdown_pruned_root_fail_fast_when_unproduced_child_reaped_but_produced_child_survives_reversed()
-> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park B1's detached fetch on the QPI gate (never released) so R
    // stays Substituting through the cancel + cleanup.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep1 with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless).
    let r_out = test_store_path("tdmr-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdmr-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep1 = || {
        let mut n = make_node("tdmr-dep1");
        n.expected_output_paths = vec![test_store_path("tdmr-dep1-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep1()],
        vec![make_test_edge("tdmr-r", "tdmr-dep1")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdmr-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // BC: dep2 is produced (cache-hit at merge) and kept alive by BC's
    // interest.
    let dep2_out = test_store_path("tdmr-dep2-out");
    store.seed_with_content(&dep2_out, b"dep2-out");
    let mk_dep2 = || {
        let mut n = make_node("tdmr-dep2");
        n.expected_output_paths = vec![dep2_out.clone()];
        n
    };
    let bc = Uuid::new_v4();
    let _evc = merge_dag(&handle, bc, vec![mk_dep2()], vec![], false).await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdmr-dep2").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep2 is produced before B2 attaches it to R"
    );

    // B2: full merge app→R→{dep1 unbuilt, dep2 produced}. The mark
    // survives (children not all produced).
    let mut app = make_node("tdmr-app");
    app.expected_output_paths = vec![test_store_path("tdmr-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep1(), mk_dep2()],
        vec![
            make_test_edge("tdmr-app", "tdmr-r"),
            make_test_edge("tdmr-r", "tdmr-dep1"),
            make_test_edge("tdmr-r", "tdmr-dep2"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdmr-r").await.topdown_pruned,
        "fixture premise: a merge whose added children are not all produced keeps the mark"
    );

    // Cancel B2 and reap its sole-interest nodes WHILE R's walk is still
    // parked (no verdict yet): dep1 — never produced — is reaped out
    // from under R; dep2 survives via BC's interest.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdmr-dep1").await?.is_none(),
        "B2's sole-interest unbuilt dep1 must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdmr-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdmr-dep2").await?.is_some(),
        "dep2 must survive the reap (BC still holds interest in it)"
    );
    // The reap defers to the in-flight walk (pre-existing behavior,
    // pinned by the respawned-walk test above): R stays Substituting
    // with the mark intact and B1 stays Active for now.
    let mid = expect_drv(&handle, "tdmr-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Substituting,
        "fixture premise: the reap must not park a survivor whose walk is in flight"
    );
    assert!(
        mid.topdown_pruned,
        "fixture premise: the mark is left for the walk's own verdict to consume"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "fixture premise: B1 is not failed while R's walk is still in flight"
    );
    // The leader-gated reap hook persists the breadcrumb the reap just
    // set (`migrations/064`), and the deferred verdict loop leaves the
    // in-flight survivor alone — so the persisted column is observable
    // here, exactly the durable evidence a failover in this window
    // would restore (without it, the produced survivor dep2 would
    // launder the recovery-time clear and re-arm the doomed dispatch).
    let (pg_hole,): (bool,) =
        sqlx::query_as("SELECT closure_hole FROM derivations WHERE drv_hash = 'tdmr-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg_hole,
        "the leader's reap hook must persist closure_hole for a survivor that lost an \
         un-produced child"
    );

    // The walk now genuinely fails. R's surviving children ({dep2}) are
    // all produced, but they are a reap-truncated view of the pruned
    // closure — the verdict must take the resubmit-directing fail-fast,
    // not the lazy clear + Ready revert.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdmr-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Failed as i32,
        "B1 must fail via the resubmit-directing fail-fast when the walk fails \
         after an un-produced child was reaped (was: lazy clear over the \
         surviving produced child and a Ready revert)"
    );
    assert!(
        s1.error_summary.contains("topdown") && s1.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s1.error_summary
    );
    let r = expect_drv(&handle, "tdmr-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "R must not be dispatchable from source after the closure hole; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    // The fail-fast's persisted clear drops the breadcrumb together with
    // the mark it qualifies — a stale persisted hole would otherwise
    // re-arm the conservative arm after every later failover.
    let (pg_pruned, pg_hole): (bool, bool) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = 'tdmr-r'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert!(
        !pg_pruned && !pg_hole,
        "the fail-fast must clear both persisted bits (topdown_pruned={pg_pruned}, \
         closure_hole={pg_hole})"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Negative companion to the two mixed-shape tests above: the reap
/// removes only PRODUCED children (dep1, completed via a cache hit and
/// orphaned when B2 goes away) while an UNBUILT child (dep2, kept by
/// live build B3) survives. R's child set still under-represents
/// nothing that was lost un-produced, so the reap-time re-evaluation
/// must NOT fail-fast: R stays Queued behind its surviving unbuilt
/// child, the mark stays, and B1 stays Active — dep2 completing later
/// (or being reaped unbuilt later) settles R through the established
/// paths. This pins the un-produced-reaped criterion against an
/// over-broad "any reap survivor fails fast" regression.
#[tokio::test]
async fn test_topdown_pruned_root_fail_fast_when_unproduced_child_reaped_but_produced_child_survives_negative_companion()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park detached fetches on the QPI gate (never released) so R stays
    // Substituting until its failure verdict is injected.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep1 with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless).
    let r_out = test_store_path("tdmn-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdmn-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let dep1_out = test_store_path("tdmn-dep1-out");
    let mk_dep1 = || {
        let mut n = make_node("tdmn-dep1");
        n.expected_output_paths = vec![dep1_out.clone()];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep1()],
        vec![make_test_edge("tdmn-r", "tdmn-dep1")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdmn-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // dep1's output becomes present upstream BEFORE B2 merges, so B2's
    // merge completes dep1 inline (cache hit) — dep1 is PRODUCED and
    // B2's sole interest. dep2 stays unbuilt (neither seeded nor
    // substitutable).
    store.seed_with_content(&dep1_out, b"dep1-out");
    let mk_dep2 = || {
        let mut n = make_node("tdmn-dep2");
        n.expected_output_paths = vec![test_store_path("tdmn-dep2-out")];
        n
    };

    // B2: full merge app→R→{dep1, dep2}. dep1 cache-hits to Completed;
    // dep2 stays unbuilt, so R keeps the mark.
    let mut app = make_node("tdmn-app");
    app.expected_output_paths = vec![test_store_path("tdmn-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep1(), mk_dep2()],
        vec![
            make_test_edge("tdmn-app", "tdmn-r"),
            make_test_edge("tdmn-r", "tdmn-dep1"),
            make_test_edge("tdmn-r", "tdmn-dep2"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdmn-dep1").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep1 is produced (cache hit) at B2's merge"
    );
    assert!(
        expect_drv(&handle, "tdmn-r").await.topdown_pruned,
        "fixture premise: dep2 is still unbuilt, so R keeps the mark"
    );

    // B3 registers interest in dep2 so it survives B2's reap unbuilt.
    let b3 = Uuid::new_v4();
    let _ev3 = merge_dag(&handle, b3, vec![mk_dep2()], vec![], false).await?;
    barrier(&handle).await;

    // R's parked walk fails while dep2 is still attached and unbuilt:
    // the fail-fast is suppressed, the mark kept, R parked Queued with
    // the one-shot flag set.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdmn-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdmn-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: the suppressed fail-fast parks R Queued behind its unbuilt child"
    );
    assert!(mid.topdown_pruned && mid.substitute_tried);

    // Cancel B2 and reap its sole-interest nodes: dep1 — PRODUCED — and
    // app. dep2 (unbuilt) survives via B3's interest, R via B1's.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdmn-dep1").await?.is_none(),
        "B2's sole-interest dep1 must be reaped (it was produced, then orphaned)"
    );
    assert!(
        handle.debug_query_derivation("tdmn-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdmn-dep2").await?.is_some(),
        "dep2 must survive the reap (B3 still holds interest in it)"
    );

    // Designed outcome: only PRODUCED children were reaped, so there is
    // no closure hole — no fail-fast. R waits Queued behind its live
    // unbuilt child with the mark kept, and B1 stays Active.
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 must NOT be failed: the reaped child was produced, and R's \
         surviving unbuilt child is still being driven by a live build"
    );
    assert_eq!(
        query_status(&handle, b3).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B3 must stay live (it still wants the surviving unbuilt dep2)"
    );
    let r = expect_drv(&handle, "tdmn-r").await;
    assert_eq!(
        r.status,
        DerivationStatus::Queued,
        "R must keep waiting behind its surviving unbuilt child, not be \
         fail-fasted or promoted; got {:?}",
        r.status
    );
    assert!(
        r.topdown_pruned,
        "the mark must be kept while an unbuilt child remains attached"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The `topdown_pruned` STAMP must treat a closure-holed kept node like
/// a childless one. Staging: BC full-merges R → D (D's output already
/// present upstream, so it cache-hits to Completed) and keeps both
/// alive; B2 full-merges app → R → {D, E} with E unbuilt and B2's sole
/// interest; B2 is cancelled and its cleanup reaps E — never produced —
/// out from under R, stamping the `closure_hole` breadcrumb while the
/// produced D survives. No prune has fired for R yet, so it is
/// unmarked. A NEW pruned merge (B3: R → dep3 with R's wanted output
/// substitutable upstream) then keeps {R}, drops dep3, and MUST stamp R
/// in memory and in PG: the surviving child set ({D}, produced) is a
/// reap-truncated view of R's input closure, so it must not exempt R
/// from the must-substitute guard any more than an empty set would.
/// Pre-classifier the stamp gates keyed on produced-children alone and
/// skipped the mark over the truncated survivor, leaving R guard-less
/// for the doomed from-source dispatch (bughunter round-20 bug_010).
#[tokio::test]
async fn test_topdown_stamp_fires_for_closure_holed_node_with_produced_survivors() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park any detached fetch (B3's walk for R below) on the QPI gate
    // so no SubstituteComplete verdict can race the stamp assertions.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    let r_out = test_store_path("tdsh-r-out");
    let d_out = test_store_path("tdsh-d-out");
    let mk_r = || {
        let mut n = make_node("tdsh-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_d = || {
        let mut n = make_node("tdsh-d");
        n.expected_output_paths = vec![d_out.clone()];
        n
    };
    let mk_e = || {
        let mut n = make_node("tdsh-e");
        n.expected_output_paths = vec![test_store_path("tdsh-e-out")];
        n
    };

    // BC: full merge R → D. D's output is already present upstream, so
    // it cache-hits to Completed at merge; R's output is neither
    // present nor substitutable yet, so no prune fires and R carries no
    // mark. BC keeps R and D alive across B2's reap below.
    store.seed_with_content(&d_out, b"d-out");
    let bc = Uuid::new_v4();
    let _evc = merge_dag(
        &handle,
        bc,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdsh-r", "tdsh-d")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdsh-d").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: D is produced at BC's merge"
    );
    assert!(
        !expect_drv(&handle, "tdsh-r").await.topdown_pruned,
        "fixture premise: no prune has fired for R yet"
    );

    // B2: full merge app → R → {D, E}; E (unbuilt) and app are B2's
    // sole interest.
    let mut app = make_node("tdsh-app");
    app.expected_output_paths = vec![test_store_path("tdsh-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_d(), mk_e()],
        vec![
            make_test_edge("tdsh-app", "tdsh-r"),
            make_test_edge("tdsh-r", "tdsh-d"),
            make_test_edge("tdsh-r", "tdsh-e"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Cancel B2 and reap its sole-interest nodes: E — never produced —
    // is reaped out from under R (closure hole), app goes with it; D
    // and R survive via BC's interest.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdsh-e").await?.is_none(),
        "B2's sole-interest unbuilt E must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdsh-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdsh-d").await?.is_some(),
        "D must survive the reap (BC still holds interest in it)"
    );
    assert!(
        !expect_drv(&handle, "tdsh-r").await.topdown_pruned,
        "fixture premise: R is still unmarked after the reap"
    );

    // B3: R's wanted output is now substitutable upstream → the prune
    // fires, keeps {R}, drops dep3.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_dep3 = || {
        let mut n = make_node("tdsh-dep3");
        n.expected_output_paths = vec![test_store_path("tdsh-dep3-out")];
        n
    };
    let b3 = Uuid::new_v4();
    let _ev3 = merge_dag(
        &handle,
        b3,
        vec![mk_r(), mk_dep3()],
        vec![make_test_edge("tdsh-r", "tdsh-dep3")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        query_status(&handle, b3).await?.total_derivations,
        1,
        "fixture premise: B3 took the roots-only prune path"
    );

    // Designed outcome: the closure-holed kept node must be stamped —
    // its produced survivor must not vouch for the dropped closure.
    assert!(
        expect_drv(&handle, "tdsh-r").await.topdown_pruned,
        "a closure-holed kept node must be stamped even though its surviving \
         children are all produced (was: stamp skipped over the reap-truncated \
         child set, leaving R guard-less for the doomed from-source dispatch)"
    );
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdsh-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg_pruned,
        "the stamp for a closure-holed kept node must also be persisted so a \
         failover cannot resurrect the unguarded shape"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The dispatch-time guards must treat a marked closure-holed survivor
/// like a childless one (bughunter round-20 merged_bug_001). Staging
/// follows that report's proof: B1's prune (wanting only R's `out`,
/// which is substitutable upstream) stamps R and parks its detached
/// fetch; BC produces dep2; B2 full-merges app → R → {dep1 unbuilt,
/// dep2 produced}; B4 (live) wants R's `debug` output — missing and
/// never substitutable. The parked walk reports ok with `debug`
/// forgiven → the forgiven-now-wanted downgrade parks R Queued with the
/// mark kept and `substitute_tried` deliberately left false. B2's
/// cancel then reaps the un-produced dep1 (closure hole); the reap
/// hook's fail-fast arm requires `substitute_tried`, so the promote arm
/// lifts R to Ready over its produced survivor — the merged_bug_001
/// exit shape: Ready, marked, holed, NOT childless, no walk pending.
/// At the next dispatch pass R's wanted `debug` output can neither
/// complete inline nor route to substitution; the guards must take the
/// resubmit-directing fail-fast instead of assigning R from source over
/// the reap-truncated child set (pre-classifier they keyed on literal
/// childlessness and handed R to the worker).
#[tokio::test]
async fn test_topdown_pruned_holed_survivor_fails_fast_at_dispatch_not_assigned_from_source()
-> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park R's detached fetch on the QPI gate (never released) so its
    // verdict is injected manually below.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // R declares two outputs: `out` is substitutable upstream (B1's
    // narrow wanted set, so the prune fires); `debug` is missing and
    // never substitutable (the output the walk forgives and a later
    // build then wants).
    let r_out = test_store_path("tdhd-r-out");
    let r_dbg = test_store_path("tdhd-r-debug");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = |wanted: Vec<String>| {
        let mut n = make_node("tdhd-r");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![r_out.clone(), r_dbg.clone()];
        n.wanted_output_names = wanted;
        n
    };
    let mk_dep1 = || {
        let mut n = make_node("tdhd-dep1");
        n.expected_output_paths = vec![test_store_path("tdhd-dep1-out")];
        n
    };

    // B1: root → dep1, wanting only R's `out` → the prune fires, keeps
    // {R} (stamped, childless), drops dep1, and parks R's fetch.
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(vec!["out".into()]), mk_dep1()],
        vec![make_test_edge("tdhd-r", "tdhd-dep1")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdhd-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // BC: dep2 is produced by an unrelated build and survives B2's
    // later reap via BC's interest.
    let dep2_out = test_store_path("tdhd-dep2-out");
    store.seed_with_content(&dep2_out, b"dep2-out");
    let mk_dep2 = || {
        let mut n = make_node("tdhd-dep2");
        n.expected_output_paths = vec![dep2_out.clone()];
        n
    };
    let bc = Uuid::new_v4();
    let _evc = merge_dag(&handle, bc, vec![mk_dep2()], vec![], false).await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdhd-dep2").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep2 is produced before B2 attaches it to R"
    );

    // B2: full merge app → R → {dep1 unbuilt (B2's sole interest),
    // dep2 produced (shared with BC)}. The mark survives.
    let mut app = make_node("tdhd-app");
    app.expected_output_paths = vec![test_store_path("tdhd-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(vec!["out".into()]), mk_dep1(), mk_dep2()],
        vec![
            make_test_edge("tdhd-app", "tdhd-r"),
            make_test_edge("tdhd-r", "tdhd-dep1"),
            make_test_edge("tdhd-r", "tdhd-dep2"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdhd-r").await.topdown_pruned,
        "fixture premise: a merge whose added children are not all produced keeps the mark"
    );

    // B4: a live build that wants R's `debug` output — the output the
    // parked walk's spawn-time forgivable set could forgive.
    let b4 = Uuid::new_v4();
    let _ev4 = merge_dag(&handle, b4, vec![mk_r(vec!["debug".into()])], vec![], false).await?;
    barrier(&handle).await;

    // R's parked walk reports success but with `debug` forgiven (it was
    // unwanted at spawn time). A live build now wants it → the
    // forgiven-now-wanted downgrade: lazy clear and fail-fast are both
    // skipped, R parks Queued behind its unbuilt child with the mark
    // kept and `substitute_tried` deliberately left false.
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdhd-r".into(),
            ok: true,
            forgiven: vec![r_dbg.clone()],
        })
        .await?;
    barrier(&handle).await;
    let mid = expect_drv(&handle, "tdhd-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Queued,
        "fixture premise: the forgiven-now-wanted downgrade parks R Queued behind dep1"
    );
    assert!(
        mid.topdown_pruned && !mid.substitute_tried,
        "fixture premise: mark kept, one-shot flag deliberately not set by the downgrade"
    );

    // Cancel B2 and reap its sole-interest nodes: dep1 — never produced
    // — is reaped out from under R (closure hole), app goes with it;
    // dep2 survives via BC. The reap hook's fail-fast arm requires
    // `substitute_tried`, so it skips R; the promote arm lifts the
    // vacuously dep-complete R to Ready over its produced survivor.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdhd-dep1").await?.is_none(),
        "B2's sole-interest unbuilt dep1 must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdhd-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdhd-dep2").await?.is_some(),
        "dep2 must survive the reap (BC still holds interest in it)"
    );
    let staged = expect_drv(&handle, "tdhd-r").await;
    assert_eq!(
        staged.status,
        DerivationStatus::Ready,
        "fixture premise: the reap-time promote arm lifts R Ready over its produced survivor"
    );
    assert!(
        staged.topdown_pruned,
        "fixture premise: R is still marked when it reaches Ready"
    );

    // A builder is available — the doomed from-source dispatch has
    // somewhere to go if the dispatch-time guards ignore the hole.
    let mut worker_rx = connect_executor(&handle, "tdhd-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // Designed outcome: R's wanted `debug` output is missing upstream
    // and not substitutable, so the probes can neither complete it
    // inline nor route it to substitution — the holed survivor must
    // take the resubmit-directing fail-fast, NOT a from-source
    // assignment over the reap-truncated child set.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "a marked closure-holed survivor must never be dispatched from source \
             (its pruned closure was never merged — the worker would ENOENT)"
        );
    }
    let r = expect_drv(&handle, "tdhd-r").await;
    assert!(
        !matches!(
            r.status,
            DerivationStatus::Ready | DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "R must not be left dispatchable from source after the closure hole; got {:?}",
        r.status
    );
    assert!(
        !r.topdown_pruned,
        "the fail-fast consumes (clears) the mark it acted on"
    );
    for (label, b) in [("B1", b1), ("B4", b4)] {
        let s = query_status(&handle, b).await?;
        assert_eq!(
            s.state,
            rio_proto::types::BuildState::Failed as i32,
            "{label} must fail via the resubmit-directing fail-fast; got state={} error={:?}",
            s.state,
            s.error_summary
        );
        assert!(
            s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
            "{label}'s error summary should direct resubmit; got {:?}",
            s.error_summary
        );
    }
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Guard companion: a closure hole alone — no `topdown_pruned` mark —
/// must not defer or fail-fast anything at dispatch time. Same staging
/// as the stamp test above (BC keeps R + D, B2's cancel reaps the
/// un-produced E out from under R) but no prune ever fires for R: its
/// wanted output is neither present nor substitutable, so the probes
/// leave it Ready and the generic dispatch hands it to a worker from
/// source — the correct outcome for an unmarked node whose closure no
/// prune ever dropped. Pins that the must-substitute judgment requires
/// the mark, not just broken closure evidence.
#[tokio::test]
async fn test_unmarked_closure_holed_node_still_dispatches_from_source() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdnm-r-out");
    let d_out = test_store_path("tdnm-d-out");
    let mk_r = || {
        let mut n = make_node("tdnm-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_d = || {
        let mut n = make_node("tdnm-d");
        n.expected_output_paths = vec![d_out.clone()];
        n
    };
    let mk_e = || {
        let mut n = make_node("tdnm-e");
        n.expected_output_paths = vec![test_store_path("tdnm-e-out")];
        n
    };

    // BC: full merge R → D; D cache-hits to Completed, R stays unbuilt
    // and unmarked (its output is neither present nor substitutable).
    store.seed_with_content(&d_out, b"d-out");
    let bc = Uuid::new_v4();
    let _evc = merge_dag(
        &handle,
        bc,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdnm-r", "tdnm-d")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdnm-d").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: D is produced at BC's merge"
    );

    // B2: full merge app → R → {D, E}; E (unbuilt) and app are B2's
    // sole interest. Cancel + cleanup reaps E un-produced out from
    // under R (closure hole) while the produced D survives via BC.
    let mut app = make_node("tdnm-app");
    app.expected_output_paths = vec![test_store_path("tdnm-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_d(), mk_e()],
        vec![
            make_test_edge("tdnm-app", "tdnm-r"),
            make_test_edge("tdnm-r", "tdnm-d"),
            make_test_edge("tdnm-r", "tdnm-e"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdnm-e").await?.is_none(),
        "B2's sole-interest unbuilt E must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdnm-d").await?.is_some(),
        "D must survive the reap (BC still holds interest in it)"
    );
    assert!(
        !expect_drv(&handle, "tdnm-r").await.topdown_pruned,
        "fixture premise: R is unmarked (no prune ever fired for it)"
    );

    // A builder connects; the next dispatch pass must hand R to it.
    let mut worker_rx = connect_executor(&handle, "tdnm-w", "x86_64-linux").await?;
    tick(&handle).await?;
    let assn = recv_assignment(&mut worker_rx).await;
    assert_eq!(
        assn.drv_path,
        test_drv_path("tdnm-r"),
        "an unmarked node with a closure hole and produced survivors must still \
         dispatch from source (no prune ever dropped its closure)"
    );
    assert_eq!(
        expect_drv(&handle, "tdnm-r").await.status,
        DerivationStatus::Assigned,
        "R must be assigned to the worker, not deferred or fail-fasted"
    );
    assert_eq!(
        query_status(&handle, bc).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "BC must stay Active (no spurious resubmit-directing fail-fast)"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The closure-hole VETO on the produced-children clears, and the HEAL
/// that lifts it. Staging: B1's prune stamps R and parks its detached
/// fetch on the QPI gate; B3 (a live single-node build) holds the
/// unbuilt dep2; B2 full-merges app→R→{dep1 unbuilt, dep2 unbuilt}; B2
/// is then cancelled and its sole-interest dep1 — never produced — is
/// reaped out from under R, stamping the `closure_hole` breadcrumb on
/// R while the unbuilt dep2 survives via B3. R's walk stays IN FLIGHT
/// across the reap on purpose: had its verdict already landed (R
/// parked Queued with `substitute_tried`), the reap-time re-evaluation
/// would settle the holed survivor immediately via the
/// resubmit-directing fail-fast — that ordering is pinned by the
/// ordering-A test above. The veto exists for the survivor that is NOT
/// yet settled when its remaining children become produced.
///
/// Phase A (the completion-time veto): dep2 completes via its own
/// substitution, so R's surviving children are now all produced — but
/// they are a reap-truncated view of R's pruned closure.
/// `clear_topdown_pruned_for_produced_parents` must skip the holed
/// parent: the mark stays, in memory AND in PG, R is not promoted to
/// Ready/Assigned/Running (it stays parked on its own walk, so no
/// from-source WorkAssignment can be cut for it), and B1 stays Active.
/// Without the veto the mark would be dropped here and R's eventual
/// walk failure would revert it Ready over the truncated child set —
/// the doomed from-source dispatch this machinery exists to prevent.
///
/// Phase B (the heal): B4 full-merges app2→R→dep2, re-declaring R's
/// real edge set. The post-reconciliation pass in `handle_merge_dag`
/// heals the breadcrumb (R's child set is representative again) and,
/// with dep2 produced, clears the mark in memory and PG — no fail-fast
/// or resubmit needed once a full merge has re-supplied the closure.
#[tokio::test]
async fn test_topdown_pruned_kept_after_closure_hole_until_full_remerge_heals() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Park every detached fetch on the QPI gate (never released): R's
    // walk must still be in flight when the closure hole is created,
    // and dep2's fetch is settled by an injected verdict instead.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // B1: root → dep1 with the root's wanted output substitutable
    // upstream → the prune fires, keeps {R} (stamped, childless),
    // drops dep1.
    let r_out = test_store_path("tdch-r-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());
    let mk_r = || {
        let mut n = make_node("tdch-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_dep1 = || {
        let mut n = make_node("tdch-dep1");
        n.expected_output_paths = vec![test_store_path("tdch-dep1-out")];
        n
    };
    let b1 = Uuid::new_v4();
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_dep1()],
        vec![make_test_edge("tdch-r", "tdch-dep1")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );
    let pre = expect_drv(&handle, "tdch-r").await;
    assert!(
        pre.topdown_pruned,
        "fixture premise: R stamped by the prune"
    );
    assert_eq!(
        pre.status,
        DerivationStatus::Substituting,
        "fixture premise: R's detached fetch is parked on the QPI gate"
    );

    // B3: dep2's output is substitutable upstream too, so its
    // single-node merge routes it to substitution and its own detached
    // fetch parks on the same gate — dep2 is UNBUILT and kept alive by
    // a live build across B2's later reap.
    let dep2_out = test_store_path("tdch-dep2-out");
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(dep2_out.clone());
    let mk_dep2 = || {
        let mut n = make_node("tdch-dep2");
        n.expected_output_paths = vec![dep2_out.clone()];
        n
    };
    let b3 = Uuid::new_v4();
    let _ev3 = merge_dag(&handle, b3, vec![mk_dep2()], vec![], false).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "tdch-dep2").await.status,
        DerivationStatus::Substituting,
        "fixture premise: dep2 is unbuilt, parked on its own detached fetch"
    );

    // B2: a full merge that gives R BOTH children — dep1 unbuilt (B2's
    // sole interest) and dep2 unbuilt (shared with live B3). app's
    // output is not substitutable → no prune for B2, R keeps the mark.
    let mut app = make_node("tdch-app");
    app.expected_output_paths = vec![test_store_path("tdch-app-out")];
    let b2 = Uuid::new_v4();
    let _ev2 = merge_dag(
        &handle,
        b2,
        vec![app, mk_r(), mk_dep1(), mk_dep2()],
        vec![
            make_test_edge("tdch-app", "tdch-r"),
            make_test_edge("tdch-r", "tdch-dep1"),
            make_test_edge("tdch-r", "tdch-dep2"),
        ],
        false,
    )
    .await?;
    assert!(
        expect_drv(&handle, "tdch-r").await.topdown_pruned,
        "fixture premise: a merge whose added children are not all produced keeps the mark"
    );

    // Cancel B2 and reap its sole-interest nodes WHILE R's walk is
    // still parked: dep1 — never produced — is reaped out from under R
    // (closure hole), app goes with it; dep2 survives via B3's
    // interest, R via B1's. The reap-time re-evaluation defers to R's
    // in-flight walk, so R stays Substituting with the mark and B1
    // stays Active.
    let (ctx, crx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: b2,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: ctx,
        })
        .await?;
    assert!(crx.await??, "B2 cancel must be accepted");
    handle
        .send_unchecked(ActorCommand::CleanupTerminalBuild { build_id: b2 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdch-dep1").await?.is_none(),
        "B2's sole-interest unbuilt dep1 must be reaped (closure hole on R)"
    );
    assert!(
        handle.debug_query_derivation("tdch-app").await?.is_none(),
        "B2's sole-interest app must be reaped"
    );
    assert!(
        handle.debug_query_derivation("tdch-dep2").await?.is_some(),
        "dep2 must survive the reap (B3 still holds interest in it)"
    );
    let mid = expect_drv(&handle, "tdch-r").await;
    assert_eq!(
        mid.status,
        DerivationStatus::Substituting,
        "fixture premise: the reap must not settle a survivor whose walk is in flight"
    );
    assert!(
        mid.topdown_pruned,
        "fixture premise: the mark is kept across the reap"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "fixture premise: B1 is not failed while R's walk is still in flight"
    );

    // Phase A — the closure-hole veto on the completion-time clear.
    // dep2's own fetch succeeds (output seeded so later merges see it
    // present; verdict injected past the armed gate): R's surviving
    // children are now all produced, but the completion-time clear
    // must NOT trust that reap-truncated view.
    store.seed_with_content(&dep2_out, b"dep2-out");
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdch-dep2".into(),
            ok: true,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    assert!(
        matches!(
            expect_drv(&handle, "tdch-dep2").await.status,
            DerivationStatus::Completed | DerivationStatus::Skipped
        ),
        "fixture premise: dep2 is produced after its substitution"
    );
    let holed = expect_drv(&handle, "tdch-r").await;
    assert!(
        holed.topdown_pruned,
        "the completion-time clear must skip a closure-holed parent: its \
         surviving produced child is a reap-truncated view of the pruned \
         closure (was: mark dropped → a later walk failure reverts R Ready \
         for the doomed from-source dispatch)"
    );
    let (pg_marked,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdch-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg_marked,
        "the persisted mark must survive the completion too — the column is \
         what a failover restores, and a holed parent restored unmarked would \
         be eligible for the doomed from-source dispatch"
    );
    assert_eq!(
        holed.status,
        DerivationStatus::Substituting,
        "R must stay parked on its own walk — not promoted Ready/Assigned/\
         Running over the truncated child set; got {:?}",
        holed.status
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 must stay Active: nothing may settle R off the truncated child set"
    );

    // Phase B — the heal. B4 full-merges R with its real edge set
    // (app2 → R → dep2; app2's output is not substitutable, so no
    // prune). Re-declaring R's edges makes its child set representative
    // again: the post-reconciliation pass heals the breadcrumb and,
    // with dep2 produced, clears the mark in memory and PG.
    let mut app2 = make_node("tdch-app2");
    app2.expected_output_paths = vec![test_store_path("tdch-app2-out")];
    let b4 = Uuid::new_v4();
    let _ev4 = merge_dag(
        &handle,
        b4,
        vec![app2, mk_r(), mk_dep2()],
        vec![
            make_test_edge("tdch-app2", "tdch-r"),
            make_test_edge("tdch-r", "tdch-dep2"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;
    let healed = expect_drv(&handle, "tdch-r").await;
    assert!(
        !healed.topdown_pruned,
        "a full merge that re-declares R's edges heals the closure hole, and \
         with its (now representative) children all produced the mark must be \
         cleared in memory"
    );
    let (pg_marked_after_heal,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdch-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg_marked_after_heal,
        "the post-reconciliation clear must reach PG as well, so a failover \
         cannot resurrect the mark after the heal"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The lazy clear in `handle_substitute_complete`: when a pruned root's
/// children are ALL produced by the time its own walk fails, the mark is
/// moot — cleared in memory AND in PG (best-effort, so a failover cannot
/// resurrect it) — and the node takes the normal revert, not the
/// fail-fast.
#[tokio::test]
async fn test_topdown_pruned_lazy_clear_when_children_produced_at_walk_failure() -> TestResult {
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // Full merge: R → dep (nothing substitutable → no prune).
    let mut r = make_node("tdlazy-r");
    r.expected_output_paths = vec![test_store_path("tdlazy-r-out")];
    let mut dep = make_node("tdlazy-dep");
    dep.expected_output_paths = vec![test_store_path("tdlazy-dep-out")];
    let b = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        b,
        vec![r, dep],
        vec![make_test_edge("tdlazy-r", "tdlazy-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Stage: R was pruned by an earlier build and is mid-fetch; its
    // child has since been produced; the persisted flag is still set
    // (as it would be after a pruned merge whose children arrived
    // unbuilt and completed later).
    handle
        .debug_force_status("tdlazy-dep", DerivationStatus::Completed)
        .await?;
    handle
        .debug_force_status("tdlazy-r", DerivationStatus::Substituting)
        .await?;
    handle.debug_set_topdown_pruned("tdlazy-r", true).await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'tdlazy-r'")
        .execute(&db.pool)
        .await?;

    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tdlazy-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "tdlazy-r").await;
    assert!(
        !post.topdown_pruned,
        "children all produced → the lazy clear must drop the mark"
    );
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "normal revert (closure is produced, building is legitimate); got {:?}",
        post.status
    );
    let s = query_status(&handle, b).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Active as i32,
        "no fail-fast — the closure is produced"
    );
    let (pg,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdlazy-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg,
        "the lazy clear must also clear the persisted flag so a failover \
         cannot resurrect the doomed-dispatch guard for a produced closure"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The clear decision must be taken AFTER `verify_preexisting_completed`
/// (phase 6c) has had its say: a merge that re-adds the edge R → C while
/// C is `Completed` in the DAG but C's recorded output is gone from the
/// store (and not substitutable) must NOT clear R's `topdown_pruned`
/// mark — 6c demotes C back to Ready in the same merge, so R's closure
/// is NOT in the store and dropping the guard would re-open the doomed
/// from-source dispatch the mark exists to prevent. A clear computed
/// before 6c (against the stale Completed status) would be laundered by
/// exactly the child this merge is about to demote. The mark must
/// survive in memory AND in PG (the column is what a failover restores).
#[tokio::test]
async fn test_topdown_pruned_kept_when_merge_child_is_stale_completed() -> TestResult {
    let (db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // Full merge: R → C (nothing substitutable → no prune fires).
    let c_out = test_store_path("tdstale-c-out");
    let mk_r = || {
        let mut n = make_node("tdstale-r");
        n.expected_output_paths = vec![test_store_path("tdstale-r-out")];
        n
    };
    let mk_c = || {
        let mut n = make_node("tdstale-c");
        n.expected_output_paths = vec![c_out.clone()];
        n
    };
    let b1 = Uuid::new_v4();
    merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_c()],
        vec![make_test_edge("tdstale-r", "tdstale-c")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Stage: C completed earlier and its recorded output has since been
    // GC'd (never seeded in the mock store, not substitutable); R was
    // topdown-pruned by an earlier build and is mid-fetch, mark set in
    // memory and in PG — the post-pruned-merge shape (mirrors the
    // lazy-clear test's staging above).
    handle
        .debug_force_status("tdstale-c", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("tdstale-c", vec![c_out.clone()])
        .await?;
    handle
        .debug_force_status("tdstale-r", DerivationStatus::Substituting)
        .await?;
    handle.debug_set_topdown_pruned("tdstale-r", true).await?;
    sqlx::query("UPDATE derivations SET topdown_pruned = true WHERE drv_hash = 'tdstale-r'")
        .execute(&db.pool)
        .await?;

    // B2: a full merge that re-adds the edge R → C. C is a pre-existing
    // Completed candidate whose recorded output is missing and not
    // substitutable → phase 6c demotes it in this very merge.
    let b2 = Uuid::new_v4();
    merge_dag(
        &handle,
        b2,
        vec![mk_r(), mk_c()],
        vec![make_test_edge("tdstale-r", "tdstale-c")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Fixture premise: the stale-Completed child WAS demoted by 6c.
    let c_post = expect_drv(&handle, "tdstale-c").await;
    assert!(
        matches!(
            c_post.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "fixture premise: stale-Completed child (output GC'd, not \
         substitutable) must be demoted by verify_preexisting_completed; \
         got {:?}",
        c_post.status
    );

    // The mark must survive: the child this merge re-added is NOT
    // produced (it was demoted in the same merge), so the clear
    // criterion does not hold once 6c has run.
    assert!(
        expect_drv(&handle, "tdstale-r").await.topdown_pruned,
        "a merge whose re-added child is stale-Completed (demoted by this \
         same merge) must NOT clear the in-memory topdown_pruned mark — \
         R's closure is not in the store"
    );
    let (pg,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdstale-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        pg,
        "the persisted mark must survive too — clearing it against a \
         stale-Completed child would let a failover hand R a doomed \
         from-source dispatch"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// A prune-led merge that fails at the PG-persist step (step 5) must
/// not leave `topdown_pruned=true` on a pre-existing childless root
/// shared with an unrelated live build. `cleanup_failed_merge` →
/// `rollback_merge` reverts only the fields tracked in `MergeResult`;
/// the stamp is not among them and its only clearing site requires the
/// node to HAVE children — so a stamp applied before the fallible
/// persist steps would leak the rejected build's prune verdict onto
/// B1's root R, and a later routine `SubstituteComplete{ok=false}` for
/// R would take `handle_substitute_complete`'s fail-fast arm and
/// terminally fail B1 with the "deps were pruned; resubmit" error
/// instead of the normal revert-to-Ready fallback.
///
/// Step 5 is failed deterministically without a fault hook: the new
/// node S carries a NUL byte in `pname`. PG rejects NUL in text
/// values, so `batch_upsert_derivations` (the first write inside
/// `persist_and_activate`) errors; nothing in steps 0–4 reads pname,
/// so the merge reaches step 5 intact (the Database-error assertion
/// plus the prune counter pin that down).
#[tokio::test]
async fn test_topdown_stamp_not_leaked_when_merge_fails_at_persist() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // B1: single-node submission containing only root R. Nothing is
    // substitutable yet → no prune, no detached fetch; R seeds Ready
    // (no executors connected) and B1 stays Active.
    let r_out = test_store_path("tds-r-out");
    let mut r_b1 = make_node("tds-r");
    r_b1.expected_output_paths = vec![r_out.clone()];
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![r_b1], vec![], false).await?;
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "precondition: B1 live and Active"
    );
    assert!(
        !expect_drv(&handle, "tds-r").await.topdown_pruned,
        "precondition: B1's plain merge does not stamp R"
    );

    // Stage the store so B2's submission satisfies the roots-only
    // prune criterion: R's and S's wanted outputs substitutable.
    let s_out = test_store_path("tds-s-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(r_out.clone());
        subs.push(s_out.clone());
    }

    // B2: {R, S, S-dep} with S→S-dep. The prune fires (drops S-dep),
    // then persist fails on S's NUL pname.
    let mut r_b2 = make_node("tds-r");
    r_b2.expected_output_paths = vec![r_out.clone()];
    let mut s = make_node("tds-s");
    s.expected_output_paths = vec![s_out.clone()];
    s.pname = "tds-s\0pg-rejects-nul".into();
    let mut s_dep = make_node("tds-s-dep");
    s_dep.expected_output_paths = vec![test_store_path("tds-s-dep-out")];

    let b2 = Uuid::new_v4();
    let reply = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b2,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![r_b2, s, s_dep],
            edges: vec![make_test_edge("tds-s", "tds-s-dep")],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await;
    assert!(
        matches!(
            reply.as_ref().err().and_then(|e| e.downcast_ref()),
            Some(ActorError::Database(_))
        ),
        "B2's merge must be rejected by the step-5 persist failure, got {reply:?}"
    );
    // Guard against the scenario silently degrading: the prune must
    // actually have fired before the persist failure.
    assert_eq!(
        recorder.get("rio_scheduler_topdown_prune_total{}"),
        1,
        "B2's submission should have taken the roots-only prune path"
    );

    // The failed merge rolled back: S gone, B2 unknown, B1 untouched.
    assert!(
        handle.debug_query_derivation("tds-s").await?.is_none(),
        "rollback removes B2's newly-inserted node"
    );
    assert!(
        matches!(
            try_query_status(&handle, b2).await?,
            Err(ActorError::BuildNotFound(_))
        ),
        "rejected B2 should be unknown after rollback"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 unaffected by B2's failed merge"
    );

    // Load-bearing: the rejected merge's prune verdict must not survive
    // on the shared pre-existing root.
    let r_post = expect_drv(&handle, "tds-r").await;
    assert!(
        !r_post.topdown_pruned,
        "failed merge must not leave topdown_pruned on pre-existing root R \
         (rollback_merge does not revert the stamp)"
    );

    // The harm a leaked stamp causes: a later substitute failure for R
    // (fetched on behalf of B1) would hit the topdown fail-fast arm —
    // R is childless — and terminally fail B1 with the resubmit error.
    // Stage R mid-fetch and deliver ok=false; B1 must survive via the
    // normal revert.
    handle
        .debug_force_status("tds-r", DerivationStatus::Substituting)
        .await?;
    handle
        .send_unchecked(ActorCommand::SubstituteComplete {
            drv_hash: "tds-r".into(),
            ok: false,
            forgiven: vec![],
        })
        .await?;
    barrier(&handle).await;
    let s1 = query_status(&handle, b1).await?;
    assert_eq!(
        s1.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 must not be terminally failed by the topdown fail-fast arm \
         after B2's rejected merge; error_summary={:?}",
        s1.error_summary
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The `topdown_pruned` marker must land only on kept nodes whose
/// dependency closure the prune actually dropped. A dep-less demanded
/// leaf (here: one target of a multi-target submission with no
/// inputDrvs of its own) never had a closure to drop — a from-source
/// dispatch of it would succeed — so marking it would only convert a
/// routine substitute failure into a wrongful resubmit-directing
/// terminal failure.
#[tokio::test]
async fn test_topdown_stamp_only_nodes_whose_closure_was_dropped() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Multi-target-style submission: R → D (R's closure), plus the
    // dep-less leaf target L. Both demanded outputs substitutable → the
    // prune fires and keeps {R, L}; D is dropped.
    let r_out = test_store_path("tdl-r-out");
    let l_out = test_store_path("tdl-l-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(r_out.clone());
        subs.push(l_out.clone());
    }
    let mut r = make_node("tdl-r");
    r.expected_output_paths = vec![r_out.clone()];
    let mut l = make_node("tdl-l");
    l.expected_output_paths = vec![l_out.clone()];
    let mut d = make_node("tdl-d");
    d.expected_output_paths = vec![test_store_path("tdl-d-out")];

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![r, l, d],
        vec![make_test_edge("tdl-r", "tdl-d")],
        false,
    )
    .await?;
    assert_eq!(
        query_status(&handle, build_id).await?.total_derivations,
        2,
        "fixture premise: the prune fired and kept the demand set {{R, L}}"
    );
    assert!(
        handle.debug_query_derivation("tdl-d").await?.is_none(),
        "fixture premise: R's dep was dropped from the submission"
    );

    // R lost its dependency closure → marked, in memory and in PG.
    assert!(
        expect_drv(&handle, "tdl-r").await.topdown_pruned,
        "kept root whose closure was dropped must be stamped"
    );
    let (r_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdl-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(r_pruned, "kept root's stamp must be persisted");

    // L never had a closure to drop → NOT marked anywhere; building it
    // from source stays a valid fallback.
    assert!(
        !expect_drv(&handle, "tdl-l").await.topdown_pruned,
        "dep-less kept leaf must not be stamped in memory (it has no \
         dropped closure; from-source dispatch of it is valid)"
    );
    let (l_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdl-l'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !l_pruned,
        "dep-less kept leaf must not be persisted as pruned"
    );

    // Let the detached fetches settle before teardown.
    settle_substituting(&handle, &["tdl-r", "tdl-l"]).await;
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// A pruned merge whose build-activation write fails must reject the
/// build AND leave nothing of the merge behind in PG — in particular no
/// `topdown_pruned = true` on a shared pre-existing childless root.
/// Pre-fix the merge transaction (with the stamp) committed first and
/// the Pending→Active flip ran as a separate statement: a transient
/// failure there rejected the build but the stamp survived in PG, ready
/// to be restored onto a childless node at the next failover and turn a
/// routine substitute failure into a wrongful terminal fail-fast.
///
/// The activation failure is injected with a test-installed PG trigger
/// that rejects exactly the `builds.status → 'active'` flip (B1 is
/// already active when the trigger is created, so only B2's activation
/// can hit it) — no production fault seam, and the failing statement is
/// precisely the one that used to run outside the transaction.
#[tokio::test]
async fn test_topdown_stamp_rolled_back_when_activation_fails() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    // B1: plain merge of root R only — childless, nothing substitutable
    // → no prune, R seeds Ready, B1 Active. R is the shared node a
    // rejected later merge must not leave stamped.
    let r_out = test_store_path("tda-r-out");
    let mut r_b1 = make_node("tda-r");
    r_b1.expected_output_paths = vec![r_out.clone()];
    let b1 = Uuid::new_v4();
    merge_dag(&handle, b1, vec![r_b1], vec![], false).await?;
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "precondition: B1 live and Active"
    );
    // Positive control for the in-transaction activation: a committed
    // merge implies the builds row is already 'active' in PG (B1 has no
    // substitutable outputs and no workers, so nothing can advance it
    // past Active before this query).
    let (b1_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id = $1")
        .bind(b1)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        b1_status, "active",
        "committed merge must leave the builds row Active in PG"
    );

    // Make B2's demand set substitutable so the prune fires.
    let s_out = test_store_path("tda-s-out");
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.push(r_out.clone());
        subs.push(s_out.clone());
    }

    // Deterministic activation failure: reject any builds-row flip to
    // 'active' from now on. B1 is already active; the only statement
    // this can hit is B2's activation.
    sqlx::raw_sql(
        "CREATE FUNCTION reject_activation() RETURNS trigger LANGUAGE plpgsql AS $$ \
         BEGIN RAISE EXCEPTION 'injected activation failure'; END $$; \
         CREATE TRIGGER reject_activation BEFORE UPDATE OF status ON builds \
         FOR EACH ROW WHEN (NEW.status = 'active') EXECUTE FUNCTION reject_activation();",
    )
    .execute(&db.pool)
    .await?;

    // B2: {R → R-dep, S → S-dep}; demand = {R, S}, both substitutable →
    // prune fires, keeps {R, S} (both had a dependency dropped) → the
    // persist transaction stamps them — and must then abort on the
    // activation statement.
    let mut r_b2 = make_node("tda-r");
    r_b2.expected_output_paths = vec![r_out.clone()];
    let mut r_dep = make_node("tda-r-dep");
    r_dep.expected_output_paths = vec![test_store_path("tda-r-dep-out")];
    let mut s = make_node("tda-s");
    s.expected_output_paths = vec![s_out.clone()];
    let mut s_dep = make_node("tda-s-dep");
    s_dep.expected_output_paths = vec![test_store_path("tda-s-dep-out")];

    let b2 = Uuid::new_v4();
    let reply = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: b2,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![r_b2, r_dep, s, s_dep],
            edges: vec![
                make_test_edge("tda-r", "tda-r-dep"),
                make_test_edge("tda-s", "tda-s-dep"),
            ],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await;
    assert!(
        matches!(
            reply.as_ref().err().and_then(|e| e.downcast_ref()),
            Some(ActorError::Database(_))
        ),
        "B2's merge must be rejected by the injected activation failure, got {reply:?}"
    );
    // Guard against the scenario silently degrading: the prune must
    // actually have fired before the activation failure.
    assert_eq!(
        recorder.get("rio_scheduler_topdown_prune_total{}"),
        1,
        "B2's submission should have taken the roots-only prune path"
    );

    // Load-bearing: the activation failure must take the WHOLE merge
    // transaction with it — the shared root's row keeps a clean marker.
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tda-r'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg_pruned,
        "rejected merge must not leave topdown_pruned persisted on the shared \
         pre-existing root (the stamp must roll back with the activation failure)"
    );
    // The rest of the merge rolled back with it.
    let s_rows: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM derivations WHERE drv_hash = 'tda-s'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        s_rows, 0,
        "B2's newly-inserted derivation rows must roll back with the failed activation"
    );
    let b2_status: Option<String> =
        sqlx::query_scalar("SELECT status FROM builds WHERE build_id = $1")
            .bind(b2)
            .fetch_optional(&db.pool)
            .await?;
    assert_ne!(
        b2_status.as_deref(),
        Some("active"),
        "a rejected build must never be active in PG"
    );

    // In-memory: stamp never applied (it runs only after a successful
    // persist), B2 unknown, B1 untouched.
    assert!(
        !expect_drv(&handle, "tda-r").await.topdown_pruned,
        "rejected merge must not leave the in-memory stamp either"
    );
    assert!(
        matches!(
            try_query_status(&handle, b2).await?,
            Err(ActorError::BuildNotFound(_))
        ),
        "rejected B2 should be unknown after rollback"
    );
    assert_eq!(
        query_status(&handle, b1).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B1 unaffected by B2's failed merge"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
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
    // r[sched.substitute.detached+5]: the bottom-up fetch is spawned; let
    // SubstituteComplete land before checking qpi_calls.
    settle_substituting(&handle, &["glibc-ft"]).await;
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.contains(&glibc_out),
        "bottom-up should fetch substitutable dep on fall-through; qpi_calls={qpi:?}"
    );

    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
/// Top-down negative: a root whose `wanted_output_names` matches NO
/// declared output (a client sending `drv^bogus` — the gateway does
/// not validate the root OutputsSpec against the drv's declared
/// outputs) resolves to an EMPTY wanted subset. The all-roots-
/// available check must NOT treat that as vacuously satisfied: the
/// root's output is missing from the store, so pruning would drop the
/// dependency closure from the submission and dispatch the root to a
/// builder whose inputs were never scheduled. A wanted set that
/// resolves to nothing must fall through to the full merge.
#[tokio::test]
async fn test_topdown_unresolvable_wanted_set_falls_through() -> TestResult {
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // Root output deliberately NOT seeded and NOT substitutable: with
    // an honest criterion the prune cannot fire. The bogus wanted name
    // matches no declared output, so the wanted subset is empty — a
    // vacuous `.any(missing)` would report "all available" and prune.
    let mut root = make_node("vac-app");
    root.output_names = vec!["out".into()];
    root.expected_output_paths = vec![test_store_path("vac-app-out")];
    root.wanted_output_names = vec!["bogus".into()];
    let mut dep = make_node("vac-dep");
    dep.expected_output_paths = vec![test_store_path("vac-dep-out")];

    let build_id = Uuid::new_v4();
    merge_dag(
        &handle,
        build_id,
        vec![root, dep],
        vec![make_test_edge("vac-app", "vac-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Full DAG merged — the dependency closure must survive.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.total_derivations, 2,
        "a root wanted set that resolves to no declared output must NOT \
         vacuously satisfy the all-roots-available prune — the dep \
         closure would be dropped and the root dispatched without its \
         inputs"
    );
    assert_eq!(
        expect_drv(&handle, "vac-dep").await.status,
        DerivationStatus::Ready,
        "the dep survived into the merged DAG and is schedulable"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
// r[verify sched.merge.wanted-outputs+2]
/// Top-down negative: a PRE-EXISTING root shared with a live build whose
/// effective wanted set is NOT satisfiable must refuse the prune, even
/// when the submitting build's own (narrower) wanted set is.
///
/// Build A merges `R → dep` wanting ALL of R's outputs (the empty
/// sentinel) while R's `debug` output is missing upstream → full merge,
/// R stays Queued, A stays Active (live). R's `out` output then becomes
/// substitutable upstream. Build B re-submits the same closure wanting
/// only `out`: against B's set alone every wanted root output is
/// available, but post-merge classification evaluates R against the
/// LIVE effective wanted set (A ∪ B = all), keeping R on the
/// from-source path — no substitute fetch, no `topdown_pruned`
/// fail-fast. Pruning B's deps would leave B's progress hostage to A
/// staying alive: A's cancellation sweeps the sole-interest deps and B
/// hangs on a Queued root. The prune criterion must therefore union
/// the submission's wanted set with the pre-existing root's live
/// effective wanted set and fall through to the full merge here.
#[tokio::test]
async fn test_topdown_prune_gated_on_live_effective_wanted_of_preexisting_root() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdl-r-out");
    let r_debug = test_store_path("tdl-r-debug");
    let mk_root = |wanted: &[&str]| {
        let mut r = make_node("tdl-r");
        r.output_names = vec!["out".into(), "debug".into()];
        r.expected_output_paths = vec![r_out.clone(), r_debug.clone()];
        r.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        r
    };
    let mk_dep = || {
        let mut d = make_node("tdl-dep");
        d.expected_output_paths = vec![test_store_path("tdl-dep-out")];
        d
    };

    // Build A: R → dep, wanting ALL outputs. Nothing substitutable yet
    // → no prune, full merge. R Queued (dep incomplete), dep Ready, A
    // Active (no worker connected).
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![mk_root(&[]), mk_dep()],
        vec![make_test_edge("tdl-r", "tdl-dep")],
        false,
    )
    .await?;
    assert_eq!(
        expect_drv(&handle, "tdl-r").await.status,
        DerivationStatus::Queued,
        "precondition: R pre-exists non-terminal with A's interest"
    );

    // Upstream gains R's `out` between the two submissions; `debug`
    // stays missing and unsubstitutable.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());

    // Build B: same closure, but wanting only `out` of R.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![mk_root(&["out"]), mk_dep()],
        vec![make_test_edge("tdl-r", "tdl-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let status_b = query_status(&handle, build_b).await?;
    assert_eq!(
        status_b.total_derivations, 2,
        "live build A still wants R's missing `debug` → the prune must \
         NOT fire on B's narrower wanted set; B keeps (and registers \
         interest in) its dependency closure"
    );

    // The point of refusing the prune: B's own dep interest keeps the
    // closure schedulable even after A goes away. Cancel A — the dep
    // must NOT be swept as a sole-interest node.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: build_a,
            caller_tenant: None,
            reason: "test: cancel the wide build".into(),
            reply: reply_tx,
        })
        .await?;
    assert!(reply_rx.await??, "cancel of active build A should apply");
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "tdl-dep").await.status,
        DerivationStatus::Ready,
        "B registered interest in the dep, so A's cancel sweep must not \
         take it down — B can still build R from source"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "B stays Active with a schedulable closure after A's cancel"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Top-down positive companion: a PRE-EXISTING root whose live
/// effective wanted set IS satisfiable keeps the prune. Same shape as
/// the negative test above, but build A wants only `out` too — the
/// union of live contributions resolves to paths that are all
/// available, so B's submission is still pruned to roots-only and
/// completes via the detached substitute fetch.
#[tokio::test]
async fn test_topdown_prune_fires_when_preexisting_roots_live_wanted_satisfiable() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tds-r-out");
    let r_debug = test_store_path("tds-r-debug");
    let mk_root = |wanted: &[&str]| {
        let mut r = make_node("tds-r");
        r.output_names = vec!["out".into(), "debug".into()];
        r.expected_output_paths = vec![r_out.clone(), r_debug.clone()];
        r.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        r
    };
    let mk_dep = || {
        let mut d = make_node("tds-dep");
        d.expected_output_paths = vec![test_store_path("tds-dep-out")];
        d
    };

    // Build A: R → dep, wanting only `out`. Nothing substitutable yet
    // → no prune, full merge, R Queued, A Active (live).
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![mk_root(&["out"]), mk_dep()],
        vec![make_test_edge("tds-r", "tds-dep")],
        false,
    )
    .await?;
    assert_eq!(
        expect_drv(&handle, "tds-r").await.status,
        DerivationStatus::Queued,
        "precondition: R pre-exists non-terminal with A's interest"
    );

    // Upstream gains R's `out`; the unwanted `debug` stays missing.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());

    // Build B: same closure, also wanting only `out`. Every output
    // wanted by a live build (A ∪ B = {out}) is available → the
    // optimization is preserved: deps pruned, roots-only merge.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![mk_root(&["out"]), mk_dep()],
        vec![make_test_edge("tds-r", "tds-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    assert_eq!(
        query_status(&handle, build_b).await?.total_derivations,
        1,
        "all live builds' wanted outputs of the pre-existing root are \
         available → the roots-only prune still fires"
    );

    // The pruned build still completes via the detached fetch (the
    // missing-but-unwanted `debug` is forgiven, not a failure).
    settle_substituting(&handle, &["tds-r"]).await;
    assert_eq!(
        query_status(&handle, build_b).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "pruned root substitutes successfully → B succeeds"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
// r[verify sched.merge.wanted-outputs+2]
/// Top-down negative: the submission's OWN root selector resolving to
/// no declared output (`drv^bogus`) blocks the prune even when the
/// root pre-exists and its STORED wanted union is fully available.
///
/// The pre-existing root's only prior interested build is gone
/// (cancelled), so the prune criterion's effective-wanted lookup falls
/// back to the stored union — which resolves and is substitutable.
/// But post-merge classification evaluates the root against the
/// now-live submitter's own contribution, and an unresolvable set is
/// unclassifiable: nothing would substitute, while the submitter had
/// already pruned away its own dependency closure. The own-set
/// resolvability guard must keep the fall-through-to-full-merge
/// behavior of `test_topdown_unresolvable_wanted_set_falls_through`
/// for pre-existing roots too.
#[tokio::test]
async fn test_topdown_unresolvable_wanted_set_falls_through_on_preexisting_root() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdo-r-out");
    let mk_root = |wanted: &[&str]| {
        let mut r = make_node("tdo-r");
        r.output_names = vec!["out".into()];
        r.expected_output_paths = vec![r_out.clone()];
        r.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        r
    };

    // Build A seeds R into the DAG wanting `out` (the stored union),
    // then is cancelled: R keeps its stored wanted set but no live
    // interested build is left on it.
    let build_a = Uuid::new_v4();
    merge_dag(&handle, build_a, vec![mk_root(&["out"])], vec![], false).await?;
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id: build_a,
            caller_tenant: None,
            reason: "test: drop the prior build".into(),
            reply: reply_tx,
        })
        .await?;
    assert!(reply_rx.await??, "cancel of active build A should apply");
    barrier(&handle).await;

    // The stored union's only output becomes substitutable upstream.
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(r_out.clone());

    // Build B re-submits R with a selector matching NO declared
    // output, plus its dependency closure.
    let mut dep = make_node("tdo-dep");
    dep.expected_output_paths = vec![test_store_path("tdo-dep-out")];
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![mk_root(&["bogus"]), dep],
        vec![make_test_edge("tdo-r", "tdo-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // The dependency closure must survive into the merge.
    assert_eq!(
        query_status(&handle, build_b).await?.total_derivations,
        2,
        "an unresolvable submission selector must refuse the prune even \
         when the pre-existing root's stored wanted union is available"
    );
    assert_eq!(
        expect_drv(&handle, "tdo-dep").await.status,
        DerivationStatus::Ready,
        "the dep survived into the merged DAG and is schedulable"
    );

    Ok(())
}

// r[verify sched.substitute.detached+5]
/// Substitutable nodes go `Substituting` (detached fetch spawned),
/// not synchronously `Completed` at merge. The closure-invariant
/// gate (output references ⊆ inputDrv outputs) is enforced by the
/// detached task's `walk_substitute_closure` BFS, not by the
/// scheduler's apply_cached_hits fixed-point: a
/// `SubstituteComplete{ok=true}` means the full reference closure IS
/// in store. Second half: when
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

    // r[sched.substitute.detached+5] — substitutable nodes go to
    // Substituting (detached fetch) instead of cached_hits, so the
    // closure gate is enforced by the detached task's BFS, not by the
    // apply_cached_hits fixed-point. The mock store doesn't
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

// r[verify sched.merge.substitute-topdown+10]
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

// r[verify sched.merge.stale-completed-verify+5]
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

    // r[sched.substitute.detached+5] — the fetch is spawned, not awaited.
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

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.merge.stale-completed-verify+5]
/// `verify_preexisting_completed` × the LIVE effective wanted set: a
/// missing recorded output of a pre-existing Completed node is forgiven
/// (no Completed→Ready reset) only when NO live interested build wants
/// it. Build A merges first wanting ALL outputs (the empty sentinel);
/// build B re-merges with a per-case wanted set while the recorded
/// P_debug is missing from the store:
///
/// - A LIVE: A's all-outputs contribution keeps P_debug in the
///   effective wanted set, so even a build-B-only-wants-`out` re-merge
///   must reset the node (the re-open that lets A's delta be
///   substituted or rebuilt).
/// - A TERMINAL (a failed keep_going build inside the ≤60 s pre-cleanup
///   window — interest and BuildInfo still present): only B's {out}
///   counts → P_debug is unwanted by every live build → forgiven →
///   stays Completed (it was legitimately never substituted; resetting
///   it on every re-merge would ping-pong Completed↔Ready forever).
///
/// Setup: Build A merges `app-a → dep` where dep declares {out, debug};
/// the worker reports both outputs but only P_out is ever uploaded to
/// the store (P_debug is recorded in `output_paths` yet missing). Build
/// B re-merges dep with a per-case wanted set.
#[rstest::rstest]
// P_debug missing, build B only wants {out}, but build A is LIVE and
// wants ALL outputs → the live effective set still wants P_debug → reset.
#[case::missing_unwanted_but_live_build_wants_resets(false, &["out"], DerivationStatus::Ready, 0)]
// Same shape but build A is TERMINAL: no live build wants P_debug →
// forgiven → stays Completed.
#[case::missing_unwanted_no_live_build_wants_forgiven(true, &["out"], DerivationStatus::Completed, 1)]
// P_debug missing and build B wants everything (empty sentinel) → reset.
#[case::missing_wanted_resets(false, &[], DerivationStatus::Ready, 0)]
// P_debug missing and build B's wanted set resolves to no declared
// output (a `drv^bogus` root) → nothing is POSITIVELY identifiable as
// unwanted, so nothing is forgiven → reset. The complement of an
// unresolvable wanted subset must be empty, not every declared path —
// otherwise a GC'd output is never re-opened.
#[case::missing_unresolvable_wanted_resets(false, &["bogus"], DerivationStatus::Ready, 0)]
#[tokio::test]
async fn test_preexisting_completed_missing_unwanted_output_not_reset(
    #[case] a_terminal: bool,
    #[case] wanted: &[&str],
    #[case] expect_dep_status: DerivationStatus,
    #[case] expect_cached: u32,
) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut w1 = connect_executor(&handle, "vw-w1", "x86_64-linux").await?;

    let out = test_store_path("vw-dep-out");
    let dbg = test_store_path("vw-dep-debug");
    let mk_dep = |wanted: &[&str]| {
        let mut d = make_node("vw-dep");
        d.output_names = vec!["out".into(), "debug".into()];
        d.expected_output_paths = vec![out.clone(), dbg.clone()];
        d.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        d
    };

    // Build A: app-a → dep. dep dispatches first (leaf). The worker
    // reports BOTH outputs so output_paths records both, but only
    // P_out is uploaded to the store — P_debug stays missing.
    // keep_going=true in the terminal case so app-a's permanent failure
    // routes through check_build_completion (no cancel sweep) — A's DAG
    // interest and terminal BuildInfo linger, the window under test.
    let build_a = Uuid::new_v4();
    merge_dag(
        &handle,
        build_a,
        vec![make_node("vw-app-a"), mk_dep(&[])],
        vec![make_test_edge("vw-app-a", "vw-dep")],
        a_terminal,
    )
    .await?;
    let assn = recv_assignment(&mut w1).await;
    assert!(assn.drv_path.ends_with("vw-dep.drv"));
    store.seed_with_content(&out, b"out");
    complete_ca(
        &handle,
        "vw-w1",
        &assn.drv_path,
        &[("out", &out, vec![0u8; 32]), ("debug", &dbg, vec![0u8; 32])],
    )
    .await?;
    barrier(&handle).await;
    // app-a dispatches next. Live case: hold it Running so Build A stays
    // Active and dep stays in DAG. Terminal case: fail it permanently so
    // Build A goes Failed while its interest + BuildInfo linger.
    let mut w2 = connect_executor(&handle, "vw-w2", "x86_64-linux").await?;
    let assn_app = recv_assignment(&mut w2).await;
    if a_terminal {
        assert!(assn_app.drv_path.ends_with("vw-app-a.drv"));
        complete_failure(
            &handle,
            "vw-w2",
            &assn_app.drv_path,
            rio_proto::types::BuildResultStatus::PermanentFailure,
            "permanent",
        )
        .await?;
        barrier(&handle).await;
        assert_eq!(
            query_status(&handle, build_a).await?.state,
            rio_proto::types::BuildState::Failed as i32,
            "precondition: A is terminal but not yet cleaned up"
        );
    }
    let pre = expect_drv(&handle, "vw-dep").await;
    assert_eq!(pre.status, DerivationStatus::Completed, "precondition");
    assert_eq!(
        pre.output_paths,
        vec![out.clone(), dbg.clone()],
        "precondition: both outputs recorded, only P_out in store"
    );

    // Build B: app-b → dep (pre-existing Completed). The stale-verify
    // probe finds P_debug missing; whether that triggers the reset
    // depends on whether any LIVE build (A if still live, B's new
    // contribution) wants `debug`.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![make_node("vw-app-b"), mk_dep(wanted)],
        vec![make_test_edge("vw-app-b", "vw-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "vw-dep").await.status,
        expect_dep_status,
        "a_terminal={a_terminal} wanted={wanted:?}: a missing recorded \
         output triggers the Completed→Ready reset iff some LIVE \
         interested build wants it"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.cached_derivations,
        expect_cached,
        "a_terminal={a_terminal} wanted={wanted:?}"
    );

    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
// r[verify sched.merge.stale-completed-verify+5]
/// `verify_preexisting_completed` × an UNAVAILABLE effective wanted set:
/// when a live interested build's contribution is unknown — the
/// post-failover shape, where recovery rebuilds DAG interest from
/// `build_derivations` but per-build contributions are not persisted —
/// the unwanted complement MUST be taken against the STORED node-level
/// union, not the partial union of the contributions that happen to be
/// known (which would silently under-count the recovered build's wants
/// and forgive an output it may still need).
///
/// Build A submits `app-a → dep` wanting ALL outputs (the empty
/// sentinel) before failover. The recovered leader dispatches dep,
/// which records {out, debug} but only P_out is ever uploaded. Build B
/// then re-merges dep wanting only {out}: A is live with an unknown
/// contribution → stored-union fallback (all outputs) → P_debug is
/// still wanted → the stale-Completed reset MUST fire (no cache hit
/// for B). With a partial-union bug, only B's {out} would count and
/// the missing P_debug would be wrongly forgiven.
#[tokio::test]
async fn test_preexisting_completed_unknown_contribution_falls_back_to_stored_union() -> TestResult
{
    let out = test_store_path("pf-dep-out");
    let dbg = test_store_path("pf-dep-debug");
    fn mk_dep(wanted: &[&str]) -> rio_proto::types::DerivationNode {
        let mut d = make_node("pf-dep");
        d.output_names = vec!["out".into(), "debug".into()];
        d.expected_output_paths = vec![
            test_store_path("pf-dep-out"),
            test_store_path("pf-dep-debug"),
        ];
        d.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        d
    }

    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;

    // Phase 1 (pre-failover leader): build A merges app-a → dep with
    // the all-wanted sentinel, then the leader "crashes" (actor drops).
    let build_a = Uuid::new_v4();
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, _pool| {
        merge_dag(
            &handle,
            build_a,
            vec![make_node("pf-app-a"), mk_dep(&[])],
            vec![make_test_edge("pf-app-a", "pf-dep")],
            false,
        )
        .await?;
        barrier(&handle).await;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // Recovered leader: A is Active again with its DAG interest rebuilt
    // but NO recorded contribution for dep.
    assert_eq!(
        query_status(&handle, build_a).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "precondition: the recovered pre-failover build is live"
    );

    // P_out exists in the store; P_debug never does (and is not
    // substitutable).
    store.seed_with_content(&out, b"out");

    // The recovered Ready dep dispatches and completes reporting BOTH
    // outputs — only P_out was actually uploaded.
    let mut w1 = connect_executor(&handle, "pf-w1", "x86_64-linux").await?;
    tick(&handle).await?;
    let assn = recv_assignment(&mut w1).await;
    assert!(assn.drv_path.ends_with("pf-dep.drv"));
    complete_ca(
        &handle,
        "pf-w1",
        &assn.drv_path,
        &[("out", &out, vec![0u8; 32]), ("debug", &dbg, vec![0u8; 32])],
    )
    .await?;
    barrier(&handle).await;
    // app-a dispatches next; hold it Running so A stays live.
    let mut w2 = connect_executor(&handle, "pf-w2", "x86_64-linux").await?;
    let assn_app = recv_assignment(&mut w2).await;
    assert!(assn_app.drv_path.ends_with("pf-app-a.drv"));
    let pre = expect_drv(&handle, "pf-dep").await;
    assert_eq!(pre.status, DerivationStatus::Completed, "precondition");

    // Build B re-merges dep wanting only {out} while P_debug is missing.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![make_node("pf-app-b"), mk_dep(&["out"])],
        vec![make_test_edge("pf-app-b", "pf-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "pf-dep").await.status,
        DerivationStatus::Ready,
        "a live recovered build with an unknown contribution must force \
         the stored-union fallback — P_debug stays wanted → reset"
    );
    assert_eq!(
        query_status(&handle, build_b).await?.cached_derivations,
        0,
        "the stale node must not count as a cache hit for build B"
    );
    Ok(())
}

// r[verify sched.merge.wanted-outputs+2]
/// `check_cached_outputs` × the LIVE effective wanted set: the merge-time
/// cache-hit classification must be evaluated against the union of the
/// wanted contributions of LIVE interested builds, not the never-shrinking
/// stored node-level union. Build A wants ALL outputs (the empty sentinel)
/// and saturates the stored union; build B wants only `out`. P_out is in
/// the store, P_debug is missing and not substitutable.
///
/// - A TERMINAL (a failed keep_going build whose interest and BuildInfo
///   are still around — the ≤60 s pre-cleanup window): only B's
///   contribution counts → all wanted outputs present → cache hit → B
///   completes all-cached.
/// - A LIVE: its all-outputs contribution still counts → P_debug missing
///   → NOT a hit → the node stays pending and B stays Active.
#[rstest::rstest]
// A terminal → only B's {out} is effectively wanted → hit.
#[case::interested_build_terminal(true, "x86_64-linux", DerivationStatus::Completed)]
// A live → its all-wanted contribution keeps P_debug wanted → no hit.
#[case::interested_build_live(false, "aarch64-linux", DerivationStatus::Ready)]
#[tokio::test]
async fn merge_cache_hit_classified_against_live_builds_effective_wanted(
    #[case] a_terminal: bool,
    #[case] system: &str,
    #[case] expect_status: DerivationStatus,
) -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut w1 = connect_executor(&handle, "lew-w1", "x86_64-linux").await?;

    let out = test_store_path("lew-dep-out");
    let dbg = test_store_path("lew-dep-debug");
    let mk = |wanted: &[&str]| {
        let mut d = make_node("lew-dep");
        d.system = system.into();
        d.output_names = vec!["out".into(), "debug".into()];
        d.expected_output_paths = vec![out.clone(), dbg.clone()];
        d.wanted_output_names = wanted.iter().map(|s| (*s).to_string()).collect();
        d
    };
    // P_out is in the store from the start; P_debug never is (and is not
    // substitutable). With an all-outputs wanted set the node can never
    // be a cache hit; with {out} it always is.
    store.seed_with_content(&out, b"out");

    // Build A wants ALL declared outputs (empty sentinel) — saturates the
    // stored union for everyone. keep_going so a derivation failure
    // routes through check_build_completion (no cancel sweep).
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag(&handle, build_a, vec![mk(&[])], vec![], true).await?;

    if a_terminal {
        // Drive A terminal WITHOUT losing its DAG interest: the node is
        // dispatched (x86_64 in this case) and fails permanently;
        // keep_going=true means the build fails via
        // check_build_completion, which does NOT strip interest — A's
        // membership and terminal BuildInfo linger for
        // TERMINAL_CLEANUP_DELAY, exactly the window under test.
        let assn = recv_assignment(&mut w1).await;
        assert!(assn.drv_path.ends_with("lew-dep.drv"));
        complete_failure(
            &handle,
            "lew-w1",
            &assn.drv_path,
            rio_proto::types::BuildResultStatus::PermanentFailure,
            "permanent",
        )
        .await?;
        barrier(&handle).await;
        assert_eq!(
            expect_drv(&handle, "lew-dep").await.status,
            DerivationStatus::Poisoned,
            "precondition: A's permanent failure poisons the node"
        );
        assert_eq!(
            query_status(&handle, build_a).await?.state,
            rio_proto::types::BuildState::Failed as i32,
            "precondition: A is terminal but not yet cleaned up"
        );
    }

    // Build B re-merges the node wanting only {out}.
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(&handle, build_b, vec![mk(&["out"])], vec![], false).await?;
    barrier(&handle).await;

    assert_eq!(
        expect_drv(&handle, "lew-dep").await.status,
        expect_status,
        "a_terminal={a_terminal}: the merge-time cache-hit verdict must \
         follow the live builds' effective wanted set, not the stored \
         union"
    );
    let status_b = query_status(&handle, build_b).await?;
    if a_terminal {
        assert_eq!(
            status_b.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "all of B's wanted outputs are present → all-cached build"
        );
        assert_eq!(status_b.cached_derivations, 1);
    } else {
        assert_eq!(
            status_b.state,
            rio_proto::types::BuildState::Active as i32,
            "A (live) still wants P_debug → no hit → B keeps building"
        );
    }
    Ok(())
}

// r[verify sched.merge.stale-substitutable]
// r[verify sched.merge.wanted-outputs+2]
/// `verify_preexisting_completed` ROUTING × wanted outputs: once the
/// reset HAS fired (a wanted recorded output is missing), the choice
/// between the detached re-substitution (`to_spawn`) and the ready
/// queue must be made over the same wanted-aware view as the reset
/// decision. A recorded-but-UNWANTED output that was never present and
/// is not substitutable — the steady state the demand-driven cache-hit
/// criterion leaves behind — must not disqualify the node from the
/// substitution lane when the wanted output IS substitutable. Routed to
/// the ready queue instead, the node only re-substitutes if the
/// dispatch-time batch probe rescues it (cap-truncatable, fail-open),
/// and `rio_scheduler_stale_completed_substituted_total` never moves.
///
/// Setup mirrors the reset-decision test above: dep declares
/// {out, debug}, every consumer only wants `out`, the worker reports
/// both outputs but only P_out is uploaded. Then P_out is GC'd but
/// substitutable upstream; P_debug stays missing and NOT substitutable.
/// Build B re-merges dep wanting only `out`.
#[tokio::test]
async fn test_preexisting_completed_unwanted_missing_output_routes_to_substitution() -> TestResult {
    // Thread-local recorder: #[tokio::test]'s current-thread runtime
    // means the actor task sees it at .await points (same mechanism as
    // misc.rs's gauge tests). Installed before the actor spawns so the
    // merge-time increment is captured.
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;
    let mut w1 = connect_executor(&handle, "vr-w1", "x86_64-linux").await?;

    let out = test_store_path("vr-dep-out");
    let dbg = test_store_path("vr-dep-debug");
    let mk_dep = || {
        let mut d = make_node("vr-dep");
        d.output_names = vec!["out".into(), "debug".into()];
        d.expected_output_paths = vec![out.clone(), dbg.clone()];
        d.wanted_output_names = vec!["out".into()];
        d
    };

    // Build A: app-a → dep; nothing ever wants `debug`. The worker
    // reports BOTH outputs so output_paths records both, but only P_out
    // is uploaded to the store — P_debug stays missing.
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("vr-app-a"), mk_dep()],
        vec![make_test_edge("vr-app-a", "vr-dep")],
        false,
    )
    .await?;
    let assn = recv_assignment(&mut w1).await;
    assert!(assn.drv_path.ends_with("vr-dep.drv"));
    store.seed_with_content(&out, b"out");
    complete_ca(
        &handle,
        "vr-w1",
        &assn.drv_path,
        &[("out", &out, vec![0u8; 32]), ("debug", &dbg, vec![0u8; 32])],
    )
    .await?;
    barrier(&handle).await;
    // Hold app-a Running so Build A stays Active and dep stays in DAG.
    let mut _w2 = connect_executor(&handle, "vr-w2", "x86_64-linux").await?;
    let _ = recv_assignment(&mut _w2).await;
    assert_eq!(
        expect_drv(&handle, "vr-dep").await.status,
        DerivationStatus::Completed,
        "precondition"
    );

    // GC P_out but leave it substitutable upstream; P_debug stays
    // missing AND not substitutable.
    store.state.paths.write().unwrap().remove(&out);
    store.state.substitutable.write().unwrap().push(out.clone());

    // Build B: app-b → dep, wanting only `out`. The reset fires (P_out
    // is missing and wanted); the routing must take the detached
    // substitution lane because the only non-substitutable missing path
    // is the unwanted P_debug.
    let build_b = Uuid::new_v4();
    merge_dag(
        &handle,
        build_b,
        vec![make_node("vr-app-b"), mk_dep()],
        vec![make_test_edge("vr-app-b", "vr-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // The discriminator: the verify-time routing took the detached
    // substitution. With a routing computed over ALL recorded paths
    // (ignoring `unwanted`), dep lands in the ready queue instead and
    // this counter never moves — even if the dispatch-time batch probe
    // later rescues the node.
    assert_eq!(
        recorder.get("rio_scheduler_stale_completed_substituted_total{}"),
        1,
        "a missing-but-substitutable WANTED output plus a missing-and-\
         unsubstitutable UNWANTED output must route to the detached \
         substitution at verify time, not the ready queue; counters \
         seen: {:?}",
        recorder.all_keys()
    );

    // And the detached fetch settles the node back to Completed (the
    // walk forgives the unwanted P_debug), so build B never re-builds.
    settle_substituting(&handle, &["vr-dep"]).await;
    assert_eq!(
        expect_drv(&handle, "vr-dep").await.status,
        DerivationStatus::Completed,
        "SubstituteComplete{{ok=true}} returns the reset node to Completed"
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
// r[verify sched.substitute.detached+5]
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

// r[verify sched.merge.stale-completed-verify+5]
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

// r[verify sched.merge.stale-completed-verify+5]
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
// r[verify sched.merge.stale-completed-verify+5]
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

// r[verify sched.substitute.detached+5]
/// Floating-CA reprobe → re-substitute lane: `verify_preexisting_
/// completed` finds a Completed floating-CA node's REALIZED output
/// gone-but-substitutable, resets + spawns the detached fetch with the
/// realized path. After `SubstituteComplete{ok=true}`, `output_paths`
/// must be the realized path — pre-fix `complete_ready_from_store_batch`
/// clobbered it with `expected_output_paths == [""]` (GC retention
/// lost; clients got `[""]`).
#[tokio::test]
async fn reprobe_substitute_floating_ca_preserves_realized_path() -> TestResult {
    use std::sync::atomic::Ordering;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Floating-CA: expected_output_paths == [""] (path unknown until
    // built). Hold a build1 reference so X stays in DAG.
    let mut x = make_node("rsc-x");
    x.is_content_addressed = true;
    x.expected_output_paths = vec![String::new()];
    let real = test_store_path("rsc-x-realized");
    merge_dag(&handle, Uuid::new_v4(), vec![x.clone()], vec![], false).await?;
    handle
        .debug_force_status("rsc-x", DerivationStatus::Completed)
        .await?;
    handle
        .debug_set_output_paths("rsc-x", vec![real.clone()])
        .await?;
    barrier(&handle).await;

    // Realized output is gone-from-store but upstream-substitutable.
    // Gate QPI so we can seed the path between FMP (reports missing+
    // substitutable) and the detached fetch's QPI (succeeds).
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .push(real.clone());
    store
        .faults
        .query_path_info_gate_armed
        .store(true, Ordering::SeqCst);

    // Build 2 references X (pre-existing Completed). verify_preexisting_
    // completed: output missing+substitutable → reset to Ready → to_spawn
    // → spawn_substitute_fetches with the REALIZED path.
    merge_dag(&handle, Uuid::new_v4(), vec![x], vec![], false).await?;
    wait_for_status(&handle, "rsc-x", DerivationStatus::Substituting).await;
    // Detached task is parked at the QPI gate. output_paths must already
    // hold the realized path (spawn_substitute_fetches stashes it).
    let mid = expect_drv(&handle, "rsc-x").await;
    assert_eq!(
        mid.output_paths,
        vec![real.clone()],
        "spawn_substitute_fetches must stash the realized path on state"
    );

    // Seed locally so the released QPI succeeds → ok=true.
    store.seed_with_content(&real, b"rsc-x-contents");
    store
        .faults
        .query_path_info_gate_armed
        .store(false, Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    settle_substituting(&handle, &["rsc-x"]).await;

    let post = expect_drv(&handle, "rsc-x").await;
    assert_eq!(post.status, DerivationStatus::Completed);
    assert_eq!(
        post.output_paths,
        vec![real],
        "complete_ready_from_store_batch must NOT clobber the realized \
         path with expected_output_paths==[\"\"] (pre-fix: did)"
    );
    Ok(())
}

// r[verify sched.state.transitions]
/// `verify_preexisting_completed` reset on a node whose dep is
/// terminally-failed must go `DependencyFailed`, not `Queued`. Pre-fix
/// 2-way `Ready|Queued` → stuck forever (same hole as
/// `handle_substitute_complete`).
#[tokio::test]
async fn verify_preexisting_with_poisoned_dep_goes_dependency_failed() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;

    // X depends on Y. Build1 holds them in DAG.
    let x_out = test_store_path("vpd-x-out");
    let mut x = make_node("vpd-x");
    x.expected_output_paths = vec![x_out.clone()];
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![x.clone(), make_node("vpd-y")],
        vec![make_test_edge("vpd-x", "vpd-y")],
        false,
    )
    .await?;
    // Y Poisoned-at-limit (so re-merge does NOT reset it). X Completed
    // with output_paths set (so it's a verify_preexisting candidate).
    handle
        .debug_force_poisoned("vpd-y", POISON_RESUBMIT_RETRY_LIMIT)
        .await?;
    handle
        .debug_force_status("vpd-x", DerivationStatus::Completed)
        .await?;
    handle.debug_set_output_paths("vpd-x", vec![x_out]).await?;
    barrier(&handle).await;

    // Build 2 references X→Y. X's output is NOT in store (never seeded)
    // and NOT substitutable → verify_preexisting_completed resets X.
    // revert_target_for(X): Y Poisoned → DependencyFailed.
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![x, make_node("vpd-y")],
        vec![make_test_edge("vpd-x", "vpd-y")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let xs = expect_drv(&handle, "vpd-x").await;
    assert_eq!(
        xs.status,
        DerivationStatus::DependencyFailed,
        "dep Y Poisoned → X resets to DependencyFailed (pre-fix: Queued, \
         stuck forever)"
    );
    Ok(())
}

// ===========================================================================
// r[sched.substitute.fanout-bound]: cd83a9b2-cannot-recur structural assertion
// ===========================================================================

/// cd83a9b2 regression: with the store now owning admission
/// (`r[store.substitute.admission]`), the scheduler-side
/// `DEFAULT_SUBSTITUTE_CONCURRENCY` is purely a memory bound. Under
/// store-side `ResourceExhausted` backpressure on a wide substitutable
/// DAG, the detached fetch tasks MUST retry-then-succeed and the
/// semaphore MUST not leak permits.
///
/// 500 leaf nodes (5000 dominates the suite — see plan §7 R6; with 256
/// permits and 3× per-path RE → ~1.75 s held permit × ⌈500/256⌉ = ~4 s
/// real-time), 3 `ResourceExhausted` per path then success
/// (`SUBSTITUTE_FETCH_MAX_ATTEMPTS=8`, so 3 sits well inside the budget;
/// the plan's "10 per path" exceeds 8 and would assert the wrong thing).
///
/// STRUCTURAL — no wall-clock assertions:
///   (a) every node → `Completed` (zero demotions to build-from-source);
///   (b) `substitute_sem.available_permits()` returns to the configured
///       cap (no leaked permits — the cap held);
///   (c) every path saw exactly N+1 QPI attempts (N retries + 1 success
///       — proxies `rio_scheduler_substitute_fetch_retries_total` without
///       a process-global recorder);
///   (d) zero failures: implied by (a) + `qpi_calls.len() == N` (every
///       path reached the success arm exactly once).
// r[verify sched.substitute.fanout-bound]
#[tokio::test]
async fn cd83a9b2_cannot_recur() -> TestResult {
    use crate::state::DerivationStatus;
    const N: usize = 500;
    const RE_PER_PATH: u32 = 3;

    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    let mut nodes = Vec::with_capacity(N);
    let mut hashes = Vec::with_capacity(N);
    let mut outs = Vec::with_capacity(N);
    for i in 0..N {
        let tag = format!("fanout-n{i}");
        let out = test_store_path(&format!("fanout-n{i}-out"));
        let mut n = make_node(&tag);
        n.expected_output_paths = vec![out.clone()];
        hashes.push(n.drv_hash.clone());
        outs.push(out);
        nodes.push(n);
    }
    store
        .state
        .substitutable
        .write()
        .unwrap()
        .extend(outs.iter().cloned());
    store
        .faults
        .fail_qpi_resource_exhausted_per_path_n
        .store(RE_PER_PATH, std::sync::atomic::Ordering::SeqCst);

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, nodes, vec![], false).await?;
    barrier(&handle).await;

    // Drain: poll until none Substituting. The 30 s cap is hang-detection
    // (real-time backoff = ~4 s expected), NOT the assertion.
    let deadline = std::time::Instant::now() + std::time::Duration::from_secs(30);
    loop {
        barrier(&handle).await;
        let any = {
            let mut any = false;
            for h in &hashes {
                if expect_drv(&handle, h).await.status == DerivationStatus::Substituting {
                    any = true;
                    break;
                }
            }
            any
        };
        if !any {
            break;
        }
        assert!(
            std::time::Instant::now() < deadline,
            "detached fetches did not drain in 30 s (expected ~4 s); hung task?"
        );
        tokio::time::sleep(std::time::Duration::from_millis(50)).await;
    }

    // (a) all Completed — zero demotions / zero failures.
    for h in &hashes {
        let st = expect_drv(&handle, h).await.status;
        assert_eq!(
            st,
            DerivationStatus::Completed,
            "{h}: store ResourceExhausted must be retried, not demoted; got {st:?}"
        );
    }

    // (b) semaphore returned to cap — no leaked permits across N tasks.
    let snap = handle.debug_counters().await?;
    assert_eq!(
        snap.substitute_sem_permits,
        crate::actor::DEFAULT_SUBSTITUTE_CONCURRENCY,
        "substitute_sem leaked permits: {} available, expected {}",
        snap.substitute_sem_permits,
        crate::actor::DEFAULT_SUBSTITUTE_CONCURRENCY,
    );

    // (c) every path retried exactly RE_PER_PATH times then succeeded.
    // Structural proxy for substitute_fetch_retries_total ≥ N×RE_PER_PATH
    // and substitute_fetch_failures_total == 0.
    let attempts = store.calls.qpi_attempts_by_path.read().unwrap();
    for out in &outs {
        assert_eq!(
            attempts.get(out).copied(),
            Some(RE_PER_PATH + 1),
            "{out}: expected {RE_PER_PATH} ResourceExhausted retries + 1 success"
        );
    }

    // (d) every path reached the success arm exactly once.
    assert_eq!(
        store.calls.qpi_calls.read().unwrap().len(),
        N,
        "every path should record one successful QPI"
    );
    Ok(())
}

// r[verify sched.substitute.eager-probe]
/// Merge-time substitution covers the WHOLE submission in one
/// `FindMissingPaths`: with the store-side 4096-path truncation
/// removed, 5000 IA leaves whose outputs are all
/// upstream-substitutable MUST all transition to `Substituting` at
/// merge time. Regression guard: pre-change, only the first 4096 (the
/// store's truncated `substitutable_paths`) hit; the tail fell through
/// to dispatch-time layer-by-layer.
#[tokio::test]
async fn merge_probe_whole_dag_substituting() -> TestResult {
    const N: usize = 5000;
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Arm the QPI gate so the detached substitute-fetch tasks park
    // (don't post SubstituteComplete) — keeps every node IN
    // Substituting at snapshot time. The assertion is on the
    // merge-time verdict COUNT, not on fetch completion.
    store
        .faults
        .query_path_info_gate_armed
        .store(true, std::sync::atomic::Ordering::SeqCst);

    // 5000 IA leaves, each with a known expected_output_path. Seed
    // ALL outputs as substitutable in the mock store so
    // FindMissingPaths returns the full set in substitutable_paths.
    let mut nodes = Vec::with_capacity(N);
    {
        let mut subs = store.state.substitutable.write().unwrap();
        subs.reserve(N);
        for i in 0..N {
            let tag = format!("eager-{i}");
            let out = format!(
                "/nix/store/{}-{tag}-out",
                rio_test_support::fixtures::rand_store_hash()
            );
            let mut n = make_node(&tag);
            n.expected_output_paths = vec![out.clone()];
            nodes.push(n);
            subs.push(out);
        }
    }

    let build_id = Uuid::new_v4();
    merge_dag(&handle, build_id, nodes, vec![], false).await?;

    // Tick refreshes the cached snapshot.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations as usize, N,
        "all {N} leaves must receive a merge-time substitutable verdict \
         (would be ≤4096 with store-side truncation)"
    );
    // Release parked fetches so test teardown doesn't wait on them.
    store
        .faults
        .query_path_info_gate_armed
        .store(false, std::sync::atomic::Ordering::SeqCst);
    store.faults.query_path_info_gate.notify_waiters();
    Ok(())
}
