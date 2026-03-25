# Verification

## Protocol Conformance

- Live-daemon golden tests: each test starts an isolated nix-daemon, exchanges with it, and compares the response field-by-field against rio-build at the byte level
- No stored fixtures — tests always run against the current nix-daemon version, eliminating fixture staleness
- STDERR activity stripping handles daemon messages (START\_ACTIVITY/STOP\_ACTIVITY) that rio-build omits
- Fields that legitimately differ (version\_string, trusted) are skipped via a configurable skip list

### Multi-Nix compatibility matrix

Per-push CI runs conformance tests against the single Nix version pinned in `flake.nix` (`inputs.nix`). The **weekly** tier runs the full matrix via `.#golden-matrix` — four daemon variants:

| Variant | Source | Notes |
|---------|--------|-------|
| `nix-pinned` | `inputs.nix` (2.34.x) | Same as per-push CI — sanity row |
| `nix-stable` | `github:NixOS/nix/2.20-maintenance` | Oldest supported protocol minor |
| `nix-unstable` | `github:NixOS/nix` (master HEAD) | Surfaces breakage early |
| `lix` | `git.lix.systems/lix-project/lix` | Fork — diverges on feature set, version string, opcode additions |

The weekly run bumps `nix-unstable` and `lix` inputs before building (via `--override-input` in `weekly.yml`); `nix-pinned` and `nix-stable` remain at their locked revs. Dev and CI always use locked revs — only the weekly run tests tip.

Test harness reads `RIO_GOLDEN_DAEMON_BIN` (absolute daemon path) and `RIO_GOLDEN_DAEMON_VARIANT` (skip-list key). Per-variant skips live in `rio-gateway/tests/golden/daemon.rs::VARIANT_SKIP` — each row is `(variant, test_name, reason)`. The `reason` field is load-bearing: it documents WHY so the skip can be removed once upstream converges.

Known Lix divergences:
- Version string format (`"Lix N.N.N"` vs `"nix (Nix) N.N.N"`) — handled by the existing `SKIP_FIELDS` mechanism at field level
- Daemon feature set advertised during handshake — `test_golden_live_handshake` skipped for Lix until the comparator tolerates feature-set supersets

`nix build .#golden-matrix` produces a linkfarm keyed by variant; `ls result/` shows one dir per daemon. Cold-cache build time ~60-90min (three full Nix source-tree builds); subsequent warm runs are minutes.

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
  - **CI tier:** 2min/target run with seed corpus (`nix flake check` includes `checks.fuzz-*`)
  - **Deep runs:** `cd <crate>/fuzz && cargo fuzz run <target>` in the dev shell — libFuzzer accumulates corpus in `./corpus/`
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

## Functional Tests

Gateway wire protocol against **real `rio-store`** (`StoreServiceImpl` + ephemeral PostgreSQL) — the `RioStack` fixture at `rio-gateway/tests/functional/`. No k8s, no VM, no KVM; runs in `cargo nextest` alongside unit tests (sub-second). Catches bugs `MockStore` hides:

- `wopAddToStoreNar` → `wopQueryPathInfo` with real hash verification (`MockStore` accepts any hash; real store runs `validate_nar_digest`)
- `wopAddMultipleToStore` → `wopNarFromPath` through real FastCDC chunk + PG manifest + reassembly (`MockStore` is `HashMap` insert/get — byte-identical by construction, not by correctness)
- Reference chains: first tests to send non-empty `references` on the wire (`wire_opcodes/` always sends `NO_STRINGS`)

