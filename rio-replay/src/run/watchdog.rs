//! Suspension predicate and stall/queued watchdog for the campaign's
//! submit/collect phase.
//!
//! Per-job stall clocks accrue wall-clock time only while the cluster is
//! actually able to make progress. The suspension predicate is the OR of
//! four independently observable components; while any of them holds, every
//! clock is frozen so a cluster-wide outage is never charged to individual
//! jobs as a stall:
//!
//! - [`COMPONENT_PAUSE`]: the engine itself is not submitting (manual
//!   operator pause or backpressure pause), so queued work is expected to
//!   sit still.
//! - [`COMPONENT_IDLE`]: the cluster reports queued derivations but nothing
//!   occupied — running on an executor or substituting — for several
//!   consecutive polls; the scheduler is not dispatching at all, which is a
//!   cluster problem, not a per-job one. Asserts only while the
//!   ClusterStatus feed is fresh: after
//!   `CLUSTER_STALE_AFTER_FAILED_POLLS` consecutive failed polls the
//!   saturated streak stops asserting until a poll succeeds again, so a
//!   persistent RPC outage cannot freeze the stall clocks on a streak the
//!   engine can no longer confirm.
//! - [`COMPONENT_ICE`]: capacity provisioning is failing broadly (at least
//!   the configured number of spawn cells are masked after capacity
//!   errors). The masked-cells snapshot is sticky between the (slower)
//!   spawn-intent polls but may not outlive failed ones: after
//!   `ICE_STALE_AFTER_FAILED_POLLS` consecutive failed polls the component
//!   de-asserts until a poll succeeds again, so a persistent RPC outage
//!   cannot freeze the stall clocks on a snapshot the engine can no longer
//!   confirm.
//! - [`COMPONENT_DISPATCH`]: executors sit idle while work is queued and
//!   substitution is quiescent (the active-executors minus
//!   running-derivations gap stays above the threshold for several
//!   consecutive polls with no derivation substituting). Substitutions run
//!   as detached scheduler→store fetches that occupy no executor slot, and
//!   a substitution cascade legitimately keeps the ready queue non-empty
//!   with deferrals waiting on the next probe pass — so an idle fleet next
//!   to queued work indicts the dispatcher only once nothing is
//!   substituting. The run loop also pauses submission on this component,
//!   so the engine stops piling more work onto a scheduler that is not
//!   dispatching what it already has. Like the idle component, a saturated
//!   streak asserts only while the ClusterStatus feed is fresh — a
//!   submission pause must never outlive the engine's ability to observe
//!   the gap it is reacting to.
//!
//! While no component holds, the clocks run: a job Active for
//! `active_stall_hours` is reported as an active stall (the run loop
//! decides auto-retry vs terminal), and a job Queued for
//! `queued_watchdog_hours` is reported for a non-terminal re-enqueue
//! (clock reset), escalating to a terminal verdict only after
//! `max_queued_requeues` re-enqueues.
//!
//! This is a pure state machine over [`PollTick`]s — the run loop owns the
//! polling cadence, feeds the observations, and acts on the verdicts.
//!
//! Scope: only campaign jobs registered via [`Watchdog::observe_job`] are
//! tracked — the job ledger emits those observations at the transition
//! sites (batch commitment, requeue, retirement), so a job is tracked
//! from its first offer and never before. Warm-stage batches are
//! deliberately NOT under this watchdog — the warm stage never registers
//! its roots; a wedged warm batch is bounded by the per-batch child
//! timeout (`batch_timeout_hours`) instead, and every warm root receives
//! a terminal disposition as soon as its batch settles.

use std::collections::{BTreeMap, HashMap};

use serde::{Deserialize, Serialize};

use super::grpc::{ClusterCounts, IceSnapshot};
use super::spec::Knobs;

/// [`TickOutcome::components`] / [`SuspensionWindow::components`] value for
/// the engine's own submission pause (manual or backpressure).
pub const COMPONENT_PAUSE: &str = "pause";

/// Suspension-component value for the cluster-idle condition: queued
/// derivations with nothing running or substituting for
/// `idle_polls_for_suspend` consecutive polls.
pub const COMPONENT_IDLE: &str = "idle";

/// Suspension-component value for the capacity condition: at least
/// `ice_masked_cells_threshold` spawn cells masked after capacity errors.
pub const COMPONENT_ICE: &str = "ice";

/// Suspension-component value for the dispatch-gap condition: idle
/// executors while work is queued and substitution is quiescent, sustained
/// for `dispatch_gap_polls` consecutive polls. Also pauses submission.
pub const COMPONENT_DISPATCH: &str = "dispatch";

/// Consecutive failed spawn-intents polls after which the sticky
/// [`COMPONENT_ICE`] snapshot is considered stale and stops asserting the
/// component. One failed poll holds the prior state — a leader-failover or
/// deadline blip must not flap the suspension; from the second consecutive
/// failure the latched evidence is at least two poll intervals old (~2 ×
/// `spawn_intents_poll_secs`). A capacity suspension freezes every stall
/// clock, so it must never persist on evidence the engine can no longer
/// confirm — only a fresh successful poll re-arms the component. The stale
/// snapshot itself is retained, not zeroed: a failed poll is missing
/// evidence, not an observation that the cells were unmasked.
const ICE_STALE_AFTER_FAILED_POLLS: u32 = 2;

/// Consecutive failed ClusterStatus polls after which the saturated
/// idle/dispatch streaks stop asserting their components — the cluster
/// feed's twin of [`ICE_STALE_AFTER_FAILED_POLLS`], with the same
/// rationale: one failed poll holds the prior state (no flapping on a
/// leader-failover blip), a second leaves the latched streaks resting on
/// evidence the engine can no longer confirm. The streak VALUES are
/// retained, not zeroed — a failed poll is missing evidence, not an
/// observation that the cluster recovered — so the first fresh poll that
/// still matches the predicate re-asserts immediately. Gating matters on
/// both components: a latched COMPONENT_DISPATCH wedges a timeless
/// campaign (it pauses submission), and a latched COMPONENT_IDLE silently
/// disables stall detection for as long as the outage lasts.
const CLUSTER_STALE_AFTER_FAILED_POLLS: u32 = 2;

/// Engine-side phase of one campaign job, as fed by the run loop.
///
/// `Active` means "member of an in-flight batch"; everything else is
/// `Queued`. Per-drv assigned/running/substituting refinement is deferred —
/// in-band per-root results arrive only when a batch settles, so there is
/// no mid-batch per-drv view — and the per-batch timeout remains the hard
/// backstop either way.
#[derive(Debug, Clone, Copy, PartialEq, Eq)]
pub enum JobPhase {
    Queued,
    Active,
}

/// Per-job stall clock: unsuspended seconds accrued in the current phase,
/// plus how many queued-watchdog re-enqueues this job has already used.
#[derive(Debug, Clone)]
struct JobClock {
    phase: JobPhase,
    accrued_secs: f64,
    requeues: u32,
}

