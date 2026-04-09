# rio-dashboard

> Web dashboard for operational visibility. Svelte 5 SPA,
> in-process tonic-web on the scheduler, Cilium Gateway API ingress,
> DAG visualization via @xyflow/svelte.

## Architecture

The dashboard is a **Svelte 5** single-page application (`rio-dashboard/`, built
by `nix/dashboard.nix` via `fetchPnpmDeps` + Vite). It does NOT share a process
with any backend component — it is a pure frontend consuming `AdminService` and
`SchedulerService` via gRPC-Web.

**Transport chain (browser → scheduler):**

```
browser (connect-web, gRPC-Web framing)
    │  HTTP/1.1 POST /rio.admin.AdminService/ListBuilds
    ▼
  nginx (baked into the dashboard image, nix/docker.nix)
    │  proxy_buffering off; proxies /rio.* to the Cilium Gateway Service
    ▼
  Cilium Gateway (Gateway API + GRPCRoute, embedded envoy)
    │  plain HTTP routing — no protocol translation here
    ▼
  rio-scheduler:9001 (tonic-web layer accepts gRPC-Web natively;
                      same port serves native gRPC over h2)
```

gRPC-Web translation happens **in-process at the scheduler** via `tonic-web`
(`GrpcWebLayer` + `accept_http1`). The Cilium Gateway is a plain HTTP router
reconciled from `GatewayClass`/`Gateway`/`GRPCRoute` CRDs
(`infra/helm/rio-build/templates/dashboard-gateway.yaml`); Cilium's embedded
envoy handles it (no separate Envoy Gateway operator). nginx is a thin HTTP/1.1
proxy that serves the SPA static assets and forwards `/rio.*` to the
Cilium-provisioned Gateway Service. CORS lives in the scheduler
(`tower-http` `CorsLayer`, `RIO_DASHBOARD__CORS_ALLOW_ORIGINS`), not in a proxy
CRD.

**No Ingress.** Access is via `kubectl port-forward svc/rio-dashboard 8080:80` —
the dashboard is an operator-facing tool (matches the Grafana model, not a public
endpoint). CORS `allowOrigins` defaults to the in-cluster nginx Service hostname.

**Frontend stack:** Svelte 5 runes mode (`$state`/`$effect`/`$props`),
`svelte-routing` for client-side routing, `@connectrpc/connect-web` with
`createGrpcWebTransport` + binary framing, `@xyflow/svelte` for DAG visualization,
`@dagrejs/dagre` for layout (falls back to a Web Worker above 500 nodes).

## Key Views

| View | Data Source | Description |
|------|-------------|-------------|
| Cluster | `AdminService.ClusterStatus` | Executor/build/derivation counts, entry point to GC |
| Builds | `AdminService.ListBuilds` | Paginated list with status filter + per-build drawer; entry point to the killer journey. `/builds/:id` deep-links directly to a build's drawer (currently resolved via a one-shot `listBuilds(1000)` scan until a dedicated `GetBuild` RPC lands). |
| Build drawer · Graph tab | `AdminService.GetBuildGraph` | Interactive DAG visualization (`@xyflow/svelte`), color-coded status, degrades to table >2000 nodes, polls 5s until all-terminal |
| Build drawer · Logs tab | `AdminService.GetBuildLogs` (server stream) | Live-tail build output, UTF-8-lossy decode, virtualized scroller; `drvPath` filter set by Graph node click |
| Executors | `AdminService.ListExecutors` | Busy/idle pill (one-build-per-pod ⇒ binary), kind filter (builder/fetcher), stale-heartbeat highlight, drain button |
| GC | `AdminService.TriggerGC` (server stream) | Dry-run toggle, grace-period number input (hours), live sweep progress, cancel via `AbortController` |
| Toast portal | — | Single `<Toast/>` mounted in `App.svelte`; any component imports `toast.{info,error}` to push, auto-dismiss 4s |

There is no standalone "Graph page" — the DAG and the log viewer are **drawer tabs** under Builds. `BuildDrawer` keeps `focusedDrv` state across tab switches so a Graph→DrvNode click survives the tab flip and filters the log stream.

Executor utilization time-series and cache hit-rate analytics are **NOT** dashboard
scope — they live in the Grafana dashboards (`infra/helm/rio-build/dashboards/`). The
rio-dashboard focuses on interactive per-build detail (DAG, logs, management
actions) that a Prometheus/Grafana stack can't give you.

## Normative requirements

r[dash.envoy.grpc-web-translate+3]
The scheduler accepts gRPC-Web natively on its main port via `tonic-web` (`GrpcWebLayer` + `accept_http1(true)`): browser HTTP/1.1 POST and native gRPC over h2 share `:9001`. A Cilium-managed `GRPCRoute` CRD routes `rio.admin.AdminService` and `rio.scheduler.SchedulerService` methods to `rio-scheduler:9001` over plain HTTP — no protocol translation, no upstream TLS. CORS is in-process (`tower-http` `CorsLayer`) with `grpc-status`/`grpc-message`/`grpc-status-details-bin` in `expose_headers`; `RIO_DASHBOARD__CORS_ALLOW_ORIGINS` configures allowed origins. No separate Envoy Gateway operator, no `BackendTLSPolicy`/`SecurityPolicy`/`EnvoyProxy` CRDs.

