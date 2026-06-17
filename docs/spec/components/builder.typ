#import "/lib/rio.typ": *
#show: rio.with(domains: ("builder",))


One-shot process in a K8s Job pod that executes a single derivation, then
exits.

Per ADR-019, this component is scoped to *non-@fod builds only* --- fully
airgapped, no internet egress. Fixed-output derivation fetches route to the
separate rio-fetcher executor. Both share the same `rio-builder` binary,
distinguished by `RIO_EXECUTOR_KIND`.

#info(title: [Formerly `rio-worker`])[
  Renamed in ADR-019 alongside the builder/fetcher split. Tracey markers moved
  from `r[worker.*]` to `r[builder.*]`.
]

= Responsibilities

- Pull the single assignment this pod was spawned for from the scheduler
  (`PullAssignment`; one derivation per pod, then exit)
- Run the @fuse store daemon (the `fuse` module) that mounts at
  `/var/rio/fuse-store` (configurable) with lazy on-demand fetching from
  rio-store
- Set up the build's overlay filesystem: FUSE mount as lower layer, local SSD
  as upper layer; the overlay's merged dir is bind-mounted at `/nix/store`
  inside the build's mount namespace
- Execute build: invoke `nix-daemon --stdio` locally for sandboxed build
  execution
- Stream build logs to rio-store's `LogService.AppendLog` (the log data
  plane; the scheduler never relays log bytes)
- After build: upload output @nar to rio-store (chunked), report the outcome
  with `ReportOutcome` (retried until acknowledged)
- Resource usage reporting (CPU, memory, disk, build duration)

= FUSE Store (`rio-builder::fuse`)

Each builder runs a FUSE filesystem that presents store paths to the build.
The FUSE daemon mounts at `/var/rio/fuse-store` (configurable --- *never*
directly at `/nix/store`, which would shadow the host store and break every
process on the machine including the builder itself). The overlay's merged
directory is what gets bind-mounted at `/nix/store`, and only inside the
build's mount namespace. The FUSE daemon communicates with rio-store via gRPC
to lazily fetch @store-path content on demand.

#figure(
  caption: [Builder pod store layout. The FUSE mount is the overlay's lower;
    nix-daemon's chroot store points at the merged dir.],
  diagram(
    spacing: (16mm, 11mm),
    node-stroke: 0.5pt,
    node(
      (0, 0),
      align(
        left,
      )[FUSE daemon\ #text(size: 0.8em)[mount `/var/rio/fuse-store`]],
      name: <fuse>,
      fill: accent.lighten(88%),
    ),
    node(
      (0, 1),
      align(left)[SSD Cache\ #text(size: 0.8em)[LRU, `Arc<Cache>`]],
      name: <cache>,
    ),
    node(
      (2, 0.5),
      [rio-store\ #text(size: 0.8em)[(gRPC)]],
      name: <store>,
      shape: fletcher.shapes.cylinder,
    ),
    node(
      (0, 2.2),
      align(left)[@overlayfs merged\ #text(size: 0.8em)[upper:
          `/var/rio/overlays/{build}`\ lower: FUSE\ bind-mounted at
          `/nix/store`]],
      name: <ov>,
    ),
    node(
      (0, 3.2),
      align(left)[nix sandbox\ #text(size: 0.8em)[(user/mount/PID/net ns)]],
      name: <sb>,
      stroke: (dash: "dashed"),
    ),
    edge(<fuse>, <cache>, "-|>"),
    edge(<cache>, <store>, "-|>", [miss], label-size: 0.8em),
    edge(<ov>, <fuse>, "-|>", [lower], label-size: 0.8em, bend: 30deg),
    edge(<sb>, <ov>, "-|>"),
    node(
      enclose: (<ov>, <sb>),
      stroke: (paint: muted, dash: "dashed"),
      inset: 8pt,
      snap: false,
      align(top + left, text(size: 0.8em, fill: muted)[Build (child mount
        ns)]),
    ),
    node(
      enclose: (<fuse>, <cache>, <ov>, <sb>),
      stroke: (paint: muted, dash: "dotted"),
      inset: 14pt,
      snap: false,
      align(top + left, text(size: 0.8em, fill: muted)[Builder Pod]),
    ),
  ),
)

== Why FUSE Instead of a Shared PV

- *Overlay-over-NFS is unsupported*: The Linux kernel does not guarantee
  overlayfs correctness over NFS/EFS. FUSE mounts appear as local filesystems
  and work correctly with overlayfs.
- *No shared infrastructure*: Each builder manages its own cache
  independently. No RWX PersistentVolume, no NFS/EFS/CephFS provisioning, no
  StoreSync reconciler.
- *Lazy loading*: Only paths actually accessed during a build are fetched. A
  nixpkgs @closure is tens of GB, but a typical build accesses a small
  fraction.
- *Perfect caching*: Store paths are immutable and content-addressed. Once
  cached, data never needs invalidation or re-fetching. The SSD cache is
  purely additive with LRU eviction under disk pressure.
- *Predictive warm-up*: the executor computes the input closure itself and
  primes the FUSE cache's manifest hints (#rref("builder.warmgate.manifest-prime"))
  before daemon spawn; paths are then materialized on demand by JIT lookup.

== FUSE Cache

- *Backend*: Local SSD (`emptyDir`)
- *Granularity*: Whole store paths (not individual chunks). The FUSE daemon
  reassembles NARs from chunks via rio-store and materializes them as
  directory trees on disk.
- *Metadata*: A lightweight SQLite index tracks cached paths
- *Lifetime*: Pod-scoped. The cache holds one build's input closure and is
  discarded with the `emptyDir` when the pod terminates; no eviction is
  needed. Node-level FSx caching survives pod churn, so common paths (glibc,
  coreutils, etc.) stay warm at the storage layer even though every pod-level
  FUSE cache is fresh.

#r("builder.platform.i686")[
  The per-build daemon's `nix.conf` MUST set `extra-platforms` to the worker's
  resolved `RIO_SYSTEMS` (minus the `builtin` pseudo-system). This keeps
  daemon acceptance consistent with the pod's resolved identity systems: an
  x86_64 Pool with `systems: [x86_64-linux, i686-linux]` routes
  i686 derivations to its pods, and the daemon accepts them because
  `extra-platforms = x86_64-linux i686-linux`. The host system appearing in
  `extra-platforms` is a no-op; on aarch64 builders the line contains only
  `aarch64-linux` so the setting is inert.
]

#r("builder.fuse.cache-ephemeral-memory")[
  The SQLite cache index is `:memory:` --- the pod's filesystem is discarded
  after the single build, so persistence is pointless, and on tiny-class node
  storage on-disk writes cost >1s each (I-141).
]

#r("builder.nar.entry-name-safety")[
  NAR directory entry names MUST be rejected at parse time if empty, equal to
  `.` or `..`, or containing `/` or NUL. This matches the Nix C++ reference
  (`archive.cc` `parseDump`). The rejection happens in
  `rio_nix::nar::parse_directory` before any filesystem call ---
  `extract_to_path` never sees a dangerous name. Rationale: `Path::join("..")`
  traverses upward; `Path::join("/abs")` discards the base. A crafted NAR from
  a compromised store could otherwise write arbitrary files on builder nodes
  via the FUSE fetch path.
]

#r("builder.nar.canonical-mtime")[
  Regular files and directories restored from a NAR MUST have their
  modification time set to the canonical Nix store-path value of one second
  past the Epoch (`mtime=1`).
]

NAR carries no timestamps; `restore_node` writes via `File::create()` /
`create_dir()` which leave `mtime≈now`. Nix's reference `restorePath()`
finishes with `canonicalisePathMetaData()` (`posix-fs-canonicalise.cc`,
`mtimeStore = 1`). Without that step, every input store path the build reads
through the FUSE chroot store carries the fetch wall-clock as its mtime, and
the nixpkgs `set-source-date-epoch-to-latest.sh` `postUnpackHook` --- which
scans `$sourceRoot` for the newest regular file --- raises `SOURCE_DATE_EPOCH`
to that value. Any FOD that bakes `SOURCE_DATE_EPOCH` into its output (the
`tar --mtime=@$SOURCE_DATE_EPOCH` archives `fetchPnpmDeps` v3,
`fetchYarnDeps`, and `fetchNpmDeps` produce) becomes non-deterministic and
fails its hash check on every rebuild. Symlink mtime is intentionally out of
scope here: `std` has no API to set a symlink's own mtime without a new
dependency, the `find -type f` in the SOURCE_DATE_EPOCH hook ignores
symlinks, and the FUSE attribute layer
(#rref("builder.fuse.canonical-metadata")) hardcodes canonical times for
every node type regardless of on-disk state.

#r("builder.fuse.canonical-metadata+2")[
  The FUSE store filesystem MUST present canonical Nix store-path metadata
  (`mtime`/`atime`/`ctime` of one second past the Epoch, `perm` `0o444` for
  non-executable regular files, `0o777` for symlinks, and `0o555` otherwise,
  `uid`/`gid` of `0`) rather than the on-disk metadata of the backing cache
  files.
]

The FUSE FS *is* the chroot store's lower layer; its visible metadata is the
metadata builds receive. Cache files are written at fetch time with the
process `uid`/`gid`, an `umask`-derived mode, and `mtime≈now` --- none of
which match what Nix's reference daemon presents for valid store paths. The
serve-side hardcode is defense-in-depth on top of
#rref("builder.nar.canonical-mtime"): even if a cache file's on-disk
timestamp drifts (filesystem maintenance, a future cache backend that
forgets to canonicalize), the build never sees it. Canonical permissions
also prevent a build from observing a writable mode on an input store path
and attempting an in-place mutation (overlayfs would silently copy-up; a
stock Nix build would `EACCES` --- a behavior divergence worth avoiding even
though no in-tree build relies on it). Symlinks are the one exception:
`canonicalisePathMetaData` never chmods them (Linux has no `lchmod(2)`), so a
stock daemon presents the Linux-immutable `0o777`; rio mirrors that exactly.
This does not reintroduce the writable-mode concern --- Linux ignores symlink
permission bits for access control (`man 7 path_resolution`), so the perm
value carries no semantics beyond cross-builder parity.

== Input Warm-up & Manifest Prime

*Retired (1d builder collapse — scheduler-pushed prefetch):*
`builder.warmgate.handshake` and `builder.warmgate.filter` described the
scheduler-pushed `PrefetchHint` → `PrefetchComplete` warm-gate handshake and
its I-212 path filter. The handshake's scheduler half retired with the stream
placement layer (1c' deletion commit B); the builder half — the hint handler,
its semaphore-bounded fetch tasks, and the two prefetch outcome counters
(rio_builder_prefetch_total and rio_builder_prefetch_filtered_total)
— is deleted with the stream client, and those two metrics are retired with
it (no replacement series: the warm path they measured no longer exists).
Surviving carriers of the load-bearing content: the input-closure warm-up is
the executor's own manifest prime (#rref("builder.warmgate.manifest-prime"))
plus JIT lookup, and the "never materialize a path the build cannot read"
property is owned by the JIT allowlist
(#rref("builder.fuse.jit-register"), #rref("builder.fuse.jit-lookup")), whose
classification helper (`jit_classify`) is unchanged.

#r("builder.warmgate.manifest-prime")[
  After computing the input closure and before daemon spawn, the executor
  issues ONE `BatchGetManifest` for the full closure and primes the FUSE
  cache's manifest-hint map (basename → `ManifestHint`). Each subsequent JIT
  `GetPath` carries the primed hint so the store skips its two PG lookups
  (#rref("store.get.manifest-hint")). Any `BatchGetManifest` error degrades to
  a no-op --- per-path `GetPath` then queries PG as before. I-110c: \~1600 PG
  hits per builder collapse to ≤2.
]

== FUSE Implementation

#r("builder.fuse.fetch-bounded-memory")[
  `ensure_cached` MUST stream NAR bytes to a same-filesystem spool file and
  extract via a bounded-memory streaming restore (`restore_path_streaming`);
  it MUST NOT hold the full NAR `Vec<u8>` or the parsed `NarNode` tree in
  memory. Peak per-fetch heap is O(chunk size) --- one 256 KiB gRPC chunk plus
  a `BufReader` --- not O(NAR size). A 1.8 GB input previously held \~3.6 GB
  peak (NAR bytes + parsed tree), OOMing 1 Gi-limit builders during input
  fetch; the streaming path bounds this to under 1 MiB regardless of NAR size.
]

#r("builder.fuse.fetch-progress-timeout+2")[
  `fuse_fetch_timeout` MUST bound the idle gap between successive stream
  messages (`GetPath` initial response and each subsequent NarChunk), NOT the
  wall-clock duration of the whole fetch. A stalled store (no message for
  `fuse_fetch_timeout`) MUST trip `DeadlineExceeded` → `EIO`, preserving the
  I-165 circuit-breaker latency (60s × 5 consecutive failures = 300s to
  circuit-open). A healthy store streaming a multi-GB NAR (I-211: 2.9 GB
  `clang-21.1.8-debug`) MUST complete regardless of total duration as long as
  every inter-message gap is below the timeout. The pre-I-211 wall-clock bound
  aborted such fetches mid-stream → daemon EIO → build failure on a healthy
  store. Concurrent `WaitFor` threads bound staleness (time since the fetcher
  last heartbeat progress on retry), not total elapsed, so a fetcher in its
  retry loop is not abandoned.
]

