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

- Receive a single build assignment from the scheduler via gRPC (one
  derivation per pod, then exit)
- Run the @fuse store daemon (the `fuse` module) that mounts at
  `/var/rio/fuse-store` (configurable) with lazy on-demand fetching from
  rio-store
- Set up the build's overlay filesystem: FUSE mount as lower layer, local SSD
  as upper layer; the overlay's merged dir is bind-mounted at `/nix/store`
  inside the build's mount namespace
- Execute build: construct a rio-exec sandbox and run the derivation's
  builder in it natively
- Stream build logs back to scheduler via gRPC bidirectional streaming
- After build: upload output @nar to rio-store (chunked), report completion
- Heartbeat / health checking to scheduler
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
    the build sandbox mounts the merged dir writable at `/nix/store`.],
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
      align(
        left,
      )[rio-exec sandbox\ #text(size: 0.8em)[(mount/PID/IPC/UTS/cgroup ns)]],
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
- *Predictive prefetch*: The scheduler sends #glspl("prefetch-hint") via the build
  execution stream before assigning work. The FUSE daemon warms its cache with
  the build's input closure paths before the build starts.

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

#r("builder.platform.i686+2")[
  When a worker executes a derivation whose `system` is a 32-bit platform
  hosted by its 64-bit kernel (`i686-linux` on an `x86_64-*` worker, 32-bit
  ARM on an `aarch64-*` worker), the executor MUST set the 32-bit
  architecture personality (`PER_LINUX32`) on the build process before
  `execve`, so `uname -m` and the syscall ABI observed by the build match
  the declared system.
]
Routing is unchanged: a Pool advertising `systems: [x86_64-linux, i686-linux]`
still receives i686 derivations via the heartbeat. The personality switch
(`personality_for` in the request glue, applied by rio-exec before exec) is
what makes the accepted build behave as i686 --- the daemon-era
`extra-platforms` `nix.conf` mechanism is gone with the daemon, and the
multi-ABI seccomp filter already admits the 32-bit syscall ABI.

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

== Prefetch Warm-Gate

#r("builder.warmgate.handshake")[
  On receipt of a `PrefetchHint` from the scheduler, the builder spawns one
  fire-and-forget fetch task per hinted path (bounded by a semaphore), then a
  joiner task that awaits ALL of them and sends
  `PrefetchComplete{paths_fetched, paths_cached}` on the BuildExecution
  stream. The scheduler gates the first assignment on receipt of this ACK
  (#rref("sched.assign.warm-gate")), so the build starts with a warm cache. An
  empty hint sends the ACK immediately. The hint handler MUST NOT block the
  BuildExecution event loop --- per-path tasks queue in tokio's scheduler and
  only enter the blocking pool once a permit is acquired. Per-path outcomes
  are recorded in #(refs.metric)("rio_builder_prefetch_total")`{result}`.
]

#r("builder.warmgate.filter")[
  Each `PrefetchHint` path is classified BEFORE entering the blocking pool:
  (a) JIT allowlist armed AND path NOT a declared input → skip
  (`reason=not_input`; FUSE lookup would ENOENT it anyway); (b) JIT allowlist
  NOT armed (initial warm-gate batch, before any assignment) AND
  `QueryPathInfo.nar_size > 256 MiB` → skip (`reason=size_cap`); (c) declared
  input OR under cap → fetch. The size cap stops the warm-gate from
  speculatively pulling multi-GB sibling outputs the scheduler over-includes
  (I-212: `approx_input_closure` sends ALL outputs of each input drv, e.g., a
  2.9 GB `clang-debug` alongside the `clang-out` the build actually needs).
  Declared inputs that exceed the cap are still fetched on-demand by JIT
  lookup, so the filter never blocks a correct build. Filtered paths increment
  #(refs.metric)("rio_builder_prefetch_filtered_total")`{reason}`.
]

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

#r("builder.heartbeat.rpc-timeout")[
  Each heartbeat RPC MUST be bounded by a timeout strictly less than
  `HEARTBEAT_INTERVAL`. The interval loop is sequential (`tick → await RPC →
  apply`); without a per-RPC bound, one RPC stalled past
  `HEARTBEAT_TIMEOUT_SECS` (scheduler actor mpsc backpressure at
  `send_unchecked`, or asymmetric network delay on a live connection --- h2
  keepalive detects dead transports, not slow application handlers) consumes
  all `MAX_MISSED_HEARTBEATS` budgets and `tick_check_heartbeats` reaps a
  healthy worker (bug_044). On elapse, `apply_heartbeat_response`'s `Err` arm
  sets `ready=false` and the next tick fires on schedule
  (`MissedTickBehavior::Delay`, not `Burst`).
]

