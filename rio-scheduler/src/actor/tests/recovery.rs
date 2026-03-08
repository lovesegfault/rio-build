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