#r("builder.fuse.retry-jitter")[
  The JIT fetch retry loop MUST apply per-attempt full jitter (`delay ×
  U(0.5, 1.5)`) to the `RETRY_BACKOFF` schedule, and MUST treat
  `tonic::Code::Aborted` as transient (retry) alongside
  `Unavailable`/`Unknown`/`ResourceExhausted`. Under thundering-herd (I-189:
  `hello-deep-256x` ≈ 38000 drvs, hundreds of builders all `GetPath`-ing the
  same 164 MB gcc within seconds), every builder hits the same h2 stream reset
  and then retries at the same instant --- without jitter the retry IS the
  herd, and a fixed 7.6 s budget exhausts. The store returns `Aborted` for
  retryable PG conflicts (Serialization, Deadlock) with explicit retry intent;
  without it in the transient set the no-manifest-hint fallback path EIOs
  immediately on PG contention.
]

#r("builder.fuse.lookup-caches+2")[
  The FUSE daemon is implemented using the `fuser` crate and runs as part of
  the builder process (not a sidecar). It handles:
  - `lookup`: *Top-level lookups* (direct children of the FUSE root, i.e.,
    store basenames like `abc...-hello`) consult the per-build JIT allowlist
    (see #rref("builder.fuse.jit-lookup")). Names that ARE registered inputs
    MUST be materialized (whole store-path tree on disk) before returning ---
    the kernel caches the lookup attr with 1h TTL and never calls `getattr`,
    so child lookups (`lookup(busybox_ino, "bin")`) would hit an empty cache →
    ENOENT otherwise. *Child lookups* (inside an already-materialized tree)
    hit local disk directly with `symlink_metadata` --- no gRPC.
  - `getattr`: Return file metadata from cached path info
  - `read`/`readlink`/`readdir`: Serve content from local SSD cache, fetching
    from rio-store on cache miss
]

#r("builder.fuse.listxattr-empty")[
  `listxattr` on a FUSE-served store path MUST return an empty list (not an
  error) when queried with a non-zero buffer; replying with a
  `fuse_getxattr_out{size:0}` struct to a non-zero-buffer query trips the
  kernel's `fuse_verify_xattr_list` zero-length-name check and surfaces as
  `-EIO` to the caller (e.g., Python's `shutil.copy2`).
]

#r("builder.fuse.circuit-breaker+3")[
  The FUSE fetch path has a circuit breaker. Two trip conditions (EITHER opens
  the circuit): (a) `threshold` (default 5) consecutive fetch failures; (b)
  `last_success.elapsed() > wall_clock_trip` (default 720s) AND at least one
  failure since the last success --- catches the degraded-but-alive store
  (accepting connections, serving slowly) without waiting for 5×fetch-timeout.
  The failure-gate on (b) is critical: an idle build (no store traffic for
  >720s, e.g., a long sleep) has a stale `last_success` but a healthy store
  --- without the gate, the first post-idle fetch trips → EIO on upload →
  InfrastructureFailure → reassign loop. After `auto_close_after` (default
  30s) the circuit goes half-open: the next `check()` probes --- success
  closes the circuit, failure re-opens it. Every singleflight `Fetch` owner
  (`ensure_cached` AND `prefetch_path_blocking`) checks the breaker before
  fetching and records the outcome after --- under singleflight a
  prefetch-owned failure is observed by FUSE waiters via EIO, so prefetch is
  NOT silent and MUST feed the breaker. The fetch timeout is
  #(refs.cfg)("builder", "fuse_fetch_timeout_secs") (default
  #(refs.cfg-default)("builder", "fuse_fetch_timeout_secs")) from
  `builder.toml` --- NOT the
  global `GRPC_STREAM_TIMEOUT`. *CRITICAL: std::sync ONLY* --- FUSE callbacks
  run on fuser's thread pool, NOT in a tokio context. `AtomicU32` +
  `parking_lot::Mutex`; zero `tokio::sync`, zero `.await`.
]

*Retired (1d builder collapse — the heartbeat task):*
`builder.heartbeat.rpc-timeout` and `builder.heartbeat.store-degraded`
described the periodic heartbeat loop (per-RPC timeout strictly below the
interval; the `store_degraded` capacity flag). The heartbeat task, its
constants, and the retired `HeartbeatRequest`/`HeartbeatResponse` wire surface are
deleted with the stream session: pod liveness belongs to the kubelet/Job
lifecycle, attempt liveness to the durable open-attempt row plus the
establishment sweep (#rref("sched.attempt.establishment-window")), and there
is no scheduler-side capacity state left for a degraded-store flag to gate —
the FUSE circuit breaker (#rref("builder.fuse.circuit-breaker+3")) still
fails the affected build fast, and the resulting infra-classed outcome
reaches the scheduler through the normal report/retry path. The retirement
traded away "wait out the outage": a correlated store outage turned every
affected build into an ordinary chargeable infra failure, draining retry
budgets fleet-wide and, at the infra-window edge, reaching poison — the
fleet-amplification trade-off the heartbeat-era flag had absorbed. The
successor restores the signal AT CLASSIFICATION instead of as capacity
state (bug_408): the breaker's verdict rides the completion report.

#r("builder.outcome.store-degraded+3")[
  A `CompletionReport` whose status is `INFRASTRUCTURE_FAILURE` MUST carry
  `BuildResult.store_degraded = true` when any store-evidence lane saw
  the store degraded — the FUSE-breaker lane (breaker open at completion
  time or its monotonic trip count rose during the build), the
  upload-transport lane (the output upload exhausted its retries with a
  final status of `UNAVAILABLE`, `UNKNOWN`, or `DEADLINE_EXCEEDED`), or
  the metadata-fetch lane (the input-metadata fetch failed `UNAVAILABLE`,
  `UNKNOWN`, or `DEADLINE_EXCEEDED`) — and MUST NOT carry the flag for
  any other status or for an infra failure with no lane evidence; both
  transport lanes MUST consume one shared store-unreachable code
  predicate (the alphabet has exactly one executable source), and the
  lane fold over the executor error alphabet MUST be exhaustive, every
  variant naming its lane or named laneless.
] The during-the-build half of the FUSE lane (trip-count delta, not a
point-in-time `is_open()`) is what catches the open-then-auto-closed
window: the 30s auto-close beats most build durations, and a one-shot
pod's breaker is always fresh-closed at spawn. The upload-transport lane
classifies only transport unreachability — an answered upload (any other
gRPC code, including the NAR-wrapping `INTERNAL`) is the store's verdict
or a local fault, not unreachability. The scheduler routes the flagged
class to an uncharged backoff requeue
(#rref("sched.retry.store-degraded-uncharged")).

- `open`: Open the already-materialized local file (fast path, since `lookup`
  fetched the tree). Falls back to `ensure_cached()` on ENOENT. With
  passthrough enabled, hands the kernel a backing fd via `open_backing()` so
  subsequent `read()` calls bypass userspace. *Warm-up is separate* --- the
  executor primes manifest hints for the input closure before daemon spawn
  (#rref("builder.warmgate.manifest-prime")), not triggered by `open()`.

== FUSE Design Notes

The FUSE daemon is split across submodules: `fuse/mod.rs` (daemon lifecycle,
mount management, `NixStoreFs` struct), `fuse/ops.rs` (the `Filesystem` trait
impl --- all kernel callbacks: `lookup`, `getattr`, `open`, `read`,
`readlink`, `readdir`, `forget`), `fuse/inode.rs` (bidirectional inode↔path
map with kernel `nlookup` refcounting), `fuse/lookup.rs` (attribute helpers:
`stat_to_attr`, `ATTR_TTL`), `fuse/read.rs` (file-range read helper + errno
translation), `fuse/cache.rs` (LRU cache management, SQLite-backed), and
`fuse/fetch.rs` (`ensure_cached`: NAR fetch + extract from rio-store). The
FUSE daemon handles concurrent access from multiple overlays via `Arc<Cache>`
with a read-mostly access pattern --- store paths are immutable, so concurrent
reads require no synchronization beyond the cache index.

*`fuser` 0.17 API (validated in Phase 1a spike):*

The `fuser` 0.17 crate includes breaking API changes from 0.14/0.15 that
affect the FUSE daemon implementation:
- `Filesystem` trait data-path methods (e.g., `lookup`, `read`, `open`,
  `readdir`) changed from `&mut self` to `&self`, requiring interior
  mutability patterns (`RwLock`, `Atomic*`) for all mutable state. Lifecycle
  methods (`init`, `destroy`) retain `&mut self`.
- Raw integer parameters replaced by newtypes: `INodeNo(u64)`,
  `FileHandle(u64)`, `Generation(u64)`, `LockOwner(u64)`, `Errno`,
  `FopenFlags`, `OpenFlags`, `AccessFlags`.
- Mount configuration uses a `Config` struct with `mount_options:
  Vec<MountOption>`, `acl: SessionACL` (replaces `MountOption::AllowOther`
  with `SessionACL::All`), `n_threads`, and `clone_fd`.
- Passthrough API: `KernelConfig::set_max_stack_depth(1)` in `init()`,
  `ReplyOpen::open_backing(impl AsFd) -> Result<BackingId>`,
  `ReplyOpen::opened_passthrough(FileHandle, FopenFlags, &BackingId)`.
  `BackingId` must be kept alive (via a map keyed by file handle) until
  `release()`.

#info(title: [Fallback architecture])[
  If the FUSE+overlay spike (Phase 1a) fails, the fallback is a bind-mount
  approach with `nix-store --realise` pre-materialization. All input store
  paths are fully materialized on the builder's local disk before the build
  starts and bind-mounted into the sandbox. This trades lazy loading for
  simplicity and eliminates the FUSE dependency, at the cost of higher
  pre-build latency (full closure materialization instead of on-demand
  fetching). *Phase 1a result: GO --- the FUSE+overlay approach works;
  fallback not activated.*
]

= Builder Nix Configuration

Builder pods ship a minimal `nix.conf` with an optional operator override from
the `rio-nix-conf` ConfigMap, mounted *as a directory* at `/etc/rio/nix-conf/`
(not via `subPath` --- an optional ConfigMap with `subPath` produces an empty
directory rather than a clean ENOENT, which caused `substitute=true` to
silently re-enable and hang on DNS). `setup_nix_conf` checks
`/etc/rio/nix-conf/nix.conf` first; if present, it's copied into the overlay.
Otherwise the compiled-in default is used:

```ini
# Prevent build hook recursion --- workers ARE the builders
builders =
# All substitution handled by rio-store; don't try external substituters
substitute = false
# Enable sandbox for build purity
sandbox = true
# Hard-fail if sandbox setup fails (never fall back to unsandboxed builds)
sandbox-fallback = false
# Prevent derivations from accessing paths outside the Nix store during eval
restrict-eval = true
# Content-addressed derivation support (Phase 2c+)
experimental-features = ca-derivations
```

#info(title: [Security note])[
  `__noChroot` derivations (which disable the sandbox) are rejected at the
  gateway level before they ever reach a builder. See Derivation Validation in
  the security chapter.
]

This configuration ensures workers only build derivations locally and never
attempt to delegate or substitute externally.

== Builder Capabilities

Each builder resolves two capability lists at startup; the scheduler applies
the same vocabulary server-side when it filters spawn intents per pool
(`GetSpawnIntentsRequest.{kind, systems, filter_features}`), so the pod that
pulls a derivation was spawned from a pool whose capabilities already match:

- *`systems`* (`Vec<String>`): Nix system identifiers this builder can build
  for (e.g., `x86_64-linux`, `aarch64-linux`). The scheduler's `hard_filter()`
  does an *any-match* against the derivation's `system` field. If unset, the
  builder auto-detects a single element as `{arch}-{os}` via
  `std::env::consts`. Multi-element configurations are for qemu-user-static or
  cross-arch builders. Configure via `RIO_SYSTEMS=x86_64-linux,aarch64-linux`
  (comma-separated env), `systems = ["x86_64-linux"]` (TOML array), or
  repeated `--system` CLI flags.
- *`features`* (`Vec<String>`): `requiredSystemFeatures` this builder supports
  (e.g., `kvm`, `big-parallel`). The scheduler's `hard_filter()` does an
  *all-match* --- every feature the derivation requires must be present here,
  or `rejection_reason()` reports `feature-missing`. Empty by default.
  Configure via `RIO_FEATURES`, `features` in TOML, or repeated `--feature`
  flags.

#warning(title: [Recursive Nix is not supported])[
  Derivations that invoke Nix internally (`__recursive` / `recursive-nix`
  experimental feature) will fail because `substitute = false` and `builders
  =` prevent the inner Nix from fetching dependencies or delegating builds,
  and `recursive-nix` is not in `experimental-features`. This is an explicit
  non-goal for the initial release. Supporting recursive Nix would require the
  builder to act as both a builder and a store client for the inner Nix
  instance, significantly complicating the builder architecture.
]