/// One RPC-feed observation carried by a [`PollTick`]: the watchdog's two
/// remote evidence feeds (ClusterStatus counts, spawn-intents capacity)
/// both arrive through this type, never as `Option<T>`.
///
/// Three-valued because a failed poll and a tick that never attempted one
/// mean different things to latched suspension state: between polls the
/// last observation simply stays current ([`Polled::NotPolled`] — a
/// by-design cadence gap), while a failed attempt ([`Polled::Failed`])
/// leaves the latched state one interval staler than the cadence
/// promises. Collapsing failure into "no observation" (the `Option`/`.ok()`
/// shape this type replaces) lets latched state — a sticky over-threshold
/// capacity snapshot, a saturated idle/dispatch streak — assert a
/// suspension forever across a persistent RPC outage, freezing stall
/// clocks (or pausing submission) on evidence the engine can no longer
/// confirm.
///
/// Scope: exactly the RPC feeds. `PollTick::engine_paused` is in-process
/// state, fresh by construction, and needs no staleness arm; the
/// infra-rate pause's evidence (the rolling terminal-record window) is not
/// a poll at all — its staleness is causal (the pause suppresses its own
/// producers) and is handled by the canary-probe ladder behind
/// `BackpressureSource::InfraRate`'s self-clearing witness, not by this
/// type.
#[derive(Debug, Clone, Default, PartialEq)]
pub enum Polled<T> {
    /// No poll was attempted this tick (by-design cadence gap): the last
    /// observation remains in effect.
    #[default]
    NotPolled,
    /// A poll was attempted and the RPC failed. The prior state holds for
    /// the first failure (a transient blip must not flap a suspension, and
    /// the stall clocks with it); from the per-feed staleness threshold
    /// ([`ICE_STALE_AFTER_FAILED_POLLS`] /
    /// [`CLUSTER_STALE_AFTER_FAILED_POLLS`]) consecutive failures the
    /// latched state stops asserting its components until a poll succeeds
    /// again.
    Failed,
    /// The poll succeeded: this observation replaces the latched one and
    /// resets the feed's failure streak.
    Fresh(T),
}

impl<T> Polled<T> {
    /// The fresh observation, when this tick carries one.
    pub fn fresh(&self) -> Option<&T> {
        match self {
            Polled::Fresh(value) => Some(value),
            Polled::Failed | Polled::NotPolled => None,
        }
    }
}

/// Spawn-intents (capacity) observation carried by one [`PollTick`].
pub type IcePoll = Polled<IceSnapshot>;

/// One observation the run loop feeds the watchdog (typically every
/// `cluster_status_poll_secs`; `ice` is refreshed only every
/// `spawn_intents_poll_secs`, so most ticks carry [`Polled::NotPolled`]
/// there and the last snapshot stays in effect).
#[derive(Debug, Clone, Default)]
pub struct PollTick {
    pub at_unix: i64,
    /// Latest ClusterStatus counts. The run loop polls this feed every
    /// tick, so `NotPolled` does not occur in production (tests use it for
    /// ticks that only exercise the clocks); a failed poll arrives as
    /// [`Polled::Failed`] — the idle and dispatch streaks then stay
    /// unchanged (missing evidence is not an observation), and from
    /// [`CLUSTER_STALE_AFTER_FAILED_POLLS`] consecutive failures the
    /// saturated streaks stop asserting their components.
    pub cluster: Polled<ClusterCounts>,
    /// Spawn-intents poll outcome for this tick.
    pub ice: IcePoll,
    /// The engine's own submission-pause flag — in-process state, fresh by
    /// construction (never an RPC feed, so no staleness arm).
    pub engine_paused: bool,
}

/// What kind of stall the watchdog reports for a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallKind {
    /// Active for at least `active_stall_hours` of unsuspended time. The
    /// run loop decides what follows (the single auto-retry, or a terminal
    /// infrastructure record once that budget is spent). The clock is NOT
    /// reset at emission — the verdict stays armed (re-firing each tick)
    /// until the run loop's action commits a ledger transition that
    /// clears it (`observe_job`'s phase change or `retire`'s removal), so
    /// a failed action retries on the next poll instead of waiting out a
    /// full re-accrual.
    ActiveStall,
    /// Queued for at least `queued_watchdog_hours` of unsuspended time
    /// with re-enqueue budget remaining: non-terminal re-enqueue. Armed
    /// like the other arms — the clock resets and the consumed-requeue
    /// count increments only when [`Watchdog::confirm_queued_requeue`]
    /// commits after the run loop journals the ladder step, so a failed
    /// append re-fires the verdict instead of losing a budget move that
    /// resume would never see.
    QueuedRequeue,
    /// Queued past the threshold again after `max_queued_requeues`
    /// re-enqueues were already spent: the run loop records a terminal
    /// infrastructure outcome instead of re-enqueueing forever. Like
    /// [`StallKind::ActiveStall`], armed until the terminal record's
    /// `retire` commits.
    QueuedEscalate,
}

/// One stall verdict for one job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub struct StallVerdict {
    pub job: String,
    pub kind: StallKind,
    /// Queued-watchdog re-enqueues this job has used AFTER this verdict
    /// (the post-increment count for a [`StallKind::QueuedRequeue`]), so
    /// the operator log can report "requeue n/max_queued_requeues".
    pub requeues_used: u32,
}

/// One contiguous window during which the suspension predicate was true.
#[derive(Debug, Clone, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SuspensionWindow {
    pub started_at_unix: i64,
    /// `None` while the window is still open.
    pub ended_at_unix: Option<i64>,
    /// The `COMPONENT_*` values observed during the window (each listed
    /// once, in first-seen order).
    pub components: Vec<String>,
}

/// Serializable suspension summary for progress.json / the final report.
#[derive(Debug, Clone, Default, Serialize, Deserialize, PartialEq)]
#[serde(rename_all = "camelCase")]
pub struct SuspensionSummary {
    pub windows: Vec<SuspensionWindow>,
    /// Total suspended seconds attributed to each component (a tick's
    /// interval counts toward every component active on that tick, so the
    /// per-component totals can overlap rather than summing to the
    /// suspended wall-clock time).
    pub total_secs_by_component: BTreeMap<String, f64>,
}

/// The watchdog state machine: suspension streaks, per-job stall clocks,
/// and the suspension-window log.
pub struct Watchdog {
    knobs: Knobs,
    /// Per-job clocks, keyed by job name. BTreeMap so multiple verdicts on
    /// one tick (and any logging derived from them) come out in a
    /// deterministic order.
    jobs: BTreeMap<String, JobClock>,
    /// Per-job queued-watchdog re-enqueues already consumed in PRIOR
    /// processes, folded from the requeue journal on resume. Consulted
    /// when a clock is created so a pod restart cannot re-grant a
    /// mid-ladder job the full `(max_queued_requeues + 1) ×
    /// queued_watchdog_hours` escalation ladder — the same restart-edge
    /// budget reset the journal exists to prevent for the resubmission
    /// counters. (The clocks themselves are deliberately volatile — builds
    /// restart, so stall BASELINES must too — but the consumed ladder
    /// budget is not a baseline.)
    requeue_seed: HashMap<String, u32>,
    idle_streak: u32,
    dispatch_streak: u32,
    /// Consecutive ClusterStatus polls that failed (not-polled ticks leave
    /// it unchanged; a successful poll resets it). At
    /// [`CLUSTER_STALE_AFTER_FAILED_POLLS`] the saturated idle/dispatch
    /// streaks stop asserting their components — the streak values are
    /// retained, only their authority lapses.
    cluster_failed_polls: u32,
    /// Last seen capacity snapshot — sticky between the (less frequent)
    /// spawn-intent polls, but only allowed to assert [`COMPONENT_ICE`]
    /// while fresh: see [`ICE_STALE_AFTER_FAILED_POLLS`].
    last_ice: IceSnapshot,
    /// Consecutive spawn-intents polls that failed (not-polled ticks leave
    /// it unchanged; a successful poll resets it). At
    /// [`ICE_STALE_AFTER_FAILED_POLLS`] the sticky snapshot stops
    /// asserting [`COMPONENT_ICE`].
    ice_failed_polls: u32,
    last_tick_unix: Option<i64>,
    current_window: Option<SuspensionWindow>,
    summary: SuspensionSummary,
}

