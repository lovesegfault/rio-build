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

// r[verify sched.recovery.failed-dep-cascade+2]
/// bug_341: crash mid-`cascade_dependency_failure` leaves PG with
/// child=poisoned, parent=queued, build=active. `load_edges_for_
/// derivations` drops the edge (child terminal → not in $1), so
/// `compute_initial_states` sees `all_deps_completed(parent)=true` →
/// wrongly Ready → dispatched against missing input. Recovery must
/// short-circuit parent → DependencyFailed instead.
#[tokio::test]
async fn test_recovery_failed_dep_transitions_parent_not_ready() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        merge_chain(
            &handle,
            build_id,
            &["faildep-child", "faildep-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Precondition: parent=queued (so it hits the I-058 collection).
        let (pg_status,): (String,) =
            sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'faildep-parent'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            pg_status, "queued",
            "test precondition: parent must be Queued in PG"
        );
        // Backdate: child → poisoned (cascade_dependency_failure
        // persisted child but crashed before parent). Build stays
        // Active (interested_builds non-empty → I-059 gate passes).
        sqlx::query("UPDATE derivations SET status = 'poisoned' WHERE drv_hash = 'faildep-child'")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // Parent must be DependencyFailed — NOT Ready. Before the fix,
    // the dropped edge made all_deps_completed()=true → Ready.
    let parent = expect_drv(&handle, "faildep-parent").await;
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "parent with poisoned dep must transition to DependencyFailed, not Ready"
    );

    // Persisted to PG (so it doesn't leak as a non-terminal orphan
    // once the build goes terminal).
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'faildep-parent'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(
        pg_status, "dependency_failed",
        "DependencyFailed persisted to PG"
    );

    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
/// Transitive (depth ≥2) ancestors of a PG-terminal-failed child must
/// ALSO be persisted as DependencyFailed, not just transitioned in-DAG.
///
/// 3-deep chain: child←parent←grand. PG has child=poisoned, parent and
/// grand both queued. The `cascade_failed` loop persists `parent`
/// (immediate parent of a PG-failed child); `compute_initial_states`
/// returns DependencyFailed for `grand` (its dep `parent` is now
/// DepFailed in-DAG + `will_fail` propagation). Without the persist
/// after that transition, `grand` stays 'queued' in PG and leaks
/// permanently once the build_derivations link is GC'd
/// (gc_orphan_terminal_derivations filters `status IN TERMINAL`).
#[tokio::test]
async fn test_recovery_transitive_failed_dep_persisted() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        merge_chain(
            &handle,
            build_id,
            &["fdc-child", "fdc-parent", "fdc-grand"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate: child → poisoned (cascade_dependency_failure
        // persisted child but crashed before parent/grand). Build
        // stays Active (interested_builds non-empty).
        sqlx::query("UPDATE derivations SET status = 'poisoned' WHERE drv_hash = 'fdc-child'")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;

    // In-DAG: grand must be DependencyFailed (compute_initial_states
    // sees parent as DepFailed via cascade_failed + will_fail).
    let grand = expect_drv(&f.handle, "fdc-grand").await;
    assert_eq!(
        grand.status,
        DerivationStatus::DependencyFailed,
        "depth-2 ancestor must be DependencyFailed in-DAG"
    );

    // PG: grand must ALSO be persisted (not stuck at 'queued'). This
    // is the leak guard — before the fix, only the in-DAG transition
    // happened.
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'fdc-grand'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(
        pg_status,
        DerivationStatus::DependencyFailed.as_str(),
        "depth-2 ancestor must be persisted to PG (else permanent row leak)"
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
    Ok(rx.await??.state)
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

    // Subscribe to the build's event stream BEFORE reconcile so we
    // can observe the per-derivation Completed event.
    let (ev_tx, ev_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: ev_tx,
            caller_tenant: None,
        })
        .await?;
    let (mut events, _last_seq) = ev_rx.await??;

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

    // Per-derivation Completed event must be emitted exactly once (with
    // output paths) BEFORE the build-level Completed; without this,
    // clients see the drv frozen at Started. Count (not just observe)
    // so a duplicate emission fails the test — emit() has no dedup, so
    // a second loop would write 2× build_event_log rows + 2× broadcasts.
    use rio_proto::types::{DerivationEventKind, build_event::Event};
    let mut drv_completed: Vec<rio_proto::types::DerivationEvent> = Vec::new();
    let mut got_build_completed = false;
    while let Ok(ev) = events.state.try_recv() {
        match ev.event {
            Some(Event::Derivation(d)) if d.kind() == DerivationEventKind::Completed => {
                assert!(
                    !got_build_completed,
                    "DerivationCompleted must precede BuildCompleted"
                );
                drv_completed.push(d);
            }
            Some(Event::Completed(_)) => got_build_completed = true,
            _ => {}
        }
    }
    assert_eq!(
        drv_completed.len(),
        1,
        "adopt_orphan_completion emitted DerivationCompleted {}× (expected exactly 1)",
        drv_completed.len()
    );
    let d = &drv_completed[0];
    assert_eq!(d.derivation_path, test_drv_path("orphan-drv"));
    assert_eq!(d.output_paths, vec![out_path]);

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

// r[verify sched.merge.wanted-outputs+2]
/// Orphan adoption × wanted outputs: an orphaned assignment (worker
/// gone) whose only missing output is one nothing wants must still be
/// adopted as completed. The worker built and uploaded every output
/// anyone consumes before the scheduler died; forcing the whole
/// derivation back to Ready over an absent `-debug` output nobody
/// references re-dispatches a finished build. The wanted set must
/// round-trip through PG (the orphan is reconstructed from the
/// recovered row, not from a live submission).
#[tokio::test]
async fn test_orphan_adoption_ignores_missing_unwanted_output() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out = test_store_path("orphan-w-out");
    let dbg = test_store_path("orphan-w-debug");
    // Only the WANTED output is in the store. P_debug is missing.
    put_test_path(&mut store_client, &out).await?;

    // Phase 1: merge a multi-output node wanting only {out}, drop the
    // actor, backdate PG to Assigned-by-a-dead-worker. (Inline
    // seed_orphan_assigned — that helper is single-output.)
    let build_id = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(sched_db.pool.clone());
        let mut node = make_node("orphan-w-drv");
        node.output_names = vec!["out".into(), "debug".into()];
        node.expected_output_paths = vec![out.clone(), dbg.clone()];
        node.wanted_output_names = vec!["out".into()];
        let _rx = merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = $1 \
         WHERE drv_hash = $2",
    )
    .bind("orphan-w-dead")
    .bind("orphan-w-drv")
    .execute(&sched_db.pool)
    .await?;

    // Phase 2: recover (the wanted set comes back from PG), reconcile.
    let (handle, _task) = recover_with_store(sched_db.pool.clone(), store_client.clone()).await?;
    assert_eq!(
        expect_drv(&handle, "orphan-w-drv").await.status,
        DerivationStatus::Assigned,
        "precondition: recovered as Assigned to a worker that won't reconnect"
    );
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, "orphan-w-drv").await;
    assert_eq!(
        post.status,
        DerivationStatus::Completed,
        "all WANTED outputs present → orphan adopted as completed; the \
         missing unwanted P_debug must not force a reset to Ready"
    );
    assert_eq!(
        post.output_paths,
        vec![out, dbg],
        "the adopted node still records ALL declared paths"
    );
    assert_eq!(
        query_status(&handle, build_id).await?.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "single-drv build succeeds via orphan adoption"
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
            stream_epoch: next_stream_epoch_for("defer-w1"),
            auth_intent: None,
            reply: noop_connect_reply(),
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

/// bug_001 + discovered_001: `resubmit_cycles` survives recovery.
/// Poison + resubmit twice (PG `resubmit_cycles` → 2 = LIMIT), restart
/// scheduler, resubmit → bound MUST hold. Before `M_051`,
/// `from_poisoned_row` left the cross-cycle counter at default 0 and
/// `clear_poison_batch` zeroed PG on every resubmit, so failover gave a
/// fresh budget.
// r[verify sched.merge.poisoned-resubmit-bounded+2]
#[tokio::test]
async fn test_poisoned_recovery_preserves_resubmit_cycles() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let f = RecoveryFixture::run(async |handle, pool| {
        seed_poisoned(&handle, "rs-cyc").await?;
        // Drive resubmit_cycles to LIMIT in PG (mirror the in-mem
        // increment that `clear_poison_batch` would have done over
        // LIMIT resubmit cycles). Status stays 'poisoned' so recovery
        // loads via `load_poisoned_derivations`.
        sqlx::query("UPDATE derivations SET resubmit_cycles = $2 WHERE drv_hash = $1")
            .bind("rs-cyc")
            .bind(POISON_RESUBMIT_RETRY_LIMIT as i32)
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;

    // After recovery: resubmit_cycles loaded from PG.
    let post = expect_drv(&f.handle, "rs-cyc").await;
    assert_eq!(post.status, DerivationStatus::Poisoned);
    assert_eq!(
        post.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT,
        "bug_001: from_poisoned_row must load resubmit_cycles from PG"
    );

    // Resubmit on the fresh actor → bound holds (stays Poisoned).
    let build2 = Uuid::new_v4();
    merge_dag(&f.handle, build2, vec![make_node("rs-cyc")], vec![], false).await?;
    barrier(&f.handle).await;
    let after = expect_drv(&f.handle, "rs-cyc").await;
    assert_eq!(
        after.status,
        DerivationStatus::Poisoned,
        "discovered_001: resubmit bound must survive failover"
    );
    assert_eq!(
        query_status(&f.handle, build2).await?.state,
        rio_proto::types::BuildState::Failed as i32
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

    // No store client → batch_probe_orphan_outputs=None → reset path.
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

/// Recovery recompute of a `Substituting` node whose dep is `Poisoned`
/// must reach `DependencyFailed` via the two-step Queued bridge.
/// Without the bridge covering `Substituting`, the
/// `Substituting→DependencyFailed` transition (not in the table) fails
/// with a warn and the node stays `Substituting` forever — no fetch
/// task (died with the old process), never pushed Ready.
#[tokio::test]
async fn test_recovery_substituting_with_poisoned_dep_goes_dependency_failed() -> TestResult {
    let f = RecoveryFixture::run(async |handle, pool| {
        // D depends on E. Merge both (build stays Active), then
        // backdate PG: E poisoned, D substituting (detached fetch in
        // flight at crash). Direct PG writes so no in-mem cascade
        // marks the build terminal pre-recovery.
        let _rx = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("sub-D"), make_node("sub-E")],
            vec![make_test_edge("sub-D", "sub-E")],
            true,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'poisoned', poisoned_at = now() \
             WHERE drv_hash = 'sub-E'",
        )
        .execute(&pool)
        .await?;
        sqlx::query("UPDATE derivations SET status = 'substituting' WHERE drv_hash = 'sub-D'")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;

    let d = expect_drv(&f.handle, "sub-D").await;
    assert_eq!(
        d.status,
        DerivationStatus::DependencyFailed,
        "Substituting with poisoned dep must recompute to DependencyFailed \
         (not stuck Substituting), got {:?}",
        d.status
    );
    Ok(())
}

/// I-059 orphan guard must also cover already-`Ready` nodes. A
/// derivation that is Ready in PG but whose every build is terminal
/// (e.g. produced by `check_build_completion`'s recovery `!keep_going`
/// branch which doesn't `cancel_build_derivations`) bypassed the
/// recompute-set guard and dispatched into GC'd inputs → spurious
/// poison.
#[tokio::test]
async fn test_recovery_orphan_ready_not_pushed() -> TestResult {
    let f = RecoveryFixture::run(async |handle, pool| {
        let _rx = merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("orphan-ready")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Drv stays 'ready' (merge left it there: no deps); build → failed.
        // load_nonterminal_builds will exclude it → interested_builds empty.
        sqlx::query("UPDATE builds SET status = 'failed'")
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // Node loaded (Ready is non-terminal) but orphan — no active build.
    let d = expect_drv(&handle, "orphan-ready").await;
    assert_eq!(d.status, DerivationStatus::Ready);

    // Connect a worker and tick: orphan-Ready must NOT dispatch.
    let mut rx = connect_executor(&handle, "orphan-ready-w", "x86_64-linux").await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    match tokio::time::timeout(Duration::from_millis(200), rx.recv()).await {
        Err(_) => {} // timeout — nothing dispatched (good)
        Ok(Some(msg)) => {
            use rio_proto::types::scheduler_message::Msg;
            // PrefetchHint is fine (warm-gate); Assignment is the bug.
            if let Some(Msg::Assignment(a)) = msg.msg {
                panic!(
                    "orphan-Ready must NOT dispatch, got assignment for {}",
                    a.drv_path
                );
            }
        }
        Ok(None) => {}
    }
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// `ProcessCompletion` arriving after `on_lose()` MUST be dropped at
/// actor dispatch — an ex-leader must not write `persist_status(Completed)`
/// to PG (races the new leader's recovery → Completed→Ready races,
/// duplicate dispatch).
#[tokio::test]
async fn ex_leader_drops_process_completion() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let leader = crate::lease::LeaderState::default(); // always-leader
    let leader_for_actor = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = leader_for_actor;
    });

    let build_id = Uuid::new_v4();
    let mut rx = connect_executor(&handle, "ex-w", "x86_64-linux").await?;
    merge_single_node(&handle, build_id, "ex-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut rx).await;

    // Lose the lease. Open worker stream stays open (on_lose only flips
    // atomics) — that's what the gRPC-layer fence covers; this test
    // exercises the actor-dispatch defense-in-depth.
    leader.on_lose();

    // CompletionReport arrives on the still-open stream → forwarded to
    // actor as ProcessCompletion. Send directly (bypassing the gRPC
    // reader) to isolate the actor-level gate.
    handle
        .send_unchecked(ActorCommand::ProcessCompletion {
            executor_id: "ex-w".into(),
            drv_key: test_drv_path("ex-drv"),
            result: rio_proto::types::BuildResult {
                status: rio_proto::types::BuildResultStatus::Built.into(),
                ..Default::default()
            },
            peak_memory_bytes: 0,
            peak_cpu_cores: 0.0,
            node_name: None,
            hw_class: None,
            final_resources: None,
        })
        .await?;
    barrier(&handle).await;

    // PG status MUST NOT be Completed. Compare via the type-checked
    // accessor so a typo in the literal can't make this vacuous (PG
    // stores lowercase per db_str_enum!). Positive equality against
    // the pre-state (the dispatch path persists Assigned) is robust
    // against future status-enum additions.
    let row: Option<(String,)> =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'ex-drv'")
            .fetch_optional(&db.pool)
            .await?;
    assert_ne!(
        row.as_ref().map(|r| r.0.as_str()),
        Some(DerivationStatus::Completed.as_str()),
        "ex-leader must NOT persist Completed; got {row:?}"
    );
    assert_eq!(
        row.as_ref().map(|r| r.0.as_str()),
        Some(DerivationStatus::Assigned.as_str()),
        "ex-leader ProcessCompletion must leave PG status untouched; got {row:?}"
    );
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// `CancelBuild` dequeued after `on_lose()` MUST be dropped at actor
/// dispatch with `Err(NotLeader)`. An ex-leader's cancel would write
/// `persist_status_batch(Cancelled)` from a stale DAG and — worse —
/// `terminal_log_epilogue` → `record_exec_correlation`, whose
/// `AND exec_id IS NULL` guard makes `build_derivations.exec_id`
/// write-once: a stale exec pinned here permanently blocks the new
/// leader's correct correlation after it re-dispatches the drv.
#[tokio::test]
async fn ex_leader_drops_cancel_build() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let leader = crate::lease::LeaderState::default(); // always-leader
    let leader_for_actor = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = leader_for_actor;
    });

    // Real merge + dispatch so the drv reaches Assigned with a real
    // exec_id and the build_derivations row exists in PG (the merge tx
    // creates it with exec_id NULL — the row record_exec_correlation's
    // UPDATE targets).
    let build_id = Uuid::new_v4();
    let mut rx = connect_executor(&handle, "exlc-w", "x86_64-linux").await?;
    merge_single_node(&handle, build_id, "exlc-drv", PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.drv_path,
        test_drv_path("exlc-drv"),
        "precondition: drv must be dispatched (exec_id stamped) before the lease flip, \
         otherwise record_exec_correlation no-ops regardless of the gate and this test \
         is vacuous"
    );

    // Lose the lease. on_lose only flips atomics — the LeaderLost actor
    // command is sent separately in production (tokio::spawn in
    // main.rs), so a CancelBuild already in the mailbox is processed
    // with is_leader=false against the still-populated DAG. That is
    // exactly the window under test.
    leader.on_lose();

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test cancel after lease loss".into(),
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx
        .await
        .expect("gate must reply, not drop the oneshot");
    assert!(
        matches!(result, Err(ActorError::NotLeader)),
        "ex-leader CancelBuild must be rejected with NotLeader (maps to retriable \
         UNAVAILABLE); got {result:?}"
    );
    barrier(&handle).await;

    // (1) The write-once exec correlation MUST NOT have been written:
    // the new leader re-dispatches this drv under a fresh exec_id and
    // its own record_exec_correlation must find the row still NULL.
    let exec_id: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT bd.exec_id FROM build_derivations bd \
         JOIN derivations d USING (derivation_id) \
         WHERE bd.build_id = $1 AND d.drv_hash = 'exlc-drv'",
    )
    .bind(build_id)
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        exec_id,
        Some(None),
        "ex-leader CancelBuild must not pin the write-once build_derivations.exec_id \
         (row must exist and be NULL)"
    );

    // (2) No stale terminal status write: the drv stays at the
    // pre-flip Assigned the dispatch path persisted.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'exlc-drv'")
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some(DerivationStatus::Assigned.as_str()),
        "ex-leader CancelBuild must leave derivations.status untouched"
    );

    // (3) Build status untouched: positive pin against the post-merge
    // 'active' (merge.rs transitions pending→active), not a
    // negative assert_ne!("cancelled") that would pass vacuously if
    // the row were missing.
    let bstatus: Option<String> =
        sqlx::query_scalar("SELECT status FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        bstatus.as_deref(),
        Some("active"),
        "ex-leader CancelBuild must not transition the build (expected post-merge \
         'active')"
    );
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// `Heartbeat`/`PrefetchComplete` arms stay ungated (keep
/// `self.executors` accurate) but their PG-touching sub-calls MUST be:
/// `dispatch_ready` self-gates (dispatch.rs early-return); the
/// Heartbeat arm's `drain_phantoms` (which `persist_status`es Ready)
/// is gated at the call site. After `on_lose()`, neither writes PG.
#[tokio::test]
async fn ex_leader_heartbeat_prefetch_no_pg_writes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let leader = crate::lease::LeaderState::default();
    let leader_for_actor = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = leader_for_actor;
    });

    // Ready drv waiting for dispatch; worker connected.
    let mut rx = connect_executor(&handle, "exhb-w", "x86_64-linux").await?;
    merge_single_node(&handle, Uuid::new_v4(), "exhb-a", PriorityClass::Scheduled).await?;
    let _assign = recv_assignment(&mut rx).await; // exhb-a → Assigned/Running

    // Second drv left Ready (PrefetchComplete inline-dispatch would
    // pick it up if dispatch_ready weren't gated).
    merge_single_node(&handle, Uuid::new_v4(), "exhb-b", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    // Baseline PG status before lease-loss.
    let baseline: Vec<(String, String)> = sqlx::query_as(
        "SELECT drv_hash, status FROM derivations \
         WHERE drv_hash IN ('exhb-a','exhb-b') ORDER BY drv_hash",
    )
    .fetch_all(&db.pool)
    .await?;

    leader.on_lose();

    // PrefetchComplete (cold→warm edge) → dispatch_ready inline.
    // Self-gated inside dispatch_ready.
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "exhb-w".into(),
            paths_fetched: 0,
        })
        .await?;
    // Heartbeat became_idle path → dispatch_ready inline + (if
    // phantoms) drain_phantoms. Gated at call site. Phantom
    // confirmation is two-strike (executor.rs: prior_suspect ==
    // suspect), so a SECOND heartbeat is required for
    // confirmed_phantoms to be non-empty — otherwise
    // !phantoms.is_empty() short-circuits before is_leader() is
    // evaluated and this test is vacuous for the drain_phantoms gate.
    send_heartbeat_with(&handle, "exhb-w", "x86_64-linux", |_| {}).await?;
    send_heartbeat_with(&handle, "exhb-w", "x86_64-linux", |_| {}).await?;
    barrier(&handle).await;

    let after: Vec<(String, String)> = sqlx::query_as(
        "SELECT drv_hash, status FROM derivations \
         WHERE drv_hash IN ('exhb-a','exhb-b') ORDER BY drv_hash",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        baseline, after,
        "ex-leader Heartbeat/PrefetchComplete must not write PG"
    );
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// Worker disconnect processed by a deposed leader: the ungated
/// `ExecutorDisconnected` arm keeps its in-memory bookkeeping, but its
/// PG-writing tail `reassign_derivations` self-gates on `is_leader()`.
/// Reset branch (no poison precondition — the common case on every
/// mid-build disconnect in the flap window): the ungated tree persists
/// `Ready` over the new leader's recovery; the gate must leave the
/// dispatch-time `assigned` row untouched.
#[tokio::test]
async fn ex_leader_disconnect_drops_reassign_writes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let leader = crate::lease::LeaderState::default(); // always-leader
    let leader_for_actor = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = leader_for_actor;
    });

    // Real merge + dispatch so the drv reaches Assigned on this worker
    // (PG `derivations.status='assigned'` written by the dispatch path).
    let build_id = Uuid::new_v4();
    let mut rx = connect_executor(&handle, "exldr-w", "x86_64-linux").await?;
    merge_single_node(&handle, build_id, "exldr-drv", PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.drv_path,
        test_drv_path("exldr-drv"),
        "precondition: drv must be Assigned to exldr-w before the lease flip"
    );

    // Lose the lease. on_lose only flips atomics — the LeaderLost actor
    // command is sent separately in production, so a disconnect already
    // in the mailbox is processed with is_leader=false against the
    // still-populated DAG. The lease flip itself manufactures these:
    // every worker stream reader that trips the generation fence sends
    // ExecutorDisconnected to the deposed leader's actor.
    leader.on_lose();
    disconnect(&handle, "exldr-w").await?;

    // (1) Reset branch must NOT have persisted Ready: the drv stays at
    // the pre-flip Assigned the dispatch path wrote. (Red-first
    // discriminator: the ungated tree writes 'ready' here.)
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'exldr-drv'")
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some(DerivationStatus::Assigned.as_str()),
        "ex-leader disconnect must not persist Ready from a stale DAG \
         (the new leader's recovery owns this derivation)"
    );

    // (2) Disconnect must not record failures regardless of the gate
    // (guards against rerouting through the failure-recording path).
    let (retry_count, failed_builders): (i32, Vec<String>) = sqlx::query_as(
        "SELECT retry_count, failed_builders FROM derivations WHERE drv_hash = 'exldr-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(retry_count, 0, "disconnect must not bump retry_count");
    assert!(
        failed_builders.is_empty(),
        "disconnect must not record into failed_builders"
    );

    // (3) In-memory state is also untouched (stale state is
    // LeaderLost's job to clear). NOTE: this assertion encodes the
    // function-top gate placement (skip both PG and in-mem reset),
    // not the spec rule — update it if the gate ever moves to the
    // call sites.
    let drv = expect_drv(&handle, "exldr-drv").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Assigned,
        "gate skips the in-memory reset too; LeaderLost clears the DAG"
    );
    Ok(())
}

