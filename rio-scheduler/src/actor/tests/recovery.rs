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

    // A puller is available — the doomed from-source delivery has
    // somewhere to go if the restore+guard are missing.
    tick(&handle).await?;

    // No from-source delivery ever happens for the dep-less node.
    let pull = try_pull_attempt(&handle, "fov-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "childless topdown-pruned root must never be dispatched from source \
         after failover (its inputDrvs were never merged — worker would ENOENT); got {pull:?}"
    );
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
    tick(&handle3).await?;

    let s2 = query_status(&handle3, build2).await?;
    assert_ne!(
        s2.state,
        rio_proto::types::BuildState::Failed as i32,
        "resubmitted build must not be fail-fasted again after the next failover \
         (stale topdown_pruned re-armed the guard); error={:?}",
        s2.error_summary
    );
    let a = pull_attempt(&handle3, "ffc-root").await;
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

    // A puller is available — the parent should be deliverable from
    // source once the stale mark is dropped.
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
    let a = pull_attempt(&handle, "tdcp-root").await;
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

    // A puller is available — the doomed from-source delivery has
    // somewhere to go if the kept mark fails to gate it.
    tick(&handle).await?;

    // No from-source delivery ever happens for the parent: its closure
    // was never merged for B1, so a from-source build would ENOENT.
    let pull = try_pull_attempt(&handle, "tdhist-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "a pruned root whose produced children belong to a terminal build \
         must not be dispatched from source after failover; got {pull:?}"
    );
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
// r[verify sched.evidence.closure-hole]
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

    // A puller is available — the doomed from-source delivery has
    // somewhere to go if the produced survivor launders the clear.
    tick(&handle).await?;

    // No from-source delivery ever happens for the parent: its pruned
    // closure was truncated by the reap, so a from-source build would
    // ENOENT.
    let pull = try_pull_attempt(&handle, "tdvh-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "a closure-holed pruned root must not be dispatched from source after \
         failover, however produced its surviving persisted children look; got {pull:?}"
    );
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

    // A puller is available — a wrongful from-source delivery would have
    // somewhere to land.
    tick(&handle).await?;
    // Deterministic end state: the kept mark routes the node through the
    // substitution carve-out and the detached fetch completes it.
    wait_for_status(&handle, "bug9s-root", DerivationStatus::Completed).await;

    // No from-source delivery ever happened for the parent (substitution,
    // not a from-source build).
    let pull = try_pull_attempt(&handle, "bug9s-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "the parent must complete via substitution, never via a from-source \
         dispatch (its closure was never merged for B1); got {pull:?}"
    );
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

    // A puller is available — the doomed from-source delivery has
    // somewhere to go if the kept mark fails to gate it.
    tick(&handle).await?;

    // No from-source delivery ever happens for the parent: its closure
    // was never merged for B1, so a from-source build would ENOENT.
    let pull = try_pull_attempt(&handle, "bug9f-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "a pruned root with an unsatisfiable wanted set must take the bounded \
         fail-fast, never a from-source dispatch; got {pull:?}"
    );
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
// r[verify sched.evidence.closure-hole]
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
    tick(&handle).await?;
    let a = pull_attempt(&handle, "bug6t3-keep").await;
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
    pull_complete_success(&handle, "bug6t3-keep", &keep_out).await?;
    barrier(&handle).await;
    assert_eq!(
        expect_drv(&handle, "bug6t3-keep").await.status,
        DerivationStatus::Completed,
        "fixture premise: the surviving sibling completed under its live build"
    );

    // No from-source delivery ever happens for P: its pruned closure was
    // truncated at recovery, so a from-source build would ENOENT on the
    // never-produced subtree.
    let pull = try_pull_attempt(&handle, "bug6t3-root").await;
    assert!(
        !matches!(pull, Ok(crate::actor::pull::PullOutcome::Deliver(_))),
        "the surviving sibling's completion must not launder the mark of a \
         parent whose un-produced terminal child was dropped at recovery; got {pull:?}"
    );
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
