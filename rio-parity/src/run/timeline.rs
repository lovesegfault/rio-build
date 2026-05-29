//! Timed-replay schedule construction: when each recorded request fires.
//!
//! [`build_schedule`] turns recorded client requests into a paced
//! [`ScheduledRequest`] list — offset-sorted, speedup-scaled, optionally
//! truncated — and arms a per-request disconnect timer for requests whose
//! recorded outcome was an interruption (a cancellation or a client
//! disconnect). [`build_timeout_for`] derives the per-request build deadline
//! from recorded durations, [`lateness_summary`] condenses per-request
//! dispatch lateness into max/p50/p95, and [`re_anchor_pending`] shifts a
//! partially completed schedule at resume time so pending requests fire
//! immediately while keeping their recorded relative spacing.
//!
//! Everything in this module is pure construction over engine-owned input
//! types ([`RecordedRequest`], [`RecordedTiming`]); loading those inputs
//! from a replay archive and dispatching the resulting schedule belong to
//! the run orchestration, not here.

use std::collections::HashSet;
use std::time::Duration;

/// Disconnect delay assumed when a recorded interruption carries no stop
/// offset (scaled by the speedup like a recorded one).
const DEFAULT_DISCONNECT_DELAY_S: f64 = 60.0;

/// Lower bound on the scheduled disconnect delay, so the interrupted build
/// is always actually submitted before the channel is dropped (a high
/// speedup or a tiny recorded gap cannot turn the replay into a no-op).
const DISCONNECT_FLOOR: Duration = Duration::from_secs(1);

/// One recorded client submission, as loaded from the archive at the wiring
/// point.
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedRequest {
    /// Opaque grouping key for the recorded client connection.
    pub session: i64,
    /// Seconds after the recording started at which the request was made.
    pub offset_s: f64,
    /// The derivations (and outputs) the request asked for.
    pub targets: Vec<RecordedTarget>,
}

/// One requested derivation within a [`RecordedRequest`].
#[derive(Debug, Clone, PartialEq)]
pub struct RecordedTarget {
    /// Store path of the requested derivation.
    pub drv: String,
    /// Requested output names; `[]` and `["*"]` both mean every output.
    pub outputs: Vec<String>,
}

/// Per-unit timing/interruption truth needed for scheduling (subset of the
/// archive's expected-outcome record).
#[derive(Debug, Clone, Default, PartialEq)]
pub struct RecordedTiming {
    /// Wall-clock duration of the source attempt, in seconds.
    pub duration_s: Option<f64>,
    /// Seconds after the recording started at which the source attempt
    /// stopped.
    pub stop_offset_s: Option<f64>,
    /// The recorded outcome was an interruption (a cancellation or a client
    /// disconnect) eligible for replay.
    pub interrupted: bool,
}

/// One recorded request, scheduled for replay.
#[derive(Debug, Clone, PartialEq)]
pub struct ScheduledRequest {
    /// Unique per-run id (index into the schedule).
    pub index: usize,
    /// The recorded request being replayed.
    pub request: RecordedRequest,
    /// When to dispatch, relative to the run start: `offset_s / speedup`.
    pub due: Duration,
    /// When interruption replay is armed for this request: how long after
    /// dispatch to abandon the channel
    /// (`(stop_offset_s − offset_s).max(0) / speedup`, or 60 s divided by
    /// the speedup when no stop offset was recorded, never below 1 s);
    /// `None` otherwise.
    pub disconnect_after: Option<Duration>,
    /// The targets whose recorded timing was interrupted — the units the
    /// armed disconnect timer stands in for. Empty whenever interruption
    /// replay is disabled or nothing in the request was interrupted, so a
    /// timer is armed exactly when this list is non-empty.
    pub interruption_drvs: Vec<String>,
}

