//! Observability setup: structured logging, Prometheus metrics, and optional
//! OpenTelemetry OTLP trace export.
//!
//! The subscriber is assembled as a `tracing_subscriber::Registry` with three
//! layers (last two optional):
//!
//!   1. `EnvFilter` — from `RUST_LOG` or fallback default.
//!   2. `fmt` layer — JSON (production) or pretty (dev), via `RIO_LOG_FORMAT`.
//!   3. `tracing-opentelemetry` layer — OTLP export via gRPC, only if
//!      `RIO_OTEL_ENDPOINT` is set. Unset = no OTel overhead at all (not even
//!      a no-op layer; the `Option<Layer>` is `None` and the registry
//!      composes it out at compile-time-ish via `Layer for Option<L>`).
// r[impl obs.log.required-fields]
// r[impl obs.trace.w3c-traceparent]
//!
//! `RIO_LOG_FORMAT` and `RIO_OTEL_ENDPOINT` are read as direct env vars, NOT
//! via figment — logging must initialize before figment could fail, and a
//! figment error with no logging would be invisible.

use std::fmt;
use std::str::FromStr;

use opentelemetry_otlp::WithExportConfig;
use tracing_subscriber::layer::SubscriberExt;
use tracing_subscriber::util::SubscriberInitExt;
use tracing_subscriber::{EnvFilter, Layer};

/// Log output format.
#[derive(Debug, Clone, Copy, Default, PartialEq, Eq)]
pub(crate) enum LogFormat {
    /// JSON structured logs (production default).
    #[default]
    Json,
    /// Human-readable pretty-printed logs (development).
    Pretty,
}

impl fmt::Display for LogFormat {
    fn fmt(&self, f: &mut fmt::Formatter<'_>) -> fmt::Result {
        match self {
            LogFormat::Json => write!(f, "json"),
            LogFormat::Pretty => write!(f, "pretty"),
        }
    }
}

impl FromStr for LogFormat {
    type Err = String;

    fn from_str(s: &str) -> Result<Self, Self::Err> {
        match s.to_ascii_lowercase().as_str() {
            "json" => Ok(LogFormat::Json),
            "pretty" => Ok(LogFormat::Pretty),
            other => Err(format!(
                "unknown log format: {other:?} (expected \"json\" or \"pretty\")"
            )),
        }
    }
}

/// Returned from [`init_tracing`]. On drop, flushes any pending spans
/// to the OTLP exporter (if one was configured). Hold this for the
/// lifetime of `main()` — if dropped early, spans emitted after are
/// still recorded locally but never shipped to the collector.
///
/// The contained `Option` is `None` when OTel wasn't configured, in which
/// case `Drop` is a no-op.
pub struct OtelGuard(Option<opentelemetry_sdk::trace::SdkTracerProvider>);

impl Drop for OtelGuard {
    fn drop(&mut self) {
        if let Some(provider) = self.0.take() {
            // force_flush blocks until all buffered spans are exported.
            // Calling this from Drop is fine: main() is returning, we're
            // on the way out, blocking for ~100ms to ship the last batch
            // is the right thing. If the collector is unreachable, this
            // times out (default 10s) and logs to stderr.
            let _ = provider.force_flush();
            let _ = provider.shutdown();
        }
    }
}

