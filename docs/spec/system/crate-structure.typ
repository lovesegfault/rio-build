#import "/lib/rio.typ": *
#show: rio.with(domains: none)

// gen/workspace.json — members + per-crate `deps:{prod,optional,dev}`.
// Loaded directly (not via refs.typ) so the autograph block can spread
// `_ws.deps.pairs()`.
#let _ws = json("/gen/workspace.json")
// gen/modules.json — recursive src/ walk per crate (path, depth, doc =
// first //! line). The §Module Structure trees derive from this so a
// new/renamed/deleted module is reflected without a manual edit
// (R4-m002 tls.rs + R5-m004 karpenter.rs were hand-tree drift).
#let _mods = json("/gen/modules.json")
#let _module-tree(crate) = {
  let entries = _mods.at(crate)
  raw(
    "src/\n"
      + entries
        .map(m => {
          let leaf = m.path.split("/").filter(s => s != "").last()
          let indent = "│   " * m.depth
          // `indent.len()` is UTF-8 bytes (│ = 3); use display width.
          let pad = " " * calc.max(1, 22 - 4 * m.depth - leaf.len())
          let doc = if m.doc != "" { "# " + m.doc } else { "" }
          indent + "├── " + leaf + pad + doc
        })
        .join("\n"),
    block: true,
  )
}

= Workspace Layout (#(refs.crate-count)() crates)

// ASCII tree derives from gen/workspace.json (same _ws.members the
// autograph below spreads). bug_016: the hand-maintained tree omitted
// rio-lease while the derived heading and graph showed 14.
#let _tree-rows = _ws.members.map(m => {
  let pad = " " * calc.max(0, 19 - m.name.len())
  "├── " + m.name + "/" + pad + "# " + m.description
})
#raw(
  "rio-build/\n"
    + "├── Cargo.toml           # Workspace root\n"
    + _tree-rows.join("\n")
    + "\n"
    + "├── workspace-hack/      # cargo-hakari unification crate (no source, dep-only)\n"
    + "└── rio-dashboard/       # Svelte 5 SPA — NOT a Rust crate; built by nix/dashboard.nix\n",
  block: true,
)

`rio-dashboard/` is a workspace sibling but NOT a Cargo workspace member. It has
its own `package.json`/`pnpm-lock.yaml` and is built by `nix/dashboard.nix` via
`fetchPnpmDeps` + Vite. TypeScript stubs are generated in-sandbox from
`rio-proto/proto/*.proto` via `buf generate` (protobuf-es v2), so the dashboard
derivation is invalidated on `.proto` changes but not on Rust-only commits.

= Dependency Graph

#{
  // QA5: full Cargo.toml descriptions made each node 80+ chars wide
  // → 1539pt graph (~2051px), 3× column scroll. autograph 0.1.0 can't
  // pass rankdir/nodesep to graphviz (its README says so), and dot
  // already lays out TB — the ~1000pt width even without subtitles is
  // ~8 consumer crates landing on one rank with dot's fixed inter-node
  // spacing. Drop the description subtitle (repeated verbatim in
  // §Module Structure right below) and use twopi (radial: most-shared
  // deps near centre, consumers on the rim) — the only autograph-
  // reachable engine that fits the 750px column.
  let crate(name) = text(
    font: "DejaVu Sans Mono",
    weight: 600,
    size: 0.75em,
    name,
  )
  let dev = (stroke: (paint: muted, dash: "dashed", thickness: 0.6pt))
  let opt = (stroke: (paint: rule-color, dash: "dotted", thickness: 0.6pt))
  figure(
    autograph.diagram(
      engine: "twopi",
      node-shape: fletcher.shapes.rect,
      node-stroke: 0.6pt + rule-color,
      node-inset: 0.4em,
      edge-stroke: 0.6pt,
      edge-corner-radius: 8pt,
      // Nodes AND edges derive from gen/workspace.json (each crate's
      // Cargo.toml [dependencies] / [dev-dependencies] /
      // [target.*.dependencies] rio-* entries). bug_021: the
      // hand-maintained list mis-classified scheduler→store as dev-only
      // and omitted rio-auth/rio-lease nodes entirely.
      .._ws.members.map(m => autograph.node(label(m.name), crate(m.name))),
      .._ws
        .deps
        .pairs()
        .map(p => {
          let (c, d) = p
          d.prod.map(t => autograph.edge(label(c), label(t)))
          d.optional.map(t => autograph.edge(label(c), label(t), ..opt))
          d.dev.map(t => autograph.edge(label(c), label(t), ..dev))
        })
        .flatten(),
    ),
    caption: [Workspace dependency graph. Solid = prod (default-feature
      reachable); dotted = `optional = true` not enabled by `default`;
      dashed = `[dev-dependencies]` only.],
  )
}