= rio-nix Client Protocol

#r("builder.daemon.stdio-client")[
  Builders invoke `nix-daemon --stdio` and must speak the Nix worker protocol
  as a *client*. The `rio-nix` crate implements both server-side (gateway:
  responds to opcodes from Nix clients) and client-side (builder: sends
  `wopBuildDerivation` to the local daemon and receives `BuildResult`)
  protocol handling.
]

#r("builder.daemon.no-unwrap-stdio")[
  When spawning `nix-daemon --stdio`, never `.unwrap()` on
  `daemon.stdin.take()` / `daemon.stdout.take()` --- use `.ok_or_else()`.
]

#r("builder.daemon.timeout-wrap")[
  Wrap all daemon communication in `tokio::time::timeout` (default: 2h,
  configurable via `RIO_DAEMON_TIMEOUT_SECS` / `--daemon-timeout-secs` /
  `builder.toml`).
]

#r("builder.daemon.kill-both-paths")[
  Always `daemon.kill().await` in both success and error paths, and set
  `kill_on_drop` on the Command to guard against early-exit leaks.
]

#r("builder.daemon.negotiated-version")[
  The builder MUST pass the version negotiated by `client_handshake` to all
  subsequent version-dependent wire reads (notably `read_build_result`).
  Hardcoding `PROTOCOL_VERSION` desyncs on the next protocol bump if the
  pinned daemon lags, or on any container rebuild with a daemon that
  negotiates lower. `client_handshake` enforces a `MIN_DAEMON_VERSION` floor
  (mirroring `server_handshake`'s `MIN_CLIENT_VERSION`) so an
  unexpectedly-old daemon fails at handshake with a clear error rather than a
  wire desync at result-read.
]

#r("builder.retry.daemon-transient")[
  The build-spawn loop retries `execute_build` locally when the failure is
  daemon-transient: `DaemonSpawn` (nix-daemon failed to exec), `Handshake`
  (daemon died before protocol negotiation), or `Wire(Io(UnexpectedEof))`
  (daemon crashed mid-conversation --- core dump, OOM-kill, SIGABRT). Up to
  `DAEMON_RETRY_MAX=3` attempts with backoff `500ms/1s/2s` (no jitter --- one
  daemon per pod, no herd). After exhaustion the error propagates as
  `InfrastructureFailure` and the scheduler's own retry policy takes over. The
  retry MUST short-circuit if the build's cancelled flag is set.
  `BuildFailed`, network-side errors (`Upload`/`Grpc`/`MetadataFetch`), and
  deterministic setup failures (`Overlay`/`SynthDb`/`NixConf`) are NOT retried
  locally. Rationale: a scheduler round-trip re-dispatches + re-fetches
  closure + re-generates synth DB; without the local retry a hot-loop daemon
  crash flooded the scheduler with 800+ `InfrastructureFailure` reports in
  \<10min.
]

#r("builder.silence.timeout-kill")[
  `maxSilentTime` (seconds, forwarded from client `--option max-silent-time`)
  is enforced rio-side in the stderr read loop: on each `STDERR_NEXT` and
  `STDERR_RESULT BuildLogLine` (types 101/107 --- the output-producing
  messages), reset `last_output`; a `select!` arm fires at `last_output +
  max_silent_time` → `BuildResult { status: TimedOut, error_msg: "no output
  for Ns (maxSilentTime)" }` → caller's unconditional `cgroup.kill()`.
  Activity/Progress chatter does NOT reset the timer --- a build spinning
  progress updates with no stderr output is still "silent". The local
  nix-daemon MAY also enforce it (forwarded via `client_set_options`) ---
  rio-side is the authoritative backstop ensuring the correct `TimedOut`
  status regardless.
]

Before the build process starts, the worker writes a 3-line `rio:` header
(`exec`, `builder`, `started`) as a direct `BuildLogBatch` at line 0; after the
process exits it writes a 2-line footer (`exec`, `result`) at the final line
offset. Both go to the same per-execution rio-store log uploader as build
output, *not* through the `LogBatcher` (which is created and consumed inside
the daemon lifecycle). The `LogBatcher` is seeded with the header line count so the
build's real output numbers after the header. The header carries the
`WorkAssignment.exec_id`, the system + `hw_class`, and the assigned resource
triple --- never pod or node identity. The banner is per-execution, not
per-attempt: the daemon-transient retry loop
(#rref("builder.retry.daemon-transient")) re-invokes the executor up to
`DAEMON_RETRY_MAX` more times for one `exec_id`, but the header is sent only on
the first attempt and the footer once after the loop with the most recent
daemon-running attempt's outcome (overridden to `cancelled` by the assignment's
cancel flag) --- re-emitting the banner per attempt would
write conflicting `rio: result` lines and violate the store's monotone
line-number gate (#rref("store.log.ingest-bounds")). Subsequent attempts seed
the `LogBatcher` with the prior attempt's final line count so output line
numbers continue into the same `AppendLog` session. The normative
requirement and the display-only / no-pod-identity rationale live in
#rref("obs.log.worker-header") in the observability spec.

#r("builder.daemon.stderr-result-logs")[
  Modern `nix-daemon` sends build output via `STDERR_RESULT` with
  `BuildLogLine`, NOT raw `STDERR_NEXT`. The builder's stderr loop MUST handle
  `STDERR_RESULT` --- otherwise all build logs are silently dropped.
]

#r("builder.stderr.forward-set-phase")[
  The builder's stderr loop forwards the daemon's `STDERR_RESULT{SetPhase}`
  (result type 104) as a `BuildPhase{derivation_path, phase}`
  `ExecutorMessage`. Phase is a state edge, not log content --- it is sent
  unbatched and does not reset the max-silent-time deadline.
]

#r("builder.stderr.msg-cap")[
  The stderr loop enforces a hard cap of `MAX_BUILD_STDERR_MESSAGES` (10M)
  protocol frames per build, counted at `dispatch()` so every variant
  including `SetPhase`/activity-lifecycle is covered. Exceeding it terminates
  with `BuildStatus::LogLimitExceeded` --- same non-retryable semantics as the
  byte limit.
]

#r("builder.log-limit+2")[
  The log batcher enforces per-build `LogLimits`. `total_bytes` (cumulative
  across flushed batches) is a hard cap: a line whose PROSPECTIVE total would
  exceed it is rejected, `add_line` returns `LimitExceeded{reason}`, the
  stderr loop flushes already-buffered lines and breaks with
  `BuildStatus::LogLimitExceeded` --- terminal, non-retryable (same build on a
  different executor spews the same logs). Maps to
  #(refs.metric)("rio_builder_builds_total")`{outcome="log_limit"}`.
  `rate_lines_per_sec` (1-second tumbling window, monotonic `Instant`) is a
  suppression threshold: excess lines within a window are DROPPED, and a
  single `[rio: N lines suppressed by log_rate_limit (M lines/s)]` marker is
  injected at the next window reset. The build continues. Dropped lines do not
  count toward `total_bytes`. Maps to
  #(refs.metric)("rio_builder_log_lines_suppressed_total"). Either limit set
  to `0` means unlimited.
]

#r("builder.log.loss-disclosure+5")[
  Every line handed to the log-upload data plane that is not durably stored
  MUST be disclosed through one chokepoint that derives the loss from the
  abandonment reason: an upload that ends with un-acked lines increments
  #(refs.metric)("rio_builder_log_drain_abandoned_total")`{reason}` and logs
  at `error!` if and only if the lines are durably lost --- the sole zero-loss
  reason is the store provably holding the complete `[0, final_line_count)`
  log. The disclosure partition MUST be TOTAL over the produced population:
  lines the uploader accepted disclose from the unwind guard (`panic`);
  lines resident in the upload channel disclose through the
  recv-to-`None` exhaustion protocol on every NORMAL task exit --- the only
  lossless teardown the channel offers, since a send permit granted before
  `close()` can still deliver after it --- and the task MUST hold the
  input-exhaustion witness before minting a terminal status; when the
  receiver instead dies UNWINDING (the one path that cannot run the async
  drain), the residue discloses from the receiver's own BEST-EFFORT drop
  guard (`uploader_dead` --- they never reached the
  uploader's accounting, so no ack can cover them and no progress snapshot
  counts them; the guard closes, lets cross-thread permits granted before
  the close land within a bounded spin, and discloses what it drained ---
  a permit delivery that outlives even that bounded window is the
  unwind path's documented residual); lines the channel refused disclose producer-side as
  `uploader_dead`: inside the stderr loop through the discard-ledger sink
  (ONE accumulated total at teardown); banner/footer sends --- which run
  outside the stderr loop --- disclose directly at the send site through
  the same chokepoint, one disclosure per bounced batch (an uploader death
  may therefore mint several disclosure events; the law is per-line
  conservation, not one-event-one-disclosure). A permanent store rejection
  arriving mid-stream MUST terminate the session loop (no reconnect can
  succeed against it), and a panic in the upload task MUST disclose during
  unwind, independent of whether any caller awaits the task. The
  stderr-loop producer-side sink MUST make an uncounted discard
  unrepresentable --- once the upload channel is gone, the only way for the
  stderr loop to drop a batch is the ledger method whose teardown discloses
  the accumulated total. Footer lines MUST be folded into the reported
  `final_line_count` ONLY when their send succeeded: sealing lines that
  exist nowhere makes the store's contiguous-coverage completeness
  predicate permanently unsatisfiable with no disclosure.
]
The reason vocabulary is the `x-rio-log-reject` metadata class the store
attaches to permanent rejections (`cap`/`complete`/`superseded`) plus the
builder-local `deadline_expired`, `panic`, and `uploader_dead`; bare-code
fallbacks (`FAILED_PRECONDITION` → complete, `PERMISSION_DENIED` →
superseded) keep the mapping total against pre-metadata stores. The
producer-side ledger is disjoint from the upload task's own unwind guard by
construction: the guard covers lines the uploader ACCEPTED, the ledger only
lines the closed channel REFUSED (the bounced batch that detected the death
seeds it), so the two surfaces partition the loss exactly.

= Overlay Store Architecture

#r("builder.overlay.per-build")[
  Each active build gets its own overlayfs mount with a separate upper
  directory and work directory. A synthetic Nix store SQLite database is
  placed in each overlay's upper layer so that Nix recognizes the input paths.
]

#r("builder.exec.build-id-sanitized")[
  The per-build identifier (used as the overlay directory name and the cgroup
  v2 sub-cgroup name) is the basename of the derivation store path with every
  byte outside `[A-Za-z0-9_-]` replaced by `_`. Derivation names from nixpkgs
  are not constrained to filesystem-safe or URL-safe characters --- e.g.
  `fetchpatch` against a Gentoo mirror yields names containing `?id=<sha>`
  (I-167). The sanitized form MUST be safe to embed in a cgroup v2 directory
  name, a filesystem path component, and a `sqlite://` URI without further
  escaping.
]

#r("builder.overlay.stacked-lower+2")[
  The overlay lower is the FUSE mount only (`lowerdir={fuse_mount}`). The host
  `/nix/store` is *not* in the lowerdir: `nix-daemon` runs with `--store
  'local?root={build_dir}'` and reads its own binary + libs from the host
  store directly, while its store operations target `{build_dir}/nix/store`.
  The per-build store therefore contains exactly the build's input closure
  (lower) plus its outputs (upper) --- the daemon's runtime closure is
  structurally outside it (see I-060 in @sec-ns-order).
]

#r("builder.overlay.userns-exdev")[
  *Userns directory-rename constraint (I-185):* When the builder pod runs with
  `hostUsers: false` (ADR-012), `setup_overlay`'s `mount(2)` happens inside a
  non-init user namespace; the kernel forces `redirect_dir=off` (and refuses
  `redirect_dir=on`) on such mounts. overlayfs without `redirect_dir` returns
  `EXDEV` for any `rename(2)` of a *directory* whose target parent is a
  merge-type dir --- and the overlay root (= the chroot-store `realStoreDir`)
  is always merge-type (its dentry stack carries both the upper root and the
  lower root by construction). nix-daemon's post-build
  `movePath(chrootRootDir/nix/store/{out} → realStoreDir/{out})` is a raw
  `std::filesystem::rename` with no fallback, so every directory-typed output
  fails; file-typed outputs rename fine. nix's `moveFile()` temp-then-rename
  fallback also targets the overlay root and hits the same `EXDEV`. The
  builder image ships a patched `nix` whose `movePath()` falls back to a
  recursive copy on `EXDEV`
  (`nix/patches/nix-movepath-exdev-fallback.patch`).
]

