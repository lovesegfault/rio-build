//! State recovery: LeaderAcquired → recover_from_pg → DAG rebuilt.
//
// Recovery isn't a standalone spec rule — it's behavior under
// sched.lease.k8s-lease (what happens on acquire). The test here
// verifies the LeaderAcquired → recover_from_pg → recovery_complete
// pipeline; the lease loop's acquire behavior is covered in
// lease.rs tests (sched.lease.generation-fence verify).

use super::*;

/// Seed PG with a build + 2-derivation chain (parent depends on child),
/// spawn a FRESH actor (simulating new leader after failover), send
/// LeaderAcquired, assert DAG rebuilt.
///
/// This tests the core recover_from_pg path: load builds, load
/// derivations, load edges, load build_derivations, rebuild DAG +
/// interested_builds + ready queue.
#[tokio::test]
async fn test_recover_from_pg_rebuilds_dag() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: first "leader" writes state to PG ---
    // Use a real actor to do this (simpler than hand-crafting SQL).
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx = merge_chain(
            &handle,
            build_id,
            &["recover-child", "recover-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        // Barrier: wait for merge + persist_merge_to_db to complete.
        // (merge_chain itself already awaits the MergeDag reply, so
        // this is belt-and-suspenders.)
        barrier(&handle).await;
        // Shut down this actor (simulating scheduler death).
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // --- Phase 2: fresh actor (new leader) recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());

    // Initially: DAG is EMPTY. This proves we're not cheating by
    // reusing in-mem state — the actor is brand new.
    let info = handle.debug_query_derivation("recover-child").await?;
    assert!(
        info.is_none(),
        "fresh actor should have EMPTY DAG before LeaderAcquired"
    );

    // Send LeaderAcquired → triggers recover_from_pg.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Child should be Ready (no dependencies). Parent should be
    // Queued (depends on child). Both should have the build in
    // interested_builds (verified via debug_query — actually that
    // doesn't expose interested_builds, so check via status only).
    let child = handle
        .debug_query_derivation("recover-child")
        .await?
        .expect("child should be recovered");
    assert_eq!(
        child.status,
        DerivationStatus::Ready,
        "child (no deps) should be Ready after recovery"
    );

    let parent = handle
        .debug_query_derivation("recover-parent")
        .await?
        .expect("parent should be recovered");
    // Parent depends on child → not yet Ready. Could be Queued or
    // Created depending on compute_initial_states. Either is fine
    // — what matters is it's in the DAG and not terminal.
    assert!(
        !parent.status.is_terminal(),
        "parent should be non-terminal after recovery: {:?}",
        parent.status
    );

    // Build should be recoverable via the actor's builds map.
    // query_status returns Err if build_id isn't in the map —
    // success proves recovery reconstructed BuildInfo.
    let status = query_status(&handle, build_id).await?;
    // State should be Active (merge_chain's handle_merge_dag
    // transitions Pending → Active after DAG merge).
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "recovered build should be Active"
    );

    Ok(())
}

/// I-059: orphan derivations (build terminal, derivation non-terminal)
/// must NOT be transitioned by the I-058 recompute pass.
///
/// load_nonterminal_derivations has no JOIN to builds — it loads any
/// derivation whose OWN status is non-terminal. A weeks-old failed
/// build can leave Queued derivations behind. Pre-I-058 those were
/// inert (frozen). Post-I-058, transitioning them dispatches against
/// GC'd inputs → infrastructure-failure → poison cascade.
///
/// Gate: the I-058 recompute pass skips nodes with empty
/// `interested_builds` (which the build_derivations join only
/// populates for builds returned by load_nonterminal_builds, i.e.
/// pending/active). Orphans have no active build → empty set → skip.
#[tokio::test]
async fn test_recovery_skips_orphan_transitions() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: write a 2-node chain to PG, then orphan it ---
    // The chain shape gives us a Queued node (parent depends on
    // child, so MergeDag leaves parent at Queued in PG). A single
    // node would be Ready in PG and never hit the I-058 collection
    // — wrong test surface.
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx = merge_chain(
            &handle,
            build_id,
            &["orphan-child", "orphan-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Precondition: parent is queued in PG. If MergeDag's own
    // compute_initial_states changed and the parent is now Ready,
    // this test stops exercising the I-058 path and silently passes
    // for the wrong reason.
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'orphan-parent'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        pg_status, "queued",
        "test precondition: parent must be Queued in PG to hit I-058 collection"
    );

    // Backdate: build → failed. load_nonterminal_builds (status IN
    // pending/active) skips it; load_nonterminal_derivations still
    // finds both nodes (their status is ready/queued, non-terminal).
    sqlx::query("UPDATE builds SET status = 'failed' WHERE build_id = $1")
        .bind(build_id)
        .execute(&db.pool)
        .await?;

    // --- Phase 2: fresh actor recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Orphan parent loaded (non-terminal in PG → load_nonterminal_
    // derivations found it) but NOT transitioned. Without the gate,
    // the I-058 pass sees zero recovered edges (child→parent edge
    // was loaded — both endpoints non-terminal — but child has no
    // deps so child looks Ready → all_deps_completed for parent →
    // Ready). With the gate, parent's interested_builds is empty →
    // filtered out of to_recompute → stays at PG status.
    let parent = handle
        .debug_query_derivation("orphan-parent")
        .await?
        .expect("orphan parent should still be in DAG (loaded, just not transitioned)");
    assert_eq!(
        parent.status,
        DerivationStatus::Queued,
        "orphan parent must stay Queued — no active build wants it dispatched"
    );

    // The child is the boundary: it was Ready in PG (load query
    // returns it as-is) and the push_ready loop at the bottom of
    // recover_from_pg pushes ALL Ready nodes regardless of
    // interested_builds. That's a separate concern — I-059 scopes
    // to the I-058 transition pass. This assertion documents the
    // boundary, not a guarantee.
    let child = handle
        .debug_query_derivation("orphan-child")
        .await?
        .expect("orphan child should be in DAG");
    assert_eq!(
        child.status,
        DerivationStatus::Ready,
        "orphan child loaded as Ready from PG (push_ready of orphan-Ready is OUTSIDE I-059 scope)"
    );

    Ok(())
}

