# Summary

[Introduction](./introduction.md)
[Contributing](./contributing.md)

# Design

- [System Architecture](./architecture.md)
- [Architecture Decisions](./decisions.md)
  - [ADR-001: Protocol-Level Integration](./decisions/001-protocol-integration.md)
  - [ADR-002: Evaluation Is External](./decisions/002-external-evaluation.md)
  - [ADR-003: Org-Scale Build Backend](./decisions/003-org-scale-backend.md)
  - [ADR-004: CA-Ready Design](./decisions/004-ca-ready-design.md)
  - [ADR-005: Worker Store Model](./decisions/005-worker-store-model.md)
  - [ADR-006: Custom Chunked CAS](./decisions/006-custom-chunked-cas.md)
  - [ADR-007: PostgreSQL for Scheduler State](./decisions/007-postgresql-scheduler-state.md)
  - [ADR-008: Custom Nix Protocol Implementation](./decisions/008-custom-nix-protocol.md)
  - [ADR-009: Predictive Cache Warming](./decisions/009-predictive-cache-warming.md)
  - [ADR-010: Protocol Version 1.37+](./decisions/010-protocol-version.md)
  - [ADR-011: Streaming Worker Model](./decisions/011-streaming-worker-model.md)
  - [ADR-012: Privileged Worker Pods](./decisions/012-privileged-worker-pods.md)
  - [ADR-013: Incremental Crate Structure](./decisions/013-incremental-crate-structure.md)
  - [ADR-014: Web Dashboard](./decisions/014-web-dashboard.md)
  - [ADR-015: Size-Class Routing](./decisions/015-size-class-routing.md)
  - [ADR-018: CA Resolution and dependentRealisations Schema](./decisions/018-ca-resolution.md)
- [Components](./components.md)
  - [rio-gateway](./components/gateway.md)
  - [rio-scheduler](./components/scheduler.md)
  - [rio-worker](./components/worker.md)
  - [rio-store](./components/store.md)
  - [rio-controller](./components/controller.md)
  - [rio-proto](./components/proto.md)
  - [rio-dashboard](./components/dashboard.md)
- [Data Flows](./data-flows.md)
- [Crate Structure](./crate-structure.md)

# Reference

- [Configuration](./configuration.md)
- [Error Taxonomy](./errors.md)
- [Security & Threat Model](./security.md)
- [Multi-Tenancy](./multi-tenancy.md)
- [Observability](./observability.md)
- [Key Dependencies](./dependencies.md)
- [Key Challenges](./challenges.md)
- [Integration Patterns](./integration.md)
- [Failure Modes](./failure-modes.md)
- [Capacity Planning](./capacity-planning.md)
- [Deployment](./deployment.md)
- [Verification](./verification.md)
- [Glossary](./glossary.md)

# Runbooks

- [GC Enablement Checklist](./runbooks/gc-enablement.md)
- [EKS Smoke Test](./runbooks/eks-smoke.md)

# Archive

- [Phase history (archived)](./phases-archive.md)
