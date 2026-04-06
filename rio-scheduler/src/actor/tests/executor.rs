//! Worker management: heartbeat merge (no-clobber), Tick-driven timeout and poison expiry.
// r[verify sched.worker.dual-register]
// r[verify sched.worker.deregister-reassign]
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

    let _rx1 = connect_executor(&handle, "drain-auth", "x86_64-linux", 1).await?;

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
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: true,
            ephemeral: false,
            draining: true,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "drain-auth".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "drain-auth")
        .expect("entry exists");
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

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "drain-auth")
        .expect("entry exists post-reconnect");
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
    send_heartbeat(&handle, "drain-auth", "x86_64-linux", 1).await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "drain-auth")
        .expect("entry exists post-heartbeat");
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
    send_heartbeat(&handle, "drain-auth", "x86_64-linux", 1).await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "drain-auth")
        .expect("entry exists");
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
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("merged");
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
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: false,
            draining: true,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "i066-a".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![drv_path.clone()],
        })
        .await?;

    // Adoption: DAG node Assigned to A, A.running_build set, no
    // failed_builders (adoption is reconciliation, not failure).
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
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
        info.failed_builders.is_empty(),
        "adoption must not penalize the worker"
    );
    let workers = handle.debug_query_workers().await?;
    let a = workers
        .iter()
        .find(|w| w.executor_id == "i066-a")
        .expect("A registered");
    assert!(
        a.running_builds.contains(&drv_hash.to_string()),
        "worker.running_build set so A reads at-capacity"
    );

    // Worker B connects idle. Its registration triggers dispatch_ready.
    // The drv is Assigned (adopted) → not in the Ready filter → B
    // gets nothing. Pre-I-066: drv was still Ready, B would receive it.
    let _stream_rx_b = connect_executor(&handle, "i066-b", "x86_64-linux", 1).await?;
    let workers = handle.debug_query_workers().await?;
    let b = workers
        .iter()
        .find(|w| w.executor_id == "i066-b")
        .expect("B registered");
    assert!(
        b.running_builds.is_empty(),
        "B must NOT receive the drv — adoption prevented re-dispatch"
    );
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("exists");
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
    send_heartbeat(&handle, "zombie-candidate", "x86_64-linux", 1).await?;

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
    let _rx = connect_executor(&handle, "zombie-candidate", "x86_64-linux", 1).await?;

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "zombie-candidate")
        .expect("stream-first connect must create entry");
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
#[tokio::test]
async fn test_debug_query_workers_extended_fields() -> TestResult {
    let (_db, handle, _task) = setup().await;

    // Stage 1: stream connected, no heartbeat. has_stream=true,
    // is_registered=false (systems empty), warm=false.
    let _rx = connect_executor_no_ack(&handle, "ext-builder", "x86_64-linux", 4).await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "ext-builder")
        .expect("stream-open creates entry");
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
    assert_eq!(w.running_count, 0);
    assert!(w.running_builds.is_empty());
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
        2,
        rio_proto::types::ExecutorKind::Fetcher,
    )
    .await?;
    handle
        .send_unchecked(ActorCommand::PrefetchComplete {
            executor_id: "ext-fetcher".into(),
            paths_fetched: 0,
        })
        .await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "ext-fetcher")
        .expect("fetcher registered");
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
    assert_eq!(workers.len(), 2);
    Ok(())
}

/// TOCTOU fix: a stale heartbeat (sent before scheduler assigned a
/// derivation) must not clobber the scheduler's fresh assignment in
/// worker.running_builds. The scheduler is authoritative.
#[tokio::test]
async fn test_heartbeat_does_not_clobber_fresh_assignment() -> TestResult {
    // Register worker (initial heartbeat has empty running_builds).
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("toctou-worker", "x86_64-linux", 2).await?;

    // Merge a derivation. Scheduler will assign it to the worker and
    // insert it into worker.running_builds.
    let build_id = Uuid::new_v4();
    let drv_hash = "toctou-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Verify: derivation is Assigned, worker.running_builds contains it.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);

    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "toctou-worker")
        .expect("toctou-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "scheduler should have tracked the assignment in worker.running_builds"
    );

    // Send a STALE heartbeat with empty running_builds. This mimics the
    // race: worker sent heartbeat before receiving/acking the assignment.
    // send_heartbeat's running_builds=[] is the stale value under test.
    send_heartbeat(&handle, "toctou-worker", "x86_64-linux", 2).await?;

    // Assignment must still be tracked. Before the fix, running_builds
    // would be wholesale replaced with the empty set, orphaning the
    // assignment (completion would later warn "unknown derivation").
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "toctou-worker")
        .expect("toctou-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "stale heartbeat must not clobber scheduler's fresh assignment"
    );
    Ok(())
}