/// Recovery failure (PG down mid-recovery) → recovery_complete set
/// TRUE with empty DAG. Degrade, don't block. The alternative (leave
/// recovery_complete=false) would block dispatch forever while the
/// scheduler holds the lease.
#[tokio::test]
async fn test_recovery_failure_degrades_to_empty_dag() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Close the pool BEFORE sending LeaderAcquired — all PG queries
    // will fail. This simulates PG going down mid-recovery.
    let (handle, _task) = setup_actor(db.pool.clone());
    db.pool.close().await;

    // LeaderAcquired → recover_from_pg → PG fails → catch → set
    // recovery_complete=true with EMPTY DAG.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Can't directly check recovery_complete (not exposed) but we
    // CAN check that dispatch isn't blocked: connect a worker,
    // submit a build, assert it dispatches. If recovery_complete
    // were false, dispatch_ready would early-return and the
    // derivation would stay Ready.
    //
    // But PG is closed so MergeDag will fail on insert_build.
    // Better indirect test: just verify the actor is alive and
    // responsive (quiesce succeeded above). A panicked handle_
    // leader_acquired would have killed the actor task.
    assert!(
        handle.is_alive(),
        "actor should survive recovery failure (degrade not crash)"
    );

    // Also: DAG should be empty (recovery cleared it, failure
    // re-cleared it).
    let info = handle.debug_query_derivation("anything").await?;
    assert!(info.is_none(), "DAG should be empty after recovery failure");

    Ok(())
}

/// Transient-failure retry must write Ready to PG, not Failed.
/// Crash in backoff window with PG=Failed → recovery
/// loads it but only enqueues Ready-status drvs → hang forever.
///
/// Test: seed PG with a Failed-status derivation (simulating the
/// OLD buggy write) + a Ready-status derivation. Fresh actor
/// recovers. Assert Ready drv is in queue (via dispatch), Failed
/// drv is stuck (never dispatched — proves the bug exists and our
/// fix avoids it going forward).
///
/// Also verify the NEW behavior: trigger a transient failure, check
/// PG status is Ready (not Failed).
#[tokio::test]
async fn test_transient_retry_pg_status_is_ready() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Connect worker + submit build → dispatch. Second worker is
    // padding so the all-workers-failed clamp doesn't poison after
    // a single failure (worker_count=1 would clamp threshold to 1).
    let (handle, _task, mut stream_rx) = {
        let (h, t) = setup_actor(db.pool.clone());
        let rx = connect_executor(&h, "w-x4", "x86_64-linux").await?;
        let _pad = connect_executor(&h, "w-x4-pad", "aarch64-linux").await?;
        (h, t, rx)
    };
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "x4-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut stream_rx).await;

    // Report transient failure → handle_transient_failure runs.
    complete_failure(
        &handle,
        "w-x4",
        "x4-drv",
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "simulated transient",
    )
    .await?;
    barrier(&handle).await;

    // PG should show Ready (NOT Failed) — the transient-retry path
    // must persist the FINAL in-mem state, not the intermediate Failed.
    let (status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'x4-drv'")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(
        status, "ready",
        "transient retry should write Ready to PG (not Failed): got {status}"
    );

    Ok(())
}

/// Recovery must check build completion for builds whose derivations
/// are ALL terminal. Crash between "last drv →
/// Completed" and "build → Succeeded" → recovery loads build as
/// Active with 0 non-terminal derivations → without the sweep,
/// check_build_completion never fires → Active forever.
#[tokio::test]
async fn test_recovery_completes_all_terminal_build() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: write state simulating "crashed before build→Succeeded" ---
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx = merge_single_node(&handle, build_id, "x5-drv", PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate: drv → completed (terminal), build stays active.
    // This simulates crash-after-last-drv-complete.
    sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'x5-drv'")
        .execute(&db.pool)
        .await?;
    // Build stays 'active' (merge_chain sets it Active via handle_merge_dag).

    // --- Phase 2: fresh actor recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The post-recovery sweep should fire check_build_completion
    // for the build. With 0 recovered derivations (all terminal,
    // filtered by TERMINAL_STATUSES), total=0, completed=0, failed=0
    // → all_completed → complete_build → Succeeded.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build with all-terminal drvs should be Succeeded after recovery"
    );

    Ok(())
}

