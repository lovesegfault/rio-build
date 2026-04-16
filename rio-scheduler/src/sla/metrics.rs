//! ADR-023 SLA observability. `describe_all()` is wired into
//! `rio_scheduler::describe_metrics()`; the per-event helpers are
//! called inline from `SlaEstimator::refresh` / `explore::next` /
//! `solve::intent_for` so the metric names live in one place.

use metrics::{Unit, counter, describe_counter, describe_gauge, describe_histogram, histogram};

use super::solve::SlaPrediction;

/// Register `# HELP` lines for every `rio_scheduler_sla_*` metric.
/// One call site (lib.rs `describe_metrics`) — keeping the SLA block
/// together means adding a metric is a one-file change.
pub fn describe_all() {
    describe_histogram!(
        "rio_scheduler_sla_prediction_ratio",
        Unit::Count,
        "actual/predicted, by dimension (labeled dim=wall|mem|disk). \
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
        "infeasible-at-any-tier count: solve_mvp returned BestEffort \
         because c* exceeded ceilings on every bounded tier"
    );
    describe_counter!(
        "rio_scheduler_sla_resize_retry_total",
        "OOM/ENOSPC penalty-bump retries (labeled kind=oom|enospc)"
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
        "fleet prior hit clamp: per-tenant fit diverged from the \
         fleet prior by more than the partial-pooling band"
    );
    describe_gauge!(
        "rio_scheduler_sla_hw_cost_stale_seconds",
        Unit::Seconds,
        "age of the hw-band $/vCPU·hr snapshot. Climbs when the \
         lease-gated spot-price poller is failing or this replica is \
         standby (price is PG-backed; standby reads but doesn't write)"
    );
    describe_counter!(
        "rio_scheduler_sla_ice_backoff_total",
        "(band, cap) cells marked ICE-infeasible by the Pending-watch \
         (no heartbeat within hw_fallback_after_secs of first emitting \
         a band-targeted SpawnIntent). 60s TTL; next solve excludes the \
         cell. Labeled `band`, `cap`."
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
/// `record_build_history` call site stays a one-liner and the
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
/// Envelope: `miss` if wall blew the tier's p90 OR memory blew the
/// reservation (`constraint` names which). Memory-miss is the
/// OOM-adjacent case — the build fit because the controller's headroom
/// pad absorbed it, but the model under-predicted.
pub fn score_completion(
    actual_wall: f64,
    actual_mem: u64,
    pred: &SlaPrediction,
) -> CompletionScore {
    let ratio_wall = pred.wall_secs.filter(|p| *p > 0.0).map(|p| actual_wall / p);
    let ratio_mem =
        (actual_mem > 0 && pred.mem_bytes > 0).then(|| actual_mem as f64 / pred.mem_bytes as f64);
    let envelope = pred.tier.as_ref().map(|tier| {
        let wall_miss = pred.tier_p90.is_some_and(|p90| actual_wall > p90);
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

#[cfg(test)]
mod tests {
    use super::*;

    fn pred(wall: f64, mem: u64, tier: &str, p90: f64) -> SlaPrediction {
        SlaPrediction {
            wall_secs: Some(wall),
            mem_bytes: mem,
            tier: Some(tier.into()),
            tier_p90: Some(p90),
        }
    }

    #[test]
    fn ratio_wall_and_mem() {
        // wall=100, predicted=90 → ~1.11
        let s = score_completion(100.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!((s.ratio_wall.unwrap() - 1.111).abs() < 0.01);
        assert!((s.ratio_mem.unwrap() - 0.5).abs() < 0.01);
    }

    #[test]
    fn envelope_hit_within_p90() {
        let s = score_completion(100.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }

    #[test]
    fn envelope_miss_wall() {
        // wall > tier.p90 → result=miss, constraint=wall
        let s = score_completion(1500.0, 1 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "wall")));
    }

    #[test]
    fn envelope_miss_mem() {
        let s = score_completion(100.0, 4 << 30, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert_eq!(s.envelope, Some(("normal".into(), "miss", "mem")));
    }

    #[test]
    fn no_tier_no_envelope() {
        let p = SlaPrediction {
            wall_secs: Some(90.0),
            mem_bytes: 2 << 30,
            tier: None,
            tier_p90: None,
        };
        assert_eq!(score_completion(100.0, 1 << 30, &p).envelope, None);
    }

    #[test]
    fn zero_mem_is_no_signal() {
        let s = score_completion(100.0, 0, &pred(90.0, 2 << 30, "normal", 1200.0));
        assert!(s.ratio_mem.is_none());
        // 0 mem shouldn't trip a mem-miss either.
        assert_eq!(s.envelope, Some(("normal".into(), "hit", "-")));
    }
}
