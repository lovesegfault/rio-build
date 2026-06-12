//! Test-only `metrics::Recorder` implementations.
//!
//! Two recorders for two assertion shapes:
//!
//! - `DescribedNames` — captures `describe_*!` macro calls. For
//!   "every spec'd metric has a describe call" checks (the
//!   `metrics_registered.rs` pattern). `register_*` return noop.
//!
//! - [`CountingRecorder`] — captures `counter!().increment()` deltas and
//!   `gauge!().set()` values keyed by `name{sorted,labels}`. For "this
//!   code path fired this metric" behavioral assertions. Gauge f64
//!   roundtrips via `AtomicU64::to_bits/from_bits` — no precision loss.
//!
//! All pair with `metrics::with_local_recorder` (sync closure) or
//! `metrics::set_default_local_recorder` (guard-scoped, visible across
//! `.await` on a current-thread tokio runtime — `#[tokio::test]` default).
//!
//! Extracted from 5× byte-identical DescribedNames copies
//! (rio-{controller,gateway,scheduler,store,worker}/tests/metrics_registered.rs)
//! and 3× drifting CountingRecorder copies (scheduler/src/actor/tests/helpers.rs
//! canonical; controller/src/reconcilers/gc_schedule.rs + gateway/tests/ssh_hardening.rs
//! stripped subsets). P0212 left the breadcrumb at gc_schedule.rs:229.

use std::collections::{BTreeSet, HashMap, HashSet};
use std::path::Path;
use std::sync::atomic::{AtomicU64, Ordering};
use std::sync::{Arc, Mutex};

use metrics::{Counter, Gauge, Histogram, Key, KeyName, Metadata, Recorder, SharedString, Unit};

/// Render a `metrics::Key` as `name{k=v,k2=v2}` with labels sorted.
/// Used by [`CountingRecorder`] for map keying.
fn render_key(key: &Key) -> String {
    let mut labels: Vec<_> = key
        .labels()
        .map(|l| format!("{}={}", l.key(), l.value()))
        .collect();
    labels.sort();
    format!("{}{{{}}}", key.name(), labels.join(","))
}

// ===========================================================================
// DescribedNames — captures describe_*! names
// ===========================================================================

/// Recorder that captures names passed to `describe_*` and ignores
/// everything else. `register_*` return noop handles — we never
/// touch a metric, only describe.
#[derive(Default)]
pub(crate) struct DescribedNames(Arc<Mutex<Vec<String>>>);

