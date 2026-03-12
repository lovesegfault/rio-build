//! W3C trace-context propagation across gRPC boundaries.
//!
//! Per `docs/src/observability.md:164-166`: trace context is propagated via
//! gRPC metadata using the W3C `traceparent` header. This module provides:
//!
//! - [`inject_current`]: copy the current span's trace context into a
//!   `tonic::Request` (client side, before sending).
//! - [`link_parent`]: recover a parent Context from an incoming
//!   `tonic::Request`'s metadata (server side, at handler entry).
//!
//! # Design: manual inject/extract, not a tonic Interceptor
//!
//! Tonic's `Interceptor` trait would change `connect_*` return types from
//! `XClient<Channel>` to `XClient<InterceptedService<Channel, F>>`. That's
//! a 62-callsite sweep across 19 files. Instead, we keep `Channel` and
//! call [`inject_current`] explicitly right before each outgoing RPC.
//!
//! This is slightly less automatic, but:
//! - Zero type churn.
//! - The injection point is VISIBLE in the code (you can grep for it).
//! - Works for RPCs that don't go through our `connect_*` helpers
//!   (e.g., test clients).
//!
//! Server-side is the same: [`link_parent`] + `Span::current().set_parent()`
//! at the top of each `#[instrument]`ed handler body. Tonic has no server-
//! interceptor trait that composes with `#[instrument]` anyway.
//!
//! # Usage
//!
//! **Client:**
//! ```ignore
//! let mut req = tonic::Request::new(SubmitBuildRequest { ... });
//! rio_proto::interceptor::inject_current(req.metadata_mut());
//! let resp = client.submit_build(req).await?;
//! ```
//!
//! **Server:**
//! ```ignore
//! #[instrument(skip(self, request), fields(rpc = "SubmitBuild"))]
//! async fn submit_build(&self, request: Request<...>) -> ... {
//!     rio_proto::interceptor::link_parent(&request);
//!     // ... handler body; the #[instrument] span is now a child of the
//!     //     client's span (same trace_id, parent span_id filled in).
//! }
//! ```

use opentelemetry::propagation::{Extractor, Injector};
use tonic::metadata::{MetadataKey, MetadataMap, MetadataValue};
use tracing_opentelemetry::OpenTelemetrySpanExt;

// r[impl obs.trace.w3c-traceparent]
/// Inject the current span's trace context as W3C `traceparent` header
/// into outgoing gRPC metadata.
///
/// No-op if there's no active span (e.g., called outside any
/// `#[instrument]` or `info_span!().entered()`). No-op-ish if the
/// propagator isn't registered (rio_common::observability::init_tracing
/// registers it unconditionally, so in practice this always works after
/// `init_tracing()`).
pub fn inject_current(metadata: &mut MetadataMap) {
    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut MetadataInjector(metadata));
    });
}

/// Returns the current span's trace_id as a 32-char lowercase hex string,
/// or empty if no span is active or the trace context is invalid.
///
/// Used by the gateway to emit `rio trace_id: {hex}` to the Nix client via
/// `STDERR_NEXT` — gives operators a grep handle for Tempo.
pub fn current_trace_id_hex() -> String {
    use opentelemetry::trace::TraceContextExt;
    let cx = tracing::Span::current().context();
    let trace_id = cx.span().span_context().trace_id();
    if trace_id == opentelemetry::trace::TraceId::INVALID {
        String::new()
    } else {
        // TraceId's Display impl is lowercase hex (32 chars for a 128-bit ID).
        format!("{trace_id:032x}")
    }
}

/// Returns the current span's W3C traceparent as a string, for embedding
/// in non-gRPC payloads (e.g., [`WorkAssignment.traceparent`]).
///
/// ssh-ng has no gRPC metadata channel, so the scheduler injects its
/// current span's traceparent directly into the `WorkAssignment` proto
/// message. The worker then parses this string via [`extract_traceparent`]
/// and parents its build-task span accordingly.
///
/// Returns an empty string if no span is active or no propagator is
/// registered — the worker treats empty as "create a fresh root span"
/// (same as the no-traceparent gRPC case).
///
/// [`WorkAssignment.traceparent`]: crate::types::WorkAssignment
pub fn current_traceparent() -> String {
    let mut carrier = std::collections::HashMap::new();
    let cx = tracing::Span::current().context();
    opentelemetry::global::get_text_map_propagator(|prop| {
        prop.inject_context(&cx, &mut carrier);
    });
    carrier.remove("traceparent").unwrap_or_default()
}

/// Extract a parent Context from a W3C traceparent string (e.g., from
/// `WorkAssignment.traceparent`). Pairs with [`current_traceparent`].
///
/// Empty/malformed traceparent → returns a no-op Context (fresh root).
pub fn extract_traceparent(traceparent: &str) -> opentelemetry::Context {
    if traceparent.is_empty() {
        return opentelemetry::Context::new();
    }
    let carrier: std::collections::HashMap<String, String> =
        [("traceparent".to_string(), traceparent.to_string())].into();
    opentelemetry::global::get_text_map_propagator(|prop| prop.extract(&carrier))
}