#r("builder.overlay.upper-not-overlayfs")[
  *Filesystem constraint (validated in Phase 1a spike):* The overlayfs upper
  and work directories must reside on a different filesystem than the FUSE
  lower layer. The kernel rejects overlay mounts where upper and lower are on
  the same filesystem when the lower is a FUSE mount. In practice, the
  upper/work directories should be on the builder's local SSD (`emptyDir` or
  PVC), while the lower is the FUSE mount at `/var/rio/fuse-store`. The upper
  also MUST NOT itself be on an overlayfs (containerd root overlay) ---
  overlayfs-as-upperdir cannot create `trusted.*` xattrs and `mount()` returns
  `EINVAL`.
]

After build completes:

+ Read new paths from upper layer
+ Chunk and upload to rio-store (@cas). Each `PutPath` request carries the
  scheduler-issued HMAC @assignment-token in the `x-rio-assignment-token` gRPC
  metadata header; the store verifies the token and rejects uploads for paths
  not in `claims.expected_outputs` (see #rref("common.hmac.claims+3"))
+ Register path metadata (@narinfo, references)
+ Discard upper layer

*Teardown failure handling:* Overlay teardown (`umount2`) can fail if the
mount is stuck busy (open file handles, zombie `nix-daemon`, FUSE hang). A
leaked mount increments
#(refs.metric)("rio_builder_overlay_teardown_failures_total"); the pod is
one-shot, so the leak is bounded to that single build and discarded with the
pod's emptyDir.

== Multi-Output Derivation Upload

#r("builder.upload.idempotent-precheck")[
  Before uploading, the builder batch-checks all scanned outputs via
  `FindMissingPaths`. Outputs already present in the store (`'complete'`
  manifest exists) are skipped --- `QueryPathInfo` fetches the existing
  `nar_hash`/`nar_size` instead of re-reading disk + re-streaming the NAR. The
  skip is *best-effort*: if `FindMissingPaths` errors (store transient), all
  outputs fall back to the upload path and #rref("store.put.idempotent")
  catches duplicates server-side. The skip saves the pre-scan disk read, the
  NAR-stream disk read, and the gRPC stream setup --- NOT a correctness
  requirement. Emits
  #(refs.metric)("rio_builder_upload_skipped_idempotent_total") per skipped
  output.
]

#r("builder.upload.multi-output")[
  Derivations may produce multiple outputs (e.g., `out`, `dev`, `lib`). After
  a build completes:
  + *Detect outputs*: Scan the overlay upper layer for all new store paths. A
    multi-output derivation produces one path per output (e.g.,
    `/nix/store/abc...-hello`, `/nix/store/def...-hello-dev`).
  + *NAR each output*: Serialize each output path independently into a NAR
    archive.
  + *Chunk*: Split each NAR into content-addressed chunks (matching
    rio-store's chunk size).
  + *Upload*: Upload chunks to rio-store in parallel across outputs.
    Deduplicate against existing chunks (CAS).
  + *Register*: Register each output path's NAR hash, NAR size, references,
    and deriver with rio-store. Signatures are sent empty --- output signing
    is done store-side.
]

#r("builder.upload.references-scanned")[
  Before the retry loop, `upload_output` performs a *pre-scan pass*: a single
  extra disk read through `RefScanSink` only (no hash, no network). The NAR is
  dumped via `dump_path_streaming` into the scanner, which finds every
  candidate hash part embedded anywhere in the stream (including inside
  binaries, RPATH strings, symlink targets, directory names). The candidate
  set is the *transitive input closure* ∪ `drv.outputs()`: every path
  reachable via BFS over store references from the derivation's inputs, plus
  all of this derivation's own outputs (for self-references and cross-output
  references). This matches Nix's `computeFSClosure`
  (`derivation-building-goal.cc:444,450` / `derivation-builder.cc:1335-1344`).
  A build can legitimately embed any transitively-reachable path --- e.g.
  `hello-2.12.2` references `glibc`, which is not a direct input but arrives
  via `closure(stdenv)`. The resolved reference list is *sorted* (affects the
  narinfo signature fingerprint --- must be deterministic).
]

#r("builder.upload.deriver-populated")[
  `PathInfo.deriver` is set to the `.drv` store path of the derivation that
  produced this output. The deriver is the same for all outputs of a
  multi-output derivation.
]

#info(title: [Pre-scan cost])[
  The scan is a separate disk read before the first upload attempt. Retries do
  NOT re-scan (the scan result is deterministic). The Boyer-Moore skip-scan
  over the restricted @nixbase32 alphabet does \~memcpy speed on binary
  sections (skips \~31/32 bytes); a 4 GiB output adds \~4s wall time on NVMe.
  If this becomes measurable, the escape hatch is a trailer-refs protocol
  extension (send refs in `PutPathTrailer` instead of the first `PathInfo`
  message) --- deferred to a later phase.
]

#r("builder.upload.batch+2")[
  For *multi-output derivations (≥2 outputs)*, the builder uses
  `PutPathBatch`: all outputs stream serially on one RPC, the store commits
  them in ONE database transaction. If any output fails validation, zero
  outputs are registered --- atomic per #rref("store.atomic.multi-output").
  All per-output prep (path parse, reference scan) is done BEFORE the first
  byte is sent, so a local prep failure on output $k$ cannot leave outputs
  $0..k-1$ committed. The batch RPC's stream timeout scales with output count
  (`GRPC_STREAM_TIMEOUT × N`, capped at `MAX_BATCH_OUTPUTS`). Batch retries up
  to `MAX_UPLOAD_RETRIES` on transient errors; on `FailedPrecondition` it
  falls through to independent `PutPath` calls (pre-P0267 behavior:
  `buffer_unordered(MAX_PARALLEL_UPLOADS)`, no cross-output atomicity).
]

For *single-output derivations*, the builder uses independent `PutPath`
directly (atomicity is vacuous for one output).

*Upload failure handling:* If the upload to rio-store fails (S3 unavailable,
network timeout), the builder retries the upload with exponential backoff (up
to `MAX_UPLOAD_RETRIES` (8) attempts). If all upload retries are exhausted,
the builder reports an `InfrastructureFailure` to the scheduler. The scheduler
may reassign the derivation to a different builder, which must rebuild from
scratch --- there is no mechanism to transfer the completed output from the
original builder's local overlay. This is a known limitation; the completed
output on the original builder is lost when the overlay is discarded.

#r("builder.upload.aborted-poll")[
  When `PutPath` returns `Aborted` with message containing `"concurrent
  PutPath"`, the builder polls `QueryPathInfo` with backoff (1s, 2s, 4s, 8s,
  16s ≈ 31s total) before falling back to a fresh upload attempt. If the path
  appears, the builder adopts the store's `PathInfo` as its upload result
  (#(refs.metric)("rio_builder_uploads_total")`{status="adopted"}`) --- output
  paths are derivation-addressed, so the contending uploader's content is
  identical. If the poll exhausts, the contending placeholder has likely been
  released by the store's drop-path cleanup (#rref("store.put.drop-cleanup+3"))
  and the next upload attempt succeeds. `QueryPathInfo` errors during the poll
  are treated as not-found (logged, keep polling). Other `Aborted` reasons (GC
  mark serialization, admin cancel) keep the plain retry without polling.
  I-125b.
]

= Store Database Management

#r("builder.synth-db.per-build")[
  Nix requires a functional store database (SQLite at
  `/nix/var/nix/db/db.sqlite`) to operate. It refuses to build derivations
  whose inputs are not registered in the local database, even if the paths
  physically exist on disk.
]

For each build, the builder synthesizes a minimal SQLite database in the
overlay upper layer:

+ Query rio-store's PostgreSQL for path metadata of the build's input closure
  (deriver, NAR hash, NAR size, references, sigs, ca).
+ Generate the database via direct SQLite writes into the overlay's upper
  layer at `var/nix/db/db.sqlite`. Use a single transaction with `PRAGMA
  journal_mode=WAL` and `PRAGMA synchronous=OFF` for maximum speed (the DB is
  ephemeral).
+ The database must include the `ValidPaths`, `Refs`, and `DerivationOutputs`
  tables with proper indexes (`IndexValidPathsPath`, `IndexValidPathsHash`).
  The `SchemaVersion` in the `Config` table must match the Nix version running
  in the builder (target: Nix 2.20+ schema).
+ The database contains only path registrations for that specific build's
  input closure --- not the entire store.
+ After the build completes, the synthetic database is discarded along with
  the rest of the overlay upper layer.

#r("builder.synth-db.derivation-outputs")[
  The `DerivationOutputs` table MUST be populated --- `nix-daemon`'s
  `queryPartialDerivationOutputMap()` reads it. Empty → `scratchPath =
  makeFallbackPath(drvPath)` → `OutputRejected`.
]

#r("builder.executor.resolve-input-drvs")[
  The executor must merge resolved inputDrv outputs into `BasicDerivation`
  inputSrcs before constructing the derivation. The sandbox only bind-mounts
  inputSrcs; unresolved inputDrv paths would be invisible.
]

#r("builder.executor.kind-gate")[
  Per ADR-019, the executor re-derives `is_fod` from the `.drv` (ground truth,
  not the scheduler-sent flag) and checks it against `config.executor_kind`
  BEFORE overlay setup or daemon spawn. If `is_fod != (executor_kind ==
  Fetcher)`, the build fails with `ExecutorError::WrongKind`.
  Defense-in-depth --- the scheduler's `hard_filter` should never misroute,
  but a bug or stale-generation race must not grant a builder internet access
  even transiently.
]

#r("builder.synth-db.refs-table")[
  *Critical (validated in Phase 1a spike):* The `Refs` table must accurately
  reflect each path's references. When `sandbox = true`, Nix resolves the
  derivation's input closure by walking the `Refs` table to determine which
  store paths to bind-mount into the sandbox chroot. If references are
  missing, the sandbox will not bind-mount transitive dependencies (e.g.,
  `glibc` needed by `bash`), causing builds to fail with "No such file or
  directory" errors when the builder's dynamic linker cannot be found.
]

Performance: direct SQLite writes handle 1000+ paths in \<50ms. The bottleneck
is the PostgreSQL metadata query, not the SQLite generation.

== Synthetic DB Risks

- *Schema version coupling*: Nix store DB schema (currently version 10) is an
  internal API with no stability guarantees. Pin to a specific Nix version and
  test schema compatibility on upgrade.
- *`Realisations` table*: Required for Phase 5 CA support. Add the table
  structure proactively but leave empty until CA early cutoff is activated.
- *`registrationTime`*: Set to 0 for input paths (not locally built). Only
  outputs built on this builder get a real timestamp.
- *`ultimate`*: Always 0 for input paths (they were not built on this
  builder). Set to 1 only for locally built outputs.
- *Journal mode*: Create with `journal_mode=WAL` (matching Nix's expectation)
  instead of `journal_mode=OFF`. While the DB is ephemeral, Nix may check the
  journal mode on open.

= Concurrent Build Isolation

#r("builder.cgroup.sibling-layout")[
  Per-build cgroups are *siblings* of the builder's own cgroup under the
  delegated root. With systemd `DelegateSubgroup=builds`, the builder lives at
  `.../service/builds/`; per-build cgroups go in `.../service/` as siblings.
  When running in a cgroup-namespace root (containerd in pods:
  `/proc/self/cgroup` shows `0::/`), the builder MUST move itself into a
  `/leaf/` subgroup first so the namespace root becomes the delegated_root ---
  otherwise writing to `/sys/fs/cgroup/` would hit the HOST root.
]

#r("builder.cgroup.ns-root-remount")[
  When running in a cgroup-namespace root (`/proc/self/cgroup` shows `0::/`)
  under a non-privileged security context, the builder MUST remount
  `/sys/fs/cgroup` read-write before creating the `/leaf/` subgroup.
  Containerd mounts `/sys/fs/cgroup` read-only for non-privileged pods even
  with `CAP_SYS_ADMIN`; the `MS_REMOUNT | MS_BIND` call clears the
  per-mount-point RO flag (preserving superblock `nosuid`/`nodev`/`noexec`).
  Under `privileged: true` containerd mounts rw already and the remount is a
  no-op --- this path is load-bearing only in the production `privileged:
  false` + `base_runtime_spec` device-injection configuration (ADR-012).
]

#r("builder.cgroup.memory-peak+2")[
  cgroup v2 `memory.peak` + polled `cpu.stat` provide *tree-wide* resource
  accounting for each build. This fixes the Phase 2c bug where `VmHWM` (daemon
  PID only) measured \~10MB regardless of what the builder consumed.
  `memory.peak` and the polled `cpu.stat` peak MUST be reported in
  `CompletionReport` for every build that reached cgroup attachment, including
  `CgroupOom` / `BuildFailed` / `Upload` outcomes --- 0 is reserved for
  pre-cgroup setup errors.
]

#r("builder.cgroup.kill-on-teardown")[
  On any error exit after the build cgroup is populated, the executor MUST
  write `cgroup.kill` and poll `cgroup.procs` until empty (bounded) before
  dropping the cgroup handle. `daemon.kill()` alone only signals the daemon
  PID; forked builders reparent to init.
]

