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
                nodes: vec![node],
                edges: vec![],
                ..Default::default()
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
/// If check_cached_outputs ran AFTER persist_merges +
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
                nodes: vec![node],
                edges: vec![],
                ..Default::default()
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
    crate::actor::tests::seed_default_tenant(&test_db.pool).await;
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

// r[verify sched.merge.wanted-outputs+3]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

    // --- Case 3: missing WANTED output is substitutable → job -------
    // (D3-retarget: the routed mechanism is a materialization job —
    // the node is neither a hit nor a from-source fall-through.)
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
    let _ev3 = merge_dag(&handle, b3, vec![n3], vec![], false).await?;
    barrier(&handle).await;

    let s3 = query_status(&handle, b3).await?;
    assert_eq!(
        s3.state,
        rio_proto::types::BuildState::Active as i32,
        "a missing WANTED output that is substitutable routes to the \
         pending_substitute lane's materialization job — not hits"
    );
    assert_eq!(s3.cached_derivations, 0, "not classified as a hit");
    let (origin, job_state): (String, String) =
        sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'wo-sub'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "cache_opportunity", "the new_sub lane's job origin");
    assert_eq!(job_state, "pending");
    assert_eq!(
        expect_drv(&handle, "wo-sub").await.status,
        DerivationStatus::Ready,
        "the node stays Ready (claimable); the job is the in-flight marker"
    );

    Ok(())
}

// r[verify sched.merge.substitute-probe]
// r[verify sched.materialize.job+2]
/// Substitutable-probe matrix at merge time. A path NOT in the store
/// but reported as `substitutable_paths` by FindMissingPaths is routed
/// to a materialization job (the walk-era eager QueryPathInfo fetch
/// retired with sched.merge.substitute-fetch).
/// Missing-and-not-substitutable stays missing.
///
/// Before P0472: scheduler ignored `substitutable_paths` → dispatched
/// builds cache.nixos.org already had. Before P0473: marked
/// substitutable paths completed but never fetched → builder ENOENT on
/// FUSE access (FUSE GetPath carries no JWT so lazy fetch can't work).
#[rstest::rstest]
// substitutable → routed to a materialization job (claimable)
#[case::substitutable("hello-2.12.3", true, true)]
// not substitutable → plain miss → no job, from-source dispatch
// (guards "all missing = substitutable")
#[case::missing("truly-missing-out", false, false)]
// D3-retarget (flipped with the walk spawner's deletion): classification
// survives; the routed mechanism is a materialization job. The walk-era
// fetch_fail case (QPI failure demoting to a from-source miss) retired
// with the walk's QPI ladder — fetch failures are now the store
// executor's, settled through the job report routing (the materialize.rs
// routing battery).
#[tokio::test]
async fn test_substitutable_probe_matrix(
    #[case] out_tag: &str,
    #[case] substitutable: bool,
    #[case] expect_job: bool,
) -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out_path = test_store_path(out_tag);
    if substitutable {
        store
            .state
            .substitutable
            .write()
            .unwrap()
            .push(out_path.clone());
    }

    let mut node = make_node("sub-probe");
    node.expected_output_paths = vec![out_path.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "the build stays Active either way — the job (or the from-source \
         dispatch) resolves it later"
    );
    let jobs: i64 =
        sqlx::query_scalar("SELECT count(*) FROM materialization_jobs WHERE drv_hash = $1")
            .bind("sub-probe")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        jobs,
        i64::from(expect_job),
        "substitutable={substitutable}: job creation must follow the probe verdict"
    );
    assert_eq!(
        expect_drv(&handle, "sub-probe").await.status,
        DerivationStatus::Ready,
        "the node stays Ready in both cases (claimable / from-source dispatchable)"
    );
    Ok(())
}

