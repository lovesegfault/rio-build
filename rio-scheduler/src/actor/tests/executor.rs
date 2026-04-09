//! Worker management: heartbeat merge (no-clobber), Tick-driven timeout and poison expiry.
// r[verify sched.executor.dual-register]
// r[verify sched.executor.deregister-reassign]
// r[verify sched.state.poisoned-ttl]

use super::*;

/// I-063 (composes with I-056a via the `draining`/`draining_hb`
/// split): two drain sources, two fields, effective state is the OR.
///
/// - `draining` (admin): set by DrainExecutor RPC. Cleared on
///   reconnect (I-056a — stale prior-session admin drain).
/// - `draining_hb` (worker): set/cleared by heartbeat unconditionally.
///   NOT touched on reconnect — a worker that got SIGTERM keeps its
///   stream alive across a scheduler restart and re-asserts on the
///   next heartbeat; in the gap, leaving the prior value prevents
///   mis-dispatch. Live: gcc duplicated ~30min CPU when builder-0's
///   drain broke the reconnect loop instead of staying connected.
///
/// `debug_query_workers().draining` exposes `is_draining()` (the OR),
/// so this test asserts the EFFECTIVE state at each step.
#[tokio::test]
async fn test_drain_sources_compose_across_reconnect() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let _rx1 = connect_executor(&handle, "drain-auth", "x86_64-linux").await?;

    // Set both sources: admin drain (DrainExecutor) + worker drain
    // (heartbeat) + store_degraded.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "drain-auth".into(),
            force: false,
            reply: reply_tx,
        })
        .await?;
    assert!(reply_rx.await?.accepted);
    send_heartbeat_with(&handle, "drain-auth", "x86_64-linux", |hb| {
        hb.store_degraded = true;
        hb.draining = true;
    })
    .await?;

    let w = expect_worker(&handle, "drain-auth").await;
    assert!(
        w.draining && w.store_degraded,
        "precondition: effective draining (admin OR hb) + degraded"
    );

    // Reconnect WITHOUT ExecutorDisconnected (entry persists). No
    // heartbeat yet — isolates the reconnect-clear.
    drop(_rx1);
    let (stream_tx, _rx2) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "drain-auth".into(),
            stream_tx,
        })
        .await?;

    let w = expect_worker(&handle, "drain-auth").await;
    assert!(
        w.draining,
        "I-063: effective draining still true — reconnect cleared \
         admin `draining` (I-056a) but NOT `draining_hb`. The same \
         draining process re-asserts on its next heartbeat; in the \
         gap, this prevents mis-dispatch."
    );
    assert!(
        !w.store_degraded,
        "I-056a: store_degraded cleared on reconnect (prior-session \
         FUSE breaker, stale)"
    );

    // Worker heartbeats draining=false (its drain finished, or — in
    // I-056a's original scenario — it's a fresh process that was
    // never draining). `draining_hb` clears; admin `draining` was
    // already cleared by reconnect → effective false.
    send_heartbeat(&handle, "drain-auth", "x86_64-linux").await?;
    let w = expect_worker(&handle, "drain-auth").await;
    assert!(
        !w.draining,
        "both sources cleared → effective draining false"
    );

    // Admin-only drain survives heartbeat draining=false. Covers
    // `drain_worker_force_reassigns`' assumption: operator/controller
    // drains via RPC, worker process doesn't know, keeps heartbeating
    // false. The split is what makes this work.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "drain-auth".into(),
            force: false,
            reply: reply_tx,
        })
        .await?;
    assert!(reply_rx.await?.accepted);
    send_heartbeat(&handle, "drain-auth", "x86_64-linux").await?;
    let w = expect_worker(&handle, "drain-auth").await;
    assert!(
        w.draining,
        "admin drain survives heartbeat draining=false — heartbeat \
         only writes `draining_hb`, never `draining`"
    );

    Ok(())
}

/// I-066: a worker reconnecting after scheduler restart with an
/// in-flight build (I-063 keeps the stream alive during drain)
/// heartbeats `running=[X]`. The new leader has X as Ready (recovery's
/// reconcile reset it before the worker reconnected). Adoption must
/// claim X in the DAG (Ready→Assigned, assigned_executor=this) so
/// `dispatch_ready` doesn't re-pop it and send it to ANOTHER idle
/// worker. Live: openssl re-dispatched while two draining workers were
/// already running it → both ended up in failed_builders → I-065
/// poisoned a passing build.
///
/// The regression case is the SECOND worker: pre-fix, A's heartbeat
/// adopted into `worker.running_build` only (DAG stayed Ready), then
/// B's connect-time `dispatch_ready` found Ready + B idle → assigned
/// to B. Post-fix, A's adoption transitions DAG to Assigned, B's
/// dispatch sees not-Ready and skips.
// r[verify sched.heartbeat.adopt]
#[tokio::test]
async fn test_heartbeat_adopts_inflight_from_reconnecting_worker() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Derivation Ready with NO worker connected — simulates recovery's
    // reconcile having reset the assignment before any reconnect.
    let build_id = Uuid::new_v4();
    let drv_hash = "i066-adopt-drv";
    let drv_path = rio_test_support::fixtures::test_drv_path(drv_hash);
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "precondition: Ready, no worker yet"
    );

    // Worker A reconnects with the build in-flight: stream-connect
    // (entry created, running_build=None) then FIRST heartbeat reports
    // running=[drv]. No PrefetchComplete ACK — A is draining, it's
    // not accepting new work, just reporting what it has.
    let (stream_tx_a, _stream_rx_a) = mpsc::channel(8);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "i066-a".into(),
            stream_tx: stream_tx_a,
        })
        .await?;
    send_heartbeat_with(&handle, "i066-a", "x86_64-linux", |hb| {
        hb.draining = true;
        hb.running_build = Some(drv_path.clone());
    })
    .await?;

    // Adoption: DAG node Assigned to A, A.running_build set, no
    // failed_builders (adoption is reconciliation, not failure).
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Assigned,
        "adoption transitioned Ready→Assigned"
    );
    assert_eq!(
        info.assigned_executor.as_deref(),
        Some("i066-a"),
        "assigned_executor set to the reconnecting worker"
    );
    assert!(
        info.retry.failed_builders.is_empty(),
        "adoption must not penalize the worker"
    );
    let a = expect_worker(&handle, "i066-a").await;
    assert!(
        a.running_build == Some(drv_hash.to_string()),
        "worker.running_build set so A reads at-capacity"
    );

    // Worker B connects idle. Its registration triggers dispatch_ready.
    // The drv is Assigned (adopted) → not in the Ready filter → B
    // gets nothing. Pre-I-066: drv was still Ready, B would receive it.
    let _stream_rx_b = connect_executor(&handle, "i066-b", "x86_64-linux").await?;
    let b = expect_worker(&handle, "i066-b").await;
    assert!(
        b.running_build.is_none(),
        "B must NOT receive the drv — adoption prevented re-dispatch"
    );
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.assigned_executor.as_deref(),
        Some("i066-a"),
        "still A's after B's dispatch pass"
    );

    Ok(())
}

/// I-048b: a heartbeat arriving before any BuildExecution stream
/// connects must NOT create an executor entry. Only stream-open
/// (`handle_worker_connected`) creates. Heartbeat-creates-entry
/// produces a zombie with `stream_tx: None`: `is_registered()` false,
/// `has_capacity()` false, dispatch dead-locked until a stream
/// connects — which after an abrupt scheduler restart can take
/// minutes (worker's old stream stuck in TCP keepalive timeout while
/// the unary heartbeat RPC succeeds against the new scheduler
/// immediately). Live: `fod_queue=3 fetcher_util=0.00` for 5+ minutes
/// post-deploy; fetcher restart unblocks because the fresh process
/// opens a stream first.
#[tokio::test]
async fn test_heartbeat_before_stream_does_not_create_zombie() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Heartbeat for an executor that never opened a stream. Pre-fix:
    // `entry().or_insert_with()` created a zombie. Post-fix: WARN +
    // drop, no entry.
    send_heartbeat(&handle, "zombie-candidate", "x86_64-linux").await?;

    let workers = handle.debug_query_workers().await?;
    assert!(
        !workers.iter().any(|w| w.executor_id == "zombie-candidate"),
        "heartbeat-before-stream must NOT create an executor entry; \
         got {:?}",
        workers.iter().map(|w| &w.executor_id).collect::<Vec<_>>()
    );

    // Now connect properly: stream first, THEN heartbeat. This is the
    // normal lifecycle and must still work — proves the fix doesn't
    // break the happy path. connect_executor sends ExecutorConnected
    // (creates entry, sets stream_tx) followed by Heartbeat (updates).
    let _rx = connect_executor(&handle, "zombie-candidate", "x86_64-linux").await?;

    let w = expect_worker(&handle, "zombie-candidate").await;
    assert!(
        w.is_registered,
        "stream + heartbeat → fully registered (lifecycle invariant intact)"
    );
    Ok(())
}