/// What one [`Watchdog::on_tick`] call decided.
#[derive(Debug, Clone, Default, PartialEq)]
pub struct TickOutcome {
    /// True when any suspension component is active this tick.
    pub suspended: bool,
    /// The active components (`COMPONENT_*` values).
    pub components: Vec<&'static str>,
    /// Jobs whose stall clocks crossed a threshold this tick.
    pub stalled: Vec<StallVerdict>,
    /// True when the dispatch-gap component is active — the run loop also
    /// pauses submission on it, not just the clocks.
    pub dispatch_pause: bool,
    /// The suspension window that ended on this tick, if one did (already
    /// recorded in the summary). Exposed so the run loop can act on window
    /// edges without edge-detecting `suspended` across ticks itself.
    pub closed_window: Option<SuspensionWindow>,
}

impl Watchdog {
    pub fn new(knobs: Knobs) -> Self {
        Self {
            knobs,
            jobs: BTreeMap::new(),
            requeue_seed: HashMap::new(),
            idle_streak: 0,
            dispatch_streak: 0,
            cluster_failed_polls: 0,
            last_ice: IceSnapshot::default(),
            ice_failed_polls: 0,
            last_tick_unix: None,
            current_window: None,
            summary: SuspensionSummary::default(),
        }
    }

    /// The job ledger reports phases at the transition sites (batch
    /// commitment → Active, collect/stall requeue → Queued). A phase
    /// change resets the clock (but keeps the requeue count); terminal
    /// jobs are removed via [`Self::remove_job`].
    pub fn observe_job(&mut self, job: &str, phase: JobPhase) {
        match self.jobs.get_mut(job) {
            Some(clock) if clock.phase == phase => {}
            Some(clock) => {
                clock.phase = phase;
                clock.accrued_secs = 0.0;
            }
            None => {
                self.jobs.insert(
                    job.to_string(),
                    JobClock {
                        phase,
                        accrued_secs: 0.0,
                        // Prior processes' consumed ladder budget survives
                        // the restart; only the clock baseline restarts.
                        requeues: self.requeue_seed.get(job).copied().unwrap_or(0),
                    },
                );
            }
        }
    }

    /// Stop tracking a job (it reached a terminal record).
    pub fn remove_job(&mut self, job: &str) {
        self.jobs.remove(job);
    }

    /// Seed the consumed queued-requeue budgets from the resume fold
    /// (the `REQUEUE_SOURCE_QUEUED` slice of requeues.jsonl). Called once
    /// at construction time on the production path; clocks created later
    /// start their ladder from the seeded count instead of zero.
    pub fn set_requeue_seed(&mut self, seed: HashMap<String, u32>) {
        self.requeue_seed = seed;
    }

    /// Commit one queued-watchdog re-enqueue: increment the job's consumed
    /// ladder budget and reset its clock. Called by the run loop AFTER the
    /// transition is journaled — the QueuedRequeue verdict stays armed
    /// (re-firing each tick) until this commits, exactly like the other
    /// deferred arms, so a failed journal append can never consume a
    /// ladder step that resume would not see.
    pub fn confirm_queued_requeue(&mut self, job: &str) {
        if let Some(clock) = self.jobs.get_mut(job)
            && clock.phase == JobPhase::Queued
        {
            clock.requeues = clock.requeues.saturating_add(1);
            clock.accrued_secs = 0.0;
        }
    }

    /// Test-only view of a job's current phase (`None` = not tracked), so
    /// transition-site tests can assert observations without reaching into
    /// the private clock map.
    #[cfg(test)]
    pub fn phase_of(&self, job: &str) -> Option<JobPhase> {
        self.jobs.get(job).map(|clock| clock.phase)
    }

