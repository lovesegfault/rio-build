//! After `describe_metrics()`, every spec'd metric name has a `describe_*!`
//! call. Catches the "incremented but never described" class
//! (§3 obs-*-unregistered findings: metrics appear in `/metrics` only on
//! first increment, with no `# HELP` line — Grafana tooltips empty,
//! absence alerts can't distinguish "zero" from "not registered").
//!
//! This does NOT catch "described but never incremented in any code path"
//! — that's dead-metric detection, a grep job not a unit test.
//!
//! # Why a custom recorder and not `PrometheusBuilder::build_recorder()`
//!
//! `PrometheusRecorder::render()` only emits `# HELP` for metrics that
//! have been REGISTERED (touched at least once via `counter!()` etc.).
//! `describe_*!` stores the description in a side map but does not
//! register — so `describe_metrics()` alone produces an empty scrape.
//! The custom recorder below intercepts `describe_*` directly.

use std::sync::{Arc, Mutex};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// Metric names from observability.md's Scheduler Metrics table.
/// Keep in sync; the tracey rule `r[obs.metric.scheduler]` on
/// `describe_metrics()` is the spec link, this is the enforcement.
const SCHEDULER_METRICS: &[&str] = &[
    "rio_scheduler_builds_total",
    "rio_scheduler_builds_active",
    "rio_scheduler_derivations_queued",
    "rio_scheduler_derivations_running",
    "rio_scheduler_assignment_latency_seconds",
    "rio_scheduler_build_duration_seconds",
    "rio_scheduler_cache_hits_total",
    "rio_scheduler_cache_check_failures_total",
    "rio_scheduler_queue_backpressure",
    "rio_scheduler_workers_active",
    "rio_scheduler_assignments_total",
    "rio_scheduler_prefetch_hints_sent_total",
    "rio_scheduler_prefetch_paths_sent_total",
    "rio_scheduler_cleanup_dropped_total",
    "rio_scheduler_transition_rejected_total",
    "rio_scheduler_log_lines_forwarded_total",
    "rio_scheduler_log_flush_total",
    "rio_scheduler_log_flush_failures_total",
    "rio_scheduler_log_flush_dropped_total",
    "rio_scheduler_log_forward_dropped_total",
    "rio_scheduler_critical_path_accuracy",
    "rio_scheduler_size_class_assignments_total",
    "rio_scheduler_misclassifications_total",
    "rio_scheduler_class_drift_total",
    "rio_scheduler_cutoff_seconds",
    "rio_scheduler_class_queue_depth",
    "rio_scheduler_cache_check_circuit_open_total",
    "rio_scheduler_event_persist_dropped_total",
    "rio_scheduler_backstop_timeouts_total",
    "rio_scheduler_build_timeouts_total",
    "rio_scheduler_recovery_total",
    "rio_scheduler_recovery_duration_seconds",
    "rio_scheduler_worker_disconnects_total",
    "rio_scheduler_cancel_signals_total",
    "rio_scheduler_lease_acquired_total",
    "rio_scheduler_lease_lost_total",
    "rio_scheduler_estimator_refresh_total",
    "rio_scheduler_build_graph_edges",
];

/// Recorder that captures names passed to `describe_*` and ignores
/// everything else. `register_*` return noop handles — we never
/// touch a metric, only describe.
#[derive(Default)]
struct DescribedNames(Arc<Mutex<Vec<String>>>);

impl Recorder for DescribedNames {
    fn describe_counter(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().push(key.as_str().to_string());
    }
    fn describe_gauge(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().push(key.as_str().to_string());
    }
    fn describe_histogram(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().push(key.as_str().to_string());
    }
    fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
        Counter::noop()
    }
    fn register_gauge(&self, _: &Key, _: &Metadata<'_>) -> Gauge {
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

// r[verify obs.metric.scheduler]
#[test]
fn all_spec_metrics_have_describe_call() {
    let recorder = DescribedNames::default();
    let names = recorder.0.clone();

    // with_local_recorder: describe_*! macros route to `recorder` for
    // the duration of the closure only. No global install → no
    // cross-test contamination, no #[serial] needed.
    metrics::with_local_recorder(&recorder, || {
        rio_scheduler::describe_metrics();
    });

    let described = names.lock().unwrap();
    let missing: Vec<_> = SCHEDULER_METRICS
        .iter()
        .filter(|name| !described.iter().any(|d| d == *name))
        .collect();

    assert!(
        missing.is_empty(),
        "spec'd metrics missing from describe_metrics(): {missing:?}\n\
         \n\
         described:\n{described:#?}"
    );
}

/// Emit-side: every `metrics::{counter,gauge,histogram}!("name")`
/// literal in src/ has a `describe_*!` with the same name.
///
/// The spec-list→describe check above is unidirectional: a metric
/// added to code without touching SCHEDULER_METRICS (forgotten step
/// 3) AND without a `describe_*!` (forgotten step 2) sails through.
/// P0214's `rio_scheduler_build_timeouts_total` at worker.rs:593 did
/// exactly this — emit-only, CI green.
///
/// This check is necessarily a build-time source grep, not a runtime
/// recorder hook: you can't trigger every emit code path from a unit
/// test (most are in handlers gated on actor state). The grep runs at
/// `cargo build` time via build.rs; output consumed here via
/// `include_str!`.
// r[verify obs.metric.scheduler]
#[test]
fn all_emitted_metrics_are_described() {
    let emitted: Vec<&str> = EMITTED_METRICS.lines().filter(|l| !l.is_empty()).collect();

    // Precondition self-check: if the build-script grep returns
    // near-zero names, either src/ has no metrics (implausible — there
    // are 39) or the grep pattern broke (e.g. someone imported the
    // macros unqualified and dropped the `metrics::` prefix that the
    // regex requires). Fail loudly instead of passing vacuously.
    assert!(
        emitted.len() >= 30,
        "EMITTED_METRICS has only {} entries — build-script grep likely broke \
         (check build.rs regex vs. src/ macro call style)",
        emitted.len()
    );

    let recorder = DescribedNames::default();
    let described = recorder.0.clone();
    metrics::with_local_recorder(&recorder, || {
        rio_scheduler::describe_metrics();
    });
    let described = described.lock().unwrap();

    let undescribed: Vec<_> = emitted
        .iter()
        .filter(|name| !described.iter().any(|d| d == **name))
        .collect();

    assert!(
        undescribed.is_empty(),
        "metrics emitted in src/ but NOT in describe_metrics():\n  {undescribed:#?}\n\
         \n\
         Add describe_counter!/describe_gauge!/describe_histogram! to \
         rio-scheduler/src/lib.rs::describe_metrics() AND a row to \
         docs/src/observability.md."
    );
}

// Generated by build.rs — grep of `metrics::{counter,gauge,histogram}!`
// literals across src/. One name per line, sorted, deduplicated.
const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));
