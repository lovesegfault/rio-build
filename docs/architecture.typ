#import "/lib/rio.typ": *

#show: rio.with(domains: none)

= Overview

#figure(
  caption: [System overview --- layered request path. Clients speak the Nix
    @worker-protocol over SSH; the gateway translates to internal gRPC; the
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
          Path B (@build-hook): `nix.buildMachines = [{ hostName="rio"; protocol="ssh-ng"; }]`
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
          Global build @dag \
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
        #text(size: 0.8em, fill: muted)[chunked @cas --- @fastcdc + @blake3]
        #block(
          stroke: 0.4pt + rule-color,
          inset: 4pt,
          radius: 2pt,
          width: 100%,
          above: 0.5em,
          below: 0.4em,
          text(
            size: 0.75em,
          )[*PostgreSQL* --- @narinfo, refs, manifests, CA index],
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
      [gRPC pull
        (`PullAssignment`)],
      label-size: 0.75em,
      label-side: right,
    ),
    edge(<store>, <pods>, "-|>", [gRPC], label-size: 0.8em, label-side: left),
    // ───── layer 4: builder pods
    let bldr(tag) = align(left)[
      *#tag* \
      #text(size: 0.75em)[
        @fuse `/nix/store` + SSD cache \
        @overlayfs + synth SQLite DB \
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

- *#cross-link("/spec/components/gateway.typ")[rio-gateway]* --- SSH server, Nix protocol
  frontend
- *#cross-link("/spec/components/scheduler.typ")[rio-scheduler]* --- DAG-aware build
  scheduler
- *#cross-link("/spec/components/store.typ")[rio-store]* --- Chunked CAS
- *#cross-link("/spec/components/builder.typ")[rio-builder]* --- Build executor with FUSE
  store
- *#cross-link("/spec/components/controller.typ")[rio-controller]* --- Kubernetes operator
- *#cross-link("/spec/components/proto.typ")[rio-proto]* --- gRPC service definitions
- *rio-nix* --- Nix protocol implementation library (wire primitives, @aterm,
  @nar, store paths)
- *rio-common* --- shared utilities (limits, observability init)
- *#cross-link("/spec/components/dashboard.typ")[rio-dashboard]* --- Web dashboard (Phase 5)

#figure(
  caption: [Component topology. Gateway terminates ssh-ng and fans out to
    scheduler/store via gRPC; builder and store-executor pods pull work
    over the unary pull protocol (`PullAssignment` /
    `ReportAttemptOutcome`); controller reconciles builder pods via the
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
    edge(<gw>, <sched>, "-|>", [gRPC], label-size: 0.8em, label-side: left),
    edge(<gw>, <store>, "-|>", [gRPC], label-size: 0.8em, label-side: right),
    // QA4-#4: label-pos pulls "gRPC-Web" toward <dash> so the bend's
    // midpoint (which crosses <gw>/<store>) stays clear.
    edge(
      <dash>,
      <sched>,
      "-|>",
      [gRPC-Web],
      label-size: 0.75em,
      bend: -20deg,
      label-pos: 0.18,
      layer: 1,
    ),
    edge(
      <builders>,
      <sched>,
      "-|>",
      [pull RPCs],
      label-size: 0.75em,
      label-side: left,
      label-pos: 0.35,
    ),
    edge(
      <builders>,
      <store>,
      "-|>",
      [gRPC],
      label-size: 0.8em,
      label-side: right,
    ),
    edge(<store>, <s3>, "-|>"),
    edge(<sched>, <pg>, "-|>", bend: -15deg),
    edge(<store>, <pg>, "-|>", bend: 15deg),
    edge(<ctrl>, <k8s>, "-|>"),
    edge(<k8s>, <builders>, "..>", [manages], label-size: 0.8em),
  ),
)

= Data Flows