/// Sort the recorded requests by offset, apply the optional limit, and
/// compute each request's due time and (when interruption replay is
/// enabled) its disconnect timer and interrupted targets.
///
/// `timing` answers per-`(session, drv)` lookups; it is a closure so callers
/// can serve it from whatever expected-outcome index they hold.
pub fn build_schedule(
    requests: &[RecordedRequest],
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    speedup: f64,
    limit: Option<usize>,
    replay_interruptions: bool,
) -> Vec<ScheduledRequest> {
    let mut sorted: Vec<RecordedRequest> = requests.to_vec();
    // Stable sort: requests recorded at the same offset keep their input
    // order.
    sorted.sort_by(|a, b| a.offset_s.total_cmp(&b.offset_s));
    if let Some(limit) = limit {
        sorted.truncate(limit);
    }
    sorted
        .into_iter()
        .enumerate()
        .map(|(index, request)| {
            let offset = request.offset_s.max(0.0);
            let due = Duration::from_secs_f64(offset / speedup);
            let (disconnect_after, interruption_drvs) = if replay_interruptions {
                (
                    disconnect_after_for(&request, timing, speedup),
                    interrupted_targets(&request, timing),
                )
            } else {
                (None, Vec::new())
            };
            ScheduledRequest {
                index,
                request,
                due,
                disconnect_after,
                interruption_drvs,
            }
        })
        .collect()
}

/// Disconnect timer for one request: present only when at least one of its
/// targets has an interrupted recorded timing. The delay is the recorded
/// dispatch-to-stop gap scaled by the speedup ([`DEFAULT_DISCONNECT_DELAY_S`]
/// when the record carries no stop offset), never below
/// [`DISCONNECT_FLOOR`].
fn disconnect_after_for(
    request: &RecordedRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    speedup: f64,
) -> Option<Duration> {
    let offset = request.offset_s.max(0.0);
    let interrupted = request
        .targets
        .iter()
        .find_map(|target| timing(request.session, &target.drv).filter(|t| t.interrupted))?;
    let scaled = match interrupted.stop_offset_s {
        Some(stop) => (stop - offset).max(0.0) / speedup,
        None => DEFAULT_DISCONNECT_DELAY_S / speedup,
    };
    Some(Duration::from_secs_f64(scaled).max(DISCONNECT_FLOOR))
}

/// The request's targets whose recorded timing was an interruption — the
/// units a replayed disconnect stands in for.
fn interrupted_targets(
    request: &RecordedRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
) -> Vec<String> {
    request
        .targets
        .iter()
        .filter(|target| {
            timing(request.session, &target.drv).is_some_and(|record| record.interrupted)
        })
        .map(|target| target.drv.clone())
        .collect()
}

/// Build deadline for one scheduled request: twice the slowest recorded
/// duration among its targets, clamped to `[floor, cap]`; the floor alone
/// when no target carries a recorded duration.
pub fn build_timeout_for(
    scheduled: &ScheduledRequest,
    timing: &dyn Fn(i64, &str) -> Option<RecordedTiming>,
    floor: Duration,
    cap: Duration,
) -> Duration {
    let session = scheduled.request.session;
    let slowest = scheduled
        .request
        .targets
        .iter()
        .filter_map(|target| timing(session, &target.drv).and_then(|record| record.duration_s))
        .fold(None::<f64>, |acc, duration| {
            Some(acc.map_or(duration, |current| current.max(duration)))
        });
    match slowest {
        Some(duration) if duration.is_finite() && duration > 0.0 => {
            // Cap before converting so an absurd recorded duration cannot
            // overflow the conversion; then apply the floor.
            let capped = (2.0 * duration).min(cap.as_secs_f64());
            Duration::from_secs_f64(capped).max(floor)
        }
        _ => floor,
    }
}

/// `"<drv>!out1,out2"` / `"<drv>!*"` formatting for `BuildPathsWithResults`:
/// `[]` and `["*"]` both mean every output.
pub fn format_derived(drv: &str, outputs: &[String]) -> String {
    if outputs.is_empty() || (outputs.len() == 1 && outputs[0] == "*") {
        format!("{drv}!*")
    } else {
        format!("{drv}!{}", outputs.join(","))
    }
}

/// Max / p50 / p95 dispatch lateness over one run, in milliseconds.
///
/// Percentiles are nearest-rank over the sorted samples (the smallest sample
/// with at least the requested share of samples at or below it); an empty
/// slice summarizes to all zeros.
pub fn lateness_summary(lateness_ms: &[u64]) -> (u64, u64, u64) {
    if lateness_ms.is_empty() {
        return (0, 0, 0);
    }
    let mut sorted = lateness_ms.to_vec();
    sorted.sort_unstable();
    let max = *sorted.last().expect("slice checked non-empty above");
    let nearest_rank = |percentile: f64| -> u64 {
        let rank = ((percentile / 100.0) * sorted.len() as f64).ceil() as usize;
        sorted[rank.clamp(1, sorted.len()) - 1]
    };
    (max, nearest_rank(50.0), nearest_rank(95.0))
}

