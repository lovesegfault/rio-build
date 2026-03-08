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
pub enum LogFormat {
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
        match s {
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
    // (C12) uses this to inject/extract regardless of whether spans are
    // exported. Cheap: it's a stateless parser/formatter.
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
    let Ok(endpoint) = std::env::var("RIO_OTEL_ENDPOINT") else {
        return Ok((None, OtelGuard(None)));
    };

    let sample_rate = std::env::var("RIO_OTEL_SAMPLE_RATE")
        .ok()
        .and_then(|s| s.parse::<f64>().ok())
        .unwrap_or(1.0)
        .clamp(0.0, 1.0);

    // OTLP gRPC span exporter. 0.31 API: SpanExporter::builder().with_tonic().
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

/// Histogram bucket boundaries for build-duration metrics (seconds).
///
/// Default Prometheus buckets top out at 10s — useless for Nix builds that
/// routinely run 1–60 minutes. These span 1s..2h with denser coverage in the
/// 1–10min range where most builds land.
const BUILD_DURATION_BUCKETS: &[f64] = &[
    1.0, 5.0, 15.0, 30.0, 60.0, 120.0, 300.0, 600.0, 1800.0, 3600.0, 7200.0,
];

/// Histogram bucket boundaries for `rio_scheduler_critical_path_accuracy`.
///
/// Ratio of actual/estimated build duration. `1.0` = perfect prediction,
/// values above `1.0` = underestimate (build took longer than predicted).
/// Bucket edges are chosen to give resolution around `1.0` and capture
/// long tails on both sides.
const CRITICAL_PATH_ACCURACY_BUCKETS: &[f64] = &[0.5, 0.75, 0.9, 1.0, 1.1, 1.25, 1.5, 2.0, 5.0];

/// Histogram bucket boundaries for controller reconcile latency (seconds).
///
/// Reconciles are mostly K8s API round-trips — expect 10–500ms normally,
/// seconds only under API-server stress. Default Prometheus buckets
/// actually work here but the low end (5ms) is wasted; this set trades
/// that for a 10s top bucket.
const RECONCILE_DURATION_BUCKETS: &[f64] = &[0.01, 0.05, 0.1, 0.25, 0.5, 1.0, 2.5, 5.0, 10.0];

/// Histogram bucket boundaries for scheduler assignment latency (seconds).
///
/// Time from a derivation becoming Ready to being assigned to a worker.
/// Healthy system: sub-millisecond to low-tens-of-ms. Seconds = backlog.
const ASSIGNMENT_LATENCY_BUCKETS: &[f64] = &[0.001, 0.005, 0.01, 0.05, 0.1, 0.5, 1.0, 5.0];

/// Initialize Prometheus metrics exporter.
///
/// This starts an HTTP server on the given address that serves `/metrics`.
///
/// Installs custom histogram bucket boundaries for known duration/ratio
/// metrics — the `metrics-exporter-prometheus` default buckets
/// (`[0.005..10.0]`) are tuned for HTTP request latencies and are useless
/// for build durations that span seconds to hours. See `observability.md`
/// for the full bucket table.
pub fn init_metrics(addr: std::net::SocketAddr) -> anyhow::Result<()> {
    use metrics_exporter_prometheus::{Matcher, PrometheusBuilder};

    // set_buckets_for_metric returns Result<Self, BuildError> — the only
    // error variant is EmptyBucketsOrQuantiles, which cannot fire for the
    // non-empty const slices above. Unwrap with `.expect()` is safe here,
    // but we propagate anyway to keep the error surface uniform.
    PrometheusBuilder::new()
        .set_buckets_for_metric(
            Matcher::Full("rio_scheduler_build_duration_seconds".to_string()),
            BUILD_DURATION_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("rio_worker_build_duration_seconds".to_string()),
            BUILD_DURATION_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("rio_scheduler_critical_path_accuracy".to_string()),
            CRITICAL_PATH_ACCURACY_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("rio_controller_reconcile_duration_seconds".to_string()),
            RECONCILE_DURATION_BUCKETS,
        )?
        .set_buckets_for_metric(
            Matcher::Full("rio_scheduler_assignment_latency_seconds".to_string()),
            ASSIGNMENT_LATENCY_BUCKETS,
        )?
        .with_http_listener(addr)
        .install()
        .map_err(|e| anyhow::anyhow!("failed to install Prometheus exporter: {e}"))?;

    tracing::info!(addr = %addr, "Prometheus metrics exporter started");
    Ok(())
}

/// Parse `RIO_LOG_FORMAT` environment variable, defaulting to JSON.
fn log_format_from_env() -> LogFormat {
    match std::env::var("RIO_LOG_FORMAT") {
        Ok(val) => match val.parse() {
            Ok(fmt) => fmt,
            Err(_) => {
                eprintln!(
                    "warning: invalid RIO_LOG_FORMAT={val:?}, valid options are 'json' or 'pretty'; defaulting to json"
                );
                LogFormat::default()
            }
        },
        Err(_) => LogFormat::default(),
    }
}

// r[verify obs.log.required-fields]
// r[verify obs.trace.w3c-traceparent]
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
}