Notable edges:

- *`rio-proto → rio-nix`*: `ValidatedPathInfo` wraps `StorePath` from rio-nix. No cycle — rio-nix has no rio-\* deps.
- *`rio-proto → rio-common`*: `connect_channel`/`connect_with_retry` use `rio_common::backoff` and `rio_common::grpc` constants. Contract tests floor-assert `rio_common::limits` constants at compile time (e.g., `MAX_DAG_NODES >= 70_000`).
- *`rio-scheduler → rio-nix` (prod)*: `Derivation` parsing for closure resolution and `StorePath` validation in the merge path.
- *`rio-lease → rio-crds` (prod)*: `KubeErrorExt::is_conflict` for the election 409 branches only.
- *`rio-scheduler → rio-store` (prod, `schema` feature)*: `ca/resolve.rs` → `rio_store::realisations::query`; integration tests additionally pull `test-utils`.
- *`rio-gateway → rio-store` (dev-only)*: golden-daemon tests assert against a real `StoreServiceServer` (with `test-utils` feature) instead of `MockStore`.

= Module Structure

// Per-crate prose that follows each module tree. Headings + trees
// derive from gen/modules.json (so a new crate appears automatically);
// the dict below holds ALL trailing per-crate content (#r() markers,
// raw paragraphs, fuzz/field-rules notes). merged_001: rio-lease had
// no heading; rio-proto src/ was hand-maintained and stale.
#let _crate-prose = (
  "rio-common": [
    #r(
      "common.bootstrap",
    )[`bootstrap<C, A>(component, cli, describe_metrics, histogram_buckets)` is the cold-start prologue every binary calls from `main()`: `init_tracing` (returns `OtelGuard`) → config `load` → `ValidateConfig::validate` → `shutdown_signal` + `init_metrics` (with the per-crate `histogram_buckets` table) + `describe_metrics()` → enter root `component` span. Returns `Bootstrap<C>{ cfg, shutdown, serve_shutdown, otel_guard, root_span }` — `otel_guard` and `root_span` MUST be bound (not destructured with `..`) or OTel teardown / span exit happens immediately. `HasCommonConfig` projects each binary's `Config` to its flattened `CommonConfig` so `bootstrap` can read `metrics_addr`/`metric_labels` without knowing the concrete type. There is no application-level TLS --- transport encryption is mesh-level (see #rref("sec.transport.cilium-wireguard")).]

    #r(
      "common.signal.sighup-reload",
    )[`sighup_reload(shutdown, reload)` spawns a SIGHUP listener that runs the async `reload` closure on each signal, looping until `shutdown` fires. A failed reload is logged and the loop continues — old state stays active. Used for JWT pubkey rotation (re-read ConfigMap mount → swap `Arc<RwLock<VerifyingKey>>`).]

    #r(
      "common.helpers",
    )[`default_addr` produces `[::]` dual-stack bind addresses (Linux `bindv6only=0`: one socket answers both v4-mapped and native v6). `grpc.rs` is the proto-agnostic tonic helper layer (timeout wrappers, `StatusExt`, `check_bound`, h2 window constants, `x-rio-*` metadata keys); anything naming a generated proto type lives in `rio-proto::client` instead. `ValidateConfig::validate` is the post-load bounds check; `JwtConfig` carries the dual-mode `required`/`key_path` switch. `init_metrics` applies `global_labels` and a global `DEFAULT_BUCKETS` (so unmapped histograms still emit `_bucket` series), then per-metric overrides from the per-crate `HISTOGRAM_BUCKETS` table threaded through `bootstrap`.]
  ],
  "rio-auth": [
    Extracted from `rio-common` so a JWT-comment edit no longer rebuilds every binary. Depends on `rio-common` for `signal::Token` / `sighup_reload` and the `TENANT_TOKEN_HEADER` constant; nothing in `rio-common` depends back. Gateway holds the JWT signing key; scheduler/store/controller verify via `JwtLayer`.
  ],
  "rio-nix": [
    #r(
      "nix.hash.algos",
    )[`HashAlgo` is `{SHA256, SHA512, SHA1}` — the set Nix accepts for `outputHashAlgo` and store-path computation. SHA-1 is included for legacy fixed-output derivations (`fetchgit` historically defaulted to it). BLAKE3 is used internally by rio-build for chunk addressing but is NOT a `HashAlgo` variant — it never crosses the Nix-facing protocol boundary.]

    #r(
      "nix.hash.sri",
    )[`NixHash::parse_sri`/`to_sri` handle the SRI form (`sha256-BASE64=`); `parse_colon`/`to_colon` handle the Nix colon form (`sha256:nixbase32`). `NixHash::parse` auto-detects by separator.]

    #r(
      "nix.narinfo.verify-sig",
    )[`NarInfo::verify_sig` checks each `Sig:` line against a list of trusted `name:base64(ed25519-pubkey)` keys. The fingerprint is reconstructed from `store_path`/`nar_hash`/`nar_size`/`references` (basenames re-prefixed with the store dir, sorted). Malformed keys or sigs are treated as non-matching, never errors. Returns the first matching key name or `None`.]

    #r(
      "nix.client.set-options",
    )[`client_set_options` sends `wopSetOptions` to a `nix-daemon --stdio` process. Only `max_silent_time` and `build_cores` are caller-supplied; all other fields (`keepFailed`, `keepGoing`, `verbosity`, `maxBuildJobs=1`, obsolete fields) are hardwired. The daemon sets `NIX_BUILD_CORES` inside the sandbox from this value — setting the env var on the daemon process is ignored.]

    #r(
      "nix.stderr.pid-activity-id",
    )[`StderrWriter` allocates activity IDs as `(getpid() << 32) + counter` to match upstream Nix's `libutil/logging.cc` convention. Starting at bare `1` would put server-allocated IDs in the same low range a client may use for its own activities (I-206: nom showed completed builds as stuck at their last phase).]

    #r(
      "nix.drv.like-trait",
    )[`DerivationLike` is the shared predicate trait over `Derivation` and `BasicDerivation`: `outputs()`/`platform()`/`env()` accessors plus the default-method predicates `is_fixed_output`, `has_ca_floating_outputs`, `is_content_addressed`. Inherent accessor methods are kept alongside so existing callers don't need a trait import; callers of the predicate methods must `use DerivationLike`.]

    #r(
      "nix.drv.parse-from-nar",
    )[`Derivation::parse_from_nar` extracts the single regular file from a NAR, UTF-8-decodes it, and runs the ATerm parser — the convenience path for `.drv` blobs that arrive NAR-wrapped over the wire.]

    #r(
      "nix.closure.cycle-safe",
    )[`closure::ClosureSet::extend` (incremental visited-set BFS, the `computeFSClosure` shape), `closure::find_cycle` (Kahn-style peeling over a closed member set, self-references ignored), and `closure::closure_sizes` (per-member BFS with one reusable scratch set, O(largest closure) auxiliary memory) MUST terminate on arbitrary reference graphs — including cyclic ones — and every rio consumer that traverses adversary-influenceable reference metadata MUST delegate to these primitives instead of hand-rolling the walk. Cycle-safety is a per-consumer obligation in rio because rio-store deliberately admits reference cycles (#rref("store.gc.sweep-cycle-reclaim")) for GC reclamation, unlike CppNix's local store where `registerValidPaths`' topological sort makes cycles unrepresentable.]

    Fuzz targets for the parsers live in `fuzz/rio-nix/` (separate workspace, own `Cargo.lock`). A second fuzz workspace at `fuzz/rio-store/` covers the manifest parser. Both are excluded from the main workspace — when a fuzzed crate's deps change, run `cd fuzz/<crate> && cargo update -p <crate>` to sync the independent lockfile.
  ],
  "rio-test-support": [
    `rio-test-support` is a `[dependencies]` (not dev-dep) of `xtask` — `xtask regen sqlx` reuses `PgServer::bootstrap`. All other crates depend on it under `[dev-dependencies]` only; `rio-store` additionally has it under `[dependencies]` with `optional = true` (`test-utils` feature, not in `default`).

    #r(
      "ts.mock.admin",
    )[`MockAdmin` returns `Default::default()` for every unary `AdminService` RPC. The per-RPC stub bodies are generated by `rio-test-support/build.rs` from `admin.proto` into `mock_admin_default_methods!()` — adding a new unary admin RPC requires zero hand-written Rust. Streaming RPCs and the two call-recording unaries (`ClearPoison`, `CreateTenant`) are listed in `MANUAL_METHODS` and implemented by hand in `grpc/admin.rs`.]

    #r(
      "ts.mock.store-chunk",
    )[`MockStore` implements both `StoreService` AND `ChunkService` against a single in-memory state, mirroring the real store which serves both on one port. `spawn_mock_store` registers both service servers on the same router; tests that never touch chunk RPCs are unaffected.]

    #r(
      "ts.mock.store-faults",
    )[`MockStoreFaults` carries the full fault-injection knob set: `fail_next_puts`/`abort_next_puts` (decrement-and-fail), `fail_find_missing`/`fail_query_path_info`/`fail_get_path` (toggle), `get_path_garbage` (non-NAR bytes), `get_path_gate`/`get_path_gate_armed` (Notify hold-then-release for concurrency tests), and `get_path_chunk_delay_ms` (per-chunk delay for progress-timeout tests). Call recorders live in `MockStoreCalls`.]

    #r(
      "ts.mock.store-put-validate",
    )[`MockStore::put_path` mirrors the real store's stream validation: rejects non-empty `metadata.nar_hash` (hash-upfront removed pre-phase3a), independently SHA-256-hashes NAR chunks and verifies against the trailer, and rejects a stream that closes without a trailer. This keeps mock-passing tests honest against `rio-store`.]

    #r(
      "ts.mock.scheduler-outcome+2",
    )[`MockScheduler` is configured via two orthogonal knobs. `SubmitOutcome` is an enum of mutually-exclusive `SubmitBuild` modes: `Error(code)` (immediate failure), `Simple { send_completed, close_early }` (Started then optionally Completed/close/hang), or `Scripted { events, error_after_n, interval }` (verbatim event list with auto-filled `build_id`/`sequence`, optional mid-stream `Err(Status)` injection, optional per-event sleep for disconnect-race tests). `WatchOutcome` carries `scripted_events` (WatchBuild replay honoring `since_sequence`) and `fail_count` (decrement-and-Unavailable). `SubmitBuild` sets `BUILD_ID_HEADER` in initial metadata; `WatchBuild` does NOT (the gateway already has the build_id when it calls WatchBuild).]

    #r(
      "ts.spawn.layered",
    )[`spawn_grpc_server` accepts a prebuilt `Router` and binds it to an ephemeral `127.0.0.1` port. `spawn_grpc_server_layered` is the generic variant for routers carrying tower layers (`Server::builder().layer(...)` changes the `Router<L>` type parameter). `spawn_mock_store`/`spawn_mock_store_with_client` compose StoreService + ChunkService; `spawn_mock_store_inproc` uses a tokio duplex transport (no real TCP) for `start_paused = true` tests where kernel-side accept would race tokio's auto-advance.]

    #r(
      "ts.kube.verifier-guard",
    )[`ApiServerVerifier::run` returns a `VerifierGuard` drop-bomb: dropping it without calling `.verified().await` panics. `Scenario::ok` and `Scenario::k8s_error` are the two response shorthands; `k8s_error` emits the `metav1.Status` envelope that `kube::Error::Api` deserializes from. `Scenario.body_contains` optionally asserts on request-body substrings.]

    #r(
      "ts.fixtures.builders",
    )[`fixtures` provides `rand_store_hash()` (32 random nixbase32 chars, distinct per call — use when scheduler dedup must NOT short-circuit), `make_derivation_node`/`make_edge` (DAG builders keyed on a tag), `make_nar`/`make_large_nar`/`make_path_info_for_nar` (NAR + ValidatedPathInfo builders), `pseudo_random_bytes` (FastCDC-friendly deterministic content), and `seed_store_output` (writes a file under `{tmp}/nix/store/{basename}` for builder upload/FOD tests).]

    #r(
      "ts.metrics.asserts",
    )[`metrics_suite!` expands to the three-test `metrics_registered.rs` body. The bodies call `assert_spec_metrics_described` (spec→describe), `assert_emitted_metrics_described` (emit→describe, with a min-count regex-health guard), and `assert_histograms_have_buckets` (describe→bucket, against the crate's `HISTOGRAM_BUCKETS` table). `CountingRecorder` is the runtime recorder impl for behavioral assertions.]
  ],
  "xtask": [
    `cargo xtask` subcommands for codegen (`regen cargo-json`, `regen sqlx`,
    `regen fuzz-lock`, `regen docs-data`), local cluster lifecycle
    (`up`/`down`/`status`), AMI build, and helm/k8s helpers. Depends on
    #(_ws.deps.at("xtask").prod.map(raw).join(", ")) --- see
    `gen/workspace.json` for the live list.
  ],
)