/// Re-anchor a partially completed schedule at resume time.
///
/// Requests whose index is in `already_terminal` keep their original slot;
/// the earliest pending request becomes due at `now_offset` and every other
/// pending request shifts by the same amount, so the recorded relative
/// spacing between pending requests is preserved and no pending request is
/// ever scheduled earlier than `now_offset`.
pub fn re_anchor_pending(
    scheduled: &mut [ScheduledRequest],
    already_terminal: &HashSet<usize>,
    now_offset: Duration,
) {
    let Some(earliest_pending) = scheduled
        .iter()
        .filter(|entry| !already_terminal.contains(&entry.index))
        .map(|entry| entry.due)
        .min()
    else {
        return;
    };
    for entry in scheduled
        .iter_mut()
        .filter(|entry| !already_terminal.contains(&entry.index))
    {
        // Every pending due is >= the earliest pending due by construction;
        // saturate anyway so a malformed slice degrades to "due now" instead
        // of panicking.
        entry.due = now_offset + entry.due.saturating_sub(earliest_pending);
    }
}

#[cfg(test)]
mod tests {
    use std::collections::HashMap;

    use super::*;

    const DRV_A: &str = "/nix/store/a1111111111111111111111111111111-a.drv";
    const DRV_B: &str = "/nix/store/b2222222222222222222222222222222-b.drv";
    const DRV_C: &str = "/nix/store/c3333333333333333333333333333333-c.drv";
    const DRV_D: &str = "/nix/store/d4444444444444444444444444444444-d.drv";

    /// Single-target request for fabricated-schedule cases.
    fn request(session: i64, offset_s: f64, drv: &str) -> RecordedRequest {
        RecordedRequest {
            session,
            offset_s,
            targets: vec![RecordedTarget {
                drv: drv.to_string(),
                outputs: vec!["out".to_string()],
            }],
        }
    }

    /// Timing lookup that knows nothing — no durations, no interruptions.
    fn no_timing(_: i64, _: &str) -> Option<RecordedTiming> {
        None
    }

