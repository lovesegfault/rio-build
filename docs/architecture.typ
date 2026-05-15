#import "/lib/rio.typ": *

#show: rio.with(domains: none)

= Overview

```text
┌──────────────────────────────────────────────────────────────────────┐
│                        Nix Clients                                   │
│                                                                      │
│  Path A (remote store):                                              │
│    nix build --store ssh-ng://rio .#package                          │
│                                                                      │
│  Path B (remote builder / build hook):                               │
│    nix.buildMachines = [{ hostName="rio"; protocol="ssh-ng"; ... }]  │
│    nix build .#package  (daemon delegates via build hook)            │
└─────────────┬──────────────────────────────────┬─────────────────────┘
              │ ssh-ng (worker protocol)         │ ssh-ng (worker protocol)
              ▼                                  ▼
┌──────────────────────────────────────────────────────────────────────┐
│                  rio-gateway (multiple replicas)                     │
│                                                                      │
│  SSH server (russh) -> Nix worker protocol handler                   │
│  Handles: handshake, wopSetOptions, wopBuildDerivation,              │
│           wopQueryPathInfo, wopAddToStoreNar, wopNarFromPath, etc.   │
│  Translates protocol ops -> internal gRPC calls                      │
│  Auth: SSH key-based, maps to tenants                                │
│  Multiplexes concurrent SSH sessions (no persistent state;           │
│  per-connection ephemeral state only)                                │
└──────────┬──────────────────────┬────────────────────────────────────┘
           │ gRPC                 │ gRPC
           ▼                      ▼
┌────────────────────┐  ┌─────────────────────────────────────────────┐
│   rio-scheduler    │  │              rio-store                      │
│   (leader-elected) │  │                                             │
│                    │  │  Chunked CAS (FastCDC + BLAKE3)             │
│  Global build DAG  │  │  ┌─────────────────────────────────┐        │
│  Critical-path     │◄►│  │ Metadata (PostgreSQL)           │        │
│  scheduling        │  │  │ narinfo, references, manifests  │        │
│  Resource-fit      │  │  │ CA content index (SHA-256)      │        │
│  hard-filter       │  │  └─────────────────────────────────┘        │
│  Streaming builder │  │  ┌─────────────────────────────────┐        │
│  assignment        │  │  │ Blobs (S3-compatible)           │        │
│  State: PostgreSQL │  │  │ Deduplicated chunks (BLAKE3)    │        │
│                    │  │  │ Inline blobs for NARs < 256 KiB │        │
└────────┬───────────┘  │                                             │
         │ gRPC         └──────────────┬──────────────────────────────┘
         │  (builders stream           │ gRPC
         │   work via BuildExecution)  │
         ▼                             ▼
┌──────────────────────────────────────────────────────────────────────┐
│                     Builder Pods (K8s, CAP_SYS_ADMIN)                │
│                                                                      │
│  ┌────────────┐  ┌────────────┐  ┌────────────┐  ┌────────────┐      │
│  │ builder-0  │  │ builder-1  │  │ builder-2  │  │ builder-N  │      │
│  │            │  │            │  │            │  │            │      │
│  │ FUSE mount │  │ FUSE mount │  │ FUSE mount │  │ FUSE mount │      │
│  │ /nix/store │  │ /nix/store │  │ /nix/store │  │ /nix/store │      │
│  │ + local    │  │ + local    │  │ + local    │  │ + local    │      │
│  │   SSD cache│  │   SSD cache│  │   SSD cache│  │   SSD cache│      │
│  │            │  │            │  │            │  │            │      │
│  │ per-build  │  │ per-build  │  │ per-build  │  │ per-build  │      │
│  │ overlayfs  │  │ overlayfs  │  │ overlayfs  │  │ overlayfs  │      │
│  │ + synth    │  │ + synth    │  │ + synth    │  │ + synth    │      │
│  │ SQLite DB  │  │ SQLite DB  │  │ SQLite DB  │  │ SQLite DB  │      │
│  │            │  │            │  │            │  │            │      │
│  │ nix sandbox│  │ nix sandbox│  │ nix sandbox│  │ nix sandbox│      │
│  └────────────┘  └────────────┘  └────────────┘  └────────────┘      │
└──────────────────────────────────────────────────────────────────────┘

┌──────────────────────────────────────────────────────────────────────┐
│                   rio-controller (K8s Operator)                      │
│                                                                      │
│  Manages: Pool Jobs, GC                                              │
│  CRDs: Pool, ComponentScaler                                         │
│  Watches: K8s API -> reconciles Jobs                                 │
│  Single-replica by design (not leader-elected)                       │
└──────────────────────────────────────────────────────────────────────┘
```