== Build Resource Limits

A builder pod runs *one* build, then exits. The pod's `resources.limits` ARE
the build's limits --- there is no per-build cgroup `memory.max`/`cpu.max`
layer. A runaway build can @oom only its own pod; the next queued derivation
gets a fresh Job. Operators size the pod via `Pool.spec.resources`.

The per-build sub-cgroup is *measurement and cancellation only*: cgroup v2
`memory.peak` + polled `cpu.stat` for resource accounting
(#rref("builder.cgroup.memory-peak+2")), and `cgroup.kill` for clean teardown
(#rref("builder.cgroup.kill-on-teardown")) without touching the rio-builder
process or its FUSE threads.

#r("builder.cores.cgroup-clamp+2")[
  The executor MUST clamp `build_cores` (passed to nix-daemon via
  `wopSetOptions`, becomes `NIX_BUILD_CORES` → `make -jN`) to
  `ceil(quota/period)` from the pod cgroup's `cpu.max`, minimum 1. It MUST NOT
  pass `build_cores=0`: nix-daemon resolves 0 to host nproc, and cgroup CPU
  quota does not reduce visible cores --- a 0.5-core pod on a 16-core node
  would run `make -j16`, OOM-loop on compiler RSS (I-196). A client-requested
  `build_cores > 0` is capped at the same ceiling. When `cpu.max` reads `max`
  (no quota), use host nproc --- but builder/fetcher pools set `limits.cpu ==
  requests.cpu` (I-197, hard limits, no burst) so production pods always see a
  real quota; the `max` fallback only fires in VM tests / bare-metal dev. The
  same clamped value MUST also be written to the per-build `nix.conf` as
  `cores = N` and `max-jobs = 1`, appended after any operator override (later
  lines win): defense-in-depth against an upstream `wopSetOptions` regression
  where the daemon would otherwise fall back to nix.conf → host nproc.
]

#r("builder.oom.cgroup-watch+3")[
  The executor MUST sample the pod cgroup's `memory.events` `oom_kill` counter
  at build start, poll it during the build, and read it once more
  synchronously at build-exit. On increment, it MUST `cgroup.kill` the
  per-build cgroup and report `InfrastructureFailure` (not `BuildFailed`) so
  the scheduler retries with a doubled memory floor. Without this, an
  under-provisioned build OOM-loops (kernel kills cc1, make respawns) until
  the silence timeout, and the eventual failure is misattributed to the
  derivation. The final synchronous read closes the gap for fast-exit
  toolchains (cargo, single `cc`, `python setup.py`) whose driver exits \~100
  ms after a child OOM --- before the next 1 Hz watcher tick --- which would
  otherwise be misclassified `MiscFailure → PermanentFailure`. If the final
  synchronous read shows an increment but the daemon reported `Built`, the
  build script tolerated the OOM-killed child; the executor MUST keep the
  successful result (discarding it would loop re-dispatch on a build that
  deterministically succeeds-with-child-OOM) while still emitting the OOM
  metric as a sizing signal.
]

#r("builder.disk.quota-classified+2")[
  When a build fails in an ENOSPC-consistent shape AND the project-quota
  usage peak shows the overlay's `dqb_curspace` within
  `DISK_FULL_QUOTA_SLACK_BYTES` of its hard limit AND the node filesystem
  reports at least `DISK_FULL_NODE_HEADROOM_BYTES` free, the executor MUST
  reclassify the result as disk exhaustion (`InfrastructureFailure` carrying
  the pinned `DISK_FULL_MSG` contract substring), never `PermanentFailure`
  --- a quota-exhausted build is a SIZING signal (the cgroup-OOM twin on the
  disk axis). A node-attributed exhaustion (below the headroom floor) MUST
  keep the non-quota lane: the node's exhaustion is not the build's sizing
  signal. The rewrite authority is the typed allow-list shared by BOTH
  sizing overrides (disk and oom): only the ordinary ENOSPC-consistent
  daemon failures may be claimed; `TimedOut`, `LogLimitExceeded`,
  daemon-transient shapes, and the cancel/network/store/permanent lanes are
  NEVER rewritable --- their own sizing/retry laws own them // quantifier: census(sizing_rewrite_authority_rows_pinned)
  --- and the
  authority enumerates every status and executor-error shape with no
  catch-all arm, on either seam.
]

The slack term exists because the ENOSPC-refused write never lands ---
the usage sample sits below the hard limit by up to the refused
write's size. Both thresholds are violable typed constants with recorded
derivations beside the limit-read face (`rio-builder/src/quota.rs`).

#r("builder.disk.satisfiable-letter+2")[
  The disk-exhaustion classification's inputs MUST be satisfiable in the
  PRODUCTION TOPOLOGY: the usage input is the DURING-BUILD peak
  (max-tracked at \>= 1 Hz alongside the cgroup monitors --- `keep-failed`
  is unset, so the daemon deletes a failed build's scratch before any
  post-daemon sample, and `dqb_curspace` carries no kernel high-water mark),
  and the node-headroom input is sampled from a vantage DECOUPLED from the
  project clamp --- the first same-device ancestor that is neither
  project-owned nor clamp-shaped, OR (when the quota'd dir is itself a
  mount root, as in-pod where the overlays emptyDir's parent is
  container-rootfs overlayfs) a same-device sibling mount on the same node
  filesystem outside the project subtree. Under enforced prjquota with
  `PROJINHERIT` the kernel clamps statvfs taken inside the project view to
  `limit − used`, making same-directory conjunct pairs mutually exclusive
  exactly when the quota conjunct holds. No decoupled vantage MUST mean no
  attribution --- never a fabricated headroom.
]

#r("builder.disk.enforcement-posture")[
  The builder MUST surface the project-quota enforcement posture observed
  on the overlay emptyDir as a typed letter (`QuotaEnforcement` ---
  `Enforcing` / `NonEnforcing` / `NoLimit` / `Unavailable`) derived from
  the kernel's own limit record after the projid is in place, emitted via
  the #(refs.metric)("rio_builder_quota_enforcement") gauge (label `mode`)
  and a once-per-pod log line. The DiskFull lane's first conjunct depends on an
  external system's posture (kubelet's `AssignQuota` and the builder's own
  mint policy); the lane's dormancy MUST be a fact in the telemetry, not
  an inference from the lane never firing.
]

The satisfiability witnesses are kernel-level (the prjquota VM probe at
`nix/tests/scenarios/quota-probe.nix` drives the production classifier
chain against a real filled XFS project quota and asserts the clamp, the
retired same-dir vantage's structural false, the decoupled vantage's true,
and the post-cleanup collapse that motivates the peak monitor) AND
production-topology-level (the in-pod mount-root unit witness asserts the
ancestor walk dead-ends and the sibling fallback answers --- merged_bug_012:
without the sibling the conjunct was structurally `None` in every builder
pod). The wire/floor consumption of the letter is pinned at unit level
scheduler-side; the composition seam is the typed completion-report field
family.

*The lane disposition (D-3, recorded 2026-06-13).* The DiskFull
classification lane is DOCUMENTED-DORMANT at the deployed posture: every
builder pool runs `hostUsers: true` (the I-186 FUSE-passthrough pin, until
P0560 --- the same root as the `sec.pod.host-users-false` exception
clause), so kubelet declines projid assignment and the builder mints
monitoring-only projids with no hard limit (`QuotaEnforcement::NoLimit`);
under `hostUsers: false` kubelet's `AssignQuota` writes the `-1`
non-enforcing sentinel at the deployed minors
(`QuotaEnforcement::NonEnforcing`, pinned by the `vm-kubelet-projquota`
cells). In neither posture does the first conjunct of
`classify_quota_exhaustion` hold, so the lane never fires on the fleet ---
kubelet's du-walk eviction (`EvictedDiskPressure`) is the operative
disk-exhaustion signal. The two independent blockers are now both VISIBLE
(the enforcement-posture gauge) or REPAIRED (the in-pod sibling vantage)
rather than silent. Prerequisites for the enforcing flip, all owner-
scheduled and outside this record: (a) the `hostUsers` posture flip or a
kubelet minor that enforces under host-users; (b) a hard-limit mint
(either kubelet's, when it enforces, or the builder's own --- the
deliberate non-goal of `ensure_project_quota` today); (c) the in-pod
vantage (landed); (d) the production-topology witness (landed). The flip's
readback is #(refs.metric)("rio_builder_quota_enforcement") with
`mode="enforcing"` reporting nonzero on a builder pod.

The overlay is per-build. Each build gets its own overlayfs mount with
separate upper and work directories. The Nix sandbox provides process-level
isolation (user, mount, PID, and network namespaces). Even if the Nix sandbox
is compromised, the per-build overlay upper layer ensures rogue writes are
isolated and discarded; the next build runs in a fresh pod and sees none of
it.

= Fixed-Output Derivation (FOD) Handling

#info[
  Per ADR-019, FODs route to the rio-fetcher executor, not builders. Builders
  are airgapped (#rref("builder.netpol.airgap")) and reject any FOD assignment
  with `ExecutorError::WrongKind` (#rref("builder.executor.kind-gate")). The
  section below documents the FOD verification logic that the `rio-builder`
  binary runs when invoked as a fetcher (`RIO_EXECUTOR_KIND=fetcher`).
]

#r("builder.fod.verify-hash")[
  Fixed-output derivations (FODs) have a known output hash declared in
  `outputHash`. They require special handling:
  + *Detection*: A derivation is a FOD if its `outputHash` attribute is
    non-empty.
  + *Network access*: Unlike regular derivations, FODs are allowed network
    access inside the sandbox. This is handled by `nix-daemon` internally ---
    when it sees `outputHash` set on a derivation via `wopBuildDerivation`, it
    automatically relaxes network namespace isolation for that build.
    `sandbox = true` in `nix.conf` is sufficient (Nix's sandbox is
    FOD-aware). Network
    egress is governed at the pod level by the fetcher NetworkPolicy
    (#rref("fetcher.netpol.egress-open")).
  + *Output verification*: After the build completes, the executor computes
    the hash of the output and verifies it matches the declared `outputHash`.
    A mismatch is reported as `BuildResultStatus::OutputRejected` (NOT
    `BuildFailed`) and the output is discarded locally without entering the
    store.
  + *Caching*: FODs are cached by their output hash, not their derivation
    hash. Two FODs with different `src` attributes but the same `outputHash`
    share the same cached output.
]

= Namespace Ordering
<sec-ns-order>

#r("builder.ns.order+2")[
  Overlayfs and the Nix sandbox both use mounts; the per-build store is
  reached via Nix's chroot-store mechanism, not by bind-mounting at
  `/nix/store`. The ordering is:
  + Builder sets up the FUSE mount at `/var/rio/fuse-store` and creates the
    per-build overlayfs (lower: FUSE only; upper: SSD; merged at
    `{build_dir}/nix/store`) --- all in the builder's mount namespace.
  + Builder forks `nix-daemon --stdio --store 'local?root={build_dir}'` in a
    thin child mount namespace (`unshare(CLONE_NEWNS)`). The namespace exists
    *only* so `/proc` can be remounted unmasked for the daemon (containerd
    masks `/proc` paths in non-privileged pods; nix-daemon's
    `mountAndPidNamespacesSupported()` probe needs an unmasked `/proc`, and
    PSA rejects `procMount: Unmasked` with `hostUsers: true` per KEP-4265).
    The child's `/nix/store` is the host's; nothing is bind-mounted there.
  + Nix sandbox does its own `unshare(CLONE_NEWNS)` for the build itself.
  + Inside the sandbox, Nix bind-mounts each input from
    `{build_dir}/nix/store/{hash}` (its `realStoreDir`) to the chroot's
    `/nix/store/{hash}` (`storeDir`).
  + Nix calls `pivot_root` to enter the chroot.
  The builder must NOT drop `CAP_SYS_ADMIN` between overlay setup and Nix
  invocation, as both operations require it.
]

#info(title: [History (I-060)])[
  Earlier versions bind-mounted the overlay at `/nix/store` in the daemon's
  namespace and stacked the host store as `lowerdir[0]` so the daemon could
  find its own libs. When a build's `$out` collided with a daemon-runtime path
  (same nixpkgs → same `libunistring` hash), overlay copy-up shadowed the
  daemon's lib mid-build and the daemon's hook subprocess died with
  `libunistring.so.5: cannot open`. The chroot-store layout makes the daemon's
  runtime and the per-build store disjoint filesystem paths, so the collision
  is structurally impossible.
]

= Security Context

Workers require elevated privileges for FUSE mounts, overlayfs mounts, and the
Nix sandbox (user/mount/PID/network namespaces).

*Required capabilities:* `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`. Do NOT use
`privileged: true` --- it disables @seccomp profiles entirely.

