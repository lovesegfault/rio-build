# Phase 4: Production Hardness (Months 17-20)

**Goal:** Observability, reliability, performance tuning.

**Implements:** [Observability](../observability.md) (production pipeline), [Error Taxonomy](../errors.md) (hardened retry/poison)

## Tasks

- [ ] OpenTelemetry distributed tracing — **completion pass**
  - W3C traceparent propagation across gRPC hops is implemented (Phase 2b: `rio-proto/src/interceptor.rs` + `rio-common/src/observability.rs`). Remaining work: audit span coverage (every user-visible latency path should have an `#[instrument]` span), add `link_parent()` calls to any handlers missing them, and verify end-to-end traces in Tempo for a real multi-worker build.
  - Propagate trace context through the **Nix worker protocol** (SSH channel, not gRPC — needs a side-band or STDERR metadata frame).
- [ ] Prometheus metrics — **completion pass**
  - Most metrics are already emitted (build times, queue depth, cache hits, dedup ratio, assignment latency, critical path accuracy — see [observability.md](../observability.md)). Remaining work: transfer volume metrics (bytes sent/received per component), worker utilization (cpu/mem fraction gauges), and Grafana dashboard JSON for the existing metrics.
- [ ] Enhanced structured logging (extend Phase 2b correlation IDs with full distributed tracing pipeline)
- [ ] Store garbage collection
  - Path-level GC: mark-and-sweep from GC roots (automated scheduling)
  - Chunk-level GC: reference counting
  - Configurable retention policies per tenant
- [ ] Production-hardened retry policy (backoff tuning, per-worker failure tracking)
- [ ] Poison derivation tracking and `AdminService.ClearPoison` endpoint (auto-skip after N failures; see [Error Taxonomy](../errors.md) for the retry and poison designs introduced in Phase 3)
- [ ] Rate limiting and backpressure at the gateway
- [ ] Adaptive cutoff learning (SITA-E): activate `CutoffRebalancer` to automatically adjust `WorkerPoolSet` class boundaries based on production workload data. Misclassification detection and EMA penalty feedback. See [ADR-015](../decisions/015-size-class-routing.md).
- [ ] `rio-cli`: operational commands (status, builds list, workers, GC trigger)
- [ ] Helm chart for production deployment (PostgreSQL, S3, all components)
- [ ] Load testing and benchmarking
  - Single-build latency at various DAG sizes
  - Throughput: derivations/second
  - Cache hit rate impact on total build time
  - Compare against Hydra on equivalent hardware

## Milestone

Pass all chaos tests (S3 timeout, PG unavailability, worker disconnect, scheduler crash).