The controller is a supervisor that manages the lifecycle of all other
components via the Kubernetes API. It does not receive direct traffic from
builders or other components --- it watches CRDs and reconciles desired state.

== Component Links

- *#link("./components/gateway.md")[rio-gateway]* --- SSH server, Nix protocol
  frontend
- *#link("./components/scheduler.md")[rio-scheduler]* --- DAG-aware build
  scheduler
- *#link("./components/store.md")[rio-store]* --- Chunked CAS
- *#link("./components/builder.md")[rio-builder]* --- Build executor with FUSE
  store
- *#link("./components/controller.md")[rio-controller]* --- Kubernetes operator
- *#link("./components/proto.md")[rio-proto]* --- gRPC service definitions
- *rio-nix* --- Nix protocol implementation library (wire primitives, ATerm,
  NAR, store paths)
- *rio-common* --- shared utilities (limits, observability init)
- *#link("./components/dashboard.md")[rio-dashboard]* --- Web dashboard (Phase 5)

#figure(
  caption: [Component topology. Gateway terminates ssh-ng and fans out to
    scheduler/store via gRPC; builders pull work over the bidirectional
    `BuildExecution` stream; controller reconciles builder pods via the
    Kubernetes API.],
  diagram(
    spacing: (16mm, 12mm),
    node-stroke: 0.5pt,
    node((1.5, 0), [Nix Client], name: <client>),
    node((1.5, 1), [rio-gateway], name: <gw>, fill: accent.lighten(85%)),
    node((3.2, 1), [rio-dashboard], name: <dash>),
    node((0.5, 2), [rio-scheduler], name: <sched>, fill: accent.lighten(85%)),
    node((2.5, 2), [rio-store], name: <store>, fill: accent.lighten(85%)),
    node((4, 2), [S3], shape: fletcher.shapes.cylinder, name: <s3>),
    node((1.5, 3), [builder-N\ (FUSE + overlay)], name: <builders>),
    node((1.5, 4), [PostgreSQL], shape: fletcher.shapes.cylinder, name: <pg>),
    node((-1, 3), [rio-controller], name: <ctrl>),
    node((-1, 4), [K8s API], shape: fletcher.shapes.hexagon, name: <k8s>),
    edge(<client>, <gw>, "-|>", [ssh-ng], label-size: 0.8em),
    edge(<gw>, <sched>, "-|>", [gRPC], label-size: 0.8em),
    edge(<gw>, <store>, "-|>", [gRPC], label-size: 0.8em),
    edge(<dash>, <sched>, "-|>", [gRPC-Web], label-size: 0.75em, bend: -20deg),
    edge(<builders>, <sched>, "-|>", [BuildExecution], label-size: 0.75em),
    edge(<builders>, <store>, "-|>", [gRPC], label-size: 0.8em),
    edge(<store>, <s3>, "-|>"),
    edge(<sched>, <pg>, "-|>", bend: -15deg),
    edge(<store>, <pg>, "-|>", bend: 15deg),
    edge(<ctrl>, <k8s>, "-|>"),
    edge(<k8s>, <builders>, "..>", [manages], label-size: 0.8em),
  ),
)

= Data Flows

== Remote Store: `nix build --store ssh-ng://rio .#package`

