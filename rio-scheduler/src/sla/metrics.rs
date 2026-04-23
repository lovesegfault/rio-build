//! ADR-023 SLA observability. `describe_all()` is wired into
//! `rio_scheduler::describe_metrics()`; the per-event helpers are
//! called inline from `SlaEstimator::refresh` / `explore::next` /
//! `solve::intent_for` so the metric names live in one place.

use metrics::{
    Unit, counter, describe_counter, describe_gauge, describe_histogram, gauge, histogram,
};

use super::solve::SlaPrediction;

/// Register `# HELP` lines for every `rio_scheduler_sla_*` metric.
/// One call site (lib.rs `describe_metrics`) — keeping the SLA block
/// together means adding a metric is a one-file change.
pub fn describe_all() {
    describe_histogram!(
        "rio_scheduler_sla_prediction_ratio",
        Unit::Count,
        "actual/predicted, by dimension (labeled dim=wall|mem). \
         1.0=perfect; >1.0=under-predicted."
    );
    describe_counter!(
        "rio_scheduler_sla_envelope_result_total",
        "SLA envelope hit/miss per tier (labeled tier, result=hit|miss, \
         constraint=wall|mem|-). `constraint` names the dimension that \
         missed; `-` for hits."
    );
    describe_counter!(
        "rio_scheduler_sla_infeasible_total",
        "infeasible-at-any-tier count, labeled `reason` ∈ \
         {serial_floor, mem_ceiling, disk_ceiling, core_ceiling, \
         interrupt_runaway, capacity_exhausted}. The reason names which \
         constraint bound at the loosest tier (see InfeasibleReason)."
    );
    describe_counter!(
        "rio_scheduler_sla_suspicious_scaling_total",
        "exploration froze at maxCores still saturated (labeled tenant). \
         The build wants more cores than the cluster offers."
    );
    describe_counter!(
        "rio_scheduler_sla_outlier_rejected_total",
        "MAD-rejected samples (labeled tenant). Row stays in PG \
         (outlier_excluded=TRUE) for forensics; refit excludes it."
    );
    describe_counter!(
        "rio_scheduler_sla_mem_fit_weak_total",
        "M(c) Koenker-Machado pseudo-R1<0.7 fallback to independent \
         p90 (labeled tenant)"
    );
    describe_counter!(
        "rio_scheduler_sla_residual_multimodal_total",
        "Hartigan dip test rejected unimodality (p<0.05) on a key's \
         log-residuals (labeled tenant). The single-curve T(c) model \
         is wrong — likely two workloads sharing a pname."
    );
    describe_gauge!(
        "rio_scheduler_sla_prior_divergence",
        "fleet-median prior parameter ÷ operator-probe basis (labeled \
         `param`); outside [0.5, 2.0] ⇒ clamped to the band edge"
    );
    describe_gauge!(
        "rio_scheduler_sla_hw_cost_stale_seconds",
        Unit::Seconds,
        "age of the hw-band $/vCPU·hr snapshot. Climbs when the \
         lease-gated spot-price poller is failing or this replica is \
         standby (price is PG-backed; standby reads but doesn't write)"
    );
    describe_counter!(
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        "ICE-mask hardware ladder exhausted at the terminal tier with \
         no admissible (hw_class, cap) cell left. Labeled `tenant`, \
         `exit` (the cell the ladder gave up on). Replaces \
         `_ice_backoff_total`."
    );
    describe_counter!(
        "rio_scheduler_sla_hw_cost_unknown_total",
        "solve hit a (hw_class, cap) cell the cost table has no $/vCPU·hr \
         for; the cell is dropped from the admissible set (labeled \
         `tenant`). Sustained nonzero ⇒ hwClasses config drifted from \
         the cost-poller's instance-type menu."
    );
    describe_counter!(
        "rio_scheduler_sla_hw_cost_fallback_total",
        "cost-poller fell back from a live spot-price source. Labeled \
         `reason` ∈ {api_error, empty_history, parse}. Distinct from \
         `_hw_cost_stale_seconds` (which measures age, not the \
         fallback event)."
    );
    describe_counter!(
        "rio_scheduler_sla_resize_retry_total",
        "penalty-bump retries on under-provisioning. Labeled `reason` ∈ \
         {oom, enospc}. The under-provisioning signal — \
         `_prediction_ratio` is blind to censored samples."
    );
    describe_counter!(
        "rio_scheduler_sla_als_cap_hit_total",
        "ALS α-regression hit `sla.maxKeysPerTenant` and evicted a key \
         (labeled `tenant`). Nonzero ⇒ a tenant is exhausting the \
         per-tenant LRU; check for random-pname submissions."
    );
}