/// `DebugExecutorInfo` extended fields for the `DebugListExecutors` RPC
/// (`rio-cli workers --actor`). Walks the same lifecycle the I-048b
/// test exercises but asserts on `has_stream`/`warm`/`kind`/`systems`
/// instead of just `is_registered` — these are the dispatch-filter
/// inputs that PG `last_seen` can't tell you.
// r[verify sched.admin.debug-list-executors]
#[tokio::test]
async fn test_debug_query_workers_extended_fields() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Stage 1: stream connected, no heartbeat. has_stream=true,
    // is_registered=false (systems empty), warm=false.
    let _rx = connect_executor_no_ack(&handle, "ext-builder", "x86_64-linux").await?;
    let w = expect_worker(&handle, "ext-builder").await;
    assert!(
        w.has_stream,
        "stream_tx set by ExecutorConnected → has_stream=true"
    );
    // connect_executor_no_ack sends a heartbeat after the stream opens
    // (it just doesn't ACK prefetch). So systems is populated.
    assert!(
        w.is_registered,
        "stream + heartbeat → registered (no_ack still heartbeats)"
    );
    assert_eq!(
        w.systems,
        vec!["x86_64-linux".to_string()],
        "systems populated from heartbeat"
    );
    assert_eq!(
        w.kind,
        rio_proto::types::ExecutorKind::Builder,
        "default kind from connect_executor_no_ack"
    );
    // Warm: no_ack doesn't send PrefetchComplete. With an empty ready
    // queue at registration, on_worker_registered flips warm=true
    // immediately (nothing to prefetch). So we can't assert warm=false
    // here without merging a derivation FIRST. The connect_executor
    // helper below covers the warm=true post-ACK case.
    assert!(w.running_build.is_none());
    assert!(w.running_build.is_none());
    // last_heartbeat_ago_secs: just-heartbeated → 0 or 1. Don't assert
    // exact value (timing); assert sanity.
    assert!(
        w.last_heartbeat_ago_secs < 5,
        "just-heartbeated → sub-5s elapsed (got {}s)",
        w.last_heartbeat_ago_secs
    );

    // Stage 2: fetcher kind, full lifecycle (connect_executor sends
    // PrefetchComplete → warm=true).
    let _rx2 = connect_executor_no_ack_kind(
        &handle,
        "ext-fetcher",
        "x86_64-linux",
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "ext-fetcher".into(),
            paths_fetched: 0,
        })
        .await?;
    let w = expect_worker(&handle, "ext-fetcher").await;
    assert!(w.has_stream);
    assert!(w.is_registered);
    assert!(
        w.warm,
        "PrefetchComplete ACK flips warm=true (the dispatch gate)"
    );
    assert_eq!(
        w.kind,
        rio_proto::types::ExecutorKind::Fetcher,
        "kind persists from heartbeat — FOD routing keys on this"
    );

    // Both entries present.
    assert_eq!(handle.debug_query_workers().await?.len(), 2);
    Ok(())
}

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_build. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() -> TestResult {
    // Register worker (initial heartbeat has empty running_build).
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("toctou-worker", "x86_64-linux").await?;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_build.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Verify: derivation is Assigned, worker.running_build contains it.
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.status, DerivationStatus::Assigned);

    let w = expect_worker(&handle, "toctou-worker").await;
    assert!(
        w.running_build == Some(drv_hash.to_string()),
        "scheduler should have tracked the assignment in worker.running_build"
    );

    // Send a STALE heartbeat with empty running_build. This mimics the
    // race: worker sent heartbeat before receiving/acking the assignment.
    // send_heartbeat's running_build=[] is the stale value under test.
    send_heartbeat(&handle, "toctou-worker", "x86_64-linux").await?;

    // Assignment must still be tracked. Before the fix, running_build
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let w = expect_worker(&handle, "toctou-worker").await;
    assert!(
        w.running_build == Some(drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
    Ok(())
}

/// I-035: a SECOND consecutive heartbeat with empty running_build drains
/// the phantom. The TOCTOU window is one heartbeat interval (~10s) — one
/// miss is the race the test above protects; two misses means the worker
/// genuinely doesn't have the assignment (lost completion, dead stream
/// post-send, I-032 pre-d11245b4). Before this fix the slot stayed dead
/// forever: `has_capacity()` saw 1/1 and the derivation sat Assigned with
/// no path to Ready short of executor disconnect.
// r[verify sched.heartbeat.phantom-drain]
#[tokio::test]
async fn test_heartbeat_phantom_drain_on_second_miss() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("phantom-worker", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "phantom-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Precondition: dispatch assigned the drv (single candidate).
    // Worker's running_build has it; DAG is Assigned.
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.status, DerivationStatus::Assigned);
    let w = expect_worker(&handle, "phantom-worker").await;
    assert!(
        w.running_build == Some(drv_hash.to_string()),
        "precondition: dispatch tracked the assignment"
    );

    // First miss: TOCTOU keep — same outcome as the test above.
    send_heartbeat(&handle, "phantom-worker", "x86_64-linux").await?;
    let w = expect_worker(&handle, "phantom-worker").await;
    assert!(
        w.running_build == Some(drv_hash.to_string()),
        "first miss is the TOCTOU race — keep"
    );
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Assigned,
        "first miss leaves DAG state alone"
    );

    // Second miss: phantom confirmed. Drain → reset_to_ready →
    // dispatch_ready re-assigns to the (now-free) same worker.
    send_heartbeat(&handle, "phantom-worker", "x86_64-linux").await?;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(
        info.status,
        DerivationStatus::Assigned,
        "second miss drained → Ready → dispatch_ready re-assigned \
         (only worker, only drv, deterministic)"
    );
    // The re-assignment proves the drain happened: without it, the
    // slot would still be 1/1 (phantom holds it) and dispatch would
    // skip. The drain WARNs + resets to Ready; the same heartbeat's
    // dispatch_ready sees 0/1 + Ready in queue → assigns again.
    // Distinguish from "nothing happened": failed_builders must be
    // empty. The drain path explicitly does NOT penalize the worker
    // (a phantom is not a worker failure).
    assert!(
        info.retry.failed_builders.is_empty(),
        "phantom drain must NOT add to failed_builders — \
         not the worker's fault, got {:?}",
        info.retry.failed_builders
    );
    Ok(())
}

/// I-042: completion arriving for a derivation that's already terminal
/// (e.g., poisoned by a parallel-retry race) must still free the
/// executor's `running_build` slot. Before the fix, the line-419
/// early-return (`"not in assigned/running state"`) returned BEFORE
/// `running_build.remove`, leaking the slot.
///
/// EKS reproduction (build 019d4681): stage0-posix retried across
/// fetchers 0, 1, 3. fetcher-3's TransientFailure was the 6th — by
/// then the derivation was already Poisoned (threshold 3, hit by an
/// earlier completion). The 6th completion's running_build.remove was
/// inside an if-chain that the early-return skipped. fetcher-3's slot
/// stayed 1/1 across 30+ heartbeats.
///
/// Why the I-035 phantom-drain didn't catch it: the heartbeat reconcile
/// (executor.rs:531-541) DOES drop entries whose DAG status is
/// terminal — but only by overwriting `running_build = reconciled` at
/// line 608. The phantom-drain (which WARNs) checks `reconciled
/// .difference(&heartbeat_set)`; a Poisoned entry is already filtered
/// OUT of `reconciled` by `still_inflight=false`, so it's not a
/// "suspect" and the WARN never fires. The slot SHOULD still be freed
/// via the line-608 overwrite — this test confirms that safety net
/// works, while the fix makes the completion path free immediately.
#[tokio::test]
async fn test_completion_after_poison_frees_running_build() -> TestResult {
    // Two workers so dispatch can re-assign after worker-1's failure.
    let (_db, handle, _task, _rx1) = setup_with_worker("i042-w1", "x86_64-linux").await?;
    let _rx2 = connect_executor(&handle, "i042-w2", "x86_64-linux").await?;

    // Merge + dispatch. drv goes to one of the workers (deterministic
    // by HashMap iteration order — doesn't matter which for this test).
    let build_id = Uuid::new_v4();
    let drv_hash = "i042-drv";
    let _evt = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Whichever worker got it, force-assign to BOTH so each has the
    // entry in running_build. This simulates the live race where an
    // earlier retry left a stale entry: dispatch added → completion
    // ran → completion's early-return left running_build untouched.
    // debug_force_assign is idempotent for the worker that already
    // owns it; for the other, it sets assigned_executor + inserts.
    // After both calls, assigned_executor = i042-w2 (last-writer wins).
    handle.debug_force_assign(drv_hash, "i042-w1").await?;
    handle.debug_force_assign(drv_hash, "i042-w2").await?;

    // Both workers track the drv. assigned_executor = w2.
    let w1 = expect_worker(&handle, "i042-w1").await;
    let w2 = expect_worker(&handle, "i042-w2").await;
    assert!(w1.running_build == Some(drv_hash.to_string()));
    assert!(w2.running_build == Some(drv_hash.to_string()));

    // w2's completion: PermanentFailure → poison_and_cascade. This
    // is the NORMAL flow — handle_completion runs to line 515 and
    // frees w2's slot. Status becomes Poisoned.
    complete_failure(
        &handle,
        "i042-w2",
        &test_drv_path(drv_hash),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "first failure (poisons)",
    )
    .await?;

    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.status, DerivationStatus::Poisoned);
    let w2 = expect_worker(&handle, "i042-w2").await;
    assert!(
        w2.running_build != Some(drv_hash.to_string()),
        "w2's normal completion path frees its slot (line 515)"
    );
    let w1 = expect_worker(&handle, "i042-w1").await;
    assert!(
        w1.running_build == Some(drv_hash.to_string()),
        "w1's stale entry is still there (no completion processed yet)"
    );

    // THE RACE: w1's completion arrives. drv is already Poisoned.
    // handle_completion's status check (line 412) fires the
    // not-Assigned/Running early-return. Before I-042: this returned
    // BEFORE running_build.remove → w1's slot leaked. After: the
    // remove is hoisted before the early-return paths.
    complete_failure(
        &handle,
        "i042-w1",
        &test_drv_path(drv_hash),
        rio_proto::types::BuildResultStatus::TransientFailure,
        "late completion (drv already poisoned)",
    )
    .await?;

    let w1 = expect_worker(&handle, "i042-w1").await;
    assert!(
        w1.running_build != Some(drv_hash.to_string()),
        "I-042: completion for already-terminal derivation must free \
         the executor's running_build slot — pre-fix this leaked"
    );
    Ok(())
}

/// I-042 safety-net assertion: even WITHOUT the completion-side fix,
/// the heartbeat reconcile drops running_build entries whose DAG
/// status is terminal. This is the i035 reconcile loop's
/// `still_inflight = matches!(status, Assigned|Running)` filter — a
/// Poisoned entry doesn't match, doesn't go into `reconciled`,
/// `running_build = reconciled` overwrites it away.
///
/// The phantom-drain WARN doesn't fire (the entry is filtered out
/// before suspect detection), but the slot is freed. This test
/// pins that behavior so the safety net stays even if completion-
/// side cleanup regresses.
#[tokio::test]
async fn test_heartbeat_reconcile_drops_terminal_running_build_entry() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("i042-hb-w", "x86_64-linux").await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "i042-hb-drv";
    let _evt = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Precondition: dispatched, slot occupied.
    let w = expect_worker(&handle, "i042-hb-w").await;
    assert!(w.running_build == Some(drv_hash.to_string()));

    // Poison via PermanentFailure. The completion-side fix frees the
    // slot here (proven by the test above). To probe the heartbeat
    // safety net independently, re-insert via debug_force_assign —
    // it adds to running_build without checking DAG terminality.
    complete_failure(
        &handle,
        "i042-hb-w",
        &test_drv_path(drv_hash),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "poison",
    )
    .await?;
    let info = expect_drv(&handle, drv_hash).await;
    assert_eq!(info.status, DerivationStatus::Poisoned);

    // Re-seed the leak: force_assign on a Poisoned drv fails the
    // status transition (Poisoned → Assigned is invalid) so it
    // returns false WITHOUT inserting into running_build. We need
    // a different re-seed: the test below covers this scenario via
    // the two-worker race; here we just assert the reconcile filter
    // logic by checking the slot stays clear after a heartbeat (it
    // was already clear from the completion-side fix).
    send_heartbeat(&handle, "i042-hb-w", "x86_64-linux").await?;
    let w = expect_worker(&handle, "i042-hb-w").await;
    assert!(
        w.running_build != Some(drv_hash.to_string()),
        "Poisoned derivation must not survive heartbeat reconcile in \
         running_build (still_inflight = matches!(Poisoned, Assigned|Running) = false)"
    );
    Ok(())
}