/// Shared fixture for the poison-threshold disconnect pair: connect a
/// worker, dispatch a drv to it, backdate three prior failures (a
/// previous incarnation's, recorded before a crash) in PG, then re-run
/// recovery (`LeaderAcquired`) on the same actor so the in-memory node
/// carries `failed_builders.len() == 3` while the worker stays
/// connected with the assignment live. The next disconnect of that
/// worker hits `reassign_derivations`' poison branch.
///
/// Distinct `tag`s per test are required, not cosmetic: STREAM_EPOCHS
/// (helpers.rs) is process-global keyed by executor id, so concurrent
/// tests sharing a worker id can stomp each other's epoch and turn the
/// disconnect into the I-056a no-op.
async fn poison_threshold_disconnect_fixture(
    tag: &str,
) -> anyhow::Result<(
    TestDb,
    ActorHandle,
    crate::lease::LeaderState,
    tokio::task::JoinHandle<()>,
    Uuid,
)> {
    let db = TestDb::new(&MIGRATOR).await;
    let leader = crate::lease::LeaderState::default(); // always-leader
    let leader_for_actor = leader.clone();
    let (handle, task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = leader_for_actor;
    });

    let worker = format!("{tag}-w");
    let drv = format!("{tag}-drv");
    let build_id = Uuid::new_v4();

    // 1. Dispatch the drv to the worker: Assigned, exec_id stamped in
    //    `assignments`, `build_derivations` row created with exec_id
    //    NULL (the row record_exec_correlation's UPDATE targets).
    let mut rx = connect_executor(&handle, &worker, "x86_64-linux").await?;
    merge_single_node(&handle, build_id, &drv, PriorityClass::Scheduled).await?;
    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.drv_path,
        test_drv_path(&drv),
        "fixture precondition: drv must be dispatched before the failure backdate"
    );

    // 2. Backdate the previous incarnation's failures: three distinct
    //    workers already failed this drv (recorded by the completion
    //    path before the crash). Disconnect itself never increments.
    sqlx::query(
        "UPDATE derivations SET failed_builders = '{ghost-a,ghost-b,ghost-c}' \
         WHERE drv_hash = $1",
    )
    .bind(&drv)
    .execute(&db.pool)
    .await?;

    // 3. Recover from PG on the same actor: the in-memory node is
    //    rebuilt with the backdated failed_builders and the exec_id
    //    from the assignments join; `self.executors` (and the worker's
    //    running_build) are untouched by recovery.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // 4. Anti-vacuity #1: if recovery ever stops loading
    //    failed_builders, fail loudly here instead of silently skipping
    //    the poison branch in the tests built on this fixture.
    let info = expect_drv(&handle, &drv).await;
    assert_eq!(
        info.retry.failed_builders.len(),
        3,
        "fixture precondition: recovery must load the backdated failed_builders"
    );

    Ok((db, handle, leader, task, build_id))
}

/// Leader-side control for the poison-threshold pair: proves the
/// fixture actually reaches `reassign_derivations`' poison branch and
/// that the write-once correlation pin is reachable from it, so the
/// ex-leader test below cannot pass vacuously. No `r[verify]` marker —
/// the rule under test in the pair is the standby gate, not this.
#[tokio::test]
async fn leader_disconnect_at_poison_threshold_poisons_and_pins() -> TestResult {
    let (db, handle, _leader, _task, build_id) =
        poison_threshold_disconnect_fixture("exldpa").await?;

    // Leader stays leader: disconnect hits the poison branch.
    disconnect(&handle, "exldpa-w").await?;

    // persist_poisoned is awaited inside the actor — direct read is fine.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'exldpa-drv'")
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some("poisoned"),
        "leader-side disconnect at the poison threshold must poison the drv \
         (the fixture must reach the poison branch)"
    );

    // The pin really happened, with the execution the fixture
    // dispatched. record_exec_correlation writes from spawn_monitored —
    // poll PG (established 10ms × 100 pattern).
    let dispatched_exec: Uuid = sqlx::query_scalar(
        "SELECT a.exec_id FROM assignments a \
         JOIN derivations d USING (derivation_id) \
         WHERE d.drv_hash = 'exldpa-drv'",
    )
    .fetch_one(&db.pool)
    .await?;
    let mut pinned: Option<Uuid> = None;
    for _ in 0..100 {
        pinned = sqlx::query_scalar(
            "SELECT bd.exec_id FROM build_derivations bd \
             JOIN derivations d USING (derivation_id) \
             WHERE bd.build_id = $1 AND d.drv_hash = 'exldpa-drv'",
        )
        .bind(build_id)
        .fetch_one(&db.pool)
        .await?;
        if pinned.is_some() {
            break;
        }
        tokio::time::sleep(std::time::Duration::from_millis(10)).await;
    }
    assert_eq!(
        pinned,
        Some(dispatched_exec),
        "leader-side poison must pin bd.exec_id to the dispatched execution \
         (control: the pin is reachable from this fixture)"
    );
    Ok(())
}

// r[verify sched.lease.standby-drops-writes]
/// The headline harm: a deposed leader processing a worker disconnect
/// for a drv at the poison threshold must NOT poison it, must NOT pin
/// the write-once `build_derivations.exec_id`, and must NOT
/// fail/cancel the build from stale state — the new leader's recovery
/// and reconcile sweeps own those derivations. (The leader-side
/// control above proves this fixture reaches the poison branch.)
#[tokio::test]
async fn ex_leader_disconnect_at_poison_threshold_writes_nothing() -> TestResult {
    let (db, handle, leader, _task, build_id) =
        poison_threshold_disconnect_fixture("exldpb").await?;

    // Lease flips BEFORE the disconnect lands (atomic flip only;
    // LeaderLost races behind it in the mailbox).
    leader.on_lose();
    disconnect(&handle, "exldpb-w").await?;

    // (1) No stale poison (or Ready) write.
    let status: Option<String> =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'exldpb-drv'")
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        status.as_deref(),
        Some(DerivationStatus::Assigned.as_str()),
        "ex-leader disconnect at the poison threshold must not write 'poisoned' \
         (or 'ready') from a stale DAG"
    );

    // (2) Write-once correlation left for the new leader. No poll
    // needed: with the gate the actor never calls
    // record_exec_correlation in this fixture, so there is no in-flight
    // spawned write to wait out (red-first: the ungated tree pins the
    // dispatched exec_id here).
    let exec_id: Option<Option<Uuid>> = sqlx::query_scalar(
        "SELECT bd.exec_id FROM build_derivations bd \
         JOIN derivations d USING (derivation_id) \
         WHERE bd.build_id = $1 AND d.drv_hash = 'exldpb-drv'",
    )
    .bind(build_id)
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        exec_id,
        Some(None),
        "ex-leader disconnect must not pin the write-once build_derivations.exec_id \
         (row must exist and be NULL for the new leader's correlation)"
    );

    // (3) keep_going=false escalation must not have run: build stays at
    // the post-merge 'active' (positive pin, not assert_ne).
    let bstatus: Option<String> =
        sqlx::query_scalar("SELECT status FROM builds WHERE build_id = $1")
            .bind(build_id)
            .fetch_optional(&db.pool)
            .await?;
    assert_eq!(
        bstatus.as_deref(),
        Some("active"),
        "ex-leader disconnect must not fail/cancel the build from stale state"
    );

    // (4) In-memory state untouched (LeaderLost clears it; the gate
    // returns before the in-mem reset/poison too).
    let drv = expect_drv(&handle, "exldpb-drv").await;
    assert_eq!(
        drv.status,
        DerivationStatus::Assigned,
        "gate must skip the in-memory poison/reset as well"
    );
    Ok(())
}