    /// Timing lookup backed by an explicit `(session, drv)` map.
    fn timing_in(
        map: &HashMap<(i64, String), RecordedTiming>,
    ) -> impl Fn(i64, &str) -> Option<RecordedTiming> + '_ {
        move |session, drv| map.get(&(session, drv.to_string())).cloned()
    }

    /// Recorded timing for an interrupted attempt with an optional stop
    /// offset.
    fn interrupted(stop_offset_s: Option<f64>) -> RecordedTiming {
        RecordedTiming {
            duration_s: None,
            stop_offset_s,
            interrupted: true,
        }
    }

    /// Schedule entry with just an index and a due time, for re-anchoring
    /// cases.
    fn scheduled(index: usize, due_secs: u64) -> ScheduledRequest {
        ScheduledRequest {
            index,
            request: request(index as i64, due_secs as f64, DRV_A),
            due: Duration::from_secs(due_secs),
            disconnect_after: None,
            interruption_drvs: Vec::new(),
        }
    }

    #[test]
    fn build_schedule_sorts_limits_and_scales() {
        // Recorded out of order; offsets 0.25/2.0/5.5/9.0 at speedup 2.0:
        // due times halve, order is by recorded offset, indices follow the
        // schedule order.
        let requests = vec![
            request(11, 5.5, DRV_B),
            request(10, 0.25, DRV_A),
            request(12, 9.0, DRV_C),
            request(13, 2.0, DRV_D),
        ];
        let schedule = build_schedule(&requests, &no_timing, 2.0, None, true);
        assert_eq!(schedule.len(), 4);
        let due: Vec<Duration> = schedule.iter().map(|entry| entry.due).collect();
        assert_eq!(
            due,
            vec![
                Duration::from_millis(125),
                Duration::from_millis(1000),
                Duration::from_millis(2750),
                Duration::from_millis(4500),
            ]
        );
        let sessions: Vec<i64> = schedule.iter().map(|entry| entry.request.session).collect();
        assert_eq!(sessions, vec![10, 13, 11, 12]);
        let indices: Vec<usize> = schedule.iter().map(|entry| entry.index).collect();
        assert_eq!(indices, vec![0, 1, 2, 3]);

        // The limit keeps the first N by offset.
        let limited = build_schedule(&requests, &no_timing, 2.0, Some(2), true);
        assert_eq!(limited.len(), 2);
        let limited_sessions: Vec<i64> =
            limited.iter().map(|entry| entry.request.session).collect();
        assert_eq!(limited_sessions, vec![10, 13]);

        // A fabricated negative offset clamps to a zero due time and sorts
        // ahead of everything else.
        let mut requests = requests.clone();
        requests.push(request(99, -3.5, DRV_A));
        let schedule = build_schedule(&requests, &no_timing, 2.0, None, false);
        assert_eq!(schedule[0].request.session, 99);
        assert_eq!(schedule[0].due, Duration::ZERO);
    }

    #[test]
    fn disconnect_after_uses_stop_offset() {
        // Only session 12's target has an interrupted recorded timing,
        // stopping at offset 11.0 for a request at offset 9.0:
        // (11 - 9) / 2.0 = 1s. Everything else stays unarmed; a
        // non-interrupted timing record never arms a timer.
        let requests = vec![
            request(10, 0.25, DRV_A),
            request(13, 2.0, DRV_D),
            request(11, 5.5, DRV_B),
            request(12, 9.0, DRV_C),
        ];
        let mut map = HashMap::new();
        map.insert((12_i64, DRV_C.to_string()), interrupted(Some(11.0)));
        map.insert(
            (10_i64, DRV_A.to_string()),
            RecordedTiming {
                duration_s: Some(3.0),
                stop_offset_s: None,
                interrupted: false,
            },
        );
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 2.0, None, true);
        let timers: Vec<Option<Duration>> = schedule
            .iter()
            .map(|entry| entry.disconnect_after)
            .collect();
        assert_eq!(timers, vec![None, None, None, Some(Duration::from_secs(1))]);
        // The armed request also names which targets the timer stands in
        // for; unarmed requests name none.
        assert_eq!(schedule[3].interruption_drvs, vec![DRV_C.to_string()]);
        assert!(
            schedule[..3]
                .iter()
                .all(|entry| entry.interruption_drvs.is_empty())
        );

        // Interruption replay disabled: no timers and nothing armed at all.
        let schedule = build_schedule(&requests, &timing, 2.0, None, false);
        assert!(
            schedule
                .iter()
                .all(|entry| entry.disconnect_after.is_none()
                    && entry.interruption_drvs.is_empty())
        );

        // An interruption without a stop offset falls back to 60s scaled by
        // the speedup.
        let requests = vec![request(50, 4.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert((50_i64, DRV_A.to_string()), interrupted(None));
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 2.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(30)));

        // The 1s floor holds when the recorded gap is tiny and the speedup
        // is high.
        let requests = vec![request(51, 5.0, DRV_A)];
        let mut map = HashMap::new();
        map.insert((51_i64, DRV_A.to_string()), interrupted(Some(5.001)));
        let timing = timing_in(&map);
        let schedule = build_schedule(&requests, &timing, 100.0, None, true);
        assert_eq!(schedule[0].disconnect_after, Some(Duration::from_secs(1)));
    }

    #[test]
    fn format_derived_forms() {
        let drv = DRV_A;
        assert_eq!(
            format_derived(drv, &["out".to_string()]),
            format!("{drv}!out")
        );
        assert_eq!(format_derived(drv, &["*".to_string()]), format!("{drv}!*"));
        assert_eq!(format_derived(drv, &[]), format!("{drv}!*"));
        assert_eq!(
            format_derived(drv, &["out".to_string(), "dev".to_string()]),
            format!("{drv}!out,dev")
        );
    }

    #[test]
    fn build_timeout_scales_clamps_and_falls_back() {
        let floor = Duration::from_secs(30 * 60);
        let cap = Duration::from_secs(2 * 60 * 60);
        let session = 7_i64;

        let single = |duration: Option<f64>| {
            let schedule = build_schedule(
                &[request(session, 0.0, DRV_A)],
                &no_timing,
                1.0,
                None,
                false,
            );
            let mut map = HashMap::new();
            map.insert(
                (session, DRV_A.to_string()),
                RecordedTiming {
                    duration_s: duration,
                    stop_offset_s: None,
                    interrupted: false,
                },
            );
            let timing = timing_in(&map);
            build_timeout_for(&schedule[0], &timing, floor, cap)
        };

        // Twice the recorded duration when that lands between the bounds.
        assert_eq!(single(Some(1800.0)), Duration::from_secs(3600));
        // Short recorded builds clamp up to the floor…
        assert_eq!(single(Some(4.2)), floor);
        // …massive ones clamp down to the cap…
        assert_eq!(single(Some(100_000.0)), cap);
        // …and a record without a duration falls back to the floor,
        assert_eq!(single(None), floor);
        // as does a request with no matching record at all.
        let schedule = build_schedule(
            &[request(session, 0.0, DRV_A)],
            &no_timing,
            1.0,
            None,
            false,
        );
        assert_eq!(
            build_timeout_for(&schedule[0], &no_timing, floor, cap),
            floor
        );

        // Several targets: the slowest recorded duration wins.
        let two_targets = RecordedRequest {
            session,
            offset_s: 0.0,
            targets: vec![
                RecordedTarget {
                    drv: DRV_A.to_string(),
                    outputs: vec!["out".to_string()],
                },
                RecordedTarget {
                    drv: DRV_B.to_string(),
                    outputs: vec!["out".to_string()],
                },
            ],
        };
        let schedule = build_schedule(&[two_targets], &no_timing, 1.0, None, false);
        let mut map = HashMap::new();
        map.insert(
            (session, DRV_A.to_string()),
            RecordedTiming {
                duration_s: Some(900.0),
                stop_offset_s: None,
                interrupted: false,
            },
        );
        map.insert(
            (session, DRV_B.to_string()),
            RecordedTiming {
                duration_s: Some(2000.0),
                stop_offset_s: None,
                interrupted: false,
            },
        );
        let timing = timing_in(&map);
        assert_eq!(
            build_timeout_for(&schedule[0], &timing, floor, cap),
            Duration::from_secs(4000)
        );
    }

    #[test]
    fn lateness_summary_percentiles() {
        // Empty input summarizes to zeros; a single sample is its own max
        // and percentiles.
        assert_eq!(lateness_summary(&[]), (0, 0, 0));
        assert_eq!(lateness_summary(&[42]), (42, 42, 42));

        // Unsorted input is sorted before ranking: nearest-rank p50 of five
        // samples is the 3rd smallest, p95 the 5th.
        assert_eq!(lateness_summary(&[50, 10, 30, 20, 40]), (50, 30, 50));

        // A 100..2000 ladder of twenty samples: p50 is the 10th value, p95
        // the 19th.
        let ladder: Vec<u64> = (1..=20).map(|step| step * 100).collect();
        assert_eq!(lateness_summary(&ladder), (2000, 1000, 1900));
    }

    #[test]
    fn re_anchor_preserves_relative_spacing() {
        // Request 0 is already terminal: it keeps its recorded slot. The
        // earliest pending request fires at the resume offset and the later
        // one keeps its recorded 150s gap behind it.
        let mut schedule = vec![scheduled(0, 100), scheduled(1, 200), scheduled(2, 350)];
        let terminal: HashSet<usize> = [0].into_iter().collect();
        re_anchor_pending(&mut schedule, &terminal, Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(100));
        assert_eq!(schedule[1].due, Duration::from_secs(7));
        assert_eq!(schedule[2].due, Duration::from_secs(157));

        // A late resume (past the earliest pending due) shifts pending
        // requests later, never earlier than the resume offset.
        let mut schedule = vec![scheduled(0, 5), scheduled(1, 10)];
        re_anchor_pending(&mut schedule, &HashSet::new(), Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(7));
        assert_eq!(schedule[1].due, Duration::from_secs(12));

        // Nothing pending: nothing moves.
        let mut schedule = vec![scheduled(0, 100)];
        let terminal: HashSet<usize> = [0].into_iter().collect();
        re_anchor_pending(&mut schedule, &terminal, Duration::from_secs(7));
        assert_eq!(schedule[0].due, Duration::from_secs(100));
    }
}