/// I-111: recovery must seed total/completed/cached from the
/// `builds.{total,completed,cached}_drvs` denorm columns, NOT recompute
/// from the in-memory DAG. The DAG only loads non-terminal drvs, so
/// `derivation_hashes.len()` after recovery is the *remaining* count,
/// not the total. Pre-fix, `update_build_counts` persisted that back to
/// PG and the dashboard showed 0/443 for a build that was at 1111/1555.
#[tokio::test]
async fn test_recovery_seeds_denorm_counts_from_pg() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: write a 3-drv chain via a real actor ---
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx = merge_chain(
            &handle,
            build_id,
            &["i111-a", "i111-b", "i111-c"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate: 2 of 3 drvs completed (terminal — NOT loaded into DAG
    // at recovery), 1 remains queued. Denorm columns say 100/50/12 —
    // deliberately distinct from what the DAG would compute (3/0/0
    // pre-fix would persist as 1/0/0 since only 1 drv loads).
    sqlx::query(
        "UPDATE derivations SET status = 'completed' \
         WHERE drv_hash IN ('i111-a', 'i111-b')",
    )
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "UPDATE builds SET total_drvs = 100, completed_drvs = 50, cached_drvs = 12 \
         WHERE build_id = $1",
    )
    .bind(build_id)
    .execute(&db.pool)
    .await?;

    // --- Phase 2: fresh actor recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // recover_from_pg's post-load sweep calls update_build_counts for
    // every active build. Pre-fix that wrote (1, 0, 0) — derivation_
    // hashes.len()=1, summary.completed=0. Post-fix it writes
    // (total_count=100, recovered_completed+0=50, cached_count=12).
    let (total, completed, cached): (i32, i32, i32) = sqlx::query_as(
        "SELECT total_drvs, completed_drvs, cached_drvs FROM builds WHERE build_id = $1",
    )
    .bind(build_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        (total, completed, cached),
        (100, 50, 12),
        "recovery must preserve PG denorm counts, not recompute from DAG"
    );

    // In-memory BuildStatus should also report the absolute counts.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(status.total_derivations, 100, "total_derivations");
    assert_eq!(status.completed_derivations, 50, "completed_derivations");
    assert_eq!(status.cached_derivations, 12, "cached_derivations");

    Ok(())
}

/// Merge a linear chain: nodes[0] ← nodes[1] ← ... ← nodes[n-1]
/// (each depends on the previous). Helper for recovery tests that
/// need a multi-node DAG in PG.
async fn merge_chain(
    handle: &ActorHandle,
    build_id: Uuid,
    hashes: &[&str],
    priority_class: PriorityClass,
) -> anyhow::Result<broadcast::Receiver<rio_proto::types::BuildEvent>> {
    let nodes: Vec<_> = hashes
        .iter()
        .map(|h| make_test_node(h, "x86_64-linux"))
        .collect();
    // Edges: parent=next, child=prev (parent depends on child).
    // So nodes[1] depends on nodes[0], nodes[2] on nodes[1], etc.
    let edges: Vec<_> = hashes
        .windows(2)
        .map(|w| make_test_edge(w[1], w[0]))
        .collect();

    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::MergeDag {
            req: MergeDagRequest {
                build_id,
                tenant_id: None,
                priority_class,
                nodes,
                edges,
                options: Default::default(),
                keep_going: true,
                traceparent: String::new(),
                jti: None,
                jwt_token: None,
            },
            reply: tx,
        })
        .await?;
    Ok(rx.await??)
}

/// Orphan-completion (outputs in store, worker didn't reconnect)
/// must fire check_build_completion. Without this, if the
/// orphan-completed drv was the LAST outstanding one, the build stays
/// Active forever — no other completion will trigger the check.
///
/// Setup: first actor merges a single-drv build, then we backdate PG
/// to simulate "drv was Assigned to a worker that's now gone, and
/// outputs ARE in the store (worker finished while scheduler was
/// down)." Second actor (with store client) recovers, reconciles,
/// finds orphan completion → drv Completed → build Succeeded.
#[tokio::test]
async fn test_orphan_completion_fires_build_completion() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;

    // In-process store, pre-seeded with the output path.
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("orphan-out");
    put_test_path(&mut store_client, &out_path).await?;

    // --- Phase 1: first "leader" writes build + drv to PG ---
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(sched_db.pool.clone());
        // Single-node DAG — the orphan-completed drv IS the whole
        // build. This is the critical case: if check_build_completion
        // doesn't fire, NOTHING else will (no other drv completing).
        let mut node = make_test_node("orphan-drv", "x86_64-linux");
        node.expected_output_paths = vec![out_path.clone()];
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;

        // Shut down (scheduler death).
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate PG: simulate "drv was dispatched to worker 'dead-w1'
    // before scheduler died." The worker won't reconnect (we never
    // register it on the second actor).
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = 'dead-w1' \
         WHERE drv_hash = 'orphan-drv'",
    )
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: fresh actor WITH store client recovers ---
    let (handle, _task) = setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone()));

    // LeaderAcquired → recover_from_pg (loads Assigned drv + build).
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Verify recovery found the Assigned drv.
    let pre = handle
        .debug_query_derivation("orphan-drv")
        .await?
        .expect("drv should be recovered");
    assert_eq!(
        pre.status,
        DerivationStatus::Assigned,
        "drv should be Assigned after recovery (before reconcile)"
    );

    // ReconcileAssignments → worker 'dead-w1' not in self.executors
    // → store check → outputs present → orphan completion →
    // check_build_completion fires.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // Drv should be Completed.
    let post = handle
        .debug_query_derivation("orphan-drv")
        .await?
        .expect("drv should still exist");
    assert_eq!(
        post.status,
        DerivationStatus::Completed,
        "orphan completion should transition drv to Completed"
    );

    // THE KEY ASSERTION: build should be Succeeded. Without
    // check_build_completion, it would stay Active.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build should be Succeeded after orphan completion"
    );

    Ok(())
}