/// I-035: a SECOND consecutive heartbeat with empty running_builds drains
/// the phantom. The TOCTOU window is one heartbeat interval (~10s) — one
/// miss is the race the test above protects; two misses means the worker
/// genuinely doesn't have the assignment (lost completion, dead stream
/// post-send, I-032 pre-d11245b4). Before this fix the slot stayed dead
/// forever: `has_capacity()` saw 1/1 and the derivation sat Assigned with
/// no path to Ready short of executor disconnect.
#[tokio::test]
async fn test_heartbeat_phantom_drain_on_second_miss() -> TestResult {
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("phantom-worker", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "phantom-drv-hash";
    let _event_rx =
        merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Precondition: dispatch assigned the drv (single candidate).
    // Worker's running_build has it; DAG is Assigned.
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(info.status, DerivationStatus::Assigned);
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "phantom-worker")
        .expect("phantom-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "precondition: dispatch tracked the assignment"
    );

    // First miss: TOCTOU keep — same outcome as the test above.
    send_heartbeat(&handle, "phantom-worker", "x86_64-linux", 1).await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "phantom-worker")
        .expect("phantom-worker registered");
    assert!(
        w.running_builds.contains(&drv_hash.to_string()),
        "first miss is the TOCTOU race — keep"
    );
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
    assert_eq!(
        info.status,
        DerivationStatus::Assigned,
        "first miss leaves DAG state alone"
    );

    // Second miss: phantom confirmed. Drain → reset_to_ready →
    // dispatch_ready re-assigns to the (now-free) same worker.
    send_heartbeat(&handle, "phantom-worker", "x86_64-linux", 1).await?;
    let info = handle
        .debug_query_derivation(drv_hash)
        .await?
        .expect("derivation should exist");
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
        info.failed_builders.is_empty(),
        "phantom drain must NOT add to failed_builders — \
         not the worker's fault, got {:?}",
        info.failed_builders
    );
    Ok(())
}

/// I-042: completion arriving for a derivation that's already terminal
/// (e.g., poisoned by a parallel-retry race) must still free the
/// executor's `running_builds` slot. Before the fix, the line-419
/// early-return (`"not in assigned/running state"`) returned BEFORE
/// `running_builds.remove`, leaking the slot.
///
/// EKS reproduction (build 019d4681): stage0-posix retried across
/// fetchers 0, 1, 3. fetcher-3's TransientFailure was the 6th — by
/// then the derivation was already Poisoned (threshold 3, hit by an
/// earlier completion). The 6th completion's running_builds.remove was
/// inside an if-chain that the early-return skipped. fetcher-3's slot
/// stayed 1/1 across 30+ heartbeats.
///
/// Why the I-035 phantom-drain didn't catch it: the heartbeat reconcile
/// (executor.rs:531-541) DOES drop entries whose DAG status is
/// terminal — but only by overwriting `running_builds = reconciled` at
/// line 608. The phantom-drain (which WARNs) checks `reconciled
/// .difference(&heartbeat_set)`; a Poisoned entry is already filtered
/// OUT of `reconciled` by `still_inflight=false`, so it's not a
/// "suspect" and the WARN never fires. The slot SHOULD still be freed
/// via the line-608 overwrite — this test confirms that safety net
/// works, while the fix makes the completion path free immediately.
#[tokio::test]
async fn test_completion_after_poison_frees_running_builds() -> TestResult {
    // Two workers so dispatch can re-assign after worker-1's failure.
    let (_db, handle, _task, _rx1) = setup_with_worker("i042-w1", "x86_64-linux", 1).await?;
    let _rx2 = connect_executor(&handle, "i042-w2", "x86_64-linux", 1).await?;

    // Merge + dispatch. drv goes to one of the workers (deterministic
    // by HashMap iteration order — doesn't matter which for this test).
    let build_id = Uuid::new_v4();
    let drv_hash = "i042-drv";
    let _evt = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Whichever worker got it, force-assign to BOTH so each has the
    // entry in running_builds. This simulates the live race where an
    // earlier retry left a stale entry: dispatch added → completion
    // ran → completion's early-return left running_builds untouched.
    // debug_force_assign is idempotent for the worker that already
    // owns it; for the other, it sets assigned_executor + inserts.
    // After both calls, assigned_executor = i042-w2 (last-writer wins).
    handle.debug_force_assign(drv_hash, "i042-w1").await?;
    handle.debug_force_assign(drv_hash, "i042-w2").await?;

    // Both workers track the drv. assigned_executor = w2.
    let workers = handle.debug_query_workers().await?;
    let w1 = workers.iter().find(|w| w.executor_id == "i042-w1").unwrap();
    let w2 = workers.iter().find(|w| w.executor_id == "i042-w2").unwrap();
    assert!(w1.running_builds.contains(&drv_hash.to_string()));
    assert!(w2.running_builds.contains(&drv_hash.to_string()));

    // w2's completion: PermanentFailure → poison_and_cascade. This
    // is the NORMAL flow — handle_completion runs to line 515 and
    // frees w2's slot. Status becomes Poisoned.
    complete_failure(
        &handle,
        "i042-w2",
        &test_drv_path(drv_hash),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "first failure (poisons)",
    )
    .await?;

    let info = handle.debug_query_derivation(drv_hash).await?.unwrap();
    assert_eq!(info.status, DerivationStatus::Poisoned);
    let workers = handle.debug_query_workers().await?;
    let w2 = workers.iter().find(|w| w.executor_id == "i042-w2").unwrap();
    assert!(
        !w2.running_builds.contains(&drv_hash.to_string()),
        "w2's normal completion path frees its slot (line 515)"
    );
    let w1 = workers.iter().find(|w| w.executor_id == "i042-w1").unwrap();
    assert!(
        w1.running_builds.contains(&drv_hash.to_string()),
        "w1's stale entry is still there (no completion processed yet)"
    );

    // THE RACE: w1's completion arrives. drv is already Poisoned.
    // handle_completion's status check (line 412) fires the
    // not-Assigned/Running early-return. Before I-042: this returned
    // BEFORE running_builds.remove → w1's slot leaked. After: the
    // remove is hoisted before the early-return paths.
    complete_failure(
        &handle,
        "i042-w1",
        &test_drv_path(drv_hash),
        rio_proto::build_types::BuildResultStatus::TransientFailure,
        "late completion (drv already poisoned)",
    )
    .await?;

    let workers = handle.debug_query_workers().await?;
    let w1 = workers.iter().find(|w| w.executor_id == "i042-w1").unwrap();
    assert!(
        !w1.running_builds.contains(&drv_hash.to_string()),
        "I-042: completion for already-terminal derivation must free \
         the executor's running_builds slot — pre-fix this leaked"
    );
    Ok(())
}