/// Initialize the full observability stack.
///
/// `component` becomes the OTel `service.name` resource attribute — this is
/// what Jaeger shows in the service dropdown. Use the binary name:
/// "gateway", "scheduler", "store", "worker".
///
/// Returns an [`OtelGuard`] which MUST be held for the lifetime of `main()`.
/// Dropping it early stops span export.
///
/// # Env vars read here (not figment — see module doc)
///
/// - `RUST_LOG`: log level filter (e.g., `info,rio_scheduler=debug`)
/// - `RIO_LOG_FORMAT`: `json` or `pretty` (default: `json`)
/// - `RIO_OTEL_ENDPOINT`: OTLP gRPC collector endpoint (e.g.,
///   `http://otel-collector:4317`). Unset = OTel disabled entirely.
/// - `RIO_OTEL_SAMPLE_RATE`: 0.0–1.0, fraction of traces to sample
///   (default: 1.0). Per `configuration.md:102`.
pub fn init_tracing(component: &'static str) -> anyhow::Result<OtelGuard> {
    let env_filter = EnvFilter::try_from_default_env().unwrap_or_else(|_| EnvFilter::new("info"));

    // fmt layer: JSON or pretty. `.boxed()` so both arms have the same type.
    let fmt_layer = match log_format_from_env() {
        LogFormat::Json => tracing_subscriber::fmt::layer().json().boxed(),
        LogFormat::Pretty => tracing_subscriber::fmt::layer().pretty().boxed(),
    };

    // OTel layer: Some if RIO_OTEL_ENDPOINT is set, None otherwise.
    // `Layer` is impl'd for `Option<L>`, so `with(None)` is a pure no-op
    // — zero overhead when OTel is off. The layer comes back already
    // boxed (Box<dyn Layer<S>>) so it composes over ANY subscriber shape;
    // the concrete OpenTelemetryLayer<Registry, Tracer> would hard-code
    // the stack shape and fail to compose over Layered<EnvFilter, ...>.
    let (otel_layer, guard) = build_otel_layer(component)?;

    // W3C TraceContext propagator for traceparent header inject/extract.
    // Registered globally even if otel_layer is None — the interceptor
    // (rio-proto::interceptor) uses this to inject/extract regardless of
    // whether spans are exported. Cheap: it's a stateless parser/formatter.
    opentelemetry::global::set_text_map_propagator(
        opentelemetry_sdk::propagation::TraceContextPropagator::new(),
    );

    tracing_subscriber::registry()
        .with(env_filter)
        .with(fmt_layer)
        .with(otel_layer)
        .init();

    Ok(guard)
}

/// `Box<dyn Layer<S>>` where `S` is the full registry stack. Concrete
/// `OpenTelemetryLayer<Registry, Tracer>` would hard-code `Registry` and
/// fail to compose over `Layered<EnvFilter, Layered<fmt, Registry>>`.
/// Boxing erases the `S` type parameter so the layer composes anywhere.
type BoxedLayer<S> = Box<dyn Layer<S> + Send + Sync + 'static>;

/// Build the OTel layer. Returns `(None, OtelGuard(None))` if
/// `RIO_OTEL_ENDPOINT` is unset. Split out for readability.
fn build_otel_layer<S>(
    component: &'static str,
) -> anyhow::Result<(Option<BoxedLayer<S>>, OtelGuard)>
where
    S: tracing::Subscriber + for<'a> tracing_subscriber::registry::LookupSpan<'a> + Send + Sync,
{
    // Unset and empty (`RIO_OTEL_ENDPOINT=""` in a k8s env block) both
    // mean disabled. Building an exporter against "" succeeds but every
    // export fails at DEBUG — effectively silent — so treat as unset.
    let endpoint: String = crate::config::env_or("RIO_OTEL_ENDPOINT", String::new());
    if endpoint.is_empty() {
        return Ok((None, OtelGuard(None)));
    }

    // `f64::from_str` accepts "nan"/"inf"; `f64::clamp(NaN, 0, 1)`
    // returns NaN — so finite-check after parse, before clamp.
    let raw: f64 = crate::config::env_or("RIO_OTEL_SAMPLE_RATE", 1.0);
    let sample_rate = if raw.is_finite() {
        raw.clamp(0.0, 1.0)
    } else {
        eprintln!("warning: RIO_OTEL_SAMPLE_RATE={raw} is not finite; defaulting to 1.0");
        1.0
    };

    // OTLP gRPC span exporter.
    let exporter = opentelemetry_otlp::SpanExporter::builder()
        .with_tonic()
        .with_endpoint(endpoint.clone())
        .build()
        .map_err(|e| anyhow::anyhow!("failed to build OTLP exporter for {endpoint}: {e}"))?;

    // SdkTracerProvider with batch processor (buffers spans, flushes on
    // interval or when buffer full — avoids a network roundtrip per span).
    let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder()
        .with_batch_exporter(exporter)
        .with_resource(
            opentelemetry_sdk::Resource::builder()
                .with_service_name(component)
                .build(),
        )
        // TraceIdRatioBased: sample `sample_rate` fraction of root traces.
        // Children inherit the parent's sampling decision (that's what
        // "ParentBased" means). So a 0.1 rate means 10% of BUILDS are
        // traced, and within a traced build, ALL spans are captured.
        .with_sampler(opentelemetry_sdk::trace::Sampler::ParentBased(Box::new(
            opentelemetry_sdk::trace::Sampler::TraceIdRatioBased(sample_rate),
        )))
        .build();

    // The tracer is what tracing-opentelemetry bridges to. Name it after
    // this crate — Jaeger shows it as the "instrumentation library".
    use opentelemetry::trace::TracerProvider as _;
    let tracer = provider.tracer("rio-common");

    let layer = tracing_opentelemetry::layer().with_tracer(tracer).boxed();

    // The provider must outlive main() for batch export to work. We can't
    // return it by value (the layer holds a tracer derived from it, but
    // NOT the provider itself — a dropped provider means spans go to
    // /dev/null). OtelGuard holds it and flushes on drop.
    //
    // Also set it globally so `opentelemetry::global::tracer()` works if
    // anything calls it directly (we don't, but future code or a dep might).
    opentelemetry::global::set_tracer_provider(provider.clone());

    eprintln!(
        "OTel OTLP export enabled: endpoint={endpoint}, sample_rate={sample_rate}, service={component}"
    );

    Ok((Some(layer), OtelGuard(Some(provider))))
}

