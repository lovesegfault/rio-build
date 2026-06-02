//! State recovery: LeaderAcquired → recover_from_pg → DAG rebuilt.
//
// Recovery isn't a standalone spec rule — it's behavior under
// sched.lease.k8s-lease (what happens on acquire). The test here
// verifies the LeaderAcquired → recover_from_pg → recovery_complete
// pipeline; the lease loop's acquire behavior is covered in
// lease.rs tests (sched.lease.generation-fence+3 verify).

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

/// Recovery failure with PG fully down (pool closed: BOTH the DAG load
/// and the independent PG-floor read fail) still requires the PG-free
/// post-claim leadership confirmation, then completes at the
/// recovery-entry generation with an EMPTY DAG — degrade after
/// confirmation, don't block. The alternative (never completing) would
/// block dispatch forever while the scheduler holds the lease and the
/// standby cannot take over. The unconfirmed direction of the same
/// fallback is pinned by
/// `test_recovery_floor_unreadable_unconfirmed_is_discarded`; the
/// floored load-failure path is pinned by
/// `test_recovery_load_failure_still_floors_claims_and_confirms`.
// r[verify sched.recovery.gate-dispatch]
#[tokio::test]
async fn test_recovery_failure_degrades_to_empty_dag() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;

    // Keep a clone of the LeaderState so the completion is observable
    // from the test (same pattern as test_recovery_toctou_on_lease_flap
    // below).
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(1)),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    // Close the pool BEFORE sending LeaderAcquired — all PG queries
    // will fail. This simulates PG going down mid-recovery.
    db.pool.close().await;

    // LeaderAcquired → recover_from_pg fails AND the floor read fails →
    // the floor-unreadable fallback. Park at the gate and drive one
    // post-claim Leading round so the (PG-free) confirmation succeeds
    // on its first poll — no real-time stall.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    let round = leader.begin_renew_round();
    leader.confirm_leading_round(round);
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "a confirmed floor-unreadable term completes (degrade after confirmation, \
         don't block dispatch)"
    );
    assert_eq!(
        leader.generation(),
        1,
        "with the floor unreadable the term completes at the recovery-entry \
         generation (no floor to seed from, nothing to claim)"
    );
    let info = handle.debug_query_derivation("anything").await?;
    assert!(info.is_none(), "DAG should be empty after recovery failure");

    Ok(())
}

/// The floor-unreadable fallback is confirmation-gated: when the PG
/// floor cannot be read, the term proceeds unclaimed at the entry
/// generation but must still obtain the PG-free post-claim leadership
/// confirmation before completing — with no post-claim Leading round
/// the recovery must be discarded, never completed, never advertised,
/// and never silently counted as a success. The DAG load succeeds here;
/// only the floor read fails. Pairs with
/// `test_recovery_floor_unreadable_confirms_and_completes_unclaimed`
/// (the confirmed direction) and with
/// `test_recovery_failure_degrades_to_empty_dag` (both PG reads down).
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_floor_unreadable_unconfirmed_is_discarded() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    use crate::sla::metrics::counter_map_by;

    // The increments fire inside the SPAWNED actor task, so install the
    // recorder process-globally before the actor spawns (safe under
    // nextest's process-per-test model).
    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    rec.install().expect("install global debugging recorder");

    let db = TestDb::new(&MIGRATOR).await;
    // Saturated-regime entry generation: completing here without the
    // confirmation is exactly the deposed-but-unaware leapfrog the
    // confirmation exists to prevent.
    let generation = Arc::new(AtomicU64::new(2));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.fail_next_floor_read = true;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    // Deliberately grant NO Leading round before releasing: the
    // confirmation can never arrive.
    release_tx.send(()).expect("actor still listening");

    // No lease loop is running, so no confirmation can ever arrive. The
    // term must neither seed, complete, nor advertise inside this
    // window — on the unconfirmed-completion regression it does all
    // three within the first few polls.
    for _ in 0..20 {
        assert!(
            generation.load(Ordering::Acquire) <= 2,
            "a floor-unreadable term must never seed above the entry generation"
        );
        assert!(
            !leader.recovery_complete(),
            "a floor-unreadable term must not complete before the post-claim confirmation"
        );
        assert_eq!(
            handle.generation.advertised(),
            0,
            "claim-before-advertise: an unconfirmed floor-unreadable recovery must not advertise"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The deposal observation that in production arrives within a renew
    // interval: the lose edge ends the wait and the gate discards.
    leader.on_lose();
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "an unconfirmed floor-unreadable recovery must be discarded, not completed"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        2,
        "the generation must still be the entry generation after the discard"
    );
    // Exactly one rio_scheduler_recovery_total increment, and it is a
    // discard — never a silent success. (The lose edge that ends the
    // wait makes the gate label the discard a flap; which discard label
    // applies is not the point — the absence of `success` is.)
    let by_outcome = counter_map_by(&snap, "rio_scheduler_recovery_total", Some("outcome"));
    assert_eq!(
        by_outcome.get("success").copied().unwrap_or(0),
        0,
        "an unconfirmed floor-unreadable recovery must not count as success: {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("failure").copied().unwrap_or(0),
        0,
        "the DAG load succeeded; only the floor read failed: {by_outcome:?}"
    );
    let discarded: u64 = by_outcome
        .iter()
        .filter(|(k, _)| k.starts_with("discarded"))
        .map(|(_, v)| *v)
        .sum();
    assert_eq!(
        discarded, 1,
        "the discard must be counted exactly once: {by_outcome:?}"
    );
    Ok(())
}

/// The degraded-but-confirmed leg of the floor-unreadable fallback:
/// when only the floor read fails (the DAG load succeeds) and the lease
/// loop completes a post-claim Leading round, the term completes
/// unclaimed at the entry generation, advertises it, and the failure is
/// visible to the operator (the floor-read-failure counter) instead of
/// being silent. There is no claim INSERT on this path — the floor
/// could not be read, so nothing is offered to the ledger.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_floor_unreadable_confirms_and_completes_unclaimed() -> TestResult {
    use metrics_util::debugging::{DebugValue, DebuggingRecorder};

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    rec.install().expect("install global debugging recorder");

    let db = TestDb::new(&MIGRATOR).await;
    let generation = Arc::new(AtomicU64::new(2));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.fail_next_floor_read = true;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    // Parked before the confirmation wait: simulate the lease loop
    // completing one post-claim Leading round so the confirmation
    // succeeds on its first poll. (No claim INSERT precedes the park on
    // this path — the floor read already failed.)
    let round = leader.begin_renew_round();
    leader.confirm_leading_round(round);
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "a confirmed floor-unreadable term completes (degrade after confirmation, don't block)"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        2,
        "the floor-unreadable term completes at the entry generation (nothing to seed from)"
    );
    assert_eq!(
        handle.generation.advertised(),
        2,
        "a confirmed floor-unreadable term advertises the entry generation"
    );
    let claim: Option<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims \
         WHERE holder_id = 'pod-us'",
    )
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        claim, None,
        "no claim INSERT is possible when the floor cannot be read"
    );
    // One drained capture serves both metric assertions (the
    // snapshotter drains on read — see counter_map_by's caveat).
    let mut floor_failures = 0u64;
    let mut success = 0u64;
    let mut non_success: Vec<(String, u64)> = Vec::new();
    for (ck, _, _, v) in snap.snapshot().into_vec() {
        let DebugValue::Counter(c) = v else { continue };
        let k = ck.key();
        match k.name() {
            "rio_scheduler_generation_floor_read_failed_total" => floor_failures += c,
            "rio_scheduler_recovery_total" => {
                let outcome = k
                    .labels()
                    .find(|l| l.key() == "outcome")
                    .map(|l| l.value().to_owned())
                    .unwrap_or_default();
                if outcome == "success" {
                    success += c;
                } else {
                    non_success.push((outcome, c));
                }
            }
            _ => {}
        }
    }
    assert_eq!(
        floor_failures, 1,
        "the floor-read failure must be visible to the operator (counter)"
    );
    assert_eq!(
        success, 1,
        "the confirmed floor-unreadable term still counts as a (degraded) success"
    );
    assert!(
        non_success.iter().all(|(_, c)| *c == 0),
        "no discard or failure outcome may be recorded for a confirmed completion: \
         {non_success:?}"
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

// r[verify sched.recovery.fetch-max-seed+4]
/// Recovery must seed generation from PG's floor (assignments ∪ claims)
/// via fetch_max, and must durably CLAIM the generation it lands on
/// before ungating dispatch. Defensive monotonicity: if the k8s Lease
/// object is deleted (its `leaseTransitions` counter — the primary
/// generation source — resets to 0), a worker holding a stale
/// assignment with generation=100 would ALSO accept new ones from
/// whatever the fresh lease derived (e.g., 1). Seeding from PG's
/// high-water mark bounds that: after recovery, generation >= PG max
/// + 1, and the claims ledger now records that generation so the NEXT
/// leader's floor covers it even if this one never persists an
/// assignment.
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

    // The seeded generation must be durably claimed BEFORE dispatch was
    // ungated — that row is what a successor's floor query reads if
    // this leader is deposed before persisting a single assignment.
    let claimed: Vec<(i64,)> =
        sqlx::query_as("SELECT generation FROM leader_generation_claims ORDER BY generation")
            .fetch_all(&f.db.pool)
            .await?;
    assert!(
        claimed.iter().any(|(c,)| *c == g as i64),
        "the generation recovery landed on ({g}) must be in the claims ledger, got {claimed:?}"
    );

    Ok(())
}

