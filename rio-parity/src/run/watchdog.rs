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
//!   running or substituting for several consecutive polls — the scheduler
//!   is not dispatching at all, which is a cluster problem, not a per-job
//!   one.
//! - [`COMPONENT_ICE`]: capacity provisioning is failing broadly (at least
//!   the configured number of spawn cells are masked after capacity errors).
//! - [`COMPONENT_DISPATCH`]: executors sit idle while work is queued (the
//!   active-executors minus running-derivations gap stays above the
//!   threshold for several consecutive polls). The run loop also pauses
//!   submission on this component, so the engine stops piling more work
//!   onto a scheduler that is not dispatching what it already has.
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
//! tracked. Warm-stage batches are deliberately NOT under this watchdog —
//! the warm stage never registers its roots; a wedged warm batch is bounded
//! by the per-batch child timeout (`batch_timeout_hours`) instead, and every
//! warm root receives a terminal disposition as soon as its batch settles.

use std::collections::BTreeMap;

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
/// executors while work is queued, sustained for `dispatch_gap_polls`
/// consecutive polls. Also pauses submission.
pub const COMPONENT_DISPATCH: &str = "dispatch";

/// Engine-side phase of one campaign job, as fed by the run loop.
///
/// `Active` means "member of an in-flight batch"; everything else is
/// `Queued`. Per-drv assigned/running/substituting refinement needs
/// mid-batch build-scoped reads (the batched per-drv status reader) and is
/// deferred — the per-batch child timeout remains the hard backstop either
/// way.
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

/// One observation the run loop feeds the watchdog (typically every
/// `cluster_status_poll_secs`; `ice` is refreshed only every
/// `spawn_intents_poll_secs`, so most ticks carry `None` and the last
/// snapshot stays in effect).
#[derive(Debug, Clone, Default)]
pub struct PollTick {
    pub at_unix: i64,
    /// Latest ClusterStatus counts; `None` when the poll failed — the idle
    /// and dispatch streaks then stay unchanged (a poll outage neither
    /// builds nor clears a streak).
    pub cluster: Option<ClusterCounts>,
    /// Fresh GetSpawnIntents snapshot, when this tick carried one.
    pub ice: Option<IceSnapshot>,
    pub engine_paused: bool,
}