/// Create a tracing span parented by the given W3C traceparent string.
/// Bundles [`extract_traceparent`] + `OpenTelemetrySpanExt::set_parent`
/// so callers (e.g., `rio-worker`) don't need `tracing-opentelemetry`
/// as a direct dependency.
///
/// Empty traceparent → the span is a fresh root.
pub fn span_from_traceparent(name: &'static str, traceparent: &str) -> tracing::Span {
    let span = tracing::info_span!("span", otel.name = name);
    let parent = extract_traceparent(traceparent);
    let _ = span.set_parent(parent);
    span
}

/// Extract a parent Context from incoming gRPC metadata and link the
/// current span to it. Call this FIRST inside each `#[instrument]`ed
/// server handler.
///
/// The `#[instrument]` macro has already entered a span by the time the
/// handler body runs. `set_parent` retroactively stitches that span into
/// the client's trace — same trace_id, the client's span_id as parent.
/// Jaeger shows this as a contiguous trace across the gRPC hop.
///
/// No-op if the incoming metadata has no `traceparent` header (the
/// client isn't tracing, or it's a raw grpcurl call).
pub fn link_parent<T>(request: &tonic::Request<T>) {
    let parent = opentelemetry::global::get_text_map_propagator(|prop| {
        prop.extract(&MetadataExtractor(request.metadata()))
    });
    // set_parent returns Result<(), SetError> where SetError means the
    // span is disabled (filtered out by EnvFilter). Disabled span +
    // inbound traceparent = we WON'T propagate, which is correct (the
    // child span isn't being recorded anyway). Not an error to surface.
    let _ = tracing::Span::current().set_parent(parent);
}

// ---------------------------------------------------------------------------
// MetadataMap adapters for the OTel TextMapPropagator trait
// ---------------------------------------------------------------------------

/// Writes into tonic metadata. OTel's propagator calls `set("traceparent",
/// "00-{trace_id}-{span_id}-{flags}")`.
struct MetadataInjector<'a>(&'a mut MetadataMap);

impl Injector for MetadataInjector<'_> {
    fn set(&mut self, key: &str, value: String) {
        // tonic metadata keys are lowercase ASCII (HTTP/2 header rules).
        // W3C keys ("traceparent", "tracestate") already conform, but
        // defensive parse — a future propagator emitting "X-Trace-Id"
        // would fail this and we'd silently drop that header rather than
        // panicking on a hot path.
        if let Ok(key) = MetadataKey::from_bytes(key.as_bytes())
            && let Ok(value) = MetadataValue::try_from(value)
        {
            self.0.insert(key, value);
        }
    }
}

/// Reads from tonic metadata. OTel's propagator calls `get("traceparent")`.
struct MetadataExtractor<'a>(&'a MetadataMap);

impl Extractor for MetadataExtractor<'_> {
    fn get(&self, key: &str) -> Option<&str> {
        self.0.get(key).and_then(|v| v.to_str().ok())
    }

    fn keys(&self) -> Vec<&str> {
        // The W3C propagator only ever asks for "traceparent" + "tracestate"
        // by name (via get()), not via keys(). But the Extractor trait
        // requires keys() for other propagator impls (e.g., Baggage).
        self.0
            .keys()
            .filter_map(|k| match k {
                tonic::metadata::KeyRef::Ascii(a) => Some(a.as_str()),
                // Binary keys (grpc-*-bin) can't carry text-map propagation
                // data; skip.
                tonic::metadata::KeyRef::Binary(_) => None,
            })
            .collect()
    }
}

#[cfg(test)]
mod tests {
    use super::*;
    use opentelemetry::trace::{TraceContextExt, Tracer, TracerProvider};

    /// Round-trip: inject current → extract → same trace_id.
    ///
    /// This test has to set up a local tracer provider because the global
    /// one isn't installed in test builds (no init_tracing() call).
    /// We also need to register the W3C propagator (normally done by
    /// init_tracing; tests don't go through that).
    #[test]
    fn inject_then_extract_roundtrips_trace_id() {
        // Register the propagator (normally done by init_tracing).
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        // Build a non-no-op tracer. The default global tracer is a noop
        // that generates all-zero trace IDs; we need real IDs to assert
        // roundtrip. A bare SdkTracerProvider (no exporter) gives real IDs
        // without any network.
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        use opentelemetry::trace::{Tracer, TracerProvider};
        let tracer = provider.tracer("test");

        // Create an OTel span directly (not via tracing — we want to
        // control the context precisely for this test). Make it current.
        let span = tracer.start("test-span");
        let cx = opentelemetry::Context::current_with_span(span);
        let expected_trace_id = cx.span().span_context().trace_id();
        assert_ne!(
            expected_trace_id,
            opentelemetry::trace::TraceId::INVALID,
            "test tracer should produce a real trace ID"
        );

        // Inject into metadata using that context.
        let mut metadata = MetadataMap::new();
        opentelemetry::global::get_text_map_propagator(|prop| {
            prop.inject_context(&cx, &mut MetadataInjector(&mut metadata));
        });

        // The traceparent header should be present.
        let traceparent = metadata
            .get("traceparent")
            .expect("inject should add traceparent header");
        let tp_str = traceparent.to_str().expect("header should be valid UTF-8");
        // W3C format: 00-{32-hex trace_id}-{16-hex span_id}-{2-hex flags}
        assert!(
            tp_str.starts_with("00-"),
            "W3C traceparent should start with version 00: {tp_str}"
        );
        assert_eq!(
            tp_str.len(),
            55,
            "W3C traceparent should be exactly 55 chars: {tp_str}"
        );

        // Extract back out.
        let extracted = opentelemetry::global::get_text_map_propagator(|prop| {
            prop.extract(&MetadataExtractor(&metadata))
        });
        let extracted_trace_id = extracted.span().span_context().trace_id();
        assert_eq!(
            extracted_trace_id, expected_trace_id,
            "roundtrip should preserve trace_id"
        );
    }

