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

    /// Returns the current value for `rendered_key` (as produced by
    /// [`counter_key`]), or 0 if never incremented. A counter with no
    /// labels has key `"name{}"`.
    ///
    /// [`counter_key`]: Self::counter_key
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
