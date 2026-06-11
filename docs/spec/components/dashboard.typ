#import "/lib/rio.typ": *
#show: rio.with(domains: ("dash",))


_Web dashboard for operational visibility. Svelte 5 SPA, in-process tonic-web
on the scheduler, Cilium Gateway API ingress, @dag visualization via
`@xyflow/svelte`._

= Architecture

The dashboard is a *Svelte 5* single-page application (`rio-dashboard/`, built
by `nix/dashboard.nix` via `fetchPnpmDeps` + Vite). It does NOT share a process
with any backend component --- it is a pure frontend consuming `AdminService`
and `SchedulerService` via gRPC-Web.

#figure(
  caption: [Transport chain (browser → scheduler). gRPC-Web translation is
    in-process at the scheduler; the Cilium Gateway is a plain HTTP router.],
  diagram(
    spacing: (0mm, 10mm),
    node-stroke: 0.5pt,
    node(
      (0, 0),
      align(
        center,
      )[browser\ #text(size: 0.85em)[connect-web, gRPC-Web framing]],
      name: <br>,
    ),
    node(
      (0, 1),
      align(center)[nginx\ #text(size: 0.85em)[baked into the dashboard image,
          `nix/docker.nix`]],
      name: <ng>,
      fill: accent.lighten(88%),
    ),
    node(
      (0, 2),
      align(center)[Cilium Gateway\ #text(size: 0.85em)[Gateway API +
          `GRPCRoute`, embedded envoy]],
      name: <gw>,
      fill: accent.lighten(88%),
    ),
    node(
      (0, 3),
      align(center)[`rio-scheduler:9001`\ #text(size: 0.85em)[`tonic-web`
          accepts gRPC-Web natively;\ same port serves native gRPC over h2]],
      name: <sch>,
      fill: accent.lighten(88%),
    ),
    edge(
      <br>,
      <ng>,
      "-|>",
      [HTTP/1.1 POST `/rio.admin.AdminService/ListBuilds`],
      label-size: 0.8em,
      label-side: right,
    ),
    edge(
      <ng>,
      <sch>,
      "-|>",
      [`proxy_buffering off`; proxies `/rio.*` to the leader-only
        `rio-scheduler-leader` Service],
      label-size: 0.8em,
      label-side: left,
      bend: -35deg,
    ),
    edge(
      <br>,
      <gw>,
      "-|>",
      [north-south route (no port-forward): Gateway API `GRPCRoute` via the
        LoadBalancer IP],
      label-size: 0.8em,
      label-side: right,
      bend: 35deg,
    ),
    edge(
      <gw>,
      <sch>,
      "-|>",
      [plain HTTP routing --- no protocol translation here],
      label-size: 0.8em,
      label-side: right,
    ),
  ),
)

gRPC-Web translation happens *in-process at the scheduler* via `tonic-web`
(`GrpcWebLayer` + `accept_http1`). The Cilium Gateway is a plain HTTP router
reconciled from `GatewayClass`/`Gateway`/`GRPCRoute` CRDs
(`infra/helm/rio-build/templates/dashboard-gateway.yaml`); Cilium's embedded
envoy handles it (no separate Envoy Gateway operator) and it carries only the
north-south (browser → LoadBalancer) route. nginx is a thin HTTP/1.1 proxy that
serves the SPA static assets and forwards `/rio.*` directly to the leader-only
`rio-scheduler-leader` Service: its selector admits only pods carrying
`rio.build/scheduler-role=leader`, which the lease holder maintains on itself
(and sweeps off peers as part of the same reconcile), so endpoints converge to
the current leader on the holder's first successful reconcile after acquiring.
Operationally that means a brief 0-endpoint window during failover, and a
leader that never labels itself (RBAC, patch bug) leaves this Service empty
while the plain `rio-scheduler` Service still shows two ready pods; the full
failover/partition semantics are documented in `nix/dashboard-nginx.conf`. CORS
lives in the scheduler (`tower-http` `CorsLayer`,
`RIO_DASHBOARD__CORS_ALLOW_ORIGINS`), not in a proxy #gls("crd").