#info(title: [Spike finding (Phase 1a)])[
  `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT` without `privileged: true` is not
  sufficient for `/dev/fuse` access because the container's device cgroup does
  not include the FUSE character device (major 10, minor 229) by default.
  Production deployments inject `/dev/fuse` via containerd `base_runtime_spec`
  (OCI `linux.devices` + `linux.resources.devices` ---
  `nix/base-runtime-spec.nix`), which adds the node to the container's `/dev`
  and the device cgroup allowlist, enabling the non-privileged security
  context described above. See @sec-rationale-device-inject.
]

*Seccomp profile:* Builder pods set `seccompProfile: Localhost` pointing at
`seccomp-rio-builder.json` when `privileged != true`. The profile is a
default-deny allowlist derived from moby `default.json` v27.5.1 (see
#rref("builder.seccomp.localhost-profile")), permitting the namespace/mount
syscalls the FUSE mount + overlayfs + nix-daemon sandbox need --- plus the
read-side trace syscalls (`ptrace`, `process_vm_readv`) that
sanitizer/debugger-based check phases require, Yama-confined to descendants ---
while blocking `bpf`, `setns`, `process_vm_writev`, `kexec_load`,
`open_by_handle_at`, `userfaultfd`. When the profile is unset (or
`privileged=true`), pods fall back to `RuntimeDefault`.

*Recommended cluster configuration:*
- Dedicated node pool with taint `rio.build/builder=true:NoSchedule` to
  isolate builder pods from other workloads.
- `automountServiceAccountToken: false` --- builders communicate with the
  scheduler via gRPC, not the Kubernetes API.
- @networkpolicy restricting egress to rio-scheduler and rio-store only (gRPC
  ports). No access to the Kubernetes API server or cloud metadata service
  (`fd00:ec2::254` / `169.254.169.254`). See #rref("builder.netpol.airgap") in
  ADR-019.
- @imdsv2 with hop limit = 1 on builder nodes (defense-in-depth against
  metadata access from privileged pods).

= Device Access

Workers require access to `/dev/fuse` for the FUSE filesystem. Mount it as a
`hostPath` volume:

```yaml
volumes:
  - name: dev-fuse
    hostPath:
      path: /dev/fuse
      type: CharDevice
containers:
  - name: builder
    volumeMounts:
      - name: dev-fuse
        mountPath: /dev/fuse
```

Without `/dev/fuse`, the FUSE daemon cannot create the store mount and the
builder will fail to start.

= FUSE Passthrough Mode (Linux 6.9+)

#r("builder.fuse.passthrough")[
  Linux 6.9 introduced FUSE passthrough mode (`FUSE_PASSTHROUGH`), which
  allows the FUSE daemon to hand off file descriptors to backing files. For
  cached store paths on local SSD, passthrough mode bypasses the
  kernel-userspace context switch entirely, providing near-native I/O
  performance.
]

This is relevant to the FUSE daemon because the warm-cache path (store paths
already fetched to local SSD) is the most performance-critical. With
passthrough:
- Reads from cached paths go directly to the SSD-backed file via the kernel,
  no userspace FUSE daemon involvement
- Only cache-miss reads require the full FUSE round-trip to rio-store via gRPC
- The performance concern from @sec-rationale-fuse-perf ("FUSE overhead must
  be < 2x direct reads") may be reduced to near-native for warm builds

*Status:* Validated in Phase 1a spike. `fuser` 0.17 supports passthrough
natively via `KernelConfig::set_max_stack_depth(1)` +
`ReplyOpen::open_backing()` + `opened_passthrough()`.

== Spike Findings

The Phase 1a spike validated passthrough on EKS AL2023 (kernel 6.12). Key
findings:

+ *Passthrough works on ext4/xfs-backed files.* `open_backing()` succeeds and
  the kernel handles `read()` directly without entering userspace.

+ *Passthrough does NOT work on overlay-backed files.* The kernel's
  `fuse_passthrough_open` checks the backing file's filesystem stack depth and
  returns `EPERM` if it's on a stacked filesystem (overlayfs, another FUSE
  mount). This means the backing files must be on a real filesystem (local
  SSD, emptyDir), not on a container's overlay rootfs. This is consistent with
  the production design where the FUSE daemon serves from local SSD cache.

+ *Passthrough does not help for open-heavy workloads.* The spike benchmark
  (open+read+close per file, 74k files) showed identical latency with and
  without passthrough. The bottleneck is `lookup()` and `open()` calls which
  still traverse userspace even with passthrough enabled. Passthrough only
  bypasses `read()`.

+ *Passthrough benefits sustained reads on open file handles.* For
  production, this means the cache should keep file handles open across
  multiple reads from the same store path. A build that reads a large `.so` or
  header file repeatedly will benefit; a build that opens thousands of small
  files once will not.

*Implications for the FUSE daemon design:*
- The FUSE cache (`fuse/cache.rs`) should maintain open file handles for
  cached paths, not just the path data. When a file is opened via `open()`,
  register a passthrough backing fd and keep it alive until eviction.
- `max_stack_depth` must be set to 1 in `init()`. Setting it to 2 allows the
  FUSE mount itself to be used as the lower layer of an overlayfs (which is
  the production layout: FUSE lower + SSD upper).
- The `fuser` crate (0.17+) supports passthrough without patches or forks.

#info(title: [Constraint])[
  `max_stack_depth` has a kernel maximum of 2. With `max_stack_depth=1`, the
  FUSE mount can be stacked under one overlayfs layer. With
  `max_stack_depth=2`, the backing files themselves can be on a stacked
  filesystem. For production, `max_stack_depth=1` is correct: backing files
  are on ext4 (depth 0), FUSE adds depth 1, and overlayfs adds the final
  layer.
]

= Nix Version Pinning

#r("builder.nix.pinned-schema")[
  The synthetic SQLite store database generated per-build in the overlay upper
  layer is coupled to Nix's internal DB schema (version 10). This schema
  (`ValidPaths`, `Refs`, `DerivationOutputs` tables) is an internal API with
  no stability guarantees from the Nix project.
]

*Requirements:*
- Pin the Nix version in the builder container image (e.g., `nix_2_24` from
  nixpkgs)
- CI must test synthetic DB generation against the pinned Nix version (Phase
  3a validation checklist)
- Nix version upgrades should be treated as potentially breaking changes: test
  the synthetic DB against the new version before rolling out
- Document the pinned Nix version and the expected schema version in the
  builder configuration

= Future: Privilege Splitting

The current design holds `CAP_SYS_ADMIN` throughout build execution because
both overlayfs setup and the Nix sandbox require it. A sandbox escape gives
the attacker full `CAP_SYS_ADMIN` capabilities.

A future improvement would split the builder into two processes:

+ *Privileged setup process* (`rio-builder-setup`): Runs with
  `CAP_SYS_ADMIN`. Creates the overlayfs mount, generates the synthetic SQLite
  DB, and prepares the build environment. After setup, it forks the
  unprivileged supervisor and exits (or drops capabilities).

+ *Unprivileged build supervisor* (`rio-builder-supervisor`): Runs WITHOUT
  `CAP_SYS_ADMIN`. Invokes `nix-daemon --stdio` within the pre-configured
  overlay (which is already mounted). Streams logs, monitors the build
  process, and uploads outputs via gRPC. The Nix sandbox itself uses
  `CLONE_NEWUSER` which does not require `CAP_SYS_ADMIN` when user namespaces
  are enabled (requires `sysctl kernel.unprivileged_userns_clone=1`).

*Open question:* Can `nix-daemon --stdio` operate without `CAP_SYS_ADMIN` if
the mount namespace is already set up? The answer depends on whether the Nix
sandbox uses `mount()` directly (requires capability) or only
`unshare(CLONE_NEWNS)` + `pivot_root()` (may work with user namespaces). This
requires empirical testing against the target Nix version.

*Status:* Deferred. Will be investigated when the basic builder architecture
is stable (post Phase 3).

= Build Status Reporting

#r("builder.status.nix-to-proto")[
  The mapping from `rio_nix::BuildStatus` to `proto::BuildResultStatus` MUST
  be exhaustive (no `_` arm). Adding a Nix variant is a compile error until
  the mapping is extended.
]

#r("builder.timeout.no-reassign")[
  Build timeout is a build outcome, not an executor fault. It MUST surface as
  `BuildResultStatus::TimedOut` (permanent, not reassignable), not as
  `InfrastructureFailure`.
]

= Build Cancellation

#r("builder.cancel.cgroup-kill+2")[
  When the build must be aborted, the builder's `try_cancel_build` writes `1`
  to the target build's `cgroup.kill`
  (SIGKILLs the entire cgroup tree). The build's executor task detects the
  daemon exit, releases the semaphore permit, tears down the overlay, and
  sends `CompletionReport{status: Cancelled}`. The abort trigger is SIGTERM
  itself (AD5: any pod termination --- cancel, preemption, node drain ---
  aborts the in-flight build through this same path, the resulting
  `Cancelled` completion gets one bounded best-effort `ReportOutcome`
  attempt, and the process exits inside the pull-mode 45 s grace --- there is
  no finish-if-you-can mode).
]

#r("builder.cancel.pre-cgroup-deferred+2")[
  A cancel (the SIGTERM-abort) that arrives before the per-build cgroup
  exists (`cgroup.kill` → ENOENT) MUST leave the cancelled flag set. The executor MUST check the flag
  before the prefetch/register phase and abort with `Cancelled` status without
  spawning nix-daemon. The pre-cgroup window is overlay setup → resolve →
  prepare_sandbox → register_inputs + prefetch_manifests --- sub-second since
  the I-043 redesign deleted the warm phase (which I-165 showed could stall
  for tens of minutes). The misclassification risk (a later unrelated `Err`
  reported as `Cancelled`) is the lesser evil vs. an unkillable builder
  burning `activeDeadlineSeconds` of compute.
]

= Just-in-time Input Fetch

#r("builder.fuse.jit-register")[
  The executor MUST register the build's input closure (basename → nar_size,
  the projection of `compute_input_closure`'s result) on the FUSE cache via
  `register_inputs()` after `compute_input_closure` and before daemon spawn.
  This arms the FUSE `lookup()` allowlist and is the ONLY signal `lookup()`
  uses to decide whether a top-level name may trigger a store fetch.
]

#r("builder.fuse.jit-lookup")[
  Top-level FUSE `lookup` for a name in the registered input set MUST block on
  `ensure_cached` with a per-path timeout of at least `nar_size /
  JIT_MIN_THROUGHPUT_BPS` (size-scaled, floored at `fuse_fetch_timeout`;
  I-178: a flat 60 s aborted a 1.9 GB input mid-fetch). On any fetch failure
  it MUST return `EIO` (NEVER `ENOENT`) --- overlayfs `ovl_lookup` propagates
  a lower's non-ENOENT error to the caller without caching a negative dentry;
  an `ENOENT` would be negative-cached and the daemon's retry would never
  re-ask FUSE → `MiscFailure` → `PermanentFailure` poison (the I-043 failure
  mode). For a name NOT in the registered set (and not already on local disk),
  `lookup` MUST return `ENOENT` immediately without contacting the store ---
  daemon `.lock`/`.chroot`/`.check` probes, output-path pre-checks, and
  `.links` all land here. This is a pure allowlist: builds cannot read store
  paths outside their declared input closure (hermeticity).
]

= Completion Delivery

