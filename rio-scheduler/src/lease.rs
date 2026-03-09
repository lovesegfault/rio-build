//! Kubernetes Lease-based leader election.
//!
//! When `RIO_LEASE_NAME` is set, a background task acquires and
//! renews a `coordination.k8s.io/v1` Lease. On acquire, it
//! increments the generation counter (workers see the new gen in
// r[impl sched.lease.k8s-lease]
// r[impl sched.lease.generation-fence]
//! heartbeat, reject stale-gen assignments from the old leader)
//! and sets `is_leader=true` (dispatch_ready checks this).
//!
//! When NOT set (VM tests, single-scheduler deployments): no kube
//! dependency at runtime, `is_leader` defaults to `true`,
//! generation stays at 1. Zero behavior change for existing
//! deployments.
//!
//! # No fencing — why it's OK
//!
//! `kube-leader-election` is a polling loop, not a watch-based
//! fence. If the network partitions, both replicas may believe
//! they're leader for up to `lease_ttl` (15s). During that window
//! both dispatch. This is ACCEPTABLE because dispatch is
//! idempotent:
//!
//! - DAG merge dedups by `drv_hash`. Two schedulers merging the
//!   same SubmitBuild both end up with the same DAG node.
//! - Workers compare `WorkAssignment.generation` against
//!   `HeartbeatResponse.generation`. After the new leader
//!   increments, the old leader's assignments are stale and
//!   workers reject them.
//! - Worst case: a derivation dispatches twice (one from each
//!   leader), builds twice, produces the same output (deterministic
//!   builds). Wasteful but correct.
//!
//! A proper fenced lease (etcd-style linearizable) would eliminate
//! the dual-leader window entirely. kube-leader-election doesn't
//! offer that. If we need it: replace with a direct etcd client
//! or a raft crate. Not worth the complexity today.
//!
//! # Lease TTL and renew cadence
//!
//! 15s TTL, renewed every 5s. The 3:1 ratio is the kubernetes
//! convention (see kube-controller-manager's defaults). Three
//! renewal attempts before the lease expires — survives one or
//! two transient apiserver hiccups.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::Duration;

use kube_leader_election::{LeaseLock, LeaseLockParams, LeaseLockResult};
use tracing::{info, warn};

/// Lease TTL. After this much time without renewal, another
/// replica can acquire.
const LEASE_TTL: Duration = Duration::from_secs(15);

/// Renewal interval. LEASE_TTL / 3 per K8s convention.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);

/// Lease configuration, read from env at startup.
///
/// `Option` because it's entirely optional — `None` means non-K8s
/// mode (the common case for VM tests and dev). `from_env()`
/// returns `None` unless `RIO_LEASE_NAME` is set.
#[derive(Debug, Clone)]
pub struct LeaseConfig {
    /// The Lease object's `.metadata.name`. Unique per scheduler
    /// deployment — two independent rio-build clusters in the same
    /// K8s namespace would use different names.
    pub lease_name: String,
    /// Namespace for the Lease. Usually the scheduler pod's own
    /// namespace (read from the downward API or the service-account
    /// mount).
    pub namespace: String,
    /// This replica's identity. Usually the pod name (HOSTNAME env,
    /// set by K8s). Written into `Lease.spec.holderIdentity` when
    /// we hold the lock — `kubectl get lease` shows who's leading.
    pub holder_id: String,
}

impl LeaseConfig {
    /// Read from environment. Returns `None` if `RIO_LEASE_NAME`
    /// is unset — the signal for "not running under K8s."
    ///
    /// `RIO_LEASE_NAMESPACE` defaults to reading the service-account
    /// namespace mount (standard K8s downward-API path). If that's
    /// ALSO missing (running locally against a remote cluster),
    /// defaults to "default" — probably wrong, but the operator
    /// will notice when the Lease doesn't appear where expected.
    ///
    /// `HOSTNAME` is set by K8s to the pod name. If missing (non-
    /// K8s with RIO_LEASE_NAME manually set — weird but possible
    /// for testing), falls back to a UUID. Unique, just not
    /// human-readable in `kubectl get lease`.
    pub fn from_env() -> Option<Self> {
        let lease_name = std::env::var("RIO_LEASE_NAME").ok()?;

        let namespace = std::env::var("RIO_LEASE_NAMESPACE").unwrap_or_else(|_| {
            // The standard in-cluster namespace mount. Every pod
            // gets this via the service-account projected volume.
            std::fs::read_to_string("/var/run/secrets/kubernetes.io/serviceaccount/namespace")
                .map(|s| s.trim().to_string())
                .unwrap_or_else(|_| "default".to_string())
        });

        let holder_id =
            std::env::var("HOSTNAME").unwrap_or_else(|_| uuid::Uuid::new_v4().to_string());

        Some(Self {
            lease_name,
            namespace,
            holder_id,
        })
    }
}