/// What kind of stall the watchdog reports for a job.
#[derive(Debug, Clone, PartialEq, Eq)]
pub enum StallKind {
    /// Active for at least `active_stall_hours` of unsuspended time. The
    /// run loop decides what follows (the single auto-retry, or a terminal
    /// infrastructure record once that budget is spent) — the watchdog only
    /// reports and resets the clock.
    ActiveStall,
    /// Queued for at least `queued_watchdog_hours` of unsuspended time with
    /// re-enqueue budget remaining: non-terminal re-enqueue (the clock
    /// resets and the job's requeue count increments).
    QueuedRequeue,
    /// Queued past the threshold again after `max_queued_requeues`
    /// re-enqueues were already spent: the run loop records a terminal
    /// infrastructure outcome instead of re-enqueueing forever.
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
    idle_streak: u32,
    dispatch_streak: u32,
    /// Last seen capacity snapshot — sticky between the (less frequent)
    /// spawn-intent polls.
    last_ice: IceSnapshot,
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
            idle_streak: 0,
            dispatch_streak: 0,
            last_ice: IceSnapshot::default(),
            last_tick_unix: None,
            current_window: None,
            summary: SuspensionSummary::default(),
        }
    }

    /// The run loop reports each non-terminal job's phase. A phase change
    /// resets the clock (but keeps the requeue count); terminal jobs are
    /// removed via [`Self::remove_job`].
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
                        requeues: 0,
                    },
                );
            }
        }
    }

    /// Stop tracking a job (it reached a terminal record).
    pub fn remove_job(&mut self, job: &str) {
        self.jobs.remove(job);
    }

    /// Update the streaks from this tick's observations and return the
    /// currently active suspension components.
    fn evaluate_components(&mut self, tick: &PollTick) -> Vec<&'static str> {
        let mut components = Vec::new();
        if tick.engine_paused {
            components.push(COMPONENT_PAUSE);
        }
        if let Some(cluster) = &tick.cluster {
            if cluster.queued_derivations > 0
                && cluster.running_derivations + cluster.substituting_derivations == 0
            {
                self.idle_streak += 1;
            } else {
                self.idle_streak = 0;
            }
            let gap = i64::from(cluster.active_executors) - i64::from(cluster.running_derivations);
            if gap > self.knobs.dispatch_gap_threshold {
                self.dispatch_streak += 1;
            } else {
                self.dispatch_streak = 0;
            }
        }
        if self.idle_streak >= self.knobs.idle_polls_for_suspend {
            components.push(COMPONENT_IDLE);
        }
        if self.dispatch_streak >= self.knobs.dispatch_gap_polls {
            components.push(COMPONENT_DISPATCH);
        }
        if let Some(ice) = &tick.ice {
            self.last_ice = ice.clone();
        }
        // TODO: only the masked-cell-count arm of the capacity suspension is
        // implemented. The complementary arm — suspend when ALL spawn cells
        // serving the campaign's systems are masked, however few they are —
        // needs a stable mapping from spawn-intent cell names to nixpkgs
        // systems before it can be added; until then a 1–2-cell fleet for a
        // niche system can starve without suspending the clocks.
        if self.last_ice.ice_masked_cells.len() >= self.knobs.ice_masked_cells_threshold {
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
                        clock.accrued_secs = 0.0;
                    }
                    JobPhase::Queued if clock.accrued_secs >= queued_limit => {
                        if clock.requeues >= self.knobs.max_queued_requeues {
                            stalled.push(StallVerdict {
                                job: job.clone(),
                                kind: StallKind::QueuedEscalate,
                                requeues_used: clock.requeues,
                            });
                        } else {
                            clock.requeues += 1;
                            stalled.push(StallVerdict {
                                job: job.clone(),
                                kind: StallKind::QueuedRequeue,
                                requeues_used: clock.requeues,
                            });
                        }
                        clock.accrued_secs = 0.0;
                    }
                    _ => {}
                }
            }
        }
        // TODO: refine `Active` to per-drv assigned/running/substituting once
        // a batched per-drv status read allows mid-batch build-scoped reads;
        // today Active means "member of an in-flight batch".
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
            cluster: Some(c),
            ice: None,
            engine_paused: false,
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
        t.ice = Some(IceSnapshot {
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
        t.ice = Some(IceSnapshot::default());
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
        // non-terminal re-enqueue (clock resets, requeues=1). Active is at
        // 2h of its 6h.
        let o = wd.on_tick(&tick(12 * 3600, cluster(5, 5, 0, 8)));
        assert_eq!(
            o.stalled,
            vec![StallVerdict {
                job: "queued.x86_64-linux".into(),
                kind: StallKind::QueuedRequeue,
                requeues_used: 1,
            }]
        );

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
    fn poll_outage_preserves_an_in_progress_idle_streak() {
        let mut wd = Watchdog::new(knobs());
        // Two idle polls build the streak to 2 (threshold is 3).
        assert!(!wd.on_tick(&tick(0, cluster(10, 0, 0, 8))).suspended);
        assert!(!wd.on_tick(&tick(60, cluster(10, 0, 0, 8))).suspended);
        // Poll outage: no cluster counts. The streak must neither advance
        // nor reset.
        let outage = PollTick {
            at_unix: 120,
            cluster: None,
            ice: None,
            engine_paused: false,
        };
        assert!(!wd.on_tick(&outage).suspended);
        // Next observed idle poll is the third in the streak → suspended.
        // (A reset streak would need two more idle polls to get here.)
        let o = wd.on_tick(&tick(180, cluster(10, 0, 0, 8)));
        assert!(o.suspended, "outage tick preserved the idle streak");
        assert_eq!(o.components, vec![COMPONENT_IDLE]);
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