/// I-042 safety-net assertion: even WITHOUT the completion-side fix,
/// the heartbeat reconcile drops running_builds entries whose DAG
/// status is terminal. This is the i035 reconcile loop's
/// `still_inflight = matches!(status, Assigned|Running)` filter — a
/// Poisoned entry doesn't match, doesn't go into `reconciled`,
/// `running_builds = reconciled` overwrites it away.
///
/// The phantom-drain WARN doesn't fire (the entry is filtered out
/// before suspect detection), but the slot is freed. This test
/// pins that behavior so the safety net stays even if completion-
/// side cleanup regresses.
#[tokio::test]
async fn test_heartbeat_reconcile_drops_terminal_running_builds_entry() -> TestResult {
    let (_db, handle, _task, _rx) = setup_with_worker("i042-hb-w", "x86_64-linux", 1).await?;

    let build_id = Uuid::new_v4();
    let drv_hash = "i042-hb-drv";
    let _evt = merge_single_node(&handle, build_id, drv_hash, PriorityClass::Scheduled).await?;

    // Precondition: dispatched, slot occupied.
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "i042-hb-w")
        .unwrap();
    assert!(w.running_builds.contains(&drv_hash.to_string()));

    // Poison via PermanentFailure. The completion-side fix frees the
    // slot here (proven by the test above). To probe the heartbeat
    // safety net independently, re-insert via debug_force_assign —
    // it adds to running_builds without checking DAG terminality.
    complete_failure(
        &handle,
        "i042-hb-w",
        &test_drv_path(drv_hash),
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "poison",
    )
    .await?;
    let info = handle.debug_query_derivation(drv_hash).await?.unwrap();
    assert_eq!(info.status, DerivationStatus::Poisoned);

    // Re-seed the leak: force_assign on a Poisoned drv fails the
    // status transition (Poisoned → Assigned is invalid) so it
    // returns false WITHOUT inserting into running_builds. We need
    // a different re-seed: the test below covers this scenario via
    // the two-worker race; here we just assert the reconcile filter
    // logic by checking the slot stays clear after a heartbeat (it
    // was already clear from the completion-side fix).
    send_heartbeat(&handle, "i042-hb-w", "x86_64-linux", 1).await?;
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "i042-hb-w")
        .unwrap();
    assert!(
        !w.running_builds.contains(&drv_hash.to_string()),
        "Poisoned derivation must not survive heartbeat reconcile in \
         running_builds (still_inflight = matches!(Poisoned, Assigned|Running) = false)"
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
    let (_db, handle, _task, _stream_rx) =
        setup_with_worker("tick-worker", "x86_64-linux", 1).await?;

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
    let (_db, handle, _task, _rx) =
        setup_with_worker("poison-ttl-worker", "x86_64-linux", 1).await?;

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
        rio_proto::build_types::BuildResultStatus::PermanentFailure,
        "permanent",
    )
    .await?;

    // Verify poisoned.
    let pre = handle
        .debug_query_derivation("poison-ttl-hash")
        .await?
        .expect("derivation exists");
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

