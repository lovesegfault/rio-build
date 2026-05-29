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

// r[verify sched.ca.cutoff-propagate+2]
/// The real m052 scenario: A→B→C. Build X={A,B,C}. Build Y={A} only.
/// C fails, cascades to B,A. A.interested={X,Y}, C.interested={X}.
/// Old code: notify trigger's set {X} → Y hangs. New: union {X,Y}.
#[tokio::test]
async fn test_cascade_notifies_union_across_chain() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Build X: A→B→C.
    let build_x = Uuid::new_v4();
    let _ev_x = merge_dag(
        &handle,
        build_x,
        vec![
            make_node("chain-a"),
            make_node("chain-b"),
            make_node("chain-c"),
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
    let _ev_y = merge_dag(&handle, build_y, vec![make_node("chain-a")], vec![], true).await?;

    // C fails. Cascade: B→DependencyFailed, A→DependencyFailed.
    pull_complete_failure(
        &handle,
        "chain-c",
        rio_proto::types::BuildResultStatus::PermanentFailure,
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
        vec![make_node("canc-a"), make_node("canc-b")],
        vec![make_test_edge("canc-a", "canc-b")],
        false,
    )
    .await?;

    let pre_a = expect_drv(&handle, "canc-a").await;
    assert_eq!(pre_a.status, DerivationStatus::Queued);
    let pre_b = expect_drv(&handle, "canc-b").await;
    assert_eq!(pre_b.status, DerivationStatus::Ready);

    // Cancel. Before fix: A stays Queued, B stays Ready (only
    // Running/Assigned were transitioned). After: both DependencyFailed.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test".into(),
            reply: reply_tx,
        })
        .await?;
    let _ = reply_rx.await?;

    let post_a = expect_drv(&handle, "canc-a").await;
    assert_eq!(
        post_a.status,
        DerivationStatus::DependencyFailed,
        "sole-interest Queued must transition on cancel"
    );
    let post_b = expect_drv(&handle, "canc-b").await;
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
    let (_db, handle, _task) = setup().await;

    // Two independent nodes, keep_going=false. Both in flight.
    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![make_node("kg-a"), make_node("kg-b")],
        vec![],
        false, // keep_going=false (critical)
    )
    .await?;

    // Open both attempts so B is in flight when A fails.
    let _a1 = pull_attempt(&handle, "kg-a").await;
    let _a2 = pull_attempt(&handle, "kg-b").await;

    // A fails permanently. Before fix: build → Failed, but B stays
    // Assigned/Running (burning worker CPU). After: B cancelled.
    pull_complete_failure(
        &handle,
        "kg-a",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "perm",
    )
    .await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.state, rio_proto::types::BuildState::Failed as i32);

    let post_b = expect_drv(&handle, "kg-b").await;
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
    use sha2::Digest;

    let (db, store, handle, _tasks) = setup_with_mock_store().await?;

    let tenant = rio_store::test_helpers::seed_tenant(&db.pool, "cache-tenant").await;

    // Seed the store so FindMissingPaths returns empty (= cache hit).
    let out_path = test_store_path("cached-out");
    store.seed_with_content(&out_path, b"dummy");

    let mut node = make_node("cached-drv");
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

    let (db, handle, _task) = setup().await;

    let tenant_a = rio_store::test_helpers::seed_tenant(&db.pool, "pre-tenant-a").await;
    let tenant_b = rio_store::test_helpers::seed_tenant(&db.pool, "pre-tenant-b").await;

    // Build A completes the drv.
    let build_a = Uuid::new_v4();
    let _ev_a = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id: build_a,
            tenant_id: Some(tenant_a),
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("pre-drv")],
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
    pull_complete_success(&handle, "pre-drv", &out_path).await?;
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
            nodes: vec![make_node("pre-drv")],
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
    let (db, handle, _task) = setup().await;

    // No tenant_id on the build (None = single-tenant mode).
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "nt-drv", PriorityClass::Scheduled).await?;
    pull_complete_success(&handle, "nt-drv", &test_store_path("nt-out")).await?;
    barrier(&handle).await;

    let total: i64 = sqlx::query_scalar("SELECT COUNT(*) FROM path_tenants")
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(total, 0, "no tenant → no upsert");
    Ok(())
}

// ═══════════════════════════════════════════════════════════════════════════
// P0443: poison-removal paths miss derivation_hashes update → keep_going hang
// ═══════════════════════════════════════════════════════════════════════════

// r[verify sched.build.keep-going]
// r[verify sched.poison.ttl-persist]
// r[verify sched.admin.clear-poison+2]
/// keep_going=true build with 2 independent derivations. D1 poisoned,
/// D2 keeps running. D1 is removed from the DAG (via TTL-expiry tick OR
/// admin ClearPoison). D2 completes → build must reach terminal.
///
/// Pre-fix: `remove_node` left D1 in `derivation_hashes` → total=2,
/// completed=1, failed=0 (D1 gone from DAG so `build_summary` doesn't
/// count it) → hang Active forever. Post-fix: prune drops D1 →
/// total=1 → terminal.
#[rstest::rstest]
#[case::ttl_expiry(true)]
#[case::admin_clear(false)]
#[tokio::test]
async fn test_poison_removal_keep_going_completes(#[case] via_ttl: bool) -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![make_node("pr-d1"), make_node("pr-d2")],
        vec![],
        true, // keep_going
    )
    .await?;

    // Open both attempts so D2 is in flight while D1 poisons.
    let _a1 = pull_attempt(&handle, "pr-d1").await;
    let _a2 = pull_attempt(&handle, "pr-d2").await;

    // Poison D1. keep_going=true → build stays Active, D2 keeps running.
    pull_complete_failure(
        &handle,
        "pr-d1",
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "bad",
    )
    .await?;
    assert_eq!(
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Active as i32,
        "keep_going=true: build stays Active while D2 runs"
    );

    // Remove D1 from DAG: either TTL-expiry tick or admin ClearPoison.
    if via_ttl {
        tokio::time::sleep(crate::state::POISON_TTL + std::time::Duration::from_millis(50)).await;
        handle.send_unchecked(ActorCommand::Tick).await?;
        barrier(&handle).await;
    } else {
        let (tx, rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::ClearPoison {
                drv_hash: "pr-d1".into(),
                reply: tx,
            })
            .await?;
        assert!(rx.await?, "ClearPoison → cleared=true");
    }
    assert!(
        handle.debug_query_derivation("pr-d1").await?.is_none(),
        "D1 must be removed from the DAG"
    );

    // Complete D2. Pre-fix: spuriously Succeeded (failed_count derived from
    // DAG → 0 after D1 removed). Post-fix: sticky error_summary forces Failed.
    pull_complete_success_empty(&handle, "pr-d2").await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "D1 was poisoned → build must end Failed even after poison cleared"
    );
    assert!(
        !status.error_summary.is_empty(),
        "error_summary must report the poisoned derivation"
    );
    Ok(())
}