impl DescribedNames {
    /// Snapshot of all names captured so far. Clones out of the lock.
    pub(crate) fn names(&self) -> Vec<String> {
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
// DescribedHistograms — captures describe_histogram! names only
// ===========================================================================

/// Recorder that captures `describe_histogram!` names and ignores
/// counters/gauges. For "every describe_histogram! has a bucket config"
/// checks ([`assert_histograms_have_buckets`]) where the histogram set
/// must be distinguished from the rest. `DescribedNames` already covers
/// the "all names regardless of type" case.
#[derive(Default)]
pub(crate) struct DescribedHistograms(Arc<Mutex<Vec<String>>>);

impl DescribedHistograms {
    /// Snapshot of all `describe_histogram!` names captured so far.
    pub(crate) fn histograms(&self) -> Vec<String> {
        self.0.lock().unwrap().clone()
    }
}

impl Recorder for DescribedHistograms {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
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
    // Gauge values keyed by rendered `name{labels}`. `metrics` provides
    // `impl GaugeFn for AtomicU64` (stores `f64::to_bits` on set()), so
    // `Gauge::from_arc(Arc<AtomicU64>)` is a real handle and
    // [`gauge_value`] reads back via `f64::from_bits` — no precision
    // loss. Presence in the map also serves as the touch-set for
    // absence-checks (leader-gate: standby must NOT set).
    gauges: Mutex<HashMap<String, Arc<AtomicU64>>>,
    // Histogram touch-set: rendered `name{labels}` keys, mirroring
    // `gauges`. For "this code path recorded into this histogram"
    // assertions where the value is non-deterministic (elapsed time);
    // the rendered labels let tests pin the label arm too.
    histograms: Mutex<HashSet<String>>,
}

impl CountingRecorder {
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

    /// Returns the last value set for `rendered_key` (rendered as
    /// `name{k=v}` with sorted labels), or `None` if never touched.
    pub fn gauge_value(&self, rendered_key: &str) -> Option<f64> {
        self.gauges
            .lock()
            .unwrap()
            .get(rendered_key)
            .map(|a| f64::from_bits(a.load(Ordering::Relaxed)))
    }

    /// True if any `gauge!()` invocation has been observed for `name`
    /// (unlabeled name only — sufficient for the handle_tick gauges,
    /// which carry no labels).
    pub fn gauge_touched(&self, name: &str) -> bool {
        self.gauge_value(&format!("{name}{{}}")).is_some()
    }

    /// True if any `histogram!()` invocation has been observed for `name`,
    /// regardless of labels. For "this code path recorded into this
    /// histogram" assertions where the value is non-deterministic.
    pub fn histogram_touched(&self, name: &str) -> bool {
        let prefix = format!("{name}{{");
        self.histograms
            .lock()
            .unwrap()
            .iter()
            .any(|k| k.starts_with(&prefix))
    }

    /// True if `histogram!()` was observed for exactly `rendered_key`
    /// (rendered as `name{k1=v1,k2=v2}` with labels sorted; no labels →
    /// `"name{}"`). For pinning the label arm, not just the name —
    /// e.g. a per-tier latency histogram recorded under the wrong tier
    /// passes [`Self::histogram_touched`] but fails this.
    pub fn histogram_key_touched(&self, rendered_key: &str) -> bool {
        self.histograms.lock().unwrap().contains(rendered_key)
    }

    /// All histogram keys seen so far (sorted). For assertion-failure
    /// diagnostics: when a label-arm check fails, this shows what DID
    /// get recorded.
    pub fn histogram_keys(&self) -> Vec<String> {
        let mut keys: Vec<_> = self.histograms.lock().unwrap().iter().cloned().collect();
        keys.sort();
        keys
    }

    /// All gauge names seen so far (sorted). For assertion-failure
    /// diagnostics: when an absence-check fails, this shows what DID
    /// get touched.
    pub fn gauge_names(&self) -> Vec<String> {
        let mut names: Vec<_> = self.gauges.lock().unwrap().keys().cloned().collect();
        names.sort();
        names
    }
}

impl Recorder for CountingRecorder {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, key: &Key, _: &Metadata<'_>) -> Counter {
        let rendered = render_key(key);
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
        let rendered = render_key(key);
        let atomic = self
            .gauges
            .lock()
            .unwrap()
            .entry(rendered)
            .or_insert_with(|| Arc::new(AtomicU64::new(0)))
            .clone();
        Gauge::from_arc(atomic)
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        self.histograms.lock().unwrap().insert(render_key(key));
        Histogram::noop()
    }
}

// ===========================================================================
// source-text grep — runs at TEST time (was: per-crate build.rs at BUILD time)
// ===========================================================================

/// Grep `<manifest_dir>/src/**.rs` for `metrics::{counter,gauge,histogram}!("…")`
/// macro literals.
///
/// Source-text, not a Prometheus scrape: most `metrics::counter!()`
/// calls are deep in handlers gated on actor state you can't trigger
/// from a unit test. The failure mode is "developer wrote a literal
/// string in a macro call" — textual by nature.
///
/// `\bmetrics::` prefix is REQUIRED (matches this codebase's
/// convention — no one imports the macros unqualified) and avoids
/// false-matching `describe_counter!("…")`. `\s*` handles rustfmt's
/// multi-line break after the paren.
// r[impl ts.metrics.grep+2]
pub fn grep_emitted_names(manifest_dir: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"\bmetrics::(?:counter|gauge|histogram)!\s*\(\s*"([a-z0-9_]+)""#)
        .unwrap();
    let mut names = BTreeSet::new();
    fn walk(dir: &Path, re: &regex::Regex, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, re, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for cap in re.captures_iter(&text) {
                    out.insert(cap[1].to_string());
                }
            }
        }
    }
    walk(&Path::new(manifest_dir).join("src"), &re, &mut names);
    names.into_iter().collect()
}

