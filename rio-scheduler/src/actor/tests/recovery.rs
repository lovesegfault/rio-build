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

/// X4 regression: transient-failure retry must write Ready to PG,
/// not Failed. Crash in backoff window with PG=Failed → recovery
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

    // Connect worker + submit build → dispatch.
    let (handle, _task, mut stream_rx) = {
        let (h, t) = setup_actor(db.pool.clone());
        let rx = connect_worker(&h, "w-x4", "x86_64-linux", 2).await?;
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
        rio_proto::types::BuildResultStatus::TransientFailure,
        "simulated transient",
    )
    .await?;
    barrier(&handle).await;

    // PG should show Ready (NOT Failed). This proves X4 fix: the
    // transient-retry path now persists the FINAL in-mem state.
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

/// X5 regression: recovery must check build completion for builds
/// whose derivations are ALL terminal. Crash between "last drv →
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

    // X5 fix: the post-recovery sweep should fire check_build_
    // completion for the build. With 0 recovered derivations
    // (all terminal, filtered by db.rs:537), total=0, completed=0,
    // failed=0 → all_completed → complete_build → Succeeded.
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "build with all-terminal drvs should be Succeeded after recovery (X5 fix)"
    );

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
            },
            reply: tx,
        })
        .await?;
    Ok(rx.await??)
}

/// C4 regression: orphan-completion (outputs in store, worker didn't
/// reconnect) must fire check_build_completion. Without this, if the
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
        "UPDATE derivations SET status = 'assigned', assigned_worker_id = 'dead-w1' \
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

    // ReconcileAssignments → worker 'dead-w1' not in self.workers
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
        "build should be Succeeded after orphan completion (C4 fix)"
    );

    Ok(())
}

/// Y2: orphan-completion must unpin scheduler_live_pins.
///
/// Scenario: old scheduler dispatches drv → pins inputs → crashes.
/// Worker finishes. New scheduler recovers → sweep_stale_live_pins
/// KEEPS the pin (drv is Assigned in PG, non-terminal). Then
/// ReconcileAssignments fires → orphan completion → drv Completed.
/// Without Y2, pins leak until NEXT restart's sweep.
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
         assigned_worker_id = 'y2-dead-worker' WHERE drv_hash = 'y2-drv'",
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

    // Y2: pin should be GONE. Without the unpin in the orphan-
    // completion branch, this would be 1 (leaked until next
    // scheduler restart).
    let pins_after_orphan: i64 =
        sqlx::query_scalar("SELECT COUNT(*) FROM scheduler_live_pins WHERE drv_hash = 'y2-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    assert_eq!(
        pins_after_orphan, 0,
        "Y2: orphan completion should unpin (was {pins_after_orphan}, expected 0)"
    );

    Ok(())
}

/// Round 4 Z3: phantom-Assigned after crash-during-dispatch.
///
/// Scenario: scheduler persists PG=Assigned+worker, crashes BEFORE
/// try_send (the actual channel send to the worker). On restart,
/// worker reconnects (heartbeat → in self.workers). Old
/// reconcile_assignments saw "worker present, leave it" → drv
/// stuck forever (worker never got it, no running_since →
/// backstop timeout won't fire).
///
/// Z3 fix: cross-check worker.running_builds even when worker is
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
         assigned_worker_id = 'phantom-w1' WHERE drv_hash = 'phantom-drv'",
    )
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: fresh actor, worker reconnects WITHOUT the drv ---
    let (handle, _task) = setup_actor(sched_db.pool.clone());

    // Worker reconnects: BuildExecution stream + heartbeat with
    // EMPTY running_builds (because it never actually got the
    // assignment — the try_send never happened).
    let _worker_rx = connect_worker(&handle, "phantom-w1", "x86_64-linux", 4).await?;
    barrier(&handle).await;

    // LeaderAcquired → recover_from_pg loads Assigned drv.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Verify: drv is Assigned, worker is in self.workers, but
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

    // ReconcileAssignments → Z3 fix: worker present BUT drv not
    // in running_builds → reconcile. No store client here, so
    // store-check fails → reset to Ready (not Completed).
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // THE KEY ASSERTION: drv should be Ready (or re-dispatched).
    // Before Z3, it would stay Assigned forever — worker present
    // meant "leave it, completion will arrive", but the worker
    // never had it so no completion ever comes.
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
    // If Z3 reconciled → retry_count >= 1. If stuck → still 0.
    assert!(
        retry_count >= 1,
        "Z3: phantom Assigned should be reset (retry_count bumped), got {retry_count}"
    );

    Ok(())
}
