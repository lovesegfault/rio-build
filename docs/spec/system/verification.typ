#import "/lib/rio.typ": *
#show: rio.with(domains: ("ts",))


= Protocol Conformance

- Live-daemon golden tests: each test starts an isolated nix-daemon, exchanges
  with it, and compares the response field-by-field against rio-build at the
  byte level
- No stored fixtures --- tests always run against the current nix-daemon
  version, eliminating fixture staleness
- STDERR activity stripping handles daemon messages
  (`START_ACTIVITY`/`STOP_ACTIVITY`) that rio-build omits
- Fields that legitimately differ (`version_string`, `trusted`) are skipped via
  a configurable skip list

== Multi-Nix compatibility matrix

Per-push CI runs `golden_conformance` against three CppNix daemon variants
(`checks.golden-<variant>`):

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([Variant], [Source], [Notes]),
  [`nix-pinned`], [`inputs.nix` (2.34.x)], [Same as per-push CI --- sanity row],

  [`nix-stable`],
  [`pkgs.nixVersions.nix_2_28`],
  [Oldest CppNix nixpkgs still ships],

  [`nix-unstable`], [`pkgs.nixVersions.git`], [Surfaces breakage early],
)

Two of three daemons come from the pinned nixpkgs (no separate flake inputs)
and substitute from cache.nixos.org; only `nix-pinned` builds from
`inputs.nix`. The runs are scoped to `rio-gateway` (single-member nextest
meta), so gen-matrix's cache-filter skips all three on PRs that don't touch
`rio-gateway`'s closure.

Lix is *not* in the golden matrix: it is policy-frozen at protocol 1.35,
and the harness sends 1.38-shaped opcode payloads (it doesn't downgrade per
negotiated version), so Lix-as-reference-daemon would just compare
`STDERR_ERROR` vs rio's `STDERR_LAST` for every opcode. Lix-as-*client*
coverage --- the direction that matters in production --- is
`checks.vm-protocol-warm-lix-standalone`.

Test harness reads `RIO_GOLDEN_DAEMON_BIN` (absolute daemon path) and
`RIO_GOLDEN_DAEMON_VARIANT` (skip-list key). Per-variant skips live in
#src("rio-gateway/tests/golden/daemon.rs::VARIANT_SKIP") --- each row is
`(variant, test_name, reason)`. The `reason` field is load-bearing: it
documents WHY so the skip can be removed once upstream converges.

