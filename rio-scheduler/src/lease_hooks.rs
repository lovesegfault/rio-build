//! Scheduler-side [`LeaseHooks`](crate::lease::LeaseHooks) impl: ordered,
//! non-blocking delivery of lease transitions to the DAG actor.
//!
//! The lease loop calls `on_acquire`/`on_lose` synchronously from the
//! renewal tick, so the hooks MUST NOT block — a blocked hook stalls the
//! tick and the loop can neither renew nor self-fence while a standby
//! steals after `STEAL_AFTER` of observed staleness → dual-leader (see
//! `rio_lease::LeaseHooks`). Each hook therefore just enqueues onto an
//! unbounded FIFO handoff queue (a synchronous, never-blocking send) and
//! returns; a single long-lived forwarder task drains that queue into the
//! actor's bounded command channel.
//!
//! Delivery preserves invocation order end-to-end: one FIFO queue, one
//! forwarder, one actor mpsc. That ordering is load-bearing — the
//! tick-time self-fence can fire `on_lose` and the same tick's renew can
//! re-fire `on_acquire` (false alarm), and the actor's same-epoch
//! recovery reasoning assumes the pair arrives in that order; an inverted
//! pair would let a late `LeaderLost` wipe the freshly re-recovered DAG
//! with `is_leader`/`recovery_complete` left true. Per-call
//! `tokio::spawn` (the previous shape) gives no such guarantee.
//!
//! The bounded actor channel remains the only backpressure stage. Lease
//! transitions are control messages, not work submission, so the
//! forwarder uses the hysteresis-bypassing raw sender (the channel half
//! of `ActorHandle::send_unchecked`); the unbounded handoff grows only at
//! the lease-edge rate and is drained immediately. All clones share the
//! same queue and forwarder, so cloning does not fork ordering domains.
//! The forwarder is spawned from `SchedulerLeaseHooks::new`, which main.rs
//! calls inside the tokio runtime.

use tokio::sync::mpsc;
use tracing::error;

use crate::actor::{ActorCommand, ActorHandle};

/// Scheduler-specific lease transition hooks: emit `rio_scheduler_lease_*`
/// metrics and enqueue `LeaderAcquired`/`LeaderLost` for the actor in
/// invocation order.
#[derive(Clone)]
pub struct SchedulerLeaseHooks {
    /// FIFO handoff to the forwarder task. Unbounded so the enqueue is
    /// synchronous and never blocks the renewal tick.
    tx: mpsc::UnboundedSender<ActorCommand>,
}

impl SchedulerLeaseHooks {
    /// Build the hooks for a running actor and spawn the forwarder.
    pub fn new(actor: &ActorHandle) -> Self {
        Self::from_sender(actor.command_sender())
    }

    /// Test seam: build the hooks around a caller-supplied command
    /// sender (no actor needed), so tests can observe exactly what the
    /// forwarder delivers and in which order.
    #[cfg(test)]
    pub(crate) fn with_command_sender(actor_tx: mpsc::Sender<ActorCommand>) -> Self {
        Self::from_sender(actor_tx)
    }

    fn from_sender(actor_tx: mpsc::Sender<ActorCommand>) -> Self {
        let (tx, mut rx) = mpsc::unbounded_channel::<ActorCommand>();
        // r[impl sched.lease.hook-order]
        // Single forwarder task: drains the FIFO handoff into the
        // actor's command channel, preserving invocation order. Break
        // on send error — a closed actor channel is terminal (the
        // process is shutting down or the actor panicked), matching
        // the error arm of the previous per-spawn shape without
        // leaving a wakeup-per-edge zombie loop behind.
        rio_common::task::spawn_monitored("lease-hook-forwarder", async move {
            while let Some(cmd) = rx.recv().await {
                if actor_tx.send(cmd).await.is_err() {
                    error!(
                        "lease-hook forwarder: actor command channel closed; \
                         dropping further lease transitions"
                    );
                    break;
                }
            }
        });
        Self { tx }
    }

    fn enqueue(&self, cmd: ActorCommand, what: &'static str) {
        if self.tx.send(cmd).is_err() {
            // Forwarder gone ⇒ actor gone. We may still hold the lease
            // but cannot dispatch; the process is most likely shutting
            // down or crashing.
            error!("failed to queue {what} (lease-hook forwarder gone)");
        }
    }
}

impl crate::lease::LeaseHooks for SchedulerLeaseHooks {
    fn on_acquire(&self) {
        // Counter for VM test observability: the leader-election VM
        // scenario polls this to confirm the lease loop actually
        // acquired (vs silently failing kube-client init and running
        // standby forever). The info! log has the same signal but
        // metrics are less brittle for VM grep. Also fired on a rebound
        // (a holder change observed late on a still-leading round —
        // `sched.lease.rebound`), so this counter counts rebounds too;
        // deliberate, no separate counter.
        metrics::counter!("rio_scheduler_lease_acquired_total").increment(1);
        self.enqueue(ActorCommand::LeaderAcquired, "LeaderAcquired");
    }

    fn on_lose(&self) {
        metrics::counter!("rio_scheduler_lease_lost_total").increment(1);
        self.enqueue(ActorCommand::LeaderLost, "LeaderLost");
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use crate::lease::LeaseHooks as _;

    // r[verify sched.lease.hook-order]
    /// The false-alarm tick shape: `on_lose` then `on_acquire` invoked
    /// back-to-back from one task must reach the actor channel as
    /// exactly `[LeaderLost, LeaderAcquired]` — invocation order, no
    /// drops, no extras. Deterministic against the ordered forwarder;
    /// the pre-fix per-spawn delivery had no such guarantee (observed
    /// inversions are recorded in the introducing commit message).
    #[tokio::test]
    async fn hook_pair_delivered_in_invocation_order() {
        let (tx, mut rx) = mpsc::channel(16);
        let hooks = SchedulerLeaseHooks::with_command_sender(tx);

        hooks.on_lose();
        hooks.on_acquire();

        let first = rx.recv().await.expect("first hook command delivered");
        let second = rx.recv().await.expect("second hook command delivered");
        assert!(
            matches!(first, ActorCommand::LeaderLost),
            "first delivered command must be LeaderLost (the invocation order)"
        );
        assert!(
            matches!(second, ActorCommand::LeaderAcquired),
            "second delivered command must be LeaderAcquired (the invocation order)"
        );
        assert!(
            rx.try_recv().is_err(),
            "exactly two commands must be delivered for the pair"
        );
    }
}