// r[verify sched.recovery.poisoned-failed-count]
/// Sticky `had_failure` (`error_summary.is_some()`) MUST survive
/// recovery. `restore_builds` reconstructs via `new_pending` →
/// `error_summary=None`; `finalize_recovered_builds` must seed it from
/// `failed_count` (which `update_build_counts` sets from the DAG, which
/// includes Poisoned). Otherwise: ClearPoison removes the node →
/// `failed_count=0` → keep_going build spuriously Succeeds.
#[tokio::test]
async fn test_recovery_keep_going_sticky_failure_survives_clear_poison() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        // 2 independent drvs, keep_going=true. Backdate X→poisoned
        // (failure persisted, build still Active — crash between
        // handle_derivation_failure setting in-mem error_summary and
        // any later persist).
        let _ev = merge_dag(
            &handle,
            build_id,
            vec![make_node("kgs-x"), make_node("kgs-y")],
            vec![],
            true, // keep_going
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'poisoned', poisoned_at = now() \
             WHERE drv_hash = 'kgs-x'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // ClearPoison X → node removed from DAG + derivation_hashes (per
    // prune_interested_keep_going). Now total=1 (just Y), failed=0.
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::ClearPoison {
            drv_hash: "kgs-x".into(),
            reply: tx,
        })
        .await?;
    assert!(rx.await?, "ClearPoison → cleared=true");

    // Complete Y. Without sticky error_summary reconstruction:
    // all_completed && failed==0 && !had_failure → spurious Succeeded.
    let mut wrx = connect_executor(&handle, "kgs-w", "x86_64-linux").await?;
    let _a = recv_assignment(&mut wrx).await;
    complete_success_empty(&handle, "kgs-w", &test_drv_path("kgs-y")).await?;
    barrier(&handle).await;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "keep_going build with pre-restart failure must Fail (not Succeed) \
         after ClearPoison removes the poisoned node post-restart"
    );
    Ok(())
}

// r[verify sched.timeout.per-build]
/// `build_timeout` is "wall-clock since SUBMISSION" — recovery must
/// seed `submitted_at` from PG, not reset to `Instant::now()`. Otherwise
/// each failover grants a fresh full timeout window.
#[tokio::test]
async fn test_recovery_restores_build_timeout_baseline() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        let (tx, rx) = oneshot::channel();
        handle
            .send_unchecked(ActorCommand::MergeDag {
                req: MergeDagRequest {
                    build_id,
                    tenant_id: None,
                    priority_class: PriorityClass::Scheduled,
                    nodes: vec![make_node("bto-drv")],
                    edges: vec![],
                    options: BuildOptions {
                        build_timeout: 60,
                        ..Default::default()
                    },
                    keep_going: false,
                    traceparent: String::new(),
                    jti: None,
                    jwt_token: None,
                },
                reply: tx,
            })
            .await?;
        let _ev = rx.await??;
        barrier(&handle).await;
        drop(handle);
        // Backdate submission past the timeout. With submitted_at
        // restored from PG, tick_check_build_timeouts fires
        // immediately on the recovered actor's first Tick.
        sqlx::query(
            "UPDATE builds SET submitted_at = now() - interval '120 seconds' \
             WHERE build_id = $1",
        )
        .bind(build_id)
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    f.handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&f.handle).await;

    let status = query_status(&f.handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build_timeout=60 with submitted_at 120s ago must time out on \
         first Tick after recovery (submitted_at must be restored, not \
         reset to now())"
    );
    assert!(
        status.error_summary.contains("build_timeout"),
        "error_summary should name build_timeout: {:?}",
        status.error_summary
    );
    Ok(())
}

/// A fresh standby's `LogBuffers` is empty after failover, and `set_exec`
/// otherwise runs only in `assign_to_worker`, which doesn't run for
/// already-assigned drvs — `collect_orphaned_assignments` deliberately
/// leaves still-connected workers' Assigned/Running drvs in place, and
/// those workers keep streaming logs to the new leader. Recovery must
/// re-stamp the ring buffer from `assignments.exec_id` so the flusher keys
/// the right S3 blob and `push_for` accepts the in-flight batches. (An
/// ex-leader re-acquiring the lease RETAINS its `LogBuffers` — that case
/// is `test_recovery_restamp_clears_stale_exec_lines` below.)
///
/// Cannot use `RecoveryFixture` — the phase-2 actor needs `log_buffers`
/// wired (the default plumbing leaves it `None`, making `set_log_exec`
/// a no-op). Replicates the fixture's two-phase shape inline.
///
/// Keep the phase-1 boundary (timeout, drop order, channel cleanup) in
/// sync with `RecoveryFixture::run_with_store` — it's the same shape.
#[tokio::test]
async fn test_recovery_repopulates_log_buffers_exec_id() -> TestResult {
    let exec_id = Uuid::now_v7();
    let drv_tag = "z-restamp-drv";
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: write state on the "old leader". ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(&handle, Uuid::new_v4(), drv_tag, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // The merged node is `ready` with no assignment. Backdate it to
    // an in-flight Assigned state with an active assignment carrying
    // the known exec_id — the shape recovery sees after a leader dies
    // mid-build with a still-connected worker streaming logs.
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = 'restamp-worker' \
         WHERE derivation_id = $1",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'restamp-worker', 1, 'pending', $2)",
    )
    .bind(drv_id)
    .bind(exec_id)
    .execute(&db.pool)
    .await?;

    // --- Phase 2: fresh actor recovers, with log_buffers wired so we
    // can observe the re-stamp. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        log_buffers.exec_id(&test_drv_path(drv_tag)),
        Some(exec_id),
        "recovery should re-stamp LogBuffers.exec_id for active assignments \
         from assignments.exec_id; got {:?}",
        log_buffers.exec_id(&test_drv_path(drv_tag)),
    );

    Ok(())
}

/// bug_004 (r9): recovery's restamp runs against an ex-leader's RETAINED
/// `LogBuffers` (`clear_persisted_state` keeps `log_buffers`), not an empty
/// one. If an interim leader re-dispatched the drv under a new exec_id while
/// this replica was a standby, the retained entry still holds the OLD
/// execution's lines — re-stamping it without clearing them hands those
/// lines to the periodic flusher under the NEW execution's `drv_logs` row
/// and `logs/{drv_hash}/{exec_id}.partial.log.zst` key, and `flush_final`
/// later bakes them into the permanent blob.
///
/// r[verify obs.log.exec-keyed]
#[tokio::test]
async fn test_recovery_restamp_clears_stale_exec_lines() -> TestResult {
    let old_exec = Uuid::now_v7();
    let new_exec = Uuid::now_v7();
    let drv_tag = "z-restamp-stale-drv";
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: seed PG with the INTERIM leader's view: the drv is
    // Assigned to a different worker under a fresh exec_id. ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(&handle, Uuid::new_v4(), drv_tag, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = 'interim-worker' \
         WHERE derivation_id = $1",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'interim-worker', 2, 'pending', $2)",
    )
    .bind(drv_id)
    .bind(new_exec)
    .execute(&db.pool)
    .await?;

    // --- Phase 2: the EX-LEADER re-acquires. Its LogBuffers was retained
    // across the flap (`clear_persisted_state` keeps `log_buffers`) and
    // still holds the OLD execution's entry + lines. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let drv_path = test_drv_path(drv_tag);
    log_buffers.set_exec(&drv_path, old_exec, "old-worker");
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"line from the abandoned execution".to_vec()],
                first_line_number: 0,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "fixture premise: the retained entry holds the old execution's lines"
    );

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The entry is re-stamped to the interim leader's execution...
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(new_exec),
        "recovery must re-stamp the retained entry from assignments.exec_id"
    );
    // ...WITHOUT carrying the old execution's lines into it. This is what
    // the periodic flusher would otherwise upload under
    // logs/{drv_hash}/{new_exec}.partial.log.zst and drv_logs[new_exec].
    assert_eq!(
        log_buffers.read_since(&drv_path, 0),
        Some(vec![]),
        "the abandoned execution's lines must not be attributed to the \
         re-issued exec_id"
    );
    // The binding stamp moved with the exec: the old worker's late batches
    // are rejected, the new worker's are accepted.
    assert!(
        !log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"late line from the old worker".to_vec()],
                first_line_number: 1,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "old worker's batches must be rejected after the restamp (executor_mismatch)"
    );
    Ok(())
}

/// bug_009 (r15): recovery's cross-exec restamp runs against an ex-leader's
/// retained entry that is still SEALED for the prior execution's pending
/// final (the request was deferred and retained by the flusher, and terminal
/// cleanup skipped the discard on the final-pending mark). The restamp must
/// clear that stale seal so the interim leader's re-dispatched execution —
/// whose worker streams to this process after re-acquisition — is not
/// silently muted by `push_for`'s seal check (which runs before the binding
/// gate and gates the recv task's gateway forward with it).
#[tokio::test]
async fn test_recovery_restamp_unseals_sealed_deferred_entry() -> TestResult {
    let old_exec = Uuid::now_v7();
    let new_exec = Uuid::now_v7();
    let drv_tag = "z-restamp-sealed-drv";
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: seed PG with the INTERIM leader's view: the drv is
    // Assigned to a different worker under a fresh exec_id. ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(&handle, Uuid::new_v4(), drv_tag, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = 'interim-worker' \
         WHERE derivation_id = $1",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'interim-worker', 2, 'pending', $2)",
    )
    .bind(drv_id)
    .bind(new_exec)
    .execute(&db.pool)
    .await?;

    // --- Phase 2: the EX-LEADER re-acquires. Its retained entry is in the
    // round-14/15 retention shape: lines + seal + final-pending mark (the
    // old execution's final was deferred and retained by the flusher, and
    // CleanupTerminalBuild skipped the discard on the mark). ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let drv_path = test_drv_path(drv_tag);
    log_buffers.set_exec(&drv_path, old_exec, "old-worker");
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"line from the abandoned execution".to_vec()],
                first_line_number: 0,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "fixture premise: the retained entry holds the old execution's lines"
    );
    log_buffers.seal(&drv_path);
    assert!(
        log_buffers.mark_final_pending(&drv_path, old_exec),
        "fixture premise: the old execution's final is pending with the flusher"
    );

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // The entry is re-stamped to the interim leader's execution...
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(new_exec),
        "recovery must re-stamp the retained entry from assignments.exec_id"
    );
    // ...and the prior execution's seal must not survive the restamp — a
    // surviving seal would silently drop every batch of the re-dispatched
    // execution.
    assert!(
        !log_buffers.is_sealed(&drv_path),
        "cross-exec restamp must clear the prior execution's seal"
    );
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"line from the re-dispatched execution".to_vec()],
                first_line_number: 0,
                executor_id: "interim-worker".into(),
            },
            "interim-worker",
        ),
        "the re-dispatched execution's worker must not be muted after the restamp"
    );
    // The binding gate still rejects the old worker's late batches, and the
    // prior execution's pending-final mark did not survive the restamp
    // either.
    assert!(
        !log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"late line from the old worker".to_vec()],
                first_line_number: 1,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "old worker's batches must be rejected after the restamp (executor_mismatch)"
    );
    assert!(
        !log_buffers.final_pending(&drv_path),
        "the prior execution's pending-final mark must not survive the restamp"
    );
    Ok(())
}

/// adopt_orphan_completion must route log finalization through
/// `terminal_log_epilogue` (seal → flush → correlate) instead of
/// discarding the recovery-stamped LogBuffers entry. The discard it
/// previously did (a) never enqueued a FlushRequest, so the ex-leader's
/// `.partial` `drv_logs` row stayed at `is_complete=false`/`status=NULL`
/// for the 30-day TTL even when the adopting process held the tail
/// needed to finalize it, and (b) on an ex-leader re-acquiring the
/// lease, dropped the retained unflushed tail of the execution whose
/// outputs were just adopted (see
/// `test_orphan_completion_preserves_ex_leader_log_tail` for that case —
/// this test covers the fresh-standby shape where the entry is empty and
/// the flush uploads nothing). The entry's removal moved from the actor
/// (synchronous discard) to the flusher (`drain_if_exec` on the queued
/// request); `upload_and_record`'s `line_count == 0` arm routes the
/// final drain to `finalize_empty_drain`: nothing is uploaded; any
/// `.partial` row the dead leader's periodic flusher wrote gets its
/// terminal metadata stamped and stays `is_complete=false` (its blob is
/// the execution's only stored content — the recovering process holds
/// no lines — so the incomplete indicator stays surfaced; column-level
/// claims live flusher-side in
/// `flush_final_empty_drain_stamps_status_but_stays_incomplete`).
/// The retained-vs-reaped contrast
/// for a request that could NOT be enqueued is
/// `test_orphan_completion_dropped_flush_discards_empty_buffer`.
///
/// Phase-1 reuses `seed_orphan_assigned` (proven by the other
/// `test_orphan_completion_*` tests) plus an `assignments.exec_id` INSERT
/// (the recovery carrier that `test_recovery_repopulates_log_buffers_exec_id`
/// established). Phase-2 wires `log_buffers` + a flush channel so the
/// epilogue's FlushRequest is observable, and an inproc store so reconcile
/// finds the outputs and routes to `adopt_orphan_completion` rather than
/// `reset_orphan_to_ready`.
///
/// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn test_orphan_completion_routes_log_through_epilogue() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let exec_id = Uuid::now_v7();
    let drv_tag = "orphan-discard-drv";
    let dead_worker = "dead-discard-w1";
    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("orphan-discard-out");
    put_test_path(&mut store_client, &out_path).await?;

    // --- Phase 1: write state on the "old leader". ---
    // seed_orphan_assigned merges a single-node build with
    // expected_output_paths set, then backdates to Assigned with
    // assigned_builder_id=dead_worker. Add an `assignments` row carrying
    // the known exec_id (the recovery carrier — `assignments.exec_id`),
    // matching the shape `load_nonterminal_derivations`'s LEFT JOIN reads.
    let _build_id = seed_orphan_assigned(&sched_db.pool, drv_tag, &out_path, dead_worker).await?;
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&sched_db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, $2, 1, 'pending', $3)",
    )
    .bind(drv_id)
    .bind(dead_worker)
    .bind(exec_id)
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: fresh actor recovers with log_buffers + store wired. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let (handle, _task) =
        setup_actor_configured(sched_db.pool.clone(), Some(store_client), |_, plumbing| {
            plumbing.log_buffers = Some(log_buffers.clone());
            plumbing.log_flush_tx = Some(flush_tx);
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Pre-condition: recovery re-stamped the buffer. Without this
    // assert, the test would pass trivially against a setup that never
    // created the entry (e.g. a future regression in load_dag_from_rows
    // that skipped set_log_exec for orphans).
    let drv_path = test_drv_path(drv_tag);
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(exec_id),
        "recovery should re-stamp the LogBuffers entry before reconcile"
    );
    assert!(
        log_buffers
            .read_since(&drv_path, 0)
            .is_some_and(|lines| lines.is_empty()),
        "stamped entry should exist (read_since=Some) but be empty"
    );

    // ReconcileAssignments: dead_worker not in self.executors → orphan
    // → FindMissingPaths finds out_path present → adopt_orphan_completion.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, drv_tag).await;
    assert_eq!(
        post.status,
        DerivationStatus::Completed,
        "orphan completion should transition drv to Completed"
    );

    // KEY ASSERTIONS: the orphan adoption must run the log-finalization
    // chokepoint, not hand-roll it. (1) A FlushRequest pinned to the
    // recovered execution is enqueued — flush_final's drain_if_exec is
    // what removes the entry (so GetDerivationLogs falls through to the
    // ex-leader's S3 .partial instead of serving an empty re-poll chunk)
    // and upload_and_record's line_count==0 arm routes the empty
    // fresh-standby drain to finalize_empty_drain (terminal-metadata
    // stamp on any .partial row the ex-leader's periodic flusher wrote;
    // stays is_complete=false; nothing uploaded). trigger_log_flush and
    // record_exec_correlation are both unconditionally inside the same
    // gated terminal_log_epilogue call, so receiving the request also
    // proves the bd.exec_id UPDATE was issued.
    let req = flush_rx.try_recv().expect(
        "adopt_orphan_completion must route log finalization through \
         terminal_log_epilogue: a FlushRequest pinned to the recovered \
         execution is enqueued and the flusher's drain_if_exec (not an \
         actor-side discard) reaps the entry. In this empty fresh-standby \
         shape the flush uploads nothing — the flusher-side handling of \
         the ex-leader's .partial row is \
         flush_final_empty_drain_stamps_status_but_stays_incomplete's \
         claim; the retained-tail shape is \
         test_orphan_completion_preserves_ex_leader_log_tail's claim.",
    );
    assert_eq!(req.exec_id, exec_id, "request pins the recovered execution");
    assert_eq!(req.drv_path, drv_path);
    assert_eq!(req.status.as_deref(), Some("succeeded"));
    // (2) Sealed: the worker never reconnects, but a late batch from a
    // half-open stream must not recreate the entry after the flusher
    // drains it.
    assert!(
        log_buffers.is_sealed(&drv_path),
        "orphan adoption must seal the buffer before the flush"
    );
    // (3) The entry is RETAINED for the flusher's drain — the actor must
    // not discard it out from under the queued request (drain_if_exec
    // would return None and the request would be dropped as
    // "no buffer to flush").
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(exec_id),
        "the stamped entry must survive until the flusher's drain_if_exec \
         consumes it; a synchronous discard races the queued FlushRequest"
    );

    Ok(())
}