    /// Update the streaks from this tick's observations and return the
    /// currently active suspension components.
    fn evaluate_components(&mut self, tick: &PollTick) -> Vec<&'static str> {
        let mut components = Vec::new();
        if tick.engine_paused {
            components.push(COMPONENT_PAUSE);
        }
        match &tick.cluster {
            Polled::Fresh(_) => self.cluster_failed_polls = 0,
            Polled::Failed => {
                self.cluster_failed_polls = self.cluster_failed_polls.saturating_add(1);
            }
            Polled::NotPolled => {}
        }
        if let Polled::Fresh(cluster) = &tick.cluster {
            if cluster.queued_derivations > 0 && cluster.occupied_derivations() == 0 {
                self.idle_streak += 1;
            } else {
                self.idle_streak = 0;
            }
            // Idle executors are a dispatch failure only while work is
            // queued AND substitution is quiescent — both guards keep this
            // predicate consistent with what the idle predicate above
            // counts as occupancy, so one snapshot can never be classified
            // as progress by one sibling and failure by the other.
            //
            // Substitution gates as a quiescence conjunct, not as gap
            // arithmetic, because of how the scheduler executes it:
            // substitutions run as detached scheduler→store fetch tasks
            // (`spawn_substitute_fetches`), never on executor slots, and
            // ClusterStatus's running_derivations counts only worker slots
            // (Assigned|Running). A cascade therefore looks exactly like a
            // dispatch failure from the slot side — running 0, every
            // executor idle — while the ready queue legitimately stays
            // non-empty: each dispatch pass re-queues substitution-lane
            // deferrals (probe-no-verdict nodes, the truncated batch-probe
            // tail) that wait on the next probe, not on an executor.
            // Subtracting substituting from the gap would misread slot
            // accounting; while fetches are in flight the queue's
            // composition (dispatchable vs deferral) is unknowable from
            // these counts, so the gap is evidence of nothing. Once nothing
            // is substituting, queued entries can only be waiting on slots
            // and the gap indicts the dispatcher again.
            //
            // The queued guard is also what keeps this pause self-clearing:
            // the run loop stops submission on this component, so the
            // predicate must only persist on state that can still change
            // while paused. Queued work can drain and in-flight
            // substitutions can complete while paused; an empty queue
            // cannot refill itself — without the guard, a drained cluster
            // would hold the pause, the pause would keep the queue empty,
            // and the campaign would wedge until its deadline.
            let gap = i64::from(cluster.active_executors) - i64::from(cluster.running_derivations);
            if cluster.queued_derivations > 0
                && cluster.substituting_derivations == 0
                && gap > self.knobs.dispatch_gap_threshold
            {
                self.dispatch_streak += 1;
            } else {
                self.dispatch_streak = 0;
            }
        }
        // Stale-evidence gate, the cluster feed's twin of the ICE gate
        // below: a saturated streak asserts its component only while the
        // feed is fresh. The streaks are mutated only under a fresh poll,
        // so across an outage they latch at their last value — without the
        // gate a persistent ClusterStatus outage would keep COMPONENT_IDLE
        // freezing every stall clock and COMPONENT_DISPATCH pausing
        // submission indefinitely, both resting on evidence the engine can
        // no longer confirm (the cluster may have drained or recovered
        // invisibly). The streak values survive the lapse: the first fresh
        // poll that still matches the predicate re-asserts on that tick.
        if self.cluster_failed_polls < CLUSTER_STALE_AFTER_FAILED_POLLS {
            if self.idle_streak >= self.knobs.idle_polls_for_suspend {
                components.push(COMPONENT_IDLE);
            }
            if self.dispatch_streak >= self.knobs.dispatch_gap_polls {
                components.push(COMPONENT_DISPATCH);
            }
        }
        match &tick.ice {
            IcePoll::Fresh(ice) => {
                self.last_ice = ice.clone();
                self.ice_failed_polls = 0;
            }
            IcePoll::Failed => {
                self.ice_failed_polls = self.ice_failed_polls.saturating_add(1);
            }
            IcePoll::NotPolled => {}
        }
        // TODO: only the masked-cell-count arm of the capacity suspension is
        // implemented. The complementary arm — suspend when ALL spawn cells
        // serving the campaign's systems are masked, however few they are —
        // needs a stable mapping from spawn-intent cell names to nixpkgs
        // systems before it can be added; until then a 1–2-cell fleet for a
        // niche system can starve without suspending the clocks.
        //
        // Stale-evidence gate: the sticky snapshot asserts the component
        // only while fresh. An ICE suspension freezes every stall clock,
        // so — like the dispatch predicate's queued guard — it must only
        // persist on state the engine can still observe changing. Without
        // the gate, a latched over-threshold snapshot plus a persistent
        // poll outage would keep the suspension active forever after the
        // cluster recovered, silently disabling stall detection for the
        // rest of the campaign.
        if self.ice_failed_polls < ICE_STALE_AFTER_FAILED_POLLS
            && self.last_ice.ice_masked_cells.len() >= self.knobs.ice_masked_cells_threshold
        {
            components.push(COMPONENT_ICE);
        }
        components
    }

    /// Feed one poll tick. The first tick is a baseline (no time has
    /// elapsed from the watchdog's point of view); every later tick
    /// attributes the seconds since the previous tick either to the active
    /// suspension components (when suspended) or to the per-job stall
    /// clocks (when not).
    pub fn on_tick(&mut self, tick: &PollTick) -> TickOutcome {
        let delta = match self.last_tick_unix {
            Some(prev) => (tick.at_unix - prev).max(0) as f64,
            None => 0.0,
        };
        self.last_tick_unix = Some(tick.at_unix);

        let components = self.evaluate_components(tick);
        let suspended = !components.is_empty();
        let dispatch_pause = components.contains(&COMPONENT_DISPATCH);

        // Suspension-window bookkeeping.
        let mut closed_window = None;
        if suspended {
            for c in &components {
                *self
                    .summary
                    .total_secs_by_component
                    .entry((*c).to_string())
                    .or_default() += delta;
            }
            match &mut self.current_window {
                Some(w) => {
                    for c in &components {
                        if !w.components.iter().any(|x| x == c) {
                            w.components.push((*c).to_string());
                        }
                    }
                }
                None => {
                    tracing::info!(
                        components = ?components,
                        at_unix = tick.at_unix,
                        "suspension window opened: stall clocks frozen"
                    );
                    self.current_window = Some(SuspensionWindow {
                        started_at_unix: tick.at_unix,
                        ended_at_unix: None,
                        components: components.iter().map(|c| (*c).to_string()).collect(),
                    });
                }
            }
        } else if let Some(mut w) = self.current_window.take() {
            w.ended_at_unix = Some(tick.at_unix);
            tracing::info!(
                components = ?w.components,
                duration_secs = tick.at_unix - w.started_at_unix,
                "suspension window closed: stall clocks resume"
            );
            self.summary.windows.push(w.clone());
            closed_window = Some(w);
        }

        // Clocks accrue only while not suspended.
        //
        // Verdict consumption is level-triggered for every arm: emission
        // neither resets a clock nor moves a budget — only the committed
        // ledger transition clears the level (the auto-retry's
        // `observe_job` phase change zeroes the clock, a terminal record's
        // `retire` removes it, and a journaled queued re-enqueue's
        // `confirm_queued_requeue` increments-and-resets). A failed action
        // (journal or results append) thus leaves the clock over its limit
        // and the verdict genuinely re-fires on the next poll tick,
        // instead of being silently forfeited until a full re-accrual.
        let mut stalled = Vec::new();
        if !suspended && delta > 0.0 {
            let active_limit = self.knobs.active_stall_hours * 3600.0;
            let queued_limit = self.knobs.queued_watchdog_hours * 3600.0;
            for (job, clock) in &mut self.jobs {
                clock.accrued_secs += delta;
                match clock.phase {
                    JobPhase::Active if clock.accrued_secs >= active_limit => {
                        stalled.push(StallVerdict {
                            job: job.clone(),
                            kind: StallKind::ActiveStall,
                            requeues_used: clock.requeues,
                        });
                    }
                    JobPhase::Queued if clock.accrued_secs >= queued_limit => {
                        if clock.requeues >= self.knobs.max_queued_requeues {
                            stalled.push(StallVerdict {
                                job: job.clone(),
                                kind: StallKind::QueuedEscalate,
                                requeues_used: clock.requeues,
                            });
                        } else {
                            stalled.push(StallVerdict {
                                job: job.clone(),
                                kind: StallKind::QueuedRequeue,
                                // Post-increment count for the operator log;
                                // the clock itself moves only when the
                                // journaled transition confirms.
                                requeues_used: clock.requeues + 1,
                            });
                        }
                    }
                    _ => {}
                }
            }
        }
        // TODO: refine `Active` to per-drv assigned/running/substituting if a
        // mid-batch per-drv view ever becomes available — in-band per-root
        // results only arrive when a batch settles; today Active means
        // "member of an in-flight batch".
        TickOutcome {
            suspended,
            components,
            stalled,
            dispatch_pause,
            closed_window,
        }
    }

    /// Snapshot for progress.json / the report. Closes nothing: a still-open
    /// window is included with `ended_at_unix: None`.
    pub fn suspension_summary(&self) -> SuspensionSummary {
        let mut summary = self.summary.clone();
        if let Some(open) = &self.current_window {
            summary.windows.push(open.clone());
        }
        summary
    }
}

#[cfg(test)]
mod tests {
    use super::*;

    fn knobs() -> Knobs {
        // Spelled out (rather than relying on the defaults) because the
        // tick arithmetic below depends on these exact values: active 6h vs
        // queued 2h, 3 idle polls, 5 dispatch polls, gap 50, 3 masked cells,
        // 2 queued re-enqueues.
        Knobs {
            active_stall_hours: 6.0,
            queued_watchdog_hours: 2.0,
            idle_polls_for_suspend: 3,
            ice_masked_cells_threshold: 3,
            dispatch_gap_threshold: 50,
            dispatch_gap_polls: 5,
            max_queued_requeues: 2,
            ..Knobs::default()
        }
    }

    fn cluster(queued: u32, running: u32, substituting: u32, executors: u32) -> ClusterCounts {
        ClusterCounts {
            active_executors: executors,
            queued_derivations: queued,
            running_derivations: running,
            substituting_derivations: substituting,
        }
    }