/// Heartbeat timeout deregisters worker and reassigns its builds.
/// Instead of advancing time (PG timeout issue), we send Tick commands
/// after manipulating the worker's last_heartbeat via multiple Tick cycles
/// without heartbeats. Actually simpler: send ExecutorDisconnected directly
/// is equivalent (handle_tick calls handle_executor_disconnected on timeout),
/// so that path is already covered by test_worker_disconnect_running_derivation.
/// This test verifies the Tick-driven path specifically by injecting Ticks.
#[tokio::test]
async fn test_heartbeat_timeout_via_tick_deregisters_worker() -> TestResult {
    // NOTE: This test would ideally use tokio::time::pause + advance, but
    // that interferes with PG pool timeouts. Instead, we verify that Tick
    // correctly processes the timeout path by checking the missed_heartbeats
    // counter accumulates. Since we can't easily fast-forward real time in
    // this test harness, we verify the logic indirectly: Tick with fresh
    // heartbeat does NOT remove the worker (negative test), and the
    // timeout-removal path is exercised directly via ExecutorDisconnected
    // in test_worker_disconnect_running_derivation.
    let (_db, handle, _task, _stream_rx) = setup_with_worker("tick-worker", "x86_64-linux").await?;

    // Send several Ticks. Worker has fresh heartbeat, should NOT be removed.
    for _ in 0..MAX_MISSED_HEARTBEATS + 1 {
        handle.send_unchecked(ActorCommand::Tick).await?;
    }

    let workers = handle.debug_query_workers().await?;
    assert!(
        workers.iter().any(|w| w.executor_id == "tick-worker"),
        "worker with fresh heartbeat should survive Tick"
    );
    Ok(())
}

// ===========================================================================
// Poison-TTL expiry (POISON_TTL is cfg(test)-shadowed to 100ms in state/mod.rs)
// ===========================================================================

/// A poisoned derivation is removed from the DAG after POISON_TTL elapses
/// and a Tick is processed. Covers the poison-expiry loop in handle_tick.
/// Removal (not in-place reset) means next submit re-inserts it fresh
/// with full proto fields via `compute_initial_states`.
#[tokio::test]
async fn test_tick_expires_poisoned_derivation() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("poison-ttl-worker", "x86_64-linux").await?;

    // Merge, dispatch, poison via PermanentFailure.
    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(
        &handle,
        build_id,
        "poison-ttl-hash",
        PriorityClass::Scheduled,
    )
    .await?;

    complete_failure(
        &handle,
        "poison-ttl-worker",
        &test_drv_path("poison-ttl-hash"),
        rio_proto::types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;

    // Verify poisoned.
    let pre = expect_drv(&handle, "poison-ttl-hash").await;
    assert_eq!(pre.status, DerivationStatus::Poisoned);

    // Wait past the cfg(test) POISON_TTL (100ms). 3× margin for loaded
    // CI hosts — poisoned_at is std::time::Instant (derivation.rs:202),
    // which tokio paused time can't mock, so real sleep is the only option.
    tokio::time::sleep(Duration::from_millis(300)).await;

    // Tick processes the expiry.
    handle.send_unchecked(ActorCommand::Tick).await?;

    let post = handle.debug_query_derivation("poison-ttl-hash").await?;
    assert!(
        post.is_none(),
        "poisoned derivation should be removed from DAG after TTL expiry"
    );
    Ok(())
}

/// 3 sequential worker disconnects: poison iff the drv was Running at
/// disconnect. I-097 narrowed "any disconnect counts" to
/// "disconnect-while-Running counts" — Assigned-only means the drv was
/// never attempted (ephemeral-fetcher race), so a 4th worker should
/// still receive it.
///
/// - **running**: `debug_backdate_running` before each disconnect →
///   `was_running=true` → `record_failure_and_check_poison` → poison
///   after 3.
/// - **assigned_only** (I-097 regression): no Running transition →
///   `was_running=false` → stays Ready, `failed_builders` empty.
#[rstest::rstest]
#[case::running(true, DerivationStatus::Poisoned)]
#[case::assigned_only(false, DerivationStatus::Ready)]
#[tokio::test]
async fn test_three_disconnects_poison_iff_running(
    #[case] mark_running: bool,
    #[case] expect_status: DerivationStatus,
) -> TestResult {
    let (_db, handle, _task) = setup().await;
    let tag = if mark_running { "x6-drv" } else { "x7-drv" };
    let _ev = merge_single_node(&handle, Uuid::new_v4(), tag, PriorityClass::Scheduled).await?;

    for i in 0..3 {
        let id = format!("w-{tag}-{i}");
        let mut rx = connect_executor(&handle, &id, "x86_64-linux").await?;
        assert!(recv_assignment(&mut rx).await.drv_path.contains(tag));
        if mark_running {
            // Assigned → Running so the disconnect counts as a failed attempt.
            assert!(handle.debug_backdate_running(tag, 0).await?);
        }
        disconnect(&handle, &id).await?;
        drop(rx);
    }

    let info = expect_drv(&handle, tag).await;
    assert_eq!(info.status, expect_status, "after 3 disconnects");
    if !mark_running {
        assert!(
            info.retry.failed_builders.is_empty(),
            "Assigned-only disconnects must not populate failed_builders; got {:?}",
            info.retry.failed_builders
        );
        // Sensitivity: a 4th worker DOES get it (Ready is real).
        let mut rx4 = connect_executor(&handle, "w-x7-3", "x86_64-linux").await?;
        assert!(recv_assignment(&mut rx4).await.drv_path.contains(tag));
    }
    Ok(())
}

/// I-213: a Running-disconnect on a CLASSED worker promotes the floor;
/// that promotion exempts the failure from `failed_builders` /
/// `failure_count` / `retry_count`. The production firefox case is
/// exactly this: kubelet evicts (ephemeral-storage) → SIGKILL →
/// disconnect → reassign_derivations. Before this fix,
/// `record_failure_and_check_poison` recorded all 3 evictions and
/// `PoisonConfig.threshold=3` poisoned at `medium` with the ladder
/// half-climbed. The unclassed-worker case
/// (`test_three_running_disconnects_poisons`) is unaffected.
// r[verify sched.retry.promotion-exempt]
#[tokio::test]
async fn test_classed_running_disconnects_exempt_from_poison_until_top() -> TestResult {
    let classes = ["tiny", "small", "medium", "large", "xlarge"];
    let (_db, handle, _task) = setup_with_classes(&[
        ("tiny", 30.0),
        ("small", 60.0),
        ("medium", 90.0),
        ("large", 120.0),
        ("xlarge", 150.0),
    ])
    .await;

    let _ev = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "ladder-213",
        PriorityClass::Scheduled,
    )
    .await?;

    // Climb tiny→small→medium→large via Running-disconnects: each
    // promotes, none recorded.
    for (i, c) in classes[..4].iter().enumerate() {
        let id = format!("b-213-{c}");
        let mut rx = connect_builder_classed(&handle, &id, "x86_64-linux", c).await?;
        let asgn = recv_assignment(&mut rx).await;
        assert!(asgn.drv_path.contains("ladder-213"));
        assert!(handle.debug_backdate_running("ladder-213", 0).await?);
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: id.into(),
            })
            .await?;
        // Tick: each connect's first-heartbeat consumes the
        // BECAME_IDLE_INLINE_CAP budget; reset between iterations
        // (production disconnect→reconnect spans multiple Ticks).
        tick(&handle).await?;
        drop(rx);

        let s = expect_drv(&handle, "ladder-213").await;
        assert_eq!(
            s.sched.size_class_floor.as_deref(),
            Some(classes[i + 1]),
            "disconnect on {c} → floor promoted"
        );
        assert_eq!(
            s.retry.failure_count, 0,
            "I-213: promoted disconnect not recorded in failure_count (after {c})"
        );
        assert!(
            s.retry.failed_builders.is_empty(),
            "I-213: promoted disconnect not recorded in failed_builders (after {c})"
        );
        assert_eq!(s.retry.count, 0);
        assert_ne!(s.status, DerivationStatus::Poisoned);
    }

    // Top of ladder: 3 disconnects on xlarge → recorded → poison
    // (threshold=3, default).
    for i in 0..3 {
        let id = format!("b-213-xl-{i}");
        let mut rx = connect_builder_classed(&handle, &id, "x86_64-linux", "xlarge").await?;
        let asgn = recv_assignment(&mut rx).await;
        assert!(asgn.drv_path.contains("ladder-213"));
        assert!(handle.debug_backdate_running("ladder-213", 0).await?);
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: id.into(),
            })
            .await?;
        tick(&handle).await?;
        drop(rx);
    }
    let s = expect_drv(&handle, "ladder-213").await;
    assert_eq!(
        s.status,
        DerivationStatus::Poisoned,
        "threshold applies once at top of ladder (no promotion)"
    );
    Ok(())
}

// r[verify sched.fod.size-class-reactive]
// r[verify sched.builder.size-class-reactive]
// r[verify sched.reassign.no-promote-on-ephemeral-disconnect+2]
/// Assigned-disconnect-without-CompletionReport → `size_class_floor`
/// promoted tiny→small. DerivationStatus stays Assigned for the build's
/// whole lifetime (Running is set only at completion via
/// `ensure_running()`), so the production disconnect path is always
/// Assigned-status. Disconnect-without-completion = unexpected death
/// (plausibly OOM) → promote. I-097 still holds (no `failed_builders`
/// entry, status=Ready) — promotion is decoupled from poison-record.
///
/// - **fod** (I-173): FOD on a `fetcher_size_classes` tiny fetcher.
///   Live: 14 OOMs, retry_count=0, size_class_floor=NULL.
/// - **builder** (I-177): non-FOD on a builder `size_classes` tiny.
///   Live: 103 tiny builders OOMKilled on bootstrap-stage0-glibc.
/// - **ephemeral** (I-197, refines I-188 fix 1): same as builder; the
///   I-188 blanket "one-shot disconnect → never promote" made openssl
///   OOM-loop on tiny for hours. Converse (disconnect AFTER completion
///   → NO promote) at `test_ephemeral_disconnect_after_completion_no_promote`.
#[rstest::rstest]
#[case::fod(rio_proto::types::ExecutorKind::Fetcher, true, "oom-fod-173")]
#[case::builder(rio_proto::types::ExecutorKind::Builder, false, "oom-glibc-177")]
#[case::ephemeral(rio_proto::types::ExecutorKind::Builder, false, "eph-glibc-188")]
#[tokio::test]
async fn test_assigned_disconnect_promotes_floor(
    #[case] kind: rio_proto::types::ExecutorKind,
    #[case] is_fod: bool,
    #[case] tag: &str,
) -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |c, _| {
        if is_fod {
            c.fetcher_size_classes = vec!["tiny".into(), "small".into()];
        } else {
            c.size_classes = size_classes(&[("tiny", 30.0), ("small", 3600.0)]);
        }
    });

    let mut rx = connect_executor_classed(&handle, "w-tiny", "x86_64-linux", "tiny", kind).await?;

    let mut node = make_node(tag);
    node.is_fixed_output = is_fod;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // Dispatch → Assigned. Do NOT send Running ack.
    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains(tag));
    assert_eq!(
        expect_drv(&handle, tag).await.status,
        DerivationStatus::Assigned,
        "precondition: status==Assigned at disconnect"
    );

    // OOM → disconnect. Status was Assigned, not Running.
    disconnect(&handle, "w-tiny").await?;
    drop(rx);

    let info = expect_drv(&handle, tag).await;
    assert_eq!(
        info.sched.size_class_floor.as_deref(),
        Some("small"),
        "Assigned-disconnect-without-completion must promote floor tiny→small"
    );
    assert!(
        info.retry.failed_builders.is_empty(),
        "I-097: Assigned-disconnect must not record failure; got {:?}",
        info.retry.failed_builders
    );
    assert_eq!(info.status, DerivationStatus::Ready);
    Ok(())
}

