//! Kubernetes Lease-based leader election.
//!
//! When `lease_name` is configured, a background task acquires and
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
//! # Election mechanics (see `election.rs`)
//!
//! All lease mutations go through `kube::Api::replace()` (PUT),
//! which requires the GET's `metadata.resourceVersion` — the
//! apiserver rejects with 409 if the object changed. Two racing
//! writers: exactly one wins. On top of that, standbys use a
//! local-monotonic "observed record" clock to decide staleness
//! (immune to cross-node clock skew — we never compare against
//! the lease's `renewTime`).
//!
//! This STILL isn't a linearizable fence — a partitioned leader
//! can keep dispatching until its TTL runs out locally. That's
//! acceptable because dispatch is idempotent:
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
//! # Lease TTL and renew cadence
//!
//! 15s TTL, renewed every 5s. The 3:1 ratio is the kubernetes
//! convention (see kube-controller-manager's defaults). Three
//! renewal attempts before the lease expires — survives one or
//! two transient apiserver hiccups.

use std::sync::Arc;
use std::sync::atomic::{AtomicBool, AtomicU64, Ordering};
use std::time::{Duration, Instant};

use k8s_openapi::api::core::v1::Pod;
use k8s_openapi::serde_json::json;
use kube::api::{Api, Patch, PatchParams};
use tracing::{debug, info, warn};

mod election;
pub use election::{Decision, ElectionResult, LeaderElection, Observed, decide};

/// Callbacks fired on lease acquire/lose transitions.
///
/// `run_lease_loop` calls these synchronously from the renewal tick.
/// **They MUST NOT block** — a blocked hook stalls the renewal tick,
/// the lease expires, and another replica acquires (dual-leader). Spawn
/// a task if you need async work (e.g. notifying an actor channel).
///
/// Per-component metrics (`rio_{scheduler,controller}_lease_*_total`)
/// are emitted from the hook impl, not from `run_lease_loop`, so each
/// consumer names them per the `rio_{component}_` convention.
pub trait LeaseHooks: Clone + Send + 'static {
    /// Called once per standby→leader transition, AFTER
    /// [`LeaderState::on_acquire`] has incremented generation and set
    /// `is_leader=true`.
    fn on_acquire(&self);
    /// Called once per leader→standby transition (explicit lose OR local
    /// self-fence), AFTER [`LeaderState::on_lose`] has cleared `is_leader`
    /// and `recovery_complete`.
    fn on_lose(&self);
}

/// Lease TTL. After this much time without renewal, another
/// replica can acquire.
const LEASE_TTL: Duration = Duration::from_secs(15);

/// Renewal interval. LEASE_TTL / 3 per K8s convention.
const RENEW_INTERVAL: Duration = Duration::from_secs(5);

/// Slack between renew timeout and RENEW_INTERVAL. Each renew
/// attempt must return BEFORE the next interval tick would fire;
/// otherwise a hung apiserver burns multiple ticks on one call.
/// 3s deadline for a Lease GET+PUT is generous (healthy p99 <100ms)
/// while still giving 3 attempts before LEASE_TTL.
const RENEW_SLOP: Duration = Duration::from_secs(2);