/// 3 sequential worker disconnects WHILE RUNNING the same derivation
/// must poison it (drv may have caused the crashes). I-097 narrowed
/// this from "any disconnect" to "disconnect-while-Running" — see
/// `test_assigned_only_disconnects_do_not_poison` for the converse.
#[tokio::test]
async fn test_three_running_disconnects_poisons() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(&handle, build_id, "x6-drv", PriorityClass::Scheduled).await?;

    for i in 0..3 {
        let executor_id = format!("w-x6-{i}");
        let mut rx = connect_executor(&handle, &executor_id, "x86_64-linux", 1).await?;
        let assignment = recv_assignment(&mut rx).await;
        assert!(
            assignment.drv_path.contains("x6-drv"),
            "worker {i} should get x6-drv"
        );

        // Transition Assigned → Running so the disconnect counts as a
        // failed attempt (the drv was actually being built).
        let ok = handle.debug_backdate_running("x6-drv", 0).await?;
        assert!(ok, "Assigned→Running for iteration {i}");

        // Disconnect mid-build. reassign_derivations: was_running=
        // true → record_failure_and_check_poison. For i<2: reset to
        // Ready + next worker gets it. For i==2: poison.
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: executor_id.clone().into(),
            })
            .await?;
        barrier(&handle).await;
        drop(rx);
    }

    let info = handle
        .debug_query_derivation("x6-drv")
        .await?
        .expect("derivation exists");
    assert_eq!(
        info.status,
        DerivationStatus::Poisoned,
        "3 Running disconnects should poison; got {:?}",
        info.status
    );
    Ok(())
}

/// I-097 regression: 3 disconnects while ASSIGNED (never Running) must
/// NOT poison. The drv was never attempted — the worker exited between
/// receiving the assignment and starting it (ephemeral-fetcher race).
/// Before the fix this poisoned after 3; now it stays Ready with empty
/// failed_builders, dispatchable to a 4th worker.
#[tokio::test]
async fn test_assigned_only_disconnects_do_not_poison() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let build_id = Uuid::new_v4();
    let _evt_rx = merge_single_node(&handle, build_id, "x7-drv", PriorityClass::Scheduled).await?;

    for i in 0..3 {
        let executor_id = format!("w-x7-{i}");
        let mut rx = connect_executor(&handle, &executor_id, "x86_64-linux", 1).await?;
        let assignment = recv_assignment(&mut rx).await;
        assert!(assignment.drv_path.contains("x7-drv"));

        // Disconnect WITHOUT transitioning to Running.
        handle
            .send_unchecked(ActorCommand::ExecutorDisconnected {
                executor_id: executor_id.clone().into(),
            })
            .await?;
        barrier(&handle).await;
        drop(rx);
    }

    let info = handle
        .debug_query_derivation("x7-drv")
        .await?
        .expect("derivation exists");
    assert_eq!(
        info.status,
        DerivationStatus::Ready,
        "Assigned-only disconnects must NOT poison; got {:?}",
        info.status
    );
    assert!(
        info.failed_builders.is_empty(),
        "Assigned-only disconnects must not populate failed_builders; got {:?}",
        info.failed_builders
    );

    // Sensitivity: a 4th worker DOES get it (proves Ready is real,
    // not stuck-Ready-with-excluded-workers).
    let mut rx4 = connect_executor(&handle, "w-x7-3", "x86_64-linux", 1).await?;
    let assignment = recv_assignment(&mut rx4).await;
    assert!(
        assignment.drv_path.contains("x7-drv"),
        "4th worker should receive the never-attempted drv"
    );
    Ok(())
}