/// Global fallback bucket boundaries — Prometheus client_golang's defaults.
///
/// `metrics-exporter-prometheus` renders histograms as **summaries**
/// (quantile series, no `_bucket`) unless buckets are configured. The
/// per-metric `Matcher::Full` entries from each crate's `HISTOGRAM_BUCKETS` cover the
/// metrics that need custom ranges; this global makes every OTHER histogram
/// emit `_bucket` series instead of summary quantiles, so
/// `histogram_quantile()` works. I-156: without this, every "exempt" metric
/// (the per-crate `DEFAULT_BUCKETS_OK` lists) had zero `_bucket` series and
/// dashboard p99 panels showed "No data".
const DEFAULT_BUCKETS: &[f64] = &[
    0.005, 0.01, 0.025, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0,
];

/// Histogram bucket boundaries for build-duration metrics (seconds).
///
/// Default Prometheus buckets top out at 10s — useless for Nix builds that
/// routinely run 1–60 minutes. These span 1s..2h with denser coverage in the
/// 1–10min range where most builds land.
///
/// Shared by `rio_scheduler_build_duration_seconds` and
/// `rio_builder_build_duration_seconds` — the only bucket set that is
/// genuinely cross-crate. Per-crate consts live next to each crate's
/// `HISTOGRAM_BUCKETS` table.
pub const BUILD_DURATION_BUCKETS: &[f64] = &[
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
];