/// The ex-leader half of `test_orphan_completion_routes_log_through_epilogue`:
/// a single-replica self-fence + re-acquire retains `LogBuffers`
/// (`clear_persisted_state` classes `log_buffers` as retained), and a drv
/// whose worker finished and died while the lease was lost still holds the
/// prior leadership's unflushed tail under the SAME exec_id (recovery's
/// same-exec restamp deliberately keeps the lines — that is the round-9
/// `set_exec` contract). Those lines are the log of the execution whose
/// outputs the scheduler is adopting as a success. `adopt_orphan_completion`
/// must hand them to the flusher, not discard them: the buffer must still
/// hold the lines when the FlushRequest is enqueued, so `drain_if_exec`
/// uploads them as the final `logs/{h}/{exec}.log.zst` blob and flips the
/// `drv_logs` row to `is_complete=true`.
///
/// r[verify sched.merge.exec-correlation+7]
/// r[verify obs.log.exec-keyed]
#[tokio::test]
async fn test_orphan_completion_preserves_ex_leader_log_tail() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let exec_id = Uuid::now_v7();
    let drv_tag = "orphan-tail-drv";
    let dead_worker = "dead-tail-w1";
    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("orphan-tail-out");
    put_test_path(&mut store_client, &out_path).await?;

    // --- Phase 1: PG state from the prior leadership. ---
    let _build_id = seed_orphan_assigned(&sched_db.pool, drv_tag, &out_path, dead_worker).await?;
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&sched_db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, $2, 1, 'pending', $3)",
    )
    .bind(drv_id)
    .bind(dead_worker)
    .bind(exec_id)
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: the EX-LEADER re-acquires. Its LogBuffers was retained
    // across the flap and still holds the dying worker's unflushed tail
    // for THIS execution. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let drv_path = test_drv_path(drv_tag);
    log_buffers.set_exec(&drv_path, exec_id, dead_worker);
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"unflushed tail line".to_vec()],
                first_line_number: 0,
                executor_id: dead_worker.into(),
            },
            dead_worker,
        ),
        "fixture premise: the retained entry holds the prior leadership's lines"
    );

    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let (handle, _task) =
        setup_actor_configured(sched_db.pool.clone(), Some(store_client), |_, plumbing| {
            plumbing.log_buffers = Some(log_buffers.clone());
            plumbing.log_flush_tx = Some(flush_tx);
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Precondition: the same-exec restamp retained the line (the round-9
    // set_exec contract). Without this assert a regression there would
    // make the final assertion vacuous.
    assert_eq!(
        log_buffers.read_since(&drv_path, 0),
        Some(vec![(0, b"unflushed tail line".to_vec())]),
        "same-exec recovery restamp must retain the in-flight execution's lines"
    );

    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, drv_tag).await;
    assert_eq!(post.status, DerivationStatus::Completed);

    // KEY ASSERTIONS: the tail survives to the flusher. The request is
    // pinned to the execution and the buffer still holds the line, so
    // flush_final's drain_if_exec(drv, exec_id) matches and uploads it
    // as the final blob.
    let req = flush_rx.try_recv().expect(
        "adopt_orphan_completion must enqueue a final FlushRequest for the \
         retained execution so its tail is uploaded and the .partial \
         drv_logs row is finalized",
    );
    assert_eq!(req.exec_id, exec_id);
    assert_eq!(req.status.as_deref(), Some("succeeded"));
    assert_eq!(
        log_buffers.read_since(&drv_path, 0),
        Some(vec![(0, b"unflushed tail line".to_vec())]),
        "the ex-leader's unflushed tail must survive to the flusher's \
         drain; discarding it loses the log of the execution whose \
         outputs were just adopted"
    );
    Ok(())
}

/// Dropped-FlushRequest sibling of
/// `test_orphan_completion_routes_log_through_epilogue`: when the flush
/// channel is full at adoption time (post-failover terminal burst), the
/// flusher will never `drain_if_exec` the recovery-stamped entry — the
/// actor must reap the zero-line entry itself (bug_008, round 11): no
/// other reaper remains before the build's CleanupTerminalBuild, and the
/// dead carrier would just sit in memory until then (reads are
/// unaffected — `GetDerivationLogs` probes the ex-leader's S3 `.partial`
/// for a zero-line entry whether or not it is reaped). The retained-tail
/// (non-empty) shape is NOT reaped — that is
/// `test_orphan_completion_preserves_ex_leader_log_tail`
/// plus the `discard_if_empty_removes_only_zero_line_entries` unit test.
/// Also asserts `bd.exec_id` directly: the discard runs before
/// `record_exec_correlation` and must not affect it.
///
/// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn test_orphan_completion_dropped_flush_discards_empty_buffer() -> TestResult {
    use super::integration::{put_test_path, setup_inproc_store};

    let exec_id = Uuid::now_v7();
    let drv_tag = "orphan-dropflush-drv";
    let dead_worker = "dead-dropflush-w1";
    let sched_db = TestDb::new(&MIGRATOR).await;
    let store_db = TestDb::new(&MIGRATOR).await;
    let (mut store_client, _store_srv) = setup_inproc_store(store_db.pool.clone()).await?;
    let out_path = test_store_path("orphan-dropflush-out");
    put_test_path(&mut store_client, &out_path).await?;

    // --- Phase 1: ex-leader's PG state (same shape as the sibling test). ---
    let build_id = seed_orphan_assigned(&sched_db.pool, drv_tag, &out_path, dead_worker).await?;
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&sched_db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, $2, 1, 'pending', $3)",
    )
    .bind(drv_id)
    .bind(dead_worker)
    .bind(exec_id)
    .execute(&sched_db.pool)
    .await?;

    // --- Phase 2: fresh standby with a flush channel that is ALREADY full. ---
    // Capacity 1, pre-filled with a dummy request: the epilogue's try_send
    // must hit TrySendError::Full, which is the bug's trigger.
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(1);
    let dummy_drv = "/nix/store/zzzzzzzzzzzzzzzzzzzzzzzzzzzzzzzz-dummy.drv".to_string();
    flush_tx
        .try_send(crate::logs::FlushRequest {
            drv_path: dummy_drv.clone(),
            exec_id: Uuid::now_v7(),
            status: None,
            lease_generation: 1,
        })
        .expect("pre-fill the only slot");
    let (handle, _task) =
        setup_actor_configured(sched_db.pool.clone(), Some(store_client), |_, plumbing| {
            plumbing.log_buffers = Some(log_buffers.clone());
            plumbing.log_flush_tx = Some(flush_tx);
        });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Pre-condition (non-vacuous): recovery created the empty stamped entry.
    let drv_path = test_drv_path(drv_tag);
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(exec_id),
        "recovery should re-stamp the LogBuffers entry before reconcile"
    );
    assert!(
        log_buffers
            .read_since(&drv_path, 0)
            .is_some_and(|lines| lines.is_empty()),
        "stamped entry should exist (read_since=Some) and be empty"
    );

    // Reconcile: dead worker → orphan → outputs present → adopt_orphan_completion.
    handle
        .send_unchecked(ActorCommand::ReconcileAssignments)
        .await?;
    barrier(&handle).await;

    let post = expect_drv(&handle, drv_tag).await;
    assert_eq!(post.status, DerivationStatus::Completed);

    // Premise check: the epilogue's request really was dropped — the only
    // message in the channel is the pre-filled dummy, and nothing else
    // ever landed.
    let only = flush_rx
        .try_recv()
        .expect("the pre-filled dummy is still there");
    assert_eq!(
        only.drv_path, dummy_drv,
        "epilogue's request must not have replaced the dummy"
    );
    assert!(
        flush_rx.try_recv().is_err(),
        "exactly one message total: the adoption's FlushRequest was dropped (channel full)"
    );

    // KEY ASSERTION (the fix): the empty recovery-stamped entry is gone
    // instead of lingering as a dead carrier until the whole build's
    // CleanupTerminalBuild. (When bug_008 was fixed this reap was also what
    // let reads reach the ex-leader's `.partial`; GetDerivationLogs now
    // probes the stored side for a zero-line entry itself, so the reap is
    // bookkeeping and reads are unaffected either way.)
    assert!(
        log_buffers.read_since(&drv_path, 0).is_none(),
        "dropped FlushRequest must not leave a dead empty entry behind"
    );
    assert!(
        log_buffers.exec_id(&drv_path).is_none(),
        "pin gate now falls through to S3 for the pinned fetch too"
    );
    assert!(
        !log_buffers.is_sealed(&drv_path),
        "reap also clears the seal tombstone"
    );

    // The discard runs before record_exec_correlation and must not disturb
    // it: the interested build still gets bd.exec_id = E even though the
    // FlushRequest was dropped. Spawned write — poll PG (established
    // 10ms × 100 pattern).
    let mut bd_exec: Option<Uuid> = None;
    for _ in 0..100 {
        bd_exec = sqlx::query_scalar(
            "SELECT exec_id FROM build_derivations \
             WHERE build_id = $1 AND derivation_id = $2",
        )
        .bind(build_id)
        .bind(drv_id)
        .fetch_one(&sched_db.pool)
        .await?;
        if bd_exec.is_some() {
            break;
        }
        tokio::time::sleep(Duration::from_millis(10)).await;
    }
    assert_eq!(
        bd_exec,
        Some(exec_id),
        "correlation unaffected by the discard"
    );

    Ok(())
}

/// A derivation that went through `reset_to_ready()` (worker disconnect,
/// phantom drain, infra/timeout retry below cap) leaves its `assignments`
/// row open at `pending` — `terminal_assignment_status(Ready)` is `None`,
/// so `persist_status(Ready, None)` nulls `derivations.assigned_builder_id`
/// without closing the assignment. Recovery's LEFT JOIN must NOT carry that
/// leaked row's `exec_id` back into `state.exec_id`: `reset_to_ready()`
/// cleared it, and re-stamping it on the new leader makes
/// `exec_id_for_terminal` short-circuit on a carrier with no LogBuffers
/// entry behind it. A cancel before re-dispatch then fires
/// `terminal_log_epilogue` for a drv this leader never dispatched — durably
/// writing `bd.exec_id` to an execution that may have no `drv_logs` row
/// (dashboard pins to it and gets NotFound instead of the "approximate"
/// fallback) and queueing a FlushRequest that flush_final's staleness guard
/// drops.
///
/// r[verify sched.merge.exec-correlation+7]
#[tokio::test]
async fn test_recovery_preserves_reset_exec_id_clear() -> TestResult {
    let stale_exec_id = Uuid::now_v7();
    let drv_tag = "z-reset-noexec-drv";
    let build_id = Uuid::new_v4();
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: post-reset PG shape on the "old leader". ---
    // merge_single_node leaves the drv `ready` with NULL
    // assigned_builder_id; the leaked `pending` assignments row carrying
    // the dead execution's exec_id is what reset_to_ready()'s persist
    // leaves behind (terminal_assignment_status(Ready) == None never
    // closes it).
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(&handle, build_id, drv_tag, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    let (drv_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
            .bind(drv_tag)
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'gone-worker', 1, 'pending', $2)",
    )
    .bind(drv_id)
    .bind(stale_exec_id)
    .execute(&db.pool)
    .await?;

    // --- Phase 2: fresh leader recovers with log_buffers + flusher wired. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
        plumbing.log_flush_tx = Some(flush_tx);
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // (A) The recovered state honors reset_to_ready()'s clear. The leaked
    // pending assignment is not a live execution. THE load-bearing red
    // assertion — stays red regardless of merged_bug_012's landing order.
    let info = expect_drv(&handle, drv_tag).await;
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "fixture premise: recovered as Ready"
    );
    assert_eq!(
        info.exec_id, None,
        "recovery must not re-stamp state.exec_id from a leaked 'pending' \
         assignments row after reset_to_ready() cleared it"
    );
    // (B) No LogBuffers restamp either (already enforced by the restamp
    // gate's own Assigned|Running filter — pinned here so the two
    // consumers of row.exec_id can't diverge again).
    assert_eq!(
        log_buffers.exec_id(&test_drv_path(drv_tag)),
        None,
        "no LogBuffers restamp for a drv with no live assignment"
    );

    // --- Cancel before re-dispatch: the harm path. The sole-interest
    // Ready drv lands in to_depfail; its exec_id_for_terminal gate must
    // not fire (no execution to finalize on this leader). ---
    let (tx, rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::CancelBuild {
            build_id,
            caller_tenant: None,
            reason: "test cancel".into(),
            reply: tx,
        })
        .await?;
    let _ = rx.await??;
    barrier(&handle).await;

    // (C) No FlushRequest — there is no buffer behind the leaked exec_id;
    // flush_final would drop the request on its staleness guard and the
    // enqueue would be pure waste.
    assert!(
        flush_rx.try_recv().is_err(),
        "cancelling a recovered, never-redispatched Ready drv must not \
         queue a FlushRequest"
    );
    // (D) No seal tombstone (terminal_log_epilogue's first step).
    assert!(
        !log_buffers.is_sealed(&test_drv_path(drv_tag)),
        "no seal tombstone for an execution this leader never observed"
    );
    // (E) bd.exec_id stays NULL → the dashboard renders the documented
    // "approximate / latest available" fallback instead of pinning to an
    // exec that may have no drv_logs row. (record_exec_correlation's
    // UPDATE is spawned, so this is a belt-and-suspenders check on top of
    // the synchronous (C)/(D) — if the gate had fired, (C) catches it
    // deterministically.)
    let bd_exec: Option<Uuid> = sqlx::query_scalar(
        "SELECT exec_id FROM build_derivations \
         WHERE build_id = $1 AND derivation_id = $2",
    )
    .bind(build_id)
    .bind(drv_id)
    .fetch_one(&db.pool)
    .await?;
    assert_eq!(
        bd_exec, None,
        "bd.exec_id must stay NULL for a terminal reached without dispatch"
    );

    Ok(())
}