// D3-retarget (flipped with the walk spawner's deletion): classification
// pin — see test_substitutable_probe_matrix.
/// `FindMissingPaths.indeterminate_paths` (probe got 429/5xx/deadline)
/// is treated optimistically: the node routes to a materialization job
/// (the store-side fetch decides), never straight to a from-source
/// build dispatch. Without this, indeterminate was treated as
/// confirmed-miss and dispatched as a build.
// r[verify sched.merge.substitute-probe-indeterminate+2]
#[tokio::test]
async fn test_indeterminate_probe_tries_substitute_not_build() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let out = test_store_path("indet-out");
    // Probe says indeterminate (NOT in `substitutable`). Mirrors the
    // live case: the HEAD probe 429'd; the store-side fetch may still
    // succeed.
    store.state.indeterminate.write().unwrap().push(out.clone());

    let mut node = make_node("indet-drv");
    node.expected_output_paths = vec![out.clone()];
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "indeterminate → optimistic job routing; the build awaits the job"
    );
    let (origin, job_state): (String, String) = sqlx::query_as(
        "SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'indet-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(origin, "cache_opportunity");
    assert_eq!(
        job_state, "pending",
        "indeterminate must yield a claimable job, not a builder dispatch"
    );
    assert_eq!(
        expect_drv(&handle, "indet-drv").await.status,
        DerivationStatus::Ready,
        "never handed to a builder while the job is unresolved"
    );
    Ok(())
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half) — the prune
// survives D'; stamp/walk arms re-target to origin='pruned' job rows.
// r[verify sched.merge.substitute-topdown+13]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let _ev = merge_dag(&handle, build_id, nodes, edges, false).await?;
    barrier(&handle).await;

    // The prune fired: only the root merged, with an origin='pruned'
    // materialization job riding the merge transaction (the routed
    // mechanism — D3-retarget: the detached fetch is gone; the store
    // replica executes the job).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "the pruned root awaits its materialization job; got state={}",
        status.state
    );

    // Total derivations reported = 1 (root only), not 4.
    assert_eq!(
        status.total_derivations, 1,
        "pruned DAG should report root count, not original submission size"
    );

    // The job row: origin='pruned', pending (claimable).
    let (origin, job_state): (String, String) =
        sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'hello'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "pruned", "the prune's job origin");
    assert_eq!(job_state, "pending");

    // No scheduler-side fetch for anything — root or deps.
    let qpi = store.calls.qpi_calls.read().unwrap();
    assert!(
        qpi.is_empty(),
        "no scheduler-side walk fetches any more; qpi_calls={qpi:?}"
    );

    Ok(())
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+13]
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
    barrier(&handle).await;

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
// r[verify sched.merge.substitute-topdown+13]
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
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let _ev = merge_dag(
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
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.total_derivations, 2,
        "the pruned submission is the demand set: app (root) AND the \
         explicitly requested lib; only dep is dropped"
    );
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "both demanded nodes await their materialization jobs"
    );

    // Both demanded nodes were routed to pruned-origin jobs riding the
    // merge transaction (D3-retarget: the routed mechanism — the
    // requested lib gets a REAL job, not a fabricated success).
    for hash in ["tdk-app", "tdk-lib"] {
        let (origin, job_state): (String, String) =
            sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = $1")
                .bind(hash)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(origin, "pruned", "{hash}: the prune's job origin");
        assert_eq!(job_state, "pending", "{hash}: claimable");
    }

    // No scheduler-side fetch for anything, and the dropped dep never
    // entered the DAG.
    {
        let qpi = store.calls.qpi_calls.read().unwrap();
        assert!(
            qpi.is_empty(),
            "no scheduler-side walk fetches any more; qpi_calls={qpi:?}"
        );
    }
    let _ = (app_out, lib_out);
    assert!(
        handle.debug_query_derivation("tdk-dep").await?.is_none(),
        "dep should be pruned from the submission, not in the global DAG"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+13]