/// Initialize Prometheus metrics exporter.
///
/// This starts an HTTP server on the given address that serves `/metrics`.
///
/// Installs custom histogram bucket boundaries for every entry in
/// `histogram_buckets` (each crate passes its own table via
/// [`crate::server::bootstrap`]) — the `metrics-exporter-prometheus`
/// default buckets (`[0.005..10.0]`) are tuned for HTTP request latencies
/// and are useless for build durations that span seconds to hours. See
/// `observability.md` for the full bucket table.
///
/// Per-crate `metrics_registered` tests assert every `describe_histogram!`
/// has an entry in that crate's `HISTOGRAM_BUCKETS` OR is in its
/// `DEFAULT_BUCKETS_OK` exemption list. Missing entry → the metric falls
/// through to `DEFAULT_BUCKETS`. I-156: before the global default
/// existed, a missing entry meant **summary mode** (no `_bucket` series at
/// all) — `rio_store_put_path_duration_seconds` shipped that way and
/// dashboard `histogram_quantile()` panels showed "No data".
// r[impl common.helpers]
pub fn init_metrics(
    addr: std::net::SocketAddr,
    global_labels: &[(&'static str, String)],
    histogram_buckets: &[(&'static str, &'static [f64])],
) -> anyhow::Result<()> {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

    // set_buckets / set_buckets_for_metric return Result<Self, BuildError> —
    // the only error variant is EmptyBucketsOrQuantiles, which cannot fire
    // for the non-empty const slices above. Propagate anyway to keep the
    // error surface uniform.
    //
    // Global default first: without it, histograms NOT in `histogram_buckets`
    // render as **summaries** (quantile series, no `_bucket`). I-156: every
    // per-crate `DEFAULT_BUCKETS_OK` exemption was silently a summary, and
    // dashboard `histogram_quantile()` panels showed "No data". Per-metric
    // `Matcher::Full` overrides the global for entries in the map.
    let mut builder = PrometheusBuilder::new().set_buckets(DEFAULT_BUCKETS)?;
    for (name, buckets) in histogram_buckets {
        builder = builder.set_buckets_for_metric(Matcher::Full((*name).to_string()), buckets)?;
    }
    for (k, v) in global_labels {
        builder = builder.add_global_label(*k, v.clone());
    }
    builder
        .with_http_listener(addr)
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus exporter: {e}"))?;

    tracing::info!(addr = %addr, "Prometheus metrics exporter started");
    Ok(())
}

/// Parse `RIO_LOG_FORMAT` environment variable, defaulting to JSON.
fn log_format_from_env() -> LogFormat {
    crate::config::env_or("RIO_LOG_FORMAT", LogFormat::default())
}

#[cfg(test)]
// See config.rs test module for the same allow — figment::Jail closure's
// Result<(), figment::Error> is 208 bytes, API-fixed, can't box it.
#[allow(clippy::result_large_err)]
mod tests {
    use super::*;

    #[test]
    fn log_format_parse() {
        assert_eq!("json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert!("nope".parse::<LogFormat>().is_err());
    }

    #[test]
    fn log_format_parse_case_insensitive() {
        // RIO_LOG_FORMAT=JSON (or Json, Pretty, etc.) should not be
        // rejected just because the casing doesn't match.
        assert_eq!("JSON".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("Json".parse::<LogFormat>().unwrap(), LogFormat::Json);
        assert_eq!("PRETTY".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
        assert_eq!("Pretty".parse::<LogFormat>().unwrap(), LogFormat::Pretty);
    }

    #[test]
    fn otel_guard_none_drop_is_noop() {
        // Dropping a guard with no provider must not panic or hang.
        let g = OtelGuard(None);
        drop(g); // if this hangs/panics, the test fails
    }

    /// Can't easily unit-test the full OTel init (it installs a global
    /// subscriber, which collides with other tests). But we CAN test that
    /// `build_otel_layer` returns None/None when the endpoint is unset —
    /// the "OTel is opt-in, not opt-out" guarantee.
    ///
    /// figment::Jail serializes env manipulation under its own mutex AND
    /// resets env on scope exit. Jail doesn't have a "remove var" API — if
    /// the host test env has RIO_OTEL_ENDPOINT set (it shouldn't; CI
    /// doesn't), this test would take the Some branch, try to connect to
    /// that endpoint, and fail for a different reason. Good enough: the
    /// important invariant is "unset → None", and CI env is clean.
    #[test]
    fn build_otel_layer_unset_endpoint_returns_none() {
        // figment::Jail::expect_with takes a thunk, no arg. It jails cwd +
        // env; we just want the mutex serialization, not the cwd jail.
        figment::Jail::expect_with(|_jail| {
            // build_otel_layer is generic over S; monomorphize to Registry
            // for the test (we're not layering it over anything here).
            let (layer, guard) =
                build_otel_layer::<tracing_subscriber::Registry>("test-component").unwrap();
            assert!(layer.is_none(), "no endpoint → no layer");
            assert!(guard.0.is_none(), "no endpoint → no provider to flush");
            Ok(())
        });
    }

    /// `RIO_OTEL_ENDPOINT=""` (empty string, as from an unset Helm value
    /// or `export RIO_OTEL_ENDPOINT=`) must behave like unset: no layer,
    /// no provider. Without the guard, the exporter builds against an
    /// empty URL and every export fails silently at DEBUG.
    #[test]
    fn build_otel_layer_empty_endpoint_returns_none() {
        figment::Jail::expect_with(|jail| {
            jail.set_env("RIO_OTEL_ENDPOINT", "");
            let (layer, guard) =
                build_otel_layer::<tracing_subscriber::Registry>("test-component").unwrap();
            assert!(layer.is_none(), "empty endpoint → no layer");
            assert!(guard.0.is_none(), "empty endpoint → no provider");
            Ok(())
        });
    }
}