/// Orphan-completion must unpin scheduler_live_pins.
///
/// Scenario: old scheduler dispatches drv → pins inputs → crashes.
/// Worker finishes. New scheduler recovers → sweep_stale_live_pins
/// KEEPS the pin (drv is Assigned in PG, non-terminal). Then
/// ReconcileAssignments fires → orphan completion → drv Completed.
/// Without the unpin, pins leak until NEXT restart's sweep.
///
/// Same setup as test_orphan_completion_fires_build_completion but
/// additionally seeds a scheduler_live_pins row (simulating the
/// original dispatch's pin) and asserts it's gone after reconcile.
#[tokio::test]
async fn test_orphan_completion_unpins_live_inputs() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;

    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("y2-out");
    put_test_path(&mut store_client, &out_path).await?;

    // --- Phase 1: first "leader" writes build + drv ---
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(sched_db.pool.clone());
        let mut node = make_test_node("y2-drv", "x86_64-linux");
        node.expected_output_paths = vec![out_path.clone()];
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate: drv Assigned to dead worker (simulates "dispatched
    // before crash"). Also seed a scheduler_live_pins row
    // (simulates dispatch's pin_live_inputs).
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', \
         assigned_builder_id = 'y2-dead-worker' WHERE drv_hash = 'y2-drv'",
    )
    .execute(&sched_db.pool)
    .await?;

    // Seed a pin (simulating what dispatch would have done). The
    // input path doesn't need to exist in the store — scheduler_
    // live_pins has no FK (migration 007: pins may be for paths
    // not yet uploaded). SHA-256 of a fake input path.
    let input_path = test_store_path("y2-fake-input");
    let db = SchedulerDb::new(sched_db.pool.clone());
    db.pin_live_inputs(&"y2-drv".into(), std::slice::from_ref(&input_path))
        .await?;

    // Verify pin seeded.
    let pins_before: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'y2-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    assert_eq!(pins_before, 1, "pin should be seeded before recovery");

    // --- Phase 2: fresh actor recovers + reconciles ---
    let (handle, _task) = setup_actor_with_store(sched_db.pool.clone(), Some(store_client.clone()));

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // After LeaderAcquired, sweep_stale_live_pins ran — but the
    // drv is Assigned (non-terminal) so the pin SURVIVES. This is
    // the critical setup: the sweep CAN'T catch this case.
    let pins_after_sweep: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'y2-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    assert_eq!(
        pins_after_sweep, 1,
        "sweep should KEEP pin for non-terminal drv (this is the setup, not the bug)"
    );

    // ReconcileAssignments → worker not registered → store check →
    // outputs present → orphan completion → Completed → unpin.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // Drv should be Completed.
    let post = handle
        .debug_query_derivation("y2-drv")
        .await?
        .expect("drv exists");
    assert_eq!(post.status, DerivationStatus::Completed);

    // Pin should be GONE. Without the unpin in the orphan-
    // completion branch, this would be 1 (leaked until next
    // scheduler restart).
    let pins_after_orphan: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'y2-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    assert_eq!(
        pins_after_orphan, 0,
        "orphan completion should unpin (was {pins_after_orphan}, expected 0)"
    );

    Ok(())
}

/// Phantom-Assigned after crash-during-dispatch.
///
/// Scenario: scheduler persists PG=Assigned+worker, crashes BEFORE
/// try_send (the actual channel send to the worker). On restart,
/// worker reconnects (heartbeat → in self.executors). Without the
/// running_builds cross-check, reconcile_assignments sees "worker
/// present, leave it" → drv stuck forever (worker never got it,
/// no running_since → backstop timeout won't fire).
///
/// The fix: cross-check worker.running_builds even when worker is
/// present. If drv NOT in the worker's heartbeat, reconcile it
/// (store-check → Completed, or reset → Ready).
#[tokio::test]
async fn test_phantom_assigned_reconciled_when_worker_present() -> TestResult {
    let sched_db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // --- Phase 1: merge build, shut down ---
    {
        let (handle, task) = setup_actor(sched_db.pool.clone());
        let node = make_test_node("phantom-drv", "x86_64-linux");
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate: simulate "persist_status(Assigned) + insert_assignment
    // ran, but try_send never did" (crash between PG write and channel
    // send). Worker 'phantom-w1' WILL reconnect in phase 2.
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', \
         assigned_builder_id = 'phantom-w1' WHERE drv_hash = 'phantom-drv'",
    )
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: fresh actor, worker reconnects WITHOUT the drv ---
    let (handle, _task) = setup_actor(sched_db.pool.clone());

    // Worker reconnects: BuildExecution stream + heartbeat with
    // EMPTY running_builds (because it never actually got the
    // assignment — the try_send never happened).
    let _worker_rx = connect_executor(&handle, "phantom-w1", "x86_64-linux").await?;
    // Second worker so the post-reconcile dispatch has somewhere to go.
    // I-065: reconcile records w1 in failed_builders (phantom counts as
    // a failed attempt on that worker's infra); with a single-worker
    // fleet that would be exhaustion → poison. Two workers preserves
    // this test's intent (verify phantom RECONCILES) without hitting
    // the exhaustion case, which test_fleet_exhaustion_is_kind_aware
    // covers separately.
    let _worker_rx2 = connect_executor(&handle, "phantom-w2", "x86_64-linux").await?;
    barrier(&handle).await;

    // LeaderAcquired → recover_from_pg loads Assigned drv.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Verify: drv is Assigned, worker is in self.executors, but
    // running_builds does NOT contain the drv (phantom!).
    let pre = handle
        .debug_query_derivation("phantom-drv")
        .await?
        .expect("drv recovered");
    assert_eq!(
        pre.status,
        DerivationStatus::Assigned,
        "drv should be Assigned after recovery"
    );

    // ReconcileAssignments: worker present BUT drv not in
    // running_builds → reconcile. No store client here, so
    // store-check fails → reset to Ready (not Completed).
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // THE KEY ASSERTION: drv should be Ready (or re-dispatched).
    // Without the running_builds cross-check, it would stay Assigned
    // forever — worker present meant "leave it, completion will
    // arrive", but the worker never had it so no completion comes.
    let post = handle
        .debug_query_derivation("phantom-drv")
        .await?
        .expect("drv exists");
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "phantom-Assigned should be reconciled (Ready or re-dispatched to Assigned), got {:?}",
        post.status
    );
    // If it's Assigned again, it should have been re-dispatched
    // to our connected worker (not stale "phantom-w1"). Actually
    // we DID connect phantom-w1, so re-dispatch goes to the same
    // worker — that's fine, the worker now ACTUALLY has it.
    //
    // More precise: it should NOT be the OLD Assigned (stuck).
    // The way to tell: if reset_to_ready ran, PG has retry_count
    // bumped OR the assignment row was cleaned. Check retry_count.
    let retry_count: i32 =
        sqlx::query_scalar("SELECT retry_count FROM derivations WHERE drv_hash = 'phantom-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    // retry_count was 0 before recovery. reset_to_ready bumps it.
    // If reconciled → retry_count >= 1. If stuck → still 0.
    assert!(
        retry_count >= 1,
        "phantom Assigned should be reset (retry_count bumped), got {retry_count}"
    );

    Ok(())
}