// pinit callout helper for chronos sequence diagrams. PDF-only: pinit
// resolves pins via page-absolute coordinates, which has no analogue in
// the HTML target. The same prose is rendered as a compact note list
// after each figure for the web build (`flow-notes-web`).
//
// `flow-note` places every callout body in a fixed right-hand column
// (page-absolute x = `_note-col-x`) at the pin's y-position, then draws
// a leader arrow of whatever length is needed back to the pin. This
// keeps bodies aligned regardless of which lifeline the pin sits on;
// the diagram itself is scaled (`_flow-scale`) to leave that column
// empty.
#let _flow-scale = 58%
#let _note-col-w = 4.1cm
// A4, rio template x-margin 2.6cm → content right edge at 18.4cm.
#let _note-col-x = 18.4cm - _note-col-w
#let flow-note(key, dy: 0pt, body) = context if not is-html-target() {
  pinit(key, callback: pos => {
    let body-y = pos.y + dy
    absolute-place(dx: _note-col-x, dy: body-y - 0.55em, block(
      width: _note-col-w,
      text(size: 0.78em, fill: muted.darken(15%), body),
    ))
    absolute-place(simple-arrow(
      start: (_note-col-x - 3pt, body-y + 1pt),
      end: (pos.x + 3pt, pos.y + 2pt),
      fill: muted,
      thickness: 1pt,
    ))
  })
}
// block(width: 100%) is paged-only — inside html.frame()'s paged
// sub-context there's no container width, so 100% → 0pt → zero-width
// SVG (QA #1). is-html-target() (compile-global via `--input x-target`,
// NOT the contextual `target()` which would evaluate to "paged" inside
// html.frame) gates the wrapper; html mode emits bare scale() for
// intrinsic width and frame-figure's .rio-figure CSS handles centering.
#let flow-diagram(factor: _flow-scale, body) = if is-html-target() {
  scale(factor, reflow: true, origin: top + left, body)
} else {
  block(
    width: 100%,
    align(left, scale(factor, reflow: true, origin: top + left, body)),
  )
}
#let flow-notes-web(..items) = if is-html-target() {
  info(title: [Flow notes])[#list(..items.pos())]
}

== Remote Store: `nix build --store ssh-ng://rio .#package`

The client evaluates locally and drives the worker protocol; rio translates
each opcode into internal gRPC. Load-bearing protocol detail is annotated
on the message arrows below.

#info[
  *Status:* CA cutoff is end-to-end: compare (completion-time output-hash check
  against the content index) + propagate (Skipped status + DAG cascade) +
  resolve (CA-on-CA placeholder rewrite at dispatch time) + realisation_deps
  insert. The #(refs.metric)("rio_scheduler_ca_cutoff_saves_total") metric is the direct
  efficacy signal. See `r[sched.ca.cutoff-compare]`,
  `r[sched.ca.cutoff-propagate+2]`, `r[sched.ca.resolve]` in the
  #cross-link("/spec/components/scheduler.typ")[scheduler spec].
]

See #cross-link("/spec/components/gateway.typ")[rio-gateway] for protocol opcode details,
#cross-link("/spec/components/scheduler.typ")[rio-scheduler] for the scheduling algorithm,
and #cross-link("/spec/components/store.typ")[rio-store] for the chunked CAS.