/// Grep `docs/gen/metrics.json` for metric names with `prefix`.
///
/// The file is `{"names":["rio_a","rio_b",...]}` (sorted, unique),
/// emitted by `xtask regen docs-data` from a regex scan of
/// `describe_*!("rio_...")` literals across all `rio-*/src/**`. We
/// don't pull in `serde_json` for this — splitting on `"` and
/// keeping the `[a-z0-9_]+` tokens that match `prefix` is sufficient
/// for a flat string array (and matches the old markdown-table grep
/// in spirit).
///
/// This was the markdown-table parser before the typst spec
/// migration. The spec table is now GENERATED from metrics.json, so
/// the original "spec → describe" assertion has narrowed to
/// "regex-scanned `describe_*!` literals → `describe_metrics()`
/// fires them" — catches a `describe_*!` call that's in source but
/// not reachable from the per-crate `describe_metrics()` body
/// (cfg-gated, dead, or in the wrong fn). The `spec_floor` vacuity
/// guard still trips if metrics.json is empty/stale.
pub fn grep_spec_names(metrics_json_body: &str, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = metrics_json_body
        .split('"')
        .filter(|s| {
            s.starts_with(prefix)
                && !s.is_empty()
                && s.chars().all(|c| c.is_ascii_alphanumeric() || c == '_')
        })
        .map(str::to_owned)
        .collect();
    names.sort();
    names.dedup();
    names
}

// ===========================================================================
// metrics_suite! — generates the 3-test metrics_registered.rs body
// ===========================================================================

/// Expand to the three `metrics_registered.rs` tests
/// (spec→describe, emit→describe, describe→buckets).
///
/// Invoked once per crate at `tests/metrics_registered.rs`. The
/// `// r[verify obs.metric.X]` tracey marker goes ABOVE the macro
/// invocation (tracey reads source text, not macro expansions).
///
/// The grep runs at test time against the runtime
/// `CARGO_MANIFEST_DIR` env (NOT compile-time `env!()` — under
/// crate2nix the compile-time path is a per-crate build sandbox
/// that's gone by the time nextest runs the binary). No build.rs,
/// no `OUT_DIR` artifacts, no per-crate build-script invocation on
/// every `cargo build`.
///
/// Parameters:
/// - `describe_fn`: path to the crate's `pub fn describe_metrics()`
/// - `crate_name`: human-readable name for error messages
/// - `prefix`: `"rio_X_"` — selects this crate's rows from `docs/gen/metrics.json` (line 354)
/// - `histogram_buckets`: the crate's `pub const HISTOGRAM_BUCKETS` table
/// - `spec_floor`: min rows expected in metrics.json (vacuity guard)
/// - `emit_floor`: min `metrics::*!` literals expected in src/ (regex-health guard)
/// - `default_buckets_ok`: histograms deliberately on `[0.005..10.0]` defaults
#[macro_export]
macro_rules! metrics_suite {
    (
        describe_fn: $describe_fn:path,
        crate_name: $crate_name:literal,
        prefix: $prefix:literal,
        histogram_buckets: $histogram_buckets:expr,
        spec_floor: $spec_floor:literal,
        emit_floor: $emit_floor:literal,
        default_buckets_ok: [$($ok:literal),* $(,)?] $(,)?
    ) => {
        fn manifest_dir() -> ::std::string::String {
            ::std::env::var("CARGO_MANIFEST_DIR").expect("CARGO_MANIFEST_DIR set by cargo/nextest")
        }

        #[test]
        fn all_spec_metrics_have_describe_call() {
            let path = format!("{}/../docs/gen/metrics.json", manifest_dir());
            let body = ::std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path}: {e}; run `cargo xtask regen docs-data`"));
            let spec = $crate::metrics::grep_spec_names(&body, $prefix);
            assert!(
                spec.len() >= $spec_floor,
                "docs/gen/metrics.json has only {} {} entries — stale? run \
                 `cargo xtask regen docs-data`",
                spec.len(),
                $prefix,
            );
            let spec: ::std::vec::Vec<&str> = spec.iter().map(String::as_str).collect();
            $crate::metrics::assert_spec_metrics_described(&spec, $describe_fn, $crate_name);
        }

        #[test]
        fn all_emitted_metrics_are_described() {
            let emitted = $crate::metrics::grep_emitted_names(&manifest_dir()).join("\n");
            $crate::metrics::assert_emitted_metrics_described(
                &emitted,
                $emit_floor,
                $describe_fn,
                $crate_name,
            );
        }

        #[test]
        fn all_histograms_have_bucket_config() {
            $crate::metrics::assert_histograms_have_buckets(
                $describe_fn,
                $histogram_buckets,
                &[$($ok),*],
                $crate_name,
            );
        }
    };
}