/// The depose-before-persist scenario the claims ledger exists for: the
/// previous leader claimed generation 200 but was deposed before a
/// single assignment row landed (or its assignment rows have since been
/// cascade-deleted by the orphan-terminal-derivation sweep — migration
/// 034's `ON DELETE CASCADE` makes `MAX(generation) FROM assignments`
/// regress on a quiescent cluster). The next leader's floor must still
/// see 200 and seed past it. Without the claims arm of
/// `max_known_generation`, the floor here is NULL and the new leader
/// would re-use a generation a live believer may still hold.
// r[verify sched.lease.generation-claim+2]
#[tokio::test]
async fn test_recovery_seeds_generation_from_unpersisted_claim() -> TestResult {
    let f = RecoveryFixture::run(async |_handle, pool| {
        // The ONLY trace of the previous leadership term is its claim
        // row — no builds, no derivations, no assignments.
        sqlx::query(
            "INSERT INTO leader_generation_claims (generation, holder_id) \
             VALUES (200, 'deposed-before-persist')",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    // Floor = GREATEST(NULL, 200) = 200 → seed to 201.
    let g = f.handle.leader_generation();
    assert!(
        g >= 201,
        "generation must seed past a claimed-but-never-persisted term: expected >= 201, got {g}"
    );

    Ok(())
}

/// Saturated-floor regime (post-lease-deletion): a fresh leader whose
/// lease-derived generation (1) sits far below the inherited PG floor
/// (a foreign claim row at 200) must still land its OWN recovery
/// writes — the recovery exceeds the floor, claims 201, stamps
/// `serving_generation` to it BEFORE `recover_from_pg` runs, and the
/// recovery's fenced writes (the poisoned-dep DependencyFailed status
/// persist exercised here) land under that claimed generation. (The
/// walk-era closure-hole stamp half of this test died with the
/// recovery R-gates in T-D5.1.)
///
/// TRIPWIRE for the claims-floor fence work
/// (`sched.evidence.durability`): this test must stay green through
/// every fencing commit, with zero assertion changes. Under a
/// per-dequeue / lease-derived capture design the recovery writes
/// would carry generation 1, sit below the floor of 200, and be
/// silently rolled back by the fence — re-introducing the
/// stale-evidence loss class the fence exists to eliminate, and
/// turning the PG assertions below red. If a fencing change makes this
/// test fail, the capture design is wrong (stop-and-report condition
/// 2); do NOT adjust this test.
// r[verify sched.evidence.durability+3]
#[tokio::test]
async fn saturated_floor_recovery_evidence_writes_land() -> TestResult {
    let b_poison = Uuid::new_v4(); // owner of the poisoned-dep pair D→E
    let f = RecoveryFixture::run(async |handle, pool| {
        // --- The recovery DependencyFailed status persist ---
        // Same staging as
        // test_recovery_substituting_with_poisoned_dep_goes_dependency_
        // failed: a legacy mid-substitution parent over a within-TTL
        // poisoned dep, both co-owned by one live build.
        merge_dag(
            &handle,
            b_poison,
            vec![make_node("satfloor-D"), make_node("satfloor-E")],
            vec![make_test_edge("satfloor-D", "satfloor-E")],
            true,
        )
        .await?;
        barrier(&handle).await;
        drop(handle);

        // Backdate AFTER the merges (a later merge re-upserts the rows).
        // Future-dated poisoned_at so the within-TTL load is
        // deterministic under the 100ms cfg(test) POISON_TTL.
        sqlx::query(
            "UPDATE derivations \
             SET status = 'poisoned', poisoned_at = now() + interval '1 hour' \
             WHERE drv_hash = 'satfloor-E'",
        )
        .execute(&pool)
        .await?;
        // Post-080 the walk-era status is unrepresentable; 'queued' is
        // the exact image the 080 data step (and the transitional
        // decode arm before it) gave such rows.
        sqlx::query("UPDATE derivations SET status = 'queued' WHERE drv_hash = 'satfloor-D'")
            .execute(&pool)
            .await?;

        // --- The saturated floor itself ---
        // The only trace of a foreign previous term: its claim row at
        // generation 200, far above the fresh leader's lease-derived 1.
        sqlx::query(
            "INSERT INTO leader_generation_claims (generation, holder_id) \
             VALUES (200, 'deposed-before-persist')",
        )
        .execute(&pool)
        .await?;
        Ok(())
    })
    .await?;

    // 1. The recovery exceeded the saturated floor and durably claimed
    //    past it (200 → 201) before its evidence writes ran.
    let g = f.handle.leader_generation();
    assert!(
        g >= 201,
        "generation must seed past the foreign claim at 200: expected >= 201, got {g}"
    );
    let claimed: Vec<(i64,)> =
        sqlx::query_as("SELECT generation FROM leader_generation_claims ORDER BY generation")
            .fetch_all(&f.db.pool)
            .await?;
    assert!(
        claimed.iter().any(|(c,)| *c == g as i64),
        "the generation recovery landed on ({g}) must be in the claims ledger, got {claimed:?}"
    );

    // 2. The recovery DependencyFailed status persist LANDED in PG for
    //    the legacy mid-substitution-over-poisoned-dep node.
    //    (Post-fence: this write goes through the claims-floor fence
    //    carrying the claimed generation 201 ≥ floor 201 — it must
    //    apply, never be fenced.)
    let d = expect_drv(&f.handle, "satfloor-D").await;
    assert_eq!(
        d.status,
        DerivationStatus::DependencyFailed,
        "a queued node with a co-owned poisoned dep must recompute to DependencyFailed"
    );
    let (pg_d_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'satfloor-D'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(
        pg_d_status, "dependency_failed",
        "the recovery's DependencyFailed persist must LAND in the saturated-floor regime"
    );

    Ok(())
}

/// Same-epoch re-acquire is idempotent: a self-fence false alarm
/// followed by a successful renew re-fires the acquire edge and re-runs
/// recovery, and the PG floor now contains OUR OWN claim row from the
/// previous run. The claim path must recognize it (same generation,
/// same holder) and retain the generation rather than bumping past its
/// own ledger entry — bumping would burn a generation per connectivity
/// blip and fence the leader's own in-flight assignments, contradicting
/// the lease-side same-epoch semantics. The ledger must not grow a new
/// row either.
// r[verify sched.lease.generation-claim+2]
// r[verify sched.recovery.fetch-max-seed+4]
#[tokio::test]
async fn test_recovery_same_holder_reclaim_retains_generation() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Our own claim from the previous recovery run of this same epoch.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'pod-us')",
    )
    .execute(&db.pool)
    .await?;

    // gen_at_entry = 5: the generation at recovery entry. The claim
    // path never reads the transition count, so this fixture covers
    // the fresh shape (entry == leaseTransitions + 1) and the
    // saturated post-deletion shape (entry seeded above it) alike.
    let generation = Arc::new(AtomicU64::new(5));
    let g = Arc::clone(&generation);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(g, Arc::new(AtomicBool::new(true)), false);
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        5,
        "a same-holder re-acquire of the same epoch must retain the generation"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(5, "pod-us".to_string())],
        "the ledger must not grow a new row on a same-epoch re-acquire"
    );
    Ok(())
}

