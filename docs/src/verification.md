# Verification

## Protocol Conformance

- Live-daemon golden tests: each test starts an isolated nix-daemon, exchanges with it, and compares the response field-by-field against rio-build at the byte level
- No stored fixtures — tests always run against the current nix-daemon version, eliminating fixture staleness
- STDERR activity stripping handles daemon messages (START\_ACTIVITY/STOP\_ACTIVITY) that rio-build omits
- Fields that legitimately differ (version\_string, trusted) are skipped via a configurable skip list

> **Phase 4 deferral — multi-version Nix compatibility matrix:** Currently conformance tests run against the single Nix version pinned in `flake.nix`. Testing against multiple pinned Nix versions (2.20+ stable, unstable, Lix) and a nightly job against nixpkgs-unstable is deferred to Phase 4.

## Fuzzing

Security-critical protocol parsers must be fuzz-tested. Targets live in per-crate fuzz workspaces (`rio-nix/fuzz/`, `rio-store/fuzz/`):

- `wire_primitives` --- u64, padded strings, framed streams, empty strings, maximum sizes
- `opcode_parsing` --- each opcode's payload parsing (wopAddToStoreNar, wopBuildDerivation, etc.)
- `nar_parsing` --- NAR streaming reader with malformed input
- `narinfo_parsing` --- narinfo text format parser
- `derivation_parsing` --- `.drv` ATerm format parser (including `__structuredAttrs` with `__json`)
- `derived_path_parsing` --- DerivedPath wire format (`!`-separated `drvPath!output` strings)
- `build_result_parsing` --- BuildResult wire format (status, error message, timing, built outputs)
- `manifest_deserialize` (rio-store) --- chunk manifest deserialization
- Run continuously via `cargo-fuzz` / `libFuzzer`:
  - **PR tier:** 30s/target smoke run with seed corpus (`nix flake check` includes `checks.fuzz-smoke-*`)
  - **Nightly tier:** 10min/target deep run (`nix build .#fuzz-nightly-<target>`, or via `.#ci-slow` aggregate)
  - Corpus seeded from `rio-nix/fuzz/corpus/<target>/` and `rio-store/fuzz/corpus/<target>/` (committed seeds prefixed `seed-`; NAR seeds regenerable via `gen-nar-corpus.sh`)

## Unit Tests

- Wire format: roundtrip serialization for all protocol types (property tests via `proptest`)
- DAG scheduling: known graphs -> expected critical paths and worker assignments
- Scheduler invariants (proptest): for any DAG and completion sequence, no derivation is dispatched before all dependencies complete
- DAG merging: merging two DAGs produces correct dedup and shared-node priority inheritance
- FastCDC chunking: deterministic chunking, dedup verification, chunk/reassembly roundtrip
- CAS: put/get/gc correctness, content-indexed lookup, PutPath idempotency
- CA early cutoff: propagation through multi-level DAGs, mixed CA/input-addressed DAGs
- Narinfo: parse/generate roundtrip against known-good narinfo files
- Store path computation: verify against known nix store paths
- FUSE store: cache hit/miss behavior, LRU eviction, concurrent access

## Integration Tests

- `nix build --store ssh-ng://rio nixpkgs#hello` --- minimal end-to-end
- `nix build --builders 'ssh-ng://rio x86_64-linux'` --- build hook path
- `nix flake check --store ssh-ng://rio` --- checks output
- Multi-derivation chain (A -> B -> C) distributed across workers
- Cache hit path: second build of same derivation returns instantly
- Chunk dedup: build two similar packages, verify shared chunks
- Worker failure mid-build -> rescheduled to another worker
- CA early cutoff: change input that produces same output -> downstream skipped
- Binary cache: configure rio-store as substituter, `nix build` from cache
- Binary cache `/nix-cache-info` endpoint returns valid response
- Gateway handles concurrent client sessions
- Graceful shutdown: in-flight builds complete or are cleanly requeued
- Scheduler state recovery: kill scheduler mid-build, restart, verify builds resume
- FUSE store: build with cold cache, verify paths fetched from rio-store on demand

## Security Integration Tests

- `PutPath` with invalid assignment token (wrong derivation hash) -> rejected with `PERMISSION_DENIED`
- `PutPath` with expired assignment token -> rejected with `PERMISSION_DENIED`
- `PutPath` for output path not in assignment token's `expected_output_paths` -> rejected
- Cross-tenant data isolation: tenant A cannot query tenant B's builds via `AdminService`
- Cross-tenant data isolation: tenant A's `wopQueryPathInfo` returns 404 for tenant B's paths (when per-tenant scoping is enabled)
- DAG size exceeding `max_dag_size` -> rejected at the scheduler (not gateway --- the gateway forwards derivations; the scheduler enforces DAG-level limits)