// rio-proto first — it has a hand-maintained proto/*.proto block
// (NOT under src/; modules.json covers src/ only). The src/ tree
// IS now derived (the previous hand-tree had drifted).
== rio-proto — gRPC definitions

// .proto files derive from gen/protos.json (`service X` decls + first
// `//` comment per file). bug_030: hand-tree said BuilderService; the
// file defines ExecutorService.
#let _protos = json("/gen/protos.json")
#raw(
  "proto/\n"
    + _protos
      .pairs()
      .sorted(key: p => p.at(0))
      .map(p => {
        let (f, info) = p
        let pad = " " * calc.max(1, 22 - f.len())
        let ann = if info.services.len() > 0 {
          info.services.join(" + ")
        } else {
          info.doc
        }
        "├── " + f + pad + "# " + ann
      })
      .join("\n"),
  block: true,
)
#_module-tree("rio-proto")

*Field-addition rule.* A new proto3 scalar field whose consumer
behaviour differs between "field absent" and "field = zero-value" MUST
be declared `optional`. The consumer MUST handle `None` as "sender
pre-dates this field" — i.e., reproduce the pre-addition behaviour. For
`bool` this is almost always required (default `false` is rarely
back-compat-safe). For `repeated`/`map`, empty is usually safe. Every
such field gets a `tests/roundtrip.rs` case in the consumer crate that
decodes a byte-slice _without_ the new tag and asserts the consumer's
behaviour matches the old.

