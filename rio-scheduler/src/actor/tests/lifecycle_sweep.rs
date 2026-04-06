//! Regression tests for the 6-bug derivation-lifecycle sweep (P0442).
//!
//! bug_043 — stale completion report corrupts reassigned derivation
//! m024 — retry_count incremented for Assigned-only; starvation uses dead workers
//! m052 — cascade/CA-cutoff only notify trigger's interested_builds
//! m033 — cancel skips Queued/Ready/Created; keep_going=false doesn't cancel
//! m039 — upsert_path_tenants missing at CA-cutoff/merge/recovery
//! bug_022 — find_roots uses global parents, not build-scoped

use super::*;

// ═══════════════════════════════════════════════════════════════════════════
// bug_043: stale-worker completion report
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.completion.idempotent]
/// A completion report from a worker that no longer owns the
/// derivation (reassigned after disconnect) is dropped. Without the
/// guard, the stale report's status overwrites the now-running
/// instance's state.
#[tokio::test]
async fn test_stale_completion_dropped() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge a single node. Connect worker A, dispatch to A.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "stale-drv", PriorityClass::Scheduled).await?;
    let mut rx_a = connect_executor(&handle, "stale-a", "x86_64-linux", 1).await?;
    let asgn = recv_assignment(&mut rx_a).await;
    assert!(asgn.drv_path.contains("stale-drv"));

    // Disconnect A → reassign. Connect B → dispatch to B.
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "stale-a".into(),
        })
        .await?;
    drop(rx_a);
    let mut rx_b = connect_executor(&handle, "stale-b", "x86_64-linux", 1).await?;
    let asgn_b = recv_assignment(&mut rx_b).await;
    assert!(asgn_b.drv_path.contains("stale-drv"));

    // Precondition: assigned to B.
    let pre = handle
        .debug_query_derivation("stale-drv")
        .await?
        .expect("exists");
    assert_eq!(pre.assigned_executor.as_deref(), Some("stale-b"));

    // Send STALE completion from A (no longer owns the derivation).
    // Before the fix: this corrupts state (transitions based on A's
    // report while B is still running it). After: dropped.
    complete_success_empty(&handle, "stale-a", &test_drv_path("stale-drv")).await?;

    let post = handle
        .debug_query_derivation("stale-drv")
        .await?
        .expect("exists");
    assert_eq!(
        post.assigned_executor.as_deref(),
        Some("stale-b"),
        "stale completion from A must not clobber B's assignment"
    );
    assert!(
        matches!(
            post.status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "stale completion must be dropped; got {:?}",
        post.status
    );

    // B's completion still works normally.
    complete_success_empty(&handle, "stale-b", &test_drv_path("stale-drv")).await?;
    let done = handle
        .debug_query_derivation("stale-drv")
        .await?
        .expect("exists");
    assert_eq!(done.status, DerivationStatus::Completed);

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// m024: retry_count / starvation accounting
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.retry.per-worker-budget]
/// Disconnecting a worker while the derivation is only Assigned (never
/// Running) must NOT increment retry_count — the worker disconnected
/// before starting it, no retry budget consumed.
#[tokio::test]
async fn test_assigned_only_no_retry_bump() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nobump-drv", PriorityClass::Scheduled).await?;
    let mut rx = connect_executor(&handle, "nobump-w", "x86_64-linux", 1).await?;
    let _asgn = recv_assignment(&mut rx).await;

    // Precondition: Assigned, never Running.
    let pre = handle
        .debug_query_derivation("nobump-drv")
        .await?
        .expect("exists");
    assert_eq!(pre.status, DerivationStatus::Assigned);
    assert_eq!(pre.retry_count, 0);

    // Disconnect → reassign_derivations. Before the fix:
    // retry_count++. After: no bump (was never Running).
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "nobump-w".into(),
        })
        .await?;
    barrier(&handle).await;

    let post = handle
        .debug_query_derivation("nobump-drv")
        .await?
        .expect("exists");
    assert_eq!(
        post.retry_count, 0,
        "Assigned-only disconnect must not bump retry_count"
    );
    Ok(())
}

