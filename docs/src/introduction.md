# rio-build

**Kubernetes-Native Distributed Build Backend for Nix**

Nix's build model --- pure, deterministic derivations forming a DAG with content-addressable outputs --- is theoretically ideal for distributed execution. In practice, the ecosystem's distributed build story is weak:

- **Hydra** is a monolithic Perl/C++ application with a single-machine evaluator, round-robin scheduling, and aging Perl web UI. It doesn't scale horizontally and has poor observability.
- **Remote builders** (`nix.buildMachines`) use SSH-based 1:1 delegation with no global scheduling, no work-stealing, no awareness of the full DAG, and no cache locality optimization.
- **Binary caches** (cachix, attic, S3) solve distribution of *results* but not distribution of *work*.
- **Tvix** pioneers the right storage model (chunked CAS, Merkle DAGs) but doesn't provide a Kubernetes-native distributed scheduler.

rio-build fills this gap by implementing Nix's own remote protocols (`ssh-ng://` remote store and `--builders` remote builder) as a Kubernetes-native distributed build backend. From Nix's perspective, rio-build *is* a Nix store and builder. From Kubernetes' perspective, it's an operator that schedules build DAGs across worker pods with intelligent caching.

## Key Innovations

1. **Protocol-native** --- speaks Nix's own wire protocol, zero custom client tooling needed
2. **DAG-aware scheduling** with critical-path analysis and transfer-cost-weighted closure-locality affinity
3. **Chunked content-addressable store** with cross-build deduplication (FastCDC + BLAKE3), with inline fast-path for small NARs
4. **CA-ready design** --- store schema and scheduler support content-addressed derivations from day one; early cutoff optimization with per-edge tracking
5. **FUSE-backed worker stores** with lazy on-demand fetching from CAS, local SSD caching, and per-build overlay isolation with synthetic SQLite DB
6. **Observability** from Phase 1 (structured logging, metrics) with distributed tracing from Phase 2

## Non-Goals

rio-build is a **build execution backend**, not a complete CI/CD system. The following are explicitly out of scope:

- **Nix evaluation**: rio-build receives derivations via the protocol. It does not evaluate flakes, channels, or Nix expressions. Evaluation is performed by the client (e.g., `nix build`) or an external eval orchestrator (e.g., `nix-eval-jobs`).
- **VCS integration**: No GitHub/GitLab webhook receiver, no PR status reporting, no commit status updates. Use an external CI system (GitHub Actions, Buildkite, etc.) to trigger builds via `nix build --store ssh-ng://rio`.
- **Notification hooks**: No email, Slack, or webhook notifications on build completion. These belong in the CI layer above rio-build.
- **Eval scheduling / jobsets**: No equivalent of Hydra's jobset model. For periodic rebuilds, use cron + `nix-eval-jobs` + rio-build. See [Integration](./integration.md) for patterns.
- **macOS (Darwin) workers**: The worker architecture (FUSE, overlayfs, Linux namespaces) is Linux-only. Darwin builds require a separate worker architecture (future work).
- **Recursive Nix**: Derivations that invoke Nix internally (`__recursive` / `recursive-nix`) are not supported. Workers disable substitution and remote builders to prevent build hook recursion. See [worker configuration](./components/worker.md#worker-nix-configuration).

## When to Use rio-build

rio-build is the right choice when:

- You have **many machines' worth of builds** and need better scheduling than round-robin
- You want **chunk-level deduplication** in your binary cache (not just per-NAR)
- You already **run Kubernetes** and want builds to integrate with your existing infrastructure
- You need a **self-hosted** build backend (air-gapped environments, compliance, data residency)
- You want **zero client-side changes** --- `nix build --store ssh-ng://rio` just works

It is NOT the right choice when:

- You have a **single project with moderate build volume** --- use Cachix + GitHub Actions
- You **don't run Kubernetes** --- the operational overhead is significant
- You need **GitHub PR status checks** --- use a CI system with rio-build as the build backend

> **Multi-tenancy warning:** Multi-tenant deployments with untrusted tenants are **unsafe before Phase 5**. Prior to Phase 5, resource quotas are not enforced, per-tenant signing keys are not available, and data isolation relies on incomplete query-level filtering. Deploy pre-Phase-5 releases as single-tenant or in environments where all tenants are trusted. See [Multi-Tenancy](./multi-tenancy.md) for details.

## Landscape

| Tool | Category | Relationship to rio-build |
|------|----------|---------------------------|
| **Hydra** | Eval + build + cache | rio-build replaces the build execution and binary cache layers; an external eval orchestrator replaces the eval layer |
| **Tvix** | Nix reimplementation | Influenced rio-store's CAS design (MIT protobuf refs). Different goals: tvix is a Nix replacement, rio-build is a build backend |
| **Attic** | Binary cache | rio-store subsumes Attic's functionality (chunked CAS + binary cache HTTP). If you only need a cache, Attic is simpler |
| **Cachix / Garnix** | Managed CI/cache | SaaS alternatives. rio-build is for self-hosted deployments |
| **GitHub Actions + Nix** | CI with Nix | The "wrap existing CI" approach. rio-build offers better scheduling but higher operational cost |
| **ofborg** | Nixpkgs CI | Specialized for nixpkgs PR review. rio-build could serve as ofborg's build execution backend |