#r("builder.heartbeat.store-degraded")[
  `HeartbeatRequest.store_degraded` (proto bool, field 9) reflects
  `CircuitBreaker::is_open()`. Scheduler treats it like `draining`:
  `has_capacity()` returns false, builder is excluded from assignment.
  Wire-compatible: old workers don't send it, scheduler reads default `false`.
  Cleared when the breaker closes or half-opens.
]

- `open`: Open the already-materialized local file (fast path, since `lookup`
  fetched the tree). Falls back to `ensure_cached()` on ENOENT. With
  passthrough enabled, hands the kernel a backing fd via `open_backing()` so
  subsequent `read()` calls bypass userspace. *Prefetch is separate* --- it's
  scheduler-driven via `PrefetchHint` messages on the assignment stream, not
  triggered by `open()`.

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

= Sandbox Configuration

The native executor needs no `nix.conf`: every behavior the daemon-era
configuration carried is either structural (substitution cannot happen ---
inputs come exclusively through the FUSE-backed store view), enforced in code
(the rio-exec sandbox is always on and never falls back to an unsandboxed
build), or expressed as explicit worker configuration:

#r("builder.sandbox.shell")[
  `RIO_SANDBOX_SHELL` MUST point at a statically linked POSIX shell in the
  worker image; the executor bind-mounts it read-only at `/bin/sh` inside
  every build sandbox. nixpkgs builds assume `/bin/sh` exists. An empty value
  disables the mount and is only viable for corpora whose builders never
  invoke `/bin/sh`.
]

- `RIO_CA_BUNDLE` --- host path of the CA bundle exposed read-only at
  `/etc/ssl/certs/ca-certificates.crt` inside network (fixed-output) sandboxes
  (default: the worker image's `cacert` bundle).
- `RIO_EXTRA_SANDBOX_PATHS` --- additional host paths bind-mounted read-only
  into every sandbox, for site-local impurities.
- `RIO_HASHED_MIRRORS` --- content-addressed mirrors consulted by
  `builtin:fetchurl` (#rref("fetcher.mirrors.hashed")), injected by the
  controller from the Pool spec.

#info(title: [Security note])[
  `__noChroot` derivations (which disable the sandbox) are rejected at the
  gateway level before they ever reach a builder. See Derivation Validation in
  the security chapter.
]

#warning(title: [Recursive Nix is not supported])[
  Derivations that invoke Nix internally (`recursive-nix`) fail: there is no
  Nix inside the sandbox to invoke, no daemon socket to talk to, and inputs
  are limited to the declared closure. This remains an explicit non-goal.
]

== Builder Capabilities

Each builder advertises two capability lists in its heartbeat so the scheduler
can route derivations:

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

= Native Build Execution

The builder executes derivations itself: the request glue translates the
parsed derivation and its resolved input closure into a build-system-agnostic
`rio_exec::ExecutionRequest`, the rio-exec sandbox runs it, and the native
result pipeline classifies the exit and processes the outputs. No Nix binary
is present in the worker image and no external process is delegated to.
Derivations whose builder is a `builtin:` program (today `builtin:fetchurl`)
are no exception: the worker implements them natively by re-exec'ing its own
binary in fetch mode inside the same rio-exec sandbox --- network reachable
only because such derivations are fixed-output --- as specified in the
fetcher component (#rref("fetcher.fetchurl.sandboxed")).

#r("builder.exec.sandbox+3")[
  Every build runs inside a rio-exec sandbox constructed from fresh Linux
  namespaces: mount, PID, IPC, UTS, and cgroup for every build, plus a fresh
  (loopback-only) network namespace for every build EXCEPT fixed-output
  derivations --- FODs skip the network namespace so they retain network
  access for their fetch. The input closure is bind-mounted read-only inside a
  writable per-build store view, a private `/proc`, `/dev` and `/etc`
  population, `pivot_root` into the per-build root, a multi-ABI seccomp
  filter (covering the native, 32-bit sibling, and x32 syscall ABIs) that
  denies setting setuid/setgid mode bits with `EPERM` and denies the
  extended-attribute get/set families --- including their kernel ≥ 6.13
  `*xattrat` forms --- with `ENOTSUP`, `PR_SET_NO_NEW_PRIVS`, and a drop to
  the unprivileged build user before `execve`. Sandbox construction failure
  MUST fail the build attempt (never fall back to an unsandboxed build).
]

#r("builder.exec.structured-attrs")[
  A derivation is treated as structured-attrs iff its environment carries the
  `__json` blob (the same detection as Nix's `ParsedDerivation`); the
  `__structuredAttrs` name never appears as an env var in instantiated
  derivations. For structured-attrs builds the executor materializes
  `.attrs.json` and `.attrs.sh` in the build directory and exports only
  `NIX_ATTRS_JSON_FILE`/`NIX_ATTRS_SH_FILE` alongside the base environment.
]