Scenarios ported from Lix [`functionaltests2`](https://git.lix.systems/lix-project/lix/src/branch/main/tests/functional2). Port is **scenario** (what's being proved), not **invocation shape** (Lix's harness is nix-CLI, rio's is wire-protocol). White-box assertions query PG directly (`narinfo`, `manifests`) to prove the graph is real — not an in-memory echo.

**Coverage:** tranche 1 is store-roundtrip (put/get/query). Tranche 2 (CA builds, refscan, trustless remote) needs real scheduler; tranche 4 (ssh-ng transport) needs russh fixture. Survey at `.claude/work/plan-0305-functional-tests-tranche1.md § Survey output`.

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

> **Implemented:** security VM test fragments cover JWT validation, mTLS client-cert rejection, binary-cache auth (`nix/tests/scenarios/security.nix`); FOD proxy domain allowlist (`nix/tests/scenarios/fod-proxy.nix`); `__noChroot` gateway pre-check (`r[gw.reject.nochroot]`).

## Chaos Testing

- S3 timeout during PutPath -> verify orphan scanner reclaims stale manifests
- Worker disconnect during build -> verify reassignment to another worker
- PostgreSQL unavailability -> verify readiness probes gate traffic; verify recovery
- Scheduler crash during active builds -> verify state recovery algorithm
- Network partition between worker and scheduler -> verify completion buffering and retry

> **Implemented:** toxiproxy fault-injection chaos harness at [`nix/tests/scenarios/chaos.nix`](../../nix/tests/scenarios/chaos.nix).

## CI Pipeline Tiers

| Tier | Trigger | Tests | Aggregate target | Time Budget |
|------|---------|-------|------------------|-------------|
| CI | Every push | Unit tests, functional tests (real rio-store), clippy, treefmt, live-daemon golden conformance tests, cargo-deny, 2min fuzz ×8, VM integration tests | `.#ci` | < 20 min |
| Weekly | Scheduled | + golden-matrix (4 daemons), mutation testing, EKS cluster tests, chaos tests, load tests | `.#golden-matrix`, `.#mutants` | Unbounded |

> **Implemented:** criterion benchmarks live in the `rio-bench` crate.

## Mutation Testing

`cargo-mutants` mutates source — swap `<` for `<=`, delete a statement, replace a return value with `Default::default()` — reruns the test suite, and flags mutations that **survive** (the tests still pass). A surviving mutant is code the tests don't actually constrain. tracey answers "is this spec rule covered"; mutants answers "does the test that covers it actually catch bugs." Complementary signals.

**Weekly tier, not per-push.** Mutation testing is O(mutations × test-suite-time); for the scoped target set (~320 mutations, scheduler state machine / wire primitives / ATerm parser / HMAC / manifest) it's hours per run. The `.github/workflows/weekly.yml` `mutants` job builds `.#mutants` and surfaces caught/missed counts in the job summary. **Missed-count is a trend metric, not a gate** — the job does not fail on nonzero. Diff week-over-week; an increase means a recent change weakened a test or introduced untested code.

**Scoping** lives in [`.config/mutants.toml`](../../.config/mutants.toml): `examine_globs` lists high-signal files where a surviving mutant is a genuine gap (not "you didn't test your tracing span"). `exclude_re` filters out tracing/metric calls — those are already covered by the per-crate `metrics_registered` test. `cap_lints = true` prevents the `--deny warnings` policy from marking mutations unviable before a test can kill them.

| Invocation | What |
|---|---|
| `cargo xtask mutants` | Local run against `$PWD` with `--in-place`. Commit/stash first — a `^C` mid-mutation can leave a mutated file behind. Results in `./mutants.out/`. |
| `nix build .#mutants` | Hermetic (vendored deps, pinned toolchain). `result/mutants.out/outcomes.json` + `result/{caught,missed}-count`. Week-over-week comparable. |
| `cargo mutants --list --config .config/mutants.toml` | Preview which mutations would be applied, without running them. |

## VM Integration Tests

NixOS-VM tests exercise full-system flows with real kernel features (FUSE, cgroup v2, overlayfs, k3s). Each test spins up 2--5 QEMU VMs via `nixosTest`. Run via `nix-build-remote .#ci` (needs KVM):

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
