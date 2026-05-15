#import "/lib/rio.typ": *

#show: rio.with(domains: none)

= Overview

#figure(
  caption: [System overview --- layered request path. Clients speak the Nix
    worker protocol over SSH; the gateway translates to internal gRPC; the
    scheduler and store are replica sets backed by PostgreSQL/S3; builders are
    ephemeral one-shot pods reconciled by the controller.],
  diagram(
    spacing: (26mm, 10mm),
    node-stroke: 0.5pt,
    // ───── layer 1: clients
    node(
      (0.5, 0),
      name: <clients>,
      width: 30em,
      align(left)[
        *Nix Clients* \
        #text(size: 0.8em)[
          Path A (remote store): `nix build --store ssh-ng://rio .#pkg` \
          Path B (build hook): `nix.buildMachines = [{ hostName="rio"; protocol="ssh-ng"; }]`
        ]
      ],
    ),
    edge(<clients>, <gw>, "-|>", [ssh-ng (worker protocol)], label-size: 0.8em),
    // ───── layer 2: gateway
    node(
      (0.5, 1),
      name: <gw>,
      width: 30em,
      fill: accent.lighten(88%),
      align(left)[
        *rio-gateway* #text(size: 0.8em, fill: muted)[--- multiple replicas, stateless] \
        #text(size: 0.8em)[
          SSH server (russh) → worker-protocol handler → gRPC. \
          Handles `wopBuildDerivation`, `wopQueryPathInfo`, `wopAddToStoreNar`, … \
          Auth: SSH key → tenant. Per-connection ephemeral state only.
        ]
      ],
    ),
    edge(<gw>, <sched>, "-|>", [gRPC], label-size: 0.8em, label-side: right),
    edge(<gw>, <store>, "-|>", [gRPC], label-size: 0.8em, label-side: left),
    // ───── layer 3: scheduler ∥ store
    node(
      (-0.2, 2.3),
      name: <sched>,
      shape: rect,
      width: 12em,
      fill: accent.lighten(88%),
      align(left)[
        *rio-scheduler* \
        #text(size: 0.8em, fill: muted)[leader-elected] \
        #text(size: 0.8em)[
          Global build DAG \
          Critical-path scheduling \
          Resource-fit hard-filter \
          State: PostgreSQL
        ]
      ],
    ),
    node(
      (1.2, 2.3),
      name: <store>,
      shape: rect,
      width: 15em,
      fill: accent.lighten(88%),
      align(left)[
        *rio-store* \
        #text(size: 0.8em, fill: muted)[chunked CAS --- FastCDC + BLAKE3]
        #block(
          stroke: 0.4pt + rule-color,
          inset: 4pt,
          radius: 2pt,
          width: 100%,
          above: 0.5em,
          below: 0.4em,
          text(
            size: 0.75em,
          )[*PostgreSQL* --- narinfo, refs, manifests, CA index],
        )
        #block(
          stroke: 0.4pt + rule-color,
          inset: 4pt,
          radius: 2pt,
          width: 100%,
          below: 0em,
          text(size: 0.75em)[*S3* --- BLAKE3 chunks, inline blobs \<256 KiB],
        )
      ],
    ),
    edge(<sched>, <store>, "<|-|>", label-size: 0.8em),
    edge(
      <sched>,
      <pods>,
      "-|>",
      [gRPC `BuildExecution`],
      label-size: 0.75em,
      label-side: right,
    ),
    edge(<store>, <pods>, "-|>", [gRPC], label-size: 0.8em, label-side: left),
    // ───── layer 4: builder pods
    let bldr(tag) = align(left)[
      *#tag* \
      #text(size: 0.75em)[
        FUSE `/nix/store` + SSD cache \
        overlayfs + synth SQLite DB \
        nix sandbox
      ]
    ],
    node(
      (-0.2, 3.6),
      name: <pods-hdr>,
      stroke: none,
      inset: 0pt,
      text(size: 0.8em, fill: muted)[Builder Pods (K8s, `CAP_SYS_ADMIN`)],
    ),
    node((-0.2, 4.0), name: <b0>, shape: rect, bldr[builder-0]),
    node((0.5, 4.0), name: <bdots>, stroke: none, text(fill: muted)[⋯]),
    node((1.2, 4.0), name: <bn>, shape: rect, bldr[builder-N]),
    node(
      enclose: (<pods-hdr>, <b0>, <bdots>, <bn>),
      name: <pods>,
      stroke: (paint: muted, dash: "dashed"),
      inset: 8pt,
    ),
    // ───── layer 5: controller (no direct traffic)
    node(
      (0.5, 5.2),
      name: <ctrl>,
      width: 30em,
      align(left)[
        *rio-controller* #text(size: 0.8em, fill: muted)[--- K8s operator, single-replica] \
        #text(size: 0.8em)[
          CRDs: `Pool`, `ComponentScaler`. Watches K8s API → reconciles builder Jobs, GC.
        ]
      ],
    ),
    edge(<ctrl>, <pods>, "..|>", [reconciles], label-size: 0.8em),
  ),
)

The controller is a supervisor that manages the lifecycle of all other
components via the Kubernetes API. It does not receive direct traffic from
builders or other components --- it watches CRDs and reconciles desired state.

== Component Links

- *#link("./spec/components/gateway.typ")[rio-gateway]* --- SSH server, Nix protocol
  frontend
- *#link("./spec/components/scheduler.typ")[rio-scheduler]* --- DAG-aware build
  scheduler
- *#link("./spec/components/store.typ")[rio-store]* --- Chunked CAS
- *#link("./spec/components/builder.typ")[rio-builder]* --- Build executor with FUSE
  store
