//! State recovery: LeaderAcquired → recover_from_pg → DAG rebuilt.
//
// Recovery isn't a standalone spec rule — it's behavior under
// sched.lease.k8s-lease (what happens on acquire). The test here
// verifies the LeaderAcquired → recover_from_pg → recovery_complete
// pipeline; the lease loop's acquire behavior is covered in
// lease.rs tests (sched.lease.generation-fence verify).

use super::*;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};

/// Seed PG with a build + 2-derivation chain (parent depends on child),
/// spawn a FRESH actor (simulating new leader after failover), send
/// LeaderAcquired, assert DAG rebuilt.
///
/// This tests the core recover_from_pg path: load builds, load
/// derivations, load edges, load build_derivations, rebuild DAG +
/// interested_builds + ready queue. RecoveryFixture::run guarantees
/// the phase-2 actor is brand new (empty DAG before LeaderAcquired).
#[tokio::test]
async fn test_recover_from_pg_rebuilds_dag() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, _| {
        merge_chain(
            &handle,
            build_id,
            &["recover-child", "recover-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // Child should be Ready (no dependencies). Parent should be
    // Queued (depends on child). Both should have the build in
    // interested_builds (verified via debug_query — actually that
    // doesn't expose interested_builds, so check via status only).
    let child = expect_drv(&handle, "recover-child").await;
    assert_eq!(
        child.status,
        DerivationStatus::Ready,
        "child (no deps) should be Ready after recovery"
    );

    let parent = expect_drv(&handle, "recover-parent").await;
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
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        // Chain shape gives us a Queued node (parent depends on child,
        // so MergeDag leaves parent at Queued in PG). A single node
        // would be Ready in PG and never hit the I-058 collection.
        merge_chain(
            &handle,
            build_id,
            &["orphan-child", "orphan-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Precondition: parent is queued in PG. If MergeDag's own
        // compute_initial_states changed and the parent is now Ready,
        // this test stops exercising the I-058 path and silently
        // passes for the wrong reason.
        let (pg_status,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'orphan-parent'")
                .fetch_one(&pool)
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
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // Orphan parent loaded (non-terminal in PG → load_nonterminal_
    // derivations found it) but NOT transitioned. Without the gate,
    // the I-058 pass sees zero recovered edges (child→parent edge
    // was loaded — both endpoints non-terminal — but child has no
    // deps so child looks Ready → all_deps_completed for parent →
    // Ready). With the gate, parent's interested_builds is empty →
    // filtered out of to_recompute → stays at PG status.
    let parent = expect_drv(&handle, "orphan-parent").await;
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
    let child = expect_drv(&handle, "orphan-child").await;
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
// r[verify sched.recovery.gate-dispatch]
#[tokio::test]
async fn test_recovery_failure_degrades_to_empty_dag() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Inject an observable recovery_complete via from_parts (same
    // pattern as test_recovery_toctou_on_lease_flap below).
    let recovery_complete = Arc::new(AtomicBool::new(false));
    let rc = Arc::clone(&recovery_complete);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(
            Arc::new(AtomicU64::new(1)),
            Arc::new(AtomicBool::new(true)),
            rc,
        );
    });
    // Close the pool BEFORE sending LeaderAcquired — all PG queries
    // will fail. This simulates PG going down mid-recovery.
    db.pool.close().await;

    // LeaderAcquired → recover_from_pg → PG fails → Err arm → set
    // recovery_complete=true with EMPTY DAG.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        recovery_complete.load(Ordering::Acquire),
        "Err arm must set recovery_complete=true (degrade, don't block dispatch)"
    );
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

    // Connect worker + submit build → dispatch. Padding worker is
    // statically-eligible (same system) so the fleet-exhaustion clamp
    // doesn't poison after a single failure (1-worker fleet would);
    // store_degraded keeps it ineligible for dispatch so w-x4 is
    // deterministically picked and the post-failure Ready isn't
    // immediately re-dispatched. NOT `running_build=Some("busy")`:
    // heartbeat reconcile resolves the path against the DAG and
    // "busy" → None → pad becomes idle (HashMap-order-dependent flake).
    let (handle, _task, mut stream_rx) = {
        let (h, t) = setup_actor(db.pool.clone());
        let rx = connect_executor(&h, "w-x4", "x86_64-linux").await?;
        let _pad = connect_executor_with(&h, "w-x4-pad", "x86_64-linux", true, |hb| {
            hb.store_degraded = true;
        })
        .await?;
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
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        merge_single_node(&handle, build_id, "x5-drv", PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate: drv → completed, build stays active (crash-after-last-drv-complete).
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'x5-drv'")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;

    // The post-recovery sweep fires check_build_completion → Succeeded.
    let status = query_status(&f.handle, build_id).await?;
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
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        merge_chain(&handle, build_id, &["i111-a", "i111-b", "i111-c"], PriorityClass::Scheduled)
            .await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate: 2 of 3 drvs completed (terminal — NOT loaded into DAG
        // at recovery). Denorm columns say 100/50/12 — deliberately
        // distinct from what the DAG would compute.
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash IN ('i111-a', 'i111-b')")
            .execute(&pool)
            .await?;
        sqlx::query("UPDATE builds SET total_drvs = 100, completed_drvs = 50, cached_drvs = 12 WHERE build_id = $1")
            .bind(build_id)
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let (db, handle) = (f.db, f.handle);

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
    let nodes: Vec<_> = hashes.iter().map(|h| make_node(h)).collect();
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

/// Phase-1 + backdate-to-Assigned for the orphan-reconcile tests:
/// spawn an actor on `pool`, merge single `drv_hash` (with `out_path`
/// as expected output), drop the actor, then set PG `status='assigned'`
/// with `assigned_builder_id=dead_worker`. Returns the build_id.
async fn seed_orphan_assigned(
    pool: &sqlx::PgPool,
    drv_hash: &str,
    out_path: &str,
    dead_worker: &str,
) -> anyhow::Result<Uuid> {
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(pool.clone());
        let mut node = make_node(drv_hash);
        node.expected_output_paths = vec![out_path.into()];
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = $1 WHERE drv_hash = $2",
    )
    .bind(dead_worker)
    .bind(drv_hash)
    .execute(pool)
    .await?;
    Ok(build_id)
}

/// Phase-2 for orphan-reconcile tests: spawn actor on `pool` with
/// `store` wired, send LeaderAcquired, barrier. Mirrors the tail of
/// `RecoveryFixture::run_with_store` for tests that need an inproc
/// store with its own TestDb (so can't use the fixture's single-db).
async fn recover_with_store(
    pool: sqlx::PgPool,
    store: StoreServiceClient<Channel>,
) -> anyhow::Result<(ActorHandle, tokio::task::JoinHandle<()>)> {
    let (handle, task) = setup_actor_with_store(pool, Some(store));
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    Ok((handle, task))
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
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("orphan-out");
    put_test_path(&mut store_client, &out_path).await?;

    // Single-node DAG — the orphan-completed drv IS the whole build.
    // Critical case: if check_build_completion doesn't fire, NOTHING
    // else will (no other drv completing). Backdated to Assigned by a
    // worker that won't reconnect.
    let build_id = seed_orphan_assigned(&sched_db.pool, "orphan-drv", &out_path, "dead-w1").await?;
    let (handle, _task) = recover_with_store(sched_db.pool.clone(), store_client.clone()).await?;

    // Verify recovery found the Assigned drv.
    let pre = expect_drv(&handle, "orphan-drv").await;
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
    let post = expect_drv(&handle, "orphan-drv").await;
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

    let _build_id =
        seed_orphan_assigned(&sched_db.pool, "y2-drv", &out_path, "y2-dead-worker").await?;

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

    let (handle, _task) = recover_with_store(sched_db.pool.clone(), store_client.clone()).await?;

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
    let post = expect_drv(&handle, "y2-drv").await;
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
/// running_build cross-check, reconcile_assignments sees "worker
/// present, leave it" → drv stuck forever (worker never got it,
/// no running_since → backstop timeout won't fire).
///
/// The fix: cross-check worker.running_build even when worker is
/// present. If drv NOT in the worker's heartbeat, reconcile it
/// (store-check → Completed, or reset → Ready).
#[tokio::test]
async fn test_phantom_assigned_reconciled_when_worker_present() -> TestResult {
    // Backdate: simulate "persist_status(Assigned) + insert_assignment
    // ran, but try_send never did" (crash between PG write and channel
    // send). Worker 'phantom-w1' WILL reconnect in phase 2.
    let f = RecoveryFixture::run(async |handle, pool| {
        let _rx = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("phantom-drv")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'assigned', \
             assigned_builder_id = 'phantom-w1' WHERE drv_hash = 'phantom-drv'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let (sched_db, handle) = (f.db, f.handle);

    // Worker reconnects: BuildExecution stream + heartbeat with
    // EMPTY running_build (because it never actually got the
    // assignment — the try_send never happened). Single worker
    // suffices: reset_orphan_to_ready does NOT record the phantom
    // as a failure (r[sched.reassign.no-promote-on-ephemeral-
    // disconnect]), so w1 stays eligible and receives the retry.
    let mut worker_rx = connect_executor(&handle, "phantom-w1", "x86_64-linux").await?;
    barrier(&handle).await;

    // Verify: drv is Assigned, worker is in self.executors, but
    // running_build does NOT contain the drv (phantom!).
    let pre = expect_drv(&handle, "phantom-drv").await;
    assert_eq!(
        pre.status,
        DerivationStatus::Assigned,
        "drv should be Assigned after recovery"
    );

    // ReconcileAssignments: worker present BUT drv not in
    // running_build → reconcile. No store client here, so
    // store-check fails → reset to Ready (not Completed).
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // THE KEY ASSERTION: drv should be Ready (or re-dispatched).
    // Without the running_build cross-check, it would stay Assigned
    // forever — worker present meant "leave it, completion will
    // arrive", but the worker never had it so no completion comes.
    let post = expect_drv(&handle, "phantom-drv").await;
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "phantom-Assigned should be reconciled (Ready or re-dispatched to Assigned), got {:?}",
        post.status
    );
    // Reconcile must NOT count the phantom as a derivation failure
    // (alignment with reassign_derivations).
    assert!(
        post.retry.failed_builders.is_empty(),
        "phantom reconcile must NOT insert into failed_builders, got {:?}",
        post.retry.failed_builders
    );
    assert_eq!(
        post.retry.count, 0,
        "phantom reconcile must NOT bump retry.count"
    );
    let (retry_count, failed): (i32, Vec<String>) = sqlx::query_as(
        "SELECT retry_count, failed_builders FROM derivations WHERE drv_hash = 'phantom-drv'",
    )
    .fetch_one(&sched_db.pool)
    .await?;
    assert_eq!(retry_count, 0, "PG retry_count must NOT be bumped");
    assert!(failed.is_empty(), "PG failed_builders must NOT be appended");

    // Proof reconcile actually ran (not the OLD stuck Assigned): the
    // post-reconcile dispatch_ready re-assigned to w1, so w1's stream
    // now has the assignment.
    let assignment = recv_assignment(&mut worker_rx).await;
    assert_eq!(assignment.drv_path, test_drv_path("phantom-drv"));

    Ok(())
}

/// Phantom-check race with first-heartbeat-after-reconnect.
///
/// `collect_orphaned_assignments` reads `running_build`, which stays
/// `None` from stream-connect until the first ACCEPTED heartbeat
/// (I-048b drops pre-stream heartbeats; the worker's 10s tick doesn't
/// fire-on-reconnect). Gating the phantom-check on `contains_key`
/// alone misclassifies an actively-running build as phantom when the
/// stream lands shortly before `RECONCILE_DELAY` but the heartbeat
/// hasn't yet — spurious failure_count++ + duplicate dispatch.
///
/// Fix: gate on `is_registered()` (stream AND ≥1 heartbeat), defer
/// otherwise. The heartbeat path's two-strike `confirmed_phantoms`
/// catches real phantoms once heartbeats flow.
#[tokio::test]
async fn test_reconcile_defers_stream_connected_unregistered_worker() -> TestResult {
    let f = RecoveryFixture::run(async |handle, pool| {
        let _rx = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("defer-drv")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'assigned', \
             assigned_builder_id = 'defer-w1' WHERE drv_hash = 'defer-drv'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let (sched_db, handle) = (f.db, f.handle);

    // Stream-connect ONLY — no heartbeat. is_registered()=false,
    // running_build=None. NOT connect_executor() (which heartbeats).
    let (stream_tx, _stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "defer-w1".into(),
            stream_tx,
        })
        .await?;
    barrier(&handle).await;

    let pre = expect_drv(&handle, "defer-drv").await;
    assert_eq!(pre.status, DerivationStatus::Assigned);
    assert_eq!(pre.assigned_executor.as_deref(), Some("defer-w1"));

    // ReconcileAssignments while stream-connected-but-unregistered:
    // must DEFER, not flag phantom.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "defer-drv").await;
    assert_eq!(
        post.status,
        DerivationStatus::Assigned,
        "stream-connected-but-unheartbeated worker must defer, not reset"
    );
    assert_eq!(
        post.assigned_executor.as_deref(),
        Some("defer-w1"),
        "assignment unchanged"
    );
    let retry_count: i32 =
        sqlx::query_scalar("SELECT retry_count FROM derivations WHERE drv_hash = 'defer-drv'")
            .fetch_one(&sched_db.pool)
            .await?;
    assert_eq!(
        retry_count, 0,
        "deferral must NOT bump retry_count (was spurious failure before fix)"
    );

    // First heartbeat now arrives reporting the build IS running:
    // is_registered() flips true; adopt path takes it. No failure
    // recorded, no duplicate dispatch.
    send_heartbeat_with(&handle, "defer-w1", "x86_64-linux", |hb| {
        hb.running_build = Some(test_drv_path("defer-drv"));
    })
    .await?;
    barrier(&handle).await;

    let adopted = expect_drv(&handle, "defer-drv").await;
    assert!(
        matches!(
            adopted.status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "drv adopted after heartbeat, got {:?}",
        adopted.status
    );
    assert_eq!(adopted.assigned_executor.as_deref(), Some("defer-w1"));
    assert_eq!(adopted.retry.count, 0, "no spurious retry++ on adopt");

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
    let f = RecoveryFixture::run(async |handle, pool| {
        // Valid row via normal merge.
        merge_single_node(
            &handle,
            Uuid::new_v4(),
            "z1-good-drv",
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Bad row: garbage drv_path that StorePath::parse rejects.
        sqlx::query(
            "INSERT INTO derivations (drv_hash, drv_path, system, status) \
             VALUES ('z1-bad-drv', 'not-a-store-path', 'x86_64-linux', 'ready')",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

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
    let f = RecoveryFixture::run(async |handle, pool| {
        // Seed a derivation (FK target) + an assignment with generation=100.
        merge_single_node(
            &handle,
            Uuid::new_v4(),
            "z2-gen-drv",
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let (drv_id,): (Uuid,) =
            sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = 'z2-gen-drv'")
                .fetch_one(&pool)
                .await?;
        sqlx::query(
            "INSERT INTO assignments (derivation_id, builder_id, generation, status) \
             VALUES ($1, 'seed-worker', 100, 'completed')",
        )
        .bind(drv_id)
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    // Recovery's fetch_max should bump gen to max(1, 100+1) = 101.
    let g = f.handle.leader_generation();
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
    let orphan_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        // Seed orphan (NO build_derivations links) + normal build.
        sqlx::query("INSERT INTO builds (build_id, status) VALUES ($1, 'active')")
            .bind(orphan_id)
            .execute(&pool)
            .await?;
        merge_single_node(
            &handle,
            Uuid::new_v4(),
            "z16-normal",
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        Ok(())
    })
    .await?;
    let handle = f.handle;

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
    // In-process store (real client) — broken by closing its PG pool.
    let (store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;

    // expected_output_paths must be set or reconcile short-circuits
    // before the store call ("No expected outputs → treat as
    // incomplete") — a DIFFERENT code path.
    let _build_id = seed_orphan_assigned(
        &sched_db.pool,
        "z4-drv",
        &test_store_path("z4-out"),
        "z4-dead-worker",
    )
    .await?;

    // Break the store: close its PG pool. FindMissingPaths will
    // return an Err (sqlx::Error::PoolClosed → tonic::Status).
    store_db.pool.close().await;
    let (handle, _task) = recover_with_store(sched_db.pool.clone(), store_client).await?;

    // Pre-reconcile: drv should be Assigned (recovered from PG).
    let pre = expect_drv(&handle, "z4-drv").await;
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
    // outputs). retry.count NOT bumped: orphan reset is an
    // infrastructure event (alignment with reassign_derivations).
    let post = expect_drv(&handle, "z4-drv").await;
    assert_eq!(
        post.status,
        DerivationStatus::Ready,
        "store unreachable → assume incomplete → reset to Ready"
    );
    assert_eq!(
        post.retry.count, 0,
        "orphan reset is an infra event, not a derivation failure"
    );
    assert!(
        post.assigned_executor.is_none(),
        "reset_to_ready clears assigned_executor"
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
    let f =
        RecoveryFixture::run(async |handle, _| seed_poisoned(&handle, "poison-rec").await).await?;

    // Verify PG has it poisoned + poisoned_at set (the as_bytes bug broke this).
    let (status, has_ts): (String, bool) =
        sqlx::query_as("SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash=$1")
            .bind("poison-rec")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(status, "poisoned");
    assert!(has_ts, "PG poisoned_at should be set");

    // After recovery: derivation is back in the DAG with Poisoned status.
    let post = expect_drv(&f.handle, "poison-rec").await;
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
    let f = RecoveryFixture::run(async |handle, pool| {
        seed_poisoned(&handle, "exp-poison").await?;
        drop(handle);
        // Backdate poisoned_at well past POISON_TTL (cfg(test) = 100ms,
        // so 10s is 100× past). PG computes elapsed_secs at load time.
        sqlx::query(
            "UPDATE derivations SET poisoned_at = now() - interval '10 seconds' WHERE drv_hash = $1",
        )
        .bind("exp-poison")
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    // Derivation NOT in the DAG — recovery filtered it.
    let post = f.handle.debug_query_derivation("exp-poison").await?;
    assert!(
        post.is_none(),
        "expired-at-load poison should be cleared, not reloaded into DAG"
    );

    // PG: clear_poison ran → status='created', poisoned_at NULL.
    let (status, has_ts): (String, bool) =
        sqlx::query_as("SELECT status, poisoned_at IS NOT NULL FROM derivations WHERE drv_hash=$1")
            .bind("exp-poison")
            .fetch_one(&f.db.pool)
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
    let f =
        RecoveryFixture::run(async |handle, _| seed_poisoned(&handle, "zombie-drv").await).await?;
    let handle = f.handle;
    let mut worker_rx = connect_executor(&handle, "zombie-w2", "x86_64-linux").await?;

    // Precondition: recovery loaded the poisoned node.
    let recovered = expect_drv(&handle, "zombie-drv").await;
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
    let post_merge = expect_drv(&handle, "zombie-drv").await;
    assert_ne!(
        post_merge.status,
        DerivationStatus::Created,
        "node must progress past Created (compute_initial_states ran)"
    );

    Ok(())
}

// r[verify sched.recovery.poisoned-failed-count]
/// Route 1: crash between `persist_poisoned` and the build transition
/// to Failed. PG has drv `status='poisoned'`, `poisoned_at` SET, build
/// `status='active'`. Recovery must load the poisoned drv into
/// `id_to_hash` → `bd_rows` join succeeds → `build_summary` counts it
/// in `failed` → build → Failed. Regardless of `keep_going`.
///
/// - **keep_going=true**: exercises `all_resolved && failed>0`. Before
///   the keystone fix: poisoned row loaded into DAG but NOT id_to_hash
///   → join fell through → total=0 → spurious Succeeded.
/// - **keep_going=false** (default): before the `|| !keep_going` fix in
///   `check_build_completion`, fell through both branches → stuck
///   Active forever (live failures go through `handle_derivation_failure`;
///   recovery's sweep calls `check_build_completion` directly).
#[rstest::rstest]
#[case::keep_going_true(true)]
#[case::keep_going_false(false)]
#[tokio::test]
async fn test_recovery_poisoned_orphan_build_fails(#[case] keep_going: bool) -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        let mut rx = connect_executor(&handle, "r1-w", "x86_64-linux").await?;
        let _ev = merge_dag(
            &handle,
            build_id,
            vec![make_node("r1-drv")],
            vec![],
            keep_going,
        )
        .await?;
        let _ = rx.recv().await.expect("assignment");
        barrier(&handle).await;
        drop(handle);
        // Simulate crash-after-persist_poisoned: drv poisoned, build still active.
        sqlx::query(
            "UPDATE derivations SET status='poisoned', poisoned_at=now() WHERE drv_hash=$1",
        )
        .bind("r1-drv")
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    let status = query_status(&f.handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "recovered build with only-poisoned drv MUST be Failed (keep_going={keep_going})"
    );
    let (pg_status,): (String,) = sqlx::query_as("SELECT status FROM builds WHERE build_id=$1")
        .bind(build_id)
        .fetch_one(&f.db.pool)
        .await?;
    assert_eq!(pg_status, "failed", "PG build status must follow in-mem");
    Ok(())
}

// ---- Recovery TOCTOU on lease flap (remediation 08) ------------
// r[verify sched.recovery.gate-dispatch]
//   Generation snapshot + re-check: if the lease flaps (lose→
//   reacquire) mid-recovery, discard the stale DAG instead of
//   dispatching from it with the NEW generation stamped on.

/// Recovery TOCTOU: if the lease flaps (lose→reacquire, generation
/// bumps) mid-recovery, discard the stale DAG instead of dispatching
/// from it with the NEW generation stamped on. If no bump, complete
/// normally (proves no false-positive — would regress every recovery
/// test).
///
/// Timeline (bump case): actor snapshots gen=2 → runs recover_from_pg
/// → parks at gate → [test simulates lease flap: clear
/// recovery_complete + fetch_add gen 2→3] → release → actor re-loads
/// gen=3, sees 3≠2 → DISCARD. Pre-fix: unconditional `store(true)`
/// clobbered the lease loop's clear → dispatch_ready fired with gen-2
/// DAG and gen-3 stamps.
#[rstest::rstest]
#[case::gen_bump_discards(true, false)]
#[case::no_bump_completes(false, true)]
#[tokio::test]
async fn test_recovery_toctou_on_lease_flap(
    #[case] bump_gen: bool,
    #[case] expect_recovery_complete: bool,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let generation = Arc::new(AtomicU64::new(2));
    let recovery_complete = Arc::new(AtomicBool::new(false));
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let (g, rc) = (Arc::clone(&generation), Arc::clone(&recovery_complete));
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(g, Arc::new(AtomicBool::new(true)), rc);
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");

    if bump_gen {
        // Simulate lease flap: lose (clear) + reacquire (bump gen).
        recovery_complete.store(false, Ordering::Relaxed);
        generation.fetch_add(1, Ordering::Release);
    }
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert_eq!(
        recovery_complete.load(Ordering::Acquire),
        expect_recovery_complete,
        "bump_gen={bump_gen}: recovery_complete must be {expect_recovery_complete}"
    );
    if !bump_gen {
        assert_eq!(generation.load(Ordering::Acquire), 2, "gen unchanged");
    }
    Ok(())
}

/// `handle_reconcile_assignments` must NOT write to PG when not leader.
/// The 45s reconcile timer is fire-and-forget and `on_lose` doesn't
/// cancel it or clear the DAG; without an `is_leader()` gate, an
/// ex-leader's timer fires against a stale DAG and overwrites the new
/// leader's PG derivation state.
// r[verify sched.reconcile.leader-gate]
#[tokio::test]
async fn test_reconcile_skipped_when_not_leader() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let is_leader = Arc::new(AtomicBool::new(true));
    let il = Arc::clone(&is_leader);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(
            Arc::new(AtomicU64::new(1)),
            il,
            Arc::new(AtomicBool::new(true)),
        );
    });

    // Seed an Assigned drv on a worker that won't be in self.executors
    // → would be reset_orphan_to_ready'd if reconcile ran.
    let _rx = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("ex-leader-drv")],
        vec![],
        false,
    )
    .await?;
    barrier(&handle).await;
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', \
         assigned_builder_id = 'gone-w' WHERE drv_hash = 'ex-leader-drv'",
    )
    .execute(&db.pool)
    .await?;
    // Re-recover so the actor's in-mem DAG reflects PG (Assigned/gone-w).
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    let pre = expect_drv(&handle, "ex-leader-drv").await;
    assert_eq!(pre.status, DerivationStatus::Assigned);

    // Lease lost: flip is_leader=false (on_lose's effect on the atomic).
    is_leader.store(false, Ordering::Release);

    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    // PG must be UNTOUCHED — still 'assigned', no failed_builders entry.
    let (status, failed): (String, Vec<String>) = sqlx::query_as(
        "SELECT status, failed_builders FROM derivations WHERE drv_hash = 'ex-leader-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        status, "assigned",
        "ex-leader reconcile must NOT mutate PG status (got {status})"
    );
    assert!(
        failed.is_empty(),
        "ex-leader reconcile must NOT append failed_builders (got {failed:?})"
    );
    Ok(())
}

/// `reset_orphan_to_ready` must NOT count an orphaned assignment as a
/// derivation failure — same semantics as `reassign_derivations`. An
/// orphan (worker died OR phantom) is an infrastructure event, not a
/// build failure; counting it penalized innocent workers and pushed
/// derivations toward poison purely because the scheduler restarted.
// r[verify sched.reassign.no-promote-on-ephemeral-disconnect+4]
#[tokio::test]
async fn test_reset_orphan_does_not_record_failure() -> TestResult {
    // Seed Assigned drv on worker 'dead-w' that won't reconnect.
    let f = RecoveryFixture::run(async |handle, pool| {
        let _rx = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("orphan-nofail")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'assigned', \
             assigned_builder_id = 'dead-w' WHERE drv_hash = 'orphan-nofail'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let (db, handle) = (f.db, f.handle);

    // No store client → outputs_present_in_store=false → reset path.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "orphan-nofail").await;
    assert_eq!(
        post.status,
        DerivationStatus::Ready,
        "orphan should reset to Ready"
    );
    assert!(
        post.retry.failed_builders.is_empty(),
        "orphan reset must NOT record failed_builders (got {:?})",
        post.retry.failed_builders
    );
    assert_eq!(
        post.retry.count, 0,
        "orphan reset must NOT bump retry.count"
    );
    assert_eq!(
        post.retry.failure_count, 0,
        "orphan reset must NOT bump failure_count"
    );

    let (retry_count, failed): (i32, Vec<String>) = sqlx::query_as(
        "SELECT retry_count, failed_builders FROM derivations WHERE drv_hash = 'orphan-nofail'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(retry_count, 0, "PG retry_count must be 0");
    assert!(
        failed.is_empty(),
        "PG failed_builders must be empty (got {failed:?})"
    );
    Ok(())
}