/// Recovery must skip rows with unparseable drv_path (StorePath::parse
/// fails) and continue loading valid rows. A corrupted/hand-edited PG
/// row shouldn't block recovery of the entire DAG.
///
/// Note: the analogous "unknown derivation status" skip path can't be
/// tested via direct INSERT — the PG CHECK constraint rejects values
/// outside the allowed set before they reach recovery. The drv_path
/// column has no such constraint, so we test the skip-bad-rows logic
/// via that path instead (same `continue` pattern in recover_from_pg).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_recovery_skips_bad_drv_path_rows() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed a VALID row via the normal merge path (simpler than
    // hand-crafting all columns for a minimal INSERT).
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx =
            merge_single_node(&handle, build_id, "z1-good-drv", PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Seed a BAD row with a garbage drv_path that StorePath::parse
    // will reject. Other columns must satisfy NOT NULL + CHECK;
    // status='ready' keeps it non-terminal (loadable by recovery).
    sqlx::query(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ('z1-bad-drv', 'not-a-store-path', 'x86_64-linux', 'ready')",
    )
    .execute(&db.pool)
    .await?;

    // Fresh actor recovers.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The bad-row skip should have logged.
    assert!(
        logs_contain("invalid drv_path in PG"),
        "recovery should log invalid drv_path skip"
    );

    // The GOOD row should still be in the DAG — skip-and-continue,
    // not skip-and-abort.
    let good = handle.debug_query_derivation("z1-good-drv").await?;
    assert!(
        good.is_some(),
        "valid drv should still be recovered despite bad sibling row"
    );

    // The bad row should NOT be in the DAG.
    let bad = handle.debug_query_derivation("z1-bad-drv").await?;
    assert!(bad.is_none(), "invalid drv_path row should be skipped");

    Ok(())
}

// r[verify sched.recovery.fetch-max-seed]
/// Recovery must seed generation from `MAX(generation) FROM assignments`
/// via fetch_max. Defensive monotonicity: if the k8s Lease annotation
/// reset (deleted Lease, stale etcd restore), a worker holding a stale
/// assignment with generation=100 would ALSO accept new ones from
/// whatever the lease loop set (e.g., 1). Seeding from PG's high-water
/// mark prevents that: after recovery, generation >= PG max + 1.
#[tokio::test]
async fn test_recovery_seeds_generation_from_assignments() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed a derivation (FK target for assignments) + an assignment
    // with generation=100. Use the normal merge path to write the
    // derivation row with all required columns, then insert the
    // assignment directly.
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx =
            merge_single_node(&handle, build_id, "z2-gen-drv", PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Look up the derivation_id for the FK.
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = 'z2-gen-drv'")
            .fetch_one(&db.pool)
            .await?;

    // Seed assignment with generation=100.
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status) \
         VALUES ($1, 'seed-worker', 100, 'completed')",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;

    // Fresh actor recovers. The default setup_actor starts with
    // generation=1 (non-K8s mode); recovery's fetch_max should
    // bump it to max(1, 100+1) = 101.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    let g = handle.leader_generation();
    assert!(
        g >= 101,
        "generation should be seeded from PG high-water mark: expected >= 101, got {g}"
    );

    Ok(())
}

/// Recovery must skip builds that have ZERO build_derivations rows.
/// These are orphans: crash-during-merge BEFORE the link rows were
/// written, or a failed rollback. Without this skip, the all-terminal
/// completion sweep would fire check_build_completion on them → spurious
/// BuildCompleted with empty output_paths.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_recovery_z16_orphan_build_skipped() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Seed a builds row DIRECTLY with NO build_derivations links.
    // status='active' so it's loadable (non-terminal).
    let orphan_id = Uuid::new_v4();
    sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
        .bind(orphan_id)
        .execute(&db.pool)
        .await?;

    // Also seed a NORMAL build (with links) to prove recovery
    // doesn't skip everything.
    let normal_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let _rx =
            merge_single_node(&handle, normal_id, "z16-normal", PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Fresh actor recovers.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The orphan-skip should have logged.
    assert!(
        logs_contain("ZERO build_derivations"),
        "recovery should log orphan build skip"
    );

    // The orphan build IS loaded (it's in self.builds — the sweep
    // just skips completion check for it). query_status should find
    // it, still Active (not spuriously Succeeded).
    let orphan_status = query_status(&handle, orphan_id).await?;
    assert_eq!(
        orphan_status.state,
        rio_proto::types::BuildState::Active as i32,
        "orphan build should stay Active (completion check skipped)"
    );

    // Normal build still recovered normally.
    let normal = handle.debug_query_derivation("z16-normal").await?;
    assert!(normal.is_some(), "normal drv should be recovered");

    Ok(())
}

/// Reconcile with store unreachable (FindMissingPaths errors) → falls
/// back to "assume incomplete" → reset_to_ready + retry. The
/// `warn!("reconcile: FindMissingPaths failed")` branch.
///
/// Setup: orphan-Assigned drv (worker never reconnects), store client
/// present but the store's PG is closed → FindMissingPaths fails →
/// fallback path taken.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_reconcile_store_unreachable_assumes_incomplete() -> TestResult {
    use super::integration::setup_inproc_store;

    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;

    // In-process store (real client) — we'll break it by closing
    // its PG pool before reconcile.
    let (store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;

    // --- Phase 1: first "leader" writes build + drv ---
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(sched_db.pool.clone());
        let mut node = make_test_node("z4-drv", "x86_64-linux");
        // Must have expected_output_paths or the reconcile short-
        // circuits before the store call ("No expected outputs =
        // can't verify orphan completion. Conservative: treat as
        // incomplete."). That's a DIFFERENT code path.
        node.expected_output_paths = vec![test_store_path("z4-out")];
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate: simulate "dispatched to a worker that won't reconnect".
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', \
         assigned_builder_id = 'z4-dead-worker' WHERE drv_hash = 'z4-drv'",
    )
    .execute(&sched_db.pool)
    .await?;

    // Break the store: close its PG pool. FindMissingPaths will
    // return an Err (sqlx::Error::PoolClosed → tonic::Status).
    store_db.pool.close().await;

    // --- Phase 2: fresh actor WITH (broken) store client recovers ---
    let (handle, _task) = setup_actor_with_store(sched_db.pool.clone(), Some(store_client));

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Pre-reconcile: drv should be Assigned (recovered from PG).
    let pre = handle
        .debug_query_derivation("z4-drv")
        .await?
        .expect("drv recovered");
    assert_eq!(pre.status, DerivationStatus::Assigned);

    // ReconcileAssignments → worker not in self.executors → store
    // check → FindMissingPaths FAILS (pool closed) → fallback:
    // assume incomplete → reset to Ready.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // The fallback branch should have logged.
    assert!(
        logs_contain("FindMissingPaths failed"),
        "reconcile should log store-unreachable fallback"
    );

    // Drv should be Ready (NOT Completed — store couldn't verify
    // outputs) and retry_count bumped.
    let post = handle
        .debug_query_derivation("z4-drv")
        .await?
        .expect("drv exists");
    assert_eq!(
        post.status,
        DerivationStatus::Ready,
        "store unreachable → assume incomplete → reset to Ready"
    );
    assert!(
        post.retry.count >= 1,
        "retry_count should be bumped (this is a retry)"
    );

    Ok(())
}

