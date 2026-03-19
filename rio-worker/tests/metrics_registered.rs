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

/// Metric names from observability.md's Worker Metrics table.
/// Keep in sync; the tracey rule `r[obs.metric.worker]` on
/// `describe_metrics()` is the spec link, this is the enforcement.
const WORKER_METRICS: &[&str] = &[
    "rio_worker_builds_total",
    "rio_worker_builds_active",
    "rio_worker_uploads_total",
    "rio_worker_build_duration_seconds",
    "rio_worker_fuse_cache_size_bytes",
    "rio_worker_fuse_cache_hits_total",
    "rio_worker_fuse_cache_misses_total",
    "rio_worker_fuse_fetch_duration_seconds",
    "rio_worker_fuse_fallback_reads_total", // ← the one remediation §2a adds
    "rio_worker_overlay_teardown_failures_total",
    "rio_worker_prefetch_total",
    "rio_worker_upload_bytes_total",
    "rio_worker_fuse_fetch_bytes_total",
    "rio_worker_fuse_circuit_open",
    "rio_worker_cpu_fraction",
    "rio_worker_memory_fraction",
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

// r[verify obs.metric.worker]
#[test]
fn all_spec_metrics_have_describe_call() {
    let recorder = DescribedNames::default();
    let names = recorder.0.clone();

    // with_local_recorder: describe_*! macros route to `recorder` for
    // the duration of the closure only. No global install → no
    // cross-test contamination, no #[serial] needed.
    metrics::with_local_recorder(&recorder, || {
        rio_worker::describe_metrics();
    });

    let described = names.lock().unwrap();
    let missing: Vec<_> = WORKER_METRICS
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
/// See rio-scheduler's `all_emitted_metrics_are_described` for full
/// rationale — this is the same check, different crate.
// r[verify obs.metric.worker]
#[test]
fn all_emitted_metrics_are_described() {
    let emitted: Vec<&str> = EMITTED_METRICS.lines().filter(|l| !l.is_empty()).collect();

    // Precondition self-check: worker has 19 metrics. If the
    // build-script grep returns near-zero, the regex broke.
    assert!(
        emitted.len() >= 15,
        "EMITTED_METRICS has only {} entries — build-script grep likely broke",
        emitted.len()
    );

    let recorder = DescribedNames::default();
    let described = recorder.0.clone();
    metrics::with_local_recorder(&recorder, || {
        rio_worker::describe_metrics();
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
         rio-worker/src/lib.rs::describe_metrics() AND a row to \
         docs/src/observability.md."
    );
}

// Generated by build.rs — grep of `metrics::{counter,gauge,histogram}!`
// literals across src/. One name per line, sorted, deduplicated.
const EMITTED_METRICS: &str = include_str!(concat!(env!("OUT_DIR"), "/emitted_metrics.txt"));