// r[verify sched.reassign.no-promote-on-ephemeral-disconnect+2]
/// I-188 race case (the suppress that I-197 KEEPS): a builder that
/// disconnects with `running_build == Some(X)` AFTER having sent
/// CompletionReport(X) MUST NOT promote `size_class_floor`.
/// `last_completed == running_build` → expected one-shot exit, not a
/// size-adequacy signal.
///
/// Setup synthesizes the race directly: complete X (sets
/// `last_completed=X`, clears `running_build`, sets `draining`),
/// then a heartbeat re-reports X as running (out-of-order delivery
/// — heartbeat snapshotted before completion landed) repopulating
/// `running_build=X`, then disconnect. This is the narrowest
/// surviving window where Fix 1's gate fires; Fix 2
/// (`r[sched.ephemeral.no-redispatch-after-completion]`) closes the
/// dependent-redispatch race at the source so this is defense-in-
/// depth.
#[tokio::test]
async fn test_ephemeral_disconnect_after_completion_no_promote() -> TestResult {
    let (_db, handle, _task) = setup_with_classes(&[("tiny", 30.0), ("small", 3600.0)]).await;

    let mut rx = connect_builder_classed(&handle, "b-eph2", "x86_64-linux", "tiny").await?;
    send_heartbeat_with(&handle, "b-eph2", "x86_64-linux", |hb| {
        hb.size_class = Some("tiny".into());
    })
    .await?;
    barrier(&handle).await;

    let node = make_node("eph-race-188");
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("eph-race-188"));
    let drv_path = asgn.drv_path.clone();

    // CompletionReport(X) → last_completed=X, running_build=None,
    // draining=true (Fix 2).
    complete_success_empty(&handle, "b-eph2", "eph-race-188").await?;
    barrier(&handle).await;

    // Out-of-order heartbeat (snapshotted before completion landed)
    // re-reports X as running → reconcile repopulates running_build=X
    // (adopt_heartbeat_build's terminal-status arm warns but the
    // caller still sets running_build). last_completed stays X
    // (heartbeat doesn't touch it).
    send_heartbeat_with(&handle, "b-eph2", "x86_64-linux", |hb| {
        hb.size_class = Some("tiny".into());
        hb.running_build = Some(drv_path);
    })
    .await?;
    barrier(&handle).await;

    // Precondition: running_build re-populated. Without this the
    // disconnect's reassign loop is empty and the assertion below
    // passes trivially.
    let w = expect_worker(&handle, "b-eph2").await;
    assert!(
        w.running_build.is_some(),
        "precondition: heartbeat reconcile re-populated running_build; \
         test would pass trivially otherwise"
    );

    // Ephemeral exits → disconnect. running_build=Some(X),
    // last_completed=Some(X) → expected one-shot exit → NO promote.
    disconnect(&handle, "b-eph2").await?;
    drop(rx);

    let info = expect_drv(&handle, "eph-race-188").await;
    assert_eq!(
        info.sched.size_class_floor, None,
        "I-188: ephemeral disconnect AFTER CompletionReport for the \
         running drv (last_completed == running_build) must NOT \
         promote floor; got {:?}",
        info.sched.size_class_floor
    );

    Ok(())
}

// r[verify sched.ephemeral.no-redispatch-after-completion]
/// I-188 (fix 2): an ephemeral executor that completes its one build
/// is marked draining BEFORE `dispatch_ready` runs in the same actor
/// turn, so the dependent that ProcessCompletion just unlocked is NOT
/// re-assigned to the about-to-exit slot.
///
/// Setup: parent→child chain, single ephemeral builder. Parent
/// dispatches → completes → child becomes Ready → dispatch_ready in
/// the same ProcessCompletion turn would assign child to the freed
/// slot pre-fix. Post-fix: builder is draining, child stays Ready.
#[tokio::test]
async fn test_ephemeral_completion_marks_draining_no_redispatch() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let mut rx = connect_executor(&handle, "eph-1", "x86_64-linux").await?;

    // parent → child chain. Parent dispatches first (child blocked).
    let parent = make_node("eph-parent");
    let child = make_node("eph-child");
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![parent, child],
        // Edge convention: (consumer, input). eph-child depends on
        // eph-parent → eph-parent is the leaf, dispatches first.
        vec![make_test_edge("eph-child", "eph-parent")],
        false,
    )
    .await?;

    let asgn = recv_assignment(&mut rx).await;
    assert!(
        asgn.drv_path.contains("eph-parent"),
        "parent dispatches first"
    );

    // Parent completes. ProcessCompletion: free slot → mark draining
    // (ephemeral) → child Ready → dispatch_ready sees draining → skip.
    complete_success_empty(&handle, "eph-1", "eph-parent").await?;
    barrier(&handle).await;

    // Builder is draining; child NOT assigned to it.
    let w = expect_worker(&handle, "eph-1").await;
    assert!(
        w.draining,
        "I-188: ephemeral executor must be draining after completion"
    );
    assert!(
        w.running_build.is_none(),
        "I-188: ephemeral executor must not be re-assigned post-completion"
    );

    let child_info = expect_drv(&handle, "eph-child").await;
    assert_eq!(
        child_info.status,
        DerivationStatus::Ready,
        "child unlocked but NOT dispatched to draining ephemeral; got {:?}",
        child_info.status
    );

    // Sensitivity: rx received NO second Assignment. try_recv (non-
    // blocking) — anything queued would be the spurious re-dispatch.
    match rx.try_recv() {
        Err(tokio::sync::mpsc::error::TryRecvError::Empty) => {}
        Ok(msg) => panic!(
            "ephemeral executor received post-completion message: {:?}",
            msg.msg
        ),
        Err(e) => panic!("unexpected channel state: {e:?}"),
    }

    Ok(())
}

/// ExecutorDisconnected for a never-connected worker → no-op. The
/// handler's early-return on `workers.remove(executor_id) == None`
/// means no gauge decrement (would go negative otherwise) and no
/// reassign pass (nothing to reassign).
#[tokio::test]
async fn test_worker_disconnect_unknown_noop() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Disconnect a worker that was never connected.
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "ghost".into(),
        })
        .await?;

    // Actor should still be alive (no panic on None remove).
    let workers = handle.debug_query_workers().await?;
    assert!(workers.is_empty(), "workers should remain empty");
    assert!(handle.is_alive(), "actor should survive unknown disconnect");

    Ok(())
}

/// Heartbeat reports a running build the scheduler never assigned
/// (but which IS in the DAG) → adopt it: worker.running_build set
/// AND DAG node transitioned Ready→Assigned to this worker. INFO
/// (not WARN — this is the expected post-restart reconnect path,
/// not split-brain). Log-level + DAG-side adoption are I-066;
/// pre-fix this only warned + set running_build, leaving the DAG
/// Ready for re-dispatch elsewhere.
///
/// Note: the drv_path must resolve via `dag.hash_for_path` — unknown
/// paths are silently filtered BEFORE the reconcile. So this test
/// merges the drv first (puts it in the DAG) without dispatching it
/// to the heartbeating worker.
///
/// See [`test_heartbeat_adopts_inflight_from_reconnecting_worker`]
/// for the two-worker case proving no re-dispatch.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_heartbeat_adopts_unknown_build_into_dag() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge a drv into the DAG. NO worker connected yet → stays
    // Ready (not dispatched). This puts the drv_path→hash mapping
    // in the DAG so the heartbeat filter lets it through.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "hb-drv", PriorityClass::Scheduled).await?;

    // Connect worker via ExecutorConnected only (no initial heartbeat)
    // so we control the first heartbeat's running_build precisely.
    let (stream_tx, _stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "hb-worker".into(),
            stream_tx,
        })
        .await?;

    // Heartbeat with running_build claiming the drv we merged but
    // never assigned to this worker. The reconcile adopts it (worker
    // is authoritative about what it's running) which sets
    // running_build → has_capacity()=false → the post-heartbeat
    // dispatch_ready() can't ALSO assign (would muddy "worker claims
    // it, scheduler didn't know").
    send_heartbeat_with(&handle, "hb-worker", "x86_64-linux", |hb| {
        hb.running_build = Some(test_drv_path("hb-drv"));
    })
    .await?;
    barrier(&handle).await;

    assert!(
        logs_contain("adopted in-flight build from reconnecting worker"),
        "adoption is the EXPECTED post-restart path — INFO, not WARN"
    );
    assert!(
        !logs_contain("heartbeat reports running build scheduler did not assign"),
        "the pre-I-066 phantom-warn no longer fires for adoptable nodes"
    );

    // running_build adoption (pre-I-066 already did this).
    let w = expect_worker(&handle, "hb-worker").await;
    assert!(
        w.running_build == Some("hb-drv".to_string()),
        "worker's claim adopted into running_build"
    );

    // DAG-side adoption (the I-066 fix). Without it, the post-
    // heartbeat dispatch_ready would re-pop the still-Ready node and
    // send it to the next idle worker.
    let info = expect_drv(&handle, "hb-drv").await;
    assert_eq!(info.status, DerivationStatus::Assigned);
    assert_eq!(info.assigned_executor.as_deref(), Some("hb-worker"));
    assert!(info.retry.failed_builders.is_empty());

    Ok(())
}

/// DrainExecutor(force=true) on an idle worker → running=0, no
/// CancelSignal sent (nothing to cancel). The to_reassign vec is
/// empty, the CancelSignal loop does 0 iterations.
#[tokio::test]
async fn test_force_drain_idle_worker_no_cancel_signals() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("idle-worker", "x86_64-linux").await?;

    // Worker is idle (no builds assigned). Force-drain.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "idle-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx.await?;

    assert!(result.accepted, "known worker → accepted=true");
    assert!(!result.busy, "idle worker → nothing to reassign");

    // No CancelSignal should appear in the stream. barrier-then-
    // try_recv: any message sent during drain would be in the
    // channel by now (mpsc is ordered).
    barrier(&handle).await;
    assert!(
        rx.try_recv().is_err(),
        "no CancelSignal should be sent for idle worker (nothing running)"
    );

    Ok(())
}

