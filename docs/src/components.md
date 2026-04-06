# Components

rio-build is composed of seven main components, each a dedicated workspace crate (the 3→9 crate split happened during Phase 1-2a; see [Crate Structure](./crate-structure.md)).

| Component | Role |
|-----------|------|
| [rio-gateway](./components/gateway.md) | Nix protocol frontend (SSH + worker protocol, multiple replicas) |
| [rio-scheduler](./components/scheduler.md) | DAG-aware build scheduler (leader-elected, streaming assignment) |
| [rio-builder](./components/builder.md) | Build executor with FUSE store, per-build overlay + synthetic SQLite DB |
| [rio-fetcher](./components/fetcher.md) | FOD-only executor (same binary, egress-open, hash-checked) |
| [rio-store](./components/store.md) | Chunked CAS with inline fast-path, binary cache server |
| [rio-controller](./components/controller.md) | Kubernetes operator (CRDs, autoscaling, RBAC, lifecycle) |
| [rio-proto](./components/proto.md) | gRPC service definitions (internal + external APIs) |
| [rio-dashboard](./components/dashboard.md) | Web dashboard (TypeScript SPA, Phase 5) |

Supporting crates (see [Crate Structure](./crate-structure.md) for details):

| Crate | Role |
|-------|------|
| [`rio-nix`](./crate-structure.md#rio-nix--nix-protocol-and-data-types) | Nix wire protocol, derivation parsing, NAR, store paths (from scratch, MIT/Apache-2.0) |
| `rio-cli` | Command-line interface for operators |
| [`rio-common`](./crate-structure.md#rio-common--shared-utilities) | Shared utilities: config, observability, error types |