/// The contrast case to the same-holder re-claim: the claim row at our
/// entry generation belongs to a DIFFERENT holder. That is the
/// post-lease-deletion collision the PK-CAS exists for — two replicas
/// raced through fresh acquisitions onto the same floor — and it MUST
/// bump, not retain. Distinct holders must never share a generation.
// r[verify sched.lease.generation-claim+2]
// r[verify sched.recovery.fetch-max-seed+4]
#[tokio::test]
async fn test_recovery_other_holder_at_our_generation_bumps() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Another replica claimed generation 5 first.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'pod-other')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(5));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    // Bump target (5 → 6): the recovery waits for a post-claim Leading
    // round, so simulate a healthy lease loop.
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        6,
        "another holder at our generation is a collision; we must exceed it"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(5, "pod-other".to_string()), (6, "pod-us".to_string())],
        "the colliding claimer lands on the next generation with its own row"
    );
    Ok(())
}

/// An assignments-only floor that ties the entry generation, with NO
/// claim row at all, must be exceeded — the ledger cannot affirm the
/// floor is ours. This is the first post-upgrade handover (assignment
/// history written before the claims ledger existed, migration 065
/// ships no backfill) and the unclaimed-proceed-predecessor case:
/// assignment rows carry no scheduler-holder identity, so a silent
/// ledger at our generation reads as foreign. Retaining here would let
/// a deposed pre-claim-ledger leader's in-flight term share the new
/// leader's generation, suspending the executor fence for that term.
// r[verify sched.recovery.fetch-max-seed+4]
#[tokio::test]
async fn test_recovery_assignments_only_floor_at_our_generation_bumps() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Pre-claim-ledger history: an assignment at exactly the entry
    // generation, claims ledger empty. Terminal status + parseable
    // drv_path so recovery neither loads the row nor logs a skip (the
    // floor query has no status filter, so terminal rows still set it).
    let (drv_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ('z-pre-upgrade', $1, 'x86_64-linux', 'completed') RETURNING derivation_id",
    )
    .bind(format!("/nix/store/{}-pre-upgrade.drv", "a".repeat(32)))
    .fetch_one(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status) \
         VALUES ($1, 'pre-upgrade-worker', 5, 'completed')",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(5));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    // Bump target (5 → 6): the recovery waits for a post-claim Leading
    // round, so simulate a healthy lease loop.
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        6,
        "an assignments-only floor at our generation cannot be proven ours; exceed it"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(6, "pod-us".to_string())],
        "the bumped generation must be claimed; the silent floor gains no row"
    );
    Ok(())
}

/// Companion row to the assignments-only bump: the same assignment-row
/// evidence at our generation PLUS our own claim row there. The
/// predicate keys on the own-claim witness, not on "claims table
/// empty", so this retains — it is the steady state of every same-epoch
/// re-acquire after the first post-upgrade term claimed its generation.
// r[verify sched.lease.generation-claim+2]
#[tokio::test]
async fn test_recovery_assignment_and_own_claim_at_our_generation_retains() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (drv_id,): (Uuid,) = sqlx::query_as(
        "INSERT INTO derivations (drv_hash, drv_path, system, status) \
         VALUES ('z-own-claim', $1, 'x86_64-linux', 'completed') RETURNING derivation_id",
    )
    .bind(format!("/nix/store/{}-own-claim.drv", "a".repeat(32)))
    .fetch_one(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO assignments (derivation_id, builder_id, generation, status) \
         VALUES ($1, 'own-claim-worker', 5, 'completed')",
    )
    .bind(drv_id)
    .execute(&db.pool)
    .await?;
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (5, 'pod-us')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(5));
    let g = Arc::clone(&generation);
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = crate::lease::LeaderState::from_parts(g, Arc::new(AtomicBool::new(true)), false);
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        5,
        "our own claim row at the tied floor proves it is ours; retain"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(5, "pod-us".to_string())],
        "the ledger must not grow a new row when our own claim already covers the floor"
    );
    Ok(())
}

/// A deposed-but-unaware leader's recovery must not complete above a
/// live successor: a claim target above the entry generation is seeded
/// only after a post-claim apiserver round-trip that ended with this
/// replica as the Lease holder. Here the ledger already holds a live
/// successor's claim at our entry generation (it claimed before our
/// claim path ran) and no confirmation ever arrives — the recovery must
/// be discarded, never seeded. The leftover (13, 'pod-us') claim row is
/// the documented harmless over-claim and is deliberately not asserted.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_unconfirmed_bump_above_live_holder_is_discarded() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // The dead prior term's claim and the LIVE successor's claim.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (11, 'old-term'), (12, 'pod-live')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(12));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    release_tx.send(()).expect("actor still listening");

    // No lease loop is running, so no confirmation can ever arrive. The
    // seed to 13 must never land — on the unconfirmed-seed regression
    // it lands within the first few polls.
    for _ in 0..20 {
        assert!(
            generation.load(Ordering::Acquire) <= 12,
            "an unconfirmed bump recovery must never seed above the entry generation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The deposal observation that in production arrives within a renew
    // interval: the lose edge ends the wait and the gate discards.
    leader.on_lose();
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "an unconfirmed bump recovery must be discarded, not completed"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        12,
        "the generation must still be the entry generation after the discard"
    );
    Ok(())
}

/// The legitimate post-deletion bump still works: when the floor's
/// excess belongs to a dead predecessor and the lease loop completes a
/// post-claim Leading round, the recovery seeds the bumped target and
/// completes. Pairs with
/// `test_recovery_unconfirmed_bump_above_live_holder_is_discarded` to
/// pin both directions of the confirmation gate.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_confirmed_bump_seeds_and_completes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Only a dead predecessor's claim sits at our entry generation.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (12, 'dead-previous')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(12));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    // Parked after the claim INSERT and the rounds_at_claim snapshot:
    // simulate the lease loop completing one post-claim Leading round.
    let round = leader.begin_renew_round();
    leader.confirm_leading_round(round);
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        13,
        "a confirmed bump must exceed the dead predecessor's floor"
    );
    assert!(
        leader.recovery_complete(),
        "a confirmed bump recovery completes"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![
            (12, "dead-previous".to_string()),
            (13, "pod-us".to_string())
        ],
        "the bump is claimed; the predecessor's row stays"
    );
    Ok(())
}

/// A DAG-load failure must not skip the floor: the term still reads the
/// PG floor, claims its target, and (the target exceeds the entry
/// generation here) waits for the post-claim confirmation before
/// ungating dispatch — only the builds are lost. Saturated regime: the
/// durable floor sits well above the lease-derived entry generation, so
/// completing at the entry generation would advertise a generation
/// below every long-lived executor's `fetch_max` latch and its
/// dispatches would be silently rejected (the inversion this pins
/// against). Pairs with `test_recovery_failure_degrades_to_empty_dag`,
/// which closes the pool so the floor read fails too and pins the
/// floor-unreadable fallback's confirmed completion. Also pairs with
/// `test_recovery_load_failure_unconfirmed_bump_is_discarded` (the
/// unconfirmed direction).
// r[verify sched.recovery.fetch-max-seed+4]
// r[verify sched.lease.generation-claim+2]
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_load_failure_still_floors_claims_and_confirms() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Saturated regime: a dead predecessor's claim row far above the
    // lease-derived entry generation (2).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (40, 'dead-previous')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(2));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.fail_next_recovery_load = true;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    // Parked after the claim attempt and the rounds_at_claim snapshot:
    // simulate the lease loop completing one post-claim Leading round so
    // the bump confirmation can succeed.
    let round = leader.begin_renew_round();
    leader.confirm_leading_round(round);
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert_eq!(
        generation.load(Ordering::Acquire),
        41,
        "a load-failure term must still seed one past the durable floor, \
         not complete at the under-floor entry generation"
    );
    let claim: Option<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims \
         WHERE generation = 41",
    )
    .fetch_optional(&db.pool)
    .await?;
    assert_eq!(
        claim,
        Some((41, "pod-us".to_string())),
        "a load-failure term must durably claim its floored target"
    );
    assert!(
        leader.recovery_complete(),
        "floored, claimed, and confirmed: the load-failure term completes \
         (degrade, don't block — builds lost only)"
    );
    assert_eq!(
        handle.generation.advertised(),
        41,
        "heartbeats advertise the floored, claimed generation — not the \
         under-floor entry generation"
    );
    Ok(())
}