#let rs-notes = (
  [Protocol $>=$ 1.32 batches sources via `wopAddMultipleToStore` instead.],
  [Called unconditionally by Nix $>=$ 2.4 for both input-addressed and CA
    derivations.],
  [Inline `BasicDerivation` _without_ `inputDrvs` --- gateway rebuilds the
    DAG from the `.drv` files uploaded above.],
  [`PrefetchHint` precedes the assignment so the executor can pre-warm its
    FUSE cache.],
  [FastCDC chunking is server-side; executors never chunk locally.],
  [`STDERR_LAST` first, then raw NAR bytes --- no `STDERR_WRITE` framing
    (client's `processStderr` has no sink for this opcode).],
)

#figure(
  {
    // 5 lifelines + long opcode comments → widest of the flows; needs
    // harder scale to clear the right-hand callout column.
    flow-diagram(factor: 48%, chronos.diagram({
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
      _seq(
        "Client",
        "GW",
        comment: [`wopAddToStoreNar` (.drv files)#pin("rs-add")],
      )
      _seq("GW", "Store", comment: [`PutPath`])
      _seq(
        "Client",
        "GW",
        comment: [`wopQueryDerivationOutputMap`#pin("rs-qmap")],
      )
      _seq("GW", "Store", comment: [`GetPath` (.drv NAR)])
      _seq("GW", "GW", comment: [parse ATerm → output map])
      _seq("GW", "Client", comment: [derivation output map], dashed: true)
      _seq("Client", "GW", comment: [`wopBuildDerivation`#pin("rs-build")])
      _seq("GW", "Sched", comment: [`SubmitBuild` (DAG)])
      _seq("Sched", "Store", comment: [`FindMissingPaths` (cache check)])
      _seq("Store", "Sched", comment: [missing paths], dashed: true)
      _seq(
        "Sched",
        "Builder",
        comment: [`PullAssignment` delivery (pull mode)#pin("rs-assign")],
      )
      _seq("Builder", "Store", comment: [`GetPath` (FUSE fetch)])
      _seq("Builder", "Builder", comment: [nix sandbox build])
      _seq("Builder", "Sched", comment: [`BuildLogBatch`])
      _seq("Sched", "GW", comment: [`BuildEvent` (logs)])
      _seq("GW", "Client", comment: [`STDERR_NEXT`])
      _seq("Builder", "Store", comment: [`PutPath` (output)#pin("rs-put")])
      _seq("Builder", "Sched", comment: [`CompletionReport`])
      _seq("Sched", "GW", comment: [`BuildEvent` (completed)])
      _seq("GW", "Client", comment: [`STDERR_LAST` + `BuildResult`])
      _seq("Client", "GW", comment: [`wopNarFromPath`])
      _seq("GW", "Store", comment: [`GetPath`])
      _seq("Store", "GW", comment: [NAR stream], dashed: true)
      _seq("GW", "Client", comment: [NAR data#pin("rs-nar")], dashed: true)
    }))
    flow-note("rs-add", dy: -10pt, rs-notes.at(0))
    flow-note("rs-qmap", dy: -4pt, rs-notes.at(1))
    flow-note("rs-build", dy: -4pt, rs-notes.at(2))
    flow-note("rs-assign", dy: 6pt, rs-notes.at(3))
    flow-note("rs-put", dy: -4pt, rs-notes.at(4))
    flow-note("rs-nar", dy: -32pt, rs-notes.at(5))
  },
  caption: [Remote-store build flow (`nix build --store ssh-ng://rio`).],
)
#flow-notes-web(..rs-notes)

== Remote Builder: `nix build --builders 'ssh-ng://rio ...'`

#let rb-notes = (
  [rio-gateway sees a normal ssh-ng session --- it does not distinguish
    build-hook from direct-client connections. The build hook is a local
    nix-daemon concept.],
  [Local daemon drives DAG traversal --- rio only ever sees one derivation
    at a time. Less optimal scheduling, but compatible with any existing Nix
    setup.],
)

#figure(
  {
    flow-diagram(chronos.diagram({
      import chronos: *
      _par("Daemon", display-name: [Local nix-daemon])
      _par("Hook", display-name: [Build Hook])
      _par("GW", display-name: [rio-gateway])
      _par("Sched", display-name: [rio-scheduler])
      _par("Builder", display-name: [rio-builder])

      _seq("Daemon", "Hook", comment: [delegate derivation])
      _seq("Hook", "GW", comment: [SSH connect#pin("rb-conn")])
      _seq("Hook", "GW", comment: [`wopBuildDerivation` (single)])
      _seq("GW", "Sched", comment: [`SubmitBuild` (single node)])
      _seq("Sched", "Builder", comment: [`WorkAssignment`])
      _seq("Builder", "Builder", comment: [build])
      _seq("Builder", "Sched", comment: [`CompletionReport`])
      _seq("Sched", "GW", comment: [`BuildEvent` (completed)])
      _seq("GW", "Hook", comment: [`BuildResult`])
      _seq("Hook", "Daemon", comment: [output path])
      _seq("Daemon", "Daemon", comment: [continue DAG#pin("rb-dag")])
    }))
    flow-note("rb-conn", dy: -6pt, rb-notes.at(0))
    flow-note("rb-dag", dy: -28pt, rb-notes.at(1))
  },
  caption: [Build-hook flow (`--builders 'ssh-ng://rio'`).],
)
#flow-notes-web(..rb-notes)

== Client Disconnection <sec-client-disconnect>

#let cd-notes = (
  [Shared derivation nodes stay live as long as at least one other interested
    build remains (DAG merge).],
  [Running executors are allowed to complete --- wasted work is bounded by
    one derivation per executor.],
  [On re-submit, outputs already stored are instant cache hits via
    `FindMissingPaths`.],
)

#info[
  *Not implemented (by design):* No orphan timeout window or explicit
  "reattach" mechanism. Reconnection safety comes from (a) shared-derivation
  DAG merge and (b) cache hits on already-stored outputs. A timed orphan grace
  period is not planned.
]

#figure(
  {
    flow-diagram(chronos.diagram({
      import chronos: *
      _par("Client", display-name: [Nix Client])
      _par("GW", display-name: [rio-gateway])
      _par("Sched", display-name: [rio-scheduler])
      _par("Builder", display-name: [rio-builder])

      _seq("Client", "GW", end-tip: "x", comment: [SSH connection drops])
      _seq("GW", "Sched", comment: [`CancelBuild` (client_disconnect)])
      _alt(
        [Shared derivation],
        {
          _seq(
            "Sched",
            "Sched",
            comment: [continue (other builds need it)#pin("cd-shared")],
          )
        },
        [Unique derivation],
        { _seq("Sched", "Sched", comment: [remove from queue immediately]) },
      )
      _seq(
        "Builder",
        "Sched",
        comment: [`CompletionReport` (if already Running)#pin("cd-running")],
      )
      _note("over", [Outputs kept in store regardless], pos: "Sched")
      _seq("Client", "GW", comment: [Reconnect + re-submit])
      _seq(
        "Sched",
        "Sched",
        comment: [DAG merge + cache hits on stored outputs#pin("cd-rejoin")],
      )
    }))
    flow-note("cd-shared", dy: -14pt, cd-notes.at(0))
    flow-note("cd-running", dy: -8pt, cd-notes.at(1))
    flow-note("cd-rejoin", dy: -2pt, cd-notes.at(2))
  },
  caption: [Client-disconnection handling.],
)
#flow-notes-web(..cd-notes)

== Scheduler Failover

+ Scheduler leader pod dies (crash, node failure, rolling update).
+ New scheduler pod acquires the Kubernetes Lease for #gls("leader-election").
+ New leader reconstructs in-memory state from PostgreSQL (see the
  #cross-link("/spec/components/scheduler.typ")[scheduler spec] State Recovery
  section). Dispatch is gated on `recovery_complete`.
+ Executor pulls fail over to the new leader: there are no streams to
  reconnect — the next `PullAssignment` poll simply lands there.
+ For gateway connections with active `SubmitBuild` streams:
  + The `BuildEvent` response stream breaks with a gRPC Transport error.
  + Gateway's `process_stream` classifies the error as
    `StreamProcessError::Transport` and re-subscribes via
    `WatchBuild(build_id)` --- up to
    #(refs.const)("MAX_RECONNECT") times with exponential
    backoff (1/2/4/8/16s, capped at 16s).
  + The new scheduler's snapshot-first `WatchBuild` attach describes the
    build's current state (aggregate counts, running derivations, terminal
    outcome if any); the gateway resynchronizes from it and continues with
    the live stream. The Nix client sees continuous `STDERR` streaming
    (possibly a brief pause during backoff).
  + If all #(refs.const)("MAX_RECONNECT") reconnects fail, or the error is
    `Wire` (#(refs.error-doc)("StreamProcessError", "Wire")), the gateway
    returns `MiscFailure` to the client (manual retry). See
    #rref("gw.reconnect.backoff") for the full classification.
  + If the gateway itself also restarted, see @sec-client-disconnect above.
+ Log events between the old leader's last S3 flush and its crash may be lost
  --- bounded by the 30s periodic flush (see
  #cross-link("/spec/system/observability.typ")[observability]).

== Import-From-Derivation (IFD)

@ifd occurs when Nix evaluation depends on a build result:

+ Client begins evaluation and discovers it needs to build a derivation
  before evaluation can continue.
+ Client opens a separate SSH channel (the primary channel is blocked in
  evaluation) and sends `wopBuildDerivation` for the IFD derivation.
+ rio-gateway receives a single-derivation build request on the new channel
  and forwards it to rio-scheduler as a `SubmitBuild` with
  `priority_class = "interactive"` (IFD builds are evaluation-blocking).
+ rio-scheduler assigns maximum priority to this derivation --- above all
  queued non-IFD work.
+ Executor builds the derivation and uploads the output.
+ rio-gateway returns `BuildResult` to the client on the IFD channel.
+ Client retrieves the output via `wopNarFromPath` on the IFD channel and
  resumes evaluation.
+ Client may submit the full DAG (including the IFD derivation) on the
  primary channel --- the IFD derivation is already cached (instant hit).

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

Nix evaluation may block on build results. The gateway must handle this
gracefully --- the client sends a build request mid-evaluation, and rio must
prioritize these "evaluation-blocking" builds. These show up as individual
`wopBuildDerivation` calls that arrive before the full DAG is known. See the
Import-From-Derivation data flow above.

== Schema migration

Database schema evolves across phases (new tables, new columns, index
changes). Migrations must be:

- *Forward-compatible*: old code must tolerate new columns (use `ADD COLUMN
  ... DEFAULT`)
- *Versioned*: use `sqlx migrate` with numbered migration files
- *Tested*: rollback scripts for each migration, tested in CI
- *Blue-green compatible*: during deployment, both old and new code versions
  may run simultaneously
