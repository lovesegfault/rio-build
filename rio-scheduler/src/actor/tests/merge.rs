//! Merge error paths: DB-failure rollback, cache-check store errors, circuit breaker.
// r[verify sched.merge.toctou-serial]

use super::*;

// ===========================================================================
// Shared-node priority bump on higher-priority merge
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
// D3-retarget: classification survives; the routed-mechanism assertion
// flips to job creation when the walk spawner dies.
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

// D3-retarget: classification pin — see test_substitutable_probe_matrix.
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half) — the prune
// survives D'; stamp/walk arms re-target to origin='pruned' job rows.
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// D5-retarget: selection-predicate pin — see test_topdown_stamp_only_nodes_*.
// r[verify sched.merge.substitute-topdown+12]
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

// D5-retarget: selection-predicate pin — see test_topdown_stamp_only_nodes_*.
// r[verify sched.merge.substitute-topdown+12]
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

// D5-retarget: the SELECTION predicate survives as the origin='pruned'
// criterion (D2.1); the column-state assertions flip to job-row origin
// assertions when the column machinery deletes.
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

    // The recovered Ready dep is taken by a pull-mode attempt and
    // completes reporting BOTH outputs — only P_out was actually
    // uploaded.
    tick(&handle).await?;
    let assn = pull_attempt(&handle, "pf-dep").await;
    assert!(assn.drv_path.ends_with("pf-dep.drv"));
    pull_complete_ca(
        &handle,
        "pf-dep",
        &[("out", &out, vec![0u8; 32]), ("debug", &dbg, vec![0u8; 32])],
    )
    .await?;
    barrier(&handle).await;
    // app-a's attempt opens next; hold it Running so A stays live.
    let assn_app = pull_attempt(&handle, "pf-app-a").await;
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

// D3-retarget: the reprobe lane survives (AS-5 reset + origin='reprobe'
// jobs); Substituting-status assertions flip to reset+job assertions.
// r[verify sched.merge.poisoned-resubmit-bounded+4]
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

// D3-retarget: reprobe-lane pin — see test_resubmit_poisoned_at_limit_*.
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

// D3-retarget: reprobe-lane pin — see test_resubmit_poisoned_at_limit_*.
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

// D3-retarget: reprobe-lane pin — see test_resubmit_poisoned_at_limit_*.
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

// ---------------------------------------------------------------------------
// Attempt ledger (drv_attempts, Phase 1a): reset events are durable rows
// and the suffix loader cuts at them.
// ---------------------------------------------------------------------------

/// Suffix classes for one derivation via the production loader
/// (`load_attempt_suffix`) — what the Phase-1b fold will actually see.
async fn suffix_classes(pool: &sqlx::PgPool, drv_hash: &str) -> Vec<&'static str> {
    let derivation_id: Uuid =
        sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_hash)
            .fetch_one(pool)
            .await
            .expect("derivation row");
    let sdb = crate::db::SchedulerDb::new(pool.clone());
    let suffix = sdb
        .load_attempt_suffix(&[derivation_id])
        .await
        .expect("suffix load");
    suffix
        .get(&derivation_id)
        .map(|rows| rows.iter().map(|r| r.outcome_class.as_str()).collect())
        .unwrap_or_default()
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+12]
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

// ===========================================================================
// Claims-floor fence on the merge transaction (sched.evidence.durability)
// ===========================================================================