*Field-retype/removal rule.* A retyped or removed proto3 field MUST
keep its old field number and name `reserved` — never reuse a field
number on a new type. A same-number retype is wire-incompatible during
a rolling upgrade in two flavours: cross-wire-type (e.g. `double`
(fixed64) → `string` (length-delimited)) fails the _whole message_
decode with an opaque `DecodeError` (prost's per-field `merge` rejects
the wire-type mismatch, so every other field in the message is lost
too); same-wire-type (e.g. `int32` → `sint32`, both varint) is worse —
the receiver silently decodes the wrong value with no error at all.

The `.fields` snapshot tripwires
(`rio-proto/tests/proto_field_presence.rs`, one `<name>_fields_frozen`
test per `proto/*.proto` file) fail CI on any field-set change until
the corresponding snapshot is regenerated, forcing both decisions to be
explicit. A structural test (`every_proto_has_a_snapshot_test`)
cross-checks the registered tripwire list against the on-disk `proto/`
directory, so a new proto file cannot ship without one.


#for name in _mods.keys().filter(n => n != "rio-proto").sorted() {
  let m = _ws.members.find(m => m.name == name)
  let desc = if m != none { m.at("description", default: "") } else { "" }
  heading(level: 2)[#raw(name)#if desc != "" [ --- #desc]]
  _module-tree(name)
  _crate-prose.at(name, default: [])
}

