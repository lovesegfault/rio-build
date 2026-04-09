//! Test-only `metrics::Recorder` implementations.
//!
//! Two recorders for two assertion shapes:
//!
//! - `DescribedNames` — captures `describe_*!` macro calls. For
//!   "every spec'd metric has a describe call" checks (the
//!   `metrics_registered.rs` pattern). `register_*` return noop.
//!
//! - [`CountingRecorder`] — captures `counter!().increment()` deltas
//!   keyed by `name{sorted,labels}`. For "this code path fired this
//!   metric" behavioral assertions. Gauge touch-set for absence checks.
//!
//! - [`GaugeValues`] — captures `gauge!().set()` values (not just names).
//!   For "this gauge was set to value X" assertions. f64 roundtrips via
//!   `AtomicU64::to_bits/from_bits` — no precision loss.
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
/// Shared by [`CountingRecorder`] and [`GaugeValues`] for map keying.
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
    pub fn histograms(&self) -> Vec<String> {
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
    // Gauge touch-set: names only, no values. `gauge!(name).set()`
    // expands to `recorder.register_gauge(key, _).set(v)` — the
    // register call fires on EVERY `gauge!()` invocation, so tracking
    // the key here captures "gauge was touched" regardless of value.
    // Used for absence-checks (leader-gate: standby must NOT set).
    gauges: Mutex<HashSet<String>>,
    // Histogram touch-set: names only, mirroring `gauges`. For "this
    // code path recorded into this histogram" assertions where the
    // value is non-deterministic (elapsed time).
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

    /// True if any `gauge!()` invocation has been observed for `name`
    /// (unlabeled name only — sufficient for the handle_tick gauges,
    /// which carry no labels).
    pub fn gauge_touched(&self, name: &str) -> bool {
        self.gauges.lock().unwrap().contains(name)
    }

    /// True if any `histogram!()` invocation has been observed for `name`
    /// (unlabeled name only). For "this code path recorded into this
    /// histogram" assertions where the value is non-deterministic.
    pub fn histogram_touched(&self, name: &str) -> bool {
        self.histograms.lock().unwrap().contains(name)
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
        self.gauges.lock().unwrap().insert(key.name().to_string());
        Gauge::noop()
    }
    fn register_histogram(&self, key: &Key, _: &Metadata<'_>) -> Histogram {
        self.histograms
            .lock()
            .unwrap()
            .insert(key.name().to_string());
        Histogram::noop()
    }
}

// ===========================================================================
// GaugeValues — captures gauge.set() values (not just names)
// ===========================================================================

/// Captures `gauge!().set()` values for test assertions. Partner to
/// [`CountingRecorder`] (which tracks gauge NAMES but not values).
///
/// `register_gauge` hands back an `Arc<AtomicU64>` (metrics' `GaugeFn
/// for AtomicU64` stores `f64::to_bits` on set()); [`GaugeValues::get`]
/// reads back via `f64::from_bits` — no precision loss for exact sets.
///
/// Keys render as `name{k=v,k2=v2}` with labels sorted, so tests can
/// assert on the full labeled metric identity.
#[derive(Default)]
pub struct GaugeValues {
    gauges: Mutex<std::collections::HashMap<String, Arc<AtomicU64>>>,
}

impl GaugeValues {
    /// Returns the last value set for `rendered_key` (rendered as
    /// `name{k=v}` with sorted labels), or `None` if never touched.
    pub fn get(&self, rendered_key: &str) -> Option<f64> {
        self.gauges
            .lock()
            .unwrap()
            .get(rendered_key)
            .map(|a| f64::from_bits(a.load(std::sync::atomic::Ordering::Relaxed)))
    }
}

impl Recorder for GaugeValues {
    fn describe_counter(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_gauge(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}
    fn describe_histogram(&self, _: KeyName, _: Option<Unit>, _: SharedString) {}

    fn register_counter(&self, _: &Key, _: &Metadata<'_>) -> Counter {
        Counter::noop()
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
    fn register_histogram(&self, _: &Key, _: &Metadata<'_>) -> Histogram {
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
// r[impl ts.metrics.grep]
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

/// Grep the observability.md table for metric names with `prefix`.
///
/// Table rows look like `` | `rio_component_metric_name` | Type | Desc | ``.
/// First `|` stripped, cell trimmed of backticks, result must be
/// purely `[a-z0-9_]+` (rejects prose mentions, comma-separated
/// cells like the Histogram Buckets table, `{label}` examples,
/// `|---|---|` separator).
pub fn grep_spec_names(obs_md_src: &str, prefix: &str) -> Vec<String> {
    let mut names: Vec<String> = obs_md_src
        .lines()
        .filter_map(|l| {
            let first = l
                .strip_prefix('|')?
                .split('|')
                .next()?
                .trim()
                .trim_matches('`');
            (first.starts_with(prefix)
                && !first.is_empty()
                && first.chars().all(|c| c.is_ascii_alphanumeric() || c == '_'))
            .then(|| first.to_string())
        })
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
/// - `prefix`: `"rio_X_"` — selects this crate's rows from observability.md
/// - `histogram_buckets`: the crate's `pub const HISTOGRAM_BUCKETS` table
/// - `spec_floor`: min rows expected in the obs.md table (vacuity guard)
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
            let path = format!("{}/../docs/src/observability.md", manifest_dir());
            let obs_md = ::std::fs::read_to_string(&path)
                .unwrap_or_else(|e| panic!("read {path}: {e}"));
            let spec = $crate::metrics::grep_spec_names(&obs_md, $prefix);
            assert!(
                spec.len() >= $spec_floor,
                "obs.md grep found only {} {} entries — table format changed?",
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
/// Spec→describe direction: catches "spec'd in observability.md but
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
/// `metrics::counter!("new_thing")` deep in a handler but forgot both
/// the `describe_*!` AND the observability.md row" — P0214's
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
    fn grep_extracts_table_column_one() {
        let obs_md = "\
| Metric | Type | Description |
|---|---|---|
| `rio_gateway_foo_total` | Counter | desc |
| `rio_gateway_bar_seconds` | Histogram | desc |
| rio_scheduler_baz | Counter | wrong prefix (excluded) |
| `rio_gateway_foo_total` | Counter | dup row — deduped |

prose mention of rio_gateway_inline (excluded — no leading `|`)

| `rio_gateway_foo_total`, `rio_builder_bar` | `[1, 5]` | excluded — comma-sep cell |
";
        assert_eq!(
            grep_spec_names(obs_md, "rio_gateway_"),
            vec!["rio_gateway_bar_seconds", "rio_gateway_foo_total"],
            "sort+dedup; prose, comma-cells excluded"
        );
        // Separator + header rows excluded.
        assert_eq!(
            grep_spec_names("|---|---|\n| `rio_x_ok` | g |\n", "rio_x_"),
            vec!["rio_x_ok"]
        );
    }
}