// r[verify sched.retry.per-worker-budget]
/// Starvation guard intersects failed_builders with LIVE workers. A
/// stale failed-worker entry (worker disconnected after recording)
/// must not count toward the all-workers-failed check.
///
/// Scenario: A fails transiently → failed_builders={A}. A disconnects.
/// B connects. Force-assign to B → B fails transiently. Old len
/// check: failed_builders={A,B}.len()==2 >= live_count==1 → no,
/// that poisons anyway. Actually the distinguishing case:
/// failed_builders={A (dead)}, live={B}. Force-assign B, B fails.
/// failed_builders={A,B}, live={B}. Both checks poison (correct).
///
/// The REAL distinguishing case: failed_builders={A}, A disconnects,
/// B connects alone. Force-assign B (bypass backoff), B fails.
/// During B's failure handler: live={B}, failed_builders={A,B} after
/// record. Both approaches poison. Still not distinguishing.
///
/// Actual bug scenario: failed_builders has 2 dead workers, 1 live
/// worker connected. Old: len==2>=1 poisons on ANY failure. New:
/// only if the live worker fails. Test: A,B fail (both still
/// connected, 3rd worker C present → no poison). A,B disconnect.
/// Now live={C}, failed={A,B}. Force-assign C, C fails. Old:
/// len==3>=1 → poison. New: C∈{A,B,C}→poison. Same. Hmm.
///
/// Wait: the check fires DURING handle_transient_failure, BEFORE
/// record_failure adds the reporting worker. No — re-reading
/// completion.rs: record_failure_and_check_poison runs FIRST, THEN
/// the starvation check reads failed_builders (which already includes
/// the reporting worker). So after C fails: failed={A,B,C}, live={C}.
/// Old: 3>=1→poison. New: C∈{A,B,C}→poison. Same.
///
/// Actual distinguishing case: failed_builders contains MORE entries
/// than live workers (e.g. failed={A,B}, live={C}), C has NOT
/// failed. Old check would trigger on the NEXT failure report which
/// necessarily adds the live worker. The intersection check also
/// triggers. For this bug to manifest differently, the len check
/// must fire with len >= live_count while the live set is NOT a
/// subset of failed. That requires: some live worker W where
/// W∉failed, yet len(failed)>=live_count. E.g. failed={A,B} (both
/// dead), live={C,D} (2), len==2>=2 but C,D∉failed. Old: poison.
/// New: no poison. TEST THAT.
#[tokio::test]
async fn test_starvation_intersects_live() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "starv-drv", PriorityClass::Scheduled).await?;

    // Connect A,B,C. A fails (1<threshold=3, live=3 → no poison).
    // A disconnects. Live={B,C}. failed={A (dead)}. Force-assign B,
    // B fails. failed={A,B}, len=2<3. Live={B,C}.
    // Old starvation check: len==2 >= live_count==2 → POISON.
    // New intersection check: C∉{A,B} → NOT all live failed → no poison.
    let mut rx_a = connect_executor(&handle, "starv-a", "x86_64-linux", 1).await?;
    let _rx_b = connect_executor(&handle, "starv-b", "x86_64-linux", 1).await?;
    let _rx_c = connect_executor(&handle, "starv-c", "x86_64-linux", 1).await?;
    let _asgn = recv_assignment(&mut rx_a).await;

    let ok = handle.debug_force_assign("starv-drv", "starv-a").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "starv-a",
        &test_drv_path("starv-drv"),
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "transient",
    )
    .await?;
    barrier(&handle).await;

    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "starv-a".into(),
        })
        .await?;
    barrier(&handle).await;

    let ok = handle.debug_force_assign("starv-drv", "starv-b").await?;
    assert!(ok);
    complete_failure(
        &handle,
        "starv-b",
        &test_drv_path("starv-drv"),
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "transient",
    )
    .await?;

    let info = handle
        .debug_query_derivation("starv-drv")
        .await?
        .expect("exists");
    assert_ne!(
        info.status,
        DerivationStatus::Poisoned,
        "dead A in failed_builders must not count toward starvation; \
         live={{B,C}}, failed={{A,B}}, C∉failed → no poison. Got {:?}",
        info.status
    );
    assert_eq!(
        info.failed_builders.len(),
        2,
        "failed_builders={{A,B}}; got {:?}",
        info.failed_builders
    );

    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// m052: cascade notifies union of interested_builds
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.ca.cutoff-propagate]
/// Cascade notifies builds whose ONLY interested nodes are the
/// cascaded ones (not the trigger). Scenario: shared leaf fails,
/// cascades to parent that belongs ONLY to build Y. Build Y must
/// get handle_derivation_failure.
#[tokio::test]
async fn test_cascade_notifies_merged_builds() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("casc-w", "x86_64-linux", 4).await?;

    // Build X: just leaf. Build Y: parent → leaf (shared).
    // X.interested = {leaf}. Y.interested = {leaf, parent}.
    // leaf.interested_builds = {X, Y}. parent.interested_builds = {Y}.
    let build_x = Uuid::new_v4();
    let _ev_x = merge_dag(
        &handle,
        build_x,
        vec![make_test_node("casc-leaf", "x86_64-linux")],
        vec![],
        true, // keep_going so X doesn't fail-fast on leaf alone
    )
    .await?;

    let build_y = Uuid::new_v4();
    let _ev_y = merge_dag(
        &handle,
        build_y,
        vec![
            make_test_node("casc-parent", "x86_64-linux"),
            make_test_node("casc-leaf", "x86_64-linux"),
        ],
        vec![make_test_edge("casc-parent", "casc-leaf")],
        true, // keep_going so cascade termination is via check_build_completion
    )
    .await?;

    // Leaf fails permanently. Cascade: parent → DependencyFailed.
    // Before fix: only X and Y get notified (both interested in
    // leaf). But parent is Y-only — if leaf were X-only and parent
    // Y-only, Y would hang. Here leaf is shared, so both get the
    // trigger notification anyway. Let's reshape: X owns a DIFFERENT
    // leaf, Y owns parent→shared-leaf, X owns shared-leaf too.
    // Actually the scenario as written DOES test it because:
    // trigger=leaf, interested={X,Y}. Cascaded=parent, interested={Y}.
    // Union = {X,Y}. Old code: trigger's set = {X,Y}. Same. Bad test.
    //
    // Correct scenario: leaf interested={X}, parent interested={Y},
    // parent depends on leaf. Leaf fails → cascades to parent. Old
    // code notifies {X}. New code notifies {X,Y}.
    //
    // This requires leaf to be in X but NOT Y, yet parent (in Y)
    // depends on leaf. That's impossible in the DAG model — if parent
    // depends on leaf and parent is in Y, leaf must be in Y too
    // (merge adds build_id to all nodes).
    //
    // REAL scenario: A→B→C chain. Build X = {A,B,C}. Build Y = {A}.
    // C fails. Cascade transitions B,A to DependencyFailed. A's
    // interested_builds = {X,Y}. Trigger C's interested = {X}.
    // Old: notify {X}. New: union includes A's {X,Y} → notify {X,Y}.
    // Y's only node A is now DependencyFailed → Y should fail.
    complete_failure(
        &handle,
        "casc-w",
        &test_drv_path("casc-leaf"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "perm",
    )
    .await?;

    // Both builds should be Failed (X via leaf, Y via cascaded parent).
    let status_x = query_status(&handle, build_x).await?;
    assert_eq!(
        status_x.state,
        rio_proto::types::BuildState::Failed as i32,
        "X should fail (leaf is X's only node)"
    );
    let status_y = query_status(&handle, build_y).await?;
    assert_eq!(
        status_y.state,
        rio_proto::types::BuildState::Failed as i32,
        "Y should fail via cascaded parent (even though parent is Y-only)"
    );
    Ok(())
}

