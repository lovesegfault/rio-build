//! Test-only `metrics::Recorder` implementations.
//!
//! Two recorders for two assertion shapes:
//!
//! - [`DescribedNames`] — captures `describe_*!` macro calls. For
//!   "every spec'd metric has a describe call" checks (the
//!   `metrics_registered.rs` pattern). `register_*` return noop.
//!
//! - [`CountingRecorder`] — captures `counter!().increment()` deltas
//!   keyed by `name{sorted,labels}`. For "this code path fired this
//!   metric" behavioral assertions. Gauge touch-set for absence checks.
//!
//! Both pair with `metrics::with_local_recorder` (sync closure) or
//! `metrics::set_default_local_recorder` (guard-scoped, visible across
//! `.await` on a current-thread tokio runtime — `#[tokio::test]` default).
//!
//! Extracted from 5× byte-identical DescribedNames copies
//! (rio-{controller,gateway,scheduler,store,worker}/tests/metrics_registered.rs)
//! and 3× drifting CountingRecorder copies (scheduler/src/actor/tests/helpers.rs
//! canonical; controller/src/reconcilers/gc_schedule.rs + gateway/tests/ssh_hardening.rs
//! stripped subsets). P0212 left the breadcrumb at gc_schedule.rs:229.

use std::collections::{HashMap, HashSet};
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

// ===========================================================================
// DescribedNames — captures describe_*! names
// ===========================================================================

/// Recorder that captures names passed to `describe_*` and ignores
/// everything else. `register_*` return noop handles — we never
/// touch a metric, only describe.
///
/// The inner `Arc<Mutex<Vec<String>>>` is `pub` so existing callsites
/// can keep doing `recorder.0.clone()` then `.lock()`; [`names()`] is
/// a cleaner accessor for new code.
///
/// [`names()`]: Self::names
#[derive(Default)]
pub struct DescribedNames(pub Arc<Mutex<Vec<String>>>);