```text
1. User runs: nix build --store ssh-ng://rio .#package
2. Nix evaluates the flake locally -> produces derivation DAG (.drv files)
3. Nix opens SSH connection to rio-gateway
4. Worker protocol handshake (magic bytes, version negotiation)
5. Nix sends wopSetOptions (build configuration; ssh:// only — ssh-ng skips)
6. Nix sends wopQueryValidPaths --- "which outputs do you have?"
7. rio-gateway queries rio-store -> returns valid paths
8. Nix sends wopAddToStoreNar for each missing .drv file and input source
   -> rio-gateway stores in rio-store
   (for protocol >= 1.32, sources are batched via wopAddMultipleToStore
    rather than individual wopAddToStoreNar calls)
8a. Nix sends wopQueryDerivationOutputMap for each derivation
    -> Modern Nix clients (>= 2.4) call this unconditionally for all
       derivation types (input-addressed and CA). rio-gateway resolves
       via rio-store and returns the output name -> store path mapping.
9. Nix sends wopBuildDerivation (or wopBuildPathsWithResults) for top-level
   -> wopBuildDerivation sends an inline BasicDerivation (WITHOUT inputDrvs)
   -> rio-gateway reconstructs the full DAG by parsing the .drv files
      uploaded in step 8 (each .drv contains inputDrvs references forming
      the DAG edges)
   -> forwards to rio-scheduler via gRPC SubmitBuild
10. rio-scheduler:
    a. Queries rio-store for cache hits (already-built outputs)
    b. Computes remaining build graph
    c. Computes critical-path priorities
    d. Dispatches ready derivations to executors
11. For each dispatched derivation:
    a. Scheduler sends PrefetchHint (anticipated input paths) on the
       BuildExecution stream so the executor can pre-warm its FUSE cache
    b. Executor's FUSE daemon checks local SSD cache for input paths (fast path)
    c. Cache miss: FUSE daemon fetches from rio-store via gRPC, caches on SSD
    d. Executor runs build in nix sandbox (overlay-merged /nix/store)
    e. Executor streams build logs to scheduler via bidirectional
       BuildExecution stream (log lines batched for efficiency)
       Scheduler relays logs to gateway via BuildEvent stream (from SubmitBuild)
       Gateway converts to STDERR_NEXT messages for the Nix client
    f. Executor streams output NAR via PutPath; rio-store chunks via
       FastCDC on the server side (executors never chunk locally)
    g. Executor reports completion to scheduler
    h. Scheduler stores completion, releases downstream nodes
12. When top-level derivation completes:
    a. Scheduler notifies gateway
    b. Gateway sends STDERR_LAST + BuildResult to Nix client
    c. Client requests wopNarFromPath for outputs
       -> Gateway sends STDERR_LAST first, then writes the NAR as raw
          bytes directly (no STDERR framing). The Nix client's
          processStderr(ex) has no sink argument for this opcode, so
          STDERR_WRITE would fail with 'error: no sink'. See
          rio-gateway/src/handler/opcodes_read.rs.
    d. Gateway streams NAR (reassembled from chunks) back to client
```

#info[
  *Status:* CA cutoff is end-to-end: compare (completion-time output-hash check
  against the content index) + propagate (Skipped status + DAG cascade) +
  resolve (CA-on-CA placeholder rewrite at dispatch time) + realisation_deps
  insert. The `rio_scheduler_ca_cutoff_saves_total` metric is the direct
  efficacy signal. See `r[sched.ca.cutoff-compare]`,
  `r[sched.ca.cutoff-propagate+2]`, `r[sched.ca.resolve]` in the
  #link("./components/scheduler.md")[scheduler spec].
]

See #link("./components/gateway.md")[rio-gateway] for protocol opcode details,
#link("./components/scheduler.md")[rio-scheduler] for the scheduling algorithm,
and #link("./components/store.md")[rio-store] for the chunked CAS.

// TODO(typst-migration): convert to chronos sequence diagram
```mermaid
sequenceDiagram
    participant Client as Nix Client
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Builder as rio-builder
    participant Store as rio-store

    Client->>GW: SSH connect + handshake
    Client->>GW: wopSetOptions (ssh:// only; ssh-ng skips)
    Client->>GW: wopQueryValidPaths
    GW->>Store: FindMissingPaths
    Store-->>GW: missing paths
    GW-->>Client: valid paths (inverted)
    Client->>GW: wopAddToStoreNar (.drv files)
    GW->>Store: PutPath
    Client->>GW: wopQueryDerivationOutputMap
    GW->>Store: GetPath (.drv NAR)
    GW->>GW: parse ATerm -> output map
    GW-->>Client: derivation output map
    Client->>GW: wopBuildDerivation
    GW->>Sched: SubmitBuild (DAG)
    Sched->>Store: FindMissingPaths (cache check)
    Store-->>Sched: missing paths
    Sched->>Builder: WorkAssignment (via BuildExecution)
    Builder->>Store: GetPath (FUSE fetch)
    Builder->>Builder: nix sandbox build
    Builder->>Sched: BuildLogBatch
    Sched->>GW: BuildEvent (logs)
    GW->>Client: STDERR_NEXT
    Builder->>Store: PutPath (output)
    Builder->>Sched: CompletionReport
    Sched->>GW: BuildEvent (completed)
    GW->>Client: STDERR_LAST + BuildResult
    Client->>GW: wopNarFromPath
    GW->>Store: GetPath
    Store-->>GW: NAR stream
    GW-->>Client: NAR data
```

== Remote Builder: `nix build --builders 'ssh-ng://rio ...'`