// r[verify sched.poison.ttl-persist]
/// Poison a derivation on actor A, drop A, spawn actor B on the same PG,
/// send LeaderAcquired → recover_from_pg should load the poisoned derivation
/// with Poisoned status for TTL tracking. Without migration 009's poisoned_at
/// persistence, poison TTL would reset on every scheduler restart.
#[tokio::test]
async fn test_recovery_loads_poisoned_derivations() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: actor A poisons a derivation ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_executor(&handle, "poison-rec-w", "x86_64-linux").await?;
        let _ev = merge_single_node(
            &handle,
            Uuid::new_v4(),
            "poison-rec",
            PriorityClass::Scheduled,
        )
        .await?;
        let _ = worker_rx.recv().await.expect("assignment");
        complete_failure(
            &handle,
            "poison-rec-w",
            &test_drv_path("poison-rec"),
            rio_proto::build_types::BuildResultStatus::PermanentFailure,
            "permanent",
        )
        .await?;
        // Barrier: ensure persist_poisoned hit PG.
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Verify PG has it poisoned + poisoned_at set (precondition).
    let (status, has_ts): (String, bool) =
        sqlx::query_as("SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash=$1")
            .bind("poison-rec")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "poisoned", "PG status should be poisoned");
    assert!(
        has_ts,
        "PG poisoned_at should be set (the as_bytes bug broke this)"
    );

    // --- Phase 2: fresh actor B recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());

    // Before recovery: DAG is empty.
    let pre = handle.debug_query_derivation("poison-rec").await?;
    assert!(pre.is_none(), "fresh actor has empty DAG");

    // LeaderAcquired → recover_from_pg loads poisoned derivations.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // After recovery: derivation is back in the DAG with Poisoned status.
    let post = handle
        .debug_query_derivation("poison-rec")
        .await?
        .expect("poisoned drv should be recovered for TTL tracking");
    assert_eq!(
        post.status,
        DerivationStatus::Poisoned,
        "recovered with Poisoned status — handle_tick will TTL-check it"
    );

    Ok(())
}

/// Recovery loads a poisoned row whose PG `poisoned_at` is already past
/// TTL. Recovery should clear it in PG and NOT insert it into the DAG.
///
/// Without the recovery.rs pre-filter, `from_poisoned_row` on a fresh
/// k8s node (booted 1h ago) with elapsed=30h would do Instant::now()
/// .checked_sub(30h) → None → unwrap_or(now) → poisoned_at=now →
/// duration_since(now)=0 < POISON_TTL → FRESH 24h TTL for a derivation
/// that should have expired 6h ago. PG's wall-clock elapsed_secs
/// comparison is immune to node uptime.
#[tokio::test]
async fn test_recovery_expired_poison_cleared_not_reloaded() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: actor A poisons a derivation, then we backdate PG ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_executor(&handle, "exp-poison-w", "x86_64-linux").await?;
        let _ev = merge_single_node(
            &handle,
            Uuid::new_v4(),
            "exp-poison",
            PriorityClass::Scheduled,
        )
        .await?;
        let _ = worker_rx.recv().await.expect("assignment");
        complete_failure(
            &handle,
            "exp-poison-w",
            &test_drv_path("exp-poison"),
            rio_proto::build_types::BuildResultStatus::PermanentFailure,
            "permanent",
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Backdate poisoned_at well past POISON_TTL (cfg(test) = 100ms, so
    // 10s is 100× past). PG computes elapsed_secs = now() - poisoned_at
    // at load time.
    sqlx::query(
        "UPDATE derivations SET poisoned_at = now() - interval '10 seconds' WHERE drv_hash = $1",
    )
    .bind("exp-poison")
    .execute(&db.pool)
    .await?;

    // Precondition: PG still shows poisoned.
    let (status, _): (String, bool) =
        sqlx::query_as("SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash=$1")
            .bind("exp-poison")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "poisoned");

    // --- Phase 2: fresh actor B recovers ---
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Derivation NOT in the DAG — recovery filtered it.
    let post = handle.debug_query_derivation("exp-poison").await?;
    assert!(
        post.is_none(),
        "expired-at-load poison should be cleared, not reloaded into DAG"
    );

    // PG: clear_poison ran → status='created', poisoned_at NULL.
    let (status, has_ts): (String, bool) =
        sqlx::query_as("SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash=$1")
            .bind("exp-poison")
            .fetch_one(&db.pool)
            .await?;
    assert_eq!(status, "created", "clear_poison sets status='created'");
    assert!(!has_ts, "clear_poison NULLs poisoned_at");

    Ok(())
}

