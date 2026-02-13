# Verification

## Protocol Conformance

- Record bytes from real nix-daemon as golden tests (scripted for reproducibility across Nix versions)
- Replay recorded sessions against rio-build, compare byte-for-byte
- Nix version compatibility: test against Nix 2.20+ stable and unstable, and Lix
- Run conformance tests against multiple pinned Nix versions in CI
- Nightly job tests against latest Nix from nixpkgs-unstable and alerts on failures

## Fuzzing

Security-critical protocol parsers must be fuzz-tested:

- `fuzz/wire_primitives` --- u64, padded strings, framed streams, empty strings, maximum sizes
- `fuzz/opcodes` --- each opcode's payload parsing (wopAddToStoreNar, wopBuildDerivation, etc.)
- `fuzz/nar_reader` --- NAR streaming reader with malformed input
- `fuzz/narinfo_parser` --- narinfo text format parser
- `fuzz/derivation_parser` --- `.drv` ATerm format parser (including `__structuredAttrs` with `__json`)
- Run continuously via `cargo-fuzz` / `libFuzzer`, not just on PR

## Unit Tests

- Wire format: roundtrip serialization for all protocol types (fuzz with `proptest` and `arbtest`)
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
- `__noChroot` derivation rejected at gateway with `STDERR_ERROR` before reaching worker
- JWT tenant token with expired `exp` -> `UNAUTHENTICATED` on all downstream RPCs
- JWT tenant token with invalid signature -> `UNAUTHENTICATED`
- mTLS: connection with wrong client certificate -> rejected at gRPC layer
- FOD proxy: request to non-allowlisted domain -> rejected with 403
- Binary cache: unauthenticated request to `/nar/` endpoint -> rejected (when auth is required)
- DAG size exceeding `max_dag_size` -> rejected at gateway before reaching scheduler

## Chaos Testing (Phase 4)

- S3 timeout during PutPath -> verify orphan scanner reclaims stale manifests
- Worker disconnect during build -> verify reassignment to another worker
- PostgreSQL unavailability -> verify readiness probes gate traffic; verify recovery
- Scheduler crash during active builds -> verify state recovery algorithm
- Network partition between worker and scheduler -> verify completion buffering and retry
- Use `toxiproxy` or equivalent for network fault injection between components

## CI Pipeline Tiers

| Tier | Trigger | Tests | Time Budget |
|------|---------|-------|-------------|
| PR | Every push | Unit tests, clippy, treefmt, wire format golden tests, cargo-deny | < 5 min |
| Merge | Every merge to main | + Integration tests with testcontainers (PostgreSQL, MinIO) | < 15 min |
| Nightly | Scheduled | + Nix version compatibility, criterion benchmarks, fuzzing corpus refresh | < 60 min |
| Weekly | Scheduled | + EKS cluster tests, chaos tests, load tests, cargo-mutants on scheduler+store | Unbounded |

## Test Environment

| Dependency | Purpose |
|------------|---------|
| Nix daemon | Golden protocol conformance tests |
| PostgreSQL | Build state storage (testcontainers, auto-started) |
| MinIO | S3 backend tests (testcontainers, auto-started) |
| kind / EKS cluster | Phase 3+ Kubernetes integration tests |

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