/// A kept (demanded) node whose existing DAG children are ALL already
/// produced (Completed/Skipped) must NOT get the `origin = 'pruned'`
/// classification (T-D5.1 re-target of the walk-era stamp pin: the
/// selection predicate survives as the job-origin gate) — its
/// dependency closure exists in the store, so a from-source dispatch
/// is not doomed and the pruned classification would only arm the
/// resubmit-directing fail-fast for a node that could build. (A node
/// whose children are still unbuilt IS pruned-origin — see the sibling
/// test below.)
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

    // B0: only D's output is locally PRESENT → full merge (R, the sole
    // demanded node, is not available); D completes via the merge-time
    // cache hit, so R's existing child is PRODUCED by the time B1
    // prunes. (D3-retarget fixture: the detached fetch that used to
    // produce D is gone; local presence exercises the same premise.)
    store.seed_with_content(&d_out, b"tdc-d-contents");
    let b0 = Uuid::new_v4();
    let _ev0 = merge_dag(
        &handle,
        b0,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdc-r", "tdc-d")],
        false,
    )
    .await?;
    barrier(&handle).await;
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
    let _ev1 = merge_dag(
        &handle,
        b1,
        vec![mk_r(), mk_d()],
        vec![make_test_edge("tdc-r", "tdc-d")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        query_status(&handle, b1).await?.total_derivations,
        1,
        "fixture premise: B1 took the roots-only prune path"
    );

    // R's children are all produced (⇒ Vouched) → the pruned arm
    // must skip it; the substitution lane still queues its job, with
    // a non-doomed origin. bug_058 note: B1 re-submits R as ROOT —
    // an explicit resubmission — so the verdict-free band resets R
    // into the newly-inserted lane and the job classifies
    // `cache_opportunity` (the fresh-substitutable origin) instead of
    // the pre-band `reprobe`; both are non-doomed, and the law under
    // test (never pruned-origin for a produced-children keep) holds
    // identically.
    let (origin,): (String,) =
        sqlx::query_as("SELECT origin FROM materialization_jobs WHERE drv_hash = 'tdc-r'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        origin, "cache_opportunity",
        "a kept node whose DAG children are already produced must not be \
         classified pruned-origin (its closure is in the store; from-source \
         remains valid)"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+13]
/// A kept (demanded) node whose existing DAG children are still UNBUILT
/// must get the `origin = 'pruned'` classification (T-D5.1 re-target of
/// the walk-era stamp pin). Those children can belong to a different
/// build and be reaped unbuilt later (that build cancelled → its
/// sole-interest deps cascade terminal → reaped → `children[R]`
/// scrubbed); a non-pruned origin would let the arm-3 settlement
/// release R to from-source for the doomed ENOENT dispatch this
/// classification exists to prevent.
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

    let (origin,): (String,) =
        sqlx::query_as("SELECT origin FROM materialization_jobs WHERE drv_hash = 'tdu-r'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        origin, "pruned",
        "a kept closure-dropped node whose existing children are unbuilt must \
         be classified pruned-origin (the children can be reaped unbuilt later)"
    );

    // Let B1's detached fetch settle before teardown.
    barrier(&handle).await;
    Ok(())
}

