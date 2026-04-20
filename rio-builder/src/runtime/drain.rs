//! Drain-on-SIGTERM machinery: the top-of-reconnect gate that flips
//! `draining` and the exit-time `DrainExecutor` goodbye RPC.

use std::sync::Arc;
use std::sync::atomic::AtomicBool;

use tokio::sync::Notify;
use tracing::info;

use super::slot::BuildSlot;

/// Park until the slot is idle AND no `Msg::Completion` is buffered in
/// the permanent sink / relay. The drain-done and build-done watchers
/// use this (NOT bare `wait_idle()`) before signalling exit.
///
/// `completion_pending` is armed at the START of `executor_future`
/// (before any await/panic point — bug_012) and again by
/// `send_completion`, and cleared by `relay_loop` only after a
/// successful `grpc_tx.send()` into a confirmed-open stream. So
/// slot-idle ⇒ `completion_pending` reflects whether a completion is
/// owed-but-not-yet-flushed, on BOTH the normal path (bug_472:
/// `_slot_guard` drops AFTER `send_completion`) and the panic path
/// (bug_012: `_slot_guard` drops during unwind BEFORE the
/// panic-catcher's late `send_completion`). This returns iff the
/// report has reached tonic's body buffer.
///
/// Same `enable()`-before-check pattern as `BuildSlot::wait_idle` —
/// see tokio::sync::Notify docs §"Avoiding missed notifications".
pub(super) async fn wait_build_flushed(
    slot: &BuildSlot,
    completion_pending: &AtomicBool,
    completion_cleared: &Notify,
) {
    slot.wait_idle().await;
    loop {
        let notified = completion_cleared.notified();
        tokio::pin!(notified);
        notified.as_mut().enable();
        if !completion_pending.load(std::sync::atomic::Ordering::Acquire) {
            return;
        }
        notified.await;
    }
}

/// Top-of-`'reconnect` drain handling. Called each iteration BEFORE
/// the fresh `ExecutorRegister` send.
///
/// I-063 drain transition: on the FIRST call after SIGTERM (`swap` is
/// the test-and-set), set `draining=true` and spawn the idle-watcher
/// → `drain_done` notifier. The reconnect loop then KEEPS RUNNING —
/// completions for an in-flight build reach the current leader (even
/// across scheduler restart) via the same relay machinery as steady-
/// state. Exit when `drain_done` fires (in_flight=0 AND completion
/// flushed).
///
/// I-195 idle fast-path: returns `Break` when `draining` AND the slot
/// is idle AND no completion is pending in the sink. The reconnect-
/// under-drain machinery exists so an in-flight build's
/// CompletionReport reaches the leader; an idle slot WITH NO PENDING
/// COMPLETION has nothing to report. bug_472: previously gated on
/// `!slot.is_busy()` alone — but `_slot_guard` drops AFTER
/// `send_completion` queues into `sink_rx`, so a build that finished
/// during a stream-retry sleep (relay parked, target=None) would
/// Break here with its report still in-process and `run_teardown`
/// dropped it. With `completion_pending` in the gate, that case
/// returns `Continue` → loop reconnects → swap-after-Ok → relay
/// flushes → pending=false → next iteration Breaks.
///
/// Re-registering would bump the scheduler's `workers_active`, and
/// the heartbeat task (aborted only AFTER the loop exits) keeps
/// `last_heartbeat` fresh until process exit. Under coverage
/// instrumentation the profraw atexit write delays exit by ~80s (GHA
/// 24018216226) — `cov_factor` in `lifecycle.nix` band-aided that;
/// this is the structural fix.
// r[impl builder.shutdown.idle-no-reregister+2]
pub(super) fn reconnect_drain_gate(
    shutdown: &rio_common::signal::Token,
    draining: &AtomicBool,
    slot: &Arc<BuildSlot>,
    completion_pending: &Arc<AtomicBool>,
    completion_cleared: &Arc<Notify>,
    drain_done: &Arc<Notify>,
) -> std::ops::ControlFlow<()> {
    use std::sync::atomic::Ordering::{Acquire, Relaxed};
    if shutdown.is_cancelled() && !draining.swap(true, Relaxed) {
        info!(
            in_flight = u8::from(slot.is_busy()),
            "shutdown signal received, draining \
             (stream stays connected for completion reports)"
        );
        // Watcher: spawned (not awaited): the reconnect loop keeps the
        // stream alive; the select arms pick up the notification.
        // Spawned even when idle (the Break below skips the select):
        // wait_build_flushed returns immediately, notify_one stores
        // one permit that nothing consumes — harmless, and keeps the
        // busy/idle paths uniform for the watcher's lifecycle.
        let watch_slot = Arc::clone(slot);
        let pending = Arc::clone(completion_pending);
        let cleared = Arc::clone(completion_cleared);
        let done = Arc::clone(drain_done);
        tokio::spawn(async move {
            wait_build_flushed(&watch_slot, &pending, &cleared).await;
            done.notify_one();
        });
    }
    if draining.load(Relaxed) && !slot.is_busy() && !completion_pending.load(Acquire) {
        info!("draining with idle slot; exiting reconnect loop without re-register");
        return std::ops::ControlFlow::Break(());
    }
    std::ops::ControlFlow::Continue(())
}

