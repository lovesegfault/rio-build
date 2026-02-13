# Architecture Decisions

Key design decisions are recorded as Architecture Decision Records (ADRs). Each ADR documents the context, decision, alternatives considered, and consequences.

| ADR | Decision | Status |
|-----|----------|--------|
| [ADR-001](./decisions/001-protocol-integration.md) | Protocol-level integration (`ssh-ng://` + build hook) | Accepted |
| [ADR-002](./decisions/002-external-evaluation.md) | Evaluation is external (Nix evaluates, rio-build executes) | Accepted |
| [ADR-003](./decisions/003-org-scale-backend.md) | Org-scale build backend (replaces Hydra execution + cache) | Accepted |
| [ADR-004](./decisions/004-ca-ready-design.md) | CA-ready data model from Phase 2c, full CA in Phase 5 | Accepted |
| [ADR-005](./decisions/005-worker-store-model.md) | Worker store: FUSE + overlay + synthetic SQLite DB | Accepted |
| [ADR-006](./decisions/006-custom-chunked-cas.md) | Custom chunked CAS (FastCDC, BLAKE3 chunks, SHA-256 Nix hashes) | Accepted |
| [ADR-007](./decisions/007-postgresql-scheduler-state.md) | PostgreSQL for scheduler state (separate schema, same cluster) | Accepted |
| [ADR-008](./decisions/008-custom-nix-protocol.md) | Custom Nix protocol implementation in rio-nix (MIT/Apache-2.0) | Accepted |
| [ADR-009](./decisions/009-predictive-cache-warming.md) | Predictive cache warming via scheduler prefetch hints | Accepted |
| [ADR-010](./decisions/010-protocol-version.md) | Protocol version 1.37+ (Nix 2.20+) | Accepted |
| [ADR-011](./decisions/011-streaming-worker-model.md) | Streaming worker model (bidirectional BuildExecution RPC) | Accepted |
| [ADR-012](./decisions/012-privileged-worker-pods.md) | Privileged worker pods (CAP_SYS_ADMIN, custom seccomp) | Accepted |
| [ADR-013](./decisions/013-incremental-crate-structure.md) | Incremental crate structure (3 crates to 9) | Accepted |
| [ADR-014](./decisions/014-web-dashboard.md) | Web dashboard (React SPA, gRPC-Web, Phase 5) | Accepted |
| [ADR-015](./decisions/015-size-class-routing.md) | Size-class routing with adaptive SITA-E cutoffs | Accepted |