= Dependencies

#table(
  columns: (auto, 1fr, auto, 1.6fr),
  align: (left, left, center, left),
  table.header([Crate], [Purpose], [Phase], [Notes]),
  [`rio-nix` (ours)],
  [Nix types: store paths, derivations, @nar, @narinfo, wire protocol],
  [1],
  [Implemented from scratch; MIT/Apache-2.0],

  [`russh`], [Async SSH server], [1], [For ssh-ng transport],
  [`tracing` + `tracing-subscriber`],
  [Structured logging],
  [1],
  [`features = ["env-filter", "json"]`],

  [`metrics` + `metrics-exporter-prometheus`],
  [Prometheus metrics],
  [1],
  [Counters, histograms for builds/chunks/latency],

  [`tokio`], [Async runtime], [1], [`features = ["full"]`],
  [`thiserror` / `anyhow`], [Error handling], [1], [Typed vs. context errors],
  [`serde` / `serde_json`], [Serialization], [1], [Config, API types],
  [`tonic` / `prost`],
  [gRPC framework + protobuf],
  [2],
  [Internal APIs. `tonic-health` adds the `grpc.health.v1.Health` service for K8s readiness probes.],

  [`sqlx`],
  [PostgreSQL + SQLite async driver],
  [2],
  [`default-features = false, features = ["runtime-tokio", "postgres", "sqlite", "macros", "migrate", "uuid"]`. SQLite feature is for the worker's synthetic per-build store DB.],

  [`config`],
  [Layered configuration],
  [2b],
  [TOML + `RIO_*` env overlay + clap CLI args. `default-features = false, features = ["toml", "json"]`.],

  [`clap`],
  [CLI argument parsing],
  [2b],
  [`features = ["derive", "env"]`. Used by all binaries (gateway, scheduler, store, worker, controller) via the config loader's `CliArgs` pattern.],

  [`fastcdc`], [Content-defined chunking], [2], [For NAR deduplication],
  [`sha2`],
  [SHA-256 hashing],
  [1],
  [NAR hash verification, @store-path computation, content index. All Nix-facing hashes use SHA-256.],

  [`blake3`],
  [Fast cryptographic hashing],
  [2],
  [Chunk content addressing (rio-store @cas).],

  [`moka`],
  [In-process LRU cache],
  [2c],
  [Chunk cache in rio-store. Lock-free, weight-based eviction (tracks byte-size per entry so the 2GB cap is a real memory bound). `features = ["future"]`.],

  [`zstd` / `async-compression`],
  [Zstandard compression],
  [2],
  [Binary cache serves `.nar.zst`. `zstd` for buffered paths; `async-compression` for streaming `/nar/` endpoint (O(chunk) memory instead of O(NAR)).],

  [`dashmap`],
  [Concurrent hash map],
  [2],
  [Scheduler log ring buffers (written outside actor loop); @singleflight for concurrent S3 fetches.],

  [`ordered-float`],
  [`Ord` wrapper for floats],
  [2c],
  [Scheduler's `BinaryHeap` over f64 critical-path priority. f64 doesn't impl `Ord` (NaN); `OrderedFloat<f64>` is the standard workaround.],

  [`axum`], [HTTP server], [2], [Binary cache endpoint],
  [`aws-sdk-s3`], [S3 chunk storage], [2], [Production blob backend],
  [`ed25519-dalek`],
  [NAR signing/verification],
  [2],
  [Binary cache signature support. `features = ["rand_core", "pkcs8"]`.],

  [`fuser`], [@fuse filesystem], [2], [Per-worker `/nix/store` mount],
  [`tracing-opentelemetry`],
  [Distributed tracing],
  [2 (done)],
  [Trace propagation across gRPC boundaries. `init_tracing` in `rio-common/observability.rs` + `inject_current`/`link_parent` in `rio-proto/interceptor.rs`.],

  [`kube` + `kube-runtime`],
  [K8s client, CRDs, operator framework],
  [3],
  [`default-features = false`, `features = ["runtime", "derive", "client", "rustls-tls", "aws-lc-rs"]` (kube 3.x) — the defaults would select the `ring` rustls provider; the explicit set keeps the workspace on a single `aws-lc-rs` `CryptoProvider`. xtask adds `ws` for port-forward/exec.],

  [`k8s-openapi`],
  [K8s API types],
  [3],
  [`features = ["v1_35"]` (feature-gates which struct fields exist; pin to highest supported API version)],

  [`schemars`],
  [JSON Schema for CRDs],
  [3],
  [schemars 1.x (NOT 0.8 — kube 3.0 requires the major break)],

  [`rustls`],
  [TLS provider selection],
  [3],
  [Direct dep to call `install_default()`: the workspace links a single `aws-lc-rs` provider (kube is `default-features = false` + `aws-lc-rs`); the explicit install is defensive — a future transitive `ring` revival would otherwise re-create the rustls 0.23 dual-provider panic on first TLS use.],

  [`cargo-deny`],
  [License auditing, security advisories],
  [2],
  [Deny GPL-3.0+ per project policy; check advisories in CI. Dev tool, not a runtime dep.],

  [`opentelemetry` + `opentelemetry-otlp`],
  [OTLP pipeline],
  [2 (done)],
  [Full OTLP/gRPC via `opentelemetry-otlp`, batch processor, `ParentBased(TraceIdRatioBased)` sampler. `RIO_OTEL_ENDPOINT` gate; unset = zero overhead. VM test uses Tempo (not Jaeger — not packaged in nixpkgs); OTLP works with both.],

  [TypeScript/Svelte/Vite],
  [Web dashboard],
  [5],
  [Separate `rio-dashboard/` project (not a Rust dep)],
)

== System Dependencies

#table(
  columns: (auto, 1fr, auto, 1.2fr),
  align: (left, left, center, left),
  table.header([Dependency], [Purpose], [Phase], [Notes]),
  [`busybox-sandbox-shell`, `fuse3`, `util-linux`],
  [Worker-image runtime tools: the minimal static ash bind-mounted at `/bin/sh` inside every build sandbox (`RIO_SANDBOX_SHELL`), `fusermount3` for the FUSE input store, and `mount`/`umount` for overlay teardown],
  [2],
  [Shipped in the worker container image. Workers invoke no Nix tooling at runtime — sandboxed build execution is native (`rio-exec`); the daemon-era requirement to ship `nix` in worker images is gone.],
)