/// Lease configuration, built from the scheduler's figment Config.
///
/// `Option` because it's entirely optional — `None` means non-K8s
/// mode (the common case for VM tests and dev). `from_parts()`
/// returns `None` unless `lease_name` is set.
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
    /// Build from figment-merged config fields. Returns `None` if
    /// `lease_name` is unset — the signal for "not running under
    /// K8s." Goes through figment like every other config knob
    /// (previously read `std::env::var` directly, bypassing the
    /// TOML/CLI layers — plan 21 Batch E).
    ///
    /// `lease_namespace = None` falls through to reading the
    /// in-cluster service-account namespace mount (standard K8s
    /// downward-API path). If that's ALSO missing (running locally
    /// against a remote cluster), defaults to "default" — probably
    /// wrong, but the operator will notice when the Lease doesn't
    /// appear where expected.
    ///
    /// `HOSTNAME` stays a raw env read: it's set by K8s (not us),
    /// not `RIO_`-prefixed, and has no TOML/CLI equivalent. If
    /// missing (non-K8s with lease_name manually set — weird but
    /// possible for testing), falls back to a UUID. Unique, just
    /// not human-readable in `kubectl get lease`.
    pub fn from_parts(lease_name: Option<String>, lease_namespace: Option<String>) -> Option<Self> {
        let lease_name = lease_name?;

        let namespace = lease_namespace.unwrap_or_else(|| {
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
/// Three atomics — generation, is_leader, recovery_complete — all
/// updated together on acquire/lose transitions. **All writes and
/// reads use SeqCst** to prevent reordering on weak memory models
/// (ARM). Previously is_leader/recovery_complete used Relaxed, which
/// allowed a reader on ARM to see `recovery_complete=true` before
/// `is_leader=true` during an acquire transition (the two stores
/// reordered). SeqCst gives a single total order across all three
/// atomics: if a reader sees the last store of a transition, it sees
/// all prior stores too.
///
/// Transitions go through [`on_acquire`](Self::on_acquire) /
/// [`on_lose`](Self::on_lose) rather than raw field stores — these
/// encapsulate the multi-field update order.
///
/// `Clone` is a cheap triple-Arc clone; main.rs clones once for the
/// lease loop, once for the actor, and uses the per-field accessors for
/// the health-toggle polling loop.
#[derive(Clone)]
pub struct LeaderState {
    /// Generation counter. Incremented on each acquisition via
    /// [`on_acquire`](Self::on_acquire). Same Arc as
    /// DagActor.generation (see `generation_arc()` — this IS that
    /// Arc, cloned).
    generation: Arc<AtomicU64>,
    /// Whether we currently hold the lease. dispatch_ready early-
    /// returns if false (standby schedulers merge DAGs but don't
    /// dispatch — state warm for fast takeover).
    is_leader: Arc<AtomicBool>,
    /// Whether recovery has completed. dispatch_ready gates on
    /// BOTH is_leader AND this. Set by handle_leader_acquired
    /// AFTER recover_from_pg finishes. Cleared by
    /// [`on_lose`](Self::on_lose) so re-acquire re-triggers
    /// recovery.
    ///
    /// Separate from is_leader because the lease loop sets
    /// is_leader IMMEDIATELY (non-blocking — must keep renewing),
    /// then fire-and-forgets LeaderAcquired. Recovery may take
    /// seconds; dispatch waits.
    recovery_complete: Arc<AtomicBool>,
    /// `Instant` of the last [`on_acquire`](Self::on_acquire). `None`
    /// when not leading. Exposed via [`leader_for`](Self::leader_for)
    /// and surfaced as `ListExecutorsResponse.leader_for_secs` so the
    /// controller's `orphan_reap_gate` can fail-closed during the
    /// post-failover partial-reconnect window: `self.executors` fills
    /// incrementally as workers reconnect (1-10s spread), and a
    /// non-empty PARTIAL list cannot prove absence. RwLock not atomic:
    /// `Instant` is 16B; reads are on a slow admin path,
    /// acquire/lose are rare.
    became_leader_at: Arc<parking_lot::RwLock<Option<Instant>>>,
}

impl Default for LeaderState {
    /// Non-K8s/test default: leader immediately, generation = 1,
    /// recovery_complete = true. Same as
    /// `always_leader(Arc::new(AtomicU64::new(1)))`.
    fn default() -> Self {
        Self::always_leader(Arc::new(AtomicU64::new(1)))
    }
}

impl LeaderState {
    /// Shared `is_leader` Arc. SeqCst loads only; writes go through
    /// [`on_acquire`](Self::on_acquire)/[`on_lose`](Self::on_lose).
    pub fn is_leader_arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.is_leader)
    }

    /// Shared `generation` Arc.
    pub fn generation_arc(&self) -> Arc<AtomicU64> {
        Arc::clone(&self.generation)
    }

    /// Shared `recovery_complete` Arc.
    pub fn recovery_complete_arc(&self) -> Arc<AtomicBool> {
        Arc::clone(&self.recovery_complete)
    }

    /// Current generation. `Acquire` load — pairs with the `SeqCst`
    /// `fetch_add` in [`on_acquire`](Self::on_acquire) so a reader
    /// observing the new generation also sees all prior stores.
    pub fn generation(&self) -> u64 {
        self.generation.load(Ordering::Acquire)
    }

    /// Whether we currently hold the lease. `SeqCst` load — see the
    /// struct doc for the multi-field ordering rationale.
    pub fn is_leader(&self) -> bool {
        self.is_leader.load(Ordering::SeqCst)
    }

    /// Whether the actor's `recover_from_pg` has completed since the
    /// last acquire. `SeqCst` load — pairs with
    /// [`set_recovery_complete`](Self::set_recovery_complete) and
    /// [`on_lose`](Self::on_lose).
    pub fn recovery_complete(&self) -> bool {
        self.recovery_complete.load(Ordering::SeqCst)
    }

    /// Mark recovery complete. `SeqCst` store — the actor calls this
    /// AFTER `recover_from_pg` returns (success or fail-empty); pairs
    /// with the `SeqCst` load in `dispatch_ready` so dispatch sees all
    /// recovery writes before proceeding.
    pub fn set_recovery_complete(&self) {
        self.recovery_complete.store(true, Ordering::SeqCst);
    }

    /// Elapsed since this replica acquired leadership, or `None` when
    /// not leading. Populates `ListExecutorsResponse.leader_for_secs`.
    // r[impl sched.admin.list-executors-leader-age]
    pub fn leader_for(&self) -> Option<Duration> {
        self.became_leader_at.read().map(|t| t.elapsed())
    }

    /// Monotonically raise generation to at least `target`. `Release`
    /// `fetch_max` — defensive against Lease annotation reset
    /// (`kubectl delete lease` zeros the annotation; PG's high-water
    /// persists). Returns the previous value.
    pub fn seed_generation_from(&self, target: u64) -> u64 {
        self.generation.fetch_max(target, Ordering::Release)
    }

    /// Construct from pre-existing shared Arcs. Test fixtures that need
    /// to drive the flags from outside the lease loop (e.g. rio-scheduler's
    /// actor recovery tests). Not `#[cfg(test)]`: cross-crate test callers
    /// compile this crate without `--cfg test`.
    pub fn from_parts(
        generation: Arc<AtomicU64>,
        is_leader: Arc<AtomicBool>,
        recovery_complete: Arc<AtomicBool>,
    ) -> Self {
        // Test fixtures: `became_leader_at` mirrors `is_leader` —
        // Some(now) when leading, None when standby. Avoids a fourth
        // param at every from_parts callsite.
        let became_leader_at = is_leader.load(Ordering::SeqCst).then(Instant::now);
        Self {
            generation,
            is_leader,
            recovery_complete,
            became_leader_at: Arc::new(parking_lot::RwLock::new(became_leader_at)),
        }
    }

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
            // Non-K8s/test mode reports a real (small but growing) age
            // so consumers reading `leader_for_secs` don't see 0
            // forever (which the controller treats as "young leader,
            // fail-closed").
            became_leader_at: Arc::new(parking_lot::RwLock::new(Some(Instant::now()))),
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
            became_leader_at: Arc::new(parking_lot::RwLock::new(None)),
        }
    }

    /// Acquire transition: increment generation, set is_leader=true.
    /// Returns the new generation.
    ///
    /// SeqCst on both stores: the generation increment happens-
    /// before the is_leader write in the total order. A reader
    /// seeing is_leader=true (SeqCst load) sees the new generation.
    /// recovery_complete is NOT set here — that's the actor's job
    /// after recover_from_pg finishes.
    pub fn on_acquire(&self) -> u64 {
        let new_gen = self.generation.fetch_add(1, Ordering::SeqCst) + 1;
        self.is_leader.store(true, Ordering::SeqCst);
        // AFTER is_leader=true: a reader seeing `leader_for().is_some()`
        // also sees is_leader=true (RwLock acquire/release pairs with
        // the SeqCst store above).
        *self.became_leader_at.write() = Some(Instant::now());
        new_gen
    }

    /// Lose transition: clear is_leader, clear recovery_complete.
    ///
    /// SeqCst on both stores: is_leader=false happens-before
    /// recovery_complete=false in the total order. A reader seeing
    /// recovery_complete=false (SeqCst) also sees is_leader=false.
    /// Generation is NOT touched — the NEW leader increments it.
    pub fn on_lose(&self) {
        self.is_leader.store(false, Ordering::SeqCst);
        self.recovery_complete.store(false, Ordering::SeqCst);
        *self.became_leader_at.write() = None;
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
/// `hooks`: per-component callbacks (metrics + actor notification).
/// Called synchronously on the transition edge — see [`LeaseHooks`]
/// for the non-blocking constraint.
pub async fn run_lease_loop<H: LeaseHooks>(
    cfg: LeaseConfig,
    state: LeaderState,
    hooks: H,
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

    // Clone for pod-deletion-cost patching. LeaderElection::new
    // takes ownership (wraps the client in Api<Lease>).
    let pod_patch_client = client.clone();

    let mut election = LeaderElection::new(
        client,
        &cfg.namespace,
        cfg.lease_name.clone(),
        cfg.holder_id.clone(),
        LEASE_TTL,
    );

    info!(
        lease = %cfg.lease_name,
        namespace = %cfg.namespace,
        holder = %cfg.holder_id,
        ttl_secs = LEASE_TTL.as_secs(),
        "lease loop starting"
    );

    let mut was_leading = false;
    let mut owe_cost_clear = false;
    let mut last_successful_renew = Instant::now();
    let mut interval = tokio::time::interval(RENEW_INTERVAL);
    // Skip: if one renewal is slow (apiserver busy), don't fire
    // twice immediately. The lease TTL is 15s; we have slack.
    interval.set_missed_tick_behavior(tokio::time::MissedTickBehavior::Skip);

    // Stateful loop (was_leading, last_successful_renew are
    // cross-tick): not spawn_periodic. biased; inlined per
    // r[common.task.periodic-biased].
    loop {
        tokio::select! {
            biased;
            _ = shutdown.cancelled() => break,
            _ = interval.tick() => {}
        }

        let renew_deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        match tokio::time::timeout(renew_deadline, election.try_acquire_or_renew()).await {
            Ok(Ok(result)) => {
                // Successful round-trip (apiserver answered). Even
                // Standby/Conflict reset the self-fence clock — we
                // KNOW the apiserver state, we just don't hold the
                // lease. The clock tracks "am I blind", not "am I
                // leader".
                last_successful_renew = Instant::now();
                // Conflict on renew = someone stole since our GET
                // → unambiguous lose. Conflict on steal = another
                // standby raced us → we were never leading. Both
                // map to now_leading=false; was_leading edge-
                // detection below distinguishes the lose case.
                let now_leading = matches!(result, ElectionResult::Leading);

                // r[impl sched.lease.deletion-cost]
                // Deferred deletion-cost clear: if we self-fenced
                // while the apiserver was unreachable, the cost=0
                // patch was owed but skipped (no connectivity).
                // This is the first reachable observation since —
                // pay the debt now. Level-triggered (not edge):
                // `was_leading` was already flipped false by the
                // self-fence so the lose-transition arm below will
                // never see the edge. If we re-acquired before
                // anyone else (`now_leading`), the acquire arm
                // sets cost=1 anyway, so just drop the debt.
                if std::mem::take(&mut owe_cost_clear) && !now_leading {
                    spawn_patch_deletion_cost(
                        pod_patch_client.clone(),
                        cfg.namespace.clone(),
                        cfg.holder_id.clone(),
                        0,
                    );
                }

                if now_leading && !was_leading {
                    // ---- Acquire transition ----
                    // on_acquire: increment generation FIRST, then
                    // set is_leader, both SeqCst. A reader seeing
                    // is_leader=true also sees the new generation.
                    // The other order would let dispatch run with
                    // is_leader=true but OLD generation for one
                    // pass — harmless (workers compare heartbeat
                    // gen, not assignment gen, for staleness) but
                    // conceptually wrong.
                    let new_gen = state.on_acquire();
                    info!(
                        generation = new_gen,
                        holder = %cfg.holder_id,
                        "acquired leadership"
                    );

                    // r[impl sched.lease.deletion-cost]
                    // Annotate our own Pod with pod-deletion-cost=1.
                    // K8s's ReplicaSet controller sorts by this when
                    // picking which pod to kill during scale-down
                    // (incl. RollingUpdate). Leader gets the higher
                    // cost → k8s kills the standby first → no
                    // leadership churn on rollout. Fire-and-forget:
                    // the lease loop MUST NOT block (see below).
                    // Patch failure is non-fatal — without the
                    // annotation, k8s picks arbitrarily (rollout
                    // still works, just with possible double-churn).
                    spawn_patch_deletion_cost(
                        pod_patch_client.clone(),
                        cfg.namespace.clone(),
                        cfg.holder_id.clone(),
                        1,
                    );

                    // r[impl sched.lease.non-blocking-acquire]
                    // Fire the per-component on-acquire hook
                    // (metrics + actor notification). The hook MUST
                    // NOT block — see LeaseHooks doc.
                    //
                    // NON-BLOCKING IS LOAD-BEARING: if a hook
                    // awaited recovery, a slow recovery (>15s for a
                    // large DAG) would stall the renewal tick →
                    // lease expires → another replica acquires →
                    // dual-leader. Hooks spawn; the separate
                    // recovery_complete flag lets the loop keep
                    // renewing regardless.
                    hooks.on_acquire();
                } else if !now_leading && was_leading {
                    // ---- Lose transition ----
                    // Someone else acquired (we couldn't renew in
                    // time). Stop dispatching. Generation is the
                    // NEW leader's job to increment. on_lose clears
                    // both is_leader and recovery_complete (SeqCst):
                    // if we re-acquire, recovery runs again — the
                    // other replica's actions may have changed PG.
                    state.on_lose();
                    warn!(
                        holder = %cfg.holder_id,
                        "lost leadership (another replica acquired)"
                    );

                    // r[impl sched.lease.standby-tick-noop]
                    // Symmetric with on_acquire above: fire the
                    // per-component on-lose hook (metrics + actor
                    // notification). Same non-blocking constraint.
                    // is_leader is already false (above) so the
                    // consumer's tick early-returns regardless;
                    // this just lets it drop stale state and zero
                    // leader-only gauges.
                    hooks.on_lose();

                    // Clear deletion cost — we're standby now, K8s
                    // should prefer to kill us over the new leader.
                    spawn_patch_deletion_cost(
                        pod_patch_client.clone(),
                        cfg.namespace.clone(),
                        cfg.holder_id.clone(),
                        0,
                    );
                    owe_cost_clear = false;
                }
                // else: steady state (still leading, renewed; or
                // still standby, someone else holds). No log —
                // 5s interval would be noisy.

                was_leading = now_leading;
            }
            outcome @ (Ok(Err(_)) | Err(_)) => {
                // Either apiserver returned an error (Ok(Err)) or
                // our timeout fired before it answered (Err(Elapsed)).
                // Both mean: no fresh view of the Lease object.
                match &outcome {
                    Ok(Err(e)) => {
                        warn!(error = %e, "lease renew failed (apiserver error); retrying next tick");
                    }
                    Err(_) => {
                        warn!(deadline = ?renew_deadline, "lease renew TIMED OUT (apiserver hung?); retrying next tick");
                    }
                    Ok(Ok(_)) => unreachable!(),
                }
                //
                // Local self-fence: if LEASE_TTL has elapsed since
                // the last SUCCESSFUL round-trip, flip is_leader
                // locally. At this point, any replica that CAN reach
                // the apiserver has already stolen (observed-record
                // TTL = our TTL; same clock).
                //
                // The old "DON'T flip — apiserver down for EVERYONE"
                // argument is wrong once elapsed > TTL. In the
                // symmetric-partition case (nobody reaches apiserver)
                // flipping costs nothing: workers can't be scheduled
                // anyway. In the asymmetric case (WE are partitioned,
                // peer is not) NOT flipping makes us a stale-assignment
                // noise generator. Worker-side generation fence
                // (r[sched.lease.generation-fence]) saves correctness
                // either way; this fence saves ops sanity.
                if maybe_self_fence(
                    &state,
                    &mut was_leading,
                    &mut owe_cost_clear,
                    last_successful_renew,
                ) {
                    // Self-fence is a lose-transition: same on-lose
                    // hook as the explicit lose arm above.
                    hooks.on_lose();
                }
            }
        }
    }

    // r[impl sched.lease.graceful-release]
    // Graceful release: on shutdown, release the lease so the next
    // replica acquires immediately (~1s poll) instead of waiting
    // for TTL expiry (15s). Gate on was_leading to skip the
    // apiserver round-trip when we were standby all along. Any
    // error is non-fatal: we're shutting down regardless, and
    // TTL expiry is the fallback.
    if was_leading {
        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        match tokio::time::timeout(deadline, election.step_down()).await {
            Ok(Ok(())) => info!("released lease on shutdown"),
            Ok(Err(e)) => warn!(
                error = %e,
                "step_down failed; lease will expire in {}s",
                LEASE_TTL.as_secs()
            ),
            Err(_) => warn!(
                ?deadline,
                "step_down timed out (apiserver hung?); lease will expire in {}s",
                LEASE_TTL.as_secs()
            ),
        }
    }
    debug!("lease loop exited");
}

/// Local self-fence: if we believed we were leading but haven't
/// had a successful apiserver round-trip in over `LEASE_TTL`, flip
/// `is_leader=false` locally. At this point, any replica that CAN
/// reach the apiserver has already stolen. The only world where we're
/// still the rightful leader is one where NOBODY can reach the
/// apiserver — in which case dispatch is pointless anyway.
///
/// Extracted from `run_lease_loop`'s error arm so it can be unit-
/// tested without spawning the full loop (paused time + real TCP
/// mocks cause spurious deadline-exceeded; see lang-gotchas).
///
/// Returns `true` if the fence fired (for test assertions).
// r[impl sched.lease.self-fence]
fn maybe_self_fence(
    state: &LeaderState,
    was_leading: &mut bool,
    owe_cost_clear: &mut bool,
    last_successful_renew: Instant,
) -> bool {
    if *was_leading && last_successful_renew.elapsed() > LEASE_TTL {
        warn!(
            blind_for = ?last_successful_renew.elapsed(),
            "LOCAL SELF-FENCE: no successful renew in > LEASE_TTL, stepping down locally"
        );
        state.on_lose();
        *was_leading = false;
        // No spawn_patch_deletion_cost here: can't reach apiserver.
        // Record the debt so the FIRST reachable round-trip in
        // run_lease_loop's Ok arm patches cost=0. The peer's cost=1
        // doesn't clear OURS — without this deferred clear we'd stay
        // tied at cost=1 with the new leader and the next
        // RollingUpdate picks arbitrarily.
        *owe_cost_clear = true;
        true
    } else {
        false
    }
}

// r[impl sched.lease.deletion-cost]
/// Fire-and-forget PATCH on our own Pod's `controller.kubernetes.io/
/// pod-deletion-cost` annotation. K8s's ReplicaSet controller sorts
/// pods by this value (ascending) when deciding which to evict during
/// scale-down --- including RollingUpdate surge reconciliation. Leader
/// sets cost=1, standby sets cost=0, k8s kills the standby first.
///
/// `tokio::spawn` because the lease loop MUST NOT block. A slow
/// apiserver PATCH (>15s) would stall the renew tick, the lease
/// expires, another replica acquires — dual-leader. Same constraint
/// as the LeaderAcquired actor send (see `run_lease_loop`).
///
/// Merge patch (not Apply): we only touch one annotation key; Apply
/// would need a fieldManager and a fuller object shape. Merge is
/// `kubectl annotate --overwrite` semantics.
fn spawn_patch_deletion_cost(client: kube::Client, namespace: String, pod_name: String, cost: i32) {
    tokio::spawn(async move {
        let pods: Api<Pod> = Api::namespaced(client, &namespace);
        // The annotation value is a string (all k8s annotations are),
        // parsed as int32 by the ReplicaSet controller. Invalid
        // values sort as 0.
        let patch = json!({
            "metadata": {
                "annotations": {
                    "controller.kubernetes.io/pod-deletion-cost": cost.to_string()
                }
            }
        });
        match pods
            .patch(&pod_name, &PatchParams::default(), &Patch::Merge(&patch))
            .await
        {
            Ok(_) => debug!(%pod_name, cost, "patched pod-deletion-cost"),
            Err(e) => {
                // Non-fatal: rollout still works, k8s just picks
                // arbitrarily. RBAC missing `patch pods` is the
                // likely cause — 403 Forbidden. VM tests don't
                // catch this (k3s admin kubeconfig bypasses RBAC).
                warn!(
                    %pod_name, cost, error = %e,
                    "failed to patch pod-deletion-cost (rollout still works, \
                     k8s will pick arbitrarily during scale-down)"
                );
            }
        }
    });
}

// r[verify sched.lease.k8s-lease]
// r[verify sched.lease.generation-fence]
#[cfg(test)]
mod tests {
    use super::*;

    /// from_parts returns None when lease_name unset — the signal
    /// for "non-K8s mode." This is how VM tests stay unaffected.
    /// Previously `from_env()` read `std::env::var("RIO_LEASE_NAME")`
    /// directly (bypassing figment); now the scheduler's Config
    /// passes the merged value through.
    #[test]
    fn from_parts_none_when_unset() {
        assert!(
            LeaseConfig::from_parts(None, None).is_none(),
            "no lease_name → None → non-K8s mode"
        );
        // Namespace alone doesn't trigger K8s mode — lease_name is
        // the gate.
        assert!(
            LeaseConfig::from_parts(None, Some("rio-prod".into())).is_none(),
            "namespace without lease_name → still None"
        );
    }

    #[test]
    #[allow(clippy::result_large_err)] // figment::Error is 208B, API-fixed
    fn from_parts_reads_all_three() {
        // HOSTNAME is still a raw env read (K8s sets it, not us).
        // figment Jail serializes env access across parallel tests.
        figment::Jail::expect_with(|jail| {
            jail.set_env("HOSTNAME", "rio-scheduler-0");

            let cfg = LeaseConfig::from_parts(
                Some("rio-scheduler-leader".into()),
                Some("rio-prod".into()),
            )
            .expect("lease_name set → Some");
            assert_eq!(cfg.lease_name, "rio-scheduler-leader");
            assert_eq!(cfg.namespace, "rio-prod");
            assert_eq!(cfg.holder_id, "rio-scheduler-0");
            Ok(())
        });
    }

    /// Namespace None → read serviceaccount mount → fall back to
    /// "default" when that's also missing (non-K8s host running
    /// tests has no /var/run/secrets/kubernetes.io mount).
    #[test]
    fn from_parts_namespace_fallback() {
        let cfg = LeaseConfig::from_parts(Some("lease".into()), None).unwrap();
        // On a dev/CI host with no serviceaccount mount, the
        // read_to_string fails → "default". On the off chance the
        // CI runner IS a pod with a mount, the namespace will be
        // something else — just check it's non-empty.
        assert!(!cfg.namespace.is_empty());
    }

    // HOSTNAME fallback to UUID is a one-liner
    // (`.unwrap_or_else(|| Uuid::new_v4())`). Not worth a test —
    // the UUID crate tests itself.

    // r[verify sched.admin.list-executors-leader-age]
    /// `leader_for()` tracks acquire/lose. `pending` → None;
    /// `on_acquire` → Some; `on_lose` → None; `always_leader`/
    /// `Default` → Some (non-K8s mode reports a real age so the
    /// controller's `leader_for_secs` gate isn't permanently 0).
    #[test]
    fn leader_for_tracks_acquire_lose() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        assert!(state.leader_for().is_none(), "pending → None");
        state.on_acquire();
        assert!(state.leader_for().is_some(), "on_acquire → Some(elapsed)");
        state.on_lose();
        assert!(state.leader_for().is_none(), "on_lose → None");

        assert!(
            LeaderState::default().leader_for().is_some(),
            "Default (always_leader) → Some so non-K8s mode reports real age"
        );
    }

    #[test]
    fn leader_state_always_leader() {
        let gen_arc = Arc::new(AtomicU64::new(1));
        let state = LeaderState::always_leader(Arc::clone(&gen_arc));
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

    // ---- Renewal timeout + self-fence (remediation 08) -----------

    use super::election::LeaderElection;
    use rio_test_support::kube_mock::ApiServerVerifier;

    /// Apiserver accepts the connection but never responds. The
    /// renew timeout must fire within RENEW_INTERVAL - RENEW_SLOP.
    /// Without the timeout wrapper at `run_lease_loop`'s callsite,
    /// `try_acquire_or_renew` would hang until the outer tokio::test
    /// timeout — proving the bug.
    ///
    /// Hang injection: hold the verifier WITHOUT calling `.run()`.
    /// The tower-test mock's Handle stays alive (request can queue)
    /// but nobody ever calls `next_request()` to pull + respond →
    /// the client's GET pends forever. Calling `.run(vec![])` would
    /// NOT work: the spawned task drops the Handle on return, which
    /// makes the mock return `ServiceError::Closed` immediately.
    #[tokio::test]
    async fn renew_timeout_fires_on_hung_apiserver() {
        // _verifier binding keeps the tower-test Handle alive for
        // the whole test body. No drop-bomb on ApiServerVerifier
        // itself (only on the VerifierGuard returned by .run()).
        let (client, _verifier) = ApiServerVerifier::new();

        let mut election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            LEASE_TTL,
        );

        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        let started = Instant::now();
        let result = tokio::time::timeout(deadline, election.try_acquire_or_renew()).await;

        assert!(
            result.is_err(),
            "timeout should fire (apiserver hung), got {result:?}"
        );
        // Prove it was OUR timeout, not some inner kube-rs deadline.
        // If kube-rs had a default request timeout < 3s, this would
        // complete early with Ok(Err(_)) and the elapsed check would
        // catch it.
        let elapsed = started.elapsed();
        assert!(
            elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
            "timeout fired at {elapsed:?}, expected ~{deadline:?}"
        );
    }

    /// step_down on a hung apiserver must time out, not hang shutdown.
    /// Same hang-injection as renew_timeout_fires_on_hung_apiserver:
    /// hold the verifier without `.run()` so the GET pends forever.
    /// Without the timeout wrapper at `run_lease_loop`'s shutdown
    /// branch, `main.rs`'s `h.await` would block until SIGKILL.
    // r[verify sched.lease.graceful-release]
    #[tokio::test]
    async fn step_down_timeout_fires_on_hung_apiserver() {
        let (client, _verifier) = ApiServerVerifier::new();
        let election = LeaderElection::new(
            client,
            "default",
            "rio-sched".into(),
            "us".into(),
            LEASE_TTL,
        );

        let deadline = RENEW_INTERVAL.saturating_sub(RENEW_SLOP);
        let started = Instant::now();
        let result = tokio::time::timeout(deadline, election.step_down()).await;

        assert!(result.is_err(), "step_down should time out, got {result:?}");
        let elapsed = started.elapsed();
        assert!(
            elapsed >= deadline && elapsed < deadline + Duration::from_millis(500),
            "fired at {elapsed:?}, expected ~{deadline:?}"
        );
    }

    /// Self-fence fires when `last_successful_renew` is older than
    /// LEASE_TTL and we believed we were leading. Simulates the
    /// state after 4+ failed renew ticks (5s each, TTL=15s).
    #[test]
    fn self_fence_flips_is_leader_after_ttl_of_failures() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.recovery_complete.store(true, Ordering::Relaxed);

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        // 20s ago > LEASE_TTL (15s). Pattern matches election.rs:535.
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

        assert!(fired, "self-fence should fire past TTL");
        assert!(
            !state.is_leader.load(Ordering::Relaxed),
            "self-fence should flip is_leader=false"
        );
        assert!(
            !state.recovery_complete.load(Ordering::Relaxed),
            "self-fence should clear recovery_complete (re-acquire re-runs recovery)"
        );
        assert!(
            !was_leading,
            "was_leading should flip so next tick is edge-free"
        );
    }

    /// Self-fence does NOT fire within TTL. One or two transient
    /// apiserver blips should not cause step-down — the lease may
    /// still be validly held (the original "DON'T flip" comment's
    /// reasoning is correct for the FIRST few failures).
    #[test]
    fn self_fence_does_not_flip_before_ttl() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.recovery_complete.store(true, Ordering::Relaxed);

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        // 10s ago < LEASE_TTL (15s). Two failed ticks, lease still valid.
        let last_renew = Instant::now() - Duration::from_secs(10);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

        assert!(!fired, "within TTL → no self-fence");
        assert!(
            state.is_leader.load(Ordering::Relaxed),
            "within TTL → still leader (transient blip)"
        );
        assert!(state.recovery_complete.load(Ordering::Relaxed));
        assert!(was_leading);
    }

    /// Self-fence is gated on `was_leading`. A standby that has
    /// NEVER held the lease should not "step down" — it has nothing
    /// to step down from. Avoids spurious lease_lost_total increments
    /// from a standby whose apiserver connectivity is flaky.
    #[test]
    fn self_fence_no_op_when_not_leading() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        // is_leader already false, recovery_complete already false.

        let mut was_leading = false;
        let mut owe_cost_clear = false;
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);

        assert!(!fired, "not leading → no fence even past TTL");
        assert!(!state.is_leader.load(Ordering::Relaxed));
        assert!(!was_leading);
    }

    /// Self-fence sets `owe_cost_clear` so the lease loop's first
    /// reachable round-trip clears our pod-deletion-cost annotation.
    /// Without the deferred clear, an ex-leader keeps cost=1 tied with
    /// the new leader (peer's cost=1 patch doesn't touch OUR pod) and
    /// the next RollingUpdate evicts arbitrarily — defeating
    /// `r[sched.lease.deletion-cost]`. Regression: maybe_self_fence
    /// previously consumed the `was_leading` edge without arranging
    /// the deferred patch.
    // r[verify sched.lease.deletion-cost]
    #[test]
    fn self_fence_sets_owe_cost_clear() {
        let state = LeaderState::pending(Arc::new(AtomicU64::new(2)));
        state.is_leader.store(true, Ordering::Relaxed);
        state.recovery_complete.store(true, Ordering::Relaxed);

        let mut was_leading = true;
        let mut owe_cost_clear = false;
        let last_renew = Instant::now() - Duration::from_secs(20);

        let fired = maybe_self_fence(&state, &mut was_leading, &mut owe_cost_clear, last_renew);
        assert!(fired);
        assert!(
            owe_cost_clear,
            "self-fence must record the owed cost=0 patch (apiserver unreachable now, \
             so the lease loop pays it on first reachable round-trip)"
        );

        // No-fire path leaves the flag untouched (a standby that never
        // led has no cost to clear).
        let standby = LeaderState::pending(Arc::new(AtomicU64::new(1)));
        let mut was_leading = false;
        let mut owe_cost_clear = false;
        let fired = maybe_self_fence(
            &standby,
            &mut was_leading,
            &mut owe_cost_clear,
            Instant::now() - Duration::from_secs(20),
        );
        assert!(!fired);
        assert!(!owe_cost_clear, "no-fire → no debt recorded");
    }
}