#r("builder.exec.ca-finalize")[
  Floating content-addressed outputs are built at deterministic scratch paths
  and finalized by the result pipeline in topological order: accumulated
  sibling rewrites are applied *before* hashing, the content address is the
  hash of the rewritten content modulo the output's own scratch hash, the
  final store path is computed with the self-reference flag when the output
  references itself, and the output is moved to its realized path with its
  references remapped. Realized paths (not scratch paths) are reported to the
  scheduler and uploaded to the store with their content-address descriptor.
]

#r("builder.retry.infra-transient")[
  The build-spawn loop retries `execute_build` locally when the failure is a
  transient worker-local infrastructure failure --- sandbox setup
  (`SandboxSetup`: mount race, FUSE blip while binding an input). Up to
  `INFRA_RETRY_MAX=3` attempts with backoff `500ms/1s/2s`. After exhaustion
  the error propagates as `InfrastructureFailure` and the scheduler's own
  retry policy takes over. The retry MUST short-circuit if the build's
  cancelled flag is set. `BuildFailed`, glue rejections, network-side errors
  (`Upload`/`Grpc`/`MetadataFetch`), and deterministic setup failures
  (`Overlay`) are NOT retried locally.
]

#r("builder.silence.timeout-kill+3")[
  `maxSilentTime` (seconds, forwarded from client `--option max-silent-time`)
  is enforced by the executor: captured build output resets the silence
  deadline, and when the deadline passes with no output the build is killed
  via the per-build cgroup and reported as `BuildStatus::TimedOut` with an
  error message naming the silence window. All builder output counts as
  activity, including `@nix` side-channel frames (`setPhase` and friends):
  the deadline is reset by the raw pty bytes before any frame is consumed,
  matching CppNix, where any builder stderr output resets the silence clock.
  The enforcement is rio-side and authoritative; nothing else in the build
  path enforces it.
]

Before the build process starts, the worker writes a 3-line `rio:` header
(`exec`, `builder`, `started`) as a direct `BuildLogBatch` at line 0; after the
process exits it writes a 2-line footer (`exec`, `result`) at the final line
offset. Both are sent on the same `BuildExecution` stream as build output, *not*
through the `LogBatcher` (which is created and consumed inside the build
lifecycle). The `LogBatcher` is seeded with the header line count so the
build's real output numbers after the header. The header carries the
`WorkAssignment.exec_id`, the system + `hw_class`, and the assigned resource
triple --- never pod or node identity. The banner is per-execution, not
per-attempt: the infra-transient retry loop
(#rref("builder.retry.infra-transient")) re-invokes the executor up to
`INFRA_RETRY_MAX` more times for one `exec_id`, but the header is sent only on
the first attempt and the footer once after the loop with the most recent
output-producing attempt's outcome (overridden to `cancelled` by the
assignment's cancel flag) --- re-emitting the banner per attempt would
write conflicting `rio: result` lines and break the scheduler ring buffer's
line-number monotonicity. Subsequent attempts seed the `LogBatcher` with the
prior attempt's final line count so output line numbers continue. The normative
requirement and the display-only / no-pod-identity rationale live in
#rref("obs.log.worker-header") in the observability spec.

#r("builder.stderr.forward-set-phase+2")[
  The build-log loop consumes nixpkgs' `@nix {"action":"setPhase", ...}`
  side-channel lines (they never appear in the persisted build log) and
  forwards each phase change as a `BuildPhase{derivation_path, phase}`
  `ExecutorMessage`. Phase is a state edge, not log content --- it is sent
  unbatched, and forwarding it plays no part in silence accounting (the
  max-silent deadline is governed by the builder's raw output, which
  includes the frame itself; see #rref("builder.silence.timeout-kill")).
]

#r("builder.stderr.msg-cap")[
  The build-log loop enforces a hard cap of 10M captured lines per build,
  counted before filtering so phase frames and suppressed lines are covered.
  Exceeding it terminates with `BuildStatus::LogLimitExceeded` --- same
  non-retryable semantics as the byte limit.
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

= Overlay Store Architecture

#r("builder.overlay.per-build")[
  Each active build gets its own overlayfs mount with a separate upper
  directory and work directory; the merged view is what the sandbox mounts
  writable at `/nix/store`, so reads of input paths fall through to the
  FUSE-backed lower layer and outputs copy-up into the per-build upper layer.
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
  `/nix/store` is *not* in the lowerdir: the per-build store view contains
  exactly the build's input closure (lower) plus its outputs (upper). The
  worker's own runtime closure is structurally outside it --- nothing the
  build can reach through `/nix/store` exists unless it is in the declared
  input closure.
]

