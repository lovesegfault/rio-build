# Phase 4: Production Hardness (Months 17-20)

**Goal:** Observability, reliability, performance tuning.

**Implements:** [Observability](../observability.md) (production pipeline), [Error Taxonomy](../errors.md) (hardened retry/poison)

## Tasks

- [ ] OpenTelemetry distributed tracing across all components
  - Trace spans: build submission -> scheduling -> worker assignment -> build execution -> output upload
  - Propagate trace context through gRPC and worker protocol
- [ ] Prometheus metrics
  - Build times (p50, p95, p99), queue depth, cache hit rates
  - Chunk deduplication ratio, transfer volumes, worker utilization
  - Scheduler: critical path accuracy, assignment latency
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
