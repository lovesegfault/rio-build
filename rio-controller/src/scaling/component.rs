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
//! max_load   = max(GetLoad over loadEndpoint pods)       [observed]
//!
//! if max_load > high:  desired = current + 1; ratio *= 0.95; low_ticks = 0
//! elif max_load < low: low_ticks += 1; desired = predicted
//!                       if low_ticks >= 30: ratio *= 1.02; low_ticks = 0
//! else:                desired = predicted; low_ticks = 0
//!
//! desired = clamp(desired, min, max)
//! if desired < current && now-lastScaleUp < 5m: desired = current
//! if desired < current: desired = current - 1
//! ```

use std::time::Duration;

use rio_proto::types::GetSizeClassStatusResponse;

use crate::crds::componentscaler::{ComponentScalerSpec, ComponentScalerStatus, LoadThresholds};

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

/// Scale-down stabilization. `desired < current` is held at
/// `current` until this much time has passed since the last
/// scale-UP. Same anti-flap rationale as the BuilderPool autoscaler
/// (`ScalingTiming::scale_down_window`), but shorter (5m vs 10m):
/// store pods are stateless and cold-start in seconds, builders take
/// minutes (FUSE warm).
pub const SCALE_DOWN_STABILIZATION: Duration = Duration::from_secs(300);

/// Max scale-down step per tick. With a 10s tick this is −1 every
/// 10s once past the stabilization window — slow enough that the
/// store PDB (`maxUnavailable: 1`) and SIGTERM grace can drain
/// in-flight PutPath cleanly. I-125a/b made mid-PutPath termination
/// CORRECT; this keeps it CHEAP.
pub const MAX_SCALE_DOWN_STEP: i32 = 1;

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

/// Sum `Σ(class.queued + class.running)` across all size classes.
///
/// `running` is included (not just `queued`): a builder that's
/// running is STILL a store load source (FUSE reads, output
/// PutPath). I-110's batching made the per-builder RPC count
/// roughly constant across the build's lifetime, so queued and
/// running weigh the same.
///
/// Empty classes list (size-classes not configured on this
/// scheduler) → 0. Caller treats 0 builders as "scale to min" —
/// correct for an idle cluster, and a misconfigured one will be
/// caught by the high-load reactive path (`current + 1`) instead.
// r[impl ctrl.scaler.component]
pub fn total_builders(resp: &GetSizeClassStatusResponse) -> u64 {
    resp.classes
        .iter()
        .map(|c| c.queued.saturating_add(c.running))
        .fold(0u64, |a, b| a.saturating_add(b))
}