*No Ingress.* Access is via `kubectl port-forward svc/rio-dashboard 8080:80`
--- the dashboard is an operator-facing tool (matches the Grafana model, not a
public endpoint). CORS `allowOrigins` defaults to the in-cluster nginx Service
hostname.

*Frontend stack:* Svelte 5 runes mode (`$state`/`$effect`/`$props`),
`svelte-routing` for client-side routing, `@connectrpc/connect-web` with
`createGrpcWebTransport` + binary framing, `@xyflow/svelte` for DAG
visualization, `@dagrejs/dagre` for layout (falls back to a Web Worker above
500 nodes).

= Key Views

#table(
  columns: (auto, auto, 1fr),
  align: (left, left, left),
  table.header([View], [Data Source], [Description]),
  [Cluster],
  [`AdminService.ClusterStatus`],
  [Executor/build/derivation counts, entry point to GC],

  [Builds],
  [`AdminService.ListBuilds`],
  [Paginated list with status filter + per-build drawer; entry point to the
    killer journey. `/builds/:id` deep-links directly to a build's drawer
    (currently resolved via a one-shot `listBuilds(1000)` scan until a
    dedicated `GetBuild` RPC lands).],

  [Build drawer · Graph tab],
  [`AdminService.GetBuildGraph`],
  [Interactive DAG visualization (`@xyflow/svelte`), color-coded status,
    degrades to table >2000 nodes, polls 5s until all-terminal],

  [Build drawer · Logs tab],
  [`LogService.TailLog` (server stream, rio-store)],
  [Live-tail build output, UTF-8-lossy decode, virtualized scroller; `drvPath`
    filter set by Graph node click],

  [Executors],
  [`AdminService.ListExecutors`],
  [Busy/idle pill (one-build-per-pod ⇒ binary), kind filter (builder/fetcher),
    attempt-open age ("pulled" --- plain relative time, no staleness
    highlight: the timestamp never advances mid-build)],

  [GC],
  [`AdminService.TriggerGC` (server stream)],
  [Dry-run toggle, grace-period number input (hours), live sweep progress,
    cancel via `AbortController`],

  [Toast portal],
  [---],
  [Single `<Toast/>` mounted in `App.svelte`; any component imports
    `toast.{info,error}` to push, auto-dismiss 4s],
)

There is no standalone "Graph page" --- the DAG and the log viewer are *drawer
tabs* under Builds. `BuildDrawer` keeps `focusedDrv` state across tab switches
so a Graph→DrvNode click survives the tab flip and filters the log stream.

Executor utilization time-series and cache hit-rate analytics are *NOT*
dashboard scope --- they live in the Grafana dashboards
(`infra/helm/rio-build/dashboards/`). The rio-dashboard focuses on interactive
per-build detail (DAG, logs, management actions) that a Prometheus/Grafana
stack can't give you.

= Normative requirements

#r("dash.envoy.grpc-web-translate+3")[
  The scheduler accepts gRPC-Web natively on its main port via `tonic-web`
  (`GrpcWebLayer` + `accept_http1(true)`): browser HTTP/1.1 POST and native
  gRPC over h2 share `:9001`. A Cilium-managed `GRPCRoute` CRD routes
  `rio.admin.AdminService` and `rio.scheduler.SchedulerService` methods to
  `rio-scheduler:9001` over plain HTTP --- no protocol translation, no upstream
  TLS. CORS is in-process (`tower-http` `CorsLayer`) with
  `grpc-status`/`grpc-message`/`grpc-status-details-bin` in `expose_headers`;
  `RIO_DASHBOARD__CORS_ALLOW_ORIGINS` configures allowed origins. No separate
  Envoy Gateway operator, no `BackendTLSPolicy`/`SecurityPolicy`/`EnvoyProxy`
  CRDs.
]

#r("dash.auth.method-gate+5")[
  The `GRPCRoute` splits `AdminService` methods by impact: read-only methods
  (`ClusterStatus`, `ListExecutors`, `ListPoisoned`, `ListBuilds`,
  `ListTenants`, `GetBuildGraph`, `GetSpawnIntents`) route unconditionally, as
  does the read-only store-backed `LogService/TailLog` route (a separate
  `HTTPRoute` whose `backendRef` is `rio-store`); mutating methods
  (`ClearPoison`, `CreateTenant`, `TriggerGC`) route only when `dashboard.enableMutatingMethods`
  is true (default false). Until dashboard-native authz lands, mutating
  operations go through `rio-cli` over a `kubectl port-forward`. CORS
  `allowOrigins` defaults to the in-cluster nginx Service hostname, not
  wildcard.
]

