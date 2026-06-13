//! ComponentScaler decision logic — pure functions over the
//! [`ComponentScalerSpec`]/[`ComponentScalerStatus`] state machine.
//!
//! The reconciler ([`crate::reconcilers::componentscaler`]) does the
//! IO (poll scheduler, poll store pods, patch `/scale`, write
//! status); this module decides what to patch. Kept side-effect-free
//! so the predict-then-correct loop is unit-testable without a mock
//! apiserver.
//!
//! Loop sketch (10s tick):
//!
//! ```text
//! builders   = Σ(class.queued + class.running)          [predictive]
//! ratio      = status.learnedRatio ?? spec.seedRatio
//! predicted  = ceil(builders / ratio)
//! load       = denominated max(GetLoad) letter           [observed]
//! working    = builders > 0 && current > min       [sensor-free]
//!
//! if load high (any coverage):   desired = current + 1; ratio *= 0.95; ticks = 0
//! elif load low (total cover):   desired = predicted
//!                                 working ? ticks += 1 (at 30: ratio *= 1.02, ticks = 0)
//!                                         : ticks = 0
//! elif load in-band:             desired = predicted; ticks = 0
//! else (absent | partial-low):   desired = predicted
//!                                 working ? ticks -= 1 : ticks = 0   [staleness decay]
//!
//! desired = clamp(desired, min, max)
//! if desired < current && now-lastScaleUp < 5m: desired = current
//! if desired < current: desired = current - 1
//! ```

use std::time::Duration;

use rio_proto::types::ClusterStatusResponse;

use rio_crds::componentscaler::{ComponentScalerSpec, ComponentScalerStatus, LoadThresholds};

/// Consecutive low-load ticks before the learned ratio grows.
/// 30 ticks × 10s = 5 minutes of sustained `< low` before we believe
/// over-provisioning is structural (not just an inter-burst lull).
/// Matches `SCALE_DOWN_STABILIZATION` for symmetry: we don't shrink
/// replicas before 5m of low-want, and we don't loosen the ratio
/// before 5m of low-load.
pub const LOW_LOAD_TICKS_FOR_RATIO_GROWTH: u32 = 30;

/// Learned-ratio decay factor on a high-load tick. 5% per tick is
/// aggressive: high load means the I-105 cliff is approaching and
/// the prediction under-estimated; converge fast.
pub const RATIO_DECAY_ON_HIGH: f64 = 0.95;

/// Learned-ratio growth factor after `LOW_LOAD_TICKS_FOR_RATIO_
/// GROWTH` low ticks. 2% per 5-minute window: over-provisioning is
/// cheap, converge slowly.
pub const RATIO_GROWTH_ON_LOW: f64 = 1.02;

/// Floor for the learned ratio. Prevents `ratio → 0` runaway under
/// a sustained high-load misread (e.g. one stuck pod always
/// reporting 1.0). At ratio=1.0 the predictor would request one
/// replica per builder — clamped by `replicas.max` anyway, but the
/// ratio being floored means it recovers faster once the misread
/// clears.
pub const RATIO_FLOOR: f64 = 1.0;

/// Ceiling for the learned ratio. Symmetric with [`RATIO_FLOOR`]:
/// prevents `ratio → ∞` runaway under sustained idle (`builders=0`
/// misread as "over-provisioned" — see the `builders > 0` gate in
/// [`decide`]). Well above the empirical ~70 (I-110) so it never
/// pinches a real ratio; well below the ~88,000× a 48h idle would
/// produce un-capped, so a pre-fix inflated CR self-heals on first
/// reconcile (applied at the `ratio_in` read site too).
pub const RATIO_CEILING: f64 = 1000.0;

/// Scale-down stabilization. `desired < current` is held at
/// `current` until this much time has passed since the last
/// scale-UP. Anti-flap: store pods are stateless and cold-start in
/// seconds, so 5m is enough to ride out an inter-burst lull.
pub const SCALE_DOWN_STABILIZATION: Duration = Duration::from_secs(300);

/// Max scale-down step per tick. With a 10s tick this is −1 every
/// 10s once past the stabilization window — slow enough that the
/// store PDB (`maxUnavailable: 1`) and SIGTERM grace can drain
/// in-flight PutPath cleanly. I-125a/b made mid-PutPath termination
/// CORRECT; this keeps it CHEAP.
pub const MAX_SCALE_DOWN_STEP: i32 = 1;

/// Staleness cap on banked low-load evidence across sensor-ambiguous
/// ticks. On a WORKING tick whose load letter cannot adjudicate the
/// low streak (sensor absent, or partial-coverage low), the banked
/// streak DECAYS by this many ticks instead of parking bit-exact:
/// preserved evidence survives at most bank-size ambiguous ticks, so
/// the staleness envelope is priced by the two consts alone —
/// ≤ [`LOW_LOAD_TICKS_FOR_RATIO_GROWTH`] ticks (300s) of outage
/// expires any bank to zero, with no new clock or cross-tick state
/// (R17) — while a short poll blip costs exactly its own duration,
/// keeping the genuinely evidence-ambiguous cell preserved.
/// Idle/at-min ticks never reach this: their classification needs no
/// reading, so they RESET regardless of sensor availability.
pub const AMBIGUOUS_TICK_EVIDENCE_DECAY: u32 = 1;

// r[impl ctrl.scaler.load-coverage+2]
/// The denominated load letter: the `max()` fold over per-replica
/// `GetLoad` gauges PLUS its coverage denominator. A max() over
/// per-replica gauges is only a total max under total coverage — the
/// replica whose reading was dropped (timeout, connect failure,
/// readiness-censored DNS) is dropped exactly when its reading may
/// BE the max — so the letter carries `answered`/`resolved` and
/// [`decide`] consumes it asymmetrically: a survivor reading HIGH is
/// trustworthy (scale-up evidence survives partial coverage); a
/// survivor reading LOW is not (ratio-growth funding demands total
/// coverage). `answered == 0` never constructs a letter — the
/// established total-failure posture is `None` at the poll fold
/// ([`LoadAggregate::fold`]).
#[derive(Debug, Clone, Copy, PartialEq)]
pub struct LoadAggregate {
    /// `max(fold_load)` over the replicas that answered.
    pub max: f64,
    /// How many spec'd replicas answered within the RPC timeout.
    pub answered: usize,
    /// The AUTHORITATIVE denominator: `max(addrs.len(),
    /// Deployment.spec.replicas)` — the spec'd replica population
    /// (R31'-d), never the readiness-censored DNS answer alone. The
    /// headless Service has no `publishNotReadyAddresses`, so a
    /// NotReady replica drops out of DNS; sourcing `resolved` from
    /// `addrs.len()` shrank both numerator and denominator together
    /// and read as total coverage exactly when the dropout it prices
    /// was happening (merged_bug_004).
    pub resolved: usize,
}

