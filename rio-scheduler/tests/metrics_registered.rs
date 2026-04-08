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
//!
//! # The histogram-bucket check
//!
//! Catches "added `describe_histogram!` and `metrics::histogram!()` but
//! forgot the `Matcher::Full` entry in `init_metrics()`." The metric
//! scrapes fine, `# HELP` is present, the two describe checks pass — but
//! the histogram gets default `[0.005..10.0]` buckets. For count-type or
//! long-duration metrics, every sample lands in `+Inf` and p99 is
//! unusable. `rio_scheduler_build_graph_edges` shipped exactly that way
//! (P0321): described, emitted, spec'd with suggested buckets — but
//! `init_metrics` had no Matcher for it.

// r[verify obs.metric.scheduler]
rio_test_support::metrics_suite! {
    describe_fn: rio_scheduler::describe_metrics,
    crate_name: "rio-scheduler",
    spec_floor: 20,
    emit_floor: 30,
    default_buckets_ok: [
        "rio_scheduler_recovery_duration_seconds",
        // Actor commands should be sub-second; the [0.005..10.0]
        // default covers the "alert at p99>1s" range exactly. A
        // command at 10s+ is already a head-of-line stall (I-140) —
        // the +Inf bucket is the alert, finer resolution above 10s
        // doesn't change the response.
        "rio_scheduler_actor_cmd_seconds",
    ],
}