/// The negative arm of the load-failure case: a DAG-load failure with a
/// readable floor still requires the post-claim confirmation — with no
/// post-claim Leading round the recovery must be discarded, never
/// seeded, never completed, never advertised (the last also pins
/// claim-before-advertise for the discard direction). The discriminating
/// assertions are the absence poll and the final entry-generation check;
/// the completion/advertisement asserts also hold under a skipped wait
/// because `on_lose()` clears the stamp before they run. Pairs with
/// `test_recovery_load_failure_still_floors_claims_and_confirms` (the
/// confirmed direction). The leftover (41, 'pod-us') claim row is the
/// documented harmless over-claim and is deliberately not asserted,
/// same as the other discard tests.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_load_failure_unconfirmed_bump_is_discarded() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Saturated regime, same fixture as the confirmed-direction test:
    // a dead predecessor's claim row far above the entry generation (2).
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (40, 'dead-previous')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(2));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.fail_next_recovery_load = true;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    // Deliberately grant NO Leading round before releasing: the
    // confirmation can never arrive.
    release_tx.send(()).expect("actor still listening");

    // No lease loop is running, so no confirmation can ever arrive. The
    // seed to 41 must never land — on the unconfirmed-seed regression it
    // lands within the first few polls.
    for _ in 0..20 {
        assert!(
            generation.load(Ordering::Acquire) <= 2,
            "an unconfirmed load-failure bump must never seed above the entry generation"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The deposal observation that in production arrives within a renew
    // interval: the lose edge ends the wait and the gate discards.
    leader.on_lose();
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "an unconfirmed load-failure bump recovery must be discarded, not completed"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        2,
        "the generation must still be the entry generation after the discard"
    );
    assert_eq!(
        handle.generation.advertised(),
        0,
        "claim-before-advertise: a discarded load-failure recovery must not advertise"
    );
    Ok(())
}

/// A deposed-but-unaware leader must not complete a gap-retain recovery
/// either: when the durable floor sits more than one generation below
/// the entry generation (a predecessor died between its acquire edge
/// and its claim INSERT), the floor cannot vouch for the generations in
/// between -- a post-deletion successor may be live inside that gap,
/// below us. Retaining the entry generation therefore requires the same
/// post-claim tenure confirmation a bump target requires; with no
/// confirmation the recovery must be discarded, never completed. The
/// leftover (6, 'pod-us') claim row is the documented harmless
/// over-claim (it forces the next term above 6) and is deliberately not
/// asserted, same as the bump-discard test above.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_unconfirmed_gap_retain_below_entry_is_discarded() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // The last claimed generation is 4; generation 5 (a crashed,
    // never-claimed predecessor) and our entry 6 left no durable trace.
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (4, 'old-term')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(6));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    release_tx.send(()).expect("actor still listening");

    // No lease loop is running, so no confirmation can ever arrive. The
    // completion must never land -- on the unconfirmed-retain regression
    // it lands within the first few polls.
    for _ in 0..20 {
        assert!(
            !leader.recovery_complete(),
            "an unconfirmed gap-retain recovery must never complete"
        );
        assert_eq!(
            generation.load(Ordering::Acquire),
            6,
            "the generation must stay at the entry value"
        );
        tokio::time::sleep(Duration::from_millis(50)).await;
    }
    // The deposal observation that in production arrives within a renew
    // interval: the lose edge ends the wait and the gate discards.
    leader.on_lose();
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "an unconfirmed gap-retain recovery must be discarded, not completed"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        6,
        "the generation must still be the entry generation after the discard"
    );
    Ok(())
}

/// The legitimate gap-retain still works: a predecessor that died
/// between its acquire edge and its claim INSERT leaves a non-adjacent
/// floor with nobody live inside the gap -- once the lease loop
/// completes a post-claim Leading round, the recovery retains the entry
/// generation, claims it, and completes. Pairs with
/// `test_recovery_unconfirmed_gap_retain_below_entry_is_discarded` to
/// pin both directions of the gap-retain trigger.
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_gap_retain_with_confirmation_completes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) VALUES (4, 'old-term')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(6));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "a confirmed gap-retain recovery completes"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        6,
        "the entry generation is retained, not bumped"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(4, "old-term".to_string()), (6, "pod-us".to_string())],
        "the retained generation is claimed; the gap stays unclaimed"
    );
    Ok(())
}

/// The trigger boundary: a floor exactly one below the entry generation
/// is indistinguishable from an ordinary dead predecessor's, so no
/// confirmation is required and the recovery completes without any
/// lease loop running. When that adjacent row was in fact written by a
/// live post-deletion successor before our floor read, completing here
/// is the documented adjacent-floor-race residual (see the residual
/// list in the bump-confirm rationale); the narrowing conjunction it
/// needs -- this replica's renew rounds blind from the deletion through
/// its recovery gate, under the self-fence deadline -- is priced there.
/// The test also guards against an over-broad trigger: if adjacent
/// floors ever require confirmation, this only resolves after the
/// confirmation cap and the completion assertion fails.
#[tokio::test]
async fn test_recovery_adjacent_floor_retain_completes_without_confirmation() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    sqlx::query(
        "INSERT INTO leader_generation_claims (generation, holder_id) \
         VALUES (4, 'old-term'), (5, 'pod-live')",
    )
    .execute(&db.pool)
    .await?;

    let generation = Arc::new(AtomicU64::new(6));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "an adjacent floor vouches for the entry generation; no confirmation required"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        6,
        "the entry generation is retained"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![
            (4, "old-term".to_string()),
            (5, "pod-live".to_string()),
            (6, "pod-us".to_string())
        ],
        "the adjacent-floor retain claims the entry generation"
    );
    Ok(())
}

