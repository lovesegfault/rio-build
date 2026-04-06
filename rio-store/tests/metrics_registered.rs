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

/// Metric names from observability.md's Store Metrics table.
/// Keep in sync; the tracey rule `r[obs.metric.store]` on
/// `describe_metrics()` is the spec link, this is the enforcement.
const STORE_METRICS: &[&str] = &[
    "rio_store_put_path_total",
    "rio_store_put_path_duration_seconds",
    "rio_store_put_path_bytes_total",
    "rio_store_get_path_bytes_total",
    "rio_store_integrity_failures_total",
    "rio_store_chunks_total",
    "rio_store_chunk_dedup_ratio",
    "rio_store_s3_requests_total",
    "rio_store_chunk_cache_hits_total",
    "rio_store_chunk_cache_misses_total",
    "rio_store_gc_path_resurrected_total",
    "rio_store_hmac_rejected_total",
    "rio_store_hmac_bypass_total",
    "rio_store_s3_deletes_pending",
    "rio_store_s3_deletes_stuck",
    "rio_store_gc_chunk_resurrected_total",
    "rio_store_gc_path_swept_total",
    "rio_store_gc_s3_key_enqueued_total",
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

// r[verify obs.metric.store]
#[test]
fn all_spec_metrics_have_describe_call() {
    let recorder = DescribedNames::default();
    let names = recorder.0.clone();

    // with_local_recorder: describe_*! macros route to `recorder` for
    // the duration of the closure only. No global install → no
    // cross-test contamination, no #[serial] needed.
    metrics::with_local_recorder(&recorder, || {
        rio_store::describe_metrics();
    });

    let described = names.lock().unwrap();
    let missing: Vec<_> = STORE_METRICS
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