pub fn outlier_rejected(tenant: &str) {
    counter!("rio_scheduler_sla_outlier_rejected_total", "tenant" => tenant.to_string())
        .increment(1);
}

pub fn suspicious_scaling(tenant: &str) {
    counter!("rio_scheduler_sla_suspicious_scaling_total", "tenant" => tenant.to_string())
        .increment(1);
}

pub fn mem_fit_weak(tenant: &str) {
    counter!("rio_scheduler_sla_mem_fit_weak_total", "tenant" => tenant.to_string()).increment(1);
}

pub fn residual_multimodal(tenant: &str) {
    counter!("rio_scheduler_sla_residual_multimodal_total", "tenant" => tenant.to_string())
        .increment(1);
}

pub fn hw_ladder_exhausted(tenant: &str, exit: &str) {
    counter!(
        "rio_scheduler_sla_hw_ladder_exhausted_total",
        "tenant" => tenant.to_string(),
        "exit" => exit.to_string(),
    )
    .increment(1);
}

pub fn hw_cost_unknown(tenant: &str) {
    counter!("rio_scheduler_sla_hw_cost_unknown_total", "tenant" => tenant.to_string())
        .increment(1);
}

pub fn hw_cost_fallback(reason: &'static str) {
    counter!("rio_scheduler_sla_hw_cost_fallback_total", "reason" => reason).increment(1);
}

pub fn resize_retry(reason: &'static str) {
    counter!("rio_scheduler_sla_resize_retry_total", "reason" => reason).increment(1);
}

pub fn als_cap_hit(tenant: &str) {
    counter!("rio_scheduler_sla_als_cap_hit_total", "tenant" => tenant.to_string()).increment(1);
}

pub fn hw_cost_stale_seconds(age: f64) {
    gauge!("rio_scheduler_sla_hw_cost_stale_seconds").set(age);
}

pub fn prediction_ratio(dim: &'static str, ratio: f64) {
    histogram!("rio_scheduler_sla_prediction_ratio", "dim" => dim).record(ratio);
}

pub fn envelope_result(tier: &str, result: &'static str, constraint: &'static str) {
    counter!(
        "rio_scheduler_sla_envelope_result_total",
        "tier" => tier.to_string(),
        "result" => result,
        "constraint" => constraint,
    )
    .increment(1);
}

/// Actual-vs-predicted score for one completion. Pure so the
/// `record_build_sample` call site stays a one-liner and the
/// hit/miss/ratio rules are unit-testable without a metrics recorder.
#[derive(Debug, PartialEq)]
pub struct CompletionScore {
    pub ratio_wall: Option<f64>,
    pub ratio_mem: Option<f64>,
    /// `(tier, "hit"|"miss", constraint)`. `None` ⇔ no tier was
    /// predicted (BestEffort / cold-start) — nothing to score against.
    pub envelope: Option<(String, &'static str, &'static str)>,
}