/// The entry-generation-above-floor arm of the claim target: when the
/// PG floor is NULL (fresh cluster) or below the generation at recovery
/// entry, recovery must claim exactly the entry generation -- not "one
/// past the floor", which here would be a different (lower) value. In
/// this fresh shape the entry generation IS the Lease-derived one. Pins
/// the arm of the decision table where the Lease, not the PG floor,
/// decides the generation, and guards against regressing the code
/// toward a floor-only reading of the rule. An empty floor cannot vouch
/// for entry generation 7, so the recovery completes only under a
/// post-claim Leading round -- the absent-floor arm of the confirmation
/// trigger.
// r[verify sched.recovery.fetch-max-seed+4]
// r[verify sched.recovery.bump-confirm+3]
#[tokio::test]
async fn test_recovery_claims_lease_derived_generation_on_empty_floor() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // No assignments, no claims: the PG floor is NULL.

    // gen_at_entry = 7: the generation at recovery entry. In the story
    // this fixture stages, that is the Lease-derived generation for
    // this epoch (prior holder changes bumped leaseTransitions while
    // PG stayed empty -- e.g. every prior term was deposed before
    // persisting, or proceeded unclaimed).
    let generation = Arc::new(AtomicU64::new(7));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "the absent-floor retain completes once a post-claim Leading round confirms"
    );
    assert_eq!(
        generation.load(Ordering::Acquire),
        7,
        "an empty PG floor demands nothing; the entry generation is retained"
    );
    let rows: Vec<(i64, String)> = sqlx::query_as(
        "SELECT generation, holder_id FROM leader_generation_claims ORDER BY generation",
    )
    .fetch_all(&db.pool)
    .await?;
    assert_eq!(
        rows,
        vec![(7, "pod-us".to_string())],
        "the claims ledger must record the entry generation, not a floor-derived one"
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
/// Drive the resubmit bound to LIMIT, restart the scheduler, resubmit →
/// bound MUST hold. The cross-cycle counter's durable carrier is the
/// `resubmit_reset` attempt-ledger row (the mirror column it once also
/// lived in was dropped by migration 075), so the seed here is the
/// ledger row the production resubmit path appends — recovery rebuilds
/// the counter from the ledger fold.
// r[verify sched.merge.poisoned-resubmit-bounded+4]
#[tokio::test]
async fn test_poisoned_recovery_preserves_resubmit_cycles() -> TestResult {
    use crate::state::POISON_RESUBMIT_RETRY_LIMIT;
    let f = RecoveryFixture::run(async |handle, pool| {
        seed_poisoned(&handle, "rs-cyc").await?;
        // Drive the resubmit bound to LIMIT via the durable carrier:
        // append the `resubmit_reset` ledger row the LIMIT-th resubmit
        // would have appended (cycle index = LIMIT). Status stays
        // 'poisoned' so recovery loads via `load_poisoned_derivations`,
        // and the ledger fold recovers `resubmit_cycles = LIMIT`.
        let derivation_id: Uuid =
            sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                .bind("rs-cyc")
                .fetch_one(&pool)
                .await?;
        let reset = crate::db::attempts::AttemptRow::new_reset(
            derivation_id,
            crate::state::OutcomeClass::ResubmitReset,
            crate::state::ReportingParty::Scheduler,
            POISON_RESUBMIT_RETRY_LIMIT as i32,
        );
        let mut tx = pool.begin().await?;
        crate::db::SchedulerDb::append_attempt(&mut tx, &reset).await?;
        tx.commit().await?;
        Ok(())
    })
    .await?;

    // After recovery: resubmit_cycles rebuilt from the ledger fold.
    let post = expect_drv(&f.handle, "rs-cyc").await;
    assert_eq!(post.status, DerivationStatus::Poisoned);
    assert_eq!(
        post.retry.resubmit_cycles, POISON_RESUBMIT_RETRY_LIMIT,
        "bug_001: recovery must rebuild resubmit_cycles from the ledger fold"
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

/// Recovery TOCTOU: if the lease flaps (lose→reacquire, generation
/// bumps) mid-recovery, discard the stale DAG instead of dispatching
/// from it with the NEW generation stamped on. If no bump, complete
/// normally (proves no false-positive — would regress every recovery
/// test).
///
/// Timeline (bump case): actor snapshots gen=2 → runs recover_from_pg
/// → parks at gate → [test simulates a lease flap's generation bump:
/// fetch_add gen 2→3] → release → actor re-loads gen=3, sees 3≠2 →
/// DISCARD (and never stamps a completion for this attempt's epoch).
/// Pre-epoch-stamp, an unconditional boolean `store(true)` here could
/// clobber the lease loop's clear → dispatch_ready fired with a gen-2
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
    // Entry generation 2 over an empty PG floor: the floor cannot vouch
    // for it, so the no-bump case completes only under a post-claim
    // Leading round -- simulate a healthy lease loop. The bump case
    // discards on the generation signal regardless.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");

    if bump_gen {
        // Simulate the lease flap's generation signal (the gate keys on
        // its own entry snapshot; the production-writer flap shapes are
        // exercised by the saturated-regime and rebound tests below).
        generation.fetch_add(1, Ordering::Release);
    }
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert_eq!(
        leader.recovery_complete(),
        expect_recovery_complete,
        "bump_gen={bump_gen}: recovery_complete must be {expect_recovery_complete}"
    );
    if !bump_gen {
        assert_eq!(generation.load(Ordering::Acquire), 2, "gen unchanged");
    }
    Ok(())
}

/// Counter partition: a discarded recovery must produce exactly ONE
/// `rio_scheduler_recovery_total` increment (its `discarded_*`
/// disposition), never an additional pre-gate `success`/`failure` —
/// and an applied recovery counts exactly once as `success`.
///
/// Recorder mechanics: the increments fire inside the SPAWNED actor
/// task, so the thread-local `set_default_local_recorder` pattern used
/// elsewhere would observe nothing here; install the
/// `DebuggingRecorder` process-globally before the actor spawns. Safe
/// under nextest's process-per-test model — do not move this test to a
/// shared-process runner.
#[tokio::test]
async fn test_discarded_recovery_increments_recovery_total_once() -> TestResult {
    use metrics_util::debugging::DebuggingRecorder;

    use crate::sla::metrics::counter_map_by;

    let rec = DebuggingRecorder::new();
    let snap = rec.snapshotter();
    rec.install().expect("install global debugging recorder");

    let db = TestDb::new(&MIGRATOR).await;
    let generation = Arc::new(AtomicU64::new(2));
    let leader = crate::lease::LeaderState::from_parts(
        Arc::clone(&generation),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    let _confirmations = spawn_leading_confirmations(leader.clone());
    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    // Recovery #1: park at the gate, flap the lease (lose + reacquire
    // at a bumped generation), release — the gate discards.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");
    generation.fetch_add(1, Ordering::Release);
    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    let by_outcome = counter_map_by(&snap, "rio_scheduler_recovery_total", Some("outcome"));
    assert_eq!(
        by_outcome.get("discarded_flap").copied(),
        Some(1),
        "discarded recovery must count exactly once as discarded_flap: {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("success").copied().unwrap_or(0),
        0,
        "a discarded recovery must NOT also count as success: {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("failure").copied().unwrap_or(0),
        0,
        "a discarded recovery must NOT count as failure: {by_outcome:?}"
    );

    // Recovery #2: clean run at the new generation — applied, so it
    // counts exactly once as success with no further discard. The
    // snapshotter DRAINS on read (see counter_map_by's caveat), so this
    // second decode sees only the deltas since the post-discard
    // snapshot above.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    assert!(
        leader.recovery_complete(),
        "second (clean) recovery should complete"
    );
    let by_outcome = counter_map_by(&snap, "rio_scheduler_recovery_total", Some("outcome"));
    assert_eq!(
        by_outcome.get("success").copied().unwrap_or(0),
        1,
        "applied recovery counts exactly once as success: {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("discarded_flap").copied().unwrap_or(0),
        0,
        "no additional discard increment on the second recovery: {by_outcome:?}"
    );
    assert_eq!(
        by_outcome.get("failure").copied().unwrap_or(0),
        0,
        "no failure outcome for the applied recovery: {by_outcome:?}"
    );
    Ok(())
}

/// Recovery TOCTOU in the saturated-generation regime: after a
/// `kubectl delete lease`, the PG floor seeds the generation well past
/// `leaseTransitions + 1`, and from then on `on_acquire`'s `fetch_max`
/// is a generation no-op on every holder change. A foreign term (we
/// lose, another replica leads and dispatches, we re-steal) that lands
/// entirely inside our recovery window therefore leaves the generation
/// untouched — the gate must key on the recorded acquire-transitions
/// and on `is_leader`, not on the generation alone, to discard the
/// stale recovery.
///
/// Unlike `test_recovery_toctou_on_lease_flap` (manual `fetch_add`),
/// this drives the PRODUCTION transition functions
/// (`on_lose`/`on_acquire`), so the gate sees exactly what the lease
/// loop writes in this regime.
// r[verify sched.recovery.gate-dispatch]
#[rstest::rstest]
#[case::foreign_term_discards(Some(7), false)]
#[case::same_epoch_completes(Some(5), true)]
#[case::lost_without_reacquire_discards(None, false)]
#[tokio::test]
async fn test_recovery_toctou_saturated_generation_flaps(
    #[case] reacquire_transitions: Option<u64>,
    #[case] expect_recovery_complete: bool,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Saturated regime: a previous term seeded generation 13 from the
    // PG floor while the recreated Lease counts transitions from ~0.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(13)),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    // Production acquire edge for the term under test: the generation
    // fetch_max is a no-op against 13; the transition count is 5.
    leader.on_acquire(5);
    // Entry 13 over an empty floor cannot be vouched for, so the
    // same-epoch case completes only under a post-claim Leading round.
    // Not load-bearing for the two discard cases — they discard on the
    // transitions / is_leader signal regardless.
    let _confirmations = spawn_leading_confirmations(leader.clone());

    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");

    // The flap, via production transitions: on_lose clears is_leader +
    // recovery_complete; a re-acquire in this regime never moves the
    // generation, whatever the new transition count is.
    leader.on_lose();
    if let Some(transitions) = reacquire_transitions {
        leader.on_acquire(transitions);
    }

    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert_eq!(
        leader.recovery_complete(),
        expect_recovery_complete,
        "reacquire_transitions={reacquire_transitions:?}: recovery_complete must be \
         {expect_recovery_complete}"
    );
    // In ALL cases the generation never moves — any discard above came
    // from the transitions/is_leader signal, not from a generation
    // change.
    assert_eq!(leader.generation(), 13, "generation stays saturated");
    Ok(())
}

/// The recovery TOCTOU gate must discard on the production rebound
/// writer (`LeaderState::on_rebound`) — the lease loop's translation of
/// a holder change observed late, on a still-leading round (a foreign
/// term that vacated, or a delete/recreate, entirely inside our
/// observation gap). The rebound moves the recorded acquire-transitions
/// without any lose/acquire edge, so the gate's transitions signal is
/// what catches it; the follow-up `LeaderAcquired` the re-fired hook
/// sends in production (the first back-to-back LeaderAcquired with no
/// intervening LeaderLost) then re-runs recovery to completion.
// r[verify sched.recovery.gate-dispatch]
#[tokio::test]
async fn test_recovery_toctou_rebound_mid_recovery_discards_then_rerun_completes() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Saturated regime, same fixture as the saturated-flaps test: the
    // rebound's generation fetch_max is a no-op against 13 and only the
    // recorded transition count moves.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(13)),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    leader.on_acquire(5);
    // The first run claims (13, holder) before parking at the gate, so
    // the re-run retains on its own claim row; the confirmation loop
    // covers the first run's non-vouched empty floor (and keeps the
    // completion leg valid if the trigger ever widens further).
    let _confirmations = spawn_leading_confirmations(leader.clone());

    let (reached_tx, reached_rx) = oneshot::channel();
    let (release_tx, release_rx) = oneshot::channel();

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.recovery_toctou_gate = Some((reached_tx, release_rx));
    });

    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    tokio::time::timeout(Duration::from_secs(10), reached_rx)
        .await
        .expect("actor reached gate")
        .expect("reached_tx not dropped");

    // The unobserved holder change, observed late: the lease loop's
    // rebound re-records the count and clears recovery_complete — no
    // on_lose, no acquire edge.
    leader.on_rebound(7);

    release_tx.send(()).expect("actor still listening");
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "a recovery that straddled a rebound must be discarded"
    );
    assert_eq!(leader.generation(), 13, "generation stays saturated");

    // What the re-fired acquire hook does in production: a second
    // LeaderAcquired with no intervening LeaderLost. The re-run loads
    // the post-change state and completes.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        leader.recovery_complete(),
        "the re-run triggered by the rebound completes"
    );
    assert_eq!(
        leader.generation(),
        13,
        "generation stays saturated after the re-run"
    );
    assert_eq!(
        leader.acquired_transitions(),
        7,
        "the rebound's recorded count is what the re-run entered with"
    );
    Ok(())
}