    fn tick(at: i64, c: ClusterCounts) -> PollTick {
        PollTick {
            at_unix: at,
            cluster: Polled::Fresh(c),
            ice: IcePoll::NotPolled,
            engine_paused: false,
        }
    }

    /// An ice snapshot with `n` masked spawn cells.
    fn masked(n: usize) -> IceSnapshot {
        IceSnapshot {
            ice_masked_cells: (0..n).map(|i| format!("cell-{i}:spot")).collect(),
            dead_nodes: vec![],
        }
    }

    #[test]
    fn u_idle_needs_three_consecutive_polls_and_logs_windows() {
        let mut wd = Watchdog::new(knobs());
        // Healthy → not suspended.
        assert!(!wd.on_tick(&tick(0, cluster(10, 5, 0, 8))).suspended);
        // Idle streak builds: 1, 2 → still not suspended; 3rd → suspended.
        assert!(!wd.on_tick(&tick(60, cluster(10, 0, 0, 8))).suspended);
        assert!(!wd.on_tick(&tick(120, cluster(10, 0, 0, 8))).suspended);
        let o = wd.on_tick(&tick(180, cluster(10, 0, 0, 8)));
        assert!(o.suspended);
        assert_eq!(o.components, vec![COMPONENT_IDLE]);
        assert!(!o.dispatch_pause);
        // Recovery closes the window — and the closing tick reports the
        // just-closed window so the run loop never edge-detects.
        let o = wd.on_tick(&tick(240, cluster(10, 4, 0, 8)));
        assert!(!o.suspended);
        let closed = o.closed_window.expect("closing tick exposes the window");
        assert_eq!(closed.started_at_unix, 180);
        assert_eq!(closed.ended_at_unix, Some(240));
        let summary = wd.suspension_summary();
        assert_eq!(summary.windows.len(), 1);
        assert_eq!(summary.windows[0].components, vec![COMPONENT_IDLE]);
        assert_eq!(summary.windows[0].started_at_unix, 180);
        assert_eq!(summary.windows[0].ended_at_unix, Some(240));
        assert!(summary.total_secs_by_component[COMPONENT_IDLE] > 0.0);
    }

    #[test]
    fn u_ice_and_u_dispatch_and_pause_components() {
        let mut wd = Watchdog::new(knobs());
        // ICE: 3 masked cells trip it; the ice snapshot is sticky between
        // (less frequent) spawn-intent polls.
        let mut t = tick(0, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(IceSnapshot {
            ice_masked_cells: vec!["a:spot".into(), "b:spot".into(), "c:od".into()],
            dead_nodes: vec![],
        });
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        // Next tick without a fresh ice snapshot stays suspended (sticky).
        assert_eq!(
            wd.on_tick(&tick(60, cluster(5, 5, 0, 10))).components,
            vec![COMPONENT_ICE]
        );
        // Clearing the mask clears the component.
        let mut t = tick(120, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(IceSnapshot::default());
        assert!(wd.on_tick(&t).components.is_empty());

        // Dispatch gap: needs 5 consecutive polls of gap > 50, then also
        // pauses submission.
        let mut wd = Watchdog::new(knobs());
        for i in 0..4 {
            let o = wd.on_tick(&tick(i * 60, cluster(100, 10, 0, 100)));
            assert!(!o.suspended, "poll {i} not yet");
        }
        let o = wd.on_tick(&tick(240, cluster(100, 10, 0, 100)));
        assert!(o.suspended);
        assert!(o.dispatch_pause);
        assert_eq!(o.components, vec![COMPONENT_DISPATCH]);

        // Engine pause is its own component and suspends immediately.
        let mut wd = Watchdog::new(knobs());
        let mut t = tick(0, cluster(1, 1, 0, 1));
        t.engine_paused = true;
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_PAUSE]);
    }