/// Shared leader state. The lease task writes; actor + health read.
///
/// Both atomics, both Relaxed on one and Release/Acquire on the
/// other. Why the asymmetry:
///
/// `generation`: Release on write (lease acquire), Acquire on read
/// (dispatch, heartbeat). The generation is a FENCE — when
/// dispatch sees a new generation, it should also see the
/// `is_leader=true` write that happened-before. Acquire/Release
/// pairs give that.
///
/// `is_leader`: Relaxed. It's a standalone flag with no other
/// state to synchronize against. dispatch_ready checks it at the
/// top of each dispatch pass; a one-pass lag on a false→true
/// transition is harmless (next pass dispatches). A true→false
/// lag means we dispatch once more as a lame duck — also fine
/// (idempotency, see module doc).
pub struct LeaderState {
    /// Generation counter. Lease task `fetch_add(1, Release)` on
    /// each acquisition. Same Arc as DagActor.generation (see
    /// `generation_arc()` — this IS that Arc, cloned).
    pub generation: Arc<AtomicU64>,
    /// Whether we currently hold the lease. dispatch_ready early-
    /// returns if false (standby schedulers merge DAGs but don't
    /// dispatch — state warm for fast takeover).
    pub is_leader: Arc<AtomicBool>,
    /// Whether recovery has completed. dispatch_ready gates on
    /// BOTH is_leader AND this. Set by handle_leader_acquired
    /// AFTER recover_from_pg finishes. Cleared by the lease loop
    /// on LOSE transition so re-acquire re-triggers recovery.
    ///
    /// Separate from is_leader because the lease loop sets
    /// is_leader IMMEDIATELY (non-blocking — must keep renewing),
    /// then fire-and-forgets LeaderAcquired. Recovery may take
    /// seconds; dispatch waits.
    pub recovery_complete: Arc<AtomicBool>,
}

impl LeaderState {
    /// Non-K8s mode: leader immediately, generation stays at 1.
    /// This is what VM tests and single-scheduler deployments see.
    ///
    /// recovery_complete=true: no lease acquisition → no recovery
    /// trigger. Empty DAG at startup (same as Phase 3a). Single-
    /// instance deployments don't failover so PG recovery isn't
    /// meaningful.
    pub fn always_leader(generation: Arc<AtomicU64>) -> Self {
        Self {
            generation,
            is_leader: Arc::new(AtomicBool::new(true)),
            recovery_complete: Arc::new(AtomicBool::new(true)),
        }
    }

    /// K8s mode: NOT leader until the lease loop acquires. If the
    /// loop never acquires (another replica holds it), we stay
    /// standby forever — correct.
    ///
    /// recovery_complete=false: acquisition triggers recovery.
    /// dispatch_ready gates on this AND is_leader.
    pub fn pending(generation: Arc<AtomicU64>) -> Self {
        Self {
            generation,
            is_leader: Arc::new(AtomicBool::new(false)),
            recovery_complete: Arc::new(AtomicBool::new(false)),
        }
    }
}