    /// `inject_current` with NO active span should be a clean no-op
    /// (no panic, no "00-0000...-0000..." garbage header). The noop
    /// tracer's SpanContext is INVALID, and W3C propagator skips
    /// injection for invalid contexts.
    #[test]
    fn inject_current_no_active_span_is_noop() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        let mut metadata = MetadataMap::new();
        // No span entered — `Span::current()` returns the noop span with
        // an INVALID context.
        inject_current(&mut metadata);

        // W3C propagator should NOT inject an invalid context. If this
        // assertion fails, we'd be sending "00-00000000...-00" headers
        // and Jaeger would show a trace with trace_id=0 for every
        // untraced request.
        assert!(
            metadata.get("traceparent").is_none(),
            "no active span → no traceparent header injected"
        );
    }

    /// `link_parent` with no `traceparent` in metadata should be a no-op
    /// (the incoming request isn't traced; don't stitch into a bogus
    /// parent).
    #[test]
    fn link_parent_no_header_is_noop() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );

        let req: tonic::Request<()> = tonic::Request::new(());
        // No traceparent header in metadata.
        // This should not panic.
        link_parent(&req);
        // No assertion possible — set_parent with an empty context is
        // a no-op by design. If this panicked, the test would fail.
    }

    #[test]
    fn metadata_extractor_keys_skips_binary() {
        let mut meta = MetadataMap::new();
        meta.insert("ascii-key", "value".parse().unwrap());
        meta.insert_bin(
            "binary-key-bin",
            tonic::metadata::MetadataValue::from_bytes(&[0xde, 0xad]),
        );
        let extractor = MetadataExtractor(&meta);
        let keys = extractor.keys();
        assert!(keys.contains(&"ascii-key"));
        assert!(
            !keys.iter().any(|k| k.contains("binary")),
            "binary keys should be skipped: {keys:?}"
        );
    }

    /// `current_traceparent` with no active span returns empty string.
    /// The worker treats empty as "fresh root span".
    #[test]
    fn current_traceparent_no_span_is_empty() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        assert_eq!(current_traceparent(), "");
    }

    /// `current_traceparent` + `extract_traceparent` roundtrip preserves trace_id.
    #[test]
    fn current_traceparent_roundtrip() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        let provider = opentelemetry_sdk::trace::SdkTracerProvider::builder().build();
        let tracer = provider.tracer("test");

        let span = tracer.start("test-span");
        let cx = opentelemetry::Context::current_with_span(span);
        let expected_trace_id = cx.span().span_context().trace_id();
        assert_ne!(expected_trace_id, opentelemetry::trace::TraceId::INVALID);

        // Enter the context so current_traceparent() picks it up via
        // tracing::Span::current().context(). But we're using the OTel
        // context directly, not the tracing bridge — so inject manually
        // via the same machinery current_traceparent uses.
        let mut carrier = std::collections::HashMap::new();
        opentelemetry::global::get_text_map_propagator(|prop| {
            prop.inject_context(&cx, &mut carrier);
        });
        let tp = carrier.remove("traceparent").expect("should inject");

        // W3C format check.
        assert!(tp.starts_with("00-"), "{tp}");
        assert_eq!(tp.len(), 55, "{tp}");

        // extract_traceparent recovers the same trace_id.
        let extracted = extract_traceparent(&tp);
        assert_eq!(
            extracted.span().span_context().trace_id(),
            expected_trace_id
        );
    }

    /// `extract_traceparent` with empty string returns a no-op Context.
    #[test]
    fn extract_traceparent_empty_is_noop() {
        opentelemetry::global::set_text_map_propagator(
            opentelemetry_sdk::propagation::TraceContextPropagator::new(),
        );
        let cx = extract_traceparent("");
        assert_eq!(
            cx.span().span_context().trace_id(),
            opentelemetry::trace::TraceId::INVALID,
            "empty traceparent → fresh root (invalid trace_id in the no-op context)"
        );
    }
}