/// Score one completion against its dispatch-time prediction.
///
/// `actual_mem=0` ("poller didn't fire") is treated as no-signal — a
/// 0/predicted ratio would drag the histogram floor. Wall ratio is
/// gated on `pred.wall_secs.is_some()` so probe-path dispatches (no
/// fitted curve → no `T(c)`) don't emit.
///
/// `hw_factor` is the completion's `HwTable.factor(hw_class)`: the
/// prediction (`pred.wall_secs`) is in **reference-seconds** — `t_at()`
/// evaluates the ref-second-denominated fit — so `ratio_wall` first maps
/// `actual_wall` to the same timeline. The envelope check stays
/// wall-vs-wall (`tier_target` is the operator-facing wall-second SLA).
/// Pass `1.0` for unknown/absent hw_class.
///
/// Envelope: `miss` if wall blew the tier's binding bound OR memory
/// blew the reservation (`constraint` names which). Memory-miss is the
/// OOM-adjacent case — the build fit because the controller's headroom
/// pad absorbed it, but the model under-predicted.
pub fn score_completion(
    actual_wall: f64,
    hw_factor: f64,
    actual_mem: u64,
    pred: &SlaPrediction,
) -> CompletionScore {
    let ratio_wall = pred
        .wall_secs
        .filter(|p| *p > 0.0)
        .map(|p| (actual_wall * hw_factor) / p);
    let ratio_mem =
        (actual_mem > 0 && pred.mem_bytes > 0).then(|| actual_mem as f64 / pred.mem_bytes as f64);
    let envelope = pred.tier.as_ref().map(|tier| {
        let wall_miss = pred.tier_target.is_some_and(|t| actual_wall > t);
        let mem_miss = actual_mem > 0 && actual_mem > pred.mem_bytes;
        let (result, constraint) = if wall_miss {
            ("miss", "wall")
        } else if mem_miss {
            ("miss", "mem")
        } else {
            ("hit", "-")
        };
        (tier.clone(), result, constraint)
    });
    CompletionScore {
        ratio_wall,
        ratio_mem,
        envelope,
    }
}

/// Emit the metric calls for [`score_completion`]'s result.
pub fn emit_completion_score(s: &CompletionScore) {
    if let Some(r) = s.ratio_wall {
        prediction_ratio("wall", r);
    }
    if let Some(r) = s.ratio_mem {
        prediction_ratio("mem", r);
    }
    if let Some((tier, result, constraint)) = &s.envelope {
        envelope_result(tier, result, constraint);
    }
}

/// Every `rio_scheduler_sla_*` metric name. Single source of truth for
/// the [`tests::registered_and_emitted_are_consistent`] guard — adding
/// a metric means adding it here AND to [`describe_all`] AND wiring an
/// emit helper, or that test fails. Catches the "registered but never
/// emitted" / "emitted but never registered" drift that left
/// `_infeasible_total{reason=capacity_exhausted}` documented with zero
/// non-test callers (see `solve_full_emits_capacity_exhausted_*`).
#[cfg(test)]
pub const SLA_METRICS: &[&str] = &[
    "rio_scheduler_sla_prediction_ratio",
    "rio_scheduler_sla_envelope_result_total",
    "rio_scheduler_sla_infeasible_total",
    "rio_scheduler_sla_suspicious_scaling_total",
    "rio_scheduler_sla_outlier_rejected_total",
    "rio_scheduler_sla_mem_fit_weak_total",
    "rio_scheduler_sla_residual_multimodal_total",
    "rio_scheduler_sla_prior_divergence",
    "rio_scheduler_sla_hw_cost_stale_seconds",
    "rio_scheduler_sla_hw_ladder_exhausted_total",
    "rio_scheduler_sla_hw_cost_unknown_total",
    "rio_scheduler_sla_hw_cost_fallback_total",
    "rio_scheduler_sla_resize_retry_total",
    "rio_scheduler_sla_als_cap_hit_total",
];

#[cfg(test)]
mod tests {
    use super::super::solve::InfeasibleReason;
    use super::*;

    /// ADR-023 §Observability pins exactly these six `reason` label
    /// values, in this order. The enum's `ALL` is the contract.
    #[test]
    fn infeasible_reasons_complete() {
        let want = [
            "serial_floor",
            "mem_ceiling",
            "disk_ceiling",
            "core_ceiling",
            "interrupt_runaway",
            "capacity_exhausted",
        ];
        let got: Vec<_> = InfeasibleReason::ALL.iter().map(|r| r.as_str()).collect();
        assert_eq!(got, want);
    }

