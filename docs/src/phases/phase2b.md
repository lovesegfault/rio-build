# Phase 2b: Observability + Packaging (Months 7-9)

**Goal:** Production-quality observability, container images, and integration testing.

**Implements:** [Observability](../observability.md), [Configuration](../configuration.md)

## Tasks

- [ ] Build log streaming with batching and rate limiting (worker → scheduler → gateway)
  - Worker buffers up to 64 lines or 100ms per batch
  - Scheduler maintains per-derivation ring buffer for active log serving
  - Async flush to S3 on derivation completion
  - Log rate limiting per build (`log_rate_limit`, `log_size_limit`)
  - Honor 100ms BATCH_TIMEOUT during silent build periods (spawn reader into owned task, select! on channel + interval; naive timeout-wrap is cancel-unsafe)
- [ ] Build correlation IDs (UUID v7): generated at `SubmitBuild`, propagated via gRPC metadata, included in all log spans
- [ ] `tracing-opentelemetry`: trace propagation across gRPC boundaries (export to stdout or local Jaeger in dev)
- [ ] Container images for each component (Nix-based via `dockerTools.buildLayeredImage`)
- [ ] Configuration management (TOML config + env var overlay with `RIO_` prefix)
- [ ] `cargo-deny` integration: license auditing (deny GPL-3.0), security advisory checking
- [ ] Integration test: multi-derivation build (A → B → C) across 3+ workers
- [ ] Integration test: cache hit path (second build is instant)
- [ ] gRPC contract tests for each service boundary

## Milestone

Traces visible in Jaeger for a multi-worker build; container images build via `nix build .#dockerImages`.