// r[verify sched.merge.substitute-topdown+13]
/// The `origin = 'pruned'` classification must land only on kept nodes
/// whose dependency closure the prune actually dropped (T-D5.1
/// re-target of the walk-era stamp pin: the selection predicate IS the
/// origin criterion now, D2.1). A dep-less demanded leaf (here: one
/// target of a multi-target submission with no inputDrvs of its own)
/// never had a closure to drop — a from-source dispatch of it would
/// succeed — so classifying it pruned would only convert a routine
/// substitute failure into a wrongful resubmit-directing terminal
/// failure.
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

    // R lost its dependency closure → pruned-origin job.
    let (r_origin,): (String,) =
        sqlx::query_as("SELECT origin FROM materialization_jobs WHERE drv_hash = 'tdl-r'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        r_origin, "pruned",
        "kept root whose closure was dropped must be classified pruned-origin"
    );

    // L never had a closure to drop → its job is the new_sub lane's
    // cache_opportunity, never pruned; building it from source stays a
    // valid fallback.
    let (l_origin,): (String,) =
        sqlx::query_as("SELECT origin FROM materialization_jobs WHERE drv_hash = 'tdl-l'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        l_origin, "cache_opportunity",
        "dep-less kept leaf must not be classified pruned-origin (it has no \
         dropped closure; from-source dispatch of it is valid)"
    );

    // Let the detached fetches settle before teardown.
    barrier(&handle).await;
    Ok(())
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+13]
/// Top-down negative: root NOT substitutable → fall through to
/// full bottom-up check. All nodes merged, deps processed normally.
#[tokio::test]
async fn test_topdown_root_missing_falls_through() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let _ev = merge_dag(&handle, build_id, nodes, edges, false).await?;
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

    // Bottom-up still fires: glibc routed to a materialization job
    // (D3-retarget: the new_sub lane's job replaces the detached fetch).
    let (origin, job_state): (String, String) = sqlx::query_as(
        "SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'glibc-ft'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(origin, "cache_opportunity");
    assert_eq!(
        job_state, "pending",
        "bottom-up classification routes the substitutable dep to a job on fall-through"
    );

    Ok(())
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.wanted-outputs+3]
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
// r[verify sched.merge.substitute-topdown+13]
// r[verify sched.merge.wanted-outputs+3]
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
/// from-source path — no substitute fetch, no pruned-origin
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
// r[verify sched.merge.substitute-topdown+13]
/// Top-down positive companion: a PRE-EXISTING root whose live
/// effective wanted set IS satisfiable keeps the prune. Same shape as
/// the negative test above, but build A wants only `out` too — the
/// union of live contributions resolves to paths that are all
/// available, so B's submission is still pruned to roots-only and
/// completes via the detached substitute fetch.
#[tokio::test]
async fn test_topdown_prune_fires_when_preexisting_roots_live_wanted_satisfiable() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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

    // The pruned root is routed to a pruned-origin materialization job
    // (D3-retarget: the detached fetch is gone; the store replica
    // executes the job — the missing-but-unwanted `debug` is the
    // executor's wanted-set resolution to forgive, not the walk's).
    let (origin, job_state): (String, String) =
        sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'tds-r'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "pruned");
    assert_eq!(job_state, "pending");
    let _ = store;

    Ok(())
}

// D3-retarget: prune-decision pin (B10 kept-guard unit half).
// r[verify sched.merge.substitute-topdown+13]
// r[verify sched.merge.wanted-outputs+3]
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

// r[verify sched.merge.wanted-outputs+3]
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