#r("builder.completion.exactly-once-or-death+2")[
  Every assignment the builder accepts MUST produce exactly one
  `CompletionReport` delivered to the scheduler (whichever replica is leader
  when delivery succeeds), or the process MUST die without having delivered
  one --- never neither, and never two reports with different outcomes for
  the same build. Every terminal path (success, failure, cancellation,
  panic) funnels through the single `send_completion` chokepoint into the
  build-task sink; the pull loop forwards the queued report through the
  idempotent `ReportOutcome` unary, retried until the scheduler acknowledges
  it (#rref("builder.pull.retry-loop")), and exit code 0 is reserved for the
  acknowledged/charge-free outcomes (#rref("builder.pull.exit-codes")) so a
  pod can never exit "successfully" with the report still owed. The builder
  MUST NOT fabricate a report for an assignment it did not accept. A pod that
  dies before delivery is the accepted residual: its death is observed and
  classified by the controller/scheduler side (the pod-terminal
  `ReportAttemptOutcome` second installment or the establishment sweep),
  never by a second worker-side report; idempotency on re-delivery is the
  scheduler's exec_id-keyed report fill
  (#rref("sched.executor.report-idempotent")).
]

This is the delivery obligation the pull loop's report phase and the exit-code
discipline jointly implement, and the assumption the scheduler side makes
about its peer: a started build's report is eventually delivered to some
leader unless the pod dies first, and is never delivered twice as two
different outcomes.

*Retired (1d builder collapse — the stream relay):*
`builder.completion.pending-armed-early`, `builder.relay.graceful-exit-close`
and `builder.relay.reconnect` described the stream-era delivery machinery
(the `completion_pending` drain gate, the half-close flush before dropping
the bidi stream, and the parked-relay/permanent-sink reconnect choreography).
That machinery is deleted with the `BuildExecution` stream client: there is
no relay, no drain gate, and no stream to half-close. The delivery obligation
they served is carried by the re-stated
#rref("builder.completion.exactly-once-or-death") above — the report-retry
loop (#rref("builder.pull.retry-loop")), the exit-code discipline
(#rref("builder.pull.exit-codes")), and the scheduler-side classification of
a pod that dies before its report (second installment / establishment). The
build-task sink itself remains (build tasks never lose a send because the
scheduler is unreachable; delivery retries live in the report loop).

#r("builder.result.input-enoent-is-infra+2")[
  When the nix-daemon returns `MiscFailure` with an error message indicating a
  missing input path (`getting attributes of path '<p>'`) and `<p>`'s basename
  matches an entry in the build's computed input closure, the builder MUST
  report `BuildResultStatus::InfrastructureFailure` (not `PermanentFailure`).
  The input was verified present in rio-store by `compute_input_closure`; its
  absence at sandbox-setup time is a worker-local materialization failure (JIT
  fetch EIO, overlay negative-dentry race), not a build defect. I-178b: the
  matcher MUST strip ANSI SGR escapes before parsing (the daemon colors the
  path) and MUST match by basename only --- the daemon reports the overlay
  path (`/var/rio/overlays/<build_id>/nix/store/<hash>-<name>`), not the bare
  store path the closure holds. The errno suffix is NOT load-bearing: both `No
  such file or directory` and `Input/output error` (I-179) are
  materialization failures.
]

= Pull Client

The builder does not register, heartbeat, or open a dispatch stream: it asks
for its work with `ExecutorService.PullAssignment` and reports the outcome
with `ExecutorService.ReportOutcome`, both retried until acked. This is the
only delivery path — the stream session client was deleted with the
executor-lifecycle 1d collapse, and the pool-level `dispatchMode` selector
plus the `RIO_DISPATCH_MODE` pod discriminator were retired with it (the
config loader still tolerates a stray env of that name, but nothing renders
one).

#r("builder.pull.idle-undroppable")[
  The idle accumulator MUST be advanced only by scheduler answers — the
  credit for each pair of consecutive `NotYetReady` answers is the elapsed
  gap capped at twice the earlier answer's suggested re-pull delay — and the
  armed answer pair MUST be undroppable: no transport error, empty outcome,
  or other non-answer event may discard it. The cap is the sole outage
  bound; an API that can discard an armed pair is forbidden because it
  makes legitimately-idle time uncountable under interleaved errors.
]

The two polarities this balances: counting raw wall-clock matured whole
cohorts through scheduler outages (the over-count, closed by the cap —
a 300s failover between two answers credits at most twice the previous
suggestion), while the original over-correction — discarding the armed pair
on every error — made `idle_timeout` unreachable against a flaky-but-
answering scheduler (the starvation, bug_296: `idle_for` pinned at zero
forever). Deleting the discard operation closes the starvation direction
structurally: the type has no operation an error path could call, so the
property holds for every current and future caller rather than per call
site.

#r("builder.pull.retry-loop+2")[
  In pull mode the builder MUST retry a retryably-unservable `PullAssignment`
  (not-leader, recovery-gated, transport error/timeout) with jittered
  exponential backoff for as long as the pod lives, MUST re-pull after the
  suggested `retry_after` on `NotYetReady`, and MUST retry `ReportOutcome`
  until it is acknowledged or the pod's remaining lifetime is exhausted; the
  pod never exits merely because the pull cannot land ---
  `activeDeadlineSeconds` bounds the wait. Permanent rejections are the
  exception: an identity/auth rejection (`Unauthenticated`,
  `PermissionDenied`), `Unimplemented`, or `InvalidArgument` answer MUST
  terminate the pull or report loop promptly with a nonzero exit and a
  warning-or-higher log line, never a silent retry that holds the node for
  the full deadline.
]
Scheduler unavailability shorter than the Job deadline shows up as pull
retries (pods parked, building the moment the leader returns), not as Failed
Jobs; a mis-bound or expired executor token, an HMAC rotation skew, or a
pull pool pointed at a pre-pull scheduler shows up as a promptly Failed Job
with a clear log line instead of a node silently held until
`activeDeadlineSeconds`.

#r("builder.pull.exit-codes+1")[
  In pull mode exit code 0 is reserved for exactly four cases: a `Gone`
  response, a `ReportOutcome` acknowledged by the scheduler, the
  charge-free idle exit after receiving only `NotYetReady` for the
  `idle_timeout` bound, and a shutdown with provably nothing minted —
  every pull this process sent was answered, or none was sent (the
  wire-effect latch is clear). A shutdown with a maybe-minted pull (a
  pull that reached the wire and was never answered) MUST resolve before
  exit 0: exactly one bounded confirm pull inside the termination grace —
  `NotYetReady`/`Gone` confirm nothing is held; a delivered `Assignment`
  is closed with a synthesized `Cancelled` report that must be
  acknowledged. Every other termination, including a maybe-minted
  shutdown left unresolved, MUST exit nonzero so the Job goes Failed and
  classification arrives via the pod-terminal path.
]

*Owner counter-signature (Q6, bughunt-2 §5-S packet): SIGNED 2026-06-04 —
confirm-then-#[{0|nonzero}]: shutdown-with-provably-nothing-minted is the
named fourth exit-0 case; one bounded follow-up pull inside the 45s grace
is blessed; maybe-minted-unresolved exits nonzero (Failed Job).*

= Shutdown

*Retired (1d builder collapse — the stream shutdown machinery):*
`builder.idle-exit`, `builder.shutdown.idle-no-reregister` and
`builder.ephemeral.exit-aborts-heartbeat` described the stream-era exit
paths: the reconnect loop's I-116 idle-timeout arm, the SIGTERM
drain-without-re-registering fast path, and the heartbeat-abort-before-FUSE
teardown ordering. The reconnect loop, drain gate, registration and heartbeat
no longer exist. Successors of the load-bearing content: the I-116
"surplus pod exits cleanly instead of idling to `activeDeadlineSeconds`"
property is the charge-free `NotYetReady` idle exit and the `Gone` outcome
(#rref("builder.pull.exit-codes")); SIGTERM handling is the abort semantics
of #rref("builder.shutdown.sigint") below; and teardown ordering is
#rref("builder.shutdown.fuse-abort") (there is no heartbeat task left to
abort first).

#r("builder.shutdown.sigint+5")[
  The builder handles both SIGTERM and SIGINT by leaving the pull loop,
  running teardown (FUSE abort), and returning from `main()`. Local
  development (`cargo run` → Ctrl+C) and Kubernetes pod deletion (kubelet →
  SIGTERM) share the same exit path. Returning from `main()` lets
  `fuse_session`'s `Mount` drop (`fusermount -u`) and atexit handlers fire
  (LLVM profraw flush). The signal is an abort, not a drain (AD5): an
  in-flight build is cgroup-killed (#rref("builder.cancel.cgroup-kill")), the
  `Cancelled` completion gets exactly one bounded best-effort `ReportOutcome`
  attempt, the pull/report retry loops stop waiting, and the process exits
  within the pull-mode grace. A signal while still waiting for work exits 0
  without building only when the wire-effect latch is clear (nothing
  started, nothing owed); a maybe-minted pull is first resolved with the
  single bounded confirm pull of #rref("builder.pull.exit-codes") — both
  bounded RPCs fit the grace by construction (2 × the final-attempt bound
  ≤ the pull-mode termination grace, compile-asserted).
]

*Owner counter-signature (Q6, bughunt-2 §5-S packet): SIGNED 2026-06-04 —
the sigint abort law amended in lockstep with the exit-code law's fourth
case and confirm path.*

#r("builder.shutdown.fuse-abort")[
  On the shutdown path, the builder MUST abort the FUSE connection (write `1`
  to `/sys/fs/fuse/connections/<dev_minor>/abort`) BEFORE dropping the
  `BackgroundSession`. The builder serves the FUSE mount (fuser threads) while
  nix-daemon consumes it (overlay→FUSE `lstat` during JIT input fetch); if the
  runtime tears down while the daemon's threads are parked in the kernel's
  FUSE request queue, those threads enter uninterruptible D-state waiting for
  a userspace reply that will never come (I-165: main thread zombie, 4×
  D-state stat threads). Aborting the connection makes the kernel return
  `ECONNABORTED`/`ENOTCONN` to all pending requests, unblocking the D-state
  threads so the process can fully exit. fuser's `Mount::Drop` (via
  `AutoUnmount` socket close → `fusermount -u` → lazy `MNT_DETACH`) does NOT
  abort pending requests, and dropping `BackgroundSession` does NOT close
  `/dev/fuse` (the fd is `Arc`-shared with the detached bg thread). The device
  minor is captured at mount time --- statting the mountpoint at abort time
  would itself queue a FUSE `getattr(ROOT)` behind the stuck requests. The
  builder mounts `fusectl` itself if `/sys/fs/fuse/connections` is unpopulated
  (I-165b: Bottlerocket + `hostUsers:false` containers don't inherit the
  host's systemd-mounted fusectl, so the abort path was silently `None` and
  the deadlock recurred); the mount is best-effort under the same
  `CAP_SYS_ADMIN` the FUSE mount already requires.
]

= Key Files

#figure(
  table(
    columns: 2,
    align: (left, left),
    table.header([File], [Responsibility]),
    src("rio-builder/src/config.rs"),
    [`Config` + `CliArgs` (two-struct config split) and `detect_system()`],

    src("rio-builder/src/executor/"),
    [Build execution (spawns nix-daemon in mount namespace, drives protocol)],

    src("rio-builder/src/overlay.rs"), [overlayfs setup and teardown],
    src("rio-builder/src/fuse/mod.rs"),
    [FUSE daemon lifecycle, mount management, `NixStoreFs` struct],

    src("rio-builder/src/fuse/ops.rs"),
    [`Filesystem` trait implementation (all kernel callbacks: `lookup`,
      `getattr`, `open`, `read`, `readlink`, `readdir`, `forget`, `init`,
      `destroy`)],

    src("rio-builder/src/fuse/inode.rs"),
    [Bidirectional inode↔path map with kernel `nlookup` refcounting],

    src("rio-builder/src/fuse/lookup.rs"),
    [Attribute helpers: `stat_to_attr`, `ATTR_TTL`, `BLOCK_SIZE`],

    src("rio-builder/src/fuse/read.rs"),
    [File-range read helper (`pread`) + `io::Error` → `Errno` translation],

    src("rio-builder/src/fuse/cache.rs"),
    [LRU cache management (SQLite-indexed, SSD-backed)],

    src("rio-builder/src/fuse/fetch/"),
    [`ensure_cached`: NAR fetch + extract from rio-store (prefetch +
      on-demand)],

    src("rio-builder/src/synth_db.rs"),
    [Synthetic SQLite DB generation for nix-daemon],

    src("rio-builder/src/upload.rs"),
    [Chunk and upload build outputs (streaming NAR → rio-store PutPath)],

    src("rio-builder/src/log_stream.rs"),
    [Build log batching (64-line/100ms) for the rio-store log uploader],

    src("rio-builder/src/cgroup.rs"),
    [cgroup v2 per-build subtree: memory.peak + polled cpu.stat for tracking;
      memory.max + cpu.max for enforcement. Fixes the Phase 2c VmHWM bug
      (daemon-PID measured \~10MB; cgroup is tree-wide).],

    src("rio-builder/src/health.rs"),
    [axum `/healthz` + `/readyz` (builder has no gRPC server; K8s probes hit
      HTTP). Readiness tracks "assignment pulled, build in progress".],

    src("rio-builder/src/runtime/"),
    [Pull loop (`pull.rs`), build-spawn context, completion construction and
      cold-start wiring. Extracted glue between `main.rs` and the
      subsystems.],
  ),
)

= Failure modes

#figure(
  table(
    columns: (auto, 1fr),
    align: (left, left),
    [*Immediate effect*], [Running build on that pod is orphaned],
    [*Cascading effect*],
    [The pod's death reaches the scheduler as the controller's pod-terminal
      `ReportAttemptOutcome` (or, if the controller never observes it, the
      establishment sweep classifies the silent attempt) --- the derivation
      goes back to Ready through the retry fold and re-queues],

    [*Recovery*],
    [Controller spawns a fresh Job for the re-queued derivation. New pod
      starts with cold FUSE cache.],
  ),
  caption: [Builder pod failure (from the component failure matrix).],
)