impl DescribedNames {
    /// Snapshot of all names captured so far. Clones out of the lock.
    pub fn names(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

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

// ===========================================================================
// DescribedByType — captures describe_*! names, split by metric type
// ===========================================================================

/// `(counters, gauges, histograms)` — one vec per `describe_*!` kind.
type NamesByType = (Vec<String>, Vec<String>, Vec<String>);

/// Like [`DescribedNames`] but keeps the three metric types in separate
/// vecs. For "every describe_histogram! has a bucket config" checks where
/// you need to distinguish histograms from counters/gauges.
///
/// Inner tuple: `(counters, gauges, histograms)`.
#[derive(Default)]
pub struct DescribedByType(pub Arc<Mutex<NamesByType>>);

impl DescribedByType {
    /// Snapshot of all `describe_histogram!` names captured so far.
    pub fn histograms(&self) -> Vec<String> {
        self.0.lock().unwrap().2.clone()
    }
    /// Snapshot of all `describe_counter!` names captured so far.
    pub fn counters(&self) -> Vec<String> {
        self.0.lock().unwrap().0.clone()
    }
    /// Snapshot of all `describe_gauge!` names captured so far.
    pub fn gauges(&self) -> Vec<String> {
        self.0.lock().unwrap().1.clone()
    }
}

impl Recorder for DescribedByType {
    fn describe_counter(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().0.push(key.as_str().to_string());
    }
    fn describe_gauge(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().1.push(key.as_str().to_string());
    }
    fn describe_histogram(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0.lock().unwrap().2.push(key.as_str().to_string());
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

// ===========================================================================
// CountingRecorder — captures counter increments + gauge touches
// ===========================================================================

/// Recorder that captures counter increments into a shared map keyed by
/// `name{sorted,labels}`. Used for metric-delta assertions.
///
/// Unlike `with_local_recorder` (sync closure only — fine for the gateway's
/// `handle_session_error`), actor tests need the recorder visible to the
/// *spawned actor task* across `.await` points. Use
/// `metrics::set_default_local_recorder(&recorder)`, which holds the
/// thread-local for the guard's lifetime. `#[tokio::test]` uses a
/// current-thread runtime, so the spawned actor runs on the same OS thread
/// and sees the thread-local when it calls `counter!()`.
#[derive(Default)]
pub struct CountingRecorder {
    // `metrics` provides `impl CounterFn for AtomicU64` (atomics.rs), so
    // `Counter::from_arc(Arc<AtomicU64>)` is a valid counter handle.
    counters: Mutex<HashMap<String, Arc<AtomicU64>>>,
    // Gauge touch-set: names only, no values. `gauge!(name).set()`
    // expands to `recorder.register_gauge(key, _).set(v)` — the
    // register call fires on EVERY `gauge!()` invocation, so tracking
    // the key here captures "gauge was touched" regardless of value.
    // Used for absence-checks (leader-gate: standby must NOT set).
    gauges: Mutex<HashSet<String>>,
}

impl CountingRecorder {
    fn counter_key(key: &Key) -> String {
        let mut labels: Vec<_> = key
            .labels()
            .map(|l| format!("{}={}", l.key(), l.value()))
            .collect();
        labels.sort();
        format!("{}{{{}}}", key.name(), labels.join(","))
    }

    /// Returns the current value for `rendered_key`, or 0 if never
    /// incremented. Keys are rendered as `name{k1=v1,k2=v2}` with
    /// labels sorted; a counter with no labels has key `"name{}"`.
    pub fn get(&self, rendered_key: &str) -> u64 {
        self.counters
            .lock()
            .unwrap()
            .get(rendered_key)
            .map(|a| a.load(Ordering::Relaxed))
            .unwrap_or(0)
    }

    /// All counter keys seen so far. For assertion-failure diagnostics:
    /// if the expected key is absent, seeing the ACTUAL keys pinpoints
    /// a wrong-name regression ("_sent_total" vs "_signals_total").
    pub fn all_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.counters.lock().unwrap().keys().cloned().collect();
        keys.sort();
        keys
    }

    /// True if any `gauge!()` invocation has been observed for `name`
    /// (unlabeled name only — sufficient for the handle_tick gauges,
    /// which carry no labels).
    pub fn gauge_touched(&self, name: &str) -> bool {
        self.gauges.lock().unwrap().contains(name)
    }

    /// All gauge names seen so far (sorted). For assertion-failure
    /// diagnostics: when an absence-check fails, this shows what DID
    /// get touched.
    pub fn gauge_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.gauges.lock().unwrap().iter().cloned().collect();
        names.sort();
        names
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let rendered = Self::counter_key(key);
        let atomic = self
            .counters
            .lock()
            .unwrap()
            .entry(rendered)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        Counter::from_arc(atomic)
    }
    fn register_gauge(&self, key: &Key, _: &Metadata<'_>) -> Gauge {
        self.gauges.lock().unwrap().insert(key.name().to_string());
        Gauge::noop()
    }
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
        Histogram::noop()
    }
}

// ===========================================================================
// Assertion helpers — extracted from 5× metrics_registered.rs test bodies
// ===========================================================================

/// Assert that every name in `spec_metrics` appears in the set of
/// `describe_*!` calls fired by `describe_fn`.
///
/// Spec→describe direction: catches "spec'd in observability.md but
/// the `describe_metrics()` fn forgot to mention it" — the metric
/// scrapes with no `# HELP` line, Grafana tooltips empty.
///
/// `describe_fn` is the crate's `pub fn describe_metrics()` — passed
/// as a fn pointer so this helper stays crate-agnostic. `crate_name`
/// is for the error message only.
pub fn assert_spec_metrics_described(spec_metrics: &[&str], describe_fn: fn(), crate_name: &str) {
    let recorder = DescribedNames::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let described = recorder.names();

    let missing: Vec<_> = spec_metrics
        .iter()
        .filter(|name| !described.contains(&(**name).to_string()))
        .collect();

    assert!(
        missing.is_empty(),
        "spec'd metrics missing from {crate_name}::describe_metrics(): {missing:?}\n\
         \n\
         described:\n{described:#?}"
    );
}

/// Assert that every name in `emitted_metrics` (one per line — the
/// `include_str!(OUT_DIR/emitted_metrics.txt)` output) appears in the
/// set of `describe_*!` calls fired by `describe_fn`.
///
/// Emit→describe direction: catches "someone added
/// `metrics::counter!("new_thing")` deep in a handler but forgot both
/// the `describe_*!` AND the observability.md row" — P0214's
/// `rio_scheduler_build_timeouts_total` did exactly this and sailed
/// through the spec→describe check (which only knows what's IN the
/// spec list).
///
/// `min_emitted` is a precondition self-check: if the build-script
/// grep returns near-zero, either the crate genuinely has no metrics
/// (implausible for any crate large enough to need this check) or the
/// regex broke (e.g., someone imported the macros unqualified). Fail
/// loudly instead of passing vacuously. Pick `min_emitted` at ~75% of
/// the crate's current count so normal churn doesn't trip it but a
/// broken regex does.
pub fn assert_emitted_metrics_described(
    emitted_metrics: &str,
    min_emitted: usize,
    describe_fn: fn(),
    crate_name: &str,
) {
    let emitted: Vec<&str> = emitted_metrics.lines().filter(|l| !l.is_empty()).collect();

    assert!(
        emitted.len() >= min_emitted,
        "EMITTED_METRICS has only {} entries (threshold {min_emitted}) — \
         build-script grep likely broke (check build.rs regex vs. src/ \
         macro call style)",
        emitted.len()
    );

    let recorder = DescribedNames::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let described = recorder.names();

    let undescribed: Vec<_> = emitted
        .iter()
        .filter(|name| !described.contains(&(**name).to_string()))
        .collect();

    assert!(
        undescribed.is_empty(),
        "metrics emitted in {crate_name}/src/ but NOT in describe_metrics():\n  {undescribed:#?}\n\
         \n\
         Add describe_counter!/describe_gauge!/describe_histogram! to \
         {crate_name}/src/lib.rs::describe_metrics() AND a row to \
         docs/src/observability.md."
    );
}

/// Assert that every `describe_histogram!` call fired by `describe_fn`
/// has a corresponding entry in `bucket_map`, or is listed in `exempt`.
///
/// Describe→bucket direction: catches "added `describe_histogram!` and
/// `metrics::histogram!()` calls but forgot the `Matcher::Full` entry in
/// `init_metrics()`." The metric scrapes fine, `# HELP` is present, the
/// two existing helpers pass — but the histogram gets default buckets
/// `[0.005..10.0]`. For count-type or long-duration metrics, every sample
/// lands in `+Inf` and `histogram_quantile(0.99, ...)` returns `+Inf`.
/// `rio_scheduler_build_graph_edges` shipped in exactly this state (P0321).
///
/// `bucket_map` is the crate-agnostic view of
/// `rio_common::observability::HISTOGRAM_BUCKET_MAP` — pass it through so
/// this crate stays leaf (no `rio-common` dep). `exempt` names histograms
/// deliberately kept on default buckets (e.g., recovery_duration_seconds
/// — cold-start PG scan, 10ms–10s fits default).
///
/// Asserts its own precondition: fails if zero histograms collected (a
/// broken recorder would otherwise vacuously pass).
pub fn assert_histograms_have_buckets(
    describe_fn: fn(),
    bucket_map: &[(&str, &[f64])],
    exempt: &[&str],
    crate_name: &str,
) {
    let recorder = DescribedByType::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let histograms = recorder.histograms();

    assert!(
        !histograms.is_empty(),
        "test collected zero histograms from {crate_name}::describe_metrics() — \
         recorder broken or describe_fn changed shape"
    );

    let configured: HashSet<&str> = bucket_map.iter().map(|(n, _)| *n).collect();

    let missing: Vec<_> = histograms
        .iter()
        .filter(|h| !configured.contains(h.as_str()) && !exempt.contains(&h.as_str()))
        .collect();

    assert!(
        missing.is_empty(),
        "histogram(s) in {crate_name}::describe_metrics() with no \
         HISTOGRAM_BUCKET_MAP entry:\n  {missing:#?}\n\
         \n\
         Every sample will land in the default +Inf bucket and p99 is \
         unusable. Either add an entry in rio-common/src/observability.rs \
         HISTOGRAM_BUCKET_MAP, or add to the exempt list if [0.005..10.0] \
         genuinely fits.\n\
         \n\
         configured: {configured:?}\n\
         exempt: {exempt:?}"
    );
}