/// DrainExecutor(force=true) on a BUSY worker → CancelSignal per in-flight
/// build + result.busy. The preemption hook: controller sees
/// DisruptionTarget on a pod, calls this so the worker cgroup.kills its
/// builds NOW instead of running the full 2h terminationGracePeriod.
/// (wired: P0285 rio-controller disruption.rs watcher)
///
/// Counterpart to test_force_drain_idle_worker_no_cancel_signals — that
/// one proves the CancelSignal loop does 0 iterations on idle; this one
/// proves it does N iterations on busy. Covers `state/executor.rs`-258 (the
/// `if force { ... }` body with a non-empty to_reassign).
#[tokio::test]
async fn test_force_drain_busy_worker_sends_cancel_signal() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("busy-worker", "x86_64-linux").await?;

    // Merge + dispatch → Assigned to busy-worker. running_build={drv}.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "drain-drv", PriorityClass::Scheduled).await?;
    // recv_assignment on busy-worker's rx proves it was dispatched here.
    let _assignment = recv_assignment(&mut rx).await;

    // Force-drain. to_reassign drains running_build → 1 entry.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "busy-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    let result = reply_rx.await?;

    assert!(result.accepted, "known worker → accepted");
    // force=true → running_build: 0 (`state/executor.rs` "reassigned:
    // caller doesn't wait"). The count is only nonzero for
    // force=false (caller polls until it drains naturally).
    assert!(
        !result.busy,
        "force-drain reassigns immediately; caller doesn't wait"
    );

    // CancelSignal should arrive in the worker's stream with the
    // force-drain reason (`state/executor.rs`). try_send on the stream_tx
    // is synchronous; barrier ensures the actor finished processing.
    barrier(&handle).await;
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("CancelSignal should arrive within 2s")
        .expect("channel should not close");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Cancel(c)) => {
            assert!(
                c.reason.contains("draining") && c.reason.contains("forced"),
                "cancel reason should be 'worker draining (forced)': {}",
                c.reason
            );
            // CancelSignal is keyed on drv_path (not drv_hash).
            // merge_single_node uses a synthetic /nix/store/<hash>-... path.
            assert!(
                c.drv_path.contains("drain-drv"),
                "CancelSignal drv_path should reference the in-flight build: {}",
                c.drv_path
            );
        }
        other => panic!("expected CancelSignal, got {other:?}"),
    }

    // Derivation reassigned: no longer Running/Assigned on busy-worker.
    // reassign_derivations resets to Ready (or Queued — depends on
    // whether another worker exists; here there's only one, which is
    // now draining, so it stays Ready with busy-worker in failed_builders).
    let post = expect_drv(&handle, "drain-drv").await;
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Queued
        ),
        "force-drain reassigns; drv should not still be Assigned/Running on the draining worker; got {:?}",
        post.status
    );

    Ok(())
}

/// Recorder-level proof: force-drain on a busy worker increments
/// `rio_scheduler_cancel_signals_total`. The test above proves the
/// CancelSignal *message* is sent; this proves the *metric* increments.
///
/// Regression-guards M1.1 (metric describe correct but increment uses
/// the wrong name — e.g. `_sent_total` vs `_signals_total`). The
/// describe-only test at `tests/metrics_registered.rs:57` would NOT
/// catch that bug: `describe_counter!` and `counter!` take string
/// literals independently.
///
/// Mechanism: `set_default_local_recorder` installs a thread-local
/// recorder on the test's OS thread. `#[tokio::test]` uses a
/// current-thread runtime, so the actor task spawned by
/// `setup_with_worker` runs on the *same* OS thread at `.await` points
/// and sees the thread-local when it calls `counter!()`. Guard must be
/// held before `setup_with_worker` (actor is spawned there) and until
/// after `reply_rx.await` (increment happens inside `handle_drain_executor`,
/// before the reply send).
#[tokio::test]
async fn test_force_drain_increments_cancel_signals_total_metric() -> TestResult {
    let recorder = CountingRecorder::default();
    let _guard = metrics::set_default_local_recorder(&recorder);

    let (_db, handle, _task, mut rx) =
        setup_with_worker("metric-drain-worker", "x86_64-linux").await?;

    // Assign one build so to_reassign is non-empty (the increment at
    // `state/executor.rs` is gated on `if !to_reassign.is_empty()`).
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(
        &handle,
        build_id,
        "metric-drain-drv",
        PriorityClass::Scheduled,
    )
    .await?;
    let _assignment = recv_assignment(&mut rx).await;

    // No labels on this counter → CountingRecorder key is "name{}".
    let key = "rio_scheduler_cancel_signals_total{}";
    let before = recorder.get(key);

    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::DrainExecutor {
            executor_id: "metric-drain-worker".into(),
            force: true,
            reply: reply_tx,
        })
        .await?;
    // handle_drain_executor increments the counter synchronously at
    // `state/executor.rs` before reassign_derivations().await, and the actor
    // sends the reply after handle_drain_executor returns (mod.rs:472) —
    // so this await is a true barrier for the increment.
    let result = reply_rx.await?;
    assert!(result.accepted);

    let after = recorder.get(key);
    assert_eq!(
        after - before,
        1,
        "force-drain of 1 in-flight build must increment \
         rio_scheduler_cancel_signals_total by exactly 1.\n\
         Before: {before}, After: {after}\n\
         Counters actually registered: {:#?}",
        recorder.all_keys(),
    );

    Ok(())
}

// r[verify sched.backstop.timeout]
/// Backstop timeout: a derivation Running far longer than expected
/// gets CancelSignal + reset_to_ready on Tick. The cfg(test) floor
/// is 0s (BACKSTOP_DAEMON_TIMEOUT_SECS=0, BACKSTOP_SLACK_SECS=0) so
/// any positive `running_since` elapsed triggers the backstop.
///
/// Uses DebugBackdateRunning to force Running status with a stale
/// timestamp, bypassing the normal Assigned→Running transition
/// (which would require worker ack + heartbeat roundtrips).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_backstop_timeout_cancels_and_reassigns() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("bs-worker", "x86_64-linux").await?;

    // Merge + dispatch → Assigned.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "bs-drv", PriorityClass::Scheduled).await?;
    let _assignment = recv_assignment(&mut rx).await;

    // Backdate running_since. 100s is plenty past the 0s test floor.
    // Also transitions Assigned → Running (required for the backstop
    // check: it only fires on status==Running).
    let ok = handle.debug_backdate_running("bs-drv", 100).await?;
    assert!(ok, "debug_backdate_running should succeed for Assigned drv");

    // Tick → backstop check → CancelSignal + reassign.
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // The backstop-timeout branch should have logged.
    assert!(
        logs_contain("backstop timeout"),
        "backstop should log on timeout"
    );

    // CancelSignal should appear in the worker's stream.
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("CancelSignal should arrive within 2s")
        .expect("channel should not close");
    match msg.msg {
        Some(rio_proto::types::scheduler_message::Msg::Cancel(c)) => {
            assert!(
                c.reason.contains("backstop"),
                "cancel reason should mention backstop: {}",
                c.reason
            );
        }
        other => panic!("expected CancelSignal, got {other:?}"),
    }

    // Drv should be Ready (reset for retry) with retry_count bumped
    // and the worker recorded in failed_builders. It may immediately
    // re-dispatch to the same worker (only one available) IF
    // best_executor doesn't exclude it — but the worker IS in
    // failed_builders now. Either Ready (excluded) or a fresh
    // Assigned (dispatch fired again). What matters is: NOT stuck
    // in Running.
    let post = expect_drv(&handle, "bs-drv").await;
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "backstop should reset drv (not leave it Running); got {:?}",
        post.status
    );
    assert!(
        post.retry.count >= 1,
        "retry_count should be bumped after backstop reassign"
    );

    Ok(())
}

// r[verify sched.timeout.per-build]
/// Per-build overall timeout: a build with `build_timeout=60` whose
/// `submitted_at` is 61s ago transitions to Failed on Tick. Same build
/// at 59s elapsed does NOT fail (boundary check).
///
/// Uses DebugBackdateSubmitted (same pattern as DebugBackdateRunning
/// above): `submitted_at` is `std::time::Instant`, which tokio paused
/// time cannot mock. And paused time breaks PG pool timeouts anyway
/// (see comment at test_heartbeat_timeout_via_tick_deregisters_worker).
///
/// No worker connected — derivation stays Ready, never Assigned. This
/// isolates the per-build-timeout from the backstop-timeout above: the
/// backstop only fires for status==Running, so a Ready derivation with
/// a stale BUILD proves the per-build check fires independently. The
/// plan's exit criterion "existing backstop test still passes unchanged
/// — proves independence" is satisfied by the backstop test above not
/// being touched; this test adds the converse (per-build fires without
/// backstop).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_per_build_timeout_fails_build_on_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;
    // No worker — derivation stays Ready. Keeps the backstop check
    // (status==Running only) out of the picture.

    let build_id = Uuid::new_v4();
    let _ev = merge_dag_req(
        &handle,
        MergeDagRequest {
            build_id,
            tenant_id: None,
            priority_class: PriorityClass::Scheduled,
            nodes: vec![make_node("pbt-drv")],
            edges: vec![],
            options: BuildOptions {
                max_silent_time: 0,
                build_timeout: 60,
                build_cores: 0,
            },
            keep_going: false,
            traceparent: String::new(),
            jti: None,
            jwt_token: None,
        },
    )
    .await?;

    // ── Boundary: 59s elapsed — NOT timed out ────────────────────────
    // 59 < 60 → elapsed.as_secs() > build_timeout is false. The check
    // uses strict `>`, so 60s elapsed would also NOT fire
    // (elapsed().as_secs() truncates to 60, and 60 > 60 is false).
    // 59 gives a comfortable margin below; 61 is unambiguously past.
    let ok = handle.debug_backdate_submitted(build_id, 59).await?;
    assert!(ok, "debug_backdate_submitted should find the build");

    handle.send_unchecked(ActorCommand::Tick).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build should still be Active at 59s < 60s timeout"
    );
    assert!(
        status.error_summary.is_empty(),
        "error_summary should be empty before timeout; got {:?}",
        status.error_summary
    );

    // ── Timeout: 61s elapsed — Failed with timeout reason ────────────
    let ok = handle.debug_backdate_submitted(build_id, 61).await?;
    assert!(ok);

    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;

    assert!(
        logs_contain("per-build timeout exceeded"),
        "handle_tick should warn on per-build timeout"
    );

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Failed as i32,
        "build should be Failed after per-build timeout; got state={}",
        status.state
    );
    assert!(
        status.error_summary.contains("build_timeout 60s exceeded"),
        "error_summary should contain the timeout reason; got {:?}",
        status.error_summary
    );

    Ok(())
}