== Gotchas

- gRPC over HTTP/2 defeats L4 load balancers. Use a K8s headless Service + client-side DNS resolution, or an L7 proxy for inter-component gRPC.
- kube-rs: status updates trigger watch events — use conditional updates to avoid infinite reconcile loops.
- rustls `CryptoProvider` selection: the workspace links a single `aws-lc-rs` provider (kube is `default-features = false` + `aws-lc-rs`, matching aws-sdk and the rest of the TLS stack). `rio_common::server::bootstrap()` still calls `rustls::crypto::aws_lc_rs::default_provider().install_default()` as its first step — a guard against a transitive dep re-enabling `ring`, which would re-create the rustls 0.23 dual-provider panic at first TLS use.
- `rio-nix` implements the Nix protocol from scratch — reference Snix docs, Tweag blog, and Nix C++ source for protocol details. Target protocol version 1.35+ (Nix 2.18+ / Lix).

== Risk Notes

- *`russh`*: Small maintainer team / low bus factor. Consider `thrussh` fork or `ssh-rs` as a fallback if `russh` becomes unmaintained. Pin minimum version and monitor for security patches.
- *`fuser`*: Small maintainer team. Monitor for security patches; the FUSE interface is security-sensitive (runs with `CAP_SYS_ADMIN`). Pin minimum version.