impl LoadAggregate {
    /// Fold per-replica readings (`None` = timed out / errored) into
    /// the denominated letter. `None` when nothing answered — the
    /// caller's established total-failure posture.
    pub fn fold(readings: &[Option<f64>]) -> Option<Self> {
        let answered = readings.iter().flatten().count();
        let max = readings
            .iter()
            .flatten()
            .copied()
            .fold(None::<f64>, |m, l| Some(m.map_or(l, |m| m.max(l))))?;
        Some(Self {
            max,
            answered,
            resolved: readings.len(),
        })
    }

    /// True iff every resolved replica answered — the only condition
    /// under which `max` is the fleet max rather than a survivor max.
    pub fn total_coverage(&self) -> bool {
        self.answered == self.resolved
    }
}

/// Result of one reconcile decision: the next status to write and
/// the replica count to patch onto `deployments/scale`.
#[derive(Debug, Clone, PartialEq)]
pub struct Decision {
    /// Replica count to patch. Already clamped + scale-down-guarded.
    pub desired: i32,
    /// New `learnedRatio`. May equal the input ratio (no correction
    /// this tick).
    pub learned_ratio: f64,
    /// New `lowLoadTicks` counter.
    pub low_load_ticks: u32,
    /// True iff `desired > current`. Caller stamps `lastScaleUpTime`
    /// = now() on this.
    pub scaled_up: bool,
}

/// Sum queued + running + substituting derivations from `ClusterStatus`.
///
/// queued (Ready-only) and running weigh the same for store load:
/// FUSE input reads + output PutPath happen across the build's
/// lifetime, and I-110's batching made the per-builder RPC count
/// roughly constant from queued through done.
///
/// substituting weighs 1:1 with queued/running: each Substituting
/// derivation drives a store-side `try_substitute` (closure walk +
/// upstream NAR ingest), which is roughly one builder's worth of
/// FUSE-read + PutPath load on the store. A substitution cascade with
/// zero queued/running MUST NOT read as `builders=0` — that scales
/// the store toward `min` exactly when it is the bottleneck.
///
/// 0 → "scale to min" — correct for idle, and a misconfigured cluster
/// is caught by the high-load reactive path (`current + 1`).
///
/// `ClusterStatus`, NOT `GetSpawnIntents`: `Σqueued_by_system ==
/// queued_derivations` by construction (both count the actor's Ready
/// set), so the cheap scalar suffices and the scheduler avoids a
/// per-drv `solve_intent_for` + full intent-vec serialization just to
/// produce a count.
// r[impl ctrl.scaler.component+2]
// r[impl ctrl.scaler.signal-substituting+5]
pub fn total_builders(cs: &ClusterStatusResponse) -> u64 {
    u64::from(cs.queued_derivations)
        .saturating_add(u64::from(cs.running_derivations))
        .saturating_add(u64::from(cs.substituting_derivations))
}

