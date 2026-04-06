# rio-build

**A Kubernetes-native distributed build backend for Nix.**

rio-build speaks Nix's own wire protocol (`ssh-ng://`), so from Nix's perspective it *is* a remote store and builder — no client patches, no wrapper scripts. From Kubernetes' perspective it's an operator that schedules build DAGs across builder pods with chunk-deduplicated storage and cache-locality-aware placement.

```bash
nix build --store ssh-ng://rio .#your-package
```

## Why?

Nix derivations form a pure, deterministic DAG — theoretically ideal for distributed execution. In practice, the ecosystem's distributed build story is thin: Hydra is monolithic, `nix.buildMachines` is round-robin with no global view, and binary caches solve *distribution of results* but not *distribution of work*.

rio-build fills the gap:

- **Protocol-native** — implements the Nix worker protocol directly; zero custom client tooling
- **DAG-aware scheduling** — critical-path priority, transfer-cost-weighted locality, size-class routing
- **Chunked CAS** — FastCDC + BLAKE3 cross-build deduplication, inline fast-path for small NARs
- **FUSE builder stores** — lazy on-demand fetch from CAS, local SSD caching, per-build overlay isolation
- **CA-ready** — store schema and scheduler support content-addressed derivations from day one
- **Observable** — structured JSON logging, Prometheus metrics, OTLP tracing

## What it is *not*

rio-build is a **build execution backend**, not a CI system. Out of scope: Nix evaluation (bring `nix-eval-jobs`), VCS webhooks, PR status checks, notifications, Hydra-style jobsets, Darwin workers, recursive Nix. See [the design book](docs/src/introduction.md) for where each of these lives instead.

## Architecture

```
      nix build --store ssh-ng://rio ...
                      │
                      ▼  ssh-ng (Nix worker protocol)
           ┌─────────────────────┐
           │     rio-gateway     │  SSH server (russh), protocol handler,
           │                     │  translates wire ops → gRPC
           └─────┬──────────┬────┘
           gRPC  │          │  gRPC
                 ▼          ▼
    ┌──────────────────┐ ┌────────────────────────┐
    │  rio-scheduler   │ │       rio-store        │
    │                  │ │                        │
    │  global DAG      │ │  chunked CAS (FastCDC) │
    │  critical-path   │ │  PostgreSQL metadata   │
    │  locality scoring│ │  S3 blobs              │
    │  PostgreSQL      │ │  binary-cache HTTP     │
    └────────┬─────────┘ └───────────┬────────────┘
             │ BuildExecution        │
             │ (bidi gRPC stream)    │ gRPC + HTTP
             ▼                       ▼
   ┌──────────────────────────────────────────────┐
   │            rio-builder (K8s pods)            │
   │  FUSE /nix/store  •  per-build overlayfs     │
   │  synthetic SQLite db  •  nix-daemon sandbox  │
   │  cgroup v2 per-build resource tracking       │
   └──────────────────────────────────────────────┘

   rio-controller (K8s operator): BuilderPool/FetcherPool CRDs, StatefulSet
   reconciliation, autoscaling, drain-aware termination.
```

## Status

**Pre-1.0, under active development.** Phase-gated — see [`docs/src/phases-archive/`](docs/src/phases-archive/) for the roadmap. Currently through Phase 4a (multi-tenancy: JWT auth, per-tenant quotas, per-tenant signing keys, query-level isolation; CA cutoff-compare; CLI tooling).

> **Multi-tenancy warning:** Multi-tenant deployments with untrusted tenants are **unsafe before Phase 5** (incomplete query-level isolation). Deploy single-tenant or trusted-tenant until then.

## Crates

| Crate | Role |
|---|---|
| `rio-nix` | Nix wire protocol, ATerm/NAR parsers, store path types (leaf — no rio-* deps) |
| `rio-proto` | gRPC service definitions (tonic) |
| `rio-common` | Shared config, observability, limits, Arc<str> newtypes |
| `rio-gateway` | SSH server + Nix worker protocol frontend |
| `rio-scheduler` | DAG scheduler, critical-path priority, size-class routing |
| `rio-store` | Chunked CAS, narinfo signing, binary-cache HTTP server |
| `rio-builder` | Build executor, FUSE store, overlayfs isolation, cgroup metering |
| `rio-controller` | Kubernetes operator (BuilderPool/FetcherPool CRDs, autoscaler) |
| `rio-crds` | Kubernetes CRD types (kube-derive), shared between controller and CLI |
| `rio-cli` | Admin CLI (trigger GC, tenant mgmt, backfill, dry-run introspection) |
| `rio-bench` | Criterion benchmarks (wire parsers, chunker, scheduler hot paths) |
| `rio-test-support` | Ephemeral PostgreSQL bootstrap, mock gRPC, wire helpers |

## Development

The dev environment is a Nix flake. direnv activates it automatically via `.envrc`.

```bash
# Enter the dev shell (nightly Rust by default — cargo-fuzz works)
nix develop

# Build / test / lint
cargo build
cargo nextest run
cargo clippy --all-targets -- --deny warnings

# Format (rustfmt + nixfmt + taplo)
treefmt

# Full CI (build + clippy + nextest + docs + coverage + 2min fuzz + NixOS VM tests; needs KVM)
nix build .#ci
```

The default dev shell uses **nightly** Rust so `cargo fuzz` works directly. CI checks use **stable** via `rust-toolchain.toml` — nightly-only code is rejected by `nix flake check`. Use `nix develop .#stable` for CI-parity local dev.

PostgreSQL integration tests bootstrap their own ephemeral server via `rio-test-support` — no manual setup needed.

### Fuzzing

Fuzz targets live in per-crate `fuzz/` workspaces (separate `Cargo.lock` each):

```bash
cd rio-nix/fuzz && cargo fuzz run wire_primitives
# or via nix:
nix build .#checks.x86_64-linux.fuzz-smoke-wire_primitives   # 30s smoke
nix build .#fuzz-nightly-wire_primitives                     # 10min
```

### VM Tests

Per-phase NixOS VM tests validate end-to-end behavior against a real `nix` client:

```bash
nix build .#checks.x86_64-linux.vm-phase2a   # 4 VMs, distributed 2-builder build
nix build .#checks.x86_64-linux.vm-phase3a   # 3 VMs, k3s + controller + CRDs
```

## Design Book

Full design docs are in [`docs/src/`](docs/src/) (mdBook). Start with:

- [**Introduction**](docs/src/introduction.md) — goals, non-goals, landscape
- [**Architecture**](docs/src/architecture.md) — component diagram, data flows
- [**Crate Structure**](docs/src/crate-structure.md) — dependency graph, module layout
- [**Phases**](docs/src/phases-archive/) — implementation roadmap and task tracking
- [**Observability**](docs/src/observability.md) — metric naming, tracing conventions

## License

See [LICENSE](LICENSE).