#r("builder.overlay.userns-exdev")[
  *Userns mount constraint (I-185):* When the builder pod runs with
  `hostUsers: false` (ADR-012), `setup_overlay`'s `mount(2)` happens inside a
  non-init user namespace; the kernel forces `redirect_dir=off` (and refuses
  `redirect_dir=on`) on such mounts. The mount therefore MUST NOT request
  `redirect_dir` explicitly, and nothing in the build pipeline may depend on
  cross-layer directory renames over the merged root: builds write outputs
  through the merged view (copy-up), and the executor never `rename(2)`s a
  directory across it. (The daemon-era `movePath` step that tripped this
  constraint --- and the Nix patch that worked around it --- are gone.)
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
  not in `claims.expected_outputs` (see #rref("common.hmac.claims+1"))
+ Register path metadata (@narinfo, references)
+ Discard upper layer

*Teardown failure handling:* Overlay teardown (`umount2`) can fail if the
mount is stuck busy (open file handles, a leaked build process, FUSE hang). A
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

#r("builder.upload.references-scanned+2")[
  The references registered for each output are the reference sets the
  result pipeline recorded — scanned once during output processing,
  post-`unsafeDiscardReferences`, with floating-CA self/sibling references
  remapped to their final paths — and they are delivered unchanged in
  `PathInfo`; the upload performs no additional reference scan of its own.
  The pipeline's scan finds every candidate hash part embedded anywhere in
  the output (including inside binaries, RPATH strings, symlink targets,
  directory names) against the candidate set *transitive input closure* ∪
  `drv.outputs()`: every path reachable via BFS over store references from
  the derivation's inputs, plus all of this derivation's own outputs (for
  self-references and cross-output references). This matches Nix's
  `computeFSClosure` (`derivation-building-goal.cc:444,450` /
  `derivation-builder.cc:1335-1344`). A build can legitimately embed any
  transitively-reachable path --- e.g. `hello-2.12.2` references `glibc`,
  which is not a direct input but arrives via `closure(stdenv)`. The
  registered reference list is *sorted* (affects the narinfo signature
  fingerprint --- must be deterministic).
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
  released by the store's drop-path cleanup (#rref("store.put.drop-cleanup+2"))
  and the next upload attempt succeeds. `QueryPathInfo` errors during the poll
  are treated as not-found (logged, keep polling). Other `Aborted` reasons (GC
  mark serialization, admin cancel) keep the plain retry without polling.
  I-125b.
]

= Store Metadata

The native executor needs no per-build store database: input paths are
bind-mounted from the closure the scheduler already resolved, reference and
closure metadata flow from rio-store's `QueryPathInfo` responses captured
during input resolution, and output registration happens by uploading to
rio-store --- there is no SQLite anywhere in the build path. (The daemon-era
synthetic store DB, its schema pin, and its risks are gone with the daemon.)

#r("builder.executor.resolve-input-drvs")[
  The executor must merge resolved inputDrv outputs into `BasicDerivation`
  inputSrcs before constructing the derivation. The sandbox only bind-mounts
  inputSrcs; unresolved inputDrv paths would be invisible.
]

#r("builder.executor.kind-gate")[
  Per ADR-019, the executor re-derives `is_fod` from the `.drv` (ground truth,
  not the scheduler-sent flag) and checks it against `config.executor_kind`
  BEFORE overlay setup or sandbox construction. If `is_fod != (executor_kind ==
  Fetcher)`, the build fails with `ExecutorError::WrongKind`.
  Defense-in-depth --- the scheduler's `hard_filter` should never misroute,
  but a bug or stale-generation race must not grant a builder internet access
  even transiently.
]

#r("builder.exec.input-closure-binds")[
  *Critical:* the full transitive input closure (not just the direct inputs)
  MUST be bind-mounted into the sandbox. The closure comes from the
  `QueryPathInfo` reference walk performed during input resolution; if
  references are missing, transitive dependencies (e.g. `glibc` needed by
  `bash`) are invisible inside the sandbox and builds fail with "No such file
  or directory" from the dynamic linker.
]

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

#r("builder.cores.cgroup-clamp+3")[
  The executor MUST clamp `build_cores` (exported to the build as
  `NIX_BUILD_CORES` → `make -jN`) to
  `ceil(quota/period)` from the pod cgroup's `cpu.max`, minimum 1. It MUST NOT
  pass `build_cores=0`: 0 means "use nproc", and cgroup CPU
  quota does not reduce visible cores --- a 0.5-core pod on a 16-core node
  would run `make -j16`, OOM-loop on compiler RSS (I-196). A client-requested
  `build_cores > 0` is capped at the same ceiling. When `cpu.max` reads `max`
  (no quota), use host nproc --- but builder/fetcher pools set `limits.cpu ==
  requests.cpu` (I-197, hard limits, no burst) so production pods always see a
  real quota; the `max` fallback only fires in VM tests / bare-metal dev. The
  executor MUST export the clamped value as `NIX_BUILD_CORES` in the sandbox
  environment --- with the daemon-era per-build `nix.conf` gone, that variable
  is the only channel through which a build learns its core budget, so this
  clamp is the single point of enforcement.
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