/// Compute the next replica count + ratio adjustment.
///
/// `current`: the Deployment's `.spec.replicas` as observed (NOT
/// `status.desiredReplicas` — something else may have patched it).
///
/// `builders`: from [`total_builders`].
///
/// `max_load`: `max(GetLoad)` across the target's pods. `None` =
/// the load poll failed (no endpoints resolved, all RPCs errored) —
/// the reactive correction is skipped and only the predictive path
/// runs. The ratio does NOT change on `None` (we have no evidence
/// either way).
///
/// `since_last_scale_up`: `now() - status.lastScaleUpTime`. `None`
/// = never scaled up (first reconcile, or status was wiped) — the
/// scale-down guard treats `None` as "infinitely long ago" (allow
/// scale-down). The alternative (treat as "just now") would mean a
/// fresh CR can never scale down for the first 5 minutes even from
/// an over-provisioned `replicas` chart value.
// r[impl ctrl.scaler.ratio-learn]
pub fn decide(
    spec: &ComponentScalerSpec,
    status: &ComponentScalerStatus,
    current: i32,
    builders: u64,
    max_load: Option<f64>,
    since_last_scale_up: Option<Duration>,
) -> Decision {
    let ratio_in = status
        .learned_ratio
        .unwrap_or(spec.seed_ratio)
        .max(RATIO_FLOOR);
    let LoadThresholds { high, low } = spec.load_thresholds;

    // Predictive: ceil(builders / ratio). f64 ceil is fine here —
    // builders fits in f64's 53-bit mantissa for any realistic count
    // (a u64 > 2^53 builders is not a thing). i32 saturate same as
    // scaling::compute_desired.
    let predicted = ((builders as f64) / ratio_in).ceil();
    let predicted = predicted.min(i32::MAX as f64).max(0.0) as i32;

    // Reactive correction on observed load.
    let (raw_desired, ratio_out, low_ticks) = match max_load {
        Some(l) if l > high => {
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
        Some(l) if l < low => {
            // Over-provisioned (maybe). Count toward ratio growth;
            // use the prediction (which may itself be < current —
            // scale-down guard below handles that).
            let ticks = status.low_load_ticks.saturating_add(1);
            if ticks >= LOW_LOAD_TICKS_FOR_RATIO_GROWTH {
                (predicted, ratio_in * RATIO_GROWTH_ON_LOW, 0)
            } else {
                (predicted, ratio_in, ticks)
            }
        }
        // In-band load OR no load reading: trust the prediction,
        // reset the low streak (in-band) / preserve it (None).
        Some(_) => (predicted, ratio_in, 0),
        None => (predicted, ratio_in, status.low_load_ticks),
    };

    // Clamp. Same defensive min>max swap as scaling::compute_desired
    // (CEL enforces, but a pre-CEL CRD or --validate=false bypass
    // would panic on i32::clamp).
    let (min, max) = if spec.replicas.min > spec.replicas.max {
        (spec.replicas.max, spec.replicas.min)
    } else {
        (spec.replicas.min, spec.replicas.max)
    };
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
            desired = desired.max(current - MAX_SCALE_DOWN_STEP);
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
    use crate::crds::componentscaler::{Replicas, Signal, TargetRef};
    use rio_proto::types::SizeClassStatus;

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

    /// Σ(queued+running) across classes; saturates instead of
    /// wrapping. The saturate matters: a malformed scheduler
    /// response with u64::MAX in one field shouldn't wrap to 0 and
    /// scale the store to min under extreme load.
    // r[verify ctrl.scaler.component]
    #[test]
    fn total_builders_sums_and_saturates() {
        let mk = |q, r| SizeClassStatus {
            name: "x".into(),
            queued: q,
            running: r,
            ..Default::default()
        };
        let resp = GetSizeClassStatusResponse {
            classes: vec![mk(10, 5), mk(0, 7), mk(100, 0)],
            fod_classes: vec![],
        };
        assert_eq!(total_builders(&resp), 122);

        let resp = GetSizeClassStatusResponse {
            classes: vec![mk(u64::MAX, 1)],
            fod_classes: vec![],
        };
        assert_eq!(total_builders(&resp), u64::MAX, "saturates, not wraps");

        assert_eq!(
            total_builders(&GetSizeClassStatusResponse::default()),
            0,
            "empty classes → 0 (size-classes not configured)"
        );
    }

    /// Core predictive path: builders / seedRatio, clamped. No load
    /// reading → no ratio change. This is the "scheduler knows N
    /// builders are about to exist BEFORE they exist" path the plan
    /// design calls predictive.
    // r[verify ctrl.scaler.component]
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

    /// High load: +1 over current AND ratio decays. Asymmetric —
    /// under-provisioning is dangerous (I-105 cascade).
    // r[verify ctrl.scaler.ratio-learn]
    #[test]
    fn high_load_bumps_current_and_decays_ratio() {
        let s = spec(2, 14);
        // current=5, prediction would be 4 (200/50), but load=0.9
        // says we're under-provisioned NOW → +1 over current = 6.
        let d = decide(&s, &status(Some(50.0), 5), 5, 200, Some(0.9), None);
        assert_eq!(d.desired, 6);
        assert_eq!(d.learned_ratio, 50.0 * RATIO_DECAY_ON_HIGH);
        assert_eq!(d.low_load_ticks, 0, "high load resets low streak");
        assert!(d.scaled_up);

        // Ratio floors at RATIO_FLOOR under sustained high.
        let d = decide(&s, &status(Some(1.01), 0), 5, 200, Some(0.9), None);
        assert_eq!(d.learned_ratio, RATIO_FLOOR);
    }

    /// Low load: ratio grows ONLY after LOW_LOAD_TICKS_FOR_RATIO_
    /// GROWTH consecutive low ticks. Slow — over-provisioning is
    /// cheap.
    // r[verify ctrl.scaler.ratio-learn]
    #[test]
    fn low_load_grows_ratio_after_streak() {
        let s = spec(2, 14);
        // Tick 1..29: low_ticks increments, ratio unchanged.
        let d = decide(&s, &status(Some(50.0), 0), 4, 200, Some(0.1), None);
        assert_eq!(d.low_load_ticks, 1);
        assert_eq!(d.learned_ratio, 50.0);

        let d = decide(
            &s,
            &status(Some(50.0), LOW_LOAD_TICKS_FOR_RATIO_GROWTH - 2),
            4,
            200,
            Some(0.1),
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
            Some(0.1),
            None,
        );
        assert_eq!(d.low_load_ticks, 0);
        assert_eq!(d.learned_ratio, 50.0 * RATIO_GROWTH_ON_LOW);

        // In-band load resets the streak.
        let d = decide(&s, &status(Some(50.0), 10), 4, 200, Some(0.5), None);
        assert_eq!(d.low_load_ticks, 0);
        assert_eq!(d.learned_ratio, 50.0);
    }

    /// Scale-down: 5m stabilization since last UP, then max −1/tick.
    /// I-125a/b made mid-PutPath termination correct; this keeps it
    /// cheap.
    // r[verify ctrl.scaler.component]
    #[test]
    fn scale_down_stabilization_and_step() {
        let s = spec(2, 14);
        // Prediction = 4, current = 8. 30s since last up → hold.
        let d = decide(
            &s,
            &status(Some(50.0), 0),
            8,
            200,
            Some(0.5),
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
            Some(0.5),
            Some(Duration::from_secs(360)),
        );
        assert_eq!(d.desired, 7, "max −1/tick");

        // Never scaled up (None) → allow scale-down. Fresh CR at an
        // over-provisioned chart `replicas` shouldn't be stuck for 5m.
        let d = decide(&s, &status(Some(50.0), 0), 8, 200, Some(0.5), None);
        assert_eq!(d.desired, 7);
    }

    /// Exit criterion: controller restart preserves learnedRatio.
    /// Mechanically: `decide` reads `status.learnedRatio` (which the
    /// reconciler reads back from the apiserver), NOT `spec.
    /// seedRatio`, when status is populated.
    // r[verify ctrl.scaler.ratio-learn]
    #[test]
    fn learned_ratio_persists_over_seed() {
        let s = spec(2, 14);
        // status carries 67.3 (learned); seed is 50. 200/67.3 ≈ 3.
        let d = decide(&s, &status(Some(67.3), 0), 2, 200, Some(0.5), None);
        assert_eq!(d.desired, 3);
        // No status (fresh CR) → seed.
        let d = decide(&s, &status(None, 0), 2, 200, Some(0.5), None);
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
            Some(0.5),
            Some(Duration::from_secs(30)),
        );
        assert_eq!(d.desired, 14);

        // High load at max → current+1=15 → clamped to 14.
        let d = decide(&s, &status(Some(50.0), 0), 14, 200, Some(0.9), None);
        assert_eq!(d.desired, 14);
        assert!(!d.scaled_up, "desired==current → not a scale-up event");
        // Ratio still decays (the LOAD signal is real even if we
        // can't add replicas — operator should raise max).
        assert_eq!(d.learned_ratio, 50.0 * RATIO_DECAY_ON_HIGH);
    }
}