#r("dash.journey.build-to-logs+2")[
  The killer journey: click build (Builds page) → drawer opens, DAG renders
  (Graph tab) → click running node (DrvNode) → log stream renders (Logs tab).
  Both REAL chains MUST support server-streaming end-to-end, verified by the
  `0x80` trailer-frame byte in curl: in-cluster, nginx dials each backend
  DIRECTLY (the leader-only `rio-scheduler-leader` Service for the DAG and
  build list, `rio-store` for the log stream --- both serve gRPC-Web natively
  via `tonic-web`, no Gateway hop); north-south, browser → Cilium Gateway
  LoadBalancer → backend. nginx MUST NOT route through the Gateway Service:
  Cilium's L7 redirect fires only on the LoadBalancer IP, and the
  selectorless Gateway Service has no Endpoints for in-cluster clients ---
  such a request hangs.
]

#r("dash.graph.degrade-threshold")[
  Graph rendering MUST degrade to a sortable table when the node count exceeds
  2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes,
  dagre runs in a Web Worker. The server separately caps responses at 5000
  nodes (`GetBuildGraphResponse.truncated`).
]

#r("dash.stream.log-tail+6")[
  `LogService.TailLog` server-stream consumption MUST use
  `TextDecoder('utf-8', {fatal: false})` --- build output can contain non-UTF-8
  bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx
  `proxy_buffering off` is required or the stream buffers entirely before
  reaching the browser. The follow loop MUST drive the stream iterator
  manually, and the post-terminal grace deadline MUST join every race as
  an ABSOLUTE timer --- unproductive traffic cannot starve enforcement.
  The grace is a QUIET-TIME budget, not a transfer cap: every productive
  serve re-arms it (a terminal build's historical drain completes however
  long it takes), while keep-alives and resent chunks never extend it,
  and a re-open whose remaining grace cannot fund a drain attempt
  finalizes at the decision point instead of waking at the deadline; the
  one-second terminality tick exists only to wake a QUIET stream's loop
  for the terminal re-check (a never-ending stream on a terminal build,
  chatty or quiet, must not hold the tab in "streaming" forever); each
  attempt carries its own abort controller chained to the consumer's.
  Chunks MUST be visited through the execution-keyed step: a chunk from
  a different execution than the cursor's is an explicit
  execution-switch row, a cursor reset, AND a reset of the
  served-complete claim (a completion minted against the old numbering
  never finishes the new execution's tab) --- never a silent swallow as
  a "duplicate" of the old numbering, never a seamless splice. A
  switching message that starts past line zero means the new
  execution's head was filtered server-side against the stale
  watermark: the attempt MUST be cut and re-opened at `sinceLine` 0 ---
  a `sinceLine` is only ever sent for the execution it was minted in.
  EVERY exit, including a store-stamped completion, decides through the
  mirrored `tail_next` law, whose dashboard-side inputs include the
  RESOLUTION MODE: completion is a per-execution predicate, so with a
  live oracle saying the derivation is non-terminal a LATEST-resolved
  stream re-opens to follow the retry (the gateway relay's behavior) ---
  but a PINNED stream (non-empty `exec_id` on the request) structurally
  cannot observe a retry, every re-open resends the pinned id, so its
  stamped completion is terminal by construction regardless of the
  oracle; without an oracle the exec-level claim stands in for
  terminality. A status
  the store typed permanently unservable exits terminally (no re-dial)
  and surfaces the incomplete banner; the auth-required terminal
  renders the sign-in notice through the exhaustive phase law, never
  the raw transport error or the truncation diagnosis.
]

#r("dash.stream.idle-timeout+3")[
  The streaming chain MUST tolerate ≥1h of silence on an open
  `TailLog`/`WatchBuild` stream (a build that prints nothing for 5 minutes
  is normal under LLVM-cold-ccache). nginx `proxy_read_timeout` is set to 1h
  (default 60s cuts first); the scheduler sends a 30s server-initiated h2
  keep-alive PING (`http2_keep_alive_interval`) so the Cilium Gateway envoy's
  `stream_idle_timeout` (default 5m) never fires. The 1h ceiling is intentional
  (a stream truly quiet for an hour means the build is stuck).
]