/// The merge transaction (derivation upserts + build links + edges +
/// Pending→Active activation, including the `topdown_pruned` stamps) is
/// claims-floor fenced: a replica whose serving generation sits below
/// the durable floor — a successor has claimed — must NOT commit it.
/// The merge fails with `StaleGeneration` (mapped to gRPC
/// FAILED_PRECONDITION so the client retries against the live leader)
/// and leaves nothing behind: no derivation rows, no build links, no
/// Active build.
///
/// This is the deposed-believer MergeDag window the as-built posture
/// documented: leadership is checked at SubmitBuild enqueue time only,
/// so a replica deposed after the enqueue still executes its merge
/// transaction — racing the new leader's recovery — unless the
/// transaction itself is fenced.
// r[verify sched.evidence.durability+2]
#[tokio::test]
async fn merge_from_deposed_generation_is_fenced() -> TestResult {
    let (db, handle, _task) = setup().await;

    // A successor has claimed generation 2. This actor's tenure stamp
    // is 1 (the always-leader construction-time lease read).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (2, 'successor')",
    )
    .execute(&db.pool)
    .await?;

    // The deposed believer's merge must be fenced.
    let build_id = Uuid::new_v4();
    let reply =
        merge_single_node(&handle, build_id, "fenced-merge", PriorityClass::Scheduled).await;
    assert!(
        matches!(
            reply.as_ref().err().and_then(|e| e.downcast_ref()),
            Some(ActorError::StaleGeneration {
                serving: 1,
                floor: 2
            })
        ),
        "a merge from a deposed generation must fail with StaleGeneration, got {reply:?}"
    );

    // Nothing was committed: no derivation row, no build link, no
    // Active build (the rejected merge's cleanup removes the Pending
    // builds row it created before the transaction).
    let drv_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM derivations WHERE drv_hash = 'fenced-merge'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        drv_count, 0,
        "the fenced merge must leave zero derivation rows"
    );
    let link_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM build_derivations WHERE build_id = $1")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        link_count, 0,
        "the fenced merge must leave zero build links"
    );
    let active_count: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM builds WHERE build_id = $1 AND status = 'active'")
            .bind(build_id)
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        active_count, 0,
        "the fenced merge must not activate the build"
    );

    // The in-memory rollback also ran: the build is unknown to the actor.
    let status_result = try_query_status(&handle, build_id).await?;
    assert!(
        matches!(status_result, Err(ActorError::BuildNotFound(_))),
        "the fenced merge's build must be rolled back in memory, got {status_result:?}"
    );
    Ok(())
}

// r[verify sched.merge.stale-completed-verify+5]
/// A pre-existing **Skipped** node whose recorded output has been GC'd
/// must be reset by the stale-output verify when a new build merges
/// over it — the verify's candidate filter covers Skipped (the
/// CA-cutoff produced status), not just Completed. Without it,
/// dependents unlock against a gone output and dispatch into ENOENT.
///
/// Pull-mode re-add of the stream-era `test_stale_skipped_output_reset`
/// (deleted with the session machinery; the closure-evidence campaign's
/// Phase-2 acceptance table tracks this as the CE-5 Skipped-half /
/// CE-73-adjacent named coverage).
#[tokio::test]
async fn test_stale_skipped_output_reset() -> TestResult {
    let (_db, store, handle, _tasks) = setup_with_mock_store().await?;

    // Build 1: app → dep. Build dep via the pull surface so it ends
    // Completed with recorded output_paths.
    let dep_out = test_store_path("sk-dep-out");
    let mut dep = make_node("sk-dep");
    dep.expected_output_paths = vec![dep_out.clone()];
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("sk-app"), dep],
        vec![make_test_edge("sk-app", "sk-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;
    store.seed_with_content(&dep_out, b"d");
    pull_complete_success(&handle, "sk-dep", &dep_out).await?;
    wait_for_status(&handle, "sk-dep", DerivationStatus::Completed).await;

    // Force dep to Skipped (the CA-cutoff produced status). The
    // transition table has no Completed→Skipped edge; the debug handle
    // sets it without validation (test-only staging).
    handle
        .debug_force_status("sk-dep", DerivationStatus::Skipped)
        .await?;
    let pre = expect_drv(&handle, "sk-dep").await;
    assert_eq!(pre.status, DerivationStatus::Skipped, "precondition");
    assert!(
        !pre.output_paths.is_empty(),
        "precondition: Skipped carries recorded output_paths"
    );

    // GC the recorded output out of the store.
    store.state.paths.write().unwrap().remove(&dep_out);

    // Build 2 references dep (pre-existing Skipped). The stale-output
    // verify must reset it — Skipped is a verify candidate exactly like
    // Completed.
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
        "a Skipped node with a GC'd output must be reset by the \
         stale-output verify (Ready/Queued); got {:?} — a candidate \
         filter that skips Skipped unlocks dependents against a gone \
         output",
        dep.status
    );
    Ok(())
}