== Dependencies Considered and Rejected

- *`petgraph`*: @dag representation. Rejected — the scheduler's graph is a simple adjacency-list `HashMap` with a custom `DerivationStatus` state machine; petgraph's algorithms (toposort, scc) don't match the incremental ready-queue pattern.
- *`memmap2`*: Zero-copy chunk access for filesystem backend. Rejected — the filesystem backend uses buffered I/O; SIGBUS handling complexity not justified for a dev/test-only backend. Production uses S3 (streamed over HTTP, no mmap).
- *`ginepro`*: gRPC client-side load balancing via DNS. Rejected — Cilium provides L4 load-balancing via eBPF kube-proxy replacement; tonic clients connect to a ClusterIP and Cilium distributes per-connection.
- *`arbtest`*: Property testing via structure-aware fuzzing. Rejected — `proptest` covers roundtrip serialization; `cargo-fuzz` covers parser fuzzing. No gap between them.
- *`testcontainers`*: Ephemeral Docker containers for integration tests. Rejected — `rio-test-support::TestDb` bootstraps ephemeral PostgreSQL via `initdb` (Nix-provided, no Docker dependency). MinIO is exercised only in VM tests via `services.minio`.

= Rationale

== Incremental crate growth // supersedes ADR-013

The bootstrap plan (start with 3 crates — `rio-nix`, `rio-build`, `rio-proto` —
and grow to 9) is complete; the workspace has since grown well past the
original target and the transitional `rio-build` crate no longer exists. The
current layout is documented above.