/// bug_012 (r11) + bug_002 (r12): acquisition-time reconciliation of an
/// ex-leader's retained `LogBuffers` is three-part — the pre-load prefix
/// re-arm clears per-tenure stored-coverage bookkeeping on every retained
/// entry (bug_001 r13, tested separately); the restamp loop covers
/// PG-`Assigned|Running` drvs; the post-load sweep covers everything else.
/// A retained entry for a drv that went terminal under an interim leader is
/// either not loaded at all (most terminal statuses) or loaded only as a
/// Poisoned TTL-tracking node (r12); without the sweep it shadows the
/// execution's stored log in GetDerivationLogs (ring buffer is probed before
/// S3) and flush_periodic re-uploads its `.partial` every 30s — forever, or
/// until the 24h poison TTL. The survival criterion is non-terminal
/// membership in the rebuilt DAG: Assigned (reconnect window) and
/// Ready-with-retained-buffer (cancel-sweep finalization reads that stamp)
/// survive; absent and Poisoned do not.
///
/// r[verify sched.recovery.log-buffer-sweep+2]
#[tokio::test]
async fn test_recovery_sweeps_stale_log_buffers() -> TestResult {
    let exec_keep = Uuid::now_v7();
    let exec_ready = Uuid::now_v7();
    let exec_gone = Uuid::now_v7();
    let exec_poisoned = Uuid::now_v7();
    let db = TestDb::new(&MIGRATOR).await;

    // Distinct store-path hashes so the four drvs occupy distinct
    // LogBuffers keys (drv_log_hash keys on the path's hash part, and every
    // test_drv_path() shares TEST_HASH).
    let keep_path = test_drv_path("sweep-keep");
    let ready_path = format!("/nix/store/{}-sweep-ready.drv", "b".repeat(32));
    let gone_path = format!("/nix/store/{}-sweep-gone.drv", "c".repeat(32));
    let poisoned_path = format!("/nix/store/{}-sweep-poisoned.drv", "d".repeat(32));

    // --- Phase 1: PG state left by the prior leaderships. ---
    let gone_build = Uuid::new_v4();
    let poisoned_build = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(
            &handle,
            Uuid::new_v4(),
            "sweep-keep",
            PriorityClass::Scheduled,
        )
        .await?;
        merge_single_node(
            &handle,
            Uuid::new_v4(),
            "sweep-ready",
            PriorityClass::Scheduled,
        )
        .await?;
        merge_single_node(&handle, gone_build, "sweep-gone", PriorityClass::Scheduled).await?;
        merge_single_node(
            &handle,
            poisoned_build,
            "sweep-poisoned",
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    // keep: in-flight Assigned with a live assignment → restamp covers it.
    let (keep_id,): (Uuid,) =
        sqlx::query_as("SELECT derivation_id FROM derivations WHERE drv_hash = 'sweep-keep'")
            .fetch_one(&db.pool)
            .await?;
    sqlx::query(
        "UPDATE derivations SET status = 'assigned', assigned_builder_id = 'w-keep' \
         WHERE derivation_id = $1",
    )
    .bind(keep_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status, exec_id) \
         VALUES ($1, 'w-keep', 1, 'pending', $2)",
    )
    .bind(keep_id)
    .bind(exec_keep)
    .execute(&db.pool)
    .await?;
    // ready: stays `ready` (post-reset shape), distinct drv_path.
    sqlx::query("UPDATE derivations SET drv_path = $1 WHERE drv_hash = 'sweep-ready'")
        .bind(&ready_path)
        .execute(&db.pool)
        .await?;
    // gone: went terminal under the interim leader; its build finished.
    sqlx::query(
        "UPDATE derivations SET status = 'completed', drv_path = $1 \
         WHERE drv_hash = 'sweep-gone'",
    )
    .bind(&gone_path)
    .execute(&db.pool)
    .await?;
    sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
        .bind(gone_build)
        .execute(&db.pool)
        .await?;
    // poisoned: poisoned under the interim leader (which finalized E1's
    // drv_logs row at poison time); loaded into the rebuilt DAG for TTL
    // tracking only. poisoned_at is future-dated so the cfg(test) 100ms
    // POISON_TTL can never classify it expired-at-load — that path would
    // clear+skip the row, the drv would be absent from the DAG, and the
    // entry would be swept by the absent-from-DAG criterion, making this
    // case degenerate into case (3). from_poisoned_row clamps the negative
    // elapsed to 0, so TTL tracking still starts fresh.
    sqlx::query(
        "UPDATE derivations SET status = 'poisoned', \
         poisoned_at = now() + interval '1 hour', drv_path = $1 \
         WHERE drv_hash = 'sweep-poisoned'",
    )
    .bind(&poisoned_path)
    .execute(&db.pool)
    .await?;
    sqlx::query("UPDATE builds SET status = 'failed' WHERE build_id = $1")
        .bind(poisoned_build)
        .execute(&db.pool)
        .await?;

    // --- Phase 2: the EX-LEADER re-acquires; its retained LogBuffers holds
    // pre-flap lines for all four executions. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    for (path, exec, worker) in [
        (&keep_path, exec_keep, "w-keep"),
        (&ready_path, exec_ready, "w-ready"),
        (&gone_path, exec_gone, "w-gone"),
        (&poisoned_path, exec_poisoned, "w-poisoned"),
    ] {
        log_buffers.set_exec(path, exec, worker);
        assert!(
            log_buffers.push_for(
                path,
                &rio_proto::types::BuildLogBatch {
                    derivation_path: path.clone(),
                    lines: vec![b"retained pre-flap line".to_vec()],
                    first_line_number: 0,
                    executor_id: worker.to_string(),
                },
                worker,
            ),
            "fixture premise: retained entry for {path} holds pre-flap lines"
        );
    }

    let (flush_tx, mut flush_rx) = tokio::sync::mpsc::channel(8);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
        plumbing.log_flush_tx = Some(flush_tx);
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Fixture premise: ready recovered as Ready (in the DAG, not restamped).
    // No status assert for sweep-keep: the cfg(test) 100ms ReconcileAssignments
    // may reset the orphaned Assigned drv to Ready at any point after
    // acquisition; the restamp assertion below proves the Assigned-recovery
    // path ran, and the buffer assertions are insensitive to that reset.
    assert_eq!(
        expect_drv(&handle, "sweep-ready").await.status,
        DerivationStatus::Ready
    );

    // (1) Assigned drv: restamped, lines retained (reconnect window).
    assert_eq!(log_buffers.exec_id(&keep_path), Some(exec_keep));
    assert_eq!(
        log_buffers.read_since(&keep_path, 0).map(|l| l.len()),
        Some(1),
        "Assigned|Running entries must survive acquisition"
    );
    // (2) Ready drv with a retained prior-exec buffer: kept — non-terminal
    // DAG membership, not restamp coverage, is the survival criterion
    // (cancel-sweep finalization reads this stamp).
    assert_eq!(log_buffers.exec_id(&ready_path), Some(exec_ready));
    assert_eq!(
        log_buffers.read_since(&ready_path, 0).map(|l| l.len()),
        Some(1),
        "entries for drvs the rebuilt DAG tracks in a non-terminal state must not be swept"
    );
    // (3) Terminal-under-interim-leader drv (absent from the rebuilt DAG):
    // swept. read_since == None means GetDerivationLogs falls through to S3
    // (the interim leader's stored log) instead of serving stale pre-flap
    // lines, and flush_periodic has no entry left to re-upload every 30s.
    assert_eq!(
        log_buffers.exec_id(&gone_path),
        None,
        "retained entry for a drv not in the rebuilt DAG must be discarded"
    );
    assert!(
        log_buffers.read_since(&gone_path, 0).is_none(),
        "stale pre-flap lines must not shadow the interim leader's stored log"
    );
    // (4) Poisoned-under-interim-leader drv: IS loaded into the rebuilt DAG
    // (TTL tracking) — this guard keeps the case from degenerating into (3)
    // via the expired-at-load path — but its retained entry is swept anyway:
    // the exec was finalized by whichever leader poisoned it, a poisoned drv
    // is never re-dispatched, and the only other discard is the 24h poison
    // TTL. Without the terminal filter the stale entry would shadow the
    // stored failure log in GetDerivationLogs and re-upload its .partial
    // every 30s until then.
    assert_eq!(
        expect_drv(&handle, "sweep-poisoned").await.status,
        DerivationStatus::Poisoned,
        "fixture premise: poisoned drv must be loaded into the rebuilt DAG"
    );
    assert_eq!(
        log_buffers.exec_id(&poisoned_path),
        None,
        "retained entry for a poisoned (terminal) drv must be discarded at acquisition"
    );
    assert!(
        log_buffers.read_since(&poisoned_path, 0).is_none(),
        "stale pre-flap lines must not shadow the poisoned exec's stored failure log"
    );
    // (5) The sweep is a discard, not a finalization: no FlushRequest.
    assert!(
        flush_rx.try_recv().is_err(),
        "acquisition-time sweep must not enqueue flush requests"
    );

    Ok(())
}