// D3-retarget (flipped with the walk spawner's deletion): the reprobe
// lane survives — AS-5 reset + origin='reprobe' jobs replace the
// Poisoned→Substituting walk transition.
// r[verify sched.merge.poisoned-resubmit-bounded+4]
/// I-094 substitutable lane: a `Poisoned` node at the resubmit limit
/// whose output is upstream-substitutable (NOT locally present) on
/// resubmit gets the AS-5 reset to its dep-derived status plus an
/// origin='reprobe' materialization job — its prior failure is moot,
/// and the build proceeds instead of being fail-fasted by
/// `reconcile_preexisting` against the stale Poisoned status. The
/// locally-present case (routed via `cached_hits` →
/// `Poisoned → Completed`) is the synchronous lane; kept here as a
/// regression-guard so both lanes stay aligned.
#[rstest::rstest]
#[case::substitutable_upstream(false)]
#[case::locally_present(true)]
#[tokio::test]
async fn test_resubmit_poisoned_at_limit_substitutable(
    #[case] locally_present: bool,
) -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;
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
    let _ev2 = merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let info = expect_drv(&handle, tag).await;
    if locally_present {
        assert_eq!(
            info.status,
            DerivationStatus::Completed,
            "Poisoned → Completed (synchronous cached-hit lane)"
        );
        assert_eq!(
            query_status(&handle, build2).await?.state,
            rio_proto::types::BuildState::Succeeded as i32,
            "build #2 should succeed via re-probe"
        );
    } else {
        // The AS-5 reset + reprobe-origin job (the walk transition's
        // successor): prior failure moot, node claimable, build alive.
        assert!(
            matches!(
                info.status,
                DerivationStatus::Queued | DerivationStatus::Ready
            ),
            "Poisoned → dep-derived reset (AS-5), never stuck Poisoned; got {:?}",
            info.status
        );
        let (origin, job_state): (String, String) =
            sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = $1")
                .bind(tag)
                .fetch_one(&db.pool)
                .await?;
        assert_eq!(origin, "reprobe", "the reprobe lane's job origin");
        assert_eq!(job_state, "pending");
        assert_eq!(
            query_status(&handle, build2).await?.state,
            rio_proto::types::BuildState::Active as i32,
            "build #2 proceeds (the prior failure is moot)"
        );
        // The budget reset is DURABLE (the poison_cleared ledger row
        // rides the merge transaction); the in-memory counters refresh
        // when the fold replays the row — assert the row, not the
        // in-memory mirror (the walk's eager in-memory clear is gone).
        let classes: Vec<String> = sqlx::query_scalar(
            "SELECT outcome_class FROM drv_attempts t \
               JOIN derivations d USING (derivation_id) \
              WHERE d.drv_hash = $1",
        )
        .bind(tag)
        .fetch_all(&db.pool)
        .await?;
        assert!(
            classes.contains(&"poison_cleared".to_string()),
            "the poison_cleared reset row rides the merge transaction, got {classes:?}"
        );
    }
    if locally_present {
        assert_eq!(info.retry.resubmit_cycles, 0, "poison retry cleared");
    }
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
    crate::actor::tests::seed_default_tenant(&db.pool).await;
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
    // live_051(d): the hydrate seam clamps to the LIVE ceilings — the
    // test config's resolved global mem is 2 GiB (`test_default`), so
    // the 8 GiB row enters memory at the clamp, NOT raw (a floor above
    // the live global is stale evidence; the stale-solve-revalidation
    // law). Nonzero still proves I-208's own concern (RETURNING
    // carried the floor columns; `try_from_node` zeros were replaced);
    // the above-ceiling row's full law battery is
    // `floor_above_global_reclamps_at_boot` (sla_contract.rs).
    assert_eq!(
        info.sched.resource_floor.mem_bytes,
        (2 << 30) - rio_common::footprint::WORKER_MEM_OVERHEAD_BYTES,
        "I-208 + live_051(d) + merged_bug_016: floor hydrated from DB, \
         clamped at the live global's SOLVE-domain cap (global − pad — \
         a raw-global floor renders an unhostable container)"
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

// r[verify sched.merge.reconcile-order+2]
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
        "r[sched.merge.reconcile-order+2]: reprobe_unlocked advance must \
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

// D3-retarget (flipped with the walk spawner's deletion): the eager
// whole-submission probe survives; the routed mechanism is one job per
// substitutable leaf (in the merge transaction).
// r[verify sched.substitute.eager-probe]
/// Merge-time substitution covers the WHOLE submission in one
/// `FindMissingPaths`: with the store-side probe-truncation cap
/// removed, 5000 IA leaves whose outputs are all
/// upstream-substitutable MUST all be routed to materialization jobs
/// at merge time. Regression guard: pre-change, only the first 4096
/// (the store's truncated `substitutable_paths`) hit; the tail fell
/// through to dispatch-time layer-by-layer.
#[tokio::test]
async fn merge_probe_whole_dag_substituting() -> TestResult {
    const N: usize = 5000;
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let _ev = merge_dag(&handle, build_id, nodes, vec![], false).await?;

    // Tick refreshes the cached snapshot.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    // The §2.6 substitution-backlog bucket is job-derived: every leaf
    // carries a pending unclaimed job.
    let snap = handle.cluster_snapshot_cached();
    assert_eq!(
        snap.substituting_derivations as usize, N,
        "all {N} leaves must receive a merge-time substitutable verdict \
         (would be ≤4096 with store-side truncation)"
    );
    let jobs: i64 = sqlx::query_scalar("SELECT count(*) FROM materialization_jobs")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        jobs as usize, N,
        "one in-tx job per substitutable leaf — the whole submission, no truncation"
    );
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
// r[verify sched.merge.substitute-topdown+13]
/// Top-down: deps pruned from this build are NOT in the global DAG,
/// so a later build that needs them triggers its own cache-check.
///
/// Guards against the shared-DAG correctness bug where marking
/// deps as Completed without fetching would poison later builds
/// that actually need the dep NAR.
#[tokio::test]
async fn test_topdown_prune_deps_not_in_global_dag() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

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
    let _ev_a = merge_dag(
        &handle,
        build_a,
        vec![hello, glibc_a],
        vec![make_test_edge("hello-a", "glibc-a")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // A's prune fired: glibc dropped, hello routed to a pruned-origin
    // job (D3-retarget: the job replaces the detached fetch).
    assert_eq!(query_status(&handle, build_a).await?.total_derivations, 1);
    assert!(
        handle.debug_query_derivation("glibc-a").await?.is_none(),
        "glibc must be pruned out of the global DAG, not phantom-merged"
    );

    // Build B: app → glibc. app NOT substitutable → falls through
    // → full merge → glibc is newly_inserted (NOT pre-existing from
    // A, because A pruned it) → check_cached_outputs classifies it
    // substitutable → new_sub-lane job.
    let mut app = make_node("app-b");
    app.expected_output_paths = vec![test_store_path("app-b-out")];
    let mut glibc_b = make_node("glibc-a");
    glibc_b.expected_output_paths = vec![glibc_out.clone()];

    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag(
        &handle,
        build_b,
        vec![app, glibc_b],
        vec![make_test_edge("app-b", "glibc-a")],
        false,
    )
    .await?;
    barrier(&handle).await;

    // glibc got its own job under Build B — proves it wasn't stuck as
    // phantom-Completed from Build A's prune.
    let (origin, job_state): (String, String) =
        sqlx::query_as("SELECT origin, state FROM materialization_jobs WHERE drv_hash = 'glibc-a'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(origin, "cache_opportunity");
    assert_eq!(
        job_state, "pending",
        "Build B re-classifies glibc (pruned from A, newly-inserted in B)"
    );
    let _ = store;

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
/// Pending→Active activation, including the in-tx job creation) is
/// claims-floor fenced: a replica whose serving generation sits below
/// the durable floor — a successor has claimed — must NOT commit it.
/// The merge fails with `StaleGeneration` (mapped to gRPC UNAVAILABLE
/// by `actor_error_to_status` — pinned by the assertion below: the
/// health-aware balancer has already ejected the deposed replica, so
/// the gateway's bounded SubmitBuild retry-on-UNAVAILABLE lands on the
/// live leader) and leaves nothing behind: no derivation rows, no
/// build links, no Active build.
///
/// This is the deposed-believer MergeDag window the as-built posture
/// documented: leadership is checked at SubmitBuild enqueue time only,
/// so a replica deposed after the enqueue still executes its merge
/// transaction — racing the new leader's recovery — unless the
/// transaction itself is fenced.
// r[verify sched.evidence.durability+4]
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

    // bug_081 claim-twin: the status-code claim in the doc above is
    // executable — map the error through the PRODUCTION wire mapping
    // (`actor_error_to_status`), not a re-derivation, and pin the code.
    let status = crate::grpc::actor_guards::actor_error_to_status(
        reply
            .expect_err("fenced merge returned Ok")
            .downcast::<ActorError>()
            .expect("fenced merge error is not an ActorError"),
    );
    assert_eq!(
        status.code(),
        tonic::Code::Unavailable,
        "StaleGeneration's wire mapping changed: {status:?}"
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

// r[verify sched.build.keep-going]
/// Merging a build (keep_going=false) onto an at-resubmit-limit
/// Poisoned node fail-fasts with the negative-cache classification:
/// the WatchBuild snapshot reports Failed with failure_status ==
/// CachedFailure — pins the compile-forced merge-site status mapping
/// (the original classification is not stored on the DAG node; a
/// within-TTL poisoned node is exactly a cached failure).
#[tokio::test]
async fn test_merge_onto_poisoned_at_limit_snapshot_reports_cached_failure() -> TestResult {
    let (_db, _store, handle, _tasks) = setup_with_mock_store().await?;
    let tag = "merge-poisoned-status";
    let node = make_node(tag);

    // Build #1: merge + force-poison at the limit (not retriable on
    // resubmit; output neither in store nor substitutable, so the
    // reprobe lane stays cold and reconcile_preexisting fail-fasts).
    merge_dag(&handle, Uuid::new_v4(), vec![node.clone()], vec![], false).await?;
    assert!(
        handle
            .debug_force_poisoned(tag, crate::state::POISON_RESUBMIT_RETRY_LIMIT)
            .await?
    );
    barrier(&handle).await;

    // Build #2 onto the same hash → merge-time fail-fast.
    let build2 = Uuid::new_v4();
    let _ev2 = merge_dag(&handle, build2, vec![node], vec![], false).await?;
    barrier(&handle).await;

    let (reply_tx, reply_rx) = tokio::sync::oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id: build2,
            caller_tenant: None,
            reply: reply_tx,
        })
        .await?;
    let (_watch_rx, snapshot) = reply_rx.await??;
    let Some(rio_proto::types::build_event::Event::Snapshot(snap)) = snapshot.event else {
        panic!("expected a snapshot event, got {:?}", snapshot.event);
    };
    assert_eq!(
        snap.state,
        rio_proto::types::BuildState::Failed as i32,
        "merge onto poisoned-at-limit fail-fasts the build"
    );
    assert_eq!(
        snap.failure_status,
        rio_proto::types::BuildResultStatus::CachedFailure as i32,
        "merge-onto-poisoned maps to the negative-cache classification"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+13]
/// bug_390 (bughunt wave, A4): the pruned-origin gate must read the
/// DURABLE relation, not the truncatable in-memory child set. Shape:
/// R's durable children are {A (produced, live-vouched), B (unbuilt)};
/// B was sole-interest of a cancelled build and got reaped, truncating
/// the in-memory set to {A} — which is all-produced, so the in-memory
/// predicate laundered a Vouched verdict and the prune-merge created a
/// `cache`-lane job for a node whose closure is genuinely incomplete.
/// The durable read sees B unproduced and classifies the kept node
/// pruned-origin.
#[tokio::test]
async fn pruned_gate_uses_durable_evidence_not_truncated_view() -> TestResult {
    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let r_out = test_store_path("tdg-r-out");
    let a_out = test_store_path("tdg-a-out");
    let mk_r = || {
        let mut n = make_node("tdg-r");
        n.expected_output_paths = vec![r_out.clone()];
        n
    };
    let mk_a = || {
        let mut n = make_node("tdg-a");
        n.expected_output_paths = vec![a_out.clone()];
        n
    };
    let mk_b = || {
        let mut n = make_node("tdg-b");
        n.expected_output_paths = vec![test_store_path("tdg-b-out")];
        n
    };

    // B-keep: R -> A, with A's output locally present (A completes at
    // merge). Keeps R and A alive across the cancellation below.
    store.seed_with_content(&a_out, b"tdg-a-contents");
    let b_keep = Uuid::new_v4();
    merge_dag(
        &handle,
        b_keep,
        vec![mk_r(), mk_a()],
        vec![make_test_edge("tdg-r", "tdg-a")],
        false,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "tdg-a").await.status,
        DerivationStatus::Completed,
        "fixture premise: A is produced"
    );

    // B0: R -> {A, B}; B is unbuilt and sole-interest of B0.
    let b0 = Uuid::new_v4();
    merge_dag(
        &handle,
        b0,
        vec![mk_r(), mk_a(), mk_b()],
        vec![
            make_test_edge("tdg-r", "tdg-a"),
            make_test_edge("tdg-r", "tdg-b"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    // Cancel B0: B (sole interest) is reaped from the DAG; R's
    // in-memory child set truncates to {A} (all produced).
    {
        let (tx, rx) = tokio::sync::oneshot::channel();
        handle
            .send_unchecked(crate::actor::ActorCommand::CancelBuild {
                build_id: b0,
                caller_tenant: None,
                reason: "test cancel".into(),
                reply: tx,
            })
            .await?;
        let _ = rx.await??;
    }
    barrier(&handle).await;
    // Drive the deferred terminal cleanup directly (the tests'
    // standard bypass of TERMINAL_CLEANUP_DELAY): the reap truncates
    // R's in-memory child set to {A}.
    handle
        .send_unchecked(crate::actor::ActorCommand::CleanupTerminalBuild { build_id: b0 })
        .await?;
    barrier(&handle).await;
    assert!(
        handle.debug_query_derivation("tdg-b").await?.is_none(),
        "fixture premise: B is reaped (sole interest cancelled)"
    );

    // B1: R's wanted output substitutable -> the prune keeps {R}. The
    // gate must classify over the DURABLE relation (B unproduced) and
    // stamp pruned-origin; the truncated in-memory set would launder a
    // Vouched.
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
        vec![mk_r(), mk_a(), mk_b()],
        vec![
            make_test_edge("tdg-r", "tdg-a"),
            make_test_edge("tdg-r", "tdg-b"),
        ],
        false,
    )
    .await?;
    barrier(&handle).await;

    let (origin,): (String,) = sqlx::query_as(
        "SELECT origin FROM materialization_jobs WHERE drv_hash = 'tdg-r' \
         ORDER BY created_at DESC LIMIT 1",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        origin, "pruned",
        "the pruned-origin gate reads the durable relation (B unproduced) — \
         the reap-truncated in-memory set must not launder a vouch"
    );
    Ok(())
}

// r[verify sched.merge.probe-off-actor]
/// sh-036.1 red-first: with `precomputed_probe = Some(Ok(..))` threaded
/// on the request, phase-4 (`check_cached_outputs`) MUST apply the
/// pre-computed response without entering the in-actor
/// `find_missing_with_breaker` path. Structural assertion via the
/// [`FMP_AWAITS`](crate::actor::merge::FMP_AWAITS) entry counter —
/// `setup()` runs without a store client, so `check_roots_topdown`
/// short-circuits and the in-actor probe early-returns `Ok(None)`; the
/// counter is fed solely by phase-4's entry into the on-actor path.
///
/// `edges = []` → `check_roots_topdown` early-returns; all 50 nodes are
/// newly-inserted → `verify_preexisting_completed` early-returns.
#[tokio::test]
async fn merge_phase_4_never_awaits_store_rpc() -> TestResult {
    use std::sync::atomic::Ordering;

    let (_db, handle, _task) = setup().await;

    let nodes: Vec<_> = (0..50)
        .map(|i| {
            let mut n = make_node(&format!("offactor-{i}"));
            n.expected_output_paths = vec![test_store_path(&format!("offactor-{i}-out"))];
            n
        })
        .collect();
    let all_paths: Vec<String> = nodes
        .iter()
        .flat_map(|n| n.expected_output_paths.clone())
        .collect();

    let before = crate::actor::merge::FMP_AWAITS.load(Ordering::SeqCst);
    let req = MergeDagRequest {
        build_id: Uuid::new_v4(),
        tenant_id: Some(DEFAULT_TEST_TENANT),
        nodes,
        edges: vec![],
        precomputed_probe: Some(Ok(rio_proto::types::FindMissingPathsResponse {
            missing_paths: all_paths,
            ..Default::default()
        })),
        ..Default::default()
    };
    merge_dag_req(&handle, req).await?;
    let after = crate::actor::merge::FMP_AWAITS.load(Ordering::SeqCst);
    assert_eq!(
        after, before,
        "phase-4 must apply precomputed_probe without entering \
         find_missing_with_breaker (sh-036.1: 4.99s on-actor FMP); \
         RED at base: the in-actor probe path is entered → counter +1"
    );
    Ok(())
}