r[dash.auth.method-gate+2]
The `GRPCRoute` splits `AdminService` methods by impact: read-only methods (`ClusterStatus`, `ListExecutors`, `ListPoisoned`, `ListBuilds`, `GetBuildLogs`, `ListTenants`, `GetBuildGraph`, `GetSizeClassStatus`) route unconditionally; mutating methods (`ClearPoison`, `DrainExecutor`, `CreateTenant`, `TriggerGC`) route only when `dashboard.enableMutatingMethods` is true (default false). Until dashboard-native authz lands, mutating operations go through `rio-cli` over a `kubectl port-forward`. CORS `allowOrigins` defaults to the in-cluster nginx Service hostname, not wildcard.

r[dash.journey.build-to-logs]
The killer journey: click build (Builds page) → drawer opens, DAG renders (Graph tab) → click running node (DrvNode) → log stream renders (Logs tab). The nginx→Cilium Gateway→scheduler chain MUST support server-streaming end-to-end (verified by the 0x80 trailer-frame byte in curl).

r[dash.graph.degrade-threshold]
Graph rendering MUST degrade to a sortable table when the node count exceeds 2000. dagre layout on >2000 nodes freezes the main thread. Above 500 nodes, dagre runs in a Web Worker. The server separately caps responses at 5000 nodes (`GetBuildGraphResponse.truncated`).

r[dash.stream.log-tail]
`GetBuildLogs` server-stream consumption MUST use `TextDecoder('utf-8', {fatal: false})` — build output can contain non-UTF-8 bytes (compiler locale garbage). Lossy decode to `U+FFFD`, never throw. nginx `proxy_buffering off` is required or the stream buffers entirely before reaching the browser.

r[dash.stream.idle-timeout]
The streaming chain MUST tolerate ≥1h of silence on an open `GetBuildLogs`/`WatchBuild` stream (a build that prints nothing for 5 minutes is normal under LLVM-cold-ccache). nginx `proxy_read_timeout` is set to 1h (default 60s cuts first); the scheduler sends a 30s server-initiated h2 keep-alive PING (`http2_keep_alive_interval`) so the Cilium Gateway envoy's `stream_idle_timeout` (default 5m) never fires. The 1h ceiling is intentional (a stream truly quiet for an hour means the build is stuck).

r[dash.log.cap]
The log stream's reactive line buffer MUST be capped client-side. At `MAX_LINES = 50_000` the store splices the oldest `lines.length - (MAX_LINES - DROP_LINES)` lines (where `DROP_LINES = 10_000`), flips `truncated = true`, and accumulates `droppedLines` for the banner. The hysteresis gap means the splice fires once per ~10k lines instead of every chunk near the cap. 50k lines × ~100 bytes ≈ 5MB of strings — generous for a tab, small enough V8 GC keeps up. Per-chunk append is loop-push (NOT spread-push: a 100k-line backfill chunk would hit V8's ~65k-argument `RangeError`).

r[dash.log.virtualize]
`LogViewer` MUST render a windowed slice, not one DOM node per line. Fixed `line-height` (measured from `getComputedStyle`, fallback 20px under jsdom) makes the visible range arithmetic from `scrollTop`; spacer `<div>`s above/below fill the off-screen height so `scrollHeight` stays synthetically equal to `lines.length × lineH` and follow-tail's `scrollTop = scrollHeight` lands at the bottom. Lines clip with `text-overflow: ellipsis` (NOT `white-space: pre-wrap` — wrapped lines would desync the spacer math). Trade: losing wrap on 200-char lines for O(viewport) DOM under 100k-line builds.

r[dash.graph.auto-stop]
The Graph tab's 5s `GetBuildGraph` poll MUST stop once every node is in a terminal status (per `graphLayout.TERMINAL`, which mirrors `is_terminal()` scheduler-side). The check is gated on `!truncated && nodes.length > 0` — an empty response (build not yet populated) and a truncated response (visible-terminal ≠ all-terminal under insertion-order truncation) MUST NOT stop polling. The poll is also serialized by an `inflight` re-entrancy gate so a slow fetch + slow worker layout don't overlap and last-write-wins with stale statuses.

r[dash.executors.kind-filter]
The Executors page exposes a `kind` `<select>` filtering on `ExecutorInfo.kind` (raw wire integers `0`=builder, `1`=fetcher). Surfaces the ADR-019 builder/fetcher split for "narrow to airgapped builders only" diagnostics.

r[dash.clear-poison]
`ClearPoisonButton` is embedded in `DrvNode`'s right-click context menu (rendered only when `poisoned`). It calls `AdminService.ClearPoison({derivationHash})` after a `confirm()` and pushes a toast — `cleared=false` (race with a successful retry) is an info toast, not an error. No optimistic mutation; the next 5s graph poll picks up the `poisoned→queued` transition. Subject to `r[dash.auth.method-gate+2]` (mutating method).

r[dash.toast]
A single `<Toast/>` portal is mounted in `App.svelte`. Any component imports `toast.{info,error}` from `lib/toast` (a plain `writable<ToastMsg[]>`, not runes-in-module) to push without prop-drilling; messages auto-dismiss after 4s. Write-action surfaces (`DrainButton`, `ClearPoisonButton`, GC stream errors) MUST report via toast — the alternative (`alert()`) blocks the event loop and breaks server-stream consumption.