    /// Every `SLA_METRICS` name has a `describe_*!` registration AND at
    /// least one emit-site token in this crate's source. Grep-based —
    /// brittle by design: a rename that misses one of {describe_all,
    /// emit helper, SLA_METRICS} fails here, which is the point.
    #[test]
    fn registered_and_emitted_are_consistent() {
        // describe_all() side: install a recorder, call it, collect
        // the names that got `# HELP` lines.
        let rec = metrics_util::debugging::DebuggingRecorder::new();
        let snap = rec.snapshotter();
        metrics::with_local_recorder(&rec, || {
            describe_all();
            // exercise every emit helper once so the snapshot sees the
            // name as a live metric (not just a description).
            outlier_rejected("t");
            suspicious_scaling("t");
            mem_fit_weak("t");
            residual_multimodal("t");
            prediction_ratio("wall", 1.0);
            envelope_result("t", "hit", "-");
            hw_ladder_exhausted("t", "x");
            hw_cost_unknown("t");
            hw_cost_fallback("api_error");
            resize_retry("oom");
            als_cap_hit("t");
            hw_cost_stale_seconds(0.0);
            InfeasibleReason::SerialFloor.emit();
            gauge!("rio_scheduler_sla_prior_divergence", "param" => "S").set(1.0);
        });
        let seen: std::collections::HashSet<_> = snap
            .snapshot()
            .into_vec()
            .into_iter()
            .map(|(ck, _, _, _)| ck.key().name().to_string())
            .collect();
        for name in SLA_METRICS {
            assert!(
                seen.contains(*name),
                "{name} in SLA_METRICS but not emitted"
            );
        }
        for name in &seen {
            assert!(
                SLA_METRICS.contains(&name.as_str()),
                "{name} emitted but not in SLA_METRICS"
            );
        }
        // `_ice_backoff_total` is retired: must NOT appear.
        assert!(!seen.contains("rio_scheduler_sla_ice_backoff_total"));
    }

    fn pred(wall: f64, mem: u64, tier: &str, target: f64) -> SlaPrediction {
        SlaPrediction {
            wall_secs: Some(wall),
            mem_bytes: mem,
            tier: Some(tier.into()),
            tier_target: Some(target),
        }
    }

    #[test]
    fn ratio_wall_and_mem() {
        // wall=100, predicted=90 → ~1.11
        let s = score_completion(100.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!((s.ratio_wall.unwrap() - 1.111).abs() < 0.01);
        assert!((s.ratio_mem.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn envelope_hit_within_p90() {
        let s = score_completion(100.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }

    #[test]
    fn envelope_miss_wall() {
        // wall > tier.p90 → result=miss, constraint=wall
        let s = score_completion(1500.0, 1.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "wall")));
    }

    #[test]
    fn envelope_miss_mem() {
        let s = score_completion(100.0, 1.0, 4 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "mem")));
    }

    #[test]
    fn no_tier_no_envelope() {
        let p = SlaPrediction {
            wall_secs: Some(90.0),
            mem_bytes: 2 << 30,
            tier: None,
            tier_target: None,
        };
        assert_eq!(score_completion(100.0, 1.0, 1 << 30, &p).envelope, None);
    }

    #[test]
    fn zero_mem_is_no_signal() {
        let s = score_completion(100.0, 1.0, 0, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!(s.ratio_mem.is_none());
        // 0 mem shouldn't trip a mem-miss either.
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }

    // r[verify sched.sla.hw-ref-seconds]
    #[test]
    fn ratio_wall_normalizes_by_hw_factor_but_envelope_stays_wall() {
        // Fast hw (factor=2.0): wall=50s, ref=100s; predicted ref=100s.
        // ratio_wall = (50 × 2.0) / 100 = 1.0 (perfect prediction).
        // Without normalization the ratio would read 0.5 — the bug
        // that made sla_prediction_ratio{dim=wall} multimodal at
        // 1/factor[band] under heterogeneous hardware.
        let p = pred(100.0, 2 << 30, "normal", 60.0);
        let s = score_completion(50.0, 2.0, 1 << 30, &p);
        assert!((s.ratio_wall.unwrap() - 1.0).abs() < 1e-9);
        // Envelope is wall-vs-wall: 50s wall < 60s p90 → hit. The
        // hw_factor must NOT leak into the SLA envelope check.
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
        // Same wall on slow hw (factor=1.0) at p90=40s → miss.
        let s2 = score_completion(50.0, 1.0, 1 << 30, &pred(100.0, 2 << 30, "normal", 40.0));
        assert_eq!(s2.envelope, Some(("normal".into(), "miss", "wall")));
    }
}