Lix is policy-frozen at protocol 1.35 (rio's `MIN_CLIENT_VERSION`), so a Lix
client negotiates to 1.35 and rio omits the 1.37+
`BuildResult.cpu_user`/`cpu_system` fields. The Lix-as-client direction is
exercised end-to-end in checks by `vm-protocol-warm-lix-standalone` (same
scenario as `vm-protocol-warm-standalone`, client `nix.package` set to
`pkgs.lix`). Known golden-conformance divergences against rio-as-daemon
(handled at the field level by `skip_fields()` in
#src("rio-gateway/tests/golden_conformance.rs")):
- `version_string` --- each daemon has its own
- `features` (nix-unstable only) --- the handshake feature set is
  set-membership, not byte-equality. CppNix master grows
  `WorkerProto::Feature` entries continuously
  (`realisation-with-path-not-hash`, `delete-dead-specific-referrers`, ...).
  rio advertises `[]` so clients fall back to the pre-feature wire encoding
  rio implements; nix intersects client∩server and that's the correct
  negotiated result. `nix-pinned`/`nix-stable` stay byte-exact (`[]` on
  both sides) so rio accidentally advertising something is caught.

`nix build .#checks.x86_64-linux.golden-<variant>` runs one variant locally.

= Fuzzing

Security-critical protocol parsers must be fuzz-tested. Targets live in
per-crate fuzz workspaces (#src("fuzz/rio-nix/"), #src("fuzz/rio-store/")):

- `wire_primitives` --- u64, padded strings, framed streams, empty strings,
  maximum sizes
- `opcode_parsing` --- each opcode's payload parsing (wopAddToStoreNar,
  wopBuildDerivation, etc.)
- `nar_parsing` --- @nar streaming reader with malformed input
- `narinfo_parsing` --- @narinfo text format parser
- `derivation_parsing` --- `.drv` @aterm format parser
- `derived_path_parsing` --- @derivedpath wire format (`!`-separated
  `drvPath!output` strings)
- `build_result_parsing` --- BuildResult wire format (status, error message,
  timing, built outputs)
- `stderr_message_parsing` --- STDERR message wire format
  (`read_stderr_message`)
- `refscan` --- store-path reference scanner (`RefScanSink`)
- `manifest_deserialize` (rio-store) --- chunk manifest deserialization
- Run continuously via `cargo-fuzz` / `libFuzzer`:
  - *CI tier:* 2min/target run with seed corpus (`nix flake check` includes
    `checks.fuzz-*`)
  - *Deep runs:* `cd fuzz/<crate> && cargo fuzz run <target>` in the dev shell
    --- libFuzzer accumulates corpus in `./corpus/`
  - Corpus seeded from `fuzz/rio-nix/corpus/<target>/` and
    `fuzz/rio-store/corpus/<target>/` (committed seeds prefixed `seed-`; NAR
    seeds regenerable via `gen-nar-corpus.sh`)

= Unit Tests

- Wire format: roundtrip serialization for all protocol types (property tests
  via `proptest`)
- @dag scheduling: known graphs → expected critical paths and executor
  assignments
- Scheduler invariants (proptest): for any DAG and completion sequence, no
  derivation is dispatched before all dependencies complete
- DAG merging: merging two DAGs produces correct dedup and shared-node priority
  inheritance
- @fastcdc chunking: deterministic chunking, dedup verification, chunk/reassembly
  roundtrip
- #gls("cas"): put/get/gc correctness, content-indexed lookup, PutPath idempotency
- CA early cutoff: propagation through multi-level DAGs, mixed CA/input-addressed
  DAGs
- Narinfo: parse/generate roundtrip against known-good narinfo files
- Store path computation: verify against known nix store paths
- @fuse store: cache hit/miss behavior, LRU eviction, concurrent access

= Functional Tests

Gateway wire protocol against *real `rio-store`* (`StoreServiceImpl` + ephemeral
PostgreSQL) --- the `RioStack` fixture at #src("rio-gateway/tests/functional/").
No k8s, no VM, no KVM; runs in `cargo nextest` alongside unit tests
(sub-second). Catches bugs `MockStore` hides:

- `wopAddToStoreNar` → `wopQueryPathInfo` with real hash verification
  (`MockStore` accepts any hash; real store runs `validate_nar_digest`)
- `wopAddMultipleToStore` → `wopNarFromPath` through real FastCDC chunk + PG
  manifest + reassembly (`MockStore` is `HashMap` insert/get --- byte-identical
  by construction, not by correctness)
- Reference chains: first tests to send non-empty `references` on the wire
  (`wire_opcodes/` always sends `NO_STRINGS`)

Scenarios ported from Lix
#link("https://git.lix.systems/lix-project/lix/src/branch/main/tests/functional2")[`functionaltests2`].
Port is *scenario* (what's being proved), not *invocation shape* (Lix's harness
is nix-CLI, rio's is wire-protocol). White-box assertions query PG directly
(`narinfo`, `manifests`) to prove the graph is real --- not an in-memory echo.

*Coverage:* tranche 1 is store-roundtrip (put/get/query). Tranche 2 (CA builds,
refscan, trustless remote) needs real scheduler; tranche 4 (ssh-ng transport)
needs russh fixture.

= Integration Tests

- `nix build --store ssh-ng://rio nixpkgs#hello` --- minimal end-to-end
- `nix build --builders 'ssh-ng://rio x86_64-linux'` --- @build-hook path
- `nix flake check --store ssh-ng://rio` --- checks output
- Multi-derivation chain (A → B → C) distributed across executors
- Cache hit path: second build of same derivation returns instantly
- Chunk dedup: build two similar packages, verify shared chunks
- Executor failure mid-build → rescheduled to another executor
- CA early cutoff: change input that produces same output → downstream skipped
- Binary cache: configure rio-store as substituter, `nix build` from cache
- Binary cache `/nix-cache-info` endpoint returns valid response
- Gateway handles concurrent client sessions
- Graceful shutdown: in-flight builds complete or are cleanly requeued
- Scheduler state recovery: kill scheduler mid-build, restart, verify builds
  resume
- FUSE store: build with cold cache, verify paths fetched from rio-store on
  demand

= Security Integration Tests

- `PutPath` with invalid @assignment-token (wrong derivation hash) → rejected
  with `PERMISSION_DENIED`
- `PutPath` with expired assignment token → rejected with `PERMISSION_DENIED`
- `PutPath` for output path not in assignment token's `expected_output_paths` →
  rejected
- Cross-tenant data isolation: tenant A cannot query tenant B's builds via
  `AdminService`
- Cross-tenant data isolation: tenant A's `wopQueryPathInfo` returns 404 for
  tenant B's paths (when per-tenant scoping is enabled)
- DAG size exceeding `MAX_DAG_NODES` → rejected at both gateway
  (`translate::validate_dag`, early reject) and scheduler

#info(title: [Implemented])[
  Security VM test fragments
  (#src("nix/tests/scenarios/security/standalone.nix")) cover HMAC
  assignment/service tokens, executor-kind spoofing, tenant resolution, JWT
  dual-mode fallback, per-tenant rate limiting, store-quota enforcement, and
  the `__noChroot` gateway pre-check (#rref("gw.reject.nochroot+2")).
]

= Chaos Testing

- S3 timeout during PutPath → verify orphan scanner reclaims stale manifests
- Executor disconnect during build → verify reassignment to another executor
- PostgreSQL unavailability → verify readiness probes gate traffic; verify
  recovery
- Scheduler crash during active builds → verify state recovery algorithm
- Network partition between executor and scheduler → verify completion
  buffering and retry

#info(title: [Implemented])[
  toxiproxy fault-injection chaos harness at
  #src("nix/tests/scenarios/chaos.nix").
]

= Mutation Testing

`cargo-mutants` mutates source --- swap `<` for `<=`, delete a statement,
replace a return value with `Default::default()` --- reruns the test suite, and
flags mutations that *survive* (the tests still pass). A surviving mutant is
code the tests don't actually constrain. tracey answers "is this spec rule
covered"; mutants answers "does the test that covers it actually catch bugs."
Complementary signals.

*Dev-only, not per-push.* Mutation testing is O(mutations × test-suite-time);
for the scoped target set (\~320 mutations, scheduler state machine / wire
primitives / ATerm parser / HMAC / manifest) it's hours per run. Build with
`nix build .#mutants .#mutants.report-assert` when wanted; *missed-count is a
trend metric, not a gate* --- compare against a prior run. An increase means a
recent change weakened a test or introduced untested code.

*Scoping* lives in #src(".config/mutants.toml"): `examine_globs` lists
high-signal files where a surviving mutant is a genuine gap (not "you didn't
test your tracing span"). `exclude_re` filters out tracing/metric calls ---
those are already covered by the per-crate `metrics_registered` test.
`cap_lints = true` prevents the `--deny warnings` policy from marking mutations
unviable before a test can kill them.

= VM Integration Tests

NixOS-VM tests exercise full-system flows with real kernel features (FUSE,
cgroup v2, @overlayfs, k3s). Each test spins up 2--5 QEMU VMs via `nixosTest`.
Run via `nix-fast-build --flake .#checks.x86_64-linux` (needs KVM). Tests are
organized by scenario (#src("nix/tests/default.nix") is the source of truth):
`vm-protocol-*`, `vm-scheduling-*`, `vm-lifecycle-*`, `vm-le-*`
(leader-election), `vm-security-*`, `vm-dashboard-*`, `vm-observability-*`,
`vm-chaos-*`, `vm-substitute-*`, `vm-ca-cutoff-*`, `vm-nixos-node`. Suffix
`-standalone` runs against bare-process services in dedicated VMs; suffix `-k3s`
boots a single-VM k3s cluster.

= Test Environment

#table(
  columns: (auto, 1fr),
  align: (left, left),
  table.header([Dependency], [Purpose]),
  [Nix daemon],
  [Live-daemon golden conformance tests (auto-started per test via
    `fresh_daemon_socket()`)],

  [PostgreSQL],
  [Build state storage (ephemeral `initdb` per test via
    `rio-test-support::TestDb`; `PG_BIN` set by dev shell)],

  [MinIO],
  [S3 backend tests (VM tests use `services.minio`; unit tests use filesystem
    backend)],

  [k3s],
  [Kubernetes integration tests (bootstrapped in `vm-*-k3s` VMs; no external
    cluster needed)],
)

#r("ts.pg.server")[
  `PgServer` is the process-global ephemeral postgres handle. `PgServer::get()`
  lazily bootstraps via `initdb` + spawn `postgres` (Unix-socket-only,
  `fsync=off`, `max_connections=500`) on first call and is also the public entry
  point `xtask regen sqlx` reuses. `gc_stale_dirs` runs before every bootstrap
  to reclaim `/tmp/rio-pg-*` dirs left by dead test processes (the `PG` static
  never drops, so `TempDir::drop` never fires); a dir is stale iff its
  `owner.pid` PID is dead --- missing/unparseable `owner.pid` is treated as a
  live concurrent bootstrap and left alone. `TestDb::new_empty` creates the
  isolated DB without running migrations (for tests exercising the migrator
  itself); `TestDb::new` is `new_empty` + `migrator.run`.
]

#r("ts.pg.db-name")[
  `TestDb` database names are `rio_test_{nanos}_{counter}` --- nanos alone is
  not unique under raw-libtest's thread-per-test (two threads can hit the same
  nanosecond), so a process-global atomic counter is appended.
]

#r("ts.wire.macros")[
  `wire_bytes!` builds a `Vec<u8>` from `kind: value` pairs (`u64`, `string`,
  `strings`, `bool`, `bytes`, `framed`, `raw`); `wire_send!` writes the same
  primitives directly to a stream and flushes. Both expand to
  `wire::write_<kind>(..)?` calls, so callers must be in async context with a
  `Result` return.
]