/// Behavior pin for the end state the hook-ordering guarantee protects
/// (`sched.lease.hook-order`): the tick-time self-fence false alarm
/// delivers `LeaderLost` then `LeaderAcquired` (same epoch) in that
/// order, the lost arm wipes, the acquired arm re-recovers from PG, and
/// the leader ends recovered-and-dispatchable — DAG present,
/// `recovery_complete = true`. The inverted order (what unordered
/// per-spawn delivery allowed) would end with `is_leader = true`,
/// `recovery_complete = false`, and an empty DAG — dispatch gated and
/// every non-terminal build stalled until the next lease transition
/// re-runs recovery. Not red-first: the
/// ordering itself is pinned red-first by the `lease_hooks` unit test;
/// this pins what the order buys at the actor level.
// r[verify sched.lease.hook-order]
#[tokio::test]
async fn test_false_alarm_lost_then_acquired_in_order_ends_recovered() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Leading at generation 2 (transitions=1), recovery already complete
    // — the steady state a false alarm interrupts.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(2)),
        Arc::new(AtomicBool::new(true)),
        false,
    );
    leader.on_acquire(1);
    // Steady state under the current epoch: recovery already complete.
    leader.set_recovery_complete(leader.acquired_transitions());
    // Same posture as the rebound completion test: keep the re-acquire's
    // confirmation leg satisfiable if the bump-confirm trigger ever
    // widens; the same-epoch path itself does not need it.
    let _confirmations = spawn_leading_confirmations(leader.clone());

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
        p.holder_id = "pod-us".into();
    });

    // A populated DAG owned by the current term, persisted to PG by the
    // merge path (what the re-recovery below reloads).
    let build_id = Uuid::new_v4();
    let _event_rx = merge_single_node(
        &handle,
        build_id,
        "false-alarm-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // The false-alarm tick, in invocation order: the self-fence fires
    // the lose (state first, then the hook command), the same tick's
    // successful renew fires the acquire at the SAME epoch.
    leader.on_lose();
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    leader.on_acquire(1);
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;

    assert!(
        leader.is_leader(),
        "the false-alarm re-acquire leaves us leading"
    );
    assert!(
        leader.recovery_complete(),
        "the in-order pair must end with recovery complete (dispatchable)"
    );
    assert_eq!(
        leader.generation(),
        2,
        "a same-epoch false alarm must not move the generation"
    );
    let drv = expect_drv(&handle, "false-alarm-drv").await;
    assert!(
        !drv.status.is_terminal(),
        "the re-recovered DAG must contain the term's derivation, got {:?}",
        drv.status
    );
    Ok(())
}

/// Shared staging for the cross-build recovery-condemnation tests below
/// (the bug_009 shape with a within-TTL poison instead of a cancel):
///
///  - build A full-merges `xrc-parent`→`xrc-child` (A owns both rows and
///    declares the edge);
///  - build B merges `xrc-parent` alone (B co-owns the parent but never
///    the child — the pruning-build shape: interested in its kept root,
///    never in the root's closure);
///  - PG is then backdated to the post-crash shape: child poisoned
///    within TTL (poisoned under A's ownership), parent substituting
///    (its detached fetch died with the old leader), build A failed
///    (the natural consequence of its own child's poison), build B
///    still active.
///
/// The child's `poisoned_at` is future-dated by one hour so the
/// within-TTL load is deterministic regardless of host speed: the
/// cfg(test) `POISON_TTL` is 100ms, and a real `now()` timestamp could
/// expire between the backdate and `load_poisoned_derivations` on a
/// loaded CI host (`from_poisoned_row` clamps the negative elapsed to 0
/// — a fresh full TTL at recovery time).
async fn seed_cross_build_poisoned_dep(
    handle: ActorHandle,
    pool: sqlx::PgPool,
    build_a: Uuid,
    build_b: Uuid,
) -> anyhow::Result<()> {
    let _rxa = merge_dag(
        &handle,
        build_a,
        vec![make_node("xrc-parent"), make_node("xrc-child")],
        vec![make_test_edge("xrc-parent", "xrc-child")],
        false,
    )
    .await?;
    let _rxb = merge_dag(
        &handle,
        build_b,
        vec![make_node("xrc-parent")],
        vec![],
        false,
    )
    .await?;
    barrier(&handle).await;
    drop(handle);
    sqlx::query(
        "UPDATE derivations SET status = 'poisoned', poisoned_at = now() + interval '1 hour' \
         WHERE drv_hash = 'xrc-child'",
    )
    .execute(&pool)
    .await?;
    // Post-080 image of the legacy mid-substitution shape (the 080
    // data step rewrote such rows to 'queued').
    sqlx::query("UPDATE derivations SET status = 'queued' WHERE drv_hash = 'xrc-parent'")
        .execute(&pool)
        .await?;
    sqlx::query("UPDATE builds SET status = 'failed' WHERE build_id = $1")
        .bind(build_a)
        .execute(&pool)
        .await?;
    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
/// The recovery condemnation MUST be scoped by build co-ownership (the
/// `sched.recovery.failed-dep-cascade+2` MUST NOT clause): a recovered
/// parent above a within-TTL poisoned child is condemned only when a
/// LIVE build co-owns that child with it. A parent whose only
/// failed-child evidence belongs to a dead build (or to a live build
/// that never owned the parent) recovers normally — Queued above the
/// still-loaded poisoned child — and its owning build stays Active.
///
/// Both recovery condemnation mechanisms must honor the scoping: the
/// cascade pre-pass (`load_parents_with_failed_deps` — already scoped,
/// bug_341/bug_009) and the in-DAG recompute (`compute_initial_states` /
/// `any_co_owned_dep_terminally_failed` — the Wave-2 residual finding).
/// The within-TTL poisoned child IS loaded with its edge
/// (`sched.recovery.poisoned-failed-count`), so only the in-DAG
/// recompute sees it; pre-fix that recompute used the UNSCOPED
/// `any_dep_terminally_failed`, condemned the parent to DependencyFailed
/// (persisted at recovery) and `finalize_recovered_builds` terminally
/// failed build B — a wrongful C3-class failure on cross-build evidence,
/// even though B's own work could still succeed once the poison clears.
#[tokio::test]
async fn test_recovery_cross_build_poisoned_dep_spares_non_co_owning_parent() -> TestResult {
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        seed_cross_build_poisoned_dep(handle, pool, build_a, build_b).await
    })
    .await?;

    // Precondition: the within-TTL poisoned child is loaded into the DAG
    // (with its edge) for TTL tracking. If this fails the test is
    // exercising the dropped-edge path, not the residual scenario.
    let child = expect_drv(&f.handle, "xrc-child").await;
    assert_eq!(
        child.status,
        DerivationStatus::Poisoned,
        "fixture premise: within-TTL poisoned child is loaded at recovery"
    );

    // The parent must NOT be condemned: its only failed-child evidence
    // belongs to dead build A. It recovers Queued above the still-loaded
    // poisoned child, waiting for the poison to clear.
    let parent = expect_drv(&f.handle, "xrc-parent").await;
    assert_eq!(
        parent.status,
        DerivationStatus::Queued,
        "a recovered parent above a non-co-owned poisoned child must NOT be \
         condemned (sched.recovery.failed-dep-cascade+2 MUST NOT clause), \
         got {:?}",
        parent.status
    );

    // Build B (the parent's only live owner) must stay Active — failing
    // it here is exactly the wrongful terminal failure the scoping
    // prevents.
    let status = query_status(&f.handle, build_b).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build B must stay Active (its parent was spared by co-ownership \
         scoping); error_summary: {:?}",
        status.error_summary
    );

    // The wrongful condemnation must not be persisted either (pre-fix it
    // was, via the recovery DependencyFailed persist).
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'xrc-parent'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_ne!(
        pg_status, "dependency_failed",
        "the spared parent's PG row must not carry the wrongful dependency_failed"
    );

    Ok(())
}