/// The lease loop. Spawn this via `spawn_monitored` in main.rs.
///
/// Never returns (barring panic). On each tick:
/// - `try_acquire_or_renew`: creates the Lease if it doesn't
///   exist, or updates `renewTime` if we hold it, or returns
///   "not leading" if someone else holds it.
/// - On acquire transition (was standby, now leading): increment
///   generation, flip `is_leader`, fire-and-forget
///   `LeaderAcquired` to the actor. The actor's
///   `handle_leader_acquired` runs recovery then sets
///   `recovery_complete=true`. CRITICAL: this loop does NOT
///   block on recovery — it keeps renewing the lease every 5s
///   regardless. A slow recovery (>15s) would otherwise let the
///   lease expire → another replica acquires → dual-leader.
/// - On lose transition (was leading, now not): flip `is_leader`,
///   clear `recovery_complete` (re-acquire re-triggers recovery).
///   DON'T increment generation — the NEW leader does that on
///   THEIR acquire. We don't know the new gen.
///
/// On K8s API error (apiserver restarting, network blip): log
/// warn and retry next tick. Don't crash — a transient API hiccup
/// shouldn't kill the scheduler. If the error persists past
/// `lease_ttl`, our lease expires and another replica takes over,
/// which is exactly the desired behavior for "this replica's K8s
/// connectivity is broken."
///
/// `actor`: handle to fire-and-forget `LeaderAcquired`. Cloned
/// into the task. No reply channel — recovery is the actor's
/// concern, the lease loop only signals the transition.
pub async fn run_lease_loop(
    cfg: LeaseConfig,
    state: LeaderState,
    actor: crate::actor::ActorHandle,
    shutdown: rio_common::signal::Token,
) {
    // kube client from in-cluster config. If this fails (not in
    // a pod, or service account not mounted), log and exit the
    // loop — spawn_monitored logs the task death. The scheduler
    // keeps running with `is_leader=false` → never dispatches →
    // effectively a standby. Not useful but not broken either;
    // the OTHER replica (with working kube access) leads.
    let client = match kube::Client::try_default().await {
        Ok(c) => c,
        Err(e) => {
            warn!(error = %e, "kube client init failed; lease loop exiting (this replica will never lead)");
            return;
        }
    };

    let lease = LeaseLock::new(
        client,
        &cfg.namespace,
        LeaseLockParams {
            holder_id: cfg.holder_id.clone(),
            lease_name: cfg.lease_name.clone(),
            lease_ttl: LEASE_TTL,
        },
    );

    info!(
        lease = %cfg.lease_name,
        namespace = %cfg.namespace,
        holder = %cfg.holder_id,
        ttl_secs = LEASE_TTL.as_secs(),
        "lease loop starting"
    );

    let mut was_leading = false;
    let mut interval = tokio::time::interval(RENEW_INTERVAL);
    // Skip: if one renewal is slow (apiserver busy), don't fire
    // twice immediately. The lease TTL is 15s; we have slack.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    loop {
        tokio::select! {
            _ = shutdown.cancelled() => {
                tracing::debug!("lease loop shutting down");
                break;
            }
            _ = interval.tick() => {}
        }

        match lease.try_acquire_or_renew().await {
            Ok(lease_state) => {
                // LeaseLockResult is an enum: Acquired(Lease) vs
                // NotAcquired. The crate's doc comment says
                // `.acquired_lease` but that's outdated — match
                // the variant.
                let now_leading = matches!(lease_state, LeaseLockResult::Acquired(_));

                if now_leading && !was_leading {
                    // ---- Acquire transition ----
                    // Increment FIRST, then set is_leader. With
                    // Release/Acquire on generation, dispatch
                    // seeing the new gen implies seeing is_leader
                    // (happened-before). The other order would
                    // let dispatch run with is_leader=true but
                    // OLD generation for one pass — harmless
                    // (workers compare heartbeat gen, not
                    // assignment gen, for staleness) but
                    // conceptually wrong.
                    let new_gen = state.generation.fetch_add(1, Ordering::Release) + 1;
                    state.is_leader.store(true, Ordering::Relaxed);
                    info!(
                        generation = new_gen,
                        holder = %cfg.holder_id,
                        "acquired leadership"
                    );
                    // Counter for VM test observability: vm-phase3a's
                    // lease smoke test polls this to confirm the
                    // lease loop actually acquired (vs silently
                    // failing kube-client init and running standby
                    // forever). The info! log has the same signal
                    // but metrics are less brittle for VM grep.
                    metrics::counter!("rio_scheduler_lease_acquired_total").increment(1);

                    // Fire-and-forget LeaderAcquired. The actor
                    // runs recovery then sets recovery_complete.
                    // send_unchecked bypasses backpressure — this
                    // is a control message, not work submission.
                    // Spawn the send so we don't block the lease
                    // loop on actor backpressure (shouldn't happen
                    // — channel capacity is 10k — but defensive).
                    //
                    // NON-BLOCKING IS LOAD-BEARING: if we awaited
                    // a reply from recovery, a slow recovery (>15s
                    // for a large DAG) would stall the renewal
                    // tick → lease expires → another replica
                    // acquires → dual-leader. fire-and-forget +
                    // separate recovery_complete flag lets the
                    // loop keep renewing regardless.
                    let actor_clone = actor.clone();
                    tokio::spawn(async move {
                        if let Err(e) = actor_clone
                            .send_unchecked(crate::actor::ActorCommand::LeaderAcquired)
                            .await
                        {
                            // Actor dead. We're holding the lease
                            // but can't dispatch. Not much to do —
                            // the process is probably crashing.
                            tracing::error!(error = %e,
                                "failed to send LeaderAcquired (actor dead?)");
                        }
                    });
                } else if !now_leading && was_leading {
                    // ---- Lose transition ----
                    // Someone else acquired (we couldn't renew in
                    // time, or `step_down()` was called — we don't
                    // call that, but future graceful shutdown
                    // might). Stop dispatching. Generation is the
                    // NEW leader's job to increment.
                    state.is_leader.store(false, Ordering::Relaxed);
                    // Clear recovery_complete: if we re-acquire,
                    // recovery runs again. The other replica's
                    // actions may have changed PG state.
                    state.recovery_complete.store(false, Ordering::Relaxed);
                    warn!(
                        holder = %cfg.holder_id,
                        "lost leadership (another replica acquired)"
                    );
                    metrics::counter!("rio_scheduler_lease_lost_total").increment(1);
                }
                // else: steady state (still leading, renewed; or
                // still standby, someone else holds). No log —
                // 5s interval would be noisy.

                was_leading = now_leading;
            }
            Err(e) => {
                // K8s API error. Transient — retry next tick. If
                // this persists past LEASE_TTL, our lease expires
                // and another replica acquires. That's CORRECT
                // behavior for "this replica's K8s connectivity
                // is broken."
                //
                // DON'T flip is_leader here. We might still hold
                // the lease (apiserver is down for EVERYONE; no
                // one can acquire). Flipping would stop dispatch
                // even though we're the only viable leader. Let
                // the TTL expiry decide.
                warn!(error = %e, "lease renew failed (transient?); retrying next tick");
            }
        }
    }
}