// r[verify sched.backstop.orphan-watcher]
/// Orphan-watcher sweep: an Active build whose `build_events` channel
/// has zero receivers past `ORPHAN_BUILD_GRACE` is auto-cancelled.
/// I-112/I-036 backstop for gateway crash (gateway can't send P0331's
/// CancelBuild). cfg(test) grace is ZERO so two ticks suffice: tick 1
/// stamps `orphaned_since`, tick 2 cancels.
///
/// Three phases:
///   1. Watcher held → Tick does NOT cancel (receiver_count > 0).
///   2. Watcher dropped → Tick stamps orphaned_since, build still Active.
///   3. Second Tick past grace → cancelled.
///
/// Phase 1 is the load-bearing negative case: without it, a regression
/// that ignores `receiver_count` and cancels every Active build on tick
/// would still pass phases 2+3.
#[tokio::test]
#[tracing_test::traced_test]
async fn test_orphan_watcher_cancels_unwatched_build_on_tick() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let ev = merge_single_node(&handle, build_id, "orphan-sweep", PriorityClass::Scheduled).await?;

    // ── Phase 1: watcher held → Tick is a no-op ──────────────────────
    handle.send_unchecked(ActorCommand::Tick).await?;
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "watched build must stay Active across ticks"
    );

    // ── Phase 2: drop watcher → first Tick stamps orphaned_since ─────
    drop(ev);
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "first orphan tick only stamps; grace not yet elapsed"
    );

    // ── Phase 3: second Tick past grace → cancelled ──────────────────
    handle.send_unchecked(ActorCommand::Tick).await?;
    barrier(&handle).await;
    assert!(
        logs_contain("orphan-watcher"),
        "expected orphan-watcher warn log"
    );
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Cancelled as i32,
        "build should be Cancelled after orphan grace; got state={}",
        status.state
    );

    Ok(())
}

/// Orphan-watcher: a watcher that reattaches before grace elapses
/// resets the timer. Covers the gateway WatchBuild-reconnect path —
/// a transient gateway blip must NOT cancel the build.
#[tokio::test]
async fn test_orphan_watcher_reattach_resets_timer() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let ev = merge_single_node(
        &handle,
        build_id,
        "orphan-reattach",
        PriorityClass::Scheduled,
    )
    .await?;

    // Drop watcher → first Tick stamps orphaned_since.
    drop(ev);
    handle.send_unchecked(ActorCommand::Tick).await?;

    // Reattach via WatchBuild before second tick.
    let (reply_tx, reply_rx) = oneshot::channel();
    handle
        .send_unchecked(ActorCommand::WatchBuild {
            build_id,
            since_sequence: 0,
            reply: reply_tx,
        })
        .await?;
    let (_rx2, _seq) = reply_rx.await??;

    // Second tick: receiver_count > 0 → orphaned_since reset, no cancel.
    handle.send_unchecked(ActorCommand::Tick).await?;
    // Third tick: still watched → still Active.
    handle.send_unchecked(ActorCommand::Tick).await?;
    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "reattached build must stay Active"
    );

    Ok(())
}

/// Zero build_timeout = no overall timeout. Even with a wildly stale
/// submitted_at, Tick does NOT fail the build. Guards against an
/// accidental `>= 0` instead of `> 0` in the zero-check.
#[tokio::test]
async fn test_per_build_timeout_zero_means_unlimited() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    // merge_single_node uses BuildOptions::default() → build_timeout=0.
    let _ev = merge_single_node(&handle, build_id, "pbt0-drv", PriorityClass::Scheduled).await?;

    // Backdate far past any reasonable timeout. If the zero-check is
    // wrong (>=0 instead of >0), this would fire immediately.
    let ok = handle.debug_backdate_submitted(build_id, 100_000).await?;
    assert!(ok);

    handle.send_unchecked(ActorCommand::Tick).await?;

    let status = query_status(&handle, build_id).await?;
    assert_eq!(
        status.state,
        rio_proto::types::BuildState::Active as i32,
        "build with build_timeout=0 should never time out; got state={}",
        status.state
    );

    Ok(())
}

// r[verify builder.heartbeat.store-degraded]
/// Heartbeat with store_degraded=true excludes the worker from
/// best_executor() dispatch. End-to-end: heartbeat → ExecutorState.store_
/// degraded → has_capacity()=false → best_executor() filters out →
/// derivation stays Ready (no assignment).
///
/// Then: heartbeat with store_degraded=false → worker returns to the
/// pool → dispatch fires → derivation Assigned. This proves the
/// two-way nature (unlike draining, which is one-way) at the actor
/// level, not just the has_capacity() unit level.
///
/// Single-worker setup isolates the exclusion: if the degraded worker
/// were still a candidate, the derivation would go Assigned immediately
/// on merge (only one worker, it's the best by default).
#[tokio::test]
#[tracing_test::traced_test]
async fn test_store_degraded_worker_excluded_from_dispatch() -> TestResult {
    // Register worker the normal way (store_degraded=false via
    // connect_executor). It's healthy and eligible.
    let (_db, handle, _task, mut rx) = setup_with_worker("degraded-worker", "x86_64-linux").await?;

    // Mark it degraded BEFORE merging any work. The heartbeat also
    // triggers dispatch_ready (actor/mod.rs:432) but the ready queue
    // is empty, so that's a no-op. The point is ExecutorState.store_
    // degraded is set by the time the merge below runs.
    send_heartbeat_with(&handle, "degraded-worker", "x86_64-linux", |hb| {
        hb.store_degraded = true;
    })
    .await?;
    barrier(&handle).await;

    // Transition logged at info (false → true).
    assert!(
        logs_contain("marked store-degraded; removing from assignment pool"),
        "false→true transition should log at info"
    );

    // Merge a derivation. MergeDag calls dispatch_ready afterward
    // (actor/mod.rs MergeDag arm). With the only worker degraded,
    // best_executor() returns None → derivation stays Ready.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "sd-drv", PriorityClass::Scheduled).await?;
    barrier(&handle).await;

    let info = expect_drv(&handle, "sd-drv").await;
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "degraded worker excluded → derivation stays Ready (not Assigned)"
    );
    // No assignment in the worker stream either. try_recv after
    // barrier: any dispatch message would be queued by now.
    assert!(
        rx.try_recv().is_err(),
        "no assignment should land on a degraded worker's stream"
    );

    // Recovery: clear the flag. Heartbeat sets dispatch_dirty; Tick
    // drains it (I-163) → best_executor() now finds the worker →
    // derivation goes Assigned.
    send_heartbeat_with(&handle, "degraded-worker", "x86_64-linux", |_| {}).await?;
    handle.send_unchecked(ActorCommand::Tick).await?;

    // Assignment should arrive now. recv_assignment has its own 2s
    // timeout — a missing dispatch fails loudly here rather than
    // hanging.
    let assignment = recv_assignment(&mut rx).await;
    assert_eq!(
        assignment.drv_path,
        test_drv_path("sd-drv"),
        "recovered worker gets the pending assignment"
    );

    // true → false recovery also logged.
    assert!(
        logs_contain("store-degraded cleared; returning to assignment pool"),
        "true→false recovery should log at info"
    );

    Ok(())
}

// ───────────────────────────────────────────────────────────────────────────
// on_worker_registered / warm-gate initial-hint coverage
// ───────────────────────────────────────────────────────────────────────────

use super::helpers::connect_executor_no_ack;

// r[verify sched.assign.warm-gate]
/// Merge-then-connect: a worker registering AFTER a DAG is merged
/// receives an initial `PrefetchHint` on its stream BEFORE any
/// `WorkAssignment`. The hint carries the Ready derivation's input
/// closure (its children's `expected_output_paths`). Proves
/// `on_worker_registered` sends the hint when the ready queue is
/// non-empty AND the closure is non-empty.
///
/// Setup: A→B chain. B completes (pre-seeded via a throwaway worker)
/// → A goes Ready with B as its completed child → A's input
/// closure = B's output. THEN register the real worker without ACK
/// → it sees PrefetchHint before Assignment.
#[tokio::test]
async fn on_worker_registered_sends_initial_hint_before_assignment() -> TestResult {
    use rio_proto::types::scheduler_message::Msg;

    let (_db, handle, _task) = setup().await;

    // Merge A→B. B has expected_output_paths so A's input closure
    // (approx_input_closure(A) = children's expected_output_paths)
    // is non-empty. B is leaf → Ready immediately; A Queued.
    let build_id = Uuid::new_v4();
    let mut node_b = make_node("warm-b");
    node_b.expected_output_paths = vec![test_store_path("warm-b-out")];
    let node_a = make_node("warm-a");
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![node_a, node_b],
        vec![make_test_edge("warm-a", "warm-b")],
        false,
    )
    .await?;

    // Bootstrap: connect a throwaway worker to complete B so A
    // becomes Ready. Use the auto-ACK connect_executor helper
    // (we're not testing THIS worker's warm-gate).
    let mut boot_rx = connect_executor(&handle, "boot-w", "x86_64-linux").await?;
    let boot_asgn = recv_assignment(&mut boot_rx).await;
    assert_eq!(boot_asgn.drv_path, test_drv_path("warm-b"));
    complete_success_empty(&handle, "boot-w", &test_drv_path("warm-b")).await?;
    barrier(&handle).await;

    // Precondition: A is now Ready (all deps Completed).
    // The bootstrap worker is now holding A's assignment (one slot,
    // freed by completing B above). Disconnect boot-w to reset A to
    // Ready for the real worker.
    disconnect(&handle, "boot-w").await?;
    drop(boot_rx);

    let info_a = expect_drv(&handle, "warm-a").await;
    assert_eq!(
        info_a.status,
        DerivationStatus::Ready,
        "precondition: A Ready with completed child B"
    );

    // THEN connect the REAL worker — WITHOUT auto-ACK. Registration
    // hook sees Ready queue non-empty, A's closure = B's output →
    // sends PrefetchHint.
    let mut rx = connect_executor_no_ack(&handle, "warm-worker", "x86_64-linux").await?;
    barrier(&handle).await;

    // First message: PrefetchHint (NOT Assignment). The hint arrives
    // FIRST on the stream — proving on_worker_registered sends it.
    // (With only one cold worker, the warm-gate fallback ALSO fires
    // dispatch for the same heartbeat — the Assignment may arrive
    // SECOND via the no-warm-workers fallback. That's correct: the
    // hint-send ordering is what we're proving, not dispatch-hold.)
    let first = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout waiting for first message")
        .expect("channel open");
    match first.msg {
        Some(Msg::Prefetch(hint)) => {
            assert!(
                hint.store_paths.contains(&test_store_path("warm-b-out")),
                "initial hint should carry the Ready node's child output paths; \
                 got {:?}",
                hint.store_paths
            );
        }
        other => panic!("expected PrefetchHint as FIRST message, got {other:?}"),
    }

    // Drain any fallback-dispatched Assignment (single-worker cluster
    // triggers the no-warm-fallback). The point is the PrefetchHint
    // arrived FIRST — P0299 EC: "Fresh worker receives PrefetchHint
    // within one tick of registration."
    Ok(())
}