// r[verify sched.poison.clear-survivor-reevaluation+2]
// r[verify sched.recovery.failed-dep-cascade+2]
/// What un-blocks the parent spared by the co-ownership scoping (test
/// above): the poison-clear removal. When the non-co-owned child's
/// poison is cleared — admin `ClearPoison` or the poison-TTL sweep —
/// the surviving parent must be re-evaluated: it is now childless, all
/// dependencies vacuously satisfied, so it MUST be promoted to Ready
/// and pushed for dispatch, and its PG row updated.
///
/// Without the re-evaluation the spared parent sits Queued forever (no
/// completion event will ever fire for the removed child — the exact
/// hang shape the unscoped condemnation was preventing) and build B
/// never makes progress. Scoping and survivor re-evaluation are two
/// halves of one fix; this test pins the second half on both removal
/// paths.
#[rstest::rstest]
#[case::admin_clear(false)]
#[case::ttl_expiry(true)]
#[tokio::test]
async fn test_poison_clear_reevaluates_spared_recovered_parent(
    #[case] via_ttl: bool,
) -> TestResult {
    let build_a = Uuid::new_v4();
    let build_b = Uuid::new_v4();
    let f = RecoveryFixture::run(async |handle, pool| {
        seed_cross_build_poisoned_dep(handle, pool, build_a, build_b).await
    })
    .await?;

    // Recovery left the spared parent Queued above the loaded poisoned
    // child (the half-1 test pins this in detail).
    assert_eq!(
        expect_drv(&f.handle, "xrc-parent").await.status,
        DerivationStatus::Queued,
        "fixture premise: the spared parent recovered Queued"
    );

    // Clear the child's poison: admin ClearPoison, or the cfg(test)
    // 100ms TTL + a Tick. (The recovered in-memory `poisoned_at` is
    // recovery-time — `from_poisoned_row` clamps the future-dated row's
    // negative elapsed to 0 — so the TTL sweep fires after a real
    // ~100ms sleep.)
    if via_ttl {
        tokio::time::sleep(crate::state::POISON_TTL + std::time::Duration::from_millis(250)).await;
        tick(&f.handle).await?;
    } else {
        let (tx, rx) = oneshot::channel();
        f.handle
            .send_unchecked(ActorCommand::ClearPoison {
                drv_hash: "xrc-child".into(),
                reply: tx,
            })
            .await?;
        assert!(rx.await?, "ClearPoison → cleared=true");
    }
    assert!(
        f.handle
            .debug_query_derivation("xrc-child")
            .await?
            .is_none(),
        "the poisoned child must be removed from the DAG"
    );

    // The surviving parent must be promoted: Queued + childless →
    // all deps vacuously satisfied → Ready (and pushed for dispatch).
    let parent = expect_drv(&f.handle, "xrc-parent").await;
    assert_eq!(
        parent.status,
        DerivationStatus::Ready,
        "the poison-clear removal must re-evaluate the surviving parent: \
         Queued + now-childless → Ready (sched.poison.clear-survivor-reevaluation), \
         got {:?}",
        parent.status
    );

    // The promotion is persisted (so a second failover doesn't reload a
    // stale 'substituting'/'queued' row).
    let (pg_status,): (String,) =
        sqlx::query_as("SELECT status FROM derivations WHERE drv_hash = 'xrc-parent'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(
        pg_status, "ready",
        "the survivor promotion must be persisted to PG"
    );

    // Build B is still Active and now has dispatchable work.
    let status = query_status(&f.handle, build_b).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build B must still be Active with its parent now dispatchable"
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

/// Phase-1 staging for the bug_009 regression test below: build `b2`
/// full-merges parent→child (the `derivation_edges` row and B2's
/// `build_derivations` links to BOTH rows are persisted), build `b1`
/// is the single-node re-request that links ONLY the parent (no
/// edges). After the merges the phase-1 handle is dropped and the
/// persisted rows are backdated to the bug_009 crash shape: the child
/// went `cancelled` when `b2` was cancelled
/// (`builds.status = 'cancelled'`), the parent is left in the legacy
/// mid-substitution status (the PD-D3 decode arm absorbs it), and `b1`
/// stays `active`. The parent declares outputs `[out, debug]` with
/// stored wanted `'{}'` (= all declared) and `expected_output_paths`
/// `[out_path, dbg_path]` so the phase-2 store staging decides the
/// post-failover route. (The marked variants of this staging died with
/// the keeps-half recovery pins in T-D5.1 — the durable classifier's
/// stale-voucher direction is pinned by
/// classify_durable_evidence_ignores_dead_voucher.)
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
    // Post-080 image of the legacy mid-substitution shape (the 080
    // data step rewrote such rows to 'queued').
    sqlx::query("UPDATE derivations SET status = 'queued' WHERE drv_hash = $1")
        .bind(root)
        .execute(pool)
        .await?;
    sqlx::query("UPDATE builds SET status = 'cancelled' WHERE build_id = $1")
        .bind(b2)
        .execute(pool)
        .await?;
    Ok(())
}

// r[verify sched.recovery.failed-dep-cascade+2]
/// The bug_009 shape (the gateway single-node-fallback: B1 submitted
/// just the parent, no prune involved). Another build's cancelled
/// child must
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

    // The node recovered childless and unmarked → it is deliverable from
    // source through the pull path.
    tick(&handle).await?;
    let a = pull_attempt(&handle, "bug9u-root").await;
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