- *#link("./spec/components/controller.typ")[rio-controller]* --- Kubernetes operator
- *#link("./spec/components/proto.typ")[rio-proto]* --- gRPC service definitions
- *rio-nix* --- Nix protocol implementation library (wire primitives, ATerm,
  NAR, store paths)
- *rio-common* --- shared utilities (limits, observability init)
- *#link("./spec/components/dashboard.typ")[rio-dashboard]* --- Web dashboard (Phase 5)

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
  #link("./spec/components/scheduler.typ")[scheduler spec].
]

See #link("./spec/components/gateway.typ")[rio-gateway] for protocol opcode details,
#link("./spec/components/scheduler.typ")[rio-scheduler] for the scheduling algorithm,
and #link("./spec/components/store.typ")[rio-store] for the chunked CAS.

#figure(
  chronos.diagram({
    import chronos: *
    _par("Client", display-name: [Nix Client])
    _par("GW", display-name: [rio-gateway])
    _par("Sched", display-name: [rio-scheduler])
    _par("Builder", display-name: [rio-builder])
    _par("Store", display-name: [rio-store])

    _seq("Client", "GW", comment: [SSH connect + handshake])
    _seq(
      "Client",
      "GW",
      comment: [`wopSetOptions` (`ssh://` only; ssh-ng skips)],
    )
    _seq("Client", "GW", comment: [`wopQueryValidPaths`])
    _seq("GW", "Store", comment: [`FindMissingPaths`])
    _seq("Store", "GW", comment: [missing paths], dashed: true)
    _seq("GW", "Client", comment: [valid paths (inverted)], dashed: true)
    _seq("Client", "GW", comment: [`wopAddToStoreNar` (.drv files)])
    _seq("GW", "Store", comment: [`PutPath`])
    _seq("Client", "GW", comment: [`wopQueryDerivationOutputMap`])
    _seq("GW", "Store", comment: [`GetPath` (.drv NAR)])
    _seq("GW", "GW", comment: [parse ATerm → output map])
    _seq("GW", "Client", comment: [derivation output map], dashed: true)
    _seq("Client", "GW", comment: [`wopBuildDerivation`])
    _seq("GW", "Sched", comment: [`SubmitBuild` (DAG)])
    _seq("Sched", "Store", comment: [`FindMissingPaths` (cache check)])
    _seq("Store", "Sched", comment: [missing paths], dashed: true)
    _seq("Sched", "Builder", comment: [`WorkAssignment` (via `BuildExecution`)])
    _seq("Builder", "Store", comment: [`GetPath` (FUSE fetch)])
    _seq("Builder", "Builder", comment: [nix sandbox build])
    _seq("Builder", "Sched", comment: [`BuildLogBatch`])
    _seq("Sched", "GW", comment: [`BuildEvent` (logs)])
    _seq("GW", "Client", comment: [`STDERR_NEXT`])
    _seq("Builder", "Store", comment: [`PutPath` (output)])
    _seq("Builder", "Sched", comment: [`CompletionReport`])
    _seq("Sched", "GW", comment: [`BuildEvent` (completed)])
    _seq("GW", "Client", comment: [`STDERR_LAST` + `BuildResult`])
    _seq("Client", "GW", comment: [`wopNarFromPath`])
    _seq("GW", "Store", comment: [`GetPath`])
    _seq("Store", "GW", comment: [NAR stream], dashed: true)
    _seq("GW", "Client", comment: [NAR data], dashed: true)
  }),
  caption: [Remote-store build flow (`nix build --store ssh-ng://rio`).],
)

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

#figure(
  chronos.diagram({
    import chronos: *
    _par("Daemon", display-name: [Local nix-daemon])
    _par("Hook", display-name: [Build Hook])
    _par("GW", display-name: [rio-gateway])
    _par("Sched", display-name: [rio-scheduler])
    _par("Builder", display-name: [rio-builder])

    _seq("Daemon", "Hook", comment: [delegate derivation])
    _seq("Hook", "GW", comment: [SSH connect])
    _seq("Hook", "GW", comment: [`wopBuildDerivation` (single)])
    _seq("GW", "Sched", comment: [`SubmitBuild` (single node)])
    _seq("Sched", "Builder", comment: [`WorkAssignment`])
    _seq("Builder", "Builder", comment: [build])
    _seq("Builder", "Sched", comment: [`CompletionReport`])
    _seq("Sched", "GW", comment: [`BuildEvent` (completed)])
    _seq("GW", "Hook", comment: [`BuildResult`])
    _seq("Hook", "Daemon", comment: [output path])
    _seq("Daemon", "Daemon", comment: [continue DAG])
  }),
  caption: [Build-hook flow (`--builders 'ssh-ng://rio'`).],
)

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

#figure(
  chronos.diagram({
    import chronos: *
    _par("Client", display-name: [Nix Client])
    _par("GW", display-name: [rio-gateway])
    _par("Sched", display-name: [rio-scheduler])
    _par("Builder", display-name: [rio-builder])

    _seq("Client", "GW", end-tip: "x", comment: [SSH connection drops])
    _seq("GW", "Sched", comment: [`CancelBuild` (client_disconnect)])
    _alt(
      [Shared derivation],
      { _seq("Sched", "Sched", comment: [continue (other builds need it)]) },
      [Unique derivation],
      { _seq("Sched", "Sched", comment: [remove from queue immediately]) },
    )
    _seq("Builder", "Sched", comment: [`CompletionReport` (if already Running)])
    _note("over", [Outputs kept in store regardless], pos: "Sched")
    _seq("Client", "GW", comment: [Reconnect + re-submit])
    _seq("Sched", "Sched", comment: [DAG merge + cache hits on stored outputs])
  }),
  caption: [Client-disconnection handling.],
)

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