// ===========================================================================
// Assertion helpers — extracted from 5× metrics_registered.rs test bodies
// ===========================================================================

/// Assert that every name in `spec_metrics` appears in the set of
/// `describe_*!` calls fired by `describe_fn`.
///
/// Spec→describe direction: catches "name in docs/gen/metrics.json but
/// the `describe_metrics()` fn forgot to mention it" — the metric
/// scrapes with no `# HELP` line, Grafana tooltips empty.
///
/// `describe_fn` is the crate's `pub fn describe_metrics()` — passed
/// as a fn pointer so this helper stays crate-agnostic. `crate_name`
/// is for the error message only.
// r[impl ts.metrics.asserts]
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
/// joined output of [`grep_emitted_names`]) appears in the set of
/// `describe_*!` calls fired by `describe_fn`.
///
/// Emit→describe direction: catches "someone added
/// `metrics::counter!("new_thing")` deep in a handler but forgot
/// the `describe_*!` (which populates docs/gen/metrics.json)" — P0214's
/// `rio_scheduler_build_timeouts_total` did exactly this and sailed
/// through the spec→describe check (which only knows what's IN the
/// spec list).
///
/// `min_emitted` is a precondition self-check: if the source-text
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
        "emitted-metrics grep found only {} entries (threshold {min_emitted}) — \
         regex likely broke (check grep_emitted_names vs. src/ macro call style)",
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
         {crate_name}/src/lib.rs::describe_metrics(), then `cargo xtask \
         regen docs-data` to refresh docs/gen/metrics.json."
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
/// the per-crate `HISTOGRAM_BUCKETS` table — pass it through so
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
    let recorder = DescribedHistograms::default();
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
         HISTOGRAM_BUCKETS entry:\n  {missing:#?}\n\
         \n\
         Every sample will land in the default +Inf bucket and p99 is \
         unusable. Either add an entry in {crate_name}::HISTOGRAM_BUCKETS, \
         or add to the exempt list if [0.005..10.0] genuinely fits.\n\
         \n\
         configured: {configured:?}\n\
         exempt: {exempt:?}"
    );
}

#[cfg(test)]
mod grep_tests {
    use super::grep_spec_names;

    #[test]
    fn grep_extracts_json_names_array() {
        let body = r#"{
  "names": [
    "rio_gateway_bar_seconds",
    "rio_gateway_foo_total",
    "rio_scheduler_baz"
  ]
}"#;
        assert_eq!(
            grep_spec_names(body, "rio_gateway_"),
            vec!["rio_gateway_bar_seconds", "rio_gateway_foo_total"],
            "prefix-filtered, sorted; other-crate prefix excluded"
        );
        // The JSON object key ("names") doesn't start with rio_ so the
        // dquote-split can't accidentally pick it up under any real
        // per-crate prefix.
        assert!(grep_spec_names(body, "rio_").iter().all(|n| n != "names"));
        // Compact JSON (no pretty-print) still parses.
        assert_eq!(
            grep_spec_names(r#"{"names":["rio_x_ok","rio_y_nope"]}"#, "rio_x_"),
            vec!["rio_x_ok"]
        );
    }
}

// ===========================================================================
// DescribedKinds — captures describe_*! names WITH their metric kind
// ===========================================================================