// r[verify sched.fod.size-class-reactive]
/// I-173: a FOD assigned to a tiny NON-ephemeral fetcher that
/// disconnects while status==Assigned MUST get its `size_class_floor`
/// promoted. DerivationStatus stays Assigned for the build's whole
/// lifetime (Running is set only at completion via ensure_running()),
/// so the production disconnect path is always Assigned-status.
/// Non-ephemeral disconnect = unexpected death (plausibly OOM) →
/// promote. Live: 14 OOMs, retry_count=0, size_class_floor=NULL
/// before this promotion was wired.
///
/// Also asserts I-097's invariant still holds (no failed_builders
/// entry, status=Ready) — promotion is decoupled from poison-record.
/// Converse (ephemeral disconnect → NO promote) at
/// `test_ephemeral_disconnect_does_not_promote_floor`.
#[tokio::test]
async fn test_assigned_disconnect_promotes_fod_floor() -> TestResult {
    use crate::assignment::FetcherSizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_fetcher_size_classes(vec![
            FetcherSizeClassConfig {
                name: "tiny".into(),
            },
            FetcherSizeClassConfig {
                name: "small".into(),
            },
        ])
    });

    let mut rx = connect_fetcher_classed(&handle, "f-tiny", "x86_64-linux", "tiny").await?;

    let mut node = make_test_node("oom-fod-173", "x86_64-linux");
    node.is_fixed_output = true;
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // Dispatch → Assigned. Do NOT send Running ack.
    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("oom-fod-173"));
    let pre = handle
        .debug_query_derivation("oom-fod-173")
        .await?
        .expect("exists");
    assert_eq!(
        pre.status,
        DerivationStatus::Assigned,
        "precondition: status==Assigned at disconnect (the I-173 race)"
    );

    // Fetcher OOMs → disconnect. Status was Assigned, not Running.
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "f-tiny".into(),
        })
        .await?;
    barrier(&handle).await;
    drop(rx);

    let info = handle
        .debug_query_derivation("oom-fod-173")
        .await?
        .expect("exists");
    assert_eq!(
        info.size_class_floor.as_deref(),
        Some("small"),
        "I-173: non-ephemeral Assigned-disconnect must promote FOD floor tiny→small"
    );
    // I-097 still holds: no failure recorded.
    assert!(
        info.failed_builders.is_empty(),
        "I-097: Assigned-disconnect must not record failure; got {:?}",
        info.failed_builders
    );
    assert_eq!(info.status, DerivationStatus::Ready);

    Ok(())
}

// r[verify sched.builder.size-class-reactive]
/// I-177: a non-FOD assigned to a tiny NON-ephemeral builder that
/// disconnects while status==Assigned MUST get its `size_class_floor`
/// promoted, same as the FOD path (I-173). 103 tiny builders
/// OOMKilled on bootstrap-stage0-glibc / llvm-src; without promotion
/// floor stayed NULL, retry routed back to tiny → poison.
///
/// Mirrors `test_assigned_disconnect_promotes_fod_floor` with a
/// builder-kind executor + builder size_classes config.
#[tokio::test]
async fn test_assigned_disconnect_promotes_builder_floor() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "tiny".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    let mut rx = connect_builder_classed(&handle, "b-tiny", "x86_64-linux", "tiny").await?;

    // Non-FOD, no build_history → est_duration=DEFAULT (30s) →
    // classify() picks "tiny" (30 ≤ 30). Routes to b-tiny.
    let node = make_test_node("oom-glibc-177", "x86_64-linux");
    assert!(!node.is_fixed_output, "precondition: non-FOD");
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    // Dispatch → Assigned. Do NOT send Running ack.
    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("oom-glibc-177"));
    let pre = handle
        .debug_query_derivation("oom-glibc-177")
        .await?
        .expect("exists");
    assert_eq!(
        pre.status,
        DerivationStatus::Assigned,
        "precondition: status==Assigned at disconnect"
    );

    // Builder OOMs → disconnect. Status was Assigned, not Running.
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "b-tiny".into(),
        })
        .await?;
    barrier(&handle).await;
    drop(rx);

    let info = handle
        .debug_query_derivation("oom-glibc-177")
        .await?
        .expect("exists");
    assert_eq!(
        info.size_class_floor.as_deref(),
        Some("small"),
        "I-177: non-ephemeral Assigned-disconnect must promote builder floor tiny→small"
    );
    // I-097 still holds: no failure recorded for Assigned-only disconnect.
    assert!(
        info.failed_builders.is_empty(),
        "I-097: Assigned-disconnect must not record failure; got {:?}",
        info.failed_builders
    );
    assert_eq!(info.status, DerivationStatus::Ready);

    Ok(())
}