/// Exit-time deregister. The wait-for-in-flight is already done by
/// the drain watcher (or the build's permit return) before we get
/// here — this just sends DrainExecutor as an explicit goodbye.
///
/// K8s preStop sequence is now:
///   1. SIGTERM → `draining` flag set, watcher spawned
///   2. Heartbeat reports `draining=true` (worker is authority)
///   3. Reconnect loop KEEPS the stream alive — completions reach
///      whichever scheduler is leader, even across scheduler restart
///   4. In-flight=0 → drain_done fires → loop exits → here
///   5. DrainExecutor (best-effort, redundant with heartbeat)
///   6. Exit 0
///
/// terminationGracePeriodSeconds=7200 (2h). If we exceed that,
/// SIGKILL — builds lost. 2h is enough for ~any single build.
pub(super) async fn run_drain(scheduler_addr: &str, executor_id: &str) {
    use rio_common::backoff::{self, Backoff, Jitter, RetryError};

    // I-091: scheduler_addr is the k8s Service — kube-proxy picks a
    // replica per TCP connection, so ~50% land on the standby, which
    // rejects with Unavailable("not leader"). One retry on a FRESH
    // channel (tonic reuses the HTTP/2 conn, so retrying on the same
    // client would hit the same pod) gives kube-proxy another roll.
    // Still best-effort: two standby picks in a row falls through to
    // the warn path; heartbeat already reported draining either way.
    const BACKOFF: Backoff = Backoff {
        base: std::time::Duration::ZERO,
        mult: 1.0,
        cap: std::time::Duration::ZERO,
        jitter: Jitter::None,
    };
    let result = backoff::retry(
        &BACKOFF,
        2,
        &rio_common::signal::Token::new(),
        |s: &tonic::Status| s.code() == tonic::Code::Unavailable,
        |_, e| tracing::debug!(error = %e, "DrainExecutor hit standby; reconnecting once"),
        async || {
            // Fresh channel each attempt — see I-091 above.
            let mut admin = rio_proto::client::connect_single::<rio_proto::AdminServiceClient<_>>(
                scheduler_addr,
            )
            .await
            .map_err(|e| tonic::Status::unavailable(format!("admin connect: {e}")))?;
            admin
                .drain_executor(rio_proto::types::DrainExecutorRequest {
                    executor_id: executor_id.to_string(),
                    force: false,
                })
                .await
        },
    )
    .await;
    match result {
        Ok(resp) => {
            let r = resp.into_inner();
            info!(
                accepted = r.accepted,
                busy = r.busy,
                "drain acknowledged by scheduler"
            );
        }
        Err(RetryError::Exhausted { last, .. }) => {
            tracing::warn!(error = %last, "DrainExecutor RPC failed; heartbeat already reported draining");
        }
        Err(RetryError::Cancelled) => unreachable!("never-cancelled token"),
    }
    info!("drain complete, exiting");
}