/// Compute the next replica count + ratio adjustment.
///
/// `current`: the Deployment's `.spec.replicas` as observed (NOT
/// `status.desiredReplicas` — something else may have patched it).
///
/// `builders`: from [`total_builders`].
///
/// `load`: the denominated `max(GetLoad)` letter across the target's
/// pods. `None` = the load poll failed entirely (no endpoints
/// resolved, all RPCs errored) — the reactive correction is skipped
/// and only the predictive path runs; the ratio does NOT change (we
/// have no evidence either way). A letter with `answered < resolved`
/// is consumed asymmetrically per the [`LoadAggregate`] law:
/// survivor-high still scales up; survivor-low never funds growth.
///
/// `since_last_scale_up`: the freshest of (in-process record stamped
/// at `patch_scale`'s success site, `status.lastScaleUpTime`) — see
/// [`super::freshest_since_up`]. `None` = never scaled up (first
/// reconcile, or controller restart with status wiped) — the
/// scale-down guard treats `None` as "infinitely long ago" (allow
/// scale-down). The alternative (treat as "just now") would mean a
/// fresh CR can never scale down for the first 5 minutes even from
/// an over-provisioned `replicas` chart value.
// r[impl ctrl.scaler.ratio-learn+2]
pub fn decide(
    spec: &ComponentScalerSpec,
    status: &ComponentScalerStatus,
    current: i32,
    builders: u64,
    load: Option<LoadAggregate>,
    since_last_scale_up: Option<Duration>,
) -> Decision {
    let ratio_in = status
        .learned_ratio
        .unwrap_or(spec.seed_ratio)
        .clamp(RATIO_FLOOR, RATIO_CEILING);
    let LoadThresholds { high, low } = spec.load_thresholds;

    // Effective replica bounds derive ONCE, at the boundary, before
    // ANY reader — quantifier: census(test: w13_ah_swapped_bounds_gate_must_stay_satisfiable) —
    // (parse-don't-validate; bug_052): defensive min>max
    // swap and >=0 floor (CEL enforces both, but a pre-CEL CRD or
    // --validate=false bypass would otherwise panic on i32::clamp /
    // patch a negative /scale -> 422 -> error-loop with no apply-time
    // feedback; bug_027's leaked -2 came from per-site re-flooring).
    // Tolerance is a property of the INPUT, not of one consumer -- a
    // normalization living inside the clamp made the evidence gate
    // (raw `spec.replicas.min`) and the clamp (swapped+floored)
    // disagree on exactly the misconfig population the normalization
    // exists to tolerate: under min>max the gate demanded
    // `current > raw min`, unreachable under the swapped clamp, and
    // ratio learning silently froze (wave-12's relocation of the
    // gate preserved the raw read -- the free hoist missed).
    let (min, max) = if spec.replicas.min > spec.replicas.max {
        (spec.replicas.max, spec.replicas.min)
    } else {
        (spec.replicas.min, spec.replicas.max)
    };
    let (min, max) = (min.max(0), max.max(0));

    // Predictive: ceil(builders / ratio). f64 ceil is fine here —
    // builders fits in f64's 53-bit mantissa for any realistic count
    // (a u64 > 2^53 builders is not a thing).
    let predicted = ((builders as f64) / ratio_in).ceil();
    let predicted = predicted.min(i32::MAX as f64).max(0.0) as i32;

    // The working/idle classification is computable WITHOUT the load
    // reading — hoisted to the observation-alphabet boundary so it
    // evaluates on EVERY tick — quantifier: census(test: w13_ag_idle_none_ticks_must_reset_banked_streak) —
    // observation-absent ones included
    // (merged_bug_009, R34(iii)): with scale-to-zero targets
    // (replicas.min=0 ⇒ zero pods ⇒ zero resolved addrs ⇒ None
    // letter), sensor absence is CORRELATED with the idle regime the
    // gate classifies, so a predicate evaluated only when the poll
    // clock ticks parks banked streaks across exactly the idle
    // windows whose ticks are non-evidence by the gate's own
    // rationale. Low load with zero builders means "nothing to do",
    // NOT "each replica handles more than we thought" — growing on
    // that conflation inflates the ratio unboundedly over an idle
    // weekend (1.02^576 ≈ 88,000×; bug_288).
    let working = builders > 0 && current > min;
    // The genuinely evidence-ambiguous cell (working, low-side
    // unknown — sensor absent or partial-coverage low): the bank is
    // preserved DECAY-BOUNDED ([`AMBIGUOUS_TICK_EVIDENCE_DECAY`], the
    // staleness cap), never bit-exact across unbounded outages. The
    // non-working face of the same letters RESETS — the
    // classification needed no reading.
    let ambiguous_ticks = || {
        if working {
            status
                .low_load_ticks
                .saturating_sub(AMBIGUOUS_TICK_EVIDENCE_DECAY)
        } else {
            0
        }
    };

    // Reactive correction on observed load.
    // r[impl ctrl.scaler.load-coverage+2]
    // The asymmetric consume of the denominated letter: the high arm
    // accepts ANY coverage — quantifier: census(test: w13_af_partial_high_still_scales_up) —
    // (a survivor reading high is a real replica
    // really saturated — scale-up evidence survives partial
    // coverage); the low/funding arm demands TOTAL coverage (the
    // dropped replica's reading may BE the max, so a survivor-only
    // "all low" claim is unsubstantiated and funds nothing). The
    // in-band arm needs no coverage qualifier: one genuinely in-band
    // replica disproves "all replicas low" under any coverage.
    let (raw_desired, ratio_out, low_ticks) = match load {
        Some(l) if l.max > high => {
            // Under-provisioned. +1 over CURRENT (not predicted): if
            // the prediction is what got us here, "predicted + 0"
            // wouldn't help. The ratio decay makes the NEXT
            // prediction larger.
            (
                current.saturating_add(1),
                (ratio_in * RATIO_DECAY_ON_HIGH).max(RATIO_FLOOR),
                0,
            )
        }
        Some(l) if l.max < low && !l.total_coverage() => {
            // Survivor-only LOW: the letter cannot substantiate
            // "every replica is low" (the load-correlated timeout
            // regime recurs every pass — a standing cell, not a
            // blip). Funds NOTHING; the streak takes the hoisted
            // classification: ambiguous-while-working, reset
            // otherwise.
            (predicted, ratio_in, ambiguous_ticks())
        }
        Some(l) if l.max < low => {
            // Over-provisioned — maybe. Count toward ratio growth;
            // use the prediction (which may itself be < current —
            // scale-down guard below handles that).
            // r[impl ctrl.scaler.evidence-funding+2]
            // bug_147 (R29′ — fund == spend): the counter INCREMENTS
            // under exactly the predicate whose sustained truth it
            // witnesses — low-load-while-WORKING (the hoisted
            // `working`). Idle (builders==0) and at-min ticks are
            // NON-EVIDENCE by this gate's own rationale; banking
            // them as redeemable credit was the wrong-clock class on
            // the evidence axis (the parked streak fired growth on
            // the first busy low tick at every idle→busy transition,
            // with zero working evidence — an R30-shaped latch
            // missing its regime-transition exit).
            if working {
                let ticks = status.low_load_ticks.saturating_add(1);
                if ticks >= LOW_LOAD_TICKS_FOR_RATIO_GROWTH {
                    (
                        predicted,
                        (ratio_in * RATIO_GROWTH_ON_LOW).min(RATIO_CEILING),
                        0,
                    )
                } else {
                    (predicted, ratio_in, ticks)
                }
            } else {
                // Non-evidence tick: RESET (the cap died — a reset
                // banks nothing toward the regime boundary).
                (predicted, ratio_in, 0)
            }
        }
        // In-band load: one replica genuinely in-band disproves
        // "all replicas low" — reset under any coverage.
        Some(_) => (predicted, ratio_in, 0),
        // Sensor absent: the hoisted classification still evaluates
        // — idle/at-min resets (merged_bug_009's face), working
        // preserves decay-bounded (the ambiguous cell).
        None => (predicted, ratio_in, ambiguous_ticks()),
    };

    // Clamp to the boundary-normalized bounds (derived once above —
    // every `clamp(min,max)` / `.min(max)` downstream is
    // intrinsically >=0; bug_027).
    let mut desired = raw_desired.clamp(min, max);

    // Scale-down safety: 5m stabilization since last UP, then max
    // −1/tick.
    if desired < current {
        let stabilized = since_last_scale_up
            .map(|d| d >= SCALE_DOWN_STABILIZATION)
            .unwrap_or(true);
        if !stabilized {
            desired = current.clamp(min, max);
        } else {
            // `.min(max)`: `current` is the OBSERVED Deployment
            // replicas, not bounded by [min,max] (operator lowered
            // `replicas.max`, or out-of-band edit). `current-1` may
            // exceed `max`; re-clamp so `Decision.desired`'s
            // "already clamped" contract holds. `saturating_sub`:
            // `current=0` underflow guard (currently unreachable —
            // `desired ≥ 0` by floor above and `desired < current`).
            desired = desired
                .max(current.saturating_sub(MAX_SCALE_DOWN_STEP))
                .min(max);
        }
    }

    Decision {
        desired,
        learned_ratio: ratio_out,
        low_load_ticks: low_ticks,
        scaled_up: desired > current,
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use rio_crds::componentscaler::{Replicas, Signal, TargetRef};

    fn spec(min: i32, max: i32) -> ComponentScalerSpec {
        ComponentScalerSpec {
            target_ref: TargetRef {
                kind: "Deployment".into(),
                name: "rio-store".into(),
            },
            signal: Signal::SchedulerBuilders,
            replicas: Replicas { min, max },
            seed_ratio: 50.0,
            load_endpoint: "rio-store-headless.rio-store:9002".into(),
            load_thresholds: LoadThresholds::default(),
        }
    }

    fn status(ratio: Option<f64>, low_ticks: u32) -> ComponentScalerStatus {
        ComponentScalerStatus {
            learned_ratio: ratio,
            low_load_ticks: low_ticks,
            ..Default::default()
        }
    }

    /// A total-coverage letter: every resolved replica answered, so
    /// `max` is the fleet max (the pre-letter semantics).
    fn total(l: f64) -> Option<LoadAggregate> {
        Some(LoadAggregate {
            max: l,
            answered: 3,
            resolved: 3,
        })
    }

    /// A partial-coverage letter: one resolved replica did not answer
    /// — the load-correlated timeout regime's shape.
    fn partial(l: f64) -> Option<LoadAggregate> {
        Some(LoadAggregate {
            max: l,
            answered: 2,
            resolved: 3,
        })
    }

    /// Σ(queued+running+substituting) from ClusterStatus; saturates
    /// instead of wrapping. The parameter type IS the assertion:
    /// predictive signal sources from the cheap `ClusterStatus`
    /// scalar, not a full `GetSpawnIntents` stream.
    // r[verify ctrl.scaler.component+2]
    #[test]
    fn total_builders_from_cluster_status() {
        let cs = ClusterStatusResponse {
            queued_derivations: 110,
            running_derivations: 12,
            substituting_derivations: 30,
            ..Default::default()
        };
        assert_eq!(total_builders(&cs), 152);

        let cs = ClusterStatusResponse {
            queued_derivations: u32::MAX,
            running_derivations: u32::MAX,
            substituting_derivations: u32::MAX,
            ..Default::default()
        };
        assert_eq!(
            total_builders(&cs),
            3 * u64::from(u32::MAX),
            "u32→u64 widen, no wrap"
        );

        assert_eq!(
            total_builders(&ClusterStatusResponse::default()),
            0,
            "idle cluster → 0"
        );
    }

    /// Substitution cascade with zero queued/running MUST NOT produce
    /// `builders=0` — that scales the store toward `min` exactly when
    /// substitution (store-side closure walk + NAR ingest) is the
    /// bottleneck. Regression: pre-fix the `_ => {}` snapshot match
    /// dropped Substituting → predictive=0 → scale-down during a
    /// fresh-cluster cascade.
    // r[verify ctrl.scaler.signal-substituting+5]
    #[test]
    fn substituting_only_does_not_scale_down() {
        let cs = ClusterStatusResponse {
            queued_derivations: 0,
            running_derivations: 0,
            substituting_derivations: 200,
            ..Default::default()
        };
        assert_eq!(
            total_builders(&cs),
            200,
            "substituting weighs 1:1 with queued/running"
        );

        // decide(): builders=200, current=5, in-band load → predicted
        // = ceil(200/50) = 4. 4 < 5 → scale-down arm; with no last-
        // scale-up record (None) the −1 step applies → desired=4. The
        // load-bearing assertion is `desired >= 4` (predicted), NOT
        // `desired == min`: pre-fix builders=0 → predicted=0 → clamped
        // to min=2 → walked down. Post-fix the predictive signal holds
        // it near current.
        let s = spec(2, 14);
        let d = decide(
            &s,
            &status(None, 0),
            5,
            total_builders(&cs),
            total(0.5),
            None,
        );
        assert_eq!(
            d.desired, 4,
            "substitution cascade must not scale toward min (pre-fix → 2)"
        );
        // And with builders=200 the `builders > 0` low-load growth
        // gate is satisfied, so a sustained low streak DOES grow ratio
        // — substitution counts as "work" for over-provisioning
        // detection too.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            5,
            total_builders(&cs),
            total(0.1),
            None,
        );
        assert_eq!(
            d.learned_ratio,
            50.0 * RATIO_GROWTH_ON_LOW,
            "substituting>0 satisfies the builders>0 idle gate"
        );
    }

    /// Core predictive path: builders / seedRatio, clamped. No load
    /// reading → no ratio change. This is the "scheduler knows N
    /// builders are about to exist BEFORE they exist" path the plan
    /// design calls predictive.
    // r[verify ctrl.scaler.component+2]
    #[test]
    fn predictive_path_no_load() {
        let s = spec(2, 14);
        // 200 builders / seed 50 = 4.
        let d = decide(&s, &status(None, 0), 2, 200, None, None);
        assert_eq!(d.desired, 4);
        assert_eq!(d.learned_ratio, 50.0, "no load reading → ratio unchanged");
        assert!(d.scaled_up);

        // 1 builder → ceil(1/50)=1 → clamped to min=2.
        let d = decide(&s, &status(None, 0), 2, 1, None, None);
        assert_eq!(d.desired, 2);
        assert!(!d.scaled_up);

        // 10000 builders → 200 → clamped to max=14.
        let d = decide(&s, &status(None, 0), 2, 10_000, None, None);
        assert_eq!(d.desired, 14);
    }

    /// Negative replicas (CEL-bypassed via --validate=false or pre-CEL
    /// CRD) must floor at 0. Before the .max(0): {min:-5,max:-2} clamped
    /// a non-negative raw_desired into [-5,-2], walked down −1/tick,
    /// patched replicas=-1 → apiserver 422 → reconciler error-loops.
    // r[verify ctrl.crd.componentscaler]
    #[test]
    fn decide_clamps_negative_replicas_to_zero() {
        let s = spec(-5, -2);
        let d = decide(&s, &status(None, 0), 0, 100, None, None);
        assert!(
            d.desired >= 0,
            "negative replicas range must floor at 0, got {}",
            d.desired
        );
        assert_eq!(d.desired, 0);
        // Scale-down !stabilized branch must also floor: current=3,
        // not-yet-stabilized → would otherwise re-clamp into [-5,-2].
        let d = decide(
            &s,
            &status(None, 0),
            3,
            100,
            None,
            Some(std::time::Duration::from_secs(1)),
        );
        assert!(d.desired >= 0, "!stabilized branch leaked {}", d.desired);
        // bug_027 regression: STABILIZED scale-down branch (≥300s) was
        // the one site missing the floor — `.min(max)` with max=-2
        // dragged desired to -2. Floor-at-bounds chokepoint covers it.
        let d = decide(
            &s,
            &status(None, 0),
            3,
            100,
            None,
            Some(std::time::Duration::from_secs(360)),
        );
        assert!(d.desired >= 0, "stabilized branch leaked {}", d.desired);
        assert_eq!(d.desired, 0);
    }

    /// High load: +1 over current AND ratio decays. Asymmetric —
    /// under-provisioning is dangerous (I-105 cascade).
    // r[verify ctrl.scaler.ratio-learn+2]
    #[test]
    fn high_load_bumps_current_and_decays_ratio() {
        let s = spec(2, 14);
        // current=5, prediction would be 4 (200/50), but load=0.9
        // says we're under-provisioned NOW → +1 over current = 6.
        let d = decide(&s, &status(Some(50.0), 5), 5, 200, total(0.9), None);
        assert_eq!(d.desired, 6);
        assert_eq!(d.learned_ratio, 50.0 * RATIO_DECAY_ON_HIGH);
        assert_eq!(d.low_load_ticks, 0, "high load resets low streak");
        assert!(d.scaled_up);

        // Ratio floors at RATIO_FLOOR under sustained high.
        let d = decide(&s, &status(Some(1.01), 0), 5, 200, total(0.9), None);
        assert_eq!(d.learned_ratio, RATIO_FLOOR);
    }

    /// Low load: ratio grows ONLY after LOW_LOAD_TICKS_FOR_RATIO_
    /// GROWTH consecutive low ticks. Slow — over-provisioning is
    /// cheap.
    // r[verify ctrl.scaler.ratio-learn+2]
    #[test]
    fn low_load_grows_ratio_after_streak() {
        let s = spec(2, 14);
        // Tick 1..29: low_ticks increments, ratio unchanged.
        let d = decide(&s, &status(Some(50.0), 0), 4, 200, total(0.1), None);
        assert_eq!(d.low_load_ticks, 1);
        assert_eq!(d.learned_ratio, 50.0);

        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 2),
            4,
            200,
            total(0.1),
            None,
        );
        assert_eq!(d.low_load_ticks, LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1);
        assert_eq!(d.learned_ratio, 50.0);

        // Tick 30: ratio grows, streak resets.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            total(0.1),
            None,
        );
        assert_eq!(d.low_load_ticks, 0);
        assert_eq!(d.learned_ratio, 50.0 * RATIO_GROWTH_ON_LOW);

        // In-band load resets the streak.
        let d = decide(&s, &status(Some(50.0), 10), 4, 200, total(0.5), None);
        assert_eq!(d.low_load_ticks, 0);
        assert_eq!(d.learned_ratio, 50.0);
    }

    /// Idle (`builders=0`) is NOT evidence of over-provisioning —
    /// ratio must not grow. bug_288 regression: pre-fix, a 48h idle
    /// inflated ratio ~88,000× and the predictor was useless for
    /// hours after load returned.
    // r[verify ctrl.scaler.ratio-learn+2]
    #[test]
    fn low_load_idle_does_not_grow_ratio() {
        let s = spec(2, 14);
        // builders=0, current=min, low load, streak at threshold-1.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            2,
            0,
            total(0.1),
            None,
        );
        assert_eq!(d.learned_ratio, 50.0, "no work → no ratio growth");
        assert_eq!(
            d.low_load_ticks, 0,
            "bug_147: an idle tick is NON-EVIDENCE — it RESETS the \
             streak (the pre-fix cap parked it at threshold, \
             redeemable at the idle→busy boundary)"
        );

        // builders>0 but current==min: also gated — at min there's no
        // over-provisioning to learn from.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            2,
            200,
            total(0.1),
            None,
        );
        assert_eq!(d.learned_ratio, 50.0, "current==min → no ratio growth");
    }

    // r[verify ctrl.scaler.evidence-funding+2]
    /// W12-AR (bug_147) — proposition: growth consumes only
    /// working-low-load evidence (fund == spend); population: the
    /// regime boundaries the cap banked through (every idle→busy
    /// transition; the post-predictive-scale-up second tick is the
    /// same cell one step over).
    ///
    /// The idle→busy transition schedule: an idle period ≥5min parks
    /// the streak AT threshold (the cap), so pre-fix ratio growth
    /// fired on the FIRST busy low tick when current > min — zero
    /// low-load-while-WORKING evidence, violating the documented
    /// 5-minute law and the gate's own idle-is-non-evidence
    /// rationale, recurring on every idle→busy transition.
    #[test]
    fn w12_ar_idle_banked_streak_does_not_fund_growth() {
        let s = spec(2, 14);
        // The idle stretch: builders=0, low load — drive the counter
        // through the production fold to wherever idle leaves it.
        let mut st = status(Some(50.0), 0);
        for _ in 0..40 {
            let d = decide(&s, &st, 2, 0, total(0.0), None);
            assert_eq!(d.learned_ratio, 50.0, "idle never grows");
            st.low_load_ticks = d.low_load_ticks;
        }
        // The transition: work returns (builders>0, current>min),
        // load still low THIS tick (the busy ramp's first sample).
        let d = decide(&s, &st, 4, 200, total(0.1), None);
        assert_eq!(
            d.learned_ratio, 50.0,
            "the first busy low tick carries ZERO working evidence — \
             growth must wait for the full streak rebuilt from the \
             transition (pre-fix: the idle-parked counter fired here)"
        );
        assert_eq!(
            d.low_load_ticks, 1,
            "the streak rebuilds from the transition (idle banked nothing)"
        );
        // The law still fires after 30 TRUE working-low minutes.
        st.low_load_ticks = LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1;
        let d = decide(&s, &st, 4, 200, total(0.1), None);
        assert_eq!(
            d.learned_ratio,
            50.0 * RATIO_GROWTH_ON_LOW,
            "30 consecutive working-low ticks still grow (the spend \
             law unchanged; only the funding predicate tightened)"
        );
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// W13-AF (bug_061) — proposition: a partial aggregate is never
    /// consumed as a total one; population: the load-correlated
    /// timeout regime (the saturated replica is slow to answer
    /// BECAUSE it is saturated, so the timeout recurs every pass —
    /// the recurring face, not a blip). Pre-fix RED (verbatim in the
    /// commit body): the bare `Some(max_of_survivors)` letter made
    /// partial indistinguishable from total — the banked streak
    /// funded growth on survivor-only low evidence (left: 51.0,
    /// right: 50.0) while the saturated replica's high reading
    /// bought no scale-up.
    #[test]
    fn w13_af_partial_low_does_not_fund_growth() {
        let s = spec(2, 14);
        // Survivors read 0.1; one resolved replica never answered.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            partial(0.1),
            None,
        );
        assert_eq!(
            d.learned_ratio, 50.0,
            "survivor-only LOW must not fund growth (the dropped \
             replica's reading may BE the max)"
        );
        assert_eq!(
            d.low_load_ticks,
            LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 2,
            "partial-low while working is the evidence-ambiguous cell \
             — the bank is preserved decay-bounded (one tick consumed \
             per ambiguous tick, the staleness cap), never funded"
        );
        assert_eq!(d.desired, 4, "the predictive path still drives desired");
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// W13-AF, the asymmetric half: a survivor reading HIGH is a real
    /// replica really saturated — scale-up evidence survives partial
    /// coverage (degrading the whole letter to None would suppress
    /// exactly the protective action partial coverage can still
    /// justify).
    #[test]
    fn w13_af_partial_high_still_scales_up() {
        let s = spec(2, 14);
        let d = decide(&s, &status(Some(50.0), 5), 5, 200, partial(0.9), None);
        assert_eq!(
            d.desired, 6,
            "survivor-high scales up under partial coverage"
        );
        assert_eq!(d.learned_ratio, 50.0 * RATIO_DECAY_ON_HIGH);
        assert_eq!(d.low_load_ticks, 0, "high resets the streak (any coverage)");
        assert!(d.scaled_up);
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// W13-AF2 — total-coverage behavior byte-stable: with
    /// `answered == resolved` every arm reproduces the pre-letter
    /// outcomes (the letter is reader-invisible where the old fold
    /// was already sound).
    #[test]
    fn w13_af2_total_coverage_byte_stable() {
        let s = spec(2, 14);
        // High arm.
        let d = decide(&s, &status(Some(50.0), 5), 5, 200, total(0.9), None);
        assert_eq!(
            (d.desired, d.learned_ratio),
            (6, 50.0 * RATIO_DECAY_ON_HIGH)
        );
        // Funding arm at threshold.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            total(0.1),
            None,
        );
        assert_eq!(d.learned_ratio, 50.0 * RATIO_GROWTH_ON_LOW);
        assert_eq!(d.low_load_ticks, 0);
        // In-band arm.
        let d = decide(&s, &status(Some(50.0), 10), 4, 200, total(0.5), None);
        assert_eq!((d.low_load_ticks, d.learned_ratio), (0, 50.0));
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// The fold denominates: answers counted against resolved, max
    /// over answers only, zero answers = the established `None`
    /// total-failure posture (never a `Some` with a fabricated max).
    #[test]
    fn load_aggregate_fold_denominates() {
        let l = LoadAggregate::fold(&[Some(0.1), None, Some(0.7)]).expect("two answers");
        assert_eq!((l.max, l.answered, l.resolved), (0.7, 2, 3));
        assert!(!l.total_coverage());

        let l = LoadAggregate::fold(&[Some(0.2), Some(0.4)]).expect("all answered");
        assert!(l.total_coverage());
        assert_eq!(l.max, 0.4);

        assert_eq!(
            LoadAggregate::fold(&[None, None]),
            None,
            "zero answers → None (total-failure posture preserved)"
        );
        assert_eq!(LoadAggregate::fold(&[]), None, "zero resolved → None");
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// W14-B3 (merged_bug_004) — proposition: the coverage
    /// denominator is the spec'd replica population (the Deployment
    /// `spec.replicas` already in hand), never the readiness-censored
    /// DNS answer; population: the NotReady-replica shape (a
    /// saturated/restarting replica drops out of headless-Service DNS
    /// — the exact dropout the letter exists to price). Pre-fix RED
    /// (verbatim in the commit body): `resolved = addrs.len()` →
    /// answered=1, resolved=1 → total coverage → growth funded
    /// (left: 51.0, right: 50.0) and `load_poll_partial_total`
    /// never increments.
    #[test]
    fn w14_b3_notready_replica_degrades_coverage() {
        // r13-allow(decide-seam): the production-topology shape
        // expressed as the readings vector poll_max_load_addrs hands
        // to fold — DNS resolved 1 addr (the Ready survivor),
        // spec.replicas=2, the NotReady replica's slot padded None.
        let spec_replicas = 2usize;
        let dns_resolved = vec![Some(0.1)]; // survivor reads LOW
        let mut readings = dns_resolved.clone();
        readings.resize(dns_resolved.len().max(spec_replicas), None);
        let l = LoadAggregate::fold(&readings).expect("one answer");
        // Load-bearing: the decide() consequence (survivor-only LOW
        // funds nothing — the asymmetric consume's LOW face) and the
        // suppressed reactive +1.
        let s = spec(2, 14);
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            Some(l),
            None,
        );
        assert_eq!(
            d.learned_ratio, 50.0,
            "a NotReady-censored survivor LOW must NOT fund growth \
             (pre-fix: total_coverage()=true → ratio grew to 51.0)"
        );
        // Secondary mechanism checks: the denominator and the metric
        // trail (!total_coverage → publish_metrics increments
        // load_poll_partial_total — the recurrence's operator
        // visibility).
        assert_eq!(
            (l.answered, l.resolved),
            (1, 2),
            "the NotReady replica is COUNTED in the denominator \
             (pre-fix: resolved=addrs.len()=1)"
        );
        assert!(
            !l.total_coverage(),
            "answered < spec'd replicas → partial coverage \
             (pre-fix: 1/1 read as total)"
        );
    }

    // r[verify ctrl.scaler.load-coverage+2]
    /// W14-B4 — the asymmetry preserved under the re-denomination: a
    /// survivor reading HIGH still scales up under partial coverage
    /// (the protective action survives the NotReady dropout — the
    /// wave-13 W13-AF asymmetric half, re-stated against the spec'd
    /// denominator).
    #[test]
    fn w14_b4_notready_replica_high_survivor_still_scales_up() {
        let spec_replicas = 2usize;
        let mut readings = vec![Some(0.9)]; // survivor reads HIGH
        readings.resize(spec_replicas, None);
        let l = LoadAggregate::fold(&readings).expect("one answer");
        assert!(!l.total_coverage());
        let s = spec(2, 14);
        let d = decide(&s, &status(Some(50.0), 5), 5, 200, Some(l), None);
        assert_eq!(
            d.desired, 6,
            "survivor-HIGH scales up under partial coverage \
             (the asymmetry preserved against the spec'd denominator)"
        );
        assert!(d.scaled_up);
        // Surge face: addrs.len() > spec_replicas → resolved=max(...)
        // counts the surge pods (answered <= resolved invariant).
        let (dns_surge, spec_replicas) = (3usize, 2usize);
        let mut surge = vec![Some(0.5); dns_surge];
        surge.resize(dns_surge.max(spec_replicas), None);
        let l = LoadAggregate::fold(&surge).expect("three answers");
        assert_eq!((l.answered, l.resolved), (3, 3));
        assert!(l.total_coverage(), "surge pods all answered → total");
    }

    // r[verify ctrl.scaler.evidence-funding+2]
    /// W13-AG (merged_bug_009) — proposition: funding predicates
    /// computable without the observation evaluate on
    /// observation-absent ticks; population: the scale-to-zero idle
    /// window (replicas.min=0 ⇒ zero pods ⇒ zero resolved addrs ⇒
    /// None letter — the dominant path the preserve-arm
    /// misclassified, sensor absence CORRELATED with the regime).
    /// Pre-fix RED (verbatim in the commit body): the banked streak
    /// parked across the whole idle window (left: 29, right: 0) and
    /// funded growth on the first busy low tick.
    #[test]
    fn w13_ag_idle_none_ticks_must_reset_banked_streak() {
        let s = spec(0, 14);
        // Bank a 29-streak the honest way: working low-load ticks.
        let mut st = status(Some(50.0), 0);
        for _ in 0..(LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1) {
            let d = decide(&s, &st, 4, 200, total(0.1), None);
            st.low_load_ticks = d.low_load_ticks;
        }
        assert_eq!(st.low_load_ticks, LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1);
        // Scale-to-zero idle window: zero pods => poll resolves zero
        // addrs => None letter, builders=0, current=min=0. The idle
        // classification is computable WITHOUT the reading.
        for _ in 0..60 {
            let d = decide(&s, &st, 0, 0, None, None);
            st.low_load_ticks = d.low_load_ticks;
        }
        assert_eq!(
            st.low_load_ticks, 0,
            "idle/at-min ticks are NON-EVIDENCE regardless of sensor \
             availability — the banked streak must reset on the None \
             ticks (pre-fix: parked across the whole idle window)"
        );
        // Work returns, load still low: growth demands the documented
        // 30 consecutive WORKING ticks rebuilt from the transition —
        // the idle window banked nothing.
        let d = decide(&s, &st, 4, 200, total(0.1), None);
        assert_eq!(
            d.learned_ratio, 50.0,
            "the first busy low tick carries zero working evidence \
             (pre-fix: the idle-parked streak fired growth here)"
        );
        assert_eq!(
            d.low_load_ticks, 1,
            "the streak rebuilds from the transition"
        );
    }

    // r[verify ctrl.scaler.evidence-funding+2]
    /// W13-AG2 — the legitimate preserve face pinned: the genuinely
    /// evidence-ambiguous cell (WORKING, load unknown) keeps the
    /// streak across a transient poll blip at a cost of exactly the
    /// blip's duration, and an unbounded outage EXPIRES the bank
    /// (the staleness cap: ≤ bank-size ambiguous ticks ≤ 300s).
    #[test]
    fn w13_ag2_working_blip_preserved_outage_expires() {
        let s = spec(2, 14);
        // A 10-tick bank, then a 2-tick poll blip while WORKING:
        // the bank survives minus the blip's own duration.
        let mut st = status(Some(50.0), 10);
        for _ in 0..2 {
            let d = decide(&s, &st, 4, 200, None, None);
            st.low_load_ticks = d.low_load_ticks;
        }
        assert_eq!(
            st.low_load_ticks, 8,
            "working + load-unknown keeps the streak decay-bounded \
             (a blip costs its duration, never a reset)"
        );
        // Resume low: the streak continues from the decayed bank.
        let d = decide(&s, &st, 4, 200, total(0.1), None);
        assert_eq!(
            d.low_load_ticks, 9,
            "the ambiguous cell composes with funding"
        );

        // The outage face: a 29-bank under a sensor outage longer
        // than the bank expires to zero — stale evidence is never
        // redeemable after the outage.
        let mut st = status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1);
        for _ in 0..(LOW_LOAD_TICKS_FOR_RATIO_GROWTH) {
            let d = decide(&s, &st, 4, 200, None, None);
            st.low_load_ticks = d.low_load_ticks;
        }
        assert_eq!(
            st.low_load_ticks, 0,
            "an outage ≥ the bank expires it (the R17 staleness \
             envelope: ≤ LOW_LOAD_TICKS_FOR_RATIO_GROWTH ticks)"
        );
        // And the expired bank never under-flows.
        let d = decide(&s, &st, 4, 200, None, None);
        assert_eq!(d.low_load_ticks, 0);
    }

    /// W13-AH (bug_052) — proposition: effective bounds derive ONCE
    /// at the boundary and every reader consumes the normalized
    /// pair; population: the min>max CEL-bypass band, where the
    /// clamp keeps `current` in the swapped range so a raw-min gate
    /// is unsatisfiable at every scaler-reachable count. Pre-fix RED
    /// (verbatim in the commit body): zero evidence ticks across the
    /// band (left: 0, right: 1) — ratio learning silently frozen.
    #[test]
    fn w13_ah_swapped_bounds_gate_must_stay_satisfiable() {
        let s = spec(5, 2); // min>max: effective bounds are [2,5].
        // current=4 is inside the effective band and above the
        // effective min — a working low tick MUST accrue evidence.
        let d = decide(&s, &status(Some(50.0), 0), 4, 200, total(0.1), None);
        assert_eq!(
            d.low_load_ticks, 1,
            "the evidence gate must read the SAME normalized bounds \
             the clamp enforces (raw min=5 makes the gate demand \
             current>5, unreachable under the [2,5] clamp)"
        );
        // The full law still fires across the band: a banked streak
        // at threshold grows the ratio under the normalized gate.
        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            total(0.1),
            None,
        );
        assert_eq!(d.learned_ratio, 50.0 * RATIO_GROWTH_ON_LOW);
    }

    /// W13-AH2 — well-formed specs byte-stable, and the swapped
    /// CLAMP face unchanged (the normalization MOVED to the
    /// boundary; its tolerance semantics did not change).
    #[test]
    fn w13_ah2_normalization_byte_stable() {
        // Well-formed: the gate semantics match the raw read.
        let s = spec(2, 14);
        let d = decide(&s, &status(Some(50.0), 3), 4, 200, total(0.1), None);
        assert_eq!((d.low_load_ticks, d.desired), (4, 4));
        // at-min stays non-working under the normalized min.
        let d = decide(&s, &status(Some(50.0), 3), 2, 200, total(0.1), None);
        assert_eq!(d.low_load_ticks, 0, "current==min: non-evidence reset");
        // Swapped bounds: desired still clamps into the effective
        // [2,5] band exactly as before the hoist.
        let s = spec(5, 2);
        let d = decide(&s, &status(None, 0), 0, 10_000, None, None);
        assert_eq!(d.desired, 5, "clamp face unchanged under min>max");
    }

    /// `RATIO_CEILING` bounds both the read site (a pre-fix inflated
    /// CR self-heals) and the growth site (defense-in-depth).
    // r[verify ctrl.scaler.ratio-learn+2]
    #[test]
    fn ratio_in_clamped_to_ceiling() {
        let s = spec(2, 14);
        // Status carries an inflated ratio (e.g. from a pre-fix
        // controller). predicted = ceil(200/CEILING) = 1 → clamped
        // to min, NOT ceil(200/1e9)=1 — same here, but the ratio_out
        // is what matters: it must be ≤ CEILING so decay starts from
        // a sane value.
        let d = decide(&s, &status(Some(1e9), 0), 2, 200, total(0.5), None);
        assert!(d.learned_ratio <= RATIO_CEILING);
        assert_eq!(
            d.learned_ratio, RATIO_CEILING,
            "in-band → ratio_in passed through, clamped"
        );

        // Growth site also capped.
        let d = decide(
            &s,
            &status(Some(RATIO_CEILING), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 1),
            4,
            200,
            total(0.1),
            None,
        );
        assert_eq!(
            d.learned_ratio, RATIO_CEILING,
            "growth never exceeds ceiling"
        );
    }

    /// Scale-down: 5m stabilization since last UP, then max −1/tick.
    /// I-125a/b made mid-PutPath termination correct; this keeps it
    /// cheap.
    // r[verify ctrl.scaler.component+2]
    #[test]
    fn scale_down_stabilization_and_step() {
        let s = spec(2, 14);
        // Prediction = 4, current = 8. 30s since last up → hold.
        let d = decide(
            &s,
            &status(Some(50.0), 0),
            8,
            200,
            total(0.5),
            Some(Duration::from_secs(30)),
        );
        assert_eq!(d.desired, 8, "within 5m of last scale-up → hold current");
        assert!(!d.scaled_up);

        // 6m since last up → scale down by 1 (not all the way to 4).
        let d = decide(
            &s,
            &status(Some(50.0), 0),
            8,
            200,
            total(0.5),
            Some(Duration::from_secs(360)),
        );
        assert_eq!(d.desired, 7, "max −1/tick");

        // Never scaled up (None) → allow scale-down. Fresh CR at an
        // over-provisioned chart `replicas` shouldn't be stuck for 5m.
        let d = decide(&s, &status(Some(50.0), 0), 8, 200, total(0.5), None);
        assert_eq!(d.desired, 7);
    }

    /// Exit criterion: controller restart preserves learnedRatio.
    /// Mechanically: `decide` reads `status.learnedRatio` (which the
    /// reconciler reads back from the apiserver), NOT `spec.
    /// seedRatio`, when status is populated.
    // r[verify ctrl.scaler.ratio-learn+2]
    #[test]
    fn learned_ratio_persists_over_seed() {
        let s = spec(2, 14);
        // status carries 67.3 (learned); seed is 50. 200/67.3 ≈ 3.
        let d = decide(&s, &status(Some(67.3), 0), 2, 200, total(0.5), None);
        assert_eq!(d.desired, 3);
        // No status (fresh CR) → seed.
        let d = decide(&s, &status(None, 0), 2, 200, total(0.5), None);
        assert_eq!(d.desired, 4);
    }

    /// `current` may be outside `[min, max]` (operator edited
    /// replicas out-of-band, or CR bounds changed). Scale-down hold
    /// still clamps so we don't write an out-of-range desired back.
    #[test]
    fn current_outside_bounds_still_clamps() {
        let s = spec(2, 14);
        // current=20 (>max), recent scale-up → hold current, but
        // clamped to max.
        let d = decide(
            &s,
            &status(Some(50.0), 0),
            20,
            200,
            total(0.5),
            Some(Duration::from_secs(30)),
        );
        assert_eq!(d.desired, 14);

        // current=20 (>max), PAST stabilization → step down by 1, but
        // STILL clamped to max. bug_049 regression: pre-fix this
        // returned 19 (current-1), violating the "already clamped"
        // contract on Decision.desired.
        let d = decide(
            &s,
            &status(Some(50.0), 0),
            20,
            200,
            total(0.5),
            Some(Duration::from_secs(360)),
        );
        assert_eq!(d.desired, 14, "stabilized branch also respects max");

        // High load at max → current+1=15 → clamped to 14.
        let d = decide(&s, &status(Some(50.0), 0), 14, 200, total(0.9), None);
        assert_eq!(d.desired, 14);
        assert!(!d.scaled_up, "desired==current → not a scale-up event");
        // Ratio still decays (the LOAD signal is real even if we
        // can't add replicas — operator should raise max).
        assert_eq!(d.learned_ratio, 50.0 * RATIO_DECAY_ON_HIGH);
    }
}
