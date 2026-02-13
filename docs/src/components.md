# Components

rio-build is composed of seven main components. During early development (Phase 1-2), most backend components live as modules within the `rio-build` crate. They are split into separate crates as interfaces stabilize.

| Component | Role |
|-----------|------|
| [rio-gateway](./components/gateway.md) | Nix protocol frontend (SSH + worker protocol, multiple replicas) |
| [rio-scheduler](./components/scheduler.md) | DAG-aware build scheduler (leader-elected, streaming assignment) |
| [rio-worker](./components/worker.md) | Build executor with FUSE store, per-build overlay + synthetic SQLite DB |
| [rio-store](./components/store.md) | Chunked CAS with inline fast-path, binary cache server |
| [rio-controller](./components/controller.md) | Kubernetes operator (CRDs, autoscaling, RBAC, lifecycle) |
| [rio-proto](./components/proto.md) | gRPC service definitions (internal + external APIs) |
| [rio-dashboard](./components/dashboard.md) | Web dashboard (TypeScript SPA, Phase 5) |

Supporting crates (see [Crate Structure](./crate-structure.md) for details):

| Crate | Role |
|-------|------|
| [`rio-nix`](./crate-structure.md#phase-1-starting-point-3-crates) | Nix wire protocol, derivation parsing, NAR, store paths (from scratch, MIT/Apache-2.0) |
| [`rio-cli`](./crate-structure.md#phase-2-split-as-boundaries-stabilize) | Command-line interface for operators (Phase 4) |
| [`rio-common`](./crate-structure.md#target-architecture-9-crates--dashboard) | Shared utilities: config, observability, error types |