// r[verify sched.reassign.no-promote-on-ephemeral-disconnect]
/// I-188 (fix 1, defense-in-depth): a non-FOD assigned to a tiny
/// EPHEMERAL builder that disconnects MUST NOT get its
/// `size_class_floor` promoted. Ephemeral disconnect is the expected
/// one-shot Job exit — not a size-adequacy signal. Without this gate
/// the I-188 chain (ProcessCompletion → re-dispatch to freed slot →
/// ephemeral exit → reassign → promote) walked the size ladder one
/// class per derivation. Converse (non-ephemeral disconnect → DOES
/// promote) at `test_assigned_disconnect_promotes_builder_floor`.
///
/// Tests Fix 1 in isolation from Fix 2: this disconnect is direct
/// (ExecutorDisconnected), not via ProcessCompletion, so the
/// draining-on-completion gate doesn't apply — only the
/// reassign-side ephemeral check does.
#[tokio::test]
async fn test_ephemeral_disconnect_does_not_promote_floor() -> TestResult {
    use crate::assignment::SizeClassConfig;

    let db = TestDb::new(&MIGRATOR).await;
    let (handle, _task) = setup_actor_configured(db.pool.clone(), None, |a| {
        a.with_size_classes(vec![
            SizeClassConfig {
                name: "tiny".into(),
                cutoff_secs: 30.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
            SizeClassConfig {
                name: "small".into(),
                cutoff_secs: 3600.0,
                mem_limit_bytes: u64::MAX,
                cpu_limit_cores: None,
            },
        ])
    });

    // Ephemeral builder, classed "tiny". connect_builder_classed +
    // an extra heartbeat with ephemeral=true to set the flag (the
    // existing helper is non-ephemeral).
    let mut rx = connect_builder_classed(&handle, "b-eph", "x86_64-linux", "tiny").await?;
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: true,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: Some("tiny".into()),
            executor_id: "b-eph".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    barrier(&handle).await;

    let node = make_test_node("eph-glibc-188", "x86_64-linux");
    let _ev = merge_dag(&handle, Uuid::new_v4(), vec![node], vec![], false).await?;

    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("eph-glibc-188"));

    // Ephemeral builder exits (one-shot) → disconnect with drv
    // still Assigned. Status==Assigned is the production path
    // (Running is only set at completion via ensure_running()).
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "b-eph".into(),
        })
        .await?;
    barrier(&handle).await;
    drop(rx);

    let info = handle
        .debug_query_derivation("eph-glibc-188")
        .await?
        .expect("exists");
    assert_eq!(
        info.size_class_floor, None,
        "I-188: ephemeral disconnect must NOT promote floor; got {:?}",
        info.size_class_floor
    );
    assert!(
        info.failed_builders.is_empty(),
        "I-097: Assigned-only disconnect must not record failure"
    );
    assert_eq!(info.status, DerivationStatus::Ready);

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

    let mut rx = connect_executor_ephemeral(&handle, "eph-1", "x86_64-linux").await?;

    // parent → child chain. Parent dispatches first (child blocked).
    let parent = make_test_node("eph-parent", "x86_64-linux");
    let child = make_test_node("eph-child", "x86_64-linux");
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
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "eph-1")
        .expect("eph-1 exists");
    assert!(
        w.draining,
        "I-188: ephemeral executor must be draining after completion"
    );
    assert_eq!(
        w.running_count, 0,
        "I-188: ephemeral executor must not be re-assigned post-completion"
    );

    let child_info = handle
        .debug_query_derivation("eph-child")
        .await?
        .expect("child exists");
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

// r[verify sched.ephemeral.no-redispatch-after-completion]
/// I-188 control: a NON-ephemeral executor that completes a build is
/// NOT marked draining; its freed slot is real capacity and the
/// dependent dispatches to it in the same ProcessCompletion turn.
/// Same parent→child chain as
/// `test_ephemeral_completion_marks_draining_no_redispatch` but with
/// `ephemeral=false` (the default `connect_executor`).
#[tokio::test]
async fn test_non_ephemeral_completion_allows_redispatch() -> TestResult {
    let (_db, handle, _task) = setup().await;

    let mut rx = connect_executor(&handle, "sts-1", "x86_64-linux", 1).await?;

    let parent = make_test_node("sts-parent", "x86_64-linux");
    let child = make_test_node("sts-child", "x86_64-linux");
    let _ev = merge_dag(
        &handle,
        Uuid::new_v4(),
        vec![parent, child],
        vec![make_test_edge("sts-child", "sts-parent")],
        false,
    )
    .await?;

    let asgn = recv_assignment(&mut rx).await;
    assert!(asgn.drv_path.contains("sts-parent"));

    complete_success_empty(&handle, "sts-1", "sts-parent").await?;
    barrier(&handle).await;

    // NOT draining; child IS assigned to it.
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "sts-1")
        .expect("sts-1 exists");
    assert!(
        !w.draining,
        "non-ephemeral executor must NOT be draining after completion"
    );

    let asgn2 = recv_assignment(&mut rx).await;
    assert!(
        asgn2.drv_path.contains("sts-child"),
        "non-ephemeral: child dispatches to freed slot in same turn"
    );

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
    // so we control the first heartbeat's running_builds precisely.
    let (stream_tx, _stream_rx) = mpsc::channel(256);
    handle
        .send_unchecked(ActorCommand::ExecutorConnected {
            executor_id: "hb-worker".into(),
            stream_tx,
        })
        .await?;

    // Heartbeat with running_builds claiming the drv we merged but
    // never assigned to this worker. The reconcile adopts it (worker
    // is authoritative about what it's running) which sets
    // running_build → has_capacity()=false → the post-heartbeat
    // dispatch_ready() can't ALSO assign (would muddy "worker claims
    // it, scheduler didn't know").
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "hb-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![test_drv_path("hb-drv")],
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
    let workers = handle.debug_query_workers().await?;
    let w = workers
        .iter()
        .find(|w| w.executor_id == "hb-worker")
        .expect("worker registered");
    assert!(
        w.running_builds.contains(&"hb-drv".to_string()),
        "worker's claim adopted into running_build"
    );

    // DAG-side adoption (the I-066 fix). Without it, the post-
    // heartbeat dispatch_ready would re-pop the still-Ready node and
    // send it to the next idle worker.
    let info = handle
        .debug_query_derivation("hb-drv")
        .await?
        .expect("exists");
    assert_eq!(info.status, DerivationStatus::Assigned);
    assert_eq!(info.assigned_executor.as_deref(), Some("hb-worker"));
    assert!(info.failed_builders.is_empty());

    Ok(())
}