The overlay is per-build. Each build gets its own overlayfs mount with
separate upper and work directories. The rio-exec sandbox provides
process-level isolation (mount, PID, IPC, UTS, and cgroup namespaces, plus a
network namespace for non-FOD builds). Even if the sandbox is compromised,
the per-build overlay upper layer ensures rogue writes are isolated and
discarded; the next build runs in a fresh pod and sees none of it.

= Fixed-Output Derivation (FOD) Handling

#info[
  Per ADR-019, FODs route to the rio-fetcher executor, not builders. Builders
  are airgapped (#rref("builder.netpol.airgap")) and reject any FOD assignment
  with `ExecutorError::WrongKind` (#rref("builder.executor.kind-gate")). The
  section below documents the FOD verification logic that the `rio-builder`
  binary runs when invoked as a fetcher (`RIO_EXECUTOR_KIND=fetcher`).
]

#r("builder.fod.verify-hash+2")[
  Fixed-output derivations (FODs) have a known output hash declared in
  `outputHash`. They require special handling:
  + *Detection*: A derivation is a FOD if its `outputHash` attribute is
    non-empty.
  + *Network access*: Unlike regular derivations, FODs run with the sandbox's
    network namespace isolation relaxed (the executor sets
    `Isolation::network` for them and binds the resolver configuration and CA
    bundle). Network egress is governed at the pod level by the fetcher
    NetworkPolicy (#rref("fetcher.netpol.egress-open")).
  + *Output verification*: After the build completes, the executor computes
    the hash of the output (flat or recursive per `outputHashMode`) and
    verifies it matches the declared `outputHash`. The executor is the SOLE
    verifier and is fail-closed: an `outputHashAlgo` it cannot verify is
    rejected, never skipped. A mismatch is reported as
    `BuildResultStatus::OutputRejected` (NOT `BuildFailed`) and the output is
    discarded locally without entering the store.
  + *Caching*: FODs are cached by their output hash, not their derivation
    hash. Two FODs with different `src` attributes but the same `outputHash`
    share the same cached output.
]

= Namespace Ordering
<sec-ns-order>

#r("builder.ns.order+4")[
  Overlayfs and the rio-exec sandbox both use mounts; the per-build store
  is bind-mounted writable at `/nix/store` inside the sandbox (the merged
  overlay view), with the input closure nested read-only inside it. The
  ordering is:
  + Builder sets up the FUSE mount at `/var/rio/fuse-store` and creates the
    per-build overlayfs (lower: FUSE only; upper: SSD; merged at
    `{build_dir}/nix/store`) --- all in the builder's mount namespace.
  + The executor compiles the sandbox plan and materializes the chroot
    skeleton (the plan's directory tree, inline files, and symlinks)
    host-side under the per-build chroot directory --- still in the
    builder's mount namespace, before any sandbox process exists.
  + The executor forks the rio-exec sandbox: the intermediate unshares fresh
    mount/PID/IPC/UTS/cgroup (and, for non-FOD builds, network) namespaces;
    the child applies the planned binds --- the merged store writable at
    `/nix/store`, each input closure path read-only nested inside it,
    `/build` writable, the static sandbox shell at `/bin/sh`, and a fresh
    unmasked `/proc` for the new PID namespace (containerd masks `/proc`
    paths in non-privileged pods, and PSA rejects `procMount: Unmasked`
    with `hostUsers: true` per KEP-4265).
  + The child enters the new root with `pivot_root` (plus a belt-and-braces
    `chroot`), lazily detaches the old root, applies the seccomp filter, and
    drops to the build user before `execve`.
  The builder must NOT drop `CAP_SYS_ADMIN` between overlay setup and the
  rio-exec spawn: the overlay mounts and the child's bind/`pivot_root`
  sequence (performed before the privilege drop) both require it.
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
rio-exec sandbox (mount/PID/IPC/UTS/cgroup namespaces --- plus a network
namespace for non-FOD builds --- entered via `pivot_root`).

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
syscalls the FUSE mount + overlayfs + rio-exec sandbox need --- plus the
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

= Nix at Build Time Only

Nix builds the rio images and runs in CI (the gateway's golden protocol tests
and the differential parity harness drive real Nix daemons), but no deployed
rio component ships or invokes a Nix binary at runtime. There is no schema,
protocol-version, or binary pin to manage on the worker.

= Future: Privilege Splitting

The current design holds `CAP_SYS_ADMIN` throughout build execution because
both overlayfs setup and the rio-exec sandbox require it. A sandbox escape gives
the attacker full `CAP_SYS_ADMIN` capabilities.

A future improvement would split the builder into two processes:

+ *Privileged setup process* (`rio-builder-setup`): Runs with
  `CAP_SYS_ADMIN`. Creates the overlayfs mount and prepares the build
  environment. After setup, it forks the unprivileged supervisor and exits
  (or drops capabilities).

+ *Unprivileged build supervisor* (`rio-builder-supervisor`): Runs WITHOUT
  `CAP_SYS_ADMIN`. Drives the rio-exec sandbox within the pre-configured
  overlay (which is already mounted), streams logs, monitors the build
  process, and uploads outputs via gRPC. rio-exec's sandbox could construct
  its namespaces via `CLONE_NEWUSER`, which does not require `CAP_SYS_ADMIN`
  when unprivileged user namespaces are enabled.

*Open question:* which parts of sandbox construction (overlayfs mount, FUSE
mount, bind mounts inside the new mount namespace) genuinely require the
capability once the mount namespace is pre-created. This requires empirical
testing.

*Status:* Deferred. Will be investigated when the basic builder architecture
is stable (post Phase 3).

= Build Status Reporting

#r("builder.status.nix-to-proto")[
  The mapping from `rio_nix::BuildStatus` to `proto::BuildResultStatus` MUST
  be exhaustive (no `_` arm). Adding a status variant is a compile error until
  the mapping is extended. (The gateway still translates these statuses for
  Nix clients; on the builder side the native exit classification produces
  `proto::BuildResultStatus` directly.)
]

#r("builder.timeout.no-reassign")[
  Build timeout is a build outcome, not an executor fault. It MUST surface as
  `BuildResultStatus::TimedOut` (permanent, not reassignable), not as
  `InfrastructureFailure`.
]