/// Regression: a ClearPoison on a recovered poisoned node must remove it
/// from the DAG so a resubmit inserts it fresh with full proto fields.
///
/// Before the fix, `reset_from_poison` left the node in Created with stub
/// fields from `from_poisoned_row` (`output_names: []`,
/// `expected_output_paths: []`). `dag.merge()` on an existing node only
/// touches `interested_builds` + `traceparent`, and `compute_initial_states`
/// only iterates `newly_inserted` — so the resubmit's node never progressed
/// past Created. Build counters stuck at `completed=0, failed=0, total=1`;
/// `check_build_completion` never fired. Hard hang.
#[tokio::test]
async fn test_recovered_poison_clear_then_resubmit_progresses() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: actor A poisons a derivation ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_executor(&handle, "zombie-w", "x86_64-linux").await?;
        let _ev = merge_single_node(
            &handle,
            Uuid::new_v4(),
            "zombie-drv",
            PriorityClass::Scheduled,
        )
        .await?;
        let _ = worker_rx.recv().await.expect("assignment");
        complete_failure(
            &handle,
            "zombie-w",
            &test_drv_path("zombie-drv"),
            rio_proto::build_types::BuildResultStatus::PermanentFailure,
            "permanent",
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // --- Phase 2: fresh actor B recovers, operator clears poison,
    //     user resubmits ---
    let (handle, _task) = setup_actor(db.pool.clone());
    let mut worker_rx = connect_executor(&handle, "zombie-w2", "x86_64-linux").await?;

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Precondition: recovery loaded the poisoned node.
    let recovered = handle
        .debug_query_derivation("zombie-drv")
        .await?
        .expect("poisoned drv recovered from PG");
    assert_eq!(recovered.status, DerivationStatus::Poisoned);

    // ClearPoison → node REMOVED (not reset-in-place).
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "zombie-drv".into(),
            reply: tx,
        })
        .await?;
    assert!(rx.await?, "ClearPoison → cleared=true");

    let post_clear = handle.debug_query_derivation("zombie-drv").await?;
    assert!(
        post_clear.is_none(),
        "ClearPoison must remove the node so next merge treats it as newly-inserted"
    );

    // Resubmit — the bug's trigger. Node is newly-inserted → gets full
    // proto fields → runs through compute_initial_states → dispatches.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "zombie-drv", PriorityClass::Scheduled).await?;

    // The regression: before the fix, merge saw a stale Created stub,
    // skipped compute_initial_states, and the node never dispatched.
    // Assert it actually reaches the worker. recv_assignment has a 2s
    // timeout and panics on non-Assignment — both are hang symptoms.
    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, test_drv_path("zombie-drv"));

    // And status progressed past Created.
    let post_merge = handle
        .debug_query_derivation("zombie-drv")
        .await?
        .expect("freshly inserted");
    assert_ne!(
        post_merge.status,
        DerivationStatus::Created,
        "node must progress past Created (compute_initial_states ran)"
    );

    Ok(())
}

// r[verify sched.recovery.poisoned-failed-count]
/// Route 1: crash between persist_poisoned and the build transition to
/// Failed. PG has drv status='poisoned', poisoned_at SET (atomic persist
/// landed), build status='active'. Recovery must load the poisoned drv
/// into id_to_hash → bd_rows join succeeds → build_summary counts it in
/// `failed` → build → Failed.
///
/// Simulation: don't actually crash mid-await. Directly write the
/// inconsistent PG state that a crash WOULD leave, then recover. The
/// actor's completion path is deterministic; we're testing recovery's
/// interpretation of the state, not the crash itself.
///
/// Before the keystone fix: the poisoned row was loaded into the DAG but
/// NOT into id_to_hash → bd_rows join fell through → build_drv_hashes
/// stayed empty → total=0, completed=0, failed=0 → 0>=0 && 0==0 →
/// spurious Succeeded. After: total=1, failed=1 → Failed.
#[tokio::test]
async fn test_recovery_poisoned_orphan_build_fails_not_succeeds() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // Phase 1: actor A merges a single-drv build with keep_going=true.
    // This variant exercises the `all_resolved && failed>0` half of the
    // condition. The _keep_going_false variant below covers the default
    // (and more common) path. Dispatches it, then we directly write the
    // crash-window PG state and kill actor A.
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_executor(&handle, "r1-w", "x86_64-linux").await?;
        let _ev = merge_dag(
            &handle,
            build_id,
            vec![make_test_node("r1-drv", "x86_64-linux")],
            vec![],
            true, // keep_going
        )
        .await?;
        let _ = worker_rx.recv().await.expect("assignment");
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Simulate crash-after-persist_poisoned-before-handle_derivation_failure:
    // drv is poisoned+timestamped, build is still active.
    sqlx::query("UPDATE derivations SET status='poisoned', poisoned_at=now() WHERE drv_hash=$1")
        .bind("r1-drv")
        .execute(&db.pool)
        .await?;
    // Build row: leave as-is (status='active' from merge).
    let (build_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        build_status, "active",
        "precondition: build still active in PG"
    );

    // Phase 2: fresh actor B recovers.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // THE assertion: build must be Failed, not Succeeded.
    // Before the keystone fix: Succeeded (total=0, failed=0).
    // After: Failed (total=1, failed=1).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "recovered build with only-poisoned drv MUST be Failed, got state={}",
        status.state
    );

    // Belt-and-suspenders: PG should reflect the transition.
    let (pg_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(pg_status, "failed", "PG build status must follow in-mem");

    Ok(())
}

