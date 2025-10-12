# Rio Metrics and Observability

## Current Status

### Tracing (Implemented)
- ✅ Using `tracing` crate with structured logging
- ✅ `#[tracing::instrument]` on critical paths:
  - SSH server operations (auth, channel open, data transfer)
  - BuildQueue operations (enqueue, dequeue, status updates)
  - Scheduler (builder selection)
  - Builder registration
  - gRPC handlers
- ✅ Structured fields: job_id, platform, channel_id, builder_id, derivation paths
- ✅ Span hierarchies for request tracing

### Metrics (TODO - Phase 3/4)

Recommended metrics to implement:

#### Counters
- `rio_builds_total{platform, status}` - Total builds (success/failure by platform)
- `rio_ssh_connections_total` - Total SSH connections
- `rio_grpc_requests_total{method}` - Total gRPC requests by method
- `rio_builder_registrations_total` - Builder registration attempts

#### Gauges
- `rio_queue_size` - Current build queue depth
- `rio_builders_active{platform}` - Number of active builders by platform
- `rio_builds_in_progress{platform}` - Currently running builds
- `rio_ssh_connections_active` - Active SSH connections

#### Histograms
- `rio_build_duration_seconds{platform}` - Build duration distribution
- `rio_queue_wait_time_seconds` - Time jobs spend in queue
- `rio_grpc_request_duration_seconds{method}` - gRPC request latency

#### Custom Metrics
- `rio_builder_capacity{builder_id, platform}` - Builder CPU/memory/disk capacity
- `rio_builder_utilization{builder_id}` - Builder current load (0.0-1.0)
- `rio_derivation_cache_hits_total` - Cache hit rate (future)

## Implementation Plan

### Phase 3: Add Prometheus Metrics
```toml
[dependencies]
prometheus = "0.13"
```

Key integration points:
1. Increment counters on: build start, build complete, build failed
2. Update gauges on: queue operations, builder registration/unregistration
3. Observe histograms on: build completion (measure duration)
4. HTTP `/metrics` endpoint on dispatcher (separate from gRPC)

### Example Integration

```rust
use prometheus::{Counter, Gauge, Histogram, Registry};

lazy_static! {
    static ref BUILD_COUNTER: Counter =
        Counter::new("rio_builds_total", "Total builds").unwrap();

    static ref QUEUE_GAUGE: Gauge =
        Gauge::new("rio_queue_size", "Build queue depth").unwrap();

    static ref BUILD_DURATION: Histogram =
        Histogram::new("rio_build_duration_seconds", "Build duration").unwrap();
}

// In BuildQueue::enqueue()
QUEUE_GAUGE.inc();

// In build completion
BUILD_COUNTER.inc();
BUILD_DURATION.observe(duration.as_secs_f64());
```

### Tracing Best Practices (Already Following)

✅ Use `#[instrument]` on all public async functions
✅ Skip large data with `skip()` parameter
✅ Extract key identifiers as fields
✅ Use structured fields, not string interpolation
✅ Use appropriate log levels (trace/debug/info/warn/error)

### Future: Distributed Tracing

For distributed tracing across dispatcher and builders:
- OpenTelemetry integration
- Trace context propagation via gRPC metadata
- Jaeger/Tempo backend for visualization
- Cross-service request correlation

### Logging Output Formats

Current: Human-readable console output via `tracing_subscriber::fmt()`

Future options:
- JSON logging for production (`tracing_subscriber::fmt().json()`)
- Integration with log aggregators (Loki, Elasticsearch)
- Separate log levels for different components

## Debugging Tips

### View traced execution:
```bash
RIO_LOG_LEVEL=debug cargo run -p rio-dispatcher
```

### View specific module:
```bash
RUST_LOG=rio_dispatcher::scheduler=trace cargo run -p rio-dispatcher
```

### Production deployment:
- Use JSON logging for parsing
- Export metrics to Prometheus
- Set up Grafana dashboards
- Alert on queue depth, error rates, builder failures

## Key Observability Questions We Can Answer

With current tracing:
- ✅ Which user/session is making requests?
- ✅ What derivations are being built?
- ✅ Which builders are selected for which platforms?
- ✅ Job flow through queue (enqueue → dequeue → dispatch)
- ✅ SSH connection lifecycle

With future metrics:
- How many builds per minute?
- What's the average build time by platform?
- Which builders are over/underutilized?
- What's the queue depth over time?
- Cache hit rates
- Error rates and types