= Build Cancellation

#r("builder.cancel.cgroup-kill")[
  When the scheduler sends a `CancelSignal` on the BuildExecution stream, the
  builder's `try_cancel_build` writes `1` to the target build's `cgroup.kill`
  (SIGKILLs the entire cgroup tree). The build's executor task detects the
  daemon exit, releases the semaphore permit, tears down the overlay, and
  sends `CompletionReport{status: Cancelled}`. This is used for pod-preemption
  handling: the scheduler cancels builds on an evicting node before the
  SIGTERM grace period wastes `terminationGracePeriodSeconds`.
]

#r("builder.cancel.pre-cgroup-deferred")[
  A cancel that arrives before the per-build cgroup exists (`cgroup.kill` →
  ENOENT) MUST leave the cancelled flag set. The executor MUST check the flag
  before the prefetch/register phase and abort with `Cancelled` status without
  starting the build. The pre-cgroup window is overlay setup → resolve →
  glue → register_inputs + prefetch_manifests --- sub-second since
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

= Stream Relay & Reconnect

#r("builder.completion.pending-armed-early")[
  `completion_pending` MUST be armed `true` at the start of `executor_future`,
  before the first `.await`. The flag means "completion owed, not yet flushed"
  (NOT "completion queued"): on panic, `_slot_guard` drops during
  `catch_unwind`'s unwind BEFORE the panic-catcher's `handle.await` resolves
  and calls `send_completion`, so the bug_472 invariant ("`_slot_guard` drops
  AFTER `send_completion`") inverts. With the flag armed early,
  `wait_build_flushed` parks the done-watcher across that gap and the
  panic-catcher's `InfrastructureFailure` reaches the wire instead of a dead
  stream (bug_012). The redundant store inside `send_completion` stays for any
  future caller outside `executor_future`.
]

#r("builder.relay.graceful-exit-close")[
  On terminal exit (`BuildComplete` / `drain_done`), the builder MUST park the
  relay target to `None` and drain the response stream to server-close
  (bounded 2s) before dropping `build_stream`. `relay_loop` clears
  `completion_pending` on `grpc_tx.send()` Ok, which only means "in the
  256-cap mpsc buffer between relay and tonic's body driver"; a raw
  `build_stream` drop is h2 `RST_STREAM(CANCEL)` → hyper drops the
  request-body driver → buffered `Completion` discarded (bug_117). Parking
  drops all `grpc_tx` senders so `ReceiverStream` yields `None` → tonic
  flushes buffered frames + END_STREAM; draining the response side then lets
  `build_stream` drop after server half-close instead of RST. Best-effort: on
  2s elapse the scheduler observes `ExecutorDisconnected` and re-dispatches
  anyway.
]

#r("builder.relay.reconnect")[
  Running builds send `CompletionReport`/`BuildLogBatch`/`PrefetchComplete` to
  a process-lifetime `mpsc::channel(256)` (the permanent sink), NOT to the
  gRPC outbound channel directly. A `relay_loop` task pumps the sink into
  whichever gRPC outbound channel is currently live, tracked via
  `watch::channel<Option<Sender>>`. On `BuildExecution` stream close/error the
  reconnect loop swaps the watch to `None` (relay blocks on `changed()`, sink
  buffers in its 256-slot backlog --- \~25s at typical 100ms-batch log rates),
  sleeps \~1s, opens a fresh stream, and swaps the new gRPC channel in. The
  relay recovers the one in-transit message lost on transition
  (`mpsc::error::SendError<T>` holds it). The pump loop MUST `select!`
  `biased;` on `target.changed()` BEFORE `sink_rx.recv()` --- `grpc_tx.send()`
  may keep succeeding into a zombie tonic `ReceiverStream` that outlived its
  network stream (I-032: completions silently lost for \~20min after scheduler
  failover). Why a permanent sink: `stderr_loop` breaks the build with
  `MiscFailure` if its log send fails; handing build tasks the gRPC channel
  directly would kill every running build on scheduler failover.
]