#r("ts.wire.helpers")[
  `do_handshake` sends the client side of the worker-protocol handshake against
  a `DuplexStream` (not `client_handshake` --- that's the production driver in
  `rio-nix::protocol::client`). `read_path_info` reads the 8-field
  `wopQueryPathInfo` body so callsites don't repeat the discard sequence.
  `drain_stderr_until_last` panics on `STDERR_ERROR`;
  `drain_stderr_expecting_error` is the inverse for error-path tests.
]

#r("ts.metrics.grep+2")[
  `grep_emitted_names(manifest_dir)` greps the crate's `src/` for
  `metrics::{counter,gauge,histogram}!("...")` literals;
  `grep_spec_names(metrics_json_body, prefix)` filters
  #src("docs/gen/metrics.json") (the regex-scanned `describe_*!` inventory)
  by component prefix. Both run at *test time* from `metrics_suite!` (no
  per-crate build.rs). The contract narrowed to "regex-scanned `describe_*!`
  literals → `describe_metrics()` fires them" --- catches a `describe_*!`
  that's in source but not reachable from the per-crate `describe_metrics()`
  body (cfg-gated, dead, or in the wrong fn).
]

= Benchmarks

#table(
  columns: (auto, 1fr, auto),
  align: (left, left, left),
  table.header([Metric], [Description], [Target]),
  [*Scheduling latency*],
  [Time from `nix build` invocation to first derivation starting on an
    executor],
  [p99 < 5s],

  [*Cache hit latency*],
  [End-to-end time for a fully cached 1MB output],
  [< 1s],

  [*Throughput*],
  [Derivations/second at 1, 5, 10, 20 executors],
  [Document actual],

  [*Cache hit rate*],
  [Fraction of derivations served from store vs. built],
  [Document actual],

  [*Dedup ratio*],
  [Chunk storage savings compared to full NAR storage],
  [Document actual],

  [*Transfer volume*],
  [Bytes moved between store and executors per build],
  [Document actual],

  [*Critical path accuracy*],
  [Predicted vs. actual build completion time],
  [Within 2x],

  [*Comparison baseline*],
  [`nix build` with standard remote builders on same hardware],
  [Document speedup],
)

*Benchmark workloads:*
- Small: `nixpkgs#hello` (few derivations, fast builds)
- Medium: `nixpkgs#firefox` (large DAG, mix of fast and slow)
- Large: NixOS system closure (thousands of derivations)
- Incremental: rebuild after single-file change (tests cache hit + locality)