> **Phase 4/5 deferral:** the following security tests are planned but not yet implemented:
> - `__noChroot` derivation rejection at gateway (currently enforced by nix-daemon's sandbox; gateway-level pre-check deferred)
> - JWT tenant token validation (expired `exp`, invalid signature) --- JWT issuance is Phase 5
> - mTLS client certificate rejection --- mTLS is Phase 4
> - FOD proxy domain allowlisting --- FOD egress proxy is Phase 4
> - Binary cache auth enforcement --- cache auth is Phase 4

## Chaos Testing (Phase 4)

- S3 timeout during PutPath -> verify orphan scanner reclaims stale manifests
- Worker disconnect during build -> verify reassignment to another worker
- PostgreSQL unavailability -> verify readiness probes gate traffic; verify recovery
- Scheduler crash during active builds -> verify state recovery algorithm
- Network partition between worker and scheduler -> verify completion buffering and retry

> **Phase 4 deferral:** chaos tests (including network fault injection via `toxiproxy` or equivalent) are not yet implemented.

## CI Pipeline Tiers

| Tier | Trigger | Tests | Aggregate target | Time Budget |
|------|---------|-------|------------------|-------------|
| PR | Every push | Unit tests, clippy, treefmt, live-daemon golden conformance tests, cargo-deny, 30s fuzz smoke ×8 | `.#ci-local-fast` | < 5 min |
| Merge | Every merge to main | + VM integration tests (see below) | `.#ci-fast` | < 15 min |
| Nightly | Scheduled | + 10min fuzz ×8 | `.#ci-slow` | < 60 min |
| Weekly | Scheduled | + EKS cluster tests, chaos tests, load tests | — | Unbounded |

> **Phase 4 deferral:** criterion benchmarks, Nix multi-version compatibility matrix, and `cargo-mutants` mutation testing are not yet wired into any CI tier.

## VM Integration Tests

NixOS-VM tests exercise full-system flows with real kernel features (FUSE, cgroup v2, overlayfs, k3s). Each test spins up 2--5 QEMU VMs via `nixosTest`. Run via `nix-build-remote .#ci-fast` (needs KVM):

| Test | VMs | Validates |
|------|-----|-----------|
| `vm-phase1a` | 2 | Read-only opcodes (store info, path-info, store ls, verify) |
| `vm-phase1b` | 3 | Single-worker build end-to-end |
| `vm-phase2a` | 4 | Distributed 2-worker build, FUSE assertions, metrics |
| `vm-phase2b` | 5 | OTLP trace export (Tempo), build log forwarding, config overlay |
| `vm-phase2c` | 5 | Size-class routing, chunked CAS, binary cache HTTP |
| `vm-phase3a` | 3 | k3s in-cluster: WorkerPool CRD → pod → FUSE → build → cgroup memory.peak → build\_history |

## Test Environment

| Dependency | Purpose |
|------------|---------|
| Nix daemon | Live-daemon golden conformance tests (auto-started per test via `fresh_daemon_socket()`) |
| PostgreSQL | Build state storage (ephemeral `initdb` per test via `rio-test-support::TestDb`; `PG_BIN` set by dev shell) |
| MinIO | S3 backend tests (VM tests use `services.minio`; unit tests use filesystem backend) |
| k3s | Kubernetes integration tests (bootstrapped in `vm-phase3a` VM; no external cluster needed) |

## Benchmarks

| Metric | Description | Target |
|--------|-------------|--------|
| **Scheduling latency** | Time from `nix build` invocation to first derivation starting on a worker | p99 < 5s |
| **Cache hit latency** | End-to-end time for a fully cached 1MB output | < 1s |
| **Throughput** | Derivations/second at 1, 5, 10, 20 workers | Document actual |
| **Cache hit rate** | Fraction of derivations served from store vs. built | Document actual |
| **Dedup ratio** | Chunk storage savings compared to full NAR storage | Document actual |
| **Transfer volume** | Bytes moved between store and workers per build | Document actual |
| **Critical path accuracy** | Predicted vs. actual build completion time | Within 2x |
| **Comparison baseline** | `nix build` with standard remote builders on same hardware | Document speedup |

**Benchmark workloads:**
- Small: `nixpkgs#hello` (few derivations, fast builds)
- Medium: `nixpkgs#firefox` (large DAG, mix of fast and slow)
- Large: NixOS system closure (thousands of derivations)
- Incremental: rebuild after single-file change (tests cache hit + locality)