/// Metric kind as declared by the `describe_*!` macro family. Used by
/// [`assert_alert_metrics_covered`] to classify alert-referenced names:
/// counters must be boot-seeded, gauges must be leader-family-or-exempt,
/// histograms are exempt by type (a `histogram_quantile` over an absent
/// series is a rendering gap, not a verdict gap — and seeding one would
/// fabricate an observation).
#[derive(Clone, Copy, Debug, PartialEq, Eq)]
pub enum MetricKind {
    Counter,
    Gauge,
    Histogram,
}

/// Recorder that captures `describe_*!` names keyed to their
/// [`MetricKind`]. The kind-aware sibling of `DescribedNames`.
#[derive(Default)]
pub struct DescribedKinds(Arc<Mutex<HashMap<String, MetricKind>>>);

impl DescribedKinds {
    /// Snapshot of the name → kind map captured so far.
    pub fn kinds(&self) -> HashMap<String, MetricKind> {
        self.0.lock().unwrap().clone()
    }
}

impl Recorder for DescribedKinds {
    fn describe_counter(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0
            .lock()
            .unwrap()
            .insert(key.as_str().to_string(), MetricKind::Counter);
    }
    fn describe_gauge(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0
            .lock()
            .unwrap()
            .insert(key.as_str().to_string(), MetricKind::Gauge);
    }
    fn describe_histogram(&self, key: KeyName, _: Option<Unit>, _: SharedString) {
        self.0
            .lock()
            .unwrap()
            .insert(key.as_str().to_string(), MetricKind::Histogram);
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
// Alert-parity: every alert-referenced series exists from boot
// ===========================================================================

/// One boot-seeded counter family: the bare name, plus its closed
/// label axis when the series is labeled. `None` = unlabeled (one
/// series); `Some((label, values))` = one series per value, all
/// seeded with `.absolute(0)` at boot.
///
/// Mirrors the per-crate `ALERT_SEEDED_COUNTERS` tables (the seed fns
/// iterate the same shape) so the parity test and the seeder cannot
/// drift: both consume the one declaration.
pub struct SeededCounter {
    pub name: &'static str,
    pub label: Option<(&'static str, &'static [&'static str])>,
}

/// A gauge exempt from the leader-family requirement, with the reason
/// recorded at the declaration site (surfaced verbatim in failures so
/// the exemption table reads as documentation).
pub struct GaugeExemption {
    pub name: &'static str,
    pub rationale: &'static str,
}

/// Extract every `rio_<prefix>…` metric token referenced inside an
/// `expr:` block of the given YAML bodies (PrometheusRule /
/// ScaledObject templates).
///
/// Line-state-machine, not a YAML parser: helm templating (`{{ … }}`)
/// makes these files non-YAML before render. Two expr shapes:
///
/// - `expr: <inline>` — the rest of the line is the expression.
/// - `expr: |` — every following line MORE indented than the `expr:`
///   key belongs to the block; the first line at-or-left of the key's
///   indent ends it.
///
/// ScaledObjects carry `query: "<promql>"` instead of `expr:` — the
/// same inline shape, matched by the same key logic. ScaledObjects
/// built through the `rio.promTrigger` helm helper carry the promql
/// as a positional `include` argument instead of a literal `query:`
/// key — those lines (from `include "rio.promTrigger"` to the closing
/// `}}`) are scanned too; only QUOTED strings in them count, so the
/// surrounding template plumbing never contributes tokens.
///
/// Histogram-suffixed tokens (`_bucket`/`_sum`/`_count`) are stripped
/// to their base name; callers classify the base via the describe
/// kinds. Annotation/comment references deliberately do NOT count —
/// only expressions evaluate against absent series.
pub fn extract_alert_metric_names(yaml_bodies: &[String], prefix: &str) -> BTreeSet<String> {
    let token_re = regex::Regex::new(&format!(r"\b({}[a-z0-9_]+)", regex::escape(prefix))).unwrap();
    let mut out = BTreeSet::new();
    for body in yaml_bodies {
        let mut block_indent: Option<usize> = None; // inside `expr: |` at this key indent
        let mut in_trigger = false; // inside an `include "rio.promTrigger"` arg list
        for line in body.lines() {
            let indent = line.len() - line.trim_start().len();
            let trimmed = line.trim_start();
            if in_trigger || trimmed.contains("include \"rio.promTrigger\"") {
                // Quoted args only (the promql + threshold metric name);
                // unquoted template plumbing never contributes.
                let mut rest = trimmed;
                while let Some(start) = rest.find('"') {
                    let tail = &rest[start + 1..];
                    let Some(end) = tail.find('"') else { break };
                    collect_tokens(&token_re, &tail[..end], &mut out);
                    rest = &tail[end + 1..];
                }
                in_trigger = !trimmed.contains("}}");
                continue;
            }
            if let Some(key_indent) = block_indent {
                if !trimmed.is_empty() && indent <= key_indent {
                    block_indent = None; // block ended; fall through to re-test this line
                } else {
                    collect_tokens(&token_re, trimmed, &mut out);
                    continue;
                }
            }
            for key in ["expr:", "query:"] {
                if let Some(rest) = trimmed.strip_prefix(key) {
                    let rest = rest.trim();
                    if rest.is_empty() || rest == "|" || rest == "|-" || rest == ">" || rest == ">-"
                    {
                        block_indent = Some(indent);
                    } else {
                        collect_tokens(&token_re, rest, &mut out);
                    }
                }
            }
        }
    }
    out
}

fn collect_tokens(re: &regex::Regex, text: &str, out: &mut BTreeSet<String>) {
    for cap in re.captures_iter(text) {
        let mut name = cap[1].to_string();
        for suffix in ["_bucket", "_sum", "_count"] {
            if let Some(base) = name.strip_suffix(suffix) {
                name = base.to_string();
                break;
            }
        }
        out.insert(name);
    }
}

/// Extract exact-match label matchers (`label="value"`) per metric
/// name from the same expr blocks. Regex matchers (`=~`) only require
/// the AXIS to exist on the seeded entry — their value sets are open.
fn extract_label_matchers(yaml_bodies: &[String], prefix: &str) -> Vec<(String, String, String)> {
    let re = regex::Regex::new(&format!(
        r#"\b({}[a-z0-9_]+)\{{([^}}]*)}}"#,
        regex::escape(prefix)
    ))
    .unwrap();
    let pair_re = regex::Regex::new(r#"([a-z0-9_]+)="([^"]*)""#).unwrap();
    let mut out = Vec::new();
    for body in yaml_bodies {
        for cap in re.captures_iter(body) {
            let name = cap[1].to_string();
            for pair in pair_re.captures_iter(&cap[2]) {
                // `=~` regex matchers also match this pattern's tail —
                // exclude them by checking the char before `=`.
                let full = pair.get(0).unwrap();
                let before = &cap[2][..full.start() + pair[1].len()];
                if before.ends_with('~') {
                    continue;
                }
                out.push((name.clone(), pair[1].to_string(), pair[2].to_string()));
            }
        }
    }
    out
}

/// Assert every alert-referenced metric of `prefix` is covered: each
/// counter appearing in a PrometheusRule/ScaledObject `expr:` is in
/// the crate's boot-seed table (with any exact label matcher inside
/// the seeded value set), each gauge is leader-family or exempt (with
/// rationale), and each histogram is exempt by type. Names that the
/// crate's `describe_metrics()` does not declare at all fail loudly —
/// an alert over a never-described series is a typo or a stale rule.
///
/// `yaml_paths` are read relative to the test's CWD (the crate root
/// under nextest) — pass `../infra/helm/...` paths and add the files
/// to the nix test filesets, or the check passes locally and fails
/// sandboxed.
#[allow(clippy::too_many_arguments)]
pub fn assert_alert_metrics_covered(
    yaml_paths: &[&str],
    prefix: &str,
    describe_fn: fn(),
    seeded: &[SeededCounter],
    leader_family: &[&str],
    gauge_exemptions: &[GaugeExemption],
    crate_name: &str,
) {
    let bodies: Vec<String> = yaml_paths
        .iter()
        .map(|p| {
            std::fs::read_to_string(p).unwrap_or_else(|e| {
                panic!(
                    "read {p}: {e}; alert-parity test needs the helm templates \
                     in the nix fileset (nix/lib/nextest-args.nix) — without \
                     them this check passes locally and fails sandboxed"
                )
            })
        })
        .collect();
    let referenced = extract_alert_metric_names(&bodies, prefix);
    assert!(
        !referenced.is_empty(),
        "{crate_name}: no {prefix} tokens found in any expr block of {yaml_paths:?} — \
         extractor or fileset broke (vacuity guard)"
    );

    let recorder = DescribedKinds::default();
    metrics::with_local_recorder(&recorder, describe_fn);
    let kinds = recorder.kinds();

    let mut failures: Vec<String> = Vec::new();
    for name in &referenced {
        match kinds.get(name) {
            None => failures.push(format!(
                "{name}: referenced in an alert expr but never described by \
                 {crate_name}::describe_metrics() — typo or stale rule"
            )),
            Some(MetricKind::Histogram) => {} // exempt by type
            Some(MetricKind::Counter) => {
                if !seeded.iter().any(|s| s.name == *name) {
                    failures.push(format!(
                        "{name}: alert-referenced counter not in the boot-seed table — \
                         the alert evaluates an absent series until the first increment \
                         (the bug_322 birth-gap class); add it to ALERT_SEEDED_COUNTERS"
                    ));
                }
            }
            Some(MetricKind::Gauge) => {
                let in_family = leader_family.contains(&name.as_str());
                let exempt = gauge_exemptions.iter().any(|e| e.name == *name);
                if !in_family && !exempt {
                    failures.push(format!(
                        "{name}: alert-referenced gauge neither in the leader-gauge \
                         family nor exempted-with-rationale — a deposed replica's \
                         frozen series (or a never-set boot gap) feeds this alert"
                    ));
                }
            }
        }
    }

    // Exact label matchers in exprs must be inside the seeded product.
    for (name, label, value) in extract_label_matchers(&bodies, prefix) {
        let Some(entry) = seeded.iter().find(|s| s.name == name) else {
            continue; // counter-membership failure already recorded above (or a gauge)
        };
        match entry.label {
            None => failures.push(format!(
                "{name}: expr matches {{{label}=\"{value}\"}} but the seed entry is \
                 unlabeled — seed the label product or the matcher never matches \
                 the seeded series"
            )),
            Some((axis, values)) => {
                if axis != label {
                    failures.push(format!(
                        "{name}: expr matches on label {label:?} but the seed axis \
                         is {axis:?}"
                    ));
                } else if !values.contains(&value.as_str()) {
                    failures.push(format!(
                        "{name}: expr matches {{{label}=\"{value}\"}} but {value:?} \
                         is not in the seeded value set {values:?}"
                    ));
                }
            }
        }
    }

    assert!(
        failures.is_empty(),
        "{crate_name} alert-parity failures:\n  - {}",
        failures.join("\n  - ")
    );
}

/// Grep `<manifest_dir>/src/**.rs` for `metrics::gauge!("…")` literals
/// only — the gauge-scoped sibling of [`grep_emitted_names`], for the
/// single-ownership policy test: every RAW gauge emit must be an
/// exempted per-replica/own-edge gauge; leader-family members carry no
/// per-site literals at all (they publish through the typed
/// `LeaderGauge` accessors, whose names live only in the one
/// declaration), so a family name appearing here means someone
/// bypassed the family.
pub fn grep_emitted_gauge_names(manifest_dir: &str) -> Vec<String> {
    let re = regex::Regex::new(r#"\bmetrics::gauge!\s*\(\s*"([a-z0-9_]+)""#).unwrap();
    let mut names = BTreeSet::new();
    fn walk(dir: &Path, re: &regex::Regex, out: &mut BTreeSet<String>) {
        for entry in std::fs::read_dir(dir).unwrap() {
            let path = entry.unwrap().path();
            if path.is_dir() {
                walk(&path, re, out);
            } else if path.extension().is_some_and(|e| e == "rs") {
                let text = std::fs::read_to_string(&path).unwrap();
                for cap in re.captures_iter(&text) {
                    out.insert(cap[1].to_string());
                }
            }
        }
    }
    walk(&Path::new(manifest_dir).join("src"), &re, &mut names);
    names.into_iter().collect()
}