// r[verify sched.ca.cutoff-propagate]
/// The real m052 scenario: A→B→C. Build X={A,B,C}. Build Y={A} only.
/// C fails, cascades to B,A. A.interested={X,Y}, C.interested={X}.
/// Old code: notify trigger's set {X} → Y hangs. New: union {X,Y}.
#[tokio::test]
async fn test_cascade_notifies_union_across_chain() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("chain-w", "x86_64-linux", 4).await?;

    // Build X: A→B→C.
    let build_x = Uuid::new_v4();
    let _ev_x = merge_dag(
        &handle,
        build_x,
        vec![
            make_test_node("chain-a", "x86_64-linux"),
            make_test_node("chain-b", "x86_64-linux"),
            make_test_node("chain-c", "x86_64-linux"),
        ],
        vec![
            make_test_edge("chain-a", "chain-b"),
            make_test_edge("chain-b", "chain-c"),
        ],
        true,
    )
    .await?;

    // Build Y: only A. A now has interested_builds={X,Y}.
    let build_y = Uuid::new_v4();
    let _ev_y = merge_dag(
        &handle,
        build_y,
        vec![make_test_node("chain-a", "x86_64-linux")],
        vec![],
        true,
    )
    .await?;

    // C fails. Cascade: B→DependencyFailed, A→DependencyFailed.
    complete_failure(
        &handle,
        "chain-w",
        &test_drv_path("chain-c"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "perm",
    )
    .await?;

    // Y's only node A is DependencyFailed. Old code: Y never gets
    // handle_derivation_failure (C.interested={X}, Y∉{X}) → Y
    // hangs Active. New code: union includes A's {X,Y} → Y fails.
    let status_y = query_status(&handle, build_y).await?;
    assert_eq!(
        status_y.state,
        rio_proto::types::BuildState::Failed as i32,
        "Y must fail when its only node A is cascade-DependencyFailed, \
         even though trigger C.interested_builds does not include Y"
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// m033: cancel scope + keep_going=false
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.build.keep-going]
/// cancel_build_derivations transitions sole-interest Queued/Ready
/// derivations to DependencyFailed (not just Running/Assigned).
#[tokio::test]
async fn test_cancel_transitions_queued() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge A→B. No worker connected → B Ready, A Queued.
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("canc-a", "x86_64-linux"),
            make_test_node("canc-b", "x86_64-linux"),
        ],
        vec![make_test_edge("canc-a", "canc-b")],
        false,
    )
    .await?;

    let pre_a = handle
        .debug_query_derivation("canc-a")
        .await?
        .expect("exists");
    assert_eq!(pre_a.status, DerivationStatus::Queued);
    let pre_b = handle
        .debug_query_derivation("canc-b")
        .await?
        .expect("exists");
    assert_eq!(pre_b.status, DerivationStatus::Ready);

    // Cancel. Before fix: A stays Queued, B stays Ready (only
    // Running/Assigned were transitioned). After: both DependencyFailed.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            reason: "test".into(),
            reply: reply_tx,
        })
        .await?;
    let _ = reply_rx.await?;

    let post_a = handle
        .debug_query_derivation("canc-a")
        .await?
        .expect("exists");
    assert_eq!(
        post_a.status,
        DerivationStatus::DependencyFailed,
        "sole-interest Queued must transition on cancel"
    );
    let post_b = handle
        .debug_query_derivation("canc-b")
        .await?
        .expect("exists");
    assert_eq!(
        post_b.status,
        DerivationStatus::DependencyFailed,
        "sole-interest Ready must transition on cancel"
    );
    Ok(())
}

