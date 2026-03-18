# Plan 0079: OTLP tracing layer + W3C traceparent propagation

## Design

Before this plan, each component emitted structured JSON logs with spans, but spans in one process had no link to spans in another. A `SubmitBuild` in the gateway and the resulting `handle_merge_dag` in the scheduler showed up as unrelated root traces in Jaeger. This plan stitches them.

**OTLP layer** (`78b4b3f`): `init_tracing(component) → OtelGuard` replaces `init_from_env()`. Registry-based layered subscriber: EnvFilter + fmt (JSON/pretty via `RIO_LOG_FORMAT`, unchanged) + `Option<Box<dyn Layer>>` OTel layer (Some if `RIO_OTEL_ENDPOINT` set, None otherwise — `Layer` is impl'd for `Option<L>` so `with(None)` is effectively a no-op).

OTel layer: `SpanExporter::builder().with_tonic().with_endpoint()` (0.31 API), `SdkTracerProvider` with batch processor (buffers + interval flush, avoids per-span network roundtrip). `service.name = component` — this is the Jaeger service dropdown. `ParentBased(TraceIdRatioBased(RIO_OTEL_SAMPLE_RATE))` sampler: children inherit parent's decision, so 0.1 means 10% of **builds** traced, and within a traced build ALL spans captured. `OtelGuard` holds the `SdkTracerProvider` — the layer holds a tracer *derived from* the provider, not the provider itself, so a dropped provider means spans go to `/dev/null`. `Drop::drop` calls `force_flush()` + `shutdown()`.

Type-dance: `build_otel_layer` returns `Box<dyn Layer<S>>` generic over `S`. Concrete `OpenTelemetryLayer<Registry, Tracer>` would hard-code `Registry` and fail to compose over `Layered<EnvFilter, Layered<fmt, Registry>>`.

**W3C traceparent inject/extract** (`7893bb5`): manual, NOT a tonic `Interceptor`. An interceptor would change `connect_*` return types from `XClient<Channel>` to `XClient<InterceptedService<Channel, F>>` — a 62-callsite sweep across 19 files. Instead: `inject_current(metadata)` (client-side, before send; copies current span's context as W3C `traceparent` header) and `link_parent(&request)` (server-side, first line of `#[instrument]`ed handler; extracts traceparent, `set_parent()`s the already-entered span). Both no-ops when there's nothing to do: no active span → no header injected (W3C propagator skips INVALID contexts — critical, or every untraced request would send `00-00...0-00...0-00` and Jaeger shows a garbage trace_id=0). No incoming traceparent (client isn't tracing, or grpcurl) → no-op `set_parent(empty context)`.

**Wire-up** (`17c656f`): `link_parent` on 11 `#[instrument]`ed handlers (scheduler: SubmitBuild/WatchBuild/QueryBuildStatus/CancelBuild/BuildExecution/Heartbeat; store: PutPath/GetPath/QueryPathInfo/FindMissingPaths; admin: GetBuildLogs). `inject_current` at THE critical hop: gateway → `scheduler.submit_build()`. Gateway is the trace ROOT (Nix SSH client doesn't speak W3C). `SubmitBuild` also gets `build_id = tracing::field::Empty` in `#[instrument]`, filled via `Span::current().record()` once the v7 UUID is generated (per `observability.md:204`). Other client call sites (scheduler→store, worker→store) not injected in this commit — follow-up in a later phase once the gateway→scheduler leg is validated.

## Files

```json files
[
  {"path": "rio-common/src/observability.rs", "action": "MODIFY", "note": "init_tracing(component)→OtelGuard; Option<Box<dyn Layer>>; ParentBased(TraceIdRatioBased); force_flush on Drop"},
  {"path": "rio-proto/src/interceptor.rs", "action": "NEW", "note": "inject_current(metadata), link_parent(&request); W3C traceparent; no-op on INVALID context"},
  {"path": "rio-proto/src/lib.rs", "action": "MODIFY", "note": "mod interceptor"},
  {"path": "rio-proto/Cargo.toml", "action": "MODIFY", "note": "opentelemetry, tracing-opentelemetry deps"},
  {"path": "rio-gateway/src/main.rs", "action": "MODIFY", "note": "init_tracing(\"gateway\"); hold OtelGuard"},
  {"path": "rio-gateway/src/handler/build.rs", "action": "MODIFY", "note": "inject_current before scheduler.submit_build (THE critical hop — gateway is trace root)"},
  {"path": "rio-scheduler/src/main.rs", "action": "MODIFY", "note": "init_tracing(\"scheduler\"); hold OtelGuard"},
  {"path": "rio-scheduler/src/grpc/mod.rs", "action": "MODIFY", "note": "link_parent on 6 handlers; SubmitBuild build_id=field::Empty+record()"},
  {"path": "rio-scheduler/src/admin.rs", "action": "MODIFY", "note": "link_parent on GetBuildLogs"},
  {"path": "rio-store/src/main.rs", "action": "MODIFY", "note": "init_tracing(\"store\"); hold OtelGuard"},
  {"path": "rio-store/src/grpc.rs", "action": "MODIFY", "note": "link_parent on PutPath/GetPath/QueryPathInfo/FindMissingPaths"},
  {"path": "rio-worker/src/main.rs", "action": "MODIFY", "note": "init_tracing(\"worker\"); hold OtelGuard"}
]
```

## Tracey

Predates tracey adoption (phase-3a `f3957f7`). No markers added; `tracey_covered=0`. Retroactive: `r[obs.otel.layer]`, `r[obs.otel.traceparent]`.

## Entry

- Depends on **P0076** (figment config): all 4 `main.rs` migrated in P0076; adding `init_tracing` and `OtelGuard` here rebases on that.
- Depends on **P0078** (log pipeline): `rio-scheduler/src/admin.rs` was created in P0078; `link_parent` wired into it here.

## Exit

Merged as `78b4b3f..17c656f` (3 commits). `.#ci` green at merge.
