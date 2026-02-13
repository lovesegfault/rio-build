# Key Dependencies

> **Phase 1 dependencies are minimal:** rio-nix has zero external Nix deps. Only russh (SSH transport) and tracing are needed initially.

| Crate | Purpose | Phase | Notes |
|-------|---------|-------|-------|
| `rio-nix` (ours) | Nix types: store paths, derivations, NAR, narinfo, wire protocol | 1 | Implemented from scratch; MIT/Apache-2.0 |
| `russh` | Async SSH server | 1 | For ssh-ng transport |
| `tracing` + `tracing-subscriber` | Structured logging | 1 | `features = ["env-filter", "json"]` |
| `metrics` + `metrics-exporter-prometheus` | Prometheus metrics | 1 | Counters, histograms for builds/chunks/latency |
| `tokio` | Async runtime | 1 | `features = ["full"]` |
| `thiserror` / `anyhow` | Error handling | 1 | Typed vs. context errors |
| `serde` / `serde_json` | Serialization | 1 | Config, API types |
| `tonic` / `prost` | gRPC framework + protobuf | 2 | Internal APIs |
| `sqlx` | PostgreSQL async driver | 2 | `features = ["runtime-tokio", "postgres"]` |
| `fastcdc` | Content-defined chunking | 2 | For NAR deduplication |
| `sha2` | SHA-256 hashing | 1 | NAR hash verification, store path computation, content index. All Nix-facing hashes use SHA-256. |
| `blake3` | Fast cryptographic hashing | 2 | Chunk content addressing |
| `zstd` or `async-compression` | Zstandard compression | 2 | Binary cache serves `.nar.zst` compressed NARs |
| `dashmap` | Concurrent hash map | 2 | Singleflight pattern for deduplicating concurrent S3 fetches in rio-store |
| `petgraph` | Graph data structures | 2 | DAG representation |
| `memmap2` | Memory-mapped files | 2 | Zero-copy chunk access in store backends |
| `axum` | HTTP server | 2 | Binary cache endpoint |
| `aws-sdk-s3` | S3 chunk storage | 2 | Production blob backend |
| `ginepro` | gRPC client-side load balancing | 2 | DNS-based service discovery for tonic channels |
| `ed25519-dalek` | NAR signing/verification | 2 | Binary cache signature support |
| `fuser` | FUSE filesystem | 2 | Per-worker `/nix/store` mount (rio-fuse) |
| `rusqlite` | Synthetic store DB | 2 | Worker generates per-build SQLite DB |
| `tracing-opentelemetry` | Distributed tracing | 2 | Trace propagation across gRPC boundaries |
| `kube` + `kube-runtime` | K8s client, CRDs, operator framework | 3 | `features = ["runtime", "derive", "rustls-tls"]` |
| `k8s-openapi` | K8s API types | 3 | `features = ["latest"]` |
| `schemars` | JSON Schema for CRDs | 3 | Required by kube-derive |
| `cargo-deny` | License auditing, security advisories | 2 | Deny GPL-3.0+ per project policy; check advisories in CI |
| `clap` | CLI argument parsing | 4 | rio-cli |
| `opentelemetry` + `opentelemetry-otlp` | OTLP pipeline | 4 | Production Jaeger/Tempo backend |
| TypeScript/React/Vite | Web dashboard | 5 | Separate `rio-dashboard/` project (not a Rust dep) |

## System Dependencies

| Dependency | Purpose | Phase | Notes |
|-----------|---------|-------|-------|
| `nix` (the command-line tool) | Workers invoke `nix-daemon --stdio` for sandboxed build execution | 2 | Runtime dependency, not a Rust crate. Must be present in worker container images. Protocol version must match `rio-nix`'s target (1.37+, Nix 2.20+). |

## Gotchas

- gRPC over HTTP/2 defeats L4 load balancers. Use client-side LB (`ginepro`) or L7 proxy for inter-component gRPC.
- kube-rs: choose `rustls-tls` explicitly to avoid pulling both TLS stacks.
- kube-rs: status updates trigger watch events --- use conditional updates to avoid infinite reconcile loops.
- `rio-nix` implements the Nix protocol from scratch --- reference Snix docs, Tweag blog, and Nix C++ source for protocol details. Target protocol version 1.37+ (Nix 2.20+).

## Risk Notes

- **`russh`**: Small maintainer team / low bus factor. Consider `thrussh` fork or `ssh-rs` as a fallback if `russh` becomes unmaintained. Pin minimum version and monitor for security patches.
- **`fuser`**: Small maintainer team. Monitor for security patches; the FUSE interface is security-sensitive (runs with `CAP_SYS_ADMIN`). Pin minimum version.
- **`memmap2`**: Used for zero-copy chunk access in the filesystem store backend (development/testing). The production S3 backend streams chunks over HTTP and does not use memory-mapped I/O. SIGBUS risk applies only to the filesystem backend; for production, prefer buffered I/O or handle SIGBUS via a dedicated signal handler that converts it to an error result.
- **`ginepro`**: May be redundant with service mesh client-side load balancing (Istio/Linkerd). If a service mesh is deployed, disable ginepro's client-side LB to avoid double-balancing. Evaluate during Phase 2a whether both are needed.
- **`cargo-deny`**: Dev tool, not a runtime dependency. Run in CI to audit licenses (deny GPL-3.0+ per project licensing policy) and check for known security advisories in dependency tree.