```text
1. User runs: nix build .#package (with rio configured as a builder)
2. Nix evaluates locally, starts building the DAG
3. For each derivation, local nix-daemon invokes the build hook
4. Build hook connects to rio-gateway via ssh-ng
5. Build hook sends the .drv path, system, and features
6. rio-gateway receives single-derivation build request
   -> creates a mini build plan in rio-scheduler
7. rio-scheduler assigns to an executor (same algorithm but single-derivation)
8. Executor builds, uploads output to rio-store
9. rio-gateway returns output to build hook
10. Build hook copies output back to local store
11. Local daemon continues with next derivation
```

#info[
  *Key difference:* in build hook mode, the local daemon drives the DAG
  traversal. rio only sees one derivation at a time. Less optimal scheduling,
  but fully compatible with any existing Nix setup.
]

#info[
  *Note on `--builders` mode:* In `--builders` mode, the local nix-daemon (not
  the build hook program directly) connects to rio-gateway via ssh-ng. What
  rio-gateway sees is a normal ssh-ng session with a specific operation
  pattern. The build hook is a local daemon concept; rio-gateway doesn't
  distinguish build hook vs direct client connections.
]

// TODO(typst-migration): convert to chronos sequence diagram
```mermaid
sequenceDiagram
    participant Daemon as Local nix-daemon
    participant Hook as Build Hook
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Builder as rio-builder

    Daemon->>Hook: delegate derivation
    Hook->>GW: SSH connect
    Hook->>GW: wopBuildDerivation (single)
    GW->>Sched: SubmitBuild (single node)
    Sched->>Builder: WorkAssignment
    Builder->>Builder: build
    Builder->>Sched: CompletionReport
    Sched->>GW: BuildEvent (completed)
    GW->>Hook: BuildResult
    Hook->>Daemon: output path
    Daemon->>Daemon: continue DAG
```

== Client Disconnection

```text
1. Client SSH connection drops (network failure, ctrl-c, etc.)
2. rio-gateway detects SSH channel close
3. Gateway sends CancelBuild to scheduler with reason="client_disconnect"
4. Scheduler policy:
   a. For derivations shared with other active builds: continue building
      (the DAG merge logic keeps shared derivation nodes live as long as
      at least one interested build remains)
   b. For derivations unique to this build: removed from the queue
      immediately. If already Running, the executor is allowed to complete
      (wasted work is bounded by one derivation per executor)
5. Completed outputs remain in rio-store regardless of client state
6. If the client reconnects and re-submits, the scheduler's DAG merge
   re-inserts the derivations. Any outputs already stored in step 5 are
   cache hits (instant completion via FindMissingPaths)
```

#info[
  *Not implemented (by design):* No orphan timeout window or explicit
  "reattach" mechanism. Reconnection safety comes from (a) shared-derivation
  DAG merge and (b) cache hits on already-stored outputs. A timed orphan grace
  period is not planned.
]

// TODO(typst-migration): convert to chronos sequence diagram
```mermaid
sequenceDiagram
    participant Client as Nix Client
    participant GW as rio-gateway
    participant Sched as rio-scheduler
    participant Builder as rio-builder

    Client-xGW: SSH connection drops
    GW->>Sched: CancelBuild (client_disconnect)
    alt Shared derivation
        Sched->>Sched: continue (other builds need it)
    else Unique derivation
        Sched->>Sched: remove from queue immediately
    end
    Builder->>Sched: CompletionReport (if already Running)
    Note over Sched: Outputs kept in store regardless
    Client->>GW: Reconnect + re-submit
    Sched->>Sched: DAG merge + cache hits on stored outputs
```

== Scheduler Failover

```text
1. Scheduler leader pod dies (crash, node failure, rolling update)
2. New scheduler pod acquires the Kubernetes Lease for leader election
3. New leader reconstructs in-memory state from PostgreSQL
   (see scheduler.md State Recovery). Dispatch is gated on
   recovery_complete.
4. Executors detect stream break, reconnect BuildExecution streams to new leader
5. For gateway connections with active SubmitBuild streams:
   a. The SubmitBuild response stream (BuildEvent) breaks with a gRPC
      Transport error
   b. Gateway's process_stream classifies the error as
      StreamProcessError::Transport and re-subscribes via
      WatchBuild(build_id, since_sequence) — up to 5 times with
      exponential backoff (1/2/4/8/16s)
   c. New scheduler replays BuildEvents from build_event_log starting
      at since_sequence. Nix client sees continuous STDERR streaming
      (possibly a brief pause during backoff)
   d. If all 5 reconnects fail OR the error is EofWithoutTerminal/Wire
      → gateway returns MiscFailure to the client (manual retry)
   e. If the gateway itself also restarted, see Client Disconnection above
6. Log events between the old leader's last S3 flush and its crash may
   be lost (bounded by the 30s periodic flush; see observability.md)
```