#r("dash.stream.reopen-pacing")[
  The follow loop's re-open delay MUST escalate (250 → 500 → 1000 → 2000 ms
  cap) whenever a stream ends without a productive chunk verdict, and MUST
  reset only on `serve`/`gapThenServe` visits --- bare receipt (a zero-line
  keep-alive or a fully-resent chunk, the `skip` verdict) is not progress.
]
A follow stream opened against a session with no live ingest ends
immediately by contract after one final (often zero-line) chunk. The
pre-fix loop reset its backoff on any receipt, so every such re-open looked
productive and an idle tab polled the store at a flat \~4 Hz indefinitely.
The pacer consumes the `ChunkVisit` verdict itself (`ReopenPacer.noteVisit`
in `lineCursor.ts`), so "reset on a non-productive receipt" is untypeable.
The gateway relay deliberately keeps its FIXED 1 s reconnect backoff: its
subscriptions are bounded per-derivation by the drain signal and the
post-terminal grace window, so a predictable cadence is worth more than a
lower floor there; the dashboard tab has neither bound.

#r("dash.log.cap")[
  The log stream's reactive line buffer MUST be capped client-side. At
  `MAX_LINES = 50_000` the store splices the oldest
  `lines.length - (MAX_LINES - DROP_LINES)` lines (where `DROP_LINES = 10_000`),
  flips `truncated = true`, and accumulates `droppedLines` for the banner. The
  hysteresis gap means the splice fires once per \~10k lines instead of every
  chunk near the cap. 50k lines × \~100 bytes ≈ 5MB of strings --- generous for
  a tab, small enough V8 GC keeps up. Per-chunk append is loop-push (NOT
  spread-push: a 100k-line backfill chunk would hit V8's \~65k-argument
  `RangeError`).
]

#r("dash.log.virtualize")[
  `LogViewer` MUST render a windowed slice, not one DOM node per line. Fixed
  `line-height` (measured from `getComputedStyle`, fallback 20px under jsdom)
  makes the visible range arithmetic from `scrollTop`; spacer `<div>`s above/
  below fill the off-screen height so `scrollHeight` stays synthetically equal
  to `lines.length × lineH` and follow-tail's `scrollTop = scrollHeight` lands
  at the bottom. Lines clip with `text-overflow: ellipsis` (NOT
  `white-space: pre-wrap` --- wrapped lines would desync the spacer math).
  Trade: losing wrap on 200-char lines for O(viewport) DOM under 100k-line
  builds.
]

#r("dash.log.attempt-scope")[
  The Logs tab is PER-ATTEMPT: a `TailLog` request MUST carry a
  non-empty selector --- a pinned execution (`exec_id`) or a non-empty
  `derivation` --- and the dashboard MUST refuse the empty form
  client-side, before the transport, with the store's permanent
  unservable type (`x-rio-log-unservable` metadata), which the
  stream's exit law already classifies as terminal (no re-dial). An
  unfocused Logs tab MUST render a static unavailable-by-design
  affordance instead of mounting a stream. Whole-build log
  aggregation is an explicit NON-GOAL absent a server-side
  aggregation contract: no resolver exists for an empty selector
  (`drv_log_hash('')` matches no execution), so the mode is
  unrepresentable in the UI rather than a guaranteed-NotFound dial
  loop.
]

#r("dash.drawer.keyed-session")[
  Per-build drawer state MUST live in ONE self-keyed session record
  minted per `buildId`: the record carries its own key, every
  build-identity change REPLACES the record wholesale before any
  consumer renders against it (never field-wise mutation of a live
  record), a stale record keyed to a different build than the one
  being rendered MUST NOT be consumed, and no per-build drawer state
  lives outside the record --- the record's key set is the
  machine-derived census of per-build state, so cross-build bleed
  (one build's focus, poll, or stream selector surviving into
  another's render) is structurally unrepresentable rather than
  reset-by-author-discipline.
]
Conformance rule over the landed wave-8 shape (the self-keyed
`DrawerSession` + pre-render replacement close): minted
rules-after-behavior because the wave-8 close predated this
namespace; zero behavior change rides this rule.