    #[test]
    fn clocks_accrue_only_while_unsuspended_and_fire_thresholds() {
        let mut wd = Watchdog::new(knobs());
        wd.observe_job("active.x86_64-linux", JobPhase::Active);
        wd.observe_job("queued.x86_64-linux", JobPhase::Queued);

        // t=0: baseline tick (first tick → delta 0, nothing accrues).
        wd.on_tick(&tick(0, cluster(5, 5, 0, 8)));

        // t=0h..10h: ten 1h ticks with the engine paused. The pause
        // component suspends on the SAME tick (no streak warm-up like idle),
        // so none of these 10h accrue to either clock — without the
        // suspension gate the queued job (2h threshold) would have fired
        // several times over.
        for i in 1..=10 {
            let mut t = tick(i * 3600, cluster(5, 5, 0, 8));
            t.engine_paused = true;
            let o = wd.on_tick(&t);
            assert!(o.suspended, "pause suspends immediately");
            assert!(o.stalled.is_empty(), "no stall while suspended");
        }

        // t=10h..11h: first healthy hour after the pause. Both clocks now
        // hold 1h — below the 2h queued and 6h active thresholds → quiet.
        assert!(
            wd.on_tick(&tick(11 * 3600, cluster(5, 5, 0, 8)))
                .stalled
                .is_empty()
        );

        // t=11h..12h: second healthy hour → queued clock reaches 2h → first
        // non-terminal re-enqueue (requeues_used reports the post-commit
        // count; the run loop journals then confirms). Active is at 2h of
        // its 6h.
        let o = wd.on_tick(&tick(12 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(
            o.stalled,
            vec![StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedRequeue,
                requeues_used: 1,
            }]
        );
        wd.confirm_queued_requeue("queued.x86_64-linux");

        // t=12h..14h: two more healthy hours → queued reaches 2h again →
        // second re-enqueue (requeues=2). Active is at 4h of its 6h.
        let o = wd.on_tick(&tick(14 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(
            o.stalled,
            vec![StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedRequeue,
                requeues_used: 2,
            }]
        );
        wd.confirm_queued_requeue("queued.x86_64-linux");

        // t=14h..16h: two more healthy hours. Queued hits 2h with requeues
        // already at max_queued_requeues=2 → escalation. Active hits 6h of
        // accrued unsuspended time on the same tick (1+1+2+2) → active
        // stall. Both fire together.
        let o = wd.on_tick(&tick(16 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(o.stalled.len(), 2, "{:?}", o.stalled);
        assert!(o.stalled.contains(&StallVerdict {
            job: "queued.x86_64-linux".into(),
            kind: StallKind::QueuedEscalate,
            requeues_used: 2,
        }));
        assert!(o.stalled.contains(&StallVerdict {
            job: "active.x86_64-linux".into(),
            kind: StallKind::ActiveStall,
            requeues_used: 0,
        }));

        // Phase change resets the clock; terminal removal stops tracking.
        wd.observe_job("active.x86_64-linux", JobPhase::Queued);
        wd.remove_job("queued.x86_64-linux");
        let o = wd.on_tick(&tick(17 * 3600, cluster(5, 5, 0, 8)));
        assert!(o.stalled.is_empty());
    }

    #[test]
    fn second_suspension_window_accumulates_totals() {
        let mut wd = Watchdog::new(knobs());
        // Window 1: paused for 60s (0→60), recovers at 120.
        wd.on_tick(&tick(0, cluster(5, 5, 0, 8)));
        let mut t = tick(60, cluster(5, 5, 0, 8));
        t.engine_paused = true;
        assert!(wd.on_tick(&t).suspended);
        let mut t = tick(120, cluster(5, 5, 0, 8));
        t.engine_paused = true;
        assert!(wd.on_tick(&t).suspended);
        let o = wd.on_tick(&tick(180, cluster(5, 5, 0, 8)));
        assert!(o.closed_window.is_some(), "first window closes");
        // Window 2: paused again for one 60s tick, recovers at 300.
        let mut t = tick(240, cluster(5, 5, 0, 8));
        t.engine_paused = true;
        assert!(wd.on_tick(&t).suspended);
        let o = wd.on_tick(&tick(300, cluster(5, 5, 0, 8)));
        assert!(o.closed_window.is_some(), "second window closes");

        let summary = wd.suspension_summary();
        assert_eq!(summary.windows.len(), 2);
        assert_eq!(summary.windows[0].started_at_unix, 60);
        assert_eq!(summary.windows[0].ended_at_unix, Some(180));
        assert_eq!(summary.windows[1].started_at_unix, 240);
        assert_eq!(summary.windows[1].ended_at_unix, Some(300));
        // Totals accumulate ACROSS windows: 60s+60s from window 1 (the
        // deltas of its two suspended ticks) plus 60s from window 2.
        assert!(
            (summary.total_secs_by_component[COMPONENT_PAUSE] - 180.0).abs() < 1e-9,
            "{summary:?}"
        );
    }

    #[test]
    fn dispatch_pause_clears_once_the_gap_clears() {
        let mut wd = Watchdog::new(knobs());
        // Build the dispatch-gap streak to its 5-poll threshold.
        for i in 0..5 {
            wd.on_tick(&tick(i * 60, cluster(100, 10, 0, 100)));
        }
        let o = wd.on_tick(&tick(5 * 60, cluster(100, 10, 0, 100)));
        assert!(o.dispatch_pause, "gap sustained → submission paused");
        // One healthy poll (gap below threshold) clears the streak: the
        // pause lifts on the very next tick instead of lingering.
        let o = wd.on_tick(&tick(6 * 60, cluster(100, 90, 0, 100)));
        assert!(!o.dispatch_pause);
        assert!(!o.suspended);
        assert!(o.closed_window.is_some(), "dispatch window closed");
    }

    #[test]
    fn dispatch_streak_needs_queued_work() {
        // A drained cluster — plenty of registered executors, nothing queued,
        // nothing running — is benign idleness, not a dispatch failure. The
        // gap (100 - 0) stays far above the threshold on every poll, but with
        // no queued work the streak must never build, no matter how long the
        // state persists.
        let mut wd = Watchdog::new(knobs());
        for i in 0..8 {
            let o = wd.on_tick(&tick(i * 60, cluster(0, 0, 0, 100)));
            assert!(!o.dispatch_pause, "poll {i}: no queued work, no pause");
            assert!(!o.suspended, "poll {i}: drained cluster is not suspended");
        }
    }

    #[test]
    fn dispatch_pause_lifts_when_the_queue_drains() {
        let mut wd = Watchdog::new(knobs());
        // A legitimate dispatch gap: queued work, idle executors → paused.
        for i in 0..5 {
            wd.on_tick(&tick(i * 60, cluster(100, 10, 0, 100)));
        }
        let o = wd.on_tick(&tick(5 * 60, cluster(100, 10, 0, 100)));
        assert!(o.dispatch_pause, "gap with queued work → submission paused");
        // While paused the engine submits nothing, so once the cluster works
        // off its queue the counts settle at queued=0, running=0 — exactly
        // the state the pause itself produces. The pause must lift here: if
        // the empty queue kept the streak alive, paused submission would keep
        // the queue empty and the campaign would wedge until its deadline.
        let o = wd.on_tick(&tick(6 * 60, cluster(0, 0, 0, 100)));
        assert!(!o.dispatch_pause, "drained queue clears the pause");
        assert!(!o.suspended);
        assert!(o.closed_window.is_some(), "dispatch window closed");
    }

    #[test]
    fn suspension_components_match_documented_predicates() {
        // Truth table over (queued × running × substituting × executors),
        // checked against the documented predicate sentences (module doc):
        //   idle:     queued derivations but nothing occupied (running or
        //             substituting)
        //   dispatch: executors sit idle WHILE WORK IS QUEUED AND
        //             SUBSTITUTION IS QUIESCENT (gap above threshold),
        //             sustained over consecutive polls
        // Each combination is held steady long enough to saturate both
        // streaks (idle needs 3 polls, dispatch 5), then the saturated
        // tick's components must match the sentences exactly.
        //
        // Axis completeness: every ClusterCounts field is an iterated axis
        // of this table. The destructuring below has no `..` rest pattern,
        // so adding a field to ClusterCounts fails compilation HERE — the
        // new axis (and its expected effect on both predicates) must be
        // added to the table explicitly instead of being silently pinned
        // to one fixture value.
        let ClusterCounts {
            active_executors: _,
            queued_derivations: _,
            running_derivations: _,
            substituting_derivations: _,
        } = cluster(0, 0, 0, 0);
        let k = knobs();
        for queued in [0u32, 1, 100] {
            for running in [0u32, 49, 50, 100] {
                for substituting in [0u32, 1, 300] {
                    for executors in [0u32, 50, 51, 100, 151] {
                        let expect_idle = queued > 0 && running + substituting == 0;
                        let expect_dispatch = queued > 0
                            && substituting == 0
                            && i64::from(executors) - i64::from(running) > k.dispatch_gap_threshold;
                        let mut wd = Watchdog::new(k.clone());
                        let mut last = TickOutcome::default();
                        for i in 0..6 {
                            last = wd.on_tick(&tick(
                                i * 60,
                                cluster(queued, running, substituting, executors),
                            ));
                        }
                        let state = format!(
                            "queued={queued} running={running} substituting={substituting} \
                             executors={executors}"
                        );
                        assert_eq!(
                            last.components.contains(&COMPONENT_IDLE),
                            expect_idle,
                            "idle predicate for {state}: {:?}",
                            last.components
                        );
                        assert_eq!(
                            last.components.contains(&COMPONENT_DISPATCH),
                            expect_dispatch,
                            "dispatch predicate for {state}: {:?}",
                            last.components
                        );
                        assert_eq!(
                            last.dispatch_pause, expect_dispatch,
                            "dispatch_pause mirrors the component for {state}"
                        );
                        assert_eq!(
                            last.suspended,
                            expect_idle || expect_dispatch,
                            "suspension is the OR of the active components for {state}"
                        );
                    }
                }
            }
        }
    }

    #[test]
    fn substitution_cascade_is_progress_not_a_dispatch_failure() {
        // The substitution-heavy phase of a leaf campaign: the scheduler
        // has routed the closure to its detached substitution lane —
        // hundreds substituting, nothing running, every executor idle —
        // while the ready queue legitimately holds substitution-lane
        // deferrals that each dispatch pass re-queues. The idle predicate
        // classifies this snapshot as progress (occupancy > 0); the
        // dispatch predicate must agree: however long the cascade runs,
        // the streak must not build.
        let mut wd = Watchdog::new(knobs());
        for i in 0..8 {
            let o = wd.on_tick(&tick(i * 60, cluster(120, 0, 300, 60)));
            assert!(!o.suspended, "poll {i}: cascade is progress, not failure");
            assert!(!o.dispatch_pause, "poll {i}: no submission pause");
        }
        // The cascade ends with work still queued, one build running,
        // executors otherwise idle, and substitution quiescent: now the
        // gap IS evidence of a dispatch failure — and the streak starts
        // from zero, needing its full 5 polls.
        for i in 8..12 {
            let o = wd.on_tick(&tick(i * 60, cluster(120, 1, 0, 60)));
            assert!(!o.dispatch_pause, "poll {i}: streak still building");
        }
        let o = wd.on_tick(&tick(12 * 60, cluster(120, 1, 0, 60)));
        assert!(
            o.dispatch_pause,
            "sustained gap with quiescent substitution is a real dispatch failure"
        );
        assert_eq!(o.components, vec![COMPONENT_DISPATCH]);
    }

    /// ActiveStall and QueuedEscalate are level-triggered: emission does
    /// not reset the clock, so an unconsumed verdict re-fires on the very
    /// next tick — and only the committed ledger transition (observe_job's
    /// phase change for the auto-retry, remove_job for a terminal record)
    /// clears the level. QueuedRequeue stays edge-consumed (its action is
    /// an infallible log line) and must NOT re-fire.
    #[test]
    fn unconsumed_stall_verdicts_re_fire_until_a_transition_commits() {
        let mut wd = Watchdog::new(knobs());
        wd.observe_job("active.x86_64-linux", JobPhase::Active);
        wd.observe_job("queued.x86_64-linux", JobPhase::Queued);
        wd.on_tick(&tick(0, cluster(5, 5, 0, 8)));

        // Drive the queued job to its escalate state (requeues = max 2):
        // two QueuedRequeue crossings at 2h and 4h. The first crossing is
        // deliberately left unconfirmed for one tick: like the other arms,
        // QueuedRequeue is level-triggered and re-fires with the SAME
        // post-commit count until the journaled transition confirms.
        let o = wd.on_tick(&tick(2 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(o.stalled[0].kind, StallKind::QueuedRequeue);
        assert_eq!(o.stalled[0].requeues_used, 1);
        let o = wd.on_tick(&tick(2 * 3600 + 60, cluster(5, 5, 0, 8)));
        assert_eq!(
            o.stalled[0].kind,
            StallKind::QueuedRequeue,
            "unconfirmed QueuedRequeue re-fires next tick"
        );
        assert_eq!(o.stalled[0].requeues_used, 1, "same un-committed step");
        wd.confirm_queued_requeue("queued.x86_64-linux");
        let o = wd.on_tick(&tick(2 * 3600 + 60 + 2 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(o.stalled[0].kind, StallKind::QueuedRequeue);
        assert_eq!(o.stalled[0].requeues_used, 2);
        wd.confirm_queued_requeue("queued.x86_64-linux");
        // Confirmed: one minute later, no re-fire.
        let o = wd.on_tick(&tick(2 * 3600 + 120 + 2 * 3600, cluster(5, 5, 0, 8)));
        assert!(o.stalled.is_empty(), "{:?}", o.stalled);

        // At 6h the active job crosses its threshold and the queued job
        // crosses into escalation. Neither action is applied (a simulated
        // append failure): both verdicts must re-fire next tick.
        let o = wd.on_tick(&tick(6 * 3600 + 60, cluster(5, 5, 0, 8)));
        assert_eq!(o.stalled.len(), 2, "{:?}", o.stalled);
        let o = wd.on_tick(&tick(6 * 3600 + 120, cluster(5, 5, 0, 8)));
        assert!(
            o.stalled.contains(&StallVerdict {
                job: "active.x86_64-linux".into(),
                kind: StallKind::ActiveStall,
                requeues_used: 0,
            }),
            "unconsumed ActiveStall re-fires next tick: {:?}",
            o.stalled
        );
        assert!(
            o.stalled.contains(&StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedEscalate,
                requeues_used: 2,
            }),
            "unconsumed QueuedEscalate re-fires next tick: {:?}",
            o.stalled
        );

        // The committed transitions clear the levels: the auto-retry's
        // Queued observation resets the active job's clock; the terminal
        // record's removal retires the queued job's.
        wd.observe_job("active.x86_64-linux", JobPhase::Queued);
        wd.remove_job("queued.x86_64-linux");
        let o = wd.on_tick(&tick(6 * 3600 + 180, cluster(5, 5, 0, 8)));
        assert!(o.stalled.is_empty(), "{:?}", o.stalled);
    }

    /// One failed-poll tick during an outage: the streak must neither
    /// advance nor reset (a single blip is anti-flap territory, below the
    /// staleness threshold).
    #[test]
    fn poll_outage_preserves_an_in_progress_idle_streak() {
        let mut wd = Watchdog::new(knobs());
        // Two idle polls build the streak to 2 (threshold is 3).
        assert!(!wd.on_tick(&tick(0, cluster(10, 0, 0, 8))).suspended);
        assert!(!wd.on_tick(&tick(60, cluster(10, 0, 0, 8))).suspended);
        // Poll outage: the ClusterStatus RPC failed. The streak must
        // neither advance nor reset.
        let outage = PollTick {
            at_unix: 120,
            cluster: Polled::Failed,
            ice: IcePoll::NotPolled,
            engine_paused: false,
        };
        assert!(!wd.on_tick(&outage).suspended);
        // Next observed idle poll is the third in the streak → suspended.
        // (A reset streak would need two more idle polls to get here.)
        let o = wd.on_tick(&tick(180, cluster(10, 0, 0, 8)));
        assert!(o.suspended, "outage tick preserved the idle streak");
        assert_eq!(o.components, vec![COMPONENT_IDLE]);
    }

    /// The cluster feed's stale-evidence gate, closed-loop: a saturated
    /// idle streak (suspension active, stall clocks frozen) holds through
    /// ONE failed poll, de-asserts at CLUSTER_STALE_AFTER_FAILED_POLLS
    /// consecutive failures (window closes, clocks resume — a persistent
    /// admin-RPC outage no longer disables stall detection), stays
    /// de-asserted while the outage continues, and re-asserts on the FIRST
    /// fresh poll that still matches the predicate (the streak value
    /// survives the lapse — missing evidence is not an observation that
    /// the cluster recovered).
    #[test]
    fn cluster_failed_polls_age_out_idle_and_dispatch_assertions() {
        let mut wd = Watchdog::new(knobs());
        wd.observe_job("queued.x86_64-linux", JobPhase::Queued);
        let failed = |at: i64| PollTick {
            at_unix: at,
            cluster: Polled::Failed,
            ice: IcePoll::NotPolled,
            engine_paused: false,
        };
        // Saturate the idle streak: suspension active.
        wd.on_tick(&tick(0, cluster(10, 0, 0, 8)));
        wd.on_tick(&tick(60, cluster(10, 0, 0, 8)));
        let o = wd.on_tick(&tick(120, cluster(10, 0, 0, 8)));
        assert_eq!(o.components, vec![COMPONENT_IDLE]);

        // First failed poll: prior state holds (no flapping on a blip).
        let o = wd.on_tick(&failed(180));
        assert_eq!(
            o.components,
            vec![COMPONENT_IDLE],
            "one failure holds the prior assertion"
        );
        // Second consecutive failure: the latched streak is stale — the
        // component de-asserts and the suspension window closes.
        let o = wd.on_tick(&failed(240));
        assert!(
            o.components.is_empty(),
            "stale streak stops asserting: {:?}",
            o.components
        );
        assert!(!o.suspended);
        assert!(o.closed_window.is_some(), "idle window closes at staleness");
        // Further failures keep it de-asserted, and the stall clocks now
        // run: 2h of unsuspended queued time later the queued watchdog
        // fires — the outage no longer disables stall detection.
        let o = wd.on_tick(&failed(300));
        assert!(o.components.is_empty());
        let o = wd.on_tick(&failed(300 + 2 * 3600));
        assert_eq!(
            o.stalled,
            vec![StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedRequeue,
                requeues_used: 1,
            }]
        );

        // The first fresh poll that still matches the idle predicate
        // re-asserts immediately — the streak value survived the lapse.
        let o = wd.on_tick(&tick(300 + 2 * 3600 + 60, cluster(10, 0, 0, 8)));
        assert_eq!(
            o.components,
            vec![COMPONENT_IDLE],
            "retained streak re-arms on the first confirming fresh poll"
        );
        // And a fresh poll showing recovery clears the streak entirely.
        let o = wd.on_tick(&tick(300 + 2 * 3600 + 120, cluster(10, 5, 0, 8)));
        assert!(o.components.is_empty());
    }

    /// The dispatch arm of the same gate: a saturated dispatch streak
    /// pauses submission, and a persistent ClusterStatus outage must
    /// release that pause at the staleness threshold instead of wedging
    /// the campaign on evidence the engine can no longer confirm.
    #[test]
    fn cluster_outage_releases_a_latched_dispatch_pause() {
        let mut wd = Watchdog::new(knobs());
        let failed = |at: i64| PollTick {
            at_unix: at,
            cluster: Polled::Failed,
            ice: IcePoll::NotPolled,
            engine_paused: false,
        };
        // Saturate the dispatch streak (5 polls of gap > 50 with queued
        // work and quiescent substitution).
        for i in 0..5 {
            wd.on_tick(&tick(i * 60, cluster(100, 10, 0, 100)));
        }
        let o = wd.on_tick(&tick(5 * 60, cluster(100, 10, 0, 100)));
        assert!(o.dispatch_pause, "gap sustained → submission paused");

        // One failed poll holds the pause; the second releases it.
        let o = wd.on_tick(&failed(6 * 60));
        assert!(o.dispatch_pause, "one failure holds the pause");
        let o = wd.on_tick(&failed(7 * 60));
        assert!(
            !o.dispatch_pause,
            "two consecutive failures release the submission pause"
        );
        assert!(!o.suspended);

        // A fresh poll still showing the gap re-asserts the pause on that
        // very tick (streak retained through the outage).
        let o = wd.on_tick(&tick(8 * 60, cluster(100, 10, 0, 100)));
        assert!(o.dispatch_pause, "confirming fresh poll re-arms the pause");
    }

    #[test]
    fn ice_failed_polls_age_out_the_sticky_snapshot() {
        let mut wd = Watchdog::new(knobs());
        wd.observe_job("queued.x86_64-linux", JobPhase::Queued);
        // t=0: a fresh over-threshold snapshot latches → suspended, stall
        // clocks frozen.
        let mut t = tick(0, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(masked(3));
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        // Not-polled ticks keep it asserted (by-design stickiness).
        assert_eq!(
            wd.on_tick(&tick(300, cluster(5, 5, 0, 10))).components,
            vec![COMPONENT_ICE]
        );
        // First failed poll: the prior state holds — a transient RPC blip
        // must not flap the suspension.
        let mut t = tick(600, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Failed;
        assert_eq!(
            wd.on_tick(&t).components,
            vec![COMPONENT_ICE],
            "one failure holds the prior state"
        );
        // Second consecutive failed poll: the latched evidence is now ~two
        // poll intervals old — stale. The component de-asserts and the
        // window closes; only a successful poll may re-arm it.
        let mut t = tick(900, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Failed;
        let o = wd.on_tick(&t);
        assert!(
            o.components.is_empty(),
            "stale snapshot stops asserting: {:?}",
            o.components
        );
        assert!(!o.suspended);
        assert!(o.closed_window.is_some(), "ice window closed at staleness");
        // Not-polled ticks must NOT re-arm the component from the stale
        // snapshot...
        assert!(
            wd.on_tick(&tick(1200, cluster(5, 5, 0, 10)))
                .components
                .is_empty()
        );
        // ...and further failures keep it de-asserted.
        let mut t = tick(1500, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Failed;
        assert!(wd.on_tick(&t).components.is_empty());
        // The stall clocks resumed when the suspension aged out: 2h of
        // unsuspended queued time later the queued watchdog fires — a
        // persistent poll outage no longer disables stall detection.
        let o = wd.on_tick(&tick(900 + 2 * 3600, cluster(5, 5, 0, 10)));
        assert_eq!(
            o.stalled,
            vec![StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedRequeue,
                requeues_used: 1,
            }]
        );
        // A fresh over-threshold snapshot re-arms the component with
        // confirmable evidence...
        let mut t = tick(900 + 2 * 3600 + 60, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(masked(3));
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        // ...and a fresh clear snapshot clears it.
        let mut t = tick(900 + 2 * 3600 + 120, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(IceSnapshot::default());
        assert!(wd.on_tick(&t).components.is_empty());
    }

    #[test]
    fn ice_poll_failure_streak_resets_on_a_fresh_snapshot() {
        let mut wd = Watchdog::new(knobs());
        let mut t = tick(0, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(masked(3));
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        // One failure (prior state holds), then a success: the failure
        // streak restarts from zero.
        let mut t = tick(300, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Failed;
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        let mut t = tick(600, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Fresh(masked(3));
        assert_eq!(wd.on_tick(&t).components, vec![COMPONENT_ICE]);
        // The next single failure is the FIRST of its streak — the prior
        // state holds instead of compounding with the pre-success failure.
        let mut t = tick(900, cluster(5, 5, 0, 10));
        t.ice = IcePoll::Failed;
        assert_eq!(
            wd.on_tick(&t).components,
            vec![COMPONENT_ICE],
            "failure streak restarted by the successful poll"
        );
    }

    #[test]
    fn suspension_summary_serializes_camel_case_and_fixed_component_names() {
        // The component names are wire vocabulary (progress.json / report):
        // frozen strings, written only via the COMPONENT_* constants.
        assert_eq!(
            [
                COMPONENT_PAUSE,
                COMPONENT_IDLE,
                COMPONENT_ICE,
                COMPONENT_DISPATCH
            ],
            ["pause", "idle", "ice", "dispatch"]
        );

        let mut wd = Watchdog::new(knobs());
        let mut t = tick(0, cluster(1, 1, 0, 1));
        t.engine_paused = true;
        wd.on_tick(&t);
        let summary = wd.suspension_summary();
        assert_eq!(summary.windows.len(), 1, "open window is included");
        assert_eq!(summary.windows[0].ended_at_unix, None);

        let json = serde_json::to_value(&summary).unwrap();
        assert!(json["windows"][0].get("startedAtUnix").is_some(), "{json}");
        assert_eq!(json["windows"][0]["components"][0], COMPONENT_PAUSE);
        assert!(json.get("totalSecsByComponent").is_some(), "{json}");
        let back: SuspensionSummary = serde_json::from_value(json).unwrap();
        assert_eq!(back, summary);
    }
}
