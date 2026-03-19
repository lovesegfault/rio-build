# Key Dependencies


| Crate | Purpose | Phase | Notes |
|-------|---------|-------|-------|
| `rio-nix` (ours) | Nix types: store paths, derivations, NAR, narinfo, wire protocol | 1 | Implemented from scratch; MIT/Apache-2.0 |
| `russh` | Async SSH server | 1 | For ssh-ng transport |
| `tracing` + `tracing-subscriber` | Structured logging | 1 | `features = ["env-filter", "json"]` |
| `metrics` + `metrics-exporter-prometheus` | Prometheus metrics | 1 | Counters, histograms for builds/chunks/latency |
| `tokio` | Async runtime | 1 | `features = ["full"]` |
| `thiserror` / `anyhow` | Error handling | 1 | Typed vs. context errors |
| `serde` / `serde_json` | Serialization | 1 | Config, API types |
| `tonic` / `prost` | gRPC framework + protobuf | 2 | Internal APIs. `tonic-health` adds the `grpc.health.v1.Health` service for K8s readiness probes. |
| `sqlx` | PostgreSQL + SQLite async driver | 2 | `default-features = false, features = ["runtime-tokio", "postgres", "sqlite", "macros", "migrate", "uuid"]`. SQLite feature is for the worker's synthetic per-build store DB. |
| `figment` | Layered configuration | 2b | TOML + `RIO_*` env overlay + clap CLI args. `features = ["toml", "env"]`. |
| `clap` | CLI argument parsing | 2b | `features = ["derive", "env"]`. Used by all binaries (gateway, scheduler, store, worker, controller) via figment's `CliArgs` pattern. |
| `fastcdc` | Content-defined chunking | 2 | For NAR deduplication |
| `sha2` | SHA-256 hashing | 1 | NAR hash verification, store path computation, content index. All Nix-facing hashes use SHA-256. |
| `blake3` | Fast cryptographic hashing | 2 | Chunk content addressing; also bloom filter hashing (Kirsch-Mitzenmacher double-hash from split 256-bit output). |
| `moka` | In-process LRU cache | 2c | Chunk cache in rio-store. Lock-free, weight-based eviction (tracks byte-size per entry so the 2GB cap is a real memory bound). `features = ["future"]`. |
| `zstd` / `async-compression` | Zstandard compression | 2 | Binary cache serves `.nar.zst`. `zstd` for buffered paths; `async-compression` for streaming `/nar/` endpoint (O(chunk) memory instead of O(NAR)). |
| `dashmap` | Concurrent hash map | 2 | Scheduler log ring buffers (written outside actor loop); singleflight for concurrent S3 fetches. |
| `ordered-float` | `Ord` wrapper for floats | 2c | Scheduler's `BinaryHeap` over f64 critical-path priority. f64 doesn't impl `Ord` (NaN); `OrderedFloat<f64>` is the standard workaround. |
| `axum` | HTTP server | 2 | Binary cache endpoint |
| `aws-sdk-s3` | S3 chunk storage | 2 | Production blob backend |
| `ed25519-dalek` | NAR signing/verification | 2 | Binary cache signature support. `features = ["rand_core", "pkcs8"]`. |
| `fuser` | FUSE filesystem | 2 | Per-worker `/nix/store` mount |
| `tracing-opentelemetry` | Distributed tracing | 2 (done) | Trace propagation across gRPC boundaries. `init_tracing` in `rio-common/observability.rs` + `inject_current`/`link_parent` in `rio-proto/interceptor.rs`. |
| `kube` + `kube-runtime` | K8s client, CRDs, operator framework | 3 | `features = ["runtime", "derive", "client"]` (kube 3.0) |
| `k8s-openapi` | K8s API types | 3 | `features = ["v1_35"]` (feature-gates which struct fields exist; pin to highest supported API version) |
| `schemars` | JSON Schema for CRDs | 3 | schemars 1.x (NOT 0.8 --- kube 3.0 requires the major break) |
| `rustls` | TLS provider selection | 3 | Direct dep to call `install_default()`: kube pulls rustls via ring, aws-sdk via aws-lc-rs; with both active, rustls 0.23 panics on first TLS use. |
| `cargo-deny` | License auditing, security advisories | 2 | Deny GPL-3.0+ per project policy; check advisories in CI. Dev tool, not a runtime dep. |
| `opentelemetry` + `opentelemetry-otlp` | OTLP pipeline | 2 (done) | Full OTLP/gRPC via `opentelemetry-otlp` 0.31, batch processor, `ParentBased(TraceIdRatioBased)` sampler. `RIO_OTEL_ENDPOINT` gate; unset = zero overhead. VM test uses Tempo (not Jaeger --- not packaged in nixpkgs); OTLP works with both. |
| TypeScript/React/Vite | Web dashboard | 5 | Separate `rio-dashboard/` project (not a Rust dep) |

## System Dependencies

| Dependency | Purpose | Phase | Notes |
|-----------|---------|-------|-------|
| `nix` (the command-line tool) | Workers invoke `nix-daemon --stdio` for sandboxed build execution | 2 | Runtime dependency, not a Rust crate. Must be present in worker container images. Protocol version must match `rio-nix`'s target (1.37+, Nix 2.20+). |

## Gotchas

- gRPC over HTTP/2 defeats L4 load balancers. Use a K8s headless Service + client-side DNS resolution, or an L7 proxy for inter-component gRPC.
- kube-rs: status updates trigger watch events --- use conditional updates to avoid infinite reconcile loops.
- rustls dual-provider panic: kube pulls ring, aws-sdk pulls aws-lc-rs. With both features active, rustls 0.23 can't auto-select a `CryptoProvider` and panics at first TLS use. Binaries that pull both (rio-controller) must call `rustls::crypto::aws_lc_rs::default_provider().install_default()` as the first line of `main`.
- `rio-nix` implements the Nix protocol from scratch --- reference Snix docs, Tweag blog, and Nix C++ source for protocol details. Target protocol version 1.37+ (Nix 2.20+).

## Risk Notes

- **`russh`**: Small maintainer team / low bus factor. Consider `thrussh` fork or `ssh-rs` as a fallback if `russh` becomes unmaintained. Pin minimum version and monitor for security patches.
- **`fuser`**: Small maintainer team. Monitor for security patches; the FUSE interface is security-sensitive (runs with `CAP_SYS_ADMIN`). Pin minimum version.

## Dependencies Considered and Rejected

- **`petgraph`**: DAG representation. Rejected --- the scheduler's graph is a simple adjacency-list `HashMap` with a custom `DerivationStatus` state machine; petgraph's algorithms (toposort, scc) don't match the incremental ready-queue pattern.
- **`memmap2`**: Zero-copy chunk access for filesystem backend. Rejected --- the filesystem backend uses buffered I/O; SIGBUS handling complexity not justified for a dev/test-only backend. Production uses S3 (streamed over HTTP, no mmap).
- **`ginepro`**: gRPC client-side load balancing via DNS. Rejected --- K8s headless Service DNS + tonic's default round-robin is sufficient for current scale. Revisit if service mesh deployment becomes a requirement.
- **`arbtest`**: Property testing via structure-aware fuzzing. Rejected --- `proptest` covers roundtrip serialization; `cargo-fuzz` covers parser fuzzing. No gap between them.
- **`testcontainers`**: Ephemeral Docker containers for integration tests. Rejected --- `rio-test-support::TestDb` bootstraps ephemeral PostgreSQL via `initdb` (Nix-provided, no Docker dependency). MinIO is exercised only in VM tests via `services.minio`.
