# Implementation Phases

rio-build is designed to be implemented incrementally across five phases (with Phases 1-4 each having sub-phases), each delivering a usable milestone.

| Phase | Timeline | Goal |
|-------|----------|------|
| [Phase 1a](./phases-archive/phase1a.md) | Months 1-3 | Wire format, read-only store protocol, basic observability, FUSE platform validation |
| [Phase 1b](./phases-archive/phase1b.md) | Months 3-6 | Build opcodes, end-to-end single-node build |
| [Phase 2a](./phases-archive/phase2a.md) | Months 5-7 | Core distribution: protobuf, FIFO scheduler, workers, store, multi-process, basic retry |
| [Phase 2b](./phases-archive/phase2b.md) | Months 7-9 | Observability, tracing, config management, container images, integration tests |
| [Phase 2c](./phases-archive/phase2c.md) | Months 9-12 | Chunked CAS, intelligent scheduling, binary cache, CA data model |
| [Phase 3a](./phases-archive/phase3a.md) | Months 12-15 | K8s operator, CRDs, FUSE-backed worker stores, deployment manifests |
| [Phase 3b](./phases-archive/phase3b.md) | Months 15-17 | Production hardening: RBAC, NetworkPolicy, EKS, K8s-aware retry, GC, FOD proxy |
| [Phase 4a](./phases-archive/phase4a.md) | Months 17-18 | Observability completion, multi-tenancy foundation, admin RPCs, poison persistence |
| [Phase 4b](./phases-archive/phase4b.md) | Months 18-20 | GC correctness (NAR refs scanner), per-tenant GC, rio-cli, Helm, rate limiting, FUSE circuit breaker |
| [Phase 4c](./phases-archive/phase4c.md) | Months 20-22 | SITA-E adaptive cutoffs, WorkerPoolSet CRD, VM tests, Grafana, rio-bench, doc sync |
| [Phase 5](./phases-archive/phase5.md) | Months 22-28 | CA early cutoff, multi-tenancy enforcement, web dashboard, chaos testing |

> **Note on overlap:** Phase 1b and 2a overlap by one month (months 5-6). This reflects the expectation that early Phase 2a scaffolding (protobuf definitions, project structure) can begin before Phase 1b is fully complete.

> **Timeline note:** The original 24-month estimate assumed no schedule slips. The revised timeline above budgets ~26 months with the Phase 3 split, providing more realistic scope per phase. Actual timelines depend on team size and will be adjusted as implementation progresses.