== Import-From-Derivation (IFD)

IFD occurs when Nix evaluation depends on a build result. The flow is:

```text
1. Client begins evaluation, discovers it needs to build a derivation
   before it can continue evaluation
2. Client opens a separate SSH channel (the primary channel is blocked
   in evaluation) and sends wopBuildDerivation for the IFD derivation
3. rio-gateway receives a single-derivation build request on the new channel
   -> rio-gateway forwards to rio-scheduler as a SubmitBuild with
      priority_class = "interactive" (IFD builds are evaluation-blocking)
4. rio-scheduler detects IFD priority: the scheduler assigns maximum
   priority to this derivation (above all queued non-IFD work)
5. Executor builds the derivation, uploads output
6. rio-gateway returns BuildResult to the client on the IFD channel
7. Client retrieves the output via wopNarFromPath on the IFD channel
8. Client resumes evaluation using the IFD output
9. Client may submit the full DAG (including the IFD derivation) on the
   primary channel --- the IFD derivation is already cached (instant hit)
```

#info[
  *Detection heuristic:* IFD builds arrive as individual `wopBuildDerivation`
  calls, typically before the full DAG is submitted via
  `wopBuildPathsWithResults`. The gateway annotates the `SubmitBuildRequest`
  with `priority_class = "interactive"` when the session has not yet seen a
  `wopBuildPathsWithResults` call (see `rio-gateway/src/handler/build.rs`).
  There is no dedicated `is_ifd_hint` field; priority classification is
  conveyed entirely through `priority_class`.
]

= Rationale

== Evaluation is external
// supersedes ADR-002

Nix handles evaluation. rio-build receives derivations via the protocol and
orchestrates distributed execution only. Evaluation scheduling and VCS
integration are explicitly out of scope. Users run `nix-eval-jobs`, `nix
build`, or CI orchestrators to produce derivations, which are then submitted
to rio-build.

This cleanly separates concerns: the Nix evaluator is a complex, rapidly
evolving component with IFD (import-from-derivation) semantics, flake
resolution, and channel handling. rio-build focuses on what it can do better
than existing tools: distributed, scheduled, cached execution.

The result is a dramatically simpler system --- the scheduler only reasons
about derivation DAGs, not Nix expressions --- and users keep control over
evaluation (pinned Nix versions, custom evaluator flags, IFD policies). The
trade-off is that users must run evaluation themselves before submitting to
rio-build, adding a step compared to Hydra's "point at a flake" model, and
rio-build cannot optimize across the eval-build boundary (e.g., cancelling
evaluation early if a build fails).

== Org-scale backend
// supersedes ADR-003

rio-build is a multi-project, multi-user, persistent service with
observability, a web dashboard, and an API. It replaces Hydra's build
execution and binary cache layers. It does not replace evaluation scheduling
or VCS integration, which are handled by external orchestrators (GitHub
Actions, `nix-eval-jobs`, etc.).

The service is designed for org-scale: multiple teams sharing a build cluster,
with per-project isolation, priority scheduling, and a unified binary cache.
Organizations get a single shared build backend with proper multi-tenancy
(replacing ad-hoc SSH builder configurations), and the unified binary cache
across all builds eliminates redundant work. The cost is higher operational
complexity than single-user solutions --- PostgreSQL, object storage, and
Kubernetes are required --- and the scope is large enough that phased
delivery is essential to avoid over-engineering early phases.

= Design risks

== Import-From-Derivation
// from challenges.md §4

Nix evaluation may block on build results. The gateway must handle this
gracefully --- the client sends a build request mid-evaluation, and rio must
prioritize these "evaluation-blocking" builds. These show up as individual
`wopBuildDerivation` calls that arrive before the full DAG is known. See the
Import-From-Derivation data flow above.

== Schema migration
// from challenges.md §15

Database schema evolves across phases (new tables, new columns, index
changes). Migrations must be:

- *Forward-compatible*: old code must tolerate new columns (use `ADD COLUMN
  ... DEFAULT`)
- *Versioned*: use `sqlx migrate` with numbered migration files
- *Tested*: rollback scripts for each migration, tested in CI
- *Blue-green compatible*: during deployment, both old and new code versions
  may run simultaneously