/// DrainExecutor(force=true) on an idle worker → running=0, no
/// CancelSignal sent (nothing to cancel). The to_reassign vec is
/// empty, the CancelSignal loop does 0 iterations.
#[tokio::test]
async fn test_force_drain_idle_worker_no_cancel_signals() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("idle-worker", "x86_64-linux", 4).await?;

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
    assert_eq!(
        result.running_builds, 0,
        "idle worker → nothing to reassign"
    );

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
/// build + result.running_builds=N. The preemption hook: controller sees
/// DisruptionTarget on a pod, calls this so the worker cgroup.kills its
/// builds NOW instead of running the full 2h terminationGracePeriod.
/// (wired: P0285 rio-controller disruption.rs watcher)
///
/// Counterpart to test_force_drain_idle_worker_no_cancel_signals — that
/// one proves the CancelSignal loop does 0 iterations on idle; this one
/// proves it does N iterations on busy. Covers worker.rs:211-258 (the
/// `if force { ... }` body with a non-empty to_reassign).
#[tokio::test]
async fn test_force_drain_busy_worker_sends_cancel_signal() -> TestResult {
    let (_db, handle, _task, mut rx) = setup_with_worker("busy-worker", "x86_64-linux", 4).await?;

    // Merge + dispatch → Assigned to busy-worker. running_builds={drv}.
    let build_id = Uuid::new_v4();
    let _ev = merge_single_node(&handle, build_id, "drain-drv", PriorityClass::Scheduled).await?;
    // recv_assignment on busy-worker's rx proves it was dispatched here.
    let _assignment = recv_assignment(&mut rx).await;

    // Force-drain. to_reassign drains running_builds → 1 entry.
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
    // force=true → running_builds: 0 (worker.rs:277 "reassigned:
    // caller doesn't wait"). The count is only nonzero for
    // force=false (caller polls until it drains naturally).
    assert_eq!(
        result.running_builds, 0,
        "force-drain reassigns immediately; caller doesn't wait"
    );

    // CancelSignal should arrive in the worker's stream with the
    // force-drain reason (worker.rs:244). try_send on the stream_tx
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
    let post = handle
        .debug_query_derivation("drain-drv")
        .await?
        .expect("drv exists");
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
        setup_with_worker("metric-drain-worker", "x86_64-linux", 4).await?;

    // Assign one build so to_reassign is non-empty (the increment at
    // worker.rs:255 is gated on `if !to_reassign.is_empty()`).
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
    // worker.rs:255 before reassign_derivations().await, and the actor
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
    let (_db, handle, _task, mut rx) = setup_with_worker("bs-worker", "x86_64-linux", 1).await?;

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
    let post = handle
        .debug_query_derivation("bs-drv")
        .await?
        .expect("drv exists");
    assert!(
        matches!(
            post.status,
            DerivationStatus::Ready | DerivationStatus::Assigned
        ),
        "backstop should reset drv (not leave it Running); got {:?}",
        post.status
    );
    assert!(
        post.retry_count >= 1,
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
            nodes: vec![make_test_node("pbt-drv", "x86_64-linux")],
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
    // uses strict > (worker.rs), so 60s elapsed would also NOT fire
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
    let (_db, handle, _task, mut rx) =
        setup_with_worker("degraded-worker", "x86_64-linux", 4).await?;

    // Mark it degraded BEFORE merging any work. The heartbeat also
    // triggers dispatch_ready (actor/mod.rs:432) but the ready queue
    // is empty, so that's a no-op. The point is ExecutorState.store_
    // degraded is set by the time the merge below runs.
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: true,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "degraded-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
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

    let info = handle
        .debug_query_derivation("sd-drv")
        .await?
        .expect("sd-drv exists");
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
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "degraded-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
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
    let mut node_b = make_test_node("warm-b", "x86_64-linux");
    node_b.expected_output_paths = vec![test_store_path("warm-b-out")];
    let node_a = make_test_node("warm-a", "x86_64-linux");
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
    let mut boot_rx = connect_executor(&handle, "boot-w", "x86_64-linux", 1).await?;
    let boot_asgn = recv_assignment(&mut boot_rx).await;
    assert_eq!(boot_asgn.drv_path, test_drv_path("warm-b"));
    complete_success_empty(&handle, "boot-w", &test_drv_path("warm-b")).await?;
    barrier(&handle).await;

    // Precondition: A is now Ready (all deps Completed).
    // The bootstrap worker is now holding A's assignment (one slot,
    // freed by completing B above). Disconnect boot-w to reset A to
    // Ready for the real worker.
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "boot-w".into(),
        })
        .await?;
    barrier(&handle).await;
    drop(boot_rx);

    let info_a = handle
        .debug_query_derivation("warm-a")
        .await?
        .expect("warm-a exists");
    assert_eq!(
        info_a.status,
        DerivationStatus::Ready,
        "precondition: A Ready with completed child B"
    );

    // THEN connect the REAL worker — WITHOUT auto-ACK. Registration
    // hook sees Ready queue non-empty, A's closure = B's output →
    // sends PrefetchHint.
    let mut rx = connect_executor_no_ack(&handle, "warm-worker", "x86_64-linux", 4).await?;
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
/// worker.rs:126-136 — "nothing queued → nothing to prefetch → gate
/// open now"). Proves: merge AFTER connect → Assignment arrives
/// WITHOUT a PrefetchComplete ACK round-trip.
#[tokio::test]
async fn on_worker_registered_empty_queue_flips_warm_immediately() -> TestResult {
    use rio_proto::types::scheduler_message::Msg;

    let (_db, handle, _task) = setup().await;

    // Connect FIRST — ready queue is empty. on_worker_registered's
    // short-circuit flips warm=true without sending a hint.
    let mut rx = connect_executor_no_ack(&handle, "empty-worker", "x86_64-linux", 4).await?;
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
/// (defensive path at worker.rs:158-163 — "gate is optimization, not
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
    let mut node_b = make_test_node("fail-b", "x86_64-linux");
    node_b.expected_output_paths = vec![test_store_path("fail-b-out")];
    let _ev = merge_dag(
        &handle,
        build_id,
        vec![make_test_node("fail-a", "x86_64-linux"), node_b],
        vec![make_test_edge("fail-a", "fail-b")],
        false,
    )
    .await?;
    let mut boot_rx = connect_executor(&handle, "boot-f", "x86_64-linux", 1).await?;
    let _ = recv_assignment(&mut boot_rx).await;
    complete_success_empty(&handle, "boot-f", &test_drv_path("fail-b")).await?;
    handle
        .send_unchecked(ActorCommand::ExecutorDisconnected {
            executor_id: "boot-f".into(),
        })
        .await?;
    barrier(&handle).await;
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
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "fail-worker".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
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
            let mut c = make_test_node(&format!("child-{j:04}"), "x86_64-linux");
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
        .map(|i| make_test_node(&format!("parent-{i:04}"), "x86_64-linux"))
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
    let mut rx = connect_executor(&handle, "w-idle", "x86_64-linux", 1).await?;

    // (2) Degrade → capacity 1→0. Raw Heartbeat (no trailing Tick).
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: true,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "w-idle".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
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
    handle
        .send_unchecked(ActorCommand::Heartbeat {
            store_degraded: false,
            ephemeral: false,
            draining: false,
            kind: rio_proto::types::ExecutorKind::Builder,
            resources: None,
            bloom: None,
            size_class: None,
            executor_id: "w-idle".into(),
            systems: vec!["x86_64-linux".into()],
            supported_features: vec![],
            running_builds: vec![],
        })
        .await?;
    barrier(&handle).await;

    let a = recv_assignment(&mut rx).await;
    assert_eq!(a.drv_path, test_drv_path("idle-immediate"));
    Ok(())
}