#r("dash.terminal-scope")[
  The dashboard's per-derivation status vocabulary MUST be the
  scheduler's derivation-status alphabet verbatim, conformance-pinned
  by the cross-language golden snapshot (`derivation_statuses.json`:
  every status string AND its `terminal` bit are asserted on both
  sides): per-derivation surfaces (graph nodes, pills) render
  derivation/attempt-scoped vocabulary --- `cancelled` is a
  derivation's attempt state, gray and terminal, NOT a build outcome
  --- and build-scoped terminal vocabulary enters only through
  build-level surfaces. A scheduler-side status addition or
  reclassification MUST fail both the Rust snapshot test and the
  dashboard cross-language check before any unmapped string can fall
  through to the gray default at runtime.
]
Same conformance form: the wave-8 client-stream close partitioned
attempt-scoped from build-scoped terminal vocabulary gateway-side
(#rref("gw.stderr.failure-hint") is the sibling mint from the same
close family); this rule pins the dashboard half that was outside
that wave's grant.

#r("dash.graph.auto-stop+3")[
  The Graph tab's 5s `GetBuildGraph` poll MUST downshift to the settled
  cadence (`SETTLED_POLL_MS`, 30s) once every node is in a terminal status
  (per `graphLayout.TERMINAL`, which mirrors `is_terminal()`
  scheduler-side) --- never stop outright: a `ClearPoison` issued out of
  band (rio-cli, the admin RPC, another session) MUST be discovered within
  the settled cadence and restore the live cadence, without depending on
  in-process notification. The settle check is gated on
  `!truncated && nodes.length > 0` ---
  an empty response (build not yet populated) and a truncated response
  (visible-terminal ≠ all-terminal under insertion-order truncation) MUST NOT
  settle the poll. Responses are applied only when their dispatch
  generation is current: a clear or teardown invalidates every in-flight
  fetch, so a response whose server read predates the clear MUST NOT
  re-latch the settled state. The poll is also serialized by an
  epoch-keyed `inflight` re-entrancy gate
  so a slow fetch + slow worker layout don't overlap and last-write-wins with
  stale statuses --- while a STALE in-flight fetch never swallows the
  restart's immediate shot after a clear. Every dispatch MUST carry a
  per-request deadline (`GRAPH_FETCH_DEADLINE_MS`) that CANCELS the
  in-flight request --- at the deadline and on drawer teardown ---
  bounded independent of the polling cadence (`POLL_MS` < deadline <=
  `SETTLED_POLL_MS`), so one black-holed unary can never freeze the
  poll or the terminality oracle beyond the envelope. Latch
  transitions MUST consume a closed evidence classification of the
  fenced response (settled | live | empty | partial-terminal | failed)
  as ONE total transition function over the latched x evidence
  product: the latch is released ONLY on live-work evidence; on a
  settled drawer the data axis is enumerated per evidence class ---
  empty, truncated-terminal, and failed probes RETAIN the latch, the
  retained graph, AND the settled cadence, while settled and live
  responses APPLY (an externally purged build MUST NOT become an
  absorbing live-cadence storm; a truncated first-N slice MUST NOT
  replace a retained complete view); a
  probe failure with retained data degrades --- a non-replacing
  staleness surface --- while `error` is reserved for the never-loaded
  state.
]