// r[verify sched.lease.k8s-lease]
// r[verify sched.lease.generation-fence]
#[cfg(test)]
mod tests {
    use super::*;

    /// from_env returns None when RIO_LEASE_NAME unset — the
    /// signal for "non-K8s mode." This is how VM tests stay
    /// unaffected.
    ///
    /// figment Jail restores env on closure exit (parallel tests
    /// won't see each other's set_env). It does NOT clear env on
    /// entry — we explicitly remove_var for the unset case.
    #[test]
    #[allow(clippy::result_large_err)] // figment::Error is 208B, API-fixed
    fn from_env_none_when_unset() -> figment::error::Result<()> {
        figment::Jail::expect_with(|_jail| {
            // SAFETY: Jail serializes env access across tests
            // (it's a global mutex under the hood). No other
            // thread is touching RIO_LEASE_NAME while we're here.
            // std::env::remove_var is unsafe in Rust 2024 because
            // of the general "env is process-global, don't race"
            // rule — Jail's serialization is exactly the
            // synchronization that makes it safe.
            unsafe {
                std::env::remove_var("RIO_LEASE_NAME");
            }
            assert!(
                LeaseConfig::from_env().is_none(),
                "no RIO_LEASE_NAME → None → non-K8s mode"
            );
            Ok(())
        });
        Ok(())
    }

    #[test]
    #[allow(clippy::result_large_err)]
    fn from_env_reads_all_three() -> figment::error::Result<()> {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_LEASE_NAME", "rio-scheduler-leader");
            jail.set_env("RIO_LEASE_NAMESPACE", "rio-prod");
            jail.set_env("HOSTNAME", "rio-scheduler-0");

            let cfg = LeaseConfig::from_env().expect("all vars set → Some");
            assert_eq!(cfg.lease_name, "rio-scheduler-leader");
            assert_eq!(cfg.namespace, "rio-prod");
            assert_eq!(cfg.holder_id, "rio-scheduler-0");
            Ok(())
        });
        Ok(())
    }

    // HOSTNAME fallback to UUID is a one-liner
    // (`.unwrap_or_else(|| Uuid::new_v4())`). Not worth a test —
    // the UUID crate tests itself, and Jail doesn't clear env on
    // entry so we can't easily simulate "HOSTNAME unset" without
    // more unsafe remove_var.

    #[test]
    fn leader_state_always_leader() {
        let gen_arc = Arc::new(AtomicU64::new(1));
        let state = LeaderState::always_leader(gen_arc.clone());
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "non-K8s mode: immediately leader"
        );
        assert_eq!(state.generation.load(Ordering::Acquire), 1);
        // Same Arc: writes through gen_arc are visible through state.
        gen_arc.fetch_add(1, Ordering::Release);
        assert_eq!(state.generation.load(Ordering::Acquire), 2);
    }

    #[test]
    fn leader_state_pending_starts_false() {
        let gen_arc = Arc::new(AtomicU64::new(1));
        let state = LeaderState::pending(gen_arc);
        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "K8s mode: NOT leader until lease loop acquires"
        );
    }
}
