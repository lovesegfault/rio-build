//! ADR-023 SLA observability. `describe_all()` is wired into
//! `rio_scheduler::describe_metrics()`; the per-event helpers are
//! called inline from `SlaEstimator::refresh` / `explore::next` /
//! `solve::intent_for` so the metric names live in one place.

use metrics::{Unit, counter, describe_counter, describe_gauge, describe_histogram};

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
        "SLA envelope hit/miss per tier (labeled tier, result=hit|miss)"
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
    describe_gauge!(
        "rio_scheduler_sla_prior_divergence",
        "fleet prior hit clamp: per-tenant fit diverged from the \
         fleet prior by more than the partial-pooling band"
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