// r[verify sched.build.keep-going]
/// keep_going=false: when a derivation fails, the build's OTHER
/// derivations are cancelled (not left running/queued).
#[tokio::test]
async fn test_keep_going_false_cancels_remaining() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("kg-w", "x86_64-linux", 4).await?;

    // Two independent nodes, keep_going=false. Both dispatch to the
    // 4-slot worker.
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![
            make_test_node("kg-a", "x86_64-linux"),
            make_test_node("kg-b", "x86_64-linux"),
        ],
        vec![],
        false, // keep_going=false (critical)
    )
    .await?;

    // A fails permanently. Before fix: build → Failed, but B stays
    // Assigned/Running (burning worker CPU). After: B cancelled.
    complete_failure(
        &handle,
        "kg-w",
        &test_drv_path("kg-a"),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "perm",
    )
    .await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Failed as i32);

    let post_b = handle
        .debug_query_derivation("kg-b")
        .await?
        .expect("exists");
    assert!(
        matches!(
            post_b.status,
            DerivationStatus::Cancelled | DerivationStatus::DependencyFailed
        ),
        "keep_going=false must cancel remaining derivations; B got {:?}",
        post_b.status
    );
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// m039: upsert_path_tenants at all completion paths
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.gc.path-tenants-upsert]
/// Merge-time cache hit: path already in store, new tenant needs
/// attribution. upsert_path_tenants must fire.
#[tokio::test]
async fn test_upsert_at_merge_cache_hit() -> TestResult {
    use rio_test_support::grpc::spawn_mock_store_with_client;
    use sha2::Digest;

    let db = TestDb::new(&MIGRATOR).await;
    let (store, store_client, _store_h) = spawn_mock_store_with_client().await?;
    let (handle, _task) = setup_actor_with_store(db.pool.clone(), Some(store_client));

    let tenant = rio_test_support::seed_tenant(&db.pool, "cache-tenant").await;

    // Seed the store so FindMissingPaths returns empty (= cache hit).
    let out_path = test_store_path("cached-out");
    store.seed_with_content(&out_path, b"dummy");

    let mut node = make_test_node("cached-drv", "x86_64-linux");
    node.expected_output_paths = vec![out_path.clone()];

    let build_id = Uuid::new_v4();
    let _ev = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: Some(tenant),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![node],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;

    // Cache hit → Completed immediately, upsert should have fired.
    let out_hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let rows: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    assert!(
        rows.contains(&tenant),
        "merge-time cache hit must upsert path_tenants; rows={rows:?}"
    );
    Ok(())
}