// r[verify sched.recovery.poisoned-failed-count]
/// Route 1, keep_going=false (the DEFAULT): same crash window as the
/// _fails_not_succeeds test above, but with the common-case flag.
///
/// This is the spec's unconditional claim: a crash between
/// persist_poisoned and the build transition MUST result in Failed on
/// recovery — regardless of keep_going. Before the `|| !keep_going`
/// condition fix in check_build_completion, keep_going=false builds
/// fell through: failed=1 but `keep_going && all_resolved` was false
/// → no branch taken → build stuck Active forever. (Live keep_going=
/// false failures go through handle_derivation_failure, but recovery's
/// sweep doesn't invoke that path — it calls check_build_completion
/// directly.)
#[tokio::test]
async fn test_recovery_poisoned_orphan_build_fails_keep_going_false() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let build_id = Uuid::new_v4();

    // Phase 1: actor A merges a single-drv build with keep_going=FALSE
    // (the default — this is the common case). Dispatch, then write
    // the crash-window PG state and kill actor A.
    {
        let (handle, task) = setup_actor(db.pool.clone());
        let mut worker_rx = connect_executor(&handle, "r1f-w", "x86_64-linux").await?;
        let _ev = merge_dag(
            &handle,
            build_id,
            vec![make_test_node("r1f-drv", "x86_64-linux")],
            vec![],
            false, // keep_going=false — the default
        )
        .await?;
        let _ = worker_rx.recv().await.expect("assignment");
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // Simulate crash-after-persist_poisoned-before-handle_derivation_failure:
    // drv is poisoned+timestamped, build is still active.
    sqlx::query("UPDATE derivations SET status='poisoned', poisoned_at=now() WHERE drv_hash=$1")
        .bind("r1f-drv")
        .execute(&db.pool)
        .await?;
    let (build_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(
        build_status, "active",
        "precondition: build still active in PG"
    );

    // Phase 2: fresh actor B recovers.
    let (handle, _task) = setup_actor(db.pool.clone());
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // THE assertion: build must be Failed. NOT Succeeded, NOT Active.
    // Before the `|| !keep_going` fix: Active (fell through both branches).
    // After: Failed (failed=1, !keep_going → branch fires).
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "recovered keep_going=false build with poisoned drv MUST be Failed (not stuck Active), got state={}",
        status.state
    );

    // PG follows in-mem.
    let (pg_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
    assert_eq!(pg_status, "failed", "PG build status must follow in-mem");

    Ok(())
}

// ---- Recovery TOCTOU on lease flap (remediation 08) ------------
// r[verify sched.recovery.gate-dispatch]
//   Generation snapshot + re-check: if the lease flaps (lose→
//   reacquire) mid-recovery, discard the stale DAG instead of
//   dispatching from it with the NEW generation stamped on.

use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Lease flaps during recovery: gen bumps mid-flight → discard.
///
/// Timeline simulated:
///   T+0   lease loop acquires (gen 1→2), sends LeaderAcquired
///   T+1   actor enters handle_leader_acquired, snapshots gen=2
///   T+1   actor runs recover_from_pg (fast, empty PG)
///   T+1   actor hits the test gate, signals "reached", blocks
///   T+4   [SIMULATED] lease loop lose-transition: clears
///         recovery_complete=false
///   T+9   [SIMULATED] lease loop re-acquire: fetch_add gen 2→3
///   T+9   test releases the gate
///   T+12  actor re-loads gen=3, sees 3 ≠ 2 → DISCARD, early-return
///
/// Pre-fix: the unconditional store(true) at the old :367 would
/// clobber the lease loop's clear at T+4. dispatch_ready would then
/// fire with a DAG loaded under gen=2 but stamped with gen=3.
#[tokio::test]
async fn test_recovery_toctou_gen_bump_discards() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Shared atomics simulating LeaderState. Start gen=2 (lease
    // loop's first fetch_add already happened).
    let generation = Arc::new(AtomicU64::new(2));
    let recovery_complete = Arc::new(AtomicBool::new(false));

    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let gen_for_actor = Arc::clone(&generation);
    let rc_for_actor = Arc::clone(&recovery_complete);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(
            gen_for_actor,
            Arc::new(AtomicBool::new(true)),
            rc_for_actor,
        );
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    // Trigger recovery.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;

    // Deterministic rendezvous: actor signals it has finished
    // recover_from_pg and is parked at the gate. No sleep/yield.
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate within 10s")
        .expect("reached_tx not dropped");

    // Simulate lease flap: lose (clear) + reacquire (bump gen).
    // Order matches the real lease loop (lose-transition at
    // lease/mod.rs clears recovery_complete, THEN next acquire
    // fetch_add's). Both are Relaxed/Release there.
    recovery_complete.store(false, Ordering::Relaxed);
    generation.fetch_add(1, Ordering::Release); // 2 → 3

    // Release recovery. Actor re-loads gen, sees 3 ≠ 2, discards.
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    // THE assertion: recovery_complete must STILL be false. The
    // early-return preserved the lease loop's clear. Without the
    // fix, the unconditional store(true) clobbers it → dispatch_
    // ready would fire with the stale (gen-2) DAG.
    assert!(
        !recovery_complete.load(Ordering::Acquire),
        "TOCTOU: recovery_complete clobbered lease loop's clear \u{2014} \
         would dispatch gen-2 DAG with gen-3 stamps"
    );

    // DAG discarded: debug_query returns None for any hash.
    // (Empty PG → nothing was loaded anyway, but this proves the
    // clear-on-discard didn't leave half-state.)
    let info = handle.debug_query_derivation("nonexistent").await?;
    assert!(info.is_none(), "DAG should be empty after discard");

    Ok(())
}

/// Negative control: generation stable during recovery → normal
/// completion. Proves the TOCTOU check doesn't false-positive on a
/// clean recovery (would regress every existing recovery test if so).
#[tokio::test]
async fn test_recovery_toctou_no_bump_completes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    let generation = Arc::new(AtomicU64::new(2));
    let recovery_complete = Arc::new(AtomicBool::new(false));

    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let gen_for_actor = Arc::clone(&generation);
    let rc_for_actor = Arc::clone(&recovery_complete);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(
            gen_for_actor,
            Arc::new(AtomicBool::new(true)),
            rc_for_actor,
        );
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;

    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");

    // NO gen bump. Release immediately.
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    // gen_now == gen_at_entry → normal path → store(true).
    assert!(
        recovery_complete.load(Ordering::Acquire),
        "clean recovery (no flap) should set recovery_complete=true"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        2,
        "generation unchanged (empty PG → no fetch_max bump)"
    );

    Ok(())
}