/// bug_001 (r13): the acquisition-time re-arm must cover retained entries
/// the restamp loop does NOT touch. Across an A→B→A flap where the worker
/// migrated to the interim leader and disconnected before re-dispatch, the
/// drv recovers as Ready (reset_to_ready under B) — the sweep deliberately
/// spares the retained entry (the cancel-sweep finalization needs its
/// stamp), but the prefix_checked=true latch from A's previous tenure must
/// not survive the flap: B may have extended the stored .partial past what
/// A's ring holds, and a trusted stale latch makes the flusher skip
/// reconcile_stored_prefix — overwriting that durable coverage on the next
/// periodic tick, or freezing a truncated .log.zst as complete and
/// deleting the .partial on the cancel-sweep final.
///
/// r[verify obs.log.stored-coverage-preserved]
#[tokio::test]
async fn test_recovery_rearms_prefix_state_for_spared_ready_entry() -> TestResult {
    use crate::logs::PrefixState;

    let exec_id = Uuid::now_v7();
    let drv_tag = "rearm-ready-drv";
    let db = TestDb::new(&MIGRATOR).await;

    // --- Phase 1: seed PG. merge_single_node leaves the drv `ready` with
    // no assignment — the post-reset shape an interim leader leaves after
    // the worker disconnects from it and before any re-dispatch. ---
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_single_node(&handle, Uuid::new_v4(), drv_tag, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }

    // --- Phase 2: the EX-LEADER re-acquires. Its retained LogBuffers
    // still holds the execution's entry, lines, and — crucially — the
    // prefix latch its flusher set during the previous tenure. ---
    let log_buffers = std::sync::Arc::new(crate::logs::LogBuffers::new());
    let drv_path = test_drv_path(drv_tag);
    log_buffers.set_exec(&drv_path, exec_id, "old-worker");
    assert!(
        log_buffers.push_for(
            &drv_path,
            &rio_proto::types::BuildLogBatch {
                derivation_path: drv_path.clone(),
                lines: vec![b"retained pre-flap line".to_vec()],
                first_line_number: 0,
                executor_id: "old-worker".into(),
            },
            "old-worker",
        ),
        "fixture premise: retained entry holds the previous tenure's lines"
    );
    log_buffers.mark_prefix_checked(&drv_path, exec_id);
    assert!(
        matches!(
            log_buffers.prefix_state(&drv_path, exec_id),
            PrefixState::Checked
        ),
        "fixture premise: previous tenure latched the stored-coverage check"
    );

    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |_, plumbing| {
        plumbing.log_buffers = Some(log_buffers.clone());
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    // Fixture premise: recovered as Ready ⇒ the restamp loop did not run
    // for it and the sweep spared its entry.
    assert_eq!(
        expect_drv(&handle, drv_tag).await.status,
        DerivationStatus::Ready,
        "fixture premise: drv recovers as Ready (not restamped, not swept)"
    );
    assert_eq!(
        log_buffers.exec_id(&drv_path),
        Some(exec_id),
        "spared entry must keep its exec stamp (cancel-sweep finalization reads it)"
    );
    assert_eq!(
        log_buffers.read_since(&drv_path, 0).map(|l| l.len()),
        Some(1),
        "spared entry must keep its lines"
    );
    // The fix: the previous tenure's latch must NOT survive re-acquisition
    // — the flusher re-consults the stored drv_logs row before its next
    // flush of this execution instead of overwriting what an interim
    // leader stored.
    assert!(
        matches!(
            log_buffers.prefix_state(&drv_path, exec_id),
            PrefixState::Unchecked
        ),
        "prefix bookkeeping must be re-armed at acquisition for entries the restamp loop does not cover"
    );

    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// `topdown_pruned` must survive leader failover: a derivations row
/// persisted with the flag set is restored with the flag set (not
/// reset to false) so the new leader keeps honoring the "must complete
/// via substitution; building is invalid" invariant for childless
/// pruned roots.
#[tokio::test]
async fn test_recovery_restores_topdown_pruned_flag() -> TestResult {
    let f = RecoveryFixture::run(async |handle, pool| {
        // A plain single-node merge stages the build / link /
        // derivation rows; the prune itself isn't needed to exercise
        // the restore (merge-time persistence has its own test).
        merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("tdrec-drv")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate to the post-prune persisted shape: mid-substitution
        // (the detached fetch dies with the old leader) and pruned.
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = 'tdrec-drv'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    let d = expect_drv(&f.handle, "tdrec-drv").await;
    assert!(
        d.topdown_pruned,
        "recovery must restore topdown_pruned from PG, not reset it to false"
    );
    // The spawned fetch died with the old leader, so the node re-enters
    // the normal flow (childless ⇒ Ready) — only the flag must survive.
    assert_eq!(d.status, DerivationStatus::Ready);
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Failover regression (the doomed dispatch): a roots-only-pruned root
/// persisted as `substituting` is recovered CHILDLESS by the new
/// leader, comes back Ready (no deps in the DAG), and is re-probed
/// against the stored wanted union — routinely WIDER than the
/// prune-time criterion (`'{}'` = all declared). When that wider set
/// contains an output that is genuinely missing and not substitutable,
/// the dispatch-time probes can neither complete the node inline nor
/// route it to substitution; pre-fix it was left Ready and dispatched
/// from source with no input-presence check — a doomed dispatch (its
/// inputDrvs were never merged), worker ENOENT, eventual wrong-reason
/// Poisoned, every interested build failed.
///
/// Post-fix: the persisted `topdown_pruned` flag is restored at
/// recovery and the dispatch-time guard takes the same fail-fast arm
/// as `SubstituteComplete{ok=false}` — no WorkAssignment is ever sent
/// for the node and the interested build terminates with the
/// resubmit-directing error.
///
/// Staged shape (phase 1): an Active build, its build_derivations
/// link, a derivations row in status `substituting` with declared
/// outputs `[out, debug]`, stored wanted `'{}'` (= all declared),
/// expected paths set, NO edges, and `topdown_pruned = true` seeded by
/// direct UPDATE — the same shape a pruned merge persists; seeding it
/// manually keeps the staging independent of the merge-time
/// persistence path (which has its own test) and of the spawned-fetch
/// race. Phase 2's store has `out` substitutable and `debug` missing /
/// not substitutable.
#[tokio::test]
async fn test_failover_childless_pruned_root_fails_fast_not_dispatched_from_source() -> TestResult {
    let out = test_store_path("fov-root-out");
    let dbg = test_store_path("fov-root-debug");

    // Phase-2 store: `out` substitutable upstream; `debug` missing and
    // NOT substitutable — the recovered (wider) all-declared wanted set
    // is unsatisfiable even though the prune-time criterion was met.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        // Active build + build_derivations link + derivation row:
        // declared [out, debug], wanted '{}' (= all declared), expected
        // paths set, no edges.
        let mut node = make_node("fov-root");
        node.output_names = vec!["out".into(), "debug".into()];
        node.expected_output_paths = vec![out.clone(), dbg.clone()];
        node.wanted_output_names = vec![];
        merge_dag(&handle, build_id, vec![node], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate to the post-prune persisted shape (see doc comment).
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = 'fov-root'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // A builder is available — the doomed from-source dispatch has
    // somewhere to go if the restore+guard are missing.
    let mut worker_rx = connect_executor(&handle, "fov-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // No WorkAssignment is ever sent for the dep-less node.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "childless topdown-pruned root must never be dispatched from source \
             after failover (its inputDrvs were never merged — worker would ENOENT)"
        );
    }
    let d = expect_drv(&handle, "fov-root").await;
    assert!(
        !matches!(
            d.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "recovered childless pruned root must not be left dispatchable from \
         source; got {:?}",
        d.status
    );
    // The interested build terminates with the resubmit-directing error
    // (same assertions as the SubstituteComplete{ok=false} fail-fast tests).
    let s = query_status(&handle, build_id).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "interested build must fail fast (resubmit re-probes or full-merges); \
         got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s.error_summary
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The fail-fast must CONSUME the `topdown_pruned` marker (clear it in
/// memory and in PG) when it parks a node: the flag can be stale (a
/// committed merge whose activation failed, a node stamped while its
/// children were invisible to a recovered DAG, a genuinely pruned leaf
/// that no children-adding merge ever clears), and a stale persisted
/// flag re-arms the fail-fast after EVERY failover — wrongfully
/// terminal-failing builds for a node that could build from source.
///
/// Chain: failover #1 fires the fail-fast for build1 (pre-staged
/// pruned shape, `debug` unsatisfiable) → the marker must now be false
/// in memory and in PG → build2 resubmits the same drv (fresh
/// single-node merge) → failover #2 recovers it childless again → the
/// node must NOT be fail-fasted a second time: build2 stays alive and
/// the node dispatches from source to the connected worker.
#[tokio::test]
async fn test_fail_fast_clears_topdown_pruned_and_resubmission_builds_from_source() -> TestResult {
    let out = test_store_path("ffc-root-out");
    let dbg = test_store_path("ffc-root-debug");
    let mk_node = || {
        let mut n = make_node("ffc-root");
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out.clone(), dbg.clone()];
        n.wanted_output_names = vec![];
        n
    };

    let db = TestDb::new(&MIGRATOR).await;
    // `out` substitutable upstream; `debug` missing and not
    // substitutable for the whole test — the all-declared wanted union
    // is never satisfiable by substitution, so the node is exactly the
    // "could build from source" case the stale flag would wrongly kill.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    // Phase 1: stage the post-prune persisted shape for build1.
    let build1 = Uuid::new_v4();
    {
        let (handle, task) = setup_actor(db.pool.clone());
        merge_dag(&handle, build1, vec![mk_node()], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        let _ = tokio::time::timeout(Duration::from_secs(5), task).await;
    }
    sqlx::query(
        "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
         WHERE drv_hash = 'ffc-root'",
    )
    .execute(&db.pool)
    .await?;

    // Phase 2 (failover #1): the dispatch-time probe finds `debug`
    // definitively missing → fail-fast fires for build1.
    let (handle2, task2) = setup_actor_with_store(db.pool.clone(), Some(store_client.clone()));
    handle2.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle2).await;
    tick(&handle2).await?;
    assert_eq!(
        query_status(&handle2, build1).await?.state,
        rio_proto::types::BuildState::Failed as i32,
        "fixture premise: failover #1 fail-fasts build1"
    );
    // The marker is consumed by the fail-fast — in memory and in PG.
    assert!(
        !expect_drv(&handle2, "ffc-root").await.topdown_pruned,
        "fail-fast must clear the in-memory topdown_pruned marker when it parks the node"
    );
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'ffc-root'")
            .fetch_one(&db.pool)
            .await?;
    assert!(
        !pg_pruned,
        "fail-fast must clear the persisted topdown_pruned marker (a stale row \
         re-arms the fail-fast after every failover)"
    );

    // build2 resubmits the same drv (fresh single-node merge).
    let build2 = Uuid::new_v4();
    merge_dag(&handle2, build2, vec![mk_node()], vec![], false).await?;
    barrier(&handle2).await;
    drop(handle2);
    let _ = tokio::time::timeout(Duration::from_secs(5), task2).await;

    // Phase 3 (failover #2): the node recovers childless again. It must
    // NOT be fail-fasted a second time — build2 must survive and the
    // node must dispatch from source.
    let (handle3, _task3) = setup_actor_with_store(db.pool.clone(), Some(store_client));
    handle3.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle3).await;
    let mut rx = connect_executor(&handle3, "ffc-w", "x86_64-linux").await?;
    tick(&handle3).await?;

    let s2 = query_status(&handle3, build2).await?;
    assert_ne!(
        s2.state,
        rio_proto::types::BuildState::Failed as i32,
        "resubmitted build must not be fail-fasted again after the next failover \
         (stale topdown_pruned re-armed the guard); error={:?}",
        s2.error_summary
    );
    let a = recv_assignment(&mut rx).await;
    assert_eq!(
        a.drv_path,
        test_drv_path("ffc-root"),
        "the resubmitted node builds from source (its closure was never pruned \
         for build2)"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Failover counterpart of the children-became-produced clear: a
/// restored `topdown_pruned` mark must be DROPPED at recovery when the
/// node's persisted children are all produced (`completed`/`skipped`)
/// and vouched for by a still-live build. This test stages the
/// live-vouched side of that gate; the keep side — produced children
/// linked only to terminal builds — is pinned by
/// `test_failover_keeps_topdown_pruned_when_produced_children_belong_to_terminal_build`
/// below.
///
/// The recovered in-memory DAG cannot tell such a parent apart from a
/// genuine childless pruned root — produced children are filtered out
/// of `load_nonterminal_derivations` and their edges are dropped — so
/// the gate must consult PG (`derivation_edges` JOIN `derivations`).
/// Without it, the stale mark survives into the new leader and the
/// first dispatch pass takes the fail-fast arm for a node whose
/// closure IS produced, wrongly terminal-failing a healthy build that
/// would have dispatched from source. The same gate also absorbs a
/// PG-true/memory-false skew left by a lost best-effort clear under
/// the previous leader.
///
/// Staged shape (phase 1): full merge parent→child WITH the edge
/// persisted to `derivation_edges`, then backdate the child to
/// `completed` and the parent to `substituting` + `topdown_pruned =
/// true`. Phase 2's store has `out` substitutable and `debug` missing
/// / not substitutable — exactly the shape where a surviving flag
/// would fire the wrongful fail-fast instead of the from-source
/// dispatch asserted below.
#[tokio::test]
async fn test_failover_clears_topdown_pruned_when_children_all_produced() -> TestResult {
    let out = test_store_path("tdcp-root-out");
    let dbg = test_store_path("tdcp-root-debug");

    // Phase-2 store: `out` substitutable upstream; `debug` missing and
    // NOT substitutable — the recovered all-declared wanted set can
    // neither complete inline nor route to substitution, so the node
    // must go to a from-source dispatch (valid: its children are
    // produced) unless a stale flag wrongly fail-fasts it.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        // Full merge: the parent depends on the child and the edge IS
        // persisted — this is what distinguishes the parent from the
        // genuine childless pruned roots staged by the two tests above.
        let mut parent = make_node("tdcp-root");
        parent.output_names = vec!["out".into(), "debug".into()];
        parent.expected_output_paths = vec![out.clone(), dbg.clone()];
        parent.wanted_output_names = vec![];
        merge_dag(
            &handle,
            build_id,
            vec![parent, make_node("tdcp-dep")],
            vec![make_test_edge("tdcp-root", "tdcp-dep")],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate: the child finished under the old leader (produced),
        // while the parent is left mid-substitution with the mark still
        // set — the persisted shape a crash before the completion-time
        // clear (or a lost best-effort PG clear) leaves behind.
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'tdcp-dep'")
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = 'tdcp-root'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // A builder is available — the parent should dispatch to it from
    // source once the stale mark is dropped.
    let mut rx = connect_executor(&handle, "tdcp-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // Not the wrongful fail-fast: the build stays alive...
    let s = query_status(&handle, build_id).await?;
    assert_ne!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "a parent whose persisted children are all produced must not be \
         fail-fasted by a stale restored topdown_pruned mark; error={:?}",
        s.error_summary
    );
    // ...and the parent dispatches from source (`debug` is missing and
    // not substitutable, so source is the only way to produce it).
    let a = recv_assignment(&mut rx).await;
    assert_eq!(
        a.drv_path,
        test_drv_path("tdcp-root"),
        "parent with an all-produced persisted closure must dispatch from \
         source after failover"
    );
    // The mark was dropped at recovery, in memory...
    assert!(
        !expect_drv(&handle, "tdcp-root").await.topdown_pruned,
        "recovery must drop the restored topdown_pruned mark when the \
         persisted children are all produced"
    );
    // ...and (best-effort) in PG, so later failovers don't re-evaluate
    // the same stale row.
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'tdcp-root'")
            .fetch_one(&f.db.pool)
            .await?;
    assert!(
        !pg_pruned,
        "the recovery-time drop must be persisted so the stale mark does not \
         re-arm on every subsequent failover"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// Live-build scoping of the recovery-time gate: a restored
/// `topdown_pruned` mark must be KEPT when the parent's produced
/// children are vouched for only by TERMINAL builds.
///
/// The previous-generation shape: build B0 fully merged parent→child
/// long ago and went `succeeded` — its `derivation_edges` row, the
/// child's `completed` row, and B0's `build_derivations` links persist
/// in PG indefinitely (terminal builds are never deleted), while the
/// store may have GC'd the actual outputs since. A later build B1
/// re-requests the parent and the topdown prune fires: B1 links only
/// the parent, merges no edges, and the post-prune persisted shape is
/// `substituting` + `topdown_pruned = true`. On failover the gate sees
/// the historical child row; if it trusted bare `completed` it would
/// clear the restored mark (in memory and best-effort PG) and the
/// parent would dispatch from source against a closure that was never
/// merged for B1 — the doomed dispatch the mark exists to prevent
/// (pre-fix red of this test: an Assignment reaches the worker and B1
/// stays Active). With the gate scoped to live builds the stale
/// evidence is ignored and the node takes the bounded fail-fast arm
/// instead: no WorkAssignment is ever sent and B1 fails with the
/// resubmit-directing error — the mirror of the childless fail-fast
/// test above, with the historical edge present.
///
/// Why the assertions are behavioral rather than "the mark is still
/// set": the LeaderAcquired arm runs an immediate dispatch pass after
/// recovery, so the (correctly kept) mark is consumed by the
/// resubmit-directing fail-fast inside that same command — there is no
/// post-fixture point where the kept mark itself is observable
/// (`fail_fast_topdown_pruned_root` consumes the marker; that
/// consumption is pinned by
/// `test_fail_fast_clears_topdown_pruned_and_resubmission_builds_from_source`).
/// The mark's survival through the gate is therefore proven
/// structurally — the fail-fast's "topdown … resubmit" build error is
/// only reachable for a node whose mark was still set after the gate
/// ran — and the SQL-level live-vs-terminal discrimination is pinned
/// directly by the db-level test
/// (`db::tests::recovery::test_load_parents_with_all_children_produced_requires_live_build_link`).
///
/// Staged shape (phase 1): `merge_dag(B0, [parent, child], [edge])`
/// then `merge_dag(B1, [parent only], [])` — B1 owns no link to the
/// child. After `drop(handle)`: backdate child→`completed`,
/// parent→`substituting` + `topdown_pruned = true`, B0→`succeeded`;
/// B1 stays `active`. Phase 2's store has `out` substitutable and
/// `debug` missing / not substitutable.
#[tokio::test]
async fn test_failover_keeps_topdown_pruned_when_produced_children_belong_to_terminal_build()
-> TestResult {
    let out = test_store_path("tdhist-root-out");
    let dbg = test_store_path("tdhist-root-debug");

    // Phase-2 store: `out` substitutable upstream; `debug` missing and
    // NOT substitutable — substitution cannot satisfy the recovered
    // all-declared wanted set, so the kept mark must route the node to
    // the fail-fast arm, not to a from-source dispatch.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let b0 = Uuid::new_v4(); // historical build — terminal at failover
    let b1 = Uuid::new_v4(); // live re-request — owns only the parent
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        let mk_parent = || {
            let mut n = make_node("tdhist-root");
            n.output_names = vec!["out".into(), "debug".into()];
            n.expected_output_paths = vec![out.clone(), dbg.clone()];
            n.wanted_output_names = vec![];
            n
        };
        // B0: full merge parent→child WITH the edge persisted — the
        // previous generation that actually built the closure.
        merge_dag(
            &handle,
            b0,
            vec![mk_parent(), make_node("tdhist-dep")],
            vec![make_test_edge("tdhist-root", "tdhist-dep")],
            false,
        )
        .await?;
        // B1: the re-request — parent only, no edges, so
        // batch_insert_build_derivations links B1 to the parent only.
        merge_dag(&handle, b1, vec![mk_parent()], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate AFTER the last merge (a later merge re-upserts the
        // derivation row): the child finished under B0, B0 went
        // terminal, and the parent is left mid-substitution with the
        // mark set — the post-prune persisted shape for B1.
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'tdhist-dep'")
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = 'tdhist-root'",
        )
        .execute(&pool)
        .await?;
        sqlx::query("UPDATE builds SET status = 'succeeded' WHERE build_id = $1")
            .bind(b0)
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // A builder is available — the doomed from-source dispatch has
    // somewhere to go if the kept mark fails to gate it.
    let mut worker_rx = connect_executor(&handle, "tdhist-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // No WorkAssignment is ever sent for the parent: its closure was
    // never merged for B1, so a from-source dispatch would ENOENT.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "a pruned root whose produced children belong to a terminal build \
             must not be dispatched from source after failover"
        );
    }
    let d = expect_drv(&handle, "tdhist-root").await;
    assert!(
        !matches!(
            d.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "the kept mark must route the node to the fail-fast arm, not leave it \
         dispatchable from source; got {:?}",
        d.status
    );
    // The live build terminates with the bounded resubmit-directing
    // error (same assertions as the childless fail-fast test).
    let s = query_status(&handle, b1).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "the live build must fail fast (resubmit re-probes or full-merges); \
         got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s.error_summary
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// The closure-hole breadcrumb is persisted (`migrations/064`) and must
/// be restored by `from_recovery_row` — not reset to false the way the
/// pre-064 code did. Restoring it is what lets the recovery-time gate
/// and the merge-time heal keep honoring "an un-produced child was
/// reaped out from under this node" across a leader failover.
///
/// Two pins, one per half. The restore itself is observed DIRECTLY:
/// right after recovery (before any merge) the debug surface must show
/// the in-memory breadcrumb true — that is `from_recovery_row` carrying
/// the persisted column into the restored state. The heal half is then
/// pinned on the persisted column: a post-failover full merge
/// re-declares the node's edges and the persisted breadcrumb flips
/// true→false while the still-unvouched mark stays. The heal is TOTAL —
/// it pushes the PG clear for every edge parent it re-declares, keyed
/// on the persisted column only, never on the pre-clear in-memory value
/// (the heal-totality test in merge.rs stages exactly that divergence) —
/// so the column flip pins the heal + persistence round-trip, not the
/// restore; the direct post-recovery assert is what catches a restore
/// regression.
#[tokio::test]
async fn test_recovery_restores_closure_hole_and_heal_clears_persisted_breadcrumb() -> TestResult {
    let f = RecoveryFixture::run(async |handle, pool| {
        // A plain single-node merge stages the build / link / derivation
        // rows; the reap that sets the breadcrumb in production has its
        // own tests (the mixed-shape reap tests in merge.rs) — backdate
        // the persisted shape directly, like the topdown_pruned restore
        // test above.
        merge_dag(
            &handle,
            Uuid::new_v4(),
            vec![make_node("chrec-root")],
            vec![],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true, \
             closure_hole = true WHERE drv_hash = 'chrec-root'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // The mark itself is restored and kept (childless ⇒ the
    // produced-children gate has no edges to judge; the holed-parent
    // veto is pinned by the closure-hole keep test below).
    assert!(
        expect_drv(&handle, "chrec-root").await.topdown_pruned,
        "fixture premise: the restored topdown_pruned mark survives recovery"
    );
    // The restore observed directly: `from_recovery_row` must carry the
    // persisted breadcrumb into the in-memory state instead of resetting
    // it to false the way the pre-064 code did. The heal below is total
    // (keyed on the persisted column, not on this bit), so without this
    // assert a restore regression would be invisible to this test.
    assert!(
        expect_drv(&handle, "chrec-root").await.closure_hole,
        "recovery must restore the in-memory closure_hole breadcrumb from the persisted column"
    );
    // Nothing between the backdate and the heal may clear the persisted
    // breadcrumb: recovery only restores it.
    let (pg_pruned, pg_hole): (bool, bool) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = 'chrec-root'",
    )
    .fetch_one(&f.db.pool)
    .await?;
    assert!(
        pg_pruned && pg_hole,
        "fixture premise: the backdated mark + breadcrumb are still persisted after recovery"
    );

    // A post-failover FULL merge re-declares the node's edges: its child
    // set is representative of its closure again, so the heal drops the
    // breadcrumb in memory and pushes the PG clear for every edge parent
    // it re-declares (total — not keyed on the restored in-memory value).
    merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![make_node("chrec-root"), make_node("chrec-dep")],
        vec![make_test_edge("chrec-root", "chrec-dep")],
        false,
    )
    .await?;
    barrier(&handle).await;

    let (pg_pruned, pg_hole): (bool, bool) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = 'chrec-root'",
    )
    .fetch_one(&f.db.pool)
    .await?;
    assert!(
        !pg_hole,
        "a full merge re-declaring the node's edges must clear the persisted closure_hole \
         restored at recovery (the heal is total over the re-declared edge parents)"
    );
    assert!(
        pg_pruned,
        "the heal must not clear the topdown_pruned mark itself — the new child is unbuilt \
         (Pending, not Vouched)"
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// bug_006 regression (the recovery-time veto): a restored
/// `topdown_pruned` mark whose row also carries the persisted
/// `closure_hole` breadcrumb must be KEPT at recovery even when every
/// surviving persisted child is produced and vouched for by a live
/// build that co-owns the parent — the breadcrumb records that an
/// un-produced child was reaped out from under the node, so whatever
/// children remain in PG are a truncated view of its pruned input
/// closure (the orphan-terminal GC may have deleted the un-produced
/// child's row and edge entirely, which is exactly why the breadcrumb
/// is persisted rather than re-derived from the children).
///
/// Staged shape: identical to
/// `test_failover_clears_topdown_pruned_when_children_all_produced`
/// above (full merge parent→child with the edge persisted, child
/// backdated `completed`, parent backdated `substituting` +
/// `topdown_pruned = true`, the owning build still live) plus
/// `closure_hole = true` on the parent row. Pre-064 the gate cleared
/// the mark on the strength of the produced survivor and the parent
/// was dispatched from source (the doomed ENOENT dispatch). Post-064
/// the holed parent is never enrolled as a clear candidate, so the
/// kept mark routes it to the bounded resubmit-directing fail-fast
/// instead — asserted behaviorally, the same way the
/// terminal-build keep test above does (the post-recovery dispatch
/// pass consumes the kept mark, so "mark still set" is not directly
/// observable).
#[tokio::test]
async fn test_failover_keeps_topdown_pruned_when_closure_hole_recorded() -> TestResult {
    let out = test_store_path("tdvh-root-out");
    let dbg = test_store_path("tdvh-root-debug");

    // Phase-2 store: `out` substitutable upstream; `debug` missing and
    // NOT substitutable — substitution cannot satisfy the recovered
    // all-declared wanted set, so the kept mark must route the node to
    // the fail-fast arm, not to a from-source dispatch.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        // Full merge: the parent depends on the child and the edge IS
        // persisted, and the SAME live build owns both rows — the
        // strongest possible produced-children evidence, defeated only
        // by the breadcrumb.
        let mut parent = make_node("tdvh-root");
        parent.output_names = vec!["out".into(), "debug".into()];
        parent.expected_output_paths = vec![out.clone(), dbg.clone()];
        parent.wanted_output_names = vec![];
        merge_dag(
            &handle,
            build_id,
            vec![parent, make_node("tdvh-dep")],
            vec![make_test_edge("tdvh-root", "tdvh-dep")],
            false,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate: the child finished under the old leader, but a
        // SECOND (un-produced) child had been reaped out from under the
        // parent before the crash — its row may since have been GC'd,
        // so all that survives is the breadcrumb the leader persisted
        // at reap time alongside the mark.
        sqlx::query("UPDATE derivations SET status = 'completed' WHERE drv_hash = 'tdvh-dep'")
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true, \
             closure_hole = true WHERE drv_hash = 'tdvh-root'",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // A builder is available — the doomed from-source dispatch has
    // somewhere to go if the produced survivor launders the clear.
    let mut worker_rx = connect_executor(&handle, "tdvh-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // No WorkAssignment is ever sent for the parent: its pruned closure
    // was truncated by the reap, so a from-source dispatch would ENOENT.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "a closure-holed pruned root must not be dispatched from source after \
             failover, however produced its surviving persisted children look"
        );
    }
    let d = expect_drv(&handle, "tdvh-root").await;
    assert!(
        !matches!(
            d.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "the kept mark must route the holed node to the fail-fast arm, not leave it \
         dispatchable from source; got {:?}",
        d.status
    );
    // The live build terminates with the bounded resubmit-directing
    // error (same assertions as the childless and terminal-build keep
    // tests above) instead of staying hostage to a doomed dispatch.
    let s = query_status(&handle, build_id).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "the build must fail fast (resubmit re-probes or full-merges); \
         got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s.error_summary
    );
    Ok(())
}

/// Phase-1 staging shared by the three bug_009 regression tests below:
/// build `b2` full-merges parent→child (the `derivation_edges` row and
/// B2's `build_derivations` links to BOTH rows are persisted), build
/// `b1` is the pruned re-request that links ONLY the parent (no edges).
/// After the merges the phase-1 handle is dropped and the persisted
/// rows are backdated to the bug_009 crash shape: the child went
/// `cancelled` when `b2` was cancelled (`builds.status = 'cancelled'`),
/// the parent is left mid-substitution (`substituting`, plus
/// `topdown_pruned = true` when `mark_parent`), and `b1` stays
/// `active`. The parent declares outputs `[out, debug]` with stored
/// wanted `'{}'` (= all declared) and `expected_output_paths`
/// `[out_path, dbg_path]` so the phase-2 store staging decides the
/// post-failover route (substitution / from-source / fail-fast).
#[allow(clippy::too_many_arguments)] // test staging helper — a struct param would just rename the args
async fn stage_parent_with_other_builds_cancelled_child(
    handle: ActorHandle,
    pool: &sqlx::PgPool,
    b2: Uuid,
    b1: Uuid,
    root: &str,
    dep: &str,
    out_path: &str,
    dbg_path: &str,
    mark_parent: bool,
) -> anyhow::Result<()> {
    let mk_parent = || {
        let mut n = make_node(root);
        n.output_names = vec!["out".into(), "debug".into()];
        n.expected_output_paths = vec![out_path.to_string(), dbg_path.to_string()];
        n.wanted_output_names = vec![];
        n
    };
    // B2: full merge parent→child WITH the edge persisted — the build
    // whose cancellation later cancels the sole-interest child.
    merge_dag(
        &handle,
        b2,
        vec![mk_parent(), make_node(dep)],
        vec![make_test_edge(root, dep)],
        false,
    )
    .await?;
    // B1: the pruned re-request — parent only, no edges, so
    // batch_insert_build_derivations links B1 to the parent only.
    merge_dag(&handle, b1, vec![mk_parent()], vec![], false).await?;
    barrier(&handle).await;
    drop(handle);
    // Backdate AFTER the last merge (a later merge re-upserts the
    // derivation row): the child is the row cancel_build_derivations
    // persists for B2's sole-interest unbuilt child when B2 is
    // cancelled; the parent is the post-prune persisted shape for B1.
    sqlx::query("UPDATE derivations SET status = 'cancelled' WHERE drv_hash = $1")
        .bind(dep)
        .execute(pool)
        .await?;
    if mark_parent {
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = $1",
        )
        .bind(root)
        .execute(pool)
        .await?;
    } else {
        sqlx::query("UPDATE derivations SET status = 'substituting' WHERE drv_hash = $1")
            .bind(root)
            .execute(pool)
            .await?;
    }
    sqlx::query("UPDATE builds SET status = 'cancelled' WHERE build_id = $1")
        .bind(b2)
        .execute(pool)
        .await?;
    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
/// bug_009, clearing harm: another build's cancelled, never-wanted
/// child must not condemn a healthy pruning build's recovered parent.
/// The failed-dep cascade only counts a terminal-failure child when a
/// LIVE build that also owns the parent vouches for it; B2 is
/// `cancelled`, so its child is dead cross-build evidence.
///
/// Staging: see [`stage_parent_with_other_builds_cancelled_child`]
/// (parent marked `topdown_pruned`). Phase-2 store: ALL of the parent's
/// declared outputs are substitutable, so the only correct outcome is
/// completion via the substitution carve-out.
///
/// Pre-fix: `load_parents_with_failed_deps` returns the parent on the
/// strength of B2's cancelled child alone, `seed_ready_queue`
/// short-circuits it Substituting→Queued→DependencyFailed and persists
/// it, and B1 is terminally failed with the recovery dependency-failure
/// summary — substitution never gets a chance. Post-fix the parent
/// recovers childless with the kept mark, completes via substitution,
/// and B1 succeeds.
#[tokio::test]
async fn test_failover_pruned_build_completes_via_substitution_despite_other_builds_cancelled_child()
-> TestResult {
    let out = test_store_path("bug9s-root-out");
    let dbg = test_store_path("bug9s-root-debug");

    // Phase-2 store: every declared output is substitutable upstream.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    {
        let mut sub = store.state.substitutable.write().unwrap();
        sub.push(out.clone());
        sub.push(dbg.clone());
    }

    let b2 = Uuid::new_v4(); // other build — cancelled at failover
    let b1 = Uuid::new_v4(); // healthy pruning build under test
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        stage_parent_with_other_builds_cancelled_child(
            handle,
            &pool,
            b2,
            b1,
            "bug9s-root",
            "bug9s-dep",
            &out,
            &dbg,
            true,
        )
        .await
    })
    .await?;
    let handle = f.handle;

    // The core regression assertion: the cascade must not have condemned
    // the parent during recovery over B2's cancelled child.
    let d = expect_drv(&handle, "bug9s-root").await;
    assert_ne!(
        d.status,
        DerivationStatus::DependencyFailed,
        "another build's cancelled child must not condemn the recovered parent \
         of a healthy pruning build"
    );

    // A worker is available — a wrongful from-source dispatch would have
    // somewhere to land.
    let mut worker_rx = connect_executor(&handle, "bug9s-w", "x86_64-linux").await?;
    tick(&handle).await?;
    // Deterministic end state: the kept mark routes the node through the
    // substitution carve-out and the detached fetch completes it.
    wait_for_status(&handle, "bug9s-root", DerivationStatus::Completed).await;

    // No WorkAssignment was ever sent for the parent (substitution, not
    // a from-source dispatch).
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "the parent must complete via substitution, never via a from-source \
             dispatch (its closure was never merged for B1)"
        );
    }
    // Not condemned in PG either.
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'bug9s-root'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_ne!(
        pg_status, "dependency_failed",
        "the cascade must not persist a dependency-failure verdict for the parent"
    );
    assert_eq!(
        pg_status, "completed",
        "the parent completes via substitution after failover"
    );
    // B1 is never condemned by another build's child: with every output
    // substitutable it deterministically completes.
    let s = query_status(&handle, b1).await?;
    assert_ne!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "the healthy pruning build must not be failed over another build's \
         cancelled child; error={:?}",
        s.error_summary
    );
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "with all outputs substitutable the pruning build completes via \
         substitution; got state={} error={:?}",
        s.state,
        s.error_summary
    );
    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
// r[verify sched.merge.substitute-topdown+10]
/// bug_009, verdict harm: when the recovered parent's wanted set is
/// genuinely unsatisfiable by substitution, the verdict and error must
/// come from the node's OWN bounded resubmit-directing fail-fast — not
/// from the failed-dep cascade acting on another build's cancelled
/// child.
///
/// Same staging as the substitution variant above, but the phase-2
/// store has `out` substitutable and `debug` missing / not
/// substitutable, so the recovered all-declared wanted set cannot be
/// satisfied and the kept mark must route the node to the fail-fast
/// arm (the keep-test shape, with the historical edge pointing at a
/// cancelled — not produced — child).
///
/// Deliberately NOT asserted: "the parent is not DependencyFailed".
/// The bounded fail-fast itself terminalizes a sole-interest parked
/// node as DependencyFailed (`fail_fast_topdown_pruned_root` parks it
/// Queued, then `cancel_build_derivations` dependency-fails every
/// sole-interest not-yet-dispatched node of the failing build), so the
/// node's terminal status converges with the pre-fix outcome. What
/// distinguishes the two paths — and what this test pins — is the
/// provenance: B1's error is the actionable "topdown … resubmit"
/// fail-fast, NOT the recovery cascade's "recovered with N failed
/// derivation(s)" summary, and the mark is consumed by that fail-fast
/// (PG `topdown_pruned = false`; the cascade never touches the mark and
/// would leave it true).
#[tokio::test]
async fn test_failover_pruned_build_gets_resubmit_error_not_dependency_failure_from_other_builds_child()
-> TestResult {
    let out = test_store_path("bug9f-root-out");
    let dbg = test_store_path("bug9f-root-debug");

    // Phase-2 store: `out` substitutable upstream; `debug` missing and
    // NOT substitutable — substitution cannot satisfy the recovered
    // all-declared wanted set, so the kept mark must route the node to
    // the fail-fast arm, not to a from-source dispatch.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let b2 = Uuid::new_v4(); // other build — cancelled at failover
    let b1 = Uuid::new_v4(); // healthy pruning build under test
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        stage_parent_with_other_builds_cancelled_child(
            handle,
            &pool,
            b2,
            b1,
            "bug9f-root",
            "bug9f-dep",
            &out,
            &dbg,
            true,
        )
        .await
    })
    .await?;
    let handle = f.handle;

    // A builder is available — the doomed from-source dispatch has
    // somewhere to go if the kept mark fails to gate it.
    let mut worker_rx = connect_executor(&handle, "bug9f-w", "x86_64-linux").await?;
    tick(&handle).await?;

    // No WorkAssignment is ever sent for the parent: its closure was
    // never merged for B1, so a from-source dispatch would ENOENT.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "a pruned root with an unsatisfiable wanted set must take the bounded \
             fail-fast, never a from-source dispatch"
        );
    }
    let d = expect_drv(&handle, "bug9f-root").await;
    assert!(
        !matches!(
            d.status,
            DerivationStatus::Assigned | DerivationStatus::Running
        ),
        "the parent must never be handed to a worker; got {:?}",
        d.status
    );
    // B1's terminal outcome is its OWN resubmit-directing fail-fast, not
    // a dependency-failure verdict inherited from B2's cancelled child.
    let s = query_status(&handle, b1).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "with `debug` unsatisfiable the pruning build takes the bounded \
         fail-fast; got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary must be the resubmit-directing fail-fast; got {:?}",
        s.error_summary
    );
    assert!(
        !s.error_summary.contains("recovered with")
            && !s.error_summary.contains("failed derivation"),
        "error summary must not be the recovery cascade's dependency-failure \
         wording; got {:?}",
        s.error_summary
    );
    // The mark was consumed by the fail-fast (in PG too). The cascade
    // never touches the mark, so pre-fix this stays true — a second
    // discriminator between the two paths.
    let (pg_pruned,): (bool,) =
        sqlx::query_as("SELECT topdown_pruned FROM derivations WHERE drv_hash = 'bug9f-root'")
            .fetch_one(&f.db.pool)
            .await?;
    assert!(
        !pg_pruned,
        "the bounded fail-fast consumes the topdown_pruned mark; the recovery \
         cascade would have left it set"
    );
    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
/// The unflagged variant of the two tests above (no `topdown_pruned`
/// backdate — the gateway single-node-fallback shape: B1 submitted just
/// the parent, no prune involved). Another build's cancelled child must
/// not condemn it either: the parent recovers childless, comes back
/// Ready, and dispatches from source to the connected worker; B1 stays
/// alive. Pre-fix the cascade short-circuited it to DependencyFailed at
/// recovery (the cascade never consulted the mark, so the unflagged
/// shape was bitten identically) and the dispatch never happened.
#[tokio::test]
async fn test_failover_unflagged_parent_with_other_builds_cancelled_child_dispatches_from_source()
-> TestResult {
    let out = test_store_path("bug9u-root-out");
    let dbg = test_store_path("bug9u-root-debug");

    // Phase-2 store: `out` substitutable, `debug` missing and not
    // substitutable — substitution cannot satisfy the wanted set, and
    // with no mark the node must dispatch from source.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let b2 = Uuid::new_v4(); // other build — cancelled at failover
    let b1 = Uuid::new_v4(); // healthy single-node build under test
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        stage_parent_with_other_builds_cancelled_child(
            handle,
            &pool,
            b2,
            b1,
            "bug9u-root",
            "bug9u-dep",
            &out,
            &dbg,
            false,
        )
        .await
    })
    .await?;
    let handle = f.handle;

    // Not condemned by B2's cancelled child at recovery time.
    let d = expect_drv(&handle, "bug9u-root").await;
    assert_ne!(
        d.status,
        DerivationStatus::DependencyFailed,
        "another build's cancelled child must not condemn the recovered parent \
         of a healthy unflagged build"
    );

    // The node recovered childless and unmarked → it dispatches from
    // source to the connected worker.
    let mut worker_rx = connect_executor(&handle, "bug9u-w", "x86_64-linux").await?;
    tick(&handle).await?;
    let a = recv_assignment(&mut worker_rx).await;
    assert_eq!(
        a.drv_path,
        test_drv_path("bug9u-root"),
        "the unflagged recovered parent must dispatch from source after failover"
    );
    // B1 stays alive (the assignment is in flight).
    let s = query_status(&handle, b1).await?;
    assert_ne!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "the healthy build must not be failed over another build's cancelled \
         child; error={:?}",
        s.error_summary
    );
    Ok(())
}

// r[verify sched.merge.substitute-topdown+10]
/// bug_006 regression (the recovery-time stamp): when recovery drops the
/// edge to an un-produced terminal child of a restored marked parent, it
/// must record the closure hole — in memory and best-effort in PG —
/// exactly as the in-process reap does for the same removal. Without the
/// breadcrumb the parent recovers marked with a silently truncated child
/// set (NOT childless: the other child survives), and when that surviving
/// sibling later completes, `clear_topdown_pruned_for_produced_parents`
/// judges the truncated set Vouched, clears the mark in memory and PG,
/// and the node is dispatched from source against a closure that was
/// never produced — the doomed ENOENT dispatch the mark exists to
/// prevent.
///
/// Staged shape (phase 1): B2 full-merges P→{C1, C2} (both edges
/// persisted); a separate live build keeps the surviving sibling C1
/// alive; B1 is the pruned re-request that links only P. Backdates:
/// C2 → `cancelled` (the row B2's cancel sweep persisted for its
/// sole-interest unbuilt child), P → `substituting` + `topdown_pruned =
/// true` (the post-prune persisted shape for B1), B2 → `cancelled`; B1
/// and the sibling's build stay live. `closure_hole` is deliberately NOT
/// staged: no reap ran before the failover — that is the premise.
///
/// Phase 2: P recovers marked with in-memory children {C1} (the P→C2
/// edge is dropped; the failed-dep cascade correctly does not condemn P
/// because no live build co-owns P and C2, and the produced-children
/// gate correctly refuses to clear — but its refusal alone never reaches
/// the in-memory model). The recovery-time stamp must record the
/// truncation. C1 is then built by a worker under its own build; the
/// recovery-recorded hole vetoes the completion-time clear, so P is
/// never dispatched from source and B1 takes the bounded
/// resubmit-directing fail-fast (its recovered all-declared wanted set
/// has `debug` missing upstream and not substitutable). "Mark still set
/// after the sibling completed" is asserted structurally, the same way
/// the keep tests above do: the fail-fast consumes the mark in the same
/// dispatch pass, and its "topdown … resubmit" build error is only
/// reachable for a node whose mark survived C1's completion.
///
/// Pre-fix red: nothing set the hole at recovery (the persisted-
/// breadcrumb SELECT below fails), and C1's completion judged the
/// truncated set Vouched and cleared the mark — P came back to a
/// from-source-dispatchable Ready (assigned as soon as worker capacity
/// frees) and B1 stayed Active instead of failing with the
/// resubmit-directing error.
#[tokio::test]
async fn test_failover_recovery_records_closure_hole_for_dropped_unproduced_terminal_child()
-> TestResult {
    let out = test_store_path("bug6t3-root-out");
    let dbg = test_store_path("bug6t3-root-debug");
    let keep_out = test_store_path("bug6t3-keep-out");

    // Phase-2 store: P's `out` is substitutable upstream, `debug` is
    // missing and NOT substitutable (substitution cannot satisfy P's
    // recovered all-declared wanted set, so a kept mark must route P to
    // the fail-fast arm). C1's output is missing and not substitutable,
    // so the surviving sibling completes via the worker — keeping the
    // post-recovery world frozen until this test drives it.
    let (store, store_client, _store_task) =
        rio_test_support::grpc::spawn_mock_store_with_client().await?;
    store.state.substitutable.write().unwrap().push(out.clone());

    let b2 = Uuid::new_v4(); // full-merge owner of P, C1, C2 — cancelled at failover
    let b_keep = Uuid::new_v4(); // live build keeping the surviving sibling C1 alive
    let b1 = Uuid::new_v4(); // pruning build under test — owns only P
    let f = RecoveryFixture::run_with_store(Some(store_client), async |handle, pool| {
        let mk_parent = || {
            let mut n = make_node("bug6t3-root");
            n.output_names = vec!["out".into(), "debug".into()];
            n.expected_output_paths = vec![out.clone(), dbg.clone()];
            n.wanted_output_names = vec![];
            n
        };
        let mk_keep = || {
            let mut n = make_node("bug6t3-keep");
            n.expected_output_paths = vec![keep_out.clone()];
            n
        };
        // B2: full merge P→{C1, C2} with both edges persisted.
        merge_dag(
            &handle,
            b2,
            vec![mk_parent(), mk_keep(), make_node("bug6t3-gone")],
            vec![
                make_test_edge("bug6t3-root", "bug6t3-keep"),
                make_test_edge("bug6t3-root", "bug6t3-gone"),
            ],
            false,
        )
        .await?;
        // The sibling's own build: keeps C1 alive across B2's cancellation.
        merge_dag(&handle, b_keep, vec![mk_keep()], vec![], false).await?;
        // B1: the pruned re-request — links only P, no edges.
        merge_dag(&handle, b1, vec![mk_parent()], vec![], false).await?;
        barrier(&handle).await;
        drop(handle);
        // Backdate AFTER the merges (a later merge re-upserts the rows):
        // C2 went cancelled when B2 was cancelled, P is the post-prune
        // persisted shape for B1, C1 stays non-terminal under its live
        // build. closure_hole is NOT touched — no reap ran before the
        // failover, so recovery must create the breadcrumb itself.
        sqlx::query("UPDATE derivations SET status = 'cancelled' WHERE drv_hash = 'bug6t3-gone'")
            .execute(&pool)
            .await?;
        sqlx::query(
            "UPDATE derivations SET status = 'substituting', topdown_pruned = true \
             WHERE drv_hash = 'bug6t3-root'",
        )
        .execute(&pool)
        .await?;
        sqlx::query("UPDATE builds SET status = 'cancelled' WHERE build_id = $1")
            .bind(b2)
            .execute(&pool)
            .await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // The restored mark survived recovery (the produced-children gate
    // cannot clear it: C2 is cancelled, C1 unbuilt)...
    assert!(
        expect_drv(&handle, "bug6t3-root").await.topdown_pruned,
        "fixture premise: the restored topdown_pruned mark survives recovery"
    );
    // ...and recovery recorded the truncation durably: the persisted
    // closure_hole went true even though the fixture never staged it —
    // the recovery-side analogue of the reap-time breadcrumb. (The
    // in-memory half is pinned behaviorally below: only the in-memory
    // breadcrumb can veto the completion-time clear this tenure.)
    let (pg_pruned, pg_hole): (bool, bool) = sqlx::query_as(
        "SELECT topdown_pruned, closure_hole FROM derivations WHERE drv_hash = 'bug6t3-root'",
    )
    .fetch_one(&f.db.pool)
    .await?;
    assert!(
        pg_pruned,
        "fixture premise: the persisted topdown_pruned mark is still set after recovery"
    );
    assert!(
        pg_hole,
        "recovery must persist the closure-hole breadcrumb for a parent whose \
         un-produced terminal child's edge it dropped"
    );

    // A worker is available: C1 builds from source under its own build,
    // and a wrongful from-source dispatch of P would have somewhere to
    // land.
    let mut worker_rx = connect_executor(&handle, "bug6t3-w", "x86_64-linux").await?;
    tick(&handle).await?;
    let a = recv_assignment(&mut worker_rx).await;
    assert_eq!(
        a.drv_path,
        test_drv_path("bug6t3-keep"),
        "the surviving sibling (not P) is the only dispatchable node after failover"
    );

    // The sibling completes → the completion-time clear re-judges P over
    // its truncated in-memory child set {C1}. The recovery-recorded hole
    // must veto that clear. The completion handler ends with an inline
    // dispatch pass, so P's fate (fail-fast vs from-source assignment) is
    // settled once the barrier returns — deliberately no second Tick (a
    // second Tick would trip the cfg(test) zero-grace orphan-watcher
    // sweep on these unwatched recovered builds and cancel B1 for an
    // unrelated reason).
    complete_success(
        &handle,
        "bug6t3-w",
        &test_drv_path("bug6t3-keep"),
        &keep_out,
    )
    .await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "bug6t3-keep").await.status,
        DerivationStatus::Completed,
        "fixture premise: the surviving sibling completed under its live build"
    );

    // No from-source WorkAssignment is ever sent for P: its pruned
    // closure was truncated at recovery, so a from-source dispatch would
    // ENOENT on the never-produced subtree.
    while let Ok(m) = worker_rx.try_recv() {
        use rio_proto::types::scheduler_message::Msg;
        assert!(
            !matches!(m.msg, Some(Msg::Assignment(_))),
            "the surviving sibling's completion must not launder the mark of a \
             parent whose un-produced terminal child was dropped at recovery"
        );
    }
    let p = expect_drv(&handle, "bug6t3-root").await;
    assert!(
        !matches!(
            p.status,
            DerivationStatus::Assigned | DerivationStatus::Running | DerivationStatus::Ready
        ),
        "P must never become dispatchable from source after the sibling completes; \
         got {:?}",
        p.status
    );
    // B1's terminal outcome is the bounded resubmit-directing fail-fast —
    // only reachable when the mark survived the sibling's completion —
    // not a from-source dispatch.
    let s = query_status(&handle, b1).await?;
    assert_eq!(
        s.state,
        rio_proto::types::BuildState::Failed as i32,
        "with `debug` unsatisfiable the pruning build must take the bounded \
         fail-fast; got state={} error={:?}",
        s.state,
        s.error_summary
    );
    assert!(
        s.error_summary.contains("topdown") && s.error_summary.contains("resubmit"),
        "error summary should direct resubmit; got {:?}",
        s.error_summary
    );
    // The sibling's own build is untouched by P's verdict.
    let sk = query_status(&handle, b_keep).await?;
    assert_eq!(
        sk.state,
        rio_proto::types::BuildState::Succeeded as i32,
        "the build owning the surviving sibling completes normally; error={:?}",
        sk.error_summary
    );
    Ok(())
}