#r("builder.result.input-materialization-is-infra+3")[
  A build failure caused by an input path that was verified present in
  rio-store during input resolution but could not be materialized on the
  worker (FUSE JIT-fetch error, overlay race) MUST be reported as
  `BuildResultStatus::InfrastructureFailure` (not `PermanentFailure`): it is a
  worker-local fault, not a build defect. The native executor detects this
  structurally --- the input bind-mount fails during sandbox setup, which is
  an infrastructure-transient error (#rref("builder.retry.infra-transient"))
  --- rather than by parsing error text.
]
= Shutdown

#r("builder.idle-exit+2")[
  The reconnect loop's `select!` has a `tokio::time::sleep_until(last_activity
  + idle_timeout)` arm guarded by `!slot.is_busy()`. `last_activity` is bumped
  on every received scheduler message (Assignment, Cancel, Prefetch). If
  `idle_timeout` elapses with the slot still idle, the builder logs `"idle
  timeout (no assignment); exiting"` and breaks the loop with the same
  `BuildComplete` exit path as a finished build (heartbeat abort → FUSE abort
  → return from `main()`). The arm is `biased;` after the build-done arm so a
  coinciding completion wins. I-116: a Karpenter-scaled pod that the scheduler
  never dispatches to (intent mismatch, drained pool) exits cleanly instead of
  idling to `activeDeadlineSeconds`.
]

#r("builder.shutdown.sigint+2")[
  The builder handles both SIGTERM and SIGINT by breaking the BuildExecution
  select loop, running teardown (heartbeat abort → FUSE abort), and returning
  from `main()`. Local development (`cargo run` → Ctrl+C) and Kubernetes pod
  deletion (kubelet → SIGTERM) share the same exit path. Returning from
  `main()` lets `fuse_session`'s `Mount` drop (`fusermount -u`) and atexit
  handlers fire (LLVM profraw flush).
]

#r("builder.shutdown.idle-no-reregister+2")[
  On SIGTERM with an idle build slot AND no `CompletionReport` pending in the
  permanent sink, the builder MUST break the reconnect loop without sending a
  fresh `ExecutorRegister`. The reconnect-under-drain machinery exists so an
  in-flight build's `CompletionReport` reaches the (possibly new) leader; an
  idle slot with no pending completion has nothing to report. If a completion
  IS buffered in the sink (build finished during a stream-retry sleep with
  `relay_target=None`), the loop MUST reconnect once to flush before exiting
  --- `_slot_guard` drops AFTER `send_completion` queues the report, so
  slot-idle alone does not imply delivered (bug_472). Re-registering bumps the
  scheduler's `workers_active`, and the heartbeat task (aborted only after the
  loop exits) keeps `last_heartbeat` fresh until the process actually exits
  --- under coverage instrumentation the profraw atexit write delays that by
  \~80s (I-195, GHA 24018216226). The same fast-path applies on any subsequent
  `'reconnect` iteration where `draining=true`, the slot has since gone idle,
  and `completion_pending` is clear.
]

#r("builder.ephemeral.exit-aborts-heartbeat+2")[
  On exit from the reconnect loop (single-shot build done, idle timeout, or
  drain complete), the builder MUST abort the heartbeat task before
  `drop(fuse_session)`. A live heartbeat with a closed BuildExecution stream
  presents to the scheduler as an undispatchable zombie executor (I-142). The
  builder does NOT call `AdminService.DrainExecutor` --- the service-token
  gate (#rref("sec.authz.service-token")) allowlists controller and rio-cli
  only, and the builder is intentionally excluded from the `serviceHmac`
  mount; deregistration is via stream-close → `ExecutorDisconnected`
  (heartbeat already reported `draining=true`).
]

#r("builder.shutdown.fuse-abort")[
  On the shutdown path, the builder MUST abort the FUSE connection (write `1`
  to `/sys/fs/fuse/connections/<dev_minor>/abort`) BEFORE dropping the
  `BackgroundSession`. The builder serves the FUSE mount (fuser threads) while
  the sandboxed build consumes it (overlay→FUSE `lstat` during JIT input
  fetch); if the runtime tears down while build threads are parked in the kernel's
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
    [Build execution (request glue → rio-exec sandbox → result pipeline)],

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

    src("rio-builder/src/upload.rs"),
    [Chunk and upload build outputs (streaming NAR → rio-store PutPath)],

    src("rio-builder/src/log_stream.rs"),
    [Build log batching (64-line/100ms) and streaming via gRPC],

    src("rio-builder/src/cgroup.rs"),
    [cgroup v2 per-build subtree: memory.peak + polled cpu.stat for tracking;
      memory.max + cpu.max for enforcement. Fixes the Phase 2c VmHWM bug
      (daemon-PID measured \~10MB; cgroup is tree-wide).],

    src("rio-builder/src/health.rs"),
    [axum `/healthz` + `/readyz` (builder has no gRPC server; K8s probes hit
      HTTP). Readiness tracks heartbeat-accepted.],

    src("rio-builder/src/runtime.rs"),
    [Heartbeat request builder + build-spawn context + prefetch-hint handler.
      Extracted glue between `main.rs` and the subsystems.],
  ),
)