#r("dash.graph.truncated-follow")[
  A truncated graph view MUST keep following at the live cadence until
  the drawer closes: the settle latch consumes graph-shape evidence
  from the `GetBuildGraph` response alone (`!truncated` is a
  NECESSARY settle condition --- visible-terminal #sym.eq.not
  all-terminal under insertion-order truncation), and no second
  evidence source --- in particular the build-status poll's
  terminality --- may feed the latch unless a future rule mints a
  two-evidence settle law reconciled with the no-prop-fed-oracle
  posture.
]
Disposition record (merged_bug_082, PARTIAL --- a pre-campaign spec
tension, not a campaign regression): for builds above the server's
truncation threshold the MUST-downshift trigger of
#rref("dash.graph.auto-stop+3") is unobservable from this RPC exactly
where each poll is most expensive, so the drawer polls at the live
cadence for the build's whole open lifetime. That cost is the PRICED,
ACCEPTED residual --- the in-tree pricing lives at the BuildDrawer
oracle comment ("a stuck node in a >5000-node truncated view follows
until tab close --- following a stuck node is the idle-timeout rule's
own posture"), which cites this rule back. The alternative --- a
second typed evidence input from the build-status poll --- is the
recorded TRIGGER-NAMED future: it is minted (as a new two-evidence
settle rule, never an edit to this one) on the first operator
complaint about truncated-view poll cost; minting it now, with no
implementation, would violate the marker-first discipline.

#r("dash.executors.kind-filter")[
  The Executors page exposes a `kind` `<select>` filtering on
  `ExecutorInfo.kind` (raw wire integers `0`=builder, `1`=fetcher). Surfaces
  the ADR-019 builder/fetcher split for "narrow to airgapped builders only"
  diagnostics.
]

#r("dash.clear-poison")[
  `ClearPoisonButton` is embedded in `DrvNode`'s right-click context menu
  (rendered only when `poisoned`). It calls
  `AdminService.ClearPoison({derivationHash})` after a `confirm()` and pushes a
  toast --- `cleared=false` (race with a successful retry) is an info toast,
  not an error. No optimistic mutation; the next 5s graph poll picks up the
  `poisoned→queued` transition. Subject to #rref("dash.auth.method-gate+5")
  (mutating method).
]

#r("dash.toast")[
  A single `<Toast/>` portal is mounted in `App.svelte`. Any component imports
  `toast.{info,error}` from `lib/toast` (a plain `writable<ToastMsg[]>`, not
  runes-in-module) to push without prop-drilling; messages auto-dismiss after
  4s. Write-action surfaces (`DrainButton`, `ClearPoisonButton`, GC stream
  errors) MUST report via toast --- the alternative (`alert()`) blocks the
  event loop and breaks server-stream consumption.
]

= Rationale

== Web dashboard // supersedes ADR-014

Operators need interactive visibility into the build system: what is building,
what failed, build durations, and executor health. CLI tools provide
point-in-time queries but not the interactive exploration (DAG, log tail,
management actions) that an operational dashboard gives.

The dashboard is a TypeScript SPA built with *Svelte 5* (runes mode), consuming
the `AdminService` gRPC-Web API and deployed as a separate Kubernetes
Deployment (`rio-dashboard`) serving static assets via nginx. The original ADR
draft specified React for ecosystem breadth; implementation chose Svelte 5 for
its smaller runtime, runes-mode reactivity (`$state`/`$effect` map cleanly onto
server-stream consumption), and `@xyflow/svelte` covering the DAG-visualization
need without a heavier React reconciler. The gRPC-Web client
(`@connectrpc/connect-web`) is framework-agnostic. The ADR's stated negative
that gRPC-Web "requires a proxy between the browser and the gRPC backend" is
superseded by `tonic-web` accepting gRPC-Web in-process at the scheduler
(#rref("dash.envoy.grpc-web-translate+3")).

*Alternatives considered.* Grafana-only dashboards leverage existing
observability but cannot display build logs, DAG visualizations, or interactive
build management --- hence the scope split: time-series and hit-rate analytics
stay in Grafana, per-build detail lives here. A `rio-cli` terminal UI (ratatui)
suits power users but lacks team-wide visibility and shareable links. A
server-rendered web app (htmx, Leptos) avoids SPA complexity but real-time log
streaming and interactive DAG visualization benefit from a rich client-side
runtime, and gRPC-Web integration is more natural in TypeScript. Embedding the
UI in a backend binary would couple dashboard release cycles to the backend;
static asset serving doesn't justify embedding in Rust.

*Consequences.* Rich interactive UI for build monitoring, log viewing, and DAG
visualization; separate deployment allows independent scaling and release
cadence; gRPC-Web provides type-safe API integration. On the negative side: a
frontend technology stack (TypeScript, Svelte, pnpm) is added to a primarily
Rust/Nix project, and operators rely on CLI tools and Grafana for visibility
until the dashboard ships.