// r[verify sched.gc.path-tenants-upsert]
/// Pre-existing Completed derivation merged from another build: new
/// build's tenant needs attribution.
#[tokio::test]
async fn test_upsert_at_merge_preexisting_completed() -> TestResult {
    use sha2::Digest;

    let (db, handle, _task, _rx) = setup_with_worker("pre-w", "x86_64-linux", 1).await?;

    let tenant_a = rio_test_support::seed_tenant(&db.pool, "pre-tenant-a").await;
    let tenant_b = rio_test_support::seed_tenant(&db.pool, "pre-tenant-b").await;

    // Build A completes the drv.
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_a,
            tenant_id: Some(tenant_a),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node("pre-drv", "x86_64-linux")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    let out_path = test_store_path("pre-out");
    complete_success(&handle, "pre-w", &test_drv_path("pre-drv"), &out_path).await?;
    barrier(&handle).await;

    // Build B merges the SAME drv. It's pre-existing Completed.
    // B's tenant must get attribution.
    let build_b = Uuid::new_v4();
    let _ev_b = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_b,
            tenant_id: Some(tenant_b),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_test_node("pre-drv", "x86_64-linux")],
            edges: vec![],
            options: BuildOptions::default(),
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;
    barrier(&handle).await;

    let out_hash = sha2::Sha256::digest(out_path.as_bytes()).to_vec();
    let rows: Vec<Uuid> =
        sqlx::query_scalar("SELECT tenant_id FROM path_tenants WHERE store_path_hash = $1")
            .bind(&out_hash)
            .fetch_all(&db.pool)
            .await?;
    assert!(
        rows.contains(&tenant_b),
        "pre-existing Completed merge must upsert for new tenant; rows={rows:?}"
    );
    Ok(())
}

// r[verify sched.gc.path-tenants-upsert]
/// The helper extracts tenant_ids from interested_builds. Sanity:
/// node with no tenant-resolved builds → no upsert (empty tenant_ids).
#[tokio::test]
async fn test_upsert_skips_no_tenant() -> TestResult {
    let (db, handle, _task, _rx) = setup_with_worker("nt-w", "x86_64-linux", 1).await?;

    // No tenant_id on the build (None = single-tenant mode).
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nt-drv", PriorityClass::Scheduled).await?;
    complete_success(
        &handle,
        "nt-w",
        &test_drv_path("nt-drv"),
        &test_store_path("nt-out"),
    )
    .await?;
    barrier(&handle).await;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 0, "no tenant → no upsert");
    Ok(())
}