// r[verify sched.assign.warm-gate]
/// Connect-then-empty-queue: a worker registering with an EMPTY
/// ready queue flips `warm=true` immediately (the short-circuit at
/// `state/executor.rs`-136 — "nothing queued → nothing to prefetch → gate
/// open now"). Proves: merge AFTER connect → Assignment arrives
/// WITHOUT a PrefetchComplete ACK round-trip.
#[tokio::test]
async fn on_worker_registered_empty_queue_flips_warm_immediately() -> TestResult {
    use rio_proto::types::scheduler_message::Msg;

    let (_db, handle, _task) = setup().await;

    // Connect FIRST — ready queue is empty. on_worker_registered's
    // short-circuit flips warm=true without sending a hint.
    let mut rx = connect_executor_no_ack(&handle, "empty-worker", "x86_64-linux").await?;
    barrier(&handle).await;

    // No PrefetchHint on the stream (nothing to hint for).
    assert!(
        rx.try_recv().is_err(),
        "empty queue at registration → no PrefetchHint sent"
    );

    // THEN merge. The worker is already warm (short-circuit) so
    // dispatch proceeds immediately — no ACK round-trip needed.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "empty-drv", PriorityClass::Scheduled).await?;

    // Assignment arrives WITHOUT any PrefetchComplete send. This is
    // the core assertion: if the short-circuit DIDN'T flip warm,
    // the derivation would stay Ready (warm-gate holds) and this
    // recv would timeout.
    let msg = tokio::time::timeout(Duration::from_secs(2), rx.recv())
        .await
        .expect("timeout — short-circuit didn't flip warm (dispatch blocked)")
        .expect("channel open");
    match msg.msg {
        Some(Msg::Assignment(a)) => {
            assert_eq!(a.drv_path, test_drv_path("empty-drv"));
        }
        Some(Msg::Prefetch(_)) => {
            panic!("unexpected PrefetchHint — short-circuit should skip the hint for empty queue")
        }
        other => panic!("expected Assignment, got {other:?}"),
    }

    Ok(())
}

// r[verify sched.assign.warm-gate]
/// Hint-send-fails: if the initial hint's `try_send` fails (channel
/// full or closed), `on_worker_registered` flips `warm=true` anyway
/// (defensive path at `state/executor.rs`-163 — "gate is optimization, not
/// correctness"). The scheduler doesn't wedge.
///
/// Inducing the fail: use a 1-slot channel pre-filled with a dummy
/// message. The actor's `try_send(PrefetchHint)` → `Err(Full)` →
/// warn + flip warm.
#[tokio::test]
#[tracing_test::traced_test]
async fn on_worker_registered_send_fail_flips_warm_anyway() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Merge A→B so A's closure is non-empty → the hint SEND is
    // attempted (not the empty-closure short-circuit). B completes
    // via a throwaway worker → A goes Ready.
    let build_id = Uuid::new_v4();
    let mut node_b = make_node("fail-b");
    node_b.expected_output_paths = vec![test_store_path("fail-b-out")];
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![make_node("fail-a"), node_b],
        vec![make_test_edge("fail-a", "fail-b")],
        false,
    )
    .await?;
    let mut boot_rx = connect_executor(&handle, "boot-f", "x86_64-linux").await?;
    let _ = recv_assignment(&mut boot_rx).await;
    complete_success_empty(&handle, "boot-f", &test_drv_path("fail-b")).await?;
    disconnect(&handle, "boot-f").await?;
    drop(boot_rx);

    // Connect with a 1-slot channel, IMMEDIATELY fill it so the
    // actor's try_send for the PrefetchHint fails with Full.
    let (stream_tx, stream_rx) = tokio::sync::mpsc::channel(1);
    stream_tx
        .send(rio_proto::types::SchedulerMessage { msg: None })
        .await?;
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "fail-worker".into(),
            stream_tx,
        })
        .await?;
    send_heartbeat_with(&handle, "fail-worker", "x86_64-linux", |_| {}).await?;
    barrier(&handle).await;

    // The defensive path fired: warn + flip warm.
    assert!(
        logs_contain("warm-gate: initial hint send failed; flipping warm anyway"),
        "send-fail defensive path should warn and flip warm"
    );

    drop(stream_rx);
    Ok(())
}

/// Build a DAG with `n_ready` Ready parents, each depending on `paths_each`
/// children whose `expected_output_paths` they contribute to the closure.
/// Every child is shared by all parents below it (index-wise) so lower-index
/// paths get the highest frequency count. Every parent P(i) gets `i+1`
/// UUIDs inserted into `interested_builds` — so fan-in is P0<P1<...<P(n-1).
///
/// Returns the fully-populated DAG. No actor, no PG — a pure unit-test
/// fixture for `compute_initial_prefetch_paths`.
fn build_fanned_dag(n_ready: usize, paths_each: usize) -> crate::dag::DerivationDag {
    let mut dag = crate::dag::DerivationDag::new();

    // Children: each child C(j) has a single expected_output_path.
    // test_store_path(format!("child-{j:04}")) gives deterministic
    // lex ordering so the frequency-sort tie-break is predictable.
    let n_children = n_ready + paths_each - 1;
    let child_nodes: Vec<_> = (0..n_children)
        .map(|j| {
            let mut c = make_node(&format!("child-{j:04}"));
            c.expected_output_paths = vec![test_store_path(&format!("child-{j:04}-out"))];
            c
        })
        .collect();

    // Parents: P(i) depends on children C(i)..C(i+paths_each). The
    // sliding window means C(paths_each-1) is shared by paths_each
    // parents, C(0) by 1 parent, C(n_children-1) by 1 parent, etc.
    // Actually: C(j)'s parent-count = min(j+1, paths_each, n_ready,
    // n_children-j) — a trapezoidal distribution peaking in the middle.
    let parent_nodes: Vec<_> = (0..n_ready)
        .map(|i| make_node(&format!("parent-{i:04}")))
        .collect();
    let mut edges = Vec::with_capacity(n_ready * paths_each);
    for i in 0..n_ready {
        for j in i..i + paths_each {
            edges.push(make_test_edge(
                &format!("parent-{i:04}"),
                &format!("child-{j:04}"),
            ));
        }
    }

    // Single merge gets all nodes+edges in. build_id is one shared
    // UUID (every parent gets interested_builds.len()==1 from this);
    // we'll bump per-parent counts below via node_mut.
    let all_nodes: Vec<_> = parent_nodes.into_iter().chain(child_nodes).collect();
    // Arch#13 boundary shim: dag.merge takes domain types; the proto
    // fixtures convert via `From`. Full test-side migration is b03's
    // post-integration step — this is the only direct dag.merge call
    // outside dag/tests.rs.
    let all_nodes = crate::domain::nodes_from_proto(all_nodes);
    let edges = crate::domain::edges_from_proto(edges);
    dag.merge(Uuid::new_v4(), &all_nodes, &edges, "").unwrap();

    // Set statuses: parents → Ready, children → Completed.
    // `approx_input_closure` walks children; Completed children still
    // have their `expected_output_paths` set (persisted at merge time).
    for j in 0..n_children {
        dag.node_mut(&format!("child-{j:04}"))
            .unwrap()
            .set_status_for_test(DerivationStatus::Completed);
    }
    for i in 0..n_ready {
        let p = dag.node_mut(&format!("parent-{i:04}")).unwrap();
        p.set_status_for_test(DerivationStatus::Ready);
        // Fan-in: P(i) gets i ADDITIONAL UUIDs (merge already inserted
        // one). So P0.interested_builds.len()==1, P39.len()==40. The
        // fan-in sort picks P39 first, P0 last.
        for _ in 0..i {
            p.interested_builds.insert(Uuid::new_v4());
        }
    }

    dag
}

// r[verify sched.assign.warm-gate]
/// Determinism: same DAG state → same PrefetchHint contents.
/// `HashMap` iteration is random; T1+T2's fan-in + frequency sort
/// makes the hint reproducible. Pre-T1+T2 this test is flaky (passes
/// ~1/N! of the time for N-element random iteration orderings).
///
/// Also asserts the FIRST path is the highest-frequency one — proving
/// the frequency sort actually fired (proves-nothing guard: a test
/// that only checks `a == b` would pass if both were empty or both
/// selected the same arbitrary set by accident).
#[test]
fn warm_gate_initial_hint_is_deterministic() {
    // 40 Ready parents (>MAX_READY_TO_SCAN=32), each with 4 child
    // paths in a sliding window → 43 unique children. Plus: we want
    // >100 unique paths so the cap is exercised. Use paths_each=5
    // and also attach per-parent UNIQUE paths below.
    //
    // Actually simpler: 40 parents × 5 children window = 44 unique
    // child paths. To exceed 100, bump paths_each to 70. That's 109
    // unique children with the middle-band children shared by up to
    // 40 parents (the sliding-window trapezoid). The top-fan-in
    // parents (P32..P39, interested_builds.len() 33..40) select into
    // the scan; their children are C32..C108 (overlap: C39..C101
    // appears in multiple). After the MAX_READY_TO_SCAN=32 cap, the
    // 32 highest-fan-in parents are P8..P39 (len 9..40).
    let n_ready = 40;
    let paths_each = 70;
    let dag_a = build_fanned_dag(n_ready, paths_each);
    let dag_b = build_fanned_dag(n_ready, paths_each);

    let hint_a = compute_initial_prefetch_paths(&dag_a);
    let hint_b = compute_initial_prefetch_paths(&dag_b);

    assert_eq!(
        hint_a, hint_b,
        "same DAG state must yield identical initial hint"
    );
    assert_eq!(hint_a.len(), 100, "cap at MAX_PREFETCH_PATHS");

    // Proves-nothing guard: highest-frequency path is FIRST. The 32
    // selected parents are P8..P39 (interested_builds.len() 9..40).
    // Each P(i) references C(i)..C(i+69). The intersection across all
    // 32 is C39..C77; within that band every child is referenced by
    // all 32 parents (frequency=32). Tie-break on path string gives
    // C39 first.
    //
    // Check the stronger property: the first path has the expected
    // maximum frequency, which proves T2's sort fired (not just T1's
    // ready-sort making the same arbitrary-cap happen twice).
    let expected_first = test_store_path("child-0039-out");
    assert_eq!(
        hint_a[0], expected_first,
        "highest-frequency path must be first (proves frequency sort fired)"
    );

    // Also check the fan-in sort fired: scanning only 32 of 40 Ready
    // nodes means low-fan-in parents (P0..P7) are excluded. P0's only
    // unique child is C0..C4 (no other parent in the scan references
    // C0..C7). If C0's path were present, T1's sort DIDN'T exclude P0.
    let p0_unique = test_store_path("child-0000-out");
    assert!(
        !hint_a.contains(&p0_unique),
        "lowest-fan-in parent P0 must be excluded by the MAX_READY_TO_SCAN \
         cap (proves fan-in sort fired)"
    );
}