When rio-store is degraded (slow but not down), builder FUSE cache misses
queue up: read operations block, build sandboxes stall, and after 5
consecutive `ensure_cached` failures the FUSE circuit breaker
(#rref("builder.fuse.circuit-breaker+3")) opens and `check()` returns `EIO`
immediately (fail-fast). The affected build then fails with an infra-classed
outcome and requeues through the scheduler's retry fold; there is no
scheduler-side capacity state to exclude the pod from (it is one-shot and
exits with its build).

= Rationale

== Builder store model // supersedes ADR-005
<sec-rationale-store-model>

Nix builds require a populated `/nix/store` with all build inputs present,
plus a valid SQLite store database. In a distributed system, workers must
access potentially hundreds of gigabytes of store paths without
pre-materializing everything; the store model must support concurrent builds
with isolation, be Kubernetes-native, and avoid shared mutable state.

Each worker runs a custom FUSE filesystem (the `fuse` module in `rio-builder`)
mounted at a configurable path (default `/var/rio/fuse-store`). The FUSE
daemon lazily fetches store path content from rio-store via gRPC on demand,
caches fetched content on local SSD with LRU eviction, and exploits store path
immutability so cached data never needs invalidation. Each build gets a
per-build overlayfs (#rref("builder.overlay.stacked-lower+2")): the lower
layer is the FUSE mount only; the upper layer is `{overlay_base_dir}/{build_id}/upper/nix/store/` on a local-disk emptyDir volume (must be a real
filesystem --- overlayfs-as-upperdir cannot create `trusted.*` xattrs and
fails with `EINVAL`); the merged dir is mounted at `{build_dir}/nix/store` as
nix-daemon's `realStoreDir` for the chroot store. A synthetic SQLite store DB
at `{build_dir}/nix/var/nix/db` is generated per-build from rio-store's
PostgreSQL metadata, containing only the paths relevant to that build. On
completion, built outputs are scanned from the upper layer and uploaded to
rio-store.

*Alternatives considered.* A shared NFS/EFS ReadWriteMany PersistentVolume
would mean shared mutable state across workers, poor NFS performance under
concurrent builds, an unsupported overlayfs-over-NFS kernel configuration, and
SQLite lock contention. Bind-mount with pre-materialization (copy all input
paths to local storage before each build) is simple but slow --- large
closures (e.g., GHC) can be tens of gigabytes --- and wastes bandwidth
re-copying paths already present from previous builds. Container image
layering (store paths as OCI layers) is creative but OCI layer limits (\~128),
layer size overhead, and image build latency make it impractical for builds
with hundreds of store paths. Running a real nix-daemon per worker with its
own store (copying paths in via `nix copy`) works but duplicates store
management logic, requires daemon lifecycle management, and the SQLite DB
becomes a bottleneck under concurrent builds.

*Consequences.* No shared mutable state --- workers are independently
scalable. Lazy fetching means builds only transfer the paths they actually
access, not the full closure. Local SSD cache with LRU eviction gives warm
builds near-local performance. On the negative side: FUSE adds a layer of
complexity and a potential performance bottleneck for I/O-heavy builds (see
@sec-rationale-fuse-perf); it requires `CAP_SYS_ADMIN` for overlayfs and FUSE,
necessitating elevated pod security; and synthetic SQLite DB generation must
precisely match Nix's expected schema, requiring careful tracking of upstream
changes.

*The hard part: executor store lifecycle.* The FUSE + overlay approach
introduces ordering complexity --- upper layer cleanup must be deterministic
(unique per-build directory, discarded with the pod), and the namespace
ordering (FUSE mount → overlayfs → nix sandbox) must be correct
(#rref("builder.ns.order+2")). The decided approach is that each executor runs
the FUSE layer that lazily fetches store paths from rio-store; each build gets
a per-build overlayfs with the FUSE mount as lower and a per-build synthetic
SQLite database in the upper layer. This avoids shared mutable state,
eliminates shared PV infrastructure, and provides local-disk performance via
SSD caching.

== Pull-based delivery model // supersedes ADR-011 and the stream session
<sec-rationale-streaming>

The communication model between scheduler and workers determines latency,
failure handling, and operational characteristics. The original design (ADR-011
and its successors) used a since-removed bidirectional `BuildExecution` stream per worker
plus a periodic `Heartbeat` unary; the executor-lifecycle campaign replaced it,
and the stream session client was deleted at the 1d collapse.

As built, a builder pod is born knowing its derivation (`RIO_INTENT_ID` + the
HMAC executor token injected at Job spawn) and speaks exactly two idempotent
unaries: `PullAssignment` (three outcomes — the dispatch payload, `Gone`, or
`NotYetReady{retry_after}`) and `ReportOutcome`, both retried until
acknowledged. Build logs do not transit the scheduler at all — they stream to
rio-store's `LogService.AppendLog` (the log data plane), and the dashboard /
`nix build -L` tail them from rio-store. Cancellation and preemption are pod
terminations: the controller deletes the Job and the SIGTERM-abort path
(#rref("builder.shutdown.sigint")) cgroup-kills the in-flight build and makes
one bounded report attempt. Liveness is the Job's lifecycle — there is no
registration or heartbeat, and a pod that dies silently is classified by the
controller's pod-terminal report or the scheduler's establishment sweep.

*Consequences.* Work delivery is pod-initiated, so there is no per-executor
send window or scheduler-side backpressure state — an unservable pull surfaces
as a retried unary on the pod side. The polling latency the original ADR
worried about is bounded by the `NotYetReady` retry hint (seconds) and applies
only to forecast-spawned pods whose dependencies are not yet Ready; a pod
spawned for Ready work pulls exactly once. The trade-off accepted with AD4 is
that the scheduler learns of pod death only through the controller or the
establishment window rather than a dropped TCP stream; the deadline-bounded
establishment sweep is the backstop that keeps a silent loss from stranding an
attempt forever.

== `/dev/fuse` device injection and `cgroup_writable` // supersedes ADR-012 §Phase-1a
<sec-rationale-device-inject>

The Phase 1a spike discovered three constraints around `hostUsers: false`:

*`hostUsers: false` + hostPath `/dev/fuse` is incompatible.* The kernel
rejects idmap mounts on device nodes, causing the container to fail at startup
with `failed to set MOUNT_ATTR_IDMAP on /dev/fuse: invalid argument`. User
namespace isolation requires injecting `/dev/fuse` without the hostPath volume
mechanism --- containerd `base_runtime_spec` (OCI `linux.devices`) does this:
runc `mknod`s the node inside the container's `/dev` with container-namespace
uid/gid.

*`CAP_SYS_ADMIN` alone is insufficient for `/dev/fuse` access.* The
container's device cgroup does not include the FUSE character device (major
10, minor 229) by default. Without device injection, `privileged: true` is the
only way to access `/dev/fuse`. containerd `base_runtime_spec` resolves both
constraints: it adds the device to the cgroup allowlist (OCI
`linux.resources.devices`) AND avoids the hostPath volume, enabling both
`hostUsers: false` and the non-privileged security context.

*`hostUsers: false` requires the runtime to chown the pod cgroup.* The builder
creates sub-cgroups under its cgroup-namespace root for per-build resource
tracking. Per the OCI runtime-spec, runc chowns the container's cgroup to the
userns root UID only when the OCI config mounts `/sys/fs/cgroup` read-write.
containerd passes `ro` for unprivileged containers unless `cgroup_writable =
true` is set on the runc runtime section
(#link("https://github.com/containerd/containerd/pull/11131")[containerd\#11131],
v2.1+). Without the chown, `/sys/fs/cgroup/` appears as `nobody:nobody` inside
the user namespace and the builder's `mkdir` fails `EACCES` --- the rw remount
in `rio-builder/src/cgroup.rs` (#rref("builder.cgroup.ns-root-remount")) fixes
the mount flag but cannot fix inode ownership (`CAP_DAC_OVERRIDE` does not
apply to unmapped UIDs). The NixOS node AMI (ADR-021) sets `cgroup_writable =
true` directly via `virtualisation.containerd.settings`, so EKS deployments
default to `hostUsers: false`.

The controller-generated pod spec (`rio-controller/src/reconcilers/pool/pod.rs`) matches: `/dev/fuse` via containerd `base_runtime_spec`
(`nix/base-runtime-spec.nix` declares `/dev/{fuse,kvm}` in OCI `linux.devices`
+ `linux.resources.devices`; containerd's runc runtime is pointed at it via
`nix/nixos-node/containerd-config.nix` on the NixOS AMI per ADR-021 §7, or
`services.k3s.containerdConfigTemplate` on the k3s VM fixture). Every pod gets
both unconditionally; no hostPath volume; `hostUsers: false` works.
`CAP_SYS_ADMIN` is scoped to the user namespace and a container escape cannot
use it on the host. The Helm chart default is `builderPoolDefaults.privileged:
false`; no device plugin runs, no extended resource is requested. The
@pool @crd exposes an optional `privileged: bool` --- when `true` the
container runs fully privileged with the hostPath `/dev/fuse` fallback, an
escape hatch for clusters whose default seccomp profiles block `mount(2)` even
with `SYS_ADMIN`, or whose containerd lacks idmap-mount support. Production
deployments on EKS/GKE should not need this.

== Seccomp profile distribution // supersedes ADR-012 §Seccomp
<sec-rationale-seccomp-dist>

The custom Localhost profile JSON lives at
`nix/nixos-node/seccomp/rio-{builder,fetcher}.json`; the chart's
`localhostProfile` default `operator/rio-builder.json` is the path under
`/var/lib/kubelet/seccomp/` where the profile must land on every node. All
supported targets are NixOS and deliver the profile the same way: baked into
the node image as store paths and copied into
`/var/lib/kubelet/seccomp/operator/` by `systemd-tmpfiles` BEFORE kubelet
starts (`nix/nixos-node/hardening.nix` on EKS;
`nix/tests/fixtures/k3s-full.nix` for k3s VM tests). By the time any pod
schedules the file is guaranteed present, so `rio-controller` emits
`seccompProfile: Localhost` directly with no wait machinery.

*Why node-baked over an operator.* security-profiles-operator's `spod`
DaemonSet runs concurrently with workload pods; without a `wait-seccomp`
initContainer the kubelet would `CreateContainerError` on the pod that races
spod onto a fresh @karpenter node. Under ephemeral builders (one Job per
derivation, thousands per hour), that init's 5--15s poll dominated cold-start
latency, and SPO's controller OOMKilled under sustained node-churn (I-154).
Node-baked eliminates both: zero per-pod overhead, no in-cluster operator
competing for memory.

*Profile-update path.* The profile is a store path in the node image; a
profile change is `xtask k8s -p eks up --ami` + `helm upgrade --set
karpenter.amiTag=<new-sha>`. Karpenter Drift detects the resolved-AMI-ID
change and rolls nodes. Cost is paid once per node lifetime instead of once
per pod.

History: I-020 (the original 7-minute init hang) → P0540 (rio-seccomp-installer
DS → SPO `SeccompProfile` CRs) → I-154 (SPO operator OOM under ephemeral
churn) → P0541 (SPO → Bottlerocket bootstrap-container) → ADR-021
(Bottlerocket → NixOS AMI, profiles baked in) → SPO + `seccompPreinstalled`
removed once k3s VM tests adopted the same tmpfiles delivery.

*The hard part: executor pod security.* overlayfs and the Nix sandbox both
require `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`, which conflicts with
PodSecurityStandards on managed Kubernetes clusters. Mitigations: dedicated
node pools with relaxed pod security policies for executor pods, custom
seccomp profiles that allow only the specific syscalls needed (mount,
pivot_root), and NetworkPolicy isolation to restrict executor pod network
access. The Nix sandbox is NOT a security boundary --- it's a purity mechanism
that prevents builds from accessing paths outside their declared inputs. For
multi-tenant deployments, the actual security boundary is the executor pod and
node isolation provided by Kubernetes.

== FUSE local I/O performance
<sec-rationale-fuse-perf>

rio-builder's FUSE daemon runs in userspace via the `fuser` crate. FUSE
context switches between kernel and userspace could become a latency
bottleneck even when the SSD cache is warm --- this is a different risk from
rio-store overload; this is about the local I/O path. Each file `read()` from
the build sandbox crosses kernel → userspace → kernel; for builds that read
thousands of small files (e.g., header-heavy C++ compilations), the overhead
accumulates.

Mitigations: benchmark FUSE read latency (p50, p99) under concurrent load and
compare against direct filesystem reads; the `fuser` crate supports
multi-threaded FUSE dispatch; FUSE passthrough mode (Linux 6.9+,
#rref("builder.fuse.passthrough")) eliminates the `read()` context switch by
handing off file descriptors to backing files for cached paths on local SSD;
file handle caching keeps backing file handles open across reads --- builds
that open many small files once won't benefit from passthrough alone, they
need reduced `open()` overhead via kernel entry/attribute caching (high TTL on
immutable store paths). If FUSE overhead exceeds 2× vs direct reads even with
all mitigations, consider the bind-mount fallback.

*Phase 1a spike results (EKS AL2023, kernel 6.12, c8a.xlarge):* Standard FUSE
overhead was 10--50× vs direct reads (p50, varying concurrency 1--16). FUSE
passthrough showed no improvement for the open-read-close-per-file benchmark
pattern because `lookup()`/`open()` dominate, not `read()`. The overhead is
acceptable for the architecture (the full FUSE → overlayfs → nix-build chain
works), but the production FUSE daemon must optimize the `open()` path via
file handle caching and aggressive attribute/entry TTLs.
