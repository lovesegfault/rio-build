# Implementation Phases

rio-build is designed to be implemented incrementally across five phases (with Phases 1, 2, and 3 each having sub-phases), each delivering a usable milestone.

| Phase | Timeline | Goal |
|-------|----------|------|
| [Phase 1a](./phases/phase1a.md) | Months 1-3 | Wire format, read-only store protocol, basic observability, FUSE platform validation |
| [Phase 1b](./phases/phase1b.md) | Months 3-6 | Build opcodes, end-to-end single-node build |
| [Phase 2a](./phases/phase2a.md) | Months 5-7 | Core distribution: protobuf, FIFO scheduler, workers, store, multi-process, basic retry |
| [Phase 2b](./phases/phase2b.md) | Months 7-9 | Observability, tracing, config management, container images, integration tests |
| [Phase 2c](./phases/phase2c.md) | Months 9-12 | Chunked CAS, intelligent scheduling, binary cache, CA data model |
| [Phase 3a](./phases/phase3a.md) | Months 12-15 | K8s operator, CRDs, FUSE-backed worker stores, deployment manifests |
| [Phase 3b](./phases/phase3b.md) | Months 15-17 | Production hardening: RBAC, NetworkPolicy, EKS, K8s-aware retry, GC, FOD proxy |
| [Phase 4](./phases/phase4.md) | Months 17-20 | Full observability, reliability hardening, CLI, Helm chart |
| [Phase 5](./phases/phase5.md) | Months 21-26 | CA early cutoff, multi-tenancy, web dashboard |

> **Note on overlap:** Phase 1b and 2a overlap by one month (months 5-6). This reflects the expectation that early Phase 2a scaffolding (protobuf definitions, project structure) can begin before Phase 1b is fully complete.

> **Timeline note:** The original 24-month estimate assumed no schedule slips. The revised timeline above budgets ~26 months with the Phase 3 split, providing more realistic scope per phase. Actual timelines depend on team size and will be adjusted as implementation progresses.