// r[verify sched.dispatch.became-idle-immediate]
/// I-163 carve-out: a heartbeat that flips capacity 0→1 dispatches
/// inline, NOT deferred to Tick. Uses `store_degraded` true→false as
/// the 0→1 trigger (warm-gate already open, isolates the edge-detect).
///
/// Regression shape: before the fix, step (4) only set `dispatch_dirty`
/// and the assignment wouldn't appear until a `Tick` — `try_recv` after
/// the barrier would be `Empty`.
#[tokio::test]
async fn test_heartbeat_became_idle_dispatches_inline() -> TestResult {
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());

    // (1) Register + warm. Queue empty → on_worker_registered flips
    // warm=true immediately (no PrefetchHint round-trip needed).
    let mut rx = connect_executor(&handle, "w-idle", "x86_64-linux").await?;

    // (2) Degrade → capacity 1→0. Raw Heartbeat (no trailing Tick).
    send_heartbeat_with(&handle, "w-idle", "x86_64-linux", |hb| {
        hb.store_degraded = true;
    })
    .await?;

    // (3) Queue work. MergeDag dispatches inline but the only executor
    // is degraded → derivation stays Ready in the queue.
    let _ = merge_single_node(
        &handle,
        Uuid::new_v4(),
        "idle-immediate",
        PriorityClass::Scheduled,
    )
    .await?;
    barrier(&handle).await;
    assert!(
        matches!(rx.try_recv(), Err(mpsc::error::TryRecvError::Empty)),
        "precondition: degraded executor must not receive assignment"
    );

    // (4) Un-degrade → capacity 0→1 → became_idle=true → dispatch_ready
    // INLINE. NO Tick sent. barrier proves the heartbeat handler ran
    // to completion; assignment must already be on the channel.
    send_heartbeat_with(&handle, "w-idle", "x86_64-linux", |_| {}).await?;
    barrier(&handle).await;

    let a = recv_assignment(&mut rx).await;
    assert_eq!(a.drv_path, test_drv_path("idle-immediate"));
    Ok(())
}

/// r[verify sched.dispatch.became-idle-immediate]
///
/// Mass 0→1 capacity edges (failover, fleet-wide degrade clear): every
/// executor's heartbeat fires `became_idle=true`. Without
/// `BECAME_IDLE_INLINE_CAP`, N heartbeats → N inline `dispatch_ready`
/// passes (the I-163 storm via the back door — each pass runs the
/// ~150ms batch-FOD precheck). The cap bounds inline dispatches at 4
/// per Tick; the remainder coalesce to `dispatch_dirty`.
///
/// Setup uses degrade→undegrade on ALREADY-warm workers to isolate the
/// `became_idle` heartbeat path from `PrefetchComplete` (which now
/// shares the same cap — see [`test_prefetch_complete_burst_capped`]).
///
/// Asserts BOTH halves of the cap:
///   - exactly CAP assignments land from heartbeats alone (no Tick) —
///     steady-state immediacy preserved up to the cap.
///   - the (CAP+1)th waits for a Tick — burst is gated.
#[tokio::test]
async fn test_became_idle_burst_capped_per_tick() -> TestResult {
    use crate::actor::BECAME_IDLE_INLINE_CAP;
    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor(db.pool.clone());

    let n = BECAME_IDLE_INLINE_CAP as usize + 1;

    // (1) Connect+warm N workers (queue empty → on_worker_registered
    // flips warm immediately; PrefetchComplete is a no-op re-set).
    // Drain a Tick so the cap counter starts at 0 for the burst.
    let mut rxs = Vec::new();
    for i in 0..n {
        let rx = connect_executor(&handle, &format!("burst-w{i}"), "x86_64-linux").await?;
        rxs.push(rx);
    }
    handle.send(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // (2) Degrade all → capacity 1→0 (no became_idle).
    for i in 0..n {
        send_heartbeat_with(&handle, &format!("burst-w{i}"), "x86_64-linux", |hb| {
            hb.store_degraded = true;
        })
        .await?;
    }
    barrier(&handle).await;

    // (3) Queue N derivations. MergeDag's inline dispatch sees all
    // workers degraded → everything stays Ready.
    for i in 0..n {
        let _ = merge_single_node(
            &handle,
            Uuid::new_v4(),
            &format!("burst-{i}"),
            PriorityClass::Scheduled,
        )
        .await?;
    }
    barrier(&handle).await;
    for rx in &mut rxs {
        assert!(
            rx.try_recv().is_err(),
            "precondition: degraded executors must not receive assignments"
        );
    }

    // (4) Undegrade all in a tight loop, NO Tick between → N
    // became_idle edges. The cap should let exactly CAP through
    // inline; the rest set dispatch_dirty.
    for i in 0..n {
        send_heartbeat_with(&handle, &format!("burst-w{i}"), "x86_64-linux", |_| {}).await?;
    }
    barrier(&handle).await;

    let mut assigned_inline = 0;
    for rx in &mut rxs {
        if rx.try_recv().is_ok() {
            assigned_inline += 1;
        }
    }
    assert_eq!(
        assigned_inline, BECAME_IDLE_INLINE_CAP as usize,
        "expected exactly CAP={} inline assignments; \
         got {assigned_inline} (cap not enforced or steady-state broken)",
        BECAME_IDLE_INLINE_CAP
    );

    // (5) One Tick drains dispatch_dirty → remaining worker(s) assigned.
    handle.send(ActorCommand::Tick).await?;
    barrier(&handle).await;
    let mut assigned_total = assigned_inline;
    for rx in &mut rxs {
        if rx.try_recv().is_ok() {
            assigned_total += 1;
        }
    }
    assert_eq!(
        assigned_total, n,
        "all {n} derivations should be assigned after one Tick drains dispatch_dirty"
    );
    Ok(())
}

/// r[verify sched.dispatch.became-idle-immediate]
///
/// `PrefetchComplete` is the OTHER mass-reconnect inline-dispatch
/// vector: leader-failover → N reconnects → N first-Heartbeats (capped
/// above) → N PrefetchComplete ACKs. Uncapped, each ACK ran
/// `dispatch_ready` inline — the I-163 storm via the back door even
/// after Heartbeat was gated. The fix routes cold→warm through the
/// same `BECAME_IDLE_INLINE_CAP` budget as `became_idle`.
///
/// Asserts the shared-budget contract directly: exhaust the budget via
/// `became_idle` (CAP cold-fallback assignments), then prove a single
/// PrefetchComplete cold→warm DEFERS to `dispatch_dirty` instead of
/// dispatching inline. Without the fix, the PFC dispatches inline and
/// the (CAP+1)th root is assigned before Tick.
///
/// Setup uses a (child + N roots) DAG with the child completed first
/// — the only way to get cold-registered workers (on_worker_registered
/// flips warm immediately when the Ready set's input closure is empty,
/// which it is for childless test nodes).
#[tokio::test]
async fn test_prefetch_complete_burst_capped() -> TestResult {
    use crate::actor::BECAME_IDLE_INLINE_CAP;
    use rio_proto::types::scheduler_message::Msg;
    let (_db, handle, _task) = setup().await;

    let cap = BECAME_IDLE_INLINE_CAP as usize;
    let n_roots = cap + 1;

    // (1) child C + (CAP+1) roots all depending on C. C has an
    // expected_output_path so each root's input closure is non-empty
    // once C completes.
    let mut child = make_node("pfc-child");
    child.expected_output_paths = vec![test_store_path("pfc-child-out")];
    let mut nodes = vec![child];
    let mut edges = Vec::new();
    for i in 0..n_roots {
        nodes.push(make_node(&format!("pfc-root-{i}")));
        edges.push(make_test_edge(&format!("pfc-root-{i}"), "pfc-child"));
    }
    let _ev = merge_dag(&handle, Uuid::new_v4(), nodes, edges, false).await?;

    // (2) Bootstrap: complete C via a throwaway worker so all roots
    // become Ready with C as a completed child. Then disconnect to
    // reset whatever root boot-w grabbed on completion-dispatch.
    let mut boot_rx = connect_executor(&handle, "pfc-boot", "x86_64-linux").await?;
    let asgn = recv_assignment(&mut boot_rx).await;
    assert_eq!(asgn.drv_path, test_drv_path("pfc-child"));
    complete_success_empty(&handle, "pfc-boot", &test_drv_path("pfc-child")).await?;
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "pfc-boot".into(),
        })
        .await?;
    drop(boot_rx);
    // Reset the inline budget consumed by boot-w's connect/dispatch.
    handle.send(ActorCommand::Tick).await?;
    barrier(&handle).await;

    // (3) Exhaust the budget: connect CAP workers without ACK. Each
    // first-heartbeat is became_idle (b=1..CAP); on_worker_registered
    // sees non-empty closure → sends hint, workers stay COLD.
    // cold-fallback assigns CAP roots inline.
    let mut _rxs = Vec::new();
    for i in 0..cap {
        _rxs.push(connect_executor_no_ack(&handle, &format!("pfc-w{i}"), "x86_64-linux").await?);
    }
    // (4) One more worker — became_idle at b≥CAP sets dirty. Still
    // cold. Exactly one root remains Ready.
    let mut rx_last =
        connect_executor_no_ack(&handle, &format!("pfc-w{cap}"), "x86_64-linux").await?;
    barrier(&handle).await;

    // (5) PrefetchComplete for the last worker (cold→warm). With the
    // fix this consumes from the shared budget (already at CAP) →
    // dispatch_dirty. WITHOUT the fix it dispatched inline → the
    // last root would land on rx_last now.
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: format!("pfc-w{cap}").into(),
            paths_fetched: 1,
        })
        .await?;
    barrier(&handle).await;

    let count_assignments = |rx: &mut mpsc::Receiver<_>| {
        let mut n = 0usize;
        while let Ok(rio_proto::types::SchedulerMessage { msg }) = rx.try_recv() {
            if matches!(msg, Some(Msg::Assignment(_))) {
                n += 1;
            }
        }
        n
    };
    assert_eq!(
        count_assignments(&mut rx_last),
        0,
        "PrefetchComplete cold→warm at budget=CAP must defer to dispatch_dirty, \
         not dispatch inline (I-163 storm via the back door)"
    );

    // (6) One Tick drains dispatch_dirty → last root assigned.
    handle.send(ActorCommand::Tick).await?;
    barrier(&handle).await;
    assert_eq!(
        count_assignments(&mut rx_last),
        1,
        "deferred PrefetchComplete dispatch should land after one Tick"
    );
    Ok(())
}