// r[verify sched.recovery.gate-dispatch]
/// A self-fence false alarm during a long recovery queues `LeaderLost`
/// then a second `LeaderAcquired`; the same-epoch keep stamps the
/// in-flight recovery's completion after the re-acquire, so when the
/// actor later processes that queued `LeaderLost` it wipes the very
/// persisted state the completion certified. The lost handler must
/// invalidate the kept completion together with the wipe — otherwise
/// `is_leader=true` + `recovery_complete()=true` over an empty DAG
/// ungates dispatch and the heartbeat advertisement until the follow-up
/// recovery re-stamps. The follow-up `LeaderAcquired` must then
/// re-establish completion (the invalidation can never gate dispatch
/// permanently).
#[tokio::test]
async fn test_leader_lost_invalidates_kept_recovery_completion() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    // Leading at transition count 5 with a completed recovery — the
    // state an in-flight same-epoch keep starts from. The claims floor
    // is empty, so the green leg's re-recovery claims and then waits
    // for a post-claim Leading round; keep the confirmation loop
    // running for the whole test.
    let leader = crate::lease::LeaderState::from_parts(
        Arc::new(AtomicU64::new(6)),
        Arc::new(AtomicBool::new(false)),
        false,
    );
    leader.on_acquire(5);
    leader.set_recovery_complete(5);
    let _confirmations = spawn_leading_confirmations(leader.clone());

    let l = leader.clone();
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, move |_, p| {
        p.leader = l;
    });

    // Populate some persisted state so the LeaderLost wipe is not a
    // no-op. The wipe itself is long-standing handle_leader_lost
    // behavior; this test pins what happens to the completion stamp
    // and to the worker-visible advertisement.
    merge_single_node(
        &handle,
        Uuid::new_v4(),
        "lost-keep-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;

    // The self-fence false alarm, via the production transition
    // functions: lose, then a same-count re-acquire. The kept in-flight
    // recovery stamps its completion AFTER the re-acquire (production
    // ordering: the actor finishes handle_leader_acquired before the
    // queued LeaderLost is processed).
    leader.on_lose();
    leader.on_acquire(5);
    leader.set_recovery_complete(5);
    assert!(
        leader.recovery_complete() && leader.is_leader(),
        "precondition: kept completion + leadership before the queued \
         LeaderLost is processed"
    );

    // The actor now processes the queued LeaderLost from the false
    // alarm and wipes the persisted state that completion certified.
    handle.send_unchecked(ActorCommand::LeaderLost).await?;
    barrier(&handle).await;

    assert!(
        !leader.recovery_complete(),
        "processing LeaderLost must invalidate the kept completion: the \
         state it certified is gone, so dispatch must stay recovery-gated \
         until the follow-up LeaderAcquired re-runs recovery"
    );
    assert_eq!(
        handle.generation.advertised(),
        0,
        "heartbeats must advertise 0 (not the stale generation) over the \
         wiped DAG"
    );
    assert!(
        leader.is_leader(),
        "invalidation must not touch leadership — the lease loop owns it \
         and the same-count re-acquire is still live"
    );

    // Green leg: the follow-up LeaderAcquired re-runs recovery and
    // re-establishes completion — the invalidation cannot gate dispatch
    // permanently.
    handle.send_unchecked(ActorCommand::LeaderAcquired).await?;
    barrier(&handle).await;
    assert!(
        leader.recovery_complete(),
        "the follow-up LeaderAcquired's recovery must re-establish completion"
    );
    Ok(())
}

// ---------------------------------------------------------------------------
// Attempt ledger (drv_attempts, Phase 1a): acceptance battery.
// A-1a-1: every attempt produces exactly one row (full mixed-channel
//         scenario, asserted step by step).
// A-1a-4: the attempt history reloads identically across a failover.
// A-1a-3 (no row stays disconnected/NULL forever) is covered by
//         attempt_ledger_unreported_crash_established_by_sweep in
//         actor/tests/executor.rs.
// ---------------------------------------------------------------------------

// r[verify sched.retry.recovery-projection+3]
/// Companion guard for T-1b.12b: recovery over an under-budget history
/// poisons nothing (the mass-poison regression guard) — the node stays
/// dispatchable with its recovered counters intact (the fold of the
/// reloaded attempt suffix).
#[tokio::test]
async fn phase1b_recovery_under_budget_history_poisons_nothing() -> TestResult {
    let drv_hash = "cvg-under-drv";
    let f = RecoveryFixture::run(async move |handle, pool| {
        let _ev =
            merge_single_node(&handle, Uuid::new_v4(), drv_hash, PriorityClass::Scheduled).await?;
        barrier(&handle).await;
        let derivation_id: Uuid =
            sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                .bind(drv_hash)
                .fetch_one(&pool)
                .await?;
        let mut tx = pool.begin().await?;
        for w in ["cvg-u1", "cvg-u2"] {
            let mut row = crate::db::attempts::AttemptRow::new(
                derivation_id,
                crate::state::OutcomeClass::Transient,
                crate::state::ReportingParty::Worker,
            );
            row.executor_id = Some(w.into());
            // The exclusion/budget key is the source node (P12): seed
            // the rows in the bound shape so the recovered fold carries
            // two distinct source keys.
            row.source_node = Some(w.to_string());
            crate::db::SchedulerDb::append_attempt(&mut tx, &row).await?;
        }
        tx.commit().await?;
        Ok(())
    })
    .await?;
    let handle = f.handle;

    let info = expect_drv(&handle, drv_hash).await;
    assert!(
        !info.status.is_terminal(),
        "an under-budget history must not poison at recovery, got {:?}",
        info.status
    );
    assert_eq!(
        info.retry.failed_builders.len(),
        2,
        "the recovered counters are intact"
    );
    Ok(())
}

/// Red-first for T-1b.12b: an at-budget attempt history whose verdict
/// was never persisted (the rows are committed, the status is still
/// `ready` — the crash-between-stamp-and-persist shape, seeded directly
/// via `db::attempts` because recovery must converge from any committed
/// ledger state) converges to its terminal verdict AT RECOVERY: the
/// derivation is Poisoned in memory and PG, the cascade reaches its
/// dependents exactly as a runtime poison would, and the interested
/// build fails.
#[tokio::test]
async fn phase1b_recovery_enforces_at_budget_verdict() -> TestResult {
    let build_id = Uuid::new_v4();
    let f = RecoveryFixture::run(async move |handle, pool| {
        // Parent depends on child; nobody is dispatched (no executors).
        merge_chain(
            &handle,
            build_id,
            &["cvg-child", "cvg-parent"],
            PriorityClass::Scheduled,
        )
        .await?;
        barrier(&handle).await;
        // The at-budget history: three distinct executors' counted
        // failures, committed to the ledger with NO status persist —
        // the verdict the old leader never got to act on.
        let derivation_id: Uuid =
            sqlx::query_scalar("SELECT derivation_id FROM derivations WHERE drv_hash = $1")
                .bind("cvg-child")
                .fetch_one(&pool)
                .await?;
        let mut tx = pool.begin().await?;
        for w in ["cvg-w1", "cvg-w2", "cvg-w3"] {
            let mut row = crate::db::attempts::AttemptRow::new(
                derivation_id,
                crate::state::OutcomeClass::Transient,
                crate::state::ReportingParty::Worker,
            );
            row.executor_id = Some(w.into());
            row.error_msg = Some("counted failure with no persisted verdict".into());
            crate::db::SchedulerDb::append_attempt(&mut tx, &row).await?;
        }
        tx.commit().await?;
        let status: String =
            sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'cvg-child'")
                .fetch_one(&pool)
                .await?;
        assert_eq!(
            status, "ready",
            "fixture precondition: the verdict was never persisted"
        );
        Ok(())
    })
    .await?;
    let handle = f.handle;

    // The node converges to its terminal verdict at recovery — before
    // any further failure event or backstop tick.
    let child = expect_drv(&handle, "cvg-child").await;
    assert_eq!(
        child.status,
        DerivationStatus::Poisoned,
        "an at-budget attempt history with no persisted verdict must \
         converge to Poisoned at recovery (T-1b.12b)"
    );
    let pg_status: String =
        sqlx::query_scalar("SELECT status FROM derivations WHERE drv_hash = 'cvg-child'")
            .fetch_one(&f.db.pool)
            .await?;
    assert_eq!(
        pg_status, "poisoned",
        "the recovery-time verdict must be persisted"
    );

    // The poison at recovery cascades exactly as a runtime poison would.
    let parent = expect_drv(&handle, "cvg-parent").await;
    assert_eq!(
        parent.status,
        DerivationStatus::DependencyFailed,
        "the recovery-time poison must cascade to dependents"
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "the interested build observes the recovery-time poison"
    );
    Ok(())
}