= Failure modes

#figure(
  table(
    columns: (auto, 1fr),
    align: (left, left),
    [*Immediate effect*], [Running build on that pod is orphaned],
    [*Cascading effect*],
    [Scheduler detects via missed heartbeats (\~50--60s wall-clock), calls
      `reset_to_ready()` on the affected derivation --- it goes straight back
      to Ready (increments `retry_count`) and re-queues, no intermediate
      `InfrastructureFailure` classification],

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
immediately (fail-fast). The breaker state is reported to the scheduler via
#rref("builder.heartbeat.store-degraded") so the builder is excluded from
assignment.

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
fails with `EINVAL`); the merged dir is mounted at `{build_dir}/nix/store`
and bind-mounted writable at `/nix/store` inside the build sandbox. Path
metadata for the closure comes straight from rio-store's
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
with hundreds of store paths. Running a full Nix installation per worker with
its own store (copying paths in via `nix copy`) works but duplicates store
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
ordering (FUSE mount → overlayfs → rio-exec sandbox) must be correct
(#rref("builder.ns.order+4")). The decided approach is that each executor runs
the FUSE layer that lazily fetches store paths from rio-store; each build gets
a per-build overlayfs with the FUSE mount as lower and a per-build synthetic
SQLite database in the upper layer. This avoids shared mutable state,
eliminates shared PV infrastructure, and provides local-disk performance via
SSD caching.

== Streaming builder model // supersedes ADR-011
<sec-rationale-streaming>

The communication model between scheduler and workers determines latency,
failure handling, and operational characteristics; the scheduler must be able
to send control signals (cancel, prefetch hints) to the active build.

Workers connect to the scheduler via a bidirectional `BuildExecution`
streaming RPC. Builder pods are now one-build-per-pod (one-shot Jobs), so each
stream's lifetime spans exactly one derivation. A single stream per worker
carries: scheduler → worker --- build assignments (derivation + input closure
metadata), prefetch hints for cache warming, cancel signals; worker →
scheduler --- build log batches (streamed incrementally), build completion
reports (success/failure, output paths, timing), acknowledgments. Heartbeats
are a *separate unary RPC* (`Worker.Heartbeat`), not carried on the
`BuildExecution` stream --- the heartbeat loop runs independently of stream
lifecycle so liveness reporting survives stream reconnection. The stream
provides natural #gls("backpressure"): if a worker is overwhelmed, gRPC flow control
slows the scheduler's assignment rate. Connection drops are detected via gRPC
keepalives, enabling fast failure detection and rescheduling.

*Alternatives considered.* Unary RPC polling (worker pulls jobs) is simple but
adds polling latency, generates unnecessary traffic when idle, and requires
separate mechanisms for cancel signals and log streaming. A message queue
(NATS, RabbitMQ, Kafka) decouples scheduler and workers but adds
infrastructure complexity and another failure domain; build log streaming over
a message queue is awkward, and ordered delivery of cancel signals is harder
to guarantee. Server-sent events + REST has no bidirectional streaming, so log
streaming and cancel signals require separate channels. Separate gRPC unary
RPCs with server streaming for logs is more granular but requires correlating
multiple streams per build and managing their lifecycles independently.

*Consequences.* A single stream per worker simplifies connection management
and multiplexes all communication; natural backpressure prevents worker
overload without explicit rate limiting; incremental log streaming gives
dashboard users real-time build output. On the negative side: long-lived
streams are sensitive to network instability and require robust reconnection
logic with state reconciliation (#rref("builder.relay.reconnect")), and
debugging stream-level issues is harder than debugging individual
request/response RPCs.

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

*The hard part: executor pod security.* overlayfs and the rio-exec sandbox both
require `CAP_SYS_ADMIN` + `CAP_SYS_CHROOT`, which conflicts with
PodSecurityStandards on managed Kubernetes clusters. Mitigations: dedicated
node pools with relaxed pod security policies for executor pods, custom
seccomp profiles that allow only the specific syscalls needed (mount,
pivot_root), and NetworkPolicy isolation to restrict executor pod network
access. The rio-exec sandbox is NOT a security boundary --- it's a purity mechanism
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
